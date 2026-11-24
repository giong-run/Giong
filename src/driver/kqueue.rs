//! The kqueue [`Poller`]: the readiness driver on macOS.
//!
//! kqueue reports reading and writing as two filters, so a descriptor is
//! registered with two changes (`EVFILT_READ` and `EVFILT_WRITE`, both
//! `EV_CLEAR`, which is kqueue's edge-triggered mode), and each event names
//! one direction. Closing a descriptor removes its filters, as with epoll.
//!
//! macOS adds timer slack (coalescing) to a wait's time limit, so a 2 ms
//! wait takes 2 to 4 ms. With `Builder::precise_timers`, a short wait does
//! not pass its limit to `kevent`: it arms a one-shot `EVFILT_TIMER` with
//! `NOTE_CRITICAL` instead, which the kernel fires on time, and blocks
//! until an event comes. That costs CPU and power (more interrupt
//! wake-ups), so it is off by default. A long wait keeps the plain limit
//! either way: its slack, about a millisecond, is under 1% of it.
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
        kevent, kqueue, EVFILT_READ, EVFILT_TIMER, EVFILT_USER, EVFILT_WRITE, EV_ADD, EV_CLEAR,
        EV_DELETE, EV_ERROR, EV_ONESHOT, EV_RECEIPT, NOTE_CRITICAL, NOTE_NSECONDS, NOTE_TRIGGER,
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
    pub(super) const EVFILT_TIMER: i16 = -7;
    pub(super) const EVFILT_USER: i16 = -11;
    pub(super) const NOTE_TRIGGER: u32 = 0x0100_0000;
    pub(super) const NOTE_NSECONDS: u32 = 0x0004;
    /// macOS only: libkqueue has no timer slack to turn off.
    pub(super) const NOTE_CRITICAL: u32 = 0;
    pub(super) const EV_ADD: u16 = 0x0001;
    pub(super) const EV_DELETE: u16 = 0x0002;
    pub(super) const EV_ONESHOT: u16 = 0x0010;
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
    /// Short timed waits use the wait timer (`Builder::precise_timers`).
    precise: bool,
    /// The wait timer is armed: it has neither fired nor been deleted.
    timer_armed: bool,
    /// The kernel refused the wait timer once; waits pass their limit to
    /// `kevent` from then on, slack and all.
    timer_refused: bool,
}

/// The identifier of the worker's wake-up among the queue's user events.
const WAKE_IDENT: usize = 0;

/// The identifier of the wait timer among the queue's timers.
const TIMER_IDENT: usize = 0;

