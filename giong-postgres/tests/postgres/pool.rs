//! The per-worker pool.

use std::cell::Cell;
use std::rc::Rc;
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
            e.server_code(),
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
        let pool = Pool::new(s.local().database("missing_db"), 2);
        let e = pool.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.server_code(), Some("3D000"), "{e}");
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
        assert_eq!(e.server_code(), Some("57014"), "{e}");
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

#[test]
fn a_connection_comes_back_from_get_with_a_clean_session() {
    run(|s| async move {
        s.psql(
            "DROP ROLE IF EXISTS pool_tenant; CREATE ROLE pool_tenant; \
             DROP SCHEMA IF EXISTS pool_tenant_b CASCADE; CREATE SCHEMA pool_tenant_b",
        );
        let pool = Pool::new(s.local().param("search_path", "public"), 1);
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute(
                "SET ROLE pool_tenant; SET search_path = pool_tenant_b; \
                 SET statement_timeout = 0; SET work_mem = '77MB'; \
                 CREATE TEMP TABLE pool_scratch (a int); LISTEN pool_jobs; \
                 PREPARE pool_mine AS SELECT 1; \
                 BEGIN; DECLARE pool_cursor CURSOR WITH HOLD FOR SELECT 1; COMMIT",
            )
            .await
            .unwrap();
            conn.query_one("SELECT pg_advisory_lock(42)", &[])
                .await
                .unwrap();
        }
        // The next shared statement runs after the reset, on the same and
        // only connection.
        let row = pool
            .query_one(
                "SELECT current_user::text, current_setting('search_path'), \
                 current_setting('statement_timeout'), current_setting('work_mem'), \
                 (SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' \
                  AND pid = pg_backend_pid()), \
                 (SELECT count(*) FROM pg_listening_channels()), \
                 (SELECT count(*) FROM pg_cursors WHERE name = 'pool_cursor'), \
                 (SELECT count(*) FROM pg_prepared_statements WHERE name = 'pool_mine'), \
                 (SELECT count(*) FROM pg_class WHERE relname = 'pool_scratch')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "postgres");
        // Back to the value Config::param gave, not the server's default.
        assert_eq!(row.get::<&str>(1).unwrap(), "public");
        // The configured statement timeout survives RESET ALL.
        assert_eq!(row.get::<&str>(2).unwrap(), "30s");
        assert_eq!(row.get::<&str>(3).unwrap(), "4MB");
        for (i, what) in [
            (4, "advisory locks"),
            (5, "channels listened to"),
            (6, "cursors"),
            (7, "SQL-level prepared statements"),
            (8, "temporary tables"),
        ] {
            assert_eq!(row.get::<i64>(i).unwrap(), 0, "{what} left behind");
        }
        // The statement cache survived, apart from after DEALLOCATE ALL: the
        // pool's statements still run.
        for _ in 0..3 {
            pool.query_one("SELECT 1", &[]).await.unwrap();
        }
        // The next get() finds the same clean session, and may PREPARE the
        // same name again.
        let conn = pool.get().await.unwrap();
        conn.batch_execute("PREPARE pool_mine AS SELECT 2")
            .await
            .unwrap();
        assert_eq!(pool.stats().opened, 1, "one connection all along");
    });
}

#[test]
fn an_unused_connection_from_get_is_not_reset() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.query_one("SELECT 1", &[]).await.unwrap();
        drop(pool.get().await.unwrap());
        // Nothing queued for it.
        assert_eq!(pool.stats().in_flight, 0);
    });
}

