//! giong-redis against an in-process fake Redis server (RESP3), under
//! faults: a reply must never go to the wrong command, every run must end.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong_redis::{Config, ErrorKind, Pool};
use giong_sim::{Faults, Sim};

/// Parses one RESP command (an array of bulk strings) from the front of
/// `buf`; `None` if incomplete.
fn parse(buf: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    fn line(buf: &[u8], at: usize) -> Option<(&[u8], usize)> {
        let end = buf[at..].windows(2).position(|w| w == b"\r\n")? + at;
        Some((&buf[at..end], end + 2))
    }
    let (head, mut at) = line(buf, 0)?;
    assert_eq!(head[0], b'*', "fake redis: not an array");
    let n: usize = std::str::from_utf8(&head[1..]).unwrap().parse().unwrap();
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        let (h, next) = line(buf, at)?;
        assert_eq!(h[0], b'$');
        let len: usize = std::str::from_utf8(&h[1..]).unwrap().parse().unwrap();
        if buf.len() < next + len + 2 {
            return None;
        }
        args.push(buf[next..next + len].to_vec());
        at = next + len + 2;
    }
    Some((args, at))
}

fn bulk(v: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", v.len()).into_bytes();
    out.extend_from_slice(v);
    out.extend_from_slice(b"\r\n");
    out
}

fn reply(store: &RefCell<HashMap<Vec<u8>, Vec<u8>>>, id: i64, args: &[Vec<u8>]) -> Vec<u8> {
    let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
    match name.as_str() {
        "HELLO" => {
            let mut out = b"%7\r\n".to_vec();
            for (k, v) in [("server", "redis"), ("version", "7.4.0")] {
                out.extend(bulk(k.as_bytes()));
                out.extend(bulk(v.as_bytes()));
            }
            out.extend(bulk(b"proto"));
            out.extend(b":3\r\n");
            out.extend(bulk(b"id"));
            out.extend(format!(":{id}\r\n").into_bytes());
            out.extend(bulk(b"mode"));
            out.extend(bulk(b"standalone"));
            out.extend(bulk(b"role"));
            out.extend(bulk(b"master"));
            out.extend(bulk(b"modules"));
            out.extend(b"*0\r\n");
            out
        }
        "PING" => b"+PONG\r\n".to_vec(),
        "SET" => {
            store.borrow_mut().insert(args[1].clone(), args[2].clone());
            b"+OK\r\n".to_vec()
        }
        "GET" => match store.borrow().get(&args[1]) {
            Some(v) => bulk(v),
            None => b"_\r\n".to_vec(),
        },
        "CLIENT" => format!(":{id}\r\n").into_bytes(),
        _ => format!("-ERR unknown command '{name}'\r\n").into_bytes(),
    }
}

async fn serve_conn(stream: TcpStream, store: Rc<RefCell<HashMap<Vec<u8>, Vec<u8>>>>, id: i64) {
    let mut buf = Vec::new();
    loop {
        let (res, chunk) = stream.read(vec![0u8; 4096]).await;
        match res {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return,
        }
        let mut out = Vec::new();
        while let Some((args, used)) = parse(&buf) {
            buf.drain(..used);
            out.extend(reply(&store, id, &args));
        }
        if !out.is_empty() && stream.write_all(out).await.0.is_err() {
            return;
        }
    }
}

/// A fake server and 8 tasks doing SET then GET through a pool of 3
/// connections. Returns (ok, errors).
fn workload() -> (usize, usize) {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:6379").unwrap();
        let store = Rc::new(RefCell::new(HashMap::new()));
        giong::spawn(async move {
            let mut id = 0;
            loop {
                let Ok(s) = listener.accept().await else {
                    continue;
                };
                id += 1;
                giong::spawn(serve_conn(s, store.clone(), id));
            }
        });
        let config = Config::new()
            .host("127.0.0.1")
            .port(6379)
            .connect_timeout(Duration::from_secs(5))
            .response_timeout(Duration::from_secs(5));
        let pool = Rc::new(Pool::new(config, 3));
        let tasks: Vec<_> = (0..8)
            .map(|t| {
                let pool = pool.clone();
                giong::spawn(async move {
                    let (mut ok, mut err) = (0, 0);
                    for i in 0..25 {
                        let key = format!("k{t}-{i}");
                        let val = format!("v{t}-{i}-{}", "x".repeat(i * 37));
                        match pool
                            .query::<()>(&giong_redis::cmd("SET").arg(&key).arg(&val))
                            .await
                        {
                            Ok(()) => {}
                            Err(e) => {
                                assert!(
                                    matches!(
                                        e.kind(),
                                        ErrorKind::Closed
                                            | ErrorKind::Connect
                                            | ErrorKind::Timeout
                                            | ErrorKind::PoolTimeout
                                    ),
                                    "unexpected error kind: {e}"
                                );
                                err += 1;
                                continue;
                            }
                        }
                        match pool
                            .query::<Option<String>>(&giong_redis::cmd("GET").arg(&key))
                            .await
                        {
                            Ok(got) => {
                                assert_eq!(
                                    got.as_deref(),
                                    Some(val.as_str()),
                                    "a reply went to the wrong command"
                                );
                                ok += 1;
                            }
                            Err(e) => {
                                assert!(
                                    matches!(
                                        e.kind(),
                                        ErrorKind::Closed
                                            | ErrorKind::Connect
                                            | ErrorKind::Timeout
                                            | ErrorKind::PoolTimeout
                                    ),
                                    "unexpected error kind: {e}"
                                );
                                err += 1;
                            }
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
        assert_eq!(workload(), (200, 0));
    }) {
        assert!(r.steps > 0);
    }
}

#[test]
fn hostile_faults_give_errors_never_wrong_answers() {
    let seeds = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100u64);
    let mut sum = (0, 0);
    for _ in giong_sim::sweep(0..seeds, Faults::hostile(), |_| {
        let (ok, err) = workload();
        assert_eq!(ok + err, 200);
    }) {}
    let ((), r) = Sim::new(3).faults(Faults::hostile()).run(|| {
        sum = workload();
    });
    eprintln!("seed 3: {sum:?}, {r:?}");
}