/// Waits up to this long use the wait timer; longer ones the plain limit.
const TIMER_UP_TO: Duration = Duration::from_millis(100);

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
    const SKIP_EMPTY_POLL: bool = true;

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
            precise: false,
            timer_armed: false,
            timer_refused: false,
        })
    }

    fn set_precise_timers(&mut self, enable: bool) {
        // A timer armed while it was on is deleted by the next wait.
        self.precise = enable;
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
        // the descriptor is out of the set either way. What counts is the
        // read filter: once it is gone, the next wait adds both again (a
        // write filter left behind only reports once more), whereas a
        // `false` here would keep the descriptor counted as armed, and a
        // reader would wait on a filter that no longer exists.
        self.apply(&changes, &mut results).is_ok()
            && matches!(receipt_errno(&results[0]), 0 | libc::ENOENT)
    }

    fn wait(&mut self, timeout: Option<Duration>) -> usize {
        self.events.clear();
        // With precise timers on, a short limit above zero arms the wait
        // timer and blocks. A timer an earlier wait left armed (an event
        // ended that wait early) is deleted first, in the same call:
        // re-adding a timer that exists
        // does not restart it everywhere (libkqueue keeps the old time),
        // and the delete also drops an expiry that came but was not
        // collected. Any other wait takes such a timer back, or it would
        // end this one for nothing; a look (a zero limit) leaves it be.
        let mut changes = [change(
            TIMER_IDENT as RawFd,
            EVFILT_TIMER as i16,
            EV_DELETE as u16,
            0,
        ); 2];
        // A timer event can only come from a timer armed before this call
        // or added in it.
        let timer_in_play = self.timer_armed;
        let mut nchanges = usize::from(self.timer_armed);
        let mut limit = timeout;
        match timeout {
            Some(d) if d.is_zero() => nchanges = 0,
            Some(d) if self.precise && d <= TIMER_UP_TO && !self.timer_refused => {
                let ev = &mut changes[nchanges];
                ev.flags = (EV_ADD | EV_ONESHOT) as u16;
                ev.fflags = (NOTE_NSECONDS | NOTE_CRITICAL) as _;
                ev.data = d.as_nanos() as _;
                nchanges += 1;
                limit = None;
            }
            _ => {}
        }
        let adds = nchanges > 0 && changes[nchanges - 1].flags as u16 & EV_ADD as u16 != 0;
        let ts = limit.map(|d| libc::timespec {
            tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), |t| t as *const libc::timespec);
        let changes_ptr = if nchanges == 0 {
            std::ptr::null()
        } else {
            changes.as_ptr()
        };
        // SAFETY: the change list is null or `nchanges` live changes; the
        // event list has room for MAX_EVENTS events; `ts_ptr` is null or
        // points at a live timespec.
        let res = unsafe {
            kevent(
                self.kq.as_raw_fd(),
                changes_ptr,
                nchanges as libc::c_int,
                self.events.as_mut_ptr(),
                MAX_EVENTS as libc::c_int,
                ts_ptr,
            )
        };
        // The change list is applied before the wait, and before signals
        // are looked at: after EINTR too, the timer is as the changes left
        // it (a refused change comes back as an event, below).
        if nchanges > 0 {
            self.timer_armed = adds;
        }
        if res < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return 0;
            }
            super::event_loop_failed("kevent", e);
        }
        let mut n = res as usize;
        // SAFETY: the kernel initialised `n <= MAX_EVENTS` events.
        unsafe { self.events.set_len(n) };
        // The wait timer's own events are not for the caller: it firing
        // is the wait's time running out, and a change it refused comes
        // back as an event too. With no timer in play (precise timers off)
        // there is none, and the events are not looked at.
        let mut i = if timer_in_play || adds { 0 } else { n };
        while i < n {
            let ev = &self.events[i];
            if ev.filter as i16 == EVFILT_TIMER as i16 && ev.ident as usize == TIMER_IDENT {
                let flags = ev.flags as u16;
                if flags & EV_ERROR as u16 == 0 {
                    // It fired: the one-shot timer is gone.
                    self.timer_armed = false;
                } else if flags & EV_ADD as u16 != 0 {
                    // Adding it was refused (ENOMEM): timed waits pass
                    // their limit to `kevent` from now on.
                    self.timer_armed = false;
                    self.timer_refused = true;
                }
                // A refused delete: the timer was gone already, and what
                // the add in the same call did stands.
                self.events.swap_remove(i);
                n -= 1;
            } else {
                i += 1;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A queue with precise timers on.
    fn precise() -> Kqueue {
        let mut kq = Kqueue::new().unwrap();
        kq.set_precise_timers(true);
        kq
    }

    /// Precise timers are off by default: a short timed wait passes its
    /// limit to `kevent` and arms no timer.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_short_wait_arms_no_timer_by_default() {
        let mut kq = Kqueue::new().unwrap();
        let limit = Duration::from_millis(20);
        let start = Instant::now();
        assert_eq!(kq.wait(Some(limit)), 0);
        assert!(start.elapsed() >= limit, "{:?}", start.elapsed());
        let wake = kq.waker().unwrap();
        wake.ring();
        assert_eq!(kq.wait(Some(limit)), 1);
        assert!(!kq.timer_armed);
    }

    /// Turning precise timers off takes back a timer left armed: it does
    /// not end the next wait.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn turning_precise_timers_off_takes_back_an_armed_timer() {
        let mut kq = precise();
        let wake = kq.waker().unwrap();
        wake.ring();
        assert_eq!(kq.wait(Some(Duration::from_millis(30))), 1);
        assert!(kq.timer_armed);
        kq.set_precise_timers(false);
        let limit = Duration::from_millis(200);
        let start = Instant::now();
        assert_eq!(kq.wait(Some(limit)), 0);
        assert!(
            start.elapsed() >= limit,
            "the old timer ended the wait after {:?}",
            start.elapsed()
        );
        assert!(!kq.timer_armed);
    }

    /// A timed wait with nothing to report lasts its limit and reports no
    /// event: the wait timer's own event is not passed on.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_timed_wait_lasts_its_limit_and_reports_nothing() {
        let mut kq = precise();
        let limit = Duration::from_millis(20);
        for _ in 0..3 {
            let start = Instant::now();
            assert_eq!(kq.wait(Some(limit)), 0);
            assert!(start.elapsed() >= limit, "{:?}", start.elapsed());
            assert!(!kq.timer_armed);
        }
        assert!(!kq.timer_refused);
    }

    /// A timed wait that an event ends early leaves its timer armed. A
    /// later wait without a limit takes it back: the old timer does not
    /// end that wait.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_timer_left_armed_does_not_end_a_later_wait() {
        let mut kq = precise();
        let wake = kq.waker().unwrap();
        wake.ring();
        assert_eq!(kq.wait(Some(Duration::from_millis(30))), 1);
        assert_eq!(kq.event(0).0, WAKE);
        assert!(kq.timer_armed);
        let ringer = {
            let wake = wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                wake.ring();
            })
        };
        let start = Instant::now();
        assert_eq!(kq.wait(None), 1);
        assert_eq!(kq.event(0).0, WAKE);
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "the old timer ended the wait after {:?}",
            start.elapsed()
        );
        assert!(!kq.timer_armed);
        ringer.join().unwrap();
    }

    /// A short wait after a long one that an event ended early lasts the
    /// short limit: the long wait's timer is replaced, not kept (libkqueue
    /// kept the old time when the timer was only added again).
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_short_wait_after_a_long_one_ended_early_lasts_its_own_limit() {
        let mut kq = precise();
        let wake = kq.waker().unwrap();
        for long in [TIMER_UP_TO, Duration::from_secs(5)] {
            wake.ring();
            assert_eq!(kq.wait(Some(long)), 1);
            assert_eq!(kq.event(0).0, WAKE);
            // Only a wait up to the bound arms the timer.
            assert_eq!(kq.timer_armed, long <= TIMER_UP_TO, "{long:?}");
            let short = Duration::from_millis(5);
            let start = Instant::now();
            assert_eq!(kq.wait(Some(short)), 0);
            let took = start.elapsed();
            assert!(took >= short, "{took:?}");
            assert!(took < Duration::from_millis(80), "after {long:?}: {took:?}");
            assert!(!kq.timer_armed);
        }
        assert!(!kq.timer_refused);
    }

    /// A long wait passes its limit to `kevent` and takes back a timer an
    /// earlier short wait left armed.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_long_wait_takes_back_a_timer_left_armed() {
        let mut kq = precise();
        let wake = kq.waker().unwrap();
        wake.ring();
        assert_eq!(kq.wait(Some(Duration::from_millis(30))), 1);
        assert!(kq.timer_armed);
        let ringer = {
            let wake = wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                wake.ring();
            })
        };
        let start = Instant::now();
        assert_eq!(kq.wait(Some(Duration::from_secs(5))), 1);
        assert_eq!(kq.event(0).0, WAKE);
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "the old timer ended the wait after {:?}",
            start.elapsed()
        );
        assert!(!kq.timer_armed);
        ringer.join().unwrap();
    }

    /// A look (a zero limit) never waits and arms no timer.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call kevent")]
    fn a_look_arms_no_timer() {
        let mut kq = precise();
        let start = Instant::now();
        assert_eq!(kq.wait(Some(Duration::ZERO)), 0);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!kq.timer_armed);
    }
}
