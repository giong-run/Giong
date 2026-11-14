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

use crate::client::{Client, Transaction};
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::row::Row;
use crate::types::ToSql;

/// How long a connection's oldest statement may have been running before
/// shared statements avoid that connection.
const STALLED: Duration = Duration::from_millis(5);

/// The most statements the pool's statement methods keep on one connection
/// at a time; past it they use another connection, or wait.
const MAX_PIPELINE: usize = 1024;

/// Connections to one server, for the tasks of one worker thread.
///
/// Make one pool per worker, inside [`giong::Runtime::run`]: nothing is
/// shared between threads, so nothing is locked.
///
/// ```no_run
/// use giong_postgres::{Config, Pool};
///
/// fn main() -> std::io::Result<()> {
///     let config = Config::from_url("postgres://app:secret@db.internal/shop")?;
///     giong::Builder::new().build()?.run(move |_worker| {
///         let pool = Pool::new(config.clone(), 4);
///         async move {
///             let row = pool.query_one("SELECT count(*) FROM orders", &[]).await.unwrap();
///             let n: i64 = row.get(0).unwrap();
///             println!("{n} orders");
///         }
///     })
/// }
/// ```
///
/// [`query`](Self::query) and the other statement methods do not take a
/// connection for themselves: they run on the least busy connection, beside
/// statements from other tasks (pipelining), and the pool opens another
/// connection, up to its maximum, when every open one is busy. That is the
/// fast path for statements that stand alone. On one connection the server
/// answers in order, so a statement waits for those sent before it; the pool
/// steers new statements away from a connection whose oldest statement has
/// been running for more than 5 ms. Run statements known to be slow (reports,
/// migrations) on a connection of their own, from [`get`](Self::get). At most
/// 1024 statements run on one connection at a time; past that, statements
/// use another connection or wait for room.
///
/// **How many connections.** Because statements pipeline, a few connections
/// keep the server busy: start with the server's cores, shared among the
/// workers' pools. More than that adds no throughput and raises tail latency,
/// since a backend waiting for a core holds up every statement queued on its
/// connection. Statements that wait on disk or on locks need more.
///
/// [`get`](Self::get) takes a connection for one task alone: for a
/// transaction, for session settings (`SET`, `SET ROLE`, temporary tables,
/// advisory locks, `LISTEN`), or for a slow statement. While it is out, no
/// shared statement runs on it. Tasks waiting in `get` are served in the
/// order they came, and shared statements stop using a connection while a
/// `get` waits for it, so `get` is served even when shared statements keep
/// every connection busy; shared statements always keep at least one
/// connection when the pool has room for two. The connection comes back when
/// the [`PooledClient`] is dropped, and its session is reset before anything
/// else runs on it: a transaction left open is rolled back, and if anything
/// was sent on it, `SET SESSION AUTHORIZATION DEFAULT; RESET ALL; CLOSE ALL;
/// UNLISTEN *; SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD
/// SEQUENCES` follows (plus `DEALLOCATE ALL` after an SQL-level `PREPARE`).
/// Settings given to [`Config`] survive the reset; settings made with `SET`
/// do not. These requests are pipelined in front of the next statement, so
/// nobody waits a round trip for them; if they fail, the connection is closed
/// instead of reused.
///
/// **Transactions and session changes never run on the shared
/// connections**, since the statements of other tasks would run inside them
/// or under them. `query`, `query_one`, `query_opt` and `execute` refuse
/// `BEGIN`, `COMMIT`, `ROLLBACK` and the other transaction statements, and
/// all the statement methods refuse statements that change the session
/// (`SET` without `LOCAL`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`,
/// `PREPARE`, `DEALLOCATE`, `DECLARE`, `LOAD`, `CREATE TEMP ...`) and
/// `COPY ... FROM STDIN`, before sending anything ([`ErrorKind::Statement`]).
/// Functions that change the session (`set_config(.., false)`,
/// `pg_advisory_lock`) cannot be recognised this way: call them on a
/// connection from `get`. [`batch_execute`](Self::batch_execute) may carry a
/// whole transaction (`BEGIN; ...; COMMIT`); if a batch leaves one open, its
/// connection is closed, which rolls it back, and the batch and every
/// statement sent on that connection after it fail with
/// [`ErrorKind::Closed`].
///
/// Connections the server closed, or that broke, are left behind as soon as
/// that is noticed, which is when it happens: each connection is read
/// continuously. A connection unused for the [idle
/// timeout](Self::idle_timeout) is closed, and so is one older than the
/// [maximum lifetime](Self::max_lifetime) or whose server refused a write
/// because it is a standby (after a failover), once no statement uses it.
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
        "postgres pool"
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
        connections: st.shared.len() + st.taken,
        taken: st.taken,
        connecting: st.connecting,
        in_flight: st.shared.iter().map(|m| m.client.in_flight()).sum(),
        waiting: st.waiting,
        opened: st.opened,
        connect_errors: st.connect_errors,
        timeouts: st.timeouts,
        recycled: st.recycled,
    }
}

