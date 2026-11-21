//! Same-host messaging between two processes: shared-memory rings with
//! io_uring futex wake-ups (`IORING_OP_FUTEX_WAIT`/`FUTEX_WAKE`, Linux 6.7+)
//! against a Unix stream socket and TCP over loopback.
//!
//! Every variant drives one io_uring per process the way a giong worker does
//! (single issuer, deferred task running), so the comparison is between the
//! transports, not between event loops:
//!
//! * `ping`: one 64-byte message in flight; each side sleeps in
//!   `io_uring_enter` until the other answers. Round trips per second and
//!   CPU per round trip.
//! * `stream`: one side sends 64-byte messages as fast as the other takes
//!   them, 32 per batch (sockets: one 2 KiB send per batch). Messages per
//!   second and CPU per message.
//!
//! In the shared-memory variant a sleeping side announces it in a flag that
//! is also the futex word; the other side wakes it only when the flag was
//! set, and the wake rides in the same `io_uring_enter` as its own wait, so a
//! round trip costs one system call per side and none while both are busy.
//!
//!   bench-ipc <shm|unix|tcp> <ping|stream> <count> [cpu-a cpu-b]
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering::*};
use std::time::Instant;

use io_uring::{opcode, squeue, types, IoUring};

const MSG: usize = 64;
const SLOTS: u32 = 4096;
const BATCH: u32 = 32;
const FUTEX2_SIZE_U32: u32 = 0x02;
const MATCH_ANY: u64 = u32::MAX as u64;
const UD_WAIT: u64 = 1;
const UD_OTHER: u64 = 2;

#[repr(C, align(64))]
struct Line(AtomicU32);

/// One direction: a single-producer single-consumer ring of 64-byte slots.
#[repr(C)]
struct Chan {
    head: Line,
    tail: Line,
    /// Set by a consumer about to sleep on an empty ring; the futex word.
    consumer_sleeping: Line,
    /// Set by a producer about to sleep on a full ring; the futex word.
    producer_sleeping: Line,
    slots: [[u8; MSG]; SLOTS as usize],
}

#[repr(C)]
struct Shared {
    a_to_b: Chan,
    b_to_a: Chan,
}

/// BENCH_IPC_SPIN_US: how long a side checks for work before it sleeps (the
/// same idea as giong's `spin_before_park`). Sockets spin with non-blocking
/// recv(2) calls, shared memory by reading the ring.
fn spin_ns() -> u64 {
    static NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NS.get_or_init(|| {
        std::env::var("BENCH_IPC_SPIN_US")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1000
    })
}

fn ring() -> IoUring {
    IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .setup_coop_taskrun()
        .build(256)
        .expect("io_uring (needs Linux 6.1+)")
}

struct Loop {
    ring: IoUring,
    /// A futex wait is armed and has not completed.
    armed: bool,
}

impl Loop {
    fn push(&mut self, e: squeue::Entry) {
        // SAFETY: every pointer in the entries outlives their completion
        // (shared memory and buffers live until the process exits).
        unsafe { self.ring.submission().push(&e).expect("sq full") };
    }

    /// Submits what is queued and waits for one completion; returns the
    /// user_data values that completed.
    fn enter(&mut self, want: usize, seen: &mut Vec<(u64, i32)>) {
        self.ring.submit_and_wait(want).expect("io_uring_enter");
        seen.clear();
        for cqe in self.ring.completion() {
            if cqe.user_data() == UD_WAIT {
                self.armed = false;
            }
            seen.push((cqe.user_data(), cqe.result()));
        }
    }

    fn wake(&mut self, word: &AtomicU32) {
        if word.swap(0, SeqCst) == 1 {
            self.push(
                opcode::FutexWake::new(word.as_ptr(), 1, MATCH_ANY, FUTEX2_SIZE_U32)
                    .build()
                    .user_data(UD_OTHER),
            );
        }
    }

