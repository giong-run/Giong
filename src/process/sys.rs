//! The Linux calls behind child processes: pidfds and `waitid`.
//!
//! Kept apart so that another platform's driver can supply its own (kqueue
//! has `EVFILT_PROC` for the same job).

use std::io;
use std::mem;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// `P_PIDFD` for `waitid` (Linux 5.4), not in the libc crate.
const P_PIDFD: libc::idtype_t = 3;

/// A pidfd for the child `pid` (Linux 5.3): readable once the child exits,
/// and it keeps naming that child even after another process reuses its
/// number.
pub(super) fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: a plain system call; the result is a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor we own (pidfds are always close-on-exec).
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Sends `signal` to the process `pidfd` names. `ESRCH` (it has exited) is
/// reported as such.
pub(super) fn pidfd_send_signal(pidfd: RawFd, signal: i32) -> io::Result<()> {
    // SAFETY: a plain system call with no pointers (no `siginfo`).
    let res = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reaps the child `pidfd` names if it has exited, without waiting.
pub(super) fn try_reap(pidfd: RawFd) -> io::Result<Option<ExitStatus>> {
    loop {
        // SAFETY: all-zero is a valid `siginfo_t`, which waitid fills in.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        // SAFETY: a valid pointer to a `siginfo_t`.
        let res = unsafe {
            libc::waitid(
                P_PIDFD,
                pidfd as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if res < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: waitid filled in the child fields, or left them zero.
        let pid = unsafe { info.si_pid() };
        if pid == 0 {
            return Ok(None); // still running
        }
        // SAFETY: as above.
        let status = unsafe { info.si_status() };
        let raw = match info.si_code {
            libc::CLD_EXITED => (status & 0xff) << 8,
            libc::CLD_KILLED => status & 0x7f,
            libc::CLD_DUMPED => (status & 0x7f) | 0x80,
            // WEXITED reports only the three above.
            _ => (status & 0xff) << 8,
        };
        return Ok(Some(ExitStatus::from_raw(raw)));
    }
}
