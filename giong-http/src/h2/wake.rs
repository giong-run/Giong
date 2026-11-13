//! Wakers for the streams of one HTTP/2 connection.
//!
//! The handlers of a connection's streams run inside the connection's own
//! task, so that resetting a stream drops its handler at once. Each stream
//! slot has a waker of its own; waking it queues the slot and wakes the
//! connection, which then polls only the slots that were woken. The wakers
//! may be called from any thread (a handler may wait on a channel fed by
//! another worker), so the queue sits behind a mutex.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Wake, Waker};

#[derive(Default)]
struct Queue {
    slots: Vec<usize>,
    /// The connection's task, once it waits.
    conn: Option<Waker>,
}

#[derive(Default)]
struct Ready(Mutex<Queue>);

impl Ready {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct SlotWaker {
    slot: usize,
    /// In the queue already: further wakes add nothing.
    queued: AtomicBool,
    ready: Arc<Ready>,
}

impl Wake for SlotWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut queue = self.ready.lock();
        queue.slots.push(self.slot);
        if let Some(conn) = &queue.conn {
            conn.wake_by_ref();
        }
    }
}

/// The wakers of a connection's stream slots.
#[derive(Default)]
pub(crate) struct Wakers {
    ready: Arc<Ready>,
    slots: Vec<(Arc<SlotWaker>, Waker)>,
}

impl Wakers {
    /// The waker of `slot`, made the first time it is asked for.
    pub(crate) fn get(&mut self, slot: usize) -> &Waker {
        while self.slots.len() <= slot {
            let inner = Arc::new(SlotWaker {
                slot: self.slots.len(),
                queued: AtomicBool::new(false),
                ready: self.ready.clone(),
            });
            let waker = Waker::from(inner.clone());
            self.slots.push((inner, waker));
        }
        &self.slots[slot].1
    }

    /// Forgets the wakers of every slot, with no stream open. A waker of
    /// theirs still held somewhere can still fire: its slot is then polled
    /// for nothing, which is harmless.
    pub(crate) fn release(&mut self) {
        self.slots = Vec::new();
        let mut queue = self.ready.lock();
        queue.slots.clear();
        queue.slots.shrink_to_fit();
    }

    /// Moves the woken slots into `into`, without registering anything.
    pub(crate) fn drain(&self, into: &mut Vec<usize>) -> bool {
        let mut queue = self.ready.lock();
        if queue.slots.is_empty() {
            return false;
        }
        for &slot in &queue.slots {
            if let Some((inner, _)) = self.slots.get(slot) {
                inner.queued.store(false, Ordering::Release);
            }
        }
        into.append(&mut queue.slots);
        true
    }

    /// Moves the woken slots into `into`, marking them as not queued, so a
    /// wake while they are polled queues them again. If none was woken,
    /// `conn` is woken by the next wake instead.
    pub(crate) fn take(&self, conn: &Waker, into: &mut Vec<usize>) -> bool {
        let mut queue = self.ready.lock();
        if queue.slots.is_empty() {
            if !queue.conn.as_ref().is_some_and(|w| w.will_wake(conn)) {
                queue.conn = Some(conn.clone());
            }
            return false;
        }
        for &slot in &queue.slots {
            if let Some((inner, _)) = self.slots.get(slot) {
                inner.queued.store(false, Ordering::Release);
            }
        }
        into.append(&mut queue.slots);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Count(std::sync::atomic::AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn wakes_queue_slots_once_and_wake_the_connection() {
        let count = Arc::new(Count(Default::default()));
        let conn = Waker::from(count.clone());
        let mut wakers = Wakers::default();
        let mut woken = Vec::new();
        assert!(!wakers.take(&conn, &mut woken));
        wakers.get(3).wake_by_ref();
        wakers.get(3).wake_by_ref();
        wakers.get(1).wake_by_ref();
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
        assert!(wakers.take(&conn, &mut woken));
        assert_eq!(woken, [3, 1]);
        // Taken slots can be queued again, from another thread too.
        let waker = wakers.get(3).clone();
        std::thread::spawn(move || waker.wake()).join().unwrap();
        woken.clear();
        assert!(wakers.take(&conn, &mut woken));
        assert_eq!(woken, [3]);
    }
}
