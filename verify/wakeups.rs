//! Verus proof that a worker never forgets a task: every wake-up of a live task
//! is followed by a poll of that task that sees it, for any number of tasks,
//! waking threads, queued entries and steps, under the fairness the scheduler
//! gives threads (a thread that can run eventually runs). See docs/verus.md.
//!
//! This is a model, not the code: the protocol of `src/notify.rs` (`RunFlag`,
//! `Doorbell`), `TaskWaker::wake_by_ref`, `Worker::run_batch` and
//! `Worker::drive`'s sleep, and `WorkerShared::inject` / `drain_injected`, one
//! atomic step per atomic operation, under sequential consistency (loom checks
//! the same handshakes under the C++ memory model, for small bounds).
//! `verify/pins.txt` pins each function the model describes: a change to one
//! fails `scripts/verus.py drift` until the model has been checked against it.

#![allow(unused_imports, dead_code)]

use vstd::prelude::*;

verus! {

// ---------------------------------------------------------------------------
// The model.
// ---------------------------------------------------------------------------

/// Where the worker thread is in `Worker::drive` / `run_batch`.
pub enum Pc {
    /// Top of the event loop: `drain_injected` calls `Doorbell::take`.
    Loop,
    /// `take` returned true: about to take the injector's contents.
    Draining,
    /// `run_batch` reads its budget (the queue's length).
    BatchStart,
    /// `run_batch` with `left` polls of its budget left.
    Batch { left: nat },
    /// `RunFlag::begin_poll` done for task `t`: its poll reads the task's state.
    Polling { t: nat, left: nat },
    /// The batch is over: more runnable tasks, or prepare to sleep.
    AfterBatch,
    /// `Doorbell::prepare_park`: the `sleeping` swap.
    ParkSet,
    /// `prepare_park` (after its fence) reads `pending`.
    ParkRead,
    /// Blocked in the kernel until the eventfd is written.
    Parked,
    /// `Doorbell::unpark`.
    Unpark,
}

/// Where a thread that wakes a task from outside the worker is, in
/// `TaskWaker::wake_by_ref` and `WorkerShared::inject` / `Doorbell::ring`.
pub enum WPc {
    Idle,
    /// Published the task's new state; about to call `RunFlag::try_enqueue`.
    Published { t: nat },
    /// `try_enqueue` returned true: about to push the task into the injector.
    Inject { t: nat },
    /// Pushed: `ring` swaps `pending` to true.
    Ring,
    /// After the fence, `ring` loads `sleeping`.
    RingCheck,
    /// `sleeping` looked true: `ring` swaps it to false.
    RingSwap,
    /// The swap elected this thread: it writes the eventfd.
    Write,
}

pub struct Task {
    /// Not finished.
    pub live: bool,
    /// `RunFlag`.
    pub queued: bool,
    /// How many wake-ups (state changes the task waits for) were published.
    pub events: nat,
    /// `events` as the task's last poll read it.
    pub seen: nat,
}

pub struct State {
    pub tasks: Map<nat, Task>,
    pub next_id: nat,
    /// The worker's run queue (`Worker::queue`).
    pub q: Seq<nat>,
    /// `WorkerShared::injector` (wake-ups only).
    pub inj: Seq<nat>,
    /// `Doorbell::pending`.
    pub pending: bool,
    /// `Doorbell::sleeping`.
    pub sleeping: bool,
    /// The eventfd's counter.
    pub efd: nat,
    pub pc: Pc,
    pub wakers: Map<nat, WPc>,
}

pub enum Action {
    /// The worker's next step. `choice` is what the model leaves open: whether
    /// a poll finishes its task; whether, with nothing runnable after a
    /// batch, the worker goes round again instead of preparing to sleep (the
    /// spin loop, a probe or a timer found work: `spin_for_work`,
    /// `before_park`); whether, having announced its sleep, it polls instead
    /// (`jobs_len.any()`: a balanced job waits).
    Worker { choice: bool },
    /// The kernel returns from a sleep for another reason (I/O, a timer).
    Spurious,
    /// A wake-up on the worker's own thread (a poll, a timer, I/O):
    /// `wake_by_ref` with the local run queue.
    LocalWake { u: nat },
    /// `spawn` on the worker's own thread.
    Spawn,
    /// The next step of waking thread `w`; from `Idle`, it publishes a new
    /// state for task `t` and starts waking it.
    Waker { w: nat, t: nat },
    Stutter,
}

pub open spec fn live(s: State, t: nat) -> bool {
    s.tasks.dom().contains(t) && s.tasks[t].live
}

pub open spec fn wpc(s: State, w: nat) -> WPc {
    if s.wakers.dom().contains(w) {
        s.wakers[w]
    } else {
        WPc::Idle
    }
}

pub open spec fn init(s: State) -> bool {
    &&& s.tasks == Map::<nat, Task>::empty()
    &&& s.next_id == 0
    &&& s.q == Seq::<nat>::empty()
    &&& s.inj == Seq::<nat>::empty()
    &&& !s.pending
    &&& !s.sleeping
    &&& s.efd == 0
    &&& s.pc == Pc::Loop
    &&& s.wakers == Map::<nat, WPc>::empty()
}

/// The worker can take a step: everywhere but asleep with nothing written.
pub open spec fn worker_enabled(s: State) -> bool {
    !(s.pc is Parked) || s.efd > 0
}

/// The worker runs code (not inside `prepare_park` or the kernel): where its
/// own thread may wake or spawn tasks.
pub open spec fn running(s: State) -> bool {
    !(s.pc is ParkSet) && !(s.pc is ParkRead) && !(s.pc is Parked)
}

pub open spec fn worker_next(s: State, choice: bool) -> State {
    match s.pc {
        // drain_injected: `if !self.shared.bell.take() { return; }`
        Pc::Loop => if s.pending {
            State { pending: false, pc: Pc::Draining, ..s }
        } else {
            State { pc: Pc::BatchStart, ..s }
        },
        // `mem::take(&mut *lock(&self.shared.injector))`, each pushed back.
        Pc::Draining => State { q: s.q + s.inj, inj: Seq::empty(), pc: Pc::BatchStart, ..s },
        // `let budget = self.queue.borrow().len();`
        Pc::BatchStart => State { pc: Pc::Batch { left: s.q.len() }, ..s },
        Pc::Batch { left } => if left == 0 || s.q.len() == 0 {
            State { pc: Pc::AfterBatch, ..s }
        } else {
            let u = s.q[0];
            let q = s.q.drop_first();
            if live(s, u) {
                // `task.state.clear_queued()` before the poll.
                State {
                    tasks: s.tasks.insert(u, Task { queued: false, ..s.tasks[u] }),
                    q,
                    pc: Pc::Polling { t: u, left: (left - 1) as nat },
                    ..s
                }
            } else {
                // A stale wake-up for a finished task.
                State { q, pc: Pc::Batch { left: (left - 1) as nat }, ..s }
            }
        },
        // The poll reads the task's state; it may finish.
        Pc::Polling { t, left } => State {
            tasks: s.tasks.insert(
                t,
                Task { seen: s.tasks[t].events, live: s.tasks[t].live && !choice, ..s.tasks[t] },
            ),
            pc: Pc::Batch { left },
            ..s
        },
        // `if !self.queue.borrow().is_empty() { ...; continue; }`, or the
        // spin loop or `before_park` found work.
        Pc::AfterBatch => if s.q.len() > 0 || choice {
            State { pc: Pc::Loop, ..s }
        } else {
            State { pc: Pc::ParkSet, ..s }
        },
        // prepare_park: `self.sleeping.swap(true)`, fence,
        Pc::ParkSet => State { sleeping: true, pc: Pc::ParkRead, ..s },
        // `self.pending.load()`: work arrived, do not sleep (nor with a
        // balanced job waiting).
        Pc::ParkRead => if s.pending || choice {
            State { pc: Pc::Unpark, ..s }
        } else {
            State { pc: Pc::Parked, ..s }
        },
        // The eventfd read completes.
        Pc::Parked => State { efd: 0, pc: Pc::Unpark, ..s },
        // `self.shared.bell.unpark()`
        Pc::Unpark => State { sleeping: false, pc: Pc::Loop, ..s },
    }
}

/// `wake_by_ref` on the worker's thread: publish, `try_enqueue`, and on true
/// push onto the local run queue.
pub open spec fn local_wake(s: State, u: nat) -> State {
    if live(s, u) {
        let tk = s.tasks[u];
        State {
            tasks: s.tasks.insert(u, Task { events: tk.events + 1, queued: true, ..tk }),
            q: if tk.queued {
                s.q
            } else {
                s.q.push(u)
            },
            ..s
        }
    } else {
        s
    }
}

/// `spawn`: a new task, queued, with one wake-up to see (its first poll).
pub open spec fn spawn(s: State) -> State {
    State {
        tasks: s.tasks.insert(s.next_id, Task { live: true, queued: true, events: 1, seen: 0 }),
        next_id: s.next_id + 1,
        q: s.q.push(s.next_id),
        ..s
    }
}

pub open spec fn waker_next(s: State, w: nat, t: nat) -> State {
    match wpc(s, w) {
        WPc::Idle => if live(s, t) {
            State {
                tasks: s.tasks.insert(t, Task { events: s.tasks[t].events + 1, ..s.tasks[t] }),
                wakers: s.wakers.insert(w, WPc::Published { t }),
                ..s
            }
        } else {
            s
        },
        // `if !self.queued.try_enqueue() { return; }`: a swap.
        WPc::Published { t } => if s.tasks.dom().contains(t) && !s.tasks[t].queued {
            State {
                tasks: s.tasks.insert(t, Task { queued: true, ..s.tasks[t] }),
                wakers: s.wakers.insert(w, WPc::Inject { t }),
                ..s
            }
        } else {
            State {
                tasks: if s.tasks.dom().contains(t) {
                    s.tasks.insert(t, Task { queued: true, ..s.tasks[t] })
                } else {
                    s.tasks
                },
                wakers: s.wakers.insert(w, WPc::Idle),
                ..s
            }
        },
        // `queue.push(item)` under the injector's lock.
        WPc::Inject { t } => State {
            inj: s.inj.push(t),
            wakers: s.wakers.insert(w, WPc::Ring),
            ..s
        },
        // ring: `self.pending.swap(true)`, fence,
        WPc::Ring => State { pending: true, wakers: s.wakers.insert(w, WPc::RingCheck), ..s },
        // `self.sleeping.load(Relaxed) && ...`
        WPc::RingCheck => if s.sleeping {
            State { wakers: s.wakers.insert(w, WPc::RingSwap), ..s }
        } else {
            State { wakers: s.wakers.insert(w, WPc::Idle), ..s }
        },
        // `... && self.sleeping.swap(false, Relaxed)`: elects one writer.
        WPc::RingSwap => if s.sleeping {
            State { sleeping: false, wakers: s.wakers.insert(w, WPc::Write), ..s }
        } else {
            State { wakers: s.wakers.insert(w, WPc::Idle), ..s }
        },
        // `wake.ring()`: writes the eventfd.
        WPc::Write => State { efd: s.efd + 1, wakers: s.wakers.insert(w, WPc::Idle), ..s },
    }
}

pub open spec fn next(s: State, a: Action, s2: State) -> bool {
    match a {
        Action::Worker { choice } => worker_enabled(s) && s2 == worker_next(s, choice),
        Action::Spurious => s.pc is Parked && s2 == State { pc: Pc::Unpark, ..s },
        Action::LocalWake { u } => running(s) && s2 == local_wake(s, u),
        Action::Spawn => running(s) && s2 == spawn(s),
        Action::Waker { w, t } => s2 == waker_next(s, w, t),
        Action::Stutter => s2 == s,
    }
}

// ---------------------------------------------------------------------------
// Safety: the invariant.
// ---------------------------------------------------------------------------

/// Someone will put task `t` in the run queue, or it is in one.
pub open spec fn in_flight(s: State, t: nat) -> bool {
    s.q.contains(t) || s.inj.contains(t) || some_waker(s, WPc::Inject { t })
}

pub open spec fn polling(s: State, t: nat) -> bool {
    s.pc matches Pc::Polling { t: u, .. } && u == t
}

/// A wake-up the task has not seen will be seen by a poll: the task is
/// queued, a waker is about to `try_enqueue` it, or it is being polled.
pub open spec fn covered(s: State, t: nat) -> bool {
    s.tasks[t].queued || some_waker(s, WPc::Published { t }) || polling(s, t)
}

pub open spec fn some_waker(s: State, p: WPc) -> bool {
    exists|w: nat| #[trigger] wpc(s, w) == p
}

/// Every task named anywhere exists.
pub open spec fn inv_ids(s: State) -> bool {
    &&& forall|t: nat| #[trigger] s.tasks.dom().contains(t) <==> t < s.next_id
    &&& forall|k: int| 0 <= k < s.q.len() ==> #[trigger] s.q[k] < s.next_id
    &&& forall|k: int| 0 <= k < s.inj.len() ==> #[trigger] s.inj[k] < s.next_id
    &&& forall|w: nat| #[trigger] wpc(s, w) matches WPc::Published { t } ==> t < s.next_id
    &&& forall|w: nat| #[trigger] wpc(s, w) matches WPc::Inject { t } ==> t < s.next_id
    &&& s.pc matches Pc::Polling { t, .. } ==> t < s.next_id
}

/// A task's poll never saw more wake-ups than were published.
pub open spec fn inv_seen(s: State) -> bool {
    forall|t: nat| #[trigger] s.tasks.dom().contains(t) ==> s.tasks[t].seen <= s.tasks[t].events
}

/// RunFlag: a queued task is in a queue or on its way there.
pub open spec fn inv_queued(s: State) -> bool {
    forall|t: nat| #[trigger] live(s, t) && s.tasks[t].queued ==> in_flight(s, t)
}

/// RunFlag and begin_poll: no unseen wake-up is left uncovered.
pub open spec fn inv_covered(s: State) -> bool {
    forall|t: nat| #[trigger] live(s, t) && s.tasks[t].seen < s.tasks[t].events ==> covered(s, t)
}

/// Doorbell: injected work is announced, or about to be.
pub open spec fn inv_inj(s: State) -> bool {
    s.inj.len() > 0 ==> s.pending || s.pc is Draining || some_waker(s, WPc::Ring)
}

/// Doorbell: a worker about to sleep, or asleep, is woken by the eventfd.
pub open spec fn inv_sleep(s: State) -> bool {
    &&& (s.pc is ParkRead || s.pc is Parked) ==> s.sleeping || s.efd > 0 || some_waker(s, WPc::Write)
    &&& (s.pc is Parked && s.pending) ==> s.efd > 0 || some_waker(s, WPc::RingCheck)
        || some_waker(s, WPc::RingSwap) || some_waker(s, WPc::Write)
}

/// The worker sleeps only with nothing runnable.
pub open spec fn inv_idle(s: State) -> bool {
    !running(s) ==> s.q.len() == 0
}

pub open spec fn inv(s: State) -> bool {
    &&& inv_ids(s)
    &&& inv_seen(s)
    &&& inv_queued(s)
    &&& inv_covered(s)
    &&& inv_inj(s)
    &&& inv_sleep(s)
    &&& inv_idle(s)
}

proof fn lemma_init(s: State)
    requires
        init(s),
    ensures
        inv(s),
{
}

/// A step that changes no waker keeps every `some_waker`.
proof fn lemma_same_wakers(s: State, s2: State)
    requires
        s2.wakers == s.wakers,
    ensures
        forall|w: nat| #[trigger] wpc(s2, w) == wpc(s, w),
        forall|p: WPc| #[trigger] some_waker(s2, p) == some_waker(s, p),
{
    assert forall|p: WPc| #[trigger] some_waker(s2, p) == some_waker(s, p) by {
        if some_waker(s, p) {
            let w = choose|w: nat| #[trigger] wpc(s, w) == p;
            assert(wpc(s2, w) == p);
        }
        if some_waker(s2, p) {
            let w = choose|w: nat| #[trigger] wpc(s2, w) == p;
            assert(wpc(s, w) == p);
        }
    }
}

