//! Do registered (fixed) buffers pay off? Raw io_uring, one thread, no giong:
//! the same operation with a plain buffer and with a registered one.
//!
//!   fixed file <path> <ops> [direct]   4 KiB reads at random aligned offsets
//!   fixed send <ops> <len>             zero-copy sends over loopback
//!
//! Prints CSV: case,variant,ops,secs,cpu_us_per_op

use std::os::fd::AsRawFd;
use std::time::Instant;

use io_uring::{opcode, types, IoUring};

fn cpu_us() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1e6 + t.tv_usec as f64;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

struct Aligned(*mut u8, usize);
impl Aligned {
    fn new(len: usize) -> Self {
        let mut p = std::ptr::null_mut();
        assert_eq!(unsafe { libc::posix_memalign(&mut p, 4096, len) }, 0);
        unsafe { std::ptr::write_bytes(p as *mut u8, 1, len) };
        Self(p as *mut u8, len)
    }
}

fn run(name: &str, variant: &str, ops: u64, mut f: impl FnMut(u64)) {
    let (t, c) = (Instant::now(), cpu_us());
    for i in 0..ops {
        f(i);
    }
    let (secs, cpu) = (t.elapsed().as_secs_f64(), cpu_us() - c);
    println!("{name},{variant},{ops},{secs:.3},{:.3}", cpu / ops as f64);
}

fn file(path: &str, ops: u64, direct: bool) {
    const BS: usize = 4096;
    const DEPTH: usize = 32;
    let mut flags = libc::O_RDONLY;
    if direct {
        flags |= libc::O_DIRECT;
    }
    let c = std::ffi::CString::new(path).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    let size = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st);
        st.st_size as u64
    };
    let blocks = size / BS as u64;
    let mut ring = IoUring::new(256).unwrap();
    let mem = Aligned::new(BS * DEPTH);
    let iov = libc::iovec { iov_base: mem.0.cast(), iov_len: mem.1 };
    unsafe { ring.submitter().register_buffers(&[iov]).unwrap() };
    let mut x = 0x9E37_79B9u64;
    for fixed in [false, true, false, true] {
        let variant = if fixed { "fixed" } else { "plain" };
        let mode = if direct { "file-direct" } else { "file-cached" };
        run(mode, variant, ops / DEPTH as u64, |_| {
            for d in 0..DEPTH {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let off = (x % blocks) * BS as u64;
                let buf = unsafe { mem.0.add(d * BS) };
                let sqe = if fixed {
                    opcode::ReadFixed::new(types::Fd(fd), buf, BS as u32, 0).offset(off).build()
                } else {
                    opcode::Read::new(types::Fd(fd), buf, BS as u32).offset(off).build()
                };
                unsafe { ring.submission().push(&sqe).unwrap() };
            }
            ring.submit_and_wait(DEPTH).unwrap();
            for cqe in ring.completion() {
                assert_eq!(cqe.result(), BS as i32);
            }
        });
    }
}

fn send(ops: u64, len: usize) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    let sink = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut b = vec![0u8; 1 << 20];
        while std::io::Read::read(&mut s, &mut b).unwrap() > 0 {}
    });
    let s = std::net::TcpStream::connect(a).unwrap();
    let fd = s.as_raw_fd();
    let mut ring = IoUring::new(64).unwrap();
    let mem = Aligned::new(len);
    let iov = libc::iovec { iov_base: mem.0.cast(), iov_len: mem.1 };
    unsafe { ring.submitter().register_buffers(&[iov]).unwrap() };
    for fixed in [false, true, false, true] {
        let variant = if fixed { "fixed" } else { "plain" };
        run(&format!("send-zc-{len}"), variant, ops, |_| {
            let mut sqe = opcode::SendZc::new(types::Fd(fd), mem.0, len as u32);
            if fixed {
                sqe = sqe.buf_index(Some(0));
            }
            unsafe { ring.submission().push(&sqe.build()).unwrap() };
            // Result and notification.
            let mut seen = 0;
            let mut sent = 0;
            while seen < 2 {
                ring.submit_and_wait(1).unwrap();
                for cqe in ring.completion() {
                    if io_uring::cqueue::notif(cqe.flags()) {
                        seen += 1;
                    } else {
                        sent = cqe.result();
                        seen += 1;
                        if !io_uring::cqueue::more(cqe.flags()) {
                            seen += 1;
                        }
                    }
                }
            }
            assert_eq!(sent, len as i32);
        });
    }
    drop(s);
    sink.join().unwrap();
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "file" => file(&a[2], a[3].parse().unwrap(), a.get(4).is_some_and(|s| s == "direct")),
        "send" => send(a[2].parse().unwrap(), a[3].parse().unwrap()),
        c => panic!("unknown {c}"),
    }
}
