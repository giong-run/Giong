//! The benchmark lab's load generator (bench/lab/README.md).
//!
//! HTTP/1.1, one request at a time per connection, on raw epoll and threads,
//! so that it uses none of the runtimes it measures.
//!
//! * Closed loop (`--rate 0`): each connection sends its next request as soon
//!   as the answer to the last one is in. Measures throughput.
//! * Open loop (`--rate R`): requests are due at a fixed total rate, spread
//!   over the connections. A request's latency counts from when it was due,
//!   not from when it could be sent, so a server that stalls is charged for
//!   every request that waited behind the stall (no coordinated omission).
//!   Measures latency at a given load, up to p99.99.
//! * Hold (`--hold`): opens the connections, answers one request on each and
//!   keeps them open and quiet until stdin closes. For memory per connection.
//!
//! Prints one JSON object on stdout. Latencies are kept in a histogram with
//! 128 sub-buckets per power of two (under 0.8% error), in nanoseconds.

use std::collections::BinaryHeap;
use std::cmp::Reverse;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SUB: u64 = 128;
const BUCKETS: usize = (SUB + 57 * SUB + SUB) as usize;

#[derive(Clone)]
struct Hist {
    counts: Vec<u64>,
    n: u64,
    max: u64,
}

impl Hist {
    fn new() -> Self {
        Hist { counts: vec![0; BUCKETS], n: 0, max: 0 }
    }
    fn index(v: u64) -> usize {
        if v < SUB {
            return v as usize;
        }
        let e = 63 - v.leading_zeros() as u64; // >= 7
        let m = (v >> (e - 7)) - SUB;
        (SUB + (e - 7) * SUB + m) as usize
    }
    fn lower(i: usize) -> u64 {
        let i = i as u64;
        if i < SUB {
            return i;
        }
        let e = (i - SUB) / SUB + 7;
        let m = (i - SUB) % SUB;
        (SUB + m) << (e - 7)
    }
    fn add(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.n += 1;
        self.max = self.max.max(v);
    }
    fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.n += o.n;
        self.max = self.max.max(o.max);
    }
    /// The value at quantile `q`: the midpoint of its bucket.
    fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let rank = ((q * self.n as f64).ceil() as u64).clamp(1, self.n);
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                let lo = Self::lower(i);
                let hi = Self::lower(i + 1).max(lo + 1);
                return ((lo + hi) / 2).min(self.max);
            }
        }
        self.max
    }
    fn json(&self) -> String {
        let us = |ns: u64| ns as f64 / 1000.0;
        format!(
            "{{\"count\":{},\"p50_us\":{:.1},\"p90_us\":{:.1},\"p99_us\":{:.1},\"p999_us\":{:.1},\"p9999_us\":{:.1},\"max_us\":{:.1}}}",
            self.n,
            us(self.quantile(0.5)),
            us(self.quantile(0.9)),
            us(self.quantile(0.99)),
            us(self.quantile(0.999)),
            us(self.quantile(0.9999)),
            us(self.max)
        )
    }
}

struct Args {
    addr: SocketAddrV4,
    conns: usize,
    threads: usize,
    duration: f64,
    warmup: f64,
    rate: f64,
    mix: Vec<(String, u32)>,
    body: usize,
    src_ips: u32,
    hold: bool,
    headers: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: lab-loadgen --addr 127.0.0.1:3006 [--conns 50] [--threads 1] [--duration 10] [--warmup 2]\n\
         \x20                  [--rate 0] [--mix /json@9,/compute?n=27@1] [--body 0] [--src-ips 1] [--hold]\n\
         \x20                  [--header 'Accept-Encoding: gzip' ...]\n\
         Paths starting with /upload are POSTs carrying --body bytes."
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:3006".parse().unwrap(),
        conns: 50,
        threads: 1,
        duration: 10.0,
        warmup: 2.0,
        rate: 0.0,
        mix: vec![("/json".into(), 1)],
        body: 0,
        src_ips: 1,
        hold: false,
        headers: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "--hold" {
            a.hold = true;
            continue;
        }
        let v = it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--addr" => a.addr = v.parse().unwrap_or_else(|_| usage()),
            "--conns" => a.conns = v.parse().unwrap_or_else(|_| usage()),
            "--threads" => a.threads = v.parse().unwrap_or_else(|_| usage()),
            "--duration" => a.duration = v.parse().unwrap_or_else(|_| usage()),
            "--warmup" => a.warmup = v.parse().unwrap_or_else(|_| usage()),
            "--rate" => a.rate = v.parse().unwrap_or_else(|_| usage()),
            "--body" => a.body = v.parse().unwrap_or_else(|_| usage()),
            "--src-ips" => a.src_ips = v.parse().unwrap_or_else(|_| usage()),
            "--header" => {
                if !v.contains(':') || v.contains('\r') || v.contains('\n') {
                    usage();
                }
                a.headers.push_str(&v);
                a.headers.push_str("\r\n");
            }
            "--mix" => {
                a.mix = v
                    .split(',')
                    .map(|item| match item.rsplit_once('@') {
                        Some((p, w)) => (p.to_string(), w.parse().unwrap_or_else(|_| usage())),
                        None => (item.to_string(), 1),
                    })
                    .collect()
            }
            _ => usage(),
        }
    }
    if a.threads == 0 || a.conns == 0 || a.mix.is_empty() || a.src_ips == 0 {
        usage();
    }
    a.threads = a.threads.min(a.conns);
    a
}