/// A step of waker `w` (to `x`) keeps every `some_waker` that another waker
/// witnesses.
proof fn lemma_one_waker(s: State, s2: State, w: nat, x: WPc)
    requires
        s2.wakers == s.wakers.insert(w, x),
    ensures
        wpc(s2, w) == x,
        forall|v: nat| v != w ==> #[trigger] wpc(s2, v) == wpc(s, v),
        some_waker(s2, x),
        forall|p: WPc| #[trigger] some_waker(s, p) && p != wpc(s, w) ==> some_waker(s2, p),
        forall|p: WPc| #[trigger] some_waker(s2, p) && p != x ==> some_waker(s, p),
{
    assert(wpc(s2, w) == x);
    assert forall|p: WPc| #[trigger] some_waker(s, p) && p != wpc(s, w) implies some_waker(s2, p) by {
        let v = choose|v: nat| wpc(s, v) == p;
        assert(v != w);
        assert(wpc(s2, v) == p);
    }
    assert forall|p: WPc| #[trigger] some_waker(s2, p) && p != x implies some_waker(s, p) by {
        let v = choose|v: nat| wpc(s2, v) == p;
        assert(v != w);
        assert(wpc(s, v) == p);
    }
}

/// A waker's step changes no other waker.
proof fn lemma_waker_frame(s: State, w: nat, t: nat)
    ensures
        forall|v: nat| v != w ==> #[trigger] wpc(waker_next(s, w, t), v) == wpc(s, v),
{
    let s2 = waker_next(s, w, t);
    if s2.wakers == s.wakers {
        lemma_same_wakers(s, s2);
    } else {
        lemma_one_waker(s, s2, w, wpc(s2, w));
    }
}

