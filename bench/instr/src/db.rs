//! The database cases: each driver's client against a fake server in the
//! same process, which answers with fixed bytes. The fakes speak just
//! enough of each protocol for a login and the one statement measured, so
//! that no database server is needed and the count is the client's (plus a
//! few dozen instructions of fake per request).

use giong::net::TcpStream;

use crate::Meter;

/// Reads from `stream` into `pending` until `complete` finds whole
/// requests in it; `false` once the client has gone.
async fn read_more(stream: &TcpStream, pending: &mut Vec<u8>) -> bool {
    match stream.recv().await {
        Ok(data) if !data.is_empty() => {
            pending.extend_from_slice(&data);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- Redis

/// The length of the first whole RESP command (an array of bulk strings)
/// in `buf`, if there is one.
fn resp_command(buf: &[u8]) -> Option<usize> {
    fn line(buf: &[u8], at: usize) -> Option<(usize, usize)> {
        let end = at + buf.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
        let n = std::str::from_utf8(&buf[at + 1..end]).ok()?.parse().ok()?;
        Some((n, end + 2))
    }
    if buf.is_empty() {
        return None;
    }
    assert_eq!(buf[0], b'*', "a RESP array");
    let (items, mut at) = line(buf, 0)?;
    for _ in 0..items {
        let (len, next) = line(buf, at)?;
        at = next + len + 2;
        if buf.len() < at {
            return None;
        }
    }
    Some(at)
}

const HELLO_REPLY: &[u8] = b"%4\r\n$6\r\nserver\r\n$5\r\nredis\r\n$7\r\nversion\r\n$5\r\n7.2.4\r\n\
$2\r\nid\r\n:7\r\n$4\r\nmode\r\n$10\r\nstandalone\r\n";

pub async fn redis_get(m: &Meter) {
    let listener = giong::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("its address");
    let _server = giong::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut pending = Vec::new();
        let mut first = true;
        loop {
            while let Some(len) = resp_command(&pending) {
                pending.drain(..len);
                let reply: &'static [u8] = if first {
                    HELLO_REPLY
                } else {
                    b"$5\r\nhello\r\n"
                };
                first = false;
                if stream.write_all(reply).await.0.is_err() {
                    return;
                }
            }
            if pending.first().is_some_and(|&b| b != b'*') {
                panic!("the fake Redis got {:?}", String::from_utf8_lossy(&pending));
            }
            if !read_more(&stream, &mut pending).await {
                return;
            }
        }
    });
    let config = giong_redis::Config::from_url(&format!("redis://{addr}")).expect("a URL");
    let client = giong_redis::Client::connect(&config)
        .await
        .expect("connect");
    for _ in m.iter() {
        let v: Option<String> = client.get("key").await.expect("GET");
        assert_eq!(v.as_deref(), Some("hello"));
    }
}

// ----------------------------------------------------------- PostgreSQL

fn pg_message(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
    out.extend_from_slice(body);
}

/// The fields of the one statement's result: `id int4`, `name text`.
fn pg_row_description(out: &mut Vec<u8>) {
    let mut body = 2i16.to_be_bytes().to_vec();
    for (name, oid, len) in [("id", 23i32, 4i16), ("name", 25, -1)] {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table
        body.extend_from_slice(&0i16.to_be_bytes()); // column
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&len.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes()); // modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // format
    }
    pg_message(out, b'T', &body);
}

/// The result formats a `Bind` body asks for: one per column, or one for
/// all, or none (text).
fn pg_bind_formats(body: &[u8]) -> Vec<bool> {
    let mut at = body.iter().position(|&b| b == 0).expect("portal") + 1;
    at += body[at..].iter().position(|&b| b == 0).expect("statement") + 1;
    let n = u16::from_be_bytes([body[at], body[at + 1]]) as usize;
    at += 2 + 2 * n;
    let values = u16::from_be_bytes([body[at], body[at + 1]]) as usize;
    at += 2;
    for _ in 0..values {
        let len = i32::from_be_bytes(body[at..at + 4].try_into().unwrap());
        at += 4 + len.max(0) as usize;
    }
    let n = u16::from_be_bytes([body[at], body[at + 1]]) as usize;
    (0..n).map(|i| body[at + 2 + 2 * i + 1] == 1).collect()
}

