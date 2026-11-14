use std::time::Duration;

use giong_redis::{Client, ErrorKind, PubSub};

use crate::{key, run};

#[test]
fn messages_arrive_on_channels_and_patterns() {
    run(|server| async move {
        let publisher = Client::connect(&server.tcp()).await.unwrap();
        let mut sub = PubSub::connect(&server.tcp()).await.unwrap();
        let channel = key("news");
        let prefix = key("events");
        sub.subscribe(&channel).await.unwrap();
        sub.psubscribe(format!("{prefix}:*")).await.unwrap();

        assert_eq!(publisher.publish(&channel, "first").await.unwrap(), 1);
        let m = sub.next_message().await.unwrap();
        assert_eq!(m.channel(), channel.as_bytes());
        assert_eq!(m.payload_str().unwrap(), "first");
        assert_eq!(m.pattern(), None);

        let binary: Vec<u8> = vec![0, 13, 10, 255];
        let on = format!("{prefix}:7");
        assert_eq!(publisher.publish(&on, &binary).await.unwrap(), 1);
        let m = sub.next_message().await.unwrap();
        assert_eq!(m.channel(), on.as_bytes());
        assert_eq!(m.pattern(), Some(format!("{prefix}:*").as_bytes()));
        assert!(m.payload_str().is_err());
        assert_eq!(m.into_payload(), binary);

        sub.unsubscribe(&channel).await.unwrap();
        assert_eq!(publisher.publish(&channel, "unheard").await.unwrap(), 0);
        sub.punsubscribe(format!("{prefix}:*")).await.unwrap();
        assert_eq!(publisher.publish(&on, "unheard").await.unwrap(), 0);
    });
}

#[test]
fn waiting_for_a_message_can_be_cancelled_without_losing_one() {
    run(|server| async move {
        let publisher = Client::connect(&server.tcp()).await.unwrap();
        let mut sub = PubSub::connect(&server.tcp()).await.unwrap();
        let channel = key("ticks");
        sub.subscribe(&channel).await.unwrap();
        for _ in 0..5 {
            let r = giong::time::timeout(Duration::from_millis(10), sub.next_message()).await;
            assert!(r.is_err());
        }
        for i in 0..100 {
            publisher.publish(&channel, i).await.unwrap();
        }
        // Messages published while subscribing to more are kept.
        let other = key("other");
        sub.subscribe(&other).await.unwrap();
        for i in 0..100 {
            let m = loop {
                if let Ok(m) =
                    giong::time::timeout(Duration::from_millis(1), sub.next_message()).await
                {
                    break m.unwrap();
                }
            };
            assert_eq!(m.payload_str().unwrap(), i.to_string());
        }
    });
}

#[test]
fn a_subscriber_whose_connection_closed_is_told() {
    run(|server| async move {
        let admin = Client::connect(&server.tcp()).await.unwrap();
        let mut sub = PubSub::connect(&server.tcp()).await.unwrap();
        sub.subscribe(key("void")).await.unwrap();
        let _: i64 = admin
            .query(
                &giong_redis::cmd("CLIENT")
                    .arg("KILL")
                    .arg("ID")
                    .arg(sub.connection_id()),
            )
            .await
            .unwrap();
        let e = sub.next_message().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert!(e.to_string().contains("subscribe again"), "{e}");
        let e = sub.subscribe("again").await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
    });
}

#[test]
fn a_late_confirmation_for_another_channel_is_not_taken_for_this_one() {
    // A server that does not confirm SUBSCRIBE a until SUBSCRIBE b comes,
    // and confirms b 300 ms later. Before confirmations were matched by
    // channel, subscribe("b") returned at a's confirmation, before the
    // server had subscribed b.
    run(|_server| async move {
        use std::io::{Read, Write};
        let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = fake.local_addr().unwrap().port();
        let script = std::thread::spawn(move || {
            let (mut s, _) = fake.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            let mut wait_for = |s: &mut std::net::TcpStream, what: &[u8]| {
                while !seen.windows(what.len()).any(|w| w == what) {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "closed early");
                    seen.extend_from_slice(&buf[..n]);
                }
            };
            wait_for(&mut s, b"HELLO");
            s.write_all(b"%1\r\n$7\r\nversion\r\n$5\r\n7.0.0\r\n")
                .unwrap();
            wait_for(&mut s, b"$1\r\nb\r\n");
            s.write_all(b">3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:1\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
            s.write_all(b">3\r\n$9\r\nsubscribe\r\n$1\r\nb\r\n:2\r\n")
                .unwrap();
            let _ = s.read(&mut buf);
        });
        let config = giong_redis::Config::new().host("127.0.0.1").port(port);
        let mut sub = PubSub::connect(&config).await.unwrap();
        let cut = giong::time::timeout(Duration::from_millis(50), sub.subscribe("a")).await;
        assert!(cut.is_err());
        let start = std::time::Instant::now();
        sub.subscribe("b").await.unwrap();
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(250),
            "returned after {waited:?}"
        );
        drop(sub);
        while !script.is_finished() {
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        script.join().unwrap();
    });
}