#[test]
fn shared_methods_refuse_statements_that_change_the_session() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_copy; CREATE TABLE pool_copy (a int)")
            .await
            .unwrap();
        for sql in [
            "SET search_path = x",
            "SET ROLE nobody",
            "set session authorization nobody",
            "RESET ALL",
            "DISCARD ALL",
            "LISTEN x",
            "PREPARE p AS SELECT 1",
            "DEALLOCATE ALL",
            "DECLARE c CURSOR WITH HOLD FOR SELECT 1",
            "CREATE TEMP TABLE t (a int)",
            "COPY pool_copy FROM STDIN",
        ] {
            let e = pool.execute(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
            assert!(e.to_string().contains("nothing was sent"), "{e}");
            let batch = format!("SELECT 1; {sql}");
            let e = pool.batch_execute(&batch).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{batch:?}: {e}");
        }
        // Transaction-scoped settings are fine, in a batch with its
        // transaction or on their own.
        pool.batch_execute(
            "BEGIN; SET LOCAL work_mem = '8MB'; INSERT INTO pool_copy VALUES (1); COMMIT",
        )
        .await
        .unwrap();
        pool.execute("SET LOCAL work_mem = '8MB'", &[])
            .await
            .unwrap();
        // A COPY in one task never fails another's statement: it is refused
        // before anything is sent.
        let other = giong::spawn({
            let pool = pool.clone();
            async move { pool.query_one("SELECT 1", &[]).await.map(|_| ()) }
        });
        assert!(pool
            .batch_execute("COPY pool_copy FROM STDIN")
            .await
            .is_err());
        other.await.unwrap().unwrap();
        assert_eq!(pool.stats().opened, 1);
    });
}

/// Tasks running shared statements back to back until `stop`, counting them.
fn hammer(pool: &Pool, tasks: usize) -> (Rc<Cell<bool>>, Rc<Cell<u64>>) {
    let stop = Rc::new(Cell::new(false));
    let done = Rc::new(Cell::new(0u64));
    for _ in 0..tasks {
        let (pool, stop, done) = (pool.clone(), stop.clone(), done.clone());
        giong::spawn(async move {
            while !stop.get() {
                pool.query_one("SELECT 1", &[]).await.unwrap();
                done.set(done.get() + 1);
            }
        });
    }
    (stop, done)
}

#[test]
fn get_is_served_while_shared_statements_keep_every_connection_busy() {
    run(|s| async move {
        for size in [1, 2] {
            let pool = Pool::new(s.local(), size).wait_timeout(Duration::from_secs(5));
            let (stop, done) = hammer(&pool, 32);
            sleep(Duration::from_millis(100)).await;
            for _ in 0..5 {
                let started = Instant::now();
                let before = done.get();
                let conn = pool.get().await.unwrap();
                let waited = started.elapsed();
                conn.query_one("SELECT 1", &[]).await.unwrap();
                drop(conn);
                // Before the fix every get() here timed out, while hundreds
                // of thousands of shared statements ran.
                assert!(
                    waited < Duration::from_secs(1),
                    "pool of {size}: waited {waited:?}"
                );
                assert!(
                    done.get() > before || size == 1,
                    "shared statements went on"
                );
                sleep(Duration::from_millis(20)).await;
            }
            stop.set(true);
            assert_eq!(pool.stats().timeouts, 0);
        }
    });
}

#[test]
fn shared_statements_keep_a_connection_while_gets_queue_up() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2).wait_timeout(Duration::from_secs(10));
        let (stop, done) = hammer(&pool, 8);
        sleep(Duration::from_millis(50)).await;
        // More tasks want a connection of their own than the pool has, each
        // holding it a while.
        let order = Rc::new(std::cell::RefCell::new(Vec::new()));
        let gets: Vec<_> = (0..4)
            .map(|i| {
                let (pool, order) = (pool.clone(), order.clone());
                giong::spawn(async move {
                    let conn = pool.get().await.unwrap();
                    order.borrow_mut().push(i);
                    conn.query_one("SELECT pg_sleep(0.1)", &[]).await.unwrap();
                })
            })
            .collect();
        sleep(Duration::from_millis(150)).await;
        let before = done.get();
        sleep(Duration::from_millis(100)).await;
        assert!(
            done.get() > before,
            "shared statements stopped while get() calls waited"
        );
        for g in gets {
            g.await.unwrap();
        }
        // Served in the order they came.
        assert_eq!(*order.borrow(), [0, 1, 2, 3]);
        stop.set(true);
    });
}

#[test]
fn a_cancel_token_stops_working_when_its_connection_goes_back() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        let token = {
            let conn = pool.get().await.unwrap();
            let token = conn.cancel_token();
            // While the connection is out, the token works.
            let cancel = giong::spawn({
                let token = token.clone();
                async move {
                    sleep(Duration::from_millis(100)).await;
                    token.cancel().await
                }
            });
            let slow = conn.query("SELECT pg_sleep(5)", &[]).await;
            cancel.await.unwrap().unwrap();
            assert_eq!(slow.unwrap_err().server_code(), Some("57014"));
            token
        };
        // Back in the pool, where another task runs a statement on it.
        let other = giong::spawn({
            let pool = pool.clone();
            async move { pool.query("SELECT pg_sleep(0.3)", &[]).await.map(|_| ()) }
        });
        sleep(Duration::from_millis(100)).await;
        let e = token.cancel().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("gone back to its pool"), "{e}");
        other.await.unwrap().unwrap();
    });
}

