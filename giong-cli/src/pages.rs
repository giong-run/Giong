//! The error pages of docs/errors/, embedded so `giong explain` works offline.
//! Generated: `GIONG_CLI_REGENERATE=1 cargo test -p giong-cli pages` rewrites it,
//! and that test fails when it is out of date.

/// (number, page) for every page, in order.
pub const PAGES: &[(u16, &str)] = &[
    (1, r#"# GIONG-E0001: io_uring is not available

Raised by the runtime, when a worker starts. In code: `giong::error::Code::IoUringUnavailable`.

## What happened

The runtime was told to use io_uring (`Builder::driver(DriverKind::IoUring)`, or `GIONG_DRIVER=io_uring`), and `io_uring_setup` failed with `EPERM`, `EACCES` or `ENOSYS`, or with `EINVAL` for a ring Linux sets up from 5.6 on. The kernel has io_uring but this process may not use it, the kernel (or a sandbox in front of it) does not have it at all, or the sandbox has only part of it (gVisor with `runsc --iouring` takes no setup flags). Without that instruction giong would have used its epoll driver instead. The error keeps the kernel's `io::ErrorKind` (`PermissionDenied`, `Unsupported` or `InvalidInput`), and the kernel's error is its `source()`.

## How to fix it

- If epoll is good enough here, stop asking for io_uring: remove `.driver(DriverKind::IoUring)` and unset `GIONG_DRIVER`, and giong falls back to epoll by itself.
- In Docker, the default seccomp profile blocks io_uring. Run with a profile that allows `io_uring_setup`, `io_uring_enter` and `io_uring_register` (see [DEPLOYMENT.md](../DEPLOYMENT.md)), or `--security-opt seccomp=unconfined` to confirm that this is the cause.
- In Kubernetes, the container runtime applies the same default profile: set a `seccompProfile` of type `Localhost` that allows the three calls, as [DEPLOYMENT.md](../DEPLOYMENT.md) shows.
- On the host, `sysctl kernel.io_uring_disabled` must be `0`, or `1` with the process in the group named by `kernel.io_uring_group`.
- gVisor and some other sandboxes do not offer io_uring by default, and gVisor's optional one (`runsc --iouring`) is too small for giong; run giong outside them, or on its epoll driver.

[All error codes](README.md)
"#),
    (2, r#"# GIONG-E0002: the kernel is too old

Raised by the runtime, when a worker starts. In code: `giong::error::Code::KernelTooOld`.

## What happened

The runtime was told to use io_uring (`Builder::driver(DriverKind::IoUring)`, or `GIONG_DRIVER=io_uring`), and the kernel is missing an io_uring feature giong needs: registering a ring of receive buffers (`IORING_REGISTER_PBUF_RING`) failed. It arrived in Linux 5.19 and works reliably from 6.0, the oldest kernel giong's io_uring driver supports. Without that instruction giong would have used its epoll driver, which works on older kernels.

## How to fix it

- If epoll is good enough here, stop asking for io_uring: remove `.driver(DriverKind::IoUring)` and unset `GIONG_DRIVER`.
- Run on Linux 6.0 or newer; 6.1 or newer is faster (giong then uses `IORING_SETUP_DEFER_TASKRUN`). `uname -r` shows the version.
- On a managed platform, pick a node image with a newer kernel.

[All error codes](README.md)
"#),
    (3, r#"# GIONG-E0003: another runtime handles the signals

Raised by the runtime, when it starts. In code: `giong::error::Code::SignalsTaken`.

## What happened

The runtime was built with `Builder::shutdown_on_signals(true)`, but another giong runtime in this process already handles `SIGINT` and `SIGTERM`. Signal handlers belong to the whole process, so only one runtime at a time can own them.

## How to fix it

- Turn on `shutdown_on_signals` for one runtime only, and stop the others through their `Handle::shutdown`.
- Or run one runtime at a time: the handlers are released when that runtime's `run` returns.

[All error codes](README.md)
"#),
    (4, r#"# GIONG-E0004: invalid runtime setting

Raised by the runtime, when it starts. In code: `giong::error::Code::InvalidConfig`.

## What happened

A runtime setting is outside the range giong accepts, such as the number or the size of the receive buffers, or the `GIONG_DRIVER` environment variable names no driver.

## How to fix it

- Read the message: it names the setting, the range and the value given.
- `Builder::recv_buffers(count, size)` takes up to 32768 buffers, rounded up to a power of two, and a size from 64 bytes to 4 GiB.
- `GIONG_DRIVER` takes `io_uring` or `epoll` on Linux, `kqueue` on macOS and `iocp` on Windows; unset or empty (or `auto`) means io_uring where the kernel allows it and epoll elsewhere on Linux, kqueue on macOS and iocp on Windows. It is read only when the code does not call `Builder::driver`. A driver the operating system does not have is refused with this code too (io_uring with `GIONG-E0001`).

[All error codes](README.md)
"#),
    (5, r#"# GIONG-E0005: unexpected result from the kernel

Raised by the runtime's I/O operations, and as a panic that stops the worker when its event loop gets an error it cannot go on after (from `io_uring_enter` or `epoll_wait`). In code: `giong::error::Code::Unexpected`.

## What happened

The kernel returned something giong does not expect, such as received data without a buffer or a datagram from an unknown address family. It points to a bug in giong or in the kernel, with one exception: a panic naming `EBADR` or `ENOMEM` means the kernel ran out of memory and dropped completions, after which the worker cannot know which operations finished.

## How to fix it

- If the message names `EBADR` or `ENOMEM`: give the process more memory, or run fewer connections per worker.
- Otherwise, please report it at https://github.com/giong-run/Giong/issues with the message, your kernel version (`uname -r`) and, if you can, a way to reproduce it.
- Running the same program on another kernel version helps tell the two apart.

[All error codes](README.md)
"#),
    (6, r#"# GIONG-E0006: read used after recv

Raised by `TcpStream`, `UnixStream` and `UdpSocket`. In code: `giong::error::Code::MixedReads`.

## What happened

A one-off read (`read`, `read_from`) was used on a stream or socket that already has a multishot receive (`recv`, `recv_from`) running. The kernel keeps delivering data into the multishot receive, so a one-off read would see the data out of order.

On a `UnixStream`, `recv_with_fds` counts as a one-off read: it cannot follow `recv`, and once it has been used, `recv` is refused, because a multishot receive would let the kernel close descriptors that arrive.

## How to fix it

- Use one way of reading per stream or socket: `recv`/`recv_from` (buffers from the runtime's pool, best for many connections) or `read`/`read_from` (your own buffer).

[All error codes](README.md)
"#),
    (7, r#"# GIONG-E0007: the time limit passed

Raised by `giong::time::timeout`, when its error becomes an `io::Error`. In code: `giong::error::Code::TimedOut`.

## What happened

The future given to `giong::time::timeout` did not finish within its time limit, and the `Elapsed` error was turned into an `io::Error` (by `?` in a function returning `io::Result`). The future was dropped, which cancels its I/O.

## How to fix it

- Give it more time if the limit is too tight for the work.
- Otherwise find out why the operation is slow: the peer, the network, or a task that keeps the worker busy (one worker runs every task on its core).

[All error codes](README.md)
"#),
    (8, r#"# GIONG-E0008: no such worker

Raised by `giong::Handle::spawn_on`, as a panic. In code: `giong::error::Code::NoSuchWorker`.

## What happened

`spawn_on` was given a worker index that the runtime does not have. Workers are numbered from 0 to `Handle::workers() - 1`. This is a bug in the calling code, so `spawn_on` panics instead of returning an error.

## How to fix it

- Pass an index below `Handle::workers()`. To spread work over every worker, use `i % handle.workers()`.
- `Builder::workers` sets the count; without it, the runtime starts one worker for each CPU the process may run on, which differs between machines and containers.

[All error codes](README.md)
"#),
    (9, r#"# GIONG-E0009: the kernel refused io_uring buffer rings

Raised by the runtime, when a worker starts. In code: `giong::error::Code::BufferRingRefused`.

## What happened

The runtime was told to use io_uring (`Builder::driver(DriverKind::IoUring)`, or `GIONG_DRIVER=io_uring`), the kernel is 6.0 or newer, and it would not register giong's ring of receive buffers (`IORING_REGISTER_PBUF_RING`). Such a kernel should accept it, and giong already works around the kernel bugs it knows here, such as the inverted check in Ubuntu's 6.8.0-135 and later kernels (`giong::KernelWorkaround`). The message names the kernel release, the kernel's error is the `source()`, and its `io::ErrorKind` is kept. Without that instruction giong would have used its epoll driver.

## How to fix it

- If epoll is good enough here, stop asking for io_uring: remove `.driver(DriverKind::IoUring)` and unset `GIONG_DRIVER`. `giong::driver_choice()` then reports this same error as the reason giong chose epoll.
- `ENOMEM` means the memory for the ring could not be had: check the service's memory cgroup limit, or give it fewer receive buffers (`Builder::recv_buffers`). When the locked-memory limit is the cause (Linux 6.14 to 7.2 count buffer rings against it), giong raises [GIONG-E0010](E0010.md) instead.
- `EINVAL` on a 6.x kernel is most likely a kernel bug giong does not know yet. Please report it with the output of `uname -r` at <https://github.com/giong-run/Giong/issues>; a newer kernel of the same distribution may already have it fixed.

[All error codes](README.md)
"#),
    (10, r#"# GIONG-E0010: the locked-memory limit is used up

Raised by the runtime, when a worker starts. In code: `giong::error::Code::LockedMemoryLimit`.

## What happened

Setting up the worker's io_uring failed with `ENOMEM`, and the process is held to a locked-memory limit (`RLIMIT_MEMLOCK`, `ulimit -l`): it is not unlimited, and the process lacks `CAP_IPC_LOCK`, which would lift it. Linux 6.14 to 7.2 count the memory of every io_uring ring against that limit: the submission and completion queues, and each buffer ring (7.3 stopped, and so did 6.18 from 6.18.58). The count is kept per user ID, summed over all of that user's processes, and the kernel gives the memory of a closed ring back a moment after the close, not at once. A giong worker locks about 150 KiB with the default settings, so a limit of 8 MiB, a common default, holds about fifty workers, and one of 64 KiB, the default on older systems, holds none. The message names the limit and what the memory was for ("rings" or "buffer ring"); the kernel's error is the `source()`, and its `io::ErrorKind` (`OutOfMemory`) is kept.

Without `Builder::driver(DriverKind::IoUring)` or `GIONG_DRIVER=io_uring`, giong runs such a worker on its epoll driver instead, says so once on stderr, and `giong::driver_choice()` reports this error as the reason. Workers that got their rings before the limit was used up stay on io_uring.

## How to fix it

- Raise the limit for the service: `LimitMEMLOCK=infinity` in its systemd unit, `--ulimit memlock=-1` with `docker run` (in Docker Compose, `memlock` under `ulimits` with `soft: -1` and `hard: -1`), or `ulimit -l unlimited` in the shell that starts it. [DEPLOYMENT.md](../DEPLOYMENT.md#locked-memory-limit) has the details, Kubernetes included.
- Or give the process `CAP_IPC_LOCK`, which lifts the limit (`AmbientCapabilities=CAP_IPC_LOCK` in a systemd unit, `--cap-add IPC_LOCK` with `docker run`).
- Or need less: fewer workers (`Builder::workers`), smaller rings (`Builder::ring_entries`), fewer receive buffers (`Builder::recv_buffers`). Other processes of the same user ID count too, other containers running as that ID on the same machine included.
- Or run a kernel that does not count io_uring's rings: Linux 7.3 or newer, or 6.18.58 or newer in the 6.18 series.
- If epoll is good enough here, stop asking for io_uring: remove `.driver(DriverKind::IoUring)` and unset `GIONG_DRIVER`.

[All error codes](README.md)
"#),
    (11, r#"# GIONG-E0011: not inside a giong runtime

Raised by everything that needs a giong worker, as a panic: `giong::spawn`, `giong::time::sleep` and `timeout` when polled, `giong::handle`, `giong::worker_id`, `TcpStream::connect`, `TcpListener::bind`, the file operations, and the rest of the I/O API. In code: `giong::error::Code::NotInRuntime`.

## What happened

The call ran on a thread where no giong runtime runs: a plain thread, a thread of another runtime such as Tokio's, or a `Drop` that ran after the runtime had stopped. giong's I/O and timers live on the worker thread that uses them (each worker has its own io_uring or epoll instance), so they cannot work without one. This is a bug in the calling code, so giong panics instead of returning an error.

## How to fix it

- Start a runtime and call the function from the futures it runs: `giong::block_on(async { ... })` for one thread, or `giong::Builder::new().build()?.run(|worker| async move { ... })` for one worker per CPU.
- Do not await giong futures on another executor (`tokio::spawn`, `futures::executor::block_on`, an async test macro other than giong's). To use giong's sockets with crates written for Tokio's or futures' I/O traits, wrap them with `giong-compat` inside a giong runtime; it adapts the I/O traits, not the runtime.
- From another thread, send work to the runtime with a `giong::Handle` (from `Runtime::handle` or `giong::handle()`): `Handle::spawn_on` or `Handle::spawn_balanced`, whose handles can be awaited from any thread.
- Library code that may run inside or outside giong can check `giong::try_worker_id()`, which returns `None` outside a runtime.

[All error codes](README.md)
"#),
    (12, r#"# GIONG-E0012: a runtime already runs on this thread

Raised by `giong::block_on` and `giong::Builder::block_on`, as a panic. In code: `giong::error::Code::RuntimeAlreadyRunning`.

## What happened

`block_on` was called on a thread where a giong runtime is already running, usually from inside a task or a main future. One thread runs at most one giong worker, and a nested `block_on` would stop every other task of that worker until it returned. This is a bug in the calling code, so giong panics instead of returning an error.

## How to fix it

- Inside a giong task, `.await` the future instead of calling `block_on` on it.
- To run work next to the current task, use `giong::spawn`; to run it on another worker, `Handle::spawn_on` or `Handle::spawn_balanced`, and await the handle they return.
- If a separate runtime is really wanted (for example a test that starts its own), start it on a thread of its own: `std::thread::spawn(|| giong::block_on(...))`.

[All error codes](README.md)
"#),
    (13, r#"# GIONG-E0013: an operation was polled on another worker

Raised by giong's I/O futures, as a panic. In code: `giong::error::Code::WrongWorker`.

## What happened

A future that had started an I/O operation (a read, a write, a connect, a file operation) on one giong worker was polled again on a different worker or runtime. The operation lives in the first worker's io_uring or epoll instance, and only that worker can collect its result. giong's futures are not `Send`, so this takes unusual code: a future kept from one `block_on` and polled in the next one, or a future moved between threads with `unsafe`. This is a bug in the calling code, so giong panics instead of returning an error.

## How to fix it

- Await a giong future to completion on the worker that first polled it, within the same `block_on` or `Runtime::run`.
- Send data between workers and threads, not futures: `Handle::spawn_on` takes a closure and builds the future on the worker that runs it, and channels (`giong::sync`) carry values.

[All error codes](README.md)
"#),
    (14, r#"# GIONG-E0014: the compute lane is full

Raised by `giong::try_spawn_compute` and `Handle::try_spawn_compute`. In code: `giong::error::Code::ComputeBusy`. Its `io::ErrorKind` is `WouldBlock`.

## What happened

Every thread of the compute lane was busy and as many closures as `Builder::compute_backlog` allows (by default four per compute thread) were already waiting for one. The new closure was dropped without running. `giong::spawn_compute`, which never refuses, would have queued it behind the others.

## How to fix it

- Under overload this is the intended answer: shed the request (an HTTP server answers 503 with `Retry-After`) instead of queueing work that will finish too late to matter.
- If the lane is behind all the time, the machine needs more CPU for this work: give it more compute threads (`Builder::compute_threads`), fewer workers so those threads get CPUs of their own, or make each job cheaper.
- If short bursts are refused while the average load is fine, raise `Builder::compute_backlog` so a burst can wait.
- `Handle::lane_stats()` shows the compute lane's busy threads and queued closures.

[All error codes](README.md)
"#),
    (16, r#"# GIONG-E0016: this signal cannot be listened for

Raised by `giong::signal::listen`. In code: `giong::error::Code::SignalNotAllowed`.

## What happened

`listen` was given a signal that a program cannot, or should not, receive as a stream:

- `SIGKILL` and `SIGSTOP`: the kernel never lets a process catch them.
- `SIGSEGV`, `SIGBUS`, `SIGILL` and `SIGFPE`: they report a fault in the program itself (a bad memory access, an illegal instruction). Handling them later, on a worker, would let the faulting code run on.
- A number that is not a signal: 0, negative, or above `SIGRTMAX`.

## How to fix it

- Listen for the signals meant for programs to act on: `SignalKind::HANGUP` (reload the configuration), `SignalKind::USER1`/`USER2` (your own actions, such as a dump), `SignalKind::INTERRUPT`/`TERMINATE` (stop), `SignalKind::CHILD`, `SignalKind::WINDOW_CHANGE`, or a real-time signal from `SignalKind::from_raw(libc::SIGRTMIN() + n)`.
- To stop on `SIGINT` or `SIGTERM`, `Builder::shutdown_on_signals(true)` with `giong::shutdown_signal()` is usually all you need.

[All error codes](README.md)
"#),
    (17, r#"# GIONG-E0017: descriptors could not be passed

Raised by `send_with_fds`, `send_to_with_fds` and `recv_with_fds` of `giong::net::UnixStream` and `giong::net::UnixDatagram`. In code: `giong::error::Code::FdPassing`.

## What happened

One of these:

- **More than 253 descriptors for one message.** Linux passes at most 253 (`SCM_MAX_FD`, `giong::net::MAX_FDS`) per `sendmsg`. Nothing was sent.
- **Descriptors with an empty buffer.** Descriptors travel with the bytes of a message, and a stream socket sends nothing at all for an empty send, so they would be lost. Nothing was sent.
- **The kernel cut off the control data of a received message** (`MSG_CTRUNC`). The descriptors that did not fit were closed by the kernel before they arrived, and giong closed the ones that did, so the message is incomplete. giong leaves room for 253 descriptors and for credentials, so this happens only when the sender attached other control data as well, or the socket has options such as `SO_PASSCRED` turned on.

## How to fix it

- Send at most 253 descriptors per call; split a larger set over several messages.
- Send at least one byte with the descriptors, such as a message that says what they are.
- On a socket that receives descriptors, turn off other control data (`SO_PASSCRED`, `SO_TIMESTAMP` and similar), and have the sender attach only `SCM_RIGHTS`.

[All error codes](README.md)
"#),
    (18, r#"# GIONG-E0018: the inherited socket cannot be used that way

Raised by `giong::net::activated_sockets` and the methods of `giong::net::ActivatedSocket`. In code: `giong::error::Code::ActivatedSocket`.

## What happened

One of these:

- **The sockets were already taken.** systemd (or another socket manager speaking its protocol) hands a service its sockets as descriptors 3, 4, ... and says so in `LISTEN_FDS` and `LISTEN_PID`. `activated_sockets` takes ownership of them, so a second call would hand out the same descriptors twice; it fails instead.
- **The socket is not the kind asked for.** `tcp_listener` was called on a socket that is not a listening TCP socket (a UDP socket, a Unix socket, a TCP socket that is not listening), and likewise `unix_listener`, `udp_socket` and `unix_datagram`. The message says what the socket is.

## How to fix it

- Call `activated_sockets` once, early in `main`, and share the result (an `Arc<Vec<ActivatedSocket>>`) with the workers. Each worker then calls `tcp_listener()` (or the method for its kind) on the socket, which gives it its own descriptor.
- Match the method to the socket in the `.socket` unit: `ListenStream=` with an address or port is a TCP listener, `ListenStream=` with a path a Unix listener, `ListenDatagram=` with a port a UDP socket, with a path a Unix datagram socket. `FileDescriptorName=` names a socket, and `ActivatedSocket::name` returns that name, so pick sockets by name rather than by position.

[All error codes](README.md)
"#),
    (19, r#"# GIONG-E0019: cannot watch the child process

Raised by `giong::process::Command::spawn` and the methods of `giong::process::Child`. In code: `giong::error::Code::ChildProcess`.

## What happened

giong waits for a child process through a pidfd, a descriptor for the process that the kernel makes readable when it exits, so that waiting needs no thread and no `SIGCHLD` handler, and a signal can never reach another process that got the same process ID later. One of these went wrong:

- **The kernel refused the pidfd** (`pidfd_open`): Linux older than 5.3, or a seccomp profile that blocks the call. The child was started; giong killed and reaped it, since it could not have waited for it.
- **Someone else reaped the child.** Code in the same process called `waitpid(-1, ...)` or `wait()`, or set `SIGCHLD` to `SIG_IGN`, which makes the kernel reap children by itself. The exit status is gone.

## How to fix it

- Run on Linux 5.3 or newer, and allow `pidfd_open` and `pidfd_send_signal` in the seccomp profile (Docker's default profile allows both since Docker 20.10).
- Do not reap children that giong started: no `waitpid(-1, ...)`, no `wait()`, no `SIGCHLD` set to `SIG_IGN`, in your code or in a library you call. Listening for `SIGCHLD` with `giong::signal::listen` is fine: it does not reap.

[All error codes](README.md)
"#),
    (20, r#"# GIONG-E0020: a worker thread could not be started

Raised by the runtime, by `Runtime::run`. In code: `giong::error::Code::ThreadLimit`.

## What happened

giong runs each worker on a thread of its own, and starting one of them failed. The kernel says only `EAGAIN` ("Resource temporarily unavailable"), which nearly always means the process may not have more threads: every thread counts as a task against the PID limit of the process's cgroup (`pids.max`), and against `RLIMIT_NPROC` (`ulimit -u`), which is summed over all processes of the same user. Containers often set the first: `docker run --pids-limit`, the PID limit of a Kubernetes pod (`podPidsLimit` in the kubelet's settings), `TasksMax` in a systemd unit, and sandboxes that run code for AI agents. The message says which worker could not start and, where it can be read, the container's `pids.max`; the kernel's error is the `source()`, and its `io::ErrorKind` (`WouldBlock`) is kept.

The workers that had started are stopped again, and `run` returns this error; nothing has run.

## How to fix it

- Raise the limit: `--pids-limit` with `docker run` (`pids_limit` in Docker Compose), the kubelet's `podPidsLimit`, `TasksMax=` in the systemd unit, or `ulimit -u` in the shell that starts the service. Count one thread per worker, the program's own threads, and on the epoll driver up to 64 more for file operations.
- Or start fewer workers: `Builder::workers`. By default giong starts one per CPU the process may use, which on a large machine without a CPU limit can be more than a small PID limit allows.

[All error codes](README.md)
"#),
    (28, r#"# GIONG-E0028: the system ran out of network buffers

Raised by `TcpStream` and `UnixStream` writes and connects (giong-tls streams included) on macOS. In code: `giong::error::Code::NetworkBuffersExhausted`. The error kind is `OutOfMemory`, and the kernel's `ENOBUFS` is the `source()`, so `raw_os_error()` stays reachable.

## What happened

When macOS has no network buffer for a send or a connect, the call fails at once with `ENOBUFS`, even though the socket itself has room. Linux makes the sender wait instead, so the same program sees no error there. Sockets that hold much data nobody reads bring it on; some virtual machines, such as GitHub's macOS runners, also get it in short bursts with buffers far from any limit.

giong makes macOS behave like Linux for a while: a stream write or connect that gets `ENOBUFS` is tried again after 1 ms, then 2, 4, 8, 16 and every 32 ms (each step varied by up to a quarter), and fails with this code only when buffers have stayed short for 2 seconds. The worker runs its other tasks in the meantime. A `time::timeout` shorter than that ends the wait first, with its own error, and dropping the future cancels it with nothing sent. A connect whose socket could not be made for want of buffers (`socket(2)` fails with `ENOBUFS` too) makes it again, and one whose handshake the shortage broke is tried again on a new socket, within the same 2 seconds.

Calls that wait, then raise this code: `TcpStream` and `UnixStream` connects (including making their socket), writes and `send_file`, and giong-tls sends.

Calls that return the kernel's `ENOBUFS` as it is, with no code, as on Linux:

- datagram sends (`UdpSocket`), since a datagram may be dropped;
- accepts (`TcpListener::accept`, `UnixListener::accept`); try again after a short pause, as the giong-http server does;
- setup calls: the `bind` functions of `TcpListener`, `UdpSocket` and `UnixListener`, and `UnixStream::pair`.

## How to fix it

- Read from every connection promptly, and close the ones you no longer read from: each socket holds up to its receive buffer (`SO_RCVBUF`) of unread data.
- Cap the number of connections that can hold unread data at once (a semaphore around the work, or a lower connection limit on the server).
- Lower `SO_RCVBUF` and `SO_SNDBUF` if the program raised them.
- `netstat -m` shows how many requests for network buffers were denied. If the error comes with little in use, treat it as transient: try the operation again later, or on a new connection.

[All error codes](README.md)
"#),
    (60, r#"# GIONG-E0060: a file is already at the Unix socket path

Raised by `UnixListener::bind` on Windows. In code: `giong::error::Code::UnixSocketInUse`. The error kind is `AddrInUse`, and Winsock's `WSAEADDRINUSE` is the `source()`.

## What happened

A Unix domain socket that listens is a file at its path. When the listener closes, the file stays, on Windows as on Unix, and a second bind to the same path fails while it is there. giong does not remove the file for you: another server may still be listening on it, and taking its path would leave that server unreachable.

## How to fix it

- If no server listens on the path any more (an earlier run of the same program, say), remove the file with `std::fs::remove_file` and bind again. A service usually does this at start-up, after making sure no other copy of itself runs.
- Otherwise pick another path, or stop the server that holds it.

[All error codes](README.md)
"#),
    (61, r#"# GIONG-E0061: Unix socket path too long

Raised by `UnixListener::bind` and `UnixStream::connect` on Windows. In code: `giong::error::Code::UnixPathTooLong`. The error kind is `InvalidInput`.

## What happened

Winsock keeps the path of a Unix domain socket in a fixed field of 108 bytes, in UTF-8 and ending with a NUL, so a path may be at most 107 bytes long. Characters outside ASCII take two to four bytes each. The path is not shortened for you, since a shorter path names another socket.

## How to fix it

- Use a shorter path: a relative one (it is resolved against the current directory), or a directory near the root of the drive, such as `C:\run\app.sock`.
- `UnixStream::pair` binds a short-lived socket in the temporary directory (`std::env::temp_dir()`); if that directory's path is very long, point the `TMP` environment variable at a shorter one.

[All error codes](README.md)
"#),
    (62, r#"# GIONG-E0062: no Unix domain sockets on this Windows

Raised by `UnixListener::bind`, `UnixStream::connect` and `UnixStream::pair` on Windows. In code: `giong::error::Code::UnixSocketsUnavailable`. The error kind is `Unsupported`, and Winsock's error (`WSAEAFNOSUPPORT` or similar) is in the message.

## What happened

Windows has had Unix domain stream sockets (`AF_UNIX`) since Windows 10 version 1803 and Windows Server 2019. On an older system, or one where the `afunix` driver is turned off, creating such a socket fails.

## How to fix it

- Run on Windows 10 version 1803 or later, or Windows Server 2019 or later, and check that `sc query afunix` shows the driver running.
- Otherwise talk over TCP on a loopback address (`127.0.0.1`) instead.

[All error codes](README.md)
"#),
    (80, r#"# GIONG-E0080: the budget is at its limit

Raised by `Budget::try_spawn`, `Budget::try_reserve` and `Budget::try_spawn_compute` in `giong::budget`, and by `giong::try_spawn_compute` inside a task of a budget. In code: `giong::error::Code::BudgetFull`. Its `io::ErrorKind` is `WouldBlock`.

## What happened

The budget (one tenant's share of the runtime) already holds as much as one of its `Limits` allows: tasks alive, bytes reserved, or compute closures queued and running; or other calls already wait for room in it. The message names the budget, the limit, what is in use and how many wait. The work was dropped without running. The waiting forms (`Budget::spawn`, `Budget::reserve`, `Budget::spawn_compute`) would have waited their turn instead.

## How to fix it

- Under overload this is the intended answer: the tenant has as much in progress as it may, so shed the request (an HTTP server answers 503 with `Retry-After`) rather than queue work for it. Other tenants are not affected.
- If the tenant's normal load needs more, raise its limits with `Budget::set_limits`.
- To wait for room instead of refusing, use the waiting form of the call, bounded with `giong::time::timeout`.
- `Budget::stats()` shows what each resource of the budget holds, its peak, and how many calls waited or were refused.

[All error codes](README.md)
"#),
    (81, r#"# GIONG-E0081: a reservation larger than the budget

Raised by `Budget::reserve` and `Budget::try_reserve` in `giong::budget`. In code: `giong::error::Code::BudgetTooSmall`. Its `io::ErrorKind` is `InvalidInput`.

## What happened

The reservation asked for more bytes than the budget's whole memory limit (`Limits::memory`). It could never be granted, however long it waited, so it failed at once instead of waiting forever.

## How to fix it

- Raise the budget's memory limit above the largest reservation its work makes (`Budget::set_limits`).
- Or reserve in parts: a request body can be reserved as it arrives, in chunks, with each chunk's reservation kept until the body is dropped.

[All error codes](README.md)
"#),
    (90, r#"# GIONG-E0090: IPC message too large

Raised by `giong::ipc::Sender::send`. In code: `giong::error::Code::IpcMessageTooLarge`.

## What happened

The message is longer than the channel carries. A message may take at most a quarter of the channel's capacity (the capacity of one direction, chosen by `giong::ipc::connect`), less 8 bytes for its record head, so that it always fits in the ring even when it has to start again at the beginning. `Sender::max_message` says the limit. Nothing was sent.

## How to fix it

- Connect with a larger capacity: a channel of `1 << 22` bytes (4 MiB per direction) takes messages up to 1 MiB minus 8 bytes. The listener must allow it (`IpcListener::bind`'s limit).
- Or split large payloads into several messages, or send them another way (a file, a pipe) and send a short message that names them.

[All error codes](README.md)
"#),
    (91, r#"# GIONG-E0091: IPC channel memory corrupted

Raised by `giong::ipc::Sender::send` and `giong::ipc::Receiver::recv`. In code: `giong::error::Code::IpcCorrupted`.

## What happened

The other process wrote something into the channel's shared memory that breaks its rules: a read or write position out of range, or a message length that points past the data written. giong checks every position and length it reads from shared memory, so it stopped instead of reading or writing outside the channel. The message says which value was wrong. The channel cannot be trusted any more.

Causes: the other process writes into the mapping by its own means, runs a giong version with another channel layout under the same protocol version (a bug), or is hostile.

## How to fix it

- Drop both halves and connect again.
- Check that the other process uses `giong::ipc` of the same giong version and does not touch the shared memory itself.
- If both sides are unchanged giong code, report it with both versions: it is a bug.

[All error codes](README.md)
"#),
    (92, r#"# GIONG-E0092: IPC capacity out of range

Raised by `giong::ipc::connect`, `giong::ipc::IpcListener::bind` and `IpcListener::accept`. In code: `giong::error::Code::IpcCapacity`.

## What happened

An IPC channel's capacity (the bytes of one direction's ring) must be a power of two from 4096 to 2^30 (1 GiB), and at most the limit the listener was bound with. Either the number given is not such a power of two, or the connecting process asked for more than the listener allows; the listener then refuses the channel and keeps listening. Both processes map twice the capacity plus a page, so the limit bounds the memory a client can make the listener map.

## How to fix it

- Pass a power of two, such as `1 << 20` (1 MiB per direction).
- Connect with a capacity no larger than the listener's limit, or raise the limit in `IpcListener::bind`.

[All error codes](README.md)
"#),
    (93, r#"# GIONG-E0093: IPC handshake failed

Raised by `giong::ipc::connect` and `giong::ipc::IpcListener::accept`. In code: `giong::error::Code::IpcHandshake`.

## What happened

Setting up the channel over the Unix socket failed: the other end is not a giong IPC endpoint, speaks another protocol version, sent a malformed handshake, closed the connection during it, or passed shared memory that is not sealed against resizing, has the wrong size, or wake-up descriptors that are not eventfds. On the listener, it is also raised for a process that connected and sent no handshake within 5 seconds; its connection is closed. The message says which. On the listener, only this connection is refused: it keeps accepting, and runs handshakes apart (up to 64 at once), so one slow or silent client holds up no other.

## How to fix it

- Connect `giong::ipc::connect` to a socket bound with `giong::ipc::IpcListener::bind`, not to another kind of Unix socket.
- Run the same giong version (or at least the same IPC protocol version) on both sides.
- In a server loop, go on to the next `accept()` after this error rather than returning it: it concerns one client.

[All error codes](README.md)
"#),
    (94, r#"# GIONG-E0094: IPC peer process gone

Raised by `giong::ipc::Sender::send` and `giong::ipc::Receiver::recv`. In code: `giong::error::Code::IpcPeerGone`.

## What happened

The other process of the channel closed its end of the channel's Unix socket without dropping its halves first: it exited or crashed. `recv` returns every message the other process had sent before this error. `send` fails with it once it has seen the other process go, which it checks when the channel is full and it has to wait for room: while there is room, a message to a peer that is gone is written into the ring and `send` returns `Ok`, as a write to a socket whose peer has just died can.

A peer that drops its `Sender` normally ends the channel with `Ok(None)` from `recv` instead.

## How to fix it

- Treat it like a connection reset: drop both halves and connect again once the other process is back.
- Find out why the other process ended (its logs, exit status) if it was not expected to.

[All error codes](README.md)
"#),
    (95, r#"# GIONG-E0095: IPC receiver closed

Raised by `giong::ipc::Sender::send`. In code: `giong::error::Code::IpcReceiverClosed`.

## What happened

The other side dropped its `Receiver`, so nothing reads this direction of the channel any more. The message was not sent. Messages sent before the receiver was dropped may not have been read.

## How to fix it

- Stop sending on this channel and drop the `Sender`.
- If the other side should keep reading, find out why it dropped its receiver (it finished, or an error ended its loop).

[All error codes](README.md)
"#),
    (100, r#"# GIONG-E0100: invalid address

Raised by `giong::net`, when an address is given as a string, or when a host name is given to a function that takes an IP address. In code: `giong::error::Code::InvalidAddress`.

## What happened

One of these:

- An address string is not `"host:port"`: the port is missing, is not a number from 0 to 65535, or the string is empty.
- A host name, such as `"localhost:8080"`, was given to `TcpListener::bind`, `TcpListener::bind_balanced`, `UdpSocket::bind` or `UdpSocket::connect`. These functions are not `async`, so they cannot wait for a name to be looked up without blocking the worker, and a name like `localhost` can stand for `127.0.0.1`, `::1` or both, which a server should choose itself. They take IP addresses only. `TcpStream::connect` and `lookup_host` take host names.

## How to fix it

- To connect or look a name up (`TcpStream::connect`, `lookup_host`), write the host and the port: `"localhost:8080"`, `"10.0.0.5:5432"`, `"[::1]:443"` (brackets around an IPv6 address).
- To listen, write the IP address: `"127.0.0.1:8080"` for this machine only, `"0.0.0.0:8080"` for every IPv4 interface, `"[::]:8080"` for IPv6.
- To bind or connect a UDP socket to a name, resolve it first: `giong::net::lookup_host("example.com:53").await?` gives the addresses to pass.
- A URL is not an address: take the host and the port out of it first.

[All error codes](README.md)
"#),
    (101, r#"# GIONG-E0101: invalid host name

Raised by `giong::net` name resolution. In code: `giong::error::Code::InvalidHostName`.

## What happened

A host name has characters or a shape that DNS does not allow, so it was refused before any query was sent.

## How to fix it

- A name is labels of letters, digits, `-` and `_`, separated by dots; each label is 1 to 63 characters, the whole name at most 253.
- Spaces, empty labels (`a..b`) and non-ASCII characters are not allowed; an internationalized name must be given in its `xn--` form.

[All error codes](README.md)
"#),
    (102, r#"# GIONG-E0102: host not found

Raised by `giong::net` name resolution (`TcpStream::connect`, `lookup_host`, `Resolver`). In code: `giong::error::Code::HostNotFound`.

## What happened

The name has no address. giong looked in `/etc/hosts`, then asked the DNS servers in `/etc/resolv.conf` (with its `search` domains), and they answered that the name does not exist or has no `A` or `AAAA` record.

## How to fix it

- Check the spelling of the name.
- Check `/etc/hosts` and `/etc/resolv.conf`; in a container, check that it is attached to the network where the name is defined (a Docker Compose service; a Kubernetes Service in another namespace needs the namespace in its name).
- `getent hosts <name>` shows what the system resolver finds, for comparison.

[All error codes](README.md)
"#),
    (103, r#"# GIONG-E0103: no address to use

Raised by `giong::net` connect and bind. In code: `giong::error::Code::NoAddresses`.

## What happened

A connect or bind was given an empty list of addresses, so there was nothing to try.

## How to fix it

- Pass at least one address. To connect, a host name that resolves to one also works; `bind` takes IP addresses only ([GIONG-E0100](E0100.md)).

[All error codes](README.md)
"#),
    (104, r#"# GIONG-E0104: no DNS server configured

Raised by `giong::net` name resolution. In code: `giong::error::Code::NoDnsServer`.

## What happened

A name had to be looked up in DNS, but the resolver has no DNS server to ask: it was built with `ResolverConfig::nameservers` and an empty list. (An `/etc/resolv.conf` that is missing or has no `nameserver` line does not cause this: like glibc, giong then asks `127.0.0.1:53`, and if nothing answers there the error is [GIONG-E0108](E0108.md) or [GIONG-E0105](E0105.md).)

On Windows the DNS servers come from the network adapters, and Windows runs no server at `127.0.0.1:53`: when no adapter reports one (offline, or only link-local IPv6 servers, which giong leaves out), a lookup fails with this code at once rather than timing out there.

## How to fix it

- On Windows, give a network adapter a DNS server; `ipconfig /all` lists what each one has.

- Pass at least one server to `ResolverConfig::nameservers`, as in `SocketAddr::from(([10, 0, 0, 2], 53))`.
- Or use an IP address, or a name listed in `/etc/hosts`, which need no DNS.

[All error codes](README.md)
"#),
    (105, r#"# GIONG-E0105: DNS server did not answer

Raised by `giong::net` name resolution. In code: `giong::error::Code::DnsTimeout`.

## What happened

No DNS server answered in time, after trying each server in `/etc/resolv.conf` the configured number of times.

## How to fix it

- Check that the servers are reachable from here: a firewall, a VPN or a container network can drop DNS traffic (UDP and TCP port 53).
- `options timeout:N attempts:N` in `/etc/resolv.conf` (or `ResolverConfig::timeout` and `attempts`) give the servers longer.

[All error codes](README.md)
"#),
    (106, r#"# GIONG-E0106: DNS server failed

Raised by `giong::net` name resolution. In code: `giong::error::Code::DnsServerFailure`.

## What happened

A DNS server answered, but with an error such as `SERVFAIL` (it could not get an answer from upstream) or `REFUSED` (it will not answer this client). That is different from the name not existing (`GIONG-E0102`).

## How to fix it

- Try again: `SERVFAIL` is often temporary.
- Check the DNS server: its logs, and whether it allows queries from this machine.
- If the name is in a private zone, check that this server is the one that serves it.

[All error codes](README.md)
"#),
    (107, r#"# GIONG-E0107: malformed DNS answer

Raised by `giong::net` name resolution. In code: `giong::error::Code::DnsBadResponse`.

## What happened

A DNS server sent an answer giong cannot read, or closed a TCP connection before answering. Something between giong and the server may be changing or cutting the traffic.

## How to fix it

- Check the DNS server named in the message.
- Check for middleboxes that rewrite DNS or block TCP on port 53 (large answers are fetched over TCP).
- If the server is standard and the answer looks valid to `dig`, please report it at https://github.com/giong-run/Giong/issues.

[All error codes](README.md)
"#),
    (108, r#"# GIONG-E0108: cannot reach the DNS server

Raised by `giong::net` name resolution: `TcpStream::connect` with a host name, `giong::net::lookup_ip`, and everything built on them. In code: `giong::error::Code::DnsServerUnreachable`.

## What happened

A query could not reach the DNS server at all. Most often nothing listens at the server's address: the kernel answered the query with "connection refused" (`ECONNREFUSED`), as it does for a local address where no DNS server runs, such as `127.0.0.1:53` when `/etc/resolv.conf` has no `nameserver` line. The network may also have reported the server unreachable. The message names the server and the name looked up; the kernel's error is the `source()`.

Before giong 0.6.0 this came back as the kernel's bare "connection refused", so a failed `connect("db.internal:5432")` read as if the database had refused the connection.

## How to fix it

- Check `/etc/resolv.conf`: its `nameserver` lines must name a DNS server that runs. In a container, it is written by the container runtime from the network the container is attached to.
- Check that the server is reachable from here: a firewall must let UDP and TCP port 53 through, and a VPN or network policy may block it.
- Or use an IP address, or a name listed in `/etc/hosts`, which need no DNS.

[All error codes](README.md)
"#),
    (109, r#"# GIONG-E0109: buffer with no room

Raised by `giong::net::UdpSocket::read_from`. In code: `giong::error::Code::EmptyBuffer`.

## What happened

`read_from` was given a buffer whose capacity is zero, usually `Vec::new()` where `Vec::with_capacity(n)` was meant. `read_from` receives into the buffer's capacity, not its length. A datagram is received whole or not at all: received into no room, it would be taken off the socket and lost, so giong refuses before asking the kernel, and the datagram stays queued for the next read.

## How to fix it

- Pass a buffer with room for the largest datagram expected: `Vec::with_capacity(65_536)` holds any UDP datagram; `Vec::with_capacity(1_500)` holds one that fits an Ethernet frame. A longer datagram is cut off to the capacity.
- Or use `recv_from`, which receives into the runtime's buffer pool and needs no buffer of yours.

[All error codes](README.md)
"#),
    (110, r#"# GIONG-E0110: receive buffers too small for GRO

Raised by `giong::net::UdpSocket::set_gro` (feature `unstable-udp-offload`). In code: `giong::error::Code::RecvBufferTooSmall`.

## What happened

With generic receive offload the kernel hands over up to 64 KiB of datagrams from one sender as one. `recv_from` receives into the worker's pool buffers; a coalesced datagram longer than a buffer is cut off and the datagrams in the cut part are lost. The worker's buffers are smaller than 64 KiB plus the 132 bytes giong keeps in front of them, so giong refuses to turn GRO on.

## How to fix it

- Build the runtime with larger buffers, for example `Builder::new().recv_buffers(256, giong::net::GRO_RECV_BUFFER)`, before turning GRO on. Each buffer costs its size in memory for every buffer the worker has in use.
- Or leave GRO off: every datagram then arrives on its own.

[All error codes](README.md)
"#),
    (111, r#"# GIONG-E0111: receive option set after recv started

Raised by `giong::net::UdpSocket::set_gro` and `giong::net::UdpSocket::set_recv_ecn` (feature `unstable-udp-offload`). In code: `giong::error::Code::RecvAlreadyStarted`.

## What happened

These options change the layout of what the kernel writes for each received datagram. `recv_from` had already started a multishot receive with the old layout, so changing the option now would make datagrams already queued unreadable.

## How to fix it

- Set the options right after binding the socket, before the first `recv_from`.

[All error codes](README.md)
"#),
    (200, r#"# GIONG-E0200: invalid path

Raised by `giong::fs`. In code: `giong::error::Code::InvalidPath`.

## What happened

A path contains a NUL byte. Linux paths are passed to the kernel as C strings, which end at the first NUL, so such a path cannot name any file.

## How to fix it

- Remove the NUL byte; it usually comes from unchecked input or a fixed-size buffer converted whole.

[All error codes](README.md)
"#),
    (201, r#"# GIONG-E0201: the file ended before the bytes to send

Raised by `giong::net::TcpStream::send_file`, and by the `send_file` of `giong-tls` and `giong-http` built on it. In code: `giong::error::Code::FileTooShort`.

## What happened

`send_file` was asked to send `len` bytes from `offset`, and the file ends before `offset + len`. The bytes the file had were sent; the peer received fewer than it was told to expect. This usually means the file was truncated or replaced between the moment its size was read and the moment it was sent, or the range was computed from the wrong file.

The connection is not closed, but whatever announced the length (an HTTP `Content-Length`, a length prefix) is now wrong, so the peer will wait for bytes that never come. Close the connection.

## How to fix it

- Take the length from the open file itself (`File::len`), not from a separate `metadata` call on the path, so that both describe the same file.
- If the file can change while it is served, write the new version to a temporary file and `rename` it over the old one: an open file keeps its old contents.
- Check `offset + len <= file.len()?` before calling `send_file` when the range comes from a request (an HTTP `Range` header).

[All error codes](README.md)
"#),
    (202, r#"# GIONG-E0202: file offset out of range

Raised by `giong::fs::File::read_at`, `write_at`, `write_all_at` and `allocate`. In code: `giong::error::Code::OffsetOutOfRange`.

## What happened

The offset is above `i64::MAX` (9,223,372,036,854,775,807), the largest file offset Linux has. Nothing was read or written. Such an offset is almost always the result of arithmetic that wrapped around, or of `u64::MAX` used as a marker. giong checks it before asking the kernel because io_uring reads an offset of `u64::MAX` as "the file's current position": without the check, the read or write would land wherever the descriptor's position happens to be, and only on the io_uring driver (the epoll driver's `pread`/`pwrite` would fail with `EINVAL`). For `write_all_at`, the offset of a later chunk (`offset` plus the bytes already written) can be the one out of range. For `allocate`, it is the end of the range (`offset` plus `len`).

## How to fix it

- Check the arithmetic that computed the offset: use `checked_add` and handle the overflow.
- A `giong::fs::File` has no cursor; every read and write names its offset. To append, open the file with `OpenOptions::new().append(true)`, and every write goes to the end whatever offset it names.

[All error codes](README.md)
"#),
    (203, r#"# GIONG-E0203: invalid combination of open options

Raised by `giong::fs::OpenOptions::open`. In code: `giong::error::Code::InvalidOpenOptions`.

## What happened

The options ask for something `open(2)` cannot do, so nothing was opened and the kernel was not asked. The message says which of these it is:

- none of `read`, `write` and `append` is set, so the file would be opened for nothing;
- `truncate`, `create` or `create_new` is set without `write` or `append`: emptying or creating a file needs write access;
- `append` and `truncate` are both set (without `create_new`).

These are the combinations the standard library's `std::fs::OpenOptions` refuses too. The kernel would answer most of them with a bare `EINVAL`, which does not say which option is wrong.

## How to fix it

- To read: `OpenOptions::new().read(true)`.
- To create or replace a file: `OpenOptions::new().write(true).create(true).truncate(true)` (what `File::create` does).
- To add to the end of a file, creating it if needed: `OpenOptions::new().append(true).create(true)`. To empty it first, open it with `.write(true).truncate(true)` and write in order instead of appending.

[All error codes](README.md)
"#),
    (204, r#"# GIONG-E0204: file too large to read into memory

Raised by `giong::fs::read`. In code: `giong::error::Code::FileTooLarge`. Kind: `OutOfMemory`.

## What happened

`fs::read` reads a whole file into one `Vec<u8>`, sized from the file's length. The allocator could not give that much memory, so nothing was read and nothing is kept. The process goes on: without this check, Rust aborts the whole process when an allocation fails.

The length can be far more than the file holds: a sparse file (a disk image, a database file, a core dump) reports the size it was given, not the blocks it uses. A path that a client chooses (an upload directory, a static file handler) can point at such a file.

## How to fix it

- Read the file in pieces with `File::read_at`, or stream it (`giong_http::FileBody` sends a file without loading it).
- Check `File::len` (or `fs::metadata(path).await?.len()`) first, and refuse files above a limit you choose.

[All error codes](README.md)
"#),
    (1101, r#"# GIONG-E1101: invalid HTTP request

Raised by giong-http's client, before anything is sent. In code: `giong::error::Code::HttpClientRequest`.

## What happened

The request could not be sent as written: the URL is not an absolute `http` or `https` URL, a header name or value is not valid, the method is not a token, or a header that frames the message was set by hand. The message names the part. Nothing was sent.

## How to fix it

- Write the URL whole: `https://host[:port]/path?query`. Percent-encode spaces and other bytes that are not visible ASCII (`%20` for a space); give a non-ASCII host name in its `xn--` form.
- User names and passwords in the URL are not sent: set an `Authorization` header instead.
- `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade` and `TE` are set by the client from the body and the connection; leave them out.
- A header value cannot contain a line break: that would let it start a header of its own.

[All error codes](README.md)
"#),
    (1102, r#"# GIONG-E1102: cannot connect to the HTTP server

Raised by giong-http's client, opening a connection. In code: `giong::error::Code::HttpClientConnect`.

## What happened

Opening the connection failed: the host name did not resolve, the TCP connection was refused or did not answer, the TLS handshake failed (the certificate was not valid for the host, or not issued by a trusted authority), or it all took longer than the connect timeout (10 seconds by default). The cause is in the message, with the code of the network or TLS error that caused it.

## How to fix it

- Check the host and the port, and that the server is reachable from here (`curl -v` from the same machine or container shows the same thing).
- For certificate errors, see the TLS code in the message (`GIONG-E2004`): the certificate must name the host in the URL. A private authority is trusted with `ClientBuilder::tls(TlsConnector::with_root_pem(..))`.
- On a machine without certificate authorities installed (a `FROM scratch` image), install `ca-certificates` or pass `TlsConnector::with_webpki_roots()` (giong-tls's `webpki-roots` feature).
- If the server is far away or slow to accept, raise `ClientBuilder::connect_timeout`.
- Through a proxy, the message names the proxy when it is the proxy that cannot be reached: check its address, or leave the host out with `Proxy::no_proxy`.
- It is transient (`Error::is_transient`): retrying after a pause can work.

[All error codes](README.md)
"#),
    (1103, r#"# GIONG-E1103: HTTP request timed out

Raised by giong-http's client, any request. In code: `giong::error::Code::HttpClientTimeout`.

## What happened

The request did not finish within its timeout (30 seconds by default). The timeout covers the whole request: waiting for a free connection, connecting, sending, reading the whole response, and following redirects. The request was dropped, and the connection it used was closed.

Two client calls that send no request raise it too. `Client::preconnect` did not open its connection within the client's timeout. `Client::ping_idle_http2` sent a PING on an idle HTTP/2 connection and the server did not acknowledge it within the deadline given; the message says how many connections were probed and how many were closed for it. Such a connection may have died silently (a NAT or load balancer dropped it), so it was closed at once and takes no more requests.

## How to fix it

- If the server is known to be slow, raise the timeout for that request (`RequestBuilder::timeout`) or for the client (`ClientBuilder::timeout`).
- If many requests wait for a connection to the same host, raise `ClientBuilder::max_connections_per_host` (16 by default).
- After `Client::ping_idle_http2`: nothing needs fixing, the next request opens a new connection; call `Client::preconnect` to open it now. If a healthy server can take longer to answer than the deadline you gave (a few round trips), give a longer one.
- Otherwise find out why the server is slow; the error is transient, but a retry adds load to a server that is already behind.

[All error codes](README.md)
"#),
    (1104, r#"# GIONG-E1104: HTTP response too large

Raised by giong-http's client, reading a response. In code: `giong::error::Code::HttpClientTooLarge`.

## What happened

The response was larger than the client accepts: its body over `ClientBuilder::max_response_size` (16 MiB by default), or its head (status line and headers) over `ClientBuilder::max_head_size` (64 KiB). The connection was closed rather than read to the end.

## How to fix it

- If such responses are expected, raise the limit on the client that fetches them.
- For an API that can return very large results, ask for less at a time (pagination, a narrower query).

[All error codes](README.md)
"#),
    (1105, r#"# GIONG-E1105: redirect not followed

Raised by giong-http's client, following redirects. In code: `giong::error::Code::HttpClientRedirect`.

## What happened

A redirect was not followed: there were more than `ClientBuilder::redirects` of them (10 by default), it pointed from an `https` URL to an `http` one, or its `Location` was not a valid URL. The message has the redirect's target.

## How to fix it

- A redirect loop is a server problem; the message shows where it pointed last.
- A redirect from HTTPS to HTTP would send the request, and its credentials, in clear text: fix the server, or request the `http` URL directly if that is really wanted.
- To handle redirects yourself, build the client with `redirects(0)`: 3xx responses are then returned as they are.

[All error codes](README.md)
"#),
    (1106, r#"# GIONG-E1106: connection closed before the response

Raised by giong-http's client, sending a request or reading a response. In code: `giong::error::Code::HttpClientClosed`.

## What happened

The connection closed, or failed, before the whole response arrived. On a connection that had carried requests before, this usually means the server closed it as the request was sent; an idempotent request (GET, HEAD, PUT, DELETE, OPTIONS) is then sent once more on a new connection by itself, so this error means that failed too, or that the request was a POST or PATCH, which the server may have processed.

Over HTTP/2 the same error covers a stream the server reset (`RST_STREAM`; the message names its error code, such as `INTERNAL_ERROR` or `CANCEL`) and a connection the server closed with GOAWAY. A request the server says it did not process (`REFUSED_STREAM`, or a stream beyond the last one a GOAWAY names) is sent again by itself, whatever its method, up to five times in a row.

`Client::ping_idle_http2` raises it when an idle HTTP/2 connection ended while it was being probed (the server said GOAWAY or closed it). The connection left the pool; the next request opens a new one.

## How to fix it

- Retry if the request is safe to repeat (`Error::is_transient` is true).
- If it happens often on idle connections, lower `ClientBuilder::idle_timeout` below the server's (or its load balancer's) keep-alive timeout.
- If it happens in the middle of a response, look at the server's logs: it may have crashed or hit its own limit.
- If the message says the server closed the connection **without a TLS close_notify**: the response had no length and was meant to end where the connection ends, but over HTTPS only a `close_notify` alert proves that the server ended it, so the client refuses a body that may have been cut short by someone on the network path (a truncation attack). Make the server send `Content-Length` or chunked responses, or close its TLS connections properly.

[All error codes](README.md)
"#),
    (1107, r#"# GIONG-E1107: malformed HTTP response

Raised by giong-http's client, reading a response. In code: `giong::error::Code::HttpClientProtocol`.

## What happened

The server sent something that is not a valid HTTP/1.1 response, or one this client refuses because it could be read in two ways: two different `Content-Length` headers, lines that end in a bare line feed, folded header lines, a malformed chunked body, a `Transfer-Encoding` other than chunked (such as gzip), or a switch to another protocol. The connection was closed and is not used again.

Over HTTP/2 the server broke a rule of RFC 9113 instead. On one stream (a response head with an uppercase or connection-specific field such as `connection`, no `:status`, a body longer or shorter than its `content-length`, more data than the stream's flow-control window allowed), the stream is reset and the connection goes on carrying the other requests. On the whole connection (a frame that is malformed or out of place, a header block that does not decode, more data than the connection's window allowed, `SETTINGS_ENABLE_PUSH` or a `PUSH_PROMISE` from the server), the client sends a GOAWAY and every request on the connection fails.

## How to fix it

- Check that the URL points at an HTTP server, on the right port and scheme (`https` for a TLS port).
- A proxy or server sending ambiguous framing is a bug there; the message says which rule was broken.
- `Transfer-Encoding: gzip` is not read (the client decompresses `Content-Encoding: gzip` and `deflate` only): make the server use `Content-Encoding`.
- Over HTTP/2, the message names the rule. A server that gets HTTP/2 wrong can be spoken to over HTTP/1.1 with `ClientBuilder::http2(false)` until it is fixed.

[All error codes](README.md)
"#),
    (1108, r#"# GIONG-E1108: HTTP error status

Raised by giong-http's client, `Response::error_for_status`. In code: `giong::error::Code::HttpClientStatus`.

## What happened

The response has a 4xx (client error) or 5xx (server error) status, and the program asked for that to be an error. The message has the status and the start of the body, where APIs usually say what was wrong; `Error::status` has the code.

## How to fix it

- Read the body in the message: for 400 and 422 it usually names the field that was refused.
- 401 and 403: check the credentials and their scope.
- 429, 502, 503 and 504 are transient (`Error::is_transient`): retry after a pause, honouring the `Retry-After` header if the response has one.

[All error codes](README.md)
"#),
    (1109, r#"# GIONG-E1109: cannot decode the response body

Raised by giong-http's client, reading a body. In code: `giong::error::Code::HttpClientDecode`.

## What happened

The body could not be read the way it was asked for: `Response::text` found bytes that are not UTF-8, or a body the server compressed (`Content-Encoding: gzip` or `deflate`) could not be decompressed: the compressed data is damaged, fails its checksum, or ends before it should (a response cut short).

## How to fix it

- For text: read it as bytes with `Response::body`, and decode it with the character set its `Content-Type` names.
- For a compressed body: the server sent damaged data, or the connection was cut. Retrying can help if it was cut. To see the bytes as they came, send the request with its own `Accept-Encoding` header, or build the client with `ClientBuilder::decompress(false)`.
- Encodings the client does not decode (`br`, `zstd`) are never an error: the body comes as it was sent, with its `Content-Encoding` header.

[All error codes](README.md)
"#),
    (1110, r#"# GIONG-E1110: the HTTP proxy refused the request

Raised by giong-http's client, through a proxy. In code: `giong::error::Code::HttpClientProxy`.

## What happened

The proxy given with `ClientBuilder::proxy` (or read by `Proxy::from_env`) would not open a tunnel to an `https` server: it asked for credentials (407), its rules do not allow the host or port (403), it could not reach the server (502, 504), or it answered with something that is not HTTP. `Error::status` has the proxy's status, if it sent one. A proxy that cannot be reached at all is `GIONG-E1102` instead. For an `http` URL the proxy's answer, 407 included, is the response.

## How to fix it

- 407: put the user name and password in the proxy's URL, `http://user:password@proxy:3128` (with `%40` for an `@` in either). They are sent only to the proxy, never to the server.
- 403: the proxy's own rules refuse the destination. If the host should be reached without the proxy (a service in the same cluster, say), add it to `Proxy::no_proxy` or `NO_PROXY`: `.svc.cluster.local, 10.0.0.0/8`.
- 502 or 504: the proxy could not reach the server; this one is transient (`Error::is_transient`). Check the host from the proxy's side.
- An answer that is not HTTP: the proxy URL points at something else (a SOCKS proxy, or the wrong port). Only `http://` proxies are supported.

[All error codes](README.md)
"#),
    (1111, r#"# GIONG-E1111: request body too large

Raised by giong-http's server, to a handler reading a request body as a stream. In code: `giong::error::Code::HttpBodyTooLarge`.

## What happened

The handler's `Handler::stream_body` returned `Some(limit)`, and the body grew past `limit` bytes. A body that announces a longer `Content-Length` never gets this far: the server answers `413 Content Too Large` without calling the handler. So this is a chunked body, whose length is not known in advance, or a limit passed to `BodyStream::read_to_end`. The connection closes after the response, since the rest of the body is not read.

## How to fix it

- Answer `413 Content Too Large`. In giong-web, `?` on the error does that.
- If bodies this large are expected, raise the limit `stream_body` returns (or `Router::stream_bodies` in giong-web), or the one given to `read_to_end`.

[All error codes](README.md)
"#),
    (1112, r#"# GIONG-E1112: request body cut short

Raised by giong-http's server, to a handler reading a request body as a stream. In code: `giong::error::Code::HttpBodyIncomplete`.

## What happened

The body ended before its end. Either the client closed the connection, cancelled the request (an HTTP/2 client resets its stream with `RST_STREAM`) or sent nothing for the server's `request_timeout` (each piece of a streamed body gets that long, counted from the one before; the whole body has no deadline), or the handler had already returned its response, so the connection stopped reading the body. The second happens to a `BodyStream` kept past the response, in a task of its own for example.

## How to fix it

- A client that went away or stalled: nothing to fix on the server. The client may retry. For clients on slow links, raise `Server::request_timeout`.
- A stream read after the response: read the body to its end before returning the response, or stop reading it once the response is returned.

[All error codes](README.md)
"#),
    (1113, r#"# GIONG-E1113: malformed request body

Raised by giong-http's server, to a handler reading a request body as a stream. In code: `giong::error::Code::HttpBodyMalformed`.

## What happened

The request body uses `Transfer-Encoding: chunked`, and its framing is broken: a chunk size that is not hexadecimal, a chunk not followed by CRLF, a line longer than 4 KiB, or more than 16 KiB of chunk extensions in all. The pieces before the broken framing were delivered. The connection closes after the response.

Over HTTP/2 the client broke a rule of the request's stream instead: a body longer or shorter than its `content-length`, or more data than the flow-control window allowed. The server resets the stream (`RST_STREAM`); the connection goes on.

## How to fix it

- Answer `400 Bad Request`. In giong-web, `?` on the error does that.
- The client (or a proxy in between) sent an invalid body. Check what it sends with `curl -v` or a packet capture.

[All error codes](README.md)
"#),
    (1114, r#"# GIONG-E1114: response body not sent

Raised by giong-http's server, to the code producing a streamed response body. In code: `giong::error::Code::HttpBodyNotSent`.

## What happened

`BodySender::send` was called, but nothing more of the body will reach the client: the client went away (closed the connection, or did not read for the server's `write_timeout`), or the response never had a body to send (an answer to `HEAD`, or status 204 or 304), so the server dropped it. A client that goes away is noticed at the next write.

## How to fix it

- Stop producing the body. Nothing is wrong on the server; this is how a producer learns that its client is gone.
- An event stream (`text/event-stream`) that is quiet for long stretches can send a comment line (`: keep-alive`) every 15 to 30 seconds, so a client that went away is noticed then instead of at the next event.

[All error codes](README.md)
"#),
    (1115, r#"# GIONG-E1115: HTTP/2 protocol error

Sent by giong-http's server to an HTTP/2 client, in the debug data of the GOAWAY frame that closes the connection. In code: `giong::error::Code::Http2Protocol`.

## What happened

The client broke a rule of HTTP/2 (RFC 9113) that leaves the whole connection in doubt, so the server closed it. The GOAWAY carries the error code the RFC asks for (`PROTOCOL_ERROR`, `FRAME_SIZE_ERROR`, `FLOW_CONTROL_ERROR`, `COMPRESSION_ERROR` or `STREAM_CLOSED`) and, after this code, the rule that was broken, for example:

```text
GIONG-E1115: DATA on a stream that was never opened
GIONG-E1115: a header block was interrupted by another frame
GIONG-E1115: the connection did not start with the HTTP/2 preface
```

Requests already being answered are cut off. A mistake that only concerns one stream (a malformed request, a body longer than its `content-length`) resets that stream instead, and the connection goes on.

## How to fix it

- The client, or a proxy in between, has a bug. Read the rule in the debug data, then look at the frames it sends: `nghttp -nv https://host/` or `curl -v --http2` for a request of your own, a packet capture with TLS keys (`SSLKEYLOGFILE`) otherwise.
- A client that speaks HTTP/1.1 to a port where it should speak HTTP/2, or the other way round, is answered as HTTP/1.1 on a plain port and through ALPN over TLS, so it should not land here. Over plain TCP, HTTP/2 needs prior knowledge (`curl --http2-prior-knowledge`); the `Upgrade: h2c` handshake is not supported.

[All error codes](README.md)
"#),
    (1116, r#"# GIONG-E1116: too many HTTP/2 streams reset

Sent by giong-http's server to an HTTP/2 client, in the debug data of the GOAWAY frame (`ENHANCE_YOUR_CALM`) that closes the connection. In code: `giong::error::Code::Http2TooManyResets`.

## What happened

The client cancelled (`RST_STREAM`) more streams before their answer than the server allows in a short time, or had that many refused or reset by the server. Opening a stream and cancelling it at once makes the server start work that nobody waits for; done in a loop, this is the "rapid reset" attack (CVE-2023-44487). The server allows a burst of twice `max_concurrent_streams` (at least 100) and then `max_concurrent_streams` per second (at least 50).

The server also drops the handler of a cancelled stream at once, so a cancelled request costs little; the limit keeps the cost per connection bounded.

## How to fix it

- If the client is yours: cancel fewer requests, or open new streams more slowly after cancelling. Browsers and well-known HTTP/2 libraries stay far below the limit.
- A client that keeps opening streams beyond `SETTINGS_MAX_CONCURRENT_STREAMS` has them refused, which counts too: make it respect the limit the server announces (`Server::max_concurrent_streams`, default 100).
- Otherwise the client is misbehaving or attacking, and closing the connection is the intended outcome.

[All error codes](README.md)
"#),
    (1117, r#"# GIONG-E1117: too many HTTP/2 control frames

Sent by giong-http's server to an HTTP/2 client, in the debug data of the GOAWAY frame (`ENHANCE_YOUR_CALM`) that closes the connection. In code: `giong::error::Code::Http2Flood`.

## What happened

The client sent more frames that do no work than the server allows in a short time: PING, SETTINGS and their acknowledgements, PRIORITY, empty DATA or CONTINUATION frames, WINDOW_UPDATE frames when nothing waits to be sent, other frames for streams already closed, header blocks for streams the server ignores, and frame types the server does not know. Each costs the server some work and buys the client nothing, so a stream of them is a flood (the "ping flood", "settings flood", "empty frames" and CONTINUATION attacks). The server allows a burst of 1,000 such frames and then 500 per second.

WINDOW_UPDATE frames that answer data are not counted: a client that reads an answer gives window back as the DATA frames arrive, often when the rest of the answer is already sent or its stream closed. Ten WINDOW_UPDATE frames for each DATA frame the server sent are free (Go's `net/http` sends up to eight: one for the stream and one for the connection for every 4 KiB it reads); only those beyond that count. (0.5.0 counted them all, and closed connections of clients that only read fast.)

## How to fix it

- If the client is yours: send keep-alive PINGs every few seconds at most, and SETTINGS only when something changes. Send WINDOW_UPDATE as data is consumed, a few KiB at a time or more, not on a timer and not a byte at a time.
- Otherwise the client is misbehaving or attacking, and closing the connection is the intended outcome.

[All error codes](README.md)
"#),
    (1118, r#"# GIONG-E1118: HTTP/2 header block too large

Sent by giong-http's server to an HTTP/2 client, in the debug data of the GOAWAY frame (`ENHANCE_YOUR_CALM`) that closes the connection. In code: `giong::error::Code::Http2HeadersTooLarge`.

## What happened

A header block (a HEADERS frame and the CONTINUATION frames after it) grew past the larger of the server's `max_head_size` and 16 KiB before it ended. The server must keep a block until its end to decode it, so a block that does not end is the CONTINUATION flood. The connection closes, since the HPACK state of the connection depends on the block that was cut off.

A header block within that size whose fields decode to more than `max_head_size` (which the server announces as `SETTINGS_MAX_HEADER_LIST_SIZE`) is not this error: that request gets `431 Request Header Fields Too Large` and the connection goes on.

## How to fix it

- Send smaller headers: large cookies and long custom headers are the usual cause. Or raise `Server::max_head_size` (default 64 KiB) if such headers are expected.
- A client that never ends its header block is misbehaving or attacking, and closing the connection is the intended outcome.

[All error codes](README.md)
"#),
    (1119, r#"# GIONG-E1119: invalid HTTP response

Raised by giong-http's `Response::try_header` and `Response::try_new`, for a response a handler builds that cannot be sent as written. In code: `giong::error::Code::HttpInvalidResponse`.

## What happened

One of these:

- A header name is not a token: it is empty, or has a space, a colon or another character a header name cannot have.
- A header value has a line break or another control character (a tab is allowed). A line break in a value would end the header and start another one of the client's choosing, which is how response splitting works.
- A status is outside `200..=599`. Interim responses (1xx) are sent by the server itself.

The message names the header and where the bad byte is, without repeating the value.

## How to fix it

- Check values that come from the request or from outside the program (a redirect target from a query parameter, a file name for `Content-Disposition`, a status from a backend) before putting them in a response, and answer `400 Bad Request` when they are not valid. `try_header` and `try_new` do the check: answer with your own error when they fail.
- For a status passed on from another service, map what is outside `200..=599` to `502 Bad Gateway`.
- `Response::header` and `Response::new` panic on the same input: keep them for names, values and statuses written in the code.

[All error codes](README.md)
"#),
    (1120, r#"# GIONG-E1120: request refused: the server's memory budget for requests is full

Sent by giong-http's server in the body of a `503 Service Unavailable` (with `Retry-After: 1` and `Connection: close`) over HTTP/1.1; over HTTP/2 the request's stream is reset with `REFUSED_STREAM`, which carries no text. In code: `giong::error::Code::HttpServerBusy`. Each worker also says on stderr, at most once a minute, how many requests it refused:

```text
giong-http: GIONG-E1120 refused 37 request(s) on this worker: the requests it receives already hold 33554432 bytes, its max_buffered_request_bytes; see docs/errors/E1120.md
```

## What happened

The server receives a request in full (head and body) before calling its handler, unless the handler reads the body as a stream. The requests one worker is receiving share a budget, `Server::max_buffered_request_bytes`: by default 64 MiB per worker, or a quarter of the process's cgroup memory limit divided among the workers if that is less (32 MiB for one worker in a 128 MiB container). The request would have taken the worker past it, so the server refused it without calling the handler, and without reading its body if its `Content-Length` already did not fit.

Bytes count as they arrive and until the request is dropped by its handler. Over HTTP/1.1 the first 4 KiB of each request do not count, so small requests keep being served while large uploads fill the budget. Without the budget, a few hundred clients sending large bodies slowly could make the process run out of memory (in a container: be killed by the kernel's OOM killer) and take every other client with it.

A request larger than the whole budget gets `413 Content Too Large` instead, since retrying it cannot help.

## How to fix it

- If the client is yours: retry after the `Retry-After` delay. HTTP/2 clients retry refused streams by themselves; giong-http's own client does.
- If many legitimate uploads arrive at once and the machine has the memory: raise `Server::max_buffered_request_bytes`. Size it so that the workers times this limit, plus what the connections cost on their own, fits the container's memory limit with room for the rest of the program (see "Sizing memory" in `docs/DEPLOYMENT.md`).
- For large uploads, read the body as it arrives instead of in full: return a limit from `Handler::stream_body` (giong-web: `Router::stream_bodies`) and write the pieces to a file or another service as they come. Such bodies hold one piece at a time and do not count against the budget.
- Lower `Server::max_body_size` if requests that large are not expected: the same budget then holds more requests.
- If the requests are an attack (many connections sending bodies slowly), the refusals are the intended outcome: the server stays up and keeps answering other requests. Lower `request_timeout` to free the budget sooner, and rate-limit at the load balancer.

[All error codes](README.md)
"#),
    (1121, r#"# GIONG-E1121: connection refused: the server is low on memory

Sent by giong-http's server in the body of a `503 Service Unavailable` (with `Retry-After: 1` and `Connection: close`) to a new connection without TLS; a new TLS connection is closed before its handshake instead, since nothing can be said to it before. In code: `giong::error::Code::HttpServerLowMemory`. Each worker also says on stderr, at most once a minute, how many connections it refused:

```text
giong-http: GIONG-E1121 refused 812 connection(s) on this worker: less than 51 MiB of memory is left to the process, its min_free_memory; see docs/errors/E1121.md
```

## What happened

Before it takes on a new connection, the server checks how much memory is left to the process: the smaller of what its cgroup allows (a container's memory limit, systemd's `MemoryMax`: `memory.max`, or `memory.high` if lower, less what the cgroup uses apart from page cache the kernel can reclaim) and what the machine has (`MemAvailable`; on macOS, which has no cgroups, its free and inactive pages). Less than `Server::min_free_memory` was left, so the server accepted the connection and closed it at once. The connections it already had are served as before, and new ones are taken on again as soon as memory is freed.

Every connection costs memory even while idle (about 2 KiB over HTTP/1.1, 4.5 KiB over HTTP/2, 10.5 KiB over HTTPS, and about 4 KiB of kernel socket memory, which a cgroup v2 limit counts). Without this check, enough connections fill the memory the process may use and the kernel's out-of-memory killer ends it, with every connection it holds. The default reserve is a tenth of the memory limit (of the machine's memory without a cgroup limit), at least 16 MiB and at most 256 MiB.

## How to fix it

- If the client is yours: retry after the `Retry-After` delay, preferably at another instance.
- If the connections are legitimate: the server needs more memory for them. Raise the container's memory limit, or run more instances behind the load balancer. Lowering `Server::max_connections` makes the server stop accepting before memory runs low (new clients then wait in the listen queue instead of being refused); see "Sizing memory" in `docs/DEPLOYMENT.md`.
- If memory is held by something other than connections (a cache, a large handler), find and bound it: the refusals only protect the process, they do not free memory.
- If the program allocates large amounts outside its connections and those allocations must still find room, raise `Server::min_free_memory`. `0` turns the check off, which brings back the out-of-memory kill.
- If the connections are an attack (many idle connections), the refusals are the intended outcome: the server stays up and keeps serving its other clients. Lower `idle_timeout` to free memory sooner, and rate-limit connections at the load balancer.

[All error codes](README.md)
"#),
    (1122, r#"# GIONG-E1122: invalid HTTP trailer field

Raised by giong-http's `Trailers::field`, for a trailer field that cannot be sent as written. In code: `giong::error::Code::HttpInvalidTrailer`.

## What happened

One of these:

- The field name is not a token: it is empty, or has a space, a colon or another character a field name cannot have.
- The field is one that means nothing after a body, because it describes the framing, routing, credentials or connection of the message: `content-length`, `transfer-encoding`, `content-encoding`, `content-type`, `content-range`, `trailer`, `te`, `host`, `connection`, `keep-alive`, `proxy-connection`, `upgrade`, `authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `cache-control`, `expect`, `max-forwards`, `range`.
- The value has a line break or another control character (a tab is allowed). A line break in a value would end the field and start another one.

The message names the field and where the bad byte is, without repeating the value.

## How to fix it

- Send a field about the message itself as a header, before the body.
- Encode a value that can hold any byte before putting it in a trailer field: percent-encoding for text (as gRPC does for `grpc-message`), base64 for binary data.
- Check values that come from outside the program, and end the body without the field (or abort it) when they are not valid.

[All error codes](README.md)
"#),
    (1123, r#"# GIONG-E1123: connection not accepted: the server is out of file descriptors

Said on stderr by giong-http's server, at most once a minute on each worker, when it could not accept a new connection because the process holds as many open files as it may. In code: `giong::error::Code::HttpServerOutOfFiles`.

```text
giong-http: GIONG-E1123 could not accept 37 connection(s) on this worker: Too many open files (os error 24) (open files: 1024, hard limit 1024); new clients wait in the listen queue until a connection closes. Raise the hard limit on open files (docker run --ulimit nofile=, LimitNOFILE= in systemd) or lower max_connections; see docs/errors/E1123.md
```

## What happened

Every connection is a file descriptor, and so is every open file, socket and pipe of the process. `accept` failed with `EMFILE`: the process holds as many as its soft `RLIMIT_NOFILE` (`ulimit -n`) allows. Or, rarer, with `ENFILE`: the whole system holds as many as `fs.file-max` allows. The server keeps serving the connections it has and tries again 50 ms later; new clients wait in the listen queue, and those that wait longer than their own timeout give up. Nothing else reports it: the clients see a connection that never answers.

On Linux, giong raises the soft limit to the hard one when a runtime is built (`Builder::raise_open_files_limit`), so the limit that was reached is the hard one, or a soft one the program set after that. containerd starts containers at 1,024 soft and 524,288 hard, which giong raises past; a hard limit of 1,024 (`docker run --ulimit nofile=1024:1024`, `LimitNOFILE=1024`) stays.

## How to fix it

- If the server should hold more connections: raise the hard limit where the process starts: `docker run --ulimit nofile=65536:65536`, `ulimits` in Docker Compose, `LimitNOFILE=65536` in a systemd unit, `/etc/security/limits.conf` for a login shell. In Kubernetes the container runtime sets it for every container on the node.
- If the program lowered its own soft limit, or turned off `raise_open_files_limit`, raise the soft limit or leave it to giong.
- To stop accepting before the limit, lower `Server::max_connections` (per worker) so that the workers' connections and the program's other files fit under it: new clients then wait in the listen queue by design, and the program's own files (database connections, log files) still open.
- If the descriptors are not connections (a leak: files or sockets the program opens and never closes), find them: `ls -l /proc/<pid>/fd`, or giong-mcp's `overview`, which warns when four fifths of the limit are open.
- `ENFILE`: the machine as a whole is out of file descriptors; raise `fs.file-max`, or find the process that holds them.

[All error codes](README.md)
"#),
    (1201, r#"# GIONG-E1201: invalid route pattern

Raised by giong-web as a panic, when a route is added to a `Router` (`get`, `post`, `route`, `nest`...). The panic points at the line that added it. In code: `giong::error::Code::WebInvalidRoute`.

## What happened

The pattern is not one the router understands. The message quotes the pattern and says what is wrong with it: it does not start with `/`, it has an empty segment (`//`), a segment mixes text and a parameter (`/files/{name}.json`), a parameter name is not letters, digits and `_`, a name appears twice, a catch-all is not the last segment, or a segment has characters that a path cannot hold unencoded. `nest` also refuses a prefix that ends with `/` or with a catch-all.

## How to fix it

- Write each segment as plain text, one parameter `{name}`, or, last, one catch-all `{*name}`: `/users/{id}`, `/files/{*path}`.
- Write text as it is, not percent-encoded: `/café` cannot be a route, `/cafe` can. The router decodes `%XX` in the request's path before comparing.
- To match a file extension, take the whole segment as a parameter (`/files/{name}`) and check the ending in the handler.
- Give nested routers a prefix without a trailing slash: `nest("/api", router)`. The nested router's `/` route then answers `/api`.
- A pattern has at most 16 parameters.

[All error codes](README.md)
"#),
    (1202, r#"# GIONG-E1202: route added twice

Raised by giong-web as a panic, when a route is added to a `Router` that already has one with the same method and pattern. In code: `giong::error::Code::WebDuplicateRoute`.

## What happened

Two routes would answer exactly the same requests: same method, and the same pattern once parameter names are left out (`/users/{id}` and `/users/{name}` are the same). The message names both and where the first one was added. It also happens when `nest` brings in a route that the outer router has already.

## How to fix it

- Remove one of the two routes, or give them different paths.
- Different parameter names at the same place do not make different routes: the router cannot tell `{id}` from `{name}` in a path. Use one route and read the parameter as one type, or use different text before it (`/users/by-id/{id}`, `/users/by-name/{name}`).
- Routes that differ only in the method are fine: `get("/users", ...)` and `post("/users", ...)`.

[All error codes](README.md)
"#),
    (1203, r#"# GIONG-E1203: no route for the path

Raised by giong-web for a request whose path matches no route: answered with `404 Not Found`. In code: `giong::error::Code::WebNoRoute`.

## What happened

No route's pattern matches the request's path, whatever the method. Paths are matched exactly: a trailing slash makes a different path, case matters, and the query string is not part of the path. When the same path with or without a trailing slash has a route, the detail says so (`did you mean /users?`).

## How to fix it

- For the client: check the path against the service's routes, including the trailing slash and the version prefix (`/api/v1/...`).
- For the service: make sure the route is added to the router that is built and served, with the prefix you expect if it sits in a nested router.

[All error codes](README.md)
"#),
    (1204, r#"# GIONG-E1204: method not allowed

Raised by giong-web for a request whose path has routes but none for its method: answered with `405 Method Not Allowed`. In code: `giong::error::Code::WebMethodNotAllowed`.

## What happened

The path matches at least one route, but none of them is for the request's method. The response's `Allow` header lists the methods the path does take; `HEAD` is answered by a `GET` route and `OPTIONS` by the router itself, so they are always listed where they apply.

## How to fix it

- For the client: use one of the methods in `Allow`.
- For the service: add a route for the method, such as `.put("/users/{id}", update)`.

[All error codes](README.md)
"#),
    (1205, r#"# GIONG-E1205: invalid path or query parameter

Raised by giong-web's `Req::param`, `Req::query_param` and `Req::query`: answered with `400 Bad Request`. In code: `giong::error::Code::WebBadParameter`.

## What happened

A path or query parameter could not be read as the type the handler asked for, a required query parameter is missing, or the value is not valid percent-encoded UTF-8. The detail names the parameter, quotes the value (cut at 64 characters) and says what was expected, such as ``path parameter `id`: cannot read "abc" (invalid digit found in string); expected an unsigned integer``.

## How to fix it

- For the client: send the parameter in the form the detail asks for. Percent-encode reserved characters in values (`%20` or `+` for a space in a query string, `%2F` for a `/` inside a path parameter).
- For the service: an `Option` field or `query_param` returning `None` makes a parameter optional; use `unwrap_or` for a default. With `Req::query`, a key given twice is an error, and lists are not supported.

[All error codes](README.md)
"#),
    (1206, r#"# GIONG-E1206: the route has no such parameter

Raised by giong-web's `Req::param`: answered with `500 Internal Server Error`. The client gets only the code; the server's log has the route and the name. In code: `giong::error::Code::WebNoSuchParameter`.

## What happened

The handler asked for a path parameter that the route which matched does not have: usually a misspelt name (`req.param("idd")` for `/users/{id}`), or a handler shared by several routes that do not all have the parameter. This is a bug in the service, not in the request.

## How to fix it

- Use one of the names in braces in the route's pattern. The log line names the pattern.
- A handler used by several routes can check `req.route()` before asking, or take the value from the query string instead.

[All error codes](README.md)
"#),
    (1207, r#"# GIONG-E1207: unreadable request body

Raised by giong-web's `Req::json` (feature `serde`): answered with `400 Bad Request` for a body that is not valid JSON, and `422 Unprocessable Content` for valid JSON of the wrong shape. In code: `giong::error::Code::WebBadBody`.

## What happened

The body could not be turned into the type the handler expects. The detail quotes serde_json with the line and column: a syntax error or a body cut short (`400`), or a missing field, an unknown variant, a string where a number belongs (`422`).

## How to fix it

- For the client: send JSON that matches the API's schema: every required field, each with its type. An empty body is not valid JSON; send `{}` for an object with only optional fields.
- For the service: make fields that may be absent `Option`, or give them `#[serde(default)]`. Bodies larger than the server's `max_body_size` (1 MiB by default) are refused with `413` before the handler runs.

[All error codes](README.md)
"#),
    (1208, r#"# GIONG-E1208: unsupported Content-Type

Raised by giong-web's `Req::json` (feature `serde`): answered with `415 Unsupported Media Type`. In code: `giong::error::Code::WebUnsupportedMediaType`.

## What happened

The handler reads the body as JSON, but the request's `Content-Type` is not a JSON type, or it has none. `application/json`, with or without parameters such as `charset=utf-8`, and `application/*+json` types (such as `application/merge-patch+json`) are accepted.

## How to fix it

- For the client: send `Content-Type: application/json` with a JSON body. Some tools default to `application/x-www-form-urlencoded` (`curl -d`): add `-H 'Content-Type: application/json'`, or use `curl --json`.

[All error codes](README.md)
"#),
    (1209, r#"# GIONG-E1209: no response in time

Raised by giong-web's `Timeout` middleware: answered with `503 Service Unavailable`. In code: `giong::error::Code::WebTimeout`.

## What happened

The handler, with the middleware after `Timeout`, did not produce a response within the limit given to `Timeout::new`. Its future was dropped, which cancels whatever it was waiting for (a query, a call to another service). The detail gives the limit.

## How to fix it

- For the client: try again later, with a backoff.
- For the service: find what the handler waits for, such as a slow query, a pool with no free connection, or an upstream API; the log line of the request shows the route and the time. Give the dependencies their own, shorter timeouts so that their errors are clearer than this one, and raise the limit only for routes that really need longer (with a nested router and its own `Timeout`).

[All error codes](README.md)
"#),
    (1210, r#"# GIONG-E1210: the handler failed

Raised by giong-web when a handler or middleware returns an error that is not meant for the client (any error turned into `giong_web::Error` with `?`, or `Error::internal`): answered with `500 Internal Server Error`. In code: `giong::error::Code::WebInternal`.

## What happened

The handler failed for a reason of its own: a bug, a query error, a file that is missing. The client gets this code and a generic detail; the cause is written to the log, in the `Logger` middleware's line for the request, or, without a logger, in an `error` line on standard error. Both carry the request id when the `RequestId` middleware runs.

## How to fix it

- For the client: report the failure with the request id from the response (`X-Request-Id` and the body's `request_id`).
- For the service: find the request's line in the log by its id and read `error.cause`. Errors the client can do something about deserve their own status and code, such as `Error::not_found("user_not_found", ...)` or `Error::conflict(...)`, instead of becoming a `500`.

[All error codes](README.md)
"#),
    (1211, r#"# GIONG-E1211: a service the handler needs is unavailable

Raised by giong-web when a handler returns, with `?`, an error that means a dependency is unreachable or overloaded, or calls `Error::unavailable`: answered with `503 Service Unavailable` and `Retry-After: 1`. In code: `giong::error::Code::WebUnavailable`.

## What happened

A database, cache or other HTTP service the handler needs did not answer in time or could not be reached: giong's codes for a failed connection, a closed connection, a pool with no free connection, or a timeout (`GIONG-E1102`, `E1103`, `E3002`, `E3008`, `E3009`, `E3102`, `E3108`, `E3109`, `E3202`, `E3208`, `E3209`, `E3211`). The same request may well work moments later, so the client is told to retry. The cause goes to the log, never to the client.

## How to fix it

- For the client: retry after the time in `Retry-After`, with a backoff.
- For the service: the log's `error.cause` names the dependency and its own code, whose page says what to do: check that it runs and is reachable, and size its pool for the load.

[All error codes](README.md)
"#),
    (1212, r#"# GIONG-E1212: cross-origin request refused

Raised by giong-web's `Cors` middleware for a CORS preflight request: answered with `403 Forbidden`. In code: `giong::error::Code::WebCorsRejected`.

## What happened

A browser asked, with an `OPTIONS` preflight, whether a page from another origin may send a request, and the answer is no: the page's origin is not allowed, or the method, or one of the headers it wants to send. The detail says which. Browsers do not show this response to the page, only in the developer tools, and then block the request.

## How to fix it

- For the service: add the page's origin with `Cors::allow_origin` (exactly as the browser sends it: scheme, host and port, no path), the method with `allow_methods`, and the header with `allow_headers` (`content-type` for JSON bodies, `authorization` for bearer tokens).
- For a public API without cookies, `Cors::allow_any_origin()` allows every origin.

[All error codes](README.md)
"#),
    (1213, r#"# GIONG-E1213: invalid giong-web setting

Raised by giong-web as a panic, when a middleware is built with settings that cannot work: `Cors`, `Compression`, `Timeout`, `RequestId`. The panic points at the line. In code: `giong::error::Code::WebConfig`.

## What happened

A setting is out of range or contradicts another, so the middleware could never behave as written: `Cors` allowing every origin with credentials (browsers refuse it, and it would let any site act as the signed-in user), an origin with a path or a trailing slash, the `null` origin; a compression level outside 1 to 9; a zero timeout; a header name that is not one. The message names the setting.

## How to fix it

- Follow the message's fix. For CORS with cookies, list the origins with `allow_origin` instead of `allow_any_origin`.
- Write origins as the browser sends them: `https://app.example.com`, `http://localhost:5173`.

[All error codes](README.md)
"#),
    (1301, r#"# GIONG-E1301: the MCP server listens on this machine only

Raised by giong-mcp, when the server starts. In code: `giong::error::Code::McpNotLocal`.

## What happened

The MCP server was given an address that other machines could reach, such as `0.0.0.0:7777` or the machine's own network address. giong-mcp lets whoever connects read the internals of the service (tasks and where they were spawned, connection pools, profiles), so it only listens on a loopback address (`127.0.0.1`, `::1`, `localhost`) or a Unix socket, and refuses anything else.

## How to fix it

- Listen on loopback: `GIONG_MCP=127.0.0.1:7777`, or `giong_mcp::Config::tcp("127.0.0.1:7777")`.
- To reach a service on another machine or in a container, forward the port instead of opening it: `ssh -L 7777:127.0.0.1:7777 host`, or `kubectl port-forward pod/api 7777:7777`. In a container, listen on a Unix socket in a mounted volume (`GIONG_MCP=unix:/run/giong/mcp.sock`).

[All error codes](README.md)
"#),
    (1302, r#"# GIONG-E1302: invalid address for the MCP server

Raised by giong-mcp, when the server starts. In code: `giong::error::Code::McpBadAddress`.

## What happened

The address given to the MCP server, in code or in the `GIONG_MCP` environment variable, cannot be read. giong-mcp takes a loopback IP address with a port (`127.0.0.1:7777`, `[::1]:7777`), `localhost:7777`, or `unix:` followed by the path of a socket file (`unix:/run/giong/mcp.sock`). Host names other than `localhost` are not resolved.

## How to fix it

- Write the address in one of the forms above. Port `0` picks a free port; the server's `url()` says which.
- `GIONG_MCP` empty or unset leaves the server off.

[All error codes](README.md)
"#),
    (1303, r#"# GIONG-E1303: the MCP server cannot listen on its address

Raised by giong-mcp, when the server starts. In code: `giong::error::Code::McpBind`. The kernel's error kind is kept: `AddrInUse` when another process holds the address, `PermissionDenied` or `AddrNotAvailable` when the kernel refuses it.

## What happened

giong-mcp, the service's debugging endpoint (turned on by `GIONG_MCP` or `giong_mcp::Config`), could not listen on the address it was given. The message names the address and the kernel's reason. The usual cause is another process on the same address: a second copy of the service started with the same `GIONG_MCP`, an old process still finishing during a restart, or another program on that port. For a Unix socket, another live server is listening on that path. On Linux 6.0.19 the message says the kernel reported no error: that kernel answers a `bind` to a busy TCP address with success and leaves the socket unbound, so giong-mcp checks where it ended up listening and stops here rather than serve on another port of every interface.

The service's own port is not involved: only the MCP server failed.

## How to fix it

- Give each process its own address: another port, port `0` to let the kernel pick a free one (the server's URL says which), or a Unix socket path per process (`GIONG_MCP=unix:/run/giong/api-1.sock`).
- Find what holds the port with `ss -ltnp 'sport = :7777'`, and stop it if it should not be there.
- Unset `GIONG_MCP` where the endpoint is not needed. `giong_mcp::start_from_env` reports this error and lets the service run without the endpoint; code that calls `Config::start` decides for itself.

[All error codes](README.md)
"#),
    (1304, r#"# GIONG-E1304: the MCP server needs a token on TCP

Raised by giong-mcp, when the server starts. In code: `giong::error::Code::McpNoToken`.

## What happened

giong-mcp was asked to listen on a TCP address (`GIONG_MCP=127.0.0.1:7777`, or `giong_mcp::Config::tcp`) without a token. Every process on the machine can connect to a loopback port, whichever user runs it, and so can every container that shares the pod's network. Without a token, any of them could read the service's internals (tasks and where they were spawned, connection pools, memory figures) and run its profiler. So a TCP server needs a token, and every request must then carry `Authorization: Bearer <token>`.

## How to fix it

- Set a token of at least 16 characters, and give the same one to the MCP client:

  ```sh
  export GIONG_MCP=127.0.0.1:7777 GIONG_MCP_TOKEN=$(openssl rand -hex 16)
  claude mcp add --transport http giong http://127.0.0.1:7777/mcp \
      --header "Authorization: Bearer $GIONG_MCP_TOKEN"
  ```

  In code: `Config::tcp("127.0.0.1:7777").token(secret)`.
- Or listen on a Unix socket (`GIONG_MCP=unix:/run/giong/mcp.sock`): its file is created readable and writable by the service's user only, so it needs no token.
- Or, where every local process is trusted (a development machine), say so in code: `Config::tcp(addr).allow_unauthenticated()`.

[All error codes](README.md)
"#),
    (1305, r#"# GIONG-E1305: invalid token for the MCP server

Raised by giong-mcp, when the server starts. In code: `giong::error::Code::McpBadToken`.

## What happened

The token given to the MCP server (`GIONG_MCP_TOKEN`, or `giong_mcp::Config::token`) cannot protect it:

- It is empty. An empty token would let in every request without an `Authorization` header, so it is refused instead of turning authentication off. A common cause is `GIONG_MCP_TOKEN=$SOME_VAR` with the variable unset.
- It is shorter than 16 characters, short enough to guess.
- It has a character a bearer token cannot carry in an HTTP header (RFC 6750 allows letters, digits and `-._~+/`, with `=` only at the end), such as a space or a newline.

## How to fix it

- Generate one: `openssl rand -hex 16` gives 32 characters, and `head -c 24 /dev/urandom | base64` 32 more. Give the MCP client the same token in `Authorization: Bearer <token>`.
- To run without a token, leave `GIONG_MCP_TOKEN` unset and listen on a Unix socket, or, on a trusted machine, call `Config::allow_unauthenticated()` in code.

[All error codes](README.md)
"#),
    (1401, r#"# GIONG-E1401: an earlier buffered write failed

Raised by giong-compat, on `Compat` writes, flushes and shutdowns (`poll_write`, `poll_flush`, `poll_shutdown`, `poll_close`). In code: `giong::error::Code::CompatWriteFailed`.

## What happened

`Compat` buffers what is written through the Tokio or futures-io traits and sends it with giong's own `write_all`, the way `BufWriter` does. One of those sends failed, for example because the peer reset the connection, or a shutdown did (a TLS stream failing to send its `close_notify`). The first call after the failure returned the error itself (such as `ConnectionReset` or `BrokenPipe`); every later write, flush or shutdown returns this code, with that error in the message. Bytes accepted since the last flush that succeeded may not have reached the peer.

Reading is not affected: whatever the peer sent before the failure can still be read.

## How to fix it

- Treat the connection as broken: drop it and open a new one. Resend what the protocol says must be resent.
- Call `flush` (`AsyncWriteExt::flush`) at the points where you need to know that the data was sent; a failure then shows up there and not at a later write.

[All error codes](README.md)
"#),
    (1402, r#"# GIONG-E1402: write after shutdown

Raised by giong-compat, on `Compat` writes (`poll_write`, `poll_write_vectored`). In code: `giong::error::Code::CompatShutDown`.

## What happened

The sending direction of the stream was already shut down, or is being shut down (a TLS stream sending its `close_notify`), by `AsyncWriteExt::shutdown` (Tokio) or `AsyncWriteExt::close` (futures-io), or by a library that calls them when it finishes, such as a TLS or WebSocket close. Nothing more can be sent on this stream. Reading still works until the peer closes its side.

## How to fix it

- Shut down only after the last write. If a library closes the stream for you, do not write to the stream after handing it over.
- To keep sending, open a new connection.

[All error codes](README.md)
"#),
    (1403, r#"# GIONG-E1403: invalid giong-compat setting

Raised by giong-compat as a panic, when a `Compat` is configured with a value out of range. The panic points at the line. In code: `giong::error::Code::CompatConfig`.

## What happened

`Compat::write_buffer` was given a limit outside 1 byte to 1 GiB. The limit is how many written bytes `Compat` holds before a write has to wait for a send to finish; it must be finite and not zero.

## How to fix it

- Leave the default (`giong_compat::DEFAULT_WRITE_BUFFER`, 64 KiB) unless a measurement says otherwise.
- Use a smaller limit (16 KiB) for many mostly idle connections, a larger one (256 KiB to 1 MiB) for a few connections that stream large amounts.

[All error codes](README.md)
"#),
    (2001, r#"# GIONG-E2001: no trusted certificate authorities

Raised by giong-tls, `TlsConnector`. In code: `giong::error::Code::NoTrustedRoots`.

## What happened

There is no certificate authority to check servers against: `TlsConnector::with_system_roots` found none on this machine (minimal container images often have none), or the deprecated `with_roots` was given an empty store.

## How to fix it

- Install the system's CA certificates (`ca-certificates` on Debian, Ubuntu and Alpine).
- Or point `SSL_CERT_FILE` at a PEM bundle, or `SSL_CERT_DIR` at a directory of them.
- Or use `TlsConnector::with_webpki_roots` (feature `webpki-roots`), which compiles Mozilla's list into the program.

[All error codes](README.md)
"#),
    (2002, r#"# GIONG-E2002: unreadable certificate data

Raised by giong-tls, loading certificates and keys. In code: `giong::error::Code::BadCertificateData`.

## What happened

A certificate, a certificate file or a private key cannot be used: it is not PEM, holds no certificate, cannot be read, or the key does not belong to the certificate.

## How to fix it

- Check that the data is PEM: blocks starting with `-----BEGIN CERTIFICATE-----` and, for keys, `-----BEGIN PRIVATE KEY-----` (or the RSA or EC forms).
- For a certificate chain, the server's own certificate comes first and the key must be its key.
- If `SSL_CERT_FILE` is set, check that it names a readable file.

[All error codes](README.md)
"#),
    (2003, r#"# GIONG-E2003: invalid TLS server name

Raised by giong-tls, `TlsConnector::connect`. In code: `giong::error::Code::InvalidServerName`.

## What happened

The server name given to `connect` is neither a DNS name nor an IP address. It is the name the server's certificate must be valid for, so it must be the bare host.

## How to fix it

- Pass the host name the certificate is for (`"example.com"`), or an IP address; no port, no scheme, no path.

[All error codes](README.md)
"#),
    (2004, r#"# GIONG-E2004: certificate rejected

Raised by giong-tls, during the handshake. In code: `giong::error::Code::TlsCertificate`.

## What happened

The peer's certificate was rejected. The message says why: `UnknownIssuer` (not issued by a trusted authority), `Expired` or `NotValidYet`, `NotValidForName` (issued for another name), a certificate marked as a certificate authority presented as the peer's own (`CaUsedAsEndEntity`), or another reason from rustls.

## How to fix it

- Check the name you connect to: it must be one the certificate lists.
- Check the certificate's dates and the machine's clock.
- A private or self-signed authority has to be trusted explicitly: `TlsConnector::with_root_pem`.
- A self-signed certificate for a server must not be marked as an authority, and `openssl req -x509` marks it as one (`CA:TRUE`) unless told otherwise. curl and browsers accept such a certificate once it is trusted; rustls, and so giong, does not. Make it with `-addext "basicConstraints=critical,CA:FALSE"`:

  ```sh
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout key.pem -out cert.pem -days 30 -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost" -addext "basicConstraints=critical,CA:FALSE"
  ```
- A server must send its whole chain except the root.

[All error codes](README.md)
"#),
    (2005, r#"# GIONG-E2005: TLS protocol error

Raised by giong-tls. In code: `giong::error::Code::TlsProtocol`.

## What happened

The TLS session failed: the peer sent something that is not valid TLS, or sent an alert (for example because it rejected our certificate or found no common protocol version or cipher suite).

## How to fix it

- Check that the other side speaks TLS on this port (not plain HTTP, and not a protocol that starts in clear text).
- Read the alert in the message: `HandshakeFailure` usually means no common protocol version or cipher suite; `CertificateRequired` or `BadCertificate` that the server wants a different client certificate.

[All error codes](README.md)
"#),
    (2006, r#"# GIONG-E2006: TLS handshake timed out

Raised by giong-tls, `TlsAcceptor::accept` and `TlsConnector::connect`. In code: `giong::error::Code::TlsHandshakeTimeout`.

## What happened

The TLS handshake did not finish within the handshake timeout (10 seconds unless set with `handshake_timeout`).

## How to fix it

- Check that the other side speaks TLS on this port: a plain TCP server never answers a ClientHello.
- Check for an overloaded peer or a slow network, and raise `handshake_timeout` if the limit is too tight.

[All error codes](README.md)
"#),
    (2007, r#"# GIONG-E2007: connection closed during the TLS handshake

Raised by giong-tls, during the handshake. In code: `giong::error::Code::TlsHandshakeEof`.

## What happened

The peer closed the connection before the TLS handshake finished.

## How to fix it

- Check that the other side speaks TLS on this port.
- Check whether the server wants a client certificate (`TlsConnector::client_auth`) or a different server name (SNI).

[All error codes](README.md)
"#),
    (2008, r#"# GIONG-E2008: TLS stream broken

Raised by giong-tls, `TlsStream` writes. In code: `giong::error::Code::TlsBroken`.

## What happened

An earlier write on this TLS stream failed or was cancelled half-way (its future was dropped, for example by a timeout). Part of a TLS record may have been sent, so nothing more can be sent on this stream.

The same code says that the stream was shut down (`shutdown()` sent `close_notify`) or ended with an alert, so it cannot send any more.

## How to fix it

- Open a new connection.
- Write everything before calling `shutdown()`.
- Avoid cancelling writes half-way: put the timeout around the whole exchange and drop the stream when it fires.

[All error codes](README.md)
"#),
    (2009, r#"# GIONG-E2009: kernel TLS is not available

Raised by giong-tls, by `TlsAcceptor::ktls` and `TlsConnector::ktls` with `Ktls::Required`, and by a handshake on a connector or acceptor set up that way. In code: `giong::error::Code::KtlsUnavailable`.

## What happened

Kernel TLS (kTLS) was required, and this machine or this connection cannot use it. The message says which of these it was:

- The kernel has no `tls` module, or it cannot be loaded here. Containers cannot load kernel modules, and some kernels are built without `CONFIG_TLS`.
- The kernel cannot encrypt or decrypt one of the cipher suites the TLS configuration allows (ChaCha20-Poly1305 needs `CONFIG_CRYPTO_CHACHA20POLY1305`).
- The socket refused the `tls` upper-layer protocol.
- `TcpStream::recv` was already used on the TCP stream before the handshake, for example to read a plaintext greeting. giong may then hold bytes that arrived after the greeting, which the kernel would never see, so the kernel cannot take the stream. Read such a greeting with `TcpStream::read`, which kTLS allows.

`giong_tls::ktls_support()` tells what the kernel can do, in one line for a log.

## How to fix it

- Load the module on the host: `modprobe tls` (add `tls` to `/etc/modules-load.d/` to keep it). In Kubernetes, load it on the node; a container cannot.
- Use `Ktls::Auto`, which uses kTLS where the kernel can take the connection and TLS in the process elsewhere, never failing because of kTLS.
- Limit the configuration to cipher suites the kernel supports, with a custom rustls `ServerConfig` or `ClientConfig`.

[All error codes](README.md)
"#),
    (2010, r#"# GIONG-E2010: the kernel refused the TLS keys

Raised by giong-tls, at the end of a handshake that hands the connection to kernel TLS. In code: `giong::error::Code::KtlsSetup`.

## What happened

The handshake succeeded, then `setsockopt(SOL_TLS, TLS_TX or TLS_RX)` failed, so the kernel did not take the connection's keys. The keys have already left rustls, so the connection cannot go on in the process either: it was closed. The message has the kernel's error.

giong-tls checks at start-up that the kernel accepts every cipher suite the configuration allows, so this should only happen when the kernel runs out of memory or a security policy (seccomp, an LSM) blocks the call on live connections only.

The same code is raised by `write_all` and `recv` when a TLS 1.3 key update fails: the kernel refused the new keys. A network card that encrypts TLS itself (kTLS device offload) may not take new keys on a live connection. Nothing more can be sent on that connection.

## How to fix it

- Check the kernel log and memory pressure.
- Use `Ktls::Off` (the default) if it keeps happening, and report it at https://github.com/giong-run/Giong/issues with the message and `giong_tls::ktls_support()`.

[All error codes](README.md)
"#),
    (2011, r#"# GIONG-E2011: the kernel cannot change TLS keys

Raised by giong-tls, `TlsStream::recv` on a connection encrypted by the kernel. In code: `giong::error::Code::KtlsKeyUpdate`.

## What happened

The peer sent a TLS 1.3 `KeyUpdate`: from that record on it encrypts with new keys. The kernel that decrypts this connection cannot take new keys (that needs Linux 6.14 or newer), so the rest of the stream cannot be read and the connection was closed.

This only happens with `Ktls::Required`. `Ktls::Auto` uses kernel TLS for TLS 1.3 only on kernels that can change keys, so this cannot happen there.

On a TLS 1.2 connection the message says that the kernel took a piece of a handshake message for a change of keys. Linux 6.14 and newer read the first byte of every handshake record, and after a 24 (`KeyUpdate`) receive nothing more until they are given new keys, also when the record is a later piece of a message split over records. On TLS 1.3 giong-tls gives the kernel its keys back and the connection goes on. TLS 1.2 has no key updates and the kernel takes no keys twice there, so the connection is lost. Only a peer that asks to renegotiate with a `ClientHello` split over records can cause it, and giong-tls refuses renegotiation in any case.

## How to fix it

- Run Linux 6.14 or newer.
- Use `Ktls::Auto`, which keeps TLS 1.3 connections in the process on older kernels.
- For the TLS 1.2 case: open a new connection. `Ktls::Off` keeps connections in the process, where a request to renegotiate is answered with a warning alert and the connection goes on.

[All error codes](README.md)
"#),
    (2012, r#"# GIONG-E2012: TLS keys used up

Raised by giong-tls, `TlsStream` writes on a TLS 1.2 connection, or on a TLS 1.3 connection encrypted by a kernel older than Linux 6.14. In code: `giong::error::Code::TlsKeysExhausted`.

## What happened

An AES-GCM key may only encrypt so many TLS records before its security weakens (2^24 records in rustls, about 16.7 million). On TLS 1.3 giong-tls then changes keys with a `KeyUpdate`, as rustls does. This connection could not: it uses TLS 1.2, which has no way to change keys, or a kernel older than Linux 6.14 encrypts it. As rustls does in that case, the connection sent `close_notify` and refuses to send more. The write that met the limit fails with this code, and so does every later one: what it did not report as sent was not sent.

A connection that sends many small records over a long time, such as a busy keep-alive or database connection, can reach the limit in hours.

## How to fix it

- Open a new connection; HTTP clients and connection pools do this when a connection ends.
- Prefer TLS 1.3, and Linux 6.14 or newer when the kernel encrypts the connection.

[All error codes](README.md)
"#),
    (2013, r#"# GIONG-E2013: TLS setting not possible here

Raised by giong-tls, `TlsAcceptor::require_client_cert` and `TlsAcceptor::request_client_cert`. In code: `giong::error::Code::TlsConfig`.

## What happened

The acceptor was made with `TlsAcceptor::from_rustls`, from a rustls configuration built elsewhere. giong-tls follows such a configuration as given and does not rebuild it, so it cannot add client authentication to it.

## How to fix it

- Make the acceptor with `TlsAcceptor::with_certificates`, then call `require_client_cert` or `request_client_cert`.
- Or keep `from_rustls`, and set client authentication in the rustls configuration: `ServerConfig::builder().with_client_cert_verifier(...)`.

[All error codes](README.md)
"#),
    (3001, r#"# GIONG-E3001: invalid PostgreSQL configuration

Raised by giong-postgres, building a `Config` or parsing a URL. In code: `giong::error::Code::PostgresConfig`.

## What happened

The configuration or the connection URL is not valid. The message names the setting.

## How to fix it

- The URL form is `postgres://user:password@host:port/database?sslmode=...`; percent-encode special characters in the password.
- `sslmode=prefer` and `allow` are refused on purpose (they silently fall back to clear text): use `disable`, `require`, `verify-ca` or `verify-full`.

[All error codes](README.md)
"#),
    (3002, r#"# GIONG-E3002: cannot connect to PostgreSQL

Raised by giong-postgres, opening a connection. In code: `giong::error::Code::PostgresConnect`.

## What happened

Opening the connection failed before authentication: name resolution, the TCP connection, TLS, or the connect timeout (10 seconds by default). The cause is in the message. An unknown database (SQLSTATE `3D000`) is reported with this code too.

## How to fix it

- Check the host, the port and the database name, and that the server accepts connections from here (`listen_addresses`, firewalls).
- For TLS errors, see the TLS code in the message (`GIONG-E2xxx`).
- If the server is slow to accept, raise `Config::connect_timeout`.

[All error codes](README.md)
"#),
    (3003, r#"# GIONG-E3003: PostgreSQL authentication failed

Raised by giong-postgres, authentication. In code: `giong::error::Code::PostgresAuth`.

## What happened

The server refused the credentials, or asked for an authentication method giong-postgres does not support. A clear-text password is also refused when it would cross the network without TLS.

## How to fix it

- Check the user name and the password; the server log says which rule in `pg_hba.conf` matched.
- Use `scram-sha-256` in `pg_hba.conf`; `md5` and `trust` also work. GSSAPI, SSPI and certificate authentication are not supported yet.
- If the server asks for a clear-text password, turn on TLS (`sslmode=require` or stricter).

[All error codes](README.md)
"#),
    (3004, r#"# GIONG-E3004: PostgreSQL server error

Raised by giong-postgres, running a statement. In code: `giong::error::Code::PostgresServer`.

## What happened

The server reported an error for a statement. The message contains the server's text, and `Error::server_code()` returns its SQLSTATE, such as `23505` for a unique violation or `40001` for a serialization failure.

## How to fix it

- Look the SQLSTATE up in the PostgreSQL manual (appendix A, "PostgreSQL Error Codes").
- Match on `Error::server_code()` in code, not on the message, which the server may translate.
- `40001` and `40P01` mean the transaction should be retried from the start.
- A message saying that a statement of the transaction was given up comes from giong-postgres rather than the server (`Error::server_code()` is `None`): a statement in the transaction was dropped before its answer came and may have run, so the transaction may hold work the application believes undone, and this statement (or `COMMIT`) was not sent. Drop the transaction, which rolls it back, and run it again from the start.

[All error codes](README.md)
"#),
    (3005, r#"# GIONG-E3005: PostgreSQL type mismatch

Raised by giong-postgres, parameters and `Row::get`. In code: `giong::error::Code::PostgresType`.

## What happened

A value could not be converted between Rust and PostgreSQL: a parameter of a type the column does not take, a column read as the wrong Rust type, or a NULL read into a type that cannot hold it.

## How to fix it

- The message names the parameter or column and the types that would work.
- Read a column that can be NULL as `Option<T>`.

[All error codes](README.md)
"#),
    (3006, r#"# GIONG-E3006: wrong number of rows

Raised by giong-postgres, `query_one`. In code: `giong::error::Code::PostgresRowCount`.

## What happened

`query_one` expects exactly one row, and the statement returned none or more than one.

## How to fix it

- Use `query_opt` when the row may be missing, and `query` when there can be several.
- Add a `LIMIT 1` or a unique condition if exactly one row is meant.

[All error codes](README.md)
"#),
    (3007, r#"# GIONG-E3007: PostgreSQL result too large

Raised by giong-postgres, reading a result. In code: `giong::error::Code::PostgresTooLarge`.

## What happened

A result was larger than `Config::max_result_size` (64 MiB by default). The limit keeps one query from using all of a worker's memory.

## How to fix it

- Fetch fewer rows at a time (`LIMIT` and `OFFSET`, or keyset pagination).
- Raise `Config::max_result_size` if the result really has to be this big.

[All error codes](README.md)
"#),
    (3008, r#"# GIONG-E3008: PostgreSQL connection closed

Raised by giong-postgres, any statement. In code: `giong::error::Code::PostgresClosed`.

## What happened

The connection is closed: the server closed it (restart, `pg_terminate_backend`, idle timeout), the network failed, or an earlier error broke it. A pool also closes a connection it shares between tasks when a statement sent through `Pool::batch_execute` leaves a transaction open there, so that the statements of other tasks are not taken into it: everything since `BEGIN` is rolled back, and the statements in flight on that connection fail with this error.

## How to fix it

- Open a new connection; a `Pool` does this by itself on the next `get`.
- If it happens often, look for server-side timeouts (`idle_session_timeout`, a proxy's idle timeout) and the server log.
- If the message says a transaction was left open, run transactions on a connection of the task's own: `Pool::get`, then `transaction()`.

[All error codes](README.md)
"#),
    (3009, r#"# GIONG-E3009: PostgreSQL pool wait timed out

Raised by giong-postgres, `Pool::get` and the pool's statement methods (`Pool::query` and the others). In code: `giong::error::Code::PostgresPoolTimeout`.

## What happened

No pooled connection became free within the pool's wait timeout (5 seconds by default). Every connection was busy the whole time: taken with `Pool::get`, or, for `get`, running shared statements that did not finish in time. The message says which.

Tasks waiting in `get` are served in the order they asked, and shared statements stop using a connection while a `get` waits for it, so a `get` waits for the `get`s before it and for the statements already running, not for new ones. The statement methods wait only when every connection is taken with `get` or already carries 1024 statements.

## How to fix it

- Look for statements that hold connections for long: slow queries, transactions left open while doing other work, a `PooledClient` kept after its work is done.
- Size the pool to the database's cores, not to the number of tasks; more connections than cores make every statement slower.
- Raise the wait timeout only if requests can afford to wait longer.

[All error codes](README.md)
"#),
    (3010, r#"# GIONG-E3010: PostgreSQL protocol error

Raised by giong-postgres. In code: `giong::error::Code::PostgresProtocol`.

## What happened

The server sent something that is not the PostgreSQL protocol, or that giong-postgres does not understand. The connection was closed.

## How to fix it

- Check that the host and the port are a PostgreSQL server (not a proxy speaking another protocol, nor a TLS port reached without TLS).
- Connection poolers in transaction mode must support the extended query protocol.
- If it is a standard PostgreSQL server, please report it at https://github.com/giong-run/Giong/issues with the server version.

[All error codes](README.md)
"#),
    (3011, r#"# GIONG-E3011: statement refused before sending

Raised by giong-postgres, `Pool::query`, `Pool::query_one`, `Pool::query_opt`, `Pool::execute` and `Pool::batch_execute`. In code: `giong::error::Code::PostgresStatement`.

## What happened

The statement was refused before anything was sent. The pool's own methods run statements on connections that several tasks share at once, and this statement would have reached the other tasks:

- It controls a transaction (`BEGIN`, `START TRANSACTION`, `COMMIT`, `END`, `ROLLBACK`, `ABORT`, `SAVEPOINT`, `RELEASE`, `PREPARE TRANSACTION`): a transaction begun there would take in the statements of the other tasks, and a `COMMIT` there commits nothing of yours. `batch_execute` accepts these, see below.
- It changes the session (`SET` without `LOCAL`, `SET ROLE`, `SET SESSION AUTHORIZATION`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `PREPARE`, `DEALLOCATE`, `DECLARE`, `LOAD`, `CREATE TEMP TABLE` and the other temporary objects): every statement of every task that runs on the connection afterwards would see the change.
- It is `COPY ... FROM STDIN`, which giong-postgres does not support yet and which would close the shared connection, failing the statements of the other tasks.

Statements are recognised by their first words, outside comments and quoted text. Functions that change the session (`set_config(.., false)`, `pg_advisory_lock`) cannot be recognised this way and are not refused, but belong on a connection from `Pool::get` all the same.

## How to fix it

- Take a connection for the task with `Pool::get`. Run a transaction on it with `transaction()`: dropping the transaction without `commit` rolls it back. Session settings made on it last until it goes back to the pool, where the session is reset.
- For a setting every connection should have (`search_path`, `TimeZone`...), use `Config::param`; it survives the reset.
- For a setting that should last one transaction, use `SET LOCAL` inside it.
- A whole transaction in one string (`BEGIN; ...; COMMIT`) can go through `Pool::batch_execute`; if it leaves a transaction open, the connection is closed and everything since `BEGIN` is rolled back (GIONG-E3008).

[All error codes](README.md)
"#),
    (3101, r#"# GIONG-E3101: invalid MySQL configuration

Raised by giong-mysql, building a `Config` or parsing a URL. In code: `giong::error::Code::MysqlConfig`.

## What happened

The configuration or the connection URL is not valid. The message names the setting.

## How to fix it

- The URL form is `mysql://user:password@host:port/database?ssl-mode=...`; percent-encode special characters in the password, and use `?socket=/path/to/mysqld.sock` for a Unix socket.
- `ssl-mode=PREFERRED` is refused on purpose (it silently falls back to clear text): use `DISABLED`, `REQUIRED`, `VERIFY_CA` or `VERIFY_IDENTITY`.
- Session variables are set with `Config::session_variable`; `time_zone` and `max_execution_time` are set by giong-mysql itself and cannot be.
- A session variable the server rejects when connecting is reported with this code too, with the server's error.

[All error codes](README.md)
"#),
    (3102, r#"# GIONG-E3102: cannot connect to MySQL

Raised by giong-mysql, opening a connection. In code: `giong::error::Code::MysqlConnect`.

## What happened

Opening the connection failed before authentication: name resolution, the TCP or Unix socket connection, TLS, or the connect timeout (10 seconds by default). The cause is in the message. These are reported with this code too:

- an unknown database (error 1049) and a full server (1040, too many connections);
- a server older than MySQL 8.0 or MariaDB 10.6, refused before logging in;
- a session whose character set is not `utf8mb4` after `SET NAMES utf8mb4`: text would be stored converted, without an error;
- a `CancelToken::cancel` whose connection reached another server than the connection it is for, where the connection id belongs to someone else's session: `KILL QUERY` was not sent.

## How to fix it

- Check the host, the port (3306 by default) and the database name, and that the server accepts connections from here (`bind_address`, firewalls).
- For TLS errors, see the TLS code in the message (`GIONG-E2xxx`); the server's certificate must name the host you connect to. For the certificates MySQL generates itself, which name no host, use `ssl-mode=VERIFY_CA` with `ssl-ca` set to the server's own `ca.pem` (in code, `Tls::verify_ca`).
- giong-mysql needs MySQL 8.0 or later, or MariaDB 10.6 or later.
- For the character set, look for a proxy between the client and the server that changes it.
- For a cancel request that reached another server, there is a load balancer between the client and the server that sent the cancel request's connection elsewhere: cancel by dropping the statement's future and closing its connection instead.
- If the server is slow to accept, raise `Config::connect_timeout`.

[All error codes](README.md)
"#),
    (3103, r#"# GIONG-E3103: MySQL authentication failed

Raised by giong-mysql, authentication. In code: `giong::error::Code::MysqlAuth`.

## What happened

The server refused the credentials (error 1045), or asked for an authentication plugin giong-mysql does not support. A password is also never sent where someone on the network could read it: `caching_sha2_password` needs the password itself the first time the server sees a user, and without TLS that is only done over a Unix socket, to this machine (with the server's RSA key), or with a key you configured.

## How to fix it

- Check the user name, the password and the host part of the account (`'app'@'%'` or `'app'@'10.0.0.%'`).
- Turn on TLS (`ssl-mode=VERIFY_IDENTITY`), which MySQL 8 offers by default; or give the server's RSA public key with `Config::server_public_key` (its file is `public_key.pem` in the data directory).
- Use `caching_sha2_password` (MySQL 8's default) or `mysql_native_password`; `sha256_password` and clear-text plugins work only over TLS. LDAP, Kerberos and PAM plugins are not supported.

[All error codes](README.md)
"#),
    (3104, r#"# GIONG-E3104: MySQL server error

Raised by giong-mysql, running a statement. In code: `giong::error::Code::MysqlServer`.

## What happened

The server reported an error for a statement. The message contains the server's text, `Error::server_code()` returns MySQL's error number, such as 1062 for a duplicate key or 1213 for a deadlock, and `Error::db()` gives the SQLSTATE too.

## How to fix it

- Look the number up in the MySQL manual ("Server Error Message Reference").
- Match on `Error::server_code()` in code, not on the message, which the server may translate.
- 1213 (deadlock) means the server rolled the transaction back: run it again from the start. 1205 (lock wait timeout) rolls back only the statement, unless the server has `innodb_rollback_on_timeout` on, which rolls back the whole transaction, as 1206 (lock table full) does. giong-mysql reads that setting when connecting, and after other errors in a transaction asks the server whether the transaction is still open.
- 3024 means the statement ran longer than `Config::statement_timeout`; MySQL enforces it on `SELECT` only. On MariaDB the same limit is 1969, for every statement. A statement waiting for a table or metadata lock gives up after it too, with 1205.
- A message saying the server rolled the transaction back, or ended it before `COMMIT`, or that a statement of the transaction was given up, comes from giong-mysql rather than the server (`Error::server_code()` is `None`): the statement was not sent, because it would have run outside the transaction, or the transaction may hold work the application believes undone. Drop the transaction, which rolls it back, and run it again from the start.

[All error codes](README.md)
"#),
    (3105, r#"# GIONG-E3105: MySQL type mismatch

Raised by giong-mysql, parameters and `Row::get`. In code: `giong::error::Code::MysqlType`.

## What happened

A value could not be converted between Rust and MySQL without losing something: a number that does not fit the Rust type, a binary column read as text, a time outside DATETIME's range, or a NULL read into a type that cannot hold it. A wrong number of parameters is reported with this code too.

## How to fix it

- The message names the parameter or column and the types that would work.
- Read a column that can be NULL as `Option<T>`, a DECIMAL as `String` (it stays exact), and binary columns as `Vec<u8>` or `&[u8]`.

[All error codes](README.md)
"#),
    (3106, r#"# GIONG-E3106: wrong number of rows

Raised by giong-mysql, `query_one` and `query_opt`. In code: `giong::error::Code::MysqlRowCount`.

## What happened

`query_one` expects exactly one row and `query_opt` at most one, and the statement returned another number.

## How to fix it

- Use `query_opt` when the row may be missing, and `query` when there can be several.
- Add a `LIMIT 1` or a unique condition if exactly one row is meant.
- `insert` does not raise this code: since 0.7.0 it returns `Ok(None)` when the statement generated no `AUTO_INCREMENT` id.

[All error codes](README.md)
"#),
    (3107, r#"# GIONG-E3107: MySQL result too large

Raised by giong-mysql, reading a result. In code: `giong::error::Code::MysqlTooLarge`.

## What happened

A result was larger than `Config::max_result_size` (64 MiB by default). The limit keeps one query from using all of a worker's memory. The connection stays usable.

## How to fix it

- Fetch fewer rows at a time (`LIMIT`, or keyset pagination).
- Raise `Config::max_result_size` if the result really has to be this big.

[All error codes](README.md)
"#),
    (3108, r#"# GIONG-E3108: MySQL connection closed

Raised by giong-mysql, any statement. In code: `giong::error::Code::MysqlClosed`.

## What happened

The connection is closed: the server closed it (restart, `KILL`, `wait_timeout`), the network failed, an earlier error broke it, the server asked for a local file (`LOAD DATA LOCAL INFILE`), which giong-mysql never sends, or a statement sent through `Pool::batch_execute` left a transaction open, or `autocommit` off, on a connection the pool shares between tasks. In that last case the pool closes the connection so that the statements of other tasks are not taken into the transaction: everything since it began is rolled back, and the statements in flight on that connection fail with this error.

The pool also closes a connection given back from `Pool::get` whose session it could not reset (the reset failed), and one whose default database a `USE` changed when `Config::database` is not set, since the reset cannot undo that.

## How to fix it

- Open a new connection; a `Pool` does this by itself on the next `get`.
- If it happens often, look for server-side timeouts (`wait_timeout`, a proxy's idle timeout) and the server's error log.
- If the message says a transaction was left open, run transactions on a connection of the task's own: `Pool::get`, then `transaction()`.
- If the message names `USE`, set `Config::database`, or name the database in the statements instead.

[All error codes](README.md)
"#),
    (3109, r#"# GIONG-E3109: MySQL pool wait timed out

Raised by giong-mysql, `Pool::get`. In code: `giong::error::Code::MysqlPoolTimeout`.

## What happened

No pooled connection became free within the pool's wait timeout (5 seconds by default). Every connection was busy the whole time.

## How to fix it

- Look for statements that hold connections for long: slow queries, transactions left open while doing other work.
- Size the pool to the database's cores, not to the number of tasks; more connections than cores make every statement slower.
- Raise the wait timeout only if requests can afford to wait longer.

[All error codes](README.md)
"#),
    (3110, r#"# GIONG-E3110: MySQL protocol error

Raised by giong-mysql. In code: `giong::error::Code::MysqlProtocol`.

## What happened

The server sent something that is not the MySQL protocol, or that giong-mysql does not understand, such as a packet out of sequence. The connection was closed.

## How to fix it

- Check that the host and the port are a MySQL server (not a proxy speaking another protocol, nor the X Protocol port 33060).
- If it is a standard MySQL 8.0 or later server, please report it at https://github.com/giong-run/Giong/issues with the server version.

[All error codes](README.md)
"#),
    (3111, r#"# GIONG-E3111: statement refused before sending

Raised by giong-mysql, `Pool::query`, `Pool::query_one`, `Pool::query_opt`, `Pool::execute` and `Pool::insert`. In code: `giong::error::Code::MysqlStatement`.

## What happened

The statement was refused before anything was sent. The pool's own methods run statements on connections that several tasks share at once, and the statement would have changed what the other tasks' statements run in:

- It controls a transaction or locks (`BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE`, `XA`, `LOCK`, `UNLOCK`): a transaction begun there would take in the statements of the other tasks, and a `COMMIT` there commits nothing of yours.
- It changes the session: `SET` (but `SET GLOBAL`), `USE`, `CREATE` or `DROP TEMPORARY TABLE`, `GET_LOCK`, or an assignment to a user variable (`@v := ...`, `SELECT ... INTO @v`). The other tasks would run in the changed session, with other settings, a temporary table hiding a real one, or a lock they never asked for.

## How to fix it

- Take a connection for the task with `Pool::get`, and run the transaction on it with `transaction()`: dropping the transaction without `commit` rolls it back.
- Run session statements on a connection from `Pool::get` too. When the connection goes back to the pool, its session is reset (`COM_RESET_CONNECTION`, then the configuration's session settings again), so the change does not reach other tasks.
- For a setting every connection needs, use `Config::session_variable` instead: it is set when connecting, and again after each reset.
- A whole transaction in one string (`START TRANSACTION; ...; COMMIT`) can go through `Pool::batch_execute`; if it leaves a transaction open, or turns `autocommit` off, the connection is closed and everything since is rolled back (GIONG-E3108).

[All error codes](README.md)
"#),
    (3201, r#"# GIONG-E3201: invalid Redis configuration

Raised by giong-redis, building a `Config` or parsing a URL. In code: `giong::error::Code::RedisConfig`.

## What happened

The configuration or the connection URL is not valid. The message names the setting or the part of the URL.

## How to fix it

- The URL form is `redis://user:password@host:port/database`, `rediss://...` for TLS, and `redis+unix:///path/to/redis.sock?db=N` for a Unix socket; percent-encode special characters in the password.
- The parameters understood are `db`, `client-name`, `connect-timeout`, `response-timeout` and `cacert` (for `rediss://` only); any other parameter is refused rather than ignored.
- A user name needs a password: Redis logs in an ACL user with both.
- A client name cannot contain spaces (Redis refuses it).

[All error codes](README.md)
"#),
    (3202, r#"# GIONG-E3202: cannot connect to Redis

Raised by giong-redis, opening a connection. In code: `giong::error::Code::RedisConnect`.

## What happened

Opening the connection failed before it could be used: name resolution, the TCP or Unix socket connection, TLS, the `HELLO 3` handshake, selecting the database, or the connect timeout (10 seconds by default). The cause is in the message.

## How to fix it

- Check the host and the port (6379 by default; the TLS port is the server's `tls-port`), and that the server accepts connections from here (`bind`, `protected-mode`, firewalls).
- For TLS errors, see the TLS code in the message (`GIONG-E2xxx`); the server's certificate must name the host you connect to. A TLS client on the plain port, or a plain client on the TLS port, fails here too.
- giong-redis speaks RESP3, which needs Redis 6 or later, or Valkey: older servers are refused with this code.
- A database number the server does not have (`databases` in redis.conf, 16 by default) is refused with this code.
- If the server is slow to accept, raise `Config::connect_timeout`.

[All error codes](README.md)
"#),
    (3203, r#"# GIONG-E3203: Redis authentication failed

Raised by giong-redis, logging in. In code: `giong::error::Code::RedisAuth`.

## What happened

The server refused the user name or the password (`WRONGPASS`), asked for a password that was not given (`NOAUTH`), or the ACL user may not run `HELLO` (`NOPERM`).

## How to fix it

- Check the user name and the password; without `Config::user`, the password is the `default` user's (`requirepass`).
- `ACL LIST` on the server shows the users, whether they are on, and their passwords' hashes.
- An ACL user needs permission for `HELLO` (in `+@connection`) to connect, and for `SELECT` when `Config::database` is not 0.

[All error codes](README.md)
"#),
    (3204, r#"# GIONG-E3204: Redis server error

Raised by giong-redis, running a command. In code: `giong::error::Code::RedisServer`.

## What happened

The server answered the command with an error. The message contains the server's text, and `Error::server_code()` its code: `ERR`, `WRONGTYPE`, `NOSCRIPT`, `READONLY`, `OOM`, `BUSY`...

It is also the error of an atomic pipeline (`Pipeline::atomic`) that the server did not run because a key watched on the connection changed: `EXEC` answered nil. Outside a `Watch` that only happens when a `WATCH` was left behind, and `Error::server()` is `None`.

## How to fix it

- Match on `Error::server_code()` in code, not on the message.
- "the transaction did not run": nothing in the pipeline ran. Use `client.watch(&keys)` for optimistic transactions, and run the pipeline with `watch.exec`, which returns `None` in this case.
- `WRONGTYPE`: the key holds another type than the command works on; `TYPE key` says which.
- `MOVED` and `ASK` come from Redis Cluster, which giong-redis does not support yet. `READONLY`: the server is a replica; connect to the primary. `NOSCRIPT`: load the script with `SCRIPT LOAD`, or send it with `EVAL`.
- `Error::is_transient()` is true for `LOADING`, `BUSY`, `TRYAGAIN`, `MASTERDOWN` and `CLUSTERDOWN`, which can be retried after a pause.

[All error codes](README.md)
"#),
    (3205, r#"# GIONG-E3205: Redis type mismatch

Raised by giong-redis, reading a reply as a Rust type. In code: `giong::error::Code::RedisType`.

## What happened

A reply could not be converted to the Rust type asked for without losing something: a missing key (nil) read into a type that cannot hold it, a number that does not fit, bytes that are not UTF-8 read as `String`, or an array of another length than the tuple.

## How to fix it

- Read a key that may be missing as `Option<T>`, binary values as `Vec<u8>`, and replies you are unsure of as `Value`.
- The message names the reply that arrived and the types that would work.

[All error codes](README.md)
"#),
    (3206, r#"# GIONG-E3206: command refused before sending

Raised by giong-redis, before sending a command. In code: `giong::error::Code::RedisCommand`.

## What happened

The command was refused before anything was sent, because it would change the connection for the other tasks sharing it, or block them: `MULTI`/`EXEC`, `WATCH`, `SELECT`, `AUTH`/`HELLO`, the `SUBSCRIBE` family, `MONITOR`, `QUIT`/`RESET`, `CLIENT REPLY`, or a blocking command (`BLPOP`, `XREAD ... BLOCK`, `WAIT`...) sent with `query`.

## How to fix it

- The message says what to use instead.
- Transactions: `Pipeline::new().atomic()`, or `Client::watch` (or `Pool::take` and then `watch`) for optimistic ones.
- Another database: `Config::database`. Another user: `Config::user` and `Config::password`. Subscriptions: `PubSub::connect`.
- Blocking commands: `Client::query_blocking` on a connection of their own (`Pool::take`, or a `Client` that is not shared).

[All error codes](README.md)
"#),
    (3207, r#"# GIONG-E3207: Redis reply too large

Raised by giong-redis, reading a reply. In code: `giong::error::Code::RedisTooLarge`.

## What happened

A reply was larger than `Config::max_reply_size` (64 MiB by default). The limit keeps one command from using all of a worker's memory. The reply was read and thrown away as it arrived; the connection stays usable. A push message over the limit (a client-side caching invalidation, say) answers no command, so it is thrown away without an error.

## How to fix it

- Read less at a time: `SCAN`, `HSCAN` and `ZRANGE` with a range or `LIMIT` instead of `KEYS`, `HGETALL` or a whole sorted set.
- Raise `Config::max_reply_size` if the reply really has to be this big.

[All error codes](README.md)
"#),
    (3208, r#"# GIONG-E3208: Redis connection closed

Raised by giong-redis, any command. In code: `giong::error::Code::RedisClosed`.

## What happened

The connection is closed: the server closed it (restart, `CLIENT KILL`, `timeout` in redis.conf, the output buffer limits), the network failed, or an earlier error broke it. Commands that were waiting for their replies fail with this code; they may or may not have run.

## How to fix it

- Open a new connection; a `Pool` does this by itself for the next command.
- A `PubSub` that fails with this code missed the messages sent while it was down: connect and subscribe again.
- If it happens often, look at the server's log, its `timeout` setting, and idle timeouts in proxies or load balancers on the way.

[All error codes](README.md)
"#),
    (3209, r#"# GIONG-E3209: Redis pool wait timed out

Raised by giong-redis, `Pool::take` and the pool's command methods. In code: `giong::error::Code::RedisPoolTimeout`.

## What happened

No pooled connection became available within the pool's wait timeout (5 seconds by default): every connection was taken with `Pool::take`, or being opened, the whole time.

## How to fix it

- Look for tasks that hold a `PooledClient` for long, such as one waiting in a blocking command.
- Commands sent with the pool's methods share connections and seldom wait; take a connection only for `WATCH` and blocking commands.
- Raise the pool's size or its wait timeout if the load really needs it.

[All error codes](README.md)
"#),
    (3210, r#"# GIONG-E3210: Redis protocol error

Raised by giong-redis. In code: `giong::error::Code::RedisProtocol`.

## What happened

The server sent something that is not RESP3, or a reply when no command was waiting for one. The connection was closed.

## How to fix it

- Check that the host and the port are a Redis (6 or later) or Valkey server, and not a proxy speaking another protocol.
- If it is, please report it at https://github.com/giong-run/Giong/issues with the server's name and version.

[All error codes](README.md)
"#),
    (3211, r#"# GIONG-E3211: Redis reply timed out

Raised by giong-redis, waiting for a reply. In code: `giong::error::Code::RedisTimeout`.

## What happened

The reply did not arrive within `Config::response_timeout` (30 seconds by default). The command may still have run on the server. The connection stays usable: the late reply is thrown away when it comes.

## How to fix it

- Look for slow commands on the server (`SLOWLOG GET`), such as `KEYS` or a large `SORT`, and for the server being busy (`LATENCY DOCTOR`).
- Set the timeout near what the caller can wait, such as 500 ms for a cache.
- Blocking commands sent with `Client::query_blocking` are not bound by it.

[All error codes](README.md)
"#),
    (3601, r#"# GIONG-E3601: invalid QUIC configuration

Raised by giong-quic, building a `ServerConfig`, `ClientConfig` or `TransportConfig`. In code: `giong::error::Code::QuicConfig`.

## What happened

The configuration cannot work for QUIC. QUIC carries TLS 1.3 only (RFC 9001), so a rustls configuration that allows no TLS 1.3 is refused; QUIC also requires an ALPN protocol to be agreed, so a configuration with no `alpn_protocols` is refused. A transport setting out of range (for example a window larger than 2^62 bytes) is refused too.

## How to fix it

- Build the rustls configuration with TLS 1.3 enabled (rustls' defaults include it).
- Set `alpn_protocols`, for example `vec![b"h3".to_vec()]` for HTTP/3.
- Keep transport settings within the ranges given in their documentation.

[All error codes](README.md)
"#),
    (3602, r#"# GIONG-E3602: cannot set up the QUIC connection

Raised by giong-quic, `Endpoint::connect`. In code: `giong::error::Code::QuicConnect`.

## What happened

The handshake did not complete. The server did not answer over UDP before the idle timeout, refused the connection, or the TLS handshake failed: the certificate is not valid for the server name, or client and server offer no common ALPN protocol. The underlying reason is in the message.

## How to fix it

- Check that the server listens on that address and that UDP to it is not blocked by a firewall.
- Check that the server's certificate covers the server name passed to `connect`.
- Make the client's and server's `alpn_protocols` share at least one protocol.

[All error codes](README.md)
"#),
    (3603, r#"# GIONG-E3603: the QUIC connection was lost

Raised by giong-quic, by any operation on a `Connection` or its streams. In code: `giong::error::Code::QuicConnectionLost`.

## What happened

The connection is closed. The peer closed it (with an application or transport error code, given in the message), this side closed it, or no packet arrived for longer than the idle timeout.

## How to fix it

- Open a new connection; a lost QUIC connection does not come back.
- If it timed out on an idle connection, raise `TransportConfig::idle_timeout` or send something periodically.
- If the peer closed it with an error, the peer's logs say why.

[All error codes](README.md)
"#),
    (3604, r#"# GIONG-E3604: the QUIC stream was reset

Raised by giong-quic, reading or writing a stream. In code: `giong::error::Code::QuicStreamReset`.

## What happened

The peer abandoned the stream: it reset the half it was sending (`RESET_STREAM`), or asked this side to stop sending (`STOP_SENDING`). The application error code the peer chose is in the message.

## How to fix it

- Treat it as the peer cancelling this request; the connection and its other streams are unaffected.
- Retry on a new stream if the request is safe to repeat.

[All error codes](README.md)
"#),
    (3605, r#"# GIONG-E3605: the server refused the 0-RTT data

Raised by giong-quic, reading, writing or finishing a stream that a client opened in 0-RTT data (`Endpoint::connect_early`). In code: `giong::error::Code::QuicEarlyDataRejected`.

## What happened

The client resumed an earlier session and sent the first bytes of this stream with the handshake, as early data. The server did not take early data on this connection (its ticket expired, it was used before, or the server does not accept early data), so nothing of the stream reached the server's application. The connection itself is fine.

## How to fix it

- Send the request again on a new stream of the same connection: once `Connection::handshake` has returned, a new stream is no longer early data.
- giong-http's HTTP/3 client does this by itself.

[All error codes](README.md)
"#),
    (4001, r#"# GIONG-E4001: the message ends in the middle of a field

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::Truncated`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

The input stops before a field is complete: a varint's last byte, the 4 or 8 bytes of a fixed-width value, or the bytes a length prefix promised are missing. The error names the field number and the offset, counted from the start of the innermost message being read.

Almost always the message was cut off before it reached the decoder: the bytes of a gRPC frame or a file record were read only in part, or a buffer was reused before the whole message arrived.

## How to fix it

- Pass the whole message to `decode`. With length-prefixed framing (gRPC, a log file), read the prefix, then exactly that many bytes, then decode.
- If the sender is yours, check that it sends `encoded_len()` bytes and not a buffer's capacity or an older length.

[All error codes](README.md)
"#),
    (4002, r#"# GIONG-E4002: malformed varint

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::BadVarint`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

A varint runs for more than 10 bytes, or its tenth byte holds more than the top bit of a 64-bit number. No protobuf encoder writes either.

## How to fix it

- Check that the bytes are a protobuf message of this type and that nothing (a framing header, a compression flag) was left in front of them.
- If the bytes come from storage, check them for corruption.

[All error codes](README.md)
"#),
    (4003, r#"# GIONG-E4003: invalid field tag

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::BadTag`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

A field tag is invalid: field number 0, a field number above 536 870 911 (2^29 - 1), wire type 6 or 7, or a tag written in more than 5 bytes. Decoding stops there, because nothing after a bad tag can be trusted.

Common causes: the bytes are not protobuf (JSON, text, an HTML error page), they are compressed, or they start at the wrong offset.

## How to fix it

- Check the content type and the compression flag of what you received before decoding it as protobuf.
- Check that the offset you decode from is the start of the message, after any framing.

[All error codes](README.md)
"#),
    (4004, r#"# GIONG-E4004: unbalanced group

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::BadGroup`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

A proto2 group is not closed properly: an end-group tag with no group open, an end-group tag for a different field number than the group it closes, or input that ends inside a group.

## How to fix it

- The sender's encoder is broken or the bytes are corrupted: compare the `.proto` on both sides.
- Groups are a proto2 feature that newer code should not use; prefer a nested message field.

[All error codes](README.md)
"#),
    (4005, r#"# GIONG-E4005: string field is not UTF-8

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::BadUtf8`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

A field declared `string` in the `.proto` holds bytes that are not valid UTF-8. giong-proto checks every `string` field once at decode, so that the views can hand out `so that the views can hand out `&str` without checking again (Google's libraries and prost reject such input as well).str` without checking again. prost rejects such input too, and so do Google's libraries for proto3; for proto2 `string` fields Google's libraries accept it unchecked, and giong-proto does not (yet).

## How to fix it

- If the field carries binary data (hashes, encrypted values, images), declare it `bytes` in the `.proto`; the wire format is the same, so this change is compatible.
- If the sender is meant to send text, fix its encoding: text from a database or a file may be in Latin-1 or another code page and must be converted to UTF-8 before it is sent.

[All error codes](README.md)
"#),
    (4006, r#"# GIONG-E4006: messages nested too deeply

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::TooDeep`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

Messages (or groups) are nested more than 100 levels deep, the same limit as Google's C++ library and prost. The limit stops a small hostile input (a message that contains itself thousands of times) from exhausting the stack.

## How to fix it

- Flatten the data: a tree that deep is better sent as a list of nodes that refer to each other by id.
- If the data must stay nested, split it over several messages.

[All error codes](README.md)
"#),
    (4007, r#"# GIONG-E4007: malformed packed field

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::BadPacked`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

A packed repeated field is inconsistent: its length is not a multiple of 4 (fixed32, sfixed32, float) or 8 (fixed64, sfixed64, double), or its last varint runs past the end of the field.

## How to fix it

- The sender's encoder is broken or the bytes are corrupted: compare the `.proto` on both sides, in particular the type of the repeated field (an `int32` field on one side and a `fixed32` field on the other produce this).

[All error codes](README.md)
"#),
    (4008, r#"# GIONG-E4008: message larger than 2 GiB

Raised by giong-proto, when decoding a message (`View::decode`) or encoding one (`Encode::encode`). In code: `giong_proto::ErrorKind::TooLarge`.

> giong-proto is a prototype for giong 1.1 and is not published yet. When it is, these codes move into `giong::error::Code`.

## What happened

`encode` was asked to write a message larger than 2 GiB (2^31 - 1 bytes). Protobuf frames messages and fields with lengths that readers treat as signed 32-bit numbers, so no implementation can read such a message. Nothing was written.

## How to fix it

- Split the data over several messages, or send it as a stream (a gRPC streaming call) instead of one message.
- For a large file, send it in pieces of a few MiB each.

[All error codes](README.md)
"#),
    (4101, r#"# GIONG-E4101: invalid gRPC metadata

Raised by giong-grpc's `Metadata::add`, for a metadata entry that cannot be sent as written. In code: `giong::error::Code::GrpcInvalidMetadata`.

## What happened

One of these:

- The name is not made of lowercase letters, digits, `-`, `_` and `.`, or it is empty. gRPC metadata names are lowercase, and `Metadata::add` refuses an uppercase letter instead of changing the name it was given.
- The name is one gRPC or HTTP keeps for itself: a name that starts with `grpc-`, or one of `accept-encoding`, `connection`, `content-encoding`, `content-length`, `content-type`, `date`, `host`, `te`, `trailer`.
- The value of a text entry (a name that does not end in `-bin`) has a byte outside printable ASCII (space to `~`).

The message names the entry and where the bad byte is, without repeating the value.

## How to fix it

- Give an entry that carries arbitrary bytes a name ending in `-bin`. giong-grpc sends those base64-encoded and decodes them on the other side.
- Set the deadline with `CallOptions::timeout` and compression with `CallOptions::compress`, not with `grpc-timeout` or `grpc-encoding` entries.
- Check names and values that come from outside the program before adding them.

[All error codes](README.md)
"#),
    (4102, r#"# GIONG-E4102: gRPC message too large

Raised by giong-grpc, on the side that reads a message past its limit or is asked to write one. The call ends with `RESOURCE_EXHAUSTED`. In code: `giong::error::Code::GrpcMessageTooLarge`.

## What happened

A message is larger than the limit of this side: 4 MiB unless set otherwise. The limit counts the message after decompression, so a small compressed message that inflates past it is refused too, before the whole of it is inflated. The length prefix is checked before any of the message is buffered: an oversized message costs the reader five bytes.

The message gives the size and the limit.

## How to fix it

- If messages this large are expected, raise the limit on the side that refused: `Server::max_message_size` or `ChannelBuilder::max_message_size`.
- Otherwise split the data: a server-streaming or client-streaming method sends it as many messages and neither side holds all of it at once.

[All error codes](README.md)
"#),
    (4103, r#"# GIONG-E4103: gRPC protocol error

Raised by giong-grpc when the other end does not speak gRPC as specified. A server ends the call with `INTERNAL` (or `UNIMPLEMENTED` when a unary method got no message or more than one); a client returns the status to the caller. In code: `giong::error::Code::GrpcProtocol`.

## What happened

One of these, named in the message:

- A message is cut short: the stream ended inside a length prefix or inside a message.
- A frame's first byte is not 0 or 1, or it is 1 (compressed) on a call that named no `grpc-encoding`.
- Compressed data is not a valid gzip stream, or its checksum or length does not match.
- A response has no `grpc-status`, or one that is not a number; or the response is not `application/grpc` and its HTTP status is not one gRPC maps.
- A `grpc-timeout` is not up to 8 digits and one of `H`, `M`, `S`, `m`, `u`, `n`.
- A `-bin` metadata value is not base64.
- A request is not a POST over HTTP/2.
- A unary or server-streaming method received no request message or more than one; a unary or client-streaming call returned no response message or more than one.

## How to fix it

- Check that the address is a gRPC server (not a REST endpoint, a load balancer health page or a gRPC-Web proxy), and that the client calls the method with the kind (unary, streaming) its `.proto` declares.
- Check that every proxy between the two forwards HTTP/2 end to end, trailers included. A proxy that downgrades to HTTP/1.1 or drops trailers produces a response without `grpc-status`.
- Regenerate both sides from the same `.proto` if a method changed between unary and streaming.

[All error codes](README.md)
"#),
    (4104, r#"# GIONG-E4104: unsupported gRPC message encoding

Raised by giong-grpc when a message is compressed with an encoding this side does not have. The call ends with `UNIMPLEMENTED`. In code: `giong::error::Code::GrpcEncoding`.

## What happened

The `grpc-encoding` of a request or a response names a compression other than `gzip` or `identity` (for example `snappy`, `zstd` or `deflate`). giong-grpc implements those two. A server that refuses a request this way answers with `grpc-accept-encoding: identity,gzip`, which tells the client what to use.

## How to fix it

- Configure the other end to compress with gzip or not at all. Every gRPC implementation supports both.
- giong-grpc itself only ever sends `gzip` or `identity`, so on a call between two giong programs this means something between them rewrote the messages.

[All error codes](README.md)
"#),
    (4105, r#"# GIONG-E4105: gRPC message is not valid protobuf

Raised by giong-grpc when a received message does not decode as the type its method declares. The call ends with `INTERNAL`. In code: `giong::error::Code::GrpcDecode`.

## What happened

The bytes of a request (on a server) or a response (on a client) are not a valid protobuf encoding of the method's message type. The message carries the giong-proto code that says what is wrong with the bytes, for example [GIONG-E4002](E4002.md) for a malformed varint or [GIONG-E4005](E4005.md) for a string that is not UTF-8.

## How to fix it

- Follow the giong-proto code in the message: its page says what the bytes violate.
- Check that both sides were generated from the same `.proto`. A field whose type changed (a `string` that became a message, a varint that became fixed) decodes as garbage on the side with the old definition; add fields under new numbers instead.
- A handler that wants the bytes whatever they hold reads them with `as_bytes` and never decodes.

[All error codes](README.md)
"#),
    (4106, r#"# GIONG-E4106: gRPC connection failed

Raised by giong-grpc's client when the connection for a call could not be opened, or broke before the call ended. The call ends with `UNAVAILABLE`. In code: `giong::error::Code::GrpcTransport`.

## What happened

The call never reached a status from the server. The message carries the underlying error and its own code: a refused or timed-out connection, a name that does not resolve, a TLS handshake that failed, a connection the server closed or reset while the call was open, or a server that does not speak HTTP/2.

Whether the server ran the method is unknown when the connection broke after the request was sent.

## How to fix it

- Follow the code of the underlying error in the message.
- Check the address given to `Channel::new`: `http://` speaks HTTP/2 without TLS, `https://` negotiates HTTP/2 with ALPN. A TLS server reached over `http://`, or the reverse, fails here.
- Retry calls that are safe to repeat. giong-grpc does not retry by itself, because it cannot know whether the method is.

[All error codes](README.md)
"#),
    (4107, r#"# GIONG-E4107: invalid gRPC endpoint

Raised by giong-grpc's `Channel::new` and `ChannelBuilder::build`, for an address a channel cannot be made for. In code: `giong::error::Code::GrpcEndpoint`.

## What happened

The address is not an `http://` or `https://` URL with a host: the scheme is missing or is another one (`grpc://`, `dns:///`, a bare `host:port`), or the host is empty. A path, a query, a fragment or credentials (`user@`) after the scheme are refused too: a channel addresses a server, and each call adds its own `/package.Service/Method`.

## How to fix it

- Write the address as `http://host:port` for a server without TLS and `https://host:port` for one with TLS.
- Drop anything after the port (one closing `/` is accepted).

[All error codes](README.md)
"#),
    (4108, r#"# GIONG-E4108: gRPC deadline exceeded

Raised by giong-grpc when a call does not end within its deadline. The call ends with `DEADLINE_EXCEEDED`. In code: `giong::error::Code::GrpcDeadline`.

## What happened

On a client, the time given with `CallOptions::timeout` (or the channel's default, 30 seconds) passed before the call's status arrived; the client reset the stream, so the server stops working on it. On a server, the time the client sent in `grpc-timeout` (or `Server::default_timeout`, one hour, when the client sent none) passed before the handler finished; the handler's future was dropped at its next await point.

The deadline covers the whole call, streaming calls included: every message of a stream has to fit inside it.

## How to fix it

- Give long-lived streams a deadline that fits them, or raise the default with `ChannelBuilder::timeout`.
- In a handler, read `Request::deadline` and pass the time left to the calls it makes, so work for a client that has given up is not started.
- A handler that must finish a step whatever happens runs that step in its own task (`giong::spawn`); the dropped future does not take it along.

[All error codes](README.md)
"#),
    (4109, r#"# GIONG-E4109: gRPC call cancelled

Raised by giong-grpc when a call was cancelled before it ended. The call ends with `CANCELLED`. In code: `giong::error::Code::GrpcCancelled`.

## What happened

On a server: the client reset the stream or its connection closed while the handler was reading requests (`RequestStream::message`) or sending responses (`Sink::send`). For unary and client-streaming methods the handler's future is dropped instead, at its next await point, and no status is sent because nobody is left to read it.

On a client: the call was cancelled with `RequestSink::cancel` or `ClientStreaming::cancel`, its `ResponseStream` was dropped before the call's status arrived, or the server ended the call while the client was still sending (`RequestSink::send` then fails with this code, and the server's status is on the response side).

## How to fix it

- In a streaming handler, return when `send` or `message` fails; `Sink::is_closed` tells a handler that produces responses on its own whether anyone still listens.
- On a client, read the response side for the status the server gave when `send` fails. Dropping a `RequestSink` ends the requests like `finish` does; only `cancel` resets the stream.
- Free what a handler holds in `Drop`, since a cancelled handler does not run to its end.

[All error codes](README.md)
"#),
    (4110, r#"# GIONG-E4110: gRPC method not found

Raised by giong-grpc's server for a call to a service or method it does not have. The call ends with `UNIMPLEMENTED`. In code: `giong::error::Code::GrpcUnimplemented`.

## What happened

The path of the request, `/package.Service/Method`, names a service that was not added to the `Server`, or a method that service's `.proto` did not have when its code was generated. The message gives the path.

## How to fix it

- Add the service: `Server::new().add(GreeterServer::new(my_greeter))`. `Server::service_names` lists what is registered.
- Check the package name: `helloworld.Greeter` and `Greeter` are different services.
- Regenerate the server from the `.proto` the client uses if the method is new.

[All error codes](README.md)
"#),
    (5000, r#"# GIONG-E5000: the command line is not valid

Raised by the `giong` command (giong-cli): a `giong` command, when it reads its command line. In JSON output: `"code": "GIONG-E5000"`.

## What happened

The command line names a command `giong` does not have, a flag the command does not take, a flag without its value, a value given twice, or more words than the command takes. Nothing was done.

## How to fix it

- `giong --help` lists the commands; `giong <command> --help` lists the flags of one.
- Flags are written in full with two dashes (`--json`, `--template web` or `--template=web`).
- Words meant for another program (cargo, the service) go after a lone `--`: `giong dev -- --port 8080`.

[All error codes](README.md)
"#),
    (5001, r#"# GIONG-E5001: no Cargo project here

Raised by the `giong` command (giong-cli): commands that work on a project (`dev`, `check`, `doctor`, `clean`). In JSON output: `"code": "GIONG-E5001"`.

## What happened

There is no `Cargo.toml` in the current directory or any directory above it, so there is no project to work on.

## How to fix it

- Change to the project's directory and run the command again.
- Start a project with `giong new <name>`.

[All error codes](README.md)
"#),
    (5002, r#"# GIONG-E5002: cargo could not be run

Raised by the `giong` command (giong-cli): commands that run cargo. In JSON output: `"code": "GIONG-E5002"`.

## What happened

cargo could not be started, or it failed before it could build: Cargo.toml has an error, or the toolchain is missing.

## How to fix it

- Install Rust with rustup (https://rustup.rs) and open a new terminal, so that `cargo` is on the `PATH`.
- Run the cargo command the message names by hand: its own error says what is wrong in `Cargo.toml`.

[All error codes](README.md)
"#),
    (5003, r#"# GIONG-E5003: the Rust compiler is too old

Raised by the `giong` command (giong-cli): `giong doctor` and `giong upgrade`. In JSON output: `"code": "GIONG-E5003"`.

## What happened

The Rust compiler on the `PATH` is older than the minimum the giong crates need (Rust 1.80; giong-quic and HTTP/3 need 1.85).

## How to fix it

- `rustup update stable`, then check with `rustc --version`.
- If the project pins an older toolchain in `rust-toolchain.toml`, raise it.
- From `giong upgrade`: the release it was moving to needs a newer Rust than this one. Nothing was changed; update Rust and run `giong upgrade` again, or stay on the release you have.

[All error codes](README.md)
"#),
    (5004, r#"# GIONG-E5004: a dependency needs the Tokio runtime

Raised by the `giong` command (giong-cli): `giong doctor` and `giong check`. In JSON output: `"code": "GIONG-E5004"`.

## What happened

A dependency in Cargo.toml (such as sqlx, reqwest, tonic's transport or axum) runs on the Tokio runtime. On a giong worker it panics with "there is no reactor running".

## How to fix it

- Use the giong crate for the job: giong-postgres or giong-mysql for sqlx, giong-http's client for reqwest, giong-grpc for tonic, giong-web for axum (see Choosing crates in the giong README).
- A crate that only takes a stream (h2, tokio-rustls, tokio-tungstenite) works over `giong_compat::Compat`: then this finding does not apply, and the warning can be ignored.

[All error codes](README.md)
"#),
    (5005, r#"# GIONG-E5005: no such error code

Raised by the `giong` command (giong-cli): `giong explain`. In JSON output: `"code": "GIONG-E5005"`.

## What happened

The word given to `giong explain` is not an error code, or no giong crate of this version has that code.

## How to fix it

- Copy the code from the start of the error message, such as `GIONG-E0102`.
- A code from a newer giong needs a newer command: `cargo install giong-cli`.

[All error codes](README.md)
"#),
    (5006, r#"# GIONG-E5006: a blocking call inside a worker

Raised by the `giong` command (giong-cli): `giong check`. In JSON output: `"code": "GIONG-E5006"`.

## What happened

The code calls something that blocks the thread (`std::fs`, `std::net`, `std::thread::sleep`, a blocking `std::process::Command` wait) inside an `async` function. On a giong worker every other task on that core waits until it returns.

## How to fix it

- Use the giong equivalent: `giong::fs`, `giong::net`, `giong::time::sleep`.
- Run heavy computation or a blocking library on `std::thread::spawn` and await a `giong::sync::oneshot` for the result.
- The check reads the source, not the types: if the call is not on a worker (in `main` before the runtime starts, in a thread of its own), the finding does not apply.

[All error codes](README.md)
"#),
    (5007, r#"# GIONG-E5007: binding a host name

Raised by the `giong` command (giong-cli): `giong check`. In JSON output: `"code": "GIONG-E5007"`.

## What happened

A listener binds a host name such as `"localhost:8080"`. giong binds IP addresses only: a name would need a DNS lookup that could block or return several addresses.

## How to fix it

- Bind an IP address: `"127.0.0.1:8080"` for this machine only, `"0.0.0.0:8080"` for every interface.
- `giong fix` makes the change for `localhost`.

[All error codes](README.md)
"#),
    (5008, r#"# GIONG-E5008: a RefCell borrow held across an await

Raised by the `giong` command (giong-cli): `giong check`. In JSON output: `"code": "GIONG-E5008"`.

## What happened

A `RefCell` borrow (`borrow()` or `borrow_mut()`) is kept in a variable while the function awaits. Another task on the same worker that borrows the same cell then panics with "already borrowed".

## How to fix it

- Take what you need out of the borrow, drop it (end its block, or `drop(guard)`), then await.
- The check reads the source: a borrow dropped before the `.await` by other means may still be reported.

[All error codes](README.md)
"#),
    (5009, r#"# GIONG-E5009: the code does not compile

Raised by the `giong` command (giong-cli): `giong check`, `giong dev` and `giong upgrade`. In JSON output: `"code": "GIONG-E5009"`.

## What happened

The compiler reported an error. `giong dev` stops the build that was running, so that no request is answered by code older than the files, and builds again on the next save; `giong dev status` shows the first errors in `failure`.

## How to fix it

- Read the compiler's message in the finding (file, line, and its own explanation), fix the code, save.
- `rustc --explain E0XXX` explains the compiler's own codes.

[All error codes](README.md)
"#),
    (5010, r#"# GIONG-E5010: clippy has a finding

Raised by the `giong` command (giong-cli): `giong check`. In JSON output: `"code": "GIONG-E5010"`.

## What happened

clippy, Rust's linter, reported a warning. The finding carries clippy's lint name and message.

## How to fix it

- Follow clippy's suggestion; when it is marked machine-applicable, `giong fix` applies it.
- If the lint is wrong for this code, allow it where it applies with `#[allow(clippy::name)]` and a comment saying why.

[All error codes](README.md)
"#),
    (5011, r#"# GIONG-E5011: build directories use a lot of disk

Raised by the `giong` command (giong-cli): `giong doctor` and `giong clean`. In JSON output: `"code": "GIONG-E5011"`.

## What happened

The project's build directory (`target/`) holds more than a few gigabytes. Cargo never removes old builds by itself: every toolchain update and every changed dependency leaves the previous ones behind.

## How to fix it

- `giong clean` removes the builds the current toolchain and lockfile no longer use, and says how much it freed.
- `giong clean --all` removes the whole build directory (the next build starts from nothing).

[All error codes](README.md)
"#),
    (5012, r#"# GIONG-E5012: the dev profile writes full debug information

Raised by the `giong` command (giong-cli): `giong doctor`. In JSON output: `"code": "GIONG-E5012"`.

## What happened

The project's dev profile writes full debug information. Debug builds then write and link much more, which makes every rebuild slower and `target/` larger, while backtraces need only line numbers.

## How to fix it

- Add to Cargo.toml:

  ```toml
  [profile.dev]
  debug = "line-tables-only"
  ```

  Backtraces keep file and line; a debugger shows less. Projects made with `giong new` have this already.
- Keep full debug information if you step through code in a debugger often.

[All error codes](README.md)
"#),
    (5013, r#"# GIONG-E5013: the open-files limit is low

Raised by the `giong` command (giong-cli): `giong doctor`. In JSON output: `"code": "GIONG-E5013"`.

## What happened

The soft limit on open files (`ulimit -n`) is low. Every connection, file and pipe takes one; at the limit, accept fails and giong-http logs `GIONG-E1123`. On Linux giong raises the soft limit to the hard limit when the runtime starts, so this matters when the hard limit itself is low.

## How to fix it

- Raise the hard limit for the service: `LimitNOFILE=` in a systemd unit, `--ulimit nofile=` in Docker, or `/etc/security/limits.conf`.
- On macOS, `ulimit -n 10240` in the shell that runs the service.

[All error codes](README.md)
"#),
    (5014, r#"# GIONG-E5014: the port is in use

Raised by the `giong` command (giong-cli): `giong doctor` and `giong dev`. In JSON output: `"code": "GIONG-E5014"`.

## What happened

Another process listens on the port the service is configured to use, so the service cannot bind it: when `giong dev` binds it for the service, or when the service, started by `giong dev`, stops at start-up because its own bind failed. In the second case `giong dev` stopped the build that ran before it too, and starts again on the next save.

## How to fix it

- Stop the other process, or use another port. `giong dev` names the process that listens there when the system says (Linux, macOS, Windows); otherwise `ss -ltnp` on Linux, `lsof -i :PORT` on macOS or `netstat -ano` on Windows shows which one.
- When the process is `giong dev` itself, it holds the port for the service and hands the socket down: the service must bind exactly `LISTEN` with `giong::net::TcpListener` (giong 1.5 or later, which giong-web's `serve` uses), not `std::net::TcpListener` or another runtime's listener.
- When it is a `giong dev` of another project, the message names the project: `giong dev stop` there, or `giong dev stop --all` stops every `giong dev` on this machine. `giong dev status --all` lists them.

[All error codes](README.md)
"#),
    (5015, r#"# GIONG-E5015: Docker blocks io_uring

Raised by the `giong` command (giong-cli): `giong doctor`. In JSON output: `"code": "GIONG-E5015"`.

## What happened

This runs in a container whose seccomp profile blocks the io_uring system calls (Docker's default). giong works there, but on its epoll driver.

## How to fix it

- Run with a profile that allows `io_uring_setup`, `io_uring_enter` and `io_uring_register` (DEPLOYMENT.md shows how to build `giong-seccomp.json`).
- If epoll is fine for development, ignore the warning.

[All error codes](README.md)
"#),
    (5016, r#"# GIONG-E5016: a variable from .env.example is not set

Raised by the `giong` command (giong-cli): `giong doctor` and `giong dev`. In JSON output: `"code": "GIONG-E5016"`.

## What happened

`.env.example` names a variable that neither `.env` nor the environment sets. The service will probably refuse to start or run with a default.

## How to fix it

- Copy the line from `.env.example` into `.env` and fill in the value.
- Never commit `.env`: it holds secrets. `.env.example` holds the names and safe examples.

[All error codes](README.md)
"#),
    (5017, r#"# GIONG-E5017: the service stopped

Raised by the `giong` command (giong-cli): `giong dev`. In JSON output: `"code": "GIONG-E5017"`.

## What happened

The service `giong dev` started exited by itself (it panicked, returned an error from `main`, or was killed), at start-up or later. The finding carries its exit status and the last lines it printed. When it was a new build that stopped before it served, `giong dev` stopped the build that ran before it too: nothing older than the files answers. `giong dev status` shows the same finding in `failure` until a build serves again; `giong dev` does not restart a service that stopped on its own until the next save.

## How to fix it

- Read the last lines of its output in the finding: a panic message names the file and line.
- `giong dev logs` prints more of its output.
- Fix the code and save: `giong dev` builds and starts it again.

[All error codes](README.md)
"#),
    (5018, r#"# GIONG-E5018: the service did not start listening

Raised by the `giong` command (giong-cli): `giong dev`. In JSON output: `"code": "GIONG-E5018"`.

## What happened

The new build of the service did not accept connections within the time `giong dev` waits for it (30 seconds by default). `giong dev` stopped it, and the previous build too, so that nothing older than the files answers; the next save builds and starts again.

## How to fix it

- Check what the service does before it binds its listeners: a database it cannot reach, a migration that waits on a lock.
- If it legitimately needs longer, raise the wait: `giong dev --ready-timeout 120`.

[All error codes](README.md)
"#),
    (5019, r#"# GIONG-E5019: the service's giong-mcp does not answer

Raised by the `giong` command (giong-cli): `giong dev`, `giong top` and the dashboard. In JSON output: `"code": "GIONG-E5019"`.

## What happened

The command could not reach the giong-mcp server inside the service: the service does not start it, or it listens elsewhere.

## How to fix it

- Keep `let _mcp = giong_mcp::start_from_env(&rt.handle());` in `main` (projects from `giong new` have it).
- Check `GIONG_MCP` and `GIONG_MCP_TOKEN`; under `giong dev` they are set for you.

[All error codes](README.md)
"#),
    (5020, r#"# GIONG-E5020: too many files to watch

Raised by the `giong` command (giong-cli): `giong dev`. In JSON output: `"code": "GIONG-E5020"`.

## What happened

The project has more files than `giong dev` watches (a build directory or `node_modules` inside the source tree, for example). It watches the first ones and ignores the rest.

## How to fix it

- Keep build output out of the source tree, or list the directories to skip in `.gitignore`: `giong dev` skips what git ignores.
- Watch only some directories: `giong dev --watch src --watch migrations`.

[All error codes](README.md)
"#),
    (5021, r#"# GIONG-E5021: no linker found

Raised by the `giong` command (giong-cli): `giong doctor`. In JSON output: `"code": "GIONG-E5021"`.

## What happened

Rust needs a system linker (`cc` on Linux and macOS, the MSVC build tools on Windows) to make a binary, and none was found.

## How to fix it

- Linux: install your distribution's `build-essential` (Debian, Ubuntu) or `gcc` package.
- macOS: `xcode-select --install`.
- Windows: install the Visual Studio Build Tools with the C++ workload.

[All error codes](README.md)
"#),
    (5022, r#"# GIONG-E5022: giong dev already serves this project

Raised by the `giong` command (giong-cli): `giong dev`. In JSON output: `"code": "GIONG-E5022"`.

## What happened

Another `giong dev` is running for the same project: `.giong/dev.json` names its process, and that process is alive. Two would fight over the same port and the same build directory.

## How to fix it

- Use the one that runs: it rebuilds and restarts on every save. `giong dev status` says where it serves and how its last build went; `giong dev logs` prints the service's output (all of it is in `.giong/dev.log`).
- To start over, stop it first: `giong dev stop` (or Ctrl-C in its terminal), then `giong dev --background` or `giong dev`.

[All error codes](README.md)
"#),
    (5023, r#"# GIONG-E5023: the compiler has a warning

Raised by the `giong` command (giong-cli): `giong check`, `giong dev`. In JSON output: `"code": "GIONG-E5023"`.

## What happened

rustc compiled the code but warned about it: an unused variable or import, unreachable code, a deprecated item. The finding carries the lint name (`[unused_variables]`) and the compiler's whole message.

## How to fix it

- Do what the compiler's message says; when its suggestion is machine-applicable, `giong fix` applies it.
- If the warning is wrong for this code, allow it where it applies with `#[allow(name)]` and a comment saying why.

[All error codes](README.md)
"#),
    (5024, r#"# GIONG-E5024: the service would listen on every network interface

Raised by the `giong` command (giong-cli): `giong dev`. In JSON output: `"code": "GIONG-E5024"`. A warning: `giong dev` goes on.

## What happened

`LISTEN` (from `--listen`, the environment or `.env`) names `0.0.0.0` or `[::]`, every network interface of the machine. That is right for a container in production, where only the published port is reachable. On a laptop it puts the service under development, its debug routes and its giong-mcp in reach of everyone on the same network (an office, a café, a conference).

So `giong dev` serves it on loopback instead: `127.0.0.1` for `0.0.0.0`, `::1` for `[::]`, with the same port, and gives the service `LISTEN` set to that address.

With `--lan`, `giong dev` keeps the address as written and the warning says that other machines can reach the service.

## How to fix it

- Nothing to do if loopback is what you want; to silence the warning, set `LISTEN=127.0.0.1:8080` (your port) in `.env`.
- To try the service from another device (a phone on the same Wi-Fi): `giong dev --lan`. Stop it when you are done.

[All error codes](README.md)
"#),
    (5030, r#"# GIONG-E5030: the project name is not valid

Raised by the `giong` command (giong-cli): `giong new`. In JSON output: `"code": "GIONG-E5030"`.

## What happened

The name of the new project (the last part of its path, or `--name`) cannot be used. It must start with a letter a-z, hold only lower-case letters, digits, `-` and `_`, end with a letter or a digit, be at most 64 characters, and not be a Rust keyword (`fn`, `async`), the name of a crate the project depends on (`giong`, `giong-web`, `serde`), a name Cargo keeps (`test`, `build`) or a device name of Windows (`con`, `nul`, `com1`). The one name is the package, the binary, the directory and, with `_` for `-`, the crate, so it has to work as all four. Nothing was written.

## How to fix it

- Use the name the message suggests, such as `user-service` or `my-api`.
- To keep a directory name the package cannot have, give the package its own: `giong new My.Api --name my-api`.

[All error codes](README.md)
"#),
    (5031, r#"# GIONG-E5031: the directory for the new project is not empty

Raised by the `giong` command (giong-cli): `giong new`. In JSON output: `"code": "GIONG-E5031"`.

## What happened

`giong new` writes into a directory that does not exist yet or is empty, and this one holds files (or is a file, or cannot be read). It never replaces a file, so nothing was written.

## How to fix it

- Choose another path: `giong new api-2`.
- To add the files coding agents read to a project you already have, run `giong ai init` inside it instead.

[All error codes](README.md)
"#),
    (5032, r#"# GIONG-E5032: the template does not offer that choice

Raised by the `giong` command (giong-cli): `giong new`. In JSON output: `"code": "GIONG-E5032"`.

## What happened

The template, a store in `--db`, or the combination asked for is not one `giong new` offers: the templates are `web`, `grpc` and `worker`; the stores `postgres`, `mysql` and `redis`. A project takes one SQL database (postgres or mysql, with redis besides it in `web`); the `worker` template takes its jobs from one store and serves nothing, so it has no `--tls`; the `grpc` template has no database example yet. Nothing was written.

## How to fix it

- Use one of the choices the message names; `giong new --help` lists them.
- For a gRPC service with a database, start from `--template grpc` and add the pool to the service struct by hand, built in each worker as `src/main.rs` of the `web` template does.

[All error codes](README.md)
"#),
    (5033, r#"# GIONG-E5033: a file of the project could not be written

Raised by the `giong` command (giong-cli): `giong new`, `giong ai init` and `giong upgrade`. In JSON output: `"code": "GIONG-E5033"`.

## What happened

A directory or file could not be created: no permission, a full disk, a path that is too long, or a file where a directory has to go. `giong new` removes what it had written, so no half project is left behind; `giong ai init` reports the file and goes on with the others. The same code names a bug in giong-cli's own templates, which the message then says.

## How to fix it

- Check that you may write to the directory and that the disk has space, then run the command again.
- If the message says the templates have a bug, report it at https://github.com/giong-run/Giong/issues with the command you ran.

[All error codes](README.md)
"#),
    (5034, r#"# GIONG-E5034: the project's crates do not build on Windows

Raised by the `giong` command (giong-cli): `giong new`, on Windows, up to giong-cli 1.5.0. In JSON output: `"code": "GIONG-E5034"`.

## What happened

Up to giong 1.4, only the runtime crate (`giong`) built on Windows, so `giong new` warned that the project it wrote would not build there. Since 1.5 every crate builds and passes its tests on Windows, and CI builds and tests the projects `giong new` writes on Windows. giong-cli 1.5.1 and later no longer raise this code.

## How to fix it

- Update giong-cli: `giong upgrade --self`, then `giong upgrade` in the project to move it to the current giong crates.
- The project then builds on Windows as it is. Production still runs best on Linux, where io_uring is available.

[All error codes](README.md)
"#),
    (5035, r#"# GIONG-E5035: --giong-path is not a giong checkout

Raised by the `giong` command (giong-cli): `giong new`. In JSON output: `"code": "GIONG-E5035"`.

## What happened

`--giong-path` makes the new project depend on the giong crates of a checkout by path instead of crates.io (for working on giong itself, and for giong's own CI). The directory given does not exist or is not a checkout of giong: it has no root `Cargo.toml` for the `giong` package with `giong-web/` beside it. Nothing was written.

## How to fix it

- Give the directory of a clone of https://github.com/giong-run/Giong (the one holding its root `Cargo.toml`).
- Leave the flag out to use the released crates.

[All error codes](README.md)
"#),
    (5036, r#"# GIONG-E5036: an agent file exists that giong did not write

Raised by the `giong` command (giong-cli): `giong ai init` (also run by `giong new`). In JSON output: `"code": "GIONG-E5036"`.

## What happened

A file `giong ai init` would write is already there, and giong cannot tell that it is its own, so it was left exactly as it is: `.cursor/rules/giong.mdc` or `.claude/skills/giong/SKILL.md` without the `<!-- giong:owned -->` line, or an `AGENTS.md` or `CLAUDE.md` with a `<!-- giong:begin` marker but no `<!-- giong:end -->` after it. The other files were written.

It is also raised when the file, or a folder on its way, is a symbolic link (a repository can commit `.mcp.json -> ~/some/config`, and git checks it out as a link): giong writes its agent files only as plain files in the project, never through a link. The one link it keeps is `CLAUDE.md` and `AGENTS.md` naming each other, where it writes the file they share; a folder linked to another folder inside the project is fine too.

## How to fix it

- To get giong's version of a whole file, move yours away (or put back its `<!-- giong:owned -->` line) and run `giong ai init` again.
- In `AGENTS.md` or `CLAUDE.md`, delete the stray giong marker or close the block; giong keeps everything outside its block.
- For a link, replace it with a plain file or folder and run `giong ai init` again.
- To keep your own file, do nothing: the warning repeats on each run, nothing else happens.

[All error codes](README.md)
"#),
    (5037, r#"# GIONG-E5037: .mcp.json is not valid JSON

Raised by the `giong` command (giong-cli): `giong ai init` (also run by `giong new`). In JSON output: `"code": "GIONG-E5037"`.

## What happened

`giong ai init` adds the `giong` MCP server (this command over stdio) to `.mcp.json`, keeping every other server and key. The file is not one JSON object with an object under `mcpServers`, so giong left it untouched rather than guess.

## How to fix it

- Fix the JSON (the message gives the line and column), then run `giong ai init` again.
- Or add the servers by hand: `"giong": {"command": "giong", "args": ["mcp"]}` under `mcpServers`.

[All error codes](README.md)
"#),
    (5038, r#"# GIONG-E5038: the project uses another giong version than this command

Raised by the `giong` command (giong-cli): `giong ai init`. In JSON output: `"code": "GIONG-E5038"`.

## What happened

The project depends on a giong `major.minor` other than the one this `giong` command comes with. The skill and the rules `giong ai init` writes describe the command's release, so an agent may use an API the project's version lacks, or miss one it has. The files were written anyway.

## How to fix it

- Install the command that matches the project: `cargo install giong-cli --version ~1.4` for a project on giong 1.4, then run `giong ai init` again.
- Or move the project to the command's version: `giong upgrade --to <the command's version>` (`giong upgrade` moves both to the newest release).

[All error codes](README.md)
"#),
    (5039, r#"# GIONG-E5039: the project does not depend on giong

Raised by the `giong` command (giong-cli): `giong ai init` and `giong upgrade`. In JSON output: `"code": "GIONG-E5039"`.

## What happened

No package of the workspace has `giong` in its `[dependencies]`, so the rules written for agents may not apply here. The files were written anyway, so a project about to move to giong can have them first.

## How to fix it

- From `giong upgrade`: the project takes no giong crate from crates.io. A giong checkout (`path = ...`) or a git dependency is updated where it comes from (`git pull`).
- `cargo add giong` (and the giong crates you need), or start from a template with `giong new`.
- If this is not a giong project, delete the giong block from `AGENTS.md` and `CLAUDE.md` and the files `.cursor/rules/giong.mdc` and `.claude/skills/giong/`.

[All error codes](README.md)
"#),
    (5040, r#"# GIONG-E5040: neither Docker nor Podman answers

Raised by the `giong` command (giong-cli): `giong db` up and down. In JSON output: `"code": "GIONG-E5040"`.

## What happened

`giong db up` starts the development database in a container, and neither `docker info` nor `podman info` succeeded: neither is installed, or its daemon or virtual machine is not running. `GIONG_CONTAINER_RUNTIME` names the one to use when both are installed.

Docker Desktop on Windows can also answer in Windows-container mode, where no database image pulls (they are Linux images): `giong db up` then raises this code with "runs Windows containers" in the message.

## How to fix it

- Install and start Docker (Docker Desktop or OrbStack on a Mac) or Podman (`podman machine start` on a Mac or Windows).
- On Windows, switch Docker Desktop to Linux containers (right-click its tray icon: Switch to Linux containers).
- Or run the database yourself and put its URL in `DATABASE_URL` (or `REDIS_URL`): `giong db migrate` and `giong db status` then use it through the locally installed client.

[All error codes](README.md)
"#),
    (5041, r#"# GIONG-E5041: the database does not answer queries

Raised by the `giong` command (giong-cli): `giong db` up, status and migrate. In JSON output: `"code": "GIONG-E5041"`.

## What happened

The database did not answer a query in time, or refused the connection: it is still starting (MySQL's first start takes several seconds), it stopped, or the URL's user, password or database is wrong. The message ends with what the database client said.

## How to fix it

- `giong db up --timeout 300` waits longer; `docker logs <container>` shows what the database is doing.
- Check `DATABASE_URL` against the database; `giong db status` shows what giong started and where.

[All error codes](README.md)
"#),
    (5042, r#"# GIONG-E5042: no usable database URL

Raised by the `giong` command (giong-cli): `giong db` migrate and status. In JSON output: `"code": "GIONG-E5042"`.

## What happened

`giong db` found no database to use: `giong db up` started none in this project and `DATABASE_URL` (or `REDIS_URL`) is set neither in the environment nor in `.env`, or it is set to something that is not a `postgres://`, `mysql://` or `redis://` URL.

## How to fix it

- Run `giong db up`, which writes the URL into `.env`.
- Or set `DATABASE_URL=postgres://user:password@host:5432/db` (or `mysql://...:3306/db`).

[All error codes](README.md)
"#),
    (5043, r#"# GIONG-E5043: an applied migration was changed

Raised by the `giong` command (giong-cli): `giong db` migrate and status. In JSON output: `"code": "GIONG-E5043"`.

## What happened

A file in `migrations/` was edited after it was applied: its SHA-256 checksum differs from the one recorded in `giong_migrations`. The database does not have the change, and applying it again could fail or duplicate data, so `giong db migrate` applies nothing until it is resolved.

## How to fix it

- Undo the edit (`git checkout migrations/<file>`) and put the new SQL in a new migration: `giong db new <name>`.
- In development, `giong db reset --yes` drops the database giong started and applies every migration from the start.

[All error codes](README.md)
"#),
    (5044, r#"# GIONG-E5044: a migration failed

Raised by the `giong` command (giong-cli): `giong db` migrate and reset. In JSON output: `"code": "GIONG-E5044"`.

## What happened

The database refused a statement of a migration. The finding points at the file and line, with the database's message. On PostgreSQL each file runs in one transaction, so nothing of the failed file was applied and it is not recorded; later files were not run. On MySQL (which commits DDL statement by statement) and for PostgreSQL files marked `-- giong:no-transaction`, the statements before the failing one stay applied.

## How to fix it

- Fix the SQL and run `giong db migrate` again.
- On MySQL or a no-transaction file, make the statements safe to run twice (`CREATE TABLE IF NOT EXISTS`, `DROP ... IF EXISTS`) or undo the applied ones by hand first.

[All error codes](README.md)
"#),
    (5045, r#"# GIONG-E5045: a migration file is not valid

Raised by the `giong` command (giong-cli): `giong db` new, migrate and status. In JSON output: `"code": "GIONG-E5045"`.

## What happened

A `.sql` file in `migrations/` is not named `NNNN_name.sql` (at least four digits, an underscore, then lower-case letters, digits or `_`), two files share a version number, or a file is not UTF-8. Other files (a README) are ignored.

## How to fix it

- Rename the file, for example `0003_add_index.sql`; `giong db new <name>` writes the next number for you.
- Two branches that each added a migration with the same number: give the newer one the next free number.

[All error codes](README.md)
"#),
    (5046, r#"# GIONG-E5046: no database client to run the SQL

Raised by the `giong` command (giong-cli): `giong db` migrate, status and reset. In JSON output: `"code": "GIONG-E5046"`.

## What happened

`giong db` runs SQL through the database's own client. For a database `giong db up` started it uses the client inside the container; for any other `DATABASE_URL` it needs `psql` or `mysql` installed here, and it is not.

## How to fix it

- Install the client: `postgresql-client` (`brew install libpq` on a Mac) or `mysql-client` / `mariadb-client`.
- Or let `giong db up` start the development database.

[All error codes](README.md)
"#),
    (5047, r#"# GIONG-E5047: the database has a migration the project does not

Raised by the `giong` command (giong-cli): `giong db` migrate and status. In JSON output: `"code": "GIONG-E5047"`.

## What happened

`giong_migrations` records a version for which `migrations/` has no file: it was applied from another branch, or its file was deleted or renumbered. A warning: the other migrations still apply.

## How to fix it

- Switch to the branch that has it, or restore the file.
- In development, `giong db reset --yes` starts the database over from the files you have.

[All error codes](README.md)
"#),
    (5048, r#"# GIONG-E5048: the database was not started by giong db up

Raised by the `giong` command (giong-cli): `giong db` reset. In JSON output: `"code": "GIONG-E5048"`.

## What happened

`giong db reset` drops and recreates the database, so it acts only on a database `giong db up` started in this project (listed in `.giong/db.json`), never on one given by `DATABASE_URL`, which could be shared or in production.

## How to fix it

- Start the development database with `giong db up` and reset that.
- To empty another database, do it yourself with its own tools, deliberately.

[All error codes](README.md)
"#),
    (5049, r#"# GIONG-E5049: the database container could not start

Raised by the `giong` command (giong-cli): `giong db` up and down. In JSON output: `"code": "GIONG-E5049"`.

## What happened

Docker or Podman could not create, start or remove the database container, or the container stopped while starting. The message carries the runtime's or the database's own words: a port taken by another program, an image that cannot be pulled, or a data volume written by another major version of the engine. It is also raised when a container with the project's name exists but `.giong/db.json` does not list it (the file was deleted or the project copied without it): its password is not known, so `giong db up` does not guess one.

## How to fix it

- A taken port: `giong db down`, then `giong db up` picks a free one (or `--port`).
- An image: check `--image` and the network, or `docker pull` it first.
- A volume from another version: `giong db down --volumes` deletes the data and starts fresh.
- A container `.giong/db.json` does not list: `giong db down` removes it (`--volumes` also deletes its data), or restore `.giong/db.json` to keep it.

[All error codes](README.md)
"#),
    (5050, r#"# GIONG-E5050: a recorded value looks like a secret

Raised by the `giong` command (giong-cli): `giong record save`. In JSON output: `"code": "GIONG-E5050"`. As an error, nothing was saved; as a warning (with `--keep-secrets`), the value was saved.

## What happened

Case files (`tests/recorded/*.json`) are tests, meant to be committed, so `save` keeps secrets out of them. Redacted headers and request fields whose names say secret (`password`, `token`, `api_key`, `client_secret`, `card_number`, `cvv`, `ssn`, ...) are written as `{{env:GIONG_RECORD_<NAME>}}` and read from the environment when replayed. After that, `save` looks at every value left. It refuses when one still looks like a secret:

- a card number: 13 to 19 digits (spaces or dashes allowed between groups) starting with 2 to 6 and passing the Luhn check, under a field whose name says nothing (`"pan": "4242424242424242"`)
- a JWT: `eyJ...` followed by two more dot-separated parts, in a body, a header or a `Location`
- a value under a secret name that was not replaced, such as a header `x-session-token`

The message names each place, as `<case>: <where> (<what>)`, for example `login_200: request.body.pan (a card number)`.

## How to fix it

- Leave that exchange out: `giong record list` shows the numbers, and `save 1-3,5` saves only those.
- Record the request again with test values, then save the new exchange.
- When the values already are test values (the `4242 4242 4242 4242` test card, a token made for tests), pass `--keep-secrets`. The values are saved, and each one is reported as a warning.
- To check a file by hand, look for the places the message names; the rest of the file is as `save` would have written it.

[All error codes](README.md)
"#),
    (5051, r#"# GIONG-E5051: the dashboard refused a request without its token

Raised by the `giong` command (giong-cli): the dashboard of `giong dashboard` and `giong dev --ui`. The dashboard's API answers 403 with `"code": "GIONG-E5051"`.

## What happened

Each start of the dashboard makes a random token and prints the address with it: `http://127.0.0.1:7700/#t=<32 hex digits>`. Every request to the dashboard's API (`/api/...`) must send that token in the `X-Giong-Dashboard` header, and the page does so. A request without it, or with another start's token, is refused. This keeps the live request log (which holds bodies) and the send box away from other programs and users on the machine, and from web pages.

Most often the page was opened without the printed address: by typing `127.0.0.1:7700`, from a bookmark, or in a tab left over from a dashboard that has since restarted (each start has a new token).

## How to fix it

- Open the address `giong dashboard` or `giong dev --ui` printed, with its `#t=...` part. The page keeps the token for reloads in that tab.
- With `giong dashboard --json` or `giong dev --ui --json`, the address is in `data.url` or the `dashboard` event's `url`.
- A script that calls the API sends the header: `curl -H "X-Giong-Dashboard: <token>" http://127.0.0.1:7700/api/state`.

[All error codes](README.md)
"#),
    (5053, r#"# GIONG-E5053: the dashboard's port is in use

Raised by the `giong` command (giong-cli): `giong dashboard`. In JSON output: `"code": "GIONG-E5053"`.

## What happened

`giong dashboard` serves its page on `127.0.0.1:7700` (or `--port`), and something else already listens there: most often another dashboard, opened by `giong dashboard` or `giong dev --ui` in another terminal. (`giong dev --ui` itself tries the nine ports after 7700 before giving up, and prints the one it took.)

## How to fix it

- Open the dashboard that is already running, at the address it printed (`http://127.0.0.1:7700/#t=...`: the token is part of it).
- Or pick another port: `giong dashboard --port 7710`.
- To see what holds the port: `lsof -i :7700` (macOS, Linux) or `ss -ltnp 'sport = :7700'` (Linux).

[All error codes](README.md)
"#),
    (5054, r#"# GIONG-E5054: the dashboard could not do what the page asked

Raised by the `giong` command (giong-cli): `giong dashboard` and `giong dev --ui`, in the answers the dashboard page shows. In JSON: `"code": "GIONG-E5054"`.

## What happened

The dashboard page asked the `giong` command for something it could not do: the service's log could not be read (permissions), saving a recorded request as a case could not run `giong record save`, or the dashboard failed while answering (a bug: the terminal running the dashboard shows the panic). It also stops with this code when it cannot listen at all for a reason other than a taken port.

## How to fix it

- Check that the log `giong dev` writes (`.giong/dev.log`) can be read by you.
- Run the step by hand to see its full report: `giong record save <number> --name <name>` (numbers from `giong record list`).
- If the terminal shows a panic, report it with that text: it is a bug in the dashboard, not in your service.

[All error codes](README.md)
"#),
    (5055, r#"# GIONG-E5055: a worker of the service is stuck in one task

Raised by the `giong` command (giong-cli): `giong top` and the dashboard. In JSON output: `"code": "GIONG-E5055"`.

## What happened

giong-mcp saw a worker that is awake but has not moved on for a while: one task's poll has not returned. giong runs many tasks on each worker thread, so every other task on that worker (other requests, timers, pool connections) waits. The finding names the line that spawned the task; in a giong-http or giong-web service that is giong-http's connection task, which runs the request handlers.

## How to fix it

- Look for a blocking call in the code the task runs: `std::fs`, `std::thread::sleep`, `std::net`, a blocking client, a `Mutex` held long, or a loop that computes without `.await`.
- Use the giong equivalent (`giong::fs`, `giong::time::sleep`, `giong::net`, giong's database and HTTP clients), move the work to a thread of its own, or let other tasks run with `giong::yield_now().await` inside long loops.
- `giong top --profile 2000` shows where the workers spend their time.

[All error codes](README.md)
"#),
    (5056, r#"# GIONG-E5056: the service reports a warning

Raised by the `giong` command (giong-cli): `giong top` and the dashboard. In JSON output: `"code": "GIONG-E5056"`.

## What happened

giong-mcp's `overview` listed a warning other than a stuck worker: workers that fell back to the epoll driver because io_uring is blocked, or a process close to its open-files limit. The detail is the warning's text.

## How to fix it

- Read the detail. For epoll: see [GIONG-E0001](E0001.md) and `giong doctor`. For open files: raise the limit (`ulimit -n`, `LimitNOFILE=` in a systemd unit, or the container's `nofile` ulimit) or find the leak (`giong top` lists tasks by spawn line).

[All error codes](README.md)
"#),
    (5057, r#"# GIONG-E5057: no running service to ask

Raised by the `giong` command (giong-cli): `giong routes`, `giong openapi`, `giong top` and `giong dashboard`. In JSON output: `"code": "GIONG-E5057"`.

## What happened

The command reads a running service through its giong-mcp server, and found none to ask: no `--mcp` flag, no `GIONG_MCP` in the environment, and no `giong dev` serving the project in the current directory (`.giong/dev.json`).

## How to fix it

- Start the service with `giong dev` in the project and run the command from the same project.
- Or run the service with `GIONG_MCP=127.0.0.1:7777` and `GIONG_MCP_TOKEN` set (16 characters or more), keep `let _mcp = giong_mcp::start_from_env(&handle);` alive in it, and pass `--mcp 127.0.0.1:7777` with the same `GIONG_MCP_TOKEN`.
- A service listening on a Unix socket: `--mcp unix:/path/to/mcp.sock` (no token needed).

[All error codes](README.md)
"#),
    (5058, r#"# GIONG-E5058: the service's giong-mcp lacks a tool

Raised by the `giong` command (giong-cli): `giong routes`, `giong openapi` and `giong dashboard`. In JSON output: `"code": "GIONG-E5058"`.

## What happened

The service's giong-mcp answered, but has no tool the command needs. The `routes` tool is there only when giong-mcp is built with its `web` feature, and only from giong-mcp 1.5.

## How to fix it

- Turn on the feature in the service's `Cargo.toml`: `giong-mcp = { version = "1.5", features = ["web"] }`, with giong-web 1.5 or newer, and rebuild.
- Use a giong-mcp of the same version as the `giong` command (`giong --version`).

[All error codes](README.md)
"#),
    (5059, r#"# GIONG-E5059: the service's giong-mcp refused a call

Raised by the `giong` command (giong-cli): `giong routes`, `giong openapi`, `giong top` and `giong dashboard`. In JSON output: `"code": "GIONG-E5059"`.

## What happened

The service's giong-mcp answered the call with an error, or with an answer this command cannot read. The message says which: a refused argument, a worker that did not answer, or an answer of another version.

## How to fix it

- Read the message: a worker that does not answer is stuck (`giong top` shows which and in which task).
- Use giong-mcp and the `giong` command of the same version.

[All error codes](README.md)
"#),
    (5060, r#"# GIONG-E5060: the service to measure cannot be reached

Raised by the `giong` command (giong-cli): `giong bench`. In JSON output: `"code": "GIONG-E5060"`.

## What happened

`giong bench` could not measure the service: the URL is not a plain `http://` URL, its host does not resolve, nothing answers there, the service it started did not listen on the address in `LISTEN` in time, or the service exited during a round. The message names which, with the last lines of the service's log (`.giong/bench/service.log`).

## How to fix it

- Give a URL like `http://127.0.0.1:8080/`. HTTPS is not measured: start the service without `TLS_CERT` and `TLS_KEY` for the benchmark.
- When `giong bench` starts the service, the service must listen on the address in `LISTEN` (services made by `giong new` do). Otherwise start it yourself and give its URL: `giong bench http://127.0.0.1:8080/`.
- If the service exited, read `.giong/bench/service.log`: a missing `DATABASE_URL` (`giong env check`) or a panic is the usual cause.

[All error codes](README.md)
"#),
    (5061, r#"# GIONG-E5061: the build to compare against cannot be made

Raised by the `giong` command (giong-cli): `giong bench --against`. In JSON output: `"code": "GIONG-E5061"`.

## What happened

`--against <ref>` names a git commit, branch or tag to build and measure next to the working tree. The project is not in a git repository, the ref does not name a commit, `git worktree add` failed, or that commit does not build in release mode.

## How to fix it

- Check the ref: `git rev-parse --verify <ref>` must print a commit. Fetch it first if it is a remote branch (`git fetch origin main`, then `--against origin/main`).
- If the older commit does not build, pick a later one, or build it by hand to see why (the message has cargo's first error).

[All error codes](README.md)
"#),
    (5062, r#"# GIONG-E5062: the load generator fell behind its schedule

Raised by the `giong` command (giong-cli): `giong bench` with a fixed rate. In JSON output: `"code": "GIONG-E5062"`.

## What happened

With a fixed rate, every request has a time it is due, and its latency counts from then, so a service that stalls is charged for every request that waited (no coordinated omission). Here the load generator itself woke late: at the 99th percentile it sent requests more than a millisecond after they were due, because the machine is busy (the service and the generator share its cores) or the rate needs more threads than the machine runs well. Those latencies include the generator's own delay.

## How to fix it

- Lower `--rate` or `--conns`, close other programs, or run the generator on another machine against the service's URL.
- On Linux, a machine without frequency scaling or other load gives steadier numbers; numbers that matter come from the benchmark lab (`bench/lab`), not a laptop.

[All error codes](README.md)
"#),
    (5063, r#"# GIONG-E5063: requests failed during the benchmark

Raised by the `giong` command (giong-cli): `giong bench`. In JSON output: `"code": "GIONG-E5063"`.

## What happened

Some requests got no answer (the connection was refused, reset or timed out) or got an error status (4xx or 5xx). Their count is in the report; latencies of answered requests are still reported, but a service that fails fast looks faster than it is. More than 1% failed is an error; fewer is a warning. When every answer has an error status (a 404 for a path the service has no route for), `giong bench` stops after the probe or the first round instead of measuring the same error through every round.

## How to fix it

- Check the path and method: `giong bench --path /health` for a route that exists, `--method POST --data '{...}'` for one that needs a body.
- Read the service's log (`.giong/bench/service.log` when `giong bench` started it): a pool too small for `--conns`, a database that is not up, or a timeout shorter than the service's work (`--timeout`).

[All error codes](README.md)
"#),
    (5064, r#"# GIONG-E5064: the change is slower than the build it is compared with

Raised by the `giong` command (giong-cli): `giong bench --against <ref> --max-regression <percent>`. In JSON output: `"code": "GIONG-E5064"`.

## What happened

In every round the working tree was slower than the build of `<ref>` (99th percentile latency at a fixed rate, or requests per second with `--rate max`), and the median difference is larger than `--max-regression`. Rounds alternate between the two builds, so a slow machine affects both. With 5 rounds a build (the default), two identical builds would separate in every round by chance less than 1% of the time; `--max-regression` needs at least 3 rounds a build (10%).

## How to fix it

- Profile the change: `giong top --profile` on the running service shows where the time goes.
- If the slowdown is expected (more work per request), raise `--max-regression` for this comparison and say why in the pull request.

[All error codes](README.md)
"#),
    (5065, r#"# GIONG-E5065: no recorded requests to use

Raised by the `giong` command (giong-cli): `giong record list`, `giong record save` and `giong record replay`. In JSON output: `"code": "GIONG-E5065"`.

## What happened

`giong record` reads the `exchange` lines that giong-web's `Record` middleware writes, one per answered request, from the service's log (by default the log `giong dev` keeps in `.giong/dev.log`), or replays case files from `tests/recorded/`. There was nothing to read: no log (giong dev is not running in this project and no `--from` was given), a log without any `exchange` line (the service does not record), or no case file to replay.

## How to fix it

- Add the middleware to the router, for example only when `GIONG_RECORD` is set, and put it after `RequestId` so lines carry the request id:

  ```rust
  if std::env::var_os("GIONG_RECORD").is_some() {
      router = router.layer(Record::new(Sink::Stdout));
  }
  ```

- Run the service with `giong dev` (it keeps the service's output in `.giong/dev.log`) and send it some requests, then `giong record list`.
- Or point at another log: `giong record list --from service.log` (`--from -` reads standard input).
- To replay, save cases first: `giong record save 3-7 --name users` writes `tests/recorded/users.json`.

[All error codes](README.md)
"#),
    (5066, r#"# GIONG-E5066: a recorded case answers differently

Raised by the `giong` command (giong-cli): `giong record replay`. In JSON output: `"code": "GIONG-E5066"`. The finding points at the case in its file.

## What happened

A case's request was sent to the running service and the answer did not match what the case expects: the status, the `Content-Type` or `Location` header, or the body. The finding lists the first differences, each with the place in the body (`body.users[0].name: expected "ada", got "bob"`). When the status differs and the answer is a giong-web problem+json, its code and title are shown too.

A case that fails still keeps the values it captured (`{{capture:NAME}}`) from fields that were there; a field that is missing captures nothing, and later cases that use it fail with GIONG-E5068.

## How to fix it

- If the service changed on purpose, save the case again from a new recording (`giong record save <number> --name <name> --force`), or edit the expectation in the file: it is plain JSON.
- If a value changes on every run and the case has it literally, replace it with a placeholder: `{{any}}`, `{{string}}`, `{{number}}`, `{{uuid}}`, `{{time}}`, `{{hex}}`, or `{{capture:NAME}}` to remember it for later cases (used as `{{NAME}}`).
- Replay against a fresh service: a case that creates something expects the state it was recorded in (an empty table, a free email address).
- If neither applies, the service has a regression: the differences say where.

[All error codes](README.md)
"#),
    (5067, r#"# GIONG-E5067: a recording cannot be read or used

Raised by the `giong` command (giong-cli): `giong record list`, `giong record save` and `giong record replay`. In JSON output: `"code": "GIONG-E5067"`.

## What happened

One of these:

- A case file is not valid: not JSON, not in the format this `giong` reads (`"giong_record": 1`), or a case lacks `name`, `request.method`, a `request.path` starting with `/` or an `expect.status`, or has two bodies. The finding names the case.
- A line of the log says it is an exchange but cannot be read (cut short, or changed by hand).
- An exchange cannot become a case: its request body was streamed (`Router::stream_bodies`) or cut at `Record::max_body`, so the request cannot be sent again. Its answer may also be compressed, cut or streamed: then the case is saved but its body is marked `body_unchecked`.
- A case's request is not valid HTTP (a method with spaces, a path with a line break).

## How to fix it

- Fix the file; its format, in short:

  ```json
  {"giong_record": 1, "cases": [{
    "name": "post_users_201",
    "request": {"method": "POST", "path": "/users", "query": null,
                "headers": [["content-type", "application/json"]], "body_json": {"name": "ada"}},
    "expect": {"status": 201, "headers": {"content-type": "application/json"},
               "body_json": {"id": "{{capture:user_id}}", "name": "ada"}}}]}
  ```

  A body is one of `body` (text), `body_json`, `body_base64`, or in `expect` only `body_unchecked` (a reason).
- Record large bodies whole: `Record::new(sink).max_body(128 * 1024)`.
- To check answer bodies, put `Record` inside `Compression` (add `Record` after it), so it sees bodies before they are compressed.

[All error codes](README.md)
"#),
    (5068, r#"# GIONG-E5068: a recorded case needs a value that is not set

Raised by the `giong` command (giong-cli): `giong record replay`. In JSON output: `"code": "GIONG-E5068"`. The finding points at the case in its file.

## What happened

A case's request has a placeholder with no value yet, so it was not sent:

- `{{env:GIONG_RECORD_AUTHORIZATION}}` (or another `GIONG_RECORD_*` variable): the header was redacted when recorded (`Authorization`, `Cookie`, API keys), and the environment variable that gives it is not set.
- `{{NAME}}`: the value is captured by an earlier case's answer (`{{capture:NAME}}`), and that answer did not have the field (or the case does not exist in this file).
- `{{uuid}}`, `{{any}}` and the other class placeholders match answers; a request needs a real value there.

## How to fix it

- Set the variable for the replay: `GIONG_RECORD_AUTHORIZATION='Bearer <token>' giong record replay`. In CI, take it from a secret.
- Fix the earlier case first (its GIONG-E5066 finding names the missing field); cases run in file order and share captured values within one file.
- Replace a class placeholder in a request with a value.

[All error codes](README.md)
"#),
    (5069, r#"# GIONG-E5069: the service to replay against does not answer

Raised by the `giong` command (giong-cli): `giong record replay`. In JSON output: `"code": "GIONG-E5069"`.

## What happened

`giong record replay` sends each case's request to the service over plain HTTP. Nothing answered at the address it used, the connection broke, the answer was not HTTP/1.1, or no whole answer came within the timeout (`--timeout-ms`, default 10000). The replay stops there, since every later case would fail the same way. The address is `--base-url`, else the first address `giong dev` listens on, else `http://127.0.0.1:8080`.

## How to fix it

- Start the service: `giong dev` in the project, then replay from the same project.
- Give the address it listens on: `giong record replay --base-url http://127.0.0.1:3000`. HTTPS is not supported: replay against a plain-HTTP listener of the service in development.
- A handler that takes longer than the timeout: raise `--timeout-ms`, or find out why it is slow (`giong top`).

[All error codes](README.md)
"#),
    (5070, r#"# GIONG-E5070: the service cannot be built for Linux here

Raised by the `giong` command (giong-cli): `giong image`. In JSON output: `"code": "GIONG-E5070"`.

## What happened

An image runs a Linux binary. On Linux, `giong image` builds for this machine, statically with musl when the `x86_64-unknown-linux-musl` (or `aarch64-unknown-linux-musl`) target is installed, otherwise against glibc with this machine's libraries copied in. On macOS and Windows it needs a musl target and `cargo zigbuild` to cross-build. Here neither works, or the build is not a Linux ELF binary.

## How to fix it

- On Linux: nothing is needed; for a smaller, fully static image, `rustup target add x86_64-unknown-linux-musl`.
- On macOS or Windows: `rustup target add x86_64-unknown-linux-musl` (or `aarch64-unknown-linux-musl` for ARM servers) and `cargo install cargo-zigbuild` (it needs Zig: `brew install zig` or `winget install zig.zig`).
- Give the target yourself with `--target <triple>` when the server's architecture differs from this machine's.

[All error codes](README.md)
"#),
    (5071, r#"# GIONG-E5071: the shared libraries of the binary cannot be found

Raised by the `giong` command (giong-cli): `giong image`. In JSON output: `"code": "GIONG-E5071"`.

## What happened

The binary is linked against glibc, so the image needs the libraries it loads (`libc.so.6`, `libgcc_s.so.1`, the dynamic loader). `giong image` asks `ldd` for them and copies them from this machine; `ldd` is missing, failed, or names a library that is not installed.

## How to fix it

- Build statically instead: `rustup target add x86_64-unknown-linux-musl`, then run `giong image` again (it picks the musl target by itself).
- Install the missing library with the system's package manager, or run `ldd` on the binary to see what it lacks.

[All error codes](README.md)
"#),
    (5072, r#"# GIONG-E5072: no CA certificates to put in the image

Raised by the `giong` command (giong-cli): `giong image`. In JSON output: `"code": "GIONG-E5072"`.

## What happened

The image carries the CA certificates of this machine (`/etc/ssl/certs/ca-certificates.crt` or the other usual places), so the service can check the certificates of HTTPS servers, databases and APIs it calls. None was found, so the image has none: outgoing TLS connections will fail to verify. A service that makes no outgoing TLS connection does not need them.

## How to fix it

- Install the CA bundle (`ca-certificates` on Debian, Ubuntu and Alpine; `ca-certificates` or `openssl` on macOS through Homebrew), or point `SSL_CERT_FILE` at a PEM bundle before running `giong image`.

[All error codes](README.md)
"#),
    (5073, r#"# GIONG-E5073: a file to add to the image is not usable

Raised by the `giong` command (giong-cli): `giong image --add`. In JSON output: `"code": "GIONG-E5073"`.

## What happened

`--add` takes paths inside the project (comma-separated, such as `static,migrations`), copied under `/app` in the image with the same relative path. This one does not exist, is outside the project (absolute or with `..`), or cannot be read.

## How to fix it

- Give a path relative to the project root that exists: `giong image --add static,migrations`.
- Files the service needs at run time go under `/app`, its working directory; read them with a relative path (`static/index.html`).

[All error codes](README.md)
"#),
    (5074, r#"# GIONG-E5074: the image could not be loaded into Docker or Podman

Raised by the `giong` command (giong-cli): `giong image --load`. In JSON output: `"code": "GIONG-E5074"`.

## What happened

`--load` hands the image file to `docker load` or `podman load` (`GIONG_CONTAINER_RUNTIME` chooses), so it can be run right away. Neither answers, or the load failed; the image file itself was written and is still usable.

## How to fix it

- Start Docker or Podman, or load the file yourself: `docker load -i <file>` (Docker 25 or newer reads OCI archives), `podman load -i <file>`.
- Leave out `--load` to only write the file, and push it with `giong image --push` or `skopeo copy oci-archive:<file> docker://<ref>`.

[All error codes](README.md)
"#),
    (5075, r#"# GIONG-E5075: the image could not be pushed to the registry

Raised by the `giong` command (giong-cli): `giong image --push <ref>`. In JSON output: `"code": "GIONG-E5075"`.

## What happened

`--push` sends the image with the first tool found: `skopeo`, `crane`, then `docker` or `podman` (load, tag, push). None is installed, the registry refused the login, or the reference names a repository the account cannot write to. The image file was written and is still usable.

## How to fix it

- Log in first: `docker login ghcr.io` (crane and skopeo read the same credentials), or `skopeo login`.
- Check the reference: `ghcr.io/<owner>/<name>:<tag>`, with a repository the account can push to.

[All error codes](README.md)
"#),
    (5076, r#"# GIONG-E5076: the image name or tag is not valid

Raised by the `giong` command (giong-cli): `giong image --tag` and `--push`. In JSON output: `"code": "GIONG-E5076"`.

## What happened

An image reference is `[registry/]name[:tag]`: the name in lower case (letters, digits, and `.`, `_`, `-` between them, `/` between parts), the tag up to 128 letters, digits, `_`, `.` and `-`, not starting with `.` or `-`. By default the name is the package's and the tag its version.

## How to fix it

- Write `--tag myservice:1.2.0` or `--push ghcr.io/owner/myservice:1.2.0`.
- Upper-case letters are not allowed in the name: `MyService` becomes `myservice`.

[All error codes](README.md)
"#),
    (5080, r#"# GIONG-E5080: the project has changes that are not committed

Raised by the `giong` command (giong-cli): `giong fix`. In JSON output: `"code": "GIONG-E5080"`.

## What happened

`giong fix` edits source files, and the project's git repository has changes that are not committed. Mixing its edits with yours would make them hard to tell apart or undo, so nothing was changed.

## How to fix it

- Commit or stash your changes first, then run `giong fix`.
- Or pass `--allow-dirty` to apply the fixes on top of them; `--dry-run` shows the edits first.

[All error codes](README.md)
"#),
    (5081, r#"# GIONG-E5081: cargo could not apply its fixes

Raised by the `giong` command (giong-cli): `giong fix`. In JSON output: `"code": "GIONG-E5081"`.

## What happened

`giong fix` runs `cargo clippy --fix` for the compiler's and clippy's machine-applicable suggestions, and it failed: usually because the code does not compile, so there is nothing for it to check. The fixes giong finds itself were still applied when they could be.

## How to fix it

- Run `giong check` to see the first error, fix it by hand, then run `giong fix` again.
- The message carries the first line cargo printed.

[All error codes](README.md)
"#),
    (5082, r#"# GIONG-E5082: a fix no longer matches the code

Raised by the `giong` command (giong-cli): `giong fix`. In JSON output: `"code": "GIONG-E5082"`.

## What happened

A fix found by `giong check` names bytes of a file that no longer hold the code it was found in (the file changed after the check), or two fixes change the same code. That fix was not applied; the others were.

## How to fix it

- Run `giong fix` again: it checks the code afresh first.
- If it repeats, apply the fix by hand from `giong check`'s message.

[All error codes](README.md)
"#),
    (5083, r#"# GIONG-E5083: a test failed

Raised by the `giong` command (giong-cli): `giong test`. In JSON output: `"code": "GIONG-E5083"`.

## What happened

A test failed. `giong test` runs the suite on the default driver (io_uring on Linux when the kernel allows it), then again with `GIONG_DRIVER=epoll`, because containers with Docker's default seccomp profile get epoll. The finding names the test, the driver it failed on and what it printed.

## How to fix it

- Run that one test with its output: `cargo test <name> -- --nocapture` (add `GIONG_DRIVER=epoll` in front when it failed only on epoll).
- A test that fails only on one driver often reads one message with one `recv()`. TCP keeps no message boundaries: read until the whole message is in (`read_exact`, or a framed reader).
- A test that needs a database reads `DATABASE_URL` or `REDIS_URL`. `giong test` loads them from `.env`; `giong db up` writes them there.

[All error codes](README.md)
"#),
    (5084, r#"# GIONG-E5084: the tests could not run on every driver

Raised by the `giong` command (giong-cli): `giong test`. In JSON output: `"code": "GIONG-E5084"`.

## What happened

`giong test` ran the suite once. On Linux it runs it a second time with `GIONG_DRIVER=epoll`, the driver containers get when io_uring is blocked. The epoll driver exists only on Linux, so on macOS and Windows that second run was left out. This is a note, not an error.

## How to fix it

- Run `giong test` on Linux, or let CI run it there, before you ship to a Linux server or container.
- Projects made by `giong new` already run both drivers in their CI workflow.

[All error codes](README.md)
"#),
    (5085, r#"# GIONG-E5085: cargo-nextest is not installed

Raised by the `giong` command (giong-cli): `giong test --nextest`. In JSON output: `"code": "GIONG-E5085"`.

## What happened

`--nextest` runs the suite with `cargo nextest run`, but `cargo nextest --version` did not work, so cargo-nextest is not installed for this toolchain.

## How to fix it

- Install it: `cargo install cargo-nextest --locked`.
- Or leave out `--nextest`: `giong test` then runs `cargo test`.

[All error codes](README.md)
"#),
    (5086, r#"# GIONG-E5086: the tests took longer than the time limit

Raised by the `giong` command (giong-cli): `giong test`. In JSON output: `"code": "GIONG-E5086"`.

## What happened

A run of the suite went on longer than `--timeout` seconds (1800 by default), so `giong test` stopped cargo and every test process it had started. The finding names the tests libtest reported as running for over 60 seconds. A test that waits for a reply, a lock or a message that never comes runs for ever.

## How to fix it

- Run the named test alone: `cargo test <name> -- --nocapture`, and see where it waits.
- Give every wait for another party a deadline: `giong::time::timeout(duration, fut)`. A test then fails with the reason instead of hanging.
- For a suite that is slow rather than stuck, raise the limit: `giong test --timeout 3600`, or `--timeout 0` for none.

[All error codes](README.md)
"#),
    (5087, r#"# GIONG-E5087: git init failed in the new project

Raised by the `giong` command (giong-cli): `giong new`. In JSON output: `"code": "GIONG-E5087"`.

## What happened

`giong new` wrote every file of the project, then ran `git init` in it, and that failed (the message says why: often git is not installed, or the directory is inside a repository that git refuses to nest in). The project itself is complete. This is a warning.

## How to fix it

- Install git and run `git init` in the project, or leave it out: `giong new --no-git`.
- `giong fix` refuses to run on changes that are not committed, so a project without git needs `giong fix --allow-dirty`.

[All error codes](README.md)
"#),
    (5088, r#"# GIONG-E5088: .env sets a cargo or rustc setting

Raised by the `giong` command (giong-cli): `giong test`. In JSON output: `"code": "GIONG-E5088"`.

## What happened

`.env` holds the service's configuration (`DATABASE_URL`, `REDIS_URL`, ...), and `giong test` gives its variables to the tests. A variable that changes how cargo or rustc build (`RUSTFLAGS`, `RUSTDOCFLAGS`, anything starting with `CARGO`, `RUSTC` or `RUSTUP_`) would change the build of the tests, and only under `giong test`, so it was left out.

## How to fix it

- Put build settings where cargo reads them for every command: `.cargo/config.toml` (`[build] rustflags = [...]`, `[build] target-dir = "..."`), or the shell.
- Delete the variable from `.env`.

[All error codes](README.md)
"#),
    (5090, r#"# GIONG-E5090: a newer giong release is out

Raised by the `giong` command (giong-cli): `giong doctor`. In JSON output: `"code": "GIONG-E5090"`.

## What happened

The project's `Cargo.lock` holds giong crates older than the newest giong release on crates.io. `giong doctor` asks crates.io (through `cargo info giong`, with your cargo configuration) at most once a day and keeps the answer in `~/.giong/cache/releases.json`; `--offline` and `CARGO_NET_OFFLINE=true` skip the question.

Releases within 1.x only add (docs/STABILITY.md), so moving up keeps code that built building. Each release lists its fixes in the CHANGELOG.

## How to fix it

- `giong upgrade --dry-run` shows the edits, `giong upgrade` makes them: every giong version in every `Cargo.toml`, `cargo update`, `cargo check`, and back as it was if a step fails.
- To stay on a release on purpose, pin it (`giong = "=1.4.2"`): the warning stays, as a reminder.

[All error codes](README.md)
"#),
    (5091, r#"# GIONG-E5091: the project's giong crates are on different releases

Raised by the `giong` command (giong-cli): `giong doctor` and `giong upgrade`. In JSON output: `"code": "GIONG-E5091"`.

## What happened

The giong crates are released together, with one version, and meant to be used at one version. This project's `Cargo.lock` has them at different versions (for example `giong 1.5.0` with `giong-web 1.4.2`), or has one of them twice because another dependency asks for an older one. Each crate works with a newer runtime, but the newer crates' fixes and the agent files `giong ai init` writes assume one release.

From `giong upgrade`: after `cargo update`, a giong crate stayed on another version. The files were put back as they were.

## How to fix it

- `giong upgrade` moves every giong crate to one release.
- When a crate is in the lock twice, `cargo tree -i giong-web@1.4.2` (with the crate and version reported) shows which dependency holds the old one: update that dependency.

[All error codes](README.md)
"#),
    (5092, r#"# GIONG-E5092: this giong command is older than the newest release

Raised by the `giong` command (giong-cli): `giong doctor` and `giong upgrade`. In JSON output: `"code": "GIONG-E5092"`.

## What happened

The `giong` command is older than the newest giong release. The command writes templates, agent files and checks for its own release, so an older one does not know what the newer crates offer.

From `giong upgrade`: the command updated itself, and the one started afterwards still reports the old version (another `giong` comes first on the `PATH`).

## How to fix it

- `giong upgrade --self` updates it the way it was installed.
- If it repeats, `which giong` (Windows: `where giong`) shows which binary runs: remove the old one or put the right folder first on the `PATH`.

[All error codes](README.md)
"#),
    (5093, r#"# GIONG-E5093: the version to move to is not a release giong upgrade can move to

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5093"`.

## What happened

`--to` named something giong upgrade does not move to: not a version, a release of another major version, or one older than the project's. Releases within a major version only add, so moving forward within 1.x keeps the code building; moving back or across a major version is not that kind of change.

## How to fix it

- Give a 1.x release at least as new as the project's: `giong upgrade --to 1.5.0`.
- To go back to an older release, restore `Cargo.toml` and `Cargo.lock` from version control.

[All error codes](README.md)
"#),
    (5094, r#"# GIONG-E5094: the project's rust-version is below what the release needs

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5094"`.

## What happened

A package of the project promises to build on an older Rust (`rust-version` in its `Cargo.toml`) than the release it was moving to needs. giong does not raise a project's promise by itself: the people who use the package may rely on it. Nothing was changed: the files were put back as they were.

## How to fix it

- If the package can drop the older Rust, raise its `rust-version` to the version reported, then `giong upgrade` again.
- Otherwise stay on the release you have.

[All error codes](README.md)
"#),
    (5095, r#"# GIONG-E5095: a giong dependency is pinned to a version

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5095"`.

## What happened

A giong requirement names an exact version or a range (`=1.4.2`, `>=1.4, <1.5`) that does not allow the release to move to. A pin is a decision, so giong upgrade does not undo it. Nothing was changed.

## How to fix it

- Change the requirement to the release (`giong = "1.5.0"`) or remove the pin, then `giong upgrade` again.
- To keep the pin, move the rest with `giong upgrade --to` a release the pin allows.

[All error codes](README.md)
"#),
    (5096, r#"# GIONG-E5096: giong upgrade --self could not replace this command

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5096"`.

## What happened

`giong upgrade --self` (or `giong upgrade`, which updates the command before a project that moves past it) replaces the command the way it was installed: from the install script's folder (`~/.giong/bin` or `$GIONG_INSTALL`) with the release's archive, checked against its SHA-256; from `cargo install` with `cargo install giong-cli --locked --version <v>`. A command anywhere else (a package manager, a copy) is not replaced, since giong cannot know what that would break. Or a step failed: the download, the checksum, unpacking, or writing the folder.

## How to fix it

- Update it the way it was installed: `curl -fsSL https://giong.run/install.sh | GIONG_VERSION=<v> sh` (Windows: `install.ps1` with `$env:GIONG_VERSION`), or `cargo install giong-cli --locked --version <v>`.
- For a project, `giong upgrade --to <this command's version>` moves it without updating the command.
- Behind a mirror of the release downloads, set `GIONG_DOWNLOAD_URL`, as for the install script.

[All error codes](README.md)
"#),
    (5097, r#"# GIONG-E5097: the newest giong release could not be looked up

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5097"`.

## What happened

giong upgrade asks crates.io for the newest giong release through `cargo info giong`, which uses your cargo configuration (proxies, credentials). The question failed: no network, `CARGO_NET_OFFLINE=true`, a cargo older than Rust 1.82 (which has no `cargo info`), or a registry mirror that cargo info cannot read.

## How to fix it

- `giong upgrade --offline` moves the project to this command's own release, with cargo offline (the crates must be in cargo's cache).
- `giong upgrade --to 1.5.0` names the release.
- `GIONG_LATEST=1.5.0` tells `giong doctor` and `giong upgrade` which release is the newest, for a mirror or a company that approves releases.
- `rustup update stable` brings a cargo with `cargo info`.

[All error codes](README.md)
"#),
    (5098, r#"# GIONG-E5098: a giong dependency is written in a form giong upgrade cannot edit

Raised by the `giong` command (giong-cli): `giong upgrade`. In JSON output: `"code": "GIONG-E5098"`.

## What happened

giong upgrade edits only the version of each giong dependency and leaves every other byte of `Cargo.toml` as it is. It reads dependencies written on one line (`giong = "1.4"`, `giong = { version = "1.4", ... }`), as dotted keys (`giong.version = "1.4"`) and as tables of their own (`[dependencies.giong]`). After editing, it asks cargo for every requirement again; one that still does not allow the new release was written in a form it did not read, or the edited file no longer parses. Nothing was changed: the files were put back as they were.

## How to fix it

- Change the version of the dependency reported by hand, or write it in one of the forms above, then `giong upgrade` again.
- `giong upgrade --dry-run` lists every edit it would make.

[All error codes](README.md)
"#),
];
