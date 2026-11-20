//! The kqueue [`Poller`]: the readiness driver on macOS.
//!
//! kqueue reports reading and writing as two filters, so a descriptor is
//! registered with two changes (`EVFILT_READ` and `EVFILT_WRITE`, both
//! `EV_CLEAR`, which is kqueue's edge-triggered mode), and each event names
//! one direction. Closing a descriptor removes its filters, as with epoll.
//!
//! Built on Linux too with `--cfg giong_libkqueue`, against libkqueue (a
//! kqueue written over epoll), so that this file can be tested without a
//! Mac. That build is a test aid only.

// The constants are `i16`/`u16` from libc on macOS and from our own
// declarations for libkqueue; the casts keep one spelling for both.
#![allow(clippy::unnecessary_cast)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use super::readiness::{Poller, MAX_EVENTS, WAKE};

#[cfg(target_os = "macos")]
mod ffi {
    pub(super) use libc::{
        kevent, kqueue, EVFILT_READ, EVFILT_USER, EVFILT_WRITE, EV_ADD, EV_CLEAR, EV_DELETE,
        EV_ERROR, EV_RECEIPT, NOTE_TRIGGER,
    };
    pub(super) type Kevent = libc::kevent;
}

#[cfg(all(giong_libkqueue, not(target_os = "macos")))]
mod ffi {
    //! libkqueue's `sys/event.h`: the BSD layout and values.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Kevent {
        pub(super) ident: libc::uintptr_t,
        pub(super) filter: i16,
        pub(super) flags: u16,
        pub(super) fflags: u32,
        pub(super) data: libc::intptr_t,
        pub(super) udata: *mut libc::c_void,
    }
    pub(super) const EVFILT_READ: i16 = -1;
    pub(super) const EVFILT_WRITE: i16 = -2;
    pub(super) const EVFILT_USER: i16 = -11;
    pub(super) const NOTE_TRIGGER: u32 = 0x0100_0000;
    pub(super) const EV_ADD: u16 = 0x0001;
    pub(super) const EV_DELETE: u16 = 0x0002;
    pub(super) const EV_CLEAR: u16 = 0x0020;
    pub(super) const EV_RECEIPT: u16 = 0x0040;
    pub(super) const EV_ERROR: u16 = 0x4000;
    #[link(name = "kqueue")]
    extern "C" {
        pub(super) fn kqueue() -> libc::c_int;
        pub(super) fn kevent(
            kq: libc::c_int,
            changelist: *const Kevent,
            nchanges: libc::c_int,
            eventlist: *mut Kevent,
            nevents: libc::c_int,
            timeout: *const libc::timespec,
        ) -> libc::c_int;
    }
}

use ffi::*;

fn change(fd: RawFd, filter: i16, flags: u16, token: u64) -> Kevent {
    // SAFETY: all-zero is a valid `kevent` (integers and a null pointer).
    let mut ev: Kevent = unsafe { std::mem::zeroed() };
    ev.ident = fd as _;
    ev.filter = filter as _;
    ev.flags = flags as _;
    ev.udata = token as usize as *mut libc::c_void;
    ev
}

pub(crate) struct Kqueue {
    /// Shared with the worker's [`Wake`] handles, which may outlive the
    /// driver: the queue is closed when the last of them is gone.
    kq: Arc<OwnedFd>,
    events: Vec<Kevent>,
}

/// The identifier of the worker's wake-up among the queue's user events.
const WAKE_IDENT: usize = 0;

/// Wakes the worker that waits on a kqueue: triggers the queue's user event
/// (`EVFILT_USER`), which needs no descriptor of its own and no draining.
pub(crate) struct Wake {
    kq: Arc<OwnedFd>,
}

