//! A connection: requests are queued, written by one task and answered in
//! order by another, so that any number of tasks can use one connection at
//! once and their statements share round trips.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::net::Shutdown;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::config::Config;
use crate::connect::{self, Identity, Message, ReadBuf, Startup, Stream};
use crate::error::{DbError, Error, ErrorKind};
use crate::protocol::{self, com, status, ColumnDef, Reader};
use crate::row::{Column, ResultSet, Row, RowsBuilder};
use crate::session;
use crate::types::{ColumnType, ToSql};

/// The most parameters a statement can have.
const MAX_PARAMS: usize = 65_535;

/// The most columns a result can have: a prepared statement's are counted
/// in two bytes.
const MAX_COLUMNS: u64 = 65_535;

/// Buffers grown past this by one large message are given back once idle.
pub(crate) const KEEP_CAPACITY: usize = 1 << 20;

/// MySQL error numbers this crate reacts to.
mod code {
    /// `ER_UNKNOWN_STMT_HANDLER`: the statement is gone from the server.
    pub const UNKNOWN_STMT: u16 = 1243;
    /// `ER_NEED_REPREPARE`: the statement no longer fits the schema.
    pub const NEED_REPREPARE: u16 = 1615;
    /// `ER_LOCK_DEADLOCK`: the transaction was rolled back.
    pub const DEADLOCK: u16 = 1213;
    /// `ER_LOCK_WAIT_TIMEOUT`: the statement was rolled back, or the whole
    /// transaction with `innodb_rollback_on_timeout`.
    pub const LOCK_WAIT_TIMEOUT: u16 = 1205;
    /// `ER_LOCK_TABLE_FULL`: the transaction was rolled back.
    pub const LOCK_TABLE_FULL: u16 = 1206;
    /// `ER_MAX_PREPARED_STMT_COUNT_REACHED`: the server holds as many
    /// prepared statements as `max_prepared_stmt_count` allows, counting
    /// those of every client.
    pub const PREPARED_LIMIT: u16 = 1461;
    /// `ER_QUERY_TIMEOUT`: `max_execution_time` stopped a `SELECT`.
    pub const QUERY_TIMEOUT: u16 = 3024;
    /// `ER_STATEMENT_TIMEOUT` (MariaDB): `max_statement_time` stopped a
    /// statement.
    pub const STATEMENT_TIMEOUT: u16 = 1969;
}

/// A connection to a MySQL server.
///
/// Every method takes `&self`, so tasks can share a client (through an `Rc`)
/// and run statements at the same time: their requests go out together and
/// the server answers them in order, one round trip for all of them
/// (pipelining). A [`Transaction`] needs `&mut self`, so that no other task
/// can slip a statement into it.
///
/// Statements are prepared on the server the first time the connection runs
/// them and kept (see
/// [`Config::statement_cache_capacity`](crate::Config::statement_cache_capacity)),
/// so a statement that ran before takes a single round trip.
///
/// A client lives on the worker thread that opened it. Dropping it closes the
/// connection once the requests already sent have been written.
pub struct Client {
    conn: Rc<Conn>,
}

/// A prepared statement on one connection.
pub(crate) struct Statement {
    sql: Rc<str>,
    id: u32,
    params: usize,
    /// The columns of the last result, kept so that results with the same
    /// columns share them.
    columns: RefCell<Rc<[Column]>>,
    /// Set for a statement that is not cached: the connection, so that the
    /// statement can be closed when it is dropped.
    conn: Option<Weak<Conn>>,
    /// The session it was prepared in: a reset of the session frees it on
    /// the server, and its id may be given to another statement.
    generation: u64,
}

impl Drop for Statement {
    fn drop(&mut self) {
        let Some(conn) = self.conn.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        if conn.generation.get() != self.generation {
            return;
        }
        if let Ok(mut st) = conn.state.try_borrow_mut() {
            if st.broken.is_none() {
                // No answer comes for a close.
                protocol::command_stmt(&mut st.out, com::STMT_CLOSE, self.id);
                if let Some(w) = st.writer.take() {
                    w.wake();
                }
            }
        };
    }
}

/// Where an open [`Transaction`] stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tx {
    None,
    Open,
    /// A statement in it failed with an error that may or may not have
    /// rolled the whole transaction back: before anything else is sent, a
    /// ping asks the server whether it is still open.
    Uncertain,
    /// The server rolled it back before `COMMIT` (a deadlock, say).
    RolledBack,
    /// The server ended it before `COMMIT`, and cannot say how: a statement
    /// (such as DDL) committed it, or an error rolled it back.
    Ended,
    /// A statement in it was given up before its answer came (its future
    /// was dropped): it may have run, so committing would commit work the
    /// application believes undone.
    Abandoned,
}

/// Why a shared connection was closed under its statements.
const LEFT_OPEN: &str = "a statement sent through the pool's shared methods (Pool::batch_execute) \
     left a transaction open, or autocommit off, on a connection other tasks share, so the \
     connection was closed and the transaction rolled back, with every statement sent on it \
     since, this one included. Run transactions on a connection of the task's own: \
     pool.get().await?.transaction()";

/// What a pool counts on its connections.
#[derive(Default)]
pub(crate) struct Counters {
    /// Prepares refused for the server's `max_prepared_stmt_count`.
    pub statement_limit_errors: Cell<u64>,
    /// Sessions reset.
    pub resets: Cell<u64>,
}

/// The state shared by a client and its two tasks.
struct Conn {
    weak: Weak<Conn>,
    stream: Stream,
    state: RefCell<State>,
    /// When the oldest unanswered request became the oldest; `None` with
    /// nothing in flight.
    head_since: Cell<Option<giong::time::Instant>>,
    config: Config,
    connection_id: u32,
    server_version: String,
    /// The `SET` that sets the session up, run again after each reset.
    session_sql: Rc<str>,
    identity: Identity,
    rollback_on_timeout: bool,
    /// Counts resets of the session: statements prepared before one are gone
    /// from the server.
    generation: Cell<u64>,
    /// While the connection is one task's own: something it sent may have
    /// changed the session, which must then be reset before other tasks use
    /// the connection.
    dirty: Cell<bool>,
    /// Set once the connection has failed, for [`CancelToken`]s on other
    /// threads.
    closed: Arc<AtomicBool>,
    /// While a pool lends the connection to one task: the flag its cancel
    /// tokens check, cleared when the connection goes back.
    lease: RefCell<Option<Arc<Lease>>>,
    /// The default database changed since the session was last set up.
    schema_changed: Cell<bool>,
    /// Where the pool counts what happens on its connections.
    counters: RefCell<Option<Rc<Counters>>>,
    tx: Cell<Tx>,
    /// Several tasks send statements on this connection at once (it is among
    /// a pool's shared connections): none of them may leave a transaction
    /// open, or autocommit off, which would take in the statements of the
    /// others.
    shared: Cell<bool>,
    /// Woken when nothing is in flight any more, or the connection fails.
    idle_wakers: RefCell<Vec<Waker>>,
    /// Prepares take turns while the server is at its
    /// `max_prepared_stmt_count`.
    turn: Turn,
    /// When a prepare last succeeded in its turn.
    turn_progress: Cell<Option<giong::time::Instant>>,
}

/// How long prepares at the server's `max_prepared_stmt_count` keep trying
/// while none of them on the connection succeeds.
const LIMIT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

/// The longest pause between the first prepares tried at the limit.
const LIMIT_PAUSE: std::time::Duration = std::time::Duration::from_millis(20);

/// The longest pause once [`LIMIT_QUICK_TRIES`] have failed: the limit is
/// then held by other clients for a while, and a slower pace leaves the
/// server's time to them.
const LIMIT_PAUSE_LONG: std::time::Duration = std::time::Duration::from_millis(100);

/// Tries at the limit paced by [`LIMIT_PAUSE`] before [`LIMIT_PAUSE_LONG`].
const LIMIT_QUICK_TRIES: u32 = 6;

/// `pause`, less a random part of up to half of it, so that connections
/// (of this process and of others) that met the limit together do not all
/// try again at the same moment.
fn jittered(pause: std::time::Duration) -> std::time::Duration {
    use std::hash::{BuildHasher, Hasher};
    // Not for secrets: only spreads the tries. The standard library seeds
    // each RandomState differently.
    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let half = pause / 2;
    pause - half.mul_f64((seed % 1024) as f64 / 1024.0)
}