#[derive(Default)]
struct State {
    /// Open connections that are not taken.
    shared: Vec<Member>,
    taken: usize,
    connecting: usize,
    waiting: usize,
    /// Tasks of the statement methods waiting for room, woken one at a time:
    /// a statement that finishes frees room for one more.
    shared_waiters: VecDeque<Waker>,
    /// Tasks waiting in `get`, all woken when a connection may have become
    /// free (there are few of them).
    get_waiters: Vec<Waker>,
    /// The tickets of the `get` calls waiting, oldest first: only the oldest
    /// may take a connection.
    queue: VecDeque<u64>,
    next_ticket: u64,
    /// Wakers to wake once the state is no longer borrowed.
    to_wake: Vec<Waker>,
    /// The task that closes idle and old connections is running.
    reaper: bool,
    reaper_waker: Option<Waker>,
    /// The pool was dropped.
    stopped: bool,
    opened: u64,
    connect_errors: u64,
    timeouts: u64,
    recycled: u64,
}

/// A connection among the shared ones.
struct Member {
    client: Rc<Client>,
    /// Kept free for a task waiting in `get`: shared statements do not use it.
    draining: bool,
    /// When it was last lent or came back.
    used: Instant,
    /// When it is to be closed, however busy it is.
    expires: Instant,
}

impl Member {
    /// No statement method is using it: nobody else holds it.
    fn is_free(&self) -> bool {
        Rc::strong_count(&self.client) == 1
    }

    /// To be closed once free: too old, or its server is a standby.
    fn is_retiring(&self, now: Instant) -> bool {
        now >= self.expires || self.client.should_retire()
    }
}

impl State {
    fn room(&self, max: usize) -> bool {
        self.shared.len() + self.taken + self.connecting < max
    }

    /// Wakes every waiting task (later, see `to_wake`), to look again.
    fn wake_all(&mut self) {
        let shared = std::mem::take(&mut self.shared_waiters);
        self.to_wake.extend(shared);
        self.to_wake.append(&mut self.get_waiters);
    }

    /// Wakes the tasks waiting in `get`.
    fn wake_gets(&mut self) {
        let gets = std::mem::take(&mut self.get_waiters);
        self.to_wake.extend(gets);
    }

    /// Wakes one task of the statement methods.
    fn wake_one_shared(&mut self) {
        if let Some(w) = self.shared_waiters.pop_front() {
            self.to_wake.push(w);
        }
    }

    /// Closes the shared connections that broke, and the free ones that are
    /// idle, too old, or on a standby.
    fn sweep(&mut self, now: Instant, idle_timeout: Duration) {
        let before = self.shared.len();
        let mut recycled = 0;
        self.shared.retain(|m| {
            if m.client.is_closed() {
                return false;
            }
            let stale = m.is_free()
                && (m.is_retiring(now)
                    || (m.client.in_flight() == 0
                        && now.saturating_duration_since(m.used) >= idle_timeout));
            recycled += u64::from(stale);
            !stale
        });
        self.recycled += recycled;
        if self.shared.len() != before {
            // Room for a new connection, or fewer to drain.
            self.wake_all();
        }
    }

    /// Marks as many shared connections draining as tasks wait in `get`,
    /// leaving at least one to the statement methods when the pool has room
    /// for two.
    fn rebalance(&mut self, max: usize) {
        let target = match self.queue.len() {
            0 => 0,
            n if max == 1 => n.min(self.shared.len()),
            n => n.min(self.shared.len().saturating_sub(1)),
        };
        let mut draining = self.shared.iter().filter(|m| m.draining).count();
        while draining < target {
            // The least used one, so that it drains soonest.
            let next = self
                .shared
                .iter_mut()
                .filter(|m| !m.draining)
                .min_by_key(|m| (Rc::strong_count(&m.client), m.client.in_flight()));
            let Some(m) = next else { break };
            m.draining = true;
            draining += 1;
        }
        if draining > target {
            for m in self.shared.iter_mut().filter(|m| m.draining) {
                if draining == target {
                    break;
                }
                m.draining = false;
                draining -= 1;
            }
            self.wake_all();
        }
    }

    fn remove_ticket(&mut self, ticket: u64) {
        if let Some(i) = self.queue.iter().position(|&t| t == ticket) {
            self.queue.remove(i);
        }
    }
}

/// What a pool is doing, from [`Pool::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolStats {
    /// Open connections, taken or not.
    pub connections: usize,
    /// Connections taken with [`Pool::get`].
    pub taken: usize,
    /// Connections being opened.
    pub connecting: usize,
    /// Requests sent on the shared connections and not answered yet.
    pub in_flight: usize,
    /// Tasks waiting for a connection.
    pub waiting: usize,
    /// Connections opened since the pool was made.
    pub opened: u64,
    /// Attempts to open a connection that failed.
    pub connect_errors: u64,
    /// Waits that ended with [`ErrorKind::PoolTimeout`].
    pub timeouts: u64,
    /// Connections the pool closed although they worked: idle longer than
    /// the [idle timeout](Pool::idle_timeout), older than the [maximum
    /// lifetime](Pool::max_lifetime), or on a server that turned out to be a
    /// standby.
    pub recycled: u64,
}

