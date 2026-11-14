# giong-web

Routing, middleware and JSON for HTTP services on the [giong](../README.md) runtime, on top of [`giong-http`](../giong-http/README.md). A handler is a plain `async fn`; every error, yours or the framework's, is answered as a problem+json body with a stable code. No macros, no Tokio.

```toml
[dependencies]
giong = "0.4"
giong-web = { version = "0.4", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
```

```rust
use std::time::Duration;
use giong::net::TcpListener;
use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
use giong_web::{json, Error, Req, Response, Router, Server, Sink};
use serde::{Deserialize, Serialize};

struct State {
    greeting: &'static str,
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
}

#[derive(Serialize)]
struct User {
    id: u64,
    name: String,
}

async fn hello(req: Req<State>) -> Result<Response, Error> {
    let name: String = req.param("name")?;
    Ok(Response::text(format!("{}, {name}!", req.state().greeting)))
}

async fn get_user(req: Req<State>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?; // "abc" is a 400 that names `id`
    if id == 0 {
        return Err(Error::not_found("user_not_found", format!("no user {id}")));
    }
    json(200, &User { id, name: "Ada".into() })
}

async fn create_user(req: Req<State>) -> Result<Response, Error> {
    let new: NewUser = req.json()?; // 415, 400 or 422 with what is wrong
    json(201, &User { id: 1, name: new.name })
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(10))
        .build()?
        .run(|_worker| async {
            // Every worker builds its own app: nothing is locked, nothing needs `Send`.
            let app = Router::new()
                .get("/hello/{name}", hello)
                .get("/users/{id}", get_user)
                .post("/users", create_user)
                .layer(RequestId::new())
                .layer(Logger::new(Sink::Stdout))
                .layer(Compression::new())
                .layer(Timeout::new(Duration::from_secs(10)))
                .build(State { greeting: "Hello" });
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, app).await;
        })
}
```

## What it does

- **One way to write a handler.** `async fn(Req<S>) -> Result<Response, Error>`, where `S` is the state given to `build`. `?` works on any error type: what the client should know is an `Error` with a status, a code and a detail; anything else becomes a `500` whose cause goes to the log and never to the client.
- **Typed parameters.** `req.param::<u64>("id")?` and `req.query_param::<u32>("page")?` parse into any `FromStr` type. A value that does not parse is a `400` naming the parameter, the value and the type expected. With `serde`, `req.query::<T>()` reads the whole query string into a struct and `req.json::<T>()` the body.
- **Routing that answers what it can for you.** Patterns with parameters (`/users/{id}`) and a catch-all (`/files/{*path}`). A literal segment wins over a parameter and a parameter over a catch-all, with backtracking, so `/users/me` and `/users/{id}` live side by side. `HEAD` is answered by the `GET` route, `OPTIONS` and `405` with `Allow` for every path, and a `404` for `/users/` asks "did you mean /users?" when that route exists.
- **Errors that explain themselves.** Every error answer is an [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) problem+json body with a code an agent or a client can match on, and the request id when there is one. The codes giong-web uses itself (`GIONG-E1201` to `GIONG-E1213`) each have a page with the cause and the fix in [docs/errors](../docs/errors/README.md).
- **Mistakes in the program fail at startup, not on the first request.** Two routes that would answer the same requests, a pattern that is not valid, or middleware settings that cannot work (any origin with credentials) panic with a code and the line that added them.
- **Middleware is one `async fn`.** They run in the order added, and those of a nested router only for its routes. Included: request ids, JSON-line logs, CORS, gzip and timeouts, and with the `tracing` feature a `tracing` span per request.
- **Bodies of any size.** The routes of a router with `stream_bodies(max)` read their request body as it arrives; any handler can write its response body as it is produced. See "Streaming".
- **Tested without a socket.** `App::handle(request)` answers a `Request` built with `Request::from_bytes`, so routes and middleware are tested with plain unit tests.
- **Per-worker and lock-free.** The app, its state and its middleware live on one worker. A worker that compresses a large body yields to the others' requests every 64 KiB.

## Routing

```rust
let members = Router::new()
    .get("/", list_members)          // GET /teams/{team}
    .get("/members/{id}", member)    // GET /teams/{team}/members/{id}
    .layer(RequireAuth);             // only for these two routes
let app = Router::new()
    .get("/files/{*path}", file)     // path = "a/b c.txt" for /files/a/b%20c.txt
    .nest("/teams/{team}", members)
    .build(state);
```