/// Every step keeps the invariant.
proof fn lemma_next(s: State, a: Action, s2: State)
    requires
        inv(s),
        next(s, a, s2),
    ensures
        inv(s2),
{
    match a {
        Action::Worker { choice } => lemma_worker(s, choice),
        Action::Spurious => {
            lemma_same_wakers(s, s2);
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                assert(covered(s, t));
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                assert(live(s, t) && s.tasks[t].queued);
                assert(in_flight(s, t));
            }
        },
        Action::LocalWake { u } => lemma_local_wake(s, u),
        Action::Spawn => lemma_spawn(s),
        Action::Waker { w, t } => lemma_waker(s, w, t),
        Action::Stutter => {},
    }
}

proof fn lemma_worker(s: State, choice: bool)
    requires
        inv(s),
        worker_enabled(s),
    ensures
        inv(worker_next(s, choice)),
{
    let s2 = worker_next(s, choice);
    lemma_same_wakers(s, s2);
    match s.pc {
        Pc::Draining => {
            assert forall|t: nat| #[trigger] in_flight(s, t) implies in_flight(s2, t) by {
                if s.q.contains(t) {
                    let k = choose|k: int| 0 <= k < s.q.len() && s.q[k] == t;
                    assert(s2.q[k] == t);
                } else if s.inj.contains(t) {
                    let k = choose|k: int| 0 <= k < s.inj.len() && s.inj[k] == t;
                    assert(s2.q[s.q.len() + k] == t);
                }
            }
            assert forall|k: int| 0 <= k < s2.q.len() implies #[trigger] s2.q[k] < s2.next_id by {
                if k < s.q.len() {
                    assert(s2.q[k] == s.q[k]);
                } else {
                    assert(s2.q[k] == s.inj[k - s.q.len()]);
                }
            }
        },
        Pc::Batch { left } => {
            if left == 0 || s.q.len() == 0 {
            } else {
                let u = s.q[0];
                assert forall|k: int| 0 <= k < s2.q.len() implies #[trigger] s2.q[k] < s2.next_id by {
                    assert(s2.q[k] == s.q[k + 1]);
                }
                assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                    assert(live(s, t) && s.tasks[t].queued);
                    assert(in_flight(s, t));
                    if s.q.contains(t) {
                        let k = choose|k: int| 0 <= k < s.q.len() && s.q[k] == t;
                        if k == 0 {
                            assert(t == u);
                        } else {
                            assert(s2.q[k - 1] == t);
                        }
                    }
                }
                assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                    if t != u {
                        assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                        assert(covered(s, t));
                    }
                }
            }
        },
        Pc::Polling { t: u, left } => {
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                assert(t != u);
                assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                assert(covered(s, t));
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                assert(live(s, t) && s.tasks[t].queued);
                assert(in_flight(s, t));
            }
        },
        _ => {},
    }
    if s2.tasks == s.tasks && s2.q == s.q && s2.inj == s.inj && !(s.pc is Polling) {
        // Only the worker's own flags moved, and nobody was being polled.
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
            assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
            assert(covered(s, t));
        }
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
            assert(live(s, t) && s.tasks[t].queued);
            assert(in_flight(s, t));
        }
    }
    if s.pc is Draining {
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
            assert(live(s, t) && s.tasks[t].queued);
            assert(in_flight(s, t));
        }
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
            assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
            assert(covered(s, t));
        }
    }
}

proof fn lemma_local_wake(s: State, u: nat)
    requires
        inv(s),
        running(s),
    ensures
        inv(local_wake(s, u)),
{
    let s2 = local_wake(s, u);
    lemma_same_wakers(s, s2);
    if live(s, u) {
        assert forall|t: nat| #[trigger] in_flight(s, t) implies in_flight(s2, t) by {
            if s.q.contains(t) {
                let k = choose|k: int| 0 <= k < s.q.len() && s.q[k] == t;
                assert(s2.q[k] == t);
            }
        }
        if !s.tasks[u].queued {
            assert(s2.q[s.q.len() as int] == u);
        }
        assert forall|k: int| 0 <= k < s2.q.len() implies #[trigger] s2.q[k] < s2.next_id by {
            if k < s.q.len() {
                assert(s2.q[k] == s.q[k]);
            }
        }
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
            if t != u {
                assert(live(s, t) && s.tasks[t].queued);
            }
        }
        assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
            if t != u {
                assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
            }
        }
    }
}

proof fn lemma_spawn(s: State)
    requires
        inv(s),
        running(s),
    ensures
        inv(spawn(s)),
{
    let s2 = spawn(s);
    let n = s.next_id;
    lemma_same_wakers(s, s2);
    assert forall|t: nat| #[trigger] in_flight(s, t) implies in_flight(s2, t) by {
        if s.q.contains(t) {
            let k = choose|k: int| 0 <= k < s.q.len() && s.q[k] == t;
            assert(s2.q[k] == t);
        }
    }
    assert(s2.q[s.q.len() as int] == n);
    assert forall|k: int| 0 <= k < s2.q.len() implies #[trigger] s2.q[k] < s2.next_id by {
        if k < s.q.len() {
            assert(s2.q[k] == s.q[k]);
        }
    }
    assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
        if t != n {
            assert(live(s, t) && s.tasks[t].queued);
        }
    }
    assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
        if t != n {
            assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
        }
    }
}