enum Decision<T> {
    Ready(T),
    Connect,
    Wait,
}

/// Which list a waiting task is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waiter {
    Shared,
    Get,
}

/// Undoes the `connecting` count however the attempt ends.
struct Connecting<'a>(&'a Pool);

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        let mut st = self.0.inner.state.borrow_mut();
        st.connecting -= 1;
        st.wake_all();
        drop(st);
        self.0.flush_wakes();
    }
}

/// Counts a task as waiting from its first wait until it gets a connection
/// or gives up. A task of the statement methods that gives up passes its
/// turn on: it may have been woken for room it will not use.
struct Waiting<'a> {
    pool: &'a Pool,
    kind: Waiter,
    served: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut st = self.pool.inner.state.borrow_mut();
        st.waiting -= 1;
        if !self.served && self.kind == Waiter::Shared {
            st.wake_one_shared();
        }
        drop(st);
        self.pool.flush_wakes();
    }
}

/// A `get` call's place in the queue, given up however the call ends.
struct Ticket<'a> {
    pool: &'a Pool,
    id: Cell<Option<u64>>,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut st = self.pool.inner.state.borrow_mut();
            st.remove_ticket(id);
            st.rebalance(self.pool.inner.max_size);
            // The next one in the queue may go now.
            st.wake_gets();
            drop(st);
            self.pool.flush_wakes();
        }
    }
}

impl Pool {
    /// A pool of at most `max_size` connections to the server in `config`.
    /// Connections are opened when needed, not here.
    ///
    /// Size it from the server's `max_connections`: the pools of all workers
    /// of all processes together must stay below it.
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
        giong::observe::register_rc(&observed);
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

    fn settings(&mut self, what: &str) -> &mut Inner {
        Rc::get_mut(&mut self.inner)
            .unwrap_or_else(|| panic!("set the {what} before cloning the pool"))
    }

    /// The longest a task waits for a connection when all `max_size` are
    /// taken or busy: 5 seconds by default. A longer wait fails with
    /// [`ErrorKind::PoolTimeout`]. A task that opens a new connection waits
    /// for it up to [`Config::connect_timeout`] instead.
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
    /// once. An older connection is closed as soon as no statement uses it,
    /// and replaced when needed, so that connections follow DNS changes and
    /// failovers, and the server's per-connection memory is given back.
    ///
    /// # Panics
    ///
    /// Panics if other clones of this pool exist; set it right after
    /// [`new`](Self::new).
    pub fn max_lifetime(mut self, lifetime: Duration) -> Self {
        self.settings("maximum lifetime").max_lifetime = lifetime;
        self
    }

