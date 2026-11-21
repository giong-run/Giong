# giong-mcp

An [MCP](https://modelcontextprotocol.io) server that lets an AI agent look inside a running [giong](../README.md) service: what each worker is doing, which tasks are alive and which line spawned them, what the connection pools hold, and where the time goes. Read-only, on a loopback address or a Unix socket, and only when the service turns it on.

```toml
[dependencies]
giong = "1.0.0"
giong-mcp = "1.0.0"
```

```rust,no_run
fn main() -> std::io::Result<()> {
    let rt = giong::Builder::new().shutdown_on_signals(true).build()?;
    // Starts only when GIONG_MCP is set. Keep the value: dropping it stops
    // the server.
    let _mcp = giong_mcp::start_from_env(&rt.handle());
    rt.run(|_worker| async {
        // ... the service ...
        giong::shutdown_signal().await;
    })
}
```

```sh
export GIONG_MCP=127.0.0.1:7777 GIONG_MCP_TOKEN=$(openssl rand -hex 16)
cargo run --release
# Claude Code, in another shell with the same GIONG_MCP_TOKEN:
claude mcp add --transport http giong http://127.0.0.1:7777/mcp \
    --header "Authorization: Bearer $GIONG_MCP_TOKEN"
```

Any MCP client that speaks the Streamable HTTP transport works the same way; give it the token as an `Authorization: Bearer` header (in MCP Inspector, the "Bearer Token" field). Without that header a client gets `401`, and some clients then try OAuth discovery and fail with a `404` that does not say why: the header is the fix.

`start_from_env` writes one line to stderr, `giong-mcp: listening on http://127.0.0.1:7777/mcp`, or the coded error that kept the server from starting: a busy port or a bad `GIONG_MCP` leaves the service running without the endpoint, since a debugging aid should not decide whether the service starts. For a JSON log line instead, or to stop the service when the endpoint cannot start, use `Config` and log the result yourself:

```rust,no_run
fn main() -> std::io::Result<()> {
    let rt = giong::Builder::new().shutdown_on_signals(true).build()?;
    // Kept for as long as the service runs, like `_mcp` above.
    let mcp = giong_mcp::Config::from_env()?
        .map(|cfg| cfg.start(&rt.handle()))
        .transpose()?; // a coded error, such as GIONG-E1303, stops the service
    if let Some(mcp) = &mcp {
        // log mcp.url() (None on a Unix socket) your way
        let _ = mcp.url();
    }
    rt.run(|_worker| async {
        // ... the service ...
        giong::shutdown_signal().await;
    })
}
```

In code, `giong_mcp::Config::tcp("127.0.0.1:7777").token(secret).start(&handle)` does the same as the environment variables, and `Config::unix(path)` listens on a Unix socket.

## What an agent can ask

| Tool | Answers |
|---|---|
| `overview` | The process (resident memory, threads, open descriptors against the limit, uptime), the runtime (workers, shutdown, profiling), each worker's state, I/O driver, live tasks, busy share and polls per second over the last second, and warnings. |
| `workers` | Each worker in detail: counters since start (event-loop turns, polls, sleeps, idle time, spawned tasks), rates over the last second, timers, I/O operations in flight, receive buffers, draining, and the task a blocked worker is stuck in. |
| `tasks` | Live tasks grouped by the line that spawned them, across workers or for one (`worker`, `limit`). A count that keeps growing at one line is a leak. |
| `sources` | What registered sources report, summed over workers and per worker: giong-postgres, giong-mysql and giong-redis pools (open, taken, connecting, in flight, waiting, opened, connect errors, timeouts) and anything registered with `giong::observe::register`. |
| `profile` | Times every task poll for `duration_ms` (100 to 10,000, default 1,000), then lists the spawn locations the workers spent their time in: polls, total, share of the window, mean and longest poll. |
| `slow_requests` | Why requests are slow. Traces every request for `duration_ms` (100 to 30,000, default 5,000), then answers per route (`route` narrows it, `limit` caps it): requests, p50, p90, p99 and max, where the time of the requests past `quantile` (default 0.99) went compared with the faster half, a verdict with its evidence, confidence and suggested fix, and the slowest requests in full. |

The answer that matters most is the one other tools cannot give: **which task blocks a worker.** giong runs many tasks on one thread, so a task that calls a blocking function or computes for a long time without `.await` stalls every other task on its worker. The server samples each worker ten times a second; a worker that is awake but has not moved on is reported with the spawn location of the task it is in:

```json
"warnings": [
  "worker 1 has been inside one poll of the task spawned at src/routes/report.rs:88:13 for 2.3 s: that task blocks the thread (a blocking call, a long computation or a loop without .await), and every other task on worker 1 waits"
]
```

In a giong-http or giong-web service, request handlers run inside the task giong-http spawns for each connection, so a blocking handler is reported as a giong-http connection's task (a location in giong-http's `src/server.rs`), and the warning says so: giong cannot yet name the handler or route. `profile` counts the time of every handler the same way, under that location.

