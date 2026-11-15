//! The simulated world: virtual time, the seeded random source, the trace,
//! the timed events, and the scheduler that runs one thread at a time.
//!
//! Every simulated thread waits on its own futex word (`go`) until the
//! scheduler hands it the baton. A thread gives the baton up only inside an
//! interposed call: when it blocks (futex, epoll, sleep, join, a blocking
//! socket), when it exits, or when the seed says to preempt it. Because
//! every hand-over is decided while the baton is held, from the seed and
//! the world's state only, a run is a pure function of its seed.

use std::cell::{Cell, UnsafeCell};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::files::Files;
use crate::net::{Event, Kind, Net, ObjId};
use crate::raw::{self, SpinLock};
use crate::Faults;

pub(crate) type Tid = usize;

/// Steps a thread may run before another runnable thread gets a turn.
const SLICE: u64 = 100;

/// Virtual time starts here (nanoseconds), so that `Instant` arithmetic
/// that subtracts a few seconds never goes below zero.
pub(crate) const T0: u64 = 1_000_000 * 1_000_000_000;
/// `SystemTime` at virtual time `T0`: 2030-01-01T00:00:00Z, later than the
/// real files a test may look at (their times are real).
pub(crate) const REALTIME_AT_T0: u64 = 1_893_456_000 * 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    Futex { addr: usize, bitset: u32, seq: u64 },
    Epoll { ep: u64 },
    Fd { obj: u64, mask: u32 },
    Join { tid: Tid },
    Sleep,
    AllExited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TState {
    Runnable,
    Running,
    Blocked(Why, Option<u64>),
    Exited,
}

pub(crate) struct Thread {
    pub(crate) state: TState,
    pub(crate) go: &'static AtomicU32,
    pub(crate) pthread: Option<libc::pthread_t>,
    /// What woke the thread: 0, or an errno such as ETIMEDOUT.
    pub(crate) result: i32,
}

/// Counts of injected faults, by name.
pub type FaultCounts = BTreeMap<&'static str, u64>;

pub(crate) struct World {
    pub(crate) gen: u64,
    pub(crate) seed: u64,
    rng: u64,
    /// A separate stream for `getrandom`, so that how often a program asks
    /// for random bytes does not shift scheduling or faults.
    data_rng: u64,
    pub(crate) now: u64,
    pub(crate) faults: Faults,
    pub(crate) threads: Vec<Thread>,
    pub(crate) current: Tid,
    pub(crate) by_pthread: HashMap<libc::pthread_t, Tid>,
    futex_seq: u64,
    events: BinaryHeap<Reverse<(u64, u64)>>,
    event_data: HashMap<u64, Event>,
    event_seq: u64,
    pub(crate) net: Net,
    pub(crate) files: Files,
    pub(crate) steps: u64,
    pub(crate) hash: u64,
    pub(crate) step_hashes: Vec<u64>,
    pub(crate) tail: VecDeque<String>,
    pub(crate) keep: usize,
    pub(crate) full: Option<Vec<String>>,
    pub(crate) injected: FaultCounts,
    pub(crate) limit_ns: u64,
    pub(crate) failure: Option<String>,
    /// `steps` when the running thread got the baton (for time slices).
    slice_from: u64,
    /// Clock reads since the last hand-over.
    pub(crate) clock_reads: u64,
}

struct Global {
    lock: SpinLock,
    world: UnsafeCell<Option<World>>,
}

// SAFETY: `world` is only touched with `lock` held.
unsafe impl Sync for Global {}

static GLOBAL: Global = Global {
    lock: SpinLock::new(),
    world: UnsafeCell::new(None),
};

thread_local! {
    /// (world generation, tid + 1) of the simulated thread running here;
    /// 0 for a thread outside the simulation.
    static ME: Cell<(u64, usize)> = const { Cell::new((0, 0)) };
}

/// The world, locked.
pub(crate) struct Guard {
    _priv: (),
}

impl Guard {
    pub(crate) fn lock() -> Guard {
        GLOBAL.lock.lock();
        Guard { _priv: () }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        GLOBAL.lock.unlock();
    }
}

impl std::ops::Deref for Guard {
    type Target = Option<World>;
    fn deref(&self) -> &Option<World> {
        // SAFETY: the lock is held.
        unsafe { &*GLOBAL.world.get() }
    }
}

impl std::ops::DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut Option<World> {
        // SAFETY: the lock is held.
        unsafe { &mut *GLOBAL.world.get() }
    }
}

pub(crate) fn set_me(gen: u64, tid: Option<Tid>) {
    ME.with(|m| m.set((gen, tid.map_or(0, |t| t + 1))));
}