pub async fn postgres_query(m: &Meter) {
    let listener = giong::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("its address");
    let _server = giong::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut pending = Vec::new();
        // The startup message: a length, then the protocol version.
        while pending.len() < 4
            || pending.len() < u32::from_be_bytes(pending[..4].try_into().unwrap()) as usize
        {
            if !read_more(&stream, &mut pending).await {
                return;
            }
        }
        let len = u32::from_be_bytes(pending[..4].try_into().unwrap()) as usize;
        assert_eq!(
            &pending[4..8],
            &196608u32.to_be_bytes(),
            "a startup message (protocol 3.0)"
        );
        pending.drain(..len);
        let mut out = Vec::new();
        pg_message(&mut out, b'R', &0i32.to_be_bytes());
        for (k, v) in [
            ("server_version", "16.4"),
            ("client_encoding", "UTF8"),
            ("in_hot_standby", "off"),
        ] {
            pg_message(&mut out, b'S', format!("{k}\0{v}\0").as_bytes());
        }
        pg_message(&mut out, b'K', &[0, 0, 0, 1, 0, 0, 0, 2]);
        pg_message(&mut out, b'Z', b"I");
        if stream.write_all(out).await.0.is_err() {
            return;
        }
        let mut binary = Vec::new();
        let mut out = Vec::new();
        loop {
            while pending.len() >= 5 {
                let len = i32::from_be_bytes(pending[1..5].try_into().unwrap()) as usize;
                if pending.len() < 1 + len {
                    break;
                }
                let tag = pending[0];
                let body = &pending[5..1 + len];
                match tag {
                    b'P' => pg_message(&mut out, b'1', &[]),
                    b'D' => {
                        if body[0] == b'S' {
                            let mut p = 1i16.to_be_bytes().to_vec();
                            p.extend_from_slice(&23i32.to_be_bytes());
                            pg_message(&mut out, b't', &p);
                        }
                        pg_row_description(&mut out);
                    }
                    b'B' => {
                        binary = pg_bind_formats(body);
                        pg_message(&mut out, b'2', &[]);
                    }
                    b'E' => {
                        let bin = |i: usize| match binary.len() {
                            0 => false,
                            1 => binary[0],
                            _ => binary[i],
                        };
                        let mut row = 2i16.to_be_bytes().to_vec();
                        if bin(0) {
                            row.extend_from_slice(&4i32.to_be_bytes());
                            row.extend_from_slice(&42i32.to_be_bytes());
                        } else {
                            row.extend_from_slice(&2i32.to_be_bytes());
                            row.extend_from_slice(b"42");
                        }
                        row.extend_from_slice(&5i32.to_be_bytes());
                        row.extend_from_slice(b"hello");
                        pg_message(&mut out, b'D', &row);
                        pg_message(&mut out, b'C', b"SELECT 1\0");
                    }
                    b'C' => pg_message(&mut out, b'3', &[]),
                    b'S' => {
                        pg_message(&mut out, b'Z', b"I");
                        if stream.write_all(std::mem::take(&mut out)).await.0.is_err() {
                            return;
                        }
                    }
                    b'X' => return,
                    other => panic!("the fake PostgreSQL got message {:?}", other as char),
                }
                pending.drain(..1 + len);
            }
            if !read_more(&stream, &mut pending).await {
                return;
            }
        }
    });
    let config =
        giong_postgres::Config::from_url(&format!("postgres://app@{addr}/shop?sslmode=disable"))
            .expect("a URL");
    let client = giong_postgres::Client::connect(&config)
        .await
        .expect("connect");
    for _ in m.iter() {
        let rows = client
            .query("SELECT id, name FROM users WHERE id = $1", &[&42i32])
            .await
            .expect("query");
        let row = rows.first().expect("one row");
        let id: i32 = row.get(0).expect("id");
        let name: &str = row.get(1).expect("name");
        assert_eq!((id, name), (42, "hello"));
    }
}

// ---------------------------------------------------------------- MySQL

fn my_packet(out: &mut Vec<u8>, seq: u8, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes()[..3]);
    out.push(seq);
    out.extend_from_slice(payload);
}

/// A column definition: `name` of MySQL type `kind`.
fn my_column(name: &str, kind: u8, charset: u16, len: u32, flags: u16) -> Vec<u8> {
    let mut p = Vec::new();
    for s in ["def", "shop", "users", "users", name, name] {
        p.push(s.len() as u8);
        p.extend_from_slice(s.as_bytes());
    }
    p.push(0x0C);
    p.extend_from_slice(&charset.to_le_bytes());
    p.extend_from_slice(&len.to_le_bytes());
    p.push(kind);
    p.extend_from_slice(&flags.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0]);
    p
}

/// OK (`0x00`) or, ending rows, `0xFE`, with status AUTOCOMMIT.
fn my_ok(first: u8) -> Vec<u8> {
    vec![first, 0, 0, 2, 0, 0, 0]
}

const MY_CAPS: u32 = 1 // LONG_PASSWORD
    | 1 << 1 // FOUND_ROWS
    | 1 << 2 // LONG_FLAG
    | 1 << 3 // CONNECT_WITH_DB
    | 1 << 9 // PROTOCOL_41
    | 1 << 13 // TRANSACTIONS
    | 1 << 15 // SECURE_CONNECTION
    | 1 << 16 // MULTI_STATEMENTS
    | 1 << 17 // MULTI_RESULTS
    | 1 << 18 // PS_MULTI_RESULTS
    | 1 << 19 // PLUGIN_AUTH
    | 1 << 20 // CONNECT_ATTRS
    | 1 << 21 // PLUGIN_AUTH_LENENC_CLIENT_DATA
    | 1 << 24; // DEPRECATE_EOF

