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
use std::task::{Context, Poll, Waker};

use crate::config::Config;
use crate::connect::{self, Message, ReadBuf, Startup, Stream};
use crate::error::{DbError, Error, ErrorKind};
use crate::protocol::{self, com, status, ColumnDef, Reader};
use crate::row::{Column, ResultSet, Row, RowsBuilder};
use crate::types::{ColumnType, ToSql};

/// The most parameters a statement can have.
const MAX_PARAMS: usize = 65_535;

/// MySQL error numbers this crate reacts to.
mod code {
    /// `ER_UNKNOWN_STMT_HANDLER`: the statement is gone from the server.
    pub const UNKNOWN_STMT: u16 = 1243;
    /// `ER_NEED_REPREPARE`: the statement no longer fits the schema.
    pub const NEED_REPREPARE: u16 = 1615;
    /// `ER_LOCK_DEADLOCK`: the transaction was rolled back.
    pub const DEADLOCK: u16 = 1213;
    /// `ER_QUERY_TIMEOUT`: `max_execution_time` stopped a `SELECT`.
    pub const QUERY_TIMEOUT: u16 = 3024;
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
}

impl Drop for Statement {
    fn drop(&mut self) {
        let Some(conn) = self.conn.as_ref().and_then(Weak::upgrade) else {
            return;
        };
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
    /// The server ended it before `COMMIT`: a deadlock rolled it back, or a
    /// statement (such as DDL) committed it.
    RolledBack,
    Committed,
}

/// Why a shared connection was closed under its statements.
const LEFT_OPEN: &str = "a statement sent through the pool's shared methods (Pool::batch_execute) \
     left a transaction open, or autocommit off, on a connection other tasks share, so the \
     connection was closed and the transaction rolled back, with every statement sent on it \
     since, this one included. Run transactions on a connection of the task's own: \
     pool.get().await?.transaction()";

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
    tx: Cell<Tx>,
    /// Several tasks send statements on this connection at once (it is among
    /// a pool's shared connections): none of them may leave a transaction
    /// open, or autocommit off, which would take in the statements of the
    /// others.
    shared: Cell<bool>,
    /// Woken when nothing is in flight any more, or the connection fails.
    idle_wakers: RefCell<Vec<Waker>>,
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
    ok: protocol::Ok,
    outcome: Option<Result<Response, Error>>,
}

enum Response {
    Statement(Rc<Statement>),
    Rows(ResultSet, protocol::Ok, u64),
    Done,
}

