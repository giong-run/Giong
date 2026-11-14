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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::config::Config;
use crate::connect::{self, ReadBuf, Startup, Stream};
use crate::error::{DbError, Error, ErrorKind};
use crate::protocol::{self, parse_backend, Backend, MAX_PARAMS};
use crate::row::{Column, ResultSet, Row, RowsBuilder};
use crate::types::{ToSql, Type};

/// A connection to a PostgreSQL server.
///
/// Every method takes `&self`, so tasks can share a client (through an `Rc`)
/// and run statements at the same time: their messages go out together and
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
/// connection once the statements already sent have been written.
pub struct Client {
    conn: Rc<Conn>,
}

/// A prepared statement on one connection.
pub(crate) struct Statement {
    sql: Rc<str>,
    name: Box<str>,
    params: Vec<Type>,
    columns: Rc<[Column]>,
    /// Set for a statement that is not cached: the connection, so that the
    /// statement can be closed if it was never run.
    conn: Option<Weak<Conn>>,
    closed: Cell<bool>,
}

impl Drop for Statement {
    fn drop(&mut self) {
        let Some(conn) = self.conn.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        if !self.closed.get() {
            // No Sync: the close travels with the next request.
            if let Ok(mut st) = conn.state.try_borrow_mut() {
                if st.broken.is_none() {
                    let _ = protocol::close_statement(&mut st.out, &self.name);
                }
            }
        }
    }
}

/// Why a shared connection was closed under its statements.
const LEFT_OPEN: &str = "a statement sent through the pool's shared methods (Pool::batch_execute) \
     left a transaction open on a connection other tasks share, so the connection was closed and \
     the transaction rolled back, with every statement sent on it since BEGIN, this one included. \
     Run transactions on a connection of the task's own: pool.get().await?.transaction()";

/// What a pool sends when a connection taken with `get()` comes back, so
/// that the next task finds a clean session: `DISCARD ALL` without
/// `DEALLOCATE ALL` (which would also drop the statement cache) and without
/// `DISCARD PLANS` (plans are not session state). `RESET ALL` returns the
/// settings sent when connecting (`statement_timeout`, `Config::param`) to
/// those values, not to the server's defaults.
const RESET: &str = "SET SESSION AUTHORIZATION DEFAULT; RESET ALL; CLOSE ALL; UNLISTEN *; \
     SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD SEQUENCES";

/// The prefix of the names of the statements this crate prepares, chosen to
/// stay clear of names an application would give its own.
const NAME_PREFIX: &str = "__giong_";

/// The state shared by a client and its two tasks.
struct Conn {
    weak: Weak<Conn>,
    stream: Stream,
    state: RefCell<State>,
    /// When the oldest unanswered request became the oldest; `None` with
    /// nothing in flight.
    head_since: Cell<Option<giong::time::Instant>>,
    config: Config,
    key: Option<(i32, i32)>,
    /// Several tasks send statements on this connection at once (it is among
    /// a pool's shared connections): none of them may leave a transaction
    /// open, which would take in the statements of the others.
    shared: Cell<bool>,
    /// Woken when nothing is in flight any more, or the connection fails.
    idle_wakers: RefCell<Vec<Waker>>,
    /// Requests sent since the connection opened: a pool compares the count
    /// before and after lending it, to know whether it was used.
    requests: Cell<u64>,
    /// While a pool lends the connection to one task: the flag its cancel
    /// tokens check, cleared when the connection goes back.
    lease: RefCell<Option<Arc<AtomicBool>>>,
    /// The server refused a write because it is a standby (it was a primary
    /// once, or DNS still points at it): a pool closes the connection rather
    /// than keep sending writes there.
    retire: Cell<bool>,
    /// A statement ran SQL-level `PREPARE`, whose prepared statement outlives
    /// the task that made it.
    user_prepared: Cell<bool>,
}

