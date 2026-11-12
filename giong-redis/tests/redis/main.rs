//! Tests against a real Redis server (see `server.rs` for how it is
//! started).

mod client;
mod connect;
mod pool;
mod pubsub;
mod server;

/// Runs `f` on a giong runtime, or skips the test when there is no server.
fn run<F, Fut>(f: F)
where
    F: FnOnce(&'static server::Server) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(server) = server::server() else {
        return;
    };
    giong::block_on(f(server));
}

/// A key unique to the test and the call, so tests can share the server.
fn key(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{name}:{}", N.fetch_add(1, Ordering::Relaxed))
}
