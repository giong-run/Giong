//! Child processes, pipes, and the process's own standard input and output.
//!
//! [`Command`] starts a program the way `std::process::Command` does, and
//! hands back a [`Child`] whose pipes ([`PipeReader`], [`PipeWriter`]) and
//! exit ([`Child::wait`]) are awaited on the worker, without a thread:
//!
//! ```no_run
//! use std::process::Stdio;
//! use giong::process::Command;
//!
//! # giong::block_on(async {
//! // Run a tool and collect what it prints.
//! let output = Command::new("git").args(["rev-parse", "HEAD"]).output().await?;
//! assert!(output.status.success());
//!
//! // Talk to a long-running child over its stdin and stdout.
//! let mut child = Command::new("cat")
//!     .stdin(Stdio::piped())
//!     .stdout(Stdio::piped())
//!     .spawn()?;
//! let stdin = child.stdin.take().unwrap();
//! let stdout = child.stdout.take().unwrap();
//! stdin.write_all(&b"hello\n"[..]).await.0?;
//! drop(stdin); // end of input: cat exits after echoing it
//! let mut echoed = Vec::new();
//! stdout.read_to_end(&mut echoed).await?;
//! let status = child.wait().await?;
//! # drop(status);
//! # Ok::<_, std::io::Error>(())
//! # }).unwrap();
//! ```
//!
//! [`stdin`], [`stdout`] and [`stderr`] read and write this process's own
//! standard streams on the worker, as a server speaking a protocol over
//! stdio (such as a local MCP server) needs.
//!
//! ## How it works
//!
//! A child is started by the standard library (`posix_spawn` or
//! `fork`/`exec`), then watched through a pidfd: a descriptor the kernel
//! makes readable when the child exits. Waiting is a poll on it through the
//! worker's io_uring (or epoll), followed by a `waitid` that cannot block.
//! No `SIGCHLD` handler is installed and no thread waits, and
//! [`Child::signal`] cannot reach an unrelated process that reused the
//! child's process ID. This needs Linux 5.3 or newer (giong runs on 5.15 and
//! newer anyway).
//!
//! A child dropped before it was waited for is reaped by a small task on
//! the worker, so no zombie is left behind while the runtime runs; with
//! [`Command::kill_on_drop`] it is killed first.

mod pipe;
mod sys;

pub use pipe::{pipe, stderr, stdin, stdout, PipeReader, PipeWriter};
pub use std::process::{ExitStatus, Output, Stdio};

