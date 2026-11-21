//! The benchmark service on giong-grpc.
//!
//!   server <port> [workers]
//!
//! One giong-http server per worker (1 by default), all on the port.

use bench_grpc_giong::pb::bench::v1::bench::{Bench, BenchServer};
use bench_grpc_giong::pb::bench::v1::{DownloadRequest, Echo};
use giong::net::TcpListener;
use giong_grpc::{Reply, Request, RequestStream, Server, Sink, Status};

struct Service;

impl Bench for Service {
    async fn unary(&self, request: Request<Echo<'static>>) -> Result<Reply<Echo<'static>>, Status> {
        let message = request.message()?;
        Reply::new(&Echo {
            payload: message.payload(),
            id: message.id(),
            ..Default::default()
        })
    }

    async fn duplex(
        &self,
        mut requests: RequestStream<Echo<'static>>,
        mut responses: Sink<Echo<'static>>,
    ) -> Result<(), Status> {
        while let Some(received) = requests.message().await? {
            let message = received.message()?;
            responses
                .send(&Echo {
                    payload: message.payload(),
                    id: message.id(),
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }

    async fn download(
        &self,
        request: Request<DownloadRequest<'static>>,
        mut responses: Sink<Echo<'static>>,
    ) -> Result<(), Status> {
        let message = request.message()?;
        let payload = vec![b'x'; message.size() as usize];
        for id in 0..u64::from(message.count()) {
            responses
                .send(&Echo {
                    payload: &payload,
                    id,
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args[1].parse().unwrap();
    let workers: usize = args.get(2).map_or(1, |w| w.parse().unwrap());
    giong::Builder::new()
        .workers(workers)
        .build()
        .unwrap()
        .run(move |_| async move {
            let listener = TcpListener::bind_balanced(("127.0.0.1", port)).unwrap();
            let grpc = Server::new().add(BenchServer::new(Service));
            // The HTTP/2 window a stream starts with is a share of the body
            // budget, which follows max_body_size: with gRPC's 4 MiB message
            // limit as the budget, a 10,000-byte request fits its first
            // window (20 KiB instead of 5 KiB) and needs no WINDOW_UPDATE.
            giong_http::Server::new()
                .max_body_size(4 << 20)
                .serve(listener, grpc)
                .await;
        })
        .unwrap();
}
