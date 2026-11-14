//! Tests against a real PostgreSQL server (see `server.rs` for how it is
//! started).

mod auth;
mod client;
mod pgbouncer;
mod pool;
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