use std::ffi::OsStr;
use std::future::{poll_fn, Future};
use std::io;
use std::os::fd::{IntoRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::pin::pin;
use std::task::Poll;

use crate::driver::Io;
use crate::error::{raise, Code};
use crate::net::os_err;
use crate::op::Op;
use crate::runtime;
use crate::signal::SignalKind;

/// A program to start, with its arguments, environment and standard
/// streams: the builder of `std::process::Command`, whose methods it
/// mirrors, with [`spawn`](Self::spawn), [`status`](Self::status) and
/// [`output`](Self::output) that return a giong [`Child`] or are awaited.
///
/// For a setting not mirrored here (`pre_exec`, `uid`, `process_group`,
/// ...), use [`as_std_mut`](Self::as_std_mut).
pub struct Command {
    std: std::process::Command,
    /// The `pre_exec` that gives the child the open-files limit from before
    /// the runtime raised it is added (once, by the first `spawn`).
    nofile_restore: bool,
    kill_on_drop: bool,
    stdin_set: bool,
    stdout_set: bool,
    stderr_set: bool,
}

impl Command {
    /// A command to run `program`, found through `PATH` if it is not a
    /// path. By default the child inherits this process's environment,
    /// working directory and standard streams.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self::from(std::process::Command::new(program))
    }

    /// Adds an argument.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.std.arg(arg);
        self
    }

    /// Adds several arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.std.args(args);
        self
    }

    /// Sets an environment variable for the child.
    pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, key: K, val: V) -> &mut Self {
        self.std.env(key, val);
        self
    }

    /// Sets several environment variables for the child.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.std.envs(vars);
        self
    }

    /// Removes an environment variable from the child's environment.
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.std.env_remove(key);
        self
    }

    /// Starts the child with an empty environment (plus what
    /// [`env`](Self::env) adds).
    pub fn env_clear(&mut self) -> &mut Self {
        self.std.env_clear();
        self
    }

    /// Sets the child's working directory.
    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.std.current_dir(dir);
        self
    }

    /// The child's standard input: `Stdio::piped()` for a
    /// [`Child::stdin`] to write to, `Stdio::null()`, `Stdio::inherit()`,
    /// or a descriptor (`Stdio::from(reader)`, with a [`PipeReader`] or a
    /// file).
    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.std.stdin(cfg);
        self.stdin_set = true;
        self
    }

    /// The child's standard output: `Stdio::piped()` for a
    /// [`Child::stdout`] to read from, or as for [`stdin`](Self::stdin).
    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.std.stdout(cfg);
        self.stdout_set = true;
        self
    }

    /// The child's standard error: `Stdio::piped()` for a
    /// [`Child::stderr`] to read from, or as for [`stdin`](Self::stdin).
    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.std.stderr(cfg);
        self.stderr_set = true;
        self
    }

    /// Kill the child (`SIGKILL`) if its [`Child`] is dropped before the
    /// child has been waited for. Off by default: a dropped child runs on,
    /// as with the standard library, and is reaped when it exits.
    pub fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    /// The standard library's command inside.
    pub fn as_std(&self) -> &std::process::Command {
        &self.std
    }

    /// The standard library's command inside, for the settings not mirrored
    /// here (`std::os::unix::process::CommandExt`: `uid`, `gid`,
    /// `process_group`, `pre_exec`, `arg0`, ...). Set the standard streams
    /// through this type's own methods, so that [`output`](Self::output)
    /// knows about them.
    pub fn as_std_mut(&mut self) -> &mut std::process::Command {
        &mut self.std
    }

    /// Starts the child. Its piped standard streams are in the returned
    /// [`Child`]'s `stdin`, `stdout` and `stderr`.
    ///
    /// Must be called on a giong worker, which the child's pipes and exit
    /// are then tied to. Fails as `std::process::Command::spawn` does (no
    /// such program, no permission), or with
    /// [`Code::ChildProcess`] if the
    /// kernel gives no pidfd for the child (Linux older than 5.3, or a
    /// seccomp profile that blocks `pidfd_open`), in which case the child
    /// is killed and reaped.
    ///
    /// When a runtime raised this process's soft limit on open files
    /// ([`Builder::raise_open_files_limit`](crate::Builder::raise_open_files_limit)),
    /// the child gets the soft limit from before, unless a `pre_exec` of
    /// yours sets one of its own.
    pub fn spawn(&mut self) -> io::Result<Child> {
        // Panics here, with GIONG-E0011, rather than after the child runs.
        runtime::with_worker(|_| ());
        if !self.nofile_restore {
            if let Some((before, raised)) = runtime::open_files_raised() {
                restore_open_files_limit(&mut self.std, before, raised);
                self.nofile_restore = true;
            }
        }
        let mut child = self.std.spawn()?;
        let pid = child.id();
        let pidfd = match sys::pidfd_open(pid) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(raise(
                    Code::ChildProcess,
                    format!("the kernel gave no pidfd for the child process ({e})"),
                    "run on Linux 5.3 or newer, and allow pidfd_open and pidfd_send_signal in \
                     the seccomp profile (Docker's default profile allows both since 20.10).",
                ));
            }
        };
        let stdin = child
            .stdin
            .take()
            .map(|s| PipeWriter::from_owned_fd(OwnedFd::from(s)));
        let stdout = child
            .stdout
            .take()
            .map(|s| PipeReader::from_owned_fd(OwnedFd::from(s)));
        let stderr = child
            .stderr
            .take()
            .map(|s| PipeReader::from_owned_fd(OwnedFd::from(s)));
        // The standard library's `Child` neither waits nor kills on drop.
        drop(child);
        Ok(Child {
            pid,
            pidfd: Some(pidfd.into_raw_fd()),
            status: None,
            kill_on_drop: self.kill_on_drop,
            stdin,
            stdout,
            stderr,
        })
    }

    /// Starts the child and waits for it to exit. Its standard streams are
    /// inherited unless set.
    ///
    /// Cancel safe in that nothing leaks: if the future is dropped, the
    /// child runs on (or is killed, with
    /// [`kill_on_drop`](Self::kill_on_drop)) and is reaped when it exits.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        self.spawn()?.wait().await
    }

    /// Starts the child, collects all it writes to stdout and stderr, and
    /// waits for it to exit. Unless set, stdout and stderr are piped and
    /// stdin is null, as with the standard library (the command keeps these
    /// settings afterwards).
    ///
    /// The output is collected in full, without a limit: use it for tools
    /// whose output size you trust, or [`spawn`](Self::spawn) and read in a
    /// loop.
    ///
    /// Cancel safety as for [`status`](Self::status).
    pub async fn output(&mut self) -> io::Result<Output> {
        if !self.stdin_set {
            self.stdin(Stdio::null());
        }
        if !self.stdout_set {
            self.stdout(Stdio::piped());
        }
        if !self.stderr_set {
            self.stderr(Stdio::piped());
        }
        self.spawn()?.wait_with_output().await
    }
}

