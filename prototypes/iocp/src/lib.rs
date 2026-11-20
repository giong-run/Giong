//! Prototype of giong's Windows driver.
//!
//! giong's drivers sit behind one contract (`src/driver/mod.rs`): an
//! operation is started with its buffers, the driver reports its result
//! once, and a future dropped early leaves its buffers with the driver
//! until the kernel can no longer touch them. Windows completion ports are
//! completion-based like io_uring, so the same contract fits without a
//! readiness emulation in between: `WSARecv` reads straight into the
//! caller's buffer, and nothing is copied or retried.
//!
//! This crate holds that driver on its own, with the operations whose
//! Windows behaviour carries the risk (`AcceptEx`, `ConnectEx`,
//! cancellation, completions that finish inside the call), so that they
//! are proven on Windows before the runtime is ported. It builds on
//! Windows only and is not published.

#![cfg(windows)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod driver;
pub mod op;
