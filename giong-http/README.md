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
giong-http = { version = "0.3", features = ["tls"] }
giong-tls = "0.3"
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
giong-http = { version = "0.3", features = ["client"] }
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
- **Redirects** (301, 302, 303, 307, 308) are followed, up to 10. 303, and 301 or 302 after a `POST`, become a `GET` without the body; 307 and 308 repeat the request as it was. `Authorization` and `Cookie` are not sent to another origin.
- **Finite limits.** A request has 30 s in all (`ClientBuilder::timeout`, or `RequestBuilder::timeout` for one): waiting for a connection, connecting (10 s of it at most), sending, reading the response, redirects. A response body may have 16 MiB, its head 64 KiB. An unused connection is closed after 60 s.
- **Strict reading.** Bodies by `Content-Length`, chunked, or up to the end of the connection; 1xx responses are skipped. A response that could be read in two ways (two lengths, bare line feeds, folded headers, bytes after the body) is an error or keeps its connection out of the pool, so one response is never read as part of another.
- **Errors that say what to do.** `Error::kind` is one of `Request`, `Connect`, `Timeout`, `TooLarge`, `Redirect`, `Closed`, `Protocol`, `Status` and `Decode`, each with a giong code (GIONG-E1101 to E1109) and a page in [docs/errors](../docs/errors/README.md); `Error::is_transient` says whether retrying can help. Any status is a `Response`; `error_for_status` turns 4xx and 5xx into an error with the start of the body.

The client is not `Send`, like everything in giong: make one per worker (in `Builder::run`'s closure) and clone it into that worker's tasks.

### Client performance

Measured with `bench/http-client.sh` against reqwest 0.12 (rustls, on a current-thread Tokio runtime). Each client runs on one core and shares one client between 64 tasks that send `GET` requests back to back and read every body; giong-http's server answers on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), over TCP on 127.0.0.1. `json` is a 27-byte answer, `users` an 8.5 KiB one. Medians of 5 rounds of 5 s, with the range in brackets; requests per second:

| Client | Connections | `json` | `users` | `json` over HTTPS | `users` over HTTPS |
|---|---|---|---|---|---|
| **giong-http** | 64 | **113.7k** (109–123k) | **95.6k** (79–97k) | **105.5k** (91–107k) | **71.0k** (62–75k) |
| giong-http, its default | 16 | 89.9k (69–93k) | 72.4k (67–79k) | 78.0k (74–79k) | 59.7k (53–64k) |
| reqwest, its default | 64 (one per task) | 61.8k (59–71k) | 56.8k (47–60k) | 60.6k (58–69k) | 45.4k (41–52k) |

- **With as many connections: 1.6 to 1.8 times reqwest's requests per second, with a lower p99** (1.1–1.8 ms against 1.9–2.4 ms). Every client's core is the limit (98–100% busy); the server's three cores are not (54–220% of 300).
- **With its default of 16 connections, still 1.3 to 1.5 times reqwest with 64.** Fewer requests in flight means fewer of them handled per trip into the kernel, so 16 connections cost more per request than 64; the default is there to spare the server, and `max_connections_per_host` raises it.
- Not measured: a server on another machine, `POST` bodies, more than one client core, many hosts at once.

Results are in `bench/results/http-client/`.

## Not there yet

HTTP/2, streaming bodies (a body over the limit is refused, not passed on piece by piece), compression, routing. In the client: proxies, cookies, compression, streaming bodies.

## Tests

`cargo test -p giong-http --all-features`: 28 unit tests (including 500,000 random inputs for the parser and the chunked decoder, and 300,000 for the client's response parser and URLs), 20 end-to-end tests of the server on real sockets (one of them over a Unix socket), 11 end-to-end HTTPS tests with a rustls client (TLS 1.2 and 1.3, pipelining, a 3 MB body, 400/413 errors, idle connections, clients that do not speak TLS, graceful shutdown, 16 client threads at once), 14 end-to-end tests of the client against this server and against servers that misbehave on purpose, 6 documentation examples. One more client test, ignored by default (`-- --ignored`), fetches from crates.io over HTTPS.
