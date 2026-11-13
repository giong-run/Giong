//! Timers: [`sleep`], [`sleep_until`] and [`timeout`].
//!
//! Timers live in user space, in a per-worker hierarchical timing wheel. The
//! event loop passes the nearest deadline to `io_uring_enter` as its wait
//! timeout, so an idle worker with pending timers still costs a single syscall
//! per wake-up, and arming or cancelling a timer costs no syscall at all — just
//! an O(1) list operation.
//!
//! Timer resolution is one millisecond; a timer never fires early.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub use std::time::Instant;

use crate::runtime;
use crate::slab::Slab;

// ------------------------------------------------------------------ the wheel
//
// A hierarchical timing wheel with 1 ms ticks: six levels of 64 slots each.
// Level 0 holds timers due within the next 64 ms (one slot per ms), level 1
// the next ~4 s (64 ms per slot), and so on up to roughly two years. A timer
// lives in the level given by the highest bit in which its deadline differs
// from the current time; when the wheel reaches the start of a higher-level
// slot, the timers in it are redistributed ("cascaded") to lower levels.
//
// Arming and cancelling are O(1) — a linked-list insert/unlink — which is
// what matters for servers: nearly every timer is an I/O timeout that gets
// cancelled long before it would fire.

const LEVELS: usize = 6;
const SLOTS: usize = 64;
const SLOT_MASK: u64 = (SLOTS as u64) - 1;
/// Ticks covered by the whole wheel.
const MAX_TICKS: u64 = 1 << (6 * LEVELS);
const NIL: usize = usize::MAX;
const NANOS_PER_TICK: u128 = 1_000_000;

pub(crate) type TimerKey = (usize, u64);

struct Entry {
    /// Deadline in ticks since `Timers::start`.
    when: u64,
    /// Distinguishes this timer from earlier users of the same slab index.
    uid: u64,
    waker: Waker,
    level: u8,
    slot: u8,
    prev: usize,
    next: usize,
}

#[derive(Clone, Copy)]
struct Level {
    /// Bit `i` is set when `heads[i]` is non-empty.
    occupied: u64,
    heads: [usize; SLOTS],
}

pub(crate) struct Timers {
    start: Instant,
    /// The tick up to which the wheel has been advanced.
    elapsed: u64,
    entries: Slab<Entry>,
    levels: [Level; LEVELS],
    seq: u64,
}

fn level_for(elapsed: u64, when: u64) -> usize {
    let masked = ((elapsed ^ when) | SLOT_MASK).min(MAX_TICKS - 1);
    let significant = 63 - masked.leading_zeros() as usize;
    significant / 6
}

impl Timers {
    pub(crate) fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    fn starting_at(start: Instant) -> Self {
        Self {
            start,
            elapsed: 0,
            entries: Slab::new(),
            levels: [Level {
                occupied: 0,
                heads: [NIL; SLOTS],
            }; LEVELS],
            seq: 0,
        }
    }

    /// The first tick at or after `t` (deadlines round up: never fire early).
    fn tick_ceil(&self, t: Instant) -> u64 {
        let nanos = t.saturating_duration_since(self.start).as_nanos();
        nanos.div_ceil(NANOS_PER_TICK).min(u64::MAX as u128) as u64
    }

    /// The last tick at or before `t`.
    fn tick_floor(&self, t: Instant) -> u64 {
        let nanos = t.saturating_duration_since(self.start).as_nanos();
        (nanos / NANOS_PER_TICK).min(u64::MAX as u128) as u64
    }

    fn insert(&mut self, deadline: Instant, waker: Waker) -> TimerKey {
        // Strictly in the future of the wheel, and within its range. A timer
        // beyond the range fires early; `Sleep` then simply re-arms itself.
        let when = self
            .tick_ceil(deadline)
            .clamp(self.elapsed + 1, self.elapsed + MAX_TICKS - 1);
        self.seq += 1;
        let uid = self.seq;
        let idx = self.entries.insert(Entry {
            when,
            uid,
            waker,
            level: 0,
            slot: 0,
            prev: NIL,
            next: NIL,
        });
        self.link(idx);
        (idx, uid)
    }

    /// Refreshes the waker of a registered timer, re-arming it if it is gone.
    fn update(&mut self, key: TimerKey, deadline: Instant, waker: &Waker) -> TimerKey {
        match self.entries.get_mut(key.0) {
            Some(entry) if entry.uid == key.1 => {
                if !entry.waker.will_wake(waker) {
                    entry.waker = waker.clone();
                }
                key
            }
            _ => self.insert(deadline, waker.clone()),
        }
    }

