"""The words and numbers on giong.run that a person writes.

Everything the repository already says is read from it by build.py instead: the
version and the crates (Cargo.toml), lines of code (the sources), the roadmap
(ROADMAP.md), the release notes (CHANGELOG.md) and the error codes (docs/errors/).
What is left here is what only a person can write: the pitch, the explanations next to
each benchmark, the code samples and the answers to common questions.

Rules for this file:
- Every number comes from the README or bench/results/, with its caveat next to it.
  Update the README first, then this file.
- Do not present giong as the fastest at 50 connections: it ties with tuned Monoio and
  raw Tokio there. Two-core numbers are not a ranking.
- Avoid version numbers in prose; build.py fills in the current one where it matters.
- English only. Inline `code` and **bold** work in most strings.
"""

# ---------------------------------------------------------------- crates
# One entry per crate. A crate in the workspace without an entry still appears, with
# its Cargo.toml description; add an entry to give it bullets and a headline fact.
# `layer` places it in the stack diagram, from the bottom: runtime, protocols, clients,
# framework, tooling.
# `tests`: the number of tests `cargo test -p <crate> --all-features` runs (passing,
# not ignored), counted by hand when it changes; the site adds them up.
CRATES = {
    "giong": dict(
        layer="runtime", tests=226,
        short="Executor, io_uring driver, TCP, UDP, Unix sockets, files, timers, DNS, channels.",
        lede="The runtime: TCP, UDP, Unix sockets, files, timers, DNS, channels and graceful shutdown.",
        items=["`Builder::new().workers(n)`, one worker per core",
               "`TcpListener`, `TcpStream`, `UdpSocket`, `UnixListener`, `UnixStream`",
               "`fs::File` with `read_at` and `write_at`, `fs::read`, `read_dir`, `rename`",
               "`net::lookup_host` without blocking; `sync::mpsc`, `bounded`, `oneshot`",
               "`spawn_on`, `spawn_balanced` and `bind_balanced` to spread new work"],
        fact="3 dependencies: `io-uring`, `libc`, `socket2`"),
    "giong-http": dict(
        layer="protocols", tests=131,
        short="A strict HTTP/1.1 server, HTTPS behind a feature, and a pooled client.",
        lede="A small, strict HTTP/1.1 server with one async handler, and a client.",
        items=["Keep-alive, pipelining, bodies by `Content-Length` or chunked",
               "Limits on head and body size; timeouts for idle, slow and slow-reading clients",
               "Answers `400` to any request a proxy in front could read differently",
               "`Server::serve`, `serve_unix`, `serve_tls`; a pooled HTTPS `Client` with proxies "
               "and gzip behind `client`"],
        fact="the parser fuzzed every night"),
    "giong-tls": dict(
        layer="protocols", tests=34,
        short="TLS 1.2 and 1.3 for servers and clients on rustls. No OpenSSL.",
        lede="TLS 1.2 and 1.3 for servers and clients on rustls with `ring`. No OpenSSL.",
        items=["`TlsAcceptor::from_pem(cert, key)`; `TlsConnector` with the system's or built-in roots",
               "`recv()` is cancel safe",
               "A cancelled write poisons the sending side instead of corrupting the record stream",
               "ALPN, client certificates and a handshake timeout"],
        fact="TLS costs 4% on `/json`"),
    "giong-postgres": dict(
        layer="clients", tests=79,
        short="PostgreSQL client and pool; statements of every task pipeline together.",
        lede="A PostgreSQL client and pool. The statements of every task pipeline over a few connections.",
        items=["`Client::connect`, `query`, `execute`, `transaction`, `cancel_token`",
               "A `Pool` per worker: shared, pipelined statements; `get()` for a transaction",
               "Every statement prepared once per connection and cached",
               "TLS verified by default for remote hosts; SCRAM-SHA-256"],
        fact="2.6× deadpool's statements per core"),
    "giong-mysql": dict(
        layer="clients", tests=88,
        short="MySQL 8 client and pool, built the same way as giong-postgres.",
        lede="A MySQL 8 client and pool, built the same way as giong-postgres.",
        items=["`Client::connect`, `query`, `execute`, `transaction`",
               "A `Pool` per worker: shared, pipelined statements; `get()` for a transaction",
               "Prepared statements on the binary protocol, cached per connection",
               "TLS; `caching_sha2_password` and `mysql_native_password`"],
        fact="4.7× SQLx's statements per core"),
    "giong-redis": dict(
        layer="clients", tests=71,
        short="Redis client on RESP3: pipelining across tasks, Pub/Sub, a pool.",
        lede="A Redis client and pool on RESP3. The commands of every task pipeline over one connection.",
        items=["Typed commands, `query` for any other, `Pipeline` for a batch",
               "Transactions with `watch`; Pub/Sub",
               "A `Pool` per worker; `take()` for a connection of your own",
               "TLS; Redis 6 or newer (RESP3)"],
        fact="1.4× redis-rs's `GET`s per core"),
    "giong-web": dict(
        layer="framework", tests=77,
        short="Router, middleware and JSON on giong-http; errors as coded problem+json.",
        lede="A router, middleware and JSON on giong-http. Handlers are plain `async fn`s; every "
             "error is a coded problem+json answer.",
        items=["`Router::new().get(\"/users/{id}\", user).nest(\"/api\", api).build(state)`",
               "`req.param::<u64>(\"id\")?`, `req.query::<T>()?`, `req.json::<T>()?`",
               "`RequestId`, `Logger` (JSON lines), `Cors`, `Compression` (gzip), `Timeout`",
               "404, 405 with `Allow`, `OPTIONS` and `HEAD` answered for you; `App::handle` tests "
               "without a socket"],
        fact="1.3× axum's `/json` on one core"),
    "giong-mcp": dict(
        layer="tooling", tests=16,
        short="A read-only MCP server: an agent sees what each worker, task and pool is doing.",
        lede="A read-only MCP server that lets an AI agent look inside a running service: which "
             "task blocks a worker, which line spawned the tasks that leak, how full the pools are.",
        items=["`giong_mcp::start_from_env(&handle)`, on when `GIONG_MCP` is set",
               "Tools: `overview`, `workers`, `tasks`, `sources`, `profile`",
               "A worker stuck in one poll is reported with the line that spawned the task",
               "Loopback or a Unix socket only; `Host` and `Origin` checked; optional token"],
        fact="about 9 instructions per task poll"),
    "giong-compat": dict(
        layer="protocols", tests=29,
        short="Tokio's and futures-io's I/O traits for giong's streams, for h2, tokio-rustls and the like.",
        lede="`AsyncRead`, `AsyncWrite` and `AsyncBufRead` from Tokio and futures-io for giong's "
             "streams, so protocol crates that take a stream run on giong with no Tokio runtime.",
        items=["`Compat::new(stream)` for `TcpStream`, `UnixStream` and `TlsStream`",
               "Tested with tokio-rustls, futures-rustls, tokio-tungstenite, h2, hyper's `conn` and "
               "tokio-util's codecs",
               "Buffered writes like `BufWriter`; reads lose nothing when cancelled",
               "Says plainly which crates cannot work: those that need Tokio's reactor, timers or `spawn`"],
        fact="one copy per byte; sending costs 14% of throughput"),
}