- A path parameter is one segment, percent-decoded; the catch-all takes the rest of the path, at least one character. A pattern has at most 16 parameters.
- `/users` and `/users/` are different paths. giong-web does not redirect from one to the other; its `404` names the one that exists.
- `req.route()` gives the pattern that matched (`/teams/{team}/members/{id}`), for logs and metrics.
- `Router::route(method, pattern, handler)` takes any method; `get`, `post`, `put`, `patch` and `delete` are shortcuts.

## Errors

```json
{"status":400,"code":"GIONG-E1205","title":"Bad Request","detail":"path parameter `id`: cannot read \"abc\" (invalid digit found in string); expected an unsigned integer","request_id":"3f9a0c1e02000000002a"}
```

| Status | Code | When |
|---|---|---|
| 404 | `GIONG-E1203` | no route for the path |
| 405 | `GIONG-E1204` | the path has routes, not for this method; `Allow` lists them |
| 400 | `GIONG-E1205` | a path or query parameter that does not parse |
| 500 | `GIONG-E1206` | the handler asked for a parameter its route does not have (a bug: the cause names the route) |
| 400, 422 | `GIONG-E1207` | a JSON body that is not JSON (`400`) or does not fit the type (`422`) |
| 415 | `GIONG-E1208` | a body that should be JSON and has another `Content-Type` |
| 503 | `GIONG-E1209` | `Timeout` ran out |
| 500 | `GIONG-E1210` | the handler failed; the cause is logged, never sent |
| 503 | `GIONG-E1211` | a database or service the handler needs cannot be reached (giong's connect, timeout and pool errors); `Retry-After: 1` |
| 403 | `GIONG-E1212` | a CORS preflight that `Cors` refused, saying which of origin, method or header |

Your own errors take your own codes: `Error::not_found("user_not_found", "no user 42")`, `Error::conflict(..)`, `Error::new(429, "rate_limited", ..).with_header("Retry-After", "30")`. `Error::of(&response)` reads the error back from a response, for middleware and tests. Every error response has `Cache-Control: no-store`.

## Middleware

```rust
use giong_web::{Error, Middleware, Next, Req, Response};

struct RequireAuth;

impl<S: 'static> Middleware<S> for RequireAuth {
    async fn call(&self, mut req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        match req.header_str("authorization") {
            Some(token) if token.starts_with("Bearer ") => {
                req.set_extension(UserId(42)); // the handler reads req.extension::<UserId>()
                Ok(next.run(req).await)
            }
            _ => Err(Error::unauthorized("no_token", "send a bearer token")
                .with_header("WWW-Authenticate", "Bearer")),
        }
    }
}
```

`next.run(req)` returns the `Response`, with an error from further in already turned into its problem+json answer, so a middleware sees and can change every response. Add them in this order, outermost first:

| Middleware | What it does | Defaults |
|---|---|---|
| `RequestId::new()` | gives every request an id (`req.request_id()`) and sends it back in `X-Request-Id` | ids made here; an incoming one is used only with `.trust_incoming(true)` |
| `Logger::new(Sink::Stdout)` | one JSON line per request: time, method, path, route, status, duration, bytes, request id, peer, worker, and for errors the code, detail and cause | query strings left out (`.include_query(true)`); written by a background thread, never on the worker |
| `Cors::new()` | answers preflights and adds `Access-Control-Allow-*` for the origins named | no origin allowed until named with `.allow_origin(..)`; methods `GET`, `HEAD`, `POST`; no extra headers |
| `Compression::new()` | gzip for clients that accept it | level 1, bodies from 1 KiB to 16 MiB; text, JSON, JavaScript, XML (SVG too) and WebAssembly |
| `Timeout::new(d)` | answers `503` (`GIONG-E1209`) when the handler takes longer than `d` | not for routes that stream their request body |
| `Trace::new()` (feature `tracing`) | runs each request in a `tracing` span with OpenTelemetry's field names (`http.request.method`, `url.path`, `http.route`, `http.response.status_code`, `client.address`) plus `request_id` and `worker`, ended by one event; a `5xx` at `ERROR` with its code and cause | instead of `Logger`, or beside it |

A log line:

```json
{"ts":"2026-11-13T09:21:04.118302Z","level":"info","event":"request","method":"GET","path":"/users/42","route":"/users/{id}","status":200,"duration_us":84,"bytes":51,"request_id":"3f9a0c1e02000000002a","peer":"10.0.0.7:51122","worker":2}
```

The log thread batches lines every 10 ms. When it falls behind by more than 1 MiB per worker, lines are dropped and counted in a `log_dropped` line, instead of the workers waiting or memory growing. `giong_web::flush_logs()` waits for what is queued (up to 2 s); dropping an `App` calls it.

## Streaming

```rust
use giong_web::{Body, Error, Req, Response, Router};

async fn upload(req: Req) -> Result<Response, Error> {
    let body = req.body_stream().expect("a streaming route");
    let mut total = 0;
    while let Some(piece) = body.chunk().await? {
        total += piece.len(); // write it to a file, hash it, ...
    }
    Ok(Response::text(format!("{total} bytes")))
}

async fn events(_req: Req) -> Result<Response, Error> {
    let (tx, body) = Body::channel();
    drop(giong::spawn(async move {
        for n in 0.. {
            if tx.send(format!("data: {n}\n\n")).await.is_err() {
                break; // the client went away
            }
            giong::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }));
    Ok(Response::new(200).header("Content-Type", "text/event-stream").body(body))
}

let uploads = Router::new().put("/files/{*path}", upload).stream_bodies(10 << 30);
let app = Router::new().get("/events", events).nest("/api", uploads).build(());
```

- `stream_bodies(max)` applies to every route of its router, wherever the router is nested. Other routes still get their body in full, within the server's `max_body_size`.
- Nothing of the body is read until the handler asks, so a handler can refuse an upload (no credentials, no space) before the client sends it. One piece is in flight at a time: a slow handler slows the client down instead of filling memory.
- `?` on a body error answers `413` (`GIONG-E1111`, past `max`) or `400` (`GIONG-E1112` cut short, `GIONG-E1113` malformed). A body announced past `max` gets `413` before the handler runs. `req.json()` on a streaming route is a `500` whose logged cause says to read the stream.
- `Timeout` does not limit a streaming route, whose handler runs while the body arrives; each piece still has the server's `request_timeout`. `Compression` leaves streamed responses alone, and `Logger` writes `"bytes":null` for them.
- A streamed response goes out chunked, each piece as soon as it is sent. `send` fails once the client has gone (noticed at the next write), so the producer stops.

## Testing

```rust
use giong_web::{Request, Router};

let app = Router::new().get("/users/{id}", get_user).build(state);
let req = Request::from_bytes(b"GET /users/abc HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
let resp = giong::block_on(app.handle(req));
assert_eq!(resp.status(), 400);
assert_eq!(giong_web::Error::of(&resp).unwrap().code(), "GIONG-E1205");
```

## Performance

Measured with `bench/web.sh`: the same handlers on giong-web and on axum 0.8.9 (Tokio 1.54, hyper 1.12), both with serde_json 1.0, each server on one core and `wrk` on another of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), 50 connections. `/json` serializes a one-field struct, `/users` a list of 100 users (8 KB), `/users/{id}` reads a path parameter, `/compute?n=25` a typed query and then computes a Fibonacci number. The middleware rows add request ids, gzip at level 1 from 1 KiB and a 10 s timeout: giong-web's own, and tower-http 0.6.11's for axum; `wrk` asks for gzip. Requests per second, median of 5 interleaved rounds of 5 s, with the range; p99 is the median of the rounds' p99s.

