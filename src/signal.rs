//! Receiving Unix signals as a stream: `SIGHUP` to reload a configuration,
//! `SIGUSR1` to dump state, `SIGCHLD`, and any other signal a program may
//! act on.
//!
//! ```no_run
//! use giong::signal::{self, SignalKind};
//!
//! # giong::block_on(async {
//! let mut hangups = signal::listen(SignalKind::HANGUP)?;
//! loop {
//!     hangups.recv().await;
//!     println!("reloading the configuration");
//! }
//! # #[allow(unreachable_code)]
//! # Ok::<_, std::io::Error>(())
//! # }).unwrap();
//! ```
//!
//! To stop on `SIGINT` or `SIGTERM`,
//! [`Builder::shutdown_on_signals`](crate::Builder::shutdown_on_signals) with
//! [`shutdown_signal`](crate::shutdown_signal) is usually all you need; the
//! two work together, and a listener here for `SIGTERM` does not stop the
//! runtime from shutting down on it.
//!
//! ## How it works, and what that means
//!
//! The first [`listen`] for a signal installs a handler for it, for the rest
//! of the process's life. The handler counts the signal and wakes one
//! process-wide thread (`giong-signal`), which wakes every listener for it.
//! Handlers rather than a `signalfd` are used because signal masks are per
//! thread, and a library cannot make every thread of a process block a
//! signal.
//!
//! * **Signals coalesce.** Several deliveries before a listener next calls
//!   [`Signals::recv`] are seen as one, as the kernel itself merges
//!   ordinary signals that are pending.
//! * **The default action is gone for good.** Once giong handles a signal,
//!   its default action (for `SIGHUP`, ending the process) no longer
//!   happens, even after every listener is dropped: the signal is counted
//!   and ignored.
//! * **Any number of listeners**, on any worker, thread or runtime, for the
//!   same signal: each one sees every delivery. A [`Signals`] is `Send`, and
//!   its future needs no giong worker.

use std::io;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

use crate::error::{raise, Code};
use crate::runtime::lock;

/// A Unix signal, by number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalKind(i32);

impl SignalKind {
    /// `SIGHUP`: the terminal went away, or (by convention, for services)
    /// "reload your configuration".
    pub const HANGUP: Self = Self(libc::SIGHUP);
    /// `SIGINT`: Ctrl+C.
    pub const INTERRUPT: Self = Self(libc::SIGINT);
    /// `SIGQUIT`: Ctrl+\.
    pub const QUIT: Self = Self(libc::SIGQUIT);
    /// `SIGTERM`: please stop (what `kill`, systemd and Kubernetes send).
    pub const TERMINATE: Self = Self(libc::SIGTERM);
    /// `SIGUSR1`: yours to define.
    pub const USER1: Self = Self(libc::SIGUSR1);
    /// `SIGUSR2`: yours to define.
    pub const USER2: Self = Self(libc::SIGUSR2);
    /// `SIGCHLD`: a child process exited or stopped. Listening does not reap
    /// it; [`Child::wait`](crate::process::Child::wait) does.
    pub const CHILD: Self = Self(libc::SIGCHLD);
    /// `SIGWINCH`: the terminal changed size.
    pub const WINDOW_CHANGE: Self = Self(libc::SIGWINCH);
    /// `SIGALRM`: a timer set with `alarm(2)` or `setitimer(2)` expired.
    pub const ALARM: Self = Self(libc::SIGALRM);
    /// `SIGPIPE`: a write to a pipe or socket nobody reads any more. Rust
    /// programs ignore it from the start, and get `BrokenPipe` errors
    /// instead.
    pub const PIPE: Self = Self(libc::SIGPIPE);
    /// `SIGKILL`: ends the process at once; it cannot be listened for, only
    /// sent ([`Child::signal`](crate::process::Child::signal)).
    pub const KILL: Self = Self(libc::SIGKILL);

    /// The signal with this number, such as `libc::SIGRTMIN() + 1`.
    pub const fn from_raw(signal: i32) -> Self {
        Self(signal)
    }