# Crates on the roadmap that do not exist yet. Each disappears from this list on its
# own once a crate of that name is in the workspace.
COMING = {
}

LAYERS = [  # top to bottom, as drawn
    ("tooling", "Tooling"),
    ("framework", "Framework"),
    ("clients", "Clients"),
    ("protocols", "Protocols"),
    ("runtime", "Runtime"),
]

# ---------------------------------------------------------------- home
HERO = dict(
    h1="One syscall per tick, <em>not per operation.</em>",
    lede=("giong is an async runtime for Rust services on Linux, built on io_uring with one "
          "thread per core, and an epoll driver wherever io_uring is blocked. It brings its own HTTP server and client, TLS, PostgreSQL, MySQL "
          "and Redis clients and a web framework, so a whole service runs on it with no Tokio "
          "underneath."),
)

PILLARS = [
    ("io_uring_enter · DEFER_TASKRUN", "One syscall per tick",
     "Every read, write, accept, close and cancellation a turn of the loop produces enters the "
     "kernel together, and every ready completion comes back in the same call. A request costs "
     "about **0.03 system calls**; an epoll-based server needs 2."),
    ("multishot recv · provided buffers", "Idle connections hold no buffer",
     "One submission lasts the life of a connection. The kernel picks a buffer only when data "
     "arrives, so it is still warm in cache. At **5,000 connections** giong spends the least CPU "
     "per request of the runtimes measured."),
    ("tests/cancel.rs", "Cancel safe by construction",
     "Drop an I/O future mid-flight and its buffer moves to the driver until the kernel confirms "
     "the cancel. A `timeout` around `recv`, `recv_from` or `accept` loses no data."),
    ("!Send · Rc · RefCell", "Shared nothing",
     "One thread per core, and a task never changes thread. Futures need not be `Send`; `Rc` and "
     "`RefCell` just work. No locks on the hot path."),
    ("giong::fs", "Files on the same ring",
     "Open, read, write, fsync, statx and rename are entries in the worker's ring, next to the "
     "network. No pool of blocking threads."),
    ("Builder::spin_before_park", "A latency dial",
     "Let a worker watch its ring before it sleeps: **19 µs median instead of 28** on one "
     "connection. It costs CPU, so it is off by default."),
]

