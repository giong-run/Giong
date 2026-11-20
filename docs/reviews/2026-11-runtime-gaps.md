# What the Gióng runtime still lacks: gap analysis

Written 2026-11-15 to answer: if giong is developed as a runtime in the true sense and for
the long term, what is still missing, now and in the future, and which of it would be a
breakthrough rather than parity. Read from the code on main `14c02f4` (v0.7.0 + docs), the ROADMAP, the 1.1-1.4
plan and the lean-runtime plan. No code was changed.

Rules used:

- **The boundary:** the core takes only what only a runtime can do. Everything else
  is a library or framework on top. Each gap is tagged **CORE** or **LIB**. Some are **CORE
  primitive + LIB**: the runtime adds a small hook, the feature lives above it.
- **Already decided, not re-planned:** 0.8 "Verified in CI", 0.9 "Bare metal and real use",
  1.0.0 (no features before it); 1.1 gRPC (+ public `giong::sim`), 1.2 macOS (+ worker guard,
  overload protection), 1.3/1.4 HTTP/3 and Windows (+ ACME, RIO, "why slow" report), 1.5 memory
  contract. New items are placed around these.
- **"Breakthrough" is used strictly:** nobody ships it in a production runtime, or those who do
  stop well short. "Parity" means a hole that others already fill; it still matters, it just
  is not news. Each claim names the prior art.

---

## 1. Ranked short list

The handful that matter most for giong's goals (heavy load on scarce hardware, built and run by AI
agents, a true general runtime), in the order I would do them.

| # | Gap | Core/Lib | Kind | Slot |
|---|---|---|---|---|
| 1 | **Blocking and compute lane** (`spawn_blocking`, a CPU pool bound to the core budget) | CORE | Parity, but a hole | 1.1 |
| 2 | **Structured concurrency with deadlines enforced in the kernel** (scopes, cancel tree, every op of a scope linked to its deadline with `IORING_OP_LINK_TIMEOUT`) | CORE | Breakthrough in Rust; concept exists elsewhere | 1.1-1.2 |
| 3 | **Processes, signals, pipes, fd passing** (pidfd + io_uring `waitid`, stdio pipes, signal streams, `SCM_RIGHTS`) | CORE | Parity | 1.1 |
| 4 | **Production record and replay** (a flight recorder of everything nondeterministic a worker sees; replay a production bug bit-for-bit in `giong::sim`, handed to an agent through giong-mcp) | CORE | **Real breakthrough** | 1.3-1.5 |
| 5 | **Memory contract driven by the cgroup** (1.5, plus: read `memory.max` and PSI pressure, tighten before the OOM killer) | CORE | Breakthrough with PSI; Seastar and Go have parts | 1.5 |
| 6 | **Scheduling groups / QoS classes** (CPU and I/O shares per class: requests vs background vs tenant) | CORE | Parity with Seastar/glommio, new to Rust-mainstream | 1.5-1.6 |
| 7 | **NIC-queue-aligned workers** (reuseport eBPF steering so the core that receives a packet runs its connection) | CORE | Parity with Seastar; rare elsewhere | 1.6 |
| 8 | **Elastic cores** (a service shrinks to fewer active workers when quiet and grows under load, so many services share one small machine) | CORE | **Breakthrough** (research prior art only: Shenango, Caladan) | 1.7+ |
| 9 | **Zero-downtime restart with socket handoff** (and later handoff of idle keep-alive connections) | CORE primitive + LIB | Parity for listeners; breakthrough for live connections | 1.3-1.4 |
| 10 | **io_uring kernel-side pipelines** (linked ops, `splice` socket to socket, `MSG_RING` between workers, registered buffers) | CORE | Partly new in Rust | 1.x continuous |

Why these and not others:

- **1 and 3 are holes, not features.** Today there is no public way to run a blocking call or a
  CPU-heavy job without stalling a worker, and no way to start a child process or receive a
  signal other than SIGINT/SIGTERM. An agent writing a real service hits both within a day
  (password hashing, image resize, calling `ffmpeg`, `git`, a sandbox). It reaches for
  `std::thread` or Tokio and loses Gióng's guarantees. They are parity, and they come first.
