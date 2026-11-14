# giong-http

A small, strict HTTP/1.1 and HTTP/2 server, and an HTTP/1.1 and HTTP/2 client, for the [giong](../README.md) runtime.

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

- HTTP/2, over TLS (chosen through ALPN, as browsers do) and over plain TCP with prior knowledge, for the same handlers. See "HTTP/2" below.
- HTTP/1.1 and HTTP/1.0, keep-alive, pipelining. Responses to pipelined requests leave in a single write; if a handler has to wait, the responses before it are sent first.
- A body of 32 KiB or more is not copied: it leaves from where it is, right after its head, in one vectored write over TCP and Unix sockets. Over TLS it is not copied after its head either: rustls takes the head and then the body, and with kernel TLS both go to the kernel in one vectored write.
- Bodies by `Content-Length` or chunked, received in full before the handler is called. `Expect: 100-continue` is supported.
- Or sent from a file: `Body::file(file)` and `Body::file_range(file, offset, len)` send an open `giong::fs::File` with `sendfile`, straight from the page cache, over TCP and over HTTPS with kernel TLS; over HTTPS in the process the file is read and encrypted in 256 KiB pieces, over Unix sockets read and written, and over HTTP/2 read in 64 KiB pieces into DATA frames. One open file (an `Rc<File>`) can serve any number of responses at once.
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
- `Date` and `Content-Length` are added for you. A header value with control characters is refused (against response splitting): `Response::header` panics, and `Response::try_header` (with `Response::try_new` for the status) returns an error instead, for values that come from the request.

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
- a trailer line after a chunked body that would not pass as a header line
- a target that is a whole URL (`GET http://host/path`, which servers must accept) unless it is `http` or `https`, has no user name or password, and names the host the `Host` header names (compared without regard to case, ports included): a handler reads `Host`, and a proxy that routes on the URL must not see another host. So `Host` names the host of every request, as `:authority` does over HTTP/2. `*` is accepted only with `OPTIONS`, and `host:port` only with `CONNECT`.

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

