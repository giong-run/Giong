---
name: giong
description: Write, review and debug Rust services on the giong async runtime and its crates (giong-http, giong-web, giong-grpc, giong-tls, giong-quic, giong-postgres, giong-mysql, giong-redis, giong-mcp), and port Tokio code to giong. Use whenever a Cargo.toml depends on a giong crate or the user asks for giong.
---

# giong

giong is a thread-per-core async runtime for Rust: one worker per core, each with its own
io_uring ring (epoll where io_uring is blocked; kqueue on macOS; I/O completion ports on
Windows, runtime crate only), and its own crates for a whole service with no Tokio
underneath. The API is strict and stays on major version 1: one way to do each thing,
coded errors that say how to fix them, releases that only add.

The `references/` folder next to this file (in the zip from
`https://giong.run/skills/giong.zip`) holds the README of every crate, the best practices,
the deployment notes and the error code index of the release this skill was built from.
Read the one for the crate you are using before writing code against it; without the
folder, read `https://giong.run/docs/<crate>.md`. The
API reference is on docs.rs (`https://docs.rs/<crate>`); the index for models is
`https://giong.run/llms.txt`.

## Rules that are easy to get wrong

1. **No macros.** Start with `giong::Builder::new().shutdown_on_signals(true).build()?.run(|_worker| async { .. })`
   (one future per worker) or `giong::block_on(fut)` (one thread, and in tests). There is no
   `#[giong::main]` and no `#[giong::test]`.
2. **Build per-worker things inside the worker's future**: listeners, `Router`s (an `App`
   is not `Send`), database pools (`Pool::new(config.clone(), 4)`), HTTP clients. Clone
   configuration into the closure.
3. **Futures need not be `Send`.** A task stays on its worker. Per-worker state is
   `Rc<RefCell<T>>`, never borrowed across an `.await`; across workers use `Arc` of
   immutable data, atomics, `giong::sync::{mpsc, oneshot}`, or Tokio's `sync` module
   (`tokio = { version = "1", default-features = false, features = ["sync"] }`).