    /// Waits until `ready()` holds, sleeping on `word` in the ring. Queued
    /// entries (a wake for the other side) go in with the wait.
    fn wait_until(&mut self, word: &AtomicU32, mut ready: impl FnMut() -> bool) {
        let mut seen = Vec::new();
        if spin_ns() > 0 && !ready() {
            if !self.ring.submission().is_empty() {
                self.ring.submit().expect("io_uring_enter");
            }
            let t0 = Instant::now();
            while !ready() && (t0.elapsed().as_nanos() as u64) < spin_ns() {
                std::hint::spin_loop();
            }
        }
        loop {
            if ready() {
                if !self.ring.submission().is_empty() {
                    self.ring.submit().expect("io_uring_enter");
                }
                return;
            }
            if !self.armed {
                word.store(1, SeqCst);
                if ready() {
                    word.store(0, SeqCst);
                    continue;
                }
                self.push(
                    opcode::FutexWait::new(word.as_ptr(), 1, MATCH_ANY, FUTEX2_SIZE_U32)
                        .build()
                        .user_data(UD_WAIT),
                );
                self.armed = true;
            }
            self.enter(1, &mut seen);
        }
    }
}

impl Chan {
    fn len(&self) -> u32 {
        self.tail
            .0
            .load(Acquire)
            .wrapping_sub(self.head.0.load(Acquire))
    }
    fn try_push(&mut self, msg: &[u8; MSG]) -> bool {
        let tail = self.tail.0.load(Relaxed);
        if tail.wrapping_sub(self.head.0.load(Acquire)) == SLOTS {
            return false;
        }
        self.slots[(tail % SLOTS) as usize] = *msg;
        self.tail.0.store(tail.wrapping_add(1), Release);
        true
    }
    fn try_pop(&mut self, out: &mut [u8; MSG]) -> bool {
        let head = self.head.0.load(Relaxed);
        if self.tail.0.load(Acquire) == head {
            return false;
        }
        *out = self.slots[(head % SLOTS) as usize];
        self.head.0.store(head.wrapping_add(1), Release);
        true
    }
}

fn map_shared(path: &str, create: bool) -> &'static mut Shared {
    let size = std::mem::size_of::<Shared>();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .open(path)
        .expect("open shm");
    if create {
        file.set_len(size as u64).expect("ftruncate");
    }
    // SAFETY: a fresh shared mapping of a file at least `size` long; the
    // zeroed bytes are a valid `Shared` (atomics and byte arrays).
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(p, libc::MAP_FAILED, "mmap");
        &mut *(p as *mut Shared)
    }
}

/// Side A of `shm`: sends, side B answers (ping) or drains (stream).
fn shm(role: &str, mode: &str, count: u64, path: &str) {
    let sh = map_shared(path, role == "a");
    let sh2 = unsafe { &mut *(sh as *mut Shared) };
    let mut lp = Loop {
        ring: ring(),
        armed: false,
    };
    let (out, inc) = if role == "a" {
        (&mut sh.a_to_b, &mut sh2.b_to_a)
    } else {
        (&mut sh.b_to_a, &mut sh2.a_to_b)
    };
    let out_ptr = out as *mut Chan;
    let inc_ptr = inc as *mut Chan;
    // SAFETY: two views of distinct channels in the shared mapping.
    let (out, inc) = unsafe { (&mut *out_ptr, &mut *inc_ptr) };
    let mut msg = [7u8; MSG];
    match (mode, role) {
        ("ping", "a") | ("ping", "b") => {
            for i in 0..count {
                if role == "a" {
                    msg[..8].copy_from_slice(&i.to_le_bytes());
                    assert!(out.try_push(&msg));
                    lp.wake(&out.consumer_sleeping.0);
                }
                let incp = inc as *mut Chan;
                lp.wait_until(&inc.consumer_sleeping.0, || unsafe { (*incp).len() > 0 });
                assert!(inc.try_pop(&mut msg));
                if role == "b" {
                    assert!(out.try_push(&msg));
                    lp.wake(&out.consumer_sleeping.0);
                }
            }
            if !lp.ring.submission().is_empty() {
                lp.ring.submit().unwrap();
            }
        }
        ("stream", "a") => {
            let mut sent = 0u64;
            while sent < count {
                let mut n = 0;
                while n < BATCH && sent < count && out.try_push(&msg) {
                    n += 1;
                    sent += 1;
                }
                lp.wake(&out.consumer_sleeping.0);
                if n == 0 {
                    let outp = out as *mut Chan;
                    lp.wait_until(&out.producer_sleeping.0, || unsafe {
                        (*outp).len() < SLOTS
                    });
                }
            }
            lp.ring.submit().unwrap();
        }
        ("stream", "b") => {
            let mut got = 0u64;
            while got < count {
                let mut n = 0;
                while n < BATCH && inc.try_pop(&mut msg) {
                    n += 1;
                    got += 1;
                }
                lp.wake(&inc.producer_sleeping.0);
                if n == 0 {
                    let incp = inc as *mut Chan;
                    lp.wait_until(&inc.consumer_sleeping.0, || unsafe { (*incp).len() > 0 });
                }
            }
            lp.ring.submit().unwrap();
        }
        _ => unreachable!(),
    }
}