/// If the calling thread is a simulated thread of the live world, the
/// locked world and its tid.
pub(crate) fn enter() -> Option<(Guard, Tid)> {
    let (gen, t) = ME.with(|m| m.get());
    if t == 0 {
        return None;
    }
    let g = Guard::lock();
    match &*g {
        Some(w) if w.gen == gen && w.failure.is_none() => Some((g, t - 1)),
        _ => {
            drop(g);
            set_me(0, None);
            None
        }
    }
}

pub(crate) fn is_sim_thread() -> bool {
    ME.with(|m| m.get().1 != 0)
}

pub(crate) fn install(w: World) {
    let mut g = Guard::lock();
    *g = Some(w);
}

pub(crate) fn uninstall() -> Option<World> {
    let mut g = Guard::lock();
    g.take()
}

pub(crate) fn wait_go(go: &AtomicU32) {
    loop {
        if go.swap(0, Ordering::Acquire) == 1 {
            return;
        }
        raw::futex_wait(go, 0);
    }
}

fn give_go(go: &AtomicU32) {
    go.store(1, Ordering::Release);
    raw::futex_wake(go);
}

impl World {
    pub(crate) fn new(
        gen: u64,
        seed: u64,
        faults: Faults,
        keep: usize,
        full: bool,
        limit_ns: u64,
    ) -> Self {
        let mut w = World {
            gen,
            seed,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            data_rng: seed ^ 0xD1B5_4A32_D192_ED03,
            now: T0,
            faults,
            threads: Vec::new(),
            current: 0,
            by_pthread: HashMap::new(),
            futex_seq: 0,
            events: BinaryHeap::new(),
            event_data: HashMap::new(),
            event_seq: 0,
            net: Net::default(),
            files: Files::default(),
            steps: 0,
            hash: 0xcbf2_9ce4_8422_2325,
            step_hashes: Vec::new(),
            tail: VecDeque::new(),
            keep,
            full: full.then(Vec::new),
            injected: BTreeMap::new(),
            limit_ns,
            failure: None,
            slice_from: 0,
            clock_reads: 0,
        };
        w.new_thread(TState::Running);
        w
    }

    // ------------------------------------------------------------ randomness