pub async fn mysql_query(m: &Meter) {
    let listener = giong::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("its address");
    let _server = giong::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut greeting = vec![10];
        greeting.extend_from_slice(b"8.0.36\0");
        greeting.extend_from_slice(&7u32.to_le_bytes());
        greeting.extend_from_slice(b"abcdefgh\0");
        greeting.extend_from_slice(&(MY_CAPS as u16).to_le_bytes());
        greeting.push(255);
        greeting.extend_from_slice(&2u16.to_le_bytes());
        greeting.extend_from_slice(&((MY_CAPS >> 16) as u16).to_le_bytes());
        greeting.push(21);
        greeting.extend_from_slice(&[0; 10]);
        greeting.extend_from_slice(b"ijklmnopqrst\0");
        greeting.extend_from_slice(b"caching_sha2_password\0");
        let mut out = Vec::new();
        my_packet(&mut out, 0, &greeting);
        if stream.write_all(out).await.0.is_err() {
            return;
        }
        let mut pending = Vec::new();
        let mut logged_in = false;
        let mut out = Vec::new();
        loop {
            while pending.len() >= 4 {
                let len = u32::from_le_bytes([pending[0], pending[1], pending[2], 0]) as usize;
                if pending.len() < 4 + len {
                    break;
                }
                let payload = pending[4..4 + len].to_vec();
                pending.drain(..4 + len);
                if !logged_in {
                    my_packet(&mut out, 2, &my_ok(0));
                    logged_in = true;
                } else {
                    match payload[0] {
                        // COM_QUERY: the session's SET, then its check.
                        0x03 if payload[1..].starts_with(b"SELECT @@character_set_client") => {
                            my_packet(&mut out, 1, &[5]);
                            for (i, name) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
                                my_packet(
                                    &mut out,
                                    2 + i as u8,
                                    &my_column(name, 0xFD, 255, 1024, 0),
                                );
                            }
                            let mut row = Vec::new();
                            for v in ["utf8mb4", "utf8mb4", "utf8mb4", "0", "fake-server"] {
                                row.push(v.len() as u8);
                                row.extend_from_slice(v.as_bytes());
                            }
                            my_packet(&mut out, 7, &row);
                            my_packet(&mut out, 8, &my_ok(0xFE));
                        }
                        0x03 => my_packet(&mut out, 1, &my_ok(0)),
                        // COM_STMT_PREPARE: statement 1, two columns, one parameter.
                        0x16 => {
                            let mut p = vec![0];
                            p.extend_from_slice(&1u32.to_le_bytes());
                            p.extend_from_slice(&2u16.to_le_bytes());
                            p.extend_from_slice(&1u16.to_le_bytes());
                            p.extend_from_slice(&[0, 0, 0]);
                            my_packet(&mut out, 1, &p);
                            my_packet(&mut out, 2, &my_column("?", 8, 63, 20, 0x80));
                            my_packet(&mut out, 3, &my_column("id", 3, 63, 11, 0x1001));
                            my_packet(&mut out, 4, &my_column("name", 0xFD, 255, 1024, 0));
                        }
                        // COM_STMT_EXECUTE: one row (42, "hello").
                        0x17 => {
                            my_packet(&mut out, 1, &[2]);
                            my_packet(&mut out, 2, &my_column("id", 3, 63, 11, 0x1001));
                            my_packet(&mut out, 3, &my_column("name", 0xFD, 255, 1024, 0));
                            let mut row = vec![0, 0];
                            row.extend_from_slice(&42i32.to_le_bytes());
                            row.push(5);
                            row.extend_from_slice(b"hello");
                            my_packet(&mut out, 4, &row);
                            my_packet(&mut out, 5, &my_ok(0xFE));
                        }
                        // COM_STMT_CLOSE: no answer.
                        0x19 => {}
                        0x01 => return,
                        other => panic!("the fake MySQL got command {other:#04x}"),
                    }
                }
            }
            if !out.is_empty() && stream.write_all(std::mem::take(&mut out)).await.0.is_err() {
                return;
            }
            if !read_more(&stream, &mut pending).await {
                return;
            }
        }
    });
    let config =
        giong_mysql::Config::from_url(&format!("mysql://app@{addr}/shop?ssl-mode=DISABLED"))
            .expect("a URL");
    let client = giong_mysql::Client::connect(&config)
        .await
        .expect("connect");
    for _ in m.iter() {
        let rows = client
            .query("SELECT id, name FROM users WHERE id = ?", &[&42i32])
            .await
            .expect("query");
        let row = rows.first().expect("one row");
        let id: i32 = row.get(0).expect("id");
        let name: &str = row.get(1).expect("name");
        assert_eq!((id, name), (42, "hello"));
    }
}
