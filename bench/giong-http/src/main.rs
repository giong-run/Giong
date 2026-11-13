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