AGENTS = dict(
    h2="Made for the agents that write your code",
    lede=("More code is written by AI coding agents every month. giong is built so they get it "
          "right on the first try, and can fix it alone when they do not."),
    points=[
        ("Errors that say how to fix them",
         "Every error giong raises has a stable code, the cause and the fix, and a page of its "
         "own. `giong::error::code(&err)` matches on the code, never on the message."),
        ("One obvious way",
         "No macros that hide control flow. Strict types, so a mistake is a compile error. Every "
         "timeout and size limit is finite and documented."),
        ("Verification is the law",
         "Tests, ThreadSanitizer, loom, fuzzing and soak tests run in CI and block merging. "
         "Machines check the work, not only reviewers."),
        ("Docs a machine can read",
         "`AGENTS.md` in the repository and `llms.txt` on this site. `giong-mcp` lets an "
         "agent look inside a running service."),
    ],
    error=("GIONG-E0102: no address found for db.internal. Fix: check the spelling of the name, "
           "/etc/hosts, and the DNS servers in /etc/resolv.conf (in a container, the network it is "
           "attached to). See https://github.com/giong-run/Giong/blob/main/docs/errors/E0102.md"),
)

CHECKED = [
    "Tests repeated in release mode on all three io_uring setups and on epoll",
    "Real Linux 5.15, 6.0, 6.1 and Ubuntu 24.04 kernels in QEMU, and Docker's default profile, on every change",
    "ThreadSanitizer: no data race",
    "`loom` model check of the cross-thread wake-up",
    "Parsers fuzzed every night; a two-hour soak test",
    "valgrind memcheck: no invalid access, no leak",
]
NOT_YET = [
    "giong has **not run in production**",
    "No bare-metal scaling numbers: the multi-core run was under WSL2, over loopback",
    "`giong-tls` has had no independent security audit, only a review by the project's AI agent",
]

