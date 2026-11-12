use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_redis::{cmd, Client, ErrorKind, Pipeline, Value};

use crate::{key, run};

#[test]
fn strings_numbers_and_missing_keys() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();
        assert_eq!(c.server_name(), "redis");
        assert_eq!(c.server_version(), server.version);
        assert!(c.connection_id() > 0);
        c.ping().await.unwrap();

        let k = key("strings");
        let missing: Option<String> = c.get(&k).await.unwrap();
        assert_eq!(missing, None);
        let e = c.get::<String>(&k).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Type);
        assert!(e.to_string().contains("Option<String>"), "{e}");

        c.set(&k, "héllo").await.unwrap();
        assert_eq!(c.get::<String>(&k).await.unwrap(), "héllo");
        assert!(c.exists(&k).await.unwrap());
        assert!(!c.set_nx(&k, "other").await.unwrap());
        assert!(c.del(&k).await.unwrap());
        assert!(!c.del(&k).await.unwrap());
        assert!(c.set_nx(&k, "first").await.unwrap());

        let n = key("counter");
        assert_eq!(c.incr(&n, 5).await.unwrap(), 5);
        assert_eq!(c.incr(&n, -7).await.unwrap(), -2);
        assert_eq!(c.get::<i64>(&n).await.unwrap(), -2);

        let t = key("ttl");
        c.set_ex(&t, 1u8, Duration::from_millis(150)).await.unwrap();
        assert_eq!(c.get::<Option<u8>>(&t).await.unwrap(), Some(1));
        giong::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(c.get::<Option<u8>>(&t).await.unwrap(), None);
        c.set(&t, "x").await.unwrap();
        assert!(c.expire(&t, Duration::from_secs(60)).await.unwrap());
        let ms: i64 = c.query(&cmd("PTTL").arg(&t)).await.unwrap();
        assert!((59_000..=60_000).contains(&ms), "{ms}");
        assert!(!c.expire(key("none"), Duration::from_secs(1)).await.unwrap());

        let (a, b) = (key("mget"), key("mget"));
        c.set(&a, 1).await.unwrap();
        let both: Vec<Option<i64>> = c.mget(&[&a, &b]).await.unwrap();
        assert_eq!(both, [Some(1), None]);
    });
}

