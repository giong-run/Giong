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

#[test]
fn take_gets_a_connection_while_shared_commands_keep_the_pool_full() {
    // Before connections were set aside for waiting takers, take() waited
    // for a moment with no shared command in flight, which never came
    // under load: PoolTimeout after the whole wait with max_size 1 and 2,
    // while about a million shared GETs ran.
    run(|server| async move {
        for max in [1usize, 2, 3] {
            let pool = Pool::new(server.tcp(), max).wait_timeout(Duration::from_secs(10));
            let k = key("busy");
            pool.set(&k, "x").await.unwrap();
            let stop = Rc::new(std::cell::Cell::new(false));
            let done = Rc::new(std::cell::Cell::new(0u64));
            let mut load = Vec::new();
            for _ in 0..32 {
                let (pool, stop, done, k) = (pool.clone(), stop.clone(), done.clone(), k.clone());
                load.push(giong::spawn(async move {
                    while !stop.get() {
                        let v: String = pool.get(&k).await.unwrap();
                        assert_eq!(v, "x");
                        done.set(done.get() + 1);
                    }
                }));
            }
            giong::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(pool.stats().connections, max);
            // Three takers at once, served in the order they came.
            let order = Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut takers = Vec::new();
            for t in 0..3 {
                let (pool, order) = (pool.clone(), order.clone());
                takers.push(giong::spawn(async move {
                    let start = std::time::Instant::now();
                    let mut conn = pool.take().await.unwrap();
                    let waited = start.elapsed();
                    order.borrow_mut().push(t);
                    let none: Option<Value> = conn
                        .query_blocking(&cmd("BLPOP").arg(key("never")).arg(0.02))
                        .await
                        .unwrap();
                    assert_eq!(none, None);
                    waited
                }));
                // Each waits in take() before the next comes.
                giong::time::sleep(Duration::from_millis(5)).await;
            }
            let before = done.get();
            for t in takers {
                let waited = t.await.unwrap();
                assert!(
                    waited < Duration::from_secs(5),
                    "max_size {max}: {waited:?}"
                );
            }
            assert_eq!(*order.borrow(), [0, 1, 2], "max_size {max}");
            if max > 1 {
                // Shared commands kept a connection meanwhile.
                assert!(
                    done.get() > before,
                    "max_size {max}: shared commands stalled"
                );
            }
            stop.set(true);
            for t in load {
                t.await.unwrap();
            }
            let stats = pool.stats();
            assert_eq!(stats.taken, 0, "{stats:?}");
            assert_eq!(stats.connections, max, "{stats:?}");
            assert_eq!(stats.timeouts, 0, "{stats:?}");
            // Nothing is left set aside: every connection carries shared
            // commands again.
            let mut ids = std::collections::HashSet::new();
            for _ in 0..200 {
                let tasks: Vec<_> = (0..8)
                    .map(|_| {
                        let pool = pool.clone();
                        giong::spawn(async move {
                            pool.query::<i64>(&cmd("CLIENT").arg("ID")).await.unwrap()
                        })
                    })
                    .collect();
                for t in tasks {
                    ids.insert(t.await.unwrap());
                }
            }
            assert_eq!(ids.len(), max, "max_size {max}");
        }
    });
}

#[test]
fn a_taker_that_gives_up_leaves_no_connection_set_aside() {
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1).wait_timeout(Duration::from_secs(5));
        pool.ping().await.unwrap();
        // A shared command holds the only connection while a task waits to
        // take it, then the waiting task gives up.
        let blocker = giong::spawn({
            let pool = pool.clone();
            async move {
                let _: Value = pool
                    .query(&cmd("DEBUG").arg("SLEEP").arg(0.3))
                    .await
                    .unwrap();
            }
        });
        giong::time::sleep(Duration::from_millis(20)).await;
        let gave_up = giong::time::timeout(Duration::from_millis(50), pool.take()).await;
        assert!(gave_up.is_err());
        // Shared commands are not held up behind a connection set aside for
        // nobody.
        let start = std::time::Instant::now();
        pool.ping().await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        blocker.await.unwrap();
        assert_eq!(pool.stats().waiting, 0);
        assert_eq!(pool.stats().opened, 1);
    });
}

#[test]
fn a_wait_timeout_too_long_for_the_clock_means_no_limit() {
    // `Instant::now() + Duration::MAX` panicked on every command.
    run(|server| async move {
        let pool = Pool::new(server.tcp(), 1).wait_timeout(Duration::MAX);
        pool.ping().await.unwrap();
        let conn = pool.take().await.unwrap();
        let waiter = giong::spawn({
            let pool = pool.clone();
            async move { pool.ping().await }
        });
        giong::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.stats().waiting, 1);
        drop(conn);
        waiter.await.unwrap().unwrap();
    });
}

#[test]
fn a_connection_to_a_server_that_became_a_replica_is_replaced() {
    // After a failover the old primary keeps its connections open and
    // answers every write with READONLY; before, the pool kept using them
    // until they dropped for some other reason.
    run(|_server| async move {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = fake.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        {
            let (accepted, closed) = (accepted.clone(), closed.clone());
            std::thread::spawn(move || {
                for s in fake.incoming() {
                    let mut s = s.unwrap();
                    let n = accepted.fetch_add(1, Ordering::SeqCst);
                    let closed = closed.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 4096];
                        let mut seen = Vec::new();
                        let mut answered = 0;
                        loop {
                            let got = s.read(&mut buf).unwrap_or(0);
                            if got == 0 {
                                closed.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            seen.extend_from_slice(&buf[..got]);
                            // One command per `*`, as the client sends them.
                            let cmds: Vec<&[u8]> = seen.split(|&b| b == b'*').skip(1).collect();
                            for c in &cmds[answered..] {
                                if !c.ends_with(b"\r\n") || c.split(|&b| b == b'\n').count() < 3 {
                                    break;
                                }
                                let reply: &[u8] = if c.windows(5).any(|w| w == b"HELLO") {
                                    b"%1\r\n$7\r\nversion\r\n$5\r\n7.0.0\r\n"
                                } else if c.windows(3).any(|w| w == b"SET") && n == 0 {
                                    b"-READONLY You can't write against a read only replica.\r\n"
                                } else if c.windows(3).any(|w| w == b"SET") {
                                    b"+OK\r\n"
                                } else {
                                    b"+PONG\r\n"
                                };
                                s.write_all(reply).unwrap();
                                answered += 1;
                            }
                        }
                    });
                }
            });
        }
        let pool = Pool::new(giong_redis::Config::new().host("127.0.0.1").port(port), 1);
        pool.ping().await.unwrap();
        let e = pool.set("k", "v").await.unwrap_err();
        assert_eq!(e.server_code(), Some("READONLY"));
        assert!(e.is_transient());
        assert!(e.to_string().contains("failover"), "{e}");
        // The next command goes to a new connection, which accepts writes.
        pool.set("k", "v").await.unwrap();
        assert_eq!(pool.stats().opened, 2);
        assert_eq!(pool.stats().connections, 1);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        // The old one is closed.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while closed.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the old connection stayed open"
            );
            giong::time::sleep(Duration::from_millis(5)).await;
        }
    });
}
