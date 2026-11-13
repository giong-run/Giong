//! The benchmark routes on the `giong-http` library (as opposed to
//! `examples/http.rs`, which hand-rolls HTTP on the bare runtime).
use std::hint::black_box;
use std::io::Write;

use giong::net::TcpListener;
use giong_http::{Request, Response, Server};
use giong_tls::{Ktls, TlsAcceptor};

fn fib(n: u32) -> u64 { if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) } }

fn query_n(q: &str) -> u32 {
    q.split('&').find_map(|kv| kv.strip_prefix("n="))
        .map(|v| v.bytes().take_while(u8::is_ascii_digit).fold(0u32, |a, c| a * 10 + (c - b'0') as u32))
        .unwrap_or(25)
        .min(40)
}

/// `/bytes?n=N`: N bytes (at most 16 MiB), for responses large enough that
/// encrypting them dominates (bench/tls.sh compares kernel TLS on them).
fn query_bytes(q: &str) -> usize {
    q.split('&').find_map(|kv| kv.strip_prefix("n=")).and_then(|v| v.parse().ok()).unwrap_or(65536).min(16 << 20)
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
        "/bytes" => {
            // One buffer for every response, so that the server's time goes
            // to sending (and encrypting) rather than to filling memory.
            static BYTES: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
            let all = BYTES.get_or_init(|| Box::leak(vec![b'x'; 16 << 20].into_boxed_slice()));
            let n = query_bytes(req.query().unwrap_or(""));
            Response::new(200).header("content-type", "application/octet-stream").body(&all[..n])
        }
        // `/file?n=N`: the first N bytes of a 16 MiB file, kept open by each
        // worker, sent with `sendfile` over plain TCP and kernel TLS and read
        // and encrypted in the process otherwise (bench/tls.sh). With
        // `&open=1` the file is opened for every request instead, as a
        // static file server without a cache of open files does.
        "/file" => {
            let q = req.query().unwrap_or("");
            let n = query_bytes(q) as u64;
            let file = if q.split('&').any(|kv| kv == "open=1") {
                match giong::fs::File::open(bench_file()).await {
                    Ok(f) => std::rc::Rc::new(f),
                    Err(_) => return Response::new(500),
                }
            } else {
                match open_file().await {
                    Some(f) => f,
                    None => return Response::new(500),
                }
            };
            Response::new(200)
                .header("content-type", "application/octet-stream")
                .body(giong_http::Body::file_range(file, 0, n))
        }
        _ => Response::new(404),
    }
}

/// The file `/file` serves: 16 MiB, written once per process.
fn bench_file() -> &'static std::path::Path {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::var_os("BENCH_FILE_DIR").map(Into::into).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("giong-bench-file-{}", std::process::id()));
        std::fs::write(&path, vec![b'x'; 16 << 20]).expect("write the benchmark file");
        path
    })
}

/// This worker's open handle on [`bench_file`].
async fn open_file() -> Option<std::rc::Rc<giong::fs::File>> {
    thread_local! {
        static FILE: std::cell::RefCell<Option<std::rc::Rc<giong::fs::File>>> = const { std::cell::RefCell::new(None) };
    }
    if let Some(f) = FILE.with(|f| f.borrow().clone()) {
        return Some(f);
    }
    let f = std::rc::Rc::new(giong::fs::File::open(bench_file()).await.ok()?);
    FILE.with(|slot| *slot.borrow_mut() = Some(f.clone()));
    Some(f)
}

fn main() -> std::io::Result<()> {
    // With TLS_CERT and TLS_KEY (PEM files): HTTPS on port 3010.
    // GIONG_KTLS=auto|required: hand the records to the kernel after the
    // handshake (kernel TLS); off by default.
    let tls = match (std::env::var("TLS_CERT"), std::env::var("TLS_KEY")) {
        (Ok(cert), Ok(key)) => {
            let acceptor = TlsAcceptor::from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)?;
            let mode = match std::env::var("GIONG_KTLS").as_deref() {
                Ok("auto") => Ktls::Auto,
                Ok("required") => Ktls::Required,
                _ => Ktls::Off,
            };
            let acceptor = acceptor.ktls(mode)?;
            eprintln!("{} (mode {mode:?}{})", giong_tls::ktls_support(),
                acceptor.ktls_unavailable().map(|why| format!(", off: {why}")).unwrap_or_default());
            Some(acceptor)
        }
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
    let file = bench_file().to_path_buf();
    let ran = builder
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
        });
    let _ = std::fs::remove_file(file);
    ran
}

/// `BENCH_HOST` (default 127.0.0.1) lets bench/workers.sh drive the server
/// from another machine.
fn bench_addr(port: u16) -> String {
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    format!("{host}:{port}")
}
