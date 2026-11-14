//! A second server, set up the way some production servers are and the
//! defaults are not (see `server::odd`), and the two servers together.

use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::sleep;
use giong_mysql::{Client, Config, ErrorKind, Pool, Tls, TlsConnector};

use crate::server::{Server, DATABASE, SHA2_USER, TLS_USER};
use crate::{pipe, run_odd};

/// Sets a global variable, and puts it back when dropped.
struct Global<'a> {
    server: &'a Server,
    name: &'static str,
    old: String,
}

impl<'a> Global<'a> {
    fn set(server: &'a Server, name: &'static str, value: &str) -> Self {
        let old = server
            .sql(&format!("SELECT @@global.{name}"))
            .trim()
            .to_owned();
        server.sql(&format!("SET GLOBAL {name} = {value}"));
        Self { server, name, old }
    }
}

impl Drop for Global<'_> {
    fn drop(&mut self) {
        self.server
            .sql(&format!("SET GLOBAL {} = {}", self.name, self.old));
    }
}

/// Prepared statements the server holds, for all its clients.
fn prepared_count(server: &Server) -> u64 {
    let out = server.sql("SHOW GLOBAL STATUS LIKE 'Prepared_stmt_count'");
    out.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Waits until the server holds at most `n` prepared statements (the
/// statements of a closed connection go when the server notices).
async fn prepared_at_most(server: &Server, n: u64) {
    for _ in 0..500 {
        if prepared_count(server) <= n {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the server still holds {} statements",
        prepared_count(server)
    );
}

const TEXT: &str = "Tiếng Việt 😀";
const TEXT_HEX: &str = "5469E1BABF6E67205669E1BB877420F09F9880";

#[test]
fn text_is_utf8mb4_where_the_server_would_make_it_latin1() {
    run_odd(|_, odd| async move {
        // root is exempt from init_connect: this user is not.
        let client = Client::connect(&odd.tcp(SHA2_USER)).await.unwrap();
        let row = client
            .query_one(
                "SELECT @@character_set_client, @@character_set_connection, \
                 @@character_set_results",
                &[],
            )
            .await
            .unwrap();
        for i in 0..3 {
            assert_eq!(row.get::<&str>(i).unwrap(), "utf8mb4", "column {i}");
        }
        client
            .batch_execute(
                "DROP TABLE IF EXISTS odd_text; \
                 CREATE TABLE odd_text (s VARCHAR(40)) CHARACTER SET utf8mb4",
            )
            .await
            .unwrap();
        // As a parameter, and in the text of a statement.
        client
            .execute("INSERT INTO odd_text VALUES (?)", &[&TEXT])
            .await
            .unwrap();
        client
            .batch_execute(&format!("INSERT INTO odd_text VALUES ('{TEXT}')"))
            .await
            .unwrap();
        let rows = client
            .query("SELECT s, HEX(s), CHAR_LENGTH(s) FROM odd_text", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.get::<&str>(0).unwrap(), TEXT);
            assert_eq!(row.get::<&str>(1).unwrap(), TEXT_HEX, "stored once encoded");
            assert_eq!(row.get::<i64>(2).unwrap(), 12);
        }
        // Another client reads the same characters.
        assert_eq!(
            odd.sql("SELECT DISTINCT HEX(s) FROM giong.odd_text").trim(),
            TEXT_HEX
        );
    });
}

#[test]
fn the_server_wide_limit_on_prepared_statements_is_handled() {
    run_odd(|_, odd| async move {
        prepared_at_most(odd, 0).await;
        let _limit = Global::set(odd, "max_prepared_stmt_count", "8");
        let pool = Pool::new(odd.tcp(SHA2_USER), 1);
        // More statements than the server holds: the ones not in use are
        // closed to make room.
        for i in 0..20i64 {
            let row = pool
                .query_one(&format!("SELECT {i}, ?"), &[&i])
                .await
                .unwrap();
            assert_eq!(row.get::<i64>(0).unwrap(), i);
        }
        let hits = pool.stats().statement_limit_errors;
        assert!(hits >= 1, "the limit was reached: {hits}");
        drop(pool);
        prepared_at_most(odd, 0).await;

        // Another client holds them all: nothing to close here, and the
        // error says what to do.
        let hog = Client::connect(&odd.tcp(SHA2_USER)).await.unwrap();
        for i in 0..8 {
            hog.query(&format!("SELECT {i}"), &[]).await.unwrap();
        }
        let client = Client::connect(&odd.tcp(SHA2_USER)).await.unwrap();
        let e = client.query("SELECT 'one more'", &[]).await.unwrap_err();
        assert_eq!(e.code(), Some(1461), "{e}");
        assert!(e.to_string().contains("max_prepared_stmt_count"), "{e}");
        assert!(e.to_string().contains("statement_cache_capacity"), "{e}");
        drop(hog);
        prepared_at_most(odd, 0).await;
        client.query("SELECT 'one more'", &[]).await.unwrap();
    });
}

/// The connection id a new connection to `port` gets, from its greeting.
async fn next_connection_id(port: u16) -> u32 {
    let conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut got = Vec::new();
    while got.len() < 4
        || got.len() < 4 + (u32::from_le_bytes([got[0], got[1], got[2], 0]) as usize)
    {
        let chunk = conn.recv().await.unwrap();
        assert!(!chunk.is_empty(), "the server closed early");
        got.extend_from_slice(&chunk);
    }
    // Protocol version, the server version up to its NUL, then the id.
    let at = 5 + got[5..].iter().position(|&b| b == 0).unwrap() + 1;
    u32::from_le_bytes(got[at..at + 4].try_into().unwrap())
}

/// A session on `server` whose connection id is `id`, if the server has not
/// given that id out yet: ids are given in order, so connections that only
/// read the greeting move its counter on.
async fn session_with_id(server: &Server, id: u32) -> Option<Client> {
    loop {
        let next = next_connection_id(server.port).await + 1;
        if next == id {
            break;
        }
        if next > id {
            return None;
        }
    }
    let client = Client::connect(&server.tcp(SHA2_USER)).await.unwrap();
    (client.connection_id() == id).then_some(client)
}

/// A balancer in front of `backends`: each new connection goes to the next
/// one in turn, as with DNS round robin or an L4 load balancer.
fn round_robin(backends: [u16; 2]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    giong::spawn(async move {
        for next in 0.. {
            let Ok(outside) = listener.accept().await else {
                return;
            };
            let backend = backends[next % backends.len()];
            let Ok(inside) = TcpStream::connect(("127.0.0.1", backend)).await else {
                return;
            };
            let (outside, inside) = (Rc::new(outside), Rc::new(inside));
            giong::spawn(pipe(outside.clone(), inside.clone()));
            giong::spawn(pipe(inside, outside));
        }
    });
    port
}

#[test]
fn a_cancel_request_never_stops_a_statement_on_another_server() {
    run_odd(|s, odd| async move {
        // The connection goes to the server further along in connection
        // ids, so that the other can be brought to the same id.
        let (first, second) =
            if next_connection_id(s.port).await >= next_connection_id(odd.port).await {
                (s, odd)
            } else {
                (odd, s)
            };
        // One more on the first, so that the second is behind it.
        next_connection_id(first.port).await;
        let port = round_robin([first.port, second.port]);
        let config = Config::new()
            .host("127.0.0.1")
            .port(port)
            .user(SHA2_USER.0)
            .password(SHA2_USER.1)
            .database(DATABASE);
        let client = Client::connect(&config).await.unwrap();
        // Someone else's session on the other server, with the same id.
        let victim = session_with_id(second, client.connection_id()).await;
        let busy = victim.map(|victim| {
            giong::spawn(async move {
                let row = victim.query_one("SELECT SLEEP(1)", &[]).await.unwrap();
                row.get::<i64>(0).unwrap()
            })
        });
        sleep(Duration::from_millis(100)).await;
        // The request reaches the other server through the balancer.
        let e = client.cancel_token().cancel().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("different server"), "{e}");
        match busy {
            Some(busy) => assert_eq!(busy.await.unwrap(), 0, "SLEEP ran to its end"),
            None => eprintln!("no session with the same id: checked the refusal only"),
        }
        // The next one reaches the connection's own server.
        client.cancel_token().cancel().await.unwrap();
        client.query_one("SELECT 1", &[]).await.unwrap();
    });
}

#[test]
fn a_lock_wait_timeout_that_rolls_the_transaction_back_ends_it() {
    run_odd(|_, odd| async move {
        // innodb_rollback_on_timeout is on: the timeout undoes the whole
        // transaction, not only the statement.
        let config = odd
            .tcp(SHA2_USER)
            .session_variable("innodb_lock_wait_timeout", "1");
        let mut a = Client::connect(&config).await.unwrap();
        let mut b = Client::connect(&config).await.unwrap();
        a.batch_execute(
            "DROP TABLE IF EXISTS odd_rot; CREATE TABLE odd_rot (id INT PRIMARY KEY); \
             INSERT INTO odd_rot VALUES (1)",
        )
        .await
        .unwrap();
        let tx_a = a.transaction().await.unwrap();
        tx_a.query("SELECT id FROM odd_rot WHERE id = 1 FOR UPDATE", &[])
            .await
            .unwrap();
        let tx_b = b.transaction().await.unwrap();
        tx_b.execute("INSERT INTO odd_rot VALUES (10)", &[])
            .await
            .unwrap();
        let e = tx_b
            .execute("UPDATE odd_rot SET id = 2 WHERE id = 1", &[])
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(1205), "{e}");
        // Not run outside the transaction, where it would commit at once.
        let e = tx_b
            .execute("INSERT INTO odd_rot VALUES (11)", &[])
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("rolled this transaction back"),
            "{e}"
        );
        assert!(tx_b.commit().await.is_err());
        tx_a.rollback().await.unwrap();
        let ids: Vec<i64> = a
            .query("SELECT id FROM odd_rot ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0).unwrap())
            .collect();
        assert_eq!(ids, [1]);
    });
}

