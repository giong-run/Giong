//! Timers: [`sleep`], [`sleep_until`] and [`timeout`].
//!
//! Timers live in user space, in a per-worker hierarchical timing wheel. The
//! event loop passes the nearest deadline to `io_uring_enter` as its wait
//! timeout, so an idle worker with pending timers still costs a single syscall
//! per wake-up, and arming or cancelling a timer costs no syscall at all — just
//! an O(1) list operation.
//!
//! A timer fires at its deadline, to the precision of the clock and of the
//! kernel's wake-up, and never early: a sleep of 1 ms takes about 1.05 ms
//! on Linux. Timers fire in the order of their deadlines to the
//! millisecond; within one millisecond, in no set order.
//!
//! After timers fire, a worker waits for the next ones until at least a
//! millisecond later, so that thousands of timers cost about one wake-up per
//! millisecond, not one each. A loop of sleeps shorter than 1 ms therefore
//! takes about 1 ms a turn.

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
// The ticks only place timers. Each timer keeps its exact deadline, and the
// event loop sleeps until the earliest one, not until the start of its
// millisecond: once a slot's millisecond has come, the timers in it whose
// deadline has not wait on a short "due" list for it. Rounding every
// deadline up to a whole tick instead made a timer armed just after one
// fired (the usual `loop { sleep(d).await; .. }`) a whole tick late.
//
// Arming and cancelling are O(1) — a linked-list insert/unlink — which is
// what matters for servers: nearly every timer is an I/O timeout that gets
// cancelled long before it would fire. An [`Alarm`] (a connection's timeouts
// in giong-http) does neither per wait: it stays armed, and a later deadline
// only changes the deadline the entry keeps; the wheel moves the entry when
// it reaches its slot.

const LEVELS: usize = 6;
const SLOTS: usize = 64;
const SLOT_MASK: u64 = (SLOTS as u64) - 1;
/// Ticks covered by the whole wheel.
const MAX_TICKS: u64 = 1 << (6 * LEVELS);
const NIL: usize = usize::MAX;
const NANOS_PER_TICK: u128 = 1_000_000;
const TICK: Duration = Duration::from_millis(1);

pub(crate) type TimerKey = (usize, u64);

struct Entry {
    /// The deadline, in nanoseconds since `Timers::start`. Its millisecond,
    /// [`tick_of`], places the timer in the wheel.
    at: u64,
    /// Distinguishes this timer from earlier users of the same slab index.
    uid: u64,
    waker: Waker,
    /// The wheel level, or [`DUE`] for the list of timers whose millisecond
    /// has come.
    level: u8,
    slot: u8,
    /// An [`Alarm`]'s: woken at shutdown as well.
    alarm: bool,
    prev: usize,
    next: usize,
}

/// The `level` of an entry in [`Timers::due`].
const DUE: u8 = LEVELS as u8;