/// Sockets: send and receive through the ring, one enter per round trip
/// (ping) or per batch (stream).
fn sock(fd: RawFd, role: &str, mode: &str, count: u64) {
    let mut lp = Loop {
        ring: ring(),
        armed: false,
    };
    let mut seen = Vec::new();
    let msg = [7u8; MSG];
    let batch = [7u8; MSG * BATCH as usize];
    let mut buf = vec![0u8; 64 * 1024];
    let recv_exact = |lp: &mut Loop, seen: &mut Vec<(u64, i32)>, buf: &mut [u8], want: usize| {
        let mut got = 0;
        if spin_ns() > 0 {
            if !lp.ring.submission().is_empty() {
                lp.ring.submit().expect("io_uring_enter");
            }
            let t0 = Instant::now();
            while got < want && (t0.elapsed().as_nanos() as u64) < spin_ns() {
                // SAFETY: `buf[got..]` is valid for `want - got` bytes.
                let r = unsafe {
                    libc::recv(
                        fd,
                        buf[got..].as_mut_ptr().cast(),
                        want - got,
                        libc::MSG_DONTWAIT,
                    )
                };
                if r > 0 {
                    got += r as usize;
                }
            }
        }
        while got < want {
            lp.push(
                opcode::Recv::new(types::Fd(fd), buf[got..].as_mut_ptr(), (want - got) as u32)
                    .build()
                    .user_data(3),
            );
            loop {
                lp.enter(1, seen);
                if let Some(&(_, r)) = seen.iter().find(|(u, _)| *u == 3) {
                    assert!(r > 0, "recv {r}");
                    got += r as usize;
                    break;
                }
            }
        }
    };
    match (mode, role) {
        ("ping", _) => {
            for _ in 0..count {
                if role == "a" {
                    lp.push(
                        opcode::Send::new(types::Fd(fd), msg.as_ptr(), MSG as u32)
                            .build()
                            .user_data(UD_OTHER),
                    );
                }
                recv_exact(&mut lp, &mut seen, &mut buf, MSG);
                if role == "b" {
                    lp.push(
                        opcode::Send::new(types::Fd(fd), msg.as_ptr(), MSG as u32)
                            .build()
                            .user_data(UD_OTHER),
                    );
                }
            }
            lp.ring.submit().unwrap();
        }
        ("stream", "a") => {
            let batches = count / BATCH as u64;
            for _ in 0..batches {
                let mut off = 0;
                while off < batch.len() {
                    lp.push(
                        opcode::Send::new(
                            types::Fd(fd),
                            batch[off..].as_ptr(),
                            (batch.len() - off) as u32,
                        )
                        .build()
                        .user_data(4),
                    );
                    loop {
                        lp.enter(1, &mut seen);
                        if let Some(&(_, r)) = seen.iter().find(|(u, _)| *u == 4) {
                            assert!(r > 0, "send {r}");
                            off += r as usize;
                            break;
                        }
                    }
                }
            }
        }
        ("stream", "b") => {
            let total = (count / BATCH as u64) as usize * MSG * BATCH as usize;
            let mut got = 0;
            while got < total {
                let want = (total - got).min(buf.len());
                lp.push(
                    opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), want as u32)
                        .build()
                        .user_data(3),
                );
                loop {
                    lp.enter(1, &mut seen);
                    if let Some(&(_, r)) = seen.iter().find(|(u, _)| *u == 3) {
                        assert!(r > 0, "recv {r}");
                        got += r as usize;
                        break;
                    }
                }
            }
        }
        _ => unreachable!(),
    }
}

