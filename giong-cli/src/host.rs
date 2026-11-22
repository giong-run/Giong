//! Facts about the machine, read without `unsafe`: from `/proc` on Linux and
//! from the tools on the `PATH` elsewhere. Each reader returns `None` when it
//! cannot tell, and the parsers are separate so tests can feed them text.

use std::process::Command;

/// A version such as `1.80.0` or a kernel's `6.18.44-fc-v726`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The first `x.y[.z]` in the text (`rustc 1.97.0 (2d8144b78 2026-...)`, `6.18.44-fc`).
pub fn parse_version(text: &str) -> Option<Version> {
    for word in text.split(|c: char| c.is_whitespace() || c == '-' || c == '+') {
        let mut parts = word.split('.');
        let a = parts.next().and_then(|p| p.parse().ok());
        let b = parts.next().and_then(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        if let (Some(a), Some(b)) = (a, b) {
            let c = parts
                .next()
                .and_then(|p| {
                    let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                    digits.parse().ok()
                })
                .unwrap_or(0);
            return Some(Version(a, b, c));
        }
    }
    None
}

/// Runs a program and gives its standard output, if it ran and succeeded.
pub fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a program can be started.
pub fn exists(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

pub fn rustc_version() -> Option<Version> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    parse_version(&output(&rustc, &["--version"])?)
}

pub fn kernel_version() -> Option<Version> {
    if cfg!(target_os = "linux") {
        parse_version(&std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?)
    } else {
        None
    }
}

/// `/proc/sys/kernel/io_uring_disabled` (Linux 6.6+): 0, 1 or 2.
pub fn io_uring_disabled() -> Option<u8> {
    std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A limit from `/proc/self/limits`: (soft, hard), `None` meaning unlimited.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limit {
    pub soft: Option<u64>,
    pub hard: Option<u64>,
}

/// Finds the row that starts with `name` (`Max open files`, `Max locked memory`).
pub fn parse_limit(limits: &str, name: &str) -> Option<Limit> {
    let row = limits.lines().find(|l| l.starts_with(name))?;
    let mut cols = row[name.len()..].split_whitespace();
    let value = |w: Option<&str>| -> Option<Option<u64>> {
        match w? {
            "unlimited" => Some(None),
            n => n.parse().ok().map(Some),
        }
    };
    Some(Limit {
        soft: value(cols.next())?,
        hard: value(cols.next())?,
    })
}

pub fn limit(name: &str) -> Option<Limit> {
    parse_limit(&std::fs::read_to_string("/proc/self/limits").ok()?, name)
}

/// `Seccomp:` from `/proc/self/status`: 0 none, 1 strict, 2 filter.
pub fn parse_seccomp(status: &str) -> Option<u8> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp:"))
        .and_then(|v| v.trim().parse().ok())
}

pub fn seccomp() -> Option<u8> {
    parse_seccomp(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// Whether this runs in a container (Docker, Podman, containerd, Kubernetes).
pub fn in_container() -> bool {
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
    {
        return true;
    }
    std::fs::read_to_string("/proc/1/cgroup")
        .map(|c| {
            ["docker", "containerd", "kubepods", "libpod"]
                .iter()
                .any(|k| c.contains(k))
        })
        .unwrap_or(false)
}

/// Whether this process is root on the host (root in a container has lost
/// `CAP_IPC_LOCK`, so it is held to the locked-memory limit like anyone).
pub fn is_root_outside_container() -> bool {
    let root = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().nth(1).map(|e| e == "0"))
        })
        .unwrap_or(false);
    root && !in_container()
}

/// How many CPUs this process may use.
pub fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("rustc 1.97.0 (2d8144b78 2026-06-30)"),
            Some(Version(1, 97, 0))
        );
        assert_eq!(parse_version("6.18.44-fc-v726\n"), Some(Version(6, 18, 44)));
        assert_eq!(parse_version("6.1-rc3"), Some(Version(6, 1, 0)));
        assert_eq!(
            parse_version("rustc 1.80.0-nightly (x 2024)"),
            Some(Version(1, 80, 0))
        );
        assert_eq!(parse_version("no version"), None);
        assert!(Version(1, 79, 9) < Version(1, 80, 0));
    }

    #[test]
    fn limits() {
        let text = "\
Limit                     Soft Limit           Hard Limit           Units
Max locked memory         8388608              8388608              bytes
Max open files            1024                 524288               files
Max processes             unlimited            unlimited            processes
";
        assert_eq!(
            parse_limit(text, "Max open files"),
            Some(Limit {
                soft: Some(1024),
                hard: Some(524288)
            })
        );
        assert_eq!(
            parse_limit(text, "Max processes"),
            Some(Limit {
                soft: None,
                hard: None
            })
        );
        assert_eq!(parse_limit(text, "Max nice priority"), None);
    }

    #[test]
    fn seccomp_mode() {
        assert_eq!(
            parse_seccomp("Name:\tx\nSeccomp:\t2\nSeccomp_filters:\t1\n"),
            Some(2)
        );
        assert_eq!(parse_seccomp("Name:\tx\n"), None);
    }
}