proof fn lemma_waker(s: State, w: nat, t0: nat)
    requires
        inv(s),
    ensures
        inv(waker_next(s, w, t0)),
{
    let s2 = waker_next(s, w, t0);
    let x = wpc(s2, w);
    if s2.wakers == s.wakers {
        lemma_same_wakers(s, s2);
        return;
    }
    lemma_one_waker(s, s2, w, x);
    // The waker's own old state, where some invariant used it as a witness.
    match wpc(s, w) {
        WPc::Idle => {
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                assert(live(s, t) && s.tasks[t].queued);
                assert(in_flight(s, t));
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                if t != t0 {
                    assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                    assert(covered(s, t));
                }
            }
        },
        WPc::Published { t: u } => {
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                if t != u {
                    assert(live(s, t) && s.tasks[t].queued);
                    assert(in_flight(s, t));
                } else if !s.tasks[u].queued {
                    assert(wpc(s2, w) == WPc::Inject { t: u });
                } else {
                    assert(live(s, t) && s.tasks[t].queued);
                    assert(in_flight(s, t));
                    if !s.q.contains(t) && !s.inj.contains(t) {
                        assert(some_waker(s, WPc::Inject { t }));
                        assert(WPc::Inject { t } != wpc(s, w));
                    }
                }
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                if t != u {
                    assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                    assert(covered(s, t));
                }
            }
        },
        WPc::Inject { t: u } => {
            assert(s2.inj[s.inj.len() as int] == u);
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                assert(live(s, t) && s.tasks[t].queued);
                assert(in_flight(s, t));
                if s.inj.contains(t) {
                    let k = choose|k: int| 0 <= k < s.inj.len() && s.inj[k] == t;
                    assert(s2.inj[k] == t);
                }
            }
            assert forall|k: int| 0 <= k < s2.inj.len() implies #[trigger] s2.inj[k] < s2.next_id by {
                if k < s.inj.len() {
                    assert(s2.inj[k] == s.inj[k]);
                }
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                assert(covered(s, t));
            }
        },
        _ => {
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].queued implies in_flight(s2, t) by {
                assert(live(s, t) && s.tasks[t].queued);
                assert(in_flight(s, t));
            }
            assert forall|t: nat| #[trigger] live(s2, t) && s2.tasks[t].seen < s2.tasks[t].events implies covered(s2, t) by {
                assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
                assert(covered(s, t));
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Executions and fairness.
// ---------------------------------------------------------------------------

/// An infinite execution: the state before each step, and the step.
pub struct Ex {
    pub st: spec_fn(nat) -> State,
    pub act: spec_fn(nat) -> Action,
}

pub open spec fn valid(ex: Ex) -> bool {
    &&& init((ex.st)(0))
    &&& forall|i: nat| #[trigger] next((ex.st)(i), (ex.act)(i), (ex.st)(i + 1))
}

/// The worker takes a step, or cannot (asleep with the eventfd unwritten).
pub open spec fn worker_turn(ex: Ex, j: nat) -> bool {
    (ex.act)(j) is Worker || !worker_enabled((ex.st)(j))
}

/// Waking thread `w` takes a step, or has nothing to do.
pub open spec fn waker_turn(ex: Ex, w: nat, j: nat) -> bool {
    ((ex.act)(j) matches Action::Waker { w: v, .. } && v == w) || wpc((ex.st)(j), w) is Idle
}

/// Weak fairness: a thread that can take a step does, sooner or later. Each
/// waking thread, and the worker, is a thread; nothing else is assumed (any
/// number of local wake-ups, spawns and spurious returns may come in
/// between, and a waking thread may start a wake-up or not).
pub open spec fn fair(ex: Ex) -> bool {
    &&& forall|i: nat| #[trigger] worker_runs_after(ex, i)
    &&& forall|w: nat, i: nat| #[trigger] waker_runs_after(ex, w, i)
}

pub open spec fn worker_runs_after(ex: Ex, i: nat) -> bool {
    exists|j: nat| j >= i && #[trigger] worker_turn(ex, j)
}

pub open spec fn waker_runs_after(ex: Ex, w: nat, i: nat) -> bool {
    exists|j: nat| j >= i && #[trigger] waker_turn(ex, w, j)
}

proof fn lemma_step(ex: Ex, i: nat)
    requires
        valid(ex),
    ensures
        next((ex.st)(i), (ex.act)(i), (ex.st)(i + 1)),
{
}

proof fn lemma_always(ex: Ex, i: nat)
    requires
        valid(ex),
    ensures
        inv((ex.st)(i)),
    decreases i,
{
    if i == 0 {
        lemma_init((ex.st)(0));
    } else {
        lemma_always(ex, (i - 1) as nat);
        lemma_step(ex, (i - 1) as nat);
        lemma_next((ex.st)((i - 1) as nat), (ex.act)((i - 1) as nat), (ex.st)(i));
    }
}

/// `p` holds from `i` to `k`, unless `q` held at some point in between.
pub open spec fn reached(ex: Ex, q: spec_fn(State) -> bool, i: nat, k: nat) -> bool {
    exists|m: nat| i <= m <= k && #[trigger] q((ex.st)(m))
}

pub open spec fn stable(p: spec_fn(State) -> bool, q: spec_fn(State) -> bool) -> bool {
    forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) ==> p(s2)
}

proof fn lemma_hold(ex: Ex, p: spec_fn(State) -> bool, q: spec_fn(State) -> bool, i: nat, k: nat)
    requires
        valid(ex),
        stable(p, q),
        p((ex.st)(i)),
        i <= k,
    ensures
        p((ex.st)(k)) || reached(ex, q, i, k),
    decreases k - i,
{
    if k > i {
        let k1 = (k - 1) as nat;
        lemma_hold(ex, p, q, i, k1);
        if p((ex.st)(k1)) {
            lemma_always(ex, k1);
            lemma_step(ex, k1);
            if q((ex.st)(k)) {
                assert(reached(ex, q, i, k));
            }
        } else {
            let m = choose|m: nat| i <= m <= k1 && #[trigger] q((ex.st)(m));
            assert(reached(ex, q, i, k));
        }
    }
}