#[test]
fn replies_of_every_shape_are_read() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();

        let h = key("hash");
        assert!(c.hset(&h, "name", "Ada").await.unwrap());
        assert!(!c.hset(&h, "name", "Ada L.").await.unwrap());
        c.hset(&h, "born", 1815).await.unwrap();
        assert_eq!(
            c.hget::<Option<String>>(&h, "name")
                .await
                .unwrap()
                .as_deref(),
            Some("Ada L.")
        );
        assert_eq!(c.hget::<Option<String>>(&h, "nope").await.unwrap(), None);
        let all: HashMap<String, String> = c.hgetall(&h).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["born"], "1815");
        let empty: HashMap<String, String> = c.hgetall(key("nohash")).await.unwrap();
        assert!(empty.is_empty());
        // RESP3 sends a map: it is a Value::Map.
        let raw: Value = c.query(&cmd("HGETALL").arg(&h)).await.unwrap();
        assert!(matches!(raw, Value::Map(ref m) if m.len() == 2), "{raw:?}");

        let s = key("set");
        let added: i64 = c
            .query(&cmd("SADD").arg(&s).args(["a", "b", "c"]))
            .await
            .unwrap();
        assert_eq!(added, 3);
        let members: HashSet<String> = c.query(&cmd("SMEMBERS").arg(&s)).await.unwrap();
        assert_eq!(members.len(), 3);
        let raw: Value = c.query(&cmd("SMEMBERS").arg(&s)).await.unwrap();
        assert!(matches!(raw, Value::Set(_)), "{raw:?}");

        let z = key("zset");
        let _: i64 = c
            .query(&cmd("ZADD").arg(&z).arg(1.5).arg("one").arg(2.25).arg("two"))
            .await
            .unwrap();
        let ranked: Vec<(String, f64)> = c
            .query(&cmd("ZRANGE").arg(&z).arg(0).arg(-1).arg("WITHSCORES"))
            .await
            .unwrap();
        assert_eq!(
            ranked,
            [("one".to_string(), 1.5), ("two".to_string(), 2.25)]
        );
        let score: f64 = c.query(&cmd("ZSCORE").arg(&z).arg("two")).await.unwrap();
        assert_eq!(score, 2.25);

        let l = key("list");
        let len: i64 = c
            .query(&cmd("RPUSH").arg(&l).args([1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(len, 3);
        let items: Vec<i32> = c
            .query(&cmd("LRANGE").arg(&l).arg(0).arg(-1))
            .await
            .unwrap();
        assert_eq!(items, [1, 2, 3]);

        // Binary-safe values, CR LF included, and a large one.
        let b = key("bytes");
        let bytes: Vec<u8> = (0..=255u8).chain(*b"\r\n$3\r\n").collect();
        c.set(&b, &bytes).await.unwrap();
        assert_eq!(c.get::<Vec<u8>>(&b).await.unwrap(), bytes);
        assert_eq!(
            c.get::<String>(&b).await.unwrap_err().kind(),
            ErrorKind::Type
        );
        let big = key("big");
        let large = vec![b'z'; 3 << 20];
        c.set(&big, &large).await.unwrap();
        assert_eq!(c.get::<Vec<u8>>(&big).await.unwrap().len(), large.len());

        // INFO is verbatim text in RESP3.
        let info: String = c.query(&cmd("INFO").arg("server")).await.unwrap();
        assert!(info.contains("redis_version"), "{info}");
        let raw: Value = c.query(&cmd("INFO").arg("server")).await.unwrap();
        assert!(
            matches!(raw, Value::Verbatim { ref format, .. } if format == "txt"),
            "{raw:?}"
        );

        // Booleans, doubles and nil from a script.
        let v: Value = c
            .query(&cmd("EVAL").arg("return {1, 'two', {3}, false}").arg(0))
            .await
            .unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Int(1),
                Value::Bulk(b"two".to_vec()),
                Value::Array(vec![Value::Int(3)]),
                Value::Nil,
            ])
        );
    });
}

#[test]
fn many_tasks_share_one_connection_and_each_gets_its_own_reply() {
    run(|server| async move {
        let c = Rc::new(Client::connect(&server.tcp()).await.unwrap());
        let prefix = key("share");
        let mut tasks = Vec::new();
        for t in 0..200 {
            let c = c.clone();
            let k = format!("{prefix}:{t}");
            tasks.push(giong::spawn(async move {
                for i in 1..=50i64 {
                    let n = c.incr(&k, t).await.unwrap();
                    assert_eq!(n, i * t);
                    let s: String = c.query(&cmd("ECHO").arg(format!("{t}-{i}"))).await.unwrap();
                    assert_eq!(s, format!("{t}-{i}"));
                }
            }));
        }
        let mut most = 0;
        while tasks.iter().any(|t| !t.is_finished()) {
            most = most.max(c.in_flight());
            giong::time::sleep(Duration::from_micros(200)).await;
        }
        for t in tasks {
            t.await.unwrap();
        }
        // The commands of many tasks were in flight together.
        assert!(most > 10, "at most {most} in flight");
        assert_eq!(c.in_flight(), 0);
    });
}

#[test]
fn server_errors_carry_their_code_and_leave_the_connection_usable() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();
        let k = key("wrongtype");
        c.query::<()>(&cmd("LPUSH").arg(&k).arg("x")).await.unwrap();
        let e = c.get::<Option<String>>(&k).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Server);
        assert_eq!(e.server_code(), Some("WRONGTYPE"));
        assert!(e.to_string().starts_with("GIONG-E3204: WRONGTYPE"), "{e}");
        assert!(!e.is_transient());

        let e = c.query::<Value>(&cmd("NOSUCHCOMMAND")).await.unwrap_err();
        assert_eq!(e.server_code(), Some("ERR"));
        let e = c
            .query::<Value>(&cmd("EVALSHA").arg("0".repeat(40)).arg(0))
            .await
            .unwrap_err();
        assert_eq!(e.server_code(), Some("NOSCRIPT"));
        assert!(e.to_string().contains("SCRIPT LOAD"), "{e}");

        c.ping().await.unwrap();
        assert!(!c.is_closed());
    });
}

