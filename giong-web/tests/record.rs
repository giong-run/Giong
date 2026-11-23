//! The `Record` middleware: one `exchange` line per answered request, with
//! headers and bodies, through `App::handle` and through a real server.
//! A release build has `Record` only with the `dev` feature (record_off.rs
//! tests the other case).
#![cfg(any(debug_assertions, feature = "dev"))]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use giong::net::TcpListener;
use giong_http::client::Client;
use giong_web::middleware::{Record, RequestId, Timeout};
use giong_web::{App, Body, Error, Req, Response, Router, Server, Sink};
use serde_json::Value;

fn memory() -> (Sink, Rc<RefCell<Vec<String>>>) {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let keep = lines.clone();
    (
        Sink::Custom(Rc::new(move |line: &str| {
            keep.borrow_mut().push(line.to_string())
        })),
        lines,
    )
}

fn send<S: 'static>(app: &App<S>, raw: &[u8]) -> Response {
    let req = giong_web::Request::from_bytes(raw).unwrap_or_else(|s| panic!("{s}"));
    giong::block_on(app.handle(req))
}

fn parse(lines: &Rc<RefCell<Vec<String>>>) -> Vec<Value> {
    lines
        .borrow()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

fn b64decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("bad base64 {c}"),
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in chunk {
            n = (n << 6) | u32::from(if c == b'=' { 0 } else { val(c) });
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    out
}

async fn create(req: Req) -> Result<Response, Error> {
    let body = String::from_utf8_lossy(req.body()).into_owned();
    Ok(Response::new(201)
        .header("Content-Type", "application/json")
        .header("Set-Cookie", "session=s3cret")
        .header("X-Echo-Len", body.len().to_string())
        .body(format!("{{\"got\":{body}}}")))
}

async fn binary(_req: Req) -> Result<Response, Error> {
    Ok(Response::new(200).body(vec![0u8, 159, 146, 150, 255, 1, 2]))
}

async fn boom(_req: Req) -> Result<Response, Error> {
    panic!("handler exploded");
}

async fn stream_out(_req: Req) -> Result<Response, Error> {
    let (tx, body) = Body::channel();
    drop(giong::spawn(async move {
        let _ = tx.send("one").await;
        let _ = tx.send("two").await;
    }));
    Ok(Response::new(200).body(body))
}

fn app(record: Record) -> App {
    Router::new()
        .post("/users", create)
        .get("/bin", binary)
        .get("/boom", boom)
        .get("/stream", stream_out)
        .layer(RequestId::new())
        .layer(record)
        .build(())
}

#[test]
fn an_exchange_has_both_sides_with_secrets_redacted() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink));
    let body = r#"{"name":"ada"}"#;
    let raw = format!(
        "POST /users?x=1&y=two HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
         Authorization: Bearer abc\r\nCookie: a=b\r\nX-Api-Key: k\r\nX-Auth-Token: t\r\n\
         Proxy-Authorization: Basic zz\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let resp = send(&app, raw.as_bytes());
    assert_eq!(resp.status(), 201);
    let got = parse(&lines);
    assert_eq!(got.len(), 1);
    let line = &got[0];
    assert_eq!(line["event"], "exchange");
    assert_eq!(line["level"], "info");
    assert_eq!(line["method"], "POST");
    assert_eq!(line["path"], "/users");
    assert_eq!(line["query"], "x=1&y=two");
    assert_eq!(line["route"], "/users");
    assert_eq!(line["status"], 201);
    assert!(line["duration_us"].is_u64());
    assert_eq!(
        line["request_id"].as_str().unwrap(),
        resp.get_header("x-request-id").unwrap()
    );
    assert!(line["ts"].as_str().unwrap().ends_with('Z'));

    let req = &line["request"];
    assert_eq!(req["body"], body);
    assert_eq!(req["body_len"], body.len());
    assert_eq!(req["truncated"], false);
    assert_eq!(req["streamed"], false);
    let headers: Vec<(String, String)> = req["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h[0].as_str().unwrap().into(), h[1].as_str().unwrap().into()))
        .collect();
    let value = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(value("content-type"), Some("application/json"));
    for secret in [
        "authorization",
        "cookie",
        "x-api-key",
        "x-auth-token",
        "proxy-authorization",
    ] {
        assert_eq!(value(secret), Some("[redacted]"), "{secret}");
    }
    let all = lines.borrow().join("\n");
    for leaked in ["Bearer abc", "a=b", "Basic zz", "s3cret"] {
        assert!(!all.contains(leaked), "{leaked} leaked: {all}");
    }

    let resp_side = &line["response"];
    assert_eq!(resp_side["body"], format!("{{\"got\":{body}}}"));
    let set_cookie = resp_side["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h[0] == "set-cookie")
        .unwrap();
    assert_eq!(set_cookie[1], "[redacted]");
}

async fn login(_req: Req) -> Result<Response, Error> {
    Ok(Response::new(200)
        .header("Content-Type", "application/json")
        .body(r#"{"user":"ada","access_token":"eyJ.SECRET.sig","expires_in":3600}"#))
}

#[test]
fn secrets_in_queries_and_bodies_are_redacted() {
    let (sink, lines) = memory();
    let app = Router::new()
        .post("/login", login)
        .layer(Record::new(sink).keep_field("cursor_token"))
        .build(());
    let json = r#"{"user":"ada","password":"hunter2-SECRET","card":{"number":"4242424242424242","cvv":"123"}}"#;
    let raw = format!(
        "POST /login?api_key=AK-SECRET-123&page=2&cursor_token=c1 HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
        json.len()
    );
    send(&app, raw.as_bytes());
    let form = "user=ada&password=hunter2-SECRET";
    let raw = format!(
        "POST /login HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    );
    send(&app, raw.as_bytes());
    let got = parse(&lines);
    assert_eq!(got[0]["query"], "api_key=[redacted]&page=2&cursor_token=c1");
    let body: Value = serde_json::from_str(got[0]["request"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["user"], "ada");
    assert_eq!(body["password"], "[redacted]");
    assert_eq!(body["card"], "[redacted]");
    // body_len is the length of the body as it was sent.
    assert_eq!(got[0]["request"]["body_len"], json.len());
    let resp: Value = serde_json::from_str(got[0]["response"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(resp["access_token"], "[redacted]");
    assert_eq!(resp["expires_in"], 3600);
    assert_eq!(got[1]["request"]["body"], "user=ada&password=[redacted]");
    let all = lines.borrow().join("\n");
    for leaked in ["hunter2", "AK-SECRET", "4242424242424242", "eyJ.SECRET"] {
        assert!(!all.contains(leaked), "{leaked} leaked: {all}");
    }
}

#[test]
fn binary_bodies_are_base64_and_round_trip() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink));
    send(&app, b"GET /bin HTTP/1.1\r\nHost: t\r\n\r\n");
    let line = &parse(&lines)[0];
    let side = &line["response"];
    assert!(side.get("body").is_none());
    assert_eq!(
        b64decode(side["body_base64"].as_str().unwrap()),
        [0u8, 159, 146, 150, 255, 1, 2]
    );
    assert_eq!(side["body_len"], 7);
    // An empty request body is text.
    assert_eq!(line["request"]["body"], "");
}

#[test]
fn bodies_are_cut_at_max_body() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink).max_body(10));
    let body = "x".repeat(100);
    let raw = format!("POST /users HTTP/1.1\r\nHost: t\r\nContent-Length: 100\r\n\r\n{body}");
    send(&app, raw.as_bytes());
    let line = &parse(&lines)[0];
    assert_eq!(line["request"]["body"], "x".repeat(10));
    assert_eq!(line["request"]["body_len"], 100);
    assert_eq!(line["request"]["truncated"], true);
    assert_eq!(line["response"]["truncated"], true);
}

#[test]
fn max_body_zero_records_no_body_bytes() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink).max_body(0));
    send(
        &app,
        b"POST /users HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}",
    );
    let line = &parse(&lines)[0];
    assert_eq!(line["request"]["body"], "");
    assert_eq!(line["request"]["body_len"], 2);
    assert_eq!(line["request"]["truncated"], true);
}

