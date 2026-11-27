//! `giong bench`'s load generator: HTTP/1.1, one request at a time per
//! connection, one thread per connection, std only, so it builds and
//! behaves the same on Linux, macOS and Windows and uses none of the
//! runtimes it measures.
//!
//! * Fixed rate (`rate: Some(r)`): requests are due at `r` per second in
//!   total, spread evenly over the connections. A request's latency counts
//!   from when it was due, not from when it could be sent, so a service that
//!   stalls is charged for every request that waited behind the stall (no
//!   coordinated omission). The generator's own lateness (it woke after a
//!   request was due) is kept apart, in `lag`, so a busy machine is visible.
//! * As fast as it can (`rate: None`): each connection sends its next request
//!   when the answer to the last one is in. Measures throughput; latencies
//!   are those of a saturated service.
//!
//! The benchmark lab (`bench/lab/loadgen`) is the raw-epoll version of this
//! for numbers that matter; both use the histogram in [`crate::hist`].

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::hist::Hist;

/// The most connections (threads) one run opens.
pub const MAX_CONNS: usize = 4096;

/// One kind of request, ready to send.
#[derive(Clone, Debug)]
pub struct Request {
    pub bytes: Vec<u8>,
    /// A HEAD request: its answer has no body whatever it says.
    pub head: bool,
}

