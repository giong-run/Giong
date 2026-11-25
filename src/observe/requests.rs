//! Where the time of each request goes.
//!
//! See the [module documentation](super#why-a-request-is-slow).

use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Location, PollProfile};
use crate::runtime;

/// A poll at least this long is recorded while requests are traced, with
/// the task it belongs to, so that requests waiting on the same worker
/// meanwhile can tell what held them up.
pub const LONG_POLL: Duration = Duration::from_millis(1);

/// Slowest requests kept in full per route and worker.
const SLOWEST: usize = 8;
/// Routes kept apart per worker; the rest are counted under [`OTHER_ROUTES`].
const MAX_ROUTES: usize = 128;
/// The route the requests of routes past [`MAX_ROUTES`] are counted under.
const OTHER_ROUTES: Route = Route::from_static("(other routes)");
/// Long polls remembered per worker for requests still in flight.
const RECENT_LONG_POLLS: usize = 256;

thread_local! {
    /// The request whose handler this thread is polling, or null.
    static CURRENT: Cell<*mut Active> = const { Cell::new(ptr::null_mut()) };
    /// The worker on this thread traces requests: set by the worker at its
    /// first batch of tasks after tracing is turned on, cleared by the first
    /// request after it is turned off (see [`RequestTrace::start`]).
    static ON: Cell<bool> = const { Cell::new(false) };
    /// The request whose handler took the longest poll inside the task poll
    /// in progress, and how long, for [`Requests::long_poll`].
    static ENTERED: Cell<(u64, Duration)> = const { Cell::new((0, Duration::ZERO)) };
    /// The route of the request in `ENTERED`, kept only when its handler
    /// took a long poll.
    static ENTERED_ROUTE: RefCell<Option<Route>> = const { RefCell::new(None) };
    /// Ids of traced requests on this thread.
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Called when request tracing is turned on or off for this thread.
pub(crate) fn set_on(on: bool) {
    let _ = ON.try_with(|c| c.set(on));
}

/// Called by the worker before each timed task poll.
pub(crate) fn clear_entered() {
    let _ = ENTERED.try_with(|c| c.set((0, Duration::ZERO)));
}

/// The request whose handler took the longest poll in the task poll that
/// just ended, if any of them took [`LONG_POLL`] or more.
fn take_entered() -> (u64, Option<Route>) {
    let id = ENTERED.try_with(|c| c.get().0).unwrap_or(0);
    let route = ENTERED_ROUTE
        .try_with(|r| r.borrow_mut().take())
        .ok()
        .flatten();
    (id, route)
}

/// The name of a route, such as `GET /users/{id}`: what slow requests are
/// grouped by. See [`set_route`].
///
/// Cloning one is cheap (a reference count, or nothing for
/// [`from_static`](Self::from_static)); two routes are equal when their
/// names are.
#[derive(Clone)]
pub struct Route(Name);

#[derive(Clone)]
enum Name {
    Static(&'static str),
    Shared(Arc<str>),
}

impl Route {
    /// A route named by a string that lives for the whole program, which
    /// costs nothing to make, for example in a `static`:
    ///
    /// ```
    /// static CHECKOUT: giong::observe::Route =
    ///     giong::observe::Route::from_static("POST /checkout");
    /// ```
    pub const fn from_static(name: &'static str) -> Self {
        Self(Name::Static(name))
    }

    /// A route named by any string. Make it once (when the service starts)
    /// and clone it for each request.
    pub fn new(name: &str) -> Self {
        Self(Name::Shared(Arc::from(name)))
    }

    /// The route's name.
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Name::Static(s) => s,
            Name::Shared(s) => s,
        }
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for Route {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Route {}

impl Hash for Route {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Borrow<str> for Route {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// What a request waits for outside its own code: see [`wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Wait {
    /// A connection from a pool: the time from asking a pool for a
    /// connection to having one, opening a new one included.
    Pool,
    /// A database or cache command: the time from sending it to having its
    /// whole answer.
    Query,
    /// A call to another service, such as an HTTP request.
    Upstream,
}

impl Wait {
    fn index(self) -> usize {
        match self {
            Wait::Pool => 0,
            Wait::Query => 1,
            Wait::Upstream => 2,
        }
    }
}

/// Times one wait of the request being served, from [`wait`] until it is
/// dropped.
#[must_use = "the wait is timed until this is dropped"]
pub struct WaitTimer {
    // Boxed, so that the timer is one pointer in the futures that hold it
    // across an `.await` (a database command, a pool's `get`), and nothing
    // is allocated when no traced request is being served.
    started: Option<Box<Started>>,
}

/// A wait being timed: the request's id, what it waits for, and since when.
struct Started {
    id: u64,
    kind: Wait,
    what: &'static str,
    at: Instant,
}

impl WaitTimer {
    /// Ends the wait now; the same as dropping the timer.
    pub fn end(self) {}
}

impl Drop for WaitTimer {
    #[inline]
    fn drop(&mut self) {
        if let Some(started) = self.started.take() {
            charge(&started);
        }
    }
}

/// Adds a wait that ended to the request it belongs to.
#[cold]
fn charge(started: &Started) {
    let Started { id, kind, what, at } = *started;
    let took = at.elapsed();
    with_current(|active| {
        // Dropped while another request's handler is polled, or none (the
        // request's future dropped by its connection): the wait belongs to a
        // request that is not being served any more.
        if active.id == id {
            active.waits[kind.index()].add(took, what);
        }
    });
}

impl fmt::Debug for WaitTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitTimer")
            .field("timing", &self.started.is_some())
            .finish()
    }
}

/// Starts timing a wait of the request being served: `kind` says what the
/// request waits for, and `what` which system, such as `"postgres"`. The
/// wait ends when the returned timer is dropped, and its time is added to
/// the request's [`RequestRecord`].
///
/// When no traced request is being served (tracing is off, or the code
/// does not run inside a request handler, for example in a task the handler
/// spawned) it does nothing, and costs one read of a thread-local. The
/// database clients of giong (`giong-postgres`, `giong-mysql`,
/// `giong-redis`) and `giong-http`'s client time their waits with it; a
/// service can time its own, such as a call through another client:
///
/// ```
/// use giong::observe::{self, Wait};
///
/// async fn price(sku: &str) -> u64 {
///     let _wait = observe::wait(Wait::Upstream, "pricing");
///     // ... call the pricing service ...
///     # let _ = sku;
///     42
/// }
/// # giong::block_on(async { assert_eq!(price("x").await, 42) });
/// ```
#[inline]
pub fn wait(kind: Wait, what: &'static str) -> WaitTimer {
    let id = with_current(|active| active.id);
    WaitTimer {
        started: id.map(|id| start_wait(id, kind, what)),
    }
}

#[cold]
fn start_wait(id: u64, kind: Wait, what: &'static str) -> Box<Started> {
    Box::new(Started {
        id,
        kind,
        what,
        at: Instant::now(),
    })
}

/// Names the route of the request being served, for grouping: see
/// [`Route`]. `giong-web` names each request by its route's method and
/// pattern, such as `GET /users/{id}`; a service on `giong-http` alone can
/// name its own. Does nothing when no traced request is being served.
pub fn set_route(route: &Route) {
    with_current(|active| active.route = Some(route.clone()));
}

/// Runs `f` on the request whose handler is being polled, if there is one.
fn with_current<R>(f: impl FnOnce(&mut Active) -> R) -> Option<R> {
    let current = CURRENT.try_with(Cell::get).unwrap_or(ptr::null_mut());
    // SAFETY: `CURRENT` is non-null only inside `RequestTrace::poll`, which
    // points it at its own `Active` and borrows the trace mutably for the
    // whole poll without touching it, so nothing else refers to it. `f`
    // cannot reach this function again: it only updates fields.
    unsafe { current.as_mut() }.map(f)
}

/// Totals of one kind of [`Wait`] in a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitStats {
    /// Their total duration.
    pub total: Duration,
    /// How many there were.
    pub count: u32,
    /// The longest one.
    pub longest: Duration,
    /// What the longest one waited for (the `what` given to [`wait`]).
    pub what: Option<&'static str>,
}

impl WaitStats {
    fn add(&mut self, took: Duration, what: &'static str) {
        self.total += took;
        self.count = self.count.saturating_add(1);
        if took >= self.longest {
            self.longest = took;
            self.what = Some(what);
        }
    }
}

/// What a traced request gathers while its handler runs.
struct Active {
    id: u64,
    route: Option<Route>,
    poll: Duration,
    polls: u32,
    longest_poll: Duration,
    waits: [WaitStats; 3],
}

/// One request being traced, kept by the protocol code that serves it
/// (`giong-http` for HTTP/1.1 and HTTP/2): it says when the request
/// arrived, when its handler started and answered, and when the answer was
/// written, and polls the handler through [`poll`](Self::poll), so that
/// [`wait`] and [`set_route`] find the request.
///
/// Dropped without [`finish`](Self::finish), the request is counted as
/// abandoned: the connection closed or failed before the answer was
/// written.
pub struct RequestTrace {
    protocol: &'static str,
    worker: usize,
    received: Instant,
    started: Option<Instant>,
    /// When the handler's latest poll ended.
    polled: Option<Instant>,
    responded: Option<Instant>,
    status: u16,
    finished: bool,
    active: Active,
}

impl RequestTrace {
    /// `true` when the current worker traces requests: what
    /// [`start`](Self::start) checks first, for protocol code that keeps
    /// the cost of tracing off the path of every request.
    #[inline]
    pub fn is_on() -> bool {
        ON.try_with(Cell::get).unwrap_or(false)
    }

    /// A trace for a request that has just begun to arrive (its first bytes
    /// are in), on `protocol` (such as `"http/1.1"`), or `None` when the
    /// current worker does not trace requests: then it costs one read of a
    /// thread-local.
    #[inline]
    pub fn start(protocol: &'static str) -> Option<Self> {
        if !Self::is_on() {
            return None;
        }
        Self::begin(protocol)
    }

    #[cold]
    fn begin(protocol: &'static str) -> Option<Self> {
        // `ON` is turned on by the worker when tracing starts, and off here
        // once it has stopped, so that a worker with tracing off pays nothing
        // per batch of tasks to keep it current.
        let worker = runtime::try_with_worker(|w| {
            let on = w.rt.measuring() & runtime::TRACING != 0;
            if !on {
                w.tracing.set(false);
                set_on(false);
            }
            on.then_some(w.index)
        })??;
        let id = NEXT_ID
            .try_with(|n| {
                let id = n.get();
                n.set(id + 1);
                id
            })
            .ok()?;
        Some(Self {
            protocol,
            worker,
            received: Instant::now(),
            started: None,
            polled: None,
            responded: None,
            status: 0,
            finished: false,
            active: Active {
                id,
                route: None,
                poll: Duration::ZERO,
                polls: 0,
                longest_poll: Duration::ZERO,
                waits: [WaitStats::default(); 3],
            },
        })
    }

    /// The request has arrived whole (or as far as its handler needs it)
    /// and its handler is about to be called.
    pub fn started(&mut self) {
        self.started = Some(Instant::now());
    }

    /// Names the request's route; see [`set_route`].
    pub fn set_route(&mut self, route: &Route) {
        self.active.route = Some(route.clone());
    }

    /// Runs `f`, one poll of the request's handler, timed and with this
    /// request as the one being served.
    pub fn poll<R>(&mut self, f: impl FnOnce() -> R) -> R {
        struct Restore(*mut Active);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = CURRENT.try_with(|c| c.set(self.0));
            }
        }
        let me: *mut Active = &mut self.active;
        let previous = CURRENT
            .try_with(|c| c.replace(me))
            .unwrap_or(ptr::null_mut());
        let restore = Restore(previous);
        let start = Instant::now();
        let out = f();
        let end = Instant::now();
        drop(restore);
        let took = end - start;
        self.polled = Some(end);
        let active = &mut self.active;
        active.poll += took;
        active.polls = active.polls.saturating_add(1);
        active.longest_poll = active.longest_poll.max(took);
        let id = active.id;
        let _ = ENTERED.try_with(|c| {
            if took > c.get().1 {
                c.set((id, took));
                if took >= LONG_POLL {
                    let route = active.route.clone();
                    let _ = ENTERED_ROUTE.try_with(|r| *r.borrow_mut() = route);
                }
            }
        });
        out
    }

    /// The handler has answered with `status` (for HTTP, the status code).
    /// Called right after the poll that produced the answer, whose end is
    /// taken as the time of the answer (one clock read less per request);
    /// a handler never polled through [`poll`](Self::poll) answers now.
    pub fn responded(&mut self, status: u16) {
        self.responded = Some(self.polled.unwrap_or_else(Instant::now));
        self.status = status;
    }

    /// The answer has been written: the request is done, and is counted on
    /// the current worker (see [`take_requests`]).
    pub fn finish(mut self) {
        self.finished = true;
        self.record(Instant::now(), true);
    }

    fn record(&mut self, now: Instant, complete: bool) {
        let worker = self.worker;
        let _ = runtime::try_with_worker(|w| {
            if w.index == worker {
                if let Ok(mut requests) = w.observe.requests.try_borrow_mut() {
                    requests.add(self, now, complete);
                }
            }
        });
    }
}