fn now_ns(base: Instant) -> u64 {
    base.elapsed().as_nanos() as u64
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Connecting,
    Idle,
    Writing,
    Head,
    Body,
    Dead,
}

struct Conn {
    fd: i32,
    state: State,
    src: Ipv4Addr,
    req: usize,
    sent: usize,
    due: u64,
    next_due: u64,
    head: Vec<u8>,
    body_left: usize,
    held: bool,
}

struct Shared {
    args: Args,
    reqs: Vec<Vec<u8>>,
    weights: Vec<u32>,
    base: Instant,
    measure_from: u64,
    measure_to: u64,
    stop: AtomicBool,
    held: AtomicUsize,
}

#[derive(Default)]
struct Totals {
    ok: Vec<u64>,
    errors: u64,
    connect_errors: u64,
    unfinished: u64,
    non_2xx: u64,
    bytes_in: u64,
    /// Answers that arrived inside the measurement, whenever their request fell due.
    answered: u64,
    hists: Vec<Hist>,
}

fn cvt(r: i32) -> std::io::Result<i32> {
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn open_socket(addr: SocketAddrV4, src: Option<Ipv4Addr>) -> std::io::Result<i32> {
    unsafe {
        let fd = cvt(libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        ))?;
        let one: i32 = 1;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void,
            4,
        );
        let sa = |ip: Ipv4Addr, port: u16| libc::sockaddr_in {
            sin_family: libc::AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: u32::from(ip).to_be() },
            sin_zero: [0; 8],
        };
        if let Some(ip) = src {
            let s = sa(ip, 0);
            if libc::bind(fd, &s as *const _ as *const libc::sockaddr, 16) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
        }
        let d = sa(*addr.ip(), addr.port());
        if libc::connect(fd, &d as *const _ as *const libc::sockaddr, 16) < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                libc::close(fd);
                return Err(e);
            }
        }
        Ok(fd)
    }
}

fn epoll_add(ep: i32, fd: i32, token: u64) {
    let mut ev = libc::epoll_event {
        events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32,
        u64: token,
    };
    unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) };
}

/// Parses an HTTP/1.1 response head: status and Content-Length.
fn parse_head(head: &[u8]) -> Option<(u16, usize)> {
    let status = head.get(9..12)?;
    let status = std::str::from_utf8(status).ok()?.parse().ok()?;
    let mut len = 0;
    for line in head.split(|&c| c == b'\n') {
        if line.len() > 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            len = std::str::from_utf8(&line[15..]).ok()?.trim().parse().ok()?;
        }
    }
    Some((status, len))
}

struct Worker<'a> {
    sh: &'a Shared,
    /// CLOCK_MONOTONIC at `Shared::base`, for the timer.
    base_mono: u64,
    ep: i32,
    timer: i32,
    conns: Vec<Conn>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    rng: u64,
    interval: u64,
    t: Totals,
    scratch: Vec<u8>,
    armed: u64,
}

