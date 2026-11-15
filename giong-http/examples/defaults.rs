//! A server with every default of `Server::new()`, on plain HTTP (HTTP/1.1,
//! or HTTP/2 with prior knowledge): what `scripts/slow-uploads.sh` attacks
//! in a memory-limited container to check that the defaults bound memory.
//!
//!     cargo run --release -p giong-http --example defaults -- 127.0.0.1:8080
//!     curl -d hello http://127.0.0.1:8080/upload
//!
//! `GIONG_WORKERS=<n>` sets the number of worker threads. Routes: `/` (a
//! short answer) and any other path, which answers with the length of the
//! request body.

use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

async fn handle(req: Request) -> Response {
    match req.path() {
        "/" => Response::text("ok\n"),
        _ => Response::text(format!("{} bytes\n", req.body().len())),
    }
}

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".into());
    let mut builder = giong::Builder::new().shutdown_on_signals(true);
    if let Some(n) = std::env::var("GIONG_WORKERS")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        builder = builder.workers(n);
    }
    let rt = builder.build().expect("runtime");
    println!("http://{addr}  (HTTP/1.1, or HTTP/2 with prior knowledge)");
    rt.run(move |_| {
        let addr = addr.clone();
        async move {
            let listener = TcpListener::bind(addr.as_str()).expect("bind");
            Server::new().serve(listener, handle).await;
        }
    })
    .expect("run");
}