impl Drop for RequestTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Instant::now(), false);
        }
    }
}

impl fmt::Debug for RequestTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestTrace")
            .field("protocol", &self.protocol)
            .field("route", &self.active.route)
            .finish()
    }
}

/// Phases of a request, in the order of [`Bucket::sums`].
const TOTAL: usize = 0;
const READ: usize = 1;
const POLL: usize = 2;
const POOL: usize = 3;
const QUERY: usize = 4;
const UPSTREAM: usize = 5;
const OTHER: usize = 6;
const WRITE: usize = 7;
const BLOCKED: usize = 8;
const PHASES: usize = 9;

/// Latency buckets: below 1,024 ns, then four per power of two up to about
/// 2^37 ns (137 s), so a bucket is at most a quarter wider than its lower
/// edge.
const OCTAVES: usize = 27;
const BUCKETS: usize = 1 + OCTAVES * 4;

fn bucket_of(d: Duration) -> usize {
    let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    if ns < 1024 {
        return 0;
    }
    let e = 63 - ns.leading_zeros() as usize; // 10..=63
    let octave = e - 10;
    if octave >= OCTAVES {
        return BUCKETS - 1;
    }
    let sub = ((ns >> (e - 2)) & 3) as usize;
    1 + octave * 4 + sub
}

