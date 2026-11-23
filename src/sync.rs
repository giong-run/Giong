//! Message passing between tasks — on the same worker, across workers, or
//! from threads outside the runtime.
//!
//! Workers share nothing by default; when they do need to talk, they send
//! messages. giong's wakers are thread-safe, so the receiving task is woken
//! correctly no matter which thread the message comes from.

mod bounded;
pub mod oneshot;

pub mod mpsc {
    //! Multi-producer, single-consumer channels: [`unbounded`] takes every
    //! value it is sent, [`bounded`] holds a fixed number of values and makes
    //! senders wait.
    //!
    //! Tokio's `mpsc::channel(n)` is [`bounded`]`(n)` here, and its
    //! `unbounded_channel()` is [`unbounded`]`()`.
    //!
    //! The halves of the bounded channel are [`Sender`] and [`Receiver`], as in
    //! Tokio, so the short name is the channel that holds its memory to a
    //! bound; the unbounded ones are [`UnboundedSender`] and
    //! [`UnboundedReceiver`].

    pub use super::bounded::{bounded, Receiver, Sender, TrySendError};

    use std::collections::VecDeque;
    use std::fmt;
    use std::future::poll_fn;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::task::{Poll, Waker};

    struct State<T> {
        queue: VecDeque<T>,
        rx_waker: Option<Waker>,
        senders: usize,
        rx_alive: bool,
    }

    struct Shared<T>(Mutex<State<T>>);

    impl<T> Shared<T> {
        fn lock(&self) -> MutexGuard<'_, State<T>> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    /// Creates an unbounded channel: [`UnboundedSender::send`] never waits, and
    /// values queue up until the receiver takes them. Use [`bounded`] when
    /// the senders can outrun the receiver.
    pub fn unbounded<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
        let shared = Arc::new(Shared(Mutex::new(State {
            queue: VecDeque::new(),
            rx_waker: None,
            senders: 1,
            rx_alive: true,
        })));
        (
            UnboundedSender {
                shared: shared.clone(),
            },
            UnboundedReceiver {
                shared,
                taken: Mutex::new(VecDeque::new()),
            },
        )
    }

    /// The sending half. Cheap to clone; `Send` when `T` is.
    pub struct UnboundedSender<T> {
        shared: Arc<Shared<T>>,
    }

    /// The receiving half.
    pub struct UnboundedReceiver<T> {
        shared: Arc<Shared<T>>,
        /// Values already taken from the shared queue, oldest first. The
        /// receiver empties the shared queue in one go when this runs dry,
        /// so it takes the lock once per batch rather than once per value,
        /// and senders on other threads rarely find the lock held. A
        /// `Mutex` only for its auto traits: it is reached through
        /// `&mut self` with `get_mut`, which never locks.
        taken: Mutex<VecDeque<T>>,
    }

    /// Returned by [`UnboundedSender::send`] and [`Sender::send`] when the receiver
    /// is gone; holds the value.
    pub struct SendError<T>(pub T);

    impl<T> fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("SendError(..)")
        }
    }

    impl<T> fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("sending on a closed channel")
        }
    }

    impl<T> std::error::Error for SendError<T> {}

    /// Why [`UnboundedReceiver::try_recv`] or [`Receiver::try_recv`] returned
    /// no value.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TryRecvError {
        /// No value is waiting; a sender is still alive.
        Empty,
        /// Every sender is gone and every value has been received: no value
        /// will ever come.
        Closed,
    }

    impl fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TryRecvError::Empty => f.write_str("no value is waiting"),
                TryRecvError::Closed => f.write_str("the channel is closed"),
            }
        }
    }

    impl std::error::Error for TryRecvError {}

    impl<T> UnboundedSender<T> {
        /// Sends a value without blocking. Fails if the receiver was dropped.
        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            let waker = {
                let mut state = self.shared.lock();
                if !state.rx_alive {
                    return Err(SendError(value));
                }
                state.queue.push_back(value);
                state.rx_waker.take()
            };
            // Woken outside the lock.
            if let Some(waker) = waker {
                waker.wake();
            }
            Ok(())
        }
    }

    impl<T> Clone for UnboundedSender<T> {
        fn clone(&self) -> Self {
            self.shared.lock().senders += 1;
            Self {
                shared: self.shared.clone(),
            }
        }
    }

    impl<T> Drop for UnboundedSender<T> {
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

    impl<T> UnboundedReceiver<T> {
        /// Receives the next value, or `None` once every sender is gone and
        /// the channel is empty.
        ///
        /// A value that is ready at once costs the task one unit of its
        /// budget (128 per turn), as socket operations do: after that `recv`
        /// yields to the worker's other tasks first, so a channel kept full
        /// cannot starve them. To drain what is queued without yielding, use
        /// [`try_recv`](Self::try_recv). Do not poll it from another executor
        /// inside a giong task: once the budget is spent, that executor would
        /// poll it forever.
        ///
        /// Cancel safe: if the future is dropped, no value is lost; the next
        /// call returns it.
        pub async fn recv(&mut self) -> Option<T> {
            poll_fn(|cx| {
                let taken = self.taken.get_mut().unwrap_or_else(|e| e.into_inner());
                if taken.is_empty() {
                    let mut state = self.shared.lock();
                    if state.queue.is_empty() {
                        if state.senders == 0 {
                            return Poll::Ready(None);
                        }
                        match &state.rx_waker {
                            Some(w) if w.will_wake(cx.waker()) => {}
                            _ => state.rx_waker = Some(cx.waker().clone()),
                        }
                        return Poll::Pending;
                    }
                    std::mem::swap(&mut state.queue, taken);
                }
                // A value that is ready at once counts against the task's
                // budget: a receiver that other threads keep fed must still
                // let the worker's other tasks and its event loop run.
                if !crate::driver::spend_or_yield(cx) {
                    return Poll::Pending;
                }
                Poll::Ready(taken.pop_front())
            })
            .await
        }

        /// Takes a value if one is waiting, without waiting for one. Fails
        /// with [`TryRecvError::Empty`] if none is, and with
        /// [`TryRecvError::Closed`] once every sender is gone and the channel
        /// is empty, when [`recv`](Self::recv) would return `None`.
        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            let taken = self.taken.get_mut().unwrap_or_else(|e| e.into_inner());
            if let Some(value) = taken.pop_front() {
                return Ok(value);
            }
            let mut state = self.shared.lock();
            std::mem::swap(&mut state.queue, taken);
            match taken.pop_front() {
                Some(value) => Ok(value),
                None if state.senders == 0 => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        }
    }

    impl<T> Drop for UnboundedReceiver<T> {
        fn drop(&mut self) {
            let leftover = {
                let mut state = self.shared.lock();
                state.rx_alive = false;
                state.rx_waker = None;
                std::mem::take(&mut state.queue)
            };
            // Dropped outside the lock: destructors may send on this channel.
            drop(leftover);
        }
    }
}
