# giong-http

A small, strict HTTP/1.1 server for the [giong](../README.md) runtime.

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
- Bodies by `Content-Length` or chunked, received in full before the handler is called. `Expect: 100-continue` is supported.
- A `Request` shares the connection's receive buffer instead of copying it, so building a request allocates nothing. A handler may keep the request for as long as it likes.
- Limits on the size of the head (64 KiB) and the body (1 MiB); timeouts for idle connections (60 s), slow requests (30 s, against slowloris) and clients that read slowly (30 s). All of them can be changed on `Server`.
- An optional cap on open connections (`Server::max_connections`): at the cap the server stops accepting, and new clients wait in the kernel's listen queue until a connection closes.
- `Request::peer_addr` gives the client's address (`None` over a Unix socket).
- Graceful shutdown: a request in progress is answered with `Connection: close`, an idle connection is closed at once.
- HTTPS through the `tls` feature (see below).
- Serving over a Unix socket with `Server::serve_unix`, the usual setup behind a reverse proxy on the same machine.
- `Date` and `Content-Length` are added for you. A header value with control characters is refused (against response splitting).

## Strict parsing

Any request that a proxy in front could read differently is answered with `400`, because that difference is exactly how request smuggling works:

- two different `Content-Length` headers, or `Content-Length` together with `Transfer-Encoding`
- whitespace before the colon, folded header lines, line endings that are a bare LF
- control characters in a header name or value, a missing or repeated `Host` in HTTP/1.1

Other status codes: `408` the request was too slow, `413` the body is too large, `417` an unknown `Expect`, `431` the head is too large, `501` a transfer-encoding other than chunked, `505` not HTTP/1.x.

## HTTPS

Turn on the `tls` feature to get `Server::serve_tls`, which uses the [`giong-tls`](../giong-tls/README.md) crate (rustls, TLS 1.2 and 1.3):

```toml
giong-http = { version = "0.2", features = ["tls"] }
giong-tls = "0.2"
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

## Not there yet

HTTP/2, streaming bodies (a body over the limit is refused, not passed on piece by piece), compression, routing.

## Tests

`cargo test -p giong-http --all-features`: 15 unit tests (including 500,000 random inputs for the parser and the chunked decoder), 19 end-to-end tests on real sockets (one of them over a Unix socket), 11 end-to-end HTTPS tests with a rustls client (TLS 1.2 and 1.3, pipelining, a 3 MB body, 400/413 errors, idle connections, clients that do not speak TLS, graceful shutdown, 16 client threads at once), 5 documentation examples.