#[test]
fn the_certificate_authority_alone_verifies_mysqls_own_certificates() {
    run_odd(|_, odd| async move {
        if !odd.auto_certificates {
            eprintln!("skipping: the server made no certificates of its own");
            return;
        }
        let ssl = "SHOW SESSION STATUS LIKE 'Ssl_version'";
        // Its certificate names no host: checking the name fails.
        let full = Tls::Verify(TlsConnector::with_root_pem(&odd.ca_pem).unwrap());
        let e = Client::connect(&odd.tcp(TLS_USER).tls(full))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        // The chain alone, against its authority, is checked and passes.
        let ca_only = Tls::verify_ca(&odd.ca_pem).unwrap();
        let client = Client::connect(&odd.tcp(TLS_USER).tls(ca_only))
            .await
            .unwrap();
        let row = client.query_one(ssl, &[]).await.unwrap();
        assert!(row.get::<&str>(1).unwrap().starts_with("TLSv1."));
        // As MySQL's own clients spell it.
        let url = |mode: &str| {
            format!(
                "mysql://{}:{}@127.0.0.1:{}/{DATABASE}?ssl-mode={mode}&ssl-ca={}",
                TLS_USER.0,
                TLS_USER.1,
                odd.port,
                odd.ca_file.display()
            )
        };
        for mode in ["VERIFY_CA", "REQUIRED"] {
            let config = Config::from_url(&url(mode)).unwrap();
            let client = Client::connect(&config)
                .await
                .unwrap_or_else(|e| panic!("{mode}: {e}"));
            client.query_one(ssl, &[]).await.unwrap();
        }
        let config = Config::from_url(&url("VERIFY_IDENTITY")).unwrap();
        let e = Client::connect(&config).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
    });
}