/// A turn that tasks take one at a time, first come first served.
#[derive(Default)]
struct Turn {
    /// Tasks holding the turn or waiting for it.
    users: Cell<usize>,
    held: Cell<bool>,
    next_id: Cell<u64>,
    /// The tasks waiting, oldest first.
    queue: RefCell<VecDeque<(u64, Option<Waker>)>>,
}

impl Turn {
    /// Someone holds the turn or waits for it.
    fn busy(&self) -> bool {
        self.users.get() > 0
    }

    async fn take(&self) -> TurnHeld<'_> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.users.set(self.users.get() + 1);
        let mut waiting = TurnWait {
            turn: self,
            id,
            done: false,
        };
        poll_fn(|cx| {
            let mut queue = self.queue.borrow_mut();
            let first = queue.front().map_or(true, |(front, _)| *front == id);
            if !self.held.get() && first {
                if !queue.is_empty() {
                    queue.pop_front();
                }
                self.held.set(true);
                waiting.done = true;
                return Poll::Ready(());
            }
            match queue.iter_mut().find(|(other, _)| *other == id) {
                Some((_, waker)) => *waker = Some(cx.waker().clone()),
                None => queue.push_back((id, Some(cx.waker().clone()))),
            }
            Poll::Pending
        })
        .await;
        TurnHeld { turn: self }
    }

    /// Wakes the oldest task waiting, when nobody holds the turn.
    fn wake_next(&self) {
        if self.held.get() {
            return;
        }
        if let Some((_, waker)) = self.queue.borrow_mut().front_mut() {
            if let Some(w) = waker.take() {
                w.wake();
            }
        }
    }
}

/// A task waiting for the turn; leaves the queue when given up.
struct TurnWait<'a> {
    turn: &'a Turn,
    id: u64,
    done: bool,
}

impl Drop for TurnWait<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.turn.users.set(self.turn.users.get() - 1);
        self.turn
            .queue
            .borrow_mut()
            .retain(|(id, _)| *id != self.id);
        self.turn.wake_next();
    }
}

/// The turn, while a task holds it.
struct TurnHeld<'a> {
    turn: &'a Turn,
}

impl Drop for TurnHeld<'_> {
    fn drop(&mut self) {
        self.turn.held.set(false);
        self.turn.users.set(self.turn.users.get() - 1);
        self.turn.wake_next();
    }
}

struct State {
    /// Packets waiting to be written.
    out: Vec<u8>,
    writer: Option<Waker>,
    /// One entry per command sent that the server answers, in order.
    queue: VecDeque<Rc<Slot>>,
    /// Why the connection stopped working, once it has.
    broken: Option<Error>,
    /// The client was dropped: write what is queued, then `COM_QUIT`.
    closing: bool,
    /// The server status from the last OK packet.
    status: u16,
    cache: HashMap<Rc<str>, Entry>,
    clock: u64,
}

enum Entry {
    Ready { stmt: Rc<Statement>, used: u64 },
    Preparing(Rc<Slot>),
}

/// A request in flight and what has arrived for it.
struct Slot {
    kind: Kind,
    /// Sent while the connection was shared.
    shared: bool,
    /// The session it was sent in.
    generation: u64,
    /// A statement of an open transaction: giving it up before its answer
    /// marks the transaction [`Tx::Abandoned`].
    in_tx: Option<Weak<Conn>>,
    inner: RefCell<SlotInner>,
}

enum Kind {
    Prepare {
        sql: Rc<str>,
        cached: bool,
    },
    Execute {
        stmt: Rc<Statement>,
        keep_rows: bool,
    },
    Simple,
    /// The pool's own: a reset of the session, or setting it up again. It
    /// changes the session on purpose, and if it fails, the connection is
    /// closed rather than used in a state nobody chose.
    Setup,
}

/// Where the answer to a request has got to.
#[derive(Default)]
enum Phase {
    /// Waiting for the first packet of a result.
    #[default]
    Start,
    /// Reading the parameter and column definitions after `PREPARE_OK`.
    Definitions { left: u32 },
    /// Reading the column definitions of a result set.
    Columns { left: u64 },
    /// Reading rows, until the OK packet that ends them.
    Rows,
}

#[derive(Default)]
struct SlotInner {
    /// The sequence number the next packet of the answer must have.
    seq: u8,
    phase: Phase,
    wakers: Vec<Waker>,
    /// Nobody is waiting for the result any more: rows are not kept.
    abandoned: bool,
    error: Option<Error>,
    prepared: Option<protocol::PrepareOk>,
    columns: Vec<Column>,
    /// The columns of the first result set, once it has been described.
    result_columns: Option<Rc<[Column]>>,
    rows: RowsBuilder,
    row_count: u64,
    /// The first result set has ended: later ones (from a stored
    /// procedure) are read and thrown away.
    first_done: bool,
    /// The server said the statement changed the session.
    session_changed: bool,
    /// Bytes of column definitions received, bounded like rows.
    meta_bytes: usize,
    ok: protocol::Ok,
    outcome: Option<Result<Response, Error>>,
}

enum Response {
    Statement(Rc<Statement>),
    Rows(ResultSet, protocol::Ok, u64),
    Done,
}

impl Slot {
    fn new(
        kind: Kind,
        seq: u8,
        shared: bool,
        generation: u64,
        in_tx: Option<Weak<Conn>>,
    ) -> Rc<Self> {
        Rc::new(Slot {
            kind,
            shared,
            generation,
            in_tx,
            inner: RefCell::new(SlotInner {
                seq,
                ..SlotInner::default()
            }),
        })
    }
}

/// Waits for a slot's outcome. Dropping it lets the reader throw the rows
/// away; the statement still runs.
struct Wait {
    slot: Rc<Slot>,
    /// The outcome was handed over.
    done: bool,
}

impl Wait {
    fn new(slot: Rc<Slot>) -> Self {
        Wait { slot, done: false }
    }
}

impl Future for Wait {
    type Output = Result<Response, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let slot = self.slot.clone();
        let mut inner = slot.inner.borrow_mut();
        let shared = matches!(slot.kind, Kind::Prepare { .. });
        let ready = match &inner.outcome {
            Some(Ok(Response::Statement(stmt))) if shared => {
                Some(Ok(Response::Statement(stmt.clone())))
            }
            Some(Err(e)) if shared => Some(Err(e.clone())),
            Some(_) => inner.outcome.take(),
            None => None,
        };
        if let Some(outcome) = ready {
            self.done = true;
            return Poll::Ready(outcome);
        }
        if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            inner.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if self.done || matches!(self.slot.kind, Kind::Prepare { .. }) {
            return;
        }
        // Its answer arrived before the drop: its outcome is known.
        let known = {
            let mut inner = self.slot.inner.borrow_mut();
            inner.abandoned = true;
            inner.rows = RowsBuilder::default();
            inner.outcome.is_some()
        };
        // The application gave up on a statement of its transaction, which
        // may run all the same: the transaction must not commit it unseen.
        if known {
            return;
        }
        if let Some(conn) = self.slot.in_tx.as_ref().and_then(Weak::upgrade) {
            if matches!(conn.tx.get(), Tx::Open | Tx::Uncertain) {
                conn.tx.set(Tx::Abandoned);
            }
        }
    }
}

impl Client {
    /// Opens a connection and logs in.
    ///
    /// Fails with [`ErrorKind::Connect`] when the server cannot be reached
    /// within [`Config::connect_timeout`](crate::Config::connect_timeout),
    /// [`ErrorKind::Auth`] when it refuses the credentials, and
    /// [`ErrorKind::Config`] when the configuration is incomplete.
    ///
    /// # Panics
    ///
    /// Panics when called outside of a giong runtime.
    ///
    /// Cancel safe: if the future is dropped, the connection being opened is
    /// closed.
    pub async fn connect(config: &Config) -> Result<Client, Error> {
        let startup = connect::connect(config).await?;
        Ok(Self::start(startup, config.clone()))
    }

