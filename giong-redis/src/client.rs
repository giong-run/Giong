//! One connection, shared by the tasks of a worker: their commands are
//! written together and the replies matched to them in order.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::net::Shutdown;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use giong::time::{Instant, Sleep};

use crate::cmd::{cmd, Cmd, Pipeline};
use crate::commands::commands;
use crate::config::Config;
use crate::connect::{self, Server, Startup, Stream};
use crate::error::{Error, ErrorKind, ServerError};
use crate::resp::{Next, ReadBuf};
use crate::value::{FromValue, ToArg, Value};

/// A connection to a Redis server.
///
/// ```no_run
/// use giong_redis::{Client, Config};
///
/// fn main() -> Result<(), giong_redis::Error> {
///     let config = Config::from_url("redis://localhost:6379")?;
///     giong::block_on(async {
///         let client = Client::connect(&config).await?;
///         client.set("greeting", "hello").await?;
///         let greeting: Option<String> = client.get("greeting").await?;
///         assert_eq!(greeting.as_deref(), Some("hello"));
///         Ok(())
///     })
/// }
/// ```
///
/// **Shared by many tasks.** Its methods take `&self`, so every task of a
/// worker can use one client (behind an `Rc`). Commands sent by different
/// tasks at the same time are written together and the server answers them
/// in order: one round trip, one system call, for all of them. This is
/// where most of the speed comes from, so share a client, or use a
/// [`Pool`](crate::Pool), rather than opening one per task.
///
/// For the same reason, commands that would change the connection for the
/// other tasks are refused with [`ErrorKind::Command`] before anything is
/// sent, with the way to do it instead: `MULTI`/`EXEC` (use
/// [`Pipeline::atomic`]), `WATCH` (use [`watch`](Self::watch)), `SELECT`
/// and `AUTH` (set in the [`Config`]), `SUBSCRIBE` (use
/// [`PubSub`](crate::PubSub)), and blocking commands such as `BLPOP` (use
/// [`query_blocking`](Self::query_blocking)).
///
/// The connection speaks RESP3, so it needs Redis 6 or later, or Valkey. A
/// client lives on the worker thread that opened it. Dropping it closes the
/// connection once the commands already sent have been written.
pub struct Client {
    conn: Rc<Conn>,
}

/// The state shared by a client and its two tasks.
struct Conn {
    stream: Stream,
    state: RefCell<State>,
    /// When the oldest unanswered command became the oldest; `None` with
    /// nothing in flight.
    head_since: Cell<Option<Instant>>,
    /// How many slots at the front of the queue are known not to be
    /// waiting for their deadline any more: timed out, or without one.
    expired: Cell<usize>,
    config: Config,
    server: Server,
}

struct State {
    /// Commands waiting to be written.
    out: Vec<u8>,
    writer: Option<Waker>,
    /// One entry per command (or pipeline) sent, in order.
    queue: VecDeque<Rc<Slot>>,
    /// Why the connection stopped working, once it has.
    broken: Option<Error>,
    /// The client was dropped: write what is queued, then close.
    closing: bool,
    /// The read loop, parked with no deadline to watch: woken when a command
    /// with one is queued.
    reader: Option<Waker>,
}

/// What a slot waits for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One command.
    One,
    /// The commands of a pipeline.
    Pipeline(usize),
    /// `MULTI`, the commands, `EXEC`.
    Atomic(usize),
    /// Replies nobody waits for.
    Discard(usize),
}

impl Shape {
    fn replies(self) -> usize {
        match self {
            Shape::One => 1,
            Shape::Pipeline(n) | Shape::Discard(n) => n,
            Shape::Atomic(n) => n + 2,
        }
    }
}

/// Commands in flight and what has arrived for them.
struct Slot {
    shape: Shape,
    /// A blocking command, sent with `query_blocking`.
    blocking: bool,
    /// When the reply is due: the response timeout after the command was
    /// queued. `None` for blocking commands and replies nobody waits for.
    ///
    /// Every connection has one response timeout, and commands are queued
    /// in order, so the deadlines in the queue only ever go up: one timer
    /// per connection, for the oldest, is enough.
    deadline: Option<Instant>,
    inner: RefCell<SlotInner>,
}

