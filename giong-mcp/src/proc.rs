//! The process as Linux sees it, from `/proc/self`.

use std::fs;

use serde_json::{json, Value};

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
