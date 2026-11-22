//! Replaying a case file against the running service.

use std::time::Duration;

use serde_json::{json, Value};

use super::case::{Body, Case};
use super::matcher::{fill_json, fill_text, match_json, match_text, Env, Missing, Vars};
use crate::web_client::{self, Base, Outgoing, SendError};

/// What happened to one case.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Passed,
    /// The answer differs: the differences.
    Failed(Vec<String>),
    /// It could not be sent: a value it needs is missing.
    Missing(Missing),
    /// The request was refused before sending (a bad method or path).
    Invalid(String),
}

/// One case's result.
#[derive(Clone, Debug)]
pub struct Result1 {
    pub name: String,
    pub line: usize,
    pub outcome: Outcome,
    pub status: Option<u16>,
    pub elapsed: Option<Duration>,
}

impl Result1 {
    pub fn to_json(&self) -> Value {
        let (ok, diffs, error) = match &self.outcome {
            Outcome::Passed => (true, vec![], None),
            Outcome::Failed(d) => (false, d.clone(), None),
            Outcome::Missing(m) => (false, vec![], Some(m.to_string())),
            Outcome::Invalid(e) => (false, vec![], Some(e.clone())),
        };
        json!({
            "name": self.name,
            "line": self.line,
            "ok": ok,
            "status": self.status,
            "elapsed_ms": self.elapsed.map(|d| d.as_secs_f64() * 1000.0),
            "differences": diffs,
            "error": error,
        })
    }
}

/// Builds the request of a case with the values known so far.
pub fn request(case: &Case, vars: &Vars, env: Env<'_>) -> Result<Outgoing, Missing> {
    let r = &case.request;
    let mut target = fill_text(&r.path, vars, env, true)?;
    if let Some(q) = &r.query {
        target.push('?');
        // Fill each value on its own so `&` and `=` in a value stay encoded.
        let mut parts = Vec::new();
        for pair in q.split('&') {
            match pair.split_once('=') {
                Some((k, v)) => parts.push(format!(
                    "{}={}",
                    fill_text(k, vars, env, true)?,
                    fill_text(v, vars, env, true)?
                )),
                None => parts.push(fill_text(pair, vars, env, true)?),
            }
        }
        target.push_str(&parts.join("&"));
    }
    let mut headers = Vec::with_capacity(r.headers.len());
    for (n, v) in &r.headers {
        headers.push((n.clone(), fill_text(v, vars, env, false)?));
    }
    let body = match &r.body {
        Body::None | Body::Unchecked(_) => Vec::new(),
        Body::Text(t) => fill_text(t, vars, env, false)?.into_bytes(),
        Body::Json(j) => fill_json(j, vars, env)?.to_string().into_bytes(),
        Body::Bytes(b) => b.clone(),
    };
    Ok(Outgoing {
        method: r.method.clone(),
        target,
        headers,
        body,
    })
}

/// Checks an answer against a case's expectation, remembering captures.
pub fn check(case: &Case, answer: &web_client::Answer, vars: &mut Vars) -> Vec<String> {
    let mut diffs = Vec::new();
    let e = &case.expect;
    if answer.status != e.status {
        let mut d = format!("status: expected {}, got {}", e.status, answer.status);
        // A problem+json answer says why: show its code and title.
        if let Ok(p) = serde_json::from_slice::<Value>(&answer.body) {
            if let (Some(code), Some(title)) = (p["code"].as_str(), p["title"].as_str()) {
                d.push_str(&format!(" ({code}: {title})"));
            }
        }
        diffs.push(d);
    }
    for (name, template) in &e.headers {
        match answer.header(name) {
            None => diffs.push(format!("header {name}: missing")),
            Some(actual) => {
                let ok = if name == "content-type" {
                    // Media types are case-insensitive; spacing around `;` varies.
                    let norm = |s: &str| s.to_ascii_lowercase().replace("; ", ";");
                    if template.contains("{{") {
                        match_text(template, actual, vars).is_ok()
                    } else {
                        norm(template) == norm(actual)
                    }
                } else {
                    match match_text(template, actual, vars) {
                        Ok(()) => true,
                        Err(why) => {
                            diffs.push(format!("header {name}: {why}"));
                            continue;
                        }
                    }
                };
                if !ok {
                    diffs.push(format!(
                        "header {name}: expected {template:?}, got {actual:?}"
                    ));
                }
            }
        }
    }
    match &e.body {
        Body::Unchecked(_) => {}
        Body::None => {}
        Body::Text(t) => {
            let actual = String::from_utf8_lossy(&answer.body);
            if let Err(why) = match_text(t, &actual, vars) {
                diffs.push(format!("body: {why}"));
            }
        }
        Body::Json(j) => match serde_json::from_slice::<Value>(&answer.body) {
            Ok(actual) => match_json(j, &actual, "body", vars, &mut diffs),
            Err(_) => diffs.push(format!(
                "body: expected JSON, got {:?}",
                String::from_utf8_lossy(&answer.body[..answer.body.len().min(80)])
            )),
        },
        Body::Bytes(b) => {
            if &answer.body != b {
                diffs.push(format!(
                    "body: expected {} bytes, got {} different bytes",
                    b.len(),
                    answer.body.len()
                ));
            }
        }
    }
    diffs
}

