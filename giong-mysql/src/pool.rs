//! A pool of connections for one worker thread.

use std::cell::RefCell;
use std::future::poll_fn;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
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
/// transaction or for session settings. While it is out, no shared statement
/// runs on it. It comes back when the [`PooledClient`] is dropped; if a
/// transaction was left open, or a statement is still on its way, a
/// `ROLLBACK` is queued first. One left with autocommit off is closed.
///
/// **Transactions never run on the shared connections**, since the
/// statements of other tasks would run inside them. `query`, `query_one`,
/// `query_opt`, `execute` and `insert` refuse `START TRANSACTION`, `COMMIT`,
/// `ROLLBACK`, `LOCK TABLES` and the like before sending anything
/// ([`ErrorKind::Statement`]). [`batch_execute`](Self::batch_execute) may
/// carry a whole transaction (`START TRANSACTION; ...; COMMIT`); if a batch
/// leaves one open, or autocommit off, its connection is closed, which rolls
/// it back, and the batch and every statement sent on that connection after
/// it fail with [`ErrorKind::Closed`].
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
        ]
    }
}

fn stats_of(st: &State) -> PoolStats {
    PoolStats {
        connections: st.shared.len() + st.taken,
        taken: st.taken,
        connecting: st.connecting,
        in_flight: st.shared.iter().map(|c| c.in_flight()).sum(),
        waiting: st.waiting,
        opened: st.opened,
        connect_errors: st.connect_errors,
        timeouts: st.timeouts,
    }
}

#[derive(Default)]
struct State {
    /// Open connections that are not taken.
    shared: Vec<Rc<Client>>,
    taken: usize,
    connecting: usize,
    waiting: usize,
    waiters: Vec<Waker>,
    opened: u64,
    connect_errors: u64,
    timeouts: u64,
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
    /// [`Client::query`]. Transaction statements are refused
    /// ([`ErrorKind::Statement`]): run them on a connection from
    /// [`get`](Self::get).
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        refuse_transaction(sql)?;
        let client = self.shared().await?;
        client.query(sql, params).await
    }

    /// Runs a statement that returns exactly one row; see
    /// [`Client::query_one`]. Transaction statements are refused.
    pub async fn query_one(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Row, Error> {
        refuse_transaction(sql)?;
        let client = self.shared().await?;
        client.query_one(sql, params).await
    }

    /// Runs a statement that returns at most one row; see
    /// [`Client::query_opt`]. Transaction statements are refused.
    pub async fn query_opt(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<Row>, Error> {
        refuse_transaction(sql)?;
        let client = self.shared().await?;
        client.query_opt(sql, params).await
    }

    /// Runs a statement and returns the number of rows it changed; see
    /// [`Client::execute`]. Transaction statements are refused.
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        refuse_transaction(sql)?;
        let client = self.shared().await?;
        client.execute(sql, params).await
    }

    /// Runs an `INSERT` and returns the `AUTO_INCREMENT` value it generated;
    /// see [`Client::insert`]. Transaction statements are refused.
    pub async fn insert(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        refuse_transaction(sql)?;
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

    /// Takes a connection for this task alone, for a transaction or session
    /// settings:
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
    /// Waits up to the [wait timeout](Self::wait_timeout) when every
    /// connection is taken or busy and the pool is full.
    pub async fn get(&self) -> Result<PooledClient, Error> {
        let client = self
            .acquire(|st, max| {
                let idle = st
                    .shared
                    .iter()
                    .position(|c| Rc::strong_count(c) == 1 && c.in_flight() == 0);
                if let Some(i) = idle {
                    let c = st.shared.swap_remove(i);
                    st.taken += 1;
                    return match Rc::try_unwrap(c) {
                        Ok(client) => {
                            client.set_shared(false);
                            Decision::Ready(client)
                        }
                        Err(_) => unreachable!("checked: nobody else holds it"),
                    };
                }
                if st.shared.len() + st.taken + st.connecting < max {
                    st.connecting += 1;
                    Decision::Connect
                } else {
                    Decision::Wait
                }
            })
            .await?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
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
                let room = st.shared.len() + st.taken + st.connecting < max;
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
                let c = Rc::new(new);
                self.inner.state.borrow_mut().shared.push(c.clone());
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
                        for c in &st.shared {
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

/// Refuses a statement that controls a transaction, which must not run on a
/// connection that other tasks share.
fn refuse_transaction(sql: &str) -> Result<(), Error> {
    let words = leading_words(sql);
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    let refused = match word(0) {
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "XA" => true,
        // Table locks belong to the session, and LOCK TABLES commits.
        "LOCK" | "UNLOCK" => true,
        _ => false,
    };
    if !refused {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Statement,
        format!(
            "{} controls a transaction, and the pool's own methods run statements on \
             connections that other tasks share, whose statements would run inside it; \
             nothing was sent. Run the transaction on a connection of this task's own: \
             let mut conn = pool.get().await?; let tx = conn.transaction().await?; ...; \
             tx.commit().await?",
            word(0)
        ),
    ))
}

/// The first two words of `sql`, upper-cased, after blanks and comments.
fn leading_words(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut words = Vec::new();
    while words.len() < 2 && i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            // `-- ` needs a blank or control after the dashes in MySQL.
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2)
                    .map_or(true, |c| c.is_ascii_whitespace() || c.is_ascii_control()) =>
            {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // MySQL's block comments do not nest.
                i = match sql[i + 2..].find("*/") {
                    Some(end) => i + 2 + end + 2,
                    None => b.len(),
                };
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                words.push(sql[start..i].to_ascii_uppercase());
            }
            _ => break,
        }
    }
    words
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
            // Autocommit off would put every shared statement after it in
            // one transaction: such a connection is closed instead.
            let idle = client.in_flight() == 0;
            if !client.is_closed() && (client.autocommit() || !idle) {
                // Queued ahead of anything a shared statement sends next.
                client.reset_transaction();
                client.set_shared(true);
                st.shared.push(Rc::new(client));
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
