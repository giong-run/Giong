# giong-http

A small, strict HTTP/1.1 server and client for the [giong](../README.md) runtime.

```rust
use std::time::Duration;
use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

async fn handler(req: Request) -> Response {
    match (req.method(), req.path()) {
        ("GET", "/health") => Response::text("ok"),
        ("POST", "/echo") => Response::new(200).body(req.body().to_vec()),
        _ => Response::new(404),
    }
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(10))
        .build()?
        .run(|_worker| async {
            // One listener per worker; the kernel spreads connections over the cores.
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}
```

## What it does

- HTTP/1.1 and HTTP/1.0, keep-alive, pipelining. Responses to pipelined requests leave in a single write; if a handler has to wait, the responses before it are sent first.
- A body of 32 KiB or more is not copied: it leaves from where it is, right after its head, in one vectored write. This is over TCP and Unix sockets; over TLS the body is still copied after its head.
- Bodies by `Content-Length` or chunked, received in full before the handler is called. `Expect: 100-continue` is supported.
- Or streamed, both ways. A handler whose `Handler::stream_body` returns a limit is called as soon as the head is in and reads the body piece by piece with `Request::body_stream`; nothing is read until it asks, so it can refuse an upload before the client sends it. `Body::channel()` gives a response body that is written as it is produced (chunked), for downloads, exports and server-sent events. One piece is in flight at a time in each direction, so a slow peer slows the other side down instead of filling memory. See "Streaming" below.
- A `Request` shares the connection's receive buffer instead of copying it, so building a request allocates nothing. A handler may keep the request for as long as it likes.
- Limits on the size of the head (64 KiB) and the body (1 MiB); timeouts for idle connections (60 s), slow requests (30 s, against slowloris) and clients that read slowly (30 s). All of them can be changed on `Server`.
- An optional cap on open connections (`Server::max_connections`): at the cap the server stops accepting, and new clients wait in the kernel's listen queue until a connection closes.
- Handlers that keep a worker busy (heavy computation, long responses) leave the kernel's even spread of connections uneven. Bind each worker's listener with `giong::net::TcpListener::bind_balanced` instead of `bind` and a busy worker hands new connections to a quieter one; `Server::serve` takes either.
- A handler is an `async fn(Request) -> Response`, a closure that returns a future (write its argument's type: `|req: Request| async move { .. }`), or a type of your own that implements `Handler` and borrows from itself while it answers. [`giong-web`](../giong-web/README.md) builds routing, middleware and JSON on it.
- `Request::from_bytes` makes a request from its bytes and `Response::headers`, `get_header` and `body_bytes` read an answer back, so handlers are tested without a socket.
- `Request::peer_addr` gives the client's address (`None` over a Unix socket).
- Graceful shutdown: a request in progress is answered with `Connection: close`, an idle connection is closed at once.
- HTTPS through the `tls` feature (see below).
- Serving over a Unix socket with `Server::serve_unix`, the usual setup behind a reverse proxy on the same machine.
- `Date` and `Content-Length` are added for you. A header value with control characters is refused (against response splitting).

## Streaming

```rust
use giong_http::{Body, Handler, Request, Response};

struct Files;

impl Handler for Files {
    // Uploads to /upload are read as they arrive, up to 10 GiB.
    fn stream_body(&self, req: &Request) -> Option<u64> {
        (req.path() == "/upload").then_some(10 << 30)
    }

    async fn call(&self, req: Request) -> Response {
        if let Some(body) = req.body_stream() {
            let mut total = 0;
            loop {
                match body.chunk().await {
                    Ok(Some(piece)) => total += piece.len(), // write it somewhere
                    Ok(None) => return Response::text(format!("{total} bytes")),
                    Err(e) => return Response::new(400).body(e.to_string()),
                }
            }
        }
        // Anything else: a body sent as it is produced.
        let (tx, body) = Body::channel();
        drop(giong::spawn(async move {
            for n in 0..1000 {
                if tx.send(format!("line {n}\n")).await.is_err() {
                    return; // the client went away
                }
            }
        }));
        Response::new(200).header("Content-Type", "text/plain").body(body)
    }
}
```

- A streamed request body has the limit `stream_body` returns instead of `max_body_size`. Announced longer: `413` before the handler runs. Chunked and growing past it: `GIONG-E1111` from `chunk`. Cut short, or a piece slower than `request_timeout` (the whole body has no deadline): `GIONG-E1112`. Broken chunked framing: `GIONG-E1113`.
- A handler that returns before the end of the body has its response sent, then the connection closes (whatever the client still sends is read and dropped for a moment, so it gets the response instead of a reset).
- A streamed response goes out with `Transfer-Encoding: chunked`; an HTTP/1.0 client gets it without a length and the connection closes after it. The head is sent when the handler returns, each piece as soon as it is sent, and pieces of 32 KiB or more are written without a copy.
- The server waits for the producer without a time limit (an event stream can be quiet for long); each write has `write_timeout`. `send` fails with `GIONG-E1114` once the client is gone, which is noticed at the next write, or when the response has no body (`HEAD`, 204, 304). `BodySender::abort` ends the body without its last chunk, so the client can tell it is incomplete.

## Strict parsing

Any request that a proxy in front could read differently is answered with `400`, because that difference is exactly how request smuggling works:

- two different `Content-Length` headers, or `Content-Length` together with `Transfer-Encoding`
- whitespace before the colon, folded header lines, line endings that are a bare LF
- control characters in a header name or value, a missing or repeated `Host` in HTTP/1.1

Other status codes: `408` the request was too slow, `413` the body is too large, `417` an unknown `Expect`, `431` the head is too large, `501` a transfer-encoding other than chunked, `505` not HTTP/1.x.

## HTTPS

Turn on the `tls` feature to get `Server::serve_tls`, which uses the [`giong-tls`](../giong-tls/README.md) crate (rustls, TLS 1.2 and 1.3):

```toml
giong-http = { version = "0.4", features = ["tls"] }
giong-tls = "0.4"
```

```rust
let acceptor = giong_tls::TlsAcceptor::from_pem(
    &std::fs::read("cert.pem")?,
    &std::fs::read("key.pem")?,
)?;
// ... in each worker:
let listener = TcpListener::bind("0.0.0.0:8443").expect("bind");
Server::new().serve_tls(listener, acceptor.clone(), handler).await;
```

Handlers, limits, timeouts and graceful shutdown are the same as for plain HTTP. Each handshake runs in its connection's own task, so a slow client does not hold up accepting new connections; a client that does not speak TLS (a port scan, plain HTTP sent to the wrong port) is closed when the handshake timeout expires. When it closes a connection the server sends `close_notify`, so the client can tell a complete response from a connection that was cut.

To serve over another kind of connection (a different TLS layer, a fake connection in a test), implement the `Transport` trait and call `Server::serve_connection`.

## Client

The `client` feature adds an HTTP/1.1 client, `giong_http::client` (HTTPS included, through giong-tls):

```toml
giong-http = { version = "0.4", features = ["client"] }
```

```rust
use giong_http::client::Client;

// One client per worker; clones share its connections.
let client = Client::new();
let user = client
    .get("https://api.example.com/users/7")
    .header("authorization", "Bearer ...")
    .send()
    .await?
    .error_for_status()?;
println!("{}", user.text()?);

let created = client
    .post("https://api.example.com/users")
    .json(r#"{"name":"Ada"}"#)
    .send()
    .await?;
```

- **Connections kept and shared.** A pool for each origin (scheme, host, port), up to 16 connections by default (`ClientBuilder::max_connections_per_host`). Further requests wait in line and are served first come, first served: a connection that comes back goes to the request that has waited longest.
- **Connections that closed are noticed.** An idle connection the server has closed is not used. An idempotent request (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`) that meets a kept-alive connection closing under it is sent once more on a new one; a `POST` or `PATCH` is not, since the server may have processed it, and the error says so.
- **HTTPS, verified.** Certificates are checked against the machine's certificate authorities and the host name; `ClientBuilder::tls` takes a `TlsConnector` for a private authority or a client certificate. A redirect from HTTPS to HTTP is refused.
- **Redirects** (301, 302, 303, 307, 308) are followed, up to 10. 303, and 301 or 302 after a `POST`, become a `GET` without the body; 307 and 308 repeat the request as it was. `Authorization`, `Cookie` and a `Host` set by hand are not sent to another origin.
- **Finite limits.** A request has 30 s in all (`ClientBuilder::timeout`, or `RequestBuilder::timeout` for one): waiting for a connection, connecting (10 s of it at most), sending, reading the response, redirects. A response body may have 16 MiB, its head 64 KiB. An unused connection is closed after 60 s.
- **Strict reading.** Bodies by `Content-Length`, chunked, or up to the end of the connection; 1xx responses are skipped. A response that could be read in two ways (two lengths, bare line feeds, folded headers, bytes after the body) is an error or keeps its connection out of the pool, so one response is never read as part of another.
- **Proxies.** `ClientBuilder::proxy(Proxy::all("http://user:pass@proxy:3128")?.no_proxy(".svc.cluster.local, 10.0.0.0/8")?)`, or `Proxy::from_env()` for `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` the way curl reads them (never read unless asked). `https` goes through a `CONNECT` tunnel, so the proxy sees only the host name; the credentials go to the proxy, never to the server. Plain `http://` proxies only, for now (no SOCKS, no TLS to the proxy itself).
- **Compression.** Responses are asked for with `Accept-Encoding: gzip, deflate` and decompressed as they arrive (`ClientBuilder::decompress(false)` turns it off; a request with its own `Accept-Encoding` gets the bytes as sent). The size limit applies to the decompressed body, so 32 KiB that would expand to gigabytes fails at the limit instead of filling memory; the gzip checksum is checked, and a body cut short is an error, not a shorter body.
- **Streaming.** `RequestBuilder::send_streaming()` returns once the head has arrived; `StreamingResponse::chunk()` gives the body piece by piece (64 KiB at most, decompressed), with no limit on its size and the timeout applying to each piece. Read to its end, the connection goes back to the pool; dropped before, it is closed.
- **Errors that say what to do.** `Error::kind` is one of `Request`, `Connect`, `Timeout`, `TooLarge`, `Redirect`, `Closed`, `Protocol`, `Status`, `Decode` and `Proxy`, each with a giong code (GIONG-E1101 to E1110) and a page in [docs/errors](../docs/errors/README.md); `Error::is_transient` says whether retrying can help. Any status is a `Response`; `error_for_status` turns 4xx and 5xx into an error with the start of the body.

Proxies, compression, streamed bodies and the `Proxy` error kind are new in 0.4.0; giong-http 0.3 does not have them.

The client is not `Send`, like everything in giong: make one per worker (in `Builder::run`'s closure) and clone it into that worker's tasks.

### Client performance

Measured with `bench/http-client.sh` against reqwest 0.12 (rustls, on a current-thread Tokio runtime). Each client runs on one core and shares one client between 64 tasks that send `GET` requests back to back and read every body; giong-http's server answers on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), over TCP on 127.0.0.1. `json` is a 27-byte answer, `users` an 8.5 KiB one. Medians of 5 rounds of 5 s, with the range in brackets; requests per second:

| Client | Connections | `json` | `users` | `json` over HTTPS | `users` over HTTPS |
|---|---|---|---|---|---|
| **giong-http** | 64 | **132.1k** (122–142k) | **109.9k** (106–117k) | **132.6k** (113–137k) | **82.2k** (61–91k) |
| giong-http, its default | 16 | 95.8k (88–116k) | 84.0k (81–87k) | 91.1k (78–97k) | 70.1k (64–75k) |
| reqwest, its default | 64 (one per task) | 66.0k (61–74k) | 64.8k (55–67k) | 67.2k (60–70k) | 51.6k (45–54k) |

- **With as many connections: 1.6 to 2.0 times reqwest's requests per second, with a lower p99** (0.96–1.5 ms against 1.6–2.1 ms). Every client's core is the limit (98–100% busy); the server's three cores are not (57–228% of 300).
- **With its default of 16 connections, still 1.3 to 1.5 times reqwest with 64.** Fewer requests in flight means fewer of them handled per trip into the kernel, so 16 connections cost more per request than 64; the default is there to spare the server, and `max_connections_per_host` raises it.
- Not measured: a server on another machine, `POST` bodies, more than one client core, many hosts at once.

Results are in `bench/results/http-client/`.

## Not there yet

HTTP/2. Routing and compression of responses are in [`giong-web`](../giong-web/README.md). In the client: cookies, streaming request bodies, `br` and `zstd`, SOCKS proxies, HTTP/2.

## Tests

`cargo test -p giong-http --all-features`: 46 unit tests (including 500,000 random inputs for the parser and the chunked decoder, 300,000 for the client's response parser and URLs, and 50,000 damaged gzip and deflate bodies), 21 end-to-end tests of the server on real sockets (one of them over a Unix socket), 14 end-to-end tests of streamed bodies (a 250 KB upload read piece by piece with the connection used again after it, chunked uploads, bodies past the handler's limit, broken framing, nothing read and no `100 Continue` until the handler asks, a handler that answers early, a body cut short, the timeout of each piece; streamed responses chunked and sent piece by piece, to an HTTP/1.0 client, without a body for `HEAD`, 204 and 304, aborted, and stopped when the client goes away), 11 end-to-end HTTPS tests with a rustls client (TLS 1.2 and 1.3, pipelining, a 3 MB body, 400/413 errors, idle connections, clients that do not speak TLS, graceful shutdown, 16 client threads at once), 22 end-to-end tests of the client against this server, against servers that misbehave on purpose and through proxies, 12 documentation examples. One more client test, ignored by default (`-- --ignored`), fetches from crates.io over HTTPS.