#[derive(Default)]
struct SlotInner {
    received: usize,
    /// The reply of a single command (`Shape::One`), kept without a `Vec`.
    one: Option<Value>,
    /// The replies of a pipeline.
    replies: Vec<Value>,
    /// A reply was too large.
    failed: Option<Error>,
    waker: Option<Waker>,
    /// Nobody waits for the outcome any more: replies are not kept.
    abandoned: bool,
    /// The deadline passed before the reply came.
    timed_out: bool,
    outcome: Option<Result<Value, Error>>,
}

/// Waits for a slot's outcome, `None` when its deadline passed first.
/// Dropping it lets the reader throw the replies away; the commands still
/// run.
struct Wait {
    slot: Rc<Slot>,
}

impl Future for Wait {
    type Output = Option<Result<Value, Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.slot.inner.borrow_mut();
        if let Some(outcome) = inner.outcome.take() {
            return Poll::Ready(Some(outcome));
        }
        if inner.timed_out {
            return Poll::Ready(None);
        }
        match &inner.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => inner.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let mut inner = self.slot.inner.borrow_mut();
        inner.abandoned = true;
        inner.one = None;
        inner.replies = Vec::new();
        inner.outcome = None;
    }
}

impl Client {
    /// Opens a connection and logs in (`HELLO 3`, then `SELECT` when
    /// [`Config::database`] is not 0).
    ///
    /// Fails with [`ErrorKind::Connect`] when the server cannot be reached
    /// within [`Config::connect_timeout`], and with [`ErrorKind::Auth`] when
    /// it refuses the credentials.
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
            server,
        } = startup;
        let conn = Rc::new(Conn {
            stream,
            head_since: Cell::new(None),
            expired: Cell::new(0),
            config,
            server,
            state: RefCell::new(State {
                out: Vec::new(),
                writer: None,
                queue: VecDeque::new(),
                broken: None,
                closing: false,
                reader: None,
            }),
        });
        giong::spawn(read_loop(conn.clone(), buf));
        giong::spawn(write_loop(conn.clone()));
        Client { conn }
    }

    /// Sends a command and converts its reply to `T`:
    ///
    /// ```no_run
    /// use giong_redis::cmd;
    /// # async fn f(client: &giong_redis::Client) -> Result<(), giong_redis::Error> {
    /// client.query::<()>(&cmd("SET").arg("job:9").arg("queued").arg("EX").arg(600)).await?;
    /// let ranks: Vec<(String, f64)> = client
    ///     .query(&cmd("ZRANGE").arg("leaders").arg(0).arg(9).arg("REV").arg("WITHSCORES"))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// An error reply is returned as an [`ErrorKind::Server`] error with the
    /// server's code ([`Error::server_code`]). The reply must arrive within
    /// [`Config::response_timeout`].
    pub async fn query<T: FromValue>(&self, cmd: &Cmd) -> Result<T, Error> {
        cmd.check(false, false)?;
        let value = self
            .conn
            .send(cmd.name(), Shape::One, true, |out| cmd.encode(out))
            .await?;
        T::from_value(value)
    }

    /// Sends a command the crate builds itself (the command methods), known
    /// to be safe on a shared connection.
    pub(crate) async fn query_raw<T: FromValue>(
        &self,
        name: &'static str,
        args: &[&dyn ToArg],
    ) -> Result<T, Error> {
        let value = self
            .conn
            .send(name, Shape::One, true, |out| {
                crate::cmd::encode_raw(out, name, args)
            })
            .await?;
        T::from_value(value)
    }

    /// Sends a command that may block until it returns, such as `BLPOP`,
    /// `BLMOVE` or `XREAD ... BLOCK`, without the response timeout: its own
    /// timeout argument bounds it.
    ///
    /// It takes `&mut self` because nothing else can run on the connection
    /// while the command blocks: use a connection the task has to itself,
    /// from [`Pool::take`](crate::Pool::take) or of its own.
    ///
    /// ```no_run
    /// use giong_redis::cmd;
    /// # async fn f(pool: &giong_redis::Pool) -> Result<(), giong_redis::Error> {
    /// let mut conn = pool.take().await?;
    /// let job: Option<(String, String)> =
    ///     conn.query_blocking(&cmd("BLPOP").arg("jobs").arg(5)).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// If this is dropped before the reply comes, the command keeps the
    /// connection blocked until the server answers; a pooled connection is
    /// then closed rather than given back.
    pub async fn query_blocking<T: FromValue>(&mut self, cmd: &Cmd) -> Result<T, Error> {
        cmd.check(true, false)?;
        let value = self
            .conn
            .send(cmd.name(), Shape::One, false, |out| cmd.encode(out))
            .await?;
        T::from_value(value)
    }

    /// Sends the commands of a pipeline together and returns their replies
    /// as an array, converted to `T`: a tuple, a `Vec`, or `Vec<Value>` to
    /// see each command's error apart.
    ///
    /// ```no_run
    /// use giong_redis::{cmd, Pipeline};
    /// # async fn f(client: &giong_redis::Client) -> Result<(), giong_redis::Error> {
    /// let (visits, _): (i64, bool) = client
    ///     .pipeline(
    ///         &Pipeline::new()
    ///             .add(cmd("INCR").arg("visits"))
    ///             .add(cmd("EXPIRE").arg("visits").arg(60)),
    ///     )
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// An [atomic](Pipeline::atomic) pipeline is a transaction: see there.
    pub async fn pipeline<T: FromValue>(&self, pipe: &Pipeline) -> Result<T, Error> {
        let atomic = pipe.is_atomic();
        pipe.check(atomic)?;
        if pipe.is_empty() {
            return T::from_value(Value::Array(Vec::new()));
        }
        let shape = if atomic {
            Shape::Atomic(pipe.len())
        } else {
            Shape::Pipeline(pipe.len())
        };
        let value = self
            .conn
            .send("the pipeline", shape, true, |out| pipe.encode(out, atomic))
            .await?;
        T::from_value(value)
    }

    /// Watches keys for an optimistic transaction (`WATCH`): read them
    /// through the [`Watch`], then [`exec`](Watch::exec) a pipeline, which
    /// runs only if no other client changed them in between.
    ///
    /// ```no_run
    /// use giong_redis::{cmd, Pipeline};
    /// # async fn f(pool: &giong_redis::Pool) -> Result<(), giong_redis::Error> {
    /// let mut conn = pool.take().await?;
    /// loop {
    ///     let watch = conn.watch(&["stock:42"]).await?;
    ///     let stock: i64 = watch.query(&cmd("GET").arg("stock:42")).await?;
    ///     if stock == 0 {
    ///         break;
    ///     }
    ///     let pipe = Pipeline::new().add(cmd("SET").arg("stock:42").arg(stock - 1));
    ///     if watch.exec::<()>(&pipe).await?.is_some() {
    ///         break; // done; None means the key changed: try again
    ///     }
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// It takes `&mut self`, since `WATCH` holds for the whole connection:
    /// use a connection the task has to itself. Dropping the `Watch` without
    /// `exec` sends `UNWATCH`.
    pub async fn watch<K: ToArg>(&mut self, keys: &[K]) -> Result<Watch<'_>, Error> {
        if keys.is_empty() {
            return Err(Error::command("WATCH needs at least one key"));
        }
        let watch = cmd("WATCH").args(keys);
        self.conn
            .send("WATCH", Shape::One, true, |out| watch.encode(out))
            .await?;
        Ok(Watch {
            client: self,
            done: false,
        })
    }

    /// The server's version, from its answer to `HELLO`.
    pub fn server_version(&self) -> &str {
        &self.conn.server.version
    }

    /// The server's name, from its answer to `HELLO`: `redis` or `valkey`.
    pub fn server_name(&self) -> &str {
        &self.conn.server.name
    }

    /// The connection's id on the server (`CLIENT ID`).
    pub fn connection_id(&self) -> i64 {
        self.conn.server.id
    }

    /// Whether the connection is closed. A closed client fails every
    /// command with [`ErrorKind::Closed`]; open a new one.
    pub fn is_closed(&self) -> bool {
        self.conn.state.borrow().broken.is_some()
    }

    /// Commands (and pipelines) sent and not answered yet.
    pub fn in_flight(&self) -> usize {
        self.conn.state.borrow().queue.len()
    }

    /// Whether a blocking command is in flight: one whose
    /// `query_blocking` was cancelled, since the task is still in it
    /// otherwise.
    pub(crate) fn is_blocked(&self) -> bool {
        self.conn.state.borrow().queue.iter().any(|s| s.blocking)
    }

    /// When the oldest command in flight became the oldest, if there is one.
    pub(crate) fn head_since(&self) -> Option<Instant> {
        self.conn.head_since.get()
    }

    commands!();
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut st = self.conn.state.borrow_mut();
        st.closing = true;
        if let Some(w) = st.writer.take() {
            w.wake();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.conn.config.target())
            .field("version", &self.conn.server.version)
            .field("connection_id", &self.conn.server.id)
            .field("in_flight", &self.in_flight())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Conn {
    /// Queues a command (or several) and waits for the outcome, within the
    /// response timeout when `timed`.
    async fn send(
        &self,
        what: &str,
        shape: Shape,
        timed: bool,
        encode: impl FnOnce(&mut Vec<u8>),
    ) -> Result<Value, Error> {
        let slot = self.enqueue(shape, !timed, encode)?;
        match (Wait { slot }).await {
            Some(outcome) => outcome,
            None => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "{what} got no reply from {} within the response timeout of {:?}; it may \
                     still run. Raise Config::response_timeout, or look for slow commands with \
                     SLOWLOG GET",
                    self.config.target(),
                    self.config.response_timeout
                ),
            )),
        }
    }

    fn enqueue(
        &self,
        shape: Shape,
        blocking: bool,
        encode: impl FnOnce(&mut Vec<u8>),
    ) -> Result<Rc<Slot>, Error> {
        let mut st = self.state.borrow_mut();
        if let Some(e) = &st.broken {
            return Err(Error::closed(format!(
                "the connection to {} is closed ({})",
                self.config.target(),
                e
            )));
        }
        encode(&mut st.out);
        let now = Instant::now();
        let discard = matches!(shape, Shape::Discard(_));
        // An absurd timeout that overflows the clock means none.
        let deadline = if blocking || discard {
            None
        } else {
            now.checked_add(self.config.response_timeout)
        };
        let slot = Rc::new(Slot {
            shape,
            blocking,
            deadline,
            inner: RefCell::new(SlotInner {
                abandoned: discard,
                ..SlotInner::default()
            }),
        });
        if st.queue.is_empty() {
            self.head_since.set(Some(now));
        }
        st.queue.push_back(slot.clone());
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        if deadline.is_some() {
            if let Some(w) = st.reader.take() {
                w.wake();
            }
        }
        Ok(slot)
    }

    /// Breaks the connection: every command in flight fails with `err`.
    fn fail(&self, err: Error) {
        let slots = {
            let mut st = self.state.borrow_mut();
            if st.broken.is_some() {
                return;
            }
            st.broken = Some(err.clone());
            if let Some(w) = st.writer.take() {
                w.wake();
            }
            std::mem::take(&mut st.queue)
        };
        self.head_since.set(None);
        self.expired.set(0);
        for slot in slots {
            let mut inner = slot.inner.borrow_mut();
            inner.outcome = Some(Err(err.clone()));
            if let Some(w) = inner.waker.take() {
                w.wake();
            }
        }
        self.stream.shutdown(Shutdown::Both);
    }

    /// Fails the commands whose deadline has passed; returns the next
    /// deadline, if a command is still waiting for one.
    fn expire(&self, now: Instant) -> Option<Instant> {
        let st = self.state.borrow();
        let mut expired = self.expired.get();
        let mut next = None;
        while let Some(slot) = st.queue.get(expired) {
            match slot.deadline {
                Some(due) if due > now => {
                    next = Some(due);
                    break;
                }
                Some(_) => {
                    let mut inner = slot.inner.borrow_mut();
                    if !inner.abandoned {
                        // Its reply, when it comes, is thrown away.
                        inner.abandoned = true;
                        inner.timed_out = true;
                        inner.one = None;
                        inner.replies = Vec::new();
                        if let Some(w) = inner.waker.take() {
                            w.wake();
                        }
                    }
                }
                None => {}
            }
            expired += 1;
        }
        self.expired.set(expired);
        next
    }

    /// Keeps the connection's timer on the oldest deadline still ahead,
    /// failing the commands whose deadline has passed. Called by the read
    /// loop before it waits.
    ///
    /// The timer is moved on lazily: when the command it was set for is
    /// answered, it stays where it was, and when it fires it is set for the
    /// oldest command waiting then. A busy connection so sets one timer
    /// per response timeout, not one per command.
    fn watch_deadlines(&self, timer: &mut Option<Sleep>, cx: &mut Context<'_>) {
        loop {
            if let Some(t) = timer.as_mut() {
                if Pin::new(t).poll(cx).is_pending() {
                    return;
                }
            }
            match self.expire(Instant::now()) {
                Some(due) => match timer.as_mut() {
                    Some(t) => t.reset(due),
                    None => *timer = Some(giong::time::sleep_until(due)),
                },
                None => {
                    *timer = None;
                    self.state.borrow_mut().reader = Some(cx.waker().clone());
                    return;
                }
            }
        }
    }

    /// Matches every whole reply in `buf` to the command it answers.
    fn process(&self, buf: &mut ReadBuf) -> Result<(), Error> {
        // The clock, read once for all the replies of this chunk.
        let mut now = None;
        loop {
            let next = buf.next(self.config.max_reply_size)?;
            if let Next::Pending = next {
                return Ok(());
            }
            if let Next::Ready(n) = next {
                if buf.is_push() {
                    // Pub/Sub messages and tracking invalidations are for
                    // PubSub; a client has no use for them.
                    buf.skip(n);
                    continue;
                }
            }
            let slot = self
                .state
                .borrow()
                .queue
                .front()
                .cloned()
                .ok_or_else(|| Error::protocol("the server sent a reply nobody asked for"))?;
            let done = {
                let mut inner = slot.inner.borrow_mut();
                match next {
                    Next::Ready(n) if inner.abandoned => buf.skip(n),
                    Next::Ready(n) => {
                        let v = buf.take(n)?;
                        match slot.shape {
                            Shape::One => inner.one = Some(v),
                            shape => {
                                if inner.replies.capacity() == 0 {
                                    inner.replies.reserve_exact(shape.replies());
                                }
                                inner.replies.push(v);
                            }
                        }
                    }
                    Next::TooLarge => {
                        if inner.failed.is_none() && !inner.abandoned {
                            inner.failed = Some(Error::new(
                                ErrorKind::TooLarge,
                                format!(
                                    "a reply was larger than the limit of {} bytes and was thrown \
                                     away; raise Config::max_reply_size, or ask for less (SCAN, \
                                     GETRANGE, LRANGE with bounds)",
                                    self.config.max_reply_size
                                ),
                            ));
                        }
                    }
                    Next::Pending => unreachable!("handled above"),
                }
                inner.received += 1;
                inner.received == slot.shape.replies()
            };
            if done {
                self.complete(&slot, &mut now);
            }
        }
    }

    /// Takes the finished slot off the queue and hands its outcome over.
    fn complete(&self, slot: &Rc<Slot>, now: &mut Option<Instant>) {
        {
            let mut st = self.state.borrow_mut();
            st.queue.pop_front();
            self.expired.set(self.expired.get().saturating_sub(1));
            self.head_since
                .set((!st.queue.is_empty()).then(|| *now.get_or_insert_with(Instant::now)));
        }
        let mut inner = slot.inner.borrow_mut();
        if inner.abandoned {
            return;
        }
        let outcome = match (inner.failed.take(), inner.one.take()) {
            (Some(e), _) => Err(e),
            (None, Some(Value::Error(e))) => Err(server_error(e)),
            (None, Some(v)) => Ok(v),
            (None, None) => outcome(slot.shape, std::mem::take(&mut inner.replies)),
        };
        inner.outcome = Some(outcome);
        if let Some(w) = inner.waker.take() {
            w.wake();
        }
    }
}

