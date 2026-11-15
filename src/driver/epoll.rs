//! The epoll [`Poller`]: the readiness driver on Linux.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use super::readiness::{Poller, MAX_EVENTS};
use crate::sys::neg_errno;

pub(crate) struct Epoll {
    epfd: OwnedFd,
    events: Vec<libc::epoll_event>,
    /// `epoll_pwait2` (Linux 5.11) works here; else `epoll_wait`, whose
    /// timeout is in whole milliseconds.
    pwait2: bool,
}

impl Poller for Epoll {
    fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: a fresh descriptor we own.
            epfd: unsafe { OwnedFd::from_raw_fd(fd) },
            events: Vec::with_capacity(MAX_EVENTS),
            pwait2: true,
        })
    }

    fn watch(&self, fd: RawFd, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: token,
        };
        // SAFETY: valid descriptors and a valid event.
        if unsafe { libc::epoll_ctl(self.epfd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn register(&self, fd: RawFd, token: u64) -> Result<(), i32> {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: token,
        };
        let epfd = self.epfd.as_raw_fd();
        // SAFETY: valid descriptors and a valid event.
        if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            let e = neg_errno();
            // Only possible if the number was registered before and the
            // registration outlived our record of it: take it over.
            if e != -libc::EEXIST
                // SAFETY: as above.
                || unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut ev) } < 0
            {
                return Err(if e == -libc::EEXIST { neg_errno() } else { e });
            }
        }
        Ok(())
    }

    fn wait(&mut self, timeout: Option<Duration>) -> usize {
        self.events.clear();
        let epfd = self.epfd.as_raw_fd();
        let buf = self.events.as_mut_ptr();
        let n = loop {
            let res = if self.pwait2 {
                let ts = timeout.map(|d| libc::timespec {
                    tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
                    tv_nsec: d.subsec_nanos() as libc::c_long,
                });
                let ts_ptr = ts
                    .as_ref()
                    .map_or(std::ptr::null(), |t| t as *const libc::timespec);
                // SAFETY: `buf` has room for MAX_EVENTS events; `ts_ptr` is
                // null or points at a live timespec.
                unsafe {
                    libc::syscall(
                        libc::SYS_epoll_pwait2,
                        epfd,
                        buf,
                        MAX_EVENTS as libc::c_int,
                        ts_ptr,
                        std::ptr::null::<libc::sigset_t>(),
                        0usize,
                    )
                }
            } else {
                // Whole milliseconds, rounded up so a timer is never early.
                let ms = timeout.map_or(-1, |d| {
                    let ms = d.as_nanos().div_ceil(1_000_000);
                    ms.min(i32::MAX as u128) as libc::c_int
                });
                // SAFETY: as above.
                unsafe {
                    libc::epoll_wait(epfd, buf, MAX_EVENTS as libc::c_int, ms) as libc::c_long
                }
            };
            if res >= 0 {
                break res as usize;
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => break 0,
                // A kernel older than 5.11, or a seccomp profile that does
                // not know the call: use epoll_wait from now on.
                Some(libc::ENOSYS | libc::EPERM) if self.pwait2 => self.pwait2 = false,
                _ => super::event_loop_failed("epoll_wait", io::Error::last_os_error()),
            }
        };
        // SAFETY: the kernel initialised `n <= MAX_EVENTS` events.
        unsafe { self.events.set_len(n) };
        n
    }

    fn event(&self, i: usize) -> (u64, bool, bool) {
        const READABLE: u32 =
            (libc::EPOLLIN | libc::EPOLLPRI | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR)
                as u32;
        const WRITABLE: u32 = (libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR) as u32;
        // Copied out: the struct is packed on x86-64.
        let (events, data) = (self.events[i].events, self.events[i].u64);
        (data, events & READABLE != 0, events & WRITABLE != 0)
    }
}