To have the kernel encrypt and decrypt the records once rustls has done the handshake (kernel TLS, Linux with the `tls` module), build the acceptor with `.ktls(giong_tls::Ktls::Auto)?`; nothing else changes. When it pays off and when it does not is in [giong-tls's README](../giong-tls/README.md#kernel-tls).

To serve over another kind of connection (a different TLS layer, a fake connection in a test), implement the `Transport` trait and call `Server::serve_connection`.

## HTTP/2

The server speaks HTTP/2 ([RFC 9113](https://www.rfc-editor.org/rfc/rfc9113)) with the handlers it already has. Over TLS, `serve_tls` offers `h2` and `http/1.1` through ALPN and each client picks: browsers, curl and gRPC clients pick `h2`. Without TLS, a connection that opens with the HTTP/2 preface is served over HTTP/2 ("prior knowledge", the way gRPC clients and proxies such as Envoy talk to backends), and any other over HTTP/1.1, so one port serves both. `Server::http2(false)` turns it off.

```sh
cargo run --release -p giong-http --features tls --example h2
curl -k --http2 https://localhost:8443/
curl --http2-prior-knowledge http://localhost:8080/bytes?n=100000 -o /dev/null
```

What a handler sees:

- Each stream is a `Request` like any other, and `Request::version()` is `Version::Http2`. `:authority` becomes the `Host` header (a `Host` that differs from it is refused), and the `cookie` fields a client sends apart are joined into one. A [`giong-web`](../giong-web/README.md) app serves HTTP/2 without a change.
- Streams are served at once and answered as their handlers finish; a slow handler holds up no other stream. Each handler runs in its connection's task, like an HTTP/1.1 one, on the same core.
- The response is written as HEADERS and DATA frames. Its headers are sent in lowercase, without those HTTP/2 forbids (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`); a streamed body (`Body::channel`) goes out as DATA frames instead of chunks.
- A stream the client resets (`RST_STREAM`) drops its handler at once, as a dropped future: whatever it was waiting on is cancelled. A handler that answers before the request body has all arrived has its response sent, then the rest of the body is refused with `RST_STREAM(NO_ERROR)`; the connection stays open.
- Streamed request bodies (`Handler::stream_body`), `Expect: 100-continue` and request trailers work as over HTTP/1.1. Graceful shutdown sends a GOAWAY, and the streams in progress finish within the grace period.
- Not done: server push, and priorities (the server says so with `SETTINGS_NO_RFC7540_PRIORITIES` and serves streams in turn). Extended CONNECT (WebSockets over HTTP/2) is not supported.

### Limits

Every resource a client can make the server hold is bounded. A client that goes past a limit loses its stream or its connection; a connection ends with a GOAWAY whose debug data starts with a `GIONG-E` code, which has a page in [docs/errors](../docs/errors/README.md).

| Attack | What the server does | Code |
|---|---|---|
| Many streams at once | Beyond `max_concurrent_streams` (default 100), new streams are refused with `REFUSED_STREAM`. | |
| Rapid reset ([CVE-2023-44487](https://nvd.nist.gov/vuln/detail/CVE-2023-44487)): open streams and reset them at once | Streams reset by the client or refused draw on a budget that refills over time (a burst of twice `max_concurrent_streams`, at least 100, refilled at `max_concurrent_streams` a second, at least 50). Overdrawn: GOAWAY `ENHANCE_YOUR_CALM`. A handler stops at its stream's reset, so the work a reset leaves behind is nothing. | `GIONG-E1116` |
| Floods of frames that do no work (CVE-2019-9512, 9515, 9518): PING, SETTINGS, PRIORITY, empty DATA or CONTINUATION, WINDOW_UPDATE with nothing to send, frames for closed streams | A budget of 1,000 such frames, refilled at 500 a second. Overdrawn: GOAWAY `ENHANCE_YOUR_CALM`. | `GIONG-E1117` |
| CONTINUATION flood ([VU#421644](https://kb.cert.org/vuls/id/421644)): a header block that never ends | A header block may not be larger than `max_head_size` (at least 16 KiB), and no other frame may come between its pieces. | `GIONG-E1118` |
| HPACK bomb: a small block that decodes into a huge header list | The decoded list counts against `max_head_size`; past it, that stream is answered with `431`. The dynamic table is the 4 KiB the protocol starts with. | |
| Large bodies on many streams | A stream may send 64 KiB of its body, then gets more only as the handler takes it (streamed) or up to `max_body_size`. Together, the bodies a connection holds stay within the larger of `max_body_size` and 1 MiB: past it, only the oldest stream still receiving gets more, so bodies arrive in turn instead of at once. | |
| A client that does not read ([CVE-2019-9517](https://nvd.nist.gov/vuln/detail/CVE-2019-9517)) | With 256 KiB of output waiting (answers to PING and SETTINGS included), the server stops reading the connection. A stream that the client's flow control keeps from sending for longer than `write_timeout` is reset with `CANCEL`. | |
| Protocol errors | The connection ends with GOAWAY and the error RFC 9113 calls for; a malformed request ends its stream only. | `GIONG-E1115` |

Timeouts are the same as for HTTP/1.1: `request_timeout` for a request's head and body, `write_timeout` for each write, `idle_timeout` while no stream is open (then a GOAWAY).

### Checked

- [h2spec](https://github.com/summerwind/h2spec) passes all 147 cases over TLS in strict mode; CI runs it on every pull request, over TLS and on the plain port.
- Two fuzz targets run every night: `hpack` decodes header blocks, encodes them again and compares, and does the same for Huffman codes; `h2` plays whole connections from a client that mostly follows the protocol and checks every frame the server writes against RFC 9113 and the client's flow control. The same checks run in `cargo test` on 100,000 header blocks and 3,000 connections.
- 17 end-to-end tests on real sockets, with a client that writes its frames by hand: requests with and without bodies, trailers, `100 Continue`, streamed bodies both ways, file bodies (and a file shorter than announced, which resets its stream), the client's flow control, the body budget, a reset that drops its handler, every limit in the table, idle connections, graceful shutdown, HTTP/1.1 and HTTP/2 on one port. 20 unit tests of frames, HPACK (every example in RFC 7541) and requests.

### Performance

Measured with `bench/h2.sh` against axum 0.8 on hyper's HTTP/2 (through `axum-server` for HTTPS). Both use rustls with the `ring` provider and set `TCP_NODELAY`. The server has one core of a 4-vCPU virtual machine (Xeon 2.3 GHz, Linux 6.18) and h2load (nghttp2) the other three. `json` is a 27-byte answer and `users` an 8.5 KiB one, each over 48 connections with 10 streams at once; `json-mux` is the 27-byte answer over 3 connections with 100 streams each, the way a browser loads a page. Medians of 5 rounds of 5 s, with the range in brackets; requests per second:

| Server | `json` | `users` | `json-mux` |
|---|---|---|---|
| **giong-http**, HTTPS | **501.9k** (450–518k) | **58.0k** (52–69k) | **720.1k** (706–802k) |
| axum, HTTPS | 131.0k (126–137k) | 23.3k (22–26k) | 184.5k (149–190k) |
| **giong-http**, prior knowledge | **557.3k** (533–580k) | **69.9k** (62–74k) | **696.5k** (618–740k) |
| axum, prior knowledge | 138.9k (126–146k) | 23.0k (22–26k) | 158.0k (152–171k) |

- **3.8 to 4.4 times axum's requests per second on small answers, 2.5 to 3.0 times on 8.5 KiB ones.** The server's core is the limit in every case (97–100% busy), and no request failed. h2load's mean time per request follows: 0.83 ms against 3.63 ms for `json` over HTTPS, 8.1 against 20.5 ms for `users`.
- Our reading, not profiled: with 10 or 100 streams in flight on a connection, one read brings many requests and one write takes many answers, much as pipelining does over HTTP/1.1, and giong-http turns that batching into requests served as it does for pipelined ones ([giong-web's benchmark](../giong-web/README.md#performance)). hyper gains far less from either.
- Not measured yet: several cores, a load generator on another machine, request bodies.

Memory per connection, measured with `bench/hold` over HTTP/2 with prior knowledge: 5,000 connections on one worker thread, the server's resident memory before and after, divided by 5,000. KiB, the same in all 5 rounds within 0.2%:

| Server | Idle (one request answered) | One request in progress | Each further request in progress |
|---|---|---|---|
| **giong-http** | **4.2** | **5.9** | **1.1** |
| axum | 23.8 | 23.0 | 2.0 |

An idle connection gives its buffers back, so what it keeps is its task, its HPACK tables and its state; an idle HTTP/1.1 connection still costs 1.9 KiB. Results are in `bench/results/h2/`.

## Client

The `client` feature adds an HTTP/1.1 and HTTP/2 client, `giong_http::client` (HTTPS included, through giong-tls):

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
- **HTTP/2 when the server picks it.** `https` connections offer `h2` and `http/1.1` through ALPN; a server that picks `h2` gets every request to its origin on one connection, as many at once as it allows (another connection is opened, up to the limit, when it allows no more), and `Response::version()` says which was spoken. `ClientBuilder::http2(false)` sticks to HTTP/1.1; `ClientBuilder::http2_prior_knowledge(true)` speaks HTTP/2 to `http` URLs from the first byte, for servers known to accept it (gRPC backends, giong-http's own). What a server can make the client hold is bounded: 1 MiB of each response body and 4 MiB in all on a connection until the program reads them, header blocks by the head limit. A request the server refused unprocessed (`REFUSED_STREAM`, or left out by a GOAWAY) is sent again whatever its method; a dropped request resets its stream and the connection carries on. The client's side of HTTP/2 is fuzzed every night (`h2_client`).
- **HTTPS, verified.** Certificates are checked against the machine's certificate authorities and the host name; `ClientBuilder::tls` takes a `TlsConnector` for a private authority or a client certificate. A redirect from HTTPS to HTTP is refused.
- **Redirects** (301, 302, 303, 307, 308) are followed, up to 10. 303, and 301 or 302 after a `POST`, become a `GET` without the body; 307 and 308 repeat the request as it was. `Authorization`, `Cookie` and a `Host` set by hand are not sent to another origin.
- **Finite limits.** A request has 30 s in all (`ClientBuilder::timeout`, or `RequestBuilder::timeout` for one): waiting for a connection, connecting (10 s of it at most), sending, reading the response, redirects. A response body may have 16 MiB, its head 64 KiB. An unused connection is closed after 60 s.
- **Strict reading.** Bodies by `Content-Length`, chunked, or up to the end of the connection; 1xx responses are skipped. A response that could be read in two ways (two lengths, bare line feeds, folded headers, bytes after the body) is an error or keeps its connection out of the pool, so one response is never read as part of another. The code that reads a response on a connection, and decides whether the connection can carry the next request, is fuzzed every night (`response_body`): the body must end exactly where its framing says, whatever pieces the bytes arrive in, and a connection that received a byte past the body is never reused.
- **Proxies.** `ClientBuilder::proxy(Proxy::all("http://user:pass@proxy:3128")?.no_proxy(".svc.cluster.local, 10.0.0.0/8")?)`, or `Proxy::from_env()` for `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` the way curl reads them (never read unless asked). `https` goes through a `CONNECT` tunnel, so the proxy sees only the host name; the credentials go to the proxy, never to the server. Plain `http://` proxies only, for now (no SOCKS, no TLS to the proxy itself).
- **Compression.** Responses are asked for with `Accept-Encoding: gzip, deflate` and decompressed as they arrive (`ClientBuilder::decompress(false)` turns it off; a request with its own `Accept-Encoding` gets the bytes as sent). The size limit applies to the decompressed body, so 32 KiB that would expand to gigabytes fails at the limit instead of filling memory; the gzip checksum is checked, and a body cut short is an error, not a shorter body.
- **Streaming.** `RequestBuilder::send_streaming()` returns once the head has arrived; `StreamingResponse::chunk()` gives the body piece by piece (64 KiB at most, decompressed), with no limit on its size and the timeout applying to each piece. Read to its end, the connection goes back to the pool; dropped before, it is closed (over HTTP/2, only its stream is reset).
- **Errors that say what to do.** `Error::kind` is one of `Request`, `Connect`, `Timeout`, `TooLarge`, `Redirect`, `Closed`, `Protocol`, `Status`, `Decode` and `Proxy`, each with a giong code (GIONG-E1101 to E1110) and a page in [docs/errors](../docs/errors/README.md); `Error::is_transient` says whether retrying can help. Any status is a `Response`; `error_for_status` turns 4xx and 5xx into an error with the start of the body.

Proxies, compression, streamed bodies and the `Proxy` error kind are new in 0.4.0; giong-http 0.3 does not have them.

The client is not `Send`, like everything in giong: make one per worker (in `Builder::run`'s closure) and clone it into that worker's tasks.

### Client performance

Measured with `bench/http-client.sh` against reqwest 0.12 (rustls, HTTP/2 on, on a current-thread Tokio runtime). Each client runs on one core and shares one client between 64 tasks that send `GET` requests back to back and read every body; giong-http's server answers on the other three cores of a 4-vCPU virtual machine (Xeon 2.3 GHz, Linux 6.18), over TCP on 127.0.0.1. `json` is a 27-byte answer, `users` an 8.5 KiB one. Medians of 5 rounds of 5 s, with the range in brackets; requests per second.

Over HTTP/1.1 (both clients told to speak it, since the server also offers HTTP/2):

| Client | Connections | `json` | `users` | `json` over HTTPS | `users` over HTTPS |
|---|---|---|---|---|---|
| **giong-http** | 64 | **150.1k** (120–167k) | **91.7k** (88–136k) | **145.3k** (118–156k) | **81.2k** (76–93k) |
| giong-http, its default | 16 | 113.8k (84–126k) | 82.3k (78–100k) | 99.9k (80–110k) | 84.2k (65–90k) |
| reqwest, its default | 64 (one per task) | 84.7k (66–87k) | 58.1k (56–81k) | 84.6k (62–88k) | 48.8k (40–56k) |

Over HTTP/2, every request on one connection:

| Client | `json` over HTTPS | `users` over HTTPS | `json`, prior knowledge | `users`, prior knowledge |
|---|---|---|---|---|
| **giong-http** | **250.7k** (242–264k) | **64.2k** (55–66k) | **265.7k** (251–273k) | **68.4k** (68–71k) |
| reqwest | 122.9k (116–135k) | 48.3k (43–51k) | 123.1k (119–124k) | 56.0k (43–65k) |

- **HTTP/1.1 with as many connections: 1.6 to 1.8 times reqwest's requests per second, with a lower p99** (0.66–1.5 ms against 1.15–2.0 ms). Every client's core is the limit (99–100% busy); the server's three cores are not (50–159% of 300).
- **With its default of 16 connections, still 1.2 to 1.7 times reqwest with 64.** Fewer requests in flight means fewer of them handled per trip into the kernel; the default is there to spare the server, and `max_connections_per_host` raises it.
- **HTTP/2: 2.0 to 2.2 times reqwest on small answers, 1.2 to 1.3 times on 8.5 KiB ones**, with half the latency on small ones (p50 0.24 against 0.49 ms, p99 0.41 against 0.84 ms over HTTPS). On one connection the two ends take turns (the client writes every request queued at once, the server answers them all at once), so neither is busy all the time: giong-http's core is 51–63% busy, reqwest's 60–88%. With 8.5 KiB answers the server's single worker for the single connection is the limit (94% of a core), which is why HTTP/1.1 over 64 connections, spread over the server's three workers, gets more there; with small answers, HTTP/2 gets 1.7 times what HTTP/1.1 does.
- Not measured: a server on another machine, `POST` bodies, more than one client core, many hosts at once.

Results are in `bench/results/http-client/`.

## Not there yet

Server push. Routing and compression of responses are in [`giong-web`](../giong-web/README.md). In the client: cookies, streaming request bodies, `br` and `zstd`, SOCKS proxies, HTTP/2 through an `http://` proxy (HTTPS through a proxy's tunnel gets HTTP/2), the `Upgrade: h2c` handshake.

## Tests

`cargo test -p giong-http --all-features`: 62 unit tests (including 500,000 random inputs for the parser and the chunked decoder, 300,000 for the client's response parser and URLs, 100,000 responses read by the client on a connection in random pieces, 50,000 damaged gzip and deflate bodies, 200,000 HPACK header blocks and Huffman strings, 3,000 HTTP/2 connections whose every frame is checked, and 3,000 from the client's side, checking what it holds and writes), 37 HTTP/2 tests (17 end-to-end on real sockets with a client that writes its frames by hand, 20 of frames, HPACK and requests), 24 end-to-end tests of the server on real sockets (one of them over a Unix socket), 14 end-to-end tests of streamed bodies (a 250 KB upload read piece by piece with the connection used again after it, chunked uploads, bodies past the handler's limit, broken framing, nothing read and no `100 Continue` until the handler asks, a handler that answers early, a body cut short, the timeout of each piece; streamed responses chunked and sent piece by piece, to an HTTP/1.0 client, without a body for `HEAD`, 204 and 304, aborted, and stopped when the client goes away), 12 end-to-end HTTPS tests with a rustls client (TLS 1.2 and 1.3, pipelining, a 3 MB body, 400/413 errors, idle connections, clients that do not speak TLS, graceful shutdown, 16 client threads at once), 22 end-to-end tests of the client against this server, against servers that misbehave on purpose and through proxies, 15 of the client over HTTP/2 (against this server through ALPN, over kernel TLS where the kernel has it, and with prior knowledge, and against servers that refuse streams, send GOAWAY, reset streams, send malformed responses, close early, send past the window or wait for a window the client grants only once the connection has gone quiet), 15 documentation examples. One more client test, ignored by default (`-- --ignored`), fetches from crates.io over HTTPS.
