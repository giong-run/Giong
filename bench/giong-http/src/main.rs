//! The benchmark routes on the `giong-http` library (as opposed to
//! `examples/http.rs`, which hand-rolls HTTP on the bare runtime).
use std::hint::black_box;
use std::io::Write;

use giong::net::TcpListener;
use giong_http::{Request, Response, Server};
use giong_tls::TlsAcceptor;

fn fib(n: u32) -> u64 { if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) } }

fn query_n(q: &str) -> u32 {
    q.split('&').find_map(|kv| kv.strip_prefix("n="))
        .map(|v| v.bytes().take_while(u8::is_ascii_digit).fold(0u32, |a, c| a * 10 + (c - b'0') as u32))
        .unwrap_or(25)
        .min(40)
}

fn query_ms(q: &str) -> u64 {
    q.split('&').find_map(|kv| kv.strip_prefix("ms=")).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The bodies of `/bytes?size=N`: up to 16 MiB, made once and never freed,
/// so a response costs no allocation or copy of its own and the send path is
/// what gets measured (bench/uring-features.sh).
fn bytes(q: &str) -> &'static [u8] {
    static BYTES: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
    let all = BYTES.get_or_init(|| Vec::leak((0..16 << 20).map(|i| b'a' + (i % 26) as u8).collect()));
    let size = q.split('&').find_map(|kv| kv.strip_prefix("size=")).and_then(|v| v.parse().ok()).unwrap_or(65536);
    &all[..all.len().min(size)]
}

async fn handler(req: Request) -> Response {
    match req.path() {
        "/json" => Response::json(r#"{"message":"Hello, World!"}"#),
        "/users" => {
            let mut body = Vec::with_capacity(8500);
            body.push(b'[');
            for i in 0..100u32 {
                if i > 0 { body.push(b','); }
                let _ = write!(body, r#"{{"id":{i},"name":"User {i}","email":"user{i}@example.com","active":{},"score":{}}}"#,
                    i % 2 == 0, i as f64 * 1.5);
            }
            body.push(b']');
            Response::json(body)
        }
        "/compute" => {
            let n = query_n(req.query().unwrap_or(""));
            Response::json(format!(r#"{{"result":{}}}"#, fib(black_box(n))))
        }
        // `/wait?ms=N` answers after N milliseconds, like a handler waiting
        // for a database or another service (bench/memory.sh).
        "/wait" => {
            giong::time::sleep(std::time::Duration::from_millis(query_ms(req.query().unwrap_or("")))).await;
            Response::json(r#"{"message":"Hello, World!"}"#)
        }
        // `/stream?size=N&chunk=C`: N bytes as a streamed body of C-byte
        // pieces, each sent once the one before it is written.
        "/stream" => {
            let q = req.query().unwrap_or("");
            let num = |key: &str, default: usize| {
                q.split('&').find_map(|kv| kv.strip_prefix(key)).and_then(|v| v.parse().ok()).unwrap_or(default)
            };
            let (size, chunk) = (num("size=", 1 << 20), num("chunk=", 65536).max(1));
            let (tx, body) = giong_http::Body::channel();
            drop(giong::spawn(async move {
                let all = bytes("size=16777216");
                let mut sent = 0;
                while sent < size {
                    let n = chunk.min(size - sent).min(all.len());
                    let at = if sent + n <= all.len() { sent } else { 0 };
                    if tx.send(&all[at..at + n]).await.is_err() {
                        break;
                    }
                    sent += n;
                }
            }));
            Response::new(200).header("content-type", "application/octet-stream").body(body)
        }
        "/bytes" => Response::new(200)
            .header("content-type", "application/octet-stream")
            .body(giong_http::Body::Static(bytes(req.query().unwrap_or("")))),
        _ => Response::new(404),
    }
}

fn main() -> std::io::Result<()> {
    // With TLS_CERT and TLS_KEY (PEM files): HTTPS on port 3010.
    let tls = match (std::env::var("TLS_CERT"), std::env::var("TLS_KEY")) {
        (Ok(cert), Ok(key)) => Some(TlsAcceptor::from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)?),
        _ => None,
    };
    // GIONG_WORKERS=N: N workers instead of one per CPU (bench/memory.sh
    // compares runtimes at the same thread count).
    let mut builder = giong::Builder::new();
    if let Some(n) = std::env::var("GIONG_WORKERS").ok().and_then(|v| v.parse().ok()) {
        builder = builder.workers(n);
    }
    // GIONG_RECV_BUFS=COUNT,SIZE: the receive buffer pool of each worker.
    if let Some((count, size)) = std::env::var("GIONG_RECV_BUFS").ok().and_then(|v| {
        let (count, size) = v.split_once(',')?;
        Some((count.parse().ok()?, size.parse().ok()?))
    }) {
        builder = builder.recv_buffers(count, size);
    }
    builder
        .shutdown_on_signals(true)
        .shutdown_grace(std::time::Duration::from_secs(5))
        .build()?
        .run(move |_| {
            let tls = tls.clone();
            async move {
                // GIONG_LOG_DRIVER=1: say which driver and io_uring features
                // each worker uses (bench/uring-features.sh records it).
                if std::env::var_os("GIONG_LOG_DRIVER").is_some() {
                    eprintln!("{}", giong::driver_choice());
                }
                // BALANCED=1: listeners hand new connections to less busy
                // workers (bench/balance.sh).
                let bind = if std::env::var_os("BALANCED").is_some() {
                    TcpListener::bind_balanced
                } else {
                    TcpListener::bind
                };
                match tls {
                    Some(acceptor) => {
                        let listener = bind(bench_addr(3010)).expect("bind");
                        Server::new().serve_tls(listener, acceptor, handler).await;
                    }
                    None => {
                        let listener = bind(bench_addr(3009)).expect("bind");
                        Server::new().serve(listener, handler).await;
                    }
                }
            }
        })
}

/// `BENCH_HOST` (default 127.0.0.1) lets bench/workers.sh drive the server
/// from another machine.
fn bench_addr(port: u16) -> String {
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    format!("{host}:{port}")
}