#[test]
fn pipelines_and_transactions() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();
        let k = key("pipe");
        let (n, set, got): (i64, (), String) = c
            .pipeline(
                &Pipeline::new()
                    .add(cmd("INCR").arg(format!("{k}:n")))
                    .add(cmd("SET").arg(&k).arg("v"))
                    .add(cmd("GET").arg(&k)),
            )
            .await
            .unwrap();
        assert_eq!((n, set, got.as_str()), (1, (), "v"));
        assert_eq!(
            c.pipeline::<Vec<Value>>(&Pipeline::new()).await.unwrap(),
            []
        );

        // One command's error does not stop the others; Vec<Value> shows it.
        let replies: Vec<Value> = c
            .pipeline(
                &Pipeline::new()
                    .add(cmd("INCR").arg(&k))
                    .add(cmd("SET").arg(format!("{k}:2")).arg(2))
                    .add(cmd("GET").arg(format!("{k}:2"))),
            )
            .await
            .unwrap();
        assert!(
            matches!(&replies[0], Value::Error(e) if e.code() == "ERR"),
            "{replies:?}"
        );
        assert_eq!(replies[2], Value::Bulk(b"2".to_vec()));
        let e = c
            .pipeline::<(i64, (), i64)>(
                &Pipeline::new()
                    .add(cmd("INCR").arg(&k))
                    .add(cmd("PING"))
                    .add(cmd("PING")),
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Server);

        // A transaction.
        let a = key("balance");
        let b = key("balance");
        c.set(&a, 100).await.unwrap();
        let (x, y): (i64, i64) = c
            .pipeline(
                &Pipeline::new()
                    .add(cmd("DECRBY").arg(&a).arg(30))
                    .add(cmd("INCRBY").arg(&b).arg(30))
                    .atomic(),
            )
            .await
            .unwrap();
        assert_eq!((x, y), (70, 30));

        // A command refused as it is queued: nothing runs.
        let e = c
            .pipeline::<Value>(
                &Pipeline::new()
                    .add(cmd("INCRBY").arg(&a).arg(1))
                    .add(cmd("SET").arg(&a))
                    .atomic(),
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Server);
        assert!(e.to_string().contains("command 2 of 2"), "{e}");
        assert_eq!(c.get::<i64>(&a).await.unwrap(), 70);
        // Blocking commands do not block inside a transaction.
        let (popped,): (Option<Value>,) = c
            .pipeline(
                &Pipeline::new()
                    .add(cmd("BLPOP").arg(key("q")).arg(5))
                    .atomic(),
            )
            .await
            .unwrap();
        assert_eq!(popped, None);
    });
}

#[test]
fn watch_runs_the_transaction_only_when_the_keys_did_not_change() {
    run(|server| async move {
        let mut c = Client::connect(&server.tcp()).await.unwrap();
        let other = Client::connect(&server.tcp()).await.unwrap();
        let k = key("stock");
        c.set(&k, 5).await.unwrap();

        let watch = c.watch(&[&k]).await.unwrap();
        let stock: i64 = watch.query(&cmd("GET").arg(&k)).await.unwrap();
        other.set(&k, 4).await.unwrap();
        let ran = watch
            .exec::<Vec<Value>>(&Pipeline::new().add(cmd("SET").arg(&k).arg(stock - 1)))
            .await
            .unwrap();
        assert_eq!(ran, None);
        assert_eq!(other.get::<i64>(&k).await.unwrap(), 4);

        let watch = c.watch(&[&k]).await.unwrap();
        let stock: i64 = watch.query(&cmd("GET").arg(&k)).await.unwrap();
        let ran: Option<((),)> = watch
            .exec(&Pipeline::new().add(cmd("SET").arg(&k).arg(stock - 1)))
            .await
            .unwrap();
        assert!(ran.is_some());
        assert_eq!(other.get::<i64>(&k).await.unwrap(), 3);

        // A watch dropped without exec is ended: a later transaction runs
        // even though the key changed.
        let watch = c.watch(&[&k]).await.unwrap();
        drop(watch);
        other.set(&k, 10).await.unwrap();
        let (v,): (i64,) = c
            .pipeline(&Pipeline::new().add(cmd("INCR").arg(&k)).atomic())
            .await
            .unwrap();
        assert_eq!(v, 11);
        assert!(c.watch::<&str>(&[]).await.is_err());
    });
}