- **2 is the foundation every framework above needs.** Rust has no mainstream runtime with
  structured concurrency (Trio, Kotlin, Swift and Java's `StructuredTaskScope` do). Gióng can go
  further than all of them: because every operation is already an io_uring submission, a
  scope's deadline can ride along as a linked timeout, so the kernel enforces it with no extra
  syscall, no timer wake-up and no forgotten `timeout()` around a call.
- **4 is the one idea I would call a true breakthrough that only Gióng's design makes cheap.** A
  thread-per-core, completion-based runtime receives all nondeterminism through three doors:
  completions, timer expiry and cross-thread injects. Record those (with received bytes) into a
  ring buffer, and a crash or a wrong answer in production can be replayed exactly in the
  simulation driver. Go's flight recorder (Go 1.25) records traces but cannot replay; rr and
  Microsoft TTD replay whole processes at 2-10x slowdown; Antithesis does it at the VM level as a
  commercial service. Nobody does it at the runtime level, always on, at a few percent cost. It
  is also exactly what an AI agent needs: a failing case it can run, not a log to guess from.
- **5 to 8 are the "scarce hardware" thesis.** Seastar proves 6 and 7 pay off; nobody has
  combined them with a hard memory contract (5) and elastic cores (8) in one runtime, which is
  what "a small machine taking huge load, next to other services" actually needs.

---

## 2. Where Gióng stands today, area by area

Legend: **Have** / **Partial** / **Missing**. Each missing item: what it is, core or library, why
it matters, breakthrough or parity (with prior art), effort, slot. Effort is for one thread
working full time, honest guess.

### 2.1 Task model and scheduling

**Have.** `spawn` of `!Send` tasks on the current worker; `Handle::spawn_on`, `spawn_balanced`
(shared queue, first idle worker) and `least_loaded_worker`; `JoinHandle`/`RemoteJoinHandle` with
`abort`, `is_finished`, panic reporting; `yield_now`; FIFO run queue polled in batches sized to
the queue (`executor.rs:585`), so a waking task cannot starve the others in one batch; panics in
tasks are caught and the worker carries on; spawn sites recorded for `observe`.

**Partial.**
- Cooperative budget: the epoll driver limits immediate operations to 128 per poll
  (`driver/epoll.rs:49`). On io_uring there is no per-task budget; a task that keeps finding data
  ready can monopolize a batch. Not verified as a real problem yet; worth a test. CORE, parity
  with Tokio's coop budget, 2-3 days, 1.0.x if a test shows it.
- Task allocation: a task is a `Box<dyn Future>` plus an `Arc<TaskWaker>` (two allocations per
  spawn; Tokio does one). CORE, parity, about 1 week, 1.0.x internal.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Task groups (`JoinSet`-like) | Own a dynamic set of tasks, collect results as they finish, abort all on drop | CORE | Every server with fan-out needs it; agents otherwise write leaky loops | Parity (Tokio `JoinSet`) | 3-4 days | 1.1 |
| Task-local values | A value scoped to one task (request id, tenant, deadline) | CORE | Deadline and tenant propagation for 2.2 and 6 | Parity (Tokio `task_local!`, Go context) | 2-3 days | 1.1 |
| Priorities / scheduling groups | Classes with CPU shares (requests vs background vs per tenant), and an I/O scheduler with shares for disks | CORE | Background work (compaction, cache refresh, uploads) must not raise request p99; multi-tenant fairness | Parity with Seastar scheduling groups and glommio task queues; new among Rust-mainstream runtimes | 3-4 weeks | 1.5-1.6 |
| Opt-in stealable tasks | `spawn_send` tasks that an idle worker may take, next to `!Send` tasks that never move | CORE | Uneven long-lived CPU work is where Tokio beats any thread-per-core runtime today; the ROADMAP calls work stealing a non-goal, honestly a weakness for "any workload" | Parity with Tokio; hybrid is unusual | 3-5 weeks, risky (touches the wake path) | 2.x discussion, or never |
| Preemption | Interrupt a task that never yields | — | Not achievable for Rust futures without compiler support. BEAM (reductions) and Go (async preemption by signal) can; Gióng cannot. The honest substitute is the 1.2 worker guard (detect, report, route new work away) plus the compute lane (2.5) | — | — | — |

### 2.2 Structured concurrency and cancellation

**Have.** Cancellation safety of every I/O future (buffer handed to the driver until the kernel
confirms); `timeout`; `until_shutdown`; `shutdown_signal`; graceful drain with
`shutdown_grace`; `JoinHandle::abort`.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Scopes | A future that owns child tasks: none outlives it, a failure cancels siblings, results return as a value. Polled inside the parent (no `'static`), so borrowing is sound | CORE | Removes orphan tasks, the most common async bug agents write; base for every framework | Parity in concept (Trio nurseries, Kotlin, Swift, Java 21+); missing in Tokio (`async-scoped`, `moro` are libraries with caveats) | 2-3 weeks | 1.1-1.2 |
| Deadlines in the kernel | A scope carries a deadline; every op submitted inside it gets `IORING_OP_LINK_TIMEOUT` at submission. On epoll, the timer wheel does it | CORE | No call can forget its timeout; cancellation costs no extra syscall | **Breakthrough**: no runtime ties a structured deadline to kernel-linked timeouts | +1-2 weeks on top of scopes | 1.2 |
| Cancel reasons | Cancellation carries why (deadline, shutdown, peer gone, shed) as a coded error | CORE | Agents can tell a timeout from a shed from a bug | Parity+ (Go `context.Cause`) | 3 days | with scopes |
| Deadline propagation across services | `grpc-timeout` / a header read into the scope's deadline | LIB (giong-http, giong-grpc) | End-to-end budgets | Parity (gRPC, Go) | 2-3 days | 1.1 with gRPC |

### 2.3 Time

**Have.** Per-worker hierarchical timing wheel, arming costs no syscall, nearest deadline passed
as the ring wait (`time.rs`); `sleep`, `sleep_until`, `timeout`, `Sleep::reset`; resolution
1 ms, never early; `Sleep` is `Send` for compat.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| `interval` with missed-tick policy | Periodic timer (burst / delay / skip) | CORE | Every agent-written heartbeat; today a hand loop that drifts | Parity (Tokio) | 2 days | 1.1 |
| High-resolution timers | Sub-millisecond deadlines through an io_uring timeout op for the few that need it | CORE | Pacing (QUIC needs it for 1.3), trading, media | Parity (glommio, Seastar) | 1 week | 1.3 with QUIC |
| Clock choice | `CLOCK_BOOTTIME` for timers that must count suspend, coarse clock for cheap timestamps | CORE | Correctness on laptops/VMs; cheap `now()` for logging | Parity | 2-3 days | 1.2 (macOS needs it anyway) |
| Virtual time | Time that advances only when everything is idle | CORE | Part of `giong::sim` | Already planned (1.1) | — | 1.1 |

### 2.4 Synchronisation and channels

**Have.** Cross-worker `mpsc` (unbounded), `bounded` with backpressure, `oneshot`. Internal
`notify` with a loom-checked wake-up handshake.

**Missing.** Note first: `tokio::sync` does not need Tokio's runtime, so it already works on Gióng.
Native versions earn their place only by being faster (no atomics within a worker).
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Worker-local primitives | `Mutex`, `RwLock`, `Semaphore`, `Notify`, channel without atomics, for tasks on one worker | CORE | Zero-atomic coordination is the point of thread-per-core; semaphores are how agents should bound concurrency | Parity with monoio/glommio local channels | 1-2 weeks | 1.1-1.2 |
| `watch` and `broadcast`, cross-worker | Config reload, shutdown fan-out, cache invalidation to every worker | CORE | Every multi-worker service needs "tell all workers" | Parity | 1 week | 1.2 |
| Wait on a futex in the ring | `IORING_OP_FUTEX_WAIT/WAKE` (6.7) so a worker can wait on a lock shared with a non-Gióng thread or another process without a thread | CORE primitive | Base for local IPC (2.13) and for mixing with C libraries | **New in Rust runtimes** (nobody exposes it) | 1 week | 1.4+ |
| Cross-worker wake via `MSG_RING` | Replace eventfd + mutex inject with `IORING_OP_MSG_RING` between workers' rings | CORE internal | Fewer syscalls per cross-worker message; can also pass fds between rings | Parity with what io_uring offers; unused by Tokio, monoio uses it partly | 1-2 weeks | 1.0.x internal |

### 2.5 Blocking and CPU-bound work

**Have.** An internal blocking-thread pool used only by the epoll driver for file operations
(`driver/blocking.rs`, up to 64 threads). Nothing public.

**Missing. This is the biggest hole for "a general runtime".**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| `spawn_blocking` | Run a blocking closure on a bounded pool, await its result | CORE | Any C library, `std::fs` code, DNS via libc, bcrypt; without it agents block a worker and p99 collapses | Parity (Tokio, async-std, .NET thread pool) | 3-5 days (the pool exists) | **1.1** |
| Compute lane | CPU-heavy jobs on a pool sized against the runtime's core budget (cgroup quota minus workers), with a bounded queue and shedding | CORE | Image/JSON/ML-inference/compression jobs must not compete blindly with network workers on a small box | Parity in function; "sized against the same cgroup budget as the workers" is unusual (Tokio's blocking pool is 512 threads by default) | 1-2 weeks | 1.1-1.2 |
| Rayon-style data parallel | Parallel iterators | LIB | Use Rayon itself on the compute lane | — | — | never in core |