#[test]
fn idle_connections_are_closed() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 3).idle_timeout(Duration::from_millis(200));
        // A burst opens all three.
        let held: Vec<_> = (0..3).map(|_| pool.get()).collect();
        let mut conns = Vec::new();
        for get in held {
            conns.push(get.await.unwrap());
        }
        for conn in &conns {
            conn.query_one("SELECT 1", &[]).await.unwrap();
        }
        drop(conns);
        assert_eq!(pool.stats().connections, 3);
        let started = Instant::now();
        while pool.stats().connections > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                pool.stats()
            );
            sleep(Duration::from_millis(20)).await;
        }
        let stats = pool.stats();
        assert_eq!(stats.recycled, stats.opened);
        // And opened again when needed.
        pool.query_one("SELECT 1", &[]).await.unwrap();
        assert_eq!(pool.stats().connections, 1);
    });
}

#[test]
fn old_connections_are_replaced_even_under_load() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2).max_lifetime(Duration::from_millis(200));
        let (stop, done) = hammer(&pool, 16);
        let pid = |pool: Pool| async move {
            let row = pool
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap();
            row.get::<i32>(0).unwrap()
        };
        let first = pid(pool.clone()).await;
        sleep(Duration::from_millis(600)).await;
        let stats = pool.stats();
        assert!(stats.recycled >= 2, "{stats:?}");
        assert!(stats.connections <= 2, "{stats:?}");
        let before = done.get();
        sleep(Duration::from_millis(50)).await;
        assert!(done.get() > before, "statements go on");
        let mut later = Vec::new();
        for _ in 0..10 {
            later.push(pid(pool.clone()).await);
        }
        assert!(!later.contains(&first), "{first} still in use: {later:?}");
        stop.set(true);
        assert_eq!(pool.stats().timeouts, 0);
    });
}

#[test]
fn a_connection_carries_at_most_1024_statements_at_a_time() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2).wait_timeout(Duration::from_secs(20));
        // One connection holds a lock; the statements of 1500 tasks wait for
        // it on the other.
        let holder = pool.get().await.unwrap();
        holder
            .query_one("SELECT pg_advisory_lock(7)", &[])
            .await
            .unwrap();
        let tasks: Vec<_> = (0..1500)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move {
                    pool.query_one("SELECT pg_advisory_xact_lock_shared(7)", &[])
                        .await
                        .map(|_| ())
                })
            })
            .collect();
        let started = Instant::now();
        while pool.stats().waiting < 1500 - 1024 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                pool.stats()
            );
            sleep(Duration::from_millis(10)).await;
        }
        let stats = pool.stats();
        assert!(stats.in_flight <= 1024, "{stats:?}");
        // Back to the pool: its reset releases the lock, and everything runs.
        drop(holder);
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(pool.stats().timeouts, 0);
    });
}

#[test]
fn timeouts_too_long_for_the_clock_mean_no_limit() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1)
            .idle_timeout(Duration::MAX)
            .max_lifetime(Duration::MAX);
        pool.query_one("SELECT 1", &[]).await.unwrap();
        drop(pool.get().await.unwrap());
        pool.query_one("SELECT 1", &[]).await.unwrap();
        assert_eq!(pool.stats().opened, 1);
    });
}

/// Runs `n` copies of `SELECT $1::int4 + 1` at once on `pool`, after one
/// that prepares the statement (only prepared statements join a batch).
async fn add_one_at_once(pool: &Pool, n: i32) {
    pool.query_one("SELECT $1::int4 + 1", &[&0i32])
        .await
        .unwrap();
    let tasks: Vec<_> = (0..n)
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
}

