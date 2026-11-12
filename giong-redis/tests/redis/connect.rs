use std::time::{Duration, Instant};

use giong_redis::{cmd, Client, Config, ErrorKind, Tls, TlsConnector, Value};

use crate::server::{free_port, APP};
use crate::{key, run};

#[test]
fn acl_users_log_in_and_wrong_passwords_are_refused() {
    run(|server| async move {
        let c = Client::connect(&server.tcp().user(APP.0).password(APP.1))
            .await
            .unwrap();
        let who: String = c.query(&cmd("ACL").arg("WHOAMI")).await.unwrap();
        assert_eq!(who, APP.0);

        let url = format!("redis://{}:{}@127.0.0.1:{}", APP.0, APP.1, server.port);
        let c = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        let who: String = c.query(&cmd("ACL").arg("WHOAMI")).await.unwrap();
        assert_eq!(who, APP.0);

        let e = Client::connect(&server.tcp().user(APP.0).password("wrong"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth);
        assert_eq!(e.server_code(), Some("WRONGPASS"));
        assert!(
            e.to_string().contains("refused the user name or password"),
            "{e}"
        );

        let e = Client::connect(&server.tcp().user(APP.0))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Config);
    });
}

#[test]
fn the_database_and_the_client_name_are_set_when_connecting() {
    run(|server| async move {
        let k = key("db3");
        let in3 = Client::connect(&server.tcp().database(3).client_name("giong-test"))
            .await
            .unwrap();
        in3.set(&k, "three").await.unwrap();
        let name: String = in3.query(&cmd("CLIENT").arg("GETNAME")).await.unwrap();
        assert_eq!(name, "giong-test");

        let in0 = Client::connect(&server.tcp()).await.unwrap();
        assert_eq!(in0.get::<Option<String>>(&k).await.unwrap(), None);
        let url = format!("redis://127.0.0.1:{}/3", server.port);
        let again = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        assert_eq!(again.get::<String>(&k).await.unwrap(), "three");

        let e = Client::connect(&server.tcp().database(99))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert!(e.to_string().contains("refused database 99"), "{e}");
        let e = Client::connect(&server.tcp().client_name("has space"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Config);
    });
}

#[test]
fn unix_sockets_and_tls() {
    run(|server| async move {
        let k = key("transports");
        let unix = Client::connect(&server.unix()).await.unwrap();
        unix.set(&k, "u").await.unwrap();
        let url = format!("redis+unix://{}?db=0", server.socket.display());
        let unix = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        assert_eq!(unix.get::<String>(&k).await.unwrap(), "u");

        for host in ["localhost", "127.0.0.1"] {
            let tls = Client::connect(&server.tls(host)).await.unwrap();
            assert_eq!(tls.get::<String>(&k).await.unwrap(), "u");
        }
        let url = format!(
            "rediss://localhost:{}/0?cacert={}",
            server.tls_port,
            server.ca_path().display()
        );
        let tls = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        tls.ping().await.unwrap();

        // Another authority does not vouch for the server.
        let stranger = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let e = Client::connect(&server.tcp().host("localhost").port(server.tls_port).tls(
            Tls::Verify(TlsConnector::with_root_pem(stranger.cert.pem().as_bytes()).unwrap()),
        ))
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert!(e.to_string().contains("TLS handshake failed"), "{e}");
        // TLS to the plain port, and plain to the TLS port, both fail.
        // (The plain port waits for the end of a line that never comes.)
        let e = Client::connect(
            &server
                .tls("localhost")
                .port(server.port)
                .connect_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        let e = Client::connect(
            &server
                .tcp()
                .port(server.tls_port)
                .connect_timeout(Duration::from_secs(2)),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
    });
}

#[test]
fn connecting_fails_fast_with_a_reason() {
    run(|_server| async move {
        let closed = free_port();
        let start = Instant::now();
        let e = Client::connect(&Config::new().host("127.0.0.1").port(closed))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert!(e.is_transient());
        assert!(start.elapsed() < Duration::from_secs(2));

        // A server that accepts and never answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let start = Instant::now();
        let e = Client::connect(
            &Config::new()
                .host("127.0.0.1")
                .port(port)
                .connect_timeout(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert!(e.to_string().contains("connect timeout"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(silent);

        let e = Client::connect(&Config::new().host("/nonexistent/redis.sock"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
    });
}

#[test]
fn a_server_that_is_not_redis_is_a_protocol_error() {
    run(|_server| async move {
        let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = fake.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = fake.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        });
        let e = Client::connect(&Config::new().host("127.0.0.1").port(port))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Protocol);
        assert!(
            e.to_string().contains("is this really a Redis server"),
            "{e}"
        );
        t.join().unwrap();
        let _: Value = Value::Nil;
    });
}
