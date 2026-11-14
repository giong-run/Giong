//! Where log lines go: standard output or error, written by one background
//! thread so that a worker never waits for the terminal or the pipe.
//!
//! Each worker appends lines to its own buffer. A writer thread, started by
//! the first line, wakes when a buffer goes from empty to non-empty, waits
//! [`BATCH`] for more lines, then writes everything with one `write` per
//! stream. A busy service therefore makes about a hundred `write` calls a
//! second for its whole log instead of one per line, and a slow reader (a
//! full pipe, a stalled log shipper) delays lines instead of requests: past
//! [`MAX_PENDING`] bytes waiting on one worker, new lines are dropped and
//! counted, and a `log_dropped` line says how many. Once written, a buffer
//! shrinks back to [`RETAIN`] bytes, and a thread's buffer is removed when
//! the thread ends.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::io::Write as _;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Rendered;
use crate::json::JsonWriter;
use crate::req::RouteInfo;
use crate::Error;

/// How long the writer waits for more lines before writing a batch.
const BATCH: Duration = Duration::from_millis(10);
/// Bytes waiting on one worker, per stream, from which the writer starts at
/// once instead of waiting for the batch.
const SOON: usize = 64 * 1024;
/// Bytes waiting on one worker, per stream, past which new lines are
/// dropped.
pub(crate) const MAX_PENDING: usize = 1024 * 1024;
/// How long [`flush`] waits for the writer before giving up.
const FLUSH_WAIT: Duration = Duration::from_secs(2);
/// Capacity a buffer keeps once drained; past it, a burst's memory is given
/// back unless the round used a good part of it.
const RETAIN: usize = SOON;
/// Longest path or query string written in a line, in bytes: a request head
/// may be 64 KiB, and sixteen such lines would fill [`MAX_PENDING`].
pub(crate) const MAX_FIELD: usize = 4096;

/// Where a [`Logger`](crate::middleware::Logger) writes its JSON lines.
///
/// More sinks may come (a file, any writer), so a `match` on a `Sink` needs
/// a `_` arm:
///
/// ```compile_fail,E0004
/// fn name(sink: &giong_web::Sink) -> &'static str {
///     match sink {
///         giong_web::Sink::Stdout => "stdout",
///         giong_web::Sink::Stderr => "stderr",
///         giong_web::Sink::Custom(_) => "custom",
///     }
/// }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub enum Sink {
    /// Standard output, through the background writer.
    Stdout,
    /// Standard error, through the background writer.
    Stderr,
    /// A function called on the worker with each line (one JSON object,
    /// without the newline). It runs inside the request, so it must not
    /// block: hand the line on, do not write it to a file.
    Custom(Rc<dyn Fn(&str)>),
}

impl fmt::Debug for Sink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sink::Stdout => f.write_str("Stdout"),
            Sink::Stderr => f.write_str("Stderr"),
            Sink::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

impl Sink {
    /// Writes one line: `line` ends with `\n`.
    pub(crate) fn write(&self, line: &str) {
        match self {
            Sink::Stdout => append(Stream::Out, line.as_bytes()),
            Sink::Stderr => append(Stream::Err, line.as_bytes()),
            Sink::Custom(f) => f(line.strip_suffix('\n').unwrap_or(line)),
        }
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Lines waiting on one worker.
#[derive(Default)]
struct Pending {
    out: Vec<u8>,
    err: Vec<u8>,
    dropped: u64,
    /// The thread that wrote here has ended: once drained, the slot goes.
    closed: bool,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.out.is_empty() && self.err.is_empty() && self.dropped == 0
    }
}

struct Slot {
    pending: Mutex<Pending>,
    worker: Option<usize>,
}

#[derive(Default)]
struct State {
    slots: Vec<Arc<Slot>>,
    /// A buffer went from empty to non-empty since the writer last looked.
    signaled: bool,
    /// Write at once, without waiting for the batch.
    urgent: bool,
    flush_asked: u64,
    flush_done: u64,
}

struct Writer {
    state: Mutex<State>,
    /// The writer thread waits on this.
    work: Condvar,
    /// `flush` waits on this.
    done: Condvar,
}

static WRITER: OnceLock<Writer> = OnceLock::new();

/// This thread's slot. When the thread ends, the slot is marked closed and
/// the writer removes it once its last lines are written, so threads that
/// come and go leave nothing behind.
struct Registered(Arc<Slot>);

impl Drop for Registered {
    fn drop(&mut self) {
        lock(&self.0.pending).closed = true;
        if let Some(w) = WRITER.get() {
            lock(&w.state).signaled = true;
            w.work.notify_one();
        }
    }
}

thread_local! {
    static SLOT: RefCell<Option<Registered>> = const { RefCell::new(None) };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks leaves plain buffers behind,
    // which are still fine to use.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer() -> &'static Writer {
    WRITER.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("giong-web-log".into())
            .spawn(write_loop);
        if spawned.is_err() {
            // Without a thread the lines stay buffered (and are dropped at
            // the limit); a service is better off running without its log
            // than not running.
        }
        Writer {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            done: Condvar::new(),
        }
    })
}

