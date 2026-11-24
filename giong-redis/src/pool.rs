//! A pool of connections for one worker thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::ops::{Deref, DerefMut};
use std::pin::pin;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};
use std::time::Duration;

use giong::time::Instant;

use crate::client::Client;
use crate::cmd::{Cmd, Pipeline};
use crate::commands::commands;
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::value::{FromValue, ToArg};

/// How long a connection's oldest command may have been waiting before
/// shared commands avoid that connection.
const STALLED: Duration = Duration::from_millis(5);

/// Connections to one server, for the tasks of one worker thread.
///
/// Make one pool per worker, inside [`giong::Runtime::run`]: nothing is
/// shared between threads, so nothing is locked.
///
/// ```no_run
/// use giong_redis::{Config, Pool};
///
/// fn main() -> std::io::Result<()> {
///     let config = Config::from_url("redis://cache.internal:6379")?;
///     giong::Builder::new().build()?.run(move |_worker| {
///         let pool = Pool::new(config.clone(), 2);
///         async move {
///             let hits = pool.incr("hits", 1).await.unwrap();
///             println!("{hits} hits");
///         }
///     })
/// }
/// ```
///
/// [`query`](Self::query), [`pipeline`](Self::pipeline) and the command
/// methods (`get`, `set`...) do not take a connection for themselves: they
/// run on the least busy connection, beside the commands of other tasks
/// (pipelining), and the pool opens another connection, up to its maximum,
/// when every open one is in use by another task. On one connection the
/// server answers in order, so the pool steers new commands away from a
/// connection whose oldest command has waited more than 5 ms.
///
/// **How many connections.** Redis runs commands one at a time, and one
/// connection already carries the commands of many tasks at once, so one
/// or two per worker are usually enough; more raise the server's work for
/// little gain.
///
/// [`take`](Self::take) takes a connection for one task alone, for blocking
/// commands ([`Client::query_blocking`]) and `WATCH`
/// ([`Client::watch`]). While it is out, no shared command runs on it. It
/// comes back when the [`PooledClient`] is dropped, unless a blocking
/// command was cancelled and is still in flight, in which case it is
/// closed. When the pool is full, tasks waiting in `take` are served in
/// order: a connection is set aside for the first of them, shared commands
/// go elsewhere, and it is handed over as soon as the shared commands
/// already on it are answered. Shared commands keep at least one
/// connection unless `max_size` is 1.
///
/// Connections the server closed, or that broke, are left behind as soon
/// as that is noticed, which is when it happens: each connection is read
/// continuously. So is a connection whose server answers `READONLY`: it
/// became a replica, as the old primary does after a failover, and a new
/// connection (with the name resolved again) reaches the new primary.
#[derive(Clone)]
pub struct Pool {
    inner: Rc<Inner>,
}

struct Inner {
    config: Config,
    max_size: usize,
    wait_timeout: Duration,
    idle_timeout: Duration,
    max_lifetime: Duration,
    state: Rc<RefCell<State>>,
    /// The pool as `giong::observe` sees it; registered by a weak reference,
    /// so it leaves snapshots when the pool is dropped.
    _observed: Rc<Observed>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The task that closes idle connections ends with the pool.
        let reaper = {
            let mut st = self.state.borrow_mut();
            st.stopped = true;
            st.reaper_waker.take()
        };
        if let Some(w) = reaper {
            w.wake();
        }
    }
}

/// Reports the pool's state in `giong::observe` snapshots (and so to
/// giong-mcp).
struct Observed {
    name: String,
    max_size: usize,
    state: Rc<RefCell<State>>,
}

impl giong::observe::Source for Observed {
    fn kind(&self) -> &'static str {
        "redis pool"
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn stats(&self) -> Vec<(&'static str, u64)> {
        let s = stats_of(&self.state.borrow());
        let n = |v: usize| v as u64;
        vec![
            ("max_size", n(self.max_size)),
            ("connections", n(s.connections)),
            ("taken", n(s.taken)),
            ("connecting", n(s.connecting)),
            ("in_flight", n(s.in_flight)),
            ("waiting", n(s.waiting)),
            ("opened", s.opened),
            ("connect_errors", s.connect_errors),
            ("timeouts", s.timeouts),
            ("recycled", s.recycled),
        ]
    }
}