/// The tick (millisecond) a deadline falls in.
fn tick_of(at: u64) -> u64 {
    at / NANOS_PER_TICK as u64
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
    /// For each slot of level 0, a time no later than the earliest deadline
    /// in it: exact, unless a timer has been removed from it since. The event
    /// loop wakes up then, not at the start of the slot's millisecond.
    earliest: [u64; SLOTS],
    /// Timers whose millisecond has come but whose deadline, later in it,
    /// has not: the head of a list.
    due: usize,
    /// No later than the earliest deadline in `due`; `u64::MAX` when it is
    /// empty.
    due_at: u64,
    /// One tick after the last time timers fired. The event loop does not
    /// wake up for timers again before then: a lone timer still fires at its
    /// deadline, while many timers close together fire in batches, at most
    /// one wake-up per millisecond, as when deadlines were rounded to ticks.
    quiet_until: u64,
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
            earliest: [u64::MAX; SLOTS],
            due: NIL,
            due_at: u64::MAX,
            quiet_until: 0,
            seq: 0,
        }
    }

    /// `t` in nanoseconds since the wheel started.
    fn nanos(&self, t: Instant) -> u64 {
        let nanos = t.saturating_duration_since(self.start).as_nanos();
        nanos.min(u64::MAX as u128) as u64
    }

    /// Where `deadline` goes in the wheel: in nanoseconds since it started,
    /// and, beyond its range, at its last tick (the timer then fires early;
    /// `Sleep` and `Alarm` simply re-arm themselves).
    fn place(&self, deadline: Instant) -> u64 {
        let at = self.nanos(deadline);
        let last = self.elapsed + MAX_TICKS - 1;
        if tick_of(at) > last {
            last * NANOS_PER_TICK as u64
        } else {
            at
        }
    }

    fn insert(&mut self, deadline: Instant, waker: Waker) -> TimerKey {
        self.insert_entry(deadline, waker, false)
    }

    #[inline]
    fn insert_entry(&mut self, deadline: Instant, waker: Waker, alarm: bool) -> TimerKey {
        let at = self.place(deadline);
        self.seq += 1;
        let uid = self.seq;
        let idx = self.entries.insert(Entry {
            at,
            uid,
            waker,
            level: 0,
            slot: 0,
            alarm,
            prev: NIL,
            next: NIL,
        });
        self.link(idx);
        (idx, uid)
    }

    /// Moves a registered timer to `deadline` and refreshes its waker;
    /// `false` if it is gone (it fired). A later deadline is only noted in
    /// the entry: the entry stays where it is, and when the wheel reaches it
    /// it goes on to the slot of its new deadline (see [`expired`]), as a
    /// timer cascading from a higher level does. An earlier one, or a timer
    /// already on the due list, moves now.
    ///
    /// [`expired`]: Self::expired
    fn retarget(&mut self, key: TimerKey, deadline: Instant, waker: &Waker) -> bool {
        let at = self.place(deadline);
        let Some(entry) = self.entries.get_mut(key.0) else {
            return false;
        };
        if entry.uid != key.1 {
            return false;
        }
        if !entry.waker.will_wake(waker) {
            entry.waker = waker.clone();
        }
        if at >= entry.at && entry.level != DUE {
            // Every bound kept on the slot it is in (`earliest`) stays a
            // time no later than the earliest deadline in it.
            entry.at = at;
            return true;
        }
        if at != entry.at {
            self.unlink(key.0);
            self.entries.get_mut(key.0).expect("timer entry").at = at;
            self.link(key.0);
        }
        true
    }

    /// The wakers of every armed [`Alarm`], to wake at shutdown.
    pub(crate) fn alarm_wakers(&self) -> Vec<Waker> {
        self.entries
            .iter()
            .filter(|entry| entry.alarm)
            .map(|entry| entry.waker.clone())
            .collect()
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

    #[inline(always)]
    fn link(&mut self, idx: usize) {
        let at = self.entries.get(idx).expect("timer entry").at;
        let when = tick_of(at);
        if when <= self.elapsed {
            // Its millisecond has come: it waits for the exact deadline.
            let head = self.due;
            {
                let entry = self.entries.get_mut(idx).expect("timer entry");
                entry.level = DUE;
                entry.prev = NIL;
                entry.next = head;
            }
            if head != NIL {
                self.entries.get_mut(head).expect("timer entry").prev = idx;
            }
            self.due = idx;
            self.due_at = self.due_at.min(at);
            return;
        }
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
        if level == 0 {
            // Every timer in a slot of level 0 has the same millisecond.
            self.earliest[slot] = if head == NIL {
                at
            } else {
                self.earliest[slot].min(at)
            };
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
        } else if level == DUE as usize {
            self.due = next;
            if next == NIL {
                self.due_at = u64::MAX;
            }
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

    /// Gives back the timer table a burst of timers left behind, once it
    /// is mostly vacant (see [`Slab::trim`]): for the event loop, before
    /// it sleeps.
    /// Returns whether the table may need it again (see
    /// [`Slab::trim_armed`]).
    pub(crate) fn trim(&mut self) -> bool {
        if self.entries.trim_due() {
            self.entries.trim();
        }
        self.entries.trim_armed()
    }

    /// See [`Slab::watch_growth`].
    ///
    /// # Safety
    ///
    /// As for [`Slab::watch_growth`].
    pub(crate) unsafe fn watch_growth(&mut self, flag: &std::cell::Cell<u8>, bit: u8) {
        // SAFETY: passed on from the caller.
        unsafe { self.entries.watch_growth(flag, bit) };
    }

    /// When the event loop must wake up next for timers: at the earliest
    /// deadline itself, not at the start of its millisecond, unless timers
    /// fired less than a tick ago (see `quiet_until`).
    #[inline]
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let wheel = match self.next_expiration() {
            Some((0, slot, _)) => self.earliest[slot],
            // A slot to cascade, at the start of its millisecond.
            Some((_, _, tick)) => tick * NANOS_PER_TICK as u64,
            None if self.due == NIL => return None,
            None => u64::MAX,
        };
        let at = wheel.min(self.due_at).max(self.quiet_until);
        Some(self.start + Duration::from_nanos(at))
    }

    /// Advances the wheel to `now` and returns the wakers of all due timers:
    /// those whose deadline is not after `now`.
    pub(crate) fn expired(&mut self, now: Instant) -> Vec<Waker> {
        let now_ns = self.nanos(now);
        let now_tick = tick_of(now_ns);
        let mut fired = Vec::new();
        if self.due_at <= now_ns {
            let mut idx = self.due;
            let mut earliest = u64::MAX;
            while idx != NIL {
                let (next, at) = {
                    let entry = self.entries.get(idx).expect("timer entry");
                    (entry.next, entry.at)
                };
                if at <= now_ns {
                    self.unlink(idx);
                    fired.push(self.entries.remove(idx).expect("timer entry").waker);
                } else {
                    earliest = earliest.min(at);
                }
                idx = next;
            }
            self.due_at = earliest;
        }
        while let Some((level, slot, deadline)) = self.next_expiration() {
            if deadline > now_tick {
                break;
            }
            let mut idx = std::mem::replace(&mut self.levels[level].heads[slot], NIL);
            self.levels[level].occupied &= !(1 << slot);
            self.elapsed = deadline;
            while idx != NIL {
                let (next, at) = {
                    let entry = self.entries.get(idx).expect("timer entry");
                    (entry.next, entry.at)
                };
                // Only a timer of this slot's own millisecond fires here. The
                // others go down a level and fire as the loop reaches their
                // slots, so that timers fire in the order of their deadlines
                // even when the loop wakes up late, past several of them.
                if tick_of(at) <= deadline && at <= now_ns {
                    fired.push(self.entries.remove(idx).expect("timer entry").waker);
                } else {
                    // Cascade to a lower level, now that time has moved on,
                    // or, if its millisecond has come, to the due list.
                    self.link(idx);
                }
                idx = next;
            }
        }
        if now_tick > self.elapsed {
            self.elapsed = now_tick;
        }
        if !fired.is_empty() {
            self.quiet_until = now_ns.saturating_add(NANOS_PER_TICK as u64);
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
        self.due = NIL;
        self.due_at = u64::MAX;
        self.quiet_until = 0;
        self.entries
            .take_all()
            .into_iter()
            .map(|entry| entry.waker)
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The timer table's length in slots, occupied or not.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> usize {
        self.entries.slots()
    }
}

/// Future returned by [`sleep`] and [`sleep_until`].
///
/// Unlike giong's other futures, `Sleep` is `Send`, and so is a
/// [`Timeout`] around a `Send` future, so that code which needs `Send`
/// futures (through `giong-compat`, say) can use giong's timers. This is
/// part of the API and stays so. A `Sleep` polled on one worker and then on
/// another arms a timer on the second; the first worker's timer fires later
/// and wakes nothing. It must be polled inside a giong runtime.
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
        // The worker's clock of this turn of its loop: one clock read shared
        // by every timer polled in the turn. It is never ahead of the real
        // time, so a sleep never ends early. A deadline within a tick of it
        // may already have passed in real time (a zero timeout, a sleep
        // polled late in a long turn): those check the real clock, so they
        // end now rather than a tick later. So does a first poll: the turn
        // may have run for longer than a tick since the clock was read, and
        // a sleep made in it (a zero timeout) must not wait for the next one.
        let now = runtime::try_with_worker(|w| {
            let now = w.clock();
            if self.deadline > now && (self.key.is_none() || self.deadline - now <= TICK) {
                w.refresh_clock()
            } else {
                now
            }
        })
        .unwrap_or_else(Instant::now);
        if now >= self.deadline {
            // A sleep already over when first polled ends without the task
            // having waited, so it counts against the task's budget, as an
            // immediate send does: a loop of such sleeps (short ones, polled
            // late) still lets the other tasks and the event loop run.
            if self.key.is_none() && !crate::driver::spend_budget() {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
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
///
/// Never early, and late by about the kernel's wake-up time. Right after
/// other timers of the worker fired, by up to a millisecond: a loop of
/// sleeps shorter than 1 ms takes about 1 ms a turn (see the [module
/// documentation](self)).
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

/// The worker's clock of this turn of its event loop: read at most once a
/// turn and shared by everything that asks during it, so it is behind the
/// real time by as long as the turn has run so far, and never ahead.
/// [`Instant::now`] outside a runtime. For the crates of this workspace
/// (giong-http's `Date` header), not part of the API.
#[doc(hidden)]
#[inline]
pub fn turn_clock() -> Instant {
    runtime::try_with_worker(|w| w.clock()).unwrap_or_else(Instant::now)
}

/// What an [`Alarm`] went off for.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rang {
    /// The deadline passed.
    Deadline,
    /// The runtime is shutting down, or the worker is draining its tasks
    /// (as [`shutdown_signal`](crate::shutdown_signal) resolves).
    Shutdown,
}

/// A deadline and a shutdown signal in one registration, for something
/// that waits again and again with a deadline that moves: a connection's
/// idle and request timeouts in giong-http. A [`timeout`] around every wait
/// arms a timer and cancels it, and an [`until_shutdown`](crate::until_shutdown)
/// registers and unregisters a waker; an `Alarm` stays armed from one wait
/// to the next, and a later deadline only changes the deadline its entry in
/// the timer wheel keeps (the wheel moves the entry when it reaches it).
/// 16 bytes. Not `Send`: armed on one worker. For the crates of this
/// workspace, not part of the API.
#[doc(hidden)]
pub struct Alarm {
    /// The registration in the worker's timer wheel: its index, the low 32
    /// bits of the worker's uid (as [`Sleep`] keeps the whole uid, for an
    /// alarm polled on a worker of another runtime of the same thread), and
    /// its uid in the wheel, 0 for none.
    idx: u32,
    worker: u32,
    uid: u64,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Default for Alarm {
    fn default() -> Self {
        Self::new()
    }
}

impl Alarm {
    /// An alarm not armed yet.
    pub const fn new() -> Self {
        Self {
            idx: 0,
            worker: 0,
            uid: 0,
            _not_send: std::marker::PhantomData,
        }
    }

    /// `Ready` once the runtime shuts down (checked first) or `deadline`
    /// has passed; otherwise arms the alarm for `deadline` with the task's
    /// waker, which is also woken at shutdown. Never early: the deadline is
    /// compared with the worker's clock of the turn, which is never ahead
    /// (a deadline it has not reached yet but the real clock has is armed,
    /// and goes off when the worker fires its timers after this turn).
    ///
    /// # Panics
    ///
    /// Outside of a giong runtime.
    pub fn poll_until(&mut self, deadline: Instant, cx: &mut Context<'_>) -> Poll<Rang> {
        self.poll(deadline, true, cx)
    }

    /// As [`poll_until`](Self::poll_until), without the shutdown: for the
    /// wait that goes on after one, which then ends only at `deadline`.
    pub fn poll_deadline(&mut self, deadline: Instant, cx: &mut Context<'_>) -> Poll<Rang> {
        self.poll(deadline, false, cx)
    }

    #[inline]
    fn poll(&mut self, deadline: Instant, shutdown: bool, cx: &mut Context<'_>) -> Poll<Rang> {
        runtime::with_worker(|w| {
            // As `ShutdownSignal`: the flag is set before the wake-up is
            // handed to this worker, which wakes armed alarms only after this
            // returns, so an alarm armed here cannot miss it.
            if shutdown && w.shutting_down() {
                return Poll::Ready(Rang::Shutdown);
            }
            let now = w.clock();
            let worker = w.shared.uid as u32;
            let mut timers = w.timers.borrow_mut();
            let armed = self.uid != 0 && self.worker == worker;
            if now >= deadline {
                if armed {
                    drop(timers.remove((self.idx as usize, self.uid)));
                }
                self.uid = 0;
                return Poll::Ready(Rang::Deadline);
            }
            if armed && timers.retarget((self.idx as usize, self.uid), deadline, cx.waker()) {
                return Poll::Pending;
            }
            let (idx, uid) = timers.insert_entry(deadline, cx.waker().clone(), true);
            self.idx = u32::try_from(idx).expect("fewer than 2^32 timers");
            self.worker = worker;
            self.uid = uid;
            Poll::Pending
        })
    }
}

impl Drop for Alarm {
    fn drop(&mut self) {
        if self.uid == 0 {
            return;
        }
        let key = (self.idx as usize, self.uid);
        let worker = self.worker;
        let waker = runtime::try_with_worker(|w| {
            if w.shared.uid as u32 == worker {
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

impl fmt::Debug for Alarm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Alarm")
            .field("armed", &(self.uid != 0))
            .finish()
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
        // SAFETY: `fut` is structurally pinned (see above).
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
        run_model_with(seed, steps, max_delay_ms, max_step_us, false)
    }

    /// As `run_model`, and with `retarget`, also moves live timers to new
    /// deadlines, later or earlier, as an `Alarm` does.
    fn run_model_with(
        seed: u64,
        steps: usize,
        max_delay_ms: u64,
        max_step_us: u64,
        retarget: bool,
    ) {
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let mut rng = Rng(seed);
        let fired = Arc::new(Mutex::new(Vec::new()));
        // id -> (deadline in ns since start, wheel key)
        let mut live: BTreeMap<u64, (u128, TimerKey)> = BTreeMap::new();
        let mut now_ns: u128 = 0;
        let mut next_id = 0u64;
        // A tick after timers last fired: the loop may wait until then.
        let mut quiet_until: u128 = 0;

        for _ in 0..steps {
            match rng.below(if retarget { 12 } else { 10 }) {
                10..=11 => {
                    if !live.is_empty() {
                        let nth = rng.below(live.len() as u64) as usize;
                        let id = *live.keys().nth(nth).unwrap();
                        let delay_ns = rng.below(max_delay_ms * 1_000_000) as u128 + 1;
                        let deadline_ns = now_ns + delay_ns;
                        let waker = Waker::from(Arc::new(Recorder(id, fired.clone())));
                        let entry = live.get_mut(&id).unwrap();
                        let deadline = start + Duration::from_nanos(deadline_ns as u64);
                        assert!(
                            wheel.retarget(entry.1, deadline, &waker),
                            "seed {seed}: live timer {id} could not be moved"
                        );
                        entry.0 = deadline_ns;
                    }
                }
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
                    // In the order of their deadlines, to the millisecond.
                    let ticks: Vec<u128> =
                        got.iter().map(|id| live[id].0 / NANOS_PER_TICK).collect();
                    assert!(
                        ticks.windows(2).all(|w| w[0] <= w[1]),
                        "seed {seed}: timers fired out of order at {now_ns} ns: {ticks:?}"
                    );
                    got.sort_unstable();
                    // A timer is due once its deadline has passed, to the
                    // nanosecond.
                    let mut expected: Vec<u64> = live
                        .iter()
                        .filter(|(_, (deadline_ns, _))| *deadline_ns <= now_ns)
                        .map(|(id, _)| *id)
                        .collect();
                    expected.sort_unstable();
                    assert_eq!(
                        got, expected,
                        "seed {seed}: wrong set of timers fired at {now_ns} ns"
                    );
                    if !got.is_empty() {
                        quiet_until = now_ns + NANOS_PER_TICK;
                    }
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
                    // At the earliest deadline, or a tick after timers last
                    // fired if that is later: never later than both.
                    let wake_ns = wake.duration_since(start).as_nanos();
                    assert!(
                        wake_ns <= earliest_ns.max(quiet_until),
                        "seed {seed}: would oversleep"
                    );
                    assert!(wake_ns > now_ns, "seed {seed}: wake-up in the past");
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
    #[cfg_attr(miri, ignore = "safe code only, and minutes long under Miri")]
    fn wheel_matches_model_short_timers() {
        for seed in 1..=40 {
            run_model(seed, 4_000, 200, 3_000);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "safe code only, and minutes long under Miri")]
    fn wheel_matches_model_mixed_timers() {
        // Delays up to 10 minutes with steps up to 5 s: exercises cascading
        // across four levels.
        for seed in 100..=130 {
            run_model(seed, 4_000, 600_000, 5_000_000);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "safe code only, and minutes long under Miri")]
    fn wheel_matches_model_long_timers() {
        // Delays up to 30 days, steps up to 2 hours.
        for seed in 200..=215 {
            run_model(seed, 3_000, 30 * 86_400_000, 7_200_000_000);
        }
    }

    #[test]
    fn timers_fire_in_deadline_order_when_the_loop_wakes_late() {
        // Deadlines a few hundred milliseconds out share a slot of a coarse
        // level. A loop that wakes up past all of them (as on Windows, whose
        // waits can end late by a scheduler quantum) must still fire them in
        // order.
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let fired = Arc::new(Mutex::new(Vec::new()));
        for ms in [290, 260, 280, 270, 300] {
            let waker = Waker::from(Arc::new(Recorder(ms, fired.clone())));
            wheel.insert(start + Duration::from_millis(ms), waker);
        }
        for late in [275, 400] {
            for waker in wheel.expired(start + Duration::from_millis(late)) {
                waker.wake();
            }
        }
        assert_eq!(*fired.lock().unwrap(), [260, 270, 280, 290, 300]);
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

    #[test]
    #[cfg_attr(miri, ignore = "safe code only, and minutes long under Miri")]
    fn wheel_matches_model_with_moved_timers() {
        for seed in 300..=330 {
            run_model_with(seed, 4_000, 200, 3_000, true);
        }
        for seed in 400..=420 {
            run_model_with(seed, 4_000, 600_000, 5_000_000, true);
        }
    }

    #[test]
    fn a_timer_moved_later_stays_put_and_fires_at_its_new_deadline() {
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let fired = Arc::new(Mutex::new(Vec::new()));
        let waker = Waker::from(Arc::new(Recorder(1, fired.clone())));
        let at = |ms| start + Duration::from_millis(ms);
        let key = wheel.insert(at(100), waker.clone());
        assert!(wheel.retarget(key, at(300), &waker));
        // Still where it was: the wheel may wake up for it, but it does not
        // fire until its new deadline.
        assert!(wheel.next_deadline().unwrap() <= at(300));
        assert!(wheel.expired(at(150)).is_empty());
        assert!(wheel.expired(at(299)).is_empty());
        assert!(wheel.next_deadline().unwrap() <= at(300));
        assert_eq!(wheel.expired(at(300)).len(), 1);
        assert!(
            !wheel.retarget(key, at(400), &waker),
            "a fired timer cannot be moved"
        );
        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn a_timer_moved_earlier_fires_at_its_new_deadline() {
        let start = Instant::now();
        let mut wheel = Timers::starting_at(start);
        let fired = Arc::new(Mutex::new(Vec::new()));
        let waker = Waker::from(Arc::new(Recorder(1, fired.clone())));
        let at = |ms| start + Duration::from_millis(ms);
        let key = wheel.insert(at(60_000), waker.clone());
        assert!(wheel.retarget(key, at(20), &waker));
        assert!(wheel.next_deadline().unwrap() <= at(20));
        assert!(wheel.expired(at(19)).is_empty());
        assert_eq!(wheel.expired(at(20)).len(), 1);
        let key = wheel.insert(at(30), waker.clone());
        assert!(wheel.remove(key).is_some());
        assert!(
            !wheel.retarget(key, at(40), &waker),
            "a removed timer cannot be moved"
        );
    }

    #[test]
    fn a_dropped_alarm_leaves_no_timer() {
        crate::block_on(async {
            let timers = || runtime::with_worker(|w| w.timers.borrow().len());
            let before = timers();
            let mut alarm = Alarm::new();
            std::future::poll_fn(|cx| {
                assert!(alarm
                    .poll_until(Instant::now() + Duration::from_secs(60), cx)
                    .is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(timers(), before + 1);
            // Moved, not armed again.
            std::future::poll_fn(|cx| {
                assert!(alarm
                    .poll_until(Instant::now() + Duration::from_secs(90), cx)
                    .is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(timers(), before + 1);
            drop(alarm);
            assert_eq!(timers(), before);
        });
    }
}