| | `/json` | p99 | `/users` | p99 | `/users/{id}` | `/compute` | Pipelined `/json` |
|---|---|---|---|---|---|---|---|
| **giong-web** | **152.5k** (147–187k) | **0.93 ms** | **28.0k** (25–31k) | **3.9 ms** | **140.5k** (124–162k) | 3.7k | **804k** |
| axum | 118.3k (111–141k) | 1.26 ms | 15.2k (14–16k) | 5.3 ms | 107.1k (74–124k) | 3.9k | 18k |
| giong-http, a bare handler | 160.4k (159–168k) | 0.81 ms | 48.3k (43–51k) | 2.6 ms | | 3.5k | 1.16M |

With middleware:

| | `/json` | p99 | `/users`, gzip | p99 |
|---|---|---|---|---|
| **giong-web** | **126.2k** (124–133k) | **1.04 ms** | **12.0k** (11.2–12.2k) | **7.9 ms** |
| axum + tower-http | 65.8k (59–75k) | 1.91 ms | 5.2k (4.6–5.9k) | 13.6 ms |

- **1.3 times axum on small JSON answers, 1.8 times on the list of users, 1.9 to 2.3 times with middleware.** The routes, the parameters and serde cost giong-web about 5% against giong-http's bare handler on `/json`. giong-http's `/users` is faster because it writes its JSON by hand, without serde or the 200 small strings the handler builds; giong-web and axum run the same handler.
- **`/compute` is the handler's time,** the same for both within noise (giong-web's range 3.66–4.01k, axum's 3.58–4.07k). Its p99 is higher on giong-web (22.9 against 18.5 ms), as it is on giong-http (24.2 ms). Our guess, not measured: a worker runs every ready request before it reads its sockets again, so when each costs 250 µs of CPU, some wait for a whole batch.
- **Pipelined requests** (16 per write): axum stays at 18k, as in the [main benchmark](../README.md#throughput-at-50-connections); we have not looked into why. giong-http reads a pipeline and answers it as a batch.
- Resident memory after the run: 6.6 MB for giong-web, 5.2 MB for axum.

HTTPS and a database behind the handlers are measured with the [example REST API](../examples/rest-api/README.md#performance). Not measured yet: several cores, and middleware one at a time. Results are in `bench/results/web/`.

## Features

| Feature | Adds |
|---|---|
| `serde` | `req.json::<T>()`, `req.query::<T>()` and `json(status, &value)`, through serde and serde_json |
| `tls` | `Server::serve_tls`, through giong-http: HTTP/2 or HTTP/1.1, as ALPN picks |
| `tracing` | `middleware::Trace`, through `tracing` 0.1 |

Apps are served over HTTP/2 as well as HTTP/1.1, with nothing to change: giong-http turns each HTTP/2 stream into the request a handler already knows (see [its HTTP/2 section](../giong-http/README.md#http2)). Over TLS, ALPN picks the protocol; without TLS, a client that knows the server speaks HTTP/2 (prior knowledge) gets it on the same port.

## Not there yet

Cookies and sessions, forms and multipart bodies, WebSockets, serving static files, and compressions other than gzip. The crate has had no independent security audit.

## Tests

`cargo test -p giong-web --all-features`: 23 tests without a socket, most through `App::handle`: text beating parameters beating catch-alls, `HEAD`, `404` with its hint, `405` and `OPTIONS` with `Allow`, parameters that do not parse and a parameter the route lacks, query strings, errors from `?`, state and nested routers, the order middleware run in and which routes they see, a middleware answering by itself and passing values to the handler, closures, the startup panics for bad and duplicate routes; request ids (made, trusted only when asked, sent back), log lines and their fields, timeouts, CORS preflights allowed and refused and simple requests, every middleware setting that cannot work, gzip and when not to use it; JSON bodies in and out, `415`, `400` and `422`, typed queries. One test over real TCP with giong-http's client: gzip on the wire, CORS, `405`, 50 requests on one kept-alive connection. 5 tests of streamed bodies over real TCP: uploads past the server's body limit on a streaming route, `413` with its code for one past the route's limit, a slow upload that the timeout leaves alone, `req.json()` on a streaming route, a streamed response that passes through every middleware untouched. 2 tests of `Trace` with a recording subscriber: the span's fields, and the code and cause of errors. 32 unit tests, among them 30,000 random route sets matched against a slow reference matcher, 4,000 random requests through every middleware with the checks below, and bodies around the 64 KiB compression slice. 14 documentation examples.

Two fuzz targets in `fuzz/` (`cargo +nightly fuzz run request`, or `routes`): `routes` checks that the route table finds the route a slow reference finds, with the same parameters, or the same `404`, `405` or `OPTIONS` answer; `request` sends any bytes as a request to an app with every middleware and checks that the answer is well formed: a valid status and headers, a problem+json body with a code for every error, gzip only when asked and decoding to what the handler wrote, one log line that is JSON and agrees with the response, and the request id sent back. 5.4 million inputs ran without a failure when the crate was added; CI fuzzes both every night.
