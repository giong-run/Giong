# giong-mcp

An [MCP](https://modelcontextprotocol.io) server that lets an AI agent look inside a running [giong](../README.md) service: what each worker is doing, which tasks are alive and which line spawned them, what the connection pools hold, and where the time goes. Read-only, on a loopback address or a Unix socket, and only when the service turns it on.

```toml
[dependencies]
giong = "0.3"
giong-mcp = "0.3"
```

```rust
fn main() -> std::io::Result<()> {
    let rt = giong::Builder::new().shutdown_on_signals(true).build()?;
    // Starts only when GIONG_MCP is set, for example GIONG_MCP=127.0.0.1:7777.
    let _mcp = giong_mcp::start_from_env(&rt.handle())?;
    rt.run(|_worker| async {
        // ... the service ...
        giong::shutdown_signal().await;
    })
}
```

```sh
GIONG_MCP=127.0.0.1:7777 cargo run --release
claude mcp add --transport http giong http://127.0.0.1:7777/mcp    # Claude Code
```

Any MCP client that speaks the Streamable HTTP transport works the same way. In code, `giong_mcp::Config::tcp("127.0.0.1:7777").token(secret).start(&handle)` does the same as the environment variables, and `Config::unix(path)` listens on a Unix socket.

## What an agent can ask

| Tool | Answers |
|---|---|
| `overview` | The process (resident memory, threads, open descriptors against the limit, uptime), the runtime (workers, shutdown, profiling), each worker's state, I/O driver, live tasks, busy share and polls per second over the last second, and warnings. |
| `workers` | Each worker in detail: counters since start (event-loop turns, polls, sleeps, idle time, spawned tasks), rates over the last second, timers, I/O operations in flight, receive buffers, draining, and the task a blocked worker is stuck in. |
| `tasks` | Live tasks grouped by the line that spawned them, across workers or for one (`worker`, `limit`). A count that keeps growing at one line is a leak. |
| `sources` | What registered sources report, summed over workers and per worker: giong-postgres, giong-mysql and giong-redis pools (open, taken, connecting, in flight, waiting, opened, connect errors, timeouts) and anything registered with `giong::observe::register`. |
| `profile` | Times every task poll for `duration_ms` (100 to 10,000, default 1,000), then lists the spawn locations the workers spent their time in: polls, total, share of the window, mean and longest poll. |

The answer that matters most is the one other tools cannot give: **which task blocks a worker.** giong runs many tasks on one thread, so a task that calls a blocking function or computes for a long time without `.await` stalls every other task on its worker. The server samples each worker ten times a second; a worker that is awake but has not moved on is reported with the spawn location of the task it is in:

```json
"warnings": [
  "worker 1 has been inside one poll of the task spawned at src/routes/report.rs:88:13 for 2.3 s: that task blocks the thread (a blocking call, a long computation or a loop without .await), and every other task on worker 1 waits"
]
```

The server runs on a thread and a single-worker giong runtime of its own, so it answers even when every worker of the service is stuck. A worker that cannot answer within `snapshot_timeout` (500 ms by default) is reported as such, with what its counters say.

## How it works

The runtime keeps the data; this crate only serves it. In `giong`:

- Every worker publishes counters and the spawn location of the task it is polling, on a cache line of its own, with relaxed stores: `Handle::pulse(worker)` reads them from any thread without disturbing the worker. That costs about nine instructions per task poll (two of them stores; counted with callgrind on tasks that only yield) and one store per turn of the event loop.
- `spawn`, `Handle::spawn_on` and `Handle::spawn_balanced` record where they were called (`#[track_caller]`), so a task is named by a line of your code, and `observe::snapshot()` groups the live tasks by it.
- `Handle::set_profiling(true)` makes every worker read the clock around each poll; when it is off, it costs one relaxed load per batch of tasks (the loop is compiled twice, with and without the clock).
- Pools register themselves as `observe::Source`s, by weak reference.

## Security

Whoever can reach the server can read the internals of the service: task locations, pool addresses (never user names or passwords), memory figures. So:

- It listens on `127.0.0.1`, `::1` or a Unix socket, and refuses any other address with [GIONG-E1301](../docs/errors/E1301.md). To reach a service elsewhere, forward the port (`ssh -L 7777:127.0.0.1:7777 host`, `kubectl port-forward`).
- A request whose `Host` header is not a loopback name, or whose `Origin` is not a loopback origin, is refused with `403`: a web page cannot reach the server through DNS rebinding.
- With a token (`GIONG_MCP_TOKEN`, `Config::token`), every request needs `Authorization: Bearer <token>`; without one, any local process that can connect can read.
- Every tool is read-only. Nothing can change the service through it, apart from `profile` turning poll timing on for the time it measures.

## Protocol

MCP over Streamable HTTP at `/mcp`, protocol versions 2025-11-25, 2025-06-18 and 2025-03-26. One JSON-RPC message (or a batch) per `POST`, answered with `application/json`; notifications get `202 Accepted`. The server offers no event stream (`GET` gets `405`) and keeps no sessions. Tool results carry the JSON both as `structuredContent` and as text.

## Testing

`cargo test -p giong-mcp`: 16 tests. Seven run a two-worker service with a server on a loopback port and a plain HTTP client: the handshake and tool list, every tool's answer checked against known tasks and sources, a worker blocked in `std::thread::sleep` reported with its line while the other worker still answers, a profile finding a busy task, JSON-RPC errors, and the refusals (token, `Host`, `Origin`, method, content type, protocol version, non-loopback addresses, a Unix socket that replaces a stale one). Eight unit tests cover address parsing, the `Host` check, version negotiation, the watcher, `/proc` and a `profile` call dropped half-way (profiling goes back off), and one the documentation example. `tests/observe.rs` in the runtime (8 tests) checks the counters, spawn locations, profiles and sources underneath, on both drivers.

## License

MIT or Apache-2.0, at your option.