# ---------------------------------------------------------------- code
CODE = [
    dict(
        tab="REST API", crate="giong-web", file="src/main.rs",
        foot="Each worker builds its own app. A bad `id` is a `400` that names the parameter, "
             "as problem+json with a `GIONG-E` code.",
        src='''use giong::net::TcpListener;
use giong_web::middleware::{Logger, RequestId};
use giong_web::{json, Error, Req, Response, Router, Server, Sink};
use serde::Serialize;

#[derive(Serialize)]
struct User { id: u64, name: String }

async fn get_user(req: Req<()>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;   // "abc" is a 400 that names `id`
    if id == 0 {
        return Err(Error::not_found("user_not_found", format!("no user {id}")));
    }
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
}'''),
    dict(
        tab="HTTP server", crate="giong-http", file="src/main.rs",
        foot="One `SO_REUSEPORT` listener per worker; requests in progress are answered before a "
             "worker stops.",
        src='''use std::time::Duration;
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
        .shutdown_on_signals(true)                  // Ctrl-C / SIGTERM stop it gracefully
        .shutdown_grace(Duration::from_secs(10))    // requests in progress are answered
        .build()?
        .run(|_worker| async {
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}'''),
    dict(
        tab="PostgreSQL", crate="giong-postgres", file="src/main.rs",
        foot="Statements are prepared once per connection and cached. A `Pool` per worker "
             "pipelines the statements of every task.",
        src='''use giong_postgres::{Client, Config};

fn main() -> Result<(), giong_postgres::Error> {
    let config = Config::from_url("postgres://app:secret@localhost/shop")?;
    giong::block_on(async {
        let client = Client::connect(&config).await?;
        let rows = client
            .query("SELECT id, name FROM products WHERE price < $1", &[&100i32])
            .await?;
        for row in &rows {
            let id: i64 = row.get("id")?;
            let name: &str = row.get("name")?;
            println!("{id} {name}");
        }
        Ok(())
    })
}'''),
    dict(
        tab="Bare runtime", crate="giong", file="src/main.rs",
        foot="`recv()` lends a buffer from the worker's pool; an empty buffer means the peer "
             "closed. Buffers go to the kernel by value and come back with the result.",
        src='''use giong::net::TcpListener;

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .build()?
        .run(|_worker| async {
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            while let Some(Ok(stream)) = giong::until_shutdown(listener.accept()).await {
                giong::spawn(async move {
                    loop {
                        let Ok(data) = stream.recv().await else { return };
                        if data.is_empty() { return }       // peer closed
                        let reply = data.to_vec();
                        drop(data);                          // buffer back to the pool
                        if stream.write_all(reply).await.0.is_err() { return }
                    }
                });
            }
        })
}'''),
]

# ---------------------------------------------------------------- benchmarks
# Server names as they appear in the README tables.
NAMES = {
    "giong": "giong", "spin": "giong + spin", "http": "giong-http",
    "monoio": "Monoio (tuned)", "monoio0": "Monoio (default)", "tokio": "Raw Tokio",
    "epoll": "Hand-written epoll", "glommio": "Glommio", "axum": "Axum", "bun": "Bun",
    "node": "Node.js", "std": "std (blocking)", "tokio1": "Tokio, one core",
    "tokio4": "Tokio, 4 cores", "axumtls": "Axum + rustls", "httptls": "giong-http + giong-tls",
}
G = {"giong", "httptls"}          # ember
ALT = {"spin", "http"}            # giong's other configurations

