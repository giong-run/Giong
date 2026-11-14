//! The bounded multi-producer, single-consumer channel: [`mpsc::bounded`].
//!
//! [`mpsc::bounded`]: super::mpsc::bounded

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::poll_fn;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

use super::mpsc::{SendError, TryRecvError};

struct State<T> {
    queue: VecDeque<T>,
    capacity: usize,
    rx_waker: Option<Waker>,
    senders: usize,
    rx_alive: bool,
    /// Senders waiting for room, oldest first, each with its waker in
    /// `wakers`.
    order: VecDeque<u64>,
    wakers: HashMap<u64, Waker>,
    /// Waiting senders that have been given a free slot but have not used it
    /// yet. Their slots count as taken.
    granted: Vec<u64>,
    next_id: u64,
}

impl<T> State<T> {
    /// Free slots that nobody has a claim on.
    fn room(&self) -> bool {
        self.queue.len() + self.granted.len() < self.capacity
    }

    /// Gives free slots to the senders that have waited longest. Returns the
    /// wakers to call once the lock is released.
    fn grant(&mut self, wake: &mut Vec<Waker>) {
        while self.room() {
            let Some(id) = self.order.pop_front() else {
                return;
            };
            if let Some(waker) = self.wakers.remove(&id) {
                self.granted.push(id);
                wake.push(waker);
            }
        }
    }
}

struct Shared<T>(Mutex<State<T>>);

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

/// Creates a channel that holds at most `capacity` values. When it is full,
/// [`BoundedSender::send`] waits for room, so a fast producer is slowed down
/// to the pace of the consumer instead of filling memory.
///
/// Waiting senders get room in the order they started waiting. Senders and
/// the receiver may live on any worker or thread.
///
/// # Panics
///
/// Panics if `capacity` is 0.
///
/// ```
/// use giong::sync::mpsc;
///
/// giong::block_on(async {
///     let (tx, mut rx) = mpsc::bounded(2);
///     giong::spawn(async move {
///         for i in 0..10 {
///             // Waits whenever two values are already queued.
///             tx.send(i).await.unwrap();
///         }
///     });
///     let mut sum = 0;
///     while let Some(i) = rx.recv().await {
///         sum += i;
///     }
///     assert_eq!(sum, 45);
/// });
/// ```
pub fn bounded<T>(capacity: usize) -> (BoundedSender<T>, BoundedReceiver<T>) {
    assert!(
        capacity > 0,
        "a bounded channel needs a capacity of at least 1"
    );
    let shared = Arc::new(Shared(Mutex::new(State {
        queue: VecDeque::with_capacity(capacity.min(1024)),
        capacity,
        rx_waker: None,
        senders: 1,
        rx_alive: true,
        order: VecDeque::new(),
        wakers: HashMap::new(),
        granted: Vec::new(),
        next_id: 0,
    })));
    (
        BoundedSender {
            shared: shared.clone(),
        },
        BoundedReceiver { shared },
    )
}

/// The sending half of a [`bounded`] channel. Cheap to clone; `Send` and
/// `Sync` when `T` is `Send`.
pub struct BoundedSender<T> {
    shared: Arc<Shared<T>>,
}

/// The receiving half of a [`bounded`] channel.
pub struct BoundedReceiver<T> {
    shared: Arc<Shared<T>>,
}

/// Why [`BoundedSender::try_send`] did not send; holds the value.
pub enum TrySendError<T> {
    /// The channel is full, or other senders are already waiting for room.
    Full(T),
    /// The receiver is gone.
    Closed(T),
}

impl<T> TrySendError<T> {
    /// The value that was not sent.
    pub fn into_inner(self) -> T {
        match self {
            TrySendError::Full(value) | TrySendError::Closed(value) => value,
        }
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.write_str("Full(..)"),
            TrySendError::Closed(_) => f.write_str("Closed(..)"),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.write_str("the channel is full"),
            TrySendError::Closed(_) => f.write_str("sending on a closed channel"),
        }
    }
}

impl<T> std::error::Error for TrySendError<T> {}

/// A send that is waiting for room. Dropping it gives up its place in line,
/// and a slot it was already granted goes to the next sender in line.
struct Waiting<'a, T> {
    shared: &'a Shared<T>,
    id: Option<u64>,
}

impl<T> Drop for Waiting<'_, T> {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        let mut wake = Vec::new();
        {
            let mut state = self.shared.lock();
            if state.wakers.remove(&id).is_some() {
                // Still in line: leave it.
                if let Some(i) = state.order.iter().position(|&o| o == id) {
                    state.order.remove(i);
                }
            } else if let Some(i) = state.granted.iter().position(|&g| g == id) {
                // Granted a slot it will not use: pass it on.
                state.granted.swap_remove(i);
                state.grant(&mut wake);
            }
        }
        wake_all(wake);
    }
}

