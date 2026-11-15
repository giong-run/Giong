//! The bytes of requests a worker holds while it receives them, against a
//! limit (`Server::max_buffered_request_bytes`).
//!
//! Every connection is bounded on its own (`max_head_size`,
//! `max_body_size`, the HTTP/2 flow-control budget) and so is the number of
//! connections, but without this the sum is not: 10,000 connections each
//! holding a body one byte short of 1 MiB are about 11 GiB per worker. Here
//! the requests a worker's connections receive in full before calling the
//! handler share one count, kept per thread (a worker is a thread, and a
//! [`Charge`] never leaves it), so charging costs no atomic operation.
//!
//! Bytes are charged as they arrive, not as they are announced: a client
//! that announces a large body and sends nothing holds nothing. A request
//! that would take the count past the limit is refused (`503` over
//! HTTP/1.1, `REFUSED_STREAM` over HTTP/2), so the count never goes past
//! it.

use std::cell::Cell;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// What one HTTP/1.1 request may hold without being counted: the first
/// read buffer, enough for the head of almost every request and for small
/// bodies. Requests that fit keep being served while uploads fill the
/// budget, and charging costs them nothing but a comparison.
pub(crate) const FREE: usize = 4 * 1024;

/// The default budget's ceiling, per worker.
const DEFAULT_MAX: u64 = 64 * 1024 * 1024;

/// How often a worker says on stderr that it refused requests.
const REPORT_EVERY: Duration = Duration::from_secs(60);

thread_local! {
    /// Bytes charged on this worker.
    static USED: Cell<u64> = const { Cell::new(0) };
    /// The default budget for this worker, once worked out (0: not yet).
    static AUTO: Cell<u64> = const { Cell::new(0) };
    /// Requests refused since the last report, and when that was.
    static REFUSED: Cell<(u64, Option<Instant>)> = const { Cell::new((0, None)) };
}

/// Bytes charged against the worker's budget, given back when dropped.
///
/// Neither `Send` nor `Sync` (it changes a count of the thread it was made
/// on), like the `Request` and connection state that hold it.
#[derive(Debug, Default)]
pub(crate) struct Charge {
    bytes: u64,
    _thread: PhantomData<Rc<()>>,
}

impl Charge {
    pub(crate) const fn none() -> Self {
        Self {
            bytes: 0,
            _thread: PhantomData,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Grows the charge to `target` bytes if the worker's count stays
    /// within `limit`; `false`, and no change, otherwise. A smaller target
    /// changes nothing.
    pub(crate) fn grow_to(&mut self, target: u64, limit: u64) -> bool {
        let Some(more) = target.checked_sub(self.bytes).filter(|&more| more > 0) else {
            return true;
        };
        let fits = USED.with(|used| {
            let now = used.get();
            let fits = now.saturating_add(more) <= limit;
            if fits {
                used.set(now + more);
            }
            fits
        });
        if fits {
            self.bytes = target;
        }
        fits
    }

    /// Gives back what is charged beyond `target`.
    pub(crate) fn shrink_to(&mut self, target: u64) {
        if target < self.bytes {
            release(self.bytes - target);
            self.bytes = target;
        }
    }

    /// Keeps `keep` bytes (at most what is charged) and moves the rest into
    /// a charge of its own: for a request handed to its handler, which
    /// holds its bytes for as long as it lives.
    pub(crate) fn split_off(&mut self, keep: u64) -> Charge {
        let keep = keep.min(self.bytes);
        let moved = self.bytes - keep;
        self.bytes = keep;
        Charge {
            bytes: moved,
            _thread: PhantomData,
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.bytes > 0 {
            release(self.bytes);
        }
    }
}

fn release(bytes: u64) {
    // A charge dropped while the thread's locals are being destroyed has
    // nobody left to give back to.
    let _ = USED.try_with(|used| used.set(used.get().saturating_sub(bytes)));
}

/// Whether `more` bytes would still fit the worker's budget of `limit`.
pub(crate) fn fits(more: u64, limit: u64) -> bool {
    USED.with(|used| used.get().saturating_add(more) <= limit)
}

/// Bytes charged on this worker: for tests.
#[cfg(test)]
pub(crate) fn used() -> u64 {
    USED.with(Cell::get)
}

/// The budget when `Server::max_buffered_request_bytes` was not set: 64 MiB
/// per worker, or a quarter of the process's cgroup memory limit divided
/// among the workers if that is less; but never less than `single` (one
/// request at the server's own size limits), or the server would refuse
/// requests it says it accepts.
pub(crate) fn auto_limit(single: u64) -> u64 {
    let auto = AUTO.with(|auto| {
        if auto.get() == 0 {
            // Outside a runtime (a test driving a connection by hand), as
            // many workers as the runtime would start.
            let workers = match giong::try_worker_id() {
                Some(_) => giong::handle().workers(),
                None => std::thread::available_parallelism().map_or(1, |n| n.get()),
            } as u64;
            auto.set(default_limit(cgroup_memory_limit(), workers));
        }
        auto.get()
    });
    auto.max(single)
}

fn default_limit(cgroup: Option<u64>, workers: u64) -> u64 {
    match cgroup {
        Some(limit) => (limit / 4 / workers.max(1)).clamp(1, DEFAULT_MAX),
        None => DEFAULT_MAX,
    }
}

/// The memory limit of the process's cgroup (the smallest along its path),
/// read once. `None` without one.
fn cgroup_memory_limit() -> Option<u64> {
    static LIMIT: OnceLock<Option<u64>> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let own = fs::read_to_string("/proc/self/cgroup").ok()?;
        memory_limit_in(Path::new("/sys/fs/cgroup"), &own)
    })
}

/// The memory limit for a process whose `/proc/self/cgroup` reads `own`,
/// with the cgroup file systems mounted at `root`: cgroup v2's
/// `memory.max`, or v1's `memory.limit_in_bytes` under `root/memory`. In a
/// container the path in `own` may not exist (it names the host's cgroup)
/// and the container's own cgroup is the mount's root: every directory from
/// the path up to the root is read, and the smallest limit wins.
fn memory_limit_in(root: &Path, own: &str) -> Option<u64> {
    let mut found = None;
    for line in own.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (base, file) = if controllers.is_empty() {
            (root.to_path_buf(), "memory.max")
        } else if controllers.split(',').any(|c| c == "memory") {
            (root.join("memory"), "memory.limit_in_bytes")
        } else {
            continue;
        };
        let mut dir: PathBuf = base.join(path.trim_start_matches('/'));
        loop {
            if let Some(limit) = read_limit(&dir.join(file)) {
                found = Some(found.map_or(limit, |f: u64| f.min(limit)));
            }
            if dir == base || !dir.pop() || !dir.starts_with(&base) {
                break;
            }
        }
    }
    found
}

