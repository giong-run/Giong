//! The per-worker pool.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use giong::time::{sleep, timeout, Instant};
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
        assert_eq!(id, Some(5));
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
        assert_eq!(e.server_code(), Some(1049), "{e}");
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

#[test]
fn transactions_never_run_on_shared_connections() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_leak; CREATE TABLE pool_leak (id INT)")
            .await
            .unwrap();
        // Refused before anything is sent.
        for sql in [
            "BEGIN",
            "  /* note */ start transaction read only",
            "# note\nSTART TRANSACTION",
            "-- note\ncommit",
            "ROLLBACK",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
            "XA START 'x'",
            "LOCK TABLES pool_leak WRITE",
            "unlock tables",
        ] {
            let e = pool.execute(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
            assert!(e.to_string().contains("pool.get()"), "{e}");
            let e = pool.query(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
        }
        pool.execute("SELECT 1 AS `begin`", &[]).await.unwrap();

        // A whole transaction in one batch is fine.
        pool.batch_execute("START TRANSACTION; INSERT INTO pool_leak VALUES (1); COMMIT")
            .await
            .unwrap();

        // A batch that leaves one open closes its connection: the other
        // task's insert, pipelined behind it on the only connection, would
        // otherwise have run inside it and never been committed.
        let leak = giong::spawn({
            let pool = pool.clone();
            async move {
                pool.batch_execute("START TRANSACTION; INSERT INTO pool_leak VALUES (2)")
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

        // So does turning autocommit off.
        let e = pool.batch_execute("SET autocommit = 0").await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");

        // Only the committed row is there; the pool goes on with new
        // connections.
        let n: i64 = pool
            .query_one("SELECT COUNT(*) FROM pool_leak", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(pool.stats().opened, 3);
    });
}

#[test]
fn a_connection_given_back_with_begin_in_flight_is_rolled_back() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_late; CREATE TABLE pool_late (id INT)")
            .await
            .unwrap();
        {
            let conn = pool.get().await.unwrap();
            // The caller gives up before BEGIN is answered: the last answer
            // the connection has says no transaction is open.
            let late = timeout(
                Duration::from_millis(20),
                conn.batch_execute("DO SLEEP(0.2); BEGIN"),
            )
            .await;
            assert!(late.is_err());
        }
        pool.execute("INSERT INTO pool_late VALUES (1)", &[])
            .await
            .unwrap();
        // Committed: another session sees it.
        assert_eq!(s.sql("SELECT id FROM giong.pool_late").trim(), "1");
    });
}

#[test]
fn a_connection_given_back_with_autocommit_off_is_reset() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_ac; CREATE TABLE pool_ac (id INT)")
            .await
            .unwrap();
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("SET autocommit = 0").await.unwrap();
        }
        pool.execute("INSERT INTO pool_ac VALUES (1)", &[])
            .await
            .unwrap();
        // Committed: autocommit is on again, on the same connection.
        assert_eq!(s.sql("SELECT id FROM giong.pool_ac").trim(), "1");
        assert_eq!(pool.stats().opened, 1);
        assert_eq!(pool.stats().resets, 1);
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
            pool.execute("DO SLEEP(0.3)", &[]),
        )
        .await;
        assert!(dropped.is_err());
        // A task of its own, polled only when woken.
        let waiter = giong::spawn({
            let pool = pool.clone();
            async move {
                let started = Instant::now();
                let conn = pool.get().await?;
                Ok::<_, giong_mysql::Error>((started.elapsed(), conn.in_flight()))
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
fn a_taken_connection_comes_back_with_a_clean_session() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_clean; CREATE TABLE pool_clean (x INT)")
            .await
            .unwrap();
        let session = "SELECT @@sql_mode, @@time_zone, @@foreign_key_checks, DATABASE(), \
                       @secret, @selected";
        let before = pool.query_one(session, &[]).await.unwrap();
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute(
                "SET SESSION sql_mode = ''; SET time_zone = '+07:00'; \
                 SET foreign_key_checks = 0; SET @secret = 'user-a-token'; \
                 CREATE TEMPORARY TABLE pool_clean (y INT); USE mysql",
            )
            .await
            .unwrap();
            conn.query("SELECT 1 INTO @selected", &[]).await.unwrap();
        }
        let after = pool.query_one(session, &[]).await.unwrap();
        assert_eq!(
            after.get::<String>(0).unwrap(),
            before.get::<String>(0).unwrap()
        );
        assert_eq!(after.get::<&str>(1).unwrap(), "+00:00");
        assert_eq!(after.get::<i64>(2).unwrap(), 1);
        assert_eq!(after.get::<&str>(3).unwrap(), "giong");
        assert_eq!(
            after.get::<Option<String>>(4).unwrap(),
            None,
            "no user variable"
        );
        assert_eq!(after.get::<Option<i64>>(5).unwrap(), None);
        // The real table, not the temporary one that shadowed it.
        pool.execute("INSERT INTO pool_clean (x) VALUES (1)", &[])
            .await
            .unwrap();
        // Timestamps read right: the time zone is UTC again. (Not the epoch
        // itself, which MariaDB 11.8 has no value for: it answers NULL.)
        let day_two: std::time::SystemTime = pool
            .query_one("SELECT FROM_UNIXTIME(86400)", &[])
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(
            day_two,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(86_400)
        );
        let stats = pool.stats();
        assert_eq!((stats.opened, stats.resets), (1, 1), "{stats:?}");
    });
}