#[test]
fn batched_reads_return_each_statement_its_own_rows() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 2);
        add_one_at_once(&pool, 500).await;
        // Several rows, and none.
        pool.query("SELECT generate_series(1, 3)", &[])
            .await
            .unwrap();
        let tasks: Vec<_> = (0..50i32)
            .map(|i| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let rows = pool
                        .query("SELECT generate_series(1, $1::int4)", &[&(i % 4)])
                        .await
                        .unwrap();
                    (i, rows.len())
                })
            })
            .collect();
        for t in tasks {
            let (i, n) = t.await.unwrap();
            assert_eq!(n, (i % 4) as usize);
        }
        // Statements of one batch share one transaction, so one `now()`.
        pool.query_one("SELECT now()::text", &[]).await.unwrap();
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let row = pool.query_one("SELECT now()::text", &[]).await.unwrap();
                    row.get::<String>(0).unwrap()
                })
            })
            .collect();
        let mut times = Vec::new();
        for t in tasks {
            times.push(t.await.unwrap());
        }
        times.sort();
        times.dedup();
        assert!(
            times.len() < 16,
            "no two statements shared a batch: {times:?}"
        );
        let stats = pool.stats();
        assert_eq!(stats.in_flight, 0, "{stats:?}");
    });
}

#[test]
fn an_error_in_a_batch_fails_only_its_own_statement() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 1);
        pool.query_one("SELECT 1 / $1::int4", &[&1i32])
            .await
            .unwrap();
        for round in 0..20 {
            let tasks: Vec<_> = (0..40i32)
                .map(|i| {
                    let pool = pool.clone();
                    giong::spawn(async move {
                        // Every seventh divides by zero.
                        let d = if (i + round) % 7 == 0 { 0 } else { i + 1 };
                        (d, pool.query_one("SELECT 1 / $1::int4", &[&d]).await)
                    })
                })
                .collect();
            for t in tasks {
                match t.await.unwrap() {
                    (0, r) => {
                        let e = r.unwrap_err();
                        assert_eq!(e.server_code(), Some("22012"), "{e}");
                    }
                    (d, r) => assert_eq!(r.unwrap().get::<i32>(0).unwrap(), 1 / d),
                }
            }
        }
    });
}

#[test]
fn a_select_that_writes_runs_alone_and_its_write_stays() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 1);
        pool.batch_execute(
            "DROP TABLE IF EXISTS batch_writes; CREATE TABLE batch_writes (id int);
             CREATE OR REPLACE FUNCTION batch_write(i int) RETURNS int
             LANGUAGE sql AS 'INSERT INTO batch_writes VALUES (i) RETURNING id'",
        )
        .await
        .unwrap();
        pool.query_one("SELECT $1::int4 + 1", &[&0i32])
            .await
            .unwrap();
        pool.query_one("SELECT batch_write($1)", &[&0i32])
            .await
            .unwrap();
        for round in 1..=5i32 {
            let tasks: Vec<_> = (0..30i32)
                .map(|i| {
                    let pool = pool.clone();
                    giong::spawn(async move {
                        let id = round * 100 + i;
                        if i % 5 == 0 {
                            let row = pool.query_one("SELECT batch_write($1)", &[&id]).await;
                            assert_eq!(row.unwrap().get::<i32>(0).unwrap(), id);
                        } else {
                            let row = pool.query_one("SELECT $1::int4 + 1", &[&id]).await;
                            assert_eq!(row.unwrap().get::<i32>(0).unwrap(), id + 1);
                        }
                    })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        }
        let row = pool
            .query_one("SELECT count(*)::int4 FROM batch_writes", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<i32>(0).unwrap(), 1 + 5 * 6);
        // A locking read is a write too.
        pool.batch_execute("INSERT INTO batch_writes VALUES (-1)")
            .await
            .unwrap();
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move {
                    pool.query_one("SELECT id FROM batch_writes WHERE id = -1 FOR UPDATE", &[])
                        .await
                        .unwrap()
                        .get::<i32>(0)
                        .unwrap()
                })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), -1);
        }
        // A batch's 25006 is not a standby's: the connection stays.
        assert_eq!(pool.stats().opened, 1, "{:?}", pool.stats());
    });
}

