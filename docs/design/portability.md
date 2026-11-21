# Portability layer: one driver boundary for Linux, macOS and Windows

Status: the layer of this document (sections 1 to 3) is branch
`feat/portability-layer`: Linux only, no new driver, no public API change.
The macOS kqueue driver (`feat/macos-kqueue`) and the Windows IOCP driver
(`feat/iocp-driver`) are stacked on it. Nothing here lands on `main` before
`v1.0.0`.

## 1. What must hold

1. **The Linux hot path does not change.** io_uring and epoll make the same
   system calls, in the same order, with the same arguments, through `libc`
   (the simulated kernel in `sim/` interposes those symbols). No trait
   objects, no extra indirection, no new branch per operation.
2. **One public API.** A program written against giong on Linux builds and
   runs unchanged on macOS. Where a platform cannot do what Linux does, the
   API still behaves the same and the difference is a documented cost
   (speed, or a guarantee that is weaker), never a different result.
3. **IOCP fits later without redesign.** The boundary must already be one
   that a completion-based Windows driver can sit behind.
4. **Each OS-specific call is written once**, in one place, next to the
   reason it differs.

## 2. The layers

```text
 public API      net::TcpStream, fs::File, time, sync, Builder ...
                 (no OS-specific calls; they build an Io and await an Op)
                          |
 operation       Op<T> + Io / MultiIo         src/op.rs, src/driver/mod.rs
                 (the buffer moves into the op; dropping the op hands the
                  buffer to the driver until the kernel lets go of it)
                          |
 driver          enum Driver                  src/driver/mod.rs
                 |- Uring                     Linux, completion-based
                 |- Epoll  = Readiness<Epoll> Linux, readiness-based
                 |- Kqueue = Readiness<Kqueue> macOS, readiness-based (stacked branch)
                 '- Iocp                      Windows, completion-based (stacked branch)
                          |
 system          sys::*                       src/sys/{linux,macos}.rs
                 wake-up bells, signals, pinning, accept, stat, pread,
                 fsync, fallocate, sendfile, directory listing, random,
                 batched datagram calls
```

**The `cfg` rule.** An item only Unix has is `cfg(unix)`; an item only one
system has names that system (`cfg(target_os = "linux")`,
`cfg(target_os = "macos")`). No implementation is `cfg(not(windows))` or
`cfg(not(target_os = "linux"))`: a new system must fail to build where it has
no answer yet, not inherit another system's. The dependencies follow the same
rule: `libc` and socket2's `all` feature are `cfg(unix)` dependencies,
`io-uring` is a `cfg(target_os = "linux")` dependency.

### 2.1 The driver boundary

The boundary is the existing `Driver` enum and its methods, unchanged in
shape:

| Method | Contract |
|---|---|
| `start(&Io, &Waker) -> Start` | Begin a single-shot operation. `Done(res, flags)` if it finished at once (readiness drivers), else `Submitted(key)`. |
| `poll_op(key, &Waker, hold)` | Collect the result; `Pending` until then. |
| `cancel_op(key, data, result_is_fd, finish)` | The future was dropped. `data` (the buffers) stays with the driver until nothing can touch it; a descriptor produced by an `open` nobody collects is closed. |
| `new_multi` / `arm_multi` / `poll_multi` / `drop_multi` | Multishot accept and receive. io_uring gets them from the kernel; readiness drivers "try on poll", reporting data in pool buffers with io_uring's completion flags so the socket code has one path. |
| `park(Park)` / `has_completions` | Enter the kernel once per turn; wake what can progress. |
| `waker() -> WakeHandle` | The handle other threads wake this worker with (below). |
| `close_fd`, `shutdown`, `take_graveyard`, `inflight`, `tracked_ops`, `pool`, `ensure_recv_buffer`, `zero_copy_min`, `recv_copied`, `evacuate` | As today. |

Every variant is behind `cfg`. Where a system has one variant, every
`match` compiles to a direct call. On Linux the enum has the same two
variants as before. Two small macros (`each!`, `split!`) write the `match` once for
every method, so adding a variant touches the enum, `new` and `kind`, not
twenty methods.