### 2.6 Networking

**Have.** TCP (`bind`, `bind_balanced`, `accept` multishot, `connect` by name with RFC 8305 racing,
`recv` from provided buffers, `read`, `write`, `write_vectored`, zero-copy send for large
buffers, `send_with_control` for kTLS, `send_file` via `sendfile`, `shutdown`, `set_nodelay`);
UDP with multishot receive, `SO_REUSEPORT` across workers, `set_broadcast`; Unix stream sockets
(`bind`, `connect`, `pair`); `from_std` everywhere; DNS client of its own.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Socket options | keepalive, `TCP_USER_TIMEOUT`, buffer sizes, `IP_TOS`, `SO_MARK`, `TCP_FASTOPEN`, `TCP_DEFER_ACCEPT`, linger, `SO_BUSY_POLL`, backlog, `IPV6_V6ONLY`; today only `nodelay` and `broadcast` (users must drop to `socket2` via `from_std`) | CORE | Real deployments behind load balancers need keepalive and user timeout; agents should not need raw fds | Parity | 1 week | 1.1 |
| Unix datagram, `SCM_RIGHTS`, `SO_PEERCRED`, abstract names | Passing fds between processes, knowing who connected | CORE | Needed by socket handoff (2.9), sandboxes, local IPC | Parity (Tokio has all but fd passing in std) | 1 week | 1.1 |
| UDP offload and control data | GSO/GRO, ECN, `IP_PKTINFO`, `recvmmsg`/`sendmmsg` | CORE | QUIC speed | Already planned (1.3) | — | 1.3 |
| `splice` / `tee` socket to socket | Move bytes between two sockets (or pipe) without user space | CORE | Proxies, gateways, tunnelling at near-zero CPU for bodies | Parity with HAProxy; **new in Rust runtimes** as a first-class op | 1-2 weeks | 1.2-1.3 |
| NAPI busy poll for io_uring | Register the worker's ring for NAPI busy poll (6.9+) | CORE | Lower latency without spinning the whole core in user space | Parity with what the kernel offers; unused in Rust runtimes | 1 week + bare metal | 1.x after 0.9 data |
| Multishot receive bundles, send bundles | io_uring 6.10 bundles: one completion for many buffers | CORE internal | Fewer completions per busy connection | New in Rust runtimes | 1-2 weeks | 1.0.x/1.x |
| Zero-copy receive (zcrx) | NIC writes payload straight into user memory (6.15+, header split) | CORE | CPU per byte on large uploads, proxies | **Breakthrough-level** if it works (no runtime ships it); NIC-dependent | 3-4 weeks + hardware check | 1.6+ |
| NIC-queue alignment | Reuseport eBPF program sending each connection to the worker on the core that handles its RX queue; `SO_INCOMING_CPU` | CORE | Removes cross-core cache misses per packet; a large share of CPU at high req/s | Parity with Seastar (RSS-aware); rare elsewhere | 2-3 weeks + bare metal | 1.6 |
| MPTCP | `IPPROTO_MPTCP` sockets | CORE (tiny) | Mobile/multi-path clients | Parity, easy | 2 days | any 1.x |
| vsock | VM/host sockets (Firecracker, Kata, Nitro) | CORE | Micro-VM sandboxes are where agents run code | Parity (tokio-vsock) | 3-4 days | 1.x |
| Raw/packet sockets, netlink, TUN/TAP | Low-level networking | CORE (thin wrappers) | VPNs, network tools, container networking | Parity | 1-2 weeks | later, on demand |
| AF_XDP driver | Packets from the NIC to user space bypassing the stack, for UDP | CORE | Millions of packets per core for QUIC/DNS | Breakthrough for a general runtime; DPDK/Seastar-native exist | 2-3 months, research | 2.x horizon |

