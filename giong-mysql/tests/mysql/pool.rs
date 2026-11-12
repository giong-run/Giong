//! The per-worker pool.

use std::time::Duration;

use giong_mysql::{ErrorKind, Pool};

use crate::run;

#[test]
fn shared_statements_spread_over_connections_up_to_the_maximum() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 3);
        assert_eq!(pool.stats().connections, 0, "connections open on demand");
        let tasks: Vec<_> = (0..100i64)
            .map(|i| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let row = pool.query_one("SELECT ? + 1", &[&i]).await.unwrap();
                    row.get::<i64>(0).unwrap()
                })
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), i as i64 + 1);
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
        pool.batch_execute("DROP TABLE IF EXISTS pool_tx; CREATE TABLE pool_tx (id INT)")
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
            .query_one("SELECT COUNT(*) FROM pool_tx", &[])
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
        let id = pool
            .insert("INSERT INTO pool_tx VALUES (LAST_INSERT_ID(5))", &[])
            .await
            .unwrap();
        assert_eq!(id, 5);
        // Another session sees that row, and not the one rolled back: the
        // shared connection is not in a transaction.
        assert_eq!(s.sql("SELECT id FROM giong.pool_tx").trim(), "5");
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
        let id: u64 = pool
            .query_one("SELECT CONNECTION_ID()", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        s.sql(&format!("KILL {id}"));
        for _ in 0..100 {
            if pool.stats().connections == 0 {
                break;
            }
            // Looking at the pool drops connections that have closed.
            let _ = pool.get().await.map(drop);
            giong::time::sleep(Duration::from_millis(10)).await;
        }
        let other: u64 = pool
            .query_one("SELECT CONNECTION_ID()", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_ne!(other, id);
    });
}

#[test]
fn connect_errors_are_reported_and_counted() {
    run(|s| async move {
        let pool = Pool::new(s.local().database("missing_db"), 2);
        let e = pool.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.code(), Some(1049), "{e}");
        assert_eq!(pool.stats().connect_errors, 1);
        assert_eq!(pool.stats().connections, 0);
    });
}

#[test]
fn a_slow_statement_does_not_hold_up_the_ones_behind_it() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2);
        // Open both connections.
        let warm: Vec<_> = (0..2)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move { pool.execute("SELECT SLEEP(0.05)", &[]).await })
            })
            .collect();
        for t in warm {
            t.await.unwrap().unwrap();
        }
        assert_eq!(pool.stats().connections, 2);

        let slow = giong::spawn({
            let pool = pool.clone();
            async move { pool.execute("SELECT SLEEP(2)", &[]).await }
        });
        giong::time::sleep(Duration::from_millis(50)).await;
        // Many at once, so the connection with the slow statement is the
        // least busy by count.
        let started = giong::time::Instant::now();
        let quick: Vec<_> = (0..20i64)
            .map(|i| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let row = pool.query_one("SELECT ?", &[&i]).await.unwrap();
                    assert_eq!(row.get::<i64>(0).unwrap(), i);
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
        slow.await.unwrap().unwrap();
    });
}
