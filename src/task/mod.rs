//! Structured concurrency: deadlines that reach the kernel, scopes that own
//! their child tasks, task groups, and task-local values.
//!
//! Four tools, each for one job:
//!
//! | You want | Use |
//! |---|---|
//! | A time limit for a piece of work, including every I/O call inside it and every task it starts in a scope or group | [`with_deadline`], [`with_timeout`] |
//! | Run several futures at once that borrow local data; none outlives the call; the first error cancels the rest | [`Scope`] |
//! | Own a changing set of `'static` tasks, collect results as they finish, abort all on drop | [`TaskGroup`] |
//! | A value every call inside a piece of work can read (request id, tenant) | [`task_local!`](crate::task_local) |
//!
//! # Deadlines
//!
//! A deadline set with [`with_deadline`] holds for everything that runs
//! inside the future it wraps:
//!
//! * The future is dropped when the deadline passes, at the latest at the
//!   next turn of the worker's event loop, and the call returns
//!   [`Cancelled`] (`GIONG-E0020` as an [`io::Error`](std::io::Error)).
//!   Code inside never runs after its deadline.
//! * On io_uring, every operation inside it that waits on a peer
//!   (`connect`, a one-off `read`, waiting until a socket can take more
//!   data, reading a pipe) is submitted with the deadline linked to it
//!   (`IORING_OP_LINK_TIMEOUT`). The kernel cancels it at the deadline by
//!   itself: no timer wake-up, no cancel request, and the operation is gone
//!   even if the worker is busy at that moment. On epoll, the deadline's
//!   timer in the worker's timer wheel ends the same operations; the result
//!   your code sees is the same.
//! * Nested deadlines take the earlier one: an inner `with_timeout` cannot
//!   extend the time an outer one gives.
//! * Children of a [`Scope`] and tasks started with [`TaskGroup::spawn`]
//!   inherit it. A task started with plain [`spawn`](crate::spawn) does
//!   not: it is detached, and outlives the work that started it.
//! * Libraries read what is left with [`deadline`] or [`remaining`] to pass
//!   it on (a database statement timeout, a `grpc-timeout` header).
//!
//! [`time::timeout`](crate::time::timeout) stays what it was: a race
//! between one future and a timer, nothing inherited, nothing linked.
//!
//! ```no_run
//! use std::time::Duration;
//! use giong::net::TcpStream;
//!
//! # async fn f() -> std::io::Result<()> {
//! // Connect and read the greeting within 2 seconds, all told.
//! let greeting = giong::task::with_timeout(Duration::from_secs(2), async {
//!     let stream = TcpStream::connect("10.0.0.7:6379").await?;
//!     stream.write_all(&b"PING\r\n"[..]).await.0?;
//!     let (n, buf) = stream.read(vec![0u8; 64]).await;
//!     Ok::<_, std::io::Error>(buf[..n?].to_vec())
//! })
//! .await??; // the outer `?` is the deadline (GIONG-E0020), the inner one the I/O
//! # let _ = greeting;
//! # Ok(())
//! # }
//! ```
//!
//! # Cost
//!
//! Nothing changes for code that uses none of this. Inside a deadline, each
//! poll of the wrapping future reads the clock once, and on io_uring each
//! operation that waits on a peer takes one more submission entry (no extra
//! completion: the timeout is submitted with `IOSQE_CQE_SKIP_SUCCESS`).
//! `bench/deadline` in the repository measures it.

mod context;
mod deadline;
mod error;
mod group;
mod local;
mod ready;
mod scope;

pub(crate) use context::kernel_deadline;
pub use deadline::{deadline, remaining, with_deadline, with_timeout, Deadline};
pub use error::{CancelReason, Cancelled};
pub use group::TaskGroup;
pub use local::{LocalKey, LocalScope};
pub use scope::{CancelHandle, Run, Scope, ScopeHandle};
