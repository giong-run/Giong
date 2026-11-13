//! The per-worker pool.

use std::time::Duration;

use giong::time::{sleep, timeout, Instant};
use giong_postgres::{Client, ErrorKind, Pool};

use crate::run;

#[test]
fn shared_statements_spread_over_connections_up_to_the_maximum() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 3);
        assert_eq!(pool.stats().connections, 0, "connections open on demand");
        let tasks: Vec<_> = (0..100i32)
            .map(|i| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let row = pool.query_one("SELECT $1::int4 + 1", &[&i]).await.unwrap();
                    row.get::<i32>(0).unwrap()
                })
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), i as i32 + 1);
        }
        let stats = pool.stats();
        assert!(
            stats.connections <= 3 && stats.connections >= 1,
            "{stats:?}"
        );
        assert_eq!(stats.opened, stats.connections as u64);
        assert_eq!(stats.in_flight, 0);
        assert_eq!(stats.taken, 0);
    });
}

#[test]
fn a_taken_connection_is_exclusive_and_comes_back_clean() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1).wait_timeout(Duration::from_millis(300));
        pool.batch_execute("DROP TABLE IF EXISTS pool_tx; CREATE TABLE pool_tx (id int)")
            .await
            .unwrap();
        {
            let mut conn = pool.get().await.unwrap();
            assert_eq!(pool.stats().taken, 1);
            // The only connection is taken: shared statements wait, then time
            // out.
            let e = pool.query("SELECT 1", &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::PoolTimeout);
            assert!(e.to_string().contains("max_size"), "{e}");
            let tx = conn.transaction().await.unwrap();
            tx.execute("INSERT INTO pool_tx VALUES (1)", &[])
                .await
                .unwrap();
            // Dropped without commit, and the connection goes back.
        }
        assert_eq!(pool.stats().taken, 0);
        let n: i64 = pool
            .query_one("SELECT count(*) FROM pool_tx", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 0, "rolled back");

        // A transaction left open by hand is rolled back on return too.
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("BEGIN; INSERT INTO pool_tx VALUES (2)")
                .await
                .unwrap();
        }
        let e = pool.batch_execute("SAVEPOINT s").await.unwrap_err();
        assert_eq!(
            e.code(),
            Some("25P01"),
            "the shared connection is not in a transaction: {e}"
        );
        assert_eq!(pool.stats().timeouts, 1);
    });
}

#[test]
fn a_waiting_task_gets_the_connection_when_it_is_returned() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        let conn = pool.get().await.unwrap();
        let waiter = giong::spawn({
            let pool = pool.clone();
            async move { pool.get().await.map(|c| c.in_flight()) }
        });
        giong::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.stats().waiting, 1);
        drop(conn);
        assert_eq!(waiter.await.unwrap().unwrap(), 0);
    });
}

#[test]
fn broken_connections_are_replaced() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2);
        let pid: i32 = pool
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        s.psql(&format!("SELECT pg_terminate_backend({pid})"));
        for _ in 0..100 {
            if pool.stats().connections == 0 {
                break;
            }
            // Looking at the pool drops connections that have closed.
            let _ = pool.get().await.map(drop);
            giong::time::sleep(Duration::from_millis(10)).await;
        }
        let other: i32 = pool
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_ne!(other, pid);
    });
}

#[test]
fn connect_errors_are_reported_and_counted() {
    run(|s| async move {
        let pool = Pool::new(s.local().dbname("missing_db"), 2);
        let e = pool.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.code(), Some("3D000"), "{e}");
        assert_eq!(pool.stats().connect_errors, 1);
        assert_eq!(pool.stats().connections, 0);
    });
}

#[test]
fn a_slow_statement_does_not_hold_up_the_ones_behind_it() {
    run(|s| async move {
        // The server ends the slow statement after 2 s.
        let pool = Pool::new(s.local().statement_timeout(Duration::from_secs(2)), 2);
        // Open both connections.
        let warm: Vec<_> = (0..2)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move { pool.execute("SELECT pg_sleep(0.05)", &[]).await })
            })
            .collect();
        for t in warm {
            t.await.unwrap().unwrap();
        }
        assert_eq!(pool.stats().connections, 2);

        let slow = giong::spawn({
            let pool = pool.clone();
            async move { pool.execute("SELECT pg_sleep(10)", &[]).await }
        });
        giong::time::sleep(Duration::from_millis(50)).await;
        // Many at once, so the connection with the slow statement is the
        // least busy by count.
        let started = giong::time::Instant::now();
        let quick: Vec<_> = (0..20i32)
            .map(|i| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let row = pool.query_one("SELECT $1::int4", &[&i]).await.unwrap();
                    assert_eq!(row.get::<i32>(0).unwrap(), i);
                })
            })
            .collect();
        for t in quick {
            t.await.unwrap();
        }
        let took = started.elapsed();
        // Generous, for slow runs under valgrind or ThreadSanitizer; behind
        // the slow statement they would take 2 s.
        assert!(
            took < Duration::from_secs(1),
            "the quick statements waited behind the slow one: {took:?}"
        );
        let e = slow.await.unwrap().unwrap_err();
        assert_eq!(e.code(), Some("57014"), "{e}");
    });
}