impl Request {
    /// `method path` with `Host`, optional extra header lines (`Name: value`)
    /// and an optional body.
    pub fn new(method: &str, path: &str, host: &str, headers: &[String], body: &[u8]) -> Request {
        let mut text =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: giong-bench\r\n");
        for h in headers {
            text.push_str(h.trim());
            text.push_str("\r\n");
        }
        if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
            text.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        text.push_str("\r\n");
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(body);
        Request {
            bytes,
            head: method == "HEAD",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub addr: SocketAddr,
    /// The requests, in the order each connection cycles through them
    /// (repeat one to weigh it); connection `i` starts at `i % len`.
    pub requests: Vec<Request>,
    pub conns: usize,
    /// Requests per second in total, or `None` for as fast as the service
    /// answers.
    pub rate: Option<f64>,
    /// Sent but not counted, so connections and caches are warm.
    pub warmup: Duration,
    pub duration: Duration,
    /// For connecting, writing and each read.
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Latency of each answered request in the measured window, in ns.
    pub latency: Hist,
    /// How late the generator woke for a due request, in ns (fixed rate only).
    pub lag: Hist,
    /// Answers by status code.
    pub statuses: BTreeMap<u16, u64>,
    /// Requests that got no answer (refused, reset, timed out, not HTTP).
    pub failed: u64,
    /// The first reason a request failed.
    pub first_error: Option<String>,
    /// Connections opened, the first ones included.
    pub connects: u64,
    /// The length of the measured window.
    pub window: Duration,
}

impl Outcome {
    pub fn answered(&self) -> u64 {
        self.latency.count()
    }

    /// Answered requests per second over the window.
    pub fn rps(&self) -> f64 {
        let s = self.window.as_secs_f64();
        if s > 0.0 {
            self.answered() as f64 / s
        } else {
            0.0
        }
    }

    /// Answers with a status of 400 or more.
    pub fn error_statuses(&self) -> u64 {
        self.statuses.range(400..).map(|(_, n)| n).sum()
    }

    fn merge(&mut self, o: Outcome) {
        self.latency.merge(&o.latency);
        self.lag.merge(&o.lag);
        for (k, v) in o.statuses {
            *self.statuses.entry(k).or_default() += v;
        }
        self.failed += o.failed;
        self.connects += o.connects;
        if self.first_error.is_none() {
            self.first_error = o.first_error;
        }
    }
}

/// Runs the plan and returns what it measured. Blocks for warmup plus
/// duration, plus up to `timeout` for answers still in flight at the end.
pub fn run(plan: &Plan) -> Outcome {
    assert!(!plan.requests.is_empty() && plan.conns > 0);
    let conns = plan.conns.min(MAX_CONNS);
    let start = Instant::now() + Duration::from_millis(20);
    let outcome = std::thread::scope(|s| {
        let handles: Vec<_> = (0..conns)
            .map(|i| {
                std::thread::Builder::new()
                    .name(format!("bench-{i}"))
                    .stack_size(256 * 1024)
                    .spawn_scoped(s, move || connection(plan, conns, i, start))
            })
            .collect();
        let mut total = Outcome::default();
        for h in handles {
            match h {
                Ok(h) => total.merge(h.join().unwrap_or_default()),
                Err(e) => {
                    total.failed += 1;
                    total
                        .first_error
                        .get_or_insert(format!("cannot start a thread: {e}"));
                }
            }
        }
        total
    });
    Outcome {
        window: plan.duration,
        ..outcome
    }
}

fn connection(plan: &Plan, conns: usize, i: usize, start: Instant) -> Outcome {
    let mut out = Outcome::default();
    let window = start + plan.warmup;
    let end = window + plan.duration;
    // Nothing waits past this: every read and connect is cut short here.
    let hard_stop = end + plan.timeout;
    let interval = plan
        .rate
        .map(|r| Duration::from_secs_f64(conns as f64 / r.max(1e-9)));
    // Connection i's first request is due i/rate after the start, so the
    // requests of all connections are spread evenly.
    let mut due = match plan.rate {
        Some(r) => start + Duration::from_secs_f64(i as f64 / r.max(1e-9)),
        None => start,
    };
    let mut next = i % plan.requests.len();
    let mut conn: Option<Conn> = None;
    loop {
        let t0 = match interval {
            Some(_) => {
                if due >= end {
                    break;
                }
                if Instant::now() < due {
                    sleep_until(due);
                    if due >= window {
                        out.lag
                            .add(nanos(Instant::now().saturating_duration_since(due)));
                    }
                }
                due
            }
            None => {
                let now = Instant::now();
                if now >= end {
                    break;
                }
                if now < start {
                    std::thread::sleep(start - now);
                }
                Instant::now()
            }
        };
        let counted = t0 >= window;
        let req = &plan.requests[next];
        // Every wait ends by `hard_stop`: a service that stops answering
        // costs one timeout in all, not one per request that came due.
        let deadline = (Instant::now() + plan.timeout).min(hard_stop);
        let result = exchange(plan, &mut conn, &mut out, req, deadline);
        match result {
            Ok(status) => {
                if counted {
                    out.latency
                        .add(nanos(Instant::now().saturating_duration_since(t0)));
                    *out.statuses.entry(status).or_default() += 1;
                }
            }
            Err(e) => {
                conn = None;
                if counted {
                    out.failed += 1;
                    out.first_error.get_or_insert(e);
                }
                if interval.is_none() {
                    // Do not spin on a service that refuses connections.
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        if let Some(iv) = interval {
            due += iv;
        }
        next = (next + 1) % plan.requests.len();
    }
    out
}

/// Sleeps until `due`. macOS may wake a sleeping thread late by up to the
/// length of its sleep (timer coalescing), which made an 80 ms wait between
/// requests end up to 80 ms late and every latency include it: there the
/// wait is cut in halves, each of which can end late only by its own length,
/// so the thread wakes by `due` after a few short sleeps. Linux's timer
/// slack is 50 µs, so one sleep is enough.
fn sleep_until(due: Instant) {
    loop {
        let left = due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        if cfg!(target_os = "linux") || left <= Duration::from_micros(200) {
            std::thread::sleep(left);
            return;
        }
        std::thread::sleep(left / 2);
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Sends one request and reads its answer, opening a connection if there is
/// none. A reused connection the service had already closed is retried once
/// on a new one, as browsers and HTTP clients do.
fn exchange(
    plan: &Plan,
    conn: &mut Option<Conn>,
    out: &mut Outcome,
    req: &Request,
    deadline: Instant,
) -> Result<u16, String> {
    for attempt in 0..2 {
        let reused = conn.is_some();
        if conn.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("no answer within {:?}", plan.timeout));
            }
            let stream = TcpStream::connect_timeout(&plan.addr, left)
                .map_err(|e| format!("cannot connect to {}: {e}", plan.addr))?;
            let _ = stream.set_nodelay(true);
            out.connects += 1;
            *conn = Some(Conn::new(stream));
        }
        let c = conn.as_mut().expect("connected above");
        match c.exchange(req, deadline) {
            Ok((status, keep)) => {
                if !keep {
                    *conn = None;
                }
                return Ok(status);
            }
            Err(Fail::Closed) if reused && attempt == 0 => {
                *conn = None;
            }
            Err(Fail::Closed) => {
                *conn = None;
                return Err("the service closed the connection without answering".into());
            }
            Err(Fail::Io(e)) => {
                *conn = None;
                return Err(
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) {
                        format!("no answer within {:?}", plan.timeout)
                    } else {
                        format!("the connection failed: {e}")
                    },
                );
            }
            Err(Fail::Http(why)) => {
                *conn = None;
                return Err(format!("the answer is not HTTP/1.1: {why}"));
            }
        }
    }
    unreachable!("the second attempt returns")
}

enum Fail {
    /// The connection closed before a byte of the answer arrived.
    Closed,
    Io(io::Error),
    Http(&'static str),
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Fail {
        match e.kind() {
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted => Fail::Closed,
            _ => Fail::Io(e),
        }
    }
}

const MAX_HEAD: usize = 64 * 1024;

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    /// Unread bytes are `buf[pos..end]`.
    pos: usize,
    end: usize,
    /// When the answer being read must be in.
    deadline: Instant,
    /// Whether a read timeout has been set on this connection before.
    timed: bool,
}

impl Conn {
    fn new(stream: TcpStream) -> Conn {
        Conn {
            stream,
            buf: vec![0; 16 * 1024],
            pos: 0,
            end: 0,
            deadline: Instant::now(),
            timed: false,
        }
    }

    /// The time left before the deadline, as a socket timeout.
    fn left(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the deadline passed",
            ));
        }
        Ok(left)
    }