fn stats_of(st: &State) -> PoolStats {
    PoolStats {
        connections: st.shared.len() + st.draining.len() + st.taken,
        taken: st.taken,
        connecting: st.connecting,
        in_flight: st.untaken().map(|c| c.in_flight()).sum(),
        waiting: st.waiting,
        opened: st.opened,
        connect_errors: st.connect_errors,
        timeouts: st.timeouts,
        recycled: st.recycled,
    }
}

#[derive(Default)]
struct State {
    /// Open connections that are not taken, lent to shared commands.
    shared: Vec<Member>,
    /// Open connections set aside for the tasks waiting in `take`: no new
    /// shared command is sent on them, so they become free as soon as the
    /// commands already sent are answered.
    draining: Vec<Member>,
    /// The tasks waiting in `take` with the pool full, by ticket, oldest
    /// first. Only the first may take a connection.
    takers: VecDeque<u64>,
    next_ticket: u64,
    taken: usize,
    connecting: usize,
    waiting: usize,
    waiters: Vec<Waker>,
    /// The last attempt to open a connection failed, and none has opened
    /// since: the error. Meanwhile one attempt runs at a time, and the tasks
    /// that waited through it get its error.
    failing: Option<Error>,
    /// Attempts to open a connection that failed in a row.
    failures_in_row: u32,
    /// While opening connections fails and none is open, calls fail at once
    /// with `failing` until then, rather than try again (see
    /// [`connect_backoff`]).
    retry_at: Option<Instant>,
    opened: u64,
    connect_errors: u64,
    timeouts: u64,
    recycled: u64,
    /// The task that closes idle and old connections is running.
    reaper: bool,
    reaper_waker: Option<Waker>,
    /// The pool was dropped.
    stopped: bool,
}

/// An open connection that is not taken.
///
/// Shared commands read no clock: they only mark the connection `lent`, and
/// the task that closes idle connections turns that mark into `used` and an
/// expired lifetime into `retiring`, once per period.
struct Member {
    client: Rc<Client>,
    /// Lent to a shared command since the last sweep with a clock.
    lent: bool,
    /// When it was last seen lent or in use, or came back.
    used: Instant,
    /// When it is to be closed, however busy it is.
    expires: Instant,
    /// Past its lifetime: takes no new shared command, and closes once free.
    retiring: bool,
}

impl Member {
    /// No command uses it, and nobody else holds it.
    fn is_free(&self) -> bool {
        Rc::strong_count(&self.client) == 1 && self.client.in_flight() == 0
    }
}

/// What a pool is doing, from [`Pool::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolStats {
    /// Open connections, taken or not.
    pub connections: usize,
    /// Connections taken with [`Pool::take`].
    pub taken: usize,
    /// Connections being opened.
    pub connecting: usize,
    /// Commands sent on the shared connections and not answered yet.
    pub in_flight: usize,
    /// Tasks waiting for a connection.
    pub waiting: usize,
    /// Connections opened since the pool was made.
    pub opened: u64,
    /// Attempts to open a connection that failed.
    pub connect_errors: u64,
    /// Waits that ended with [`ErrorKind::PoolTimeout`].
    pub timeouts: u64,
    /// Connections the pool closed because they sat unused for the [idle
    /// timeout](Pool::idle_timeout) or were older than the [maximum
    /// lifetime](Pool::max_lifetime).
    pub recycled: u64,
}

/// How long calls fail at once after `failures` attempts to open a
/// connection failed in a row with none open: 10 ms doubling to 1 s, less a
/// random part of up to half, so that the pools of many workers and
/// processes do not try again together.
fn connect_backoff(failures: u32) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let full = Duration::from_millis(10)
        .saturating_mul(1 << failures.saturating_sub(1).min(7))
        .min(Duration::from_secs(1));
    // Not for secrets: only spreads the attempts. The standard library
    // seeds each RandomState differently.
    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    full - (full / 2).mul_f64((seed % 1024) as f64 / 1024.0)
}