    /// Runs a statement on a shared connection and returns its rows; see
    /// [`Client::query`]. Statements that control a transaction or change
    /// the session are refused ([`ErrorKind::Statement`]): run them on a
    /// connection from [`get`](Self::get).
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        refuse_on_shared(sql, false)?;
        let client = self.shared().await?;
        client.query(sql, params).await
    }

    /// Runs a statement that returns exactly one row; see
    /// [`Client::query_one`]. Refuses the statements [`query`](Self::query)
    /// refuses.
    pub async fn query_one(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Row, Error> {
        refuse_on_shared(sql, false)?;
        let client = self.shared().await?;
        client.query_one(sql, params).await
    }

    /// Runs a statement that returns at most one row; see
    /// [`Client::query_opt`]. Refuses the statements [`query`](Self::query)
    /// refuses.
    pub async fn query_opt(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<Row>, Error> {
        refuse_on_shared(sql, false)?;
        let client = self.shared().await?;
        client.query_opt(sql, params).await
    }

    /// Runs a statement and returns the number of rows it changed; see
    /// [`Client::execute`]. Refuses the statements [`query`](Self::query)
    /// refuses.
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        refuse_on_shared(sql, false)?;
        let client = self.shared().await?;
        client.execute(sql, params).await
    }

    /// Runs several statements without parameters; see
    /// [`Client::batch_execute`].
    ///
    /// The batch may hold a whole transaction, `BEGIN; ...; COMMIT`. If it
    /// leaves one open, the connection is closed, which rolls the
    /// transaction back, and the batch fails with [`ErrorKind::Closed`], as
    /// do the statements other tasks sent on that connection after it: they
    /// would otherwise have run inside the transaction. Statements that
    /// change the session (`SET` without `LOCAL`, `LISTEN`,
    /// `COPY ... FROM STDIN` and the others listed on [`Pool`]) are refused
    /// before anything is sent ([`ErrorKind::Statement`]).
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        refuse_on_shared(sql, true)?;
        let client = self.shared().await?;
        client.batch_execute(sql).await
    }

    /// Takes a connection for this task alone, for a transaction, session
    /// settings or a slow statement:
    ///
    /// ```no_run
    /// # async fn f(pool: &giong_postgres::Pool) -> Result<(), giong_postgres::Error> {
    /// let mut conn = pool.get().await?;
    /// let tx = conn.transaction().await?;
    /// tx.execute("INSERT INTO audit (what) VALUES ($1)", &[&"signup"]).await?;
    /// tx.commit().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Waits up to the [wait timeout](Self::wait_timeout) when every
    /// connection is taken or busy and the pool is full. Tasks waiting here
    /// are served in the order they came. The session is reset when the
    /// connection comes back (see [`Pool`]).
    pub async fn get(&self) -> Result<PooledClient, Error> {
        let ticket = Ticket {
            pool: self,
            id: Cell::new(None),
        };
        let max = self.inner.max_size;
        let taken = self
            .acquire(Waiter::Get, |st, now| {
                let mine = ticket.id.get();
                let turn = st.queue.front().map_or(true, |&t| Some(t) == mine);
                if turn {
                    // A connection nobody else holds, busy at most with
                    // requests whose tasks stopped waiting (and only
                    // briefly), draining ones first.
                    let free = st
                        .shared
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| {
                            m.is_free()
                                && !m.is_retiring(now)
                                && (m.client.in_flight() == 0
                                    || m.client.waiting_for(now) <= STALLED)
                        })
                        .min_by_key(|(_, m)| (m.client.in_flight() > 0, !m.draining))
                        .map(|(i, _)| i);
                    let decision = if let Some(i) = free {
                        let m = st.shared.swap_remove(i);
                        st.taken += 1;
                        let Ok(client) = Rc::try_unwrap(m.client) else {
                            unreachable!("checked: nobody else holds it")
                        };
                        Some(Decision::Ready((client, m.expires)))
                    } else if st.room(max) {
                        st.connecting += 1;
                        Some(Decision::Connect)
                    } else {
                        None
                    };
                    if let Some(decision) = decision {
                        if let Some(id) = ticket.id.take() {
                            st.remove_ticket(id);
                        }
                        st.rebalance(max);
                        // The next in the queue may find a connection too.
                        st.wake_gets();
                        return decision;
                    }
                }
                if mine.is_none() {
                    let id = st.next_ticket;
                    st.next_ticket += 1;
                    st.queue.push_back(id);
                    ticket.id.set(Some(id));
                }
                st.rebalance(max);
                Decision::Wait
            })
            .await?;
        let (client, expires) = match taken {
            Ok(taken) => taken,
            Err(new) => {
                self.inner.state.borrow_mut().taken += 1;
                (new, self.expiry())
            }
        };
        client.set_shared(false);
        client.begin_lease();
        Ok(PooledClient {
            requests: client.requests(),
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
        let jitter = life / 10 * u32::try_from(seed % 1000).unwrap_or(0) / 1000;
        Instant::now() + (life - jitter)
    }

    /// The least busy shared connection, or a new one if every open one is
    /// busy and there is room.
    ///
    /// A connection whose oldest statement has been running for more than
    /// [`STALLED`] is passed over while another will do: statements queue
    /// behind it in order, so a slow one would hold up everything sent after
    /// it on that connection. Connections kept for a task waiting in `get`
    /// and connections about to be closed are not used.
    async fn shared(&self) -> Result<Lent<'_>, Error> {
        let max = self.inner.max_size;
        let client = self
            .acquire(Waiter::Shared, |st, now| {
                let room = st.room(max);
                // The tasks holding the connection count, not only the
                // requests sent: tasks waiting for a statement to be
                // prepared have sent nothing yet. Abandoned requests count
                // too, though nobody holds them.
                let usable = |m: &Member| {
                    let users = Rc::strong_count(&m.client) - 1;
                    !m.draining
                        && !m.is_retiring(now)
                        && users.max(m.client.in_flight()) < MAX_PIPELINE
                };
                // Busy means used by other tasks right now.
                let best = st
                    .shared
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| usable(m) && m.client.waiting_for(now) <= STALLED)
                    .min_by_key(|(_, m)| Rc::strong_count(&m.client))
                    .map(|(i, m)| (i, m.is_free()));
                let pick = match best {
                    Some((i, free)) if free || !room => Some(i),
                    _ if room => {
                        st.connecting += 1;
                        return Decision::Connect;
                    }
                    // Every usable connection is stalled and the pool is
                    // full: the one whose oldest statement has waited least,
                    // which is the one most likely to be moving.
                    _ => st
                        .shared
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| usable(m))
                        .min_by_key(|(_, m)| m.client.waiting_for(now))
                        .map(|(i, _)| i),
                };
                match pick {
                    Some(i) => {
                        let m = &mut st.shared[i];
                        m.used = now;
                        Decision::Ready(m.client.clone())
                    }
                    None => Decision::Wait,
                }
            })
            .await?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
                new.set_shared(true);
                let c = Rc::new(new);
                let member = Member {
                    client: c.clone(),
                    draining: false,
                    used: Instant::now(),
                    expires: self.expiry(),
                };
                let mut st = self.inner.state.borrow_mut();
                st.shared.push(member);
                // Room for other waiting statements too.
                st.wake_all();
                drop(st);
                self.flush_wakes();
                c
            }
        };
        Ok(Lent {
            pool: self,
            client: Some(client),
        })
    }

    /// Wakes the tasks the state says to wake. Called with the state not
    /// borrowed.
    fn flush_wakes(&self) {
        let wakers = std::mem::take(&mut self.inner.state.borrow_mut().to_wake);
        for w in wakers {
            w.wake();
        }
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

    /// Runs `decide` until it hands out a connection (`Ok`), or a new one is
    /// opened for the caller (`Err(client)`), or the wait times out.
    async fn acquire<T>(
        &self,
        kind: Waiter,
        mut decide: impl FnMut(&mut State, Instant) -> Decision<T>,
    ) -> Result<Result<T, Client>, Error> {
        let deadline = Instant::now() + self.inner.wait_timeout;
        let mut waiting: Option<Waiting<'_>> = None;
        loop {
            let decision = {
                let mut st = self.inner.state.borrow_mut();
                let now = Instant::now();
                st.sweep(now, self.inner.idle_timeout);
                let d = decide(&mut st, now);
                if matches!(d, Decision::Wait) && waiting.is_none() {
                    st.waiting += 1;
                }
                d
            };
            self.flush_wakes();
            match decision {
                Decision::Ready(c) => {
                    if let Some(w) = &mut waiting {
                        w.served = true;
                    }
                    return Ok(Ok(c));
                }
                Decision::Connect => {
                    if let Some(w) = &mut waiting {
                        w.served = true;
                    }
                    drop(waiting);
                    let _connecting = Connecting(self);
                    let result = Client::connect(&self.inner.config).await;
                    let mut st = self.inner.state.borrow_mut();
                    return match result {
                        Ok(client) => {
                            st.opened += 1;
                            self.start_reaper(&mut st);
                            Ok(Err(client))
                        }
                        Err(e) => {
                            st.connect_errors += 1;
                            Err(e)
                        }
                    };
                }
                Decision::Wait => {
                    if waiting.is_none() {
                        waiting = Some(Waiting {
                            pool: self,
                            kind,
                            served: false,
                        });
                    }
                    let mut registered = false;
                    let woken = poll_fn(|cx| {
                        if registered {
                            return Poll::Ready(());
                        }
                        registered = true;
                        let mut st = self.inner.state.borrow_mut();
                        match kind {
                            Waiter::Shared => st.shared_waiters.push_back(cx.waker().clone()),
                            Waiter::Get => st.get_waiters.push(cx.waker().clone()),
                        }
                        // A connection kept busy only by requests whose tasks
                        // stopped waiting (a timeout dropped them) frees up
                        // once they are answered, which nothing else would
                        // announce.
                        for m in &st.shared {
                            if m.is_free() && m.client.in_flight() > 0 {
                                m.client.wake_when_idle(cx.waker());
                            }
                        }
                        Poll::Pending
                    });
                    let left = deadline.saturating_duration_since(Instant::now());
                    if giong::time::timeout(left, woken).await.is_err() {
                        let mut st = self.inner.state.borrow_mut();
                        st.timeouts += 1;
                        let (taken, max) = (st.taken, self.inner.max_size);
                        drop(st);
                        let why = if taken == max {
                            format!("all {max} are taken with Pool::get")
                        } else {
                            format!("all {max} are taken or busy ({taken} taken with Pool::get)")
                        };
                        return Err(Error::new(
                            ErrorKind::PoolTimeout,
                            format!(
                                "no connection to {} became free within {:?}: {why}. Raise the \
                                 pool's max_size (the server's max_connections allowing), look \
                                 for transactions or connections from Pool::get held too long, \
                                 or raise Pool::wait_timeout",
                                self.inner.config.target(),
                                self.inner.wait_timeout,
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
        let wakers = {
            let mut st = state.borrow_mut();
            st.sweep(Instant::now(), idle_timeout);
            std::mem::take(&mut st.to_wake)
        };
        for w in wakers {
            w.wake();
        }
    }
}

/// A shared connection lent to one statement. Dropping it, when the
/// statement is answered or its future is dropped, tells waiting tasks to
/// look again: there is room for one more statement, and the connection may
/// have become free.
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
        let client = self.client.take().expect("present until dropped");
        // The pool's reference and this one.
        let free = Rc::strong_count(&client) == 2;
        drop(client);
        let mut st = self.pool.inner.state.borrow_mut();
        st.wake_one_shared();
        if free {
            if st
                .shared
                .iter()
                .any(|m| m.is_free() && m.client.should_retire())
            {
                st.sweep(Instant::now(), self.pool.inner.idle_timeout);
            }
            st.wake_gets();
        }
        drop(st);
        self.pool.flush_wakes();
    }
}

/// Refuses, before anything is sent, a statement that must not run on a
/// shared connection: one that controls a transaction (unless in a `batch`),
/// changes the session, or starts `COPY ... FROM STDIN`. It is on the path of
/// every shared statement, so it reads no further than the first word or
/// three that decide.
fn refuse_on_shared(sql: &str, batch: bool) -> Result<(), Error> {
    // Through the extended protocol the server refuses more than one
    // statement, so only a batch needs looking at past its first.
    let mut statements = Statements {
        sql,
        i: 0,
        first_only: !batch,
    };
    while let Some(words) = statements.next() {
        let word = |i: usize| words.get(i);
        let is = |i: usize, any: &[&str]| any.iter().any(|w| word(i).eq_ignore_ascii_case(w));
        let transaction = match words.lead {
            Lead::Transaction => true,
            // Two-phase commit's second phase may run on any connection.
            Lead::CommitOrRollback => !is(1, &["PREPARED"]),
            Lead::Prepare => is(1, &["TRANSACTION"]),
            _ => false,
        };
        if transaction && !batch {
            return Err(Error::new(
                ErrorKind::Statement,
                format!(
                    "{} controls a transaction, and the pool's own methods run statements on \
                     connections that other tasks share, whose statements would run inside it; \
                     nothing was sent. Run the transaction on a connection of this task's own: \
                     let mut conn = pool.get().await?; let tx = conn.transaction().await?; ...; \
                     tx.commit().await?",
                    word(0).to_ascii_uppercase()
                ),
            ));
        }
        if transaction {
            continue;
        }
        let session = match words.lead {
            // SET LOCAL and the transaction's own settings end with it.
            Lead::Set => !is(1, &["LOCAL", "TRANSACTION", "CONSTRAINTS"]),
            Lead::Create => {
                is(1, &["TEMP", "TEMPORARY"])
                    || (is(1, &["LOCAL", "GLOBAL"]) && is(2, &["TEMP", "TEMPORARY"]))
            }
            Lead::Prepare | Lead::Session => true,
            _ => false,
        };
        if session {
            let what = match words.lead {
                Lead::Create => "CREATE TEMP".to_owned(),
                Lead::Set => format!("SET {}", word(1).to_ascii_uppercase()),
                _ => word(0).to_ascii_uppercase(),
            };
            return Err(Error::new(
                ErrorKind::Statement,
                format!(
                    "{what} changes the session, and the pool's own methods run statements on \
                     connections that other tasks share, which would then run under it; nothing \
                     was sent. Run it on a connection of this task's own, from pool.get(): its \
                     session is reset when it goes back to the pool. For a setting that every \
                     connection needs, use Config::param"
                ),
            ));
        }
        if words.lead == Lead::Copy && words.from_stdin {
            return Err(Error::new(
                ErrorKind::Statement,
                "COPY ... FROM STDIN is not supported by giong-postgres yet, and on a connection \
                 that other tasks share it would fail their statements too; nothing was sent. \
                 Insert the rows with INSERT, several per statement",
            ));
        }
    }
    Ok(())
}

/// What the first word of a statement says about it, for
/// [`refuse_on_shared`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Lead {
    /// Anything else (`SELECT`, `INSERT`, `WITH`...): the rest of the
    /// statement does not matter.
    #[default]
    Other,
    /// `BEGIN`, `START`, `END`, `ABORT`, `SAVEPOINT`, `RELEASE`.
    Transaction,
    /// `COMMIT` or `ROLLBACK`, unless `PREPARED` follows.
    CommitOrRollback,
    /// `PREPARE`, or `PREPARE TRANSACTION`.
    Prepare,
    /// `SET`, unless `LOCAL`, `TRANSACTION` or `CONSTRAINTS` follows.
    Set,
    /// `CREATE`, which matters when a temporary object follows.
    Create,
    /// `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `DEALLOCATE`, `DECLARE`,
    /// `LOAD`.
    Session,
    /// `COPY`, which matters when `FROM STDIN` appears in it.
    Copy,
}

impl Lead {
    fn of(word: &str) -> Lead {
        let mut upper = [0u8; 10];
        let Some(upper) = upper.get_mut(..word.len()) else {
            return Lead::Other;
        };
        for (u, b) in upper.iter_mut().zip(word.bytes()) {
            *u = b.to_ascii_uppercase();
        }
        match &*upper {
            b"BEGIN" | b"START" | b"END" | b"ABORT" | b"SAVEPOINT" | b"RELEASE" => {
                Lead::Transaction
            }
            b"COMMIT" | b"ROLLBACK" => Lead::CommitOrRollback,
            b"PREPARE" => Lead::Prepare,
            b"SET" => Lead::Set,
            b"CREATE" => Lead::Create,
            b"RESET" | b"DISCARD" | b"LISTEN" | b"UNLISTEN" | b"DEALLOCATE" | b"DECLARE"
            | b"LOAD" => Lead::Session,
            b"COPY" => Lead::Copy,
            _ => Lead::Other,
        }
    }

    /// How many words decide the statement (the whole of it for `COPY`).
    fn words(self) -> usize {
        match self {
            Lead::Other | Lead::Transaction | Lead::Session => 1,
            Lead::CommitOrRollback | Lead::Prepare | Lead::Set => 2,
            Lead::Create => 3,
            Lead::Copy => usize::MAX,
        }
    }
}

/// The first words of one statement, as they are written, and whether
/// `FROM STDIN` appears in it. A quoted identifier counts as the word `"`.
#[derive(Default)]
struct Words<'a> {
    lead: Lead,
    first: [&'a str; 3],
    len: usize,
    from_stdin: bool,
    after_from: bool,
}

impl<'a> Words<'a> {
    /// Word `i`, or `""` past the last one kept.
    fn get(&self, i: usize) -> &'a str {
        if i < self.len {
            self.first[i]
        } else {
            ""
        }
    }

    fn push(&mut self, word: &'a str) {
        if self.len == 0 {
            self.lead = Lead::of(word);
        }
        if self.len < self.first.len() {
            self.first[self.len] = word;
            self.len += 1;
        }
        if self.lead == Lead::Copy {
            self.from_stdin |= self.after_from && word.eq_ignore_ascii_case("STDIN");
            self.after_from = word.eq_ignore_ascii_case("FROM");
        }
    }

    /// Whether the rest of the statement can change nothing that
    /// [`refuse_on_shared`] decides.
    fn settled(&self) -> bool {
        self.len > 0 && self.len >= self.lead.words()
    }
}

/// The statements of `sql`, split at `;` outside comments, string
/// constants, quoted identifiers and dollar-quoted bodies; empty ones are
/// skipped. With `first_only`, only the first statement, and only as far as
/// it needs reading.
struct Statements<'a> {
    sql: &'a str,
    i: usize,
    first_only: bool,
}

impl<'a> Statements<'a> {
    fn next(&mut self) -> Option<Words<'a>> {
        let (sql, b) = (self.sql, self.sql.as_bytes());
        let mut i = self.i;
        let mut words = Words::default();
        while i < b.len() {
            if self.first_only && words.settled() {
                i = b.len();
                break;
            }
            match b[i] {
                b';' => {
                    i += 1;
                    if words.len > 0 {
                        break;
                    }
                }
                b'-' if b.get(i + 1) == Some(&b'-') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    // PostgreSQL's block comments nest.
                    let mut depth = 0;
                    while i < b.len() {
                        if b[i..].starts_with(b"/*") {
                            depth += 1;
                            i += 2;
                        } else if b[i..].starts_with(b"*/") {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                }
                q @ (b'\'' | b'"') => {
                    i = skip_quoted(b, i + 1, q, false);
                    if q == b'"' {
                        words.push("\"");
                    }
                }
                b'$' => match dollar_tag(b, i) {
                    Some(tag_len) => {
                        let tag = &b[i..i + tag_len];
                        i += tag_len;
                        while i < b.len() && !b[i..].starts_with(tag) {
                            i += 1;
                        }
                        i = (i + tag_len).min(b.len());
                    }
                    // A parameter, $1.
                    None => i += 1,
                },
                c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                    let start = i;
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric()
                            || matches!(b[i], b'_' | b'$')
                            || b[i] >= 0x80)
                    {
                        i += 1;
                    }
                    let word = &sql[start..i];
                    // E'...' takes backslash escapes; U&'...', B'...', X'...'
                    // and N'...' are ordinary string constants after their
                    // prefix.
                    if b.get(i) == Some(&b'\'') {
                        let escapes = word.eq_ignore_ascii_case("e");
                        if escapes || ["b", "x", "n"].iter().any(|p| word.eq_ignore_ascii_case(p)) {
                            i = skip_quoted(b, i + 1, b'\'', escapes);
                            continue;
                        }
                    }
                    if word.eq_ignore_ascii_case("u") && b[i..].starts_with(b"&'") {
                        i = skip_quoted(b, i + 2, b'\'', false);
                        continue;
                    }
                    words.push(word);
                }
                _ => i += 1,
            }
        }
        self.i = if self.first_only { b.len() } else { i };
        (words.len > 0).then_some(words)
    }
}