    /// A client of a server played from bytes (see [`crate::fuzzing`]), as
    /// if it had just connected to MySQL 8.0.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn over_memory(
        memory: crate::fuzzing::Memory,
        config: Config,
        rollback_on_timeout: bool,
    ) -> Client {
        Self::start(
            Startup {
                stream: Stream::Memory(memory),
                buf: ReadBuf::new(),
                connection_id: 1,
                server_version: "8.0.46".to_owned(),
                status: status::AUTOCOMMIT,
                session_sql: "SET time_zone = '+00:00'".into(),
                identity: Identity {
                    peer: None,
                    server_id: "memory".to_owned(),
                },
                rollback_on_timeout,
            },
            config,
        )
    }

    fn start(startup: Startup, config: Config) -> Client {
        let Startup {
            stream,
            buf,
            connection_id,
            server_version,
            status,
            session_sql,
            identity,
            rollback_on_timeout,
        } = startup;
        let conn = Rc::new_cyclic(|weak| Conn {
            weak: weak.clone(),
            head_since: Cell::new(None),
            stream,
            config,
            connection_id,
            server_version,
            session_sql,
            identity,
            rollback_on_timeout,
            generation: Cell::new(0),
            dirty: Cell::new(false),
            closed: Arc::new(AtomicBool::new(false)),
            lease: RefCell::new(None),
            schema_changed: Cell::new(false),
            counters: RefCell::new(None),
            tx: Cell::new(Tx::None),
            shared: Cell::new(false),
            idle_wakers: RefCell::new(Vec::new()),
            turn: Turn::default(),
            turn_progress: Cell::new(None),
            state: RefCell::new(State {
                out: Vec::new(),
                writer: None,
                queue: VecDeque::new(),
                broken: None,
                closing: false,
                status,
                cache: HashMap::new(),
                clock: 0,
            }),
        });
        giong::spawn(read_loop(conn.clone(), buf));
        giong::spawn(write_loop(conn.clone()));
        Client { conn }
    }

    /// Runs a statement and returns its rows.
    ///
    /// Parameters are written `?` in the SQL and passed in order:
    ///
    /// ```no_run
    /// # async fn f(client: &giong_mysql::Client) -> Result<(), giong_mysql::Error> {
    /// let rows = client
    ///     .query("SELECT id, email FROM users WHERE created_at > NOW() - INTERVAL ? DAY", &[&7])
    ///     .await?;
    /// for row in &rows {
    ///     let id: u64 = row.get("id")?;
    ///     let email: &str = row.get("email")?;
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// See the [`types`](crate::types) module for which Rust types fit
    /// which. A stored procedure that returns several result sets gives the
    /// rows of the first.
    ///
    /// Not cancel safe: if the future is dropped, the statement may still run
    /// on the server, and what it changes stays; the connection stays usable.
    /// Inside a [`Transaction`], a later [`commit`](Transaction::commit) then
    /// fails instead of committing it.
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        let (set, _, _) = self.run(sql, params, true).await?;
        Ok(set.into_rows())
    }

    /// Runs a statement that must return exactly one row, and returns it.
    ///
    /// No row or several rows fail with [`ErrorKind::RowCount`].
    ///
    /// Not cancel safe, like [`query`](Self::query).
    pub async fn query_one(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Row, Error> {
        let mut rows = self.query(sql, params).await?;
        match rows.len() {
            1 => Ok(rows.pop().expect("one row")),
            n => Err(Error::new(
                ErrorKind::RowCount,
                format!(
                    "the statement returned {n} rows where exactly one was expected; use \
                     query_opt for zero or one, or query for any number"
                ),
            )),
        }
    }

    /// Runs a statement that returns at most one row.
    ///
    /// Several rows fail with [`ErrorKind::RowCount`].
    ///
    /// Not cancel safe, like [`query`](Self::query).
    pub async fn query_opt(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<Row>, Error> {
        let mut rows = self.query(sql, params).await?;
        match rows.len() {
            0 | 1 => Ok(rows.pop()),
            n => Err(Error::new(
                ErrorKind::RowCount,
                format!(
                    "the statement returned {n} rows where at most one was expected; add a \
                     LIMIT, or use query"
                ),
            )),
        }
    }

    /// Runs a statement and returns the number of rows it inserted, updated
    /// (matched, including rows already holding the new values) or deleted,
    /// or returned, for a `SELECT`.
    ///
    /// Not cancel safe, like [`query`](Self::query).
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        let (_, ok, rows) = self.run(sql, params, false).await?;
        Ok(if rows > 0 { rows } else { ok.affected_rows })
    }

    /// Runs an `INSERT` and returns the `AUTO_INCREMENT` id it generated
    /// (for several rows, the id of the first), or `None` when it generated
    /// none: a table without such a column, or an `INSERT ... ON DUPLICATE
    /// KEY UPDATE` that updated a row without changing its id.
    ///
    /// `None` is not an error: the statement ran, and its rows are written
    /// (under autocommit, committed). An error always means the statement
    /// failed, so code that retries on errors never writes twice.
    ///
    /// Not cancel safe, like [`query`](Self::query).
    pub async fn insert(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<u64>, Error> {
        let (_, ok, _) = self.run(sql, params, false).await?;
        Ok((ok.last_insert_id != 0).then_some(ok.last_insert_id))
    }

    /// Runs SQL that may hold several statements separated by `;`, with no
    /// parameters, such as a schema migration. Rows they return are thrown
    /// away.
    ///
    /// The statements run one after the other; the first that fails stops
    /// the rest, and those before it stay done. MySQL commits DDL (`CREATE`,
    /// `ALTER`, `DROP`) as it runs, so wrapping them in a transaction does not
    /// make them all-or-nothing.
    ///
    /// Not cancel safe: if the future is dropped, the statements may still run
    /// on the server, and what they change stays.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        self.conn.check_tx().await?;
        let may_change = sql.split(';').any(session::may_change);
        if may_change && !self.conn.shared.get() {
            self.conn.dirty.set(true);
        }
        let slot = self.conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, sql))
        })?;
        // On a connection other tasks share, the statements they send next
        // would run in whatever session the batch leaves: a reset follows it
        // at once.
        if may_change && self.conn.shared.get() {
            self.conn.queue_reset();
        }
        Wait::new(slot).await.map(|_| ())
    }

    /// Runs one statement of the client's own (`BEGIN`, `ROLLBACK`) and
    /// waits for it.
    async fn text(&self, sql: &str) -> Result<(), Error> {
        let slot = self.conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, sql))
        })?;
        Wait::new(slot).await.map(|_| ())
    }

    /// Starts a transaction.
    ///
    /// It is committed by [`Transaction::commit`]; dropping it without a
    /// commit rolls it back. While it is open, it holds the client mutably,
    /// so no other statement can run on the connection outside it.
    ///
    /// ```no_run
    /// # async fn f(client: &mut giong_mysql::Client) -> Result<(), giong_mysql::Error> {
    /// let tx = client.transaction().await?;
    /// tx.execute("UPDATE accounts SET balance = balance - ? WHERE id = ?", &[&100, &1]).await?;
    /// tx.execute("UPDATE accounts SET balance = balance + ? WHERE id = ?", &[&100, &2]).await?;
    /// tx.commit().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Unlike MySQL on its own, a transaction that the server ended early
    /// stays ended: after a deadlock rolled it back, or a statement such as
    /// `CREATE TABLE` committed it, the statements that follow fail instead
    /// of running outside any transaction, and `commit` fails.
    ///
    /// Cancel safe: if the future is dropped, a transaction it began is rolled
    /// back.
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self.conn.check_tx().await?;
        // The guard exists before BEGIN is sent: if this future is dropped
        // while waiting, the guard's drop rolls the transaction back.
        let tx = Transaction {
            client: self,
            done: false,
        };
        tx.client.text("BEGIN").await?;
        tx.client.conn.tx.set(Tx::Open);
        Ok(tx)
    }

    /// Resets the session to what it was right after connecting, and waits
    /// for the server to confirm.
    ///
    /// The server (`COM_RESET_CONNECTION`) rolls back an open transaction,
    /// releases table locks and named locks (`GET_LOCK`), drops temporary
    /// tables, forgets user variables and SQL prepared statements, and sets
    /// every session variable back to the server's default; then the
    /// session is set up again as when connecting (character set, time
    /// zone, time limits and [`Config::session_variable`](crate::Config::session_variable)s).
    /// The server also frees the statements this client prepared, so the
    /// statement cache starts empty.
    ///
    /// A [`Pool`](crate::Pool) does this by itself when a connection taken
    /// with [`Pool::get`](crate::Pool::get) comes back and may have changed
    /// its session. Call it before giving a connection back when a stored
    /// function or trigger took a named lock or set a user variable with
    /// `SELECT ... INTO`, which the server does not report.
    ///
    /// Fails with [`ErrorKind::Closed`] if the server refused the reset: the
    /// connection is then closed rather than left in a state nobody chose.
    ///
    /// Cancel safe: if the future is dropped, the reset may or may not have
    /// happened, and calling again does it.
    pub async fn reset_session(&self) -> Result<(), Error> {
        let slots = self.conn.queue_reset_slots()?;
        for slot in slots {
            Wait::new(slot).await?;
        }
        Ok(())
    }

    /// A handle that asks the server to stop whatever this connection is
    /// running, from any thread.
    ///
    /// A token taken from a [`PooledClient`](crate::PooledClient) stops
    /// working when the connection goes back to the pool, so that it cannot
    /// stop the statements of the tasks that use the connection next; and a
    /// connection that goes back while a cancel request from such a token is
    /// on its way is closed rather than lent to another task.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            config: self.conn.config.clone(),
            connection_id: self.conn.connection_id,
            identity: self.conn.identity.clone(),
            closed: self.conn.closed.clone(),
            lease: self.conn.lease.borrow().clone(),
        }
    }

    /// Starts lending the connection to one task: cancel tokens taken from
    /// now on work until [`end_lease`](Self::end_lease).
    pub(crate) fn begin_lease(&self) {
        *self.conn.lease.borrow_mut() = Some(Arc::new(Lease::default()));
    }

    /// Ends the lease: the cancel tokens taken during it stop working.
    /// Returns whether a cancel request taken during it may still be on its
    /// way to the server, in which case the connection must not be lent to
    /// another task: the request would stop that task's statement.
    pub(crate) fn end_lease(&self) -> bool {
        self.conn
            .lease
            .borrow_mut()
            .take()
            .is_some_and(|lease| lease.end())
    }

    /// The server's version, such as `8.0.46`.
    pub fn server_version(&self) -> &str {
        &self.conn.server_version
    }

    /// The server's id for this connection, as `CONNECTION_ID()` returns it
    /// and `SHOW PROCESSLIST` shows it.
    pub fn connection_id(&self) -> u32 {
        self.conn.connection_id
    }

    /// Whether the connection has stopped working. A closed client fails
    /// every statement with [`ErrorKind::Closed`].
    pub fn is_closed(&self) -> bool {
        self.conn.state.borrow().broken.is_some()
    }

    /// How many requests have been sent and not answered yet.
    pub fn in_flight(&self) -> usize {
        self.conn.state.borrow().queue.len()
    }

    /// How long the oldest unanswered request has been waiting for its
    /// answer; zero with nothing in flight.
    pub(crate) fn waiting_for(&self, now: giong::time::Instant) -> std::time::Duration {
        self.conn
            .head_since
            .get()
            .map_or(std::time::Duration::ZERO, |t| {
                now.saturating_duration_since(t)
            })
    }

    /// Whether the last answer from the server said a transaction was open.
    pub(crate) fn in_transaction(&self) -> bool {
        self.conn.state.borrow().status & status::IN_TRANS != 0 || self.conn.tx.get() != Tx::None
    }

    /// Queues a statement whose result nobody waits for.
    pub(crate) fn send_and_forget(&self, sql: &str) {
        if let Ok(slot) = self.conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, sql))
        }) {
            slot.inner.borrow_mut().abandoned = true;
        }
    }

    /// Whether the connection must be reset before other tasks use it:
    /// something sent on it may have changed the session, a transaction
    /// may be open (a statement still on its way may be a `BEGIN`, which
    /// the last answer cannot tell), or autocommit is off.
    pub(crate) fn needs_reset(&self) -> bool {
        let status = self.conn.state.borrow().status;
        self.conn.dirty.get()
            || self.in_flight() > 0
            || self.in_transaction()
            || status & status::AUTOCOMMIT == 0
    }

    /// Queues a reset of the session, ahead of anything sent after it,
    /// without waiting for it; see [`reset_session`](Self::reset_session).
    pub(crate) fn queue_reset(&self) {
        self.conn.queue_reset();
    }

    /// Where the pool counts what happens on this connection.
    pub(crate) fn count_into(&self, counters: Rc<Counters>) {
        *self.conn.counters.borrow_mut() = Some(counters);
    }

    /// Marks the connection as shared between tasks, or as one task's own.
    /// Statements sent while it is shared must not leave a transaction open
    /// or autocommit off: if one does, the connection is closed, which rolls
    /// the transaction back, rather than let the statements of other tasks
    /// run inside it.
    pub(crate) fn set_shared(&self, shared: bool) {
        self.conn.shared.set(shared);
    }

    /// Wakes `waker` once nothing is in flight, or the connection fails.
    pub(crate) fn wake_when_idle(&self, waker: &Waker) {
        let mut wakers = self.conn.idle_wakers.borrow_mut();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    async fn run(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        keep_rows: bool,
    ) -> Result<(ResultSet, protocol::Ok, u64), Error> {
        self.conn.check_tx().await?;
        if params.len() > MAX_PARAMS {
            return Err(Error::conversion(format!(
                "{} parameters were given; a statement can have at most {MAX_PARAMS}",
                params.len()
            )));
        }
        if !self.conn.shared.get() && session::may_change(sql) {
            self.conn.dirty.set(true);
        }
        let mut stmt = self.conn.statement(sql).await?;
        // Prepared before a reset of the session that was queued while this
        // task waited: the server forgets it at the reset, so prepare again.
        while stmt.generation != self.conn.generation.get() {
            stmt = self.conn.statement(sql).await?;
        }
        if stmt.params != params.len() {
            return Err(Error::conversion(format!(
                "the statement has {} parameters (?) but {} values were given",
                stmt.params,
                params.len()
            )));
        }
        let slot = self.conn.enqueue(
            Kind::Execute {
                stmt: stmt.clone(),
                keep_rows,
            },
            |out| encode_execute(out, stmt.id, params),
        )?;
        drop(stmt);
        match Wait::new(slot).await? {
            Response::Rows(set, ok, rows) => Ok((set, ok, rows)),
            _ => unreachable!("an execute slot ends with rows"),
        }
    }
}

