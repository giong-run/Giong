//! A channel that carries a single value: the usual way to hand a result back
//! to the task that asked for it, on the same worker, on another worker or
//! from a thread outside the runtime.
//!
//! ```
//! use giong::sync::oneshot;
//!
//! giong::block_on(async {
//!     let (tx, rx) = oneshot::channel();
//!     std::thread::spawn(move || {
//!         let _ = tx.send(6 * 7);
//!     });
//!     assert_eq!(rx.await, Ok(42));
//! });
//! ```

use std::fmt;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    /// The receiver, waiting for the value.
    rx_waker: Option<Waker>,
    /// The sender, waiting in [`Sender::closed`].
    tx_waker: Option<Waker>,
    tx_alive: bool,
    rx_alive: bool,
}

struct Shared<T>(Mutex<State<T>>);

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Creates a channel for one value.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared(Mutex::new(State {
        value: None,
        rx_waker: None,
        tx_waker: None,
        tx_alive: true,
        rx_alive: true,
    })));
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

/// Sends the value. `Send` when `T` is.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// Receives the value: await it. `Send` when `T` is.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

/// The sender was dropped without sending a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the sender was dropped without sending a value")
    }
}

impl std::error::Error for RecvError {}

/// Why [`Receiver::try_recv`] returned no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// No value yet; the sender is still alive.
    Empty,
    /// The sender was dropped without sending a value, or the value was
    /// already taken.
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => f.write_str("no value has been sent yet"),
            TryRecvError::Closed => f.write_str("the channel is closed"),
        }
    }
}

impl std::error::Error for TryRecvError {}

impl<T> Sender<T> {
    /// Sends `value`, waking the receiver. Fails, handing the value back, if
    /// the receiver is gone.
    pub fn send(self, value: T) -> Result<(), T> {
        let waker = {
            let mut state = self.shared.lock();
            if !state.rx_alive {
                return Err(value);
            }
            state.value = Some(value);
            state.rx_waker.take()
        };
        // Woken outside the lock. `Drop` runs next and finds no waker left.
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// `true` once the receiver has been dropped: nobody wants the value any
    /// more, so the work that would produce it can stop.
    pub fn is_closed(&self) -> bool {
        !self.shared.lock().rx_alive
    }

    /// Waits until the receiver is dropped. Useful to abandon work whose
    /// result nobody will read, for example with a `select` on both.
    pub async fn closed(&self) {
        poll_fn(|cx| {
            let mut state = self.shared.lock();
            if !state.rx_alive {
                return Poll::Ready(());
            }
            match &state.tx_waker {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => state.tx_waker = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
        .await
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.shared.lock();
            state.tx_alive = false;
            state.tx_waker = None;
            state.rx_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// Takes the value if it has arrived, without waiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.shared.lock();
        match state.value.take() {
            Some(value) => Ok(value),
            None if state.tx_alive => Err(TryRecvError::Empty),
            None => Err(TryRecvError::Closed),
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    /// Resolves to the value, or to [`RecvError`] if the sender was dropped
    /// without sending one. Cancel safe: a value sent while this future is
    /// dropped is dropped with the receiver.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.lock();
        if let Some(value) = state.value.take() {
            return Poll::Ready(Ok(value));
        }
        if !state.tx_alive {
            return Poll::Ready(Err(RecvError));
        }
        match &state.rx_waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => state.rx_waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (value, waker) = {
            let mut state = self.shared.lock();
            state.rx_alive = false;
            state.rx_waker = None;
            (state.value.take(), state.tx_waker.take())
        };
        // Both outside the lock: the value's destructor may use this channel.
        drop(value);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("oneshot::Sender")
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("oneshot::Receiver { .. }")
    }
}