    fn remove(&mut self, key: TimerKey) -> Option<Waker> {
        match self.entries.get(key.0) {
            Some(entry) if entry.uid == key.1 => {}
            _ => return None,
        }
        self.unlink(key.0);
        self.entries.remove(key.0).map(|entry| entry.waker)
    }

    fn link(&mut self, idx: usize) {
        let when = self.entries.get(idx).expect("timer entry").when;
        let level = level_for(self.elapsed, when);
        let slot = ((when >> (6 * level)) & SLOT_MASK) as usize;
        let head = self.levels[level].heads[slot];
        {
            let entry = self.entries.get_mut(idx).expect("timer entry");
            entry.level = level as u8;
            entry.slot = slot as u8;
            entry.prev = NIL;
            entry.next = head;
        }
        if head != NIL {
            self.entries.get_mut(head).expect("timer entry").prev = idx;
        }
        self.levels[level].heads[slot] = idx;
        self.levels[level].occupied |= 1 << slot;
    }

    fn unlink(&mut self, idx: usize) {
        let (level, slot, prev, next) = {
            let entry = self.entries.get(idx).expect("timer entry");
            (
                entry.level as usize,
                entry.slot as usize,
                entry.prev,
                entry.next,
            )
        };
        if prev != NIL {
            self.entries.get_mut(prev).expect("timer entry").next = next;
        } else {
            self.levels[level].heads[slot] = next;
            if next == NIL {
                self.levels[level].occupied &= !(1 << slot);
            }
        }
        if next != NIL {
            self.entries.get_mut(next).expect("timer entry").prev = prev;
        }
    }

    /// The next slot that needs attention: `(level, slot, tick)`. At level 0
    /// that is a timer firing; at higher levels, a slot to cascade.
    fn next_expiration(&self) -> Option<(usize, usize, u64)> {
        // Lower levels always expire before higher ones: a level only holds
        // timers inside its current rotation, which ends where the next
        // higher-level slot begins.
        for level in 0..LEVELS {
            let occupied = self.levels[level].occupied;
            if occupied == 0 {
                continue;
            }
            let slot_range = 1u64 << (6 * level);
            let level_range = slot_range << 6;
            let now_slot = ((self.elapsed >> (6 * level)) & SLOT_MASK) as u32;
            let distance = occupied.rotate_right(now_slot).trailing_zeros();
            let slot = ((now_slot + distance) & SLOT_MASK as u32) as usize;
            let level_start = self.elapsed & !(level_range - 1);
            let mut deadline = level_start + slot as u64 * slot_range;
            if deadline <= self.elapsed {
                // Only possible on the top level, for timers clamped to the
                // wheel's range: the slot belongs to the next rotation.
                deadline += level_range;
            }
            return Some((level, slot, deadline));
        }
        None
    }

    /// When the event loop must wake up next for timers.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let (_, _, tick) = self.next_expiration()?;
        Some(self.start + Duration::from_millis(tick))
    }

    /// Advances the wheel to `now` and returns the wakers of all due timers.
    pub(crate) fn expired(&mut self, now: Instant) -> Vec<Waker> {
        let now_tick = self.tick_floor(now);
        let mut fired = Vec::new();
        while let Some((level, slot, deadline)) = self.next_expiration() {
            if deadline > now_tick {
                break;
            }
            let mut idx = std::mem::replace(&mut self.levels[level].heads[slot], NIL);
            self.levels[level].occupied &= !(1 << slot);
            self.elapsed = deadline;
            while idx != NIL {
                let (next, when) = {
                    let entry = self.entries.get(idx).expect("timer entry");
                    (entry.next, entry.when)
                };
                if when <= deadline {
                    fired.push(self.entries.remove(idx).expect("timer entry").waker);
                } else {
                    // Cascade to a lower level, now that time has moved on.
                    self.link(idx);
                }
                idx = next;
            }
        }
        if now_tick > self.elapsed {
            self.elapsed = now_tick;
        }
        fired
    }

    /// Drops every timer, returning the wakers so the caller can drop them
    /// outside of any borrow.
    pub(crate) fn clear(&mut self) -> Vec<Waker> {
        for level in self.levels.iter_mut() {
            *level = Level {
                occupied: 0,
                heads: [NIL; SLOTS],
            };
        }
        self.entries
            .take_all()
            .into_iter()
            .map(|entry| entry.waker)
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Future returned by [`sleep`] and [`sleep_until`].
#[must_use = "futures do nothing unless awaited"]
pub struct Sleep {
    deadline: Instant,
    /// Registration in the worker's timer map, and that worker's uid.
    key: Option<(TimerKey, usize)>,
}

impl Sleep {
    /// The instant at which this future completes.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Changes the deadline, re-arming the timer if it had already fired.
    pub fn reset(&mut self, deadline: Instant) {
        self.unregister();
        self.deadline = deadline;
    }

    fn unregister(&mut self) {
        if let Some((key, uid)) = self.key.take() {
            let waker = runtime::try_with_worker(|w| {
                if w.shared.uid == uid {
                    w.timers
                        .try_borrow_mut()
                        .ok()
                        .and_then(|mut t| t.remove(key))
                } else {
                    None
                }
            });
            drop(waker);
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            self.unregister();
            return Poll::Ready(());
        }
        let deadline = self.deadline;
        let registered = self.key;
        let key = runtime::with_worker(|w| {
            let mut timers = w.timers.borrow_mut();
            match registered {
                Some((key, uid)) if uid == w.shared.uid => {
                    (timers.update(key, deadline, cx.waker()), uid)
                }
                _ => (timers.insert(deadline, cx.waker().clone()), w.shared.uid),
            }
        });
        self.key = Some(key);
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline)
            .finish()
    }
}

