//! A pool of connections for one worker thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use giong::time::Instant;

use crate::client::{Client, Counters, Transaction};
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::row::Row;
use crate::session;
use crate::types::ToSql;

/// How long a connection's oldest statement may have been running before
/// shared statements avoid that connection.
const STALLED: Duration = Duration::from_millis(5);

/// Connections to one server, for the tasks of one worker thread.
///
/// Make one pool per worker, inside [`giong::Runtime::run`]: nothing is
/// shared between threads, so nothing is locked.
///
/// ```no_run
/// use giong_mysql::{Config, Pool};
///
/// fn main() -> std::io::Result<()> {
///     let config = Config::from_url("mysql://app:secret@db.internal/shop")?;
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
/// migrations) on a connection of their own, from [`get`](Self::get).
///
/// **How many connections.** Because statements pipeline, a few connections
/// keep the server busy: start with the server's cores, shared among the
/// workers' pools. More than that adds no throughput and raises tail latency,
/// since a server thread waiting for a core holds up every statement queued
/// on its connection. Statements that wait on disk or on locks need more.
///
/// [`get`](Self::get) takes a connection for one task alone, for a
/// transaction, for statements known to be slow, or for statements that
/// need the session (`SET`, `LOCK TABLES`, temporary tables). While it is
/// out, no shared statement runs on it. A task waiting in `get` is not
/// starved by shared statements: the pool sets a connection aside for it
/// and hands it over as soon as the statements already on it are answered.
/// Waiting tasks are served in order.
///
/// **A connection comes back clean.** When the [`PooledClient`] is dropped,
/// a connection whose session may have changed is reset before anything
/// else runs on it: `COM_RESET_CONNECTION` (which rolls back, releases table
/// and named locks, drops temporary tables, forgets variables), then the
/// session settings of [`Config`] again, both queued at once with no round
/// trip waited for. The reset also frees the connection's prepared
/// statements, so it is skipped when the connection ran only plain
/// statements (`SELECT`, `INSERT`, `UPDATE`, `DELETE` ... with no user
/// variable or named lock) in transactions or on their own, and the server
/// reported no change: a transaction of plain statements keeps the
/// statement cache. A stored function or trigger that takes a named lock
/// or sets a user variable with `SELECT ... INTO` is not reported by the
/// server; call [`Client::reset_session`] before dropping such a
/// connection.
///
/// **Transactions and session changes never run on the shared
/// connections**, since the statements of other tasks would run inside them.
/// `query`, `query_one`, `query_opt`, `execute` and `insert` refuse `START
/// TRANSACTION`, `COMMIT`, `ROLLBACK`, `LOCK TABLES`, `SET` (but `SET
/// GLOBAL`), `USE`, temporary tables, `GET_LOCK` and assignments to user
/// variables before sending anything ([`ErrorKind::Statement`]).
/// [`batch_execute`](Self::batch_execute) may carry a whole transaction
/// (`START TRANSACTION; ...; COMMIT`); if a batch leaves one open, or
/// autocommit off, its connection is closed, which rolls it back, and the
/// batch and every statement sent on that connection after it fail with
/// [`ErrorKind::Closed`]. A batch that may change the session is followed
/// by a reset at once, and so is any statement the server reports changed
/// it (through a trigger or a stored procedure).
///
/// **Prepared statements.** Each connection keeps up to
/// [`Config::statement_cache_capacity`] statements prepared on the server,
/// and the server's `max_prepared_stmt_count` (16,382 by default) bounds
/// the total of all its clients: processes × workers × `max_size` ×
/// capacity must stay below it.
///
/// Connections the server closed, or that broke, are left behind as soon as
/// that is noticed, which is when it happens: each connection is read
/// continuously.
#[derive(Clone)]
pub struct Pool {
    inner: Rc<Inner>,
}

struct Inner {
    config: Config,
    max_size: usize,
    wait_timeout: Duration,
    state: Rc<RefCell<State>>,
    /// The pool as `giong::observe` sees it; registered by a weak reference,
    /// so it leaves snapshots when the pool is dropped.
    _observed: Rc<Observed>,
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
        "mysql pool"
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
            ("draining", n(s.draining)),
            ("resets", s.resets),
            ("statement_limit_errors", s.statement_limit_errors),
        ]
    }
}

fn stats_of(st: &State) -> PoolStats {
    PoolStats {
        connections: st.shared.len() + st.draining.len() + st.taken,
        taken: st.taken,
        connecting: st.connecting,
        in_flight: st
            .shared
            .iter()
            .chain(&st.draining)
            .map(|c| c.in_flight())
            .sum(),
        waiting: st.waiting,
        opened: st.opened,
        connect_errors: st.connect_errors,
        timeouts: st.timeouts,
        draining: st.draining.len(),
        resets: st.counters.resets.get(),
        statement_limit_errors: st.counters.statement_limit_errors.get(),
    }
}