/// WF1 for the worker: `p` lasts until `q`, the worker can step while `p`
/// holds, and its step from `p` reaches `q`. Then `p` leads to `q`.
proof fn wf1_worker(ex: Ex, p: spec_fn(State) -> bool, q: spec_fn(State) -> bool, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        stable(p, q),
        forall|s: State| #[trigger] p(s) && inv(s) ==> worker_enabled(s),
        forall|s: State, f: bool| #[trigger] p(s) && inv(s) ==> q(#[trigger] worker_next(s, f)),
        p((ex.st)(i)),
    ensures
        j >= i,
        q((ex.st)(j)),
{
    assert(worker_runs_after(ex, i));
    let k = choose|k: nat| k >= i && #[trigger] worker_turn(ex, k);
    lemma_hold(ex, p, q, i, k);
    if reached(ex, q, i, k) {
        choose|m: nat| i <= m <= k && #[trigger] q((ex.st)(m))
    } else {
        lemma_always(ex, k);
        lemma_step(ex, k);
        let f = (ex.act)(k)->Worker_choice;
        assert(q(worker_next((ex.st)(k), f)));
        k + 1
    }
}

/// WF1 for waking thread `w`.
proof fn wf1_waker(ex: Ex, w: nat, p: spec_fn(State) -> bool, q: spec_fn(State) -> bool, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        stable(p, q),
        forall|s: State| #[trigger] p(s) && inv(s) ==> !(wpc(s, w) is Idle),
        forall|s: State, t: nat| #[trigger] p(s) && inv(s) ==> q(#[trigger] waker_next(s, w, t)),
        p((ex.st)(i)),
    ensures
        j >= i,
        q((ex.st)(j)),
{
    assert(waker_runs_after(ex, w, i));
    let k = choose|k: nat| k >= i && #[trigger] waker_turn(ex, w, k);
    lemma_hold(ex, p, q, i, k);
    if reached(ex, q, i, k) {
        choose|m: nat| i <= m <= k && #[trigger] q((ex.st)(m))
    } else {
        lemma_always(ex, k);
        lemma_step(ex, k);
        let t = (ex.act)(k)->Waker_t;
        assert(q(waker_next((ex.st)(k), w, t)));
        k + 1
    }
}

// ---------------------------------------------------------------------------
// Liveness: the chain from a wake-up to the poll that sees it.
// ---------------------------------------------------------------------------

/// Task `t` is live and has a wake-up (the `e`th) its polls have not seen.
pub open spec fn pend(s: State, t: nat, e: nat) -> bool {
    live(s, t) && s.tasks[t].seen < e && e <= s.tasks[t].events
}

/// What the proof is after: a poll of `t` saw the `e`th wake-up, or `t` is done.
pub open spec fn goal(s: State, t: nat, e: nat) -> bool {
    s.tasks.dom().contains(t) && (!s.tasks[t].live || e <= s.tasks[t].seen)
}

/// What no step undoes about task `t`: it exists, its counts only grow, and
/// once finished it stays finished.
proof fn lemma_task_grows(s: State, a: Action, s2: State, t: nat)
    requires
        inv(s),
        next(s, a, s2),
        s.tasks.dom().contains(t),
    ensures
        s2.tasks.dom().contains(t),
        s.tasks[t].events <= s2.tasks[t].events,
        s.tasks[t].seen <= s2.tasks[t].seen,
        s2.tasks[t].live ==> s.tasks[t].live,
        !(a is Worker) ==> s2.tasks[t].seen == s.tasks[t].seen && s2.tasks[t].live == s.tasks[t].live,
{
}

proof fn lemma_goal_stays(s: State, a: Action, s2: State, t: nat, e: nat)
    requires
        inv(s),
        next(s, a, s2),
        goal(s, t, e),
    ensures
        goal(s2, t, e),
{
    lemma_task_grows(s, a, s2, t);
}

/// Non-worker steps leave the worker where it is, except a spurious return
/// from the kernel.
proof fn lemma_pc_frame(s: State, a: Action, s2: State)
    requires
        next(s, a, s2),
        !(a is Worker),
        !(a is Spurious),
    ensures
        s2.pc == s.pc,
{
}

/// (1) A poll in progress sees every wake-up published before it read.
proof fn lemma_polling(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        polling((ex.st)(i), t),
    ensures
        j >= i,
        goal((ex.st)(j), t, e),
{
    let p = |s: State| pend(s, t, e) && polling(s, t);
    let q = |s: State| goal(s, t, e);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        if !(a is Worker) && !(a is Spurious) {
            lemma_pc_frame(s, a, s2);
        }
    }
    wf1_worker(ex, p, q, i)
}

/// `t` is first at index `k` of the run queue.
pub open spec fn first(s: State, t: nat, k: nat) -> bool {
    &&& k < s.q.len()
    &&& s.q[k as int] == t
    &&& forall|j: int| 0 <= j < k ==> s.q[j] != t
}

/// How far the worker is from taking the run queue's front entry.
pub open spec fn rank_pop(s: State) -> nat {
    match s.pc {
        Pc::Batch { left } => if left > 0 && s.q.len() > 0 {
            0
        } else {
            5
        },
        Pc::BatchStart => 1,
        Pc::Draining => 2,
        Pc::Loop => 3,
        Pc::AfterBatch => 4,
        Pc::Polling { .. } => 6,
        _ => 7,
    }
}

proof fn lemma_first_exists(s: State, t: nat) -> (k: nat)
    requires
        s.q.contains(t),
    ensures
        first(s, t, k),
{
    let k0 = choose|k: int| 0 <= k < s.q.len() && s.q[k] == t;
    lemma_first_below(s, t, k0 as nat)
}

proof fn lemma_first_below(s: State, t: nat, k: nat) -> (r: nat)
    requires
        k < s.q.len(),
        s.q[k as int] == t,
    ensures
        first(s, t, r),
    decreases k,
{
    if exists|j: int| 0 <= j < k && s.q[j] == t {
        let j = choose|j: int| 0 <= j < k && s.q[j] == t;
        lemma_first_below(s, t, j as nat)
    } else {
        k
    }
}

/// Appending to the queue keeps the first index.
proof fn lemma_first_append(s: State, s2: State, t: nat, k: nat)
    requires
        first(s, t, k),
        s.q.len() <= s2.q.len(),
        forall|j: int| 0 <= j < s.q.len() ==> s2.q[j] == s.q[j],
    ensures
        first(s2, t, k),
{
}

pub open spec fn q_target(s: State, t: nat, e: nat, k: nat, m: nat) -> bool {
    ||| goal(s, t, e)
    ||| (pend(s, t, e) && polling(s, t))
    ||| (pend(s, t, e) && exists|k2: nat| k2 < k && #[trigger] first(s, t, k2))
    ||| (pend(s, t, e) && first(s, t, k) && rank_pop(s) < m)
}

/// The worker's step with `t` first at `k` in the queue.
proof fn lemma_queue_turn(s: State, f: bool, t: nat, e: nat, k: nat, m: nat)
    requires
        inv(s),
        pend(s, t, e),
        first(s, t, k),
        rank_pop(s) == m,
    ensures
        q_target(worker_next(s, f), t, e, k, m),
{
    let s2 = worker_next(s, f);
    match s.pc {
        Pc::Batch { left } => {
            if left > 0 && s.q.len() > 0 {
                if k == 0 {
                    assert(polling(s2, t));
                } else {
                    assert(first(s2, t, (k - 1) as nat));
                }
            }
        },
        Pc::Draining => {
            lemma_first_append(s, s2, t, k);
        },
        _ => {},
    }
}

/// (6) A task in the run queue is polled: every turn of the worker either
/// takes the front entry or comes closer to doing so, and entries are only
/// ever added behind it.
proof fn lemma_in_queue(ex: Ex, t: nat, e: nat, i: nat, k: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        first((ex.st)(i), t, k),
    ensures
        j >= i,
        goal((ex.st)(j), t, e) || (pend((ex.st)(j), t, e) && polling((ex.st)(j), t)),
    decreases k, rank_pop((ex.st)(i)),
{
    let m = rank_pop((ex.st)(i));
    let p = |s: State| pend(s, t, e) && first(s, t, k) && rank_pop(s) == m;
    let q = |s: State| q_target(s, t, e, k, m);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        match a {
            Action::Worker { choice } => {
                lemma_queue_turn(s, choice, t, e, k, m);
            },
            Action::Spurious => {},
            _ => {
                lemma_pc_frame(s, a, s2);
                lemma_first_append(s, s2, t, k);
            },
        }
    }
    assert forall|s: State, f: bool| #[trigger] p(s) && inv(s) implies q(#[trigger] worker_next(s, f)) by {
        lemma_queue_turn(s, f, t, e, k, m);
    }
    let j = wf1_worker(ex, p, q, i);
    let sj = (ex.st)(j);
    if goal(sj, t, e) || (pend(sj, t, e) && polling(sj, t)) {
        j
    } else if exists|k2: nat| k2 < k && #[trigger] first(sj, t, k2) {
        let k2 = choose|k2: nat| k2 < k && #[trigger] first(sj, t, k2);
        lemma_in_queue(ex, t, e, j, k2)
    } else {
        lemma_in_queue(ex, t, e, j, k)
    }
}

/// Past the injector: in the run queue, being polled, or done.
pub open spec fn queued_or_done(s: State, t: nat, e: nat) -> bool {
    goal(s, t, e) || (pend(s, t, e) && s.q.contains(t)) || (pend(s, t, e) && polling(s, t))
}

/// In the injector, with the doorbell rung.
pub open spec fn rung(s: State, t: nat, e: nat) -> bool {
    pend(s, t, e) && s.inj.contains(t) && s.pending
}

/// Appending to a sequence keeps what it contains.
proof fn lemma_contains_append(a: Seq<nat>, b: Seq<nat>, t: nat)
    requires
        a.contains(t),
        a.len() <= b.len(),
        forall|j: int| 0 <= j < a.len() ==> b[j] == a[j],
    ensures
        b.contains(t),
{
    let k = choose|k: int| 0 <= k < a.len() && a[k] == t;
    assert(b[k] == t);
}

/// What a step does to the injector, the doorbell's `pending` and the run
/// queue, as far as task `t` in the injector is concerned.
proof fn lemma_inj_frame(s: State, a: Action, s2: State, t: nat)
    requires
        inv(s),
        next(s, a, s2),
        s.inj.contains(t),
    ensures
        s2.inj.contains(t) || (s.pc is Draining && s2.q.contains(t)),
        s.pending && !(s.pc is Loop) ==> s2.pending,
        !s.pending && !(a is Waker) ==> !s2.pending,
        s.q.contains(t) && !(a is Worker) ==> s2.q.contains(t),
{
    match a {
        Action::Worker { choice } => {
            if s.pc is Draining {
                lemma_contains_append(s.inj, s.inj, t);
                let k = choose|k: int| 0 <= k < s.inj.len() && s.inj[k] == t;
                assert(s2.q[s.q.len() + k] == t);
            }
        },
        Action::Waker { w, t: t0 } => {
            if wpc(s, w) is Inject {
                lemma_contains_append(s.inj, s2.inj, t);
            }
        },
        Action::LocalWake { u } => {
            if s.q.contains(t) {
                lemma_contains_append(s.q, s2.q, t);
            }
        },
        Action::Spawn => {
            if s.q.contains(t) {
                lemma_contains_append(s.q, s2.q, t);
            }
        },
        _ => {},
    }
}

/// (5a) The worker takes the injector's contents.
proof fn lemma_draining(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        (ex.st)(i).inj.contains(t),
        (ex.st)(i).pc is Draining,
    ensures
        j >= i,
        queued_or_done((ex.st)(j), t, e),
{
    let p = |s: State| pend(s, t, e) && s.inj.contains(t) && s.pc is Draining;
    let q = |s: State| queued_or_done(s, t, e);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        lemma_inj_frame(s, a, s2, t);
        if !(a is Worker) && !(a is Spurious) {
            lemma_pc_frame(s, a, s2);
        }
    }
    assert forall|s: State, f: bool| #[trigger] p(s) && inv(s) implies q(#[trigger] worker_next(s, f)) by {
        lemma_step_worker_inj(s, f, t);
    }
    wf1_worker(ex, p, q, i)
}

