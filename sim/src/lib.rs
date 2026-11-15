//! Deterministic simulation for Gióng's tests: a simulated kernel under the
//! runtime, driven by a seed, with injected faults.
//!
//! Not published, and not part of Gióng's API. A test binary that links
//! this crate takes over the libc functions the runtime calls (sockets,
//! epoll, eventfd, the clock, sleeps, futexes, thread creation and the file
//! calls of the epoll driver's blocking threads). Inside [`Sim::run`]:
//!
//! * the network is in memory: TCP, UDP and Unix streams between sockets of
//!   the process, with latency, segment splitting, loss (as retransmission
//!   delay), resets, refused or lost connects, datagram loss, duplication
//!   and reordering, and partitions between addresses;
//! * time is virtual: it moves only by a small cost per system call, and
//!   jumps to the next timer when every thread waits, so an hour-long
//!   timeout takes no real time;
//! * threads run one at a time, handed over at system calls in an order
//!   chosen by the seed (with optional preemption), so a multi-worker
//!   runtime is deterministic too;
//! * random bytes (`getrandom`) come from the seed;
//! * files are real, but Gióng's file calls can be slowed down or fail.
//!
//! The same seed gives the same run, step for step; every step is recorded
//! in a trace whose hash [`Report::hash`] compares runs. A failing run
//! prints its seed; `GIONG_SIM_SEED=<seed>` replays exactly that run.
//!
//! The runtime uses its epoll driver under the simulation (io_uring is
//! reported as missing), so the io_uring driver is not covered.

mod files;
mod interpose;
mod net;
mod raw;
mod world;

use std::net::IpAddr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub use world::FaultCounts;

/// Which faults to inject, with what probability. Probabilities are per
/// opportunity: per segment, per call, per connection attempt.
#[derive(Clone, Debug)]
pub struct Faults {
    /// One-way network latency, nanoseconds (uniform in the range).
    pub latency_ns: (u64, u64),
    /// Cut stream writes into segments of random size, delivered apart.
    pub split_segments: bool,
    /// A stream segment is lost and retransmitted 200 ms later.
    pub tcp_loss: f64,
    /// A stream write resets the connection (both sides see ECONNRESET).
    pub tcp_reset: f64,
    /// A connection attempt is refused although a listener exists.
    pub connect_refuse: f64,
    /// A SYN is lost (retried after 1, 2, 4 ... s, as Linux does).
    pub connect_drop: f64,
    /// `accept` finds the connection already reset (ECONNABORTED).
    pub accept_abort: f64,
    pub udp_loss: f64,
    pub udp_dup: f64,
    /// A datagram is delayed by 1-50 ms more than the others.
    pub udp_reorder: f64,
    /// A read or write moves fewer bytes than it could.
    pub short_io: f64,
    /// A call fails with EINTR.
    pub eintr: f64,
    /// A non-blocking read or accept says EAGAIN although data is there.
    pub spurious_eagain: f64,
    /// Another thread gets a turn at a system call.
    pub preempt: f64,
    /// The clock jumps 1-30 s forward at a system call.
    pub clock_jump: f64,
    /// Virtual time each system call costs.
    pub syscall_ns: u64,
    /// File faults apply to files opened under this path.
    pub disk_prefix: Option<String>,
    pub disk_latency_ns: (u64, u64),
    /// A file operation takes 100 ms to 2 s more.
    pub disk_slow: f64,
    pub disk_error: f64,
    pub disk_full: f64,
}

impl Faults {
    /// No faults: a perfect network with 50 µs latency.
    pub fn none() -> Self {
        Faults {
            latency_ns: (50_000, 50_000),
            split_segments: false,
            tcp_loss: 0.0,
            tcp_reset: 0.0,
            connect_refuse: 0.0,
            connect_drop: 0.0,
            accept_abort: 0.0,
            udp_loss: 0.0,
            udp_dup: 0.0,
            udp_reorder: 0.0,
            short_io: 0.0,
            eintr: 0.0,
            spurious_eagain: 0.0,
            preempt: 0.0,
            clock_jump: 0.0,
            syscall_ns: 1_000,
            disk_prefix: None,
            disk_latency_ns: (0, 0),
            disk_slow: 0.0,
            disk_error: 0.0,
            disk_full: 0.0,
        }
    }