/// An error reply as an error, with a hint for those that need one.
pub(crate) fn server_error(e: ServerError) -> Error {
    let hint = match e.code() {
        "MOVED" | "ASK" => {
            "the key is served by another node of a Redis Cluster, which giong-redis does not \
             support yet; connect to a server that is not part of a cluster"
        }
        "READONLY" => "the server is a replica; connect to the primary to write",
        "NOSCRIPT" => "load the script with SCRIPT LOAD, or send it with EVAL",
        _ => "",
    };
    Error::with_server(ErrorKind::Server, hint, e)
}

/// The outcome of a pipeline's replies (a single command's is found in
/// `complete`).
fn outcome(shape: Shape, mut replies: Vec<Value>) -> Result<Value, Error> {
    match shape {
        // Not reached: `complete` finds a single command's reply itself.
        Shape::One => Err(Error::protocol("a reply went missing")),
        Shape::Pipeline(_) => Ok(Value::Array(replies)),
        Shape::Atomic(n) => {
            let exec = replies.pop().expect("EXEC's reply");
            if let Value::Error(e) = &replies[0] {
                return Err(server_error(e.clone()));
            }
            match exec {
                Value::Error(e) => {
                    // EXECABORT: say which command was refused.
                    let queued = replies[1..].iter().enumerate().find_map(|(i, r)| match r {
                        Value::Error(q) => Some((i + 1, q.clone())),
                        _ => None,
                    });
                    Err(match queued {
                        Some((i, q)) => Error::with_server(
                            ErrorKind::Server,
                            format!(
                                "the transaction did not run: the server refused command {i} \
                                 of {n} as it was queued"
                            ),
                            q,
                        ),
                        None => server_error(e),
                    })
                }
                v => Ok(v),
            }
        }
        Shape::Discard(_) => Ok(Value::Nil),
    }
}

