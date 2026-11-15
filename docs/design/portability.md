# Portability layer: one driver boundary for Linux, macOS and Windows

Status: prototype on branch `feat/kqueue-prototype`, for 1.2. Nothing here
lands on `main` before `v1.0.0` (the rule that 0.5 to 1.0 adds no
features). Windows (IOCP, then RIO) is designed for here, not built.

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
                 |- Kqueue = Readiness<Kqueue> macOS, readiness-based
                 '- Iocp   (planned)          Windows, completion-based
                          |
 system          sys::*                       src/sys/{linux,macos}.rs
                 wake-up bells, signals, pinning, accept, stat, pread,
                 fsync, fallocate, sendfile, directory listing, random
```

### 2.1 The driver boundary

The boundary is the existing `Driver` enum and its methods, unchanged in
shape:

| Method | Contract |
|---|---|
| `start(&Io, &Waker) -> Start` | Begin a single-shot operation. `Done(res, flags)` if it finished at once (readiness drivers), else `Submitted(key)`. |
| `poll_op(key, &Waker, hold)` | Collect the result; `Pending` until then. |
| `cancel_op(key, data, result_is_fd)` | The future was dropped. `data` (the buffers) stays with the driver until nothing can touch it; a descriptor produced by an `open` nobody collects is closed. |
| `new_multi` / `arm_multi` / `poll_multi` / `drop_multi` | Multishot accept and receive. io_uring gets them from the kernel; readiness drivers "try on poll", reporting data in pool buffers with io_uring's completion flags so the socket code has one path. |
| `park(Park)` / `has_completions` | Enter the kernel once per turn; wake what can progress. |
| `close_fd`, `shutdown`, `take_graveyard`, `inflight`, `tracked_ops`, `pool`, `ensure_recv_buffer`, `zero_copy_min`, `recv_copied`, `evacuate` | As today. |

Every variant is behind `cfg`. On macOS the enum has one variant, so every
`match` compiles to a direct call. On Linux it has the same two variants as
before. Two small macros (`each!`, `split!`) write the `match` once for
every method, so adding a variant touches the enum, `new` and `kind`, not
twenty methods.

Why an enum and not `dyn Driver` or a generic runtime: a worker owns exactly
one driver for its whole life, the enum is already there, `DriverKind` is
`#[non_exhaustive]`, and a generic parameter on `Runtime` would leak into
the public API. A trait object would add an indirect call to every
operation.

### 2.2 The readiness driver is written once

The 1,182-line epoll driver was 90% portable POSIX: the non-blocking
attempts, the per-task budget, the blocking pool for files, the
cancellation and graveyard rules, the multishot emulation, the descriptor
table with generations. Only registration and waiting were epoll's. It is
now `Readiness<P: Poller>` (`src/driver/readiness.rs`, the old `epoll.rs`;
most lines changed, so git shows a new file, and `git log --follow -M30%`
finds the history), with a five-method trait:

```rust
trait Poller {
    fn new() -> io::Result<Self>;
    fn watch(&self, fd, token) -> io::Result<()>;        // a bell, level-triggered
    fn register(&self, fd, token) -> Result<(), i32>;    // read+write, edge-triggered
    fn wait(&mut self, Option<Duration>) -> usize;       // EINTR -> 0
    fn event(&self, i) -> (token, readable, writable);
}
```

`Epoll` (`src/driver/epoll.rs`) is the old code moved verbatim:
`epoll_create1`, `epoll_ctl` with `EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET`,
`epoll_pwait2` with the `epoll_wait` fallback, the same `READABLE` and
`WRITABLE` masks. `Kqueue` (`src/driver/kqueue.rs`) registers
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
| `Bell::new_worker` (worker wake-up) | `eventfd`, blocking (io_uring reads it) | pipe; `EVFILT_USER` later | `PostQueuedCompletionStatus` |
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

The loom models of the wake-up handshakes (`src/notify.rs`) do not change:
the handshake is the same, only the primitive that rings differs.

## 3. Windows: what the boundary already allows, and what it does not

IOCP is completion-based, like io_uring: an operation is submitted with its
buffer, the buffer belongs to the kernel until the completion arrives, and
cancellation (`CancelIoEx`) completes later. That is exactly the contract
`Op` and `cancel_op` already keep, so an `Iocp` variant fits the enum
without a new rule. The readiness driver is not reused for Windows.

What the boundary still assumes, to change when IOCP starts (all
crate-internal, none public):

1. `Io` and `MultiIo` carry `RawFd` and `libc` types (`sockaddr`,
   `msghdr`). Windows needs `RawSocket`/`HANDLE` and `WSAMSG`. Plan: a
   `sys::Raw` alias and a `sys::MsgHdr` type, with `net` building them.
