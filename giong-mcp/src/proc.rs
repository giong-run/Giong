//! The process as the system sees it: from `/proc/self` on Linux, from
//! `proc_pidinfo` on macOS.

#[cfg(target_os = "linux")]
use std::fs;

use serde_json::{json, Value};

#[cfg(target_os = "linux")]
pub(crate) fn process() -> Value {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| -> Option<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| v.parse().ok())
    };
    let open_fds = fs::read_dir("/proc/self/fd").map(|d| d.count()).ok();
    let fd_limit = fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|limits| {
            limits
                .lines()
                .find(|l| l.starts_with("Max open files"))
                .and_then(|l| l.split_whitespace().nth(3))
                .and_then(|v| v.parse::<u64>().ok())
        });
    json!({
        "pid": std::process::id(),
        "uptime_s": uptime_secs(),
        "rss_bytes": field("VmRSS:").map(|kb| kb * 1024),
        "threads": field("Threads:"),
        "open_fds": open_fds,
        "fd_limit": fd_limit,
    })
}

/// Seconds since the process started: its start time in clock ticks after
/// boot (`/proc/self/stat`, field 22) against the time since boot.
#[cfg(target_os = "linux")]
fn uptime_secs() -> Option<f64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // The command name, field 2, is in parentheses and may contain spaces.
    let after = &stat[stat.rfind(')')? + 1..];
    let start_ticks: f64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let since_boot: f64 = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // USER_HZ is 100 on every architecture Linux supports today.
    let up = since_boot - start_ticks / 100.0;
    Some((up.max(0.0) * 10.0).round() / 10.0)
}

/// One `proc_pidinfo` record of this process, or `None` if the system
/// refuses it.
#[cfg(target_os = "macos")]
fn pidinfo<T>(flavor: libc::c_int) -> Option<T> {
    let mut info = std::mem::MaybeUninit::<T>::zeroed();
    let size = std::mem::size_of::<T>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes, and the call
    // writes at most that many.
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            flavor,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: the record was filled in whole; these records are plain
    // integers and byte arrays, valid for any bytes.
    (written == size).then(|| unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
pub(crate) fn process() -> Value {
    let task = pidinfo::<libc::proc_taskinfo>(libc::PROC_PIDTASKINFO);
    // The directory lists this process's descriptors, the one that reads
    // the directory among them, as on Linux.
    let open_fds = std::fs::read_dir("/dev/fd").map(|d| d.count()).ok();
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a writable `rlimit`.
    let fd_limit = (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0)
        .then_some(limit.rlim_cur);
    json!({
        "pid": std::process::id(),
        "uptime_s": uptime_secs(),
        "rss_bytes": task.as_ref().map(|t| t.pti_resident_size),
        "threads": task.as_ref().map(|t| t.pti_threadnum),
        "open_fds": open_fds,
        "fd_limit": fd_limit,
    })
}

/// Seconds since the process started, from the start time the kernel
/// recorded for it.
#[cfg(target_os = "macos")]
fn uptime_secs() -> Option<f64> {
    let bsd = pidinfo::<libc::proc_bsdinfo>(libc::PROC_PIDTBSDINFO)?;
    let started =
        std::time::Duration::new(bsd.pbi_start_tvsec, (bsd.pbi_start_tvusec as u32) * 1000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let up = now.saturating_sub(started).as_secs_f64();
    Some((up * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_this_process() {
        let p = super::process();
        assert_eq!(p["pid"], std::process::id());
        assert!(p["rss_bytes"].as_u64().unwrap() > 0);
        assert!(p["threads"].as_u64().unwrap() >= 1);
        assert!(p["open_fds"].as_u64().unwrap() >= 3);
        let up = p["uptime_s"].as_f64().unwrap();
        assert!((0.0..3600.0).contains(&up), "{up}");
    }
}
