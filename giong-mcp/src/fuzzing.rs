//! The check behind the fuzz target in `giong-mcp/fuzz` (and a test):
//! any bytes as one HTTP request to the server, through every check it makes
//! (path, `Host`, `Origin`, token, method, content type, protocol version)
//! and the JSON-RPC dispatch behind them.
//!
//! The first byte picks the settings: bit 0 a token, bit 1 the `Host`
//! check. The rest is the request, which is skipped if it does not parse,
//! since giong-http's own targets cover that. Whatever it is:
//!
//! * nothing panics, and the answer comes;
//! * the status is one the server means to send, and its body is JSON;
//! * with a token, nothing is answered `200` or `202` unless the request
//!   carries exactly that token;
//! * an answer never exceeds the size limit.
//!
//! Calls to `profile` are skipped: each one measures for at least 100 ms.

use std::sync::OnceLock;
use std::time::Duration;

use giong::Handle;
use giong_http::Request;
use serde_json::Value;

use crate::http::{self, Settings};
use crate::rpc;
use crate::tools::State;

const TOKEN: &str = "fuzz-token-0123456789";

/// A one-worker service that lives as long as the process.
fn service() -> &'static Handle {
    static SERVICE: OnceLock<Handle> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let rt = giong::Builder::new()
            .workers(1)
            .build()
            .expect("a runtime for the fuzzed service");
        let handle = rt.handle();
        std::thread::spawn(move || rt.run(|_| async { giong::shutdown_signal().await }));
        handle
    })
}

/// Panics if the server answers `data` in a way it must not.
pub fn check_request(data: &[u8]) {
    let Some((&flags, raw)) = data.split_first() else {
        return;
    };
    if raw.windows(7).any(|w| w == b"profile") {
        return;
    }
    let Ok(req) = Request::from_bytes(raw) else {
        return;
    };
    let token = (flags & 1 != 0).then(|| TOKEN.to_owned());
    let settings = Settings {
        token: token.clone(),
        check_host: flags & 2 != 0,
        snapshot_timeout: Duration::from_millis(200),
    };
    let authorized = req
        .header_str("authorization")
        .and_then(http::bearer)
        .is_some_and(|t| t == TOKEN);
    let state = State::new(service().clone(), settings.snapshot_timeout);
    let response = giong::Builder::new()
        .workers(1)
        .ring_entries(8)
        .recv_buffers(1, 1024)
        .block_on(http::answer(&state, &settings, req))
        .expect("a runtime for the server");

    let status = response.status();
    assert!(
        [200, 202, 400, 401, 403, 404, 405, 415].contains(&status),
        "status {status}"
    );
    if token.is_some() && !authorized {
        assert!(!matches!(status, 200 | 202), "answered without the token");
    }
    let body = response.body_bytes();
    assert!(body.len() <= rpc::MAX_ANSWER, "{} bytes", body.len());
    if status == 202 {
        assert!(body.is_empty());
    } else {
        let parsed: Value = serde_json::from_slice(body).expect("the body is JSON");
        assert!(parsed.is_object() || parsed.is_array(), "{parsed}");
    }
    if status == 401 {
        assert_eq!(response.get_header("www-authenticate"), Some("Bearer"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(flags: u8, headers: &str, body: &str) -> Vec<u8> {
        let mut data = vec![flags];
        data.extend_from_slice(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        data
    }

    #[test]
    fn requests_the_target_starts_from() {
        let json = "Content-Type: application/json\r\n";
        let auth = format!("{json}Authorization: Bearer {TOKEN}\r\n");
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"workers"}}"#;
        for (flags, headers, body) in [
            (0, json, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            (1, json, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            (3, auth.as_str(), call),
            (
                2,
                json,
                r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#,
            ),
            (0, json, r#"{"jsonrpc":"2.0","id":5}"#),
            (0, "", "x"),
        ] {
            check_request(&request(flags, headers, body));
        }
    }
}