### 2.7 Files and storage

**Have.** `File` (open, create, `read_at`, `write_at`, `sync_all`, `sync_data`, `allocate`,
`metadata` via `statx`, `close`), `read`, `write`, `remove_file`, `remove_dir`, `create_dir`,
`rename`, `read_dir`, `symlink_metadata`, `OpenOptions` with `mode` and `custom_flags`. All on
the ring with io_uring; on blocking threads with epoll.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Registered buffers and files | `IORING_REGISTER_BUFFERS` / fixed files for reads/writes | CORE | Fewer page pins per op on storage-heavy services | Parity (glommio) | 1-2 weeks | 1.x |
| Direct I/O helpers | Aligned buffer type, `O_DIRECT` open with checks, block size discovery | CORE | Databases, caches, log stores built on Gióng | Parity (glommio `DmaFile`) | 1-2 weeks | 1.x |
| Durability ops | `RWF_DSYNC` per write, `sync_file_range`, `fsync` of a directory, `ftruncate` | CORE | Correct write-ahead logs; agents get fsync wrong all the time | Parity | 1 week | 1.1-1.2 |
| Safe path resolution | `openat2` with `RESOLVE_BENEATH`/`RESOLVE_NO_SYMLINKS`, `*at` ops relative to a dir fd | CORE | Serving files and running agents' tools without path traversal | Parity with kernel; **rare in runtimes** | 1 week | 1.2 |
| Links, xattr, permissions, copy | `symlink`, `hard_link`, `set_permissions`, `copy_file_range`, xattrs | CORE | Completeness | Parity | 1 week | 1.x |
| File watching | inotify / fanotify streams | CORE | Config reload, dev servers, agents' file tools | Parity (notify crate, Node `fs.watch`) | 1 week | 1.2 |
| Storage I/O scheduler | Shares and latency targets per class for the disk | CORE | Seastar's IO scheduler is why ScyllaDB holds p99 under compaction | Parity with Seastar | with 2.1 groups | 1.6 |

