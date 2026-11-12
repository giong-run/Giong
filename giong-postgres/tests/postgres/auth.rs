//! Connecting: sockets, TLS, authentication methods, and the errors when any
//! of it goes wrong.

use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong_postgres::{Client, Config, ErrorKind, Tls, TlsConnector};

use crate::run;
use crate::server::{CLEAR_USER, MD5_USER, SCRAM_USER, TLS_USER};

async fn one(client: &Client) -> i32 {
    client
        .query_one("SELECT 1", &[])
        .await
        .unwrap()
        .get(0)
        .unwrap()
}

#[test]
fn trust_over_the_unix_socket() {
    run(|s| async move {
        let client = Client::connect(&s.local()).await.unwrap();
        assert_eq!(one(&client).await, 1);
        assert!(client.parameter("server_version").is_some());
        assert_eq!(client.parameter("client_encoding").as_deref(), Some("UTF8"));
    });
}

#[test]
fn a_url_works_as_well_as_the_setters() {
    run(|s| async move {
        let url = format!(
            "postgres:///postgres?host={}&port={}&user=postgres&application_name=from-url",
            s.socket_dir.display(),
            s.port
        );
        let client = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        let row = client
            .query_one("SELECT current_setting('application_name')", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "from-url");
    });
}

#[test]
fn scram_sha_256_with_the_right_and_the_wrong_password() {
    run(|s| async move {
        let client = Client::connect(&s.tcp(SCRAM_USER)).await.unwrap();
        let row = client
            .query_one("SELECT current_user::text", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), SCRAM_USER.0);

        let e = Client::connect(&s.tcp((SCRAM_USER.0, "wrong")))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert_eq!(e.code(), Some("28P01"), "{e}");
        assert!(
            e.to_string().contains("password authentication failed"),
            "{e}"
        );
    });
}

#[test]
fn md5_and_clear_text_passwords() {
    run(|s| async move {
        let client = Client::connect(&s.tcp(MD5_USER)).await.unwrap();
        assert_eq!(one(&client).await, 1);
        let e = Client::connect(&s.tcp((MD5_USER.0, "wrong")))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some("28P01"), "{e}");

        // Clear text is allowed to a loopback address.
        let client = Client::connect(&s.tcp(CLEAR_USER)).await.unwrap();
        assert_eq!(one(&client).await, 1);
    });
}

#[test]
fn tls_is_verified_against_the_given_authority() {
    run(|s| async move {
        for host in ["localhost", "127.0.0.1"] {
            let client = Client::connect(&s.tls(host, TLS_USER)).await.unwrap();
            let row = client
                .query_one(
                    "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                    &[],
                )
                .await
                .unwrap();
            assert!(row.get::<bool>(0).unwrap(), "{host}");
        }

        // An authority the server's certificate does not come from.
        let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = s.tcp(TLS_USER).host("localhost").tls(Tls::Verify(
            TlsConnector::with_root_pem(other.cert.pem().as_bytes()).unwrap(),
        ));
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("TLS handshake"), "{e}");

        // An address the certificate is not for (the server listens on it
        // too).
        let e = Client::connect(&s.tls("127.0.0.2", TLS_USER))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not valid for"), "{e}");
    });
}

#[test]
fn a_server_that_requires_tls_refuses_a_plain_connection() {
    run(|s| async move {
        let e = Client::connect(&s.tcp(TLS_USER)).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert_eq!(e.code(), Some("28000"), "{e}");
    });
}

#[test]
fn startup_errors_say_what_is_missing() {
    run(|s| async move {
        let e = Client::connect(
            &s.tcp((SCRAM_USER.0, "x"))
                .dbname("no_such_db")
                .password(SCRAM_USER.1),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert_eq!(e.code(), Some("3D000"), "{e}");

        let e = Client::connect(
            &Config::new()
                .host("127.0.0.1")
                .port(s.port)
                .user(SCRAM_USER.0),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert!(e.to_string().contains("Config::password"), "{e}");

        let e = Client::connect(&Config::new().host("127.0.0.1").port(s.port))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Config, "{e}");

        let e = Client::connect(&s.local().user("nobody"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
    });
}

#[test]
fn nothing_listening_is_a_connect_error() {
    run(|_| async move {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = Client::connect(&Config::new().host("127.0.0.1").port(port).user("u"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains(&format!("127.0.0.1:{port}")), "{e}");
    });
}

#[test]
fn a_server_that_never_answers_hits_the_connect_timeout() {
    run(|_| async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _held = giong::spawn(async move {
            let _conn = listener.accept().await;
            giong::time::sleep(Duration::from_secs(10)).await;
        });
        let started = Instant::now();
        let config = Config::new()
            .host("127.0.0.1")
            .port(port)
            .user("u")
            .connect_timeout(Duration::from_millis(200));
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("connect timeout"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

/// A fake server that answers the TLS request with `answer`.
async fn tls_answer(answer: &'static [u8]) -> giong_postgres::Error {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server = giong::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let request = conn.recv().await.unwrap();
        assert_eq!(request.len(), 8);
        drop(request);
        conn.write_all(answer).await.0.unwrap();
        giong::time::sleep(Duration::from_secs(5)).await;
    });
    let connector = TlsConnector::with_root_pem(
        rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .as_bytes(),
    )
    .unwrap();
    let config = Config::new()
        .host("127.0.0.1")
        .port(port)
        .user("u")
        .tls(Tls::Verify(connector));
    Client::connect(&config).await.unwrap_err()
}

#[test]
fn a_server_without_tls_or_with_extra_bytes_is_refused() {
    run(|_| async move {
        let e = tls_answer(b"N").await;
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("sslmode=disable"), "{e}");
        // Bytes after the 'S' would be taken as coming through TLS.
        let e = tls_answer(b"SX").await;
        assert_eq!(e.kind(), ErrorKind::Protocol, "{e}");
    });
}

/// This machine's address on its default route, if it has one.
fn non_loopback_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn a_clear_text_password_is_never_sent_unencrypted_over_the_network() {
    run(|_| async move {
        let Some(ip) = non_loopback_address() else {
            eprintln!("skipping: no non-loopback address");
            return;
        };
        let listener = TcpListener::bind((ip, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = giong::spawn(async move {
            let conn = listener.accept().await.unwrap();
            drop(conn.recv().await.unwrap()); // the startup message
                                              // AuthenticationCleartextPassword
            conn.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 3][..])
                .await
                .0
                .unwrap();
            let next = conn.recv().await.unwrap();
            assert!(
                next.is_empty(),
                "the client sent something after the request: {:?}",
                &next[..]
            );
        });
        let config = Config::new()
            .host(&ip.to_string())
            .port(addr.port())
            .user("u")
            .password("secret")
            .tls(Tls::Disable);
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert!(e.to_string().contains("clear text"), "{e}");
        server.await.unwrap();
    });
}