    pub(crate) fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `true` with probability `p`.
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * 1.0 < p
    }

    /// `true` with probability `p`, counted as an injected fault `name`.
    pub(crate) fn fault(&mut self, p: f64, name: &'static str) -> bool {
        if self.chance(p) {
            *self.injected.entry(name).or_default() += 1;
            true
        } else {
            false
        }
    }

    /// Uniform in `lo..=hi`.
    pub(crate) fn between(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    pub(crate) fn fill_data(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            self.data_rng = self.data_rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.data_rng;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            let v = (z ^ (z >> 31)).to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    // ----------------------------------------------------------------- trace

    /// Records one step of the run. Only values that follow from the seed go
    /// in (virtual ids, never real descriptor numbers or addresses).
    pub(crate) fn trace(&mut self, tid: Tid, what: std::fmt::Arguments<'_>) {
        self.steps += 1;
        let mut line = String::with_capacity(48);
        let _ = write!(line, "{:>12} t{} {}", self.now - T0, tid, what);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in line.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        self.hash = (self.hash ^ h).wrapping_mul(0x100_0000_01b3);
        self.step_hashes.push(h);
        if let Some(full) = &mut self.full {
            full.push(line.clone());
        }
        if self.keep > 0 {
            if self.tail.len() == self.keep {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
    }

    // ---------------------------------------------------------------- events

    pub(crate) fn schedule(&mut self, at: u64, ev: Event) {
        let seq = self.event_seq;
        self.event_seq += 1;
        self.events.push(Reverse((at.max(self.now), seq)));
        self.event_data.insert(seq, ev);
    }

    /// Runs the events that are due.
    pub(crate) fn run_due(&mut self) {
        while let Some(Reverse((at, seq))) = self.events.peek().copied() {
            if at > self.now {
                break;
            }
            self.events.pop();
            if let Some(ev) = self.event_data.remove(&seq) {
                self.apply(ev);
            }
        }
    }

    /// Advances virtual time by the cost of one system call, runs what is
    /// due and stops the run if it has gone on too long.
    pub(crate) fn tick(&mut self) {
        let cost = self.faults.syscall_ns;
        self.now += cost;
        if self.faults.clock_jump > 0.0 && self.fault(self.faults.clock_jump, "clock-jump") {
            let jump = self.between(1, 30) * 1_000_000_000;
            self.now += jump;
        }
        self.run_due();
        if self.now - T0 > self.limit_ns && self.failure.is_none() {
            self.fail_run(format!(
                "virtual time limit reached ({} s): livelock or a run that never ends",
                self.limit_ns / 1_000_000_000
            ));
        }
    }

    // ------------------------------------------------------------- scheduler

    pub(crate) fn new_thread(&mut self, state: TState) -> Tid {
        let go: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        self.threads.push(Thread {
            state,
            go,
            pthread: None,
            result: 0,
        });
        self.threads.len() - 1
    }

    fn ready_to_wake(&self, why: Why) -> bool {
        match why {
            Why::Futex { .. } | Why::Sleep => false,
            Why::Epoll { ep } => self.net.epoll_has_ready(ep),
            Why::Fd { obj, mask } => self.net.readiness(obj) & mask != 0,
            Why::Join { tid } => self.threads[tid].state == TState::Exited,
            Why::AllExited => self
                .threads
                .iter()
                .enumerate()
                .all(|(i, t)| i == 0 || t.state == TState::Exited),
        }
    }

    fn recheck_blocked(&mut self) {
        for i in 0..self.threads.len() {
            if let TState::Blocked(why, deadline) = self.threads[i].state {
                if self.ready_to_wake(why) {
                    self.threads[i].state = TState::Runnable;
                    self.threads[i].result = 0;
                } else if deadline.is_some_and(|d| d <= self.now) {
                    self.threads[i].state = TState::Runnable;
                    self.threads[i].result = libc::ETIMEDOUT;
                }
            }
        }
    }

    /// Picks the thread to run next; `me` is a candidate if it is still
    /// running. Advances virtual time when every thread waits.
    fn choose_next(&mut self, me: Tid) -> Option<Tid> {
        loop {
            self.recheck_blocked();
            let cands: Vec<Tid> = self
                .threads
                .iter()
                .enumerate()
                .filter(|(i, t)| {
                    t.state == TState::Runnable || (*i == me && t.state == TState::Running)
                })
                .map(|(i, _)| i)
                .collect();
            if !cands.is_empty() {
                let pick = cands[(self.next_u64() % cands.len() as u64) as usize];
                return Some(pick);
            }
            let next_event = self.events.peek().map(|Reverse((at, _))| *at);
            let next_deadline = self
                .threads
                .iter()
                .filter_map(|t| match t.state {
                    TState::Blocked(_, d) => d,
                    _ => None,
                })
                .min();
            let t = match (next_event, next_deadline) {
                (None, None) => return None,
                (a, b) => a.into_iter().chain(b).min().unwrap(),
            };
            self.now = self.now.max(t);
            self.run_due();
            if self.now - T0 > self.limit_ns {
                return None;
            }
        }
    }

    pub(crate) fn fail_run(&mut self, msg: String) {
        if self.failure.is_some() {
            return;
        }
        let mut report = format!(
            "\ngiong-sim: run failed with seed {} at virtual time {:.6} s after {} steps\n  {}\n  replay: GIONG_SIM_SEED={}\n  threads:\n",
            self.seed,
            (self.now - T0) as f64 / 1e9,
            self.steps,
            msg,
            self.seed
        );
        for (i, t) in self.threads.iter().enumerate() {
            let _ = writeln!(report, "    t{i}: {:?}", t.state);
        }
        report.push_str("  last steps:\n");
        for line in &self.tail {
            let _ = writeln!(report, "    {line}");
        }
        raw::eprint(&report);
        if let (Some(full), Ok(path)) = (&self.full, std::env::var("GIONG_SIM_DUMP")) {
            raw::write_file(&path, &(full.join("\n") + "\n"));
            raw::eprint(&format!("  full trace written to {path}\n"));
        }
        self.failure = Some(msg);
    }
}

/// Releases every thread so that the failing run unwinds: all of them now
/// see a world with a failure and fall back to real system calls.
fn release_all(g: &Guard) {
    if let Some(w) = &**g {
        for t in &w.threads {
            give_go(t.go);
        }
    }
}

/// Hands the baton to the next thread and waits until it comes back.
/// `me` has already been marked blocked, runnable or running.
pub(crate) fn switch(mut g: Guard, me: Tid) -> Guard {
    let w = g.as_mut().unwrap();
    let Some(next) = w.choose_next(me) else {
        let why = if w.now - T0 > w.limit_ns {
            "virtual time limit reached while every thread waits"
        } else {
            "deadlock: every thread waits and no event or timeout is pending"
        };
        w.fail_run(why.to_string());
        release_all(&g);
        return g;
    };
    if next != me && w.threads[me].state == TState::Running {
        w.threads[me].state = TState::Runnable;
    }
    w.threads[next].state = TState::Running;
    w.current = next;
    w.slice_from = w.steps;
    w.clock_reads = 0;
    if next == me {
        return g;
    }
    let go = w.threads[next].go;
    let mine = w.threads[me].go;
    drop(g);
    give_go(go);
    wait_go(mine);
    Guard::lock()
}

/// Blocks `me` until `why` holds or `deadline` passes. Returns 0 or
/// ETIMEDOUT (or ECANCELED if the run failed meanwhile).
pub(crate) fn block(mut g: Guard, me: Tid, why: Why, deadline: Option<u64>) -> (Guard, i32) {
    let w = g.as_mut().unwrap();
    w.threads[me].state = TState::Blocked(why, deadline);
    let g = switch(g, me);
    let res = match &*g {
        Some(w) if w.failure.is_none() => w.threads[me].result,
        _ => libc::ECANCELED,
    };
    (g, res)
}

/// Called at the start of every interposed call of a simulated thread:
/// time moves, events run and, now and then, another thread gets a turn.
pub(crate) fn step(mut g: Guard, me: Tid) -> Guard {
    let w = g.as_mut().unwrap();
    w.tick();
    if w.failure.is_some() {
        release_all(&g);
        return g;
    }
    let p = w.faults.preempt;
    if w.chance(p) {
        w.threads[me].state = TState::Running;
        return switch(g, me);
    }
    // A time slice: a thread that keeps running without blocking (a busy
    // loop around a non-blocking call) lets the others run now and then,
    // as a real scheduler's tick would.
    if w.steps - w.slice_from >= SLICE {
        w.slice_from = w.steps;
        w.threads[me].state = TState::Running;
        return switch(g, me);
    }
    g
}

/// The thread `me` has finished: wake whoever joins it and hand the baton
/// on without waiting for it to come back.
pub(crate) fn exit_thread(mut g: Guard, me: Tid) {
    let w = g.as_mut().unwrap();
    w.threads[me].state = TState::Exited;
    let tr = format_args!("exit").to_string();
    w.trace(me, format_args!("{tr}"));
    match w.choose_next(me) {
        Some(next) => {
            w.threads[next].state = TState::Running;
            w.current = next;
            w.slice_from = w.steps;
            let go = w.threads[next].go;
            drop(g);
            give_go(go);
        }
        None => {
            w.fail_run("deadlock after a thread exited".into());
            release_all(&g);
        }
    }
}

/// Futex wait for a simulated thread. Returns the raw result (0 or -errno).
pub(crate) fn futex_wait(
    g: Guard,
    me: Tid,
    addr: usize,
    val: u32,
    bitset: u32,
    deadline: Option<u64>,
) -> (Guard, i64) {
    // SAFETY: the caller passed a futex word it owns; reading it is what
    // the kernel does too.
    let cur = unsafe { (*(addr as *const AtomicU32)).load(Ordering::SeqCst) };
    if cur != val {
        return (g, -(libc::EAGAIN as i64));
    }
    let mut g = g;
    let w = g.as_mut().unwrap();
    // Futex waits may wake for no reason (a signal); callers must recheck.
    let p = w.faults.eintr;
    if w.fault(p, "futex-eintr") {
        return (g, -(libc::EINTR as i64));
    }
    w.futex_seq += 1;
    let seq = w.futex_seq;
    let (g, res) = block(g, me, Why::Futex { addr, bitset, seq }, deadline);
    (g, if res == 0 { 0 } else { -(res as i64) })
}

/// Futex wake: the longest waiting threads first, as many as `n`.
pub(crate) fn futex_wake(w: &mut World, addr: usize, n: i64, bitset: u32) -> i64 {
    let mut waiters: Vec<(u64, Tid)> = w
        .threads
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t.state {
            TState::Blocked(
                Why::Futex {
                    addr: a,
                    bitset: b,
                    seq,
                },
                _,
            ) if a == addr && b & bitset != 0 => Some((seq, i)),
            _ => None,
        })
        .collect();
    waiters.sort();
    let mut woken = 0;
    for (_, tid) in waiters.into_iter().take(n.max(0) as usize) {
        w.threads[tid].state = TState::Runnable;
        w.threads[tid].result = 0;
        woken += 1;
    }
    woken
}

impl World {
    /// Descriptors still open, for [`crate::Report::open`].
    pub(crate) fn still_open(&self) -> Vec<String> {
        let mut out: Vec<(ObjId, String)> = Vec::new();
        for id in self.net.fds.values() {
            let what = match self.net.objs.get(id).map(|o| &o.kind) {
                Some(Kind::Sock(s)) => {
                    format!("{} socket", if s.stream { "stream" } else { "datagram" })
                }
                Some(Kind::EventFd { .. }) => "eventfd".to_string(),
                Some(Kind::Epoll { .. }) => "epoll".to_string(),
                None => "gone".to_string(),
            };
            out.push((*id, format!("o{id} {what}")));
        }
        out.sort();
        out.dedup();
        let mut files: Vec<u64> = self.files.open_ids();
        files.sort();
        out.into_iter()
            .map(|(_, s)| s)
            .chain(files.into_iter().map(|f| format!("f{f}")))
            .collect()
    }
}