#[test]
#[should_panic(expected = "GIONG-E1213")]
fn max_body_above_the_limit_panics_with_a_code() {
    let _ = Record::new(Sink::Stdout).max_body(128 * 1024 + 1);
}

#[test]
fn the_largest_max_body_still_fits_the_log_writer() {
    let _ = Record::new(Sink::Stdout).max_body(128 * 1024);
}

#[test]
fn redact_header_is_case_insensitive() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink).redact_header("X-Tenant-Secret"));
    send(
        &app,
        b"POST /users HTTP/1.1\r\nHost: t\r\nx-TENANT-secret: hush\r\nContent-Length: 2\r\n\r\n{}",
    );
    let all = lines.borrow().join("");
    assert!(!all.contains("hush"), "{all}");
    assert!(all.contains(r#"["x-tenant-secret","[redacted]"]"#), "{all}");
}

#[test]
fn errors_404s_and_panics_are_recorded_too() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink));
    send(&app, b"GET /nowhere HTTP/1.1\r\nHost: t\r\n\r\n");
    send(&app, b"GET /boom HTTP/1.1\r\nHost: t\r\n\r\n");
    let got = parse(&lines);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["status"], 404);
    assert_eq!(got[0]["route"], Value::Null);
    assert!(got[0]["response"]["body"]
        .as_str()
        .unwrap()
        .contains("GIONG-E1203"));
    assert_eq!(got[1]["status"], 500);
    assert_eq!(got[1]["route"], "/boom");
    // The panic message is the cause, which never reaches the client.
    assert!(!got[1]["response"]["body"]
        .as_str()
        .unwrap()
        .contains("handler exploded"));
}

