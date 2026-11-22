//! A single-shot I/O operation as a future, on either driver.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::driver::{Io, Start, ZeroCopyHold};
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
    /// Driver key while in flight (32 bits, so that an operation, which
    /// every I/O future holds, stays small).
    key: Option<u32>,
    data: Option<T>,
    /// What a zero-copy send leaves with the driver when it returns at its
    /// result (a [`ZeroCopyHold`](crate::driver::ZeroCopyHold)).
    hold: Option<Box<ZeroCopyHold>>,
    /// The worker whose driver owns `key`.
    worker_uid: usize,
    /// Operations are tied to one worker thread.
    _not_send: PhantomData<*const ()>,
}

// `Op` never hands out a pinned reference to `T`; it only moves it in and out.
impl<T: 'static> Unpin for Op<T> {}

impl<T: 'static> Op<T> {
    /// # Safety
    ///
    /// Every pointer in `io` must point into memory owned by `data` (or into
    /// `'static` memory) at an address that does not change when `data` moves,
    /// and stays valid when `data` moves: heap memory reached through a raw
    /// pointer or a `Vec`, an [`IoBuf`](crate::buf::IoBuf), or a
    /// [`KernelBox`](crate::kbox::KernelBox), never a pointer taken from a
    /// `Box` that then moves.
    pub(crate) unsafe fn new(io: Io, data: T) -> Self {
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
    ///
    /// # Safety
    ///
    /// As for [`new`](Self::new), and what the kernel still reads after the
    /// result must be owned by `hold`.
    pub(crate) unsafe fn with_hold(io: Io, data: T, hold: Option<Box<ZeroCopyHold>>) -> Self {
        // SAFETY: the caller upholds `new`'s contract.
        let mut op = unsafe { Self::new(io, data) };
        op.hold = hold;
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
                                this.key = Some(
                                    u32::try_from(key).expect("giong: 2^32 operations in flight"),
                                );
                                this.worker_uid = w.shared.uid;
                                return Poll::Pending;
                            }
                        }
                    }
                    Some(key) => {
                        if this.worker_uid != w.shared.uid {
                            drop(driver);
                            wrong_worker();
                        }
                        match driver.poll_op(key as usize, cx.waker(), &mut this.hold) {
                            Poll::Ready(done) => {
                                this.key = None;
                                done
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                };
                if res == -crate::sys::errno::EAGAIN
                    && this.io.is_some_and(|io| io.waits_when_not_ready())
                {
                    // io_uring on a non-blocking socket: it gave up waiting
                    // (Linux 6.0 and 6.1 do after a second `EAGAIN`). Nothing
                    // was transferred; submitting again waits afresh.
                    continue;
                }
                if res == -crate::sys::errno::EOPNOTSUPP {
                    if let Some(copying) = this.io.and_then(|io| io.copying()) {
                        // A zero-copy send the socket refuses (kernel TLS):
                        // nothing was sent, so the same bytes go again with
                        // a copy. The hold stays here, still keeping alive
                        // what the send points at, and its report turns
                        // zero-copy off for the socket's later sends.
                        if let Some(hold) = &this.hold {
                            hold.copied.set(true);
                        }
                        this.io = Some(copying);
                        continue;
                    }
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
        let result_is_fd = self.io.is_some_and(|io| io.result_is_fd());
        let finish = self.io.is_some_and(|io| io.finishes_when_dropped());
        let handed_over = runtime::try_with_worker(|w| {
            if w.shared.uid != self.worker_uid {
                return false;
            }
            let data = data.take().map(|d| Box::new(d) as Box<dyn Any>);
            match w.driver.try_borrow_mut() {
                Ok(mut driver) => driver.cancel_op(key as usize, data, result_is_fd, finish),
                // Dropped by code the driver runs (a waker's `wake` or
                // `drop`): cancelled at the next turn, still on this worker.
                Err(_) => w.defer_cancel(key as usize, data, result_is_fd, finish),
            }
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

#[cold]
#[inline(never)]
fn wrong_worker() -> ! {
    panic!(
        "{}",
        crate::error::Error::new(
            crate::error::Code::WrongWorker,
            "a future that started an I/O operation on one giong worker was polled on another \
             worker or runtime",
            "await a giong future on the worker that first polled it. Send data, not futures, \
             between workers and threads: Handle::spawn_on builds the future on the worker that \
             runs it.",
        )
    )
}