# rows: (server, value, label[, lo, hi]); a str instead of a tuple starts a group.
# `home`: shown on the home page, with `short` as its note there.
CHARTS = [
    dict(
        id="syscalls", home=True,
        title="System calls per request",
        sub="`/json`, 50 connections · lower is better",
        max=2.2,
        rows=[("giong", 0.03, "0.03"), ("http", 0.03, "0.03"), ("monoio", 0.04, "0.04"),
              ("epoll", 2.03, "2.03"), ("tokio", 2.05, "2.05"), ("glommio", 2.10, "2.10")],
        short="The structural difference. On 8 KB responses it is worth almost half the CPU.",
        note=("The structural difference, and the most robust result here. On the small response it "
              "buys little, because a request costs about 4 µs of kernel time whichever way it is "
              "delivered. On 8 KB responses, where a whole batch leaves in one call, it is worth "
              "almost half the CPU: 16.0 µs per request against 27.9 for raw Tokio."),
        src=["bench/syscalls.sh"],
    ),
    dict(
        id="conns", home=True,
        title="5,000 connections: CPU per request",
        sub="`/json`, µs of user plus kernel time · lower is better",
        max=9,
        rows=[("giong", 4.42, "4.42 µs"), ("epoll", 4.99, "4.99 µs"), ("spin", 5.12, "5.12 µs"),
              ("monoio", 5.25, "5.25 µs"), ("http", 5.37, "5.37 µs"), ("tokio", 5.74, "5.74 µs"),
              ("glommio", 8.29, "8.29 µs")],
        short="11% below hand-written epoll, with the lowest p99. At 50 and 1,000 connections the "
              "leaders are level.",
        note=("giong is 11% below hand-written epoll and 16% below tuned Monoio, with the lowest "
              "p99 (26 ms; epoll 32, Monoio 44, raw Tokio 54). The likely reason is the provided "
              "buffer ring: receive buffers are reused while still warm in cache. At 50 and 1,000 "
              "connections giong, tuned Monoio and epoll are level."),
        src=["bench/scale.sh"],
    ),
    dict(
        id="tls", home=True,
        title="HTTPS on one core",
        sub="requests per second, TLS 1.3, the same rustls on both sides · higher is better",
        max=160,
        rows=["`/json`, kept-alive",
              ("httptls", 144.8, "144.8k", 142.3, 152.9), ("axumtls", 81.0, "81.0k", 80.2, 82.8),
              "`/users`, 8 KB, kept-alive",
              ("httptls", 47.8, "47.8k", 43.4, 48.8), ("axumtls", 19.2, "19.2k", 18.7, 19.8),
              "a new handshake per request",
              ("httptls", 7.1, "7.1k", 6.7, 7.5), ("axumtls", 7.3, "7.3k", 6.9, 7.6)],
        short="Handshakes are level: they are almost all cryptography, from the same library.",
        note=("TLS costs giong 4% on `/json` and 14% on 8 KB responses. Handshakes are level: a "
              "handshake is almost all cryptography, and both sides use the same library for it. "
              "Most of the `/users` factor is there without TLS."),
        src=["bench/tls.sh"],
    ),
    dict(
        id="latency",
        title="One connection: median latency",
        sub="`/json`, microseconds · lower is better",
        max=36,
        rows=[("spin", 19, "19 µs"), ("giong", 28, "28 µs"), ("monoio", 28, "28 µs"),
              ("tokio", 29, "29 µs"), ("epoll", 29, "29 µs"), ("http", 30, "30 µs"),
              ("glommio", 33, "33 µs")],
        note=("With one connection every request finds a sleeping server. `spin_before_park(200 µs)` "
              "keeps the worker watching its ring: 47.9k requests/s instead of 32.4k. It costs CPU, "
              "20.5 instead of 13.6 µs per request, and the worker keeps its core busy between "
              "requests. Without it giong is the same as the others."),
        src=["bench/scale.sh"],
    ),
    dict(
        id="files",
        title="File reads on one core",
        sub="4 KiB at a random offset, one task, operations per second · higher is better",
        max=1500,
        rows=[("std", 1395, "1.40M", 1342, 1464), ("giong", 1014, "1.01M", 868, 1153),
              ("tokio1", 184, "184k", 171, 188), ("tokio4", 33, "33k", 32, 34)],
        note=("5.5 times Tokio, where every read is a hand-off to another thread and back; on giong "
              "it is one entry in a batch. A blocking `std` call is 28% faster for cached data. What "
              "giong buys is a worker that never waits when the data is not cached, which this "
              "benchmark does not measure."),
        src=["bench/fs.sh"],
    ),
    dict(
        id="tie",
        title="Small responses at 50 connections: a tie",
        sub="`/json`, requests per second",
        max=220,
        rows=[("http", 195.7, "195.7k", 193.3, 212.6), ("monoio", 176.0, "176.0k", 167.2, 181.5),
              ("tokio", 174.1, "174.1k", 168.5, 178.7), ("giong", 173.5, "173.5k", 165.0, 180.4),
              ("epoll", 170.9, "170.9k", 159.0, 174.6), ("axum", 161.0, "161.0k", 153.2, 167.9),
              ("bun", 147.7, "147.7k", 136.7, 153.1), ("glommio", 143.9, "143.9k", 133.4, 149.0),
              ("node", 39.5, "39.5k", 37.6, 40.8)],
        note=("giong, tuned Monoio, raw Tokio and epoll are within noise: the kernel's TCP stack, "
              "not the runtime, is most of the cost, and `wrk` is the limit. `giong-http` reads "
              "higher and we do not know why. It costs 7% more CPU per request, as checking every "
              "header should, and keeps its core busier. Treat that row as a measurement of the setup."),
        src=["bench/run.sh"],
    ),
]