fn append(stream: Stream, line: &[u8]) {
    let w = writer();
    let slot = SLOT.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        let Registered(slot) = slot.get_or_insert_with(|| {
            let slot = Arc::new(Slot {
                pending: Mutex::new(Pending::default()),
                worker: worker(),
            });
            lock(&w.state).slots.push(slot.clone());
            Registered(slot)
        });
        slot.clone()
    });
    let Ok(slot) = slot else {
        // Logged from another thread-local's destructor, after this
        // thread's slot is gone: written here, as the thread ends.
        let _ = match stream {
            Stream::Out => std::io::stdout().lock().write_all(line),
            Stream::Err => std::io::stderr().lock().write_all(line),
        };
        return;
    };
    let mut p = lock(&slot.pending);
    let was_empty = p.is_empty();
    let buf = match stream {
        Stream::Out => &mut p.out,
        Stream::Err => &mut p.err,
    };
    if buf.len() + line.len() > MAX_PENDING {
        p.dropped += 1;
        return;
    }
    buf.extend_from_slice(line);
    let big = buf.len() >= SOON;
    drop(p);
    if was_empty || big {
        let mut st = lock(&w.state);
        st.signaled = true;
        st.urgent |= big;
        w.work.notify_one();
    }
}

/// The worker this thread runs, if it is one of a giong runtime's.
pub(crate) fn worker() -> Option<usize> {
    giong::try_worker_id()
}

