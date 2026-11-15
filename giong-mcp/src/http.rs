//! MCP's Streamable HTTP transport, reduced to what a read-only server
//! needs: one JSON-RPC message per `POST` to `/mcp`, answered with one JSON
//! body. No event streams (`GET` is refused, as the transport allows) and no
//! sessions.
//!
//! The server lives inside the service's process, so what clients can make
//! it hold is bounded: [`MAX_CONNECTIONS`] connections over HTTP/1.1, each
//! answered one request at a time, bodies of [`MAX_BODY`] bytes, and
//! answers of [`rpc::MAX_ANSWER`] bytes.

use std::rc::Rc;
use std::time::Duration;

use giong::Handle;
use giong_http::{Request, Response, Server};
use serde_json::{json, Value};

use crate::rpc;
use crate::tools::State;
use crate::Listener;

/// Connections served at once; more wait in the kernel's listen queue.
/// Agents ask one question at a time, on a connection or two.
pub(crate) const MAX_CONNECTIONS: usize = 16;
/// The largest request body. A JSON-RPC request to this server is a few
/// hundred bytes.
pub(crate) const MAX_BODY: usize = 64 * 1024;

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
    // HTTP/1.1 only: with HTTP/2, one connection could have a hundred
    // requests in progress at once.
    let server = Server::new()
        .http2(false)
        .max_connections(MAX_CONNECTIONS)
        .idle_timeout(Duration::from_secs(60))
        .request_timeout(Duration::from_secs(10))
        .max_body_size(MAX_BODY);
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

pub(crate) async fn answer(state: &State, settings: &Settings, req: Request) -> Response {
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
            .and_then(bearer)
            .unwrap_or("");
        if given.is_empty() || !same(given.as_bytes(), token.as_bytes()) {
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
    let version = req.header_str("mcp-protocol-version");
    if let Some(version) = version {
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
    match rpc::handle(state, message, version).await {
        Some(reply) => json_response(200, reply),
        None => Response::new(202),
    }
}

/// The token in `Authorization: Bearer <token>`. The scheme's name is not
/// case-sensitive (RFC 9110, section 11.1).
pub(crate) fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim_start())
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
    fn bearer_scheme() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        assert_eq!(bearer("BEARER   abc"), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearerabc"), None);
    }

    #[test]
    fn token_comparison() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