BENCH_METHOD = ("One core per server, loopback, medians of five interleaved rounds on a 4-vCPU "
                "virtual machine; the thin line under a bar runs from the lowest round to the "
                "highest. giong is not faster than everything at everything: the charts say where "
                "the design pays off and where it does not.")

CAVEATS = [
    "**One machine.** Everything is loopback inside a 4-vCPU virtual machine, where the load "
    "generator competes for the same few cores. The 14-core run in the README (1 to 10 workers, "
    "peak 1.50M requests/s) is loopback too, on a desktop under WSL2.",
    "**Not measured:** a physical network, scaling on bare metal, and disks. The file benchmark "
    "reads cached data only.",
    "**8 KB responses:** giong and Monoio serve 63–64k requests/s, everything else 35–37k. Most of "
    "that gap comes from the virtual machine. On physical hardware expect a smaller one.",
    "**`/compute` is the same for everyone**, as it should be: the time goes into the function, "
    "not the runtime.",
    "**Two cores are measured and are not a ranking.** Two `wrk` threads are as likely to be the "
    "limit as the server.",
    "**Monoio is compared tuned**, with `DEFER_TASKRUN` passed in. Its default ring is about 20% "
    "slower on the small response; comparing against that would flatter giong.",
]

# From "Compared with Monoio and Glommio" in the README. Versions as measured there.
COMPARE_HEAD = ["", "giong", "Monoio 0.2.4", "Glommio 0.9.0"]
COMPARE = [
    ("Platforms", "Linux: io_uring on ≥ 6.0, an epoll driver elsewhere (tested on 5.15)", "Linux ≥ 5.6, with an epoll/kqueue fallback (runs on macOS)",
     "Linux ≥ 5.8, needs a raised memlock limit"),
    ("Receiving", "multishot, kernel-provided buffers", "one submission per read, caller's buffer",
     "direct `recv`, io_uring to wait"),
    ("Accepting", "multishot", "one submission per accept", "one submission per accept"),
    ("Buffer API", "ownership passing", "ownership passing", "borrowed (`AsyncRead` / `AsyncWrite`)"),
    ("Timeout around a read", "loses no data", "needs the separate `cancelable_*` API", "—"),
    ("Ring defaults", "`DEFER_TASKRUN` on", "no flags; pass your own builder", "—"),
    ("Busy-poll option", "yes", "no", "yes"),
    ("Multi-core", "built-in builder with graceful draining and signals", "`#[monoio::main(threads = N)]`",
     "executor pools placed by CPU topology"),
    ("Scheduling", "FIFO; new work placed on the least busy worker on request", "FIFO",
     "weighted queues with latency classes"),
    ("I/O", "TCP, UDP, Unix sockets, files", "TCP, UDP, Unix sockets, files",
     "TCP, UDP, Unix sockets, Direct I/O files"),
    ("Ecosystem", "its own HTTP/1.1 server and client, web framework, TLS for both sides, DNS, "
                  "PostgreSQL, MySQL and Redis clients; Tokio's I/O traits for crates such as h2",
     "TLS, HTTP, codecs, Tokio compatibility layer", "channels, semaphores, rate control"),
]

# ---------------------------------------------------------------- docs
REQS = [
    ("kernel", "**Linux.** For io_uring, 6.0 or newer, 6.1+ recommended; developed and tested on "
               "6.18. Elsewhere giong runs on its epoll driver, tested down to 5.15."),
    ("rust", "1.80 or newer for the libraries."),
    ("elsewhere", "giong does not build on Windows or macOS. Use WSL2 with a 6.x kernel or a Linux "
                  "virtual machine."),
    ("docker", "Docker's default seccomp profile blocks io_uring, so giong runs on epoll there. "
               "[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) shows how to allow io_uring in Docker, "
               "Compose and Kubernetes without turning seccomp off."),
    ("license", "MIT OR Apache-2.0, at your option."),
]

RUN_SH = '''cargo run --release          # listens on port 8080
curl localhost:8080/health   # ok'''

FROM_SOURCE_SH = '''git clone https://github.com/giong-run/Giong.git && cd Giong
cargo test --workspace --all-features   # the whole suite, every crate'''