/// The upper edge of bucket `i`.
fn bucket_top(i: usize) -> Duration {
    if i == 0 {
        return Duration::from_nanos(1024);
    }
    let octave = (i - 1) / 4;
    let sub = ((i - 1) % 4) as u64;
    let e = octave + 10;
    Duration::from_nanos((5 + sub) << (e - 2))
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    count: u64,
    /// Nanoseconds per phase, summed over the bucket's requests.
    sums: [u64; PHASES],
}

/// Requests of one route on one worker, or merged over workers with
/// [`merge`](Self::merge): how many, how long they took, and where their
/// time went, by latency.
#[derive(Clone)]
#[non_exhaustive]
pub struct RouteStats {
    /// The route, as named with [`set_route`]; `None` for requests no code
    /// named.
    pub route: Option<Route>,
    /// Requests answered, the answer written.
    pub requests: u64,
    /// Requests whose connection closed or failed before their answer was
    /// written (a client that went away, a timeout, a refused request).
    pub abandoned: u64,
    /// The longest request.
    pub max: Duration,
    /// The slowest requests, slowest first, at most 8 per worker.
    pub slowest: Vec<RequestRecord>,
    buckets: Box<[Bucket]>,
}

impl RouteStats {
    fn new(route: Option<Route>) -> Self {
        Self {
            route,
            requests: 0,
            abandoned: 0,
            max: Duration::ZERO,
            slowest: Vec::new(),
            buckets: vec![Bucket::default(); BUCKETS].into_boxed_slice(),
        }
    }

