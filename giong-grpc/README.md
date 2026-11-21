# giong-grpc

gRPC for the [giong](../README.md) runtime: servers and clients over [giong-http](../giong-http/README.md)'s HTTP/2, with messages read as [giong-proto](../giong-proto/README.md) views that borrow from the bytes received. Nothing from the Tokio ecosystem, and no `protoc` to install: [giong-grpc-build](../giong-grpc-build/README.md) parses the `.proto` files in Rust.

It interoperates with grpc-go in both directions on the test cases of [gRPC's interoperability specification](https://github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md), on io_uring and on epoll, in CI ([`interop/run.sh`](interop/run.sh)).

> **New in giong 1.1.** Not published before giong 1.0.0 is released.

```toml
[dependencies]
giong = "1"
giong-http = "1"
giong-grpc = "1"
giong-proto = "1"

[build-dependencies]
giong-grpc-build = "1"
```

`build.rs`:

```rust,ignore
fn main() {
    giong_grpc_build::compile(&["proto/greeter.proto"], &["proto"]).unwrap();
}
```

`proto/greeter.proto`:

```proto
syntax = "proto3";
package hello.v1;

service Greeter {
  rpc SayHello(HelloRequest) returns (HelloReply);
  rpc SayHelloMany(HelloRequest) returns (stream HelloReply);
}

message HelloRequest { string name = 1; uint32 count = 2; }
message HelloReply { string message = 1; }
```

The server implements the trait generated for the service, one method for each rpc:

```rust,ignore
mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}
use giong_grpc::{Code, Reply, Request, Server, Sink, Status};
use pb::hello::v1::greeter::{Greeter, GreeterServer};
use pb::hello::v1::{HelloReply, HelloRequest};

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
        Reply::new(&HelloReply { message: &format!("Xin chào, {name}!"), ..Default::default() })
    }

    async fn say_hello_many(
        &self,
        request: Request<HelloRequest<'static>>,
        mut responses: Sink<HelloReply<'static>>,
    ) -> Result<(), Status> {
        let message = request.message()?;
        for n in 1..=message.count() {
            let text = format!("Xin chào, {} ({n})!", message.name());
            // Waits while the client is slow to read; fails once it is gone.
            responses.send(&HelloReply { message: &text, ..Default::default() }).await?;
        }
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    // One server per worker thread, all on the same port.
    giong::Builder::new().build()?.run(|_worker| async {
        let listener = giong::net::TcpListener::bind("0.0.0.0:50051").expect("bind");
        let grpc = Server::new().add(GreeterServer::new(Hello));
        giong_http::Server::new().serve(listener, grpc).await;
    })
}
```

The client calls through the generated `GreeterClient`:

```rust,ignore
use giong_grpc::{CallOptions, Channel};
use pb::hello::v1::greeter::GreeterClient;

let greeter = GreeterClient::new(Channel::new("http://127.0.0.1:50051")?);
let request = HelloRequest { name: "An", count: 3, ..Default::default() };

let reply = greeter.say_hello(&request, CallOptions::new()).await?;
println!("{}", reply.message()?.message());

let mut replies = greeter.say_hello_many(&request, CallOptions::new()).await?;
while let Some(received) = replies.message().await? {
    println!("{}", received.message()?.message());
}
```

[`examples/grpc-hello`](../examples/grpc-hello) is this program, complete, with health and reflection.

## What it does

- **The four kinds of method**: unary, server streaming, client streaming and bidirectional, with requests and responses flowing at the same time on one HTTP/2 stream.
- **Messages without copies.** A message is decoded as a view over the bytes that arrived: it is not copied out of the piece of the stream it came in, and messages that arrived together share that piece. Responses are encoded from structs whose fields borrow.
- **Deadlines on both sides.** A client sends its timeout and enforces it; a server enforces the one it was sent, or its own default. A call past its deadline ends with `DEADLINE_EXCEEDED` and its handler is dropped.
- **Cancellation that reaches the handler.** When a client cancels, drops its stream or disappears, the handler's future is dropped at its next `.await`, and `Sink::send` and `RequestStream::message` fail in a handler that is still running.
- **Backpressure.** Sending waits for the reader, so a slow reader slows its writer down and nothing queues without bound.
- **Streams that batch themselves.** The messages a handler sends before it next waits leave in one write (up to 16 KiB at a time); nothing is held back for more to come, and there is no flush to call.
- **Metadata**, text and binary (`-bin`), before and after the messages; statuses with details.
- **gzip**, per message, off by default.
- **TLS** through `giong_http::Server::serve_tls` and `https://` addresses (ALPN), with private authorities and client certificates.
- **Health checking** (`grpc.health.v1.Health`: Check, List, Watch) and **server reflection** (`v1` and `v1alpha`): `grpcurl list`, `describe` and calls work without the `.proto` files.
- **Errors with codes.** The statuses giong-grpc raises itself carry `GIONG-E4101` to `GIONG-E4110`, with what happened and how to fix it in the message ([docs/errors](../docs/errors/README.md#giong-grpc)).

## The four kinds of method

| In the `.proto` | The handler takes | and returns | The client method returns |
|---|---|---|---|
| `rpc M(Req) returns (Resp)` | `Request<Req>` | `Reply<Resp>` | `Reply<Resp>` |
| `rpc M(Req) returns (stream Resp)` | `Request<Req>`, `Sink<Resp>` | `()` | `ResponseStream<Resp>` |
| `rpc M(stream Req) returns (Resp)` | `RequestStream<Req>` | `Reply<Resp>` | `ClientStreaming<Req, Resp>` |
| `rpc M(stream Req) returns (stream Resp)` | `RequestStream<Req>`, `Sink<Resp>` | `()` | (`RequestSink<Req>`, `ResponseStream<Resp>`) |

Every handler returns `Result<_, Status>`. An `Err` ends the call with that status, and the client gets it as the `Err` of its call.

A bidirectional call:

```rust,ignore
let (mut requests, mut responses) = chat.converse(CallOptions::new());
requests.send(&Line { text: "hello", ..Default::default() }).await?;
while let Some(received) = responses.message().await? {
    let line = received.message()?;
    if line.text() == "bye" {
        break;
    }
    requests.send(&Line { text: "and then?", ..Default::default() }).await?;
}
requests.finish(); // the server sees the end of the requests
```

## Limits

Every limit is finite by default.

| What | Default | Set with |
|---|---|---|
| One message, compressed or once decompressed | 4 MiB | `Server::max_message_size`, `ChannelBuilder::max_message_size` |
| A call, on the client | 30 s | `CallOptions::timeout`, `ChannelBuilder::timeout` |
| A call whose client sent no deadline, on the server | 1 h | `Server::default_timeout` |
| Opening a connection | 10 s | `ChannelBuilder::connect_timeout` |
| Connections, concurrent streams, header sizes, a client's silence between two messages | giong-http's | `giong_http::Server` |

## Health and reflection

```rust,ignore
use giong_grpc::health::Health;
use giong_grpc::reflection::Reflection;

// Both are Send and Sync: made once, shared by the servers of every worker.
let health = Health::new();
health.set("hello.v1.Greeter", true);
let reflection = Reflection::new(&[pb::FILE_DESCRIPTOR_SET, giong_grpc::health::FILE_DESCRIPTOR_SET])?;

let grpc = Server::new()
    .add(GreeterServer::new(Hello))
    .add(health.service())
    .add(reflection.v1())
    .add(reflection.v1alpha());
```

```text
$ grpcurl -plaintext localhost:50051 list
grpc.health.v1.Health
grpc.reflection.v1.ServerReflection
grpc.reflection.v1alpha.ServerReflection
hello.v1.Greeter
$ grpcurl -plaintext -d '{"name": "An"}' localhost:50051 hello.v1.Greeter/SayHello
{
  "message": "Xin chào, An!"
}
```

`health.set(service, false)` from any thread tells load balancers to stop sending calls; `health.set_all_not_serving()` does it for every service at the start of a shutdown. Reflection tells anyone who can reach the port what the server's API is: leave it off a server whose API is not public.

## Measured against tonic

`bench/grpc.sh`, against tonic 0.14 (prost, hyper, Tokio) with the same service: unary calls with 100-byte and 10,000-byte payloads, round trips on a bidirectional stream, and server streams of 1,000 messages of 100 bytes. A 4-core cloud VM (Xeon 2.1 GHz, Linux 6.18, io_uring), the median of 10 rounds of 5 s; the method is at the top of the script and the raw data in [`bench/results/grpc/`](../bench/results/grpc).

One server thread, loaded from three other cores until its core is the limit, with tonic's client and with giong-grpc's as the load generator:

| Workload | Load | tonic | giong-grpc | | Server CPU per call or message, tonic → giong-grpc |
|---|---|---:|---:|---:|---:|
| unary, 100 B | tonic | 172,842 calls/s | 259,030 | +50% | 5.54 → 3.62 µs |
| unary, 100 B | giong | 174,525 | 227,408 | +30% | 5.46 → 4.14 µs |
| unary, 10 kB | tonic | 71,918 | 89,862 | +25% | 13.25 → 10.44 µs |
| unary, 10 kB | giong | 75,010 | 90,463 | +21% | 12.77 → 10.51 µs |
| bidirectional round trips | tonic | 550,596 msg/s | 813,260 | +48% | 1.74 → 1.18 µs |
| bidirectional round trips | giong | 542,390 | 816,094 | +50% | 1.76 → 1.17 µs |
| server stream | tonic | 2,917,860 msg/s | 11,694,153 | 4.0× | 0.329 → 0.081 µs |
| server stream | giong | 2,985,038 | 11,394,657 | 3.8× | 0.321 → 0.084 µs |

No workload is behind on the server side. 10,000-byte unary calls from tonic's client were 7.5% behind until giong-http's HTTP/2 server sent the answers to a burst of short requests in one write: it cut its output at 64 KiB, and with one write in flight per connection each further write waited a turn of the worker. With 64 calls in flight per connection (`TASKS=128`, [`tasks-128/`](../bench/results/grpc/tasks-128)) giong-grpc's server does 78,401 calls/s against tonic's 70,778 (+10.8%) under tonic's client and 103,618 against 70,002 (+48%) under its own. The server stream is ahead by that much because the messages a handler sends in a row leave in one write.

One client thread, against a server with three threads (tonic's, then giong-grpc's):

| Workload | Server | tonic client | giong-grpc client | |
|---|---|---:|---:|---:|
| unary, 100 B | tonic | 87,301 calls/s | 94,606 | +8% |
| unary, 100 B | giong | 137,046 | 143,424 | +5% |
| unary, 10 kB | tonic | 51,879 | 58,964 | +14% |
| unary, 10 kB | giong | 57,778 | 60,384 | +5% |
| bidirectional round trips | tonic | 253,018 msg/s | 248,773 | **−1.7%** |
| bidirectional round trips | giong | 489,518 | 498,710 | +2% |
| server stream | tonic | 5,627,548 msg/s | 5,698,860 | +1% |
| server stream | giong | 9,399,474 | 9,446,808 | +1% |

Round trips against tonic's server are level (−1.7%; +0.8% to +2.2% in earlier runs): both clients used about three quarters of their core, so that row measures the server. Server streams from giong-grpc's server were 10% to 18% behind until two things changed: `ResponseStream::message` returns a message that has already arrived without setting up the future that waits for more (it was rebuilt for every message), and a received HTTP/2 frame is handed to the reader as it is instead of being joined to the next ones. The giong-grpc server in the client runs listens with `TcpListener::bind_balanced`, so that the client's two connections always land on two of its three workers: with the kernel's choice they sometimes shared one, and that row moved by a third between rounds. Not measured: client streams of many messages in a row, where the giong-grpc client still hands the connection one message at a time.

Memory: with 1,000 bidirectional streams open on 10 connections, the server's resident memory grew by 5.7 kB a stream with giong-grpc and 11.6 kB with tonic.

The giong-http server in these runs has `max_body_size(4 << 20)`: the HTTP/2 window a stream starts with is a share of the server's body budget (half of it divided by `max_concurrent_streams`, 5 KiB with the defaults), and a request larger than that window waits for a WINDOW_UPDATE. A service whose requests are larger than 5 KiB should set `max_body_size` to its message limit, which makes the first window 20 KiB for 4 MiB.

## Not there

Client-side load balancing and name resolution beyond DNS, retries and hedging (a failed call is returned, not sent again), gRPC-Web, compression other than gzip, and the `google.rpc.Status` details message (its bytes are available: `Status::details`).

## Tests

```sh
cargo test -p giong-grpc                 # calls, faults, health, reflection, generated code
giong-grpc/interop/run.sh                # grpc-go both directions, grpcurl (needs Go)
GIONG_SOAK_SECS=3600 cargo test --release -p giong-grpc --test soak -- --ignored --nocapture
(cd giong-grpc && cargo +nightly fuzz run framing)   # also: gzip timeout status_message metadata descriptors
```

The soak runs calls of every kind with faults mixed in (cancels, dropped streams, deadlines, connections cut in the middle of a message, bytes that are not HTTP/2) and then checks that no handler, descriptor, driver operation or memory was left behind.

The minimum Rust version is 1.80, as for giong.