#[test]
fn locks_taken_on_a_taken_connection_are_released_when_it_comes_back() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute(
            "DROP TABLE IF EXISTS pool_lt1, pool_lt2; CREATE TABLE pool_lt1 (x INT); \
             CREATE TABLE pool_lt2 (x INT)",
        )
        .await
        .unwrap();
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("LOCK TABLES pool_lt1 WRITE")
                .await
                .unwrap();
            let got: i64 = conn
                .query_one("SELECT GET_LOCK('giong-pool-test', 0)", &[])
                .await
                .unwrap()
                .get(0)
                .unwrap();
            assert_eq!(got, 1);
        }
        // Not "Table 'pool_lt2' was not locked with LOCK TABLES" (1100).
        pool.query("SELECT * FROM pool_lt2", &[]).await.unwrap();
        // Another session gets the named lock and the table at once.
        assert_eq!(
            s.sql("SELECT GET_LOCK('giong-pool-test', 0)").trim(),
            "1",
            "the named lock was released"
        );
        s.sql("SET SESSION lock_wait_timeout = 5; INSERT INTO giong.pool_lt1 VALUES (1)");
    });
}

#[test]
fn a_connection_that_only_ran_plain_statements_keeps_its_prepared_statements() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("DROP TABLE IF EXISTS pool_keep; CREATE TABLE pool_keep (x INT)")
            .await
            .unwrap();
        let resets = pool.stats().resets;
        for i in 0..3i64 {
            let mut conn = pool.get().await.unwrap();
            conn.query("SELECT x FROM pool_keep WHERE x = ?", &[&i])
                .await
                .unwrap();
            let tx = conn.transaction().await.unwrap();
            tx.execute("INSERT INTO pool_keep VALUES (?)", &[&i])
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(pool.stats().resets, resets, "no reset was needed");
        // So the statements were prepared once each.
        let prepares: i64 = pool
            .query_one(
                "SELECT COUNT(*) FROM performance_schema.prepared_statements_instances \
                 WHERE SQL_TEXT LIKE '%pool_keep%' AND SQL_TEXT NOT LIKE '%performance_schema%'",
                &[],
            )
            .await
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(prepares, 2);
    });
}

#[test]
fn statements_that_change_the_session_are_refused_on_shared_connections() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        for sql in [
            "SET sql_mode = ''",
            "set @@session.time_zone = '+07:00'",
            "SET @token = 'a'",
            "SET NAMES latin1",
            "USE mysql",
            "CREATE TEMPORARY TABLE pool_tmp (x INT)",
            "SELECT GET_LOCK('x', 0)",
            "SELECT 1 INTO @v",
            "SELECT @v := 1",
            "PREPARE p FROM 'SELECT 1'",
            "HANDLER pool_tmp OPEN",
            // Versioned comments, as dump tools write them, run as SQL.
            "/*!40103 SET time_zone = '+00:00' */",
            "/*!40101 SET NAMES latin1 */",
        ] {
            let e = pool.execute(sql, &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Statement, "{sql:?}: {e}");
            assert!(e.to_string().contains("pool.get()"), "{e}");
            assert!(e.to_string().contains("session_variable"), "{e}");
        }
        let e = pool
            .execute("/*!40000 LOCK TABLES t WRITE */", &[])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("LOCK controls a transaction"), "{e}");
        // Reading is fine, and so is changing the server for everyone.
        pool.query("SELECT @@sql_mode, @v, @@global.max_connections", &[])
            .await
            .unwrap();
        pool.execute("SET GLOBAL max_connections = 300", &[])
            .await
            .unwrap();
        assert_eq!(pool.stats().opened, 1);
    });
}

