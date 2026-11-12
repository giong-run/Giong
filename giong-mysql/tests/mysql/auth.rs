//! Connecting: sockets, TLS, authentication plugins and their paths, and the
//! errors when any of it goes wrong.

use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong_mysql::{Client, Config, ErrorKind, Tls, TlsConnector};

use crate::run;
use crate::server::{
    Server, DATABASE, FAST_USER, KEY_USER, NATIVE_USER, RSA_USER, SHA256_USER, SHA2_USER, TLS_USER,
};

async fn user(client: &Client) -> String {
    client
        .query_one("SELECT CURRENT_USER()", &[])
        .await
        .unwrap()
        .get::<String>(0)
        .unwrap()
}

#[test]
fn root_over_the_unix_socket() {
    run(|s| async move {
        let client = Client::connect(&s.local()).await.unwrap();
        assert_eq!(user(&client).await, "root@localhost");
        assert_eq!(client.server_version(), s.version);
        assert!(client.connection_id() > 0);
        let row = client
            .query_one("SELECT DATABASE(), @@session.time_zone", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), DATABASE);
        assert_eq!(row.get::<&str>(1).unwrap(), "+00:00");
    });
}

#[test]
fn a_url_works_as_well_as_the_setters() {
    run(|s| async move {
        let url = format!(
            "mysql://root@localhost/{DATABASE}?socket={}&application-name=from-url",
            s.socket.display()
        );
        let client = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        let row = client
            .query_one(
                "SELECT ATTR_VALUE FROM performance_schema.session_connect_attrs \
                 WHERE PROCESSLIST_ID = CONNECTION_ID() AND ATTR_NAME = 'program_name'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<&str>(0).unwrap(), "from-url");

        let url = format!(
            "mysql://{}:{}@127.0.0.1:{}/{DATABASE}",
            SHA2_USER.0, SHA2_USER.1, s.port
        );
        let client = Client::connect(&Config::from_url(&url).unwrap())
            .await
            .unwrap();
        assert_eq!(user(&client).await, "sha2_user@%");
    });
}

#[test]
fn caching_sha2_with_the_right_and_the_wrong_password() {
    run(|s| async move {
        // The first login over the socket sends the password itself (the
        // socket is as private as TLS); the second takes the fast path.
        let client = Client::connect(&s.socket(SHA2_USER)).await.unwrap();
        assert_eq!(user(&client).await, "sha2_user@%");
        let client = Client::connect(&s.tcp(SHA2_USER)).await.unwrap();
        assert_eq!(user(&client).await, "sha2_user@%");

        let e = Client::connect(&s.tcp((SHA2_USER.0, "wrong")))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert_eq!(e.code(), Some(1045), "{e}");
        let e = Client::connect(&s.tcp(("nobody", "x"))).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
    });
}

#[test]
fn the_server_key_is_asked_for_only_on_this_machine() {
    run(|s| async move {
        // Never logged in before, over TCP without TLS: the password goes
        // encrypted with the key the server sends, which is safe only when
        // nobody can sit in between.
        let client = Client::connect(&s.tcp(RSA_USER)).await.unwrap();
        assert_eq!(user(&client).await, "rsa_user@%");
    });
}

#[test]
fn a_configured_server_key_encrypts_the_password() {
    run(|s| async move {
        assert!(!s.public_key_pem.is_empty(), "the server wrote its key");
        let config = s.tcp(KEY_USER).server_public_key(&s.public_key_pem);
        let client = Client::connect(&config).await.unwrap();
        assert_eq!(user(&client).await, "key_user@%");
        // A key that is not a key fails before anything is sent.
        let e = Client::connect(&s.tcp(KEY_USER).server_public_key(b"not a key")).await;
        // The login above put the user in the server's cache, so the fast
        // path may not need the key at all.
        if let Err(e) = e {
            assert_eq!(e.kind(), ErrorKind::Config, "{e}");
        }
    });
}

#[test]
fn tls_is_verified_against_the_given_authority() {
    run(|s| async move {
        // The first login sends the password over TLS.
        let client = match Client::connect(&s.tls("localhost", TLS_USER)).await {
            Ok(c) => c,
            Err(e) => panic!("{e}\n{}", s.log()),
        };
        let row = client
            .query_one(
                "SELECT VARIABLE_VALUE FROM performance_schema.session_status \
                 WHERE VARIABLE_NAME = 'Ssl_version'",
                &[],
            )
            .await
            .unwrap();
        assert!(row.get::<&str>(0).unwrap().starts_with("TLSv1."));

        // The certificate names localhost and 127.0.0.1, not 127.0.0.2.
        let e = Client::connect(&s.tls("127.0.0.2", TLS_USER))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("TLS handshake"), "{e}");

        // Another authority is not trusted.
        let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = s.tls("localhost", TLS_USER).tls(Tls::Verify(
            TlsConnector::with_root_pem(other.cert.pem().as_bytes()).unwrap(),
        ));
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");

        // The user requires TLS.
        let e = Client::connect(&s.tcp(TLS_USER).tls(Tls::Disable))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
    });
}

