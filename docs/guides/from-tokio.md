# Coming from Tokio

What changes when a Tokio service moves to giong: the runtime model, the names, and the crate for each job. Most code keeps its shape; the differences are where tasks run and who owns buffers.

## The model

- **Thread per core.** Each worker owns its ring, its tasks and its buffers. A task never moves to another thread, so futures need not be `Send`.
- **State per worker.** Build listeners, routers, pools and clients inside each worker's future. Per-worker state is an `Rc<RefCell<T>>`, never borrowed across an `.await`, rather than an `Arc<Mutex<T>>`.
- **Never block a worker.** One blocked call stops every task on that worker. Use `giong::fs`, `giong::net` and `giong::time`; heavy work goes to `std::thread::spawn` and comes back through `giong::sync::oneshot`.
- **Buffers by value.** The kernel uses a buffer after the call, so it is passed in and handed back: `stream.write_all(buf).await` returns `(io::Result<()>, buf)`.

## Name for name

| Tokio | giong |
|---|---|
| `#[tokio::main]` | `giong::Builder::new().build()?.run(\|_\| async { .. })` or `giong::block_on` |
| `tokio::spawn` | `giong::spawn` (stays on the worker; no `Send`); `Handle::spawn_on` or `spawn_balanced` for other workers |
| `tokio::join!`, `select!` | `futures::join!`, `futures::select!` |
| `spawn_blocking` | `std::thread::spawn` + `giong::sync::oneshot` |
| `Arc<Mutex<T>>` in handlers | `Rc<RefCell<T>>` per worker, or a channel |
| `tokio::time::interval` | a loop around `giong::time::sleep_until(next)` |
| `tokio::signal::ctrl_c()` | `Builder::shutdown_on_signals(true)` + `giong::shutdown_signal().await` |
| `read(&mut buf)` / `write_all(&buf)` | `recv()` lends a buffer; `write_all(buf)` returns `(result, buf)` |
| `TcpListener::bind("localhost:8080")` | `TcpListener::bind("127.0.0.1:8080")`: IP addresses only (`GIONG-E0100`) |

`giong::sync` has `mpsc` and `oneshot`. Tokio's `sync` module (`Semaphore`, `Notify`, `watch`, `broadcast`) needs no Tokio runtime and works on giong as it is.

## The crate for each job

| Job | Instead of | Use |
|---|---|---|
| HTTP server | hyper, axum | [giong-http](../../giong-http/README.md) |
| Routing, JSON, middleware | axum, tower-http | [giong-web](../../giong-web/README.md) |
| HTTP client | reqwest | giong-http with feature `client` |
| TLS | tokio-rustls | [giong-tls](../../giong-tls/README.md) |
| gRPC | tonic | [giong-grpc](../../giong-grpc/README.md) |
| PostgreSQL | sqlx, tokio-postgres | [giong-postgres](../../giong-postgres/README.md) |
| MySQL | sqlx | [giong-mysql](../../giong-mysql/README.md) |
| Redis | redis-rs | [giong-redis](../../giong-redis/README.md) |

Crates that need no runtime (serde, rustls, Tokio's `sync`) work as they are. For a protocol giong has no crate for, such as WebSocket, a crate generic over a Tokio or futures-io stream runs through [giong-compat](../../giong-compat/README.md) (`giong_compat::Compat::new(stream)`), at the cost of a copy per byte. Crates that need the Tokio runtime itself (sqlx, reqwest, tonic's transport, most cloud SDKs) panic on a giong worker with "there is no reactor running".

## Let the tools find the rest

`giong check` reports the mistakes a port usually leaves: a blocking call inside async code (`GIONG-E5006`), a listener bound to a host name (`GIONG-E5007`), a `RefCell` borrow held across an `.await` (`GIONG-E5008`). `giong doctor` lists dependencies that need the Tokio runtime.