### 2.8 Processes and signals

**Have.** SIGINT/SIGTERM handler that triggers shutdown (`runtime.rs:1334`). Nothing else.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Child processes | `Command` that spawns (posix_spawn / `clone3` with pidfd), waits via `IORING_OP_WAITID` (6.7) or pidfd poll, kills, sets limits | CORE | Agents run tools; build systems; sandboxes | Parity; io_uring-native waiting is **new** | 1.5-2 weeks | **1.1** |
| Pipes and stdio | Async `ChildStdin/Stdout/Stderr`, `pipe2`, the process's own stdin/stdout | CORE | Same | Parity | with the above | 1.1 |
| Signal streams | Receive any signal (SIGHUP reload, SIGUSR1 dump, SIGCHLD) as a stream | CORE | Config reload, on-demand dumps | Parity (Tokio `signal`) | 3-4 days | 1.1 |
| Sandboxed spawn | Spawn into new namespaces, seccomp filter, cgroup, rlimits, landlock | LIB on the core `Command` | Running untrusted agent code safely | Parity (bubblewrap, nsjail); new as a runtime-native crate | 3-4 weeks | LIB, 1.x |

### 2.9 Restart and lifecycle

**Have.** Graceful shutdown with drain and grace period.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Socket activation | Take listeners from systemd (`LISTEN_FDS`) | CORE | Standard deploys, restart without dropped SYNs | Parity | 2 days | 1.1 |
| Listener handoff | New binary receives listeners from the old one over a Unix socket; old drains | CORE primitive (fd passing, `from_std`, drain) + LIB (the protocol) | Deploys drop nothing; small machines cannot run blue/green | Parity (Envoy hot restart, Cloudflare tableflip, nginx) | 1-2 weeks | 1.3-1.4 |
| Live connection handoff | Idle keep-alive connections (and kTLS sessions, whose keys sit in the kernel) handed over too | CORE + LIB | No reconnect storm after a deploy | **Breakthrough**: nobody does it for TLS connections in production | 3-4 weeks after listener handoff | 1.6+ |
| Health and readiness states | Runtime-level state (starting, ready, draining) exposed for probes | CORE (tiny) + LIB | Kubernetes integration | Parity | 2 days | 1.1 |

### 2.10 Memory

**Have.** Kernel-shared buffer ring per worker for receive (packed, filled piece by piece on
6.12); idle cost per connection measured (HTTP/1.1 2.1 KiB, HTTPS 12.5 KiB vs nginx 15.1);
finite limits documented. Plans: idle HTTPS ≤ 4.5 KiB (1.0.x), h2 allocations ≤ 4 (1.0.x),
memory contract (1.5).