#[test]
fn batched_reads_see_writes_made_before_them() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 2);
        pool.batch_execute("DROP TABLE IF EXISTS batch_seen; CREATE TABLE batch_seen (id int)")
            .await
            .unwrap();
        pool.query_one("SELECT count(*)::int4 FROM batch_seen", &[])
            .await
            .unwrap();
        for i in 1..=20i32 {
            pool.execute("INSERT INTO batch_seen VALUES ($1)", &[&i])
                .await
                .unwrap();
            let tasks: Vec<_> = (0..10)
                .map(|_| {
                    let pool = pool.clone();
                    giong::spawn(async move {
                        pool.query_one("SELECT count(*)::int4 FROM batch_seen", &[])
                            .await
                            .unwrap()
                            .get::<i32>(0)
                            .unwrap()
                    })
                })
                .collect();
            for t in tasks {
                assert_eq!(t.await.unwrap(), i);
            }
        }
    });
}

#[test]
fn batches_survive_statements_leaving_the_cache() {
    run(|s| async move {
        let config = s.local().batch_reads(true).statement_cache_capacity(2);
        let pool = Pool::new(config, 1);
        for round in 0..10i32 {
            let tasks: Vec<_> = (0..60i32)
                .map(|i| {
                    let pool = pool.clone();
                    giong::spawn(async move {
                        // Five statements take turns in a cache of two.
                        let sql = format!("SELECT $1::int4 + {}", (i + round) % 5);
                        let row = pool.query_one(&sql, &[&i]).await.unwrap();
                        assert_eq!(row.get::<i32>(0).unwrap(), i + (i + round) % 5);
                    })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        }
        assert_eq!(pool.stats().in_flight, 0);
    });
}

/// Runs `a` and `b` on `pool` from two tasks started in the same tick, so
/// that both are written together: in one batch, with batches on.
async fn together(
    pool: &Pool,
    a: (&'static str, i32),
    b: (&'static str, i32),
) -> [Result<Vec<giong_postgres::Row>, giong_postgres::Error>; 2] {
    let run = |(sql, arg): (&'static str, i32)| {
        let pool = pool.clone();
        giong::spawn(async move { pool.query(sql, &[&arg]).await })
    };
    let (ta, tb) = (run(a), run(b));
    [ta.await.unwrap(), tb.await.unwrap()]
}

#[test]
fn an_error_at_a_batch_commit_fails_only_its_statement() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 1);
        pool.batch_execute(
            "CREATE OR REPLACE FUNCTION batch_commit_setup() RETURNS int LANGUAGE plpgsql AS $$
             BEGIN
               CREATE TEMP TABLE IF NOT EXISTS batch_commit
                 (x int UNIQUE DEFERRABLE INITIALLY DEFERRED);
               RETURN 0;
             END $$;
             CREATE OR REPLACE FUNCTION batch_commit_dup(i int) RETURNS int LANGUAGE plpgsql AS
               'BEGIN INSERT INTO batch_commit VALUES (i), (i); RETURN i; END'",
        )
        .await
        .unwrap();
        // Not batched (`execute`), and on the pool's one connection, whose
        // session the temporary table belongs to.
        pool.execute("SELECT batch_commit_setup()", &[])
            .await
            .unwrap();
        for round in 0..20 {
            let tasks: Vec<_> = (0..20i32)
                .map(|i| {
                    let pool = pool.clone();
                    giong::spawn(async move {
                        if i == round % 20 {
                            // Its two rows break the deferred constraint at
                            // the commit, after the statement ran.
                            let e = pool
                                .query("SELECT batch_commit_dup($1)", &[&i])
                                .await
                                .unwrap_err();
                            assert_eq!(e.server_code(), Some("23505"), "{e}");
                        } else {
                            let row = pool.query_one("SELECT $1::int4 + 1", &[&i]).await;
                            assert_eq!(row.unwrap().get::<i32>(0).unwrap(), i + 1);
                        }
                    })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        }
        let row = pool
            .query_one("SELECT $1::int4 + 1", &[&41i32])
            .await
            .unwrap();
        assert_eq!(row.get::<i32>(0).unwrap(), 42);
        let stats = pool.stats();
        assert_eq!((stats.in_flight, stats.opened), (0, 1), "{stats:?}");
    });
}

#[test]
fn statements_before_a_failure_in_their_batch_run_again() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 1);
        pool.batch_execute(
            "CREATE OR REPLACE FUNCTION batch_again_setup() RETURNS int LANGUAGE plpgsql AS $$
             BEGIN
               CREATE TEMP TABLE IF NOT EXISTS batch_again (x int);
               RETURN 0;
             END $$;
             CREATE OR REPLACE FUNCTION batch_again_add(i int) RETURNS int LANGUAGE plpgsql AS
               'BEGIN INSERT INTO batch_again VALUES (i); RETURN i; END'",
        )
        .await
        .unwrap();
        pool.execute("SELECT batch_again_setup()", &[])
            .await
            .unwrap();
        // Temporary tables can be written in a read-only transaction, so
        // the insert runs in the batch, which the division by zero after it
        // rolls back.
        for i in 0..10 {
            let [added, failed] = together(
                &pool,
                ("SELECT batch_again_add($1)", i),
                ("SELECT 1 / $1::int4", 0),
            )
            .await;
            assert_eq!(added.unwrap()[0].get::<i32>(0).unwrap(), i);
            assert_eq!(failed.unwrap_err().server_code(), Some("22012"));
        }
        let row = pool
            .query_one("SELECT count(*)::int4 FROM batch_again", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<i32>(0).unwrap(), 10, "each insert stayed, once");
    });
}

