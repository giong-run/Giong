//! The process as the system sees it: from `/proc/self` on Linux, from
//! `proc_pidinfo` on macOS, from the process's own counters on Windows.

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
    // No limit (`RLIM_INFINITY`) is `null`, as on Linux: the number is
    // beyond what JSON carries exactly.
    let fd_limit = (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0
        && limit.rlim_cur != libc::RLIM_INFINITY)
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

/// On Windows `open_fds` counts every handle (sockets, files, events,
/// threads and the rest), and `fd_limit` is `null`: Windows sets no limit a
/// process can reach in practice (16 million handles).
#[cfg(windows)]
pub(crate) fn process() -> Value {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    // SAFETY: all-zero is a valid value of this plain structure.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the pseudo handle of this process, and a structure of the size
    // given.
    let rss = (unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) } != 0)
        .then_some(counters.WorkingSetSize as u64);
    let mut handles = 0u32;
    // SAFETY: the pseudo handle of this process, and a local to write to.
    let open_fds = (unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) } != 0)
        .then_some(handles);
    json!({
        "pid": std::process::id(),
        "uptime_s": uptime_secs(),
        "rss_bytes": rss,
        "threads": threads(),
        "open_fds": open_fds,
        "fd_limit": Value::Null,
    })
}

/// The threads of this process, from a snapshot of the system's threads.
#[cfg(windows)]
fn threads() -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    let pid = std::process::id();
    // SAFETY: takes a snapshot of every thread; the handle is checked and
    // closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: all-zero is a valid value of this plain structure; its size
    // field is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut count = 0u64;
    // SAFETY: an open snapshot, and an entry of the size its field gives.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            count += 1;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: the snapshot is open and closed once.
    unsafe { CloseHandle(snapshot) };
    (count > 0).then_some(count)
}

/// Seconds since the process started, from the creation time the system
/// recorded for it.
#[cfg(windows)]
fn uptime_secs() -> Option<f64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    // SAFETY: the pseudo handle of this process, and four locals to write.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: a local to write.
    unsafe { GetSystemTimeAsFileTime(&mut now) };
    // Both count 100 ns since 1601.
    let up = ticks(now).saturating_sub(ticks(created)) as f64 / 1e7;
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
