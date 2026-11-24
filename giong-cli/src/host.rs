//! Facts about the machine, read without `unsafe`: from `/proc` on Linux and
//! from the tools on the `PATH` elsewhere. Each reader returns `None` when it
//! cannot tell, and the parsers are separate so tests can feed them text.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// How long a tool may take to answer (a rustup lock or a network mount can
/// hang one forever).
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs a program and gives its standard output, if it ran and succeeded
/// within [`TOOL_TIMEOUT`].
pub fn output(program: &str, args: &[&str]) -> Option<String> {
    run(Command::new(program).args(args)).ok()
}

/// [`output`], with `dir` as the working directory (a rust-toolchain file
/// there picks the toolchain).
pub fn output_in(dir: &Path, program: &str, args: &[&str]) -> Option<String> {
    run(Command::new(program).args(args).current_dir(dir)).ok()
}

/// Why a tool gave no output.
#[derive(Debug)]
pub enum ToolError {
    /// It could not be started.
    Missing(#[allow(dead_code)] std::io::Error),
    /// It exited with a failure; its standard error.
    Failed(String),
    /// It took longer than [`TOOL_TIMEOUT`] and was stopped.
    TimedOut,
}

/// Programs that already took longer than [`TOOL_TIMEOUT`]: asked again, they
/// answer `TimedOut` at once, so one hung tool (a rustc behind a stuck rustup
/// lock) costs one timeout per command, not one per call.
static TIMED_OUT: std::sync::Mutex<Vec<std::ffi::OsString>> = std::sync::Mutex::new(Vec::new());

fn timed_out_before(program: &std::ffi::OsStr) -> bool {
    TIMED_OUT
        .lock()
        .map(|t| t.iter().any(|p| p == program))
        .unwrap_or(false)
}

/// Runs `cmd` with its output captured, stopped after [`TOOL_TIMEOUT`].
pub fn run(cmd: &mut Command) -> Result<String, ToolError> {
    run_within(cmd, TOOL_TIMEOUT)
}

fn run_within(cmd: &mut Command, timeout: Duration) -> Result<String, ToolError> {
    let program = cmd.get_program().to_os_string();
    if timed_out_before(&program) {
        return Err(ToolError::TimedOut);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ToolError::Missing)?;
    // Read both pipes on threads so a chatty tool cannot block on a full one.
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                if let Ok(mut t) = TIMED_OUT.lock() {
                    t.push(program);
                }
                return Err(ToolError::TimedOut);
            }
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if !status.success() {
        return Err(ToolError::Failed(
            String::from_utf8_lossy(&stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Whether a program can be started.
#[allow(dead_code)] // called by giong db and image (#220)
pub fn exists(program: &str, args: &[&str]) -> bool {
    !matches!(
        run(Command::new(program).args(args)),
        Err(ToolError::Missing(_))
    )
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

    #[cfg(unix)]
    #[test]
    fn a_tool_that_timed_out_is_not_waited_for_again() {
        // CI's docker job has no sleep(1) on the PATH.
        if !exists("sleep", &["0"]) {
            eprintln!("skipped: no sleep here");
            return;
        }
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        assert!(matches!(
            run_within(&mut cmd, Duration::from_millis(100)),
            Err(ToolError::TimedOut)
        ));
        let start = Instant::now();
        assert!(matches!(
            run_within(Command::new("sleep").arg("5"), Duration::from_secs(10)),
            Err(ToolError::TimedOut)
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
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
