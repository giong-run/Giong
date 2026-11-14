//! A pool of connections for one worker thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
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
    }
}

#[derive(Default)]
struct State {
    /// Open connections that are not taken, lent to shared commands.
    shared: Vec<Rc<Client>>,
    /// Open connections set aside for the tasks waiting in `take`: no new
    /// shared command is sent on them, so they become free as soon as the
    /// commands already sent are answered.
    draining: Vec<Rc<Client>>,
    /// The tasks waiting in `take` with the pool full, by ticket, oldest
    /// first. Only the first may take a connection.
    takers: VecDeque<u64>,
    next_ticket: u64,
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
}

impl State {
    /// The connections that are not taken.
    fn untaken(&self) -> impl Iterator<Item = &Rc<Client>> {
        self.shared.iter().chain(&self.draining)
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
            let Some(i) = (0..self.shared.len()).min_by_key(|&i| Rc::strong_count(&self.shared[i]))
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
    fn take_idle(&mut self) -> Option<Client> {
        let idle = |v: &Vec<Rc<Client>>| v.iter().position(|c| Rc::strong_count(c) == 1);
        let c = if let Some(i) = idle(&self.draining) {
            self.draining.swap_remove(i)
        } else {
            let i = idle(&self.shared)?;
            self.shared.swap_remove(i)
        };
        self.taken += 1;
        match Rc::try_unwrap(c) {
            Ok(client) => Some(client),
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
    /// [`ErrorKind::PoolTimeout`]. `Duration::MAX` waits as long as it
    /// takes.
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

    /// Sends a command on a shared connection; see [`Client::query`].
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
                    if st.shared.len() + st.draining.len() + st.taken + st.connecting < max {
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
    /// A connection whose oldest command has waited more than [`STALLED`]
    /// is passed over while another will do: commands queue behind it in
    /// order, so a slow one would hold up everything sent after it on that
    /// connection.
    async fn shared(&self) -> Result<Lent<'_>, Error> {
        let client = self
            .acquire(|st, max| {
                let room = st.shared.len() + st.draining.len() + st.taken + st.connecting < max;
                // The clock is read only if a connection is busy.
                let mut now = None;
                let mut stalled = |c: &Client| match c.head_since() {
                    None => false,
                    Some(t) => t + STALLED < *now.get_or_insert_with(Instant::now),
                };
                // Busy means used by other tasks right now.
                let mut best: Option<&Rc<Client>> = None;
                for c in &st.shared {
                    if !stalled(c)
                        && best.map_or(true, |b| Rc::strong_count(c) < Rc::strong_count(b))
                    {
                        best = Some(c);
                    }
                }
                let best = best.cloned();
                match best {
                    Some(c) if Rc::strong_count(&c) == 2 || !room => Decision::Ready(c),
                    _ if room => {
                        st.connecting += 1;
                        Decision::Connect
                    }
                    // Every connection is stalled and the pool is full: the
                    // one whose oldest command has waited least, which is
                    // the one most likely to be moving.
                    _ => match st.shared.iter().max_by_key(|c| c.head_since()) {
                        Some(c) => Decision::Ready(c.clone()),
                        None => Decision::Wait,
                    },
                }
            })
            .await?;
        let client = match client {
            Ok(c) => c,
            Err(new) => {
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
        // A timeout too long for the clock (`Duration::MAX`) means none.
        let deadline = Instant::now().checked_add(self.inner.wait_timeout);
        loop {
            let (decision, returned) = {
                let mut st = self.inner.state.borrow_mut();
                // Connections that broke, or whose server became a replica
                // (`READONLY`, after a failover), are left behind; they
                // close once the commands on them are answered.
                st.shared.retain(|c| !c.is_closed() && !c.is_stale());
                st.draining.retain(|c| !c.is_closed() && !c.is_stale());
                let returned = st.balance(self.inner.max_size);
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
            if !client.is_closed() && !client.is_blocked() && !client.is_stale() {
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