/// A limit file: a number of bytes, or `max` (v2) or a number near
/// `i64::MAX` (v1) for none.
fn read_limit(file: &Path) -> Option<u64> {
    let text = fs::read_to_string(file).ok()?;
    let limit: u64 = text.trim().parse().ok()?;
    (limit < 1 << 60).then_some(limit)
}

/// Notes a refused request; at most once a minute per worker, says so on
/// stderr, with the code and how many were refused.
pub(crate) fn note_refused(limit: u64) {
    let _ = REFUSED.try_with(|refused| {
        let (count, last) = refused.get();
        let now = Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < REPORT_EVERY) {
            refused.set((count + 1, last));
            return;
        }
        refused.set((0, Some(now)));
        eprintln!(
            "giong-http: GIONG-E1120 refused {} request(s) on this worker: the requests it \
             receives already hold {limit} bytes, its max_buffered_request_bytes; see \
             docs/errors/E1120.md",
            count + 1
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_add_up_per_thread_and_never_pass_the_limit() {
        assert_eq!(used(), 0);
        let mut a = Charge::none();
        let mut b = Charge::none();
        assert!(a.grow_to(600, 1000));
        assert!(!b.grow_to(500, 1000), "600 + 500 is past 1000");
        assert_eq!(b.bytes(), 0);
        assert!(b.grow_to(400, 1000));
        assert_eq!(used(), 1000);
        assert!(!fits(1, 1000));
        // A smaller target changes nothing.
        assert!(a.grow_to(100, 1000));
        assert_eq!(a.bytes(), 600);
        a.shrink_to(100);
        assert_eq!(used(), 500);
        let moved = b.split_off(150);
        assert_eq!((b.bytes(), moved.bytes()), (150, 250));
        assert_eq!(used(), 500);
        drop(moved);
        assert_eq!(used(), 250);
        drop((a, b));
        assert_eq!(used(), 0);
        // Another thread has a count of its own.
        let mut here = Charge::none();
        assert!(here.grow_to(1000, 1000));
        std::thread::spawn(|| assert!(fits(1000, 1000)))
            .join()
            .unwrap();
    }

    #[test]
    fn the_default_is_64_mib_or_a_quarter_of_the_cgroup_limit_per_worker() {
        assert_eq!(default_limit(None, 8), 64 << 20);
        // 128 MiB, one worker: 32 MiB.
        assert_eq!(default_limit(Some(128 << 20), 1), 32 << 20);
        // 1 GiB, two workers: 128 MiB each, so the ceiling.
        assert_eq!(default_limit(Some(1 << 30), 2), 64 << 20);
        assert_eq!(default_limit(Some(128 << 20), 0), 32 << 20);
    }

    #[test]
    fn the_cgroup_limit_is_the_smallest_along_the_path() {
        let root = std::env::temp_dir().join(format!("giong-cgroup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // v2: a service with its own limit inside a slice with a larger one.
        let service = root.join("system.slice/app.service");
        fs::create_dir_all(&service).unwrap();
        fs::write(root.join("system.slice/memory.max"), "1073741824\n").unwrap();
        fs::write(service.join("memory.max"), "134217728\n").unwrap();
        assert_eq!(
            memory_limit_in(&root, "0::/system.slice/app.service\n"),
            Some(128 << 20)
        );
        // `max` is no limit.
        fs::write(service.join("memory.max"), "max\n").unwrap();
        assert_eq!(
            memory_limit_in(&root, "0::/system.slice/app.service\n"),
            Some(1 << 30)
        );
        // A container: the path names the host's cgroup, which is not
        // there; the mount's root is the container's.
        fs::write(root.join("memory.max"), "268435456\n").unwrap();
        assert_eq!(memory_limit_in(&root, "0::/docker/abc\n"), Some(256 << 20));
        // v1, with the unlimited value.
        let v1 = root.join("memory");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("memory.limit_in_bytes"), "9223372036854771712\n").unwrap();
        assert_eq!(
            memory_limit_in(&root, "4:memory:/docker/abc\n2:cpu,cpuacct:/x\n"),
            None
        );
        fs::write(v1.join("memory.limit_in_bytes"), "67108864\n").unwrap();
        assert_eq!(
            memory_limit_in(&root, "4:cpuacct,memory:/docker/abc\n"),
            Some(64 << 20)
        );
        assert_eq!(memory_limit_in(&root, "garbage"), None);
        let _ = fs::remove_dir_all(&root);
    }
}
