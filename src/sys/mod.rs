//! The system calls giong makes outside its I/O drivers, one module per
//! operating system.
//!
//! Everything here has the same shape on every system: the runtime, the
//! readiness driver and the `fs` and `net` modules call `sys::…` and never
//! name an OS-specific function themselves. On Linux each function is the
//! call giong has always made (through `libc`, so the simulated kernel in
//! `sim/` keeps seeing the same symbols); on macOS it is the closest
//! equivalent, and where none exists the difference is written next to it.
//!
//! See `docs/design/portability.md` for the boundary and what is still
//! missing for Windows.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
