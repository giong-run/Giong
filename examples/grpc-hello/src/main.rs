//! A gRPC service on giong: the server, and a client that calls it.
//!
//! ```text
//! cargo run -p grpc-hello -- serve 127.0.0.1:50051
//! cargo run -p grpc-hello -- call http://127.0.0.1:50051 An
//! grpcurl -plaintext -d '{"name": "An"}' 127.0.0.1:50051 hello.v1.Greeter/SayHello
//! ```

mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}

use std::net::SocketAddr;
use std::time::Duration;

use giong::net::TcpListener;
use giong_grpc::health::Health;
use giong_grpc::reflection::Reflection;
use giong_grpc::{CallOptions, Channel, Code, Reply, Request, Server, Sink, Status};
use pb::hello::v1::greeter::{Greeter, GreeterClient, GreeterServer};
use pb::hello::v1::{HelloReply, HelloRequest};

/// The service: one method of the trait for each rpc of the .proto.
struct Hello;

impl Greeter for Hello {
    async fn say_hello(
        &self,
        request: Request<HelloRequest<'static>>,
    ) -> Result<Reply<HelloReply<'static>>, Status> {
        // A view over the bytes received: `name()` borrows from them.
        let name = request.message()?.name();
        if name.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "name must not be empty"));
        }
        Reply::new(&HelloReply {
            message: &format!("Xin chào, {name}!"),
            ..Default::default()
        })
    }

    async fn say_hello_many(
        &self,
        request: Request<HelloRequest<'static>>,
        mut responses: Sink<HelloReply<'static>>,
    ) -> Result<(), Status> {
        let message = request.message()?;
        for n in 1..=message.count() {
            // Waits while the client is slow to read; fails, and ends this
            // handler, once the client is gone.
            responses
                .send(&HelloReply {
                    message: &format!("Xin chào, {} ({n})!", message.name()),
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }
}

/// One server per worker thread, all on the same port.
fn serve(addr: SocketAddr) -> std::io::Result<()> {
    // Shared by the workers: health can be changed from any thread.
    let health = Health::new();
    health.set(pb::hello::v1::greeter::NAME, true);
    let reflection = Reflection::new(&[
        pb::FILE_DESCRIPTOR_SET,
        giong_grpc::health::FILE_DESCRIPTOR_SET,
    ])
    .map_err(std::io::Error::other)?;
    giong::Builder::new().build()?.run(move |_worker| {
        let grpc = Server::new()
            .add(GreeterServer::new(Hello))
            .add(health.service())
            .add(reflection.v1())
            .add(reflection.v1alpha());
        async move {
            let listener = TcpListener::bind(addr).expect("bind");
            giong_http::Server::new().serve(listener, grpc).await;
        }
    })
}

/// Calls both methods and prints what comes back.
async fn call(endpoint: &str, name: &str) -> Result<Vec<String>, Status> {
    let channel =
        Channel::new(endpoint).map_err(|e| Status::new(Code::InvalidArgument, e.to_string()))?;
    let greeter = GreeterClient::new(channel);
    let request = HelloRequest {
        name,
        count: 3,
        ..Default::default()
    };
    let mut lines = Vec::new();

    let options = CallOptions::new().timeout(Duration::from_secs(5));
    let reply = greeter.say_hello(&request, options).await?;
    lines.push(reply.message()?.message().to_owned());

    let mut replies = greeter.say_hello_many(&request, CallOptions::new()).await?;
    while let Some(received) = replies.message().await? {
        lines.push(received.message()?.message().to_owned());
    }
    Ok(lines)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve", addr] => serve(addr.parse()?)?,
        ["call", endpoint, name] => {
            for line in giong::block_on(call(endpoint, name))? {
                println!("{line}");
            }
        }
        _ => {
            eprintln!("usage: grpc-hello serve ADDR | grpc-hello call http://ADDR NAME");
            std::process::exit(2);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_gets_what_the_server_says() {
        giong::block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let grpc = Server::new().add(GreeterServer::new(Hello));
            giong::spawn(async move { giong_http::Server::new().serve(listener, grpc).await });

            let lines = call(&endpoint, "An").await.unwrap();
            assert_eq!(
                lines,
                [
                    "Xin chào, An!",
                    "Xin chào, An (1)!",
                    "Xin chào, An (2)!",
                    "Xin chào, An (3)!"
                ]
            );
            let refused = call(&endpoint, "").await.unwrap_err();
            assert_eq!(refused.code(), Code::InvalidArgument);
            assert_eq!(refused.message(), "name must not be empty");
        });
    }
}