    /// Reads more; `Ok(0)` at the end of the stream.
    fn fill(&mut self) -> io::Result<usize> {
        if self.pos == self.end {
            self.pos = 0;
            self.end = 0;
        } else if self.end == self.buf.len() {
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.end, 0);
                self.end -= self.pos;
                self.pos = 0;
            } else {
                self.buf.resize(self.buf.len() * 2, 0);
            }
        }
        loop {
            // Each read gets what is left of the whole answer's time, so a
            // server that trickles bytes cannot stretch it.
            let left = self.left()?;
            if let Err(e) = self.stream.set_read_timeout(Some(left)) {
                // macOS refuses a timeout on a socket the peer has closed
                // (EINVAL). One was set before on this connection, so the
                // read below is still bounded, and it returns at once: the
                // end of the stream or the reset.
                if !(self.timed && e.raw_os_error().is_some()) {
                    return Err(e);
                }
            }
            self.timed = true;
            match self.stream.read(&mut self.buf[self.end..]) {
                Ok(n) => {
                    self.end += n;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends the request and reads one answer (skipping 1xx ones): its status
    /// and whether the connection stays open.
    fn exchange(&mut self, req: &Request, deadline: Instant) -> Result<(u16, bool), Fail> {
        self.pos = 0;
        self.end = 0;
        self.deadline = deadline;
        if let Err(e) = self.stream.set_write_timeout(Some(self.left()?)) {
            // On macOS, a connection the peer has closed refuses the timeout
            // (EINVAL): closed, like a write would find it.
            return Err(if self.timed && e.raw_os_error().is_some() {
                Fail::Closed
            } else {
                Fail::Io(e)
            });
        }
        self.stream.write_all(&req.bytes)?;
        let mut first = true;
        loop {
            let (status, head_len, framing, keep) = self.head(first)?;
            first = false;
            self.pos += head_len;
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            let framing = if req.head || status == 204 || status == 304 {
                Framing::Length(0)
            } else {
                framing
            };
            match framing {
                Framing::Length(n) => self.skip(n)?,
                Framing::Chunked => self.chunked()?,
                Framing::Close => {
                    while self.fill()? > 0 {
                        self.pos = self.end;
                    }
                    return Ok((status, false));
                }
            }
            return Ok((status, keep));
        }
    }

    /// Reads until the end of a response head; returns its status, length,
    /// body framing and keep-alive.
    fn head(&mut self, first: bool) -> Result<(u16, usize, Framing, bool), Fail> {
        loop {
            if let Some(len) = find(&self.buf[self.pos..self.end], b"\r\n\r\n") {
                let head = &self.buf[self.pos..self.pos + len + 4];
                let (status, framing, keep) = parse_head(head).map_err(Fail::Http)?;
                return Ok((status, len + 4, framing, keep));
            }
            if self.end - self.pos > MAX_HEAD {
                return Err(Fail::Http("a response head over 64 KiB"));
            }
            if self.fill()? == 0 {
                return Err(if first && self.end == self.pos {
                    Fail::Closed
                } else {
                    Fail::Http("the connection closed inside a response head")
                });
            }
        }
    }

    /// Consumes `n` body bytes.
    fn skip(&mut self, mut n: usize) -> Result<(), Fail> {
        loop {
            let have = (self.end - self.pos).min(n);
            self.pos += have;
            n -= have;
            if n == 0 {
                return Ok(());
            }
            if self.fill()? == 0 {
                return Err(Fail::Http("the connection closed inside a body"));
            }
        }
    }

    /// Consumes a line ending in CRLF and returns it without the CRLF.
    fn line(&mut self) -> Result<Vec<u8>, Fail> {
        loop {
            if let Some(len) = find(&self.buf[self.pos..self.end], b"\r\n") {
                let l = self.buf[self.pos..self.pos + len].to_vec();
                self.pos += len + 2;
                return Ok(l);
            }
            if self.end - self.pos > MAX_HEAD {
                return Err(Fail::Http("a chunk line over 64 KiB"));
            }
            if self.fill()? == 0 {
                return Err(Fail::Http("the connection closed inside a chunked body"));
            }
        }
    }

    fn chunked(&mut self) -> Result<(), Fail> {
        loop {
            let line = self.line()?;
            let size = std::str::from_utf8(&line)
                .ok()
                .map(|l| l.split(';').next().unwrap_or("").trim())
                // Hex digits only: from_str_radix alone also takes "+5".
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|h| usize::from_str_radix(h, 16).ok())
                .ok_or(Fail::Http("a chunk size that is not hex"))?;
            if size == 0 {
                // Trailers, up to the empty line.
                while !self.line()?.is_empty() {}
                return Ok(());
            }
            self.skip(size)?;
            if !self.line()?.is_empty() {
                return Err(Fail::Http("a chunk longer than its size"));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Length(usize),
    Chunked,
    /// The body ends when the connection closes.
    Close,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Status, body framing and keep-alive of a response head (CRLF CRLF
/// included).
fn parse_head(head: &[u8]) -> Result<(u16, Framing, bool), &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "a response head that is not UTF-8")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err("no HTTP/1.x status line");
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..1000).contains(s))
        .ok_or("no status code")?;
    let mut keep = version != "HTTP/1.0";
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        let Some((name, value)) = l.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| "a Content-Length that is not a number")?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "chunked");
        } else if name.eq_ignore_ascii_case("connection") {
            let v = value.to_ascii_lowercase();
            if v.split(',').any(|t| t.trim() == "close") {
                keep = false;
            } else if v.split(',').any(|t| t.trim() == "keep-alive") {
                keep = true;
            }
        }
    }
    let framing = if chunked {
        Framing::Chunked
    } else if let Some(n) = length {
        Framing::Length(n)
    } else {
        keep = false;
        Framing::Close
    };
    Ok((status, framing, keep))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn heads_parse() {
        let h = |s: &str| parse_head(s.as_bytes());
        assert_eq!(
            h("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n"),
            Ok((200, Framing::Length(5), true))
        );
        assert_eq!(
            h("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"),
            Ok((200, Framing::Chunked, true))
        );
        assert_eq!(
            h("HTTP/1.1 503 x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            Ok((503, Framing::Length(0), false))
        );
        assert_eq!(
            h("HTTP/1.0 200 OK\r\nContent-Length: 1\r\n\r\n"),
            Ok((200, Framing::Length(1), false))
        );
        assert_eq!(
            h("HTTP/1.0 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive\r\n\r\n"),
            Ok((200, Framing::Length(1), true))
        );
        assert_eq!(
            h("HTTP/1.1 200 OK\r\n\r\n"),
            Ok((200, Framing::Close, false))
        );
        assert!(h("SSH-2.0-x\r\n\r\n").is_err());
        assert!(h("HTTP/1.1 abc\r\n\r\n").is_err());
        assert!(h("HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n").is_err());
    }

    #[test]
    fn requests_carry_host_headers_and_length() {
        let r = Request::new("POST", "/u", "h:1", &["X-A: 1".into()], b"{}");
        assert_eq!(
            String::from_utf8(r.bytes).unwrap(),
            "POST /u HTTP/1.1\r\nHost: h:1\r\nUser-Agent: giong-bench\r\nX-A: 1\r\nContent-Length: 2\r\n\r\n{}"
        );
        let r = Request::new("GET", "/", "h", &[], b"");
        assert!(!String::from_utf8(r.bytes)
            .unwrap()
            .contains("Content-Length"));
        assert!(Request::new("HEAD", "/", "h", &[], b"").head);
    }

    /// A server that answers each request on a connection with the next of
    /// `answers` (cycling), closing after `close_after` answers if set.
    fn serve(answers: Vec<&'static [u8]>, close_after: Option<usize>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let answers = answers.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let mut n = 0;
                    loop {
                        let mut got = Vec::new();
                        while find(&got, b"\r\n\r\n").is_none() {
                            match s.read(&mut buf) {
                                Ok(0) | Err(_) => return,
                                Ok(k) => got.extend_from_slice(&buf[..k]),
                            }
                        }
                        if s.write_all(answers[n % answers.len()]).is_err() {
                            return;
                        }
                        n += 1;
                        if close_after == Some(n) {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    fn plan(addr: SocketAddr, rate: Option<f64>) -> Plan {
        Plan {
            addr,
            requests: vec![Request::new("GET", "/", "x", &[], b"")],
            conns: 4,
            rate,
            warmup: Duration::from_millis(50),
            duration: Duration::from_millis(300),
            timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn closed_loop_reads_every_framing() {
        let addr = serve(
            vec![
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
                b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3;x=y\r\nabc\r\n0\r\nT: 1\r\n\r\n",
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            ],
            None,
        );
        let o = run(&plan(addr, None));
        assert_eq!(o.failed, 0, "{:?}", o.first_error);
        assert!(o.answered() > 30, "{o:?}");
        assert_eq!(
            o.statuses.keys().copied().collect::<Vec<_>>(),
            [200, 201, 404]
        );
        assert_eq!(o.error_statuses(), o.statuses[&404]);
        assert_eq!(o.connects, 4, "keep-alive reuses connections");
    }

    #[test]
    fn a_chunk_size_with_a_sign_is_not_hex() {
        // u64::from_str_radix alone takes "+3".
        let addr = serve(
            vec![b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+3\r\nabc\r\n0\r\n\r\n"],
            None,
        );
        let mut p = plan(addr, Some(50.0));
        p.duration = Duration::from_millis(100);
        let o = run(&p);
        assert_eq!(o.answered(), 0, "{o:?}");
        let e = o.first_error.unwrap();
        assert!(e.contains("not hex"), "{e}");
    }

    // Temporary (check-loadgen workflow): the old test's flood.
    #[test]
    #[ignore]
    fn tmp_flood() {
        let addr = serve(
            vec![b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nbody until close"],
            Some(1),
        );
        let o = run(&plan(addr, None));
        eprintln!("RESULT flood connects={} failed={} err={:?}", o.connects, o.failed, o.first_error);
    }

    #[test]
    fn closed_connections_reconnect_and_close_framing_works() {
        let addr = serve(
            vec![b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nbody until close"],
            Some(1),
        );
        let o = run(&plan(addr, Some(400.0)));
        eprintln!("RESULT paced connects={} failed={} err={:?}", o.connects, o.failed, o.first_error);
        assert_eq!(o.failed, 0, "{:?}", o.first_error);
        assert!(o.connects > 4 && o.connects >= o.answered(), "{o:?}");
        // A keep-alive answer the server then drops: retried once, not failed.
        let addr = serve(
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"],
            Some(2),
        );
        let o = run(&plan(addr, None));
        assert_eq!(o.failed, 0, "{:?}", o.first_error);
    }

    #[test]
    fn fixed_rate_sends_on_schedule() {
        let addr = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"], None);
        let mut p = plan(addr, Some(400.0));
        p.duration = Duration::from_millis(500);
        let o = run(&p);
        assert_eq!(o.failed, 0, "{:?}", o.first_error);
        // 400/s for 0.5 s: 200 requests due in the window.
        assert!((195..=205).contains(&o.answered()), "{}", o.answered());
        assert!(o.lag.count() > 0 && o.lag.count() <= o.answered());
    }

    #[test]
    fn sleep_until_wakes_at_the_due_time() {
        for ms in [0u64, 1, 15, 80] {
            let due = Instant::now() + Duration::from_millis(ms);
            sleep_until(due);
            assert!(Instant::now() >= due, "woke early for {ms} ms");
        }
    }

    #[test]
    fn nothing_listening_fails_every_request() {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let o = run(&plan(addr, Some(100.0)));
        assert_eq!(o.answered(), 0);
        assert!(o.failed > 0);
        // Linux refuses at once; Windows retries a refused connection for
        // about two seconds, so the request's time runs out first there.
        let e = o.first_error.unwrap();
        assert!(
            e.contains("cannot connect") || e.contains("no answer within"),
            "{e}"
        );
    }

    #[test]
    fn a_silent_server_ends_the_run_after_one_timeout() {
        // Accepts and never answers: the run ends one timeout after its end,
        // and every request that came due counts as failed.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let keep = held.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                keep.lock().unwrap().push(s);
            }
        });
        let mut p = plan(addr, Some(50.0));
        p.conns = 2;
        p.warmup = Duration::ZERO;
        p.duration = Duration::from_secs(1);
        p.timeout = Duration::from_millis(500);
        let start = Instant::now();
        let o = run(&p);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(o.answered(), 0);
        assert_eq!(o.failed, 50, "every request that was due counts");
        // A server that trickles one byte at a time is cut off the same way.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
                    while s.write_all(b"x").is_ok() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                });
            }
        });
        let mut p = plan(addr, None);
        p.conns = 1;
        p.warmup = Duration::ZERO;
        p.duration = Duration::from_millis(300);
        p.timeout = Duration::from_millis(400);
        let start = Instant::now();
        let o = run(&p);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(o.answered(), 0);
        assert!(o.failed >= 1);
    }

    #[test]
    fn stalls_are_charged_from_when_requests_were_due() {
        // One connection at 100/s against a server that holds each answer
        // 50 ms: requests queue behind each other, and latency grows past
        // the 50 ms the server takes, as it would for real users.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        std::thread::sleep(Duration::from_millis(50));
                        if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        let mut p = plan(addr, Some(100.0));
        p.conns = 1;
        p.warmup = Duration::ZERO;
        let o = run(&p);
        assert!(o.latency.max() > 150_000_000, "max {} ns", o.latency.max());
    }
}