proof fn lemma_step_worker_inj(s: State, f: bool, t: nat)
    requires
        inv(s),
        worker_enabled(s),
        s.inj.contains(t),
    ensures
        ({
            let s2 = worker_next(s, f);
            s2.inj.contains(t) || (s.pc is Draining && s2.q.contains(t))
        }),
{
    lemma_inj_frame(s, Action::Worker { choice: f }, worker_next(s, f), t);
}

/// How far the worker is from taking the injector's contents, with the
/// doorbell rung: (major, minor), compared lexicographically.
pub open spec fn rank_take(s: State) -> (nat, nat) {
    match s.pc {
        Pc::Loop => (1, 0),
        Pc::Unpark => (2, 0),
        Pc::ParkRead => (3, 0),
        Pc::ParkSet => (4, 0),
        Pc::AfterBatch => (5, 0),
        Pc::Batch { left } => (6, 2 * left + 1),
        Pc::Polling { left, .. } => (6, 2 * left + 2),
        Pc::BatchStart => (7, 0),
        _ => (0, 0),
    }
}

pub open spec fn lex_lt(a: (nat, nat), b: (nat, nat)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
}

pub open spec fn take_target(s: State, t: nat, e: nat, r: (nat, nat)) -> bool {
    ||| queued_or_done(s, t, e)
    ||| (pend(s, t, e) && s.inj.contains(t) && s.pc is Draining)
    ||| (rung(s, t, e) && !(s.pc is Parked) && !(s.pc is Draining) && lex_lt(rank_take(s), r))
}

proof fn lemma_take_turn(s: State, f: bool, t: nat, e: nat, r: (nat, nat))
    requires
        inv(s),
        rung(s, t, e),
        !(s.pc is Parked),
        !(s.pc is Draining),
        rank_take(s) == r,
    ensures
        take_target(worker_next(s, f), t, e, r),
{
    let s2 = worker_next(s, f);
    lemma_step_worker_inj(s, f, t);
    match s.pc {
        Pc::Batch { left } => {
            if left > 0 && s.q.len() > 0 {
                let u = s.q[0];
                if live(s, u) && u == t {
                    assert(polling(s2, t));
                }
            }
        },
        Pc::Polling { t: u, left } => {
            if u == t {
                assert(goal(s2, t, e));
            }
        },
        _ => {},
    }
}

/// (5b) With the doorbell rung, the worker comes round to `drain_injected`.
proof fn lemma_rung(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        rung((ex.st)(i), t, e),
        !((ex.st)(i).pc is Parked),
    ensures
        j >= i,
        queued_or_done((ex.st)(j), t, e),
    decreases rank_take((ex.st)(i)).0, rank_take((ex.st)(i)).1,
{
    if (ex.st)(i).pc is Draining {
        return lemma_draining(ex, t, e, i);
    }
    let r = rank_take((ex.st)(i));
    let p = |s: State| rung(s, t, e) && !(s.pc is Parked) && !(s.pc is Draining) && rank_take(s) == r;
    let q = |s: State| take_target(s, t, e, r);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        lemma_inj_frame(s, a, s2, t);
        match a {
            Action::Worker { choice } => {
                lemma_take_turn(s, choice, t, e, r);
            },
            Action::Spurious => {},
            _ => {
                lemma_pc_frame(s, a, s2);
            },
        }
    }
    assert forall|s: State, f: bool| #[trigger] p(s) && inv(s) implies q(#[trigger] worker_next(s, f)) by {
        lemma_take_turn(s, f, t, e, r);
    }
    let j = wf1_worker(ex, p, q, i);
    let sj = (ex.st)(j);
    if queued_or_done(sj, t, e) {
        j
    } else if sj.pc is Draining {
        lemma_draining(ex, t, e, j)
    } else {
        lemma_rung(ex, t, e, j)
    }
}

/// Asleep with the doorbell rung: the worker wakes, or `t` got past.
pub open spec fn awake_target(s: State, t: nat, e: nat) -> bool {
    queued_or_done(s, t, e) || (rung(s, t, e) && !(s.pc is Parked))
}

pub open spec fn parked(s: State, t: nat, e: nat) -> bool {
    rung(s, t, e) && s.pc is Parked
}

/// Steps other than the worker's keep `rung` and leave the worker asleep,
/// unless the kernel returns spuriously.
proof fn lemma_parked_frame(s: State, a: Action, s2: State, t: nat, e: nat)
    requires
        inv(s),
        next(s, a, s2),
        parked(s, t, e),
    ensures
        a is Worker || awake_target(s2, t, e) || parked(s2, t, e),
        s.efd <= s2.efd || a is Worker,
        forall|w: nat| (a matches Action::Waker { w: v, .. } && v == w) || a is Worker || #[trigger] wpc(s2, w) == wpc(s, w),
{
    lemma_task_grows(s, a, s2, t);
    lemma_inj_frame(s, a, s2, t);
    if !(a is Worker) && !(a is Spurious) {
        lemma_pc_frame(s, a, s2);
    }
}

/// (5c-1) Asleep with the eventfd written: the worker wakes.
proof fn lemma_efd(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        parked((ex.st)(i), t, e),
        (ex.st)(i).efd > 0,
    ensures
        j >= i,
        awake_target((ex.st)(j), t, e),
{
    let p = |s: State| parked(s, t, e) && s.efd > 0;
    let q = |s: State| awake_target(s, t, e);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_parked_frame(s, a, s2, t, e);
        if a is Worker {
            lemma_step_worker_inj(s, a->Worker_choice, t);
        }
    }
    assert forall|s: State, f: bool| #[trigger] p(s) && inv(s) implies q(#[trigger] worker_next(s, f)) by {
        lemma_step_worker_inj(s, f, t);
    }
    wf1_worker(ex, p, q, i)
}

pub open spec fn efd_or_writer(s: State, t: nat, e: nat) -> bool {
    awake_target(s, t, e) || (parked(s, t, e) && (s.efd > 0 || some_waker(s, WPc::Write)))
}