    fn add(&mut self, r: RequestRecord) {
        self.requests += 1;
        self.max = self.max.max(r.total);
        let b = &mut self.buckets[bucket_of(r.total)];
        b.count += 1;
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let phases = [
            r.total,
            r.read,
            r.poll,
            r.pool.total,
            r.query.total,
            r.upstream.total,
            r.other,
            r.write,
            r.blocked,
        ];
        for (sum, d) in b.sums.iter_mut().zip(phases) {
            *sum = sum.saturating_add(ns(d));
        }
        if self.slowest.len() < SLOWEST || self.slowest.last().is_some_and(|s| r.total > s.total) {
            let at = self.slowest.partition_point(|s| s.total >= r.total);
            self.slowest.insert(at, r);
            self.slowest.truncate(SLOWEST);
        }
    }

    /// Adds the requests of `other` (the same route on another worker).
    /// The slowest requests of both are kept, at most 8 per worker merged.
    pub fn merge(&mut self, other: &RouteStats) {
        self.requests += other.requests;
        self.abandoned += other.abandoned;
        self.max = self.max.max(other.max);
        for (a, b) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            a.count += b.count;
            for (x, y) in a.sums.iter_mut().zip(b.sums) {
                *x = x.saturating_add(y);
            }
        }
        self.slowest.extend(other.slowest.iter().cloned());
        self.slowest.sort_by_key(|s| std::cmp::Reverse(s.total));
    }

    /// The bucket that holds quantile `q`.
    fn bucket_at(&self, q: f64) -> usize {
        let q = if q.is_nan() { 0.0 } else { q.clamp(0.0, 1.0) };
        let rank = ((q * self.requests as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, b) in self.buckets.iter().enumerate() {
            seen += b.count;
            if seen >= rank {
                return i;
            }
        }
        BUCKETS - 1
    }

    /// The latency below which a share `q` of the requests finished (`0.99`
    /// for p99), to within a quarter: the upper edge of the bucket that
    /// holds it, never more than [`max`](Self::max). Zero without requests.
    pub fn quantile(&self, q: f64) -> Duration {
        if self.requests == 0 {
            return Duration::ZERO;
        }
        bucket_top(self.bucket_at(q)).min(self.max)
    }

    /// Where the time went for the requests between quantiles `from` and
    /// `to` (`breakdown(0.99, 1.0)` for the slowest percent,
    /// `breakdown(0.0, 0.5)` for the faster half), to within a bucket: all
    /// of the requests in the buckets that hold the two quantiles count.
    pub fn breakdown(&self, from: f64, to: f64) -> Breakdown {
        let mut out = Breakdown::default();
        if self.requests == 0 {
            return out;
        }
        let (lo, hi) = (self.bucket_at(from), self.bucket_at(to));
        let mut sums = [0u64; PHASES];
        for b in &self.buckets[lo.min(hi)..=hi.max(lo)] {
            out.requests += b.count;
            for (x, y) in sums.iter_mut().zip(b.sums) {
                *x = x.saturating_add(y);
            }
        }
        let d = Duration::from_nanos;
        out.total = d(sums[TOTAL]);
        out.read = d(sums[READ]);
        out.poll = d(sums[POLL]);
        out.pool = d(sums[POOL]);
        out.query = d(sums[QUERY]);
        out.upstream = d(sums[UPSTREAM]);
        out.other = d(sums[OTHER]);
        out.write = d(sums[WRITE]);
        out.blocked = d(sums[BLOCKED]);
        out
    }
}

