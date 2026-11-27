//! giong-http's HTTP/3 server: `Server::serve_h3` on a giong-quic endpoint on every worker,
//! one worker per CPU the process may use, all on UDP port 3012 of `BENCH_HOST`.

use giong_http::{Request, Response, Server};

async fn handle(req: Request) -> Response {
    match req.path() {
        "/json" => Response::json(bench_h3::JSON),
        "/users" => Response::json(bench_h3::users()),
        _ => Response::new(404).body("not found\n"),
    }
}

fn main() {
    let config = giong_quic::ServerConfig::new(bench_h3::server_tls()).expect("a QUIC server configuration");
    let addr = bench_h3::addr();
    bench_h3::users();
    let rt = giong::Builder::new()
        // QUIC receives coalesced datagrams (GRO), as in giong-http's h3 example.
        .recv_buffers(256, giong::net::GRO_RECV_BUFFER)
        .shutdown_on_signals(true)
        .build()
        .expect("runtime");
    rt.run(move |_| {
        let config = config.clone();
        async move {
            let endpoint = giong_quic::Endpoint::server(addr, config).expect("bind");
            Server::new().serve_h3(endpoint, handle).await;
        }
    })
    .expect("run");
}