#[test]
fn transaction_local_settings_stay_with_their_statement_unless_hidden() {
    run(|s| async move {
        // A setting made with `set_config(.., true)` lasts until the end of
        // the transaction. A statement that calls it runs alone, so the
        // setting reaches no other statement; a call inside a function is
        // not seen and reaches the rest of its batch, as `Config::batch_reads`
        // warns.
        const SET: &str = "SELECT set_config('giong.tenant', $1::int4::text, true)";
        const HIDDEN: &str = "SELECT giong_set_tenant($1::int4)";
        const GET: &str =
            "SELECT coalesce(current_setting('giong.tenant', true), '') || $1::int4::text";
        for batch in [false, true] {
            let pool = Pool::new(s.local().batch_reads(batch), 1);
            pool.get()
                .await
                .unwrap()
                .batch_execute(
                    "CREATE OR REPLACE FUNCTION giong_set_tenant(i int4) RETURNS text \
                     LANGUAGE plpgsql AS $$ BEGIN \
                     RETURN set_config('giong.tenant', i::text, true); END $$",
                )
                .await
                .unwrap();
            // Prepared, so that each may join a batch.
            for sql in [SET, HIDDEN, GET] {
                pool.query(sql, &[&0i32]).await.unwrap();
            }
            for i in 1..=5 {
                let [_, seen] = together(&pool, (SET, i), (GET, 0)).await;
                let seen: String = seen.unwrap()[0].get::<&str>(0).unwrap().to_owned();
                assert_eq!(seen, "0", "no setting from another statement");

                let [_, seen] = together(&pool, (HIDDEN, i), (GET, 0)).await;
                let seen: String = seen.unwrap()[0].get::<&str>(0).unwrap().to_owned();
                if batch {
                    assert_eq!(seen, format!("{i}0"), "the hidden call's setting");
                } else {
                    assert_eq!(seen, "0", "no setting from another statement");
                }
            }
        }
    });
}

#[test]
fn the_read_only_guard_is_prepared_again_after_a_deallocate() {
    run(|s| async move {
        let pool = Pool::new(s.local().batch_reads(true), 1);
        const NOW: &str = "SELECT now()::text || $1::int4::text";
        pool.query(NOW, &[&0i32]).await.unwrap();
        let shared = |[a, b]: [Result<Vec<giong_postgres::Row>, giong_postgres::Error>; 2]| {
            let a: String = a.unwrap()[0].get::<&str>(0).unwrap().to_owned();
            let b: String = b.unwrap()[0].get::<&str>(0).unwrap().to_owned();
            a[..a.len() - 1] == b[..b.len() - 1]
        };
        assert!(
            shared(together(&pool, (NOW, 1), (NOW, 2)).await),
            "one batch"
        );
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("DEALLOCATE \"__giong_read_only\"")
                .await
                .unwrap();
        }
        // The guard fails (26000): both are sent again alone.
        let [a, b] = together(&pool, (NOW, 1), (NOW, 2)).await;
        a.unwrap();
        b.unwrap();
        // Then the guard is prepared again, and batches work as before.
        assert!(
            shared(together(&pool, (NOW, 1), (NOW, 2)).await),
            "one batch again"
        );
        assert_eq!(pool.stats().opened, 1);
    });
}

