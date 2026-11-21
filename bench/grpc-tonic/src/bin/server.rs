//! The benchmark service on tonic.
//!
//!   server <port> [threads]
//!
//! With 1 thread (the default) a current-thread Tokio runtime, as one
//! giong worker is one thread; with more, the multi-thread runtime.

use std::pin::Pin;

use bench_grpc_tonic::pb::bench_server::{Bench, BenchServer};
use bench_grpc_tonic::pb::{DownloadRequest, Echo};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

struct Service;

type Echoes = Pin<Box<dyn Stream<Item = Result<Echo, Status>> + Send>>;

#[tonic::async_trait]
impl Bench for Service {
    async fn unary(&self, request: Request<Echo>) -> Result<Response<Echo>, Status> {
        let message = request.into_inner();
        Ok(Response::new(Echo {
            payload: message.payload,
            id: message.id,
        }))
    }

    type DuplexStream = Echoes;

    async fn duplex(&self, request: Request<Streaming<Echo>>) -> Result<Response<Echoes>, Status> {
        // Each message answered as it arrives, by mapping the stream of
        // requests: no task and no channel in between.
        let echoes = request.into_inner().map(|message| {
            message.map(|message| Echo {
                payload: message.payload,
                id: message.id,
            })
        });
        Ok(Response::new(Box::pin(echoes)))
    }

    type DownloadStream = ReceiverStream<Result<Echo, Status>>;

    async fn download(
        &self,
        request: Request<DownloadRequest>,
    ) -> Result<Response<Self::DownloadStream>, Status> {
        let message = request.into_inner();
        // The usual tonic server stream: a task that sends into a bounded
        // channel, which gives the backpressure.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            let payload = vec![b'x'; message.size as usize];
            for id in 0..u64::from(message.count) {
                let echo = Echo {
                    payload: payload.clone(),
                    id,
                };
                if tx.send(Ok(echo)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args[1].parse().unwrap();
    let threads: usize = args.get(2).map_or(1, |t| t.parse().unwrap());
    let runtime = if threads == 1 {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
    }
    .unwrap();
    runtime.block_on(async move {
        tonic::transport::Server::builder()
            .tcp_nodelay(true)
            .add_service(BenchServer::new(Service))
            .serve(([127, 0, 0, 1], port).into())
            .await
            .unwrap();
    });
}
