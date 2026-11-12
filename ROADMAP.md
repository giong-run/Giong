# Roadmap

Where giong is going, in the order we intend to get there. Nothing here has a date:
a milestone is done when its exit criteria are met. [Open an issue](https://github.com/giong-run/Giong/issues) before starting on
an item so the work is not duplicated.

## Goal

A runtime for **network servers on Linux** that makes the fewest possible system calls and
keeps tail latency low, by committing fully to io_uring and to one thread per core.

## Non-goals

These are deliberate. Proposals that need them will be declined.

- **Other operating systems.** No epoll, kqueue or IOCP fallback.
- **Work stealing.** Tasks stay on the worker that created them.
- **A general-purpose runtime.** giong is not trying to replace Tokio for every workload.
- **`Send` futures as a requirement.** The API stays usable with `Rc` and `RefCell`.

## 0.1 — Foundation (done)

Executor, io_uring driver, TCP, UDP, files, timers, thread-per-core runtime, busy-poll
mode, cross-thread channel, cancellation safety, backpressure, tests and benchmarks. See [CHANGELOG.md](CHANGELOG.md).

## 0.2 — A runtime you can serve from (this release)

0.2 took the pieces a real service needs first, wherever they stood in the earlier plan,
together with the verification that needed no extra hardware.

- [x] Graceful connection draining on shutdown (`Builder::shutdown_grace`).
- [x] Unix domain sockets.
- [x] Multishot receive for UDP.
- [x] More file operations: metadata, rename, remove, directories.
- [x] `giong-http`: an HTTP/1.1 server library grown out of the example.
- [x] TLS through `rustls` (`giong-tls`, server side), and HTTPS in `giong-http`.
- [x] Run the multi-threaded tests under ThreadSanitizer (nightly), locally and in CI.
- [x] Verify the declared minimum Rust version in CI (1.80, libraries).
- [x] Exercise the ring setup fallbacks by forcing them on a new kernel
      (`GIONG_RING_TIER`).
- [x] Compare against Monoio and Glommio on one core over loopback (see the README).
- [x] A first two-core measurement (the load generator is the limit; see the README).
- [x] Fuzz-style tests for the HTTP parser: 500,000 random inputs for the request parser
      and the chunked decoder in `giong-http`.

## 0.3 — Prove it

The numbers so far come from a 4-vCPU virtual machine over loopback. Before adding more
features, find out what is actually true.

- [ ] Benchmark on a multi-core machine with the load generator on a separate host and a
      physical network card.
- [ ] Measure scaling from 1 to N workers.
- [ ] Repeat the Monoio and Glommio comparison on several cores and a physical network.
- [x] Run the test suite on real 6.0 and 6.1 kernels (in QEMU, in CI; it found that
      6.0 could fail to start a runtime, now fixed).
- [x] Model-check the cross-thread wake-up handshake with `loom` (it found a lost
      wake-up, now fixed).
- [x] Coverage-guided fuzzing (`cargo fuzz`) of the `giong-http` parser, the chunked
      decoder and a whole connection, nightly in CI.
- [x] Add a soak test: hours of connection churn with fd and memory checks (two hours
      every night in CI).
- [ ] An independent look at `giong-tls` before anyone relies on it.

**Exit criteria:** published multi-core results with their method, and no open memory- or
thread-safety finding.

## 0.4 — Breadth

- [ ] Vectored writes (`writev`) so a response head and body need no copy.
- [ ] Streaming request and response bodies in `giong-http`.
- [ ] The client side of `giong-tls`.
- [ ] Remaining file operations: reading directories, `fallocate`.
- [ ] Asynchronous DNS resolution (today `bind` and `connect` resolve names blocking).
- [ ] A bounded channel and a oneshot channel in `sync`.
- [ ] `spawn_on` that returns a handle to the spawned task's result.

**Exit criteria:** every item above with tests, including cancellation tests for each new
operation.

## 0.5 — Speed

Each item lands only with a benchmark showing what it buys.

- [ ] Registered ring fd and fixed files (direct descriptors) to skip fd lookups.
- [ ] Zero-copy send for large responses.
- [ ] Receive bundles (several buffers per completion).
- [ ] Optional `SQPOLL` mode.
- [ ] Optional NAPI busy polling.
- [ ] Kernel TLS (kTLS) for `giong-tls`.
- [ ] Per-worker statistics (tasks, operations, buffer pool pressure) as a public API.

**Exit criteria:** a documented, reproducible gain for every option that is merged.

## 0.6 — Ecosystem

- [ ] HTTP/2 in `giong-http`.
- [ ] Adapters for the `futures-io` / Tokio I/O traits, so protocol crates that are generic
      over the transport can run on giong.
- [ ] Decide, based on demand, whether a hyper integration is worth maintaining.

**Exit criteria:** an HTTPS server serving real browsers over HTTP/2.

## 1.0

A stable API. Requires: every public item documented, the limitations section of the
README reduced to deliberate non-goals, and at least one production user reporting back.
