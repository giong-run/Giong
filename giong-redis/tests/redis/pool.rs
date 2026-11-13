use std::future::{poll_fn, Future};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use giong_redis::{cmd, Client, ErrorKind, Pipeline, Pool, Value};

use crate::{key, run};

#[test]
fn many_tasks_share_a_few_connections() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 2);
        let prefix = key("pool");
        let mut tasks = Vec::new();
        for t in 0..100i64 {
            let pool = pool.clone();
            let k = format!("{prefix}:{t}");
            tasks.push(giong::spawn(async move {
                for i in 1..=20 {
                    assert_eq!(pool.incr(&k, t).await.unwrap(), i * t);
                }
                let (a, b): (i64, i64) = pool
                    .pipeline(
                        &Pipeline::new()
                            .add(cmd("GET").arg(&k))
                            .add(cmd("STRLEN").arg(&k)),
                    )
                    .await
                    .unwrap();
                assert_eq!(a, 20 * t);
                assert_eq!(b, (20 * t).to_string().len() as i64);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let stats = pool.stats();
        assert!(stats.connections <= 2, "{stats:?}");
        assert_eq!(stats.opened as usize, stats.connections);
        assert_eq!(stats.in_flight, 0);
        // Refused before any connection is used.
        let e = pool
            .query::<Value>(&cmd("BLPOP").arg("q").arg(1))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Command);
    });
}

#[test]
fn a_taken_connection_is_the_task_s_alone_and_comes_back() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1).wait_timeout(Duration::from_millis(200));
        let k = key("taken");
        {
            let mut conn = pool.take().await.unwrap();
            assert_eq!(pool.stats().taken, 1);
            // While it is out, nothing else can be had.
            let e = pool.ping().await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::PoolTimeout);
            assert!(e.to_string().contains("all 1 are taken"), "{e}");
            assert_eq!(pool.stats().timeouts, 1);

            let watch = conn.watch(&[&k]).await.unwrap();
            let ran: Option<((),)> = watch
                .exec(&Pipeline::new().add(cmd("SET").arg(&k).arg(1)))
                .await
                .unwrap();
            assert!(ran.is_some());
            let none: Option<Value> = conn
                .query_blocking(&cmd("BLPOP").arg(key("q")).arg(0.05))
                .await
                .unwrap();
            assert_eq!(none, None);
        }
        // Back in the pool, and reused.
        assert_eq!(pool.stats().taken, 0);
        assert_eq!(pool.get::<i64>(&k).await.unwrap(), 1);
        assert_eq!(pool.stats().opened, 1);
    });
}

#[test]
fn a_connection_left_in_a_cancelled_blocking_command_is_closed_not_shared() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1);
        let q = key("cancelled");
        {
            let mut conn = pool.take().await.unwrap();
            let cut = giong::time::timeout(
                Duration::from_millis(50),
                conn.query_blocking::<Value>(&cmd("BLPOP").arg(&q).arg(30)),
            )
            .await;
            assert!(cut.is_err());
        }
        // A new connection answers at once instead of waiting 30 s behind
        // the BLPOP.
        let start = std::time::Instant::now();
        pool.ping().await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(pool.stats().opened, 2);
    });
}

#[test]
fn a_watch_cancelled_before_its_reply_does_not_outlive_it() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1);
        let other = Client::connect(&server.tcp()).await.unwrap();
        let k = key("cancelled-watch");
        other.set(&k, 1).await.unwrap();
        {
            let mut conn = pool.take().await.unwrap();
            let keys = [&k];
            let mut watch = pin!(conn.watch(&keys));
            // WATCH is queued, then the future is dropped before its reply.
            let pending = poll_fn(|cx| Poll::Ready(watch.as_mut().poll(cx).is_pending())).await;
            assert!(pending);
        }
        // Answered after WATCH, on the same connection, now shared.
        pool.ping().await.unwrap();
        other.set(&k, 10).await.unwrap();
        // Another task's transaction on that connection runs.
        let (v,): (i64,) = pool
            .pipeline(&Pipeline::new().add(cmd("INCR").arg(&k)).atomic())
            .await
            .unwrap();
        assert_eq!(v, 11);
        assert_eq!(pool.stats().opened, 1);
    });
}

#[test]
fn a_task_waiting_to_take_hears_when_a_dropped_command_frees_the_connection() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1).wait_timeout(Duration::from_secs(3));
        pool.ping().await.unwrap();
        let waiter = {
            let mut ping = pin!(pool.ping());
            // Sent on the only connection, which is now in use.
            let pending = poll_fn(|cx| Poll::Ready(ping.as_mut().poll(cx).is_pending())).await;
            assert!(pending);
            let waiter = giong::spawn({
                let pool = pool.clone();
                async move {
                    let start = std::time::Instant::now();
                    pool.take().await.map(|_| start.elapsed())
                }
            });
            giong::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(pool.stats().waiting, 1);
            waiter
            // The PING is dropped here, as by a timeout around it: the
            // waiting task looks again.
        };
        let waited = waiter.await.unwrap().unwrap();
        assert!(waited < Duration::from_secs(1), "{waited:?}");
    });
}

#[test]
fn connections_the_server_closed_are_replaced() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1);
        pool.ping().await.unwrap();
        let id: i64 = pool.query(&cmd("CLIENT").arg("ID")).await.unwrap();
        let admin = Client::connect(&server.tcp()).await.unwrap();
        let _: i64 = admin
            .query(&cmd("CLIENT").arg("KILL").arg("ID").arg(id))
            .await
            .unwrap();
        // Noticed as it happens, so the next command gets a new connection.
        giong::time::sleep(Duration::from_millis(50)).await;
        pool.ping().await.unwrap();
        let new_id: i64 = pool.query(&cmd("CLIENT").arg("ID")).await.unwrap();
        assert_ne!(new_id, id);
        assert_eq!(pool.stats().opened, 2);
        let shared = Rc::new(pool);
        assert_eq!(shared.stats().connections, 1);
    });
}

#[test]
fn a_pool_that_cannot_connect_says_why() {
    run(|_server| async move {
        let port = crate::server::free_port();
        let pool = Pool::new(giong_redis::Config::new().host("127.0.0.1").port(port), 2);
        let e = pool.ping().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert_eq!(pool.stats().connect_errors, 1);
        assert_eq!(pool.stats().connections, 0);
    });
}
