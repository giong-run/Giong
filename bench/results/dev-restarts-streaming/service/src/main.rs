//! A service with a streamed response, for `giong dev` restarts: `GET /`
//! answers with the build's `VERSION`; `GET /stream?secs=N` sends one line
//! every 100 ms for N seconds, each naming the build and process that wrote
//! it, then `done`.

use std::time::Duration;

use giong::net::TcpListener;
use giong_web::{Body, Error, Req, Response, Router, Server};

const VERSION: &str = "v0";

async fn version(_req: Req) -> Result<Response, Error> {
    Ok(Response::text(VERSION))
}

async fn stream(req: Req) -> Result<Response, Error> {
    let secs: u64 = req.query_param("secs")?.unwrap_or(5);
    let (tx, body) = Body::channel();
    drop(giong::spawn(async move {
        let pid = std::process::id();
        for i in 0..secs * 10 {
            if tx.send(format!("{VERSION} {pid} {i}\n")).await.is_err() {
                return;
            }
            giong::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = tx.send(format!("{VERSION} {pid} done\n")).await;
    }));
    Ok(Response::new(200)
        .header("Content-Type", "text/plain")
        .body(body))
}

fn main() {
    let listen = std::env::var("LISTEN").unwrap();
    let workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|w| w.parse().ok())
        .unwrap_or(1);
    let grace: u64 = std::env::var("GRACE")
        .ok()
        .and_then(|g| g.parse().ok())
        .unwrap_or(10);
    eprintln!("starting {VERSION} with {workers} workers, grace {grace} s");
    giong::Builder::new()
        .workers(workers)
        .shutdown_on_signals(true)
        // The `giong new` web template's grace.
        .shutdown_grace(Duration::from_secs(grace))
        .build()
        .unwrap()
        .run(move |_| {
            let listen = listen.clone();
            async move {
                let app = Router::new()
                    .get("/", version)
                    .get("/stream", stream)
                    .build(());
                let l = TcpListener::bind(listen.as_str()).unwrap();
                Server::new().serve(l, app).await;
            }
        })
        .unwrap();
}