/// The position after the quote that closes the one before `i`; a doubled
/// quote stands for itself, and with `escapes` so does a backslash and the
/// byte after it.
fn skip_quoted(b: &[u8], mut i: usize, quote: u8, escapes: bool) -> usize {
    while i < b.len() {
        if escapes && b[i] == b'\\' {
            i += 2;
        } else if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// The length of the dollar-quote tag at `i` (`$$` or `$name$`), if one
/// starts there.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1 - i)
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

/// A connection taken from a [`Pool`] with [`Pool::get`]. It dereferences to
/// the [`Client`], and goes back to the pool when dropped, where its session
/// is reset (see [`Pool`]).
pub struct PooledClient {
    client: Option<Client>,
    pool: Pool,
    /// When the connection is to be closed rather than go back.
    expires: Instant,
    /// The connection's request count when it was taken.
    requests: u64,
}

impl PooledClient {
    /// Starts a transaction; see [`Client::transaction`].
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self.deref_mut().transaction().await
    }
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
        client.end_lease();
        let now = Instant::now();
        let mut st = self.pool.inner.state.borrow_mut();
        st.taken -= 1;
        let retire = now >= self.expires || client.should_retire();
        if retire && !client.is_closed() {
            // Closing rolls back whatever was left open.
            st.recycled += 1;
        }
        if !client.is_closed() && !retire {
            let used = client.requests() != self.requests;
            // A statement still on its way (its future was dropped, by a
            // timeout say) may be a BEGIN: the last answer cannot tell.
            // Queued ahead of anything a shared statement sends next.
            if client.in_flight() > 0 || client.in_transaction() {
                client.send_and_forget("ROLLBACK");
            }
            if used {
                client.reset_session();
            }
            client.set_shared(true);
            st.shared.push(Member {
                client: Rc::new(client),
                draining: false,
                used: now,
                expires: self.expires,
            });
            st.rebalance(self.pool.inner.max_size);
        }
        st.wake_all();
        drop(st);
        self.pool.flush_wakes();
    }
}