    /// The signal's number.
    pub const fn as_raw(self) -> i32 {
        self.0
    }
}

/// Signal numbers go up to 64 on Linux (`SIGRTMAX`).
const SIGNALS: usize = 65;

/// Deliveries of each signal since the process started.
static COUNTS: [AtomicU64; SIGNALS] = [const { AtomicU64::new(0) }; SIGNALS];
/// Signals giong has installed its handler for.
static INSTALLED: [AtomicBool; SIGNALS] = [const { AtomicBool::new(false) }; SIGNALS];
/// The write end of the pipe that wakes the signal thread; never closed once
/// made, so the handler cannot race with a close.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

/// One listener's way to be woken.
struct Shared {
    signal: i32,
    waker: Mutex<Option<Waker>>,
}

struct Registry {
    /// Serialises installing handlers.
    install: Mutex<()>,
    listeners: Mutex<Vec<Arc<Shared>>>,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        install: Mutex::new(()),
        listeners: Mutex::new(Vec::new()),
    })
}

/// `true` if giong's handler is installed for `signal`: the runtime's own
/// `SIGINT`/`SIGTERM` handling then leaves the disposition alone.
pub(crate) fn handled(signal: i32) -> bool {
    usize::try_from(signal)
        .ok()
        .and_then(|i| INSTALLED.get(i))
        .is_some_and(|b| b.load(Ordering::SeqCst))
}

/// Async-signal-safe: atomics and `write(2)`, with `errno` preserved.
extern "C" fn on_signal(signal: libc::c_int) {
    // SAFETY: `__errno_location` is async-signal-safe and returns this
    // thread's errno, which is restored before returning.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: as above.
    let saved = unsafe { *errno };
    if let Some(count) = usize::try_from(signal).ok().and_then(|i| COUNTS.get(i)) {
        count.fetch_add(1, Ordering::SeqCst);
        let fd = WAKE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = signal as u8;
            // SAFETY: `write` is async-signal-safe; the descriptor is never
            // closed. A full pipe already has a wake-up queued.
            unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
        }
    }
    if signal == libc::SIGINT || signal == libc::SIGTERM {
        // Builder::shutdown_on_signals keeps working while giong's handler
        // is installed for these.
        crate::runtime::on_shutdown_signal(signal);
    }
    // SAFETY: as above.
    unsafe { *errno = saved };
}

/// Starts the signal thread once, and returns whether it runs.
fn start_thread() -> io::Result<()> {
    static STARTED: OnceLock<Result<(), i32>> = OnceLock::new();
    let res = STARTED.get_or_init(|| {
        let mut fds = [0; 2];
        // SAFETY: a valid pointer to two descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO));
        }
        let [read_fd, write_fd] = fds;
        // The handler must never block: a full pipe means a wake-up is
        // queued already.
        // SAFETY: fcntl on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(write_fd, libc::F_GETFL);
            libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let spawned = std::thread::Builder::new()
            .name("giong-signal".into())
            .spawn(move || dispatch(read_fd));
        match spawned {
            Ok(_) => {
                WAKE_FD.store(write_fd, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                // SAFETY: our descriptors, unused.
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                }
                Err(e.raw_os_error().unwrap_or(libc::EAGAIN))
            }
        }
    });
    res.map_err(io::Error::from_raw_os_error)
}

/// The signal thread: wakes the listeners of each signal the handler reports.
fn dispatch(read_fd: i32) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: a valid buffer; the descriptor is ours for good.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let got = &buf[..n as usize];
        let wakers: Vec<Waker> = lock(&registry().listeners)
            .iter()
            .filter(|l| got.contains(&(l.signal as u8)))
            .filter_map(|l| lock(&l.waker).take())
            .collect();
        for waker in wakers {
            waker.wake();
        }
    }
}