    /// What the loopback interface of a real Linux machine can show:
    /// jittery delivery, spurious EAGAIN on non-blocking calls, spurious
    /// wakeups of `epoll_wait` and futex waits, preemption. A write still
    /// arrives in one piece and datagrams are neither lost, duplicated nor
    /// reordered, so tests written for loopback must pass under it.
    pub fn loopback() -> Self {
        Faults {
            latency_ns: (5_000, 2_000_000),
            eintr: 0.02,
            spurious_eagain: 0.02,
            preempt: 0.05,
            ..Faults::none()
        }
    }

    /// Faults a real network shows to a correct program without any
    /// operation failing: delays, split and lost segments (retransmitted),
    /// lost SYNs, duplicated and reordered datagrams, short reads and
    /// writes, plus everything in [`Faults::loopback`].
    pub fn benign() -> Self {
        Faults {
            latency_ns: (10_000, 5_000_000),
            split_segments: true,
            tcp_loss: 0.02,
            connect_drop: 0.02,
            udp_dup: 0.02,
            udp_reorder: 0.05,
            short_io: 0.1,
            eintr: 0.02,
            spurious_eagain: 0.02,
            preempt: 0.05,
            ..Faults::none()
        }
    }

    /// `benign` plus faults that break operations: resets, refused
    /// connects, aborted accepts, datagram loss, clock jumps. A correct
    /// program reports them as errors, never hangs, panics or leaks.
    pub fn hostile() -> Self {
        Faults {
            tcp_reset: 0.01,
            connect_refuse: 0.02,
            accept_abort: 0.02,
            udp_loss: 0.05,
            clock_jump: 0.0005,
            ..Faults::benign()
        }
    }

    /// Disk faults for files under `prefix`.
    pub fn disk(mut self, prefix: impl Into<String>, error: f64, full: f64, slow: f64) -> Self {
        self.disk_prefix = Some(prefix.into());
        self.disk_error = error;
        self.disk_full = full;
        self.disk_slow = slow;
        self.disk_latency_ns = (20_000, 200_000);
        self
    }
}

/// The outcome of a simulated run.
#[derive(Clone, Debug)]
pub struct Report {
    pub seed: u64,
    /// Hash of the whole trace: equal for equal runs.
    pub hash: u64,
    pub steps: u64,
    pub virtual_time: Duration,
    /// How many faults of each kind were injected.
    pub injected: FaultCounts,
    /// The full trace, if asked for with [`Sim::full_trace`].
    pub trace: Option<Vec<String>>,
    /// Descriptors the run opened and never closed: simulated sockets,
    /// epoll sets and eventfds (`o<id> <kind>`) and files under the disk
    /// fault prefix (`f<id>`). Empty when nothing leaked.
    pub open: Vec<String>,
    step_hashes: Vec<u64>,
}

impl Report {
    /// The first step where two runs differ, if any.
    pub fn first_difference(&self, other: &Report) -> Option<u64> {
        let n = self.step_hashes.len().min(other.step_hashes.len());
        (0..n)
            .find(|i| self.step_hashes[*i] != other.step_hashes[*i])
            .or_else(|| (self.step_hashes.len() != other.step_hashes.len()).then_some(n))
            .map(|i| i as u64)
    }
}

/// One simulated run's settings.
#[derive(Clone, Debug)]
pub struct Sim {
    seed: u64,
    faults: Faults,
    limit: Duration,
    full_trace: bool,
    partitions: Vec<(IpAddr, IpAddr)>,
}

static RUN_LOCK: Mutex<()> = Mutex::new(());
static GEN: AtomicU64 = AtomicU64::new(1);