impl State {
    /// Room for one more connection: below the maximum, and, while opening
    /// connections fails, no attempt under way.
    fn room(&self, max: usize) -> bool {
        self.shared.len() + self.draining.len() + self.taken + self.connecting < max
            && (self.failing.is_none() || self.connecting == 0)
    }

    /// The connections that are not taken.
    fn untaken(&self) -> impl Iterator<Item = &Rc<Client>> {
        self.shared.iter().chain(&self.draining).map(|m| &m.client)
    }

    /// Leaves out the connections that broke or whose server became a
    /// replica (they close once the commands on them are answered), and
    /// closes the free ones that are past their lifetime or, given the time,
    /// idle for `idle_timeout`. Returns whether any went.
    fn sweep(&mut self, now: Option<Instant>, idle_timeout: Duration) -> bool {
        let before = self.shared.len() + self.draining.len();
        let mut recycled = 0;
        let mut keep = |m: &mut Member| {
            if m.client.is_closed() || m.client.is_stale() {
                return false;
            }
            let mut idle = false;
            if let Some(now) = now {
                // A command still in flight counts as use: one that takes
                // longer than the idle timeout leaves no mark after the
                // sweep that cleared it.
                if std::mem::take(&mut m.lent) || !m.is_free() {
                    m.used = now;
                }
                m.retiring |= now >= m.expires;
                idle = now.saturating_duration_since(m.used) >= idle_timeout;
            }
            let old = m.is_free() && (m.retiring || idle);
            recycled += u64::from(old);
            !old
        };
        self.shared.retain_mut(&mut keep);
        self.draining.retain_mut(&mut keep);
        self.recycled += recycled;
        self.shared.len() + self.draining.len() != before
    }

    /// Sets aside one connection for each task waiting in `take`, but
    /// leaves shared commands at least one unless the pool holds only one.
    /// Returns whether a connection went back to shared commands.
    fn balance(&mut self, max_size: usize) -> bool {
        let untaken = self.shared.len() + self.draining.len();
        let keep = if max_size == 1 { 0 } else { 1 };
        let want = self.takers.len().min(untaken.saturating_sub(keep));
        while self.draining.len() < want {
            // The least busy one is free soonest.
            let Some(i) =
                (0..self.shared.len()).min_by_key(|&i| Rc::strong_count(&self.shared[i].client))
            else {
                break;
            };
            let c = self.shared.swap_remove(i);
            self.draining.push(c);
        }
        let mut returned = false;
        while self.draining.len() > want {
            let c = self.draining.pop().expect("more than wanted");
            self.shared.push(c);
            returned = true;
        }
        returned
    }

    /// Hands a connection no command is using to the task taking it.
    fn take_idle(&mut self) -> Option<(Client, Instant)> {
        let idle = |v: &Vec<Member>| v.iter().position(|m| Rc::strong_count(&m.client) == 1);
        let m = if let Some(i) = idle(&self.draining) {
            self.draining.swap_remove(i)
        } else {
            let i = idle(&self.shared)?;
            self.shared.swap_remove(i)
        };
        self.taken += 1;
        match Rc::try_unwrap(m.client) {
            Ok(client) => Some((client, m.expires)),
            Err(_) => unreachable!("checked: nobody else holds it"),
        }
    }
}

/// A task's place in the queue of `take`, given up however the wait ends.
struct Ticket<'a> {
    pool: &'a Pool,
    number: Cell<Option<u64>>,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let Some(n) = self.number.get() else { return };
        {
            let mut st = self.pool.inner.state.borrow_mut();
            st.takers.retain(|&t| t != n);
            st.balance(self.pool.inner.max_size);
        }
        // The next taker may go, and a connection set aside for this one
        // may be back with shared commands.
        self.pool.notify();
    }
}

enum Decision<T> {
    Ready(T),
    Connect,
    Wait,
}

/// Undoes the `connecting` count however the attempt ends.
struct Connecting<'a>(&'a Pool);

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        self.0.inner.state.borrow_mut().connecting -= 1;
        self.0.notify();
    }
}