The server runs on a thread and a single-worker giong runtime of its own, so it answers even when every worker of the service is stuck. A worker that cannot answer within `snapshot_timeout` (500 ms by default) is reported as such, with what its counters say.

### Why is this route slow?

`slow_requests` answers "why is the p99 of `/checkout` 400 ms" from measurements, not guesses. While it runs, giong follows every request through its phases: receiving it, the handler's own code on the worker, waiting for a pool connection, database and cache commands, calls to other services, the rest of the handler's time, and writing the answer. It also notes every task poll of 1 ms or more, so that a request that waited while another task held its worker names that task, and the route of the request whose handler took that poll. Routes are named by giong-web (`POST /checkout`); a service on giong-http alone names them with `giong::observe::set_route`.

For each route it compares the slow requests with the faster half and blames the phase that accounts for most of the difference:

```json
"verdict": {
  "cause": "pool_wait",
  "confidence": "high",
  "share_of_excess": 0.993,
  "evidence": [
    "the requests from p99 up (115 requests, from about 26.6 ms) took 32.9 ms on average; the faster half took 1.9 ms",
    "Waiting for pool connections (waiting for a connection from a pool): 30.8 ms on average, 94% of their time; 0 µs in the faster half, so it accounts for 99% of the difference",
    "the longest of those waits were for postgres"
  ],
  "suggestion": "Requests queue for a postgres pool connection: every connection is in use when they ask. The database itself is not what is slow here. ..."
}
```