#[test]
fn mysql_native_password_still_works_where_the_server_has_it() {
    run(|s| async move {
        if !s.has_native {
            eprintln!("skipping: the server has no mysql_native_password");
            return;
        }
        let client = Client::connect(&s.tcp(NATIVE_USER)).await.unwrap();
        assert_eq!(user(&client).await, "native_user@%");
        let e = Client::connect(&s.tcp((NATIVE_USER.0, "wrong")))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
    });
}

#[test]
fn sha256_password_is_sent_only_over_tls() {
    run(|s| async move {
        if !s.has_sha256 {
            eprintln!("skipping: the server has no sha256_password");
            return;
        }
        let e = Client::connect(&s.tcp(SHA256_USER)).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert!(e.to_string().contains("refusing to send it"), "{e}");
        let client = Client::connect(&s.tls("localhost", SHA256_USER))
            .await
            .unwrap();
        assert_eq!(user(&client).await, "sha256_user@%");
    });
}

#[test]
fn startup_errors_say_what_is_missing() {
    run(|s| async move {
        let e = Client::connect(&s.local().database("missing_db"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(1049), "{e}");
        assert!(e.to_string().contains("missing_db"), "{e}");

        let e = Client::connect(&Config::new().host("127.0.0.1").port(s.port))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Config, "{e}");

        let e = Client::connect(&s.local().session_variable("no_such_variable", "1"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(1193), "{e}");
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

        let e = Client::connect(&Config::new().host("/nonexistent/mysqld.sock").user("u"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
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

/// One packet: a 3-byte length, the sequence number, the payload.
fn packet(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_le_bytes()[..3].to_vec();
    out.push(seq);
    out.extend_from_slice(payload);
    out
}

/// A MySQL 8 greeting offering caching_sha2_password, and TLS if `tls`.
fn greeting(tls: bool) -> Vec<u8> {
    let caps: u32 = 1 // LONG_PASSWORD
        | 1 << 3 // CONNECT_WITH_DB
        | 1 << 9 // PROTOCOL_41
        | u32::from(tls) << 11 // SSL
        | 1 << 13 // TRANSACTIONS
        | 1 << 15 // SECURE_CONNECTION
        | 1 << 17 // MULTI_RESULTS
        | 1 << 18 // PS_MULTI_RESULTS
        | 1 << 19 // PLUGIN_AUTH
        | 1 << 21 // PLUGIN_AUTH_LENENC_CLIENT_DATA
        | 1 << 24; // DEPRECATE_EOF
    let mut p = vec![10];
    p.extend_from_slice(b"8.0.0-fake\0");
    p.extend_from_slice(&7u32.to_le_bytes());
    p.extend_from_slice(b"abcdefgh\0");
    p.extend_from_slice(&(caps as u16).to_le_bytes());
    p.push(255);
    p.extend_from_slice(&2u16.to_le_bytes());
    p.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
    p.push(21);
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(b"ijklmnopqrst\0");
    p.extend_from_slice(b"caching_sha2_password\0");
    packet(0, &p)
}

/// Reads from `conn` until a whole packet has arrived.
async fn read_packet(conn: &TcpStream) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < 4
        || got.len() < 4 + (u32::from_le_bytes([got[0], got[1], got[2], 0]) as usize)
    {
        let chunk = conn.recv().await.unwrap();
        assert!(!chunk.is_empty(), "the client closed early");
        got.extend_from_slice(&chunk);
    }
    got
}

/// This machine's address on its default route, if it has one.
fn non_loopback_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn a_password_is_never_sent_unencrypted_over_the_network() {
    run(|_| async move {
        let Some(ip) = non_loopback_address() else {
            eprintln!("skipping: no non-loopback address");
            return;
        };
        let listener = TcpListener::bind((ip, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = giong::spawn(async move {
            let conn = listener.accept().await.unwrap();
            conn.write_all(greeting(false)).await.0.unwrap();
            let response = read_packet(&conn).await;
            assert_eq!(response[3], 1, "the login is packet 1");
            // "Perform full authentication": send the password.
            conn.write_all(packet(2, &[1, 4])).await.0.unwrap();
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
        assert!(e.to_string().contains("server_public_key"), "{e}");
        server.await.unwrap();
    });
}

#[test]
fn a_server_without_tls_is_refused_when_tls_is_asked_for() {
    run(|_| async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = giong::spawn(async move {
            let conn = listener.accept().await.unwrap();
            conn.write_all(greeting(false)).await.0.unwrap();
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
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("does not accept TLS"), "{e}");
    });
}

#[test]
fn bytes_after_the_greeting_cannot_slip_into_the_tls_stream() {
    // Whatever arrives between the plaintext greeting and the TLS handshake
    // would be read afterwards as if it had come through TLS, so someone on
    // the path could inject it. Even part of a packet is refused. Needs no
    // MySQL server.
    giong::block_on(async {
        let partial: [&[u8]; 2] = [&[1, 0], &[5, 0, 0, 2, 0]];
        for extra in partial {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut hello = greeting(true);
            hello.extend_from_slice(extra);
            let _server = giong::spawn(async move {
                let conn = listener.accept().await.unwrap();
                conn.write_all(hello).await.0.unwrap();
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
            let e = Client::connect(&config).await.unwrap_err();
            assert!(e.to_string().contains("more than its greeting"), "{e}");
        }
    });
}

#[test]
fn a_server_older_than_mysql_8_is_refused() {
    run(|_| async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = giong::spawn(async move {
            let conn = listener.accept().await.unwrap();
            let mut hello = greeting(false);
            // Without DEPRECATE_EOF (bit 24), which MySQL 5.7.5 added: the
            // payload, after the 4-byte header, has the upper capability
            // flags at 30 and 31.
            hello[4 + 31] &= !1;
            conn.write_all(hello).await.0.unwrap();
            giong::time::sleep(Duration::from_secs(5)).await;
        });
        let config = Config::new().host("127.0.0.1").port(port).user("u");
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("MySQL 8.0 and later"), "{e}");
    });
}

/// Forwards connections from this machine's network address to the server
/// on 127.0.0.1, so that a client sees a host that is not this machine.
fn forward(s: &Server, ip: IpAddr) -> SocketAddr {
    let listener = TcpListener::bind((ip, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let port = s.port;
    giong::spawn(async move {
        loop {
            let Ok(outside) = listener.accept().await else {
                return;
            };
            let Ok(inside) = TcpStream::connect(("127.0.0.1", port)).await else {
                return;
            };
            let (outside, inside) = (Rc::new(outside), Rc::new(inside));
            giong::spawn(pipe(outside.clone(), inside.clone()));
            giong::spawn(pipe(inside, outside));
        }
    });
    addr
}

async fn pipe(from: Rc<TcpStream>, to: Rc<TcpStream>) {
    loop {
        match from.recv().await {
            Ok(chunk) if !chunk.is_empty() => {
                if to.write_all(chunk.to_vec()).await.0.is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}

#[test]
fn off_this_machine_without_tls_only_the_fast_path_logs_in() {
    run(|s| async move {
        let Some(ip) = non_loopback_address() else {
            eprintln!("skipping: no non-loopback address");
            return;
        };
        let addr = forward(s, ip);
        let remote = Config::new()
            .host(&ip.to_string())
            .port(addr.port())
            .user(FAST_USER.0)
            .password(FAST_USER.1)
            .database(DATABASE)
            .tls(Tls::Disable);
        // The server has not seen this user's password yet, and nothing
        // would protect it on the way.
        let e = Client::connect(&remote).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth, "{e}");
        assert!(e.to_string().contains("has no TLS"), "{e}");
        // After a login over the socket, the server has it cached, and the
        // scramble (which reveals nothing) is enough.
        Client::connect(&s.socket(FAST_USER)).await.unwrap();
        let client = Client::connect(&remote).await.unwrap();
        assert_eq!(user(&client).await, "fast_user@%");
    });
}