fn write_loop() {
    // `writer()` is still initialising the cell when this thread starts.
    let w = loop {
        match WRITER.get() {
            Some(w) => break w,
            None => std::thread::yield_now(),
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        let (asked, slots) = {
            let mut st = lock(&w.state);
            while !st.signaled && st.flush_asked == st.flush_done {
                st = w.work.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            let urgent = st.urgent || st.flush_asked != st.flush_done;
            st.signaled = false;
            st.urgent = false;
            let asked = st.flush_asked;
            drop(st);
            if !urgent {
                std::thread::sleep(BATCH);
            }
            (asked, lock(&w.state).slots.clone())
        };
        let mut closed = false;
        for slot in &slots {
            let mut p = lock(&slot.pending);
            take_lines(&mut p.out, &mut out);
            take_lines(&mut p.err, &mut err);
            closed |= p.closed;
            let dropped = std::mem::take(&mut p.dropped);
            drop(p);
            if dropped > 0 {
                let mut line = JsonWriter::object();
                line.str("ts", &timestamp(SystemTime::now()));
                line.str("level", "warn");
                line.str("event", "log_dropped");
                line.u64("lines", dropped);
                match slot.worker {
                    Some(n) => line.u64("worker", n as u64),
                    None => line.null("worker"),
                }
                err.extend_from_slice(line.finish_line().as_bytes());
            }
        }
        drop(slots);
        // Errors (a closed pipe, say) are not worth more than the line.
        if !out.is_empty() {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(&out);
            let _ = stdout.flush();
            trim(&mut out);
        }
        if !err.is_empty() {
            let _ = std::io::stderr().lock().write_all(&err);
            trim(&mut err);
        }
        let mut st = lock(&w.state);
        if closed {
            // A slot whose thread ended goes once drained; one that got
            // lines after this round drained it stays for the next.
            st.slots.retain(|s| {
                let p = lock(&s.pending);
                !(p.closed && p.is_empty())
            });
        }
        st.flush_done = st.flush_done.max(asked);
        w.done.notify_all();
    }
}

/// Moves the lines in `from` to the end of `to`, leaving `from` empty.
fn take_lines(from: &mut Vec<u8>, to: &mut Vec<u8>) {
    to.extend_from_slice(from);
    trim(from);
}

/// Empties `buf` and gives back the memory a burst left in it past
/// [`RETAIN`] bytes, so that a worker holds 1 MiB per stream only while it
/// needs it.
fn trim(buf: &mut Vec<u8>) {
    buf.clear();
    if buf.capacity() > RETAIN {
        buf.shrink_to(RETAIN);
    }
}

/// Writes every line logged so far to standard output and error, waiting
/// up to 2 seconds for it.
///
/// An [`App`](crate::App) calls it when dropped, which every worker does as
/// it stops, so a service that returns from `run` loses no lines. Call it
/// before exiting some other way, such as `std::process::exit`.
pub fn flush() {
    let Some(w) = WRITER.get() else {
        return; // Nothing was ever logged.
    };
    let mut st = lock(&w.state);
    st.flush_asked += 1;
    let ticket = st.flush_asked;
    w.work.notify_one();
    let deadline = std::time::Instant::now() + FLUSH_WAIT;
    while st.flush_done < ticket {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        st = w
            .done
            .wait_timeout(st, left)
            .map(|(st, _)| st)
            .unwrap_or_else(|e| e.into_inner().0);
    }
}

/// Logs, to standard error, a `5xx` error that no logger reported, so that
/// no cause is ever lost.
pub(crate) fn unlogged_error(rendered: &Rendered, method: &str, route: Option<&RouteInfo>) {
    let error = &rendered.error;
    let mut line = JsonWriter::with_capacity(256);
    line.str("ts", &timestamp(SystemTime::now()));
    line.str("level", "error");
    line.str("event", "error");
    line.str("method", method);
    match route {
        Some(r) => line.str("route", &r.pattern),
        None => line.null("route"),
    }
    line.u64("status", u64::from(error.status()));
    if let Some(id) = &rendered.request_id {
        line.str("request_id", id.as_str());
    }
    error_fields(&mut line, error);
    if let Some(n) = worker() {
        line.u64("worker", n as u64);
    }
    Sink::Stderr.write(&line.finish_line());
}

/// `text` cut to [`MAX_FIELD`] bytes (on a character boundary), followed by
/// `...(+N bytes)` naming how much was left out.
pub(crate) fn clip(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_FIELD {
        return Cow::Borrowed(text);
    }
    let mut end = MAX_FIELD;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}...(+{} bytes)", &text[..end], text.len() - end))
}

/// The `error` object of a log line: code, detail, cause.
pub(crate) fn error_fields(line: &mut JsonWriter, error: &Error) {
    let mut e = JsonWriter::object();
    e.str("code", error.code());
    e.str("detail", error.detail());
    if let Some(cause) = error.cause_chain() {
        e.str("cause", &cause);
    }
    line.raw("error", &e.finish());
}

/// `2026-11-13T08:40:01.123456Z`: RFC 3339, UTC, microseconds.
pub(crate) fn timestamp(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        since.subsec_micros()
    )
}

/// Year, month and day of a day count since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let t = UNIX_EPOCH + Duration::from_micros(1_794_558_001_123_456);
        assert_eq!(timestamp(t), "2026-11-13T08:20:01.123456Z");
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&timestamp(leap)[..10], "2000-02-29");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn a_slot_goes_once_its_thread_has_ended() {
        let slot = std::thread::spawn(|| {
            append(Stream::Err, b""); // registers the slot, writes nothing
            SLOT.with(|s| Arc::downgrade(&s.borrow().as_ref().unwrap().0))
        })
        .join()
        .unwrap();
        flush();
        assert_eq!(slot.strong_count(), 0, "the writer still holds the slot");
    }

    #[test]
    fn a_drained_buffer_gives_back_what_a_burst_took() {
        let mut from = Vec::with_capacity(MAX_PENDING);
        from.extend_from_slice(b"{\"event\":\"request\"}\n");
        let mut to = Vec::new();
        take_lines(&mut from, &mut to);
        assert_eq!(to, b"{\"event\":\"request\"}\n");
        assert!(from.is_empty());
        assert!(from.capacity() <= RETAIN, "{}", from.capacity());

        let mut small = Vec::with_capacity(100);
        small.push(b'x');
        trim(&mut small);
        assert_eq!(small.capacity(), 100, "a small buffer is kept as it is");
    }

    #[test]
    fn long_fields_are_cut_on_a_character_boundary() {
        assert_eq!(clip("/users/7"), "/users/7");
        let exact = "a".repeat(MAX_FIELD);
        assert!(matches!(clip(&exact), Cow::Borrowed(_)));
        // `é` is two bytes; the cut falls in the middle of the one at the end.
        let text = format!("{}é{}", "a".repeat(MAX_FIELD - 1), "b".repeat(9));
        assert_eq!(
            clip(&text),
            format!("{}...(+11 bytes)", "a".repeat(MAX_FIELD - 1))
        );
    }
}
