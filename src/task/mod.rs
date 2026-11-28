//! Structured concurrency: scopes that own their children and carry a
//! deadline that reaches the kernel, task groups, and task-local values.
//!
//! One tool for each job:
//!
//! | You want | Use |
//! |---|---|
//! | Stop waiting on one future after a time | [`time::timeout`](crate::time::timeout) (unchanged) |
//! | A deadline that every operation and child task inside a piece of work obeys | [`Scope::with_deadline`] / [`Scope::with_timeout`] |
//! | Several futures that borrow local data; none outlives the call; the first error cancels the rest | [`Scope`] |
//! | A changing set of `'static` tasks, results as they finish, all aborted on drop | [`TaskGroup`] |
//! | A value every call inside a piece of work can read (request id, tenant) | [`task_local!`](crate::task_local) |
//!
//! Nothing here changes [`spawn`](crate::spawn): a spawned task is
//! detached, and inherits no deadline and no task-local value.
//!
//! # Deadlines
//!
//! A scope made with [`Scope::with_deadline`] or [`Scope::with_timeout`]
//! runs everything inside it under that deadline:
//!
//! * The scope's body and children are dropped when the deadline passes,
//!   at the latest at the next turn of the worker's event loop, and
//!   [`run`](Scope::run) returns [`Cancelled`] converted into the scope's
//!   error type (`GIONG-E0024`, kind `TimedOut`, as an
//!   [`io::Error`](std::io::Error)). Code inside never runs after its
//!   deadline.
//! * On io_uring, every operation inside it that waits on a peer
//!   (`connect`, a one-off `read`, waiting until a socket can take more
//!   data, waiting on a pipe) is submitted with the deadline linked to it
//!   (`IORING_OP_LINK_TIMEOUT`). The kernel cancels it at the deadline by
//!   itself: no timer wake-up, no cancel request, and the operation is gone
//!   even if the worker is busy at that moment, so data that arrives later
//!   stays in the socket. (A read that already completed when the scope
//!   ends, for example one the worker, stalled, handed to the kernel only
//!   after the data was there, is dropped with its data, as any dropped
//!   one-off read on io_uring.) On the other drivers the deadline's timer
//!   in the worker's timer wheel ends the same operations; the result your
//!   code sees is the same.
//! * An operation that outlives the code that started it (one an object
//!   keeps in flight across calls) and is polled again after its deadline
//!   cut it, outside the scope, is submitted again, under the deadline in
//!   force there, if any: it does not report the cut.
//! * Nested deadlines take the earlier one: an inner scope cannot extend
//!   the time an outer one gives.
//! * Tasks started with [`TaskGroup::spawn`] inside it inherit it. A task
//!   started with plain [`spawn`](crate::spawn) does not.
//! * Libraries read what is left with [`deadline`] or [`remaining`] to pass
//!   it on (a database statement timeout, a `grpc-timeout` header).
//!
//! ```no_run
//! use std::time::Duration;
//! use giong::net::TcpStream;
//! use giong::task::Scope;
//!
//! # async fn f() -> std::io::Result<()> {
//! // Connect and read the greeting within 2 seconds, all told.
//! let greeting = Scope::with_timeout(Duration::from_secs(2))
//!     .run(async {
//!         let stream = TcpStream::connect("10.0.0.7:6379").await?;
//!         stream.write_all(&b"PING\r\n"[..]).await.0?;
//!         let (n, buf) = stream.read(vec![0u8; 64]).await;
//!         Ok(buf[..n?].to_vec())
//!     })
//!     .await?; // GIONG-E0024 once the 2 seconds have passed
//! # let _ = greeting;
//! # Ok(())
//! # }
//! ```
//!
//! # Budgets
//!
//! A scope's body and children run on the task that awaits
//! [`run`](Scope::run), and a task group's tasks are ordinary tasks of the
//! worker, so both are charged to the [budget](crate::budget) the work runs
//! in, as anything else it does.
//!
//! # Cost
//!
//! Nothing changes for code that uses none of this. Inside a deadline, each
//! poll of the scope reads the clock once, and on io_uring each operation
//! that waits on a peer takes one more submission entry (no extra
//! completion: the timeout is submitted with `IOSQE_CQE_SKIP_SUCCESS`).
//! `bench/deadline` in the repository measures it.

mod context;
mod deadline;
mod error;
mod group;
mod local;
mod ready;
mod scope;

#[cfg(target_os = "linux")]
pub(crate) use context::{kernel_deadline, restart_after_cut};
pub use deadline::{deadline, remaining};
pub use error::{CancelReason, Cancelled};
pub use group::TaskGroup;
pub use local::{LocalKey, LocalScope};
pub use scope::{CancelHandle, Run, Scope, ScopeHandle};
