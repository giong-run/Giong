//! The load of `bench/spin.sh`.
//!
//!     spin-load <addr> <seconds> <rate> <conns>
//!
//! Sends `rate` requests per second (`GET /json`), open loop, spread evenly
//! over `conns` keep-alive connections, each on a thread of its own. Every
//! request has a scheduled time. Its latency runs from when it was sent to
//! when its answer arrived, or from its scheduled time when it went out late
//! because its connection was still waiting for the previous answer (so a
//! slow server is not hidden by sending less). The sending thread's own
//! oversleeping is not counted: it is the client's, not the server's.
//! Prints one CSV line: `sent,ok,err,rps,p50_us,p99_us,p999_us,max_us`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const REQUEST: &[u8] = b"GET /json HTTP/1.1\r\nHost: bench\r\n\r\n";

/// Sends one request and reads one answer.
fn exchange(stream: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<()> {
    stream.write_all(REQUEST)?;
    buf.clear();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..end]).unwrap_or("");
            let len: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return Ok(());
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args[1].clone();
    let secs: f64 = args[2].parse().unwrap();
    let rate: f64 = args[3].parse().unwrap();
    let conns: usize = args[4].parse().unwrap();
    // Sleep as precisely as the kernel can: the default 50 µs timer slack
    // would make every gap longer than asked.
    // SAFETY: plain prctl with integer arguments.
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1, 0, 0, 0) };

    let interval = Duration::from_secs_f64(conns as f64 / rate);
    let start = Instant::now() + Duration::from_millis(200);
    let end = start + Duration::from_secs_f64(secs);
    let results = Arc::new(Mutex::new((Vec::<u32>::new(), 0u64)));
    let mut threads = Vec::new();
    for c in 0..conns {
        let addr = addr.clone();
        let results = results.clone();
        threads.push(thread::spawn(move || {
            // SAFETY: as above.
            unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1, 0, 0, 0) };
            let mut stream = TcpStream::connect(&addr).expect("connect");
            stream.set_nodelay(true).ok();
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut lat = Vec::new();
            let mut errors = 0u64;
            // Connections start evenly spread over one interval.
            let mut next = start + interval.mul_f64(c as f64 / conns as f64);
            while next < end {
                let now = Instant::now();
                let late = now > next;
                if !late {
                    thread::sleep(next - now);
                }
                let sent = Instant::now();
                let from = if late { next } else { sent };
                match exchange(&mut stream, &mut buf) {
                    Ok(()) => lat.push(from.elapsed().as_micros().min(u32::MAX as u128) as u32),
                    Err(_) => {
                        errors += 1;
                        match TcpStream::connect(&addr) {
                            Ok(s) => {
                                s.set_nodelay(true).ok();
                                s.set_read_timeout(Some(Duration::from_secs(5))).ok();
                                stream = s;
                            }
                            Err(_) => thread::sleep(Duration::from_millis(10)),
                        }
                    }
                }
                next += interval;
            }
            let mut r = results.lock().unwrap();
            r.0.extend(lat);
            r.1 += errors;
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let (mut lat, errors) = std::mem::take(&mut *results.lock().unwrap());
    lat.sort_unstable();
    let q = |p: f64| -> u32 {
        if lat.is_empty() {
            return 0;
        }
        lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)]
    };
    let ok = lat.len() as u64;
    println!(
        "{},{},{},{:.0},{},{},{},{}",
        ok + errors,
        ok,
        errors,
        ok as f64 / secs,
        q(0.50),
        q(0.99),
        q(0.999),
        lat.last().copied().unwrap_or(0)
    );
}
