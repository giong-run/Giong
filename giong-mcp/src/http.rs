//! MCP's Streamable HTTP transport, reduced to what a read-only server
//! needs: one JSON-RPC message per `POST` to `/mcp`, answered with one JSON
//! body. No event streams (`GET` is refused, as the transport allows) and no
//! sessions.

use std::rc::Rc;
use std::time::Duration;

use giong::Handle;
use giong_http::{Request, Response, Server};
use serde_json::{json, Value};

use crate::rpc;
use crate::tools::State;
use crate::Listener;

pub(crate) struct Settings {
    pub(crate) token: Option<String>,
    /// Over TCP, the `Host` header must name a loopback address.
    pub(crate) check_host: bool,
    pub(crate) snapshot_timeout: Duration,
}

pub(crate) async fn serve(listener: Listener, target: Handle, settings: Settings) {
    let state = Rc::new(State::new(target, settings.snapshot_timeout));
    giong::spawn(crate::watch::run(state.clone()));
    let settings = Rc::new(settings);
    let handler = move |req: Request| {
        let state = state.clone();
        let settings = settings.clone();
        async move { answer(&state, &settings, req).await }
    };
    // The answers are small and agents ask one question at a time.
    let server = Server::new()
        .max_connections(64)
        .idle_timeout(Duration::from_secs(300))
        .max_body_size(256 * 1024);
    match listener {
        Listener::Tcp(l) => server.serve(l, handler).await,
        Listener::Unix(l) => server.serve_unix(l, handler).await,
    }
}

fn json_response(status: u16, body: String) -> Response {
    Response::new(status)
        .header("Content-Type", "application/json")
        .body(body)
}

fn plain(status: u16, message: &str) -> Response {
    json_response(status, json!({ "error": message }).to_string())
}

async fn answer(state: &State, settings: &Settings, req: Request) -> Response {
    if req.path() != "/mcp" {
        return plain(404, "not found: the MCP endpoint is /mcp");
    }
    if settings.check_host && !req.header_str("host").is_some_and(is_local_host) {
        return plain(
            403,
            "the Host header must be a loopback address such as 127.0.0.1",
        );
    }
    if let Some(origin) = req.header("origin") {
        let local = std::str::from_utf8(origin)
            .ok()
            .and_then(|o| o.split_once("://"))
            .is_some_and(|(_, host)| is_local_host(host));
        if !local {
            return plain(403, "requests from web pages on other origins are refused");
        }
    }
    if let Some(token) = &settings.token {
        let given = req
            .header_str("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !same(given.as_bytes(), token.as_bytes()) {
            return plain(401, "a valid bearer token is required")
                .header("WWW-Authenticate", "Bearer");
        }
    }
    if req.method() != "POST" {
        return plain(
            405,
            "send JSON-RPC messages with POST; this server opens no event stream",
        )
        .header("Allow", "POST");
    }
    let json_body = req.header_str("content-type").is_some_and(|ct| {
        ct.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    if !json_body {
        return plain(415, "the body must be application/json");
    }
    if let Some(version) = req.header_str("mcp-protocol-version") {
        if !rpc::VERSIONS.contains(&version) {
            return plain(400, "unsupported MCP-Protocol-Version");
        }
    }
    let message: Value = match serde_json::from_slice(req.body()) {
        Ok(m) => m,
        Err(e) => {
            let body = rpc::error_response(Value::Null, -32700, &format!("parse error: {e}"));
            return json_response(400, body.to_string());
        }
    };
    match rpc::handle(state, message).await {
        Some(reply) => json_response(200, reply.to_string()),
        None => Response::new(202),
    }
}

/// `host` is `name[:port]` or `[v6][:port]`, naming this machine.
fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((v6, _)) => v6,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Compares in time that depends on the length only.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for host in [
            "127.0.0.1:7777",
            "127.0.0.1",
            "localhost:80",
            "LOCALHOST",
            "[::1]:7777",
            "[::1]",
            "127.1.2.3:9",
        ] {
            assert!(is_local_host(host), "{host}");
        }
        for host in [
            "example.com",
            "evil.localhost.example.com:80",
            "10.0.0.1:7777",
            "[::]:7777",
            "[::1",
            "",
        ] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[test]
    fn token_comparison() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