impl fmt::Debug for RouteStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteStats")
            .field("route", &self.route)
            .field("requests", &self.requests)
            .field("abandoned", &self.abandoned)
            .field("p50", &self.quantile(0.5))
            .field("p99", &self.quantile(0.99))
            .field("max", &self.max)
            .finish()
    }
}

/// Where the time of a group of requests went, summed over them: see
/// [`RouteStats::breakdown`]. Divide by [`requests`](Self::requests) for
/// the mean.
///
/// `read`, `poll`, `pool`, `query`, `upstream`, `other` and `write` add up
/// to `total`, to within the few microseconds a wait shares with the polls
/// it starts and ends in (and more where a handler waits for several things
/// at once: each wait counts in full, and `other` is then zero). `blocked`
/// does not: it is the part of the requests' time
/// during which their worker was held by another task's poll of
/// [`LONG_POLL`] or more, whichever phase it fell in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Breakdown {
    /// How many requests.
    pub requests: u64,
    /// From the first bytes of the request to the last of the answer
    /// written.
    pub total: Duration,
    /// Receiving the request until its handler was called: a slow client,
    /// a large upload, or a busy worker.
    pub read: Duration,
    /// Inside the handler's polls: its own code running on the worker.
    pub poll: Duration,
    /// Waiting for connections from pools ([`Wait::Pool`]).
    pub pool: Duration,
    /// Waiting for database and cache commands ([`Wait::Query`]).
    pub query: Duration,
    /// Waiting for other services ([`Wait::Upstream`]).
    pub upstream: Duration,
    /// The rest of the handler's time: waits no code named (timers, locks,
    /// channels, tasks it spawned, clients that do not time their waits),
    /// and time spent runnable while the worker ran other tasks.
    pub other: Duration,
    /// From the handler's answer to its last byte written: a slow reader,
    /// a large answer, or answers queued behind earlier ones.
    pub write: Duration,
    /// Time the worker spent in other tasks' long polls while the request
    /// was in flight.
    pub blocked: Duration,
}