#[derive(Default)]
struct State {
    /// Open connections that are not taken, and that shared statements
    /// use.
    shared: Vec<Rc<Client>>,
    /// Open connections set aside for tasks waiting in [`Pool::get`]: no
    /// new shared statement is sent on them, and the oldest waiting task
    /// takes one as soon as the statements on it are answered.
    draining: Vec<Rc<Client>>,
    taken: usize,
    connecting: usize,
    waiting: usize,
    waiters: Vec<Waker>,
    /// The tasks waiting in `get`, oldest first.
    tickets: VecDeque<u64>,
    next_ticket: u64,
    opened: u64,
    connect_errors: u64,
    timeouts: u64,
    counters: Rc<Counters>,
}

impl State {
    fn open(&self) -> usize {
        self.shared.len() + self.draining.len() + self.taken + self.connecting
    }

    /// Sets aside one connection for each task waiting in `get`, keeping one
    /// for shared statements when the pool has room for more than one.
    fn rebalance(&mut self, max: usize) {
        let open = self.shared.len() + self.draining.len();
        let room = if max == 1 {
            open
        } else {
            open.saturating_sub(1)
        };
        let want = self.tickets.len().min(room);
        while self.draining.len() < want {
            // The one that frees up first: the fewest statements in flight.
            let Some((i, _)) = self
                .shared
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| (c.in_flight(), Rc::strong_count(c)))
            else {
                break;
            };
            let c = self.shared.swap_remove(i);
            self.draining.push(c);
        }
        while self.draining.len() > want {
            let c = self.draining.pop().expect("more than wanted");
            self.shared.push(c);
        }
    }
}

/// Free for a task of its own: nobody borrows it and nothing is in flight.
fn idle(c: &Rc<Client>) -> bool {
    Rc::strong_count(c) == 1 && c.in_flight() == 0
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
    /// Connections set aside for tasks waiting in [`Pool::get`].
    pub draining: usize,
    /// Sessions reset when a connection taken with [`Pool::get`] came back,
    /// or after a shared statement changed one.
    pub resets: u64,
    /// Prepares the server refused because its `max_prepared_stmt_count`
    /// was reached (error 1461), each retried once after closing unused
    /// statements.
    pub statement_limit_errors: u64,
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

/// A task's place in the line for [`Pool::get`]: given up however the wait
/// ends, so that no connection stays set aside for nobody.
struct Ticket<'a> {
    pool: &'a Pool,
    number: Cell<Option<u64>>,
}