impl Wake {
    /// Triggers the user event. Never blocks: `kevent` with no event list
    /// only records the change. Triggering an event that is already
    /// triggered is one wake-up, and one that arrives while the worker is
    /// awake is reported by its next wait.
    pub(crate) fn ring(&self) {
        let mut ev = change(WAKE_IDENT as RawFd, EVFILT_USER as i16, 0, WAKE);
        ev.fflags = NOTE_TRIGGER as _;
        // SAFETY: one valid change, no event list. The queue is open for as
        // long as `self.kq` lives.
        //
        // The result is ignored, as a failed write to a bell is: the only
        // errors are EINTR (the change is applied before signals are looked
        // at) and ENOMEM.
        unsafe {
            kevent(
                self.kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
    }
}

impl Kqueue {
    /// Applies `changes` and returns, for each, `0` or the `errno` it failed
    /// with (`EV_RECEIPT`: every change is reported, none is a real event).
    fn apply(&self, changes: &[Kevent], results: &mut [Kevent]) -> io::Result<()> {
        debug_assert_eq!(changes.len(), results.len());
        for ev in changes.iter() {
            debug_assert!(ev.flags as u16 & EV_RECEIPT as u16 != 0);
        }
        loop {
            // SAFETY: both lists are valid for the lengths passed.
            let n = unsafe {
                kevent(
                    self.kq.as_raw_fd(),
                    changes.as_ptr(),
                    changes.len() as libc::c_int,
                    results.as_mut_ptr(),
                    results.len() as libc::c_int,
                    std::ptr::null(),
                )
            };
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }
}

/// The `errno` a receipt reports, 0 for success.
fn receipt_errno(ev: &Kevent) -> i32 {
    if ev.flags as u16 & EV_ERROR as u16 != 0 {
        ev.data as i32
    } else {
        0
    }
}

impl Poller for Kqueue {
    fn new() -> io::Result<Self> {
        // libkqueue 2.3.1 can deadlock in its one-time start-up when several
        // threads make their first queue at once, so the Linux test build
        // makes queues one at a time. The kernel's kqueue needs nothing.
        #[cfg(giong_libkqueue)]
        static FIRST: std::sync::Mutex<()> = std::sync::Mutex::new(());
        #[cfg(giong_libkqueue)]
        let _one_at_a_time = FIRST.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        let kq = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        // kqueue descriptors are not inherited by a child process, but an
        // `exec` without a `fork` would keep it.
        // SAFETY: plain fcntl on our descriptor.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Self {
            kq,
            events: Vec::with_capacity(MAX_EVENTS),
        })
    }

    fn waker(&self) -> io::Result<super::WakeHandle> {
        // `EV_CLEAR`: the event is reported once per trigger and resets
        // itself, so there is nothing to drain.
        let changes = [change(
            WAKE_IDENT as RawFd,
            EVFILT_USER as i16,
            (EV_ADD | EV_CLEAR | EV_RECEIPT) as u16,
            WAKE,
        )];
        let mut results = changes;
        self.apply(&changes, &mut results)?;
        match receipt_errno(&results[0]) {
            0 => Ok(super::WakeHandle::Kqueue(Arc::new(Wake {
                kq: self.kq.clone(),
            }))),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }

    fn watch(&self, fd: RawFd, token: u64) -> io::Result<()> {
        let changes = [change(
            fd,
            EVFILT_READ as i16,
            (EV_ADD | EV_RECEIPT) as u16,
            token,
        )];
        let mut results = changes;
        self.apply(&changes, &mut results)?;
        match receipt_errno(&results[0]) {
            0 => Ok(()),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }

    fn register(&self, fd: RawFd, token: u64) -> Result<(), i32> {
        let flags = (EV_ADD | EV_CLEAR | EV_RECEIPT) as u16;
        let changes = [
            change(fd, EVFILT_READ as i16, flags, token),
            change(fd, EVFILT_WRITE as i16, flags, token),
        ];
        let mut results = changes;
        self.apply(&changes, &mut results)
            .map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
        // A descriptor that cannot be written to (the read end of a pipe)
        // may refuse the write filter; it is only ever read from. If both
        // filters fail, the descriptor cannot be waited for at all.
        match (receipt_errno(&results[0]), receipt_errno(&results[1])) {
            (0, _) | (_, 0) => Ok(()),
            (e, _) => Err(-e),
        }
    }

    fn rearm(&self, fd: RawFd, token: u64) -> i32 {
        // `EV_ADD` on a descriptor that is already readable or writable
        // posts the event, as `EPOLL_CTL_ADD` does.
        match self.register(fd, token) {
            Ok(()) => 0,
            Err(e) => e,
        }
    }

    fn unregister(&self, fd: RawFd) -> bool {
        let flags = (EV_DELETE | EV_RECEIPT) as u16;
        let changes = [
            change(fd, EVFILT_READ as i16, flags, 0),
            change(fd, EVFILT_WRITE as i16, flags, 0),
        ];
        let mut results = changes;
        // A filter that was never added (see `register`) reports ENOENT:
        // the descriptor is out of the set either way.
        self.apply(&changes, &mut results).is_ok()
            && results
                .iter()
                .all(|r| matches!(receipt_errno(r), 0 | libc::ENOENT))
    }

    fn wait(&mut self, timeout: Option<Duration>) -> usize {
        self.events.clear();
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), |t| t as *const libc::timespec);
        // SAFETY: the event list has room for MAX_EVENTS events; `ts_ptr` is
        // null or points at a live timespec.
        let res = unsafe {
            kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                self.events.as_mut_ptr(),
                MAX_EVENTS as libc::c_int,
                ts_ptr,
            )
        };
        if res < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return 0;
            }
            super::event_loop_failed("kevent", e);
        }
        let n = res as usize;
        // SAFETY: the kernel initialised `n <= MAX_EVENTS` events.
        unsafe { self.events.set_len(n) };
        n
    }

    fn event(&self, i: usize) -> (u64, bool, bool) {
        let ev = &self.events[i];
        let token = ev.udata as usize as u64;
        let flags = ev.flags as u16;
        if flags & EV_ERROR as u16 != 0 {
            return (token, true, true);
        }
        // End of stream (EV_EOF) comes on the filter of its direction, and
        // counts as ready: the next call reports the end or the error.
        let filter = ev.filter as i16;
        (
            token,
            filter == EVFILT_READ as i16,
            filter == EVFILT_WRITE as i16,
        )
    }
}