#[test]
fn commands_that_would_disturb_sharing_are_refused_before_sending() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();
        for (command, hint) in [
            (cmd("MULTI"), "atomic"),
            (cmd("SELECT").arg(1), "Config::database"),
            (cmd("SUBSCRIBE").arg("x"), "PubSub::connect"),
            (cmd("WATCH").arg("x"), "Client::watch"),
            (cmd("AUTH").arg("x"), "Config::password"),
            (cmd("BLPOP").arg("x").arg(0), "query_blocking"),
            (cmd("CLIENT").arg("REPLY").arg("OFF"), "wrong commands"),
        ] {
            let e = c.query::<Value>(&command).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Command, "{}", command.name());
            assert!(e.to_string().contains(hint), "{e}");
        }
        assert_eq!(c.in_flight(), 0);
        let e = c
            .pipeline::<Value>(&Pipeline::new().add(cmd("PING")).add(cmd("EXEC")))
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("command 2 of the pipeline (EXEC)"),
            "{e}"
        );
        c.ping().await.unwrap();
    });
}

#[test]
fn blocking_commands_run_on_a_connection_of_their_own() {
    run(|server| async move {
        let mut c = Client::connect(&server.tcp()).await.unwrap();
        let other = Client::connect(&server.tcp()).await.unwrap();
        let q = key("queue");

        let start = Instant::now();
        let none: Option<(String, String)> = c
            .query_blocking(&cmd("BLPOP").arg(&q).arg(0.2))
            .await
            .unwrap();
        assert_eq!(none, None);
        assert!(start.elapsed() >= Duration::from_millis(150));

        let q2 = q.clone();
        let pusher = giong::spawn(async move {
            giong::time::sleep(Duration::from_millis(50)).await;
            other
                .query::<i64>(&cmd("RPUSH").arg(&q2).arg("job-1"))
                .await
                .unwrap();
        });
        let got: Option<(String, String)> = c
            .query_blocking(&cmd("BLPOP").arg(&q).arg(5))
            .await
            .unwrap();
        assert_eq!(got, Some((q.clone(), "job-1".to_string())));
        pusher.await.unwrap();
        // Still refused through query.
        assert!(c
            .query::<Value>(&cmd("BRPOP").arg(&q).arg(1))
            .await
            .is_err());
    });
}

