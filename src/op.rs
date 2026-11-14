//! A single-shot I/O operation as a future, on either driver.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::driver::{Io, Start};
use crate::runtime;

/// One operation → one result.
///
/// `T` are the resources the operation points at (typically the buffer).
/// They are owned by the future while the operation is in flight and
/// returned together with the result. If the future is dropped first,
/// ownership moves to the driver until nothing can use them any more (see
/// the `driver` module docs).
pub(crate) struct Op<T: 'static> {
    io: Option<Io>,
    /// Driver key while in flight.
    key: Option<usize>,
    data: Option<T>,
    /// What a zero-copy send leaves with the driver when it returns at its
    /// result (a [`ZeroCopyHold`](crate::driver::ZeroCopyHold)).
    hold: Option<Box<dyn Any>>,
    /// The worker whose driver owns `key`.
    worker_uid: usize,
    /// Operations are tied to one worker thread.
    _not_send: PhantomData<*const ()>,
}

// `Op` never hands out a pinned reference to `T`; it only moves it in and out.
impl<T: 'static> Unpin for Op<T> {}

impl<T: 'static> Op<T> {
    /// # Safety contract (upheld by callers in this crate)
    ///
    /// Every pointer in `io` must point into memory owned by `data` (or into
    /// `'static` memory) at an address that does not change when `data` moves.
    pub(crate) fn new(io: Io, data: T) -> Self {
        Self {
            io: Some(io),
            key: None,
            data: Some(data),
            hold: None,
            worker_uid: 0,
            _not_send: PhantomData,
        }
    }

    /// A zero-copy send that completes once it has its result, rather than
    /// once the kernel stops reading its buffers: `hold` keeps what the
    /// kernel may still read alive, with the driver, until then.
    pub(crate) fn with_hold(io: Io, data: T, hold: Box<dyn Any>) -> Self {
        let mut op = Self::new(io, data);
        op.hold = Some(hold);
        op
    }
}

impl<T: 'static> Future for Op<T> {
    /// `(result, completion flags, resources)`.
    type Output = (i32, u32, T);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        runtime::with_worker(|w| {
            let mut driver = w.driver.borrow_mut();
            loop {
                let (res, flags) = match this.key {
                    None => {
                        let io = this.io.expect("giong: operation polled after completion");
                        match driver.start(&io, cx.waker()) {
                            Start::Done(res, flags) => (res, flags),
                            Start::Submitted(key) => {
                                this.key = Some(key);
                                this.worker_uid = w.shared.uid;
                                return Poll::Pending;
                            }
                        }
                    }
                    Some(key) => {
                        assert_eq!(
                            this.worker_uid, w.shared.uid,
                            "giong: operation polled on a different runtime"
                        );
                        match driver.poll_op(key, cx.waker(), &mut this.hold) {
                            Poll::Ready(done) => {
                                this.key = None;
                                done
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                };
                if res == -libc::EAGAIN && this.io.is_some_and(|io| io.waits_when_not_ready()) {
                    // io_uring on a non-blocking socket: it gave up waiting
                    // (Linux 6.0 and 6.1 do after a second `EAGAIN`). Nothing
                    // was transferred; submitting again waits afresh.
                    continue;
                }
                this.io = None;
                let data = this.data.take().expect("operation resources");
                return Poll::Ready((res, flags, data));
            }
        })
    }
}

impl<T: 'static> Drop for Op<T> {
    fn drop(&mut self) {
        // Never submitted, or already completed: nothing is in flight.
        let Some(key) = self.key.take() else { return };
        let mut data = self.data.take().map(|data| (data, self.hold.take()));
        let handed_over = runtime::try_with_worker(|w| {
            if w.shared.uid != self.worker_uid {
                return false;
            }
            let Ok(mut driver) = w.driver.try_borrow_mut() else {
                return false;
            };
            driver.cancel_op(key, data.take().map(|d| Box::new(d) as Box<dyn Any>));
            true
        })
        .unwrap_or(false);
        if !handed_over {
            // The driver is unreachable, so we cannot know when the kernel is
            // done with this memory. Leaking it is the only safe option.
            mem::forget(data);
        }
    }
}