Why an enum and not `dyn Driver` or a generic runtime: a worker owns exactly
one driver for its whole life, the enum is already there, `DriverKind` is
`#[non_exhaustive]`, and a generic parameter on `Runtime` would leak into
the public API. A trait object would add an indirect call to every
operation.

**The wake-up belongs to the driver.** Each driver makes its worker's
wake-up when it is created (the readiness driver asks its `Poller` for
it); `Driver::waker()` hands out a `WakeHandle`, an enum with one variant
per mechanism. The contract, which every variant keeps:

- it is `Send + Sync + Clone`, and `ring()` may be called from any thread;
- `ring()` never blocks and reports no error;
- a ring that arrives while the worker is awake is not lost: the next
  `park` returns at once;
- a handle may outlive its driver: ringing it then does nothing, and never
  touches a descriptor or handle that was closed or reused.

On Linux the only variant is an `Arc` around the same `eventfd` as before
(io_uring reads it, epoll watches it and drains it), so the system calls of
a wake-up did not change. kqueue adds a variant that triggers
`EVFILT_USER` and IOCP one that calls `PostQueuedCompletionStatus`,
without touching the runtime. The worker's
shared state holds the handle in a `OnceLock`, set on the worker's thread
as soon as the driver exists. Another thread rings it only when the
doorbell says the worker sleeps, and a worker cannot sleep before it has a
driver, so no wake-up can fall between the two.

### 2.2 The readiness driver is written once

The epoll driver was 90% portable POSIX: the non-blocking
attempts, the per-task budget, the blocking pool for files, the
cancellation and graveyard rules, the multishot emulation, the descriptor
table with generations, the batched datagram receive, and the rule that a
busy datagram socket leaves the poller's set. Only registration and waiting were epoll's. It is
now `Readiness<P: Poller>` (`src/driver/readiness.rs`, the old `epoll.rs`;
most lines changed, so git shows a new file, and `git log --follow -M30%`
finds the history), with an eight-method trait:

```rust
trait Poller {
    fn new() -> io::Result<Self>;
    fn waker(&self) -> io::Result<WakeHandle>;           // rings report token WAKE
    fn watch(&self, fd, token) -> io::Result<()>;        // a bell, level-triggered
    fn register(&self, fd, token) -> Result<(), i32>;    // read+write, edge-triggered
    fn rearm(&self, fd, token) -> i32;                   // back into the set; -errno
    fn unregister(&self, fd) -> bool;                    // out of the set, still open
    fn wait(&mut self, Option<Duration>) -> usize;       // EINTR -> 0
    fn event(&self, i) -> (token, readable, writable);
}
```

