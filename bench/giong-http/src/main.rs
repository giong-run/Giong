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

/// The bodies of `/static?size=N` and `/stream`: up to 16 MiB, made once and never freed,
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
        // `/static?size=N`: N bytes from memory that is never freed, sent
        // without a copy of their own (bench/uring-features.sh).
        "/static" => Response::new(200)
            .header("content-type", "application/octet-stream")
            .body(giong_http::Body::Static(bytes(req.query().unwrap_or("")))),
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
