//! `giong record` end to end: a real giong-web service records requests with
//! the `Record` middleware, `giong record list` and `save` turn them into a
//! case file, and `giong record replay` checks a second service (which makes
//! different ids) against it, then services that answer differently.
#![cfg(unix)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use giong::net::TcpListener;
use giong_web::middleware::{Record, RequestId};
use giong_web::{Error, Req, Response, Router, Server, Sink};
use serde_json::{json, Value};

/// The users of one service: the next id, and the users made.
struct Users {
    next: u64,
    names: BTreeMap<u64, String>,
    /// Answer names in upper case: a "regression".
    shout: bool,
}

type State = Rc<RefCell<Users>>;

fn user_json(id: u64, name: &str) -> String {
    json!({"id": id, "name": name, "created_at": "2026-11-22T08:40:01.123Z"}).to_string()
}

async fn create(req: Req<State>) -> Result<Response, Error> {
    let body: Value =
        serde_json::from_slice(req.body()).map_err(|e| Error::new(422, "BAD", e.to_string()))?;
    let name = body["name"].as_str().unwrap_or("").to_string();
    let mut u = req.state().borrow_mut();
    let id = u.next;
    u.next += 1;
    u.names.insert(id, name.clone());
    let shown = if u.shout { name.to_uppercase() } else { name };
    Ok(Response::new(201)
        .header("Content-Type", "application/json")
        .header("Location", format!("/users/{id}"))
        .body(user_json(id, &shown)))
}

async fn read(req: Req<State>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;
    let u = req.state().borrow();
    match u.names.get(&id) {
        Some(n) => {
            let shown = if u.shout { n.to_uppercase() } else { n.clone() };
            Ok(Response::json(user_json(id, &shown)))
        }
        None => Err(Error::new(404, "USER_NOT_FOUND", format!("no user {id}"))),
    }
}

async fn remove(req: Req<State>) -> Result<Response, Error> {
    if req.header_str("authorization") != Some("Bearer s3cret") {
        return Err(Error::new(401, "NO_AUTH", "who are you?"));
    }
    let id: u64 = req.param("id")?;
    match req.state().borrow_mut().names.remove(&id) {
        Some(_) => Ok(Response::new(204)),
        None => Err(Error::new(404, "USER_NOT_FOUND", format!("no user {id}"))),
    }
}

/// Starts a service on a free port; with `log`, it records to that file.
/// The service lives until the test process ends.
fn service(first_id: u64, shout: bool, log: Option<PathBuf>) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        giong::block_on(async move {
            let state = Rc::new(RefCell::new(Users {
                next: first_id,
                names: BTreeMap::new(),
                shout,
            }));
            let mut router = Router::new()
                .post("/users", create)
                .get("/users/{id}", read)
                .delete("/users/{id}", remove)
                .layer(RequestId::new());
            if let Some(path) = log {
                router = router.layer(Record::new(Sink::Custom(Rc::new(move |line: &str| {
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .unwrap();
                    f.write_all(line.as_bytes()).unwrap();
                    if !line.ends_with('\n') {
                        f.write_all(b"\n").unwrap();
                    }
                }))));
            }
            let app = router.build(state);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            tx.send(listener.local_addr().unwrap().to_string()).unwrap();
            let _ = Server::new().serve(listener, app).await;
        })
    });
    rx.recv_timeout(Duration::from_secs(30)).unwrap()
}