impl Slot {
    fn new(kind: Kind, seq: u8, shared: bool) -> Rc<Self> {
        Rc::new(Slot {
            kind,
            shared,
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
}

impl Future for Wait {
    type Output = Result<Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.slot.inner.borrow_mut();
        let shared = matches!(self.slot.kind, Kind::Prepare { .. });
        match &inner.outcome {
            Some(Ok(Response::Statement(stmt))) if shared => {
                return Poll::Ready(Ok(Response::Statement(stmt.clone())))
            }
            Some(Err(e)) if shared => return Poll::Ready(Err(e.clone())),
            Some(_) => return Poll::Ready(inner.outcome.take().expect("checked")),
            None => {}
        }
        if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            inner.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if !matches!(self.slot.kind, Kind::Prepare { .. }) {
            let mut inner = self.slot.inner.borrow_mut();
            inner.abandoned = true;
            inner.rows = RowsBuilder::default();
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
    pub async fn connect(config: &Config) -> Result<Client, Error> {
        let startup = connect::connect(config).await?;
        Ok(Self::start(startup, config.clone()))
    }

    fn start(startup: Startup, config: Config) -> Client {
        let Startup {
            stream,
            buf,
            connection_id,
            server_version,
            status,
        } = startup;
        let conn = Rc::new_cyclic(|weak| Conn {
            weak: weak.clone(),
            head_since: Cell::new(None),
            stream,
            config,
            connection_id,
            server_version,
            tx: Cell::new(Tx::None),
            shared: Cell::new(false),
            idle_wakers: RefCell::new(Vec::new()),
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
    /// Cancel safe in the sense that matters: dropping the future leaves the
    /// connection usable. The statement may still run on the server.
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        let (set, _, _) = self.run(sql, params, true).await?;
        Ok(set.into_rows())
    }

    /// Runs a statement that must return exactly one row, and returns it.
    ///
    /// No row or several rows fail with [`ErrorKind::RowCount`].
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
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        let (_, ok, rows) = self.run(sql, params, false).await?;
        Ok(if rows > 0 { rows } else { ok.affected_rows })
    }

    /// Runs an `INSERT` into a table with an `AUTO_INCREMENT` column and
    /// returns the id it generated (for several rows, the id of the first).
    ///
    /// Fails with [`ErrorKind::RowCount`] when the statement generated no id;
    /// use [`execute`](Self::execute) for those.
    pub async fn insert(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        let (_, ok, _) = self.run(sql, params, false).await?;
        if ok.last_insert_id == 0 {
            return Err(Error::new(
                ErrorKind::RowCount,
                format!(
                    "the statement generated no AUTO_INCREMENT id ({} rows affected); use \
                     execute for statements on tables without one",
                    ok.affected_rows
                ),
            ));
        }
        Ok(ok.last_insert_id)
    }

    /// Runs SQL that may hold several statements separated by `;`, with no
    /// parameters, such as a schema migration. Rows they return are thrown
    /// away.
    ///
    /// The statements run one after the other; the first that fails stops
    /// the rest, and those before it stay done. MySQL commits DDL (`CREATE`,
    /// `ALTER`, `DROP`) as it runs, so wrapping them in a transaction does not
    /// make them all-or-nothing.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        self.conn.check_tx()?;
        let slot = self.conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, sql))
        })?;
        Wait { slot }.await.map(|_| ())
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
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self.conn.check_tx()?;
        // The guard exists before BEGIN is sent: if this future is dropped
        // while waiting, the guard's drop rolls the transaction back.
        let tx = Transaction {
            client: self,
            done: false,
        };
        tx.client.batch_execute("BEGIN").await?;
        tx.client.conn.tx.set(Tx::Open);
        Ok(tx)
    }

