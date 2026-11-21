//! How much memory the process has left before the kernel's out-of-memory
//! killer takes it: what the accept loop asks before it takes on a new
//! connection (`Server::min_free_memory`).
//!
//! Every connection is bounded, and so is their number per worker
//! (`max_connections`), but nothing ties those bounds to the memory the
//! process actually has. In a container of 512 MiB, idle HTTPS connections
//! raised past the default limit fill it at about 14 KiB each, kernel socket
//! memory included, and the process is killed with every connection it
//! holds. Here the accept loop refuses new connections while less than a
//! reserve is left, so that the process stays up and keeps serving the
//! connections it has.
//!
//! What is left is the smaller of what the process's cgroup allows (its
//! `memory.max`, or `memory.high` if lower, minus what the cgroup uses,
//! not counting the page cache the kernel would reclaim first) and what the
//! machine has (`MemAvailable`). Reading those costs a few microseconds, so
//! a worker reads them at most every 50 ms while far from the reserve, and
//! on every accept once it is near it.
//!
//! macOS has no cgroups: what is left there is what the machine has, its
//! free and inactive pages (the kernel's `host_statistics64`), the figure
//! `vm_stat` prints and monitoring tools call "available".

use std::cell::RefCell;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The smallest default reserve.
const MIN_RESERVE: u64 = 16 << 20;

/// The largest default reserve.
const MAX_RESERVE: u64 = 256 << 20;

/// The cgroup that bounds the process's memory: the one with the smallest
/// limit along its path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Cgroup {
    /// Its limit, in bytes.
    pub(crate) limit: u64,
    /// Its directory.
    dir: PathBuf,
    /// cgroup v2 (`memory.current`, `memory.stat`), or v1
    /// (`memory.usage_in_bytes`, `memory.stat` with `total_` names).
    v2: bool,
}

/// The process's cgroup memory limit, found once. `None` without one.
pub(crate) fn cgroup() -> Option<&'static Cgroup> {
    static CGROUP: OnceLock<Option<Cgroup>> = OnceLock::new();
    CGROUP
        .get_or_init(|| {
            let own = fs::read_to_string("/proc/self/cgroup").ok()?;
            cgroup_in(Path::new("/sys/fs/cgroup"), &own)
        })
        .as_ref()
}