/// Undoes the `waiting` count however the wait ends.
struct Waiting<'a>(&'a Pool);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.inner.state.borrow_mut().waiting -= 1;
    }
}

impl Pool {
    /// A pool of at most `max_size` connections to the server in `config`.
    /// Connections are opened when needed, not here.
    ///
    /// When opening one fails, the pool opens one at a time until one
    /// succeeds, and tasks that waited through a failed attempt, with no
    /// connection open to wait for, get its error at once: while the server
    /// is down, a burst of calls makes one attempt, not one each. With no
    /// connection open, calls after a failed attempt also get its error at
    /// once for a pause that starts at 10 ms and doubles with each failure
    /// in a row up to 1 s (less a random part of up to half, so that many
    /// pools do not try together); the first call after the pause tries
    /// again, and the first success ends it.
    ///
    /// # Panics
    ///
    /// Panics if `max_size` is 0.
    pub fn new(config: Config, max_size: usize) -> Self {
        assert!(
            max_size > 0,
            "a pool needs room for at least one connection"
        );
        let state = Rc::new(RefCell::default());
        let observed = Rc::new(Observed {
            name: config.describe(),
            max_size,
            state: Rc::clone(&state),
        });
        let source: Weak<dyn giong::observe::Source> = Rc::downgrade(&observed) as _;
        giong::observe::register(source);
        Self {
            inner: Rc::new(Inner {
                config,
                max_size,
                wait_timeout: Duration::from_secs(5),
                idle_timeout: Duration::from_secs(10 * 60),
                max_lifetime: Duration::from_secs(30 * 60),
                state,
                _observed: observed,
            }),
        }
    }

    /// The longest a task waits for a connection when all `max_size` are
    /// taken: 5 seconds by default. A longer wait fails with
    /// [`ErrorKind::PoolTimeout`]. `Duration::MAX` waits as long as it
    /// takes.
    ///
    /// # Panics
    ///
    /// Panics if other clones of this pool exist; set it right after
    /// [`new`](Self::new).
    pub fn wait_timeout(mut self, timeout: Duration) -> Self {
        self.settings("wait timeout").wait_timeout = timeout;
        self
    }