impl std::fmt::Debug for PooledClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PooledClient").field(&**self).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> bool {
        refuse_on_shared(sql, false).is_err()
    }

    fn refused_in_batch(sql: &str) -> bool {
        refuse_on_shared(sql, true).is_err()
    }

    #[test]
    fn transaction_statements_are_refused_outside_batches() {
        for sql in [
            "BEGIN",
            " begin isolation level serializable",
            "/* x /* nested */ */ COMMIT",
            "-- c\nROLLBACK",
            "start transaction",
            "PREPARE TRANSACTION 'x'",
            "savepoint a",
        ] {
            assert!(refused(sql), "{sql}");
            assert!(!refused_in_batch(sql), "{sql}");
        }
        for sql in ["COMMIT PREPARED 'x'", "ROLLBACK PREPARED 'x'", "SELECT 1"] {
            assert!(!refused(sql), "{sql}");
        }
    }

    #[test]
    fn session_changes_are_refused_everywhere() {
        for sql in [
            "SET search_path = tenant_a",
            "set session authorization app",
            "SET ROLE tenant",
            "SET statement_timeout = 0",
            "RESET ALL",
            "DISCARD ALL",
            "LISTEN jobs",
            "UNLISTEN *",
            "PREPARE q AS SELECT 1",
            "DEALLOCATE ALL",
            "DECLARE c CURSOR WITH HOLD FOR SELECT 1",
            "LOAD 'auto_explain'",
            "CREATE TEMP TABLE t (a int)",
            "create temporary table t (a int)",
            "CREATE LOCAL TEMPORARY TABLE t (a int)",
            "COPY t FROM STDIN",
            "copy t (a, b) from stdin with (format csv)",
            "BEGIN; SET work_mem = '1GB'; COMMIT",
        ] {
            assert!(refused(sql), "{sql}");
            assert!(refused_in_batch(sql), "{sql}");
        }
        // Statements after the first matter in a batch only: through the
        // extended protocol, the server refuses more than one.
        assert!(refused_in_batch("SELECT 1; SET search_path = x"));
        assert!(!refused("SELECT 1; SET search_path = x"));
        for sql in [
            "SET LOCAL work_mem = '1GB'",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET CONSTRAINTS ALL DEFERRED",
            "CREATE TABLE t (a int)",
            "COPY t TO STDOUT",
            "COPY t FROM '/tmp/x'",
            "SELECT 'SET x = 1; LISTEN y'",
            "SELECT E'it\\'s; SET x = 1'",
            "SELECT $$; SET x = 1$$, $f$ ; LISTEN y $f$",
            "SELECT \"set\" FROM t",
            "SELECT $1::text; SELECT 2",
            "SELECT 1 -- ; SET x = 1",
            "SELECT /* ; SET x = 1 */ 1",
            "UPDATE t SET a = 1",
            "INSERT INTO t SELECT 'from stdin'",
        ] {
            assert!(!refused_in_batch(sql), "{sql}");
        }
        assert!(refused_in_batch("BEGIN; SET LOCAL a = 1; COMMIT; LISTEN x"));
    }

    #[test]
    fn statements_are_split_outside_quotes() {
        let all = |sql| {
            let mut s = Statements {
                sql,
                i: 0,
                first_only: false,
            };
            let mut all = Vec::new();
            while let Some(w) = s.next() {
                all.push((0..w.len).map(|i| w.get(i).to_owned()).collect::<Vec<_>>());
            }
            all
        };
        let s = all("SELECT 'a;b'; SELECT \"x;y\" ; ; /* ; */ -- ;\n VALUES (1)");
        assert_eq!(s.len(), 3);
        assert_eq!(s[1], ["SELECT", "\""]);
        assert_eq!(s[2], ["VALUES"]);
        // An unterminated quote swallows the rest, as the server would.
        assert_eq!(all("SELECT 'a; SET x = 1").len(), 1);
        assert_eq!(all("SELECT $q$ a; SET x $q").len(), 1);
        assert_eq!(all("SELECT a$b; SET x = 1")[1], ["SET", "x"]);
        // Only the first three words are kept, and FROM STDIN is seen anywhere.
        let mut s = Statements {
            sql: "copy t (a, b, c, d) from stdin; select 1",
            i: 0,
            first_only: true,
        };
        let w = s.next().unwrap();
        assert_eq!((w.lead, w.len, w.from_stdin), (Lead::Copy, 3, true));
        assert!(s.next().is_none());
    }
}