`Epoll` (`src/driver/epoll.rs`) is the old code moved verbatim:
`epoll_create1`, `epoll_ctl` with `EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET`,
`epoll_pwait2` with the `epoll_wait` fallback, the same `READABLE` and
`WRITABLE` masks, `EPOLL_CTL_DEL` and `EPOLL_CTL_ADD` for a busy datagram
socket. `Kqueue` (`src/driver/kqueue.rs`, on the stacked branch) registers
`EVFILT_READ` and `EVFILT_WRITE` with `EV_CLEAR` (kqueue's edge-triggered
mode) in one `kevent` call with `EV_RECEIPT`, so a descriptor that refuses
the write filter (a pipe's read end) still registers for reading. The
parameter is monomorphised: `Readiness<Epoll>` is the same machine code it
was as `Epoll`, which the Linux suite and the speed gate are there to
confirm (section 6).

### 2.3 The `sys` module

Everything outside the drivers that differs per OS, one function each:

| `sys::` | Linux (unchanged calls) | macOS | Windows (plan) |
|---|---|---|---|
| `Bell` (blocking pool, signals) | `eventfd` | non-blocking pipe | completion packet to the port |
| A worker's wake-up (behind `WakeHandle`) | `Bell::new_worker`: an `eventfd`, blocking (io_uring reads it) | `kqueue::Wake`: an `EVFILT_USER` event of the worker's own queue, no descriptor | `PostQueuedCompletionStatus` |
| `ring` / `drain` | 8-byte write / read | 1-byte write / read until empty | - |
| `errno_location` | `__errno_location` | `__error` | `GetLastError` |
| `allowed_cpus`, `pin_to_cpu`, `CAN_PIN` | `sched_{get,set}affinity` | all CPUs; no pinning | processor groups |
| `accept` | `accept4(SOCK_CLOEXEC\|SOCK_NONBLOCK)` | `accept` + `FD_CLOEXEC` + `FIONBIO` | `AcceptEx` |
| `MSG_NOSIGNAL` | `MSG_NOSIGNAL` | 0 (`SO_NOSIGPIPE`, set by socket2) | n/a |
| `pread` / `pwrite` | `pread64` / `pwrite64` | `pread` / `pwrite` | overlapped `ReadFile` |
| `StatBuf`, `stat_at`, `stat_fields` | `statx` (io_uring fills the same struct) | `fstatat`, `fstat` for an empty path | `GetFileInformationByHandleEx` |
| `fsync` | `fsync` / `fdatasync` | `F_FULLFSYNC` (as std) | `FlushFileBuffers` |
| `fallocate` | `fallocate64(mode 0)` | `F_PREALLOCATE` + `ftruncate` | `SetFileInformationByHandle` |
| `sendfile` | `sendfile64` | `sendfile` (file first, length in/out) | `TransmitFile` |
| `read_dir_batch` | `getdents64` | `readdir` re-encoded as `linux_dirent64` | `FindFirstFileEx` |
| `random_bytes` | `getrandom` | `arc4random_buf` | `ProcessPrng` |
| `fd_mode`, `mode::S_IF*` | `fstat64`; `mode_t` is `u32` | `fstat`; `mode_t` is `u16` | - |
| `pread_cached` | `preadv2(RWF_NOWAIT)` | not available (`-1`: the blocking pool reads) | - |
| `RECV_REAL_LEN`, `RECVFROM_REPORTS_TRUNCATION` | `MSG_TRUNC` as an input flag; `true` | 0 (BSD copies input flags into `msg_flags`, so the flag would mark every datagram truncated); `false`, so `recvmsg` is used | `WSARecvMsg` |
| `MMsgHdr`, `recvmmsg`, `sendmmsg` | `mmsghdr`, `recvmmsg`, `sendmmsg` | the same struct, a loop of `recvmsg` / `sendmsg` | a loop |
| `POLL_PEER_CLOSED` | `POLLRDHUP` | `POLLHUP` | - |

The loom models of the wake-up handshakes (`src/notify.rs`) do not change:
the handshake is the same, only the primitive that rings differs.

## 3. Windows: what the boundary already allows, and what it does not

IOCP is completion-based, like io_uring: an operation is submitted with its
buffer, the buffer belongs to the kernel until the completion arrives, and
cancellation (`CancelIoEx`) completes later. That is exactly the contract
`Op` and `cancel_op` already keep, so an `Iocp` variant fits the enum
without a new rule. The readiness driver is not reused for Windows.

What the boundary still assumes, and what was agreed with the IOCP work
(all crate-internal, none public):

1. `Io` and `MultiIo` carry `RawFd` and `libc` types (`sockaddr`,
   `msghdr`). Windows needs `RawSocket`/`HANDLE` and `WSAMSG`. They become
   `sys::RawSock`, `sys::RawFile`, `sys::SockAddrStorage` and `sys::MsgHdr`
   (aliases of today's types on Unix), in this layer; the public
   `AsRawFd` implementations stay `cfg(unix)`.
2. The worker's wake-up: done, `Driver::waker()` (section 2.1). For IOCP
   the handle posts to the completion port.
3. A listening socket shared by workers must allow both "every worker
   accepts on the same socket" (macOS) and "one acceptor hands the accepted
   sockets out" (Windows: a handle belongs to one completion port). So
   `net` must not assume that a worker accepts on a socket it bound; the
   choice is made per system in `src/net/share.rs`.
4. Error codes: macOS-specific codes continue from `E0028`; Windows has
   `E0060` to `E0079`.
5. Multishot accept becomes a loop of `AcceptEx` on sockets created ahead;
   multishot receive becomes a zero-byte `WSARecv` (no pool buffer held by
   an idle connection) followed by a receive into a pool buffer, the same
   "try on poll" the readiness driver does.
6. Unix sockets, file modes and `DirEntry::ino` are left out of a Windows
   build, as the 1.0 API freeze decided.

## 4. macOS: what passes, and the real semantic gaps

macOS is a development platform: correct, not tuned, no speed claim.
Differences that remain, by area (each is either a later item of the 1.2
plan or a limit of macOS itself):

| Area | Linux | macOS prototype | Kind |
|---|---|---|---|
| Files | io_uring, or blocking pool on epoll | blocking pool (as epoll) | macOS has no usable async file I/O |
| `sync_all`/`sync_data` | `fsync`/`fdatasync` | `F_FULLFSYNC` for both | slower but durable; macOS has no `fdatasync` |
| `allocate` | `fallocate` | `F_PREALLOCATE` + `ftruncate` | emulated; same visible result |
| TCP port shared by workers | `SO_REUSEPORT` spreads connections by a hash of their addresses | one listening socket, duplicated for each worker (`src/net/share.rs`): whichever worker accepts first gets the connection | different spread, same API; `SO_REUSEPORT` on macOS would send every connection to one socket |
| UDP port shared by workers | `SO_REUSEPORT` spreads datagrams by sender | one socket bound without `SO_REUSEPORT`, duplicated for each worker (`src/net/share.rs`); each datagram goes to the worker that reads first | macOS differs: no spreading by sender, and `connect` on one worker's socket connects them all |
| Port guard against a second process | the first socket of a port binds without `SO_REUSEPORT` (`src/net/share.rs`) | the shared socket never has `SO_REUSEPORT`, so nothing else can bind the address while it lives (with `SO_REUSEPORT` on a wildcard socket, XNU would let any user's socket join and take the traffic) | stronger than Linux, for TCP and UDP |
| Largest UDP datagram | 65507 bytes | 65507 bytes: giong raises the send buffer of the sockets it binds to 65,535 bytes, where the system's default (`net.inet.udp.maxdgram`) refuses datagrams over 9216 | equivalent; a `from_std` socket keeps what it was given |
| Listen backlog | `net.core.somaxconn` (4096) | `kern.ipc.somaxconn` (128) | macOS limit; bursts of connections are refused sooner |
| A send to a full socket | `MSG_DONTWAIT` on every send | every stream is non-blocking from the start (`StreamSocket::new`): macOS waits in a send even with `MSG_DONTWAIT`, which only a receive honours there | equivalent |
| `sendfile` to a closed peer | `EPIPE` | `ENOTCONN` from the kernel, reported as `EPIPE` | equivalent |
| Reads and writes on a terminal | `pread` fails with `ESPIPE`, then `read` | `pread` fails with `ENXIO`, then `read` | equivalent |
| Memory left (`Server::min_free_memory` in giong-http) | the cgroup's limit and `MemAvailable` | free and inactive pages from `host_statistics64`; no cgroups | equivalent for the machine; no per-container limit exists on macOS |
| Append-mode writes | `pwrite` appends on `O_APPEND` | `write(2)` when the descriptor has `O_APPEND` | emulated; same visible result |
| UDP truncated datagram | true length reported (`MSG_TRUNC` in) | truncation flagged (`recvmsg`'s `msg_flags`), true length unknown | macOS limit |
| UDP GSO/GRO, `IP_PKTINFO`, ECN | not in the API yet | not in the API yet | later 1.x, Linux first |
| TLS offload | kTLS | in-process rustls (`Ktls::Auto` falls back; `Required` fails) | kTLS is Linux-only by design |
| `SIGPIPE` on a send to a closed peer | `MSG_NOSIGNAL` on every send | `SO_NOSIGPIPE` on every socket: set by socket2 on the ones giong creates, inherited by accepted ones, set by giong on every listener and `from_std` socket | equivalent |
| CPU pinning | default where allowed | never (`pin_cores(true)` is a no-op) | macOS limit |
| `accept` close-on-exec | atomic (`accept4`) | set after `accept` | same window std has on macOS |
| Peer-closed check in the HTTP client pool | `POLLRDHUP` | the receive poll only | small; a FIN already received is seen |
| `TCP_USER_TIMEOUT` in DB clients | set | keepalive only | macOS has no such option |
| Trust roots (`TlsConnector::with_system_roots`) | bundle files under `/etc` | the keychain's trust settings (`giong-tls/src/keychain.rs`): Apple's roots and the ones the administrator or user added, without the ones turned off; `/etc/ssl/cert.pem` if the keychain cannot be read | equivalent; settings for one host name or application are passed over |
| DNS servers | `/etc/resolv.conf` | `/etc/resolv.conf`, which macOS writes from its network configuration (the primary resolver) | resolvers for some domains only (`/etc/resolver`, VPN domains) are not consulted |
| Shutdown signals | handler + eventfd | handler + pipe | equivalent; `EVFILT_SIGNAL` is not used (below) |

**Why not `EVFILT_SIGNAL`.** kqueue can report a signal, but it only
observes it: the signal's default action still happens, so `SIGINT` would
still end the process unless a handler is installed as well. With the
handler there anyway, the handler ringing a `Bell` is the whole mechanism,
and it is the same code on Linux and macOS (`sys/unix.rs`). One mechanism
on every Unix beats a second one that saves nothing.

**The wake-up on macOS** is an `EVFILT_USER` event (`EV_CLEAR`) registered
on the worker's kqueue; `WakeHandle::ring` triggers it with `NOTE_TRIGGER`
from any thread. A trigger while the worker is awake stays pending until
its next `kevent`, any number of triggers collapse into one event, and
there is nothing to drain, so it keeps the `WakeHandle` contract of
section 2.1 with one system call per ring and none per wake.

Tests that cannot run on macOS are marked, not deleted:
`tests/blocked.rs` (seccomp, capabilities, io_uring) is `cfg(linux)`;
two tests that send a Linux `SO_TIMESTAMPING` control message are
`cfg(linux)`; one UDP test that finds sockets by inode in `/proc` is
ignored elsewhere. Test helpers that read `/proc/self/fd` use `/dev/fd`
and `F_GETPATH` on macOS. Tests that named `DriverKind::Epoll` use the
platform's readiness driver.

### 4.1 Results

See section 7.

## 5. How the kqueue code is tested without a Mac

`--cfg giong_libkqueue` builds the kqueue poller on Linux against
libkqueue (a kqueue written over epoll, `apt install libkqueue-dev`), as
`DriverKind::Kqueue` beside io_uring and epoll:

```sh
RUSTFLAGS="--cfg giong_libkqueue" GIONG_DRIVER=kqueue \
  CARGO_TARGET_DIR=target/kq cargo test -p giong --all-features
```

It is a test aid for the poller's logic only: libkqueue's own edge cases
are not macOS's. The real test is the `macos-15` and `macos-26` runners
(`.github/workflows/macos.yml`, on the branch only).

Cross-checking the whole workspace for macOS from Linux also works for
`cargo check` and `cargo clippy` (no linking), given a C compiler for
`ring`'s build script:
`rustup target add aarch64-apple-darwin`, then
`cargo clippy --target aarch64-apple-darwin --workspace --all-targets --all-features`.

## 6. Proving Linux did not change

- The full Linux suite on both drivers (`cargo test`, `GIONG_DRIVER=epoll`),
  TSan, Miri, loom, MSRV 1.80 and clippy, as on `main`.
- The simulation driver's tests (`sim/`) run the readiness driver through
  the same `libc` symbols.
- Speed: the instruction-count gate (when it lands on `main`) on both
  drivers; before 1.2 merges, the bare-metal comparison of 1.0.0 against
  this branch on the same machines.

## 7. Results

**Linux, this layer** (kernel 6.18, on `main` 568d475):
`cargo test --release --workspace --all-features` passes 1187, fails 0,
ignores 5, on io_uring and with `GIONG_DRIVER=epoll`. Clippy is clean for
Linux and, cross-checked, for `aarch64-apple-darwin`; the libraries build
on Rust 1.80.

**libkqueue** (section 5): 11 test files pass whole (blocked, cancel,
channels, driver, error_docs, executor, fs, observe, spin, time, udp). The
multi-worker, large-transfer and shutdown tests hang there, as libkqueue's
thread-based emulation does; the macOS runners are the verdict for those.

**macOS, Apple Silicon runners** (`macos-15` and `macos-26`, every test
binary of the workspace under a time limit, draft PR #173):

| Commit | | Passed | Failed | Ignored | Hung (> 60 s) |
|---|---|---|---|---|---|
| 78da16d | macOS 15 | 905 | 19 | 5 | 4 |
| | macOS 26 | 903 | 20 | 5 | 5 |
| 45f7587 | macOS 15 | 1033 | 8 | 5 | 0 |
| | macOS 26 | 1032 | 9 | 5 | 0 |
| 8d00308 | macOS 15 | 1042 | 0 | 5 | 0 |
| | macOS 26 | 1042 | 0 | 5 | 0 |

What the runs found, by cause:

| Cause | Seen as | Fix |
|---|---|---|
| A send to a full socket waits despite `MSG_DONTWAIT` | giong-compat's direct send held the worker on a Unix socket pair: a hang | streams are non-blocking from the start |
| The keychain reader released a null reference when a trust settings domain was empty | `SIGTRAP` in every test that loaded the system's roots | the null check no longer builds an owner for null |
| UDP datagrams above 9216 bytes refused | two udp tests | the send buffer is raised at bind |
| `sendfile` to a closed peer gives `ENOTCONN` | one net test | reported as `EPIPE` |
| `pread` on a terminal gives `ENXIO` | the pseudo-terminal test in fs | falls back to `read`, as for `ESPIPE` |
| giong-http had no source for the memory left | three tests of `min_free_memory` | `host_statistics64` |
| The tests' port holder kept the shared listening socket from binding | balance, multi, one net test | no holder on macOS |
| Tests that relied on Linux's socket buffer sizes (a megabyte written before its reader starts) | hangs in unix and the kTLS exchange | the tests write and read together |
| Listen backlog of 128 | unix `many_connections_at_once`, net `accept_backlog_is_bounded_and_no_connection_is_lost` | fewer connections on macOS |
| A SYN to a bound socket that does not listen is dropped, not refused | giong-http client timeout test | the port is given back first |
| Tests that named epoll or read `/proc` | cancel, proc_readers, one giong-http unit test | the platform's readiness driver; `cfg(linux)` for `/proc` |
| Elapsed-time bounds that assumed an exact sleep | giong-http shutdown tests | the time really slept is counted |

**The wake-up.** `src/notify.rs` and its loom models are above
`WakeHandle` and do not change with the mechanism behind it. What is new
on macOS is the kernel's part (`EVFILT_USER`), which the lost wake-up race
of `tests/wake.rs` exercises on the runners (300,000 cross-thread wake-ups
with and without spinning), and which ThreadSanitizer checks there over
the tests of the `giong` crate (`.github/workflows/macos.yml`).

## 8. Next steps

- S1: the kqueue driver on this layer, with `EVFILT_USER` behind
  `WakeHandle` (done; shutdown signals stay on the handler, section 4).
- S2: one listening socket shared by the workers on macOS, the port
  guard, `SO_NOSIGPIPE` on every socket path (done, section 4; the shared
  socket is one function in `src/net/share.rs`, where Windows can put an
  acceptor of its own).
- S3: trust roots from the keychain; DNS servers from the file macOS
  writes from its configuration, checked against `scutil --dns` on the
  runners (done, section 4).
- S4: the rest of the suite's Linux assumptions, as the runners showed
  them (done, section 7: every test passes on macOS 15 and 26).
- Before any of it lands: `v1.0.0` tagged, the full Linux gates, and the
  speed comparison above.