4. **Never block a worker.** No `std::fs`, `std::net`, `std::thread::sleep` or long loops
   on it. Use `giong::fs`, `giong::net`, `giong::time`. Blocking calls go to
   `giong::spawn_blocking(f).await`, CPU-heavy work to `giong::spawn_compute(f).await`
   (threads below the workers' priority). Call `giong::yield_now().await` in long loops.
5. **`bind` takes IP addresses**: `TcpListener::bind("0.0.0.0:8080")`, never
   `"localhost:8080"` (`GIONG-E0100`). Every worker binds the same port. `TcpStream::connect`
   resolves names itself; `giong::net::lookup_host` resolves them without blocking.
6. **Buffers move.** `stream.recv().await?` lends a pool buffer: copy what you need, drop it,
   do not hold it across an `.await`; an empty buffer means the peer closed. A write takes
   its buffer by value and gives it back: `let (result, buf) = stream.write_all(buf).await;`.
7. **Cancel safety**: `recv`, `recv_from`, `accept` are cancel safe (a `timeout` around them
   loses nothing); `read`, `read_from`, `read_at` on a pipe are not; a dropped `write_all`
   may have sent part of its buffer. Wrap every wait for another party in
   `giong::time::timeout(duration, fut)`.
8. **Errors**: every giong error starts with a code (`GIONG-E0102: ... Fix: ...`) and links to
   its page. Match on `giong::error::code(&err)`, never on the message. When you meet a code,
   read its page (`references/errors.md`, or
   `https://github.com/giong-run/Giong/blob/main/docs/errors/<CODE>.md`) and apply its fix.
9. **Database pools**: `pool.query(..)`, `query_one`, `execute` for standalone statements
   (they pipeline across tasks). Transactions, `SET`, `LISTEN`, temporary tables and `USE`
   are refused on the pool: run them on `let mut conn = pool.get().await?;`, which comes back
   with its session reset. Redis: `pool.take()` for `WATCH` and blocking commands.
10. **giong-web handlers** are `async fn(Req<S>) -> Result<Response, Error>`. Use `?`: a giong
    error becomes the right status (an unreachable database is a `503`), anything else a
    `500` whose cause is logged, never sent. `req.param::<T>("id")?`, `req.query::<T>()?`,
    `req.json::<T>()?`, `json(200, &value)`, `Error::not_found("code", msg)`.
11. **Shutdown**: `shutdown_on_signals(true)` and `shutdown_grace(Duration)` on the builder;
    `giong::until_shutdown(fut)` around waits in your own accept loops.

## Which crate for which job

Take the first that fits:

| Job | Use | Not |
|---|---|---|
| HTTP/1.1, HTTP/2 server | `giong-http` (`Server::serve`, `serve_tls` with feature `tls`) | hyper, axum |
| Routing, JSON, middleware | `giong-web` (`Router`, `RequestId`, `Logger`, `Cors`, `Compression`, `Timeout`) | axum, tower-http |
| HTTP client | `giong-http`, feature `client` (`client::Client`, one per worker) | reqwest |
| TLS | `giong-tls` (`TlsAcceptor`, `TlsConnector`) | tokio-rustls |
| gRPC | `giong-grpc`, code from `giong-grpc-build` in `build.rs` (no `protoc`) | tonic |
| Protocol Buffers | `giong-proto` (views over the buffer) | prost |
| HTTP/3, QUIC | `giong-http` feature `unstable-http3`, `giong-quic` (Rust 1.85; may change) | quinn, h3 |
| PostgreSQL | `giong-postgres` (`Pool` per worker) | sqlx, tokio-postgres |
| MySQL, MariaDB | `giong-mysql` | sqlx, mysql_async |
| Redis | `giong-redis` | redis-rs, fred |
| Files, timers, names | `giong::fs`, `giong::time`, `giong::net::lookup_host` | `tokio::fs`, `tokio::time` |
| Looking inside a running service | `giong-mcp` | |

Crates that need no runtime (serde, rustls, `futures::join!`/`select!`, Tokio's `sync`) work
as they are. A crate generic over a Tokio or futures-io stream (tokio-tungstenite, h2,
tokio-rustls, hyper's `conn`) runs through `giong_compat::Compat::new(stream)`, only for a
protocol giong has no crate for. **A crate that needs the Tokio runtime (sqlx, reqwest,
tonic's transport, most cloud SDKs) panics on a giong worker** with "there is no reactor
running": replace it, or run it on a Tokio runtime in a thread of its own behind a channel.

## Porting from Tokio

| Tokio | giong |
|---|---|
| `#[tokio::main]` | `giong::Builder::new().build()?.run(\|_\| async { .. })` or `giong::block_on` |
| `tokio::spawn` | `giong::spawn` (stays on the worker; no `Send`); `Handle::spawn_on` / `spawn_balanced` for other workers |
| `tokio::join!`, `select!` | `futures::join!`, `futures::select!` |
| `spawn_blocking` | `giong::spawn_blocking` for blocking calls, `giong::spawn_compute` for CPU-heavy work |
| `Arc<Mutex<T>>` in handlers | `Rc<RefCell<T>>` per worker, or a channel |
| `tokio::time::interval` | a loop around `giong::time::sleep_until(next)` |
| `tokio::signal::ctrl_c()` | `Builder::shutdown_on_signals(true)` + `giong::shutdown_signal().await` |
| `read(&mut buf)` / `write_all(&buf)` | `recv()` lends a buffer; `write_all(buf)` returns `(result, buf)` |

## A service, start to finish

```rust
use giong::net::TcpListener;
use giong_web::middleware::{Logger, RequestId};
use giong_web::{json, Error, Req, Response, Router, Server, Sink};
use serde::Serialize;

#[derive(Serialize)]
struct User { id: u64, name: String }

async fn get_user(req: Req<()>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;   // "abc" is a 400 that names `id`
    json(200, &User { id, name: "Ada".into() })
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .build()?
        .run(|_worker| async {
            let app = Router::new()
                .get("/users/{id}", get_user)
                .layer(RequestId::new())
                .layer(Logger::new(Sink::Stdout))
                .build(());
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, app).await;
        })
}
```

`cargo add giong giong-web serde --features serde/derive`, then `giong dev --background`
(see Local development below; `cargo run` without the giong command).

## The giong command

`cargo install giong-cli` installs `giong`: one command per job, the same on every
project. Every command takes `--json` and reports each problem with a `GIONG-E50xx` code,
the cause and the fix.

| You want | Run |
|---|---|
| Start a project | `giong new api --template web --db postgres` (templates `web`, `grpc`, `worker`; `--db postgres`, `mysql`, `redis`, comma-separated; `--tls`) |
| Teach this project to your agent | `giong ai init` (`giong new` does it) |
| Work on it | `giong dev --background`, once (see Local development) |
| Is my machine right? | `giong doctor` |
| What is wrong with my code? | `giong check --json`, then `giong fix` |
| What does this error mean? | `giong explain GIONG-E0102` |
| Run the tests | `giong test` (on io_uring, then on epoll; reads `.env`) |
| A database | `giong db up`, `giong db migrate` |

`giong new` writes a project that builds and passes its tests: the service, an end to end
test, migrations, `.env.example`, a Dockerfile, a CI workflow and fast dev builds. Extend it
rather than starting from an empty `main.rs`. `giong fix` applies only fixes that need no
judgement (the compiler's, clippy's, and `giong check`'s own); what is left needs a decision.

## Local development

One project folder, one runner, on this machine, like `npm run dev`:

1. At the start, run `giong dev --background`. It builds, starts the service, waits until
   it serves, prints its address (`http://127.0.0.1:PORT`) and returns; if one already runs
   for this project, it reports that one. A failed build says why (`GIONG-E5009`) and keeps
   watching: fix the code.
2. Edit the code. `giong dev` rebuilds and swaps the service by itself, without refusing a
   connection.
3. Try it with `curl` on the printed address.
4. `giong dev status --json` says whether it is building, serving or failed (`state`), its
   `urls` and the last build's first error (`build.error`); `giong dev logs --lines 100` shows
   the service's output and requests.
5. `giong dev stop` when you are done. `--all` on `status` and `stop` covers every project
   on this machine.

Do not run the service with `cargo run`, `cargo build --release`, Docker or a second
terminal. One `giong dev` per project.

## Testing

- `giong::block_on(async { .. })` in a plain `#[test]`.
- giong-web without a socket: `app.handle(Request::from_bytes(b"GET /users/7 HTTP/1.1\r\nHost: t\r\n\r\n").unwrap())`,
  then check `resp.status()` and `giong_web::Error::of(&resp)`.
- Run the suite again with `GIONG_DRIVER=epoll`: containers without io_uring use it.

## Debugging a running service

When a giong service is slow, stuck or leaking, ask it instead of guessing. The service
keeps `let _mcp = giong_mcp::start_from_env(&runtime.handle());` alive; run it with
`GIONG_MCP=127.0.0.1:7777 GIONG_MCP_TOKEN=<16+ characters>` and connect:

```sh
claude mcp add --transport http giong http://127.0.0.1:7777/mcp --header "Authorization: Bearer $GIONG_MCP_TOKEN"
```

Tools: `overview` (memory, descriptors, each worker, warnings such as a blocked worker with
the line that spawned the task), `workers`, `tasks` (live tasks by spawn line: a count that
keeps growing is a leak), `sources` (database pools) and `profile` (where the time goes).
Log `giong::driver_choice()` at start-up to see which driver each worker got and why.

## Platforms and deployment

- Production: Linux. io_uring needs 6.0+ and the three io_uring system calls; otherwise
  epoll, by itself. macOS (kqueue) is for development. Windows (1.4+): the `giong` crate
  only, no Unix sockets.
- Docker works with no settings (epoll under the default seccomp profile). For io_uring,
  use the seccomp profile from `references/deployment.md`; `GIONG_DRIVER=io_uring` refuses
  to start without it. Workers follow the container's CPU quota.
- Rust 1.80+, 1.85 for giong-quic and `unstable-http3`.

## Before finishing a change

1. Nothing blocks a worker. 2. Every wait has a deadline. 3. No `RefCell` borrow across an
`.await`. 4. Pools, clients and routers are per worker. 5. Errors matched by code. 6.
Transactions on `pool.get()`. 7. No Tokio-runtime crate on a worker. 8. Tests pass with
`GIONG_DRIVER=epoll` too. 9. `giong check --json` reports nothing new.