/// `COM_STMT_EXECUTE` with its parameters; returns the sequence number of
/// the answer.
fn encode_execute(out: &mut Vec<u8>, id: u32, params: &[&dyn ToSql]) -> Result<u8, Error> {
    let start = protocol::begin(out);
    out.push(com::STMT_EXECUTE);
    out.extend_from_slice(&id.to_le_bytes());
    out.push(0); // CURSOR_TYPE_NO_CURSOR
    out.extend_from_slice(&1u32.to_le_bytes()); // iteration count
    if !params.is_empty() {
        let bitmap = out.len();
        out.resize(bitmap + params.len().div_ceil(8), 0);
        out.push(1); // the types follow
        let types = out.len();
        out.resize(types + 2 * params.len(), 0);
        for (i, p) in params.iter().enumerate() {
            let enc = p
                .to_sql(out)
                .map_err(|e| e.context(&format!("parameter {}", i + 1)))?;
            if enc.ty == ColumnType::NULL {
                out[bitmap + i / 8] |= 1 << (i % 8);
            }
            out[types + 2 * i] = enc.ty.code();
            out[types + 2 * i + 1] = if enc.unsigned { 0x80 } else { 0 };
        }
    }
    Ok(protocol::finish(out, start, 0))
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut st = self.conn.state.borrow_mut();
        st.closing = true;
        if st.broken.is_none() {
            protocol::command(&mut st.out, com::QUIT);
        }
        if let Some(w) = st.writer.take() {
            w.wake();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.conn.config.target())
            .field("connection_id", &self.conn.connection_id)
            .field("in_flight", &self.in_flight())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Conn {
    /// Queues `COM_RESET_CONNECTION`, the session's `SET` and the default
    /// database (which the reset keeps), which run before anything sent
    /// after them, and forgets the prepared statements the reset frees;
    /// returns their slots.
    ///
    /// Without a database in the configuration, a connection whose `USE`
    /// changed it cannot be put back: it is closed instead.
    fn queue_reset_slots(&self) -> Result<Vec<Rc<Slot>>, Error> {
        if self.config.database.is_none() && self.schema_changed.get() {
            let e = Error::closed(
                "a statement changed the connection's default database (USE), which resetting \
                 the session cannot undo when the configuration names none, so the connection \
                 was closed rather than shared; set Config::database",
            );
            self.fail(e.clone());
            return Err(e);
        }
        let mut slots = vec![self.enqueue(Kind::Setup, |out| {
            Ok(protocol::command(out, com::RESET_CONNECTION))
        })?];
        let sql = self.session_sql.clone();
        slots.push(self.enqueue(Kind::Setup, |out| {
            Ok(protocol::command_text(out, com::QUERY, &sql))
        })?);
        if let Some(database) = &self.config.database {
            slots.push(self.enqueue(Kind::Setup, |out| {
                Ok(protocol::command_text(out, com::INIT_DB, database))
            })?);
        }
        self.generation.set(self.generation.get() + 1);
        // Their ids mean nothing once the reset has run: nothing is closed.
        self.state.borrow_mut().cache.clear();
        self.dirty.set(false);
        self.schema_changed.set(false);
        self.tx.set(Tx::None);
        if let Some(counters) = &*self.counters.borrow() {
            counters.resets.set(counters.resets.get() + 1);
        }
        Ok(slots)
    }

    /// [`queue_reset_slots`](Self::queue_reset_slots), not waited for.
    fn queue_reset(&self) {
        if let Ok(slots) = self.queue_reset_slots() {
            for slot in slots {
                slot.inner.borrow_mut().abandoned = true;
            }
        }
    }

    /// Appends a request and queues its slot, or fails without sending
    /// anything. `encode` returns the sequence number the answer starts
    /// with.
    fn enqueue(
        &self,
        kind: Kind,
        encode: impl FnOnce(&mut Vec<u8>) -> Result<u8, Error>,
    ) -> Result<Rc<Slot>, Error> {
        let mut st = self.state.borrow_mut();
        if let Some(e) = &st.broken {
            return Err(e.clone());
        }
        let start = st.out.len();
        let seq = match encode(&mut st.out) {
            Ok(seq) => seq,
            Err(e) => {
                st.out.truncate(start);
                return Err(e);
            }
        };
        let in_tx = (matches!(kind, Kind::Execute { .. } | Kind::Simple)
            && matches!(self.tx.get(), Tx::Open | Tx::Uncertain))
        .then(|| self.weak.clone());
        let slot = Slot::new(kind, seq, self.shared.get(), self.generation.get(), in_tx);
        if st.queue.is_empty() {
            self.head_since.set(Some(giong::time::Instant::now()));
        }
        st.queue.push_back(slot.clone());
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(slot)
    }

    /// Refuses statements in a transaction the server already ended, or
    /// that gave up a statement. After an error whose effect on the
    /// transaction is unknown, first asks the server whether it is still
    /// open (one round trip, only then).
    async fn check_tx(&self) -> Result<(), Error> {
        if self.tx.get() == Tx::Uncertain {
            // A ping's OK carries the server's status: whether a
            // transaction is open. `record_ok` takes it in.
            let slot = self.enqueue(Kind::Simple, |out| Ok(protocol::command(out, com::PING)))?;
            Wait::new(slot).await?;
        }
        self.tx_error().map_or(Ok(()), Err)
    }

    /// Why no statement may be sent in the current transaction, if so.
    fn tx_error(&self) -> Option<Error> {
        let message = match self.tx.get() {
            Tx::None | Tx::Open | Tx::Uncertain => return None,
            Tx::RolledBack => {
                "the server rolled this transaction back (a deadlock, a lock wait timeout with \
                 innodb_rollback_on_timeout on, or a full lock table); this statement would run \
                 outside any transaction, so it was not sent. Drop the transaction and run it \
                 again from the start"
            }
            Tx::Ended => {
                "the server ended this transaction before COMMIT: a statement in it (such as \
                 CREATE, ALTER or DROP) made it commit, or an error rolled it back. This \
                 statement would run outside any transaction, so it was not sent; statements \
                 sent before this one became known ran outside it. Keep DDL out of \
                 transactions, and run the transaction again from the start"
            }
            Tx::Abandoned => {
                "a statement of this transaction was given up before its answer came (its \
                 future was dropped, by a timeout say), and it may have run, so the transaction \
                 cannot be committed; this statement was not sent. Drop the transaction (which \
                 rolls it back) and run it again from the start"
            }
        };
        Some(Error::new(ErrorKind::Db, message))
    }

    /// The prepared statement for `sql`: from the cache, from a prepare
    /// already in flight, or prepared now.
    ///
    /// When the server holds as many prepared statements as it allows, for
    /// all its clients together, prepares on this connection take turns:
    /// each closes the statements the connection is not using and tries
    /// again, pausing between tries (1 ms doubling to 20 ms, then to 100 ms,
    /// each less a random part of up to half), until it gets its statement or no
    /// prepare on the connection has succeeded for [`LIMIT_PATIENCE`].
    async fn statement(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        if self.turn.busy() && !self.is_cached(sql) {
            return self.statement_at_limit(sql, None).await;
        }
        let e = match self.statement_once(sql).await {
            Err(e) if e.server_code() == Some(code::PREPARED_LIMIT) => e,
            other => return other,
        };
        if let Some(counters) = &*self.counters.borrow() {
            counters
                .statement_limit_errors
                .set(counters.statement_limit_errors.get() + 1);
        }
        self.statement_at_limit(sql, Some(e)).await
    }

    /// The statement for `sql` is prepared and in the cache.
    fn is_cached(&self, sql: &str) -> bool {
        matches!(
            self.state.borrow().cache.get(sql),
            Some(Entry::Ready { .. })
        )
    }

    /// Prepares `sql` in its turn, while the server is at its
    /// `max_prepared_stmt_count`; `first` is the error that showed it.
    #[cold]
    async fn statement_at_limit(
        &self,
        sql: &str,
        mut first: Option<Error>,
    ) -> Result<Rc<Statement>, Error> {
        let entered = giong::time::Instant::now();
        let _turn = self.turn.take().await;
        let mut pause = std::time::Duration::from_millis(1);
        let mut closed = 0;
        let mut tries = 0u32;
        loop {
            tries += 1;
            closed += self.close_unused();
            let again = match self.statement_once(sql).await {
                Ok(stmt) => {
                    self.turn_progress.set(Some(giong::time::Instant::now()));
                    return Ok(stmt);
                }
                Err(again) if again.server_code() == Some(code::PREPARED_LIMIT) => again,
                Err(other) => return Err(other),
            };
            let since = self.turn_progress.get().map_or(entered, |p| p.max(entered));
            let now = giong::time::Instant::now();
            let wait = jittered(pause);
            if now + wait > since + LIMIT_PATIENCE {
                let db = again.db().cloned().expect("a server error");
                let first = first.take().unwrap_or(again);
                return Err(Error::with_db(
                    ErrorKind::Db,
                    format!(
                        "the server holds as many prepared statements as its \
                         max_prepared_stmt_count allows, counting those of every client (this \
                         connection closed the {closed} it was not using and tried again for {} \
                         ms). Lower Config::statement_cache_capacity or the number of \
                         connections, or raise max_prepared_stmt_count on the server (first \
                         error: {first})",
                        LIMIT_PATIENCE.as_millis()
                    ),
                    db,
                ));
            }
            giong::time::sleep(wait).await;
            let cap = if tries < LIMIT_QUICK_TRIES {
                LIMIT_PAUSE
            } else {
                LIMIT_PAUSE_LONG
            };
            pause = (pause * 2).min(cap);
        }
    }

    /// Closes the cached statements no task is about to use; returns how
    /// many.
    fn close_unused(&self) -> usize {
        let mut st = self.state.borrow_mut();
        let unused: Vec<Rc<str>> = st
            .cache
            .iter()
            .filter_map(|(k, e)| match e {
                Entry::Ready { stmt, .. } if Rc::strong_count(stmt) == 1 => Some(k.clone()),
                _ => None,
            })
            .collect();
        for key in &unused {
            if let Some(Entry::Ready { stmt, .. }) = st.cache.remove(key) {
                // No answer comes for a close.
                protocol::command_stmt(&mut st.out, com::STMT_CLOSE, stmt.id);
            }
        }
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        unused.len()
    }

    async fn statement_once(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        let pending = {
            let mut st = self.state.borrow_mut();
            st.clock += 1;
            let now = st.clock;
            match st.cache.get_mut(sql) {
                Some(Entry::Ready { stmt, used }) => {
                    *used = now;
                    return Ok(stmt.clone());
                }
                Some(Entry::Preparing(slot)) => Some(slot.clone()),
                None => None,
            }
        };
        let Some(slot) = pending else {
            return self.prepare(sql).await;
        };
        match Wait::new(slot).await? {
            Response::Statement(stmt) => Ok(stmt),
            _ => unreachable!("a prepare slot ends with a statement"),
        }
    }

    async fn prepare(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        let cached = self.config.statement_cache > 0;
        let key = Rc::<str>::from(sql);
        let slot = self.enqueue(
            Kind::Prepare {
                sql: key.clone(),
                cached,
            },
            |out| Ok(protocol::command_text(out, com::STMT_PREPARE, sql)),
        )?;
        if cached {
            self.state
                .borrow_mut()
                .cache
                .insert(key, Entry::Preparing(slot.clone()));
        }
        match Wait::new(slot).await? {
            Response::Statement(stmt) => Ok(stmt),
            _ => unreachable!("a prepare slot ends with a statement"),
        }
    }

    /// Stops the connection: every request waiting gets `err` (or the
    /// server's own error for it), and both tasks finish.
    fn fail(&self, err: Error) {
        let slots = {
            let mut st = self.state.borrow_mut();
            if st.broken.is_none() {
                st.broken = Some(err.clone());
            }
            self.closed.store(true, Ordering::Release);
            st.cache.clear();
            st.out.clear();
            self.head_since.set(None);
            if let Some(w) = st.writer.take() {
                w.wake();
            }
            std::mem::take(&mut st.queue)
        };
        for slot in slots {
            let mut inner = slot.inner.borrow_mut();
            let e = inner.error.take().unwrap_or_else(|| err.clone());
            inner.outcome = Some(Err(e));
            for w in inner.wakers.drain(..) {
                w.wake();
            }
        }
        self.wake_idle();
        self.stream.shutdown(Shutdown::Both);
    }

    fn wake_idle(&self) {
        let wakers = std::mem::take(&mut *self.idle_wakers.borrow_mut());
        for w in wakers {
            w.wake();
        }
    }

    /// Takes apart and handles every whole message in `buf`.
    fn process(&self, buf: &mut ReadBuf) -> Result<(), Error> {
        let limit = self.config.max_result_size.max(1 << 20);
        while let Some(message) = buf.next(limit) {
            match message {
                Message::Here { seq, payload } => {
                    self.handle(seq, seq.wrapping_add(1), buf.bytes(payload.clone()))?;
                    buf.consume(&payload);
                }
                Message::Joined { seq, next, payload } => self.handle(seq, next, &payload)?,
                Message::TooLarge { seq, next } => self.too_large(seq, next)?,
                Message::OutOfOrder { expected, got } => {
                    return Err(Error::protocol(format!(
                        "the server sent packet number {got} where {expected} was due"
                    )))
                }
            }
        }
        Ok(())
    }

    fn front(&self) -> Result<Rc<Slot>, Error> {
        self.state
            .borrow()
            .queue
            .front()
            .cloned()
            .ok_or_else(|| Error::protocol("the server sent a packet when nothing had been asked"))
    }

    /// Checks a message's sequence number against the answer it belongs to;
    /// `next` is the number the message after it must have.
    fn check_seq(inner: &mut SlotInner, seq: u8, next: u8) -> Result<(), Error> {
        if seq != inner.seq {
            return Err(Error::protocol(format!(
                "the server sent packet number {seq} where {} was due",
                inner.seq
            )));
        }
        inner.seq = next;
        Ok(())
    }

    /// A row longer than the result size limit, thrown away as it arrived.
    fn too_large(&self, seq: u8, next: u8) -> Result<(), Error> {
        let slot = self.front()?;
        let mut inner = slot.inner.borrow_mut();
        Self::check_seq(&mut inner, seq, next)?;
        if !matches!(inner.phase, Phase::Rows) {
            return Err(Error::protocol(
                "the server sent a message larger than max_result_size that is not a row",
            ));
        }
        // Only a result that is kept can be too large: `execute` counts the
        // rows and throws them away.
        if matches!(
            slot.kind,
            Kind::Execute {
                keep_rows: true,
                ..
            }
        ) {
            inner.rows.set_overflow();
        }
        if !inner.first_done {
            inner.row_count += 1;
        }
        Ok(())
    }

    fn handle(&self, seq: u8, next: u8, payload: &[u8]) -> Result<(), Error> {
        let slot = self.front()?;
        let mut inner = slot.inner.borrow_mut();
        Self::check_seq(&mut inner, seq, next)?;
        let first = payload
            .first()
            .copied()
            .ok_or_else(|| Error::protocol("the server sent an empty packet"))?;
        let done = match std::mem::take(&mut inner.phase) {
            Phase::Start if matches!(slot.kind, Kind::Prepare { .. }) => match first {
                0x00 => {
                    let ok = protocol::prepare_ok(payload)?;
                    let left = u32::from(ok.params) + u32::from(ok.columns);
                    inner.prepared = Some(ok);
                    if left == 0 {
                        true
                    } else {
                        inner.phase = Phase::Definitions { left };
                        false
                    }
                }
                0xFF => {
                    self.record_error(&mut inner, payload)?;
                    true
                }
                _ => {
                    return Err(Error::protocol(
                        "the server answered a prepare with an unexpected packet",
                    ))
                }
            },
            Phase::Start => match first {
                0x00 => self.record_ok(&slot.kind, &mut inner, payload)?,
                0xFF => {
                    self.record_error(&mut inner, payload)?;
                    true
                }
                0xFB => {
                    return Err(Error::closed(
                        "the server asked for a local file (LOAD DATA LOCAL INFILE), which \
                         giong-mysql does not send; the connection was closed",
                    ))
                }
                _ => {
                    let count = Reader::new(payload).lenenc("a column count")?;
                    if count == 0 || count > MAX_COLUMNS {
                        return Err(Error::protocol(format!(
                            "the server announced a result of {count} columns"
                        )));
                    }
                    inner.columns.clear();
                    inner.phase = Phase::Columns { left: count };
                    false
                }
            },
            Phase::Definitions { left } => {
                // Their contents are not needed: results describe their
                // own columns.
                if left > 1 {
                    inner.phase = Phase::Definitions { left: left - 1 };
                    false
                } else {
                    true
                }
            }
            Phase::Columns { left } => {
                // Bounded like the rows, so that a server cannot grow the
                // client without end with column definitions.
                inner.meta_bytes += payload.len();
                if inner.meta_bytes > self.config.max_result_size.max(1 << 20) {
                    return Err(Error::protocol(
                        "the server sent column definitions larger than max_result_size",
                    ));
                }
                let def: ColumnDef = protocol::column_def(payload)?;
                inner.columns.push(Column::from_def(def));
                if left > 1 {
                    inner.phase = Phase::Columns { left: left - 1 };
                } else {
                    inner.phase = Phase::Rows;
                    if !inner.first_done {
                        let columns = self.share_columns(&slot.kind, &inner.columns);
                        inner.result_columns = Some(columns);
                    }
                }
                false
            }
            Phase::Rows => {
                if first == 0xFF {
                    self.record_error(&mut inner, payload)?;
                    true
                } else if protocol::is_end_of_rows(payload) {
                    inner.first_done = true;
                    self.record_ok(&slot.kind, &mut inner, payload)?
                } else {
                    inner.phase = Phase::Rows;
                    let keep = matches!(
                        slot.kind,
                        Kind::Execute {
                            keep_rows: true,
                            ..
                        }
                    ) && !inner.abandoned
                        && inner.error.is_none()
                        && !inner.first_done;
                    if !inner.first_done {
                        inner.row_count += 1;
                    }
                    if keep {
                        let columns = inner.result_columns.clone().unwrap_or_default();
                        let limit = self.config.max_result_size;
                        inner.rows.push(payload, &columns, limit)?;
                    }
                    false
                }
            }
        };
        if done {
            // A statement sent on a shared connection opened a transaction,
            // or turned autocommit off: the statements other tasks send next
            // would run inside it, and be lost with it.
            let left_open = slot.shared && !matches!(slot.kind, Kind::Setup) && {
                let status = self.state.borrow().status;
                status & status::IN_TRANS != 0 || status & status::AUTOCOMMIT == 0
            };
            if left_open {
                inner.error = Some(Error::closed(LEFT_OPEN));
            }
            // The pool's own reset failed: nobody chose the state the
            // session is in now.
            let setup_failed = matches!(slot.kind, Kind::Setup) && inner.error.is_some();
            // A statement on a shared connection changed its session in a
            // way only the server saw (a trigger, a stored routine): reset
            // it before more statements of other tasks run in it.
            let reset = slot.shared
                && inner.session_changed
                && !matches!(slot.kind, Kind::Setup)
                && slot.generation == self.generation.get();
            let setup_error = setup_failed.then(|| {
                let e = inner.error.clone().expect("checked");
                Error::closed(format!(
                    "resetting the session of a pooled connection failed ({e}), so the \
                     connection was closed"
                ))
            });
            drop(inner);
            let idle = {
                let mut st = self.state.borrow_mut();
                st.queue.pop_front();
                self.head_since
                    .set((!st.queue.is_empty()).then(giong::time::Instant::now));
                st.queue.is_empty()
            };
            self.complete(&slot);
            if left_open {
                // Closing the connection makes the server roll back.
                return Err(Error::closed(LEFT_OPEN));
            }
            if let Some(e) = setup_error {
                return Err(e);
            }
            if reset {
                self.queue_reset();
            } else if idle {
                self.wake_idle();
            }
        }
        Ok(())
    }

    /// The columns of a result, shared with the statement's last result when
    /// they are the same.
    fn share_columns(&self, kind: &Kind, columns: &[Column]) -> Rc<[Column]> {
        if let Kind::Execute { stmt, .. } = kind {
            let mut kept = stmt.columns.borrow_mut();
            if &**kept != columns {
                *kept = columns.into();
            }
            return kept.clone();
        }
        columns.into()
    }

    /// Takes in an OK packet; returns whether the answer is complete.
    fn record_ok(&self, kind: &Kind, inner: &mut SlotInner, payload: &[u8]) -> Result<bool, Error> {
        let ok = protocol::ok(payload)?;
        self.state.borrow_mut().status = ok.status;
        let open = ok.status & status::IN_TRANS != 0;
        match self.tx.get() {
            // DDL committed it, or an error rolled it back.
            Tx::Open if !open => self.tx.set(Tx::Ended),
            // After an error: the answer says whether the transaction lived.
            Tx::Uncertain if open => self.tx.set(Tx::Open),
            Tx::Uncertain => self.tx.set(Tx::RolledBack),
            _ => {}
        }
        if ok.session_changed && !matches!(kind, Kind::Setup) {
            inner.session_changed = true;
            self.dirty.set(true);
            if ok.schema_changed {
                self.schema_changed.set(true);
            }
        }
        // The first OK of the answer counts (for a stored procedure, the
        // statement's own result).
        if inner.ok == protocol::Ok::default() {
            inner.ok = ok;
        }
        if ok.status & status::MORE_RESULTS_EXISTS != 0 {
            inner.phase = Phase::Start;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn record_error(&self, inner: &mut SlotInner, payload: &[u8]) -> Result<(), Error> {
        let db = DbError::parse(payload)?;
        if matches!(self.tx.get(), Tx::Open | Tx::Uncertain) {
            let rolled_back = match db.code() {
                code::DEADLOCK | code::LOCK_TABLE_FULL => true,
                code::LOCK_WAIT_TIMEOUT => self.rollback_on_timeout,
                _ => false,
            };
            // Any other error usually ends only its statement, but not
            // always (Galera conflicts, errors MySQL adds later): the next
            // statement first makes sure the transaction is still open.
            self.tx.set(if rolled_back {
                Tx::RolledBack
            } else {
                Tx::Uncertain
            });
        }
        // A statement that failed half way may have changed the session
        // (a trigger that set a variable) without the server saying so.
        self.dirty.set(true);
        if inner.error.is_none() {
            let note = match db.code() {
                code::QUERY_TIMEOUT => Some(format!(
                    "the SELECT ran longer than the statement timeout ({:?}; \
                     Config::statement_timeout)",
                    self.config.statement_timeout
                )),
                code::STATEMENT_TIMEOUT => Some(format!(
                    "the statement ran longer than the statement timeout ({:?}; \
                     Config::statement_timeout)",
                    self.config.statement_timeout
                )),
                _ => None,
            };
            inner.error = Some(Error::from_db(db, note));
        }
        Ok(())
    }

    /// Works out a finished request's outcome and wakes whoever waits.
    fn complete(&self, slot: &Rc<Slot>) {
        let mut inner = slot.inner.borrow_mut();
        let error = inner.error.take();
        let outcome = match &slot.kind {
            Kind::Prepare { sql, cached } => {
                let result = match (error, inner.prepared) {
                    (Some(e), _) => Err(e),
                    (None, Some(ok)) => Ok(Rc::new(Statement {
                        sql: sql.clone(),
                        id: ok.id,
                        params: usize::from(ok.params),
                        columns: RefCell::new(Rc::from(Vec::new())),
                        conn: (!*cached).then(|| self.weak.clone()),
                        generation: slot.generation,
                    })),
                    (None, None) => {
                        Err(Error::protocol("the server did not describe the statement"))
                    }
                };
                if *cached {
                    self.settle_cache(sql, slot, &result);
                }
                result.map(Response::Statement)
            }
            Kind::Execute { stmt, .. } => match error {
                Some(e) => {
                    self.forget_if_stale(stmt, &e);
                    Err(e)
                }
                None if inner.rows.overflow => Err(Error::new(
                    ErrorKind::TooLarge,
                    format!(
                        "the result is larger than max_result_size ({} bytes); add a LIMIT or \
                         select fewer columns, or raise Config::max_result_size",
                        self.config.max_result_size
                    ),
                )),
                None => {
                    let rows = std::mem::take(&mut inner.rows);
                    let columns = inner
                        .result_columns
                        .take()
                        .unwrap_or_else(|| Rc::from(Vec::new()));
                    Ok(Response::Rows(
                        rows.finish(columns),
                        inner.ok,
                        inner.row_count,
                    ))
                }
            },
            Kind::Simple | Kind::Setup => match error {
                Some(e) => Err(e),
                None => Ok(Response::Done),
            },
        };
        inner.outcome = Some(outcome);
        for w in inner.wakers.drain(..) {
            w.wake();
        }
    }

    /// Takes a cached statement out of the cache when the server no longer
    /// knows it (1243), or it no longer fits the schema (1615). The next run
    /// prepares it afresh.
    fn forget_if_stale(&self, stmt: &Statement, e: &Error) {
        let code = e.server_code();
        if !matches!(code, Some(code::UNKNOWN_STMT | code::NEED_REPREPARE)) {
            return;
        }
        let mut st = self.state.borrow_mut();
        if matches!(st.cache.get(&stmt.sql), Some(Entry::Ready { stmt: s, .. }) if s.id == stmt.id)
        {
            st.cache.remove(&stmt.sql);
            // The server still holds a statement that needs preparing again,
            // and counts it against max_prepared_stmt_count until closed.
            if code == Some(code::NEED_REPREPARE) && stmt.generation == self.generation.get() {
                protocol::command_stmt(&mut st.out, com::STMT_CLOSE, stmt.id);
                if let Some(w) = st.writer.take() {
                    w.wake();
                }
            }
        }
    }

    /// Puts a prepared statement in the cache (or takes out a failed one),
    /// closing the least recently used statements when it is full.
    fn settle_cache(&self, sql: &Rc<str>, slot: &Rc<Slot>, result: &Result<Rc<Statement>, Error>) {
        let mut st = self.state.borrow_mut();
        let ours = matches!(st.cache.get(sql), Some(Entry::Preparing(s)) if Rc::ptr_eq(s, slot));
        if !ours {
            // A statement prepared for a cache that was cleared: close it,
            // unless a reset of the session freed it already (its id may be
            // another statement's by now).
            if let Ok(stmt) = result {
                if slot.generation == self.generation.get() {
                    protocol::command_stmt(&mut st.out, com::STMT_CLOSE, stmt.id);
                }
            }
            return;
        }
        let Ok(stmt) = result else {
            st.cache.remove(sql);
            return;
        };
        st.clock += 1;
        let used = st.clock;
        st.cache.insert(
            sql.clone(),
            Entry::Ready {
                stmt: stmt.clone(),
                used,
            },
        );
        while st.cache.len() > self.config.statement_cache {
            // Only statements nobody is about to use can go.
            let victim = st
                .cache
                .iter()
                .filter_map(|(k, e)| match e {
                    Entry::Ready { stmt, used } if Rc::strong_count(stmt) == 1 => {
                        Some((*used, k.clone()))
                    }
                    _ => None,
                })
                .min();
            let Some((_, key)) = victim else { break };
            if let Some(Entry::Ready { stmt, .. }) = st.cache.remove(&key) {
                // No answer comes for a close.
                protocol::command_stmt(&mut st.out, com::STMT_CLOSE, stmt.id);
            }
        }
        if let Some(w) = st.writer.take() {
            w.wake();
        }
    }
}

async fn read_loop(conn: Rc<Conn>, mut buf: ReadBuf) {
    let err = loop {
        if let Err(e) = conn.process(&mut buf) {
            break e;
        }
        match conn.stream.read_into(&mut buf).await {
            Ok(0) => {
                let closing = conn.state.borrow().closing;
                break Error::closed(if closing {
                    "the connection was closed"
                } else {
                    "the server closed the connection"
                });
            }
            Ok(_) => {}
            Err(e) => break Error::io(ErrorKind::Closed, "reading from the server failed", e),
        }
    };
    conn.fail(err);
}

async fn write_loop(conn: Rc<Conn>) {
    let mut spare = Vec::new();
    loop {
        let next = poll_fn(|cx| {
            let mut st = conn.state.borrow_mut();
            if st.broken.is_some() {
                return Poll::Ready(None);
            }
            if !st.out.is_empty() {
                return Poll::Ready(Some(std::mem::replace(
                    &mut st.out,
                    std::mem::take(&mut spare),
                )));
            }
            if st.closing {
                return Poll::Ready(None);
            }
            st.writer = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        let Some(data) = next else { break };
        let (written, mut data) = conn.stream.write_all(data).await;
        if let Err(e) = written {
            conn.fail(Error::io(
                ErrorKind::Closed,
                "writing to the server failed",
                e,
            ));
            return;
        }
        data.clear();
        // One large statement must not leave every pooled connection holding
        // a buffer its size.
        if data.capacity() <= KEEP_CAPACITY {
            spare = data;
        }
    }
    if conn.state.borrow().closing {
        conn.stream.shutdown(Shutdown::Write);
    }
}

/// An open transaction. Statements run through it with the same methods as
/// on [`Client`] (it dereferences to the client).
///
/// Commit it with [`commit`](Self::commit). Dropping it otherwise rolls it
/// back: the `ROLLBACK` is queued at once, ahead of anything the connection
/// runs afterwards.
pub struct Transaction<'a> {
    client: &'a mut Client,
    done: bool,
}

impl Transaction<'_> {
    /// Commits the transaction.
    ///
    /// When the server ended the transaction early (a deadlock rolled it
    /// back, or DDL committed it), this is reported as an error, never as a
    /// successful commit.
    ///
    /// After a statement in it failed, this first asks the server whether
    /// the error ended the transaction (one round trip).
    ///
    /// Not cancel safe: if the future is dropped, the transaction may have been
    /// committed or rolled back, and the caller does not learn which.
    pub async fn commit(mut self) -> Result<(), Error> {
        let conn = self.client.conn.clone();
        // If this future is dropped while asking, the guard rolls back.
        let checked = conn.check_tx().await;
        self.done = true;
        conn.tx.set(Tx::None);
        if let Err(e) = checked {
            self.client.send_and_forget("ROLLBACK");
            return Err(e);
        }
        let slot = conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, "COMMIT"))
        })?;
        Wait::new(slot).await.map(|_| ())
    }

    /// Rolls the transaction back and waits for the server to confirm.
    ///
    /// Cancel safe: if the future is dropped, the transaction is rolled back
    /// all the same.
    pub async fn rollback(mut self) -> Result<(), Error> {
        self.done = true;
        self.client.conn.tx.set(Tx::None);
        self.client.text("ROLLBACK").await
    }
}

