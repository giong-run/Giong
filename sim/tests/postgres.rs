//! giong-postgres against an in-process fake PostgreSQL server that speaks
//! enough of the protocol (startup, simple and extended queries) to answer
//! `SELECT $1::text`, under faults: a row must never belong to another
//! query, every run must end.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong_postgres::{Config, Pool, Tls};
use giong_sim::Faults;

const TEXT: u32 = 25;

fn msg(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
    out.extend_from_slice(body);
}

fn cstr(b: &[u8], at: &mut usize) -> Vec<u8> {
    let end = b[*at..].iter().position(|c| *c == 0).unwrap() + *at;
    let s = b[*at..end].to_vec();
    *at = end + 1;
    s
}

fn i16_at(b: &[u8], at: &mut usize) -> i16 {
    let v = i16::from_be_bytes([b[*at], b[*at + 1]]);
    *at += 2;
    v
}

fn i32_at(b: &[u8], at: &mut usize) -> i32 {
    let v = i32::from_be_bytes(b[*at..*at + 4].try_into().unwrap());
    *at += 4;
    v
}

fn params_in(sql: &[u8]) -> usize {
    let s = String::from_utf8_lossy(sql);
    (1..=9).filter(|i| s.contains(&format!("${i}"))).count()
}

fn row_description(out: &mut Vec<u8>) {
    let mut b = 1i16.to_be_bytes().to_vec();
    b.extend_from_slice(b"v\0");
    b.extend_from_slice(&0i32.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&TEXT.to_be_bytes());
    b.extend_from_slice(&(-1i16).to_be_bytes());
    b.extend_from_slice(&(-1i32).to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    msg(out, b'T', &b);
}

async fn serve_conn(stream: TcpStream, pid: i32) {
    let mut buf: Vec<u8> = Vec::new();
    let mut started = false;
    let mut statements: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut portal: Option<Vec<u8>> = None;
    loop {
        let (res, chunk) = stream.read(vec![0u8; 8192]).await;
        match res {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return,
        }
        let mut out = Vec::new();
        loop {
            if !started {
                if buf.len() < 4 {
                    break;
                }
                let len = i32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
                if buf.len() < len {
                    break;
                }
                let code = i32::from_be_bytes(buf[4..8].try_into().unwrap());
                buf.drain(..len);
                if code == 80877103 {
                    out.push(b'N'); // no TLS
                    continue;
                }
                started = true;
                msg(&mut out, b'R', &0i32.to_be_bytes());
                for (k, v) in [
                    ("server_version", "16.4"),
                    ("in_hot_standby", "off"),
                    ("client_encoding", "UTF8"),
                ] {
                    msg(&mut out, b'S', format!("{k}\0{v}\0").as_bytes());
                }
                let mut key = pid.to_be_bytes().to_vec();
                key.extend_from_slice(&7i32.to_be_bytes());
                msg(&mut out, b'K', &key);
                msg(&mut out, b'Z', b"I");
                continue;
            }
            if buf.len() < 5 {
                break;
            }
            let len = i32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
            if buf.len() < 1 + len {
                break;
            }
            let tag = buf[0];
            let body: Vec<u8> = buf[5..1 + len].to_vec();
            buf.drain(..1 + len);
            let mut at = 0;
            match tag {
                b'P' => {
                    let name = cstr(&body, &mut at);
                    let sql = cstr(&body, &mut at);
                    statements.insert(name, params_in(&sql));
                    msg(&mut out, b'1', b"");
                }
                b'D' => {
                    let kind = body[0];
                    at = 1;
                    let name = cstr(&body, &mut at);
                    if kind == b'S' {
                        let n = statements.get(&name).copied().unwrap_or(0);
                        let mut b = (n as i16).to_be_bytes().to_vec();
                        for _ in 0..n {
                            b.extend_from_slice(&TEXT.to_be_bytes());
                        }
                        msg(&mut out, b't', &b);
                    }
                    row_description(&mut out);
                }
                b'B' => {
                    let _portal = cstr(&body, &mut at);
                    let _stmt = cstr(&body, &mut at);
                    let nf = i16_at(&body, &mut at);
                    at += 2 * nf as usize;
                    let np = i16_at(&body, &mut at);
                    let mut first = None;
                    for _ in 0..np {
                        let l = i32_at(&body, &mut at);
                        if l >= 0 {
                            let v = body[at..at + l as usize].to_vec();
                            at += l as usize;
                            first.get_or_insert(v);
                        }
                    }
                    portal = first;
                    msg(&mut out, b'2', b"");
                }
                b'E' => {
                    if let Some(v) = portal.take() {
                        let mut b = 1i16.to_be_bytes().to_vec();
                        b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                        b.extend_from_slice(&v);
                        msg(&mut out, b'D', &b);
                    }
                    msg(&mut out, b'C', b"SELECT 1\0");
                }
                b'C' => msg(&mut out, b'3', b""),
                b'S' => msg(&mut out, b'Z', b"I"),
                b'Q' => {
                    // Session resets and the like: answered as empty commands.
                    msg(&mut out, b'C', b"SET\0");
                    msg(&mut out, b'Z', b"I");
                }
                b'X' => return,
                b'H' => {}
                other => panic!("fake postgres: unexpected message {:?}", other as char),
            }
        }
        if !out.is_empty() && stream.write_all(out).await.0.is_err() {
            return;
        }
    }
}

/// A fake server and 6 tasks running 20 queries each through a pool of 3.
/// Returns (ok, errors).
fn workload() -> (usize, usize) {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:5432").unwrap();
        giong::spawn(async move {
            let mut pid = 100;
            loop {
                let Ok(s) = listener.accept().await else {
                    continue;
                };
                pid += 1;
                giong::spawn(serve_conn(s, pid));
            }
        });
        let config = Config::new()
            .host("127.0.0.1")
            .port(5432)
            .user("sim")
            .tls(Tls::Disable)
            .connect_timeout(Duration::from_secs(5))
            .statement_timeout(Duration::from_secs(5));
        let pool = Rc::new(Pool::new(config, 3));
        let tasks: Vec<_> = (0..6)
            .map(|t| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let (mut ok, mut err) = (0, 0);
                    for i in 0..20 {
                        let val = format!("t{t}-q{i}-{}", "y".repeat(i * 53));
                        match pool.query("SELECT $1::text", &[&val.as_str()]).await {
                            Ok(rows) => {
                                assert_eq!(rows.len(), 1);
                                let got: &str = rows[0].get(0).unwrap();
                                assert_eq!(got, val, "a row went to the wrong query");
                                ok += 1;
                            }
                            Err(_) => err += 1,
                        }
                    }
                    (ok, err)
                })
            })
            .collect();
        let mut total = (0, 0);
        for t in tasks {
            let (ok, err) = t.await.unwrap();
            total.0 += ok;
            total.1 += err;
        }
        total
    })
}

#[test]
fn benign_faults_never_reach_the_caller() {
    for r in giong_sim::sweep(0..30, Faults::benign(), |_| {
        assert_eq!(workload(), (120, 0));
    }) {
        assert!(r.steps > 0);
    }
}

#[test]
fn hostile_faults_give_errors_never_wrong_rows() {
    let seeds = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100u64);
    let mut errs = 0;
    for _ in giong_sim::sweep(0..seeds, Faults::hostile(), |_| {
        let (ok, err) = workload();
        assert_eq!(ok + err, 120);
    }) {
        errs += 1;
    }
    assert_eq!(errs, seeds as usize);
}