struct State {
    /// Messages waiting to be written.
    out: Vec<u8>,
    writer: Option<Waker>,
    /// One entry per `Sync` or simple query sent, answered in order.
    queue: VecDeque<Rc<Slot>>,
    /// Why the connection stopped working, once it has.
    broken: Option<Error>,
    /// The client was dropped: write what is queued, then `Terminate`.
    closing: bool,
    /// The transaction status from the last `ReadyForQuery`.
    status: u8,
    cache: HashMap<Rc<str>, Entry>,
    clock: u64,
    next_name: u64,
    server_params: Vec<(String, String)>,
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
    /// The pool's session reset: if it fails, the session is in an unknown
    /// state, so the connection is closed rather than reused.
    close_on_error: bool,
    inner: RefCell<SlotInner>,
}

enum Kind {
    Prepare {
        sql: Rc<str>,
        name: Box<str>,
        cached: bool,
    },
    Execute {
        stmt: Rc<Statement>,
    },
    Simple,
}

#[derive(Default)]
struct SlotInner {
    wakers: Vec<Waker>,
    /// Nobody is waiting for the result any more: rows are not kept.
    abandoned: bool,
    error: Option<Error>,
    params: Option<Vec<Type>>,
    columns: Option<Vec<Column>>,
    rows: RowsBuilder,
    tag: Option<String>,
    /// The server confirmed the Bind: the statement started running.
    bound: bool,
    /// The transaction status in the `ReadyForQuery` that ended the request.
    status: u8,
    /// The request failed on a stale cached statement before running, outside
    /// a transaction: running it again is safe.
    retry: bool,
    outcome: Option<Result<Response, Error>>,
}

enum Response {
    Statement(Rc<Statement>),
    Rows(ResultSet, Option<String>),
    Done(Option<String>),
}

