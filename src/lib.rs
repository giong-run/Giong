//! # giong
//!
//! A thread-per-core async runtime for Linux, built directly on **io_uring**.
//!
//! Named after Thánh Gióng, the child of Vietnamese legend who lay silent for
//! three years and then rose a giant: giong costs next to nothing while idle
//! and uses every core the moment load arrives.
//!
//! ## Design
//!
//! * **Thread-per-core, shared-nothing.** Every worker thread owns its own
//!   io_uring, task queue, timers and buffer pool. Tasks never migrate, so
//!   futures need not be `Send` and the hot path has no locks and no atomics
//!   beyond a waker flag.
//! * **Completion-based I/O with batching.** Everything a tick produces —
//!   reads, writes, accepts, closes, cancellations — reaches the kernel in a
//!   single `io_uring_enter`, which also returns every completion that is
//!   ready.
//! * **Multishot accept and receive** with kernel-provided buffers: one
//!   submission per listener / connection, no buffer pinned by idle
//!   connections.
//! * **One ring for everything.** TCP, UDP and Unix sockets ([`net`]) and
//!   files ([`fs`]) all go through the worker's io_uring. File I/O needs no
//!   pool of blocking threads.
//! * **A latency dial.** [`Builder::spin_before_park`] lets a worker watch its
//!   ring instead of sleeping, for services where microseconds matter more
//!   than an idle core.
//! * **Cancellation safety.** Dropping an I/O future never frees memory the
//!   kernel may still write to; ownership of the buffer moves to the driver
//!   until the kernel confirms.
//!
//! ## Example
//!
//! ```no_run
//! use giong::net::TcpListener;
//!
//! fn main() -> std::io::Result<()> {
//!     giong::Builder::new().shutdown_on_signals(true).build()?.run(|_worker| async {
//!         let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
//!         while let Some(Ok(stream)) = giong::until_shutdown(listener.accept()).await {
//!             giong::spawn(async move {
//!                 loop {
//!                     let Ok(data) = stream.recv().await else { return };
//!                     if data.is_empty() {
//!                         return; // peer closed
//!                     }
//!                     let reply = data.to_vec();
//!                     drop(data); // give the buffer back to the pool
//!                     if stream.write_all(reply).await.0.is_err() {
//!                         return;
//!                     }
//!                 }
//!             });
//!         }
//!     })
//! }
//! ```
//!
//! ## Requirements
//!
//! Linux 6.0 or newer (6.1+ recommended). giong does not build on other
//! platforms.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-icon.svg",
    html_favicon_url = "https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-icon.svg"
)]

#[cfg(not(target_os = "linux"))]
compile_error!("giong only supports Linux: it is built directly on io_uring (kernel >= 6.0)");

mod buf;
mod driver;
mod executor;
mod notify;
mod op;
mod runtime;
mod slab;

pub mod error;
pub mod fs;
pub mod net;
pub mod sync;
pub mod time;

pub use buf::{IoBuf, IoBufMut, RecvBuf};
pub use executor::{spawn, yield_now, JoinError, JoinHandle, RemoteJoinHandle};
pub use runtime::{
    block_on, handle, is_shutting_down, shutdown_signal, until_shutdown, worker_id, Builder,
    Handle, Runtime, ShutdownSignal,
};

#[doc(hidden)]
pub use runtime::{stats, Stats};
