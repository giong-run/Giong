//! Tests against a real MySQL server (see `server.rs` for how it is
//! started).

mod auth;
mod client;
mod odd;
mod pool;
mod server;

use std::rc::Rc;

use giong::net::TcpStream;

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

/// Like [`run`], with the second server (see [`server::odd`]) as well; such
/// tests take turns.
fn run_odd<F, Fut>(f: F)
where
    F: FnOnce(&'static server::Server, &'static server::Server) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(server) = server::server() else {
        return;
    };
    let (odd, _turn) = server::odd(server);
    giong::block_on(f(server, odd));
}

/// Copies what arrives on `from` to `to`, until `from` ends.
async fn pipe(from: Rc<TcpStream>, to: Rc<TcpStream>) {
    loop {
        match from.recv().await {
            Ok(chunk) if !chunk.is_empty() => {
                if to.write_all(chunk.to_vec()).await.0.is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}