/// Completes after `duration` has elapsed.
pub fn sleep(duration: Duration) -> Sleep {
    let now = Instant::now();
    // Saturate instead of panicking on absurd durations.
    let deadline = now
        .checked_add(duration)
        .unwrap_or_else(|| now + Duration::from_secs(86400 * 365 * 30));
    sleep_until(deadline)
}

/// Completes at `deadline`.
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        deadline,
        key: None,
    }
}

/// Error returned by [`timeout`] when the time runs out first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// An [`io::Error`](std::io::Error) with kind `TimedOut` and code
/// [`GIONG-E0007`](crate::error::Code::TimedOut).
impl From<Elapsed> for std::io::Error {
    fn from(_: Elapsed) -> Self {
        crate::error::raise(
            crate::error::Code::TimedOut,
            "the operation did not finish within the time given to giong::time::timeout",
            "give it more time, or find out why the operation is slow.",
        )
    }
}

/// Runs `fut`, giving up (and dropping it) if it takes longer than `duration`.
///
/// Dropping an in-flight I/O future is safe: the operation is cancelled and
/// its buffer is kept alive until the kernel confirms.
pub fn timeout<F: Future>(duration: Duration, fut: F) -> Timeout<F> {
    Timeout {
        fut,
        delay: sleep(duration),
    }
}

/// Future returned by [`timeout`]: `Ok` with the output of the future it
/// wraps, or `Err(Elapsed)` if the time ran out first.
///
/// It is only as large as the wrapped future plus a [`Sleep`], so wrapping
/// the I/O of every connection in a timeout costs no extra memory.
#[must_use = "futures do nothing unless awaited"]
pub struct Timeout<F> {
    /// Pinned with the `Timeout`: never moved out.
    fut: F,
    delay: Sleep,
}

