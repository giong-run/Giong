//! The limits a container, a pod or a service manager puts on the process:
//! the CPUs it may run on and its CPU quota, its memory, its processes and
//! threads, its open files. They explain what giong chose (how many workers)
//! and what will fail first, so an agent need not guess them from outside.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// What [`limits`] found; `None` where there is no limit or it cannot be read.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Limits {
    /// CPUs in the process's affinity mask (`--cpuset-cpus`, `taskset`).
    pub(crate) cpus: Option<u64>,
    /// The CPU quota, in CPUs (`--cpus`, `resources.limits.cpu`).
    pub(crate) cpu_quota: Option<f64>,
    /// The cgroup's memory limit, in bytes (`--memory`).
    pub(crate) memory: Option<u64>,
    /// The most processes and threads the cgroup may hold (`--pids-limit`).
    pub(crate) pids_max: Option<u64>,
    /// How many it holds now, every process of the container counted.
    pub(crate) pids_current: Option<u64>,
    /// The soft and hard limits on open files (`ulimit -n`, `ulimit -Hn`).
    pub(crate) open_files: Option<u64>,
    pub(crate) open_files_hard: Option<u64>,
}

pub(crate) fn limits() -> Limits {
    let own = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let mut l = limits_in(Path::new("/sys/fs/cgroup"), &own);
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    l.cpus = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .and_then(|list| count_cpus(list.trim()));
    let limits = fs::read_to_string("/proc/self/limits").unwrap_or_default();
    if let Some(line) = limits.lines().find(|l| l.starts_with("Max open files")) {
        let mut fields = line.split_whitespace().skip(3);
        l.open_files = fields.next().and_then(|v| v.parse().ok());
        l.open_files_hard = fields.next().and_then(|v| v.parse().ok());
    }
    l
}