# The programs in examples/, with the first line of their doc comment.
EXAMPLES = [
    ("echo", "A TCP echo server on every core.",
     "cargo run --release --example echo", "nc 127.0.0.1 7000"),
    ("http", "A small HTTP/1.1 server: keep-alive, pipelining, idle timeouts, a header size limit "
             "and graceful shutdown on SIGINT and SIGTERM. The same routes the benchmarks use.",
     "cargo run --release --example http", "curl localhost:3006/json"),
    ("udp_echo", "A UDP echo server, receiving with multishot `recvmsg`.",
     "cargo run --release --example udp_echo", "echo hello | nc -u -w1 127.0.0.1 7001"),
    ("cat", "Prints files to standard output, reading them through io_uring with positional reads.",
     "cargo run --release --example cat -- Cargo.toml", ""),
]

LIMITS = [
    ("Linux only", "io_uring on 6.0 or newer, epoll elsewhere. On epoll giong is as fast as other "
                   "epoll runtimes, not faster. macOS and Windows are planned for after 1.0."),
    ("Not Tokio's ecosystem", "Crates that need the Tokio runtime (hyper, sqlx, reqwest) do not run "
                              "on giong. Crates that only use `std::future` do."),
    ("No HTTP/2", "`giong-http` speaks HTTP/1.1. A request body over the limit is refused."),
    ("No kTLS", "TLS runs in user space on rustls, and certificates are not reloaded for you."),
    ("Running tasks stay put", "New work can go to the least busy worker (`spawn_balanced`, "
                               "`bind_balanced`), but a task never moves once it has started."),
    ("Some gaps", "Names come from `/etc/hosts` and DNS, not mDNS or LDAP. Also missing: Unix "
                  "datagram sockets, multicast, file locks."),
]

ELSEWHERE = [
    "**Monoio** is the mature choice and runs in production at ByteDance. If you need HTTP/2 or "
    "Tokio-compatible crates today, use it.",
    "**Glommio** is built for storage engines: Direct I/O and priority scheduling are its strengths.",
    "**Tokio** is the general-purpose runtime. giong is not trying to replace it for every workload.",
]

FAQ = [
    ("Is giong ready for production?",
     "Not yet. It is complete enough to write real services and is tested hard, but it has not run "
     "in production. 1.0 waits for, among other things, at least one production user reporting back."),
    ("Does it run on macOS or Windows?",
     "Not yet: both are planned for after 1.0, macOS for development and Windows for services "
     "that have to run there. Today, on Windows use WSL2 with a 6.x kernel (`uname -r`; "
     "`wsl --update` if it is older); on macOS a Linux virtual machine or Docker. In Docker giong "
     "runs on epoll; allow io_uring as [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) shows to get its "
     "full speed."),
    ("Which Linux kernel do I need?",
     "For io_uring, 6.0 or newer, 6.1 or newer recommended. giong tries three ring setups in order: "
     "`DEFER_TASKRUN` (6.1+), `COOP_TASKRUN` (6.0), then no flags. On an older kernel, or where "
     "io_uring is blocked, it runs on its epoll driver (tested on 5.15)."),
    ("Can I use hyper, reqwest, sqlx or other Tokio crates?",
     "Not crates that need the Tokio runtime; giong has its own HTTP, TLS, PostgreSQL, MySQL and "
     "Redis crates instead. Crates that only use `std::future` work. Adapters for the `futures-io` "
     "and Tokio I/O traits are on the roadmap."),
    ("Is it faster than Tokio?",
     "Sometimes, and the benchmarks page says where. Small responses at 50 connections are a tie. "
     "giong makes about 0.03 system calls per request against 2.05, spends the least CPU per "
     "request at 5,000 connections, and reads files 5.5 times faster than Tokio on one core. Every "
     "number is over loopback on one machine."),
    ("How do I report a security problem?",
     "Privately, through GitHub's private vulnerability reporting (the Security link at the "
     "bottom of every page), not in a public issue."),
    ("Why the name?",
     "After Thánh Gióng, the child of Vietnamese legend who lay silent for three years and then "
     "rose a giant: next to nothing while idle, every core the moment load arrives."),
]