**Missing beyond the plans.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| cgroup memory awareness | Read `memory.max`/`memory.high` and PSI `memory.pressure`; size buffers and the 1.5 ledger from them; tighten admission when pressure rises | CORE | The process sheds before the OOM killer acts; same binary right-sizes itself on a 256 MB or a 64 GB box | **Breakthrough with PSI** (Go `GOMEMLIMIT` and .NET `GCHeapHardLimit` are static limits; nobody reacts to PSI) | 1-2 weeks on top of 1.5 | 1.5 |
| Memory per spawn site | Bytes allocated and held, grouped by the spawn site `observe` already records | CORE (optional counting allocator hook) | "Which code holds the RAM" in one answer for an agent | Parity with Go heap profile; new grouped by task origin | 1-2 weeks | 1.5 |
| Huge pages for rings and buffers | `MAP_HUGETLB` / THP for ring and buffer memory | CORE internal | TLB misses at high connection counts | Parity (Seastar) | 3-4 days + bare metal | 1.x |
| Per-worker allocator | Allocator arenas tied to a worker | — | Already dropped by the lean plan with numbers; keep dropped | — | — | — |

### 2.11 Observability

**Have.** `Pulse` per worker from any thread (polls, state, the task being polled);
`snapshot` (tasks by spawn site, timers, I/O in flight, registered sources); poll profiling by
spawn site; coded errors; structured logs; `giong-mcp` serving all of it read-only; optional
`tracing`.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Await-point task dumps | For each live task, where it is waiting (async backtrace), not only where it was spawned | CORE | "Stuck" answered with the exact line | Parity (Tokio taskdump, unstable) | 2-3 weeks | 1.2 with worker guard |
| Runtime hooks | `on_spawn`, `on_poll_start/end`, `on_park` for libraries | CORE | Lets OTel/metrics crates exist without forking | Parity (Tokio unstable hooks) | 1 week | 1.1 |
| Static tracepoints (USDT) | Zero-cost probes readable with bpftrace/perf | CORE | Production debugging without restart | Parity (.NET EventPipe, Go trace, DTrace providers) | 1 week | 1.x |
| Execution flight recorder | Last N seconds of scheduling events in a ring, dumped on demand or on panic | CORE | Base of record and replay (2.12) | Parity with Go 1.25 FlightRecorder | 1-2 weeks | 1.3 |
| OTel/Prometheus export | Exporters | LIB | — | Parity | — | LIB |
| giong-mcp actions | Change limits, toggle profiling, take a dump, start a recording, through MCP (opt-in) | LIB (giong-mcp) on CORE hooks | An agent can investigate, not only read | New | 1-2 weeks | 1.4 with the report |

### 2.12 Testing and verification

**Have.** loom on the wake-up handshake, Miri gate on unsafe paths, TSan, fuzzing nightly,
2-hour soak nightly, real 6.0/6.1/5.15 kernels in QEMU, semver checks. In flight: Kani proofs,
internal simulation driver, instruction-count thresholds. Planned: public `giong::sim` (1.1).

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| **Record and replay** | Record a worker's completions, timer firings, injects and received bytes into a bounded ring; replay into `giong::sim` deterministically; giong-mcp hands the file to an agent | CORE | Turns a production heisenbug into a reproducible test | **Real breakthrough** (prior art: rr/TTD whole-process and slow, Antithesis VM-level and commercial, Go flight recorder without replay) | 6-8 weeks after `giong::sim` | 1.3-1.5 |
| Fault injection as a public API | Users inject disk errors, short writes, ENOMEM, reorderings in their own tests | CORE (via sim) | Agents get failure-path tests for free | Parity with turmoil/madsim/FoundationDB; better when it is the real code path | with 1.1 sim | 1.1 |
| Linearizability checks | Checker for histories recorded in sim | LIB | Correctness of databases built on Gióng | Parity (Jepsen, Porcupine) | 2 weeks | LIB |

### 2.13 Configuration and resource discovery