impl<T> BoundedSender<T> {
    /// Sends `value`, waiting while the channel is full. Fails, handing the
    /// value back, if the receiver is gone.
    ///
    /// Cancel safe: if the future is dropped, the value is dropped with it and
    /// nothing was sent; a slot it had been promised goes to the next sender.
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut value = Some(value);
        let mut waiting = Waiting {
            shared: &self.shared,
            id: None,
        };
        let waker = poll_fn(|cx| {
            let mut state = self.shared.lock();
            if !state.rx_alive {
                let value = value.take().expect("polled after completion");
                return Poll::Ready(Err(SendError(value)));
            }
            let admitted = match waiting.id {
                // First poll: go ahead only if nobody is waiting already.
                None => state.order.is_empty() && state.room(),
                Some(id) => match state.granted.iter().position(|&g| g == id) {
                    Some(i) => {
                        state.granted.swap_remove(i);
                        true
                    }
                    None => false,
                },
            };
            if admitted {
                waiting.id = None;
                state
                    .queue
                    .push_back(value.take().expect("polled after completion"));
                return Poll::Ready(Ok(state.rx_waker.take()));
            }
            let id = *waiting.id.get_or_insert_with(|| {
                let id = state.next_id;
                state.next_id += 1;
                state.order.push_back(id);
                id
            });
            match state.wakers.get_mut(&id) {
                Some(w) if w.will_wake(cx.waker()) => {}
                Some(w) => *w = cx.waker().clone(),
                None => {
                    state.wakers.insert(id, cx.waker().clone());
                }
            }
            Poll::Pending
        })
        .await?;
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Sends `value` if there is room right now and no other sender is
    /// waiting for it.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut state = self.shared.lock();
            if !state.rx_alive {
                return Err(TrySendError::Closed(value));
            }
            if !state.order.is_empty() || !state.room() {
                return Err(TrySendError::Full(value));
            }
            state.queue.push_back(value);
            state.rx_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// `true` once the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        !self.shared.lock().rx_alive
    }

    /// The most values the channel holds.
    pub fn capacity(&self) -> usize {
        self.shared.lock().capacity
    }
}

impl<T> Clone for BoundedSender<T> {
    fn clone(&self) -> Self {
        self.shared.lock().senders += 1;
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for BoundedSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.shared.lock();
            state.senders -= 1;
            if state.senders == 0 {
                state.rx_waker.take()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> BoundedReceiver<T> {
    /// Receives the next value, or `None` once every sender is gone and the
    /// channel is empty. Cancel safe.
    pub async fn recv(&mut self) -> Option<T> {
        let mut wake = Vec::new();
        let value = poll_fn(|cx| {
            let mut state = self.shared.lock();
            if let Some(value) = state.queue.pop_front() {
                state.grant(&mut wake);
                return Poll::Ready(Some(value));
            }
            if state.senders == 0 {
                return Poll::Ready(None);
            }
            match &state.rx_waker {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => state.rx_waker = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
        .await;
        wake_all(wake);
        value
    }

    /// Takes a value if one is queued, without waiting for one. Fails with
    /// [`TryRecvError::Empty`] if none is, and with [`TryRecvError::Closed`]
    /// once every sender is gone and the channel is empty, when
    /// [`recv`](Self::recv) would return `None`.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut wake = Vec::new();
        let value = {
            let mut state = self.shared.lock();
            match state.queue.pop_front() {
                Some(value) => {
                    state.grant(&mut wake);
                    Ok(value)
                }
                None if state.senders == 0 => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        };
        wake_all(wake);
        value
    }

    /// The number of values waiting to be received.
    pub fn len(&self) -> usize {
        self.shared.lock().queue.len()
    }

    /// `true` if no value is waiting to be received.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for BoundedReceiver<T> {
    fn drop(&mut self) {
        let (leftover, wake) = {
            let mut state = self.shared.lock();
            state.rx_alive = false;
            state.rx_waker = None;
            state.order.clear();
            let wake: Vec<Waker> = state.wakers.drain().map(|(_, w)| w).collect();
            (std::mem::take(&mut state.queue), wake)
        };
        // Outside the lock: destructors may send on this channel, and the
        // waiting senders find the channel closed and get their values back.
        drop(leftover);
        wake_all(wake);
    }
}

impl<T> fmt::Debug for BoundedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedSender")
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl<T> fmt::Debug for BoundedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedReceiver")
            .field("len", &self.len())
            .finish()
    }
}