impl Deref for Transaction<'_> {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.client.conn.tx.set(Tx::None);
            self.client.send_and_forget("ROLLBACK");
        }
    }
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("client", &self.client)
            .finish()
    }
}

/// Asks the server to stop what a connection is running.
///
/// It can be sent to another thread. The request is `KILL QUERY` over a new
/// connection with the same settings, so the user needs the right to kill
/// its own statements (every user has it) or others' (`CONNECTION_ADMIN`).
/// The statement that is stopped fails with error 1317 (query interrupted)
/// on MySQL, 1927 on MariaDB; the connection stays usable.
///
/// What is stopped is whatever the connection runs when the request
/// arrives: with several tasks sharing the connection, or when the
/// statement meant has just finished, that is the next statement in line.
///
/// Connection ids are unique only within one server, so the request goes to
/// the address the connection reached (not to the host name again, which
/// behind DNS round robin or a load balancer may lead to another server),
/// and `KILL QUERY` is sent only if the server there is the same one
/// (`@@server_uuid`; on MariaDB, its host name, port and server id). A
/// balancer between the two can still lead elsewhere; then
/// [`cancel`](Self::cancel) fails and kills nothing.
#[derive(Clone)]
pub struct CancelToken {
    config: Config,
    connection_id: u32,
    identity: Identity,
    closed: Arc<AtomicBool>,
    /// Set for a token taken from a pooled connection: ended when the
    /// connection goes back to the pool.
    lease: Option<Arc<Lease>>,
}