#[test]
fn transactions_never_run_on_shared_connections() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_leak; CREATE TABLE pool_leak (id int)")
            .await
            .unwrap();
        // Refused before anything is sent.
        for sql in [
            "BEGIN",
            "  /* a /* nested */ comment */ begin isolation level serializable",
            "-- note\nSTART TRANSACTION",
            "commit",
            "END",
            "ROLLBACK",
            "abort",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
            "PREPARE TRANSACTION 'x'",
        ] {
            let e = pool.execute(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
            assert!(e.to_string().contains("pool.get()"), "{e}");
            let e = pool.query(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
        }
        // Not transaction statements, though they look a little like them.
        let e = pool
            .execute("COMMIT PREPARED 'nothing'", &[])
            .await
            .unwrap_err();
        assert_ne!(e.kind(), ErrorKind::Statement, "{e}");
        pool.execute("SELECT 1 AS begin", &[]).await.unwrap();
        let e = pool
            .execute("PREPARE p AS SELECT 1; DEALLOCATE p", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Db, "{e}");

        // A whole transaction in one batch is fine.
        pool.batch_execute("BEGIN; INSERT INTO pool_leak VALUES (1); COMMIT")
            .await
            .unwrap();

        // A batch that leaves one open closes its connection: the other
        // task's insert, pipelined behind it on the only connection, would
        // otherwise have run inside it and never been committed.
        let leak = giong::spawn({
            let pool = pool.clone();
            async move {
                pool.batch_execute("BEGIN; INSERT INTO pool_leak VALUES (2)")
                    .await
            }
        });
        let other = giong::spawn({
            let pool = pool.clone();
            async move { pool.execute("INSERT INTO pool_leak VALUES (3)", &[]).await }
        });
        let e = leak.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("left a transaction open"), "{e}");
        let e = other.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");

        // Both rolled back; the pool goes on with a new connection.
        let n: i64 = pool
            .query_one("SELECT count(*) FROM pool_leak", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(pool.stats().opened, 2);
    });
}

#[test]
fn a_connection_given_back_with_begin_in_flight_is_rolled_back() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_late; CREATE TABLE pool_late (id int)")
            .await
            .unwrap();
        {
            let conn = pool.get().await.unwrap();
            // The caller gives up before BEGIN is answered: the last answer
            // the connection has says no transaction is open.
            let late = timeout(
                Duration::from_millis(20),
                conn.batch_execute("SELECT pg_sleep(0.2); BEGIN"),
            )
            .await;
            assert!(late.is_err());
        }
        pool.execute("INSERT INTO pool_late VALUES (1)", &[])
            .await
            .unwrap();
        // Committed: another session sees it.
        let other = Client::connect(&s.local()).await.unwrap();
        let n: i64 = other
            .query_one("SELECT count(*) FROM pool_late", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
    });
}

#[test]
fn a_waiting_task_hears_when_a_dropped_statement_frees_the_connection() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1).wait_timeout(Duration::from_secs(3));
        pool.execute("SELECT 1", &[]).await.unwrap();
        // Dropped while the server still runs it.
        let dropped = timeout(
            Duration::from_millis(20),
            pool.execute("SELECT pg_sleep(0.3)", &[]),
        )
        .await;
        assert!(dropped.is_err());
        // A task of its own, polled only when woken.
        let waiter = giong::spawn({
            let pool = pool.clone();
            async move {
                let started = Instant::now();
                let conn = pool.get().await?;
                Ok::<_, giong_postgres::Error>((started.elapsed(), conn.in_flight()))
            }
        });
        let (waited, in_flight) = waiter.await.unwrap().unwrap();
        assert_eq!(in_flight, 0);
        // Free after about 0.3 s; nothing announced it, so the wait ran
        // into the timeout.
        assert!(waited < Duration::from_secs(2), "waited {waited:?}");
        sleep(Duration::from_millis(1)).await;
    });
}