impl<F> Timeout<F> {
    /// The instant at which the time runs out.
    pub fn deadline(&self) -> Instant {
        self.delay.deadline()
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved. `Sleep` is `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        if let Poll::Ready(value) = fut.poll(cx) {
            return Poll::Ready(Ok(value));
        }
        Pin::new(&mut this.delay).poll(cx).map(|()| Err(Elapsed))
    }
}

impl<F> fmt::Debug for Timeout<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timeout")
            .field("deadline", &self.delay.deadline())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::task::Wake;

    /// A waker that records its id when woken.
    struct Recorder(u64, Arc<Mutex<Vec<u64>>>);

    impl Wake for Recorder {
        fn wake(self: Arc<Self>) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Drives the wheel and a trivially correct model (an ordered map) with
    /// the same random sequence of arm / cancel / advance operations and
    /// checks that they agree on exactly which timers have fired, and that
    /// the wheel never asks the event loop to sleep past a due timer.
    fn run_model(seed: u64, steps: usize, max_delay_ms: u64, max_step_us: u64) {
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let mut rng = Rng(seed);
        let fired = Arc::new(Mutex::new(Vec::new()));
        // id -> (deadline in ns since start, wheel key)
        let mut live: BTreeMap<u64, (u128, TimerKey)> = BTreeMap::new();
        let mut now_ns: u128 = 0;
        let mut next_id = 0u64;

        for _ in 0..steps {
            match rng.below(10) {
                0..=4 => {
                    let delay_ns = rng.below(max_delay_ms * 1_000_000) as u128 + 1;
                    let deadline_ns = now_ns + delay_ns;
                    let waker = Waker::from(Arc::new(Recorder(next_id, fired.clone())));
                    let key = wheel.insert(start + Duration::from_nanos(deadline_ns as u64), waker);
                    live.insert(next_id, (deadline_ns, key));
                    next_id += 1;
                }
                5..=6 => {
                    if !live.is_empty() {
                        let nth = rng.below(live.len() as u64) as usize;
                        let id = *live.keys().nth(nth).unwrap();
                        let (_, key) = live.remove(&id).unwrap();
                        assert!(
                            wheel.remove(key).is_some(),
                            "live timer {id} missing from the wheel"
                        );
                        assert!(wheel.remove(key).is_none(), "timer {id} removed twice");
                    }
                }
                _ => {
                    now_ns += rng.below(max_step_us * 1_000) as u128 + 1;
                    let now = start + Duration::from_nanos(now_ns as u64);
                    for waker in wheel.expired(now) {
                        waker.wake();
                    }
                    let mut got = std::mem::take(&mut *fired.lock().unwrap());
                    got.sort_unstable();
                    // A timer is due once the wheel's clock (whole ms) reaches its
                    // deadline rounded up to a whole ms.
                    let now_tick = now_ns / NANOS_PER_TICK;
                    let mut expected: Vec<u64> = live
                        .iter()
                        .filter(|(_, (deadline_ns, _))| {
                            deadline_ns.div_ceil(NANOS_PER_TICK) <= now_tick
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    expected.sort_unstable();
                    assert_eq!(
                        got, expected,
                        "seed {seed}: wrong set of timers fired at {now_ns} ns"
                    );
                    for id in &got {
                        let (deadline_ns, key) = live.remove(id).unwrap();
                        assert!(deadline_ns <= now_ns, "timer {id} fired early");
                        assert!(
                            wheel.remove(key).is_none(),
                            "fired timer {id} still in the wheel"
                        );
                    }
                }
            }
            assert_eq!(wheel.len(), live.len());
            // The loop must never be told to sleep past the earliest live timer.
            match (wheel.next_deadline(), live.values().map(|(d, _)| *d).min()) {
                (None, None) => {}
                (Some(wake), Some(earliest_ns)) => {
                    let wake_ns = wake.duration_since(start).as_nanos();
                    let earliest_tick_ns = earliest_ns.div_ceil(NANOS_PER_TICK) * NANOS_PER_TICK;
                    assert!(wake_ns <= earliest_tick_ns, "seed {seed}: would oversleep");
                    assert!(
                        wake_ns > (now_ns / NANOS_PER_TICK) * NANOS_PER_TICK,
                        "seed {seed}: wake-up in the past"
                    );
                }
                (wake, earliest) => {
                    panic!("seed {seed}: next_deadline {wake:?} vs model {earliest:?}")
                }
            }
        }
        let leftover = wheel.clear();
        assert_eq!(leftover.len(), live.len());
        assert_eq!(wheel.len(), 0);
        assert!(wheel.next_deadline().is_none());
    }

    #[test]
    fn wheel_matches_model_short_timers() {
        for seed in 1..=40 {
            run_model(seed, 4_000, 200, 3_000);
        }
    }

    #[test]
    fn wheel_matches_model_mixed_timers() {
        // Delays up to 10 minutes with steps up to 5 s: exercises cascading
        // across four levels.
        for seed in 100..=130 {
            run_model(seed, 4_000, 600_000, 5_000_000);
        }
    }

    #[test]
    fn wheel_matches_model_long_timers() {
        // Delays up to 30 days, steps up to 2 hours.
        for seed in 200..=215 {
            run_model(seed, 3_000, 30 * 86_400_000, 7_200_000_000);
        }
    }

    #[test]
    fn far_future_timers_are_clamped_not_lost() {
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let fired = Arc::new(Mutex::new(Vec::new()));
        let waker = Waker::from(Arc::new(Recorder(1, fired.clone())));
        // ~32 years: beyond the wheel's range.
        let key = wheel.insert(start + Duration::from_secs(1_000_000_000), waker);
        assert_eq!(wheel.len(), 1);
        assert!(wheel.next_deadline().is_some());
        assert!(wheel.expired(start + Duration::from_secs(3600)).is_empty());
        assert!(wheel.remove(key).is_some());
        assert!(wheel.next_deadline().is_none());
    }
}