/// One request, in full: see [`RouteStats::slowest`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestRecord {
    /// The worker that served it.
    pub worker: usize,
    /// The protocol, such as `"http/1.1"` or `"h2"`.
    pub protocol: &'static str,
    /// Its route; see [`set_route`].
    pub route: Option<Route>,
    /// The status it was answered with (for HTTP, the status code).
    pub status: u16,
    /// How long ago it finished, as of [`take_requests`].
    pub ago: Duration,
    /// From its first bytes to the last of its answer written.
    pub total: Duration,
    /// Receiving it until its handler was called.
    pub read: Duration,
    /// Inside its handler's polls.
    pub poll: Duration,
    /// How many times its handler was polled.
    pub polls: u32,
    /// Its handler's longest poll.
    pub longest_poll: Duration,
    /// Its waits for pool connections.
    pub pool: WaitStats,
    /// Its waits for database and cache commands.
    pub query: WaitStats,
    /// Its waits for other services.
    pub upstream: WaitStats,
    /// The rest of its handler's time (see [`Breakdown::other`]).
    pub other: Duration,
    /// From its handler's answer to the last byte written.
    pub write: Duration,
    /// Time its worker spent in other tasks' long polls while it was in
    /// flight.
    pub blocked: Duration,
    /// The task whose long polls took most of `blocked`.
    pub blocked_by: Option<BlockedBy>,
    /// When it finished, for [`take_requests`] to work out `ago`.
    finished_at: Instant,
}

/// The task that held a worker while a request waited: see
/// [`RequestRecord::blocked_by`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockedBy {
    /// Where the task was spawned.
    pub location: Location,
    /// The route of the request whose handler took the poll, when the
    /// task's long poll was spent in a traced request's handler.
    pub route: Option<Route>,
    /// How much of the request's time it held the worker.
    pub held: Duration,
}

/// What [`take_requests`] returns for one worker.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RequestReport {
    /// The worker.
    pub worker: usize,
    /// Requests by route, most first.
    pub routes: Vec<RouteStats>,
    /// Task polls of [`LONG_POLL`] or more, by where the task was spawned,
    /// longest total first.
    pub long_polls: Vec<PollProfile>,
}