    /// A handle that asks the server to stop whatever this connection is
    /// running, from any thread.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            config: self.conn.config.clone(),
            connection_id: self.conn.connection_id,
        }
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

    /// Rolls back whatever transaction is open, and forgets any
    /// [`Transaction`] that ended early. A statement still on its way (its
    /// future was dropped, by a timeout say) may be a `BEGIN`, which the last
    /// answer cannot tell, so then `ROLLBACK` is sent anyway.
    pub(crate) fn reset_transaction(&self) {
        if self.in_flight() > 0 || self.in_transaction() {
            self.send_and_forget("ROLLBACK");
        }
        self.conn.tx.set(Tx::None);
    }

    /// Whether the last answer from the server said autocommit was on.
    pub(crate) fn autocommit(&self) -> bool {
        self.conn.state.borrow().status & status::AUTOCOMMIT != 0
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
        self.conn.check_tx()?;
        if params.len() > MAX_PARAMS {
            return Err(Error::conversion(format!(
                "{} parameters were given; a statement can have at most {MAX_PARAMS}",
                params.len()
            )));
        }
        let stmt = self.conn.statement(sql).await?;
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
        match (Wait { slot }).await? {
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
        let slot = Slot::new(kind, seq, self.shared.get());
        if st.queue.is_empty() {
            self.head_since.set(Some(giong::time::Instant::now()));
        }
        st.queue.push_back(slot.clone());
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(slot)
    }

    /// Refuses statements in a transaction the server already ended.
    fn check_tx(&self) -> Result<(), Error> {
        match self.tx.get() {
            Tx::RolledBack => Err(Error::new(
                ErrorKind::Db,
                "the transaction was rolled back by the server (a deadlock); this statement \
                 would run outside any transaction, so it was not sent. Drop the transaction \
                 and run it again from the start",
            )),
            Tx::Committed => Err(Error::new(
                ErrorKind::Db,
                "a statement in this transaction (such as CREATE, ALTER or DROP) made MySQL \
                 commit it; this statement would run outside any transaction, so it was not \
                 sent. Keep DDL out of transactions",
            )),
            Tx::None | Tx::Open => Ok(()),
        }
    }

    /// The prepared statement for `sql`: from the cache, from a prepare
    /// already in flight, or prepared now.
    async fn statement(&self, sql: &str) -> Result<Rc<Statement>, Error> {
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
        match (Wait { slot }).await? {
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
        match (Wait { slot }).await? {
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
        inner.rows.set_overflow();
        inner.row_count += 1;
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
                0x00 => self.record_ok(&mut inner, payload)?,
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
                    self.record_ok(&mut inner, payload)?
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
            let left_open = slot.shared && {
                let status = self.state.borrow().status;
                status & status::IN_TRANS != 0 || status & status::AUTOCOMMIT == 0
            };
            if left_open {
                inner.error = Some(Error::closed(LEFT_OPEN));
            }
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
            if idle {
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
    fn record_ok(&self, inner: &mut SlotInner, payload: &[u8]) -> Result<bool, Error> {
        let ok = protocol::ok(payload)?;
        self.state.borrow_mut().status = ok.status;
        if self.tx.get() == Tx::Open && ok.status & status::IN_TRANS == 0 {
            self.tx.set(Tx::Committed);
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
        if db.code() == code::DEADLOCK && self.tx.get() == Tx::Open {
            self.tx.set(Tx::RolledBack);
        }
        if inner.error.is_none() {
            let note = (db.code() == code::QUERY_TIMEOUT).then(|| {
                format!(
                    "the SELECT ran longer than the statement timeout ({:?}; \
                     Config::statement_timeout)",
                    self.config.statement_timeout
                )
            });
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
            Kind::Simple => match error {
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
    /// knows it. The next run prepares it afresh.
    fn forget_if_stale(&self, stmt: &Statement, e: &Error) {
        if !matches!(e.code(), Some(code::UNKNOWN_STMT | code::NEED_REPREPARE)) {
            return;
        }
        let mut st = self.state.borrow_mut();
        if matches!(st.cache.get(&stmt.sql), Some(Entry::Ready { stmt: s, .. }) if s.id == stmt.id)
        {
            st.cache.remove(&stmt.sql);
        }
    }

    /// Puts a prepared statement in the cache (or takes out a failed one),
    /// closing the least recently used statements when it is full.
    fn settle_cache(&self, sql: &Rc<str>, slot: &Rc<Slot>, result: &Result<Rc<Statement>, Error>) {
        let mut st = self.state.borrow_mut();
        let ours = matches!(st.cache.get(sql), Some(Entry::Preparing(s)) if Rc::ptr_eq(s, slot));
        if !ours {
            // A statement prepared for a cache that was cleared: close it.
            if let Ok(stmt) = result {
                protocol::command_stmt(&mut st.out, com::STMT_CLOSE, stmt.id);
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
        match conn.stream.recv().await {
            Ok(chunk) if chunk.is_empty() => {
                let closing = conn.state.borrow().closing;
                break Error::closed(if closing {
                    "the connection was closed"
                } else {
                    "the server closed the connection"
                });
            }
            Ok(chunk) => buf.extend(&chunk),
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
        spare = data;
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
    pub async fn commit(mut self) -> Result<(), Error> {
        self.done = true;
        let conn = self.client.conn.clone();
        let state = conn.tx.replace(Tx::None);
        if state != Tx::Open {
            conn.tx.set(state);
            let err = conn.check_tx().err();
            conn.tx.set(Tx::None);
            if let Some(e) = err {
                self.client.send_and_forget("ROLLBACK");
                return Err(e);
            }
        }
        let slot = conn.enqueue(Kind::Simple, |out| {
            Ok(protocol::command_text(out, com::QUERY, "COMMIT"))
        })?;
        (Wait { slot }).await.map(|_| ())
    }

    /// Rolls the transaction back and waits for the server to confirm.
    pub async fn rollback(mut self) -> Result<(), Error> {
        self.done = true;
        self.client.conn.tx.set(Tx::None);
        self.client.batch_execute("ROLLBACK").await
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
/// The statement that is stopped fails with error 1317 (query interrupted);
/// the connection stays usable. With several tasks sharing the connection,
/// the statement running when the request arrives is the one stopped.
#[derive(Clone)]
pub struct CancelToken {
    config: Config,
    connection_id: u32,
}

impl CancelToken {
    /// Sends the request. Success means the server received it, not that
    /// something was stopped: the statement may have finished first.
    pub async fn cancel(&self) -> Result<(), Error> {
        connect::kill_query(&self.config, self.connection_id).await
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