/// Why a replay stopped before its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreachable {
    pub case: String,
    pub why: String,
}

/// Replays a file's cases in order. Stops when the service cannot be
/// reached; a case that fails or misses a value does not stop the others.
/// A failed case keeps the captures it found; a capture whose field is
/// missing makes the later cases that need it say so.
pub fn run(
    cases: &[Case],
    base: &Base,
    timeout: Duration,
    env: Env<'_>,
) -> Result<Vec<Result1>, (Vec<Result1>, Unreachable)> {
    let mut vars = Vars::new();
    let mut out = Vec::with_capacity(cases.len());
    for case in cases {
        let mut r = Result1 {
            name: case.name.clone(),
            line: case.line,
            outcome: Outcome::Passed,
            status: None,
            elapsed: None,
        };
        let req = match request(case, &vars, env) {
            Ok(req) => req,
            Err(m) => {
                r.outcome = Outcome::Missing(m);
                out.push(r);
                continue;
            }
        };
        match web_client::send(base, &req, timeout) {
            Ok(answer) => {
                r.status = Some(answer.status);
                r.elapsed = Some(answer.elapsed);
                // Captures found count even when something else differs:
                // a changed name should not hide every later case.
                let diffs = check(case, &answer, &mut vars);
                if !diffs.is_empty() {
                    r.outcome = Outcome::Failed(diffs);
                }
            }
            Err(SendError::Invalid(why)) => r.outcome = Outcome::Invalid(why),
            Err(SendError::Connect(why)) | Err(SendError::Answer(why)) => {
                let stop = Unreachable {
                    case: case.name.clone(),
                    why,
                };
                return Err((out, stop));
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// Whether a missing value is a missing environment variable.
pub fn env_missing(m: &Missing) -> Option<&str> {
    match m {
        Missing::Env(v) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::case::{Expect, Request};
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn case(name: &str, method: &str, path: &str, body: Body, status: u16, ebody: Body) -> Case {
        Case {
            name: name.into(),
            request: Request {
                method: method.into(),
                path: path.into(),
                query: None,
                headers: vec![],
                body,
            },
            expect: Expect {
                status,
                headers: BTreeMap::new(),
                body: ebody,
            },
            line: 1,
        }
    }

    /// A tiny service: POST /users makes ids 100, 101, ...; GET /users/N
    /// answers N if made; everything else 404. Records request lines.
    fn service() -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut next = 100;
            let mut made = Vec::new();
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&got).to_string();
                let first = text.lines().next().unwrap_or("").to_string();
                log.lock().unwrap().push(text.clone());
                let (status, body) = if first.starts_with("POST /users ") {
                    made.push(next);
                    next += 1;
                    (
                        201,
                        format!("{{\"id\":{},\"at\":\"2026-11-22T08:40:01Z\"}}", next - 1),
                    )
                } else if let Some(rest) = first.strip_prefix("GET /users/") {
                    let id: u32 = rest.split(' ').next().unwrap().parse().unwrap_or(0);
                    if made.contains(&id) {
                        (200, format!("{{\"id\":{id}}}"))
                    } else {
                        (
                            404,
                            r#"{"status":404,"code":"GIONG-E1201","title":"no route"}"#.to_string(),
                        )
                    }
                } else {
                    (404, String::new())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (addr, seen)
    }

    #[test]
    fn a_chain_replays_with_the_ids_made_this_time() {
        let (addr, seen) = service();
        let base = Base::parse(&addr).unwrap();
        let cases = vec![
            case(
                "make",
                "POST",
                "/users",
                Body::Json(json!({"n": 1})),
                201,
                Body::Json(json!({"id": "{{capture:user_id}}", "at": "{{time}}"})),
            ),
            case(
                "read",
                "GET",
                "/users/{{user_id}}",
                Body::None,
                200,
                Body::Json(json!({"id": "{{user_id}}"})),
            ),
        ];
        let r = run(&cases, &base, Duration::from_secs(5), &no_env).unwrap();
        assert!(r.iter().all(|c| c.outcome == Outcome::Passed), "{r:?}");
        let seen = seen.lock().unwrap();
        assert!(seen[1].starts_with("GET /users/100 "), "{}", seen[1]);
        // Twice: the second run gets 101 and still passes.
        drop(seen);
        let r = run(&cases, &base, Duration::from_secs(5), &no_env).unwrap();
        assert!(r.iter().all(|c| c.outcome == Outcome::Passed), "{r:?}");
    }

    #[test]
    fn a_failed_case_keeps_the_captures_it_found() {
        let (addr, _) = service();
        let base = Base::parse(&addr).unwrap();
        let cases = vec![
            // Expects 200, gets 201: fails, but the id is still captured.
            case(
                "make",
                "POST",
                "/users",
                Body::None,
                200,
                Body::Json(json!({"id": "{{capture:user_id}}", "at": "{{any}}"})),
            ),
            case(
                "read",
                "GET",
                "/users/{{user_id}}",
                Body::None,
                200,
                Body::None,
            ),
            // A field that is not there: nothing captured.
            case(
                "make2",
                "POST",
                "/users",
                Body::None,
                201,
                Body::Json(json!({"nope": "{{capture:other}}", "id": "{{any}}", "at": "{{any}}"})),
            ),
            case(
                "read2",
                "GET",
                "/users/{{other}}",
                Body::None,
                200,
                Body::None,
            ),
            case("gone", "GET", "/users/1", Body::None, 200, Body::None),
        ];
        let r = run(&cases, &base, Duration::from_secs(5), &no_env).unwrap();
        match &r[0].outcome {
            Outcome::Failed(d) => assert_eq!(d, &vec!["status: expected 200, got 201".to_string()]),
            o => panic!("{o:?}"),
        }
        assert_eq!(r[1].outcome, Outcome::Passed);
        assert!(
            matches!(&r[2].outcome, Outcome::Failed(d) if d.len() == 1),
            "{:?}",
            r[2]
        );
        assert_eq!(r[3].outcome, Outcome::Missing(Missing::Var("other".into())));
        match &r[4].outcome {
            Outcome::Failed(d) => assert!(d[0].contains("GIONG-E1201: no route"), "{d:?}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn nothing_listening_stops_the_replay() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Base::parse(&l.local_addr().unwrap().to_string()).unwrap();
        drop(l);
        let cases = vec![case("a", "GET", "/", Body::None, 200, Body::None)];
        let (done, stop) = run(&cases, &base, Duration::from_secs(2), &no_env).unwrap_err();
        assert!(done.is_empty());
        assert_eq!(stop.case, "a");
    }

    #[test]
    fn requests_fill_env_query_and_headers() {
        let mut c = case(
            "a",
            "GET",
            "/s/{{id}}",
            Body::Text("for {{id}}".into()),
            200,
            Body::None,
        );
        c.request.query = Some("q={{name}}&flag&k={{id}}".into());
        c.request.headers = vec![("authorization".into(), "{{env:TOKEN}}".into())];
        let mut vars = Vars::new();
        vars.insert("id".into(), json!(5));
        vars.insert("name".into(), json!("a&b=c"));
        let env = |k: &str| (k == "TOKEN").then(|| "Bearer t".to_string());
        let req = request(&c, &vars, &env).unwrap();
        assert_eq!(req.target, "/s/5?q=a%26b%3Dc&flag&k=5");
        assert_eq!(req.headers[0].1, "Bearer t");
        assert_eq!(req.body, b"for 5");
        assert_eq!(
            request(&c, &vars, &no_env).unwrap_err(),
            Missing::Env("TOKEN".into())
        );
    }

    #[test]
    fn checks_cover_headers_and_bodies() {
        let mut c = case(
            "a",
            "GET",
            "/",
            Body::None,
            200,
            Body::Text("hello {{string}}".into()),
        );
        c.expect
            .headers
            .insert("content-type".into(), "text/plain; charset=utf-8".into());
        c.expect
            .headers
            .insert("location".into(), "/u/{{capture:id}}".into());
        let answer = web_client::Answer {
            status: 200,
            headers: vec![
                ("content-type".into(), "TEXT/PLAIN;charset=UTF-8".into()),
                ("location".into(), "/u/9".into()),
            ],
            body: b"hello you".to_vec(),
            elapsed: Duration::ZERO,
        };
        let mut vars = Vars::new();
        assert!(check(&c, &answer, &mut vars).is_empty());
        assert_eq!(vars["id"], json!(9));
        let mut bad = answer.clone();
        bad.headers.clear();
        bad.body = b"bye".to_vec();
        let d = check(&c, &bad, &mut Vars::new());
        assert_eq!(d.len(), 3, "{d:?}");
        c.expect.body = Body::Bytes(vec![1, 2]);
        c.expect.headers.clear();
        assert_eq!(check(&c, &answer, &mut Vars::new()).len(), 1);
        c.expect.body = Body::Unchecked("x".into());
        assert!(check(&c, &answer, &mut Vars::new()).is_empty());
        c.expect.body = Body::Json(json!({}));
        assert!(check(&c, &answer, &mut Vars::new())[0].contains("expected JSON"));
    }
}