**Have.** Worker count from affinity and cgroup CPU quota; `pin_cores`; ring size, buffers,
spin-before-park; `GIONG_DRIVER`, `GIONG_RING_TIER`; Ubuntu kernel workaround detection;
`RLIMIT_MEMLOCK` explanation.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| CPU topology | SMT siblings, L3 groups, isolated CPUs; place workers and balance by cache domain | CORE | Balanced placement on real servers; avoid two workers on one physical core | Parity (Seastar, glommio placement) | 1-2 weeks | 1.6 |
| NUMA | Memory and rings allocated on the worker's node | CORE | Large servers lose 20-40% across nodes | Parity (Seastar, glommio) | 2 weeks + 2-socket machine | 1.6 |
| Elastic cores | Active worker count follows load; idle workers park and stop receiving connections (eBPF steering), wake within microseconds when load returns | CORE | Many services on one small box share cores instead of each reserving all of them; power and cloud bill | **Breakthrough** (Shenango NSDI'19, Caladan OSDI'20 are research systems with kernel changes; no production runtime does it) | 4-6 weeks, research | 1.7+ |
| Local IPC | Shared-memory rings between processes on one machine with futex-in-ring wake-ups | CORE primitive (2.4) + LIB | Services and sidecars on one box talk without TCP | Parity with iceoryx/memif; new as runtime-native | 3-4 weeks | LIB, 1.x |

### 2.14 Platforms and drivers

**Have.** io_uring (tiers for 6.0/6.1+), epoll fallback, kernel workaround detection.
**Planned.** kqueue (1.2), IOCP then RIO (1.4). **Missing beyond:** FreeBSD (kqueue reuse, cheap
after 1.2), Android/aarch64 CI (works in theory, never tested), WASI 0.3 async (a different world;
not core now). Honest: Gióng is Linux-only today, where Tokio runs everywhere.

### 2.15 Safety and error model

**Have.** Coded errors with cause and fix (about 140 codes with pages), finite limits, `unsafe`
documented and gated by Miri, review of all unsafe code done.

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Supervision | Restart policies for tasks (one-for-one, backoff, intensity limits), BEAM style | LIB on core `JoinHandle`/scopes | Self-healing services | Parity with Erlang/Akka; per-process heaps and preemption stay out of reach | 2 weeks | LIB, 1.x |
| Panic policy per task group | Abort the group, restart, or escalate | CORE (with scopes) | Same | Parity | with scopes | 1.2 |
| `giong lint` | Lints for async mistakes agents make (borrow across await, blocking call in a task, unbounded fan-out) | Tooling, not core | Catches at compile time what the worker guard catches at run time | New for a runtime | 2-3 weeks | 1.x |

### 2.16 Interop

**Have.** `giong-compat`: Tokio and futures I/O traits for driven crates (tokio-rustls, h2, hyper,
tungstenite tested).

**Missing.**
| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| Crates that call `tokio::spawn` or need Tokio's reactor | A side Tokio runtime bridged by channels, documented and measured | LIB | Many SDKs (AWS, cloud clients) assume Tokio; today they cannot run inside Gióng at all | Parity (bridges exist) | 1 week | LIB, 1.1 |
| C ABI / embedding | Run Gióng from Python, Node, Go via a small C API | LIB | Reach beyond Rust | New-ish | 4+ weeks | later |

### 2.17 Building MCP servers on Gióng 

**Can Gióng host MCP servers for agents (for example one for a video editor)?** Yes in principle:
an MCP server is a small JSON-RPC service over stdio or HTTP, and Gióng already has the HTTP
server, JSON handling in giong-mcp and the security rules (loopback, token, DNS-rebinding check,
bounded requests).

**What exists today (checked in `giong-mcp/src/lib.rs`, v0.7.0):** giong-mcp is **not reusable**.
It serves only five fixed introspection tools (`overview`, `workers`, `tasks`, `sources`,
`profile`). There is no public API to register your own tools, resources or prompts, no stdio
transport (only Streamable HTTP over TCP or a Unix socket), no event streams (one JSON response
per `POST`), and it runs on its own thread for introspection, not as a general service.

| Gap | What | Core/Lib | Why | Kind | Effort | Slot |
|---|---|---|---|---|---|---|
| MCP server library | A crate (e.g. `giong-mcp-server`, with giong-mcp rebuilt on it) where a user declares tools (typed input/output, JSON Schema derived from Rust types), resources and prompts, and serves them over stdio and Streamable HTTP (with SSE for progress and notifications), with giong-mcp's security defaults and bounded costs | LIB | Agents need many app-specific MCP servers; one that idles at a few KiB and handles many agent sessions per core fits the goal of heavy load on small machines | Parity with the official Rust SDK (`rmcp`, on Tokio) and the TS/Python SDKs; the edge is footprint, coded errors with fixes in tool results, and `giong::sim` tests of the server | 2-3 weeks (protocol code exists in giong-mcp) | 1.1-1.2, in parallel; branch-only before 1.0 |
| What stdio transport needs from the core | Async stdin/stdout of the process itself (non-blocking pipes on the ring), and child processes with piped stdio for the client side (an agent host spawning MCP servers) | CORE | Most local MCP servers run over stdio | Parity; already listed in 2.8 | in 2.8 | 1.1 |
| Long tool calls | Progress notifications and cancellation (`notifications/cancelled`) mapped onto scopes and deadlines (2.2), so a cancelled call frees everything it started | LIB on CORE scopes | Agents cancel calls often | Parity; cleaner than most SDKs | with the library | 1.2 |