impl Slot {
    fn new(kind: Kind, shared: bool, close_on_error: bool) -> Rc<Self> {
        Rc::new(Slot {
            kind,
            shared,
            close_on_error,
            inner: RefCell::default(),
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

/// Whether the server's error says a prepared statement no longer fits: the
/// schema changed under its cached plan (`0A000` from
/// `RevalidateCachedQuery`; the message is matched too, for servers that send
/// no routine, but it is translated when `lc_messages` is not English), or it
/// does not exist any more (`26000`, after `DEALLOCATE` or `DISCARD ALL`).
fn is_stale(db: &DbError) -> bool {
    match db.code() {
        "26000" => true,
        "0A000" => {
            db.routine() == Some("RevalidateCachedQuery")
                || db.message().contains("cached plan must not change")
        }
        _ => false,
    }
}

/// The number of rows a command tag reports: `INSERT 0 5`, `UPDATE 3`,
/// `SELECT 7`.
fn rows_affected(tag: Option<&str>) -> u64 {
    tag.and_then(|t| t.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
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

    pub(crate) fn start(startup: Startup, config: Config) -> Client {
        let Startup {
            stream,
            buf,
            params,
            key,
            status,
        } = startup;
        let conn = Rc::new_cyclic(|weak| Conn {
            weak: weak.clone(),
            head_since: Cell::new(None),
            stream,
            config,
            key,
            shared: Cell::new(false),
            idle_wakers: RefCell::new(Vec::new()),
            requests: Cell::new(0),
            lease: RefCell::new(None),
            retire: Cell::new(false),
            user_prepared: Cell::new(false),
            state: RefCell::new(State {
                out: Vec::new(),
                writer: None,
                queue: VecDeque::new(),
                broken: None,
                closing: false,
                status,
                cache: HashMap::new(),
                clock: 0,
                next_name: 0,
                server_params: params,
            }),
        });
        giong::spawn(read_loop(conn.clone(), buf));
        giong::spawn(write_loop(conn.clone()));
        Client { conn }
    }

    /// Runs a statement and returns its rows.
    ///
    /// Parameters are written `$1`, `$2`... in the SQL and passed in order:
    ///
    /// ```no_run
    /// # async fn f(client: &giong_postgres::Client) -> Result<(), giong_postgres::Error> {
    /// let rows = client
    ///     .query("SELECT id, email FROM users WHERE created_at > now() - $1::interval", &[&"7 days"])
    ///     .await?;
    /// for row in &rows {
    ///     let id: i64 = row.get("id")?;
    ///     let email: &str = row.get("email")?;
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// The server decides each parameter's type from the SQL; see the
    /// [`types`](crate::types) module for which Rust types fit which.
    ///
    /// Cancel safe in the sense that matters: dropping the future leaves the
    /// connection usable. The statement may still run on the server.
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>, Error> {
        let (set, _) = self.run(sql, params).await?;
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
    /// or deleted (or returned, for a `SELECT`).
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, Error> {
        let (_, tag) = self.run(sql, params).await?;
        Ok(rows_affected(tag.as_deref()))
    }

    /// Runs SQL that may hold several statements separated by `;`, with no
    /// parameters, such as a schema migration. Rows they return are thrown
    /// away.
    ///
    /// The statements run in one implicit transaction unless the SQL has its
    /// own `BEGIN` and `COMMIT`: if one fails, the earlier ones are rolled
    /// back.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        let slot = self
            .conn
            .enqueue(Kind::Simple, |out| protocol::query(out, sql))?;
        Wait { slot }.await.map(|_| ())
    }

    /// Starts a transaction.
    ///
    /// It is committed by [`Transaction::commit`]; dropping it without a
    /// commit rolls it back. While it is open, it holds the client mutably,
    /// so no other statement can run on the connection outside it.
    ///
    /// ```no_run
    /// # async fn f(client: &mut giong_postgres::Client) -> Result<(), giong_postgres::Error> {
    /// let tx = client.transaction().await?;
    /// tx.execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", &[&100i64, &1i64]).await?;
    /// tx.execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", &[&100i64, &2i64]).await?;
    /// tx.commit().await?;
    /// # Ok(()) }
    /// ```
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        // The guard exists before BEGIN is sent: if this future is dropped
        // while waiting, the guard's drop rolls the transaction back.
        let tx = Transaction {
            client: self,
            done: false,
        };
        tx.client.batch_execute("BEGIN").await?;
        Ok(tx)
    }

    /// A handle that asks the server to cancel whatever this connection is
    /// running, from any thread.
    ///
    /// A token taken from a [`PooledClient`](crate::PooledClient) stops
    /// working when the connection goes back to the pool, so that it cannot
    /// cancel the statements of the tasks that use the connection next.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            config: self.conn.config.without_password(),
            key: self.conn.key,
            lease: self.conn.lease.borrow().clone(),
        }
    }

    /// A setting the server reported, such as `server_version`, `TimeZone`
    /// or `integer_datetimes`.
    pub fn parameter(&self, name: &str) -> Option<String> {
        let st = self.conn.state.borrow();
        st.server_params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
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
        self.conn.state.borrow().status != b'I'
    }

    /// Marks the connection as shared between tasks, or as one task's own.
    /// Statements sent while it is shared must not leave a transaction open:
    /// if one does, the connection is closed, which rolls the transaction
    /// back, rather than let the statements of other tasks run inside it.
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

    /// How many requests have been sent on the connection since it opened.
    pub(crate) fn requests(&self) -> u64 {
        self.conn.requests.get()
    }

    /// Starts lending the connection to one task: cancel tokens taken from
    /// now on work until [`end_lease`](Self::end_lease).
    pub(crate) fn begin_lease(&self) {
        *self.conn.lease.borrow_mut() = Some(Arc::new(AtomicBool::new(true)));
    }

    /// Ends the lease: the cancel tokens taken during it stop working.
    pub(crate) fn end_lease(&self) {
        if let Some(lease) = self.conn.lease.borrow_mut().take() {
            lease.store(false, Ordering::Release);
        }
    }

    /// Whether the connection should not be used any more although it works:
    /// the server refused a write because it is a standby.
    pub(crate) fn should_retire(&self) -> bool {
        self.conn.retire.get()
    }

    /// Queues the pool's session reset ([`RESET`]), and `DEALLOCATE ALL` if a
    /// statement ran SQL-level `PREPARE` (the statement cache is emptied with
    /// it). Nobody waits for the result; if it fails, the connection is
    /// closed.
    pub(crate) fn reset_session(&self) {
        let deallocate = self.conn.user_prepared.replace(false);
        let sql = if deallocate {
            self.conn.state.borrow_mut().cache.clear();
            format!("{RESET}; DEALLOCATE ALL")
        } else {
            RESET.to_owned()
        };
        if let Ok(slot) = self
            .conn
            .enqueue_with(Kind::Simple, true, |out| protocol::query(out, &sql))
        {
            slot.inner.borrow_mut().abandoned = true;
        }
    }

    /// Queues a statement whose result nobody waits for.
    pub(crate) fn send_and_forget(&self, sql: &str) {
        if let Ok(slot) = self
            .conn
            .enqueue(Kind::Simple, |out| protocol::query(out, sql))
        {
            slot.inner.borrow_mut().abandoned = true;
        }
    }

    async fn run(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
    ) -> Result<(ResultSet, Option<String>), Error> {
        if params.len() > MAX_PARAMS {
            return Err(Error::conversion(format!(
                "{} parameters were given; a statement can have at most {MAX_PARAMS}",
                params.len()
            )));
        }
        // A cached statement the schema changed under fails before running
        // and is dropped from the cache: it is prepared again and run once
        // more, unless that happened inside a transaction, which the error
        // aborted.
        let mut retried = false;
        loop {
            let slot = self.send(sql, params).await?;
            let result = Wait { slot: slot.clone() }.await;
            if result.is_err() && !retried && slot.inner.borrow().retry {
                retried = true;
                continue;
            }
            return match result? {
                Response::Rows(set, tag) => Ok((set, tag)),
                _ => unreachable!("an execute slot ends with rows"),
            };
        }
    }

    /// Prepares the statement if needed and sends a Bind and Execute for it.
    async fn send(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Rc<Slot>, Error> {
        let stmt = self.conn.statement(sql).await?;
        if stmt.params.len() != params.len() {
            return Err(Error::conversion(format!(
                "the statement has {} parameters but {} values were given",
                stmt.params.len(),
                params.len()
            )));
        }
        let cached = stmt.conn.is_none();
        let slot = self
            .conn
            .enqueue(Kind::Execute { stmt: stmt.clone() }, |out| {
                protocol::bind(
                    out,
                    &stmt.name,
                    params.len(),
                    |i, out| {
                        params[i].to_sql(stmt.params[i], out).map_err(|e| {
                            e.context(&format!("parameter ${} ({})", i + 1, stmt.params[i]))
                        })
                    },
                    stmt.columns.iter().map(|c| c.ty().binary_result()),
                )?;
                protocol::execute(out)?;
                protocol::sync(out);
                Ok(())
            })?;
        if !cached {
            // In a request of its own: after an error the server skips the
            // rest of a request, and the statement must go either way.
            let close = self.conn.enqueue(Kind::Simple, |out| {
                protocol::close_statement(out, &stmt.name)?;
                protocol::sync(out);
                Ok(())
            });
            if let Ok(close) = close {
                close.inner.borrow_mut().abandoned = true;
                stmt.closed.set(true);
            }
        }
        Ok(slot)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut st = self.conn.state.borrow_mut();
        st.closing = true;
        if st.broken.is_none() {
            protocol::terminate(&mut st.out);
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
            .field("in_flight", &self.in_flight())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Conn {
    /// Appends a request's messages and queues its slot, or fails without
    /// sending anything.
    fn enqueue(
        &self,
        kind: Kind,
        encode: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
    ) -> Result<Rc<Slot>, Error> {
        self.enqueue_with(kind, false, encode)
    }

    fn enqueue_with(
        &self,
        kind: Kind,
        close_on_error: bool,
        encode: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
    ) -> Result<Rc<Slot>, Error> {
        let mut st = self.state.borrow_mut();
        if let Some(e) = &st.broken {
            return Err(e.clone());
        }
        let start = st.out.len();
        if let Err(e) = encode(&mut st.out) {
            st.out.truncate(start);
            return Err(e);
        }
        let slot = Slot::new(kind, self.shared.get(), close_on_error);
        self.requests.set(self.requests.get() + 1);
        if st.queue.is_empty() {
            self.head_since.set(Some(giong::time::Instant::now()));
        }
        st.queue.push_back(slot.clone());
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(slot)
    }

    /// The prepared statement for `sql`: from the cache, from a prepare
    /// already in flight, or prepared now.
    async fn statement(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        let mut again = true;
        loop {
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
            match (Wait { slot }).await {
                Ok(Response::Statement(stmt)) => return Ok(stmt),
                Ok(_) => unreachable!("a prepare slot ends with a statement"),
                // The task that sent this prepare tries another name (see
                // `prepare`); look in the cache again.
                Err(e) if again && e.code() == Some("42P05") => again = false,
                Err(e) => return Err(e),
            }
        }
    }

    async fn prepare(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        match self.prepare_once(sql).await {
            // A statement of that name exists already: prepared by the
            // application with SQL-level PREPARE, or by another client behind
            // a connection pooler. The next name will do.
            Err(e) if e.code() == Some("42P05") => self.prepare_once(sql).await,
            result => result,
        }
    }

    async fn prepare_once(&self, sql: &str) -> Result<Rc<Statement>, Error> {
        let cached = self.config.statement_cache > 0;
        let (name, key) = {
            let mut st = self.state.borrow_mut();
            st.next_name += 1;
            (
                format!("{NAME_PREFIX}{}", st.next_name),
                Rc::<str>::from(sql),
            )
        };
        let slot = self.enqueue(
            Kind::Prepare {
                sql: key.clone(),
                name: name.as_str().into(),
                cached,
            },
            |out| {
                protocol::parse(out, &name, sql)?;
                protocol::describe_statement(out, &name)?;
                protocol::sync(out);
                Ok(())
            },
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
        while let Some((tag, len)) = buf.peek()? {
            if len > limit {
                match tag {
                    // One row larger than the limit: skip it as it arrives,
                    // and fail the statement once it completes.
                    b'D' => self.front()?.inner.borrow_mut().rows.set_overflow(),
                    // A notice or a notification (nobody reads them), or
                    // COPY TO STDOUT output (thrown away, and the statement
                    // fails anyway).
                    b'N' | b'A' | b'd' => {}
                    // The statement's error: it fails with this one instead,
                    // and the connection stays usable.
                    b'E' => {
                        let slot = self.front()?;
                        let mut inner = slot.inner.borrow_mut();
                        if inner.error.is_none() {
                            inner.error = Some(Error::new(
                                ErrorKind::TooLarge,
                                format!(
                                    "the server's error for this statement was a {len}-byte \
                                     message, more than max_result_size allows, so it was \
                                     thrown away; raise Config::max_result_size to read it"
                                ),
                            ));
                        }
                    }
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            format!(
                                "the server sent a {:?} message of {len} bytes, more than \
                                 max_result_size allows, so the connection was closed; raise \
                                 Config::max_result_size if the server's messages are this large",
                                tag as char
                            ),
                        ))
                    }
                }
                buf.skip(len);
                continue;
            }
            let Some(body) = buf.body(len) else {
                return Ok(());
            };
            self.handle(tag, buf.bytes(body))?;
            buf.consume(len);
        }
        Ok(())
    }

    fn front(&self) -> Result<Rc<Slot>, Error> {
        self.state.borrow().queue.front().cloned().ok_or_else(|| {
            Error::protocol("the server sent a reply to a request that was never made")
        })
    }

    fn handle(&self, tag: u8, body: &[u8]) -> Result<(), Error> {
        let msg = parse_backend(tag, body)?;
        match msg {
            Backend::NoticeResponse | Backend::NotificationResponse => return Ok(()),
            Backend::ParameterStatus { name, value } => {
                if name == "client_encoding" && !value.eq_ignore_ascii_case("UTF8") {
                    return Err(Error::closed(format!(
                        "the session's client_encoding was changed to {value}; giong-postgres \
                         reads and writes UTF-8 only, so the connection was closed"
                    )));
                }
                let mut st = self.state.borrow_mut();
                match st.server_params.iter_mut().find(|(n, _)| n == name) {
                    Some(entry) => entry.1 = value.to_owned(),
                    None => st.server_params.push((name.to_owned(), value.to_owned())),
                }
                return Ok(());
            }
            Backend::ErrorResponse(fields) if self.state.borrow().queue.is_empty() => {
                // A FATAL error outside any request: the server is closing
                // the connection (an administrator's command, a shutdown).
                let db = DbError::parse(fields)?;
                return Err(Error::with_db(
                    ErrorKind::Closed,
                    "the server closed the connection",
                    db,
                ));
            }
            _ => {}
        }
        let slot = self.front()?;
        let mut inner = slot.inner.borrow_mut();
        match msg {
            Backend::BindComplete => inner.bound = true,
            Backend::ParseComplete
            | Backend::CloseComplete
            | Backend::NoData
            | Backend::PortalSuspended
            | Backend::EmptyQueryResponse => {}
            Backend::ParameterDescription(types) => inner.params = Some(types),
            Backend::RowDescription(fields) => {
                if matches!(slot.kind, Kind::Prepare { .. }) {
                    inner.columns = Some(
                        fields
                            .iter()
                            .map(|f| Column::new(f.name, Type::from_oid(f.type_oid)))
                            .collect(),
                    );
                }
            }
            Backend::DataRow(body) => match &slot.kind {
                Kind::Execute { stmt } if !inner.abandoned && inner.error.is_none() => {
                    let limit = self.config.max_result_size;
                    inner.rows.push(body, stmt.columns.len(), limit)?;
                }
                _ => {}
            },
            Backend::CommandComplete(tag) => {
                if tag == "PREPARE" {
                    self.user_prepared.set(true);
                }
                inner.tag = Some(tag.to_owned());
            }
            Backend::ErrorResponse(fields) => {
                if inner.error.is_none() {
                    let db = DbError::parse(fields)?;
                    if db.code() == "25006" && self.on_standby() {
                        self.retire.set(true);
                    }
                    // Only a note: the message is translated on servers whose
                    // lc_messages is not English, and then there is none.
                    let note = (db.code() == "57014" && db.message().contains("statement timeout"))
                        .then(|| match self.config.statement_timeout {
                            Some(limit) => format!(
                                "the statement ran longer than the statement timeout ({limit:?}; \
                                 Config::statement_timeout)"
                            ),
                            None => "the statement ran longer than the server's statement_timeout"
                                .to_owned(),
                        });
                    inner.error = Some(Error::from_db(db, note));
                }
            }
            Backend::CopyOutResponse | Backend::CopyData | Backend::CopyDone => {
                if inner.error.is_none() {
                    inner.error = Some(Error::conversion(
                        "COPY ... TO STDOUT is not supported by giong-postgres yet; the output \
                         was thrown away",
                    ));
                }
            }
            Backend::CopyInResponse | Backend::CopyBothResponse => {
                return Err(Error::closed(
                    "the statement started COPY FROM STDIN or replication, which giong-postgres \
                     does not support yet; the connection was closed",
                ));
            }
            Backend::ReadyForQuery(status) => {
                // A statement sent on a shared connection opened a
                // transaction (or left a failed one): the statements other
                // tasks send next would run inside it, and be lost with it.
                let left_open = slot.shared && status != b'I';
                if left_open {
                    inner.error = Some(Error::closed(LEFT_OPEN));
                }
                let reset_failed = match &inner.error {
                    Some(e) if slot.close_on_error => Some(Error::closed(format!(
                        "resetting the session of a connection that went back to the pool \
                         failed, so it was closed rather than reused: {e}"
                    ))),
                    _ => None,
                };
                inner.status = status;
                drop(inner);
                let idle = {
                    let mut st = self.state.borrow_mut();
                    st.status = status;
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
                if let Some(e) = reset_failed {
                    return Err(e);
                }
                if idle {
                    self.wake_idle();
                }
            }
            other => {
                return Err(Error::protocol(format!(
                    "the server sent an unexpected {:?} message ({other:?})",
                    tag as char
                )))
            }
        }
        Ok(())
    }

    /// Works out a finished request's outcome and wakes whoever waits.
    fn complete(&self, slot: &Rc<Slot>) {
        let mut inner = slot.inner.borrow_mut();
        let error = inner.error.take();
        let outcome = match &slot.kind {
            Kind::Prepare { sql, name, cached } => {
                let result = match error {
                    Some(e) => Err(e),
                    None => match inner.params.take() {
                        Some(params) => Ok(Rc::new(Statement {
                            sql: sql.clone(),
                            name: name.clone(),
                            params,
                            columns: inner.columns.take().unwrap_or_default().into(),
                            conn: (!*cached).then(|| self.weak.clone()),
                            closed: Cell::new(false),
                        })),
                        None => Err(Error::protocol("the server did not describe the statement")),
                    },
                };
                if *cached {
                    self.settle_cache(sql, slot, &result);
                }
                result.map(Response::Statement)
            }
            Kind::Execute { stmt } => match error {
                Some(e) => {
                    let stale = self.forget_if_stale(stmt, &e);
                    // Failed in Bind (nothing ran) and outside a transaction
                    // (none was aborted): safe to run again.
                    inner.retry = stale && !inner.bound && inner.status == b'I';
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
                    Ok(Response::Rows(
                        rows.finish(stmt.columns.clone()),
                        inner.tag.take(),
                    ))
                }
            },
            Kind::Simple => match error {
                Some(e) => Err(e),
                None => Ok(Response::Done(inner.tag.take())),
            },
        };
        inner.outcome = Some(outcome);
        for w in inner.wakers.drain(..) {
            w.wake();
        }
    }

    /// Takes a cached statement out of the cache when the server says it no
    /// longer fits: the schema changed under it, or something deallocated it.
    /// The next run prepares it afresh. Returns whether the error was one of
    /// those.
    fn forget_if_stale(&self, stmt: &Statement, e: &Error) -> bool {
        if !e.db().is_some_and(is_stale) {
            return false;
        }
        let mut st = self.state.borrow_mut();
        if matches!(st.cache.get(&stmt.sql), Some(Entry::Ready { stmt: s, .. }) if s.name == stmt.name)
        {
            st.cache.remove(&stmt.sql);
        }
        true
    }

    /// Whether the server says it is a standby, or does not say (servers
    /// before PostgreSQL 14 do not report `in_hot_standby`).
    fn on_standby(&self) -> bool {
        let st = self.state.borrow();
        st.server_params
            .iter()
            .find(|(n, _)| n == "in_hot_standby")
            .map_or(true, |(_, v)| v != "off")
    }

    /// Puts a prepared statement in the cache (or takes out a failed one),
    /// closing the least recently used statements when it is full.
    fn settle_cache(&self, sql: &Rc<str>, slot: &Rc<Slot>, result: &Result<Rc<Statement>, Error>) {
        let mut st = self.state.borrow_mut();
        let ours = matches!(st.cache.get(sql), Some(Entry::Preparing(s)) if Rc::ptr_eq(s, slot));
        if !ours {
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
                // No Sync: the close travels with the next request, whose
                // reply includes the CloseComplete.
                let _ = protocol::close_statement(&mut st.out, &stmt.name);
            }
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
    /// When a statement in the transaction failed earlier, PostgreSQL rolls
    /// the transaction back instead; this is reported as an error, never as
    /// a successful commit.
    pub async fn commit(mut self) -> Result<(), Error> {
        self.done = true;
        let slot = self
            .client
            .conn
            .enqueue(Kind::Simple, |out| protocol::query(out, "COMMIT"))?;
        match (Wait { slot }).await? {
            Response::Done(Some(tag)) if tag == "ROLLBACK" => Err(Error::with_sqlstate(
                "25P02",
                "the transaction was rolled back instead of committed, because a statement in \
                 it failed earlier",
            )),
            _ => Ok(()),
        }
    }

    /// Rolls the transaction back and waits for the server to confirm.
    pub async fn rollback(mut self) -> Result<(), Error> {
        self.done = true;
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

/// Asks the server to cancel what a connection is running.
///
/// It can be sent to another thread. The request goes over a new connection,
/// as the protocol requires; the statement being cancelled fails with SQLSTATE
/// `57014`. With several tasks sharing the connection, the statement running
/// when the request arrives is the one cancelled.
#[derive(Clone)]
pub struct CancelToken {
    /// Where to send the request; without the password, which it does not
    /// need.
    config: Config,
    key: Option<(i32, i32)>,
    /// Set for a token taken from a pooled connection: cleared when the
    /// connection goes back to the pool.
    lease: Option<Arc<AtomicBool>>,
}

impl CancelToken {
    /// Sends the cancel request. Success means the server received it, not
    /// that something was cancelled: the statement may have finished first.
    ///
    /// Fails with [`ErrorKind::Closed`], sending nothing, when the token was
    /// taken from a [`PooledClient`](crate::PooledClient) that has gone back
    /// to its pool since.
    pub async fn cancel(&self) -> Result<(), Error> {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| !lease.load(Ordering::Acquire))
        {
            return Err(Error::closed(
                "the connection this cancel token was taken from has gone back to its pool, \
                 where other tasks use it, so nothing was sent; take a new token from the next \
                 connection",
            ));
        }
        let key = self.key.ok_or_else(|| {
            Error::new(
                ErrorKind::Connect,
                "the server gave this connection no cancel key (a connection pooler may be in \
                 between), so it cannot be cancelled",
            )
        })?;
        connect::cancel(&self.config, key).await
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("server", &self.config.target())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{is_stale, rows_affected};
    use crate::error::DbError;

    fn db_error(fields: &[(u8, &str)]) -> DbError {
        let mut body = Vec::new();
        for (field, value) in fields {
            body.push(*field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        DbError::parse(&body).unwrap()
    }

    #[test]
    fn a_stale_plan_is_recognised_in_any_language() {
        let german = db_error(&[
            (b'S', "FEHLER"),
            (b'V', "ERROR"),
            (b'C', "0A000"),
            (b'M', "gecachter Plan darf den Ergebnistyp nicht ändern"),
            (b'R', "RevalidateCachedQuery"),
        ]);
        assert!(is_stale(&german));
        assert_eq!(german.severity(), "ERROR");
        // A server that sends no routine, in English.
        let english = db_error(&[
            (b'C', "0A000"),
            (b'M', "cached plan must not change result type"),
        ]);
        assert!(is_stale(&english));
        // Another "feature not supported" error.
        let other = db_error(&[
            (b'C', "0A000"),
            (b'M', "nicht unterstützt"),
            (b'R', "transformSelectStmt"),
        ]);
        assert!(!is_stale(&other));
        assert!(is_stale(&db_error(&[(b'C', "26000"), (b'M', "x")])));
    }

    #[test]
    fn command_tags_give_row_counts() {
        assert_eq!(rows_affected(Some("INSERT 0 5")), 5);
        assert_eq!(rows_affected(Some("UPDATE 3")), 3);
        assert_eq!(rows_affected(Some("SELECT 12")), 12);
        assert_eq!(rows_affected(Some("CREATE TABLE")), 0);
        assert_eq!(rows_affected(None), 0);
    }
}
