//! Message passing between tasks — on the same worker, across workers, or
//! from threads outside the runtime.
//!
//! Workers share nothing by default; when they do need to talk, they send
//! messages. giong's wakers are thread-safe, so the receiving task is woken
//! correctly no matter which thread the message comes from.

mod bounded;
pub mod oneshot;

pub mod mpsc {
    //! Multi-producer, single-consumer channels: [`channel`] is unbounded,
    //! [`bounded`] holds a fixed number of values and makes senders wait.

    pub use super::bounded::{bounded, BoundedReceiver, BoundedSender, TrySendError};

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

    /// Creates an unbounded channel.
    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared(Mutex::new(State {
            queue: VecDeque::new(),
            rx_waker: None,
            senders: 1,
            rx_alive: true,
        })));
        (
            Sender {
                shared: shared.clone(),
            },
            Receiver { shared },
        )
    }

    /// The sending half. Cheap to clone; `Send` when `T` is.
    pub struct Sender<T> {
        shared: Arc<Shared<T>>,
    }

    /// The receiving half.
    pub struct Receiver<T> {
        shared: Arc<Shared<T>>,
    }

    /// Returned by [`Sender::send`] and [`BoundedSender::send`] when the receiver
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

    impl<T> Sender<T> {
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

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            self.shared.lock().senders += 1;
            Self {
                shared: self.shared.clone(),
            }
        }
    }

    impl<T> Drop for Sender<T> {
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

    impl<T> Receiver<T> {
        /// Receives the next value, or `None` once every sender is gone and
        /// the channel is empty.
        pub async fn recv(&mut self) -> Option<T> {
            poll_fn(|cx| {
                let mut state = self.shared.lock();
                if let Some(value) = state.queue.pop_front() {
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
            .await
        }

        /// Takes a value if one is ready, without waiting.
        pub fn try_recv(&mut self) -> Option<T> {
            self.shared.lock().queue.pop_front()
        }
    }

    impl<T> Drop for Receiver<T> {
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