/// One long poll, kept for requests in flight.
struct LongPollSeen {
    start: Instant,
    end: Instant,
    location: Location,
    request: u64,
    route: Option<Route>,
}

/// The worker-local part of request tracing.
#[derive(Default)]
pub(crate) struct Requests {
    routes: HashMap<Option<Route>, RouteStats>,
    recent: VecDeque<LongPollSeen>,
    long_polls: HashMap<Location, PollProfile>,
}

impl Requests {
    fn add(&mut self, t: &mut RequestTrace, now: Instant, complete: bool) {
        let route = t.active.route.take();
        let key = if self.routes.len() >= MAX_ROUTES && !self.routes.contains_key(&route) {
            Some(OTHER_ROUTES)
        } else {
            route
        };
        if !complete {
            self.routes
                .entry(key.clone())
                .or_insert_with(|| RouteStats::new(key))
                .abandoned += 1;
            return;
        }
        let a = &t.active;
        let started = t.started.unwrap_or(t.received).max(t.received);
        let responded = t.responded.unwrap_or(now).clamp(started, now);
        let handler = responded - started;
        let waits = a.waits[0].total + a.waits[1].total + a.waits[2].total;
        let (blocked, blocked_by) = self.blocked(a.id, t.received, now);
        let record = RequestRecord {
            worker: t.worker,
            protocol: t.protocol,
            route: key.clone(),
            status: t.status,
            ago: Duration::ZERO,
            total: now - t.received,
            read: started - t.received,
            poll: a.poll,
            polls: a.polls,
            longest_poll: a.longest_poll,
            pool: a.waits[0],
            query: a.waits[1],
            upstream: a.waits[2],
            other: handler.saturating_sub(a.poll + waits),
            write: now - responded,
            blocked,
            blocked_by,
            finished_at: now,
        };
        self.routes
            .entry(key.clone())
            .or_insert_with(|| RouteStats::new(key))
            .add(record);
    }

    /// How long the worker was held by other requests' or tasks' long polls
    /// between `from` and `to`, and by which task most.
    fn blocked(&self, me: u64, from: Instant, to: Instant) -> (Duration, Option<BlockedBy>) {
        let mut total = Duration::ZERO;
        let mut worst: Option<BlockedBy> = None;
        // Newest first; each ended before the next began.
        for lp in self.recent.iter().rev() {
            if lp.end <= from {
                break;
            }
            if lp.request == me && me != 0 {
                continue;
            }
            let held = lp.end.min(to).saturating_duration_since(lp.start.max(from));
            if held.is_zero() {
                continue;
            }
            total += held;
            if worst.as_ref().map_or(true, |w| held > w.held) {
                worst = Some(BlockedBy {
                    location: lp.location,
                    route: lp.route.clone(),
                    held,
                });
            }
        }
        (total, worst)
    }

    /// A task poll of [`LONG_POLL`] or more ended.
    pub(crate) fn long_poll(&mut self, location: Location, start: Instant, took: Duration) {
        let (request, route) = take_entered();
        let p = self.long_polls.entry(location).or_insert(PollProfile {
            location,
            polls: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        });
        p.polls += 1;
        p.total += took;
        p.max = p.max.max(took);
        if self.recent.len() == RECENT_LONG_POLLS {
            self.recent.pop_front();
        }
        self.recent.push_back(LongPollSeen {
            start,
            end: start + took,
            location,
            request,
            route,
        });
    }

    fn take(&mut self, worker: usize) -> RequestReport {
        let now = Instant::now();
        let mut routes: Vec<RouteStats> = std::mem::take(&mut self.routes).into_values().collect();
        for r in &mut routes {
            for s in &mut r.slowest {
                s.ago = now.saturating_duration_since(s.finished_at);
            }
        }
        routes.sort_by(|a, b| {
            (b.requests + b.abandoned)
                .cmp(&(a.requests + a.abandoned))
                .then_with(|| {
                    let name = |r: &RouteStats| r.route.as_ref().map(|r| r.as_str().to_owned());
                    name(a).cmp(&name(b))
                })
        });
        let mut long_polls: Vec<PollProfile> =
            std::mem::take(&mut self.long_polls).into_values().collect();
        long_polls.sort_by_key(|p| std::cmp::Reverse(p.total));
        RequestReport {
            worker,
            routes,
            long_polls,
        }
    }
}