async fn read_loop(conn: Rc<Conn>, mut buf: ReadBuf) {
    let mut timer = None;
    let err = loop {
        if let Err(e) = conn.process(&mut buf) {
            break e;
        }
        let received = {
            let mut recv = pin!(conn.stream.recv());
            poll_fn(|cx| {
                if let Poll::Ready(r) = recv.as_mut().poll(cx) {
                    return Poll::Ready(r);
                }
                conn.watch_deadlines(&mut timer, cx);
                Poll::Pending
            })
            .await
        };
        match received {
            Ok(chunk) if chunk.is_empty() => {
                let closing = conn.state.borrow().closing;
                break Error::closed(if closing {
                    "the connection was closed".to_owned()
                } else {
                    format!(
                        "the server at {} closed the connection",
                        conn.config.target()
                    )
                });
            }
            Ok(chunk) => buf.extend(&chunk),
            Err(e) => {
                break Error::io(
                    ErrorKind::Closed,
                    format!("reading from {} failed", conn.config.target()),
                    e,
                )
            }
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
                format!("writing to {} failed", conn.config.target()),
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

/// Keys watched for an optimistic transaction, from [`Client::watch`].
///
/// Read through it with [`query`](Self::query), then
/// [`exec`](Self::exec) a pipeline. Dropping it without `exec` sends
/// `UNWATCH`.
pub struct Watch<'a> {
    client: &'a mut Client,
    done: bool,
}

impl Watch<'_> {
    /// Sends a command on the watching connection; see [`Client::query`].
    pub async fn query<T: FromValue>(&self, cmd: &Cmd) -> Result<T, Error> {
        self.client.query(cmd).await
    }

    /// Runs the pipeline as a transaction (`MULTI` ... `EXEC`, whether or
    /// not it was made [atomic](Pipeline::atomic)) if no watched key
    /// changed since [`Client::watch`]: `Some` with `EXEC`'s array, or
    /// `None` when a key changed and nothing ran.
    pub async fn exec<T: FromValue>(mut self, pipe: &Pipeline) -> Result<Option<T>, Error> {
        pipe.check(true)?;
        // EXEC ends the watch whatever happens next.
        self.done = true;
        let value = self
            .client
            .conn
            .send("the transaction", Shape::Atomic(pipe.len()), true, |out| {
                pipe.encode(out, true)
            })
            .await?;
        match value {
            Value::Nil => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

impl Drop for Watch<'_> {
    fn drop(&mut self) {
        if !self.done {
            let unwatch = cmd("UNWATCH");
            // Queued ahead of anything the connection sends next.
            let _ = self
                .client
                .conn
                .enqueue(Shape::Discard(1), false, |out| unwatch.encode(out));
        }
    }
}

impl std::fmt::Debug for Watch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("client", &self.client)
            .finish()
    }
}
