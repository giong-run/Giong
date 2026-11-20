//! An operation as a future, and a one-thread event loop to run it: the
//! shape of `src/op.rs` and of a worker's loop, cut down to what the
//! driver's tests need.

use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use crate::driver::{Bell, Driver, Io, Park, Start};

thread_local! {
    static DRIVER: RefCell<Option<Driver>> = const { RefCell::new(None) };
}

/// Run `f` with this thread's driver. Panics outside [`block_on`].
pub fn with_driver<R>(f: impl FnOnce(&mut Driver) -> R) -> R {
    DRIVER.with(|d| f(d.borrow_mut().as_mut().expect("no driver on this thread")))
}

/// One operation, one result. `T` owns what the operation points at; it
/// comes back with the result, or goes to the driver if the future is
/// dropped first.
pub struct Op<T: 'static> {
    io: Option<Io>,
    key: Option<usize>,
    data: Option<T>,
    finish_when_dropped: bool,
    _not_send: PhantomData<*const ()>,
}

impl<T: 'static> Unpin for Op<T> {}

impl<T: 'static> Op<T> {
    /// # Safety
    ///
    /// Every pointer in `io` points into memory owned by `data` at an
    /// address that does not change when `data` moves, and the handle in
    /// `io` is registered with this thread's driver.
    pub unsafe fn new(io: Io, data: T) -> Self {
        Self {
            io: Some(io),
            key: None,
            data: Some(data),
            finish_when_dropped: false,
            _not_send: PhantomData,
        }
    }

    /// Let the operation run to its end if the future is dropped, instead
    /// of cancelling it.
    pub fn finish_when_dropped(mut self) -> Self {
        self.finish_when_dropped = true;
        self
    }
}

impl<T: 'static> Future for Op<T> {
    type Output = (i32, T);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let res = with_driver(|driver| match this.key {
            None => {
                let io = this.io.take().expect("operation polled after completion");
                // SAFETY: `new`'s caller guarantees the pointers and handle.
                match unsafe { driver.start(&io, cx.waker()) } {
                    Start::Done(res) => Poll::Ready(res),
                    Start::Submitted(key) => {
                        this.key = Some(key);
                        Poll::Pending
                    }
                }
            }
            Some(key) => {
                let res = driver.poll_op(key, cx.waker());
                if res.is_ready() {
                    this.key = None;
                }
                res
            }
        });
        res.map(|res| (res, this.data.take().expect("operation resources")))
    }
}

impl<T: 'static> Drop for Op<T> {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        let data = self.data.take().map(|d| Box::new(d) as Box<dyn Any>);
        let finish = self.finish_when_dropped;
        let handed = DRIVER.with(|d| match d.try_borrow_mut() {
            Ok(mut d) => match d.as_mut() {
                Some(driver) => {
                    driver.cancel_op(key, data, finish);
                    None
                }
                None => Some(data),
            },
            Err(_) => Some(data),
        });
        if let Some(data) = handed {
            // No driver to keep it until the kernel is done: leak it.
            std::mem::forget(data);
        }
    }
}

struct Flag {
    woken: AtomicBool,
    bell: Bell,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        if !self.woken.swap(true, Ordering::SeqCst) {
            // Harmless on the loop's own thread, needed from any other.
            self.bell.ring();
        }
    }
}

/// Run `future` to its end on this thread with `driver`, then hand the
/// driver back.
pub fn block_on<F: Future>(driver: Driver, future: F) -> (F::Output, Driver) {
    let flag = Arc::new(Flag {
        woken: AtomicBool::new(true),
        bell: driver.bell(),
    });
    DRIVER.with(|d| *d.borrow_mut() = Some(driver));
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    let out = loop {
        if flag.woken.swap(false, Ordering::SeqCst) {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                break out;
            }
        }
        if !flag.woken.load(Ordering::SeqCst) {
            with_driver(|d| d.park(Park::Wait(Some(Duration::from_secs(30)))));
        }
    };
    let driver = DRIVER.with(|d| d.borrow_mut().take()).expect("driver");
    (out, driver)
}

/// Wait `duration`, entering the kernel like an idle worker.
pub async fn sleep(duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    std::future::poll_fn(|cx| {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Poll::Ready(());
        }
        with_driver(|d| d.park(Park::Wait(Some(left))));
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}
