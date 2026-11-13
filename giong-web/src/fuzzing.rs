//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-web/fuzz`).
//!
//! * [`check_routes`]: routes and a request path made from the input. The
//!   route table finds what a slow reference matcher finds, with the same
//!   parameters (see `route/model.rs`).
//! * [`check_request`] (feature `serde`): the input as a request to an app
//!   with every middleware. Whatever it is, the answer is well formed: a
//!   valid status and headers, a problem+json body with a code for every
//!   error, gzip that decodes to what the handler wrote, one log line that
//!   is JSON and agrees with the response, and the request id sent back.

/// Routes and a request from `data`; panics if the route table and the
/// reference matcher disagree.
pub fn check_routes(data: &[u8]) {
    crate::route::model::check(data);
}

#[cfg(feature = "serde")]
pub use app::check_request;

#[cfg(feature = "serde")]
pub(crate) mod app {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use crate::header::{is_token, is_value, list_contains};
    use crate::middleware::{Compression, Cors, Logger, RequestId};
    use crate::{json, App, Error, Req, Request, Response, Router, Sink};

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Search<'a> {
        q: Option<std::borrow::Cow<'a, str>>,
        page: Option<u32>,
        exact: Option<bool>,
        sort: Option<Sort>,
    }

    #[derive(Deserialize, Serialize)]
    struct Item<'a> {
        #[serde(borrow)]
        name: std::borrow::Cow<'a, str>,
        n: Option<i64>,
        tags: Vec<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum Sort {
        Asc,
        Desc,
    }

    async fn user(req: Req) -> Result<Response, Error> {
        let id: u64 = req.param("id")?;
        Ok(Response::text(format!("user {id}")))
    }

    async fn me(_req: Req) -> Result<Response, Error> {
        Ok(Response::text("me"))
    }

    async fn file(req: Req) -> Result<Response, Error> {
        let path: String = req.param("path")?;
        Ok(Response::text(path))
    }

    async fn search(req: Req) -> Result<Response, Error> {
        let s: Search = req.query()?;
        let n = req.query_param::<i32>("n")?;
        Ok(Response::text(format!("{:?} {:?} {n:?}", s.q, s.page)))
    }

    async fn echo(req: Req) -> Result<Response, Error> {
        Ok(Response::text(req.body().to_vec()))
    }

    async fn json_echo(req: Req) -> Result<Response, Error> {
        let value: Value = req.json()?;
        json(200, &value)
    }

    async fn create_item(req: Req) -> Result<Response, Error> {
        let item: Item = req.json()?;
        json(201, &item)
    }

    async fn conflict(req: Req) -> Result<Response, Error> {
        let id: u32 = req.param("id")?;
        Err(Error::conflict("item_busy", format!("item {id} is busy")))
    }

    thread_local! {
        static LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static APP: App = build();
    }

    fn build() -> App {
        let api = Router::new()
            .post("/echo", echo)
            .post("/json", json_echo)
            .post("/items", create_item)
            .delete("/items/{id}", conflict);
        Router::new()
            .get("/users/{id}", user)
            .get("/users/me", me)
            .get("/files/{*path}", file)
            .get("/search", search)
            .nest("/api", api)
            .layer(RequestId::new().trust_incoming(true))
            .layer(
                Logger::new(Sink::Custom(Rc::new(|line| {
                    LINES.with(|l| l.borrow_mut().push(line.to_string()));
                })))
                .include_query(true),
            )
            .layer(
                Cors::new()
                    .allow_origin("https://app.example")
                    .allow_methods(&["GET", "POST", "DELETE"])
                    .allow_headers(&["content-type"])
                    .expose_headers(&["x-request-id"])
                    .allow_credentials(true),
            )
            .layer(Compression::new().min_size(32))
            .build(())
    }

    /// Runs `fut` to the end without a runtime: nothing in the app above
    /// waits for I/O or a timer, and `yield_now` wakes itself.
    fn run<F: Future>(fut: F) -> F::Output {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: the vtable's functions do nothing with the null pointer.
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(fut);
        loop {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    /// `data` as a request; panics if the answer is not well formed.
    pub fn check_request(data: &[u8]) {
        let Ok(req) = Request::from_bytes(data) else {
            return;
        };
        check(req);
    }

    /// Answers `req` and checks the answer. Shared with the randomised
    /// tests, which build their requests on purpose.
    pub(crate) fn check(req: Request) -> Response {
        let method = req.method().to_string();
        let body = req.body().to_vec();
        let path = req.path().to_string();
        let accepts_gzip = req
            .headers()
            .filter(|(n, _)| n.eq_ignore_ascii_case("accept-encoding"))
            .filter_map(|(_, v)| std::str::from_utf8(v).ok())
            .any(|v| v.to_ascii_lowercase().contains("gzip") || v.contains('*'));
        let incoming_id = req
            .headers()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-request-id"))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
            .map(str::to_string);

        LINES.with(|l| l.borrow_mut().clear());
        let resp = APP.with(|app| run(app.handle(req)));
        let what = format!("{method} {path:?}");

        let status = resp.status();
        assert!((100..=599).contains(&status), "{what}: status {status}");
        for (name, value) in resp.headers() {
            assert!(is_token(name), "{what}: header name {name:?}");
            assert!(is_value(value), "{what}: header {name}: {value:?}");
        }

        // The request id: sent back, and kept when it was a valid one.
        let ids: Vec<&str> = resp
            .headers()
            .filter(|(n, _)| n.eq_ignore_ascii_case("x-request-id"))
            .map(|(_, v)| v)
            .collect();
        assert_eq!(ids.len(), 1, "{what}: {ids:?}");
        let id = ids[0];
        if let Some(incoming) = incoming_id.as_deref().filter(|i| valid_id(i)) {
            assert_eq!(id, incoming, "{what}");
        }
        assert!(valid_id(id), "{what}: id {id:?}");

        // The body, decompressed when it was.
        let encoded = resp.get_header("content-encoding");
        let plain = match encoded {
            Some("gzip") => {
                assert!(accepts_gzip, "{what}: gzip for a client that did not ask");
                assert!(
                    resp.headers().any(|(n, v)| n.eq_ignore_ascii_case("vary")
                        && list_contains(v, "Accept-Encoding")),
                    "{what}: gzip without Vary"
                );
                crate::middleware::gunzip(resp.body_bytes())
            }
            None => resp.body_bytes().to_vec(),
            Some(other) => panic!("{what}: Content-Encoding {other}"),
        };

        if status >= 400 {
            check_problem(&resp, &plain, id, &what);
        } else if status == 200 && method == "POST" && path == "/api/echo" {
            assert_eq!(plain, body, "{what}: the echo differs");
        } else if status == 200 && method == "POST" && path == "/api/json" {
            let sent: Value = serde_json::from_slice(&body).expect("the app accepted it");
            let back: Value = serde_json::from_slice(&plain).expect("json() writes JSON");
            assert_eq!(sent, back, "{what}");
        } else if status == 201 {
            let sent: Item = serde_json::from_slice(&body).expect("the app accepted it");
            let back: Item = serde_json::from_slice(&plain).expect("json() writes JSON");
            assert_eq!(
                (sent.name, sent.n, sent.tags),
                (back.name, back.n, back.tags),
                "{what}"
            );
        }

        // One log line per request, agreeing with the response.
        let lines = LINES.with(|l| std::mem::take(&mut *l.borrow_mut()));
        assert_eq!(lines.len(), 1, "{what}: log lines {lines:?}");
        let line: Value = serde_json::from_str(&lines[0])
            .unwrap_or_else(|e| panic!("{what}: log line is not JSON ({e}): {}", lines[0]));
        assert_eq!(line["status"], u64::from(status), "{what}: {line}");
        assert_eq!(line["method"], method.as_str(), "{what}: {line}");
        assert_eq!(line["request_id"], id, "{what}: {line}");
        assert_eq!(line["bytes"], resp.body_bytes().len(), "{what}: {line}");
        if status >= 400 && status != 405 {
            let code = &line["error"]["code"];
            assert!(code.is_string(), "{what}: no code in {line}");
        }
        resp
    }

    fn valid_id(id: &str) -> bool {
        (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/+=".contains(&b))
    }

    /// An error answer is problem+json with the response's status, a code,
    /// the reason phrase and the request id.
    fn check_problem(resp: &Response, body: &[u8], id: &str, what: &str) {
        assert_eq!(
            resp.get_header("content-type"),
            Some("application/problem+json"),
            "{what}"
        );
        assert_eq!(resp.get_header("cache-control"), Some("no-store"), "{what}");
        let problem: Value = serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "{what}: problem is not JSON ({e}): {:?}",
                String::from_utf8_lossy(body)
            )
        });
        let status = resp.status();
        assert_eq!(problem["status"], u64::from(status), "{what}: {problem}");
        assert_eq!(
            problem["title"],
            giong_http::reason_phrase(status),
            "{what}: {problem}"
        );
        assert!(problem["detail"].is_string(), "{what}: {problem}");
        assert_eq!(problem["request_id"], id, "{what}: {problem}");
        let code = problem["code"].as_str().unwrap_or_default();
        assert!(
            code.starts_with("GIONG-E12") || code == "item_busy",
            "{what}: code {code:?}"
        );
        let err = Error::of(resp).unwrap_or_else(|| panic!("{what}: no Error"));
        assert_eq!((err.status(), err.code()), (status, code), "{what}");
    }
}