fn pin(cpu: Option<usize>) {
    if let Some(cpu) = cpu {
        // SAFETY: a zeroed cpu_set_t with one CPU set.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

fn cpu_us() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let mut total = 0.0;
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        unsafe { libc::getrusage(who, &mut ru) };
        total += (ru.ru_utime.tv_sec + ru.ru_stime.tv_sec) as f64 * 1e6
            + (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) as f64;
    }
    total
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (transport, mode) = (args[1].as_str(), args[2].as_str());
    let count: u64 = args[3].parse().expect("count");
    if let Ok(role_b) = std::env::var("BENCH_IPC_CHILD") {
        pin(args.get(5).and_then(|c| c.parse().ok()));
        match transport {
            "shm" => shm("b", mode, count, &role_b),
            "unix" | "tcp" => {
                // The parent passes the connected socket as fd 3.
                let fd = unsafe { OwnedFd::from_raw_fd(3) };
                let mut s = unsafe { std::fs::File::from_raw_fd(libc::dup(fd.as_raw_fd())) };
                s.write_all(b"r")?;
                sock(fd.as_raw_fd(), "b", mode, count);
            }
            _ => unreachable!(),
        }
        return Ok(());
    }
    pin(args.get(4).and_then(|c| c.parse().ok()));
    let exe = std::env::current_exe()?;
    let path = format!("/dev/shm/giong-bench-ipc-{}", std::process::id());
    let mut child = Command::new(&exe);
    child.args(&args[1..]).stdout(Stdio::inherit());
    let (mut started, keep): (Option<Box<dyn Read>>, Option<OwnedFd>) = match transport {
        "shm" => {
            map_shared(&path, true);
            child.env("BENCH_IPC_CHILD", &path);
            (None, None)
        }
        "unix" => {
            let (a, b) = UnixStream::pair()?;
            pass_fd(&mut child, b.into());
            (Some(Box::new(a.try_clone()?)), Some(a.into()))
        }
        "tcp" => {
            let l = TcpListener::bind("127.0.0.1:0")?;
            let b = TcpStream::connect(l.local_addr()?)?;
            let (a, _) = l.accept()?;
            a.set_nodelay(true)?;
            b.set_nodelay(true)?;
            pass_fd(&mut child, b.into());
            (Some(Box::new(a.try_clone()?)), Some(a.into()))
        }
        _ => panic!("transport: shm, unix or tcp"),
    };
    let mut proc = child.spawn()?;
    if let Some(r) = started.as_mut() {
        let mut one = [0u8];
        r.read_exact(&mut one)?;
    } else {
        // The child maps the file on start; give it a moment, the first
        // messages wait in the ring anyway.
    }
    let c0 = cpu_us();
    let t0 = Instant::now();
    match transport {
        "shm" => shm("a", mode, count, &path),
        _ => sock(keep.as_ref().unwrap().as_raw_fd(), "a", mode, count),
    }
    let status = proc.wait()?;
    let secs = t0.elapsed().as_secs_f64();
    let cpu = cpu_us() - c0;
    let _ = std::fs::remove_file(&path);
    assert!(status.success());
    println!(
        "{transport},{mode},{count},{:.0},{:.3},{:.3}",
        count as f64 / secs,
        secs * 1e6 / count as f64,
        cpu / count as f64
    );
    Ok(())
}

/// Makes `fd` the child's descriptor 3.
fn pass_fd(cmd: &mut Command, fd: OwnedFd) {
    use std::os::unix::process::CommandExt;
    cmd.env("BENCH_IPC_CHILD", "sock");
    let raw = fd.as_raw_fd();
    std::mem::forget(fd);
    // SAFETY: dup2 and fcntl are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(raw, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