#[test]
fn a_streamed_response_is_marked_not_recorded() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink));
    send(&app, b"GET /stream HTTP/1.1\r\nHost: t\r\n\r\n");
    let side = &parse(&lines)[0]["response"];
    assert_eq!(side["streamed"], true);
    assert_eq!(side["body_len"], Value::Null);
    assert!(side.get("body").is_none() && side.get("body_base64").is_none());
}

#[test]
fn a_cancelled_request_writes_no_line() {
    async fn slow(_req: Req) -> Result<Response, Error> {
        giong::time::sleep(Duration::from_secs(5)).await;
        Ok(Response::text("late"))
    }
    let (sink, lines) = memory();
    let app = Router::new()
        .get("/slow", slow)
        .layer(Timeout::new(Duration::from_millis(20)))
        .layer(Record::new(sink))
        .build(());
    let resp = send(&app, b"GET /slow HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(resp.status(), 503);
    assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
}

#[test]
fn a_record_outside_the_timeout_records_the_timeout_answer() {
    async fn slow(_req: Req) -> Result<Response, Error> {
        giong::time::sleep(Duration::from_secs(5)).await;
        Ok(Response::text("late"))
    }
    let (sink, lines) = memory();
    let app = Router::new()
        .get("/slow", slow)
        .layer(Record::new(sink))
        .layer(Timeout::new(Duration::from_millis(20)))
        .build(());
    send(&app, b"GET /slow HTTP/1.1\r\nHost: t\r\n\r\n");
    let got = parse(&lines);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["status"], 503);
}

#[test]
fn odd_bytes_in_paths_headers_and_bodies_always_give_valid_json() {
    let (sink, lines) = memory();
    let app = app(Record::new(sink));
    // Every byte value in the body, quotes and backslashes in the query,
    // a non-UTF-8 header value.
    let mut raw = b"POST /users?q=%22\\\"x HTTP/1.1\r\nHost: t\r\nX-Odd: \xff\xfe\r\nContent-Length: 256\r\n\r\n".to_vec();
    raw.extend(0u8..=255);
    send(&app, &raw);
    let got = parse(&lines);
    assert_eq!(got.len(), 1);
    let side = &got[0]["request"];
    let bytes: Vec<u8> = (0u8..=255).collect();
    assert_eq!(b64decode(side["body_base64"].as_str().unwrap()), bytes);
    // A text body with control characters is base64 too, so it reads back
    // exactly.
    let raw = b"POST /users HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\na\x1bb";
    send(&app, raw);
    let got = parse(&lines);
    assert_eq!(
        b64decode(got[1]["request"]["body_base64"].as_str().unwrap()),
        b"a\x1bb"
    );
}

#[test]
fn a_streamed_request_body_is_marked_and_still_reaches_the_handler() {
    async fn upload(req: Req) -> Result<Response, Error> {
        let body = req.body_stream().expect("a streaming route");
        let mut total = 0;
        while let Some(piece) = body.chunk().await? {
            total += piece.len();
        }
        Ok(Response::text(format!("{total} bytes")))
    }
    giong::block_on(async {
        let (sink, lines) = memory();
        let app = Router::new()
            .put("/up", upload)
            .stream_bodies(1 << 20)
            .layer(Record::new(sink))
            .build(());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        giong::spawn(async move { Server::new().serve(listener, app).await });
        let resp = Client::new()
            .put(&format!("http://{addr}/up"))
            .body(vec![b'z'; 200_000])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.text().unwrap(), "200000 bytes");
        let got = parse(&lines);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["request"]["streamed"], true);
        assert_eq!(got[0]["response"]["body"], "200000 bytes");
        assert!(got[0]["worker"].is_u64());
    });
}