/// A pooled connection's time with one task, shared with the cancel tokens
/// taken during it: whether it has ended, and how many cancel requests are
/// on their way.
#[derive(Default)]
pub(crate) struct Lease(AtomicU32);

impl Lease {
    /// Set once the connection has gone back to its pool.
    const ENDED: u32 = 1 << 31;

    /// Counts a cancel request on its way, unless the lease has ended.
    fn start_cancel(&self) -> bool {
        let mut v = self.0.load(Ordering::Acquire);
        while v & Self::ENDED == 0 {
            match self
                .0
                .compare_exchange_weak(v, v + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(now) => v = now,
            }
        }
        false
    }

    /// A cancel request that `start_cancel` counted got its answer.
    fn finish_cancel(&self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }

    /// Ends the lease; returns whether a cancel request is still on its way
    /// (or was given up on the way, which counts the same).
    fn end(&self) -> bool {
        self.0.fetch_or(Self::ENDED, Ordering::AcqRel) & !Self::ENDED != 0
    }
}

impl CancelToken {
    /// Sends the request. Success means the server received it, not that
    /// something was stopped: the statement may have finished first. Once
    /// the connection is closed, it does nothing: its id may be reused.
    ///
    /// Fails with [`ErrorKind::Closed`], sending nothing, when the token was
    /// taken from a [`PooledClient`](crate::PooledClient) that has gone back
    /// to its pool since.
    ///
    /// Not cancel safe: if the future is dropped, the request may still reach
    /// the server and stop whatever the connection runs then.
    pub async fn cancel(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(lease) = &self.lease else {
            return connect::kill_query(&self.config, &self.identity, self.connection_id).await;
        };
        // Counted until the answer comes: a connection that goes back to the
        // pool meanwhile is closed rather than lent to a task whose
        // statement the request would stop. A request given up on the way
        // stays counted, for the same reason.
        if !lease.start_cancel() {
            return Err(Error::closed(
                "the connection this cancel token was taken from has gone back to its pool, \
                 where other tasks use it, so nothing was sent; take a new token from the next \
                 connection",
            ));
        }
        let sent = connect::kill_query(&self.config, &self.identity, self.connection_id).await;
        lease.finish_cancel();
        sent
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("server", &self.config.target())
            .field("connection_id", &self.connection_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_at_the_limit_are_spread_over_their_second_half() {
        let pause = std::time::Duration::from_millis(100);
        let waits: std::collections::HashSet<_> = (0..64).map(|_| jittered(pause)).collect();
        assert!(
            waits.iter().all(|w| *w >= pause / 2 && *w <= pause),
            "{waits:?}"
        );
        assert!(waits.len() > 8, "{waits:?}");
    }

    #[test]
    fn the_turn_goes_in_order_and_skips_tasks_that_gave_up() {
        giong::block_on(async {
            let turn = Rc::new(Turn::default());
            let order = Rc::new(RefCell::new(Vec::new()));
            let first = turn.take().await;
            let task = |n: u32| {
                let (turn, order) = (turn.clone(), order.clone());
                giong::spawn(async move {
                    let _held = turn.take().await;
                    order.borrow_mut().push(n);
                    giong::yield_now().await;
                })
            };
            let one = task(1);
            let two = task(2);
            let three = task(3);
            giong::yield_now().await;
            assert_eq!(turn.users.get(), 4);
            // A task that gives up its place leaves the queue.
            two.abort();
            giong::yield_now().await;
            assert_eq!(turn.users.get(), 3);
            drop(first);
            one.await.unwrap();
            three.await.unwrap();
            assert_eq!(*order.borrow(), [1, 3]);
            assert!(!turn.busy());
            // The first task waiting gives up while the turn is free: the
            // next one gets it.
            let held = turn.take().await;
            let gone = task(4);
            let next = task(5);
            giong::yield_now().await;
            drop(held);
            gone.abort();
            next.await.unwrap();
            assert_eq!(*order.borrow(), [1, 3, 5]);
            assert!(!turn.busy());
        });
    }
}