/// (5c-2) A waker elected to write the eventfd writes it.
proof fn lemma_writer(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        parked((ex.st)(i), t, e),
        some_waker((ex.st)(i), WPc::Write),
    ensures
        j >= i,
        awake_target((ex.st)(j), t, e),
{
    let w = choose|w: nat| #[trigger] wpc((ex.st)(i), w) == WPc::Write;
    let p = |s: State| parked(s, t, e) && wpc(s, w) == WPc::Write;
    let q = |s: State| awake_target(s, t, e) || (parked(s, t, e) && s.efd > 0);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_parked_frame(s, a, s2, t, e);
        if a is Worker {
            lemma_step_worker_inj(s, a->Worker_choice, t);
        }
    }
    assert forall|s: State, t0: nat| #[trigger] p(s) && inv(s) implies q(#[trigger] waker_next(s, w, t0)) by {
        lemma_parked_frame(s, Action::Waker { w, t: t0 }, waker_next(s, w, t0), t, e);
    }
    let j = wf1_waker(ex, w, p, q, i);
    if awake_target((ex.st)(j), t, e) {
        j
    } else {
        lemma_efd(ex, t, e, j)
    }
}

/// (5c-3) A waker about to swap `sleeping`: it is elected to write, or
/// another waker was, or the eventfd is written.
proof fn lemma_swapper(ex: Ex, t: nat, e: nat, i: nat, w: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        parked((ex.st)(i), t, e),
        wpc((ex.st)(i), w) == WPc::RingSwap,
    ensures
        j >= i,
        efd_or_writer((ex.st)(j), t, e),
{
    let p = |s: State| parked(s, t, e) && wpc(s, w) == WPc::RingSwap;
    let q = |s: State| efd_or_writer(s, t, e);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_parked_frame(s, a, s2, t, e);
        if a is Worker {
            lemma_step_worker_inj(s, a->Worker_choice, t);
        }
        if a is Waker && a->Waker_w == w {
            lemma_swap_step(s, w, a->Waker_t, t, e);
        }
    }
    assert forall|s: State, t0: nat| #[trigger] p(s) && inv(s) implies q(#[trigger] waker_next(s, w, t0)) by {
        lemma_swap_step(s, w, t0, t, e);
    }
    wf1_waker(ex, w, p, q, i)
}

proof fn lemma_swap_step(s: State, w: nat, t0: nat, t: nat, e: nat)
    requires
        inv(s),
        parked(s, t, e),
        wpc(s, w) == WPc::RingSwap || wpc(s, w) == WPc::RingCheck,
    ensures
        efd_or_writer(waker_next(s, w, t0), t, e) || (wpc(s, w) == WPc::RingCheck && parked(
            waker_next(s, w, t0),
            t,
            e,
        ) && wpc(waker_next(s, w, t0), w) == WPc::RingSwap),
{
    let s2 = waker_next(s, w, t0);
    lemma_next(s, Action::Waker { w, t: t0 }, s2);
    lemma_parked_frame(s, Action::Waker { w, t: t0 }, s2, t, e);
    lemma_one_waker(s, s2, w, wpc(s2, w));
}

/// (5c-4) A waker about to look at `sleeping`.
proof fn lemma_checker(ex: Ex, t: nat, e: nat, i: nat, w: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        parked((ex.st)(i), t, e),
        wpc((ex.st)(i), w) == WPc::RingCheck,
    ensures
        j >= i,
        efd_or_writer((ex.st)(j), t, e),
{
    let p = |s: State| parked(s, t, e) && wpc(s, w) == WPc::RingCheck;
    let q = |s: State| efd_or_writer(s, t, e) || (parked(s, t, e) && wpc(s, w) == WPc::RingSwap);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_parked_frame(s, a, s2, t, e);
        if a is Worker {
            lemma_step_worker_inj(s, a->Worker_choice, t);
        }
        if a is Waker && a->Waker_w == w {
            lemma_swap_step(s, w, a->Waker_t, t, e);
        }
    }
    assert forall|s: State, t0: nat| #[trigger] p(s) && inv(s) implies q(#[trigger] waker_next(s, w, t0)) by {
        lemma_swap_step(s, w, t0, t, e);
    }
    let j = wf1_waker(ex, w, p, q, i);
    if efd_or_writer((ex.st)(j), t, e) {
        j
    } else {
        lemma_swapper(ex, t, e, j, w)
    }
}

/// (5c) Asleep with the doorbell rung: `Doorbell`'s handshake means a waker
/// saw the worker asleep and writes the eventfd (or already did).
proof fn lemma_parked(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        parked((ex.st)(i), t, e),
    ensures
        j >= i,
        awake_target((ex.st)(j), t, e),
{
    let s = (ex.st)(i);
    lemma_always(ex, i);
    let j = if s.efd > 0 || some_waker(s, WPc::Write) {
        i
    } else if some_waker(s, WPc::RingSwap) {
        let w = choose|w: nat| #[trigger] wpc(s, w) == WPc::RingSwap;
        lemma_swapper(ex, t, e, i, w)
    } else {
        let w = choose|w: nat| #[trigger] wpc(s, w) == WPc::RingCheck;
        lemma_checker(ex, t, e, i, w)
    };
    let sj = (ex.st)(j);
    if awake_target(sj, t, e) {
        j
    } else if sj.efd > 0 {
        lemma_efd(ex, t, e, j)
    } else {
        lemma_writer(ex, t, e, j)
    }
}

/// (5d) In the injector before the doorbell rang: the waker that pushed it
/// rings.
proof fn lemma_unrung(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        (ex.st)(i).inj.contains(t),
        !(ex.st)(i).pending,
        !((ex.st)(i).pc is Draining),
    ensures
        j >= i,
        queued_or_done((ex.st)(j), t, e) || rung((ex.st)(j), t, e),
{
    lemma_always(ex, i);
    let w = choose|w: nat| #[trigger] wpc((ex.st)(i), w) == WPc::Ring;
    let p = |s: State| pend(s, t, e) && s.inj.contains(t) && !s.pending && !(s.pc is Draining)
        && wpc(s, w) == WPc::Ring;
    let q = |s: State| queued_or_done(s, t, e) || rung(s, t, e);
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        lemma_inj_frame(s, a, s2, t);
        match a {
            Action::Worker { choice } => {
                lemma_step_worker_inj(s, choice, t);
                if s.pc is Polling && s.pc->Polling_t == t {
                    assert(goal(s2, t, e));
                }
            },
            Action::Waker { w: v, t: t0 } => {
                lemma_waker_frame(s, v, t0);
            },
            _ => {},
        }
    }
    assert forall|s: State, t0: nat| #[trigger] p(s) && inv(s) implies q(#[trigger] waker_next(s, w, t0)) by {
        lemma_task_grows(s, Action::Waker { w, t: t0 }, waker_next(s, w, t0), t);
        lemma_inj_frame(s, Action::Waker { w, t: t0 }, waker_next(s, w, t0), t);
    }
    wf1_waker(ex, w, p, q, i)
}