/// The cgroup limits of a process whose `/proc/self/cgroup` reads `own`, with
/// the cgroup file systems mounted at `root`. As in giong-http's memory check,
/// every directory from the process's cgroup up to the mount's root is read
/// (in a container the path names the host's cgroup and may not exist), and
/// the smallest limit wins.
pub(crate) fn limits_in(root: &Path, own: &str) -> Limits {
    let mut l = Limits::default();
    for line in own.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = path.trim_start_matches('/');
        if controllers.is_empty() {
            // cgroup v2: one hierarchy for every controller.
            for dir in up(root, path) {
                smallest(&mut l.cpu_quota, quota_v2(&dir));
                smallest(&mut l.memory, number(&dir.join("memory.max")));
                if let Some(max) = number(&dir.join("pids.max")) {
                    if l.pids_max.map_or(true, |m| max < m) {
                        l.pids_max = Some(max);
                        l.pids_current = number(&dir.join("pids.current"));
                    }
                }
            }
            continue;
        }
        for controller in controllers.split(',') {
            // Mounted under the line's name ("cpu,cpuacct"), with a link per
            // controller on most systems.
            let mut base = root.join(controllers);
            if !base.is_dir() {
                base = root.join(controller);
            }
            match controller {
                "cpu" => up(&base, path).for_each(|dir| smallest(&mut l.cpu_quota, quota_v1(&dir))),
                "memory" => up(&base, path).for_each(|dir| {
                    smallest(&mut l.memory, number(&dir.join("memory.limit_in_bytes")))
                }),
                "pids" => {
                    for dir in up(&base, path) {
                        if let Some(max) = number(&dir.join("pids.max")) {
                            if l.pids_max.map_or(true, |m| max < m) {
                                l.pids_max = Some(max);
                                l.pids_current = number(&dir.join("pids.current"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    l
}

/// `base/path` and each directory above it, up to `base`.
fn up(base: &Path, path: &str) -> impl Iterator<Item = PathBuf> {
    let base = base.to_path_buf();
    let mut next = Some(base.join(path));
    std::iter::from_fn(move || {
        let dir = next.take()?;
        if dir != base && dir.starts_with(&base) {
            next = dir.parent().map(Path::to_path_buf);
        }
        Some(dir)
    })
}

fn smallest<T: PartialOrd>(found: &mut Option<T>, new: Option<T>) {
    if let Some(new) = new {
        if found.as_ref().map_or(true, |f| new < *f) {
            *found = Some(new);
        }
    }
}

/// A number in a file; `max` (v2) or a v1 value near `i64::MAX` means none.
fn number(file: &Path) -> Option<u64> {
    let n: u64 = fs::read_to_string(file).ok()?.trim().parse().ok()?;
    (n < 1 << 60).then_some(n)
}

/// cgroup v2's `cpu.max`: "quota period" in microseconds, or "max period".
fn quota_v2(dir: &Path) -> Option<f64> {
    let text = fs::read_to_string(dir.join("cpu.max")).ok()?;
    let mut fields = text.split_whitespace();
    let quota: f64 = fields.next()?.parse().ok()?;
    let period: f64 = fields.next()?.parse().ok()?;
    (quota > 0.0 && period > 0.0).then(|| quota / period)
}

/// cgroup v1's `cpu.cfs_quota_us` (-1 for none) over `cpu.cfs_period_us`.
fn quota_v1(dir: &Path) -> Option<f64> {
    let read = |file: &str| -> Option<f64> {
        fs::read_to_string(dir.join(file)).ok()?.trim().parse().ok()
    };
    let quota = read("cpu.cfs_quota_us")?;
    let period = read("cpu.cfs_period_us")?;
    (quota > 0.0 && period > 0.0).then(|| quota / period)
}

/// The CPUs in a list such as `0-3,8,10-11`.
fn count_cpus(list: &str) -> Option<u64> {
    let mut n = 0;
    for part in list.split(',') {
        n += match part.split_once('-') {
            Some((a, b)) => b.parse::<u64>().ok()? - a.parse::<u64>().ok()? + 1,
            None => {
                part.parse::<u64>().ok()?;
                1
            }
        };
    }
    Some(n)
}

/// Where the worker count came from, given the CPUs and quota found: giong
/// starts one worker per CPU it may use, and under a CPU quota one per whole
/// CPU of it, rounded down (Rust's `available_parallelism`). Anything else
/// was set by the program (`Builder::workers`) or `GIONG_WORKERS`.
pub(crate) fn workers_from(workers: usize, l: &Limits) -> &'static str {
    let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
    let cpus = l.cpus.map_or(parallelism, |c| c as usize);
    if workers != cpus.min(parallelism).max(1) {
        "set"
    } else if l.cpu_quota.is_some_and(|q| (q as usize) < cpus) {
        "cpu_quota"
    } else {
        "cpus"
    }
}

pub(crate) fn json(l: &Limits) -> Value {
    json!({
        "cpus": l.cpus,
        "cpu_quota": l.cpu_quota.map(|q| (q * 100.0).round() / 100.0),
        "memory_bytes": l.memory,
        "pids_max": l.pids_max,
        "pids_current": l.pids_current,
        "open_files": l.open_files,
        "open_files_hard": l.open_files_hard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup mount of the given files, named for the test.
    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("giong-mcp-limits-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (path, text) in files {
            let file = root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        root
    }

    #[test]
    fn a_container_on_cgroup_v2() {
        // In a container the process's path names the host's cgroup, which
        // is not mounted: the limits are at the mount's root.
        let root = tree(
            "v2",
            &[
                ("cpu.max", "150000 100000\n"),
                ("memory.max", "268435456\n"),
                ("pids.max", "100\n"),
                ("pids.current", "7\n"),
            ],
        );
        let l = limits_in(&root, "0::/system.slice/docker-abc.scope\n");
        assert_eq!(l.cpu_quota, Some(1.5));
        assert_eq!(l.memory, Some(256 << 20));
        assert_eq!((l.pids_max, l.pids_current), (Some(100), Some(7)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_smallest_limit_on_the_path_wins_and_max_is_none() {
        let root = tree(
            "path",
            &[
                ("cpu.max", "max 100000\n"),
                ("memory.max", "max\n"),
                ("pids.max", "max\n"),
                ("kubepods/cpu.max", "200000 100000\n"),
                ("kubepods/memory.max", "1073741824\n"),
                ("kubepods/pod1/cpu.max", "max 100000\n"),
                ("kubepods/pod1/memory.max", "536870912\n"),
                ("kubepods/pod1/pids.max", "max\n"),
            ],
        );
        let l = limits_in(&root, "0::/kubepods/pod1\n");
        assert_eq!(l.cpu_quota, Some(2.0));
        assert_eq!(l.memory, Some(512 << 20));
        assert_eq!(l.pids_max, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cgroup_v1() {
        let root = tree(
            "v1",
            &[
                ("cpu,cpuacct/docker/x/cpu.cfs_quota_us", "50000\n"),
                ("cpu,cpuacct/docker/x/cpu.cfs_period_us", "100000\n"),
                ("cpu,cpuacct/cpu.cfs_quota_us", "-1\n"),
                ("cpu,cpuacct/cpu.cfs_period_us", "100000\n"),
                ("memory/docker/x/memory.limit_in_bytes", "134217728\n"),
                ("memory/memory.limit_in_bytes", "9223372036854771712\n"),
                ("pids/docker/x/pids.max", "64\n"),
                ("pids/docker/x/pids.current", "3\n"),
            ],
        );
        let own = "12:pids:/docker/x\n4:cpu,cpuacct:/docker/x\n3:memory:/docker/x\n";
        let l = limits_in(&root, own);
        assert_eq!(l.cpu_quota, Some(0.5));
        assert_eq!(l.memory, Some(128 << 20));
        assert_eq!((l.pids_max, l.pids_current), (Some(64), Some(3)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(count_cpus("0"), Some(1));
        assert_eq!(count_cpus("0-3"), Some(4));
        assert_eq!(count_cpus("0-3,8,10-11"), Some(7));
        assert_eq!(count_cpus("x"), None);
    }

    #[test]
    fn this_process() {
        let l = limits();
        assert!(l.cpus.unwrap() >= 1);
        assert!(l.open_files.unwrap() >= 1);
    }
}