impl Worker<'_> {
    fn pick(&mut self) -> usize {
        if self.sh.weights.len() == 1 {
            return 0;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let total: u32 = self.sh.weights.iter().sum();
        let mut r = (self.rng % total as u64) as u32;
        for (i, &w) in self.sh.weights.iter().enumerate() {
            if r < w {
                return i;
            }
            r -= w;
        }
        0
    }

    fn connect(&mut self, i: usize) {
        let src = (self.sh.args.src_ips > 1).then_some(self.conns[i].src);
        match open_socket(self.sh.args.addr, src) {
            Ok(fd) => {
                self.conns[i].fd = fd;
                self.conns[i].state = State::Connecting;
                epoll_add(self.ep, fd, i as u64);
            }
            Err(_) => {
                self.t.connect_errors += 1;
                self.conns[i].state = State::Dead;
            }
        }
    }

    fn fail(&mut self, i: usize) {
        let c = &mut self.conns[i];
        if c.state != State::Connecting {
            self.t.errors += 1;
        } else {
            self.t.connect_errors += 1;
        }
        unsafe { libc::close(c.fd) };
        c.head.clear();
        if self.sh.stop.load(Ordering::Relaxed) || self.sh.args.hold {
            c.state = State::Dead;
            return;
        }
        // The request in flight is lost; the connection is opened again and
        // keeps its schedule.
        self.connect(i);
    }

    /// The connection is idle: sends its next request now or at its time.
    fn schedule(&mut self, i: usize, now: u64) {
        let open = self.interval > 0;
        if self.sh.args.hold {
            if self.conns[i].held {
                return;
            }
            self.start(i, now);
            return;
        }
        if !open {
            self.conns[i].due = now;
            self.start(i, now);
        } else if self.conns[i].next_due <= now {
            let due = self.conns[i].next_due;
            self.conns[i].next_due += self.interval;
            self.conns[i].due = due;
            self.start(i, now);
        } else {
            self.heap.push(Reverse((self.conns[i].next_due, i)));
            self.arm();
        }
    }

    fn arm(&mut self) {
        if let Some(&Reverse((at, _))) = self.heap.peek() {
            if self.armed != 0 && self.armed <= at {
                return;
            }
            self.armed = at;
            let abs = self.base_mono + at;
            let spec = libc::itimerspec {
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                it_value: libc::timespec {
                    tv_sec: (abs / 1_000_000_000) as i64,
                    tv_nsec: (abs % 1_000_000_000) as i64,
                },
            };
            unsafe {
                libc::timerfd_settime(self.timer, libc::TFD_TIMER_ABSTIME, &spec, std::ptr::null_mut())
            };
        }
    }

    fn start(&mut self, i: usize, now: u64) {
        let req = self.pick();
        let c = &mut self.conns[i];
        c.req = req;
        c.sent = 0;
        if self.interval == 0 {
            c.due = now;
        }
        c.state = State::Writing;
        self.write(i);
    }

    fn write(&mut self, i: usize) {
        let bytes = &self.sh.reqs[self.conns[i].req];
        loop {
            let c = &mut self.conns[i];
            let left = &bytes[c.sent..];
            if left.is_empty() {
                c.state = State::Head;
                return;
            }
            let n = unsafe { libc::write(c.fd, left.as_ptr() as *const libc::c_void, left.len()) };
            if n < 0 {
                let e = std::io::Error::last_os_error().raw_os_error();
                if e == Some(libc::EAGAIN) {
                    return;
                }
                return self.fail(i);
            }
            c.sent += n as usize;
        }
    }

    fn read(&mut self, i: usize) {
        loop {
            let fd = self.conns[i].fd;
            let n = unsafe {
                libc::read(fd, self.scratch.as_mut_ptr() as *mut libc::c_void, self.scratch.len())
            };
            if n == 0 {
                return self.fail(i);
            }
            if n < 0 {
                let e = std::io::Error::last_os_error().raw_os_error();
                if e == Some(libc::EAGAIN) {
                    return;
                }
                return self.fail(i);
            }
            let n = n as usize;
            self.t.bytes_in += n as u64;
            let mut off = 0;
            while off < n {
                let c = &mut self.conns[i];
                match c.state {
                    State::Head => {
                        let before = c.head.len();
                        c.head.extend_from_slice(&self.scratch[off..n]);
                        let Some(end) = c.head.windows(4).position(|w| w == b"\r\n\r\n") else {
                            off = n;
                            if c.head.len() > 64 * 1024 {
                                return self.fail(i);
                            }
                            continue;
                        };
                        let Some((status, len)) = parse_head(&c.head[..end + 4]) else {
                            return self.fail(i);
                        };
                        off += end + 4 - before;
                        c.head.clear();
                        if !(200..300).contains(&status) {
                            self.t.non_2xx += 1;
                        }
                        c.body_left = len;
                        c.state = State::Body;
                        if len == 0 {
                            self.done(i);
                        }
                    }
                    State::Body => {
                        let take = c.body_left.min(n - off);
                        c.body_left -= take;
                        off += take;
                        if c.body_left == 0 {
                            self.done(i);
                        }
                    }
                    // Bytes nobody asked for: the server is broken.
                    _ => return self.fail(i),
                }
            }
        }
    }

    fn done(&mut self, i: usize) {
        let now = now_ns(self.sh.base);
        let c = &mut self.conns[i];
        c.state = State::Idle;
        let due = c.due;
        if self.sh.args.hold {
            c.held = true;
            self.sh.held.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if now >= self.sh.measure_from && now <= self.sh.measure_to {
            self.t.answered += 1;
        }
        if due >= self.sh.measure_from && now <= self.sh.measure_to {
            self.t.ok[c.req] += 1;
            self.t.hists[c.req].add(now.saturating_sub(due));
        }
        if !self.sh.stop.load(Ordering::Relaxed) {
            self.schedule(i, now);
        }
    }

    fn event(&mut self, i: usize, ev: u32) {
        let state = self.conns[i].state;
        if state == State::Dead {
            return;
        }
        if state == State::Connecting {
            if ev & libc::EPOLLOUT as u32 == 0 && ev & (libc::EPOLLERR | libc::EPOLLHUP) as u32 == 0 {
                return;
            }
            let mut err: i32 = 0;
            let mut len = 4u32;
            unsafe {
                libc::getsockopt(
                    self.conns[i].fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if err != 0 || ev & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                return self.fail(i);
            }
            self.conns[i].state = State::Idle;
            let now = now_ns(self.sh.base);
            return self.schedule(i, now);
        }
        if ev & libc::EPOLLOUT as u32 != 0 && self.conns[i].state == State::Writing {
            self.write(i);
        }
        if ev & (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
            match self.conns[i].state {
                State::Head | State::Body => self.read(i),
                // An idle or held connection the server closed.
                State::Idle | State::Writing => {
                    let mut b = [0u8; 1];
                    let n = unsafe { libc::read(self.conns[i].fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
                    if n == 0 || (n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN)) {
                        self.fail(i);
                    }
                }
                _ => {}
            }
        }
    }

    fn timer_fired(&mut self) {
        let mut buf = [0u8; 8];
        unsafe { libc::read(self.timer, buf.as_mut_ptr() as *mut libc::c_void, 8) };
        self.armed = 0;
        let now = now_ns(self.sh.base);
        while let Some(&Reverse((at, i))) = self.heap.peek() {
            if at > now {
                break;
            }
            self.heap.pop();
            if self.conns[i].state == State::Idle && !self.sh.stop.load(Ordering::Relaxed) {
                self.schedule(i, now);
            }
        }
        self.arm();
    }

    fn run(&mut self) {
        let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 1024];
        let timer_token = u64::MAX;
        epoll_add(self.ep, self.timer, timer_token);
        // Open the connections in batches, so that the listen queue keeps up.
        let mut next = 0;
        while !self.sh.stop.load(Ordering::Relaxed) || self.sh.args.hold {
            if next < self.conns.len() {
                let end = (next + 256).min(self.conns.len());
                for i in next..end {
                    self.connect(i);
                }
                next = end;
            }
            if self.sh.args.hold && self.sh.stop.load(Ordering::Relaxed) {
                break;
            }
            let n = unsafe { libc::epoll_wait(self.ep, events.as_mut_ptr(), events.len() as i32, 20) };
            for e in &events[..n.max(0) as usize] {
                let (tok, ev) = (e.u64, e.events);
                if tok == timer_token {
                    self.timer_fired();
                } else {
                    self.event(tok as usize, ev);
                }
            }
            if !self.sh.args.hold && now_ns(self.sh.base) >= self.sh.measure_to {
                break;
            }
        }
        // Requests due inside the measurement and not answered by its end.
        for c in &self.conns {
            if matches!(c.state, State::Writing | State::Head | State::Body)
                && c.due >= self.sh.measure_from
                && c.due < self.sh.measure_to
            {
                self.t.unfinished += 1;
            }
        }
        for c in &self.conns {
            if c.state != State::Dead {
                unsafe { libc::close(c.fd) };
            }
        }
    }
}

fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn main() {
    let args = parse_args();
    let host = args.addr.to_string();
    let extra = args.headers.clone();
    let body = vec![b'a'; args.body];
    let reqs: Vec<Vec<u8>> = args
        .mix
        .iter()
        .map(|(path, _)| {
            if path.starts_with("/upload") {
                let mut r =
                    format!("POST {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Length: {}\r\n\r\n", body.len())
                        .into_bytes();
                r.extend_from_slice(&body);
                r
            } else {
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n").into_bytes()
            }
        })
        .collect();
    let weights = args.mix.iter().map(|(_, w)| *w).collect();
    let base = Instant::now();
    let base_mono = mono_ns();
    let warm = (args.warmup * 1e9) as u64;
    let dur = (args.duration * 1e9) as u64;
    let (measure_from, measure_to) = if args.hold { (0, u64::MAX) } else { (warm, warm + dur) };
    let sh = Arc::new(SharedWithBase {
        inner: Shared {
            reqs,
            weights,
            base,
            measure_from,
            measure_to,
            stop: AtomicBool::new(false),
            held: AtomicUsize::new(0),
            args,
        },
        base_mono,
    });

    let threads = sh.inner.args.threads;
    let conns = sh.inner.args.conns;
    let mut handles = Vec::new();
    for t in 0..threads {
        let sh = sh.clone();
        handles.push(std::thread::spawn(move || {
            let s = &sh.inner;
            let mine: Vec<usize> = (0..conns).filter(|c| c % threads == t).collect();
            let per_conn_rate = if s.args.rate > 0.0 { s.args.rate / conns as f64 } else { 0.0 };
            let interval = if per_conn_rate > 0.0 { (1e9 / per_conn_rate) as u64 } else { 0 };
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (t as u64 + 1).wrapping_mul(0xBF58476D1CE4E5B9);
            let mut w = Worker {
                sh: s,
                base_mono: sh.base_mono,
                ep: unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) },
                timer: unsafe {
                    libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
                },
                conns: mine
                    .iter()
                    .map(|&c| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        Conn {
                            fd: -1,
                            state: State::Dead,
                            src: Ipv4Addr::new(127, 0, 0, 1 + (c as u32 % s.args.src_ips) as u8),
                            req: 0,
                            sent: 0,
                            due: 0,
                            // A random phase, so that the connections do not send together.
                            next_due: if interval > 0 { rng % interval } else { 0 },
                            head: Vec::new(),
                            body_left: 0,
                            held: false,
                        }
                    })
                    .collect(),
                heap: BinaryHeap::new(),
                rng,
                interval,
                t: Totals {
                    ok: vec![0; s.reqs.len()],
                    hists: vec![Hist::new(); s.reqs.len()],
                    ..Totals::default()
                },
                scratch: vec![0; 256 * 1024],
                armed: 0,
            };
            w.run();
            w.t
        }));
    }

    let s = &sh.inner;
    if s.args.hold {
        // Wait until every connection has had its answer (or failed), report,
        // then hold until stdin closes.
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            if s.held.load(Ordering::Relaxed) >= s.args.conns || start.elapsed() > Duration::from_secs(120) {
                break;
            }
        }
        println!("HELD {} {:.2}", s.held.load(Ordering::Relaxed), start.elapsed().as_secs_f64());
        std::io::stdout().flush().ok();
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        s.stop.store(true, Ordering::Relaxed);
    } else {
        std::thread::sleep(Duration::from_nanos(measure_to) + Duration::from_millis(50));
        s.stop.store(true, Ordering::Relaxed);
    }

    let mut total = Totals { ok: vec![0; s.reqs.len()], hists: vec![Hist::new(); s.reqs.len()], ..Totals::default() };
    for h in handles {
        let t = h.join().unwrap();
        for i in 0..s.reqs.len() {
            total.ok[i] += t.ok[i];
            total.hists[i].merge(&t.hists[i]);
        }
        total.errors += t.errors;
        total.connect_errors += t.connect_errors;
        total.unfinished += t.unfinished;
        total.non_2xx += t.non_2xx;
        total.bytes_in += t.bytes_in;
        total.answered += t.answered;
    }
    let mut all = Hist::new();
    for h in &total.hists {
        all.merge(h);
    }
    let secs = s.args.duration;
    let ok: u64 = total.ok.iter().sum();
    let per_path: Vec<String> = s
        .args
        .mix
        .iter()
        .enumerate()
        .map(|(i, (p, _))| {
            format!(
                "{{\"path\":\"{}\",\"requests\":{},\"latency\":{}}}",
                p.replace('"', ""),
                total.ok[i],
                total.hists[i].json()
            )
        })
        .collect();
    println!(
        "{{\"conns\":{},\"threads\":{},\"rate\":{},\"duration_s\":{},\"requests\":{},\"rps\":{:.1},\"goodput_rps\":{:.1},\"errors\":{},\"connect_errors\":{},\"unfinished\":{},\"non_2xx\":{},\"mb_in_per_s\":{:.2},\"latency\":{},\"paths\":[{}]}}",
        s.args.conns,
        s.args.threads,
        s.args.rate,
        secs,
        ok,
        ok as f64 / secs,
        total.answered as f64 / secs,
        total.errors,
        total.connect_errors,
        total.unfinished,
        total.non_2xx,
        total.bytes_in as f64 / secs / 1e6,
        all.json(),
        per_path.join(",")
    );
}

struct SharedWithBase {
    inner: Shared,
    base_mono: u64,
}