/// (5) A task in the injector reaches the run queue (or is polled, or done).
proof fn lemma_injected(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        (ex.st)(i).inj.contains(t),
    ensures
        j >= i,
        queued_or_done((ex.st)(j), t, e),
{
    let s = (ex.st)(i);
    let j1 = if s.pc is Draining {
        i
    } else if !s.pending {
        lemma_unrung(ex, t, e, i)
    } else {
        i
    };
    let s1 = (ex.st)(j1);
    if queued_or_done(s1, t, e) {
        return j1;
    }
    if s1.pc is Draining {
        return lemma_draining(ex, t, e, j1);
    }
    let j2 = if s1.pc is Parked {
        lemma_parked(ex, t, e, j1)
    } else {
        j1
    };
    if queued_or_done((ex.st)(j2), t, e) {
        j2
    } else {
        lemma_rung(ex, t, e, j2)
    }
}

/// The worker's step changes task `t` only by popping or polling it.
proof fn lemma_worker_keeps_others(s: State, f: bool, t: nat)
    requires
        inv(s),
        worker_enabled(s),
        !polling(s, t),
        !(s.pc is Batch && s.pc->Batch_left > 0 && s.q.len() > 0 && s.q[0] == t),
    ensures
        s.tasks.dom().contains(t) ==> worker_next(s, f).tasks.dom().contains(t)
            && worker_next(s, f).tasks[t] == s.tasks[t],
{
}

/// (4) A waker that won `try_enqueue` pushes the task into the injector.
proof fn lemma_inject_waker(ex: Ex, t: nat, e: nat, i: nat, w: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        wpc((ex.st)(i), w) == (WPc::Inject { t }),
    ensures
        j >= i,
        queued_or_done((ex.st)(j), t, e) || (pend((ex.st)(j), t, e) && (ex.st)(j).inj.contains(t)),
{
    let p = |s: State| pend(s, t, e) && wpc(s, w) == (WPc::Inject { t });
    let q = |s: State| queued_or_done(s, t, e) || (pend(s, t, e) && s.inj.contains(t));
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        match a {
            Action::Worker { choice } => {
                if s.pc is Polling && s.pc->Polling_t == t {
                    assert(goal(s2, t, e));
                }
                if s.pc is Batch && s.pc->Batch_left > 0 && s.q.len() > 0 && s.q[0] == t {
                    assert(polling(s2, t));
                }
                lemma_same_wakers(s, s2);
                lemma_worker_keeps_others(s, choice, t);
            },
            Action::Waker { w: v, t: t0 } => {
                lemma_waker_frame(s, v, t0);
                if v == w {
                    assert(s2.inj[s.inj.len() as int] == t);
                }
            },
            Action::LocalWake { u } => {
                if s.q.contains(t) {
                    lemma_contains_append(s.q, s2.q, t);
                }
            },
            Action::Spawn => {
                if s.q.contains(t) {
                    lemma_contains_append(s.q, s2.q, t);
                }
            },
            _ => {},
        }
    }
    assert forall|s: State, t0: nat| #[trigger] p(s) && inv(s) implies q(#[trigger] waker_next(s, w, t0)) by {
        let s2 = waker_next(s, w, t0);
        assert(s2.inj[s.inj.len() as int] == t);
    }
    wf1_waker(ex, w, p, q, i)
}

/// (2) A waker that published a wake-up calls `try_enqueue`.
proof fn lemma_published(ex: Ex, t: nat, e: nat, i: nat, w: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        wpc((ex.st)(i), w) == (WPc::Published { t }),
    ensures
        j >= i,
        goal((ex.st)(j), t, e) || (pend((ex.st)(j), t, e) && ((ex.st)(j).tasks[t].queued
            || polling((ex.st)(j), t))),
{
    let p = |s: State| pend(s, t, e) && wpc(s, w) == (WPc::Published { t });
    let q = |s: State| goal(s, t, e) || (pend(s, t, e) && (s.tasks[t].queued || polling(s, t)));
    assert forall|s: State, a: Action, s2: State|
        #![trigger next(s, a, s2), p(s)]
        inv(s) && p(s) && next(s, a, s2) && !q(s2) implies p(s2) by {
        lemma_task_grows(s, a, s2, t);
        if a is Waker {
            lemma_waker_frame(s, a->Waker_w, a->Waker_t);
        }
    }
    wf1_waker(ex, w, p, q, i)
}

/// A queued task is in the run queue, in the injector, or about to be
/// pushed into it; follow it to a poll.
proof fn lemma_queued(ex: Ex, t: nat, e: nat, i: nat) -> (j: nat)
    requires
        valid(ex),
        fair(ex),
        pend((ex.st)(i), t, e),
        (ex.st)(i).tasks[t].queued || polling((ex.st)(i), t),
    ensures
        j >= i,
        goal((ex.st)(j), t, e),
{
    let s = (ex.st)(i);
    lemma_always(ex, i);
    let j1 = if polling(s, t) || s.q.contains(t) {
        i
    } else if s.inj.contains(t) {
        lemma_injected(ex, t, e, i)
    } else {
        assert(live(s, t) && s.tasks[t].queued);
        let w = choose|w: nat| #[trigger] wpc(s, w) == (WPc::Inject { t });
        let j0 = lemma_inject_waker(ex, t, e, i, w);
        if queued_or_done((ex.st)(j0), t, e) {
            j0
        } else {
            lemma_injected(ex, t, e, j0)
        }
    };
    let s1 = (ex.st)(j1);
    if goal(s1, t, e) {
        return j1;
    }
    let j2 = if polling(s1, t) {
        j1
    } else {
        let k = lemma_first_exists(s1, t);
        lemma_in_queue(ex, t, e, j1, k)
    };
    if goal((ex.st)(j2), t, e) {
        j2
    } else {
        lemma_polling(ex, t, e, j2)
    }
}

/// **The theorem.** In every execution of the model in which the worker and
/// each waking thread get to run (weak fairness), every wake-up of a live
/// task is followed by a poll of that task that sees it, unless the task
/// finishes first. No bound on tasks, waking threads, queued entries or
/// steps; spawns, local wake-ups and spurious returns from the kernel may
/// come at any time.
pub proof fn never_forgets_a_task(ex: Ex, i: nat, t: nat, e: nat)
    requires
        valid(ex),
        fair(ex),
        live((ex.st)(i), t),
        (ex.st)(i).tasks[t].seen < e <= (ex.st)(i).tasks[t].events,
    ensures
        exists|j: nat| j >= i && #[trigger] goal((ex.st)(j), t, e),
{
    let s = (ex.st)(i);
    lemma_always(ex, i);
    assert(live(s, t) && s.tasks[t].seen < s.tasks[t].events);
    let j = if s.tasks[t].queued || polling(s, t) {
        lemma_queued(ex, t, e, i)
    } else {
        let w = choose|w: nat| #[trigger] wpc(s, w) == (WPc::Published { t });
        let j0 = lemma_published(ex, t, e, i, w);
        if goal((ex.st)(j0), t, e) {
            j0
        } else {
            lemma_queued(ex, t, e, j0)
        }
    };
    assert(goal((ex.st)(j), t, e));
}

// ---------------------------------------------------------------------------
// The assumptions are not vacuous: an execution meets them with a task
// waiting to be polled.
// ---------------------------------------------------------------------------

pub open spec fn witness_st(i: nat) -> State
    decreases i,
{
    if i == 0 {
        State {
            tasks: Map::empty(),
            next_id: 0,
            q: Seq::empty(),
            inj: Seq::empty(),
            pending: false,
            sleeping: false,
            efd: 0,
            pc: Pc::Loop,
            wakers: Map::empty(),
        }
    } else if i == 1 {
        spawn(witness_st(0))
    } else {
        let s = witness_st((i - 1) as nat);
        if worker_enabled(s) {
            worker_next(s, false)
        } else {
            s
        }
    }
}

pub open spec fn witness_act(i: nat) -> Action {
    if i == 0 {
        Action::Spawn
    } else if worker_enabled(witness_st(i)) {
        Action::Worker { choice: false }
    } else {
        Action::Stutter
    }
}

pub open spec fn witness() -> Ex {
    Ex { st: |i: nat| witness_st(i), act: |i: nat| witness_act(i) }
}

proof fn lemma_witness_no_wakers(i: nat)
    ensures
        witness_st(i).wakers == Map::<nat, WPc>::empty(),
    decreases i,
{
    if i > 0 {
        lemma_witness_no_wakers((i - 1) as nat);
    }
}

/// A valid, fair execution in which task 0 has a wake-up to see at step 1:
/// the theorem's assumptions can all hold at once.
proof fn assumptions_are_satisfiable()
    ensures
        valid(witness()),
        fair(witness()),
        live(witness_st(1), 0),
        witness_st(1).tasks[0].seen < witness_st(1).tasks[0].events,
{
    let ex = witness();
    assert forall|i: nat| #[trigger] next((ex.st)(i), (ex.act)(i), (ex.st)(i + 1)) by {
        assert(witness_st(i + 1) == if i + 1 == 1 {
            spawn(witness_st(0))
        } else if worker_enabled(witness_st(i)) {
            worker_next(witness_st(i), false)
        } else {
            witness_st(i)
        });
    }
    assert forall|i: nat| #[trigger] worker_runs_after(ex, i) by {
        assert(worker_turn(ex, i + 1));
    }
    assert forall|w: nat, i: nat| #[trigger] waker_runs_after(ex, w, i) by {
        lemma_witness_no_wakers(i);
        assert(waker_turn(ex, w, i));
    }
    assert(witness_st(1) == spawn(witness_st(0)));
}

} // verus!