/// Statements of a batch hold the locks they take until the batch ends, so
/// two batches taking two locks in opposite orders could deadlock where the
/// same statements one by one never do: a statement of a batch gives up a
/// lock wait within 50 ms and runs again alone.
#[test]
fn batches_taking_locks_in_opposite_orders_do_not_deadlock() {
    for batch in [false, true] {
        run(move |s| async move {
            let a = Pool::new(s.local().batch_reads(batch), 1);
            let b = Pool::new(s.local().batch_reads(batch), 1);
            let slow = "SELECT 1::int4 FROM pg_advisory_xact_lock($1::int8), pg_sleep(0.3)";
            let fast = "SELECT 2::int4 FROM pg_advisory_xact_lock($1::int8)";
            for pool in [&a, &b] {
                pool.query_one(slow, &[&100i64]).await.unwrap();
                pool.query_one(fast, &[&100i64]).await.unwrap();
            }
            let mut tasks = Vec::new();
            for (pool, first, second) in [(&a, 1i64, 2i64), (&b, 2, 1)] {
                let p = pool.clone();
                tasks.push(giong::spawn(
                    async move { p.query_one(slow, &[&first]).await },
                ));
                let p = pool.clone();
                tasks.push(giong::spawn(
                    async move { p.query_one(fast, &[&second]).await },
                ));
            }
            for t in tasks {
                if let Err(e) = t.await.unwrap() {
                    panic!("batch_reads({batch}): {e}");
                }
            }
        });
    }
}

/// A migration that locks two tables while a batch reads both: without the
/// cap on the batch's lock waits, the two deadlock and the server rolls the
/// migration back.
#[test]
fn a_migration_locking_two_tables_is_not_rolled_back_by_a_batch() {
    for batch in [false, true] {
        run(move |s| async move {
            let pool = Pool::new(s.local().batch_reads(batch), 2);
            pool.batch_execute(
                "DROP TABLE IF EXISTS ta, tb; CREATE TABLE ta (x int); CREATE TABLE tb (x int)",
            )
            .await
            .unwrap();
            let s1 = "SELECT count(*)::int4 FROM ta, pg_sleep(0.3)";
            let s2 = "SELECT count(*)::int4 FROM tb";
            let reads = Pool::new(s.local().batch_reads(batch), 1);
            reads.query_one(s1, &[]).await.unwrap();
            reads.query_one(s2, &[]).await.unwrap();
            let r = reads.clone();
            let t1 = giong::spawn(async move { r.query_one(s1, &[]).await.map(|_| ()) });
            let r = reads.clone();
            let t2 = giong::spawn(async move { r.query_one(s2, &[]).await.map(|_| ()) });
            let migrate = giong::spawn({
                let pool = pool.clone();
                async move {
                    giong::time::sleep(std::time::Duration::from_millis(100)).await;
                    let c = pool.get().await.unwrap();
                    c.batch_execute("BEGIN; LOCK tb; SELECT pg_sleep(0.05); LOCK ta; COMMIT")
                        .await
                }
            });
            t1.await.unwrap().unwrap();
            t2.await.unwrap().unwrap();
            if let Err(e) = migrate.await.unwrap() {
                panic!("batch_reads({batch}): the migration failed: {e}");
            }
        });
    }
}

/// A local port nothing listens on.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[test]
fn tasks_waiting_while_the_server_is_down_share_one_connect_attempt() {
    run(|s| async move {
        let port = closed_port();
        let pool = Pool::new(s.local().host("127.0.0.1").port(port), 4);
        let calls = Rc::new(Cell::new(0u64));
        let end = Instant::now() + Duration::from_millis(300);
        let tasks: Vec<_> = (0..64)
            .map(|_| {
                let (pool, calls) = (pool.clone(), calls.clone());
                giong::spawn(async move {
                    while Instant::now() < end {
                        let e = pool.query("SELECT 1", &[]).await.unwrap_err();
                        // The server's own refusal, never a pool timeout.
                        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
                        calls.set(calls.get() + 1);
                        sleep(Duration::from_millis(1)).await;
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        let attempts = pool.stats().connect_errors;
        let calls = calls.get();
        // Each task used to make an attempt of its own, on every call.
        assert!(
            attempts * 8 <= calls,
            "{attempts} connect attempts for {calls} calls"
        );
        // And it comes back at once with the server.
        let pool = Pool::new(s.local(), 4);
        pool.query("SELECT 1", &[]).await.unwrap();
    });
}
