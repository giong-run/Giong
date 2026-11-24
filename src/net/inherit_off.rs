//! A build without debug assertions and without the `dev` feature: none of
//! giong dev's socket hand-off (see `inherit.rs`). Started by giong dev
//! anyway (`giong dev --release` on a giong without the feature, or a release
//! binary run under it by hand), the first bind of a fixed port says so with
//! a coded error, instead of binding a socket of its own that would collide
//! with giong dev's.

use std::io;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use socket2::Socket;

use crate::driver::Driver;
use crate::error::{raise, Code};

/// Never a socket: refuses when giong dev started this process.
pub(crate) fn take(addr: SocketAddr) -> io::Result<Option<Socket>> {
    static UNDER_GIONG_DEV: OnceLock<bool> = OnceLock::new();
    if addr.port() == 0 || !*UNDER_GIONG_DEV.get_or_init(started_by_giong_dev) {
        return Ok(None);
    }
    Err(raise(
        Code::InvalidConfig,
        "giong dev started this service (GIONG_DEV_LISTEN is set), but it was built without \
         debug assertions (a --release build), which leaves giong dev's socket hand-off out",
        "run giong dev without --release, or build with giong's dev feature (giong dev --release \
         turns it on for its own build: update giong-cli if it does not)",
    ))
}

/// The same test as the hand-off's: the variables are set and name this
/// process's parent (a program the service runs inherits them, not them).
fn started_by_giong_dev() -> bool {
    if std::env::var_os("GIONG_DEV_LISTEN").map_or(true, |v| v.is_empty()) {
        return false;
    }
    let Some(parent) = std::env::var("GIONG_DEV_PARENT")
        .ok()
        .and_then(|p| p.trim().parse::<libc::pid_t>().ok())
    else {
        return false;
    };
    // SAFETY: getppid(2) cannot fail.
    parent == unsafe { libc::getppid() }
}

/// Never called: no listener is inherited in this build.
pub(crate) fn poll_accept(
    _sock: &Socket,
    _key: usize,
    _driver: &mut Driver,
    _cx: &mut Context<'_>,
) -> Poll<io::Result<Socket>> {
    unreachable!("a build without giong dev's hand-off inherits no listener")
}