    /// How long a connection may sit unused before the pool closes it: 10
    /// minutes by default. The pool opens connections again when it needs
    /// them, so a burst does not hold the server's connections forever.
    ///
    /// # Panics
    ///
    /// Panics if other clones of this pool exist; set it right after
    /// [`new`](Self::new).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.settings("idle timeout").idle_timeout = timeout;
        self
    }

    /// How long a connection may live: 30 minutes by default, less up to a
    /// tenth at random so that the connections of a pool do not all close at
    /// once. An older connection takes no new command, is closed as soon as
    /// the commands on it are answered, and is replaced when needed, so that
    /// connections follow DNS changes and failovers.
    ///
    /// # Panics
    ///
    /// Panics if other clones of this pool exist; set it right after
    /// [`new`](Self::new).
    pub fn max_lifetime(mut self, lifetime: Duration) -> Self {
        self.settings("maximum lifetime").max_lifetime = lifetime;
        self
    }

    fn settings(&mut self, what: &str) -> &mut Inner {
        Rc::get_mut(&mut self.inner)
            .unwrap_or_else(|| panic!("set the {what} before cloning the pool"))
    }

    /// Sends a command on a shared connection; see [`Client::query`].
    ///
    /// Not cancel safe, like [`Client::query`].
    pub async fn query<T: FromValue>(&self, cmd: &Cmd) -> Result<T, Error> {
        // Refused before a connection is opened for it.
        cmd.check(false, false)?;
        self.shared().await?.query(cmd).await
    }

    /// Sends one of the command methods' commands on a shared connection.
    pub(crate) async fn query_raw<T: FromValue>(
        &self,
        name: &'static str,
        args: &[&dyn ToArg],
    ) -> Result<T, Error> {
        self.shared().await?.query_raw(name, args).await
    }

    /// Sends a pipeline on a shared connection; see [`Client::pipeline`].
    ///
    /// Not cancel safe, like [`Client::query`].
    pub async fn pipeline<T: FromValue>(&self, pipe: &Pipeline) -> Result<T, Error> {
        pipe.check(pipe.is_atomic())?;
        self.shared().await?.pipeline(pipe).await
    }

    commands!();

    /// Takes a connection for this task alone, for blocking commands and
    /// `WATCH`:
    ///
    /// ```no_run
    /// use giong_redis::cmd;
    /// # async fn f(pool: &giong_redis::Pool) -> Result<(), giong_redis::Error> {
    /// let mut conn = pool.take().await?;
    /// let job: Option<(String, String)> =
    ///     conn.query_blocking(&cmd("BRPOP").arg("jobs").arg(10)).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Waits up to the [wait timeout](Self::wait_timeout) when every
    /// connection is taken or busy and the pool is full.
    ///
    /// Cancel safe: if the future is dropped, no connection is taken.
    pub async fn take(&self) -> Result<PooledClient, Error> {
        let ticket = Ticket {
            pool: self,
            number: Cell::new(None),
        };
        let client = self
            .acquire(|st, max| {
                // Tasks that came first go first.
                if st.takers.front().copied() == ticket.number.get() || st.takers.is_empty() {
                    if let Some(client) = st.take_idle() {
                        return Decision::Ready(client);
                    }
                    if st.room(max) {
                        st.connecting += 1;
                        return Decision::Connect;
                    }
                }
                if ticket.number.get().is_none() {
                    let n = st.next_ticket;
                    st.next_ticket += 1;
                    st.takers.push_back(n);
                    ticket.number.set(Some(n));
                    st.balance(max);
                }
                Decision::Wait
            })
            .await?;
        drop(ticket);
        let (client, expires) = match client {
            Ok(c) => c,
            Err(new) => {
                self.inner.state.borrow_mut().taken += 1;
                (new, self.expiry())
            }
        };
        Ok(PooledClient {
            client: Some(client),
            expires,
            pool: self.clone(),
        })
    }

    /// The pool's current state and counters.
    pub fn stats(&self) -> PoolStats {
        stats_of(&self.inner.state.borrow())
    }

    /// When a connection opened now is to be closed: the maximum lifetime,
    /// less up to a tenth of it.
    fn expiry(&self) -> Instant {
        use std::hash::{BuildHasher, Hasher};
        let life = self.inner.max_lifetime;
        // Not for secrets: only spreads the closing times. The standard
        // library seeds each RandomState differently.
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let jitter = life / 10_000 * u32::try_from(seed % 1000).unwrap_or(0);
        // A lifetime too long for the clock (`Duration::MAX`) means none.
        Instant::now()
            .checked_add(life - jitter)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(100 * 365 * 24 * 3600))
    }

    /// Starts the task that closes idle and old connections, once.
    fn start_reaper(&self, st: &mut State) {
        if st.reaper {
            return;
        }
        st.reaper = true;
        let state = Rc::downgrade(&self.inner.state);
        let idle_timeout = self.inner.idle_timeout;
        let period = (self.inner.idle_timeout.min(self.inner.max_lifetime) / 4)
            .clamp(Duration::from_millis(50), Duration::from_secs(30));
        giong::spawn(reap(state, idle_timeout, period));
    }

    /// The least busy shared connection, or a new one if every open one is
    /// busy and there is room.
    ///
    /// A connection whose oldest command has waited more than [`STALLED`]
    /// is passed over while another will do: commands queue behind it in
    /// order, so a slow one would hold up everything sent after it on that
    /// connection.
    async fn shared(&self) -> Result<Lent<'_>, Error> {
        let client = self
            .acquire(|st, max| {
                let room = st.room(max);
                // The clock is read only if a connection is busy.
                let mut now = None;
                let mut stalled = |c: &Client| match c.head_since() {
                    None => false,
                    Some(t) => t + STALLED < *now.get_or_insert_with(Instant::now),
                };
                // A connection past its lifetime takes no new command, so
                // that it frees up and closes.
                let usable = |m: &Member| !m.retiring;
                // Busy means used by other tasks right now.
                let mut best: Option<usize> = None;
                for (i, m) in st.shared.iter().enumerate() {
                    if usable(m)
                        && !stalled(&m.client)
                        && best.map_or(true, |b| {
                            Rc::strong_count(&m.client) < Rc::strong_count(&st.shared[b].client)
                        })
                    {
                        best = Some(i);
                    }
                }
                let pick = match best {
                    Some(i) if Rc::strong_count(&st.shared[i].client) == 1 || !room => Some(i),
                    _ if room => {
                        st.connecting += 1;
                        return Decision::Connect;
                    }
                    // Every connection is stalled and the pool is full: the
                    // one whose oldest command has waited least, which is
                    // the one most likely to be moving.
                    _ => st
                        .shared
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| usable(m))
                        .max_by_key(|(_, m)| m.client.head_since())
                        .map(|(i, _)| i),
                };
                match pick {
                    Some(i) => {
                        let m = &mut st.shared[i];
                        m.lent = true;
                        Decision::Ready(m.client.clone())
                    }
                    None => Decision::Wait,
                }
            })
            .await?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
                let c = Rc::new(new);
                self.inner.state.borrow_mut().shared.push(Member {
                    client: c.clone(),
                    lent: false,
                    used: Instant::now(),
                    expires: self.expiry(),
                    retiring: false,
                });
                c
            }
        };
        Ok(Lent {
            pool: self,
            client: Some(client),
        })
    }

    /// Wakes every task waiting for a connection, to look again.
    fn notify(&self) {
        let waiters = std::mem::take(&mut self.inner.state.borrow_mut().waiters);
        for w in waiters {
            w.wake();
        }
    }

    /// Runs `decide` until it hands out a connection (`Ok`), or a new one is
    /// opened for the caller (`Err(client)`), or the wait times out.
    async fn acquire<T>(
        &self,
        mut decide: impl FnMut(&mut State, usize) -> Decision<T>,
    ) -> Result<Result<T, Client>, Error> {
        let _wait = giong::observe::wait(giong::observe::Wait::Pool, "redis");
        // A timeout too long for the clock (`Duration::MAX`) means none.
        let deadline = Instant::now().checked_add(self.inner.wait_timeout);
        // Failed attempts to open a connection so far: when one fails while
        // this task waits, and the pool has no connection open to wait for,
        // the task fails with its error rather than try again at once.
        let failed_before = self.inner.state.borrow().connect_errors;
        let mut waited = false;
        loop {
            let (decision, returned) = {
                let mut st = self.inner.state.borrow_mut();
                let none_open = st.shared.is_empty() && st.draining.is_empty() && st.taken == 0;
                if waited && none_open && st.connect_errors != failed_before {
                    if let Some(e) = st.failing.clone() {
                        return Err(e);
                    }
                }
                if none_open && st.connecting == 0 {
                    if let (Some(e), Some(at)) = (&st.failing, st.retry_at) {
                        if Instant::now() < at {
                            let e = e.clone();
                            drop(st);
                            return Err(e);
                        }
                    }
                }
                // Connections that broke, or whose server became a replica
                // (`READONLY`, after a failover), are left behind; they
                // close once the commands on them are answered.
                let swept = st.sweep(None, self.inner.idle_timeout);
                let returned = st.balance(self.inner.max_size) || swept;
                (decide(&mut st, self.inner.max_size), returned)
            };
            if returned {
                self.notify();
            }
            match decision {
                Decision::Ready(c) => return Ok(Ok(c)),
                Decision::Connect => {
                    let _connecting = Connecting(self);
                    let result = Client::connect(&self.inner.config).await;
                    let mut st = self.inner.state.borrow_mut();
                    return match result {
                        Ok(client) => {
                            st.opened += 1;
                            st.failing = None;
                            st.failures_in_row = 0;
                            st.retry_at = None;
                            self.start_reaper(&mut st);
                            Ok(Err(client))
                        }
                        Err(e) => {
                            st.connect_errors += 1;
                            st.failing = Some(e.clone());
                            st.failures_in_row = st.failures_in_row.saturating_add(1);
                            st.retry_at =
                                Some(Instant::now() + connect_backoff(st.failures_in_row));
                            Err(e)
                        }
                    };
                }
                Decision::Wait => {
                    waited = true;
                    self.inner.state.borrow_mut().waiting += 1;
                    let _waiting = Waiting(self);
                    let mut registered = false;
                    let woken = poll_fn(|cx| {
                        if registered {
                            return Poll::Ready(());
                        }
                        registered = true;
                        self.inner
                            .state
                            .borrow_mut()
                            .waiters
                            .push(cx.waker().clone());
                        Poll::Pending
                    });
                    let timed_out = match deadline {
                        Some(deadline) => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            giong::time::timeout(left, woken).await.is_err()
                        }
                        None => {
                            woken.await;
                            false
                        }
                    };
                    if timed_out {
                        let mut st = self.inner.state.borrow_mut();
                        st.timeouts += 1;
                        return Err(Error::new(
                            ErrorKind::PoolTimeout,
                            format!(
                                "no connection to {} became free within {:?}: all {} are taken. \
                                 Raise the pool's max_size, give connections taken with \
                                 Pool::take back sooner, or raise Pool::wait_timeout",
                                self.inner.config.target(),
                                self.inner.wait_timeout,
                                self.inner.max_size
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// Closes idle and old connections now and then, until the pool is dropped.
async fn reap(state: Weak<RefCell<State>>, idle_timeout: Duration, period: Duration) {
    loop {
        let mut sleep = pin!(giong::time::sleep(period));
        let stopped = poll_fn(|cx| {
            let Some(state) = state.upgrade() else {
                return Poll::Ready(true);
            };
            let mut st = state.borrow_mut();
            if st.stopped {
                return Poll::Ready(true);
            }
            st.reaper_waker = Some(cx.waker().clone());
            drop(st);
            sleep.as_mut().poll(cx).map(|()| false)
        })
        .await;
        if stopped {
            return;
        }
        let Some(state) = state.upgrade() else { return };
        let waiters = {
            let mut st = state.borrow_mut();
            if st.sweep(Some(Instant::now()), idle_timeout) {
                // Room for a new connection.
                std::mem::take(&mut st.waiters)
            } else {
                Vec::new()
            }
        };
        for w in waiters {
            w.wake();
        }
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("server", &self.inner.config.target())
            .field("max_size", &self.inner.max_size)
            .field("stats", &self.stats())
            .finish()
    }
}

/// A shared connection lent to one command or pipeline. Dropping it, when
/// the reply came or the future was dropped, tells waiting tasks to look
/// again: the connection may have become free to take.
struct Lent<'a> {
    pool: &'a Pool,
    client: Option<Rc<Client>>,
}

impl Deref for Lent<'_> {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client.as_ref().expect("present until dropped")
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.client = None;
        self.pool.notify();
    }
}

/// A connection taken from a [`Pool`] with [`Pool::take`]. It dereferences
/// to the [`Client`], and goes back to the pool when dropped.
pub struct PooledClient {
    client: Option<Client>,
    /// When the connection is to be closed rather than go back.
    expires: Instant,
    pool: Pool,
}

impl Deref for PooledClient {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client.as_ref().expect("present until dropped")
    }
}

impl DerefMut for PooledClient {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("present until dropped")
    }
}

impl Drop for PooledClient {
    fn drop(&mut self) {
        let client = self.client.take().expect("present until dropped");
        {
            let mut st = self.pool.inner.state.borrow_mut();
            st.taken -= 1;
            // A blocking command still in flight was cancelled: it would
            // hold up the commands of other tasks until it returns.
            let now = Instant::now();
            if !client.is_closed() && !client.is_blocked() && !client.is_stale() {
                if now >= self.expires {
                    // Past its lifetime: closed (dropped) instead of going
                    // back.
                    st.recycled += 1;
                } else {
                    st.shared.push(Member {
                        client: Rc::new(client),
                        lent: false,
                        used: now,
                        expires: self.expires,
                        retiring: false,
                    });
                }
            }
        }
        self.pool.notify();
    }
}

impl std::fmt::Debug for PooledClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PooledClient").field(&**self).finish()
    }
}