/// The seed to replay, from `GIONG_SIM_SEED`.
pub fn replay_seed() -> Option<u64> {
    std::env::var("GIONG_SIM_SEED")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

impl Sim {
    pub fn new(seed: u64) -> Self {
        Sim {
            seed,
            faults: Faults::none(),
            limit: Duration::from_secs(24 * 3600),
            full_trace: false,
            partitions: Vec::new(),
        }
    }

    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Stops the run (as a failure) once virtual time passes `limit`.
    pub fn time_limit(mut self, limit: Duration) -> Self {
        self.limit = limit;
        self
    }

    /// Keeps every step of the trace in the report.
    pub fn full_trace(mut self, on: bool) -> Self {
        self.full_trace = on;
        self
    }

    /// Cuts `a` and `b` off from each other for the whole run.
    pub fn partition(mut self, a: IpAddr, b: IpAddr) -> Self {
        self.partitions.push((a, b));
        self
    }

    /// Runs `f` on the calling thread inside the simulation, with every
    /// thread it starts. Returns once `f` has returned and every thread
    /// it started has ended (virtual time runs on until idle threads such
    /// as the blocking pool's time out). A panic in `f` is passed on after
    /// the seed is printed.
    pub fn run<R: Send>(self, f: impl FnOnce() -> R + Send) -> (R, Report) {
        // A fresh thread for every run, so that no thread-local state (the
        // standard library's hash keys, for one) carries over between runs.
        std::thread::scope(|s| {
            s.spawn(move || self.run_here(f))
                .join()
                .unwrap_or_else(|p| panic::resume_unwind(p))
        })
    }

    fn run_here<R>(self, f: impl FnOnce() -> R) -> (R, Report) {
        let _serial = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let gen = GEN.fetch_add(1, Ordering::Relaxed);
        let limit_ns = self.limit.as_nanos().min(u64::MAX as u128) as u64;
        // GIONG_SIM_TAIL=<n>: how many last steps a failure report shows.
        let keep = std::env::var("GIONG_SIM_TAIL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40);
        let mut w = world::World::new(gen, self.seed, self.faults, keep, self.full_trace, limit_ns);
        w.net.partitions = self.partitions;
        // The epoll driver, without the one-line notice of an automatic
        // fallback.
        std::env::set_var("GIONG_DRIVER", "epoll");
        world::install(w);
        world::set_me(gen, Some(0));
        let out = panic::catch_unwind(AssertUnwindSafe(f));
        // Wait for every other thread, letting virtual time run.
        if let Some((g, me)) = world::enter() {
            let (g, _) = world::block(g, me, world::Why::AllExited, None);
            drop(g);
        }
        world::set_me(0, None);
        let w = world::uninstall().expect("giong-sim: world vanished");
        let w_tail = w.tail.clone();
        let open = w.still_open();
        let report = Report {
            seed: w.seed,
            hash: w.hash,
            steps: w.steps,
            virtual_time: Duration::from_nanos(w.now - world::T0),
            injected: w.injected,
            open,
            trace: w.full,
            step_hashes: w.step_hashes,
        };
        match out {
            Ok(r) => (r, report),
            Err(p) => {
                let tail: Vec<String> = w_tail.into_iter().collect();
                raw::eprint(&format!(
                    "\ngiong-sim: the run panicked with seed {} (replay: GIONG_SIM_SEED={}) after {} steps, at virtual time {:?}; last steps:\n    {}\n",
                    report.seed,
                    report.seed,
                    report.steps,
                    report.virtual_time,
                    tail.join("\n    ")
                ));
                panic::resume_unwind(p)
            }
        }
    }
}

/// Runs `f` under each seed in `seeds` (or only `GIONG_SIM_SEED` if set)
/// with `faults`, and returns the reports. A run that leaves a descriptor
/// open fails.
///
/// The first sweep in a process starts with one run whose report is thrown
/// away. Process-wide state that is set up on first use (a `OnceLock`
/// reading the clock, say) then makes its calls there, so that every
/// reported run, and a replay of its seed in a fresh process, starts from
/// the same state.
pub fn sweep(seeds: std::ops::Range<u64>, faults: Faults, f: impl Fn(u64) + Sync) -> Vec<Report> {
    let seeds: Vec<u64> = match replay_seed() {
        Some(s) => vec![s],
        None => seeds.collect(),
    };
    if let Some(&first) = seeds.first() {
        warm_up(first, &faults, || f(first));
    }
    seeds
        .into_iter()
        .map(|s| {
            let r = Sim::new(s).faults(faults.clone()).run(|| f(s)).1;
            assert!(
                r.open.is_empty(),
                "giong-sim: seed {s} left descriptors open (replay: GIONG_SIM_SEED={s}): {:?}",
                r.open
            );
            r
        })
        .collect()
}

/// The first time in a process, one run of `f` whose report is thrown away
/// (see [`sweep`]).
fn warm_up(seed: u64, faults: &Faults, f: impl FnOnce() + Send) {
    static WARM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARM.swap(true, std::sync::atomic::Ordering::SeqCst) {
        Sim::new(seed).faults(faults.clone()).run(f);
    }
}

/// Runs `f` twice with the same seed and panics, with the first differing
/// steps, if the runs differ.
pub fn assert_deterministic(seed: u64, faults: Faults, f: impl Fn() + Sync) -> Report {
    warm_up(seed, &faults, &f);
    let (_, a) = Sim::new(seed)
        .faults(faults.clone())
        .full_trace(true)
        .run(&f);
    let (_, b) = Sim::new(seed).faults(faults).full_trace(true).run(&f);
    if let Some(i) = a.first_difference(&b) {
        let (ta, tb) = (a.trace.as_ref().unwrap(), b.trace.as_ref().unwrap());
        let i = i as usize;
        let lo = i.saturating_sub(5);
        let show = |t: &Vec<String>| t[lo.min(t.len())..(i + 3).min(t.len())].join("\n    ");
        panic!(
            "giong-sim: seed {seed} is not deterministic: runs differ at step {i}\n  first run:\n    {}\n  second run:\n    {}",
            show(ta),
            show(tb)
        );
    }
    a
}

/// Runs one test of Gióng's own suite in the simulation (used by
/// `tests/suite.rs`): seed from `GIONG_SIM_SEED` (default 1), faults from
/// `GIONG_SIM_FAULTS` (`none`, `loopback`, `benign`, `hostile`; default
/// `none`).
///
/// Each test runs in a child process (the test binary started again for
/// this one test), because a run that deadlocks aborts its process.
pub fn run_test(name: &str, should_panic: bool, f: fn()) {
    let seed = replay_seed().unwrap_or(1);
    if std::env::var("GIONG_SIM_CHILD").is_err() {
        let (module, func) = name.split_once("::").unwrap();
        let path = format!("sim_{module}::{func}");
        // Output goes to files: a pipe that fills up would stall the child.
        let dir = std::env::temp_dir().join(format!("giong-sim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (out_path, err_path) = (
            dir.join(format!("{path}.out")),
            dir.join(format!("{path}.err")),
        );
        let out_file = std::fs::File::create(&out_path).unwrap();
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                path.as_str(),
                "--exact",
                "--nocapture",
                "--test-threads=1",
                "--include-ignored",
            ])
            .env("GIONG_SIM_CHILD", "1")
            .stdout(out_file.try_clone().unwrap())
            .stderr(err_file.try_clone().unwrap())
            .spawn()
            .expect("giong-sim: cannot start the test binary again");
        // A run blocked in a real system call (a FIFO with no writer, say)
        // never ends: give it two minutes of real time.
        let start = std::time::Instant::now();
        while child.try_wait().ok().flatten().is_none() {
            if start.elapsed() > Duration::from_secs(120) {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let status = child.wait().unwrap();
        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        let _ = (
            std::fs::remove_file(&out_path),
            std::fs::remove_file(&err_path),
        );
        let tail = |s: &str| {
            s.lines()
                .rev()
                .take(60)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        };
        let passed = status.success() && stdout.contains("1 passed");
        if should_panic {
            // The child's copy is #[should_panic] too: it passed if the
            // test panicked there. Panic here to say so; otherwise return,
            // and the missing panic fails the test.
            if passed {
                // With the child's output, which holds the original panic
                // message that `expected = "..."` looks for.
                panic!("{name} panicked in the simulation, as expected:\n{stdout}\n{stderr}");
            }
            eprintln!(
                "{name} did not panic in the simulation (seed {seed}): {status}\n--- stderr\n{}",
                tail(&stderr)
            );
            return;
        }
        if !passed {
            let how = if start.elapsed() > Duration::from_secs(120) {
                "timed out (real time)".to_string()
            } else {
                status.to_string()
            };
            panic!(
                "{name} failed in the simulation (seed {seed}): {how}\n--- stdout\n{}\n--- stderr\n{}",
                tail(&stdout),
                tail(&stderr)
            );
        }
        return;
    }
    let faults = match std::env::var("GIONG_SIM_FAULTS").as_deref() {
        Ok("loopback") => Faults::loopback(),
        Ok("benign") => Faults::benign(),
        Ok("hostile") => Faults::hostile(),
        _ => Faults::none(),
    };
    let ((), r) = Sim::new(seed).faults(faults).run(f);
    if std::env::var("GIONG_SIM_VERBOSE").is_ok() {
        eprintln!(
            "{name}: seed {seed}, {} steps, {:?} virtual, hash {:x}",
            r.steps, r.virtual_time, r.hash
        );
    }
}

/// Inside a run: how many descriptors the run has open now (simulated
/// sockets, epoll sets, eventfds, and files under the disk fault prefix).
/// Outside a run: 0. For checks that a long run does not leak.
pub fn open_descriptors() -> usize {
    let g = world::Guard::lock();
    g.as_ref().map_or(0, |w| w.still_open().len())
}