2. The worker's wake-up is a descriptor created before the driver
   (`WorkerShared` exists before its worker starts, and other threads may
   inject work at once). For IOCP the bell *is* the completion port, so
   `Bell::new_worker` must create the port and the driver adopt it. The
   same adoption gives macOS `EVFILT_USER` (the bell owns the kqueue).
3. Multishot accept becomes a loop of `AcceptEx` on sockets created ahead;
   multishot receive becomes a zero-byte `WSARecv` (no pool buffer held by
   an idle connection) followed by a receive into a pool buffer, the same
   "try on poll" the readiness driver does.
4. Unix sockets, file modes and `DirEntry::ino` are left out of a Windows
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
| Ports shared by workers | `SO_REUSEPORT` spreads connections | `SO_REUSEPORT` binds but sends every connection to one socket | **gap**; plan S2: one listening socket shared by the workers |
| Port guard against a second process | a probe socket without `SO_REUSEPORT` (`src/net/share.rs`) | no probe: on macOS it would refuse the process's own workers | **gap**; macOS needs `SO_REUSEPORT` on both sockets, so another process of the user can bind the port too |
| Append-mode writes | `pwrite` appends on `O_APPEND` | `write(2)` when the descriptor has `O_APPEND` | emulated; same visible result |
| UDP truncated datagram | true length reported (`MSG_TRUNC` in) | truncation flagged, true length unknown | macOS limit |
| UDP GSO/GRO, `IP_PKTINFO`, ECN | not in the API yet | not in the API yet | later 1.x, Linux first |
| TLS offload | kTLS | in-process rustls (`Ktls::Auto` falls back; `Required` fails) | kTLS is Linux-only by design |
| CPU pinning | default where allowed | never (`pin_cores(true)` is a no-op) | macOS limit |
| `accept` close-on-exec | atomic (`accept4`) | set after `accept` | same window std has on macOS |
| Peer-closed check in the HTTP client pool | `POLLRDHUP` | the receive poll only | small; a FIN already received is seen |
| `TCP_USER_TIMEOUT` in DB clients | set | keepalive only | macOS has no such option |
| Trust roots, DNS servers | files under `/etc` | `/etc/resolv.conf` only, no keychain | plan S3 |
| Shutdown signals | handler + eventfd | handler + pipe (`EVFILT_SIGNAL` possible) | equivalent |

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

**Linux** (kernel 6.18): `cargo test --release --workspace --all-features`
passes 1062, fails 0, ignores 5, on io_uring and with `GIONG_DRIVER=epoll`.
Clippy is clean for Linux and, cross-checked, for `aarch64-apple-darwin`.

**libkqueue** (section 5): 11 test files pass whole (blocked, cancel,
channels, driver, error_docs, executor, fs, observe, spin, time, udp). The
multi-worker, large-transfer and shutdown tests hang there, as libkqueue's
thread-based emulation does; the macOS runners are the verdict for those.

**macOS 15, Apple Silicon, first run** (commit 9112aa4). Clippy and the
build of every test pass. In the `giong` crate, by binary:

| Binary | Passed | Failed | Cause |
|---|---|---|---|
| unit tests | 36 | 2 | sockaddr family read as Linux's `u16` (fixed); port probe (fixed) |
| balance | 4 | 3 (1 ignored) | port probe refused the workers' own binds (fixed) |
| cancel, channels, driver, error_docs, executor | 60 | 0 | |
| dns | 6 | 11 | 10 from the sockaddr family (fixed); 1 from a test helper whose bound, non-listening port macOS does not refuse (fixed) |
| fs | 21 | 4 | append writes (fixed), `allocate` on a read-only file (fixed), a non-UTF-8 name APFS refuses (test, fixed), a pseudo-terminal read that returns at once (open) |
| multi | 15 | hung 1 | `SO_REUSEPORT` does not spread connections (the gap above, plan S2) |

The run stopped at the job's 60-minute limit inside `multi`, so the later
binaries and crates did not run. The workflow now runs every test binary
under a time limit and names hung tests, so the next run covers the whole
workspace. (`macos-14` is no longer offered by GitHub; the matrix is
`macos-15` and `macos-26`.)

## 8. Next steps (1.2 plan items)

- S2: share one listening socket between workers on macOS; `SO_NOSIGPIPE`
  checked on every socket path; `EVFILT_USER` worker wake-up through bell
  adoption (section 3, point 2).
- S3: trust roots from the keychain, DNS servers from the system
  configuration.
- S4: the rest of the suite's Linux assumptions, as the runners show them.
- Before any of it lands: rebase onto `main` after `v1.0.0`, the full Linux
  gates, and the speed comparison above.
