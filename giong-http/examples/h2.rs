//! HTTP/2 with giong-http: one port for HTTPS, where ALPN picks HTTP/2 or
//! HTTP/1.1, and one for plain HTTP, which speaks HTTP/1.1 or HTTP/2 with
//! prior knowledge. The handler does not know which version it serves.
//!
//!     cargo run --release -p giong-http --features tls --example h2 -- 127.0.0.1:8443 127.0.0.1:8080
//!     curl -k --http2 https://localhost:8443/
//!     curl --http2-prior-knowledge http://localhost:8080/bytes?n=100000 -o /dev/null
//!     h2spec -t -k -p 8443
//!
//! Without `GIONG_CERT` and `GIONG_KEY` (PEM files), a self-signed
//! certificate for `localhost` is made at start. `GIONG_WORKERS=<n>` sets
//! the number of worker threads.
//!
//! Routes: `/` (a small page), `/bytes?n=<len>`, `/echo` (the request body
//! back), `/stream?n=<pieces>` (16 KiB pieces, produced as they are sent).

use std::time::Duration;

use giong::net::TcpListener;
use giong_http::{Body, Request, Response, Server};
use giong_tls::TlsAcceptor;

const PAGE: &str = "<!doctype html>
<html lang=\"en\">
<head><meta charset=\"utf-8\"><title>Giong over HTTP/2</title></head>
<body>
<h1>Hello from Giong</h1>
<p>This page came over the protocol named below. Browsers ask for HTTP/2 through
ALPN when they connect over TLS; curl does with <code>--http2</code>, or with
<code>--http2-prior-knowledge</code> on plain TCP.</p>
<p>The same handler answers HTTP/1.1 and HTTP/2: the server turns each stream into
the request a handler already knows, and the response back into frames.</p>
</body>
</html>
";

fn query_number(req: &Request, key: &str) -> Option<usize> {
    req.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            v.parse().ok()
        } else {
            None
        }
    })
}

async fn handle(req: Request) -> Response {
    match req.path() {
        "/" => Response::new(200)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(format!("{PAGE}<!-- {} -->\n", req.version())),
        "/bytes" => {
            let n = query_number(&req, "n").unwrap_or(1024).min(64 << 20);
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(vec![b'x'; n])
        }
        "/echo" => Response::new(200).body(req.body().to_vec()),
        "/stream" => {
            let pieces = query_number(&req, "n").unwrap_or(16).min(65536);
            let (tx, body) = Body::channel();
            drop(giong::spawn(async move {
                for _ in 0..pieces {
                    if tx.send(vec![b'y'; 16 * 1024]).await.is_err() {
                        return;
                    }
                }
            }));
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(body)
        }
        _ => Response::new(404).body("not found\n"),
    }
}

fn acceptor() -> TlsAcceptor {
    if let (Ok(cert), Ok(key)) = (std::env::var("GIONG_CERT"), std::env::var("GIONG_KEY")) {
        let cert = std::fs::read(cert).expect("GIONG_CERT");
        let key = std::fs::read(key).expect("GIONG_KEY");
        return TlsAcceptor::from_pem(&cert, &key).expect("certificate and key");
    }
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    TlsAcceptor::from_pem(
        key.cert.pem().as_bytes(),
        key.key_pair.serialize_pem().as_bytes(),
    )
    .unwrap()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let tls_addr = args.next().unwrap_or_else(|| "127.0.0.1:8443".into());
    let plain_addr = args.next().unwrap_or_else(|| "127.0.0.1:8080".into());
    let acceptor = acceptor();
    let mut builder = giong::Builder::new().shutdown_grace(Duration::from_secs(5));
    if let Some(n) = std::env::var("GIONG_WORKERS")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        builder = builder.workers(n);
    }
    let rt = builder.build().expect("runtime");
    println!("https://{tls_addr}  (HTTP/2 or HTTP/1.1 through ALPN)");
    println!("http://{plain_addr}  (HTTP/1.1, or HTTP/2 with prior knowledge)");
    rt.run(move |_| {
        let (acceptor, tls_addr, plain_addr) =
            (acceptor.clone(), tls_addr.clone(), plain_addr.clone());
        async move {
            let server = Server::new().max_body_size(16 << 20);
            let tls = TcpListener::bind(tls_addr.as_str()).expect("bind the TLS address");
            let plain = TcpListener::bind(plain_addr.as_str()).expect("bind the plain address");
            let plain_server = server.clone();
            let plain = giong::spawn(async move { plain_server.serve(plain, handle).await });
            server.serve_tls(tls, acceptor, handle).await;
            let _ = plain.await;
        }
    })
    .expect("run");
}