/// The cgroup that bounds a process whose `/proc/self/cgroup` reads `own`,
/// with the cgroup file systems mounted at `root`: cgroup v2's `memory.max`
/// (or `memory.high`, if lower), or v1's `memory.limit_in_bytes` under
/// `root/memory`. In a container the path in `own` may not exist (it names
/// the host's cgroup) and the container's own cgroup is the mount's root:
/// every directory from the path up to the root is read, and the smallest
/// limit wins.
pub(crate) fn cgroup_in(root: &Path, own: &str) -> Option<Cgroup> {
    let mut found: Option<Cgroup> = None;
    for line in own.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (base, files, v2): (PathBuf, &[&str], bool) = if controllers.is_empty() {
            (root.to_path_buf(), &["memory.max", "memory.high"], true)
        } else if controllers.split(',').any(|c| c == "memory") {
            (root.join("memory"), &["memory.limit_in_bytes"], false)
        } else {
            continue;
        };
        let mut dir: PathBuf = base.join(path.trim_start_matches('/'));
        loop {
            for file in files {
                if let Some(limit) = read_limit(&dir.join(file)) {
                    if found.as_ref().map_or(true, |f| limit < f.limit) {
                        found = Some(Cgroup {
                            limit,
                            dir: dir.clone(),
                            v2,
                        });
                    }
                }
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

/// The reserve when `Server::min_free_memory` was not set: a tenth of the
/// cgroup's limit, or of the machine's memory without one, but at least 16
/// MiB and at most 256 MiB, and never more than a quarter of the memory.
pub(crate) fn auto_reserve() -> u64 {
    static AUTO: OnceLock<u64> = OnceLock::new();
    *AUTO.get_or_init(|| {
        let total = match cgroup() {
            Some(cgroup) => Some(cgroup.limit),
            #[cfg(target_os = "linux")]
            None => fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|text| field(&text, "MemTotal:").map(|kib| kib * 1024)),
            #[cfg(target_os = "macos")]
            None => mac::total(),
        };
        total.map_or(MIN_RESERVE, reserve_for)
    })
}

fn reserve_for(total: u64) -> u64 {
    (total / 10).clamp(MIN_RESERVE, MAX_RESERVE).min(total / 4)
}

/// What a worker knows of the process's memory.
struct Watch {
    /// When to read the files again.
    next: Instant,
    /// Less than the reserve was left when they were last read.
    low: bool,
    /// The files, kept open.
    usage: Option<File>,
    stat: Option<File>,
    #[cfg(target_os = "linux")]
    meminfo: Option<File>,
    buf: Vec<u8>,
}

thread_local! {
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

/// Whether less than `reserve` bytes of memory are left to the process, as
/// last read on this worker. `0` is never.
pub(crate) fn low(reserve: u64) -> bool {
    if reserve == 0 {
        return false;
    }
    WATCH
        .try_with(|watch| {
            let mut watch = watch.borrow_mut();
            let watch = watch.get_or_insert_with(Watch::open);
            let now = Instant::now();
            if now < watch.next {
                return watch.low;
            }
            let free = watch.free(reserve);
            watch.low = free.is_some_and(|free| free < reserve);
            watch.next = now + recheck_after(free, reserve);
            watch.low
        })
        .unwrap_or(false)
}

/// How long the last reading holds: 50 ms while more than four times the
/// reserve is left, 5 ms while more than twice, and not at all below that,
/// where every accept reads again. Between two readings a worker cannot take
/// on more than what was left over the reserve: even at 100,000 connections
/// a second, 5 ms are 500 connections, about 7 MiB over HTTPS.
fn recheck_after(free: Option<u64>, reserve: u64) -> Duration {
    match free {
        None => Duration::from_secs(1),
        Some(free) if free / 4 >= reserve => Duration::from_millis(50),
        Some(free) if free / 2 >= reserve => Duration::from_millis(5),
        Some(_) => Duration::ZERO,
    }
}

impl Watch {
    fn open() -> Self {
        let (usage, stat) = match cgroup() {
            Some(cgroup) => {
                let usage = if cgroup.v2 {
                    "memory.current"
                } else {
                    "memory.usage_in_bytes"
                };
                (
                    File::open(cgroup.dir.join(usage)).ok(),
                    File::open(cgroup.dir.join("memory.stat")).ok(),
                )
            }
            None => (None, None),
        };
        Self {
            next: Instant::now(),
            low: false,
            usage,
            stat,
            #[cfg(target_os = "linux")]
            meminfo: File::open("/proc/meminfo").ok(),
            buf: Vec::with_capacity(8 * 1024),
        }
    }

    /// Bytes left to the process: the smaller of the cgroup's and the
    /// machine's. `None` if neither can be read.
    fn free(&mut self, reserve: u64) -> Option<u64> {
        let cgroup = self.cgroup_free(reserve);
        let machine = self.machine_free();
        match (cgroup, machine) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn cgroup_free(&mut self, reserve: u64) -> Option<u64> {
        let cgroup = cgroup()?;
        let usage = read_all(self.usage.as_ref()?, &mut self.buf).ok()?;
        let usage: u64 = std::str::from_utf8(usage).ok()?.trim().parse().ok()?;
        // Far from the limit even counting the page cache: no need to read
        // what of it the kernel could reclaim.
        if cgroup.limit.saturating_sub(usage) / 2 >= reserve {
            return Some(cgroup.limit.saturating_sub(usage));
        }
        let reclaimable = match self.stat.as_ref() {
            Some(stat) => {
                let text = read_all(stat, &mut self.buf).ok()?;
                reclaimable(std::str::from_utf8(text).ok()?, cgroup.v2)
            }
            None => 0,
        };
        Some(
            cgroup
                .limit
                .saturating_sub(usage.saturating_sub(reclaimable)),
        )
    }

    #[cfg(target_os = "macos")]
    fn machine_free(&mut self) -> Option<u64> {
        mac::available()
    }

    #[cfg(target_os = "linux")]
    fn machine_free(&mut self) -> Option<u64> {
        let text = read_all(self.meminfo.as_ref()?, &mut self.buf).ok()?;
        field(std::str::from_utf8(text).ok()?, "MemAvailable:").map(|kib| kib * 1024)
    }
}

/// The machine's memory on macOS, from the kernel.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    /// `HOST_VM_INFO64`.
    const VM_INFO64: c_int = 4;

    /// `vm_statistics64`, as the 32-bit words `host_statistics64` counts
    /// in: `free_count` is word 0 and `inactive_count` word 2. Larger than
    /// the structure of any release so far (40 words on macOS 26); the
    /// kernel fills what it has and says how much that was.
    #[repr(C, align(8))]
    struct Statistics([u32; 64]);

    extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics64(host: u32, flavor: c_int, info: *mut u32, count: *mut u32) -> c_int;
        fn sysctlbyname(
            name: *const c_char,
            old: *mut c_void,
            old_len: *mut usize,
            new: *mut c_void,
            new_len: usize,
        ) -> c_int;
        /// The kernel's page size, which `host_statistics64` counts in. It
        /// can differ from the process's (`getpagesize`): 16 KiB against
        /// 4 KiB for an x86_64 process under Rosetta on Apple silicon.
        static vm_kernel_page_size: usize;
    }

    /// The memory installed (`hw.memsize`).
    pub(super) fn total() -> Option<u64> {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated; the kernel writes at most
        // `len` bytes, the size of `bytes`.
        let res = unsafe {
            sysctlbyname(
                c"hw.memsize".as_ptr(),
                std::ptr::addr_of_mut!(bytes).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (res == 0 && len == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
    }

    /// The memory the kernel can hand out without paging to disk first: free
    /// pages and inactive ones.
    pub(super) fn available() -> Option<u64> {
        // The port of the host is asked for once: every call adds a
        // reference to it that is never given back.
        static HOST: OnceLock<u32> = OnceLock::new();
        // SAFETY: no arguments; returns a port name.
        let host = *HOST.get_or_init(|| unsafe { mach_host_self() });
        let mut stats = Statistics([0; 64]);
        let mut count = stats.0.len() as u32;
        // SAFETY: the kernel writes at most `count` words into `stats`.
        let res = unsafe { host_statistics64(host, VM_INFO64, stats.0.as_mut_ptr(), &mut count) };
        if res != 0 || count < 3 {
            return None;
        }
        let pages = u64::from(stats.0[0]) + u64::from(stats.0[2]);
        // SAFETY: a constant the system sets before the process starts.
        let page = u64::try_from(unsafe { vm_kernel_page_size }).ok()?;
        Some(pages * page)
    }
}

/// The whole of a small file kept open (a cgroup or `/proc` file, which
/// is generated on each read from offset 0).
fn read_all<'a>(file: &File, buf: &'a mut Vec<u8>) -> io::Result<&'a [u8]> {
    buf.clear();
    loop {
        if buf.len() == buf.capacity() {
            buf.reserve(buf.capacity().max(4096));
        }
        let at = buf.len();
        let cap = buf.capacity();
        buf.resize(cap, 0);
        match file.read_at(&mut buf[at..], at as u64) {
            Ok(0) => {
                buf.truncate(at);
                return Ok(buf);
            }
            Ok(n) => buf.truncate(at + n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => buf.truncate(at),
            Err(e) => return Err(e),
        }
    }
}

/// What of a cgroup's usage is page cache the kernel can drop or write back
/// to make room, from its `memory.stat`: file pages, less those of tmpfs and
/// shared memory, which cannot be dropped.
fn reclaimable(stat: &str, v2: bool) -> u64 {
    let (file, shmem) = if v2 {
        ("file ", "shmem ")
    } else {
        ("total_cache ", "total_shmem ")
    };
    let value = |name: &str| {
        stat.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    value(file).saturating_sub(value(shmem))
}

/// A `/proc/meminfo` field, in KiB.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn field(meminfo: &str, name: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|line| line.strip_prefix(name))?;
    line.trim().trim_end_matches("kB").trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_is_a_tenth_between_16_and_256_mib() {
        assert_eq!(reserve_for(512 << 20), (512 << 20) / 10);
        assert_eq!(reserve_for(128 << 20), 16 << 20);
        assert_eq!(reserve_for(32 << 20), 8 << 20, "never more than a quarter");
        assert_eq!(reserve_for(64 << 30), 256 << 20);
    }

    #[test]
    fn page_cache_is_reclaimable_but_not_shared_memory() {
        let v2 = "anon 1000\nfile 5000\nkernel 300\nsock 4096\nshmem 1200\nfile_mapped 10\n";
        assert_eq!(reclaimable(v2, true), 3800);
        let v1 = "cache 1\nrss 2\ntotal_cache 7000\ntotal_rss 100\ntotal_shmem 500\n";
        assert_eq!(reclaimable(v1, false), 6500);
        assert_eq!(reclaimable("", true), 0);
    }

    #[test]
    fn meminfo_fields_are_in_kib() {
        let text = "MemTotal:       16384000 kB\nMemFree:  1 kB\nMemAvailable:    8192000 kB\n";
        assert_eq!(field(text, "MemTotal:"), Some(16384000));
        assert_eq!(field(text, "MemAvailable:"), Some(8192000));
        assert_eq!(field(text, "SwapTotal:"), None);
    }

    #[test]
    fn readings_come_more_often_near_the_reserve() {
        let r = 50 << 20;
        assert_eq!(recheck_after(Some(400 << 20), r), Duration::from_millis(50));
        assert_eq!(recheck_after(Some(150 << 20), r), Duration::from_millis(5));
        assert_eq!(recheck_after(Some(60 << 20), r), Duration::ZERO);
        assert_eq!(recheck_after(None, r), Duration::from_secs(1));
    }

    #[test]
    fn a_reserve_of_zero_is_never_low() {
        assert!(!low(0));
        // This machine is not out of memory.
        assert!(!low(1));
    }

    #[test]
    fn the_cgroup_is_the_one_with_the_smallest_limit() {
        let root = std::env::temp_dir().join(format!("giong-memcg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let service = root.join("system.slice/app.service");
        fs::create_dir_all(&service).unwrap();
        fs::write(root.join("system.slice/memory.max"), "1073741824\n").unwrap();
        fs::write(service.join("memory.max"), "max\n").unwrap();
        fs::write(service.join("memory.high"), "536870912\n").unwrap();
        assert_eq!(
            cgroup_in(&root, "0::/system.slice/app.service\n"),
            Some(Cgroup {
                limit: 512 << 20,
                dir: service.clone(),
                v2: true
            })
        );
        let v1 = root.join("memory/docker/abc");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("memory.limit_in_bytes"), "67108864\n").unwrap();
        assert_eq!(
            cgroup_in(&root, "4:memory:/docker/abc\n"),
            Some(Cgroup {
                limit: 64 << 20,
                dir: v1,
                v2: false
            })
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Reading this process's own files works, and twice from one open file
    /// gives the whole file both times.
    #[cfg(target_os = "linux")]
    #[test]
    fn files_kept_open_read_whole_every_time() {
        let file = File::open("/proc/meminfo").unwrap();
        let mut buf = Vec::with_capacity(16);
        let first = read_all(&file, &mut buf).unwrap().to_vec();
        let second = read_all(&file, &mut buf).unwrap().to_vec();
        assert!(field(std::str::from_utf8(&first).unwrap(), "MemTotal:").is_some());
        assert_eq!(first.len(), second.len());
        let mut watch = Watch::open();
        assert!(watch.free(MIN_RESERVE).is_some_and(|free| free > 0));
    }

    /// The kernel says how much memory the machine has and how much of it
    /// is left: less than all of it, and more than nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_machine_s_memory_is_read_from_the_kernel() {
        let total = mac::total().unwrap();
        let available = mac::available().unwrap();
        assert!(total >= 1 << 30, "{total}");
        assert!(available > 0 && available < total, "{available} of {total}");
        let mut watch = Watch::open();
        assert!(watch.free(MIN_RESERVE).is_some());
        assert!(auto_reserve() >= MIN_RESERVE);
    }
}