/// Sends one request and gives the status line's code and the body.
fn send(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("giong-cli-record-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `giong` in `dir` with `args` plus `--json`; gives the exit code and
/// the report.
fn giong(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_giong"));
    cmd.args(args).arg("--json").current_dir(dir);
    cmd.env_remove("GIONG_RECORD_AUTHORIZATION")
        .env_remove("GIONG_MCP");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("{e}: {text} / {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap_or(-1), v)
}

fn codes(report: &Value) -> Vec<String> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

/// Records a session against a fresh service and saves it as cases.
fn record_session(dir: &Path) -> PathBuf {
    let log = dir.join("dev.log");
    std::fs::write(&log, "service starting\n").unwrap();
    let addr = service(1, false, Some(log.clone()));
    let json_ct = [("Content-Type", "application/json")];
    assert_eq!(
        send(&addr, "POST", "/users", &json_ct, r#"{"name":"ada"}"#).0,
        201
    );
    assert_eq!(send(&addr, "GET", "/users/1", &[], "").0, 200);
    assert_eq!(send(&addr, "GET", "/users/99", &[], "").0, 404);
    let auth = [("Authorization", "Bearer s3cret")];
    assert_eq!(send(&addr, "DELETE", "/users/1", &auth, "").0, 204);
    assert_eq!(send(&addr, "GET", "/users/1", &[], "").0, 404);

    let (code, list) = giong(
        dir,
        &["record", "list", "--from", log.to_str().unwrap()],
        &[],
    );
    assert_eq!(code, 0, "{list}");
    assert_eq!(list["data"]["total"], 5, "{list}");
    let ex = &list["data"]["exchanges"];
    assert_eq!(ex[0]["method"], "POST");
    assert_eq!(ex[2]["status"], 404);

    let (code, list) = giong(
        dir,
        &[
            "record",
            "list",
            "--from",
            log.to_str().unwrap(),
            "--status",
            "4xx",
        ],
        &[],
    );
    assert_eq!(code, 0);
    assert_eq!(list["data"]["matching"], 2, "{list}");

    let (code, shown) = giong(
        dir,
        &["record", "show", "4", "--from", log.to_str().unwrap()],
        &[],
    );
    assert_eq!(code, 0, "{shown}");
    // The secret never reaches the log.
    assert!(!shown.to_string().contains("s3cret"), "{shown}");

    let cases = dir.join("tests/recorded/users.json");
    let (code, saved) = giong(
        dir,
        &[
            "record",
            "save",
            "all",
            "--from",
            log.to_str().unwrap(),
            "--out",
            cases.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["data"]["added"], 5);
    assert_eq!(saved["data"]["env"], json!(["GIONG_RECORD_AUTHORIZATION"]));
    cases
}

#[test]
fn recorded_requests_replay_against_a_service_that_makes_other_ids() {
    let dir = temp_dir("replay");
    let cases = record_session(&dir);
    let text = std::fs::read_to_string(&cases).unwrap();
    assert!(text.contains("{{capture:user_id}}"), "{text}");
    assert!(text.contains("/users/{{user_id}}"), "{text}");
    assert!(
        text.contains("{{env:GIONG_RECORD_AUTHORIZATION}}"),
        "{text}"
    );

    // A fresh service whose ids start at 500: the captured id is reused.
    let addr = service(500, false, None);
    let base = format!("http://{addr}");
    let auth = [("GIONG_RECORD_AUTHORIZATION", "Bearer s3cret")];
    let (code, r) = giong(&dir, &["record", "replay", "--base-url", &base], &auth);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["data"]["passed"], 5, "{r}");
    assert_eq!(r["data"]["failed"], 0, "{r}");
    assert!(codes(&r).is_empty(), "{r}");

    // Without the secret, the DELETE cannot be sent; the cases after it
    // still run (and the final GET then finds the user, so it fails too).
    let addr = service(700, false, None);
    let (code, r) = giong(
        &dir,
        &["record", "replay", "--base-url", &format!("http://{addr}")],
        &[],
    );
    assert_eq!(code, 1, "{r}");
    let c = codes(&r);
    assert!(c.contains(&"GIONG-E5068".to_string()), "{r}");
    assert!(c.contains(&"GIONG-E5066".to_string()), "{r}");
    let f = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "GIONG-E5068")
        .unwrap();
    assert!(
        f["fix"]
            .as_str()
            .unwrap()
            .contains("GIONG_RECORD_AUTHORIZATION"),
        "{f}"
    );
    assert!(
        f["file"].as_str().is_some() || f["place"].is_object() || f.get("line").is_some(),
        "{f}"
    );
}

#[test]
fn a_regression_is_reported_with_the_place_and_the_difference() {
    let dir = temp_dir("regress");
    record_session(&dir);
    let addr = service(1, true, None);
    let auth = [("GIONG_RECORD_AUTHORIZATION", "Bearer s3cret")];
    let (code, r) = giong(
        &dir,
        &["record", "replay", "--base-url", &format!("http://{addr}")],
        &auth,
    );
    assert_eq!(code, 1, "{r}");
    let failed: Vec<&Value> = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "GIONG-E5066")
        .collect();
    // The POST and the first GET answer "ADA".
    assert_eq!(failed.len(), 2, "{r}");
    assert!(
        failed[0]["detail"]
            .as_str()
            .unwrap()
            .contains(r#"body.name: expected "ada", got "ADA""#),
        "{}",
        failed[0]
    );
}

#[test]
fn replay_against_nothing_stops_with_a_code() {
    let dir = temp_dir("dead");
    record_session(&dir);
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let (code, r) = giong(
        &dir,
        &["record", "replay", "--base-url", &format!("http://{addr}")],
        &[],
    );
    assert_eq!(code, 2, "{r}");
    assert_eq!(r["error"]["code"], "GIONG-E5069", "{r}");
}

#[test]
fn usage_mistakes_are_coded() {
    let dir = temp_dir("usage");
    for (args, code) in [
        (vec!["record"], "GIONG-E5000"),
        (vec!["record", "lsit"], "GIONG-E5000"),
        (vec!["record", "list", "--name", "x"], "GIONG-E5000"),
        (
            vec!["record", "list", "--status", "6xx", "--from", "/dev/null"],
            "GIONG-E5000",
        ),
        (vec!["record", "list"], "GIONG-E5065"),
        (vec!["record", "replay"], "GIONG-E5065"),
        (
            vec!["record", "save", "all", "--from", "/nonexistent/log"],
            "GIONG-E5065",
        ),
        (
            vec!["record", "show", "x", "--from", "/dev/null"],
            "GIONG-E5000",
        ),
    ] {
        let (exit, r) = giong(&dir, &args, &[]);
        assert_eq!(exit, 2, "{args:?}: {r}");
        assert_eq!(r["error"]["code"], code, "{args:?}: {r}");
    }
    // An empty log lists nothing, with a hint.
    let (exit, r) = giong(&dir, &["record", "list", "--from", "/dev/null"], &[]);
    assert_eq!(exit, 0, "{r}");
    assert_eq!(codes(&r), vec!["GIONG-E5065"], "{r}");
    // Saving twice to one file needs --append or --force.
    let log = dir.join("one.log");
    std::fs::write(&log, r#"{"event":"exchange","method":"GET","path":"/","query":null,"route":"/","status":200,"duration_us":1,"request":{"headers":[],"body":"","body_len":0,"truncated":false,"streamed":false},"response":{"headers":[],"body":"hi","body_len":2,"truncated":false,"streamed":false}}"#).unwrap();
    let from = log.to_str().unwrap();
    let (exit, r) = giong(
        &dir,
        &["record", "save", "1", "--from", from, "--name", "one"],
        &[],
    );
    assert_eq!(exit, 0, "{r}");
    let (exit, r) = giong(
        &dir,
        &["record", "save", "1", "--from", from, "--name", "one"],
        &[],
    );
    assert_eq!(exit, 2, "{r}");
    assert!(
        r["error"]["fix"].as_str().unwrap().contains("--append"),
        "{r}"
    );
    let (exit, r) = giong(
        &dir,
        &[
            "record", "save", "1", "--from", from, "--name", "one", "--append",
        ],
        &[],
    );
    assert_eq!(exit, 0, "{r}");
    assert_eq!(r["data"]["cases"], 2, "{r}");
    let (exit, r) = giong(
        &dir,
        &["record", "save", "1", "--from", from, "--name", "../evil"],
        &[],
    );
    assert_eq!(exit, 2, "{r}");
}