fn not_allowed(signal: i32, why: &str) -> io::Error {
    raise(
        Code::SignalNotAllowed,
        format!("signal {signal} cannot be listened for: {why}"),
        "listen for signals meant for programs to act on, such as SignalKind::HANGUP, USER1, \
         USER2, INTERRUPT, TERMINATE or CHILD; Builder::shutdown_on_signals covers stopping on \
         SIGINT and SIGTERM.",
    )
}

/// Installs giong's handler for `signal` if it is not installed yet.
fn install(signal: i32) -> io::Result<()> {
    let _guard = lock(&registry().install);
    if handled(signal) {
        return Ok(());
    }
    start_thread()?;
    // SAFETY: a zeroed `sigaction` filled in with a valid handler and mask;
    // the result is checked.
    unsafe {
        let mut action: libc::sigaction = mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    INSTALLED[signal as usize].store(true, Ordering::SeqCst);
    Ok(())
}

/// Starts listening for `kind`. Every delivery from now on wakes the
/// returned [`Signals`] (several before the next [`recv`](Signals::recv)
/// count as one).
///
/// Fails with [`Code::SignalNotAllowed`] for `SIGKILL` and `SIGSTOP`, which
/// cannot be caught, for `SIGSEGV`, `SIGBUS`, `SIGILL` and `SIGFPE`, which
/// report faults in the program itself, and for numbers that are not
/// signals (or are kept by the C library for its threads).
///
/// From the first call for a signal on, its default action is gone for the
/// rest of the process's life (see the [module docs](self)).
pub fn listen(kind: SignalKind) -> io::Result<Signals> {
    let signal = kind.0;
    match signal {
        libc::SIGKILL | libc::SIGSTOP => {
            return Err(not_allowed(
                signal,
                "the kernel never lets a process catch it",
            ))
        }
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE => {
            return Err(not_allowed(
                signal,
                "it reports a fault in the program itself, which must not run on",
            ))
        }
        _ => {}
    }
    if signal <= 0 || signal > libc::SIGRTMAX() || signal as usize >= SIGNALS {
        return Err(not_allowed(signal, "there is no such signal"));
    }
    if signal >= 32 && signal < libc::SIGRTMIN() {
        return Err(not_allowed(
            signal,
            "the C library keeps it for its own threads",
        ));
    }
    install(signal)?;
    let shared = Arc::new(Shared {
        signal,
        waker: Mutex::new(None),
    });
    lock(&registry().listeners).push(shared.clone());
    Ok(Signals {
        shared,
        seen: COUNTS[signal as usize].load(Ordering::SeqCst),
    })
}

/// Deliveries of one signal, from [`listen`]. Dropping it stops listening
/// (the signal stays handled, and ignored; see the [module docs](self)).
pub struct Signals {
    shared: Arc<Shared>,
    seen: u64,
}

impl Signals {
    /// Waits for the next delivery of the signal since the last call (or
    /// since [`listen`]). Several deliveries in between are seen as one.
    ///
    /// Cancel safe: a delivery that arrives while no `recv` waits is seen by
    /// the next one.
    pub async fn recv(&mut self) {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// The poll form of [`recv`](Self::recv).
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let count = &COUNTS[self.shared.signal as usize];
        let now = count.load(Ordering::SeqCst);
        if now != self.seen {
            self.seen = now;
            return Poll::Ready(());
        }
        // Store the waker, then look again: a delivery between the two is
        // either seen here or wakes the stored waker.
        *lock(&self.shared.waker) = Some(cx.waker().clone());
        let now = count.load(Ordering::SeqCst);
        if now != self.seen {
            self.seen = now;
            return Poll::Ready(());
        }
        Poll::Pending
    }

    /// The signal this listens for.
    pub fn kind(&self) -> SignalKind {
        SignalKind(self.shared.signal)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        lock(&registry().listeners).retain(|l| !Arc::ptr_eq(l, &self.shared));
    }
}

impl std::fmt::Debug for Signals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signals")
            .field("signal", &self.shared.signal)
            .finish()
    }
}
