//! `giong record`: exchange lines from a service's log turned into cases,
//! replayed against a service that answers with new ids. The log is written
//! here by hand (with the noise a real log holds), and the service is a
//! small std server, so the scenario needs no build.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::harness::*;

fn exchange(
    method: &str,
    path: &str,
    req_headers: Value,
    req_body: &str,
    status: u16,
    resp_body: &str,
) -> String {
    json!({
        "event": "exchange", "method": method, "path": path, "query": null, "route": path,
        "status": status, "duration_us": 120,
        "request": {"headers": req_headers, "body": req_body, "body_len": req_body.len(),
                    "truncated": false, "streamed": false},
        "response": {"headers": [["content-type", "application/json"]], "body": resp_body,
                     "body_len": resp_body.len(), "truncated": false, "streamed": false}
    })
    .to_string()
}

const ID1: &str = "550e8400-e29b-41d4-a716-446655440000";

/// The log of a service that created a user and read it back, with the noise
/// around it: a Logger line, a panic, other JSON, half a line.
fn log() -> String {
    [
        "giong-web: listening on 127.0.0.1:8080".to_string(),
        r#"{"event":"request","method":"GET","path":"/health","status":200}"#.to_string(),
        exchange(
            "POST",
            "/users",
            json!([
                ["content-type", "application/json"],
                ["authorization", "[redacted]"]
            ]),
            r#"{"name":"ana"}"#,
            201,
            &format!(r#"{{"id":"{ID1}","name":"ana","created_at":"2026-11-22T08:40:01.123456Z"}}"#),
        ),
        "thread 'worker-1' panicked at src/main.rs:3:5: oops".to_string(),
        r#"{"event":"exchange","method":"GET""#.to_string(),
        exchange(
            "GET",
            &format!("/users/{ID1}"),
            json!([]),
            "",
            200,
            &format!(r#"{{"id":"{ID1}","name":"ana"}}"#),
        ),
    ]
    .join("\n")
        + "\n"
}

/// A service that creates users with fresh ids and remembers what it saw.
fn service(wrong_status: bool) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let next = Arc::new(AtomicU64::new(1));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let (seen, next) = (s2.clone(), next.clone());
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while buf.len() < end + 4 + len {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    buf.drain(..end + 4 + len);
                    seen.lock().unwrap().push(head.clone());
                    let line = head.lines().next().unwrap_or("").to_string();
                    let (status, body) = if line.starts_with("POST /users ") {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let id = format!("00000000-0000-4000-8000-{n:012}");
                        let status = if wrong_status { 200 } else { 201 };
                        (
                            status,
                            format!(
                                r#"{{"id":"{id}","name":"ana","created_at":"2027-01-01T00:00:00Z"}}"#
                            ),
                        )
                    } else if let Some(rest) = line.strip_prefix("GET /users/") {
                        let id = rest.split(' ').next().unwrap();
                        (200, format!(r#"{{"id":"{id}","name":"ana"}}"#))
                    } else {
                        (404, "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if s.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (base, seen)
}

#[test]
fn a_log_becomes_cases_that_replay_against_new_ids() {
    if !has_command("record") {
        return;
    }
    let dir = TempDir::new("record");
    new_package(&dir, "rec", "");
    let log_path = dir.write("service.log", &log());

    let out = giong(["record", "list", "--json", "--from"])
        .arg(&log_path)
        .dir(dir.path())
        .run();
    let v = out.report("record");
    out.expect_code(0);
    let listed = v["data"].to_string();
    assert!(listed.contains("/users"), "{}", out.dump());
    assert!(
        !listed.contains("oops"),
        "a panic line was read as an exchange: {listed}"
    );

    let cases = dir.join("cases.json");
    let out = giong(["record", "save", "all", "--json", "--from"])
        .arg(&log_path)
        .arg("--out")
        .arg(&cases)
        .dir(dir.path())
        .run();
    out.report("record");
    out.expect_code(0);
    let text = std::fs::read_to_string(&cases).unwrap();
    let file: Value = serde_json::from_str(&text).expect("the case file is JSON");
    assert_eq!(file["cases"].as_array().map(Vec::len), Some(2), "{text}");
    // The id the service made, and its timestamp, are placeholders now; the
    // redacted header is read from the environment, never written.
    for c in file["cases"].as_array().unwrap() {
        let live = format!("{}{}", c["request"], c["expect"]);
        assert!(
            !live.contains(ID1),
            "the recorded id was kept literally: {c:#}"
        );
    }
    assert!(!text.contains("2026-11-22T08:40:01"), "{text}");
    assert!(!text.contains("[redacted]"), "{text}");

    let (base, seen) = service(false);
    let out = giong(["record", "replay", "--json", "--base-url", &base])
        .arg(&cases)
        .env("GIONG_RECORD_AUTHORIZATION", "Bearer e2e-token")
        .dir(dir.path())
        .run();
    out.report("record");
    out.expect_code(0);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:#?}");
    assert!(
        seen[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer e2e-token"),
        "the redacted header was not filled from the environment: {}",
        seen[0]
    );
    // The second request uses the id the first answer gave, not the old one.
    assert!(
        seen[1].starts_with("GET /users/00000000-0000-4000-8000-000000000001 "),
        "{}",
        seen[1]
    );
}

#[test]
fn a_changed_answer_fails_the_replay() {
    if !has_command("record") {
        return;
    }
    let dir = TempDir::new("record-fail");
    new_package(&dir, "recfail", "");
    let log_path = dir.write("service.log", &log());
    let cases = dir.join("cases.json");
    giong(["record", "save", "all", "--from"])
        .arg(&log_path)
        .arg("--out")
        .arg(&cases)
        .dir(dir.path())
        .run()
        .expect_code(0);
    let (base, _) = service(true);
    let out = giong(["record", "replay", "--json", "--base-url", &base])
        .arg(&cases)
        .env("GIONG_RECORD_AUTHORIZATION", "Bearer e2e-token")
        .dir(dir.path())
        .run();
    let v = out.report("record");
    out.expect_code(1);
    assert!(
        v.to_string().contains("201"),
        "the failure does not say what was expected: {v}"
    );
}

#[test]
fn a_missing_secret_is_named_before_anything_is_sent() {
    if !has_command("record") {
        return;
    }
    let dir = TempDir::new("record-env");
    new_package(&dir, "recenv", "");
    let log_path = dir.write("service.log", &log());
    let cases = dir.join("cases.json");
    giong(["record", "save", "all", "--from"])
        .arg(&log_path)
        .arg("--out")
        .arg(&cases)
        .dir(dir.path())
        .run()
        .expect_code(0);
    let (base, seen) = service(false);
    let out = giong(["record", "replay", "--json", "--base-url", &base])
        .arg(&cases)
        .env_remove("GIONG_RECORD_AUTHORIZATION")
        .dir(dir.path())
        .run();
    out.report("record");
    assert_ne!(out.code(), 0, "{}", out.dump());
    assert!(
        out.stdout.contains("GIONG_RECORD_AUTHORIZATION"),
        "{}",
        out.dump()
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|h| !h.to_ascii_lowercase().contains("authorization")),
        "a request went out without its secret"
    );
}

/// Security point 2: a case file is a test meant to be committed and
/// replayed in CI, so no secret from a request may land in it. Headers are
/// redacted already; a password in a JSON body, a key in a query string and a
/// card number must not be written either. `save` either turns them into
/// `{{env:...}}` placeholders or refuses with a coded error, and never echoes
/// them on stdout.
#[test]
fn a_saved_case_holds_no_secret_from_a_body_or_a_query() {
    if !has_command("record") {
        return;
    }
    const SECRETS: [&str; 3] = ["hunter2-SECRET", "AK-SECRET-123", "4242424242424242"];
    let mut login: Value = serde_json::from_str(&exchange(
        "POST",
        "/login",
        json!([["content-type", "application/json"]]),
        r#"{"user":"ana","password":"hunter2-SECRET","card":"4242424242424242"}"#,
        200,
        r#"{"ok":true}"#,
    ))
    .unwrap();
    login["query"] = json!("api_key=AK-SECRET-123&page=2");
    let dir = TempDir::new("record-secrets");
    new_package(&dir, "recsec", "");
    let log_path = dir.write("service.log", &format!("{login}\n"));
    let cases = dir.join("cases.json");
    let out = giong(["record", "save", "all", "--json", "--from"])
        .arg(&log_path)
        .arg("--out")
        .arg(&cases)
        .dir(dir.path())
        .run();
    let v = out.report("record");
    let leaked = |text: &str| -> Vec<&str> {
        SECRETS
            .iter()
            .copied()
            .filter(|s| text.contains(s))
            .collect()
    };
    assert_eq!(leaked(&out.stdout), Vec::<&str>::new(), "{}", out.dump());
    assert_eq!(leaked(&out.stderr), Vec::<&str>::new(), "{}", out.dump());
    if out.code() == 0 {
        let text = std::fs::read_to_string(&cases).unwrap();
        assert_eq!(leaked(&text), Vec::<&str>::new(), "{text}");
        // What is not secret is kept, so the case still means something.
        assert!(
            text.contains("page=2") || text.contains("\"page\""),
            "{text}"
        );
        assert!(text.contains("ana"), "{text}");
    } else {
        assert!(v["error"]["code"].is_string(), "{}", out.dump());
        let text = std::fs::read_to_string(&cases).unwrap_or_default();
        assert_eq!(leaked(&text), Vec::<&str>::new(), "{text}");
    }
}