#[test]
fn a_shared_batch_that_changes_the_session_is_followed_by_a_reset() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        pool.batch_execute("SET @batch = 1; SET time_zone = '+07:00'; DO 1")
            .await
            .unwrap();
        let row = pool
            .query_one("SELECT @batch, @@time_zone", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<Option<i64>>(0).unwrap(), None);
        assert_eq!(row.get::<&str>(1).unwrap(), "+00:00");
        // A batch of plain statements needs no reset.
        pool.batch_execute("DO 1; SELECT 1").await.unwrap();
        let stats = pool.stats();
        assert_eq!((stats.opened, stats.resets), (1, 1), "{stats:?}");
    });
}

#[test]
fn a_shared_statement_the_server_says_changed_the_session_is_followed_by_a_reset() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        // A trigger changes the session behind a plain INSERT; the server
        // says so, and the pool resets the connection.
        pool.batch_execute(
            "DROP TABLE IF EXISTS pool_trig; CREATE TABLE pool_trig (x INT); \
             CREATE TRIGGER pool_trig_set BEFORE INSERT ON pool_trig \
             FOR EACH ROW SET @from_trigger = NEW.x",
        )
        .await
        .unwrap();
        let resets = pool.stats().resets;
        pool.execute("INSERT INTO pool_trig VALUES (7)", &[])
            .await
            .unwrap();
        let row = pool.query_one("SELECT @from_trigger", &[]).await.unwrap();
        assert_eq!(row.get::<Option<i64>>(0).unwrap(), None);
        assert_eq!(pool.stats().resets, resets + 1);
    });
}

#[test]
fn a_connection_whose_reset_fails_is_closed_rather_than_shared() {
    run(|s| async move {
        s.sql(
            "DROP TABLE IF EXISTS giong.pool_conf; CREATE TABLE giong.pool_conf (n INT); \
             INSERT INTO giong.pool_conf VALUES (4096)",
        );
        // The session SQL reads a table, so setting the session up again
        // fails once the table is gone.
        let pool = Pool::new(
            s.local()
                .session_variable("group_concat_max_len", "(SELECT n FROM pool_conf)"),
            1,
        );
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("SET @dirty = 1").await.unwrap();
            s.sql("DROP TABLE giong.pool_conf");
        }
        let e = pool.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("resetting the session"), "{e}");
        s.sql("CREATE TABLE giong.pool_conf (n INT); INSERT INTO giong.pool_conf VALUES (4096)");
        let row = pool
            .query_one("SELECT @dirty, @@group_concat_max_len", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<Option<i64>>(0).unwrap(), None);
        assert_eq!(row.get::<u64>(1).unwrap(), 4096);
        assert_eq!(pool.stats().opened, 2);
        s.sql("DROP TABLE giong.pool_conf");
    });
}

#[test]
fn get_is_not_starved_by_shared_statements() {
    run(|s| async move {
        for max_size in [1, 2] {
            let pool = Pool::new(s.local(), max_size).wait_timeout(Duration::from_secs(5));
            let stop = std::rc::Rc::new(std::cell::Cell::new(false));
            let load: Vec<_> = (0..32)
                .map(|_| {
                    let (pool, stop) = (pool.clone(), stop.clone());
                    giong::spawn(async move {
                        let mut done = 0u64;
                        while !stop.get() {
                            pool.execute("DO SLEEP(0.002)", &[]).await.unwrap();
                            done += 1;
                        }
                        done
                    })
                })
                .collect();
            sleep(Duration::from_millis(100)).await;
            for _ in 0..3 {
                let started = Instant::now();
                let conn = pool.get().await.unwrap();
                let waited = started.elapsed();
                assert!(
                    waited < Duration::from_secs(2),
                    "pool of {max_size}: get() waited {waited:?}"
                );
                conn.query_one("SELECT 1", &[]).await.unwrap();
                drop(conn);
                sleep(Duration::from_millis(20)).await;
            }
            stop.set(true);
            let mut done = 0;
            for t in load {
                done += t.await.unwrap();
            }
            assert!(done > 32, "the shared statements went on too: {done}");
            let stats = pool.stats();
            assert_eq!(
                (stats.draining, stats.waiting, stats.taken),
                (0, 0, 0),
                "{stats:?}"
            );
            assert_eq!(stats.timeouts, 0);
        }
    });
}