Note: until the library exists, someone can already serve MCP over HTTP with giong-web and
hand-written JSON-RPC; stdio servers have to wait for 2.8.

---

## 3. Things that look like gaps but should stay out of the core 

WebSocket and SSE, rate limiting, OpenTelemetry exporters, ACME, gRPC, HTTP/3, database drivers,
durable execution for agents, WASM sandboxes, supervision trees, sandboxed spawn, local IPC
protocols, linearizability checkers. Each needs at most a small core primitive listed above
(hooks, fd passing, futex wait, `Command`, scopes); the rest is a crate on top.

## 4. Where Gióng is behind, honestly

- **Behind Tokio:** no blocking pool, no processes or signals, few sync primitives, few socket
  options, no `JoinSet`/`interval`/task-locals, no work stealing for uneven CPU work, Linux only,
  far smaller ecosystem. Most are cheap to close; work stealing and platforms are not.
- **Behind Seastar:** no scheduling groups, no I/O scheduler, no NUMA or topology placement, no
  RSS alignment, no DMA file API. These are the gaps that matter most for squeezing a small box.
- **Behind glommio:** no task-queue shares, no direct I/O API, no registered buffers for files.
- **Behind Go / BEAM:** no preemption (cannot match in Rust), no supervision, no per-process
  heaps. Go has a full execution tracer with flight recorder; Gióng has profiling by spawn site.
- **Ahead:** completion I/O with batching and kTLS, idle memory per connection below nginx for
  HTTPS, coded errors and agent tooling (giong-mcp), verification depth (loom, Miri, TSan, Kani,
  fuzz, soak, sim coming), native HTTP/DB clients measured against the Tokio ones.

**A conflict, now settled:** the ROADMAP's former non-goal "Every workload: giong covers network services"
contradicted the aim of a complete runtime that fits any workload. Gaps 2.5 (compute lane), 2.8
(processes) and the opt-in stealable tasks in 2.1 only make sense if that non-goal is softened to
"network services first". Decided 2026-11-15: softened to "network services first" (ROADMAP, Non-goals and Runtime core).

## 5. Suggested placement

The ROADMAP section "Runtime core (track R, after 1.0)" orders these as three waves and supersedes the slots below where they differ. Nothing before 1.0.0. A third track, **R (runtime core)**, beside the protocol and platform
tracks, landing in whichever 1.x ships next:

| Slot | Track R content | Effort |
|---|---|---|
| 1.0.x (internal, no API) | one-allocation tasks, `MSG_RING` cross-worker wakes, receive bundles, io_uring coop budget if a test shows the need; the lean plan's idle/alloc fixes | 3-4 weeks |
| **1.1** | `spawn_blocking` + compute lane, child processes/pipes/signals, socket options, Unix datagram + fd passing + socket activation, `JoinSet`, task-locals, `interval`, runtime hooks, readiness state | 6-7 weeks (more than gRPC's own size: split into 1.1 and a 1.1.x/1.2 if needed) |
| **1.2** | scopes + kernel-enforced deadlines + panic policy, worker-local sync + `watch`/`broadcast`, `splice`, file watching, safe path resolution, durability ops, await-point dumps (with the worker guard already planned) | 6-8 weeks |
| 1.3-1.4 | high-resolution timers (with QUIC), flight recorder, listener handoff, giong-mcp actions, futex-in-ring | 5-6 weeks |
| **1.5** | memory contract (planned) + cgroup/PSI awareness + memory per spawn site; record and replay | 8-10 weeks |
| 1.6 | scheduling groups + I/O scheduler, CPU topology, NUMA, NIC-queue alignment, zcrx (each behind a bare-metal benchmark) | 8-10 weeks |
| 1.7+ | elastic cores, live connection handoff | research first |
| 2.x horizon | AF_XDP driver, opt-in work stealing (only if a workload proves the need) | — |

Every item lands with its proof: a test under the simulation driver, a benchmark with raw data
for speed or memory claims, a coded error and doc page for every new failure mode.