#[test]
fn a_reply_over_the_limit_fails_alone() {
    run(|server| async move {
        let small = Rc::new(
            Client::connect(&server.tcp().max_reply_size(1000))
                .await
                .unwrap(),
        );
        let big = key("huge");
        small.set(&big, vec![b'h'; 100_000]).await.unwrap();
        let k = key("fine");
        small.set(&k, "ok").await.unwrap();

        // Other tasks' commands in flight around it get their own replies.
        let mut tasks = Vec::new();
        for i in 0..20 {
            let c = small.clone();
            let (big, k) = (big.clone(), k.clone());
            tasks.push(giong::spawn(async move {
                if i % 2 == 0 {
                    let e = c.get::<Vec<u8>>(&big).await.unwrap_err();
                    assert_eq!(e.kind(), ErrorKind::TooLarge);
                    assert!(e.to_string().contains("max_reply_size"), "{e}");
                } else {
                    assert_eq!(c.get::<String>(&k).await.unwrap(), "ok");
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let e = small
            .pipeline::<Value>(
                &Pipeline::new()
                    .add(cmd("GET").arg(&k))
                    .add(cmd("GET").arg(&big)),
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge);
        assert_eq!(small.get::<String>(&k).await.unwrap(), "ok");
        assert!(!small.is_closed());
    });
}

#[test]
fn the_response_timeout_fails_the_command_and_its_late_reply_is_dropped() {
    run(|server| async move {
        let c = Client::connect(&server.tcp().response_timeout(Duration::from_millis(100)))
            .await
            .unwrap();
        let k = key("late");
        c.set(&k, "value").await.unwrap();
        let e = c
            .query::<Value>(&cmd("DEBUG").arg("SLEEP").arg(0.3))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout);
        assert!(e.is_transient());
        assert!(e.to_string().contains("response_timeout"), "{e}");
        // The next command waits behind the sleep, then gets its own reply.
        let c = Client::connect(&server.tcp()).await.unwrap();
        let slow = giong::time::timeout(
            Duration::from_millis(20),
            c.query::<Value>(&cmd("DEBUG").arg("SLEEP").arg(0.1)),
        )
        .await;
        assert!(slow.is_err(), "the sleep was cut short");
        assert_eq!(c.get::<String>(&k).await.unwrap(), "value");
        assert_eq!(c.in_flight(), 0);
    });
}

#[test]
fn commands_queued_behind_an_unanswered_one_each_time_out_on_their_own_deadline() {
    run(|_server| async move {
        // A server that answers the handshake, then nothing for 600 ms; then
        // the four commands' late replies, then PONG to every new PING.
        let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = fake.local_addr().unwrap().port();
        let script = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = fake.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            while !seen.windows(5).any(|w| w == b"HELLO") {
                let n = s.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"%1\r\n$7\r\nversion\r\n$5\r\n7.0.0\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(600));
            s.write_all(b"+late\r\n+late\r\n+late\r\n+late\r\n")
                .unwrap();
            let mut pings = 0;
            let mut seen = Vec::new();
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                let now = seen.windows(4).filter(|w| *w == b"PING").count();
                for _ in pings..now.saturating_sub(4) {
                    s.write_all(b"+PONG\r\n").unwrap();
                }
                pings = pings.max(now.saturating_sub(4));
            }
        });

        let timeout = Duration::from_millis(150);
        let config = giong_redis::Config::new()
            .host("127.0.0.1")
            .port(port)
            .response_timeout(timeout);
        let c = Rc::new(Client::connect(&config).await.unwrap());
        let start = Instant::now();
        // Sent 40 ms apart: the first sets the connection's timer, the
        // others are queued while it is set for the first.
        let mut tasks = Vec::new();
        for i in 0..4u32 {
            let c = c.clone();
            tasks.push(giong::spawn(async move {
                giong::time::sleep(Duration::from_millis(40) * i).await;
                let sent = Instant::now();
                let e = c.query::<String>(&cmd("PING")).await.unwrap_err();
                (sent.elapsed(), e.kind())
            }));
        }
        for t in tasks {
            let (waited, kind) = t.await.unwrap();
            assert_eq!(kind, ErrorKind::Timeout);
            assert!(waited >= timeout, "timed out early, after {waited:?}");
            assert!(waited < timeout * 3, "timed out late, after {waited:?}");
        }
        assert!(start.elapsed() < Duration::from_millis(550));
        // The late replies are thrown away, not given to the next command.
        giong::time::sleep_until(start + Duration::from_millis(700)).await;
        let pong: String = c.query(&cmd("PING")).await.unwrap();
        assert_eq!(pong, "PONG");
        assert_eq!(c.in_flight(), 0);
        // Closing the connection ends the script; the runtime has to keep
        // running for that, so no blocking join until it has.
        drop(c);
        while !script.is_finished() {
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        script.join().unwrap();
    });
}

#[test]
fn a_connection_the_server_closed_fails_its_commands() {
    run(|server| async move {
        let c = Client::connect(&server.tcp()).await.unwrap();
        let admin = Client::connect(&server.tcp()).await.unwrap();
        c.ping().await.unwrap();
        let killed: i64 = admin
            .query(&cmd("CLIENT").arg("KILL").arg("ID").arg(c.connection_id()))
            .await
            .unwrap();
        assert_eq!(killed, 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !c.is_closed() {
            assert!(Instant::now() < deadline, "the close was not noticed");
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        let e = c.ping().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert!(e.is_transient());
    });
}
