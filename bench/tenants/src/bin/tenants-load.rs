//! The load of `bench/tenants.sh`.
//!
//!     tenants-load <addr> <seconds> <b_rate> <b_conns> <deadline_ms> <a_conns>
//!
//! Tenant B sends `b_rate` requests per second, open loop, spread over
//! `b_conns` keep-alive connections: each request has a scheduled time, and
//! counts as a success when a `200` answer arrives within `deadline_ms` of
//! that time (a request sent late because its connection was still busy
//! keeps its scheduled time). Tenant A, when `a_conns` > 0, sends requests
//! back to back on `a_conns` connections, as fast as it gets answers: as
//! much load as the server will take. Prints one CSV line:
//! `b_sent,b_ok_in_deadline,b_ok_late,b_err,b_p50_ms,b_p99_ms,a_ok,a_err`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn request(tenant: &str) -> Vec<u8> {
    format!("GET /work HTTP/1.1\r\nHost: bench\r\nx-tenant: {tenant}\r\n\r\n").into_bytes()
}

/// Sends one request and reads its answer; the status code.
fn exchange(stream: &mut TcpStream, req: &[u8], buf: &mut Vec<u8>) -> std::io::Result<u16> {
    stream.write_all(req)?;
    buf.clear();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find(buf, b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..end]).unwrap_or("");
            let status: u16 = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
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
            return Ok(status);
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Connects, trying for up to a second (the server may be gone: killed for
/// running out of memory).
fn connect(addr: &str) -> Option<TcpStream> {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(s) = TcpStream::connect(addr) {
            s.set_nodelay(true).ok();
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            return Some(s);
        }
        if Instant::now() > until {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let addr = a[0].clone();
    let secs: u64 = a[1].parse().unwrap();
    let b_rate: f64 = a[2].parse().unwrap();
    let b_conns: usize = a[3].parse().unwrap();
    let deadline = Duration::from_millis(a[4].parse().unwrap());
    let a_conns: usize = a[5].parse().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let a_ok = Arc::new(AtomicU64::new(0));
    let a_err = Arc::new(AtomicU64::new(0));
    let mut a_threads = Vec::new();
    for _ in 0..a_conns {
        let (addr, stop, ok, err) = (addr.clone(), stop.clone(), a_ok.clone(), a_err.clone());
        a_threads.push(thread::spawn(move || {
            let req = request("a");
            let mut buf = Vec::new();
            let mut s = connect(&addr);
            while !stop.load(Ordering::Relaxed) {
                let Some(stream) = s.as_mut() else {
                    thread::sleep(Duration::from_millis(100));
                    s = connect(&addr);
                    continue;
                };
                match exchange(stream, &req, &mut buf) {
                    Ok(200) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        err.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        err.fetch_add(1, Ordering::Relaxed);
                        s = None;
                    }
                }
            }
        }));
    }
    if a_conns > 0 {
        // Let A fill the server before B starts.
        thread::sleep(Duration::from_secs(1));
    }

    // B: connection i sends the requests scheduled at start + (i + k * conns) / rate.
    let start = Instant::now() + Duration::from_millis(100);
    let total = (b_rate * secs as f64) as u64;
    let results = Arc::new(Mutex::new(Vec::new()));
    let mut b_threads = Vec::new();
    for i in 0..b_conns {
        let (addr, results) = (addr.clone(), results.clone());
        b_threads.push(thread::spawn(move || {
            let req = request("b");
            let mut buf = Vec::new();
            let mut s = connect(&addr);
            let mut mine = Vec::new();
            let mut k = i as u64;
            while k < total {
                let at = start + Duration::from_secs_f64(k as f64 / b_rate);
                let now = Instant::now();
                if at > now {
                    thread::sleep(at - now);
                }
                if s.is_none() {
                    s = connect(&addr);
                }
                let r = match s.as_mut() {
                    Some(stream) => exchange(stream, &req, &mut buf),
                    None => Err(std::io::ErrorKind::ConnectionRefused.into()),
                };
                let took = at.elapsed();
                match r {
                    Ok(status) => mine.push((status, took)),
                    Err(_) => {
                        mine.push((0, took));
                        s = None;
                    }
                }
                k += b_conns as u64;
            }
            results.lock().unwrap().extend(mine);
        }));
    }
    for t in b_threads {
        t.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let a_ok_n = a_ok.load(Ordering::Relaxed);
    let a_err_n = a_err.load(Ordering::Relaxed);

    let r = results.lock().unwrap();
    let in_deadline = r.iter().filter(|(s, t)| *s == 200 && *t <= deadline).count();
    let late = r.iter().filter(|(s, t)| *s == 200 && *t > deadline).count();
    let err = r.iter().filter(|(s, _)| *s != 200).count();
    let mut lat: Vec<f64> = r
        .iter()
        .filter(|(s, _)| *s == 200)
        .map(|(_, t)| t.as_secs_f64() * 1e3)
        .collect();
    lat.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let pct = |p: f64| {
        if lat.is_empty() {
            f64::NAN
        } else {
            lat[((lat.len() - 1) as f64 * p) as usize]
        }
    };
    println!(
        "{},{},{},{},{:.2},{:.2},{},{}",
        total,
        in_deadline,
        late,
        err,
        pct(0.5),
        pct(0.99),
        a_ok_n,
        a_err_n
    );
    // A's threads may sit in a read; the process exits without them.
    std::process::exit(0);
}