/// Gives a child the soft open-files limit (`RLIMIT_NOFILE`) the process
/// had before the runtime raised it (`Builder::raise_open_files_limit`): a
/// child that uses `select(2)` must not get descriptors of 1,024 and more.
/// Only when the child still has the raised limit, so that a `pre_exec` of
/// the caller's that set a limit of its own, before or after this one, keeps
/// it. A `pre_exec` makes the standard library start the child with `fork`
/// and `exec` instead of `posix_spawn`, so this is added only in a process
/// whose limit was raised.
fn restore_open_files_limit(
    std: &mut std::process::Command,
    before: libc::rlim_t,
    raised: libc::rlim_t,
) {
    use std::os::unix::process::CommandExt;
    let restore = move || {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` and `setrlimit` are async-signal-safe system
        // calls, allowed between `fork` and `exec`; each is given a struct
        // of its type. Nothing is allocated here.
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur == raised {
                lim.rlim_cur = before;
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            }
        }
        Ok(())
    };
    // SAFETY: the closure makes only async-signal-safe system calls (see
    // above) and touches no lock or allocator.
    unsafe {
        std.pre_exec(restore);
    }
}

impl From<std::process::Command> for Command {
    fn from(std: std::process::Command) -> Self {
        Self {
            std,
            nofile_restore: false,
            kill_on_drop: false,
            stdin_set: false,
            stdout_set: false,
            stderr_set: false,
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.std.fmt(f)
    }
}

/// A running (or exited) child process, from [`Command::spawn`].
///
/// Wait for it with [`wait`](Self::wait). Dropped before that, the child
/// runs on and is reaped when it exits (killed first, with
/// [`Command::kill_on_drop`]).
pub struct Child {
    pid: u32,
    /// Closed once the child has been reaped.
    pidfd: Option<RawFd>,
    status: Option<ExitStatus>,
    kill_on_drop: bool,
    /// The child's standard input, if it was `Stdio::piped()`. Take it to
    /// write to it; dropping it closes it, which the child reads as end of
    /// input.
    pub stdin: Option<PipeWriter>,
    /// The child's standard output, if it was `Stdio::piped()`.
    pub stdout: Option<PipeReader>,
    /// The child's standard error, if it was `Stdio::piped()`.
    pub stderr: Option<PipeReader>,
}

impl Child {
    /// The child's process ID. Prefer [`signal`](Self::signal) to sending
    /// signals by ID, which can reach another process once the child has
    /// been reaped and its ID reused.
    pub fn id(&self) -> u32 {
        self.pid
    }

    fn reaped(&mut self, status: ExitStatus) -> ExitStatus {
        self.status = Some(status);
        if let Some(fd) = self.pidfd.take() {
            close(fd);
        }
        status
    }

    fn lost() -> io::Error {
        raise(
            Code::ChildProcess,
            "the child process was reaped by someone else, so its exit status is gone",
            "do not reap children giong started: no waitpid(-1, ...), no wait(), and no SIGCHLD \
             set to SIG_IGN, in your code or a library it calls.",
        )
    }

    /// The exit status if the child has exited, without waiting. Once it
    /// has, the status is kept and returned by every later call.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let fd = self.pidfd.expect("a child not reaped has its pidfd");
        match sys::try_reap(fd) {
            Ok(Some(status)) => Ok(Some(self.reaped(status))),
            Ok(None) => Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => Err(Self::lost()),
            Err(e) => Err(e),
        }
    }

    /// Waits for the child to exit and returns its status. Closes the
    /// child's stdin first (as the standard library does), so that a child
    /// reading its input to the end can finish. Calling it again returns the
    /// same status.
    ///
    /// Read the child's piped stdout and stderr while waiting (or use
    /// [`wait_with_output`](Self::wait_with_output)): a child that fills a
    /// pipe nobody reads blocks, and never exits.
    ///
    /// Cancel safe: if the future is dropped, nothing is lost, and the next
    /// call waits on.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            let fd = self.pidfd.expect("a child not reaped has its pidfd");
            readable(fd).await?;
        }
    }

    /// Waits for the child to exit while collecting all it writes to its
    /// piped stdout and stderr (closing its stdin first). A stream that was
    /// not piped comes back empty. Unbounded, as [`Command::output`].
    ///
    /// Cancel safe in that nothing leaks: if the future is dropped, the
    /// output collected so far is lost, and the child runs on (or is killed,
    /// with [`Command::kill_on_drop`]) and is reaped when it exits.
    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (status, a, b) = {
            let read_out = pin!(read_all(stdout.as_ref(), &mut out));
            let read_err = pin!(read_all(stderr.as_ref(), &mut err));
            let wait = pin!(self.wait());
            join3(wait, read_out, read_err).await
        };
        a?;
        b?;
        Ok(Output {
            status: status?,
            stdout: out,
            stderr: err,
        })
    }

    /// Sends `signal` to the child, such as `SignalKind::TERMINATE` to ask
    /// it to stop. It goes through the pidfd, so it can never reach another
    /// process that reused the child's ID. A child that has already exited
    /// is left as it is, and the call returns `Ok`.
    pub fn signal(&self, signal: SignalKind) -> io::Result<()> {
        let Some(fd) = self.pidfd else {
            return Ok(());
        };
        match sys::pidfd_send_signal(fd, signal.as_raw()) {
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            res => res,
        }
    }

    /// Kills the child (`SIGKILL`) without waiting for it: follow with
    /// [`wait`](Self::wait) to reap it. A child that has already exited is
    /// left as it is.
    pub fn kill(&mut self) -> io::Result<()> {
        self.signal(SignalKind::KILL)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let Some(fd) = self.pidfd.take() else { return };
        if self.kill_on_drop {
            let _ = sys::pidfd_send_signal(fd, libc::SIGKILL);
        }
        match sys::try_reap(fd) {
            Ok(None) => {}
            // Exited (now reaped), or reaped by someone else.
            _ => return close(fd),
        }
        // Still running: reap it when it exits, on this worker.
        let spawned = runtime::try_with_worker(|_| {
            crate::spawn(async move {
                // Until it has been reaped (or something fails).
                while let Ok(None) = sys::try_reap(fd) {
                    if readable(fd).await.is_err() {
                        break;
                    }
                }
                close(fd);
            });
        });
        if spawned.is_none() {
            // No worker: the child stays a zombie until this process exits.
            // SAFETY: we own `fd` and close it once.
            unsafe { libc::close(fd) };
        }
    }
}

impl std::fmt::Debug for Child {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Child")
            .field("pid", &self.pid)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Waits until `fd` is readable: for a pidfd, until the process exits.
async fn readable(fd: RawFd) -> io::Result<()> {
    // SAFETY: the operation points at no memory.
    let (res, _, ()) = unsafe { Op::new(Io::Readable { fd }, ()) }.await;
    if res < 0 && res != -libc::EINTR {
        return Err(os_err(res));
    }
    Ok(())
}

/// Closes `fd` through the worker's driver if there is one.
fn close(fd: RawFd) {
    let closed = runtime::try_with_worker(|w| match w.driver.try_borrow_mut() {
        Ok(mut driver) => driver.close_fd(fd),
        Err(_) => w.defer_close(fd),
    })
    .is_some();
    if !closed {
        // SAFETY: the caller owns `fd` and closes it once.
        unsafe { libc::close(fd) };
    }
}

async fn read_all(reader: Option<&PipeReader>, out: &mut Vec<u8>) -> io::Result<()> {
    if let Some(reader) = reader {
        reader.read_to_end(out).await?;
    }
    Ok(())
}

/// Runs three futures at once and returns all three results.
async fn join3<A: Future, B: Future, C: Future>(
    a: std::pin::Pin<&mut A>,
    b: std::pin::Pin<&mut B>,
    c: std::pin::Pin<&mut C>,
) -> (A::Output, B::Output, C::Output) {
    let (mut a, mut b, mut c) = (a, b, c);
    let (mut ra, mut rb, mut rc) = (None, None, None);
    poll_fn(|cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if rc.is_none() {
            if let Poll::Ready(v) = c.as_mut().poll(cx) {
                rc = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() && rc.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (ra.unwrap(), rb.unwrap(), rc.unwrap())
}