The causes are `pool_wait`, `slow_queries`, `slow_upstream`, `handler_cpu`, `worker_blocked` (another route's handler or a task held the worker: it is named), `worker_saturated`, `slow_request_arrival`, `slow_response_write`, `unattributed_wait` (the handler waited on something no code timed: wrap it in `giong::observe::wait`) and `not_slow`. Confidence is high with at least 20 slow requests and a phase that accounts for 60% of the difference, medium with 5 and 40%, low otherwise. The verdict is a proposal for an agent or a person to check against the numbers next to it; giong-mcp changes nothing in the service.

What it does not see yet: waits inside tasks a handler spawns (they are not the request's), and on HTTP/2 the time to receive a request and to write its answer.

## How it works

The runtime keeps the data; this crate only serves it. In `giong`:

- Every worker publishes counters and the spawn location of the task it is polling, on a cache line of its own, with relaxed stores: `Handle::pulse(worker)` reads them from any thread without disturbing the worker. That costs about nine instructions per task poll (two of them stores; counted with callgrind on tasks that only yield) and one store per turn of the event loop.
- `spawn`, `Handle::spawn_on` and `Handle::spawn_balanced` record where they were called (`#[track_caller]`), so a task is named by a line of your code, and `observe::snapshot()` groups the live tasks by it.
- `Handle::set_profiling(true)` makes every worker read the clock around each poll; when it is off, it costs one relaxed load per batch of tasks (the loop is compiled twice, with and without the clock).
- Pools register themselves as `observe::Source`s, by weak reference.
- `Handle::set_request_tracing(true)` makes giong-http follow each request with an `observe::RequestTrace`, and the pools and clients time their waits with `observe::wait`. Each worker adds every finished request to fixed-size latency buckets per route, with the time of each phase summed per bucket, and keeps the slowest eight: nothing is sampled away and nothing grows with load. Off, it costs one read of a thread-local per request and per database command. On, it costs 250 to 350 ns per request on one worker (several clock reads, an allocation and the per-route record); a one-worker giong-web server kept busy by `wrk` answered 5.5% fewer requests per second on `/json` and 7% fewer on `/users/{id}` while it was on (`bench/request-tracing.sh`, raw data in `bench/results/request-tracing/`). It is on only while `slow_requests` runs. With it off, what it costs is a read of a thread-local per request and a few branches: the traced paths are out of line, and the requests a connection follows are kept in a table of the worker's, not in the connection, so idle HTTP/1.1 and HTTP/2 connections are as large as without tracing. `bench/instr` counts the HTTP cases within 0.5% of their counts before (HTTP/1.1 0.4% lower, HTTPS 0.5% higher), a difference that moves with the compiler's code layout.

## Security

Whoever can reach the server can read the internals of the service: task locations, pool addresses (never user names or passwords), memory figures. So:

- It listens on `127.0.0.1`, `::1` or a Unix socket, and refuses any other address with [GIONG-E1301](../docs/errors/E1301.md). To reach a service elsewhere, forward the port (`ssh -L 7777:127.0.0.1:7777 host`, `kubectl port-forward`).
- On TCP, every request needs `Authorization: Bearer <token>` (`GIONG_MCP_TOKEN`, `Config::token`): every process on the machine can connect to a loopback port, whichever user runs it, and so can every container in the same pod. Without a token the server does not start ([GIONG-E1304](../docs/errors/E1304.md)), unless the code opts out with `Config::allow_unauthenticated()`, for a development machine. A token must be at least 16 characters a bearer token can carry; an empty one is refused rather than taken to mean "no token" ([GIONG-E1305](../docs/errors/E1305.md)).
- A Unix socket is created with mode `0600`, under a temporary name first, so that it is never reachable by other users, and needs no token. A live socket at the path is never replaced, and stopping removes only the socket this server created. Most MCP clients speak HTTP over TCP only; to use one with a socket, bridge it: `socat TCP-LISTEN:7777,bind=127.0.0.1,fork UNIX-CONNECT:/run/giong/mcp.sock` (which lets every local process in again), or `ssh -L 7777:/run/giong/mcp.sock host` from another machine.
- A request whose `Host` header is not a loopback name, or whose `Origin` is not a loopback origin, is refused with `403`: a web page cannot reach the server through DNS rebinding.
- What a client can make the server hold is bounded, since it lives in the service's process. Requests are served over HTTP/1.1 only (an HTTP/2 connection could carry a hundred at once), on at most 16 connections, each answered one request at a time; more wait in the kernel's listen queue. A body holds at most 64 KiB (`413` past that), a batch at most 16 messages, and an answer at most 4 MiB (an error past that, which tells the agent to ask for less). Connections idle for 60 s are closed, and a request must arrive within 10 s. Before these limits, 24 connections each sending one 254 KiB batch took a 4-worker service from 4 MB to 2 GB; now 64 such connections leave it under 8 MB.
- The tools only read, apart from `profile` and `slow_requests`, which make every worker time its polls (and, for `slow_requests`, follow its requests) for the window they measure (their `readOnlyHint` is false, so clients can ask before calling them). `slow_requests` is refused while the service has request tracing on itself. `profile` shares the runtime's profiling switch with the service: it is refused while the service has profiling on, and it discards poll timings the service collected earlier and has not taken yet.

## Protocol

MCP over Streamable HTTP at `/mcp`, protocol versions 2025-11-25, 2025-06-18 and 2025-03-26. One JSON-RPC message per `POST`, answered with `application/json`; notifications and the client's responses get `202 Accepted`. Batches (at most 16 messages) are answered only for clients of 2025-03-26, the one version that has them; a request with a later `MCP-Protocol-Version` gets `-32600`. The server offers no event stream (`GET` gets `405`) and keeps no sessions.

Tool results carry the JSON both as `structuredContent` and as compact text, and each tool declares the shape of its answer as an `outputSchema`, so clients can validate answers and models know the fields before the first call. That JSON is part of the crate's API: from 1.0 on, fields are added in minor releases and never renamed, retyped or removed before 2.0. Fields that are present only in some answers (a worker that did not answer, a call for one worker) are marked so in the schema.

## Testing

`cargo test -p giong-mcp`: 37 tests. Sixteen run a two-worker service with a server on a loopback port and a plain HTTP client: the handshake and tool list, every tool's answer checked against known tasks and sources and against its `outputSchema`, a service with nothing to do reported as idle, a worker blocked in `std::thread::sleep` reported with its line while the other worker still answers, a panicking source told apart from a stopped worker, a profile finding a busy task, `slow_requests` blaming a pool that makes requests wait (high confidence) and naming the route whose handler holds the worker, JSON-RPC errors and malformed messages, the limits (the review's 254 KiB batch, batches past 16 or from newer clients, 24 clients at once with the process's memory checked), the token rules, busy addresses, and the refusals (token, `Host`, `Origin`, method, content type, protocol version, non-loopback addresses, a Unix socket's mode, a stale socket replaced and a live one or a successor's left alone). `tests/env.rs` checks that `start_from_env` keeps the service running whatever goes wrong. Seventeen unit tests cover address parsing, tokens, a listener the kernel put somewhere else than asked, the `Host` check and the `Bearer` scheme, version negotiation, the watcher (including a second-long window under quick calls), `/proc`, a `profile` call dropped half-way (profiling goes back off), giong-http's connection tasks told apart from others, quantile labels and the fuzz target's starting inputs; three doc tests check the example and that dropping the server at once is a warning. `tests/observe.rs` in the runtime (13 tests) checks the counters, spawn locations, profiles, sources and request phases underneath, on both drivers.

`fuzz/` has a cargo-fuzz target, `request`, that sends any bytes as one request through every check and the JSON-RPC dispatch, and checks that the answer is one the server means to send and that no request without the token gets through (`cd giong-mcp && cargo +nightly fuzz run request`).

## License

MIT or Apache-2.0, at your option.