#[test]
fn tasks_waiting_in_get_are_served_in_order() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1);
        let held = pool.get().await.unwrap();
        let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let waiters: Vec<_> = (0..4)
            .map(|i| {
                let (pool, order) = (pool.clone(), order.clone());
                let task = giong::spawn(async move {
                    let conn = pool.get().await.unwrap();
                    order.borrow_mut().push(i);
                    sleep(Duration::from_millis(5)).await;
                    drop(conn);
                });
                (i, task)
            })
            .collect();
        // Each in line before the next arrives.
        for _ in 0..4 {
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(pool.stats().waiting, 4);
        drop(held);
        for (_, task) in waiters {
            task.await.unwrap();
        }
        assert_eq!(*order.borrow(), [0, 1, 2, 3]);
    });
}

#[test]
fn a_connection_set_aside_for_a_task_that_gave_up_goes_back_to_the_shared_ones() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 2).wait_timeout(Duration::from_millis(200));
        let busy: Vec<_> = (0..2)
            .map(|_| {
                let pool = pool.clone();
                giong::spawn(async move { pool.execute("DO SLEEP(1)", &[]).await })
            })
            .collect();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.stats().connections, 2);
        let waiter = giong::spawn({
            let pool = pool.clone();
            async move { pool.get().await.map(drop) }
        });
        sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.stats().draining, 1);
        let e = waiter.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PoolTimeout, "{e}");
        assert_eq!(pool.stats().draining, 0);
        for t in busy {
            t.await.unwrap().unwrap();
        }
        // Both connections take shared statements again.
        let stats = pool.stats();
        assert_eq!((stats.connections, stats.draining), (2, 0), "{stats:?}");
    });
}

#[test]
fn without_a_database_a_connection_that_changed_it_is_closed() {
    run(|s| async move {
        let config = giong_mysql::Config::new()
            .host(s.socket.to_str().unwrap())
            .user("root");
        let pool = Pool::new(config, 1);
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("USE mysql").await.unwrap();
        }
        // A reset keeps the database, and there is none to go back to.
        let row = pool.query_one("SELECT DATABASE()", &[]).await.unwrap();
        assert_eq!(row.get::<Option<String>>(0).unwrap(), None);
        assert_eq!(pool.stats().opened, 2);
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
        let stop = Rc::new(Cell::new(false));
        let done = Rc::new(Cell::new(0u64));
        for _ in 0..16 {
            let (pool, stop, done) = (pool.clone(), stop.clone(), done.clone());
            giong::spawn(async move {
                while !stop.get() {
                    pool.query_one("SELECT 1", &[]).await.unwrap();
                    done.set(done.get() + 1);
                }
            });
        }
        let id = |pool: Pool| async move {
            let row = pool.query_one("SELECT CONNECTION_ID()", &[]).await.unwrap();
            row.get::<u64>(0).unwrap()
        };
        let first = id(pool.clone()).await;
        sleep(Duration::from_millis(600)).await;
        let stats = pool.stats();
        assert!(stats.recycled >= 2, "{stats:?}");
        assert!(stats.connections <= 2, "{stats:?}");
        let before = done.get();
        sleep(Duration::from_millis(50)).await;
        assert!(done.get() > before, "statements go on");
        let mut later = Vec::new();
        for _ in 0..10 {
            later.push(id(pool.clone()).await);
        }
        assert!(!later.contains(&first), "{first} still in use: {later:?}");
        stop.set(true);
        assert_eq!(pool.stats().timeouts, 0);
    });
}

#[test]
fn a_taken_connection_past_its_lifetime_is_closed_when_it_comes_back() {
    run(|s| async move {
        let pool = Pool::new(s.local(), 1).max_lifetime(Duration::from_millis(100));
        let conn = pool.get().await.unwrap();
        let first = conn.query_one("SELECT CONNECTION_ID()", &[]).await.unwrap();
        let first: u64 = first.get(0).unwrap();
        sleep(Duration::from_millis(150)).await;
        // Still usable while taken: the lifetime never cuts a task off.
        conn.query_one("SELECT 1", &[]).await.unwrap();
        drop(conn);
        let stats = pool.stats();
        assert_eq!((stats.connections, stats.recycled), (0, 1), "{stats:?}");
        let row = pool.query_one("SELECT CONNECTION_ID()", &[]).await.unwrap();
        assert_ne!(row.get::<u64>(0).unwrap(), first);
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