impl Ticket<'_> {
    /// Leaves the line (the caller holds the state).
    fn leave(&self, st: &mut State) {
        if let Some(n) = self.number.take() {
            st.tickets.retain(|&t| t != n);
            st.rebalance(self.pool.inner.max_size);
        }
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.number.get().is_some() {
            self.leave(&mut self.pool.inner.state.borrow_mut());
            self.pool.notify();
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
        let state: Rc<RefCell<State>> = Rc::new(RefCell::default());
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
                state,
                _observed: observed,
            }),
        }
    }

    /// The longest a task waits for a connection when all `max_size` are
    /// taken: 5 seconds by default. A longer wait fails with
    /// [`ErrorKind::PoolTimeout`].
    ///
    /// # Panics
    ///
    /// Panics if other clones of this pool exist; set it right after
    /// [`new`](Self::new).
    pub fn wait_timeout(mut self, timeout: Duration) -> Self {
        Rc::get_mut(&mut self.inner)
            .expect("set the wait timeout before cloning the pool")
            .wait_timeout = timeout;
        self
    }

    /// Runs a statement on a shared connection and returns its rows; see
    /// [`Client::query`]. Transaction statements and statements that change
    /// the session are refused ([`ErrorKind::Statement`]): run them on a
    /// connection from [`get`](Self::get).
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        refuse_on_shared(sql)?;
        let client = self.shared().await?;
        client.query(sql, params).await
    }

    /// Runs a statement that returns exactly one row; see
    /// [`Client::query_one`]. Transaction statements are refused.
    pub async fn query_one(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Row, Error> {
        refuse_on_shared(sql)?;
        let client = self.shared().await?;
        client.query_one(sql, params).await
    }

    /// Runs a statement that returns at most one row; see
    /// [`Client::query_opt`]. Transaction statements are refused.
    pub async fn query_opt(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<Row>, Error> {
        refuse_on_shared(sql)?;
        let client = self.shared().await?;
        client.query_opt(sql, params).await
    }

    /// Runs a statement and returns the number of rows it changed; see
    /// [`Client::execute`]. Transaction statements are refused.
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        refuse_on_shared(sql)?;
        let client = self.shared().await?;
        client.execute(sql, params).await
    }

    /// Runs an `INSERT` and returns the `AUTO_INCREMENT` value it generated;
    /// see [`Client::insert`]. Transaction statements are refused.
    pub async fn insert(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        refuse_on_shared(sql)?;
        let client = self.shared().await?;
        client.insert(sql, params).await
    }

    /// Runs several statements without parameters; see
    /// [`Client::batch_execute`].
    ///
    /// The batch may hold a whole transaction, `START TRANSACTION; ...;
    /// COMMIT`. If it leaves one open, or autocommit off, the connection is
    /// closed, which rolls the transaction back, and the batch fails with
    /// [`ErrorKind::Closed`], as do the statements other tasks sent on that
    /// connection after it: they would otherwise have run inside the
    /// transaction.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        let client = self.shared().await?;
        client.batch_execute(sql).await
    }

    /// Takes a connection for this task alone, for a transaction, session
    /// settings or a slow statement:
    ///
    /// ```no_run
    /// # async fn f(pool: &giong_mysql::Pool) -> Result<(), giong_mysql::Error> {
    /// let mut conn = pool.get().await?;
    /// let tx = conn.transaction().await?;
    /// tx.execute("INSERT INTO audit (what) VALUES (?)", &[&"signup"]).await?;
    /// tx.commit().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// When the pool is full and no connection is free, the task waits, in
    /// order with the other tasks waiting here, up to the [wait
    /// timeout](Self::wait_timeout): a connection is set aside for it (no new
    /// shared statement goes on it) and handed over once the statements
    /// already on it are answered, so shared statements cannot keep it
    /// waiting. When the pool has room for more than one connection, one is
    /// always left to the shared statements.
    ///
    /// The connection goes back when the [`PooledClient`] is dropped, and its
    /// session is reset first if it may have changed (see the type's
    /// documentation).
    pub async fn get(&self) -> Result<PooledClient, Error> {
        let ticket = Ticket {
            pool: self,
            number: Cell::new(None),
        };
        let client = self
            .acquire(|st, max| {
                let my_turn = match ticket.number.get() {
                    None => st.tickets.is_empty(),
                    Some(n) => st.tickets.front() == Some(&n),
                };
                if my_turn {
                    // A connection set aside for this task, else any free
                    // one.
                    let found = match st.draining.iter().position(idle) {
                        Some(i) => Some(st.draining.swap_remove(i)),
                        None => st
                            .shared
                            .iter()
                            .position(idle)
                            .map(|i| st.shared.swap_remove(i)),
                    };
                    if let Some(c) = found {
                        st.taken += 1;
                        ticket.leave(st);
                        return match Rc::try_unwrap(c) {
                            Ok(client) => {
                                client.set_shared(false);
                                Decision::Ready(client)
                            }
                            Err(_) => unreachable!("checked: nobody else holds it"),
                        };
                    }
                }
                if st.open() < max {
                    st.connecting += 1;
                    ticket.leave(st);
                    return Decision::Connect;
                }
                if ticket.number.get().is_none() {
                    let n = st.next_ticket;
                    st.next_ticket += 1;
                    st.tickets.push_back(n);
                    ticket.number.set(Some(n));
                }
                st.rebalance(max);
                Decision::Wait
            })
            .await;
        // Whoever is next in line may be able to go now.
        drop(ticket);
        let client = client?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
                new.count_into(self.inner.state.borrow().counters.clone());
                self.inner.state.borrow_mut().taken += 1;
                new
            }
        };
        Ok(PooledClient {
            client: Some(client),
            pool: self.clone(),
        })
    }

    /// The pool's current state and counters.
    pub fn stats(&self) -> PoolStats {
        stats_of(&self.inner.state.borrow())
    }

    /// The least busy shared connection, or a new one if every open one is
    /// busy and there is room.
    ///
    /// A connection whose oldest statement has been running for more than
    /// [`STALLED`] is passed over while another will do: statements queue
    /// behind it in order, so a slow one would hold up everything sent after
    /// it on that connection.
    async fn shared(&self) -> Result<Lent<'_>, Error> {
        let client = self
            .acquire(|st, max| {
                let now = Instant::now();
                let room = st.open() < max;
                // Busy means used by other tasks right now.
                let best = st
                    .shared
                    .iter()
                    .filter(|c| c.waiting_for(now) <= STALLED)
                    .min_by_key(|c| Rc::strong_count(c))
                    .cloned();
                match best {
                    Some(c) if Rc::strong_count(&c) == 2 || !room => Decision::Ready(c),
                    _ if room => {
                        st.connecting += 1;
                        Decision::Connect
                    }
                    // Every connection is stalled and the pool is full: the
                    // one whose oldest statement has waited least, which is
                    // the one most likely to be moving.
                    _ => match st.shared.iter().min_by_key(|c| c.waiting_for(now)) {
                        Some(c) => Decision::Ready(c.clone()),
                        None => Decision::Wait,
                    },
                }
            })
            .await?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
                new.set_shared(true);
                let mut st = self.inner.state.borrow_mut();
                new.count_into(st.counters.clone());
                let c = Rc::new(new);
                st.shared.push(c.clone());
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
        let deadline = Instant::now() + self.inner.wait_timeout;
        loop {
            let decision = {
                let mut st = self.inner.state.borrow_mut();
                st.shared.retain(|c| !c.is_closed());
                st.draining.retain(|c| !c.is_closed());
                decide(&mut st, self.inner.max_size)
            };
            match decision {
                Decision::Ready(c) => return Ok(Ok(c)),
                Decision::Connect => {
                    let _connecting = Connecting(self);
                    let result = Client::connect(&self.inner.config).await;
                    let mut st = self.inner.state.borrow_mut();
                    return match result {
                        Ok(client) => {
                            st.opened += 1;
                            Ok(Err(client))
                        }
                        Err(e) => {
                            st.connect_errors += 1;
                            Err(e)
                        }
                    };
                }
                Decision::Wait => {
                    self.inner.state.borrow_mut().waiting += 1;
                    let _waiting = Waiting(self);
                    let mut registered = false;
                    let woken = poll_fn(|cx| {
                        if registered {
                            return Poll::Ready(());
                        }
                        registered = true;
                        let mut st = self.inner.state.borrow_mut();
                        st.waiters.push(cx.waker().clone());
                        // A connection kept busy only by statements whose
                        // tasks stopped waiting (a timeout dropped them) is
                        // free once they are answered, which nothing else
                        // would announce.
                        for c in st.shared.iter().chain(&st.draining) {
                            if Rc::strong_count(c) == 1 && c.in_flight() > 0 {
                                c.wake_when_idle(cx.waker());
                            }
                        }
                        Poll::Pending
                    });
                    let left = deadline.saturating_duration_since(Instant::now());
                    if giong::time::timeout(left, woken).await.is_err() {
                        let mut st = self.inner.state.borrow_mut();
                        st.timeouts += 1;
                        return Err(Error::new(
                            ErrorKind::PoolTimeout,
                            format!(
                                "no connection to {} became free within {:?}: all {} are taken or \
                                 busy. Raise the pool's max_size (the server's max_connections \
                                 allowing), look for transactions held open too long, or raise \
                                 Pool::wait_timeout",
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

/// A shared connection lent to one statement. Dropping it, when the
/// statement is answered or its future is dropped, tells waiting tasks to
/// look again: the connection may have become free.
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

/// Refuses a statement that controls a transaction or changes the session,
/// which must not run on a connection that other tasks share.
fn refuse_on_shared(sql: &str) -> Result<(), Error> {
    let words = session::Words::of(sql);
    // Table locks belong to the session, and LOCK TABLES commits.
    let refused = words.is(
        0,
        &[
            "BEGIN",
            "START",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT",
            "RELEASE",
            "XA",
            "LOCK",
            "UNLOCK",
        ],
    );
    if refused {
        return Err(Error::new(
            ErrorKind::Statement,
            format!(
                "{} controls a transaction, and the pool's own methods run statements on \
                 connections that other tasks share, whose statements would run inside it; \
                 nothing was sent. Run the transaction on a connection of this task's own: \
                 let mut conn = pool.get().await?; let tx = conn.transaction().await?; ...; \
                 tx.commit().await?",
                words.get(0).to_ascii_uppercase()
            ),
        ));
    }
    if let Some(why) = session::changes_in(&words, sql) {
        return Err(Error::new(
            ErrorKind::Statement,
            format!(
                "{why}, and the pool's own methods run statements on connections that other \
                 tasks share: the change would reach their statements, and this task's next \
                 statement may run on another connection; nothing was sent. Run it on a \
                 connection of this task's own, let conn = pool.get().await?, whose session \
                 is reset when it goes back to the pool, or set session variables for every \
                 connection with Config::session_variable"
            ),
        ));
    }
    Ok(())
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
/// the [`Client`], and goes back to the pool when dropped.
pub struct PooledClient {
    client: Option<Client>,
    pool: Pool,
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
        {
            let mut st = self.pool.inner.state.borrow_mut();
            st.taken -= 1;
            // Queued ahead of anything sent on the connection next: the
            // reset rolls back, unlocks and forgets, and sets the session up
            // again. A reset that fails closes the connection.
            if !client.is_closed() && client.needs_reset() {
                client.queue_reset();
            }
            if !client.is_closed() {
                client.set_shared(true);
                st.shared.push(Rc::new(client));
                st.rebalance(self.pool.inner.max_size);
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
