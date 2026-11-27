//! The `h3` crate's server on quinn and Tokio: one endpoint on UDP port 3012 of
//! `BENCH_HOST`, Tokio's current-thread runtime for one CPU, the multi-threaded one with a
//! worker per CPU the process may use otherwise.

use std::sync::Arc;

use bytes::Bytes;

fn main() {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let rt = if workers == 1 {
        tokio::runtime::Builder::new_current_thread().enable_all().build()
    } else {
        tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build()
    }
    .expect("runtime");
    let users = Bytes::from_static(bench_h3::users());
    rt.block_on(async move {
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(bench_h3::server_tls()).expect("QUIC TLS");
        let config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let endpoint = quinn::Endpoint::server(config, bench_h3::addr()).expect("bind");
        while let Some(incoming) = endpoint.accept().await {
            let users = users.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let Ok(mut h3c) = h3::server::Connection::<_, Bytes>::new(h3_quinn::Connection::new(conn)).await else {
                    return;
                };
                while let Ok(Some(resolver)) = h3c.accept().await {
                    let users = users.clone();
                    tokio::spawn(async move {
                        let Ok((req, mut stream)) = resolver.resolve_request().await else { return };
                        let (status, body) = match req.uri().path() {
                            "/json" => (200, Bytes::from_static(bench_h3::JSON)),
                            "/users" => (200, users),
                            _ => (404, Bytes::from_static(b"not found\n")),
                        };
                        let resp = http::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .header("content-length", body.len())
                            .body(())
                            .unwrap();
                        if stream.send_response(resp).await.is_ok() && stream.send_data(body).await.is_ok() {
                            let _ = stream.finish().await;
                        }
                    });
                }
            });
        }
    });
}