impl fmt::Debug for Requests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Requests")
            .field("routes", &self.routes.len())
            .finish()
    }
}

/// Returns what was gathered about requests on the current worker since
/// request tracing was turned on with
/// [`Handle::set_request_tracing`](crate::Handle::set_request_tracing) (or
/// since the last call), and starts over.
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn take_requests() -> RequestReport {
    runtime::with_worker(|w| w.observe.requests.borrow_mut().take(w.index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_every_duration_in_order() {
        let mut last = 0;
        for ns in [
            0u64,
            1,
            1023,
            1024,
            1279,
            1280,
            1535,
            1536,
            2047,
            2048,
            1 << 20,
            1 << 36,
        ] {
            let b = bucket_of(Duration::from_nanos(ns));
            assert!(b >= last, "{ns} ns went back to bucket {b}");
            assert!(
                Duration::from_nanos(ns) < bucket_top(b),
                "{ns} ns above its bucket"
            );
            last = b;
        }
        assert_eq!(bucket_of(Duration::from_secs(10_000)), BUCKETS - 1);
        for i in 1..BUCKETS - 1 {
            assert_eq!(bucket_of(bucket_top(i)), i + 1, "edge of bucket {i}");
        }
    }

    fn record(total_ms: u64, pool_ms: u64) -> RequestRecord {
        let ms = Duration::from_millis;
        RequestRecord {
            worker: 0,
            protocol: "test",
            route: None,
            status: 200,
            ago: Duration::ZERO,
            total: ms(total_ms),
            read: Duration::ZERO,
            poll: ms(total_ms - pool_ms),
            polls: 1,
            longest_poll: ms(total_ms - pool_ms),
            pool: WaitStats {
                total: ms(pool_ms),
                count: 1,
                longest: ms(pool_ms),
                what: Some("test"),
            },
            query: WaitStats::default(),
            upstream: WaitStats::default(),
            other: Duration::ZERO,
            write: Duration::ZERO,
            blocked: Duration::ZERO,
            blocked_by: None,
            finished_at: Instant::now(),
        }
    }

    #[test]
    fn quantiles_and_the_tail_breakdown() {
        let mut s = RouteStats::new(None);
        for _ in 0..990 {
            s.add(record(2, 0));
        }
        for _ in 0..10 {
            s.add(record(400, 390));
        }
        assert_eq!(s.requests, 1000);
        assert!(s.quantile(0.5) <= Duration::from_millis(3));
        assert!(s.quantile(0.995) >= Duration::from_millis(400));
        let tail = s.breakdown(0.995, 1.0);
        assert_eq!(tail.requests, 10);
        assert_eq!(tail.pool, Duration::from_millis(3900));
        let fast = s.breakdown(0.0, 0.5);
        assert_eq!(fast.requests, 990);
        assert_eq!(fast.pool, Duration::ZERO);
        assert_eq!(s.slowest.len(), SLOWEST);
        assert_eq!(s.slowest[0].total, Duration::from_millis(400));

        let mut merged = s.clone();
        merged.merge(&s);
        assert_eq!(merged.requests, 2000);
        assert_eq!(merged.breakdown(0.995, 1.0).requests, 20);
    }

    #[test]
    fn a_wait_outside_a_traced_request_does_nothing() {
        let t = wait(Wait::Pool, "test");
        assert!(t.started.is_none());
        set_route(&Route::from_static("GET /"));
    }

    #[test]
    fn routes_compare_by_name() {
        assert_eq!(Route::from_static("GET /a"), Route::new("GET /a"));
        assert_ne!(Route::new("GET /a"), Route::new("GET /b"));
        assert_eq!(Route::new("GET /a").to_string(), "GET /a");
    }
}
