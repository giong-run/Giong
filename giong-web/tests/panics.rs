//! A handler or a middleware that panics: the request is answered with a
//! `500` like any other failure, and the connection, with every other
//! request on it, carries on.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong_http::client::Client;
use giong_http::Version;
use giong_web::middleware::{Logger, RequestId};
use giong_web::{App, Error, Middleware, Next, Req, Request, Response, Router, Server, Sink};

fn send(app: &App, raw: &str) -> Response {
    let req = Request::from_bytes(raw.as_bytes()).unwrap_or_else(|s| panic!("{s}: {raw:?}"));
    giong::block_on(app.handle(req))
}

fn get(app: &App, target: &str) -> Response {
    send(app, &format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"))
}

fn body(resp: &Response) -> String {
    String::from_utf8_lossy(resp.body_bytes()).into_owned()
}

async fn boom(_req: Req) -> Result<Response, Error> {
    panic!("boom");
}

async fn ok(_req: Req) -> Result<Response, Error> {
    Ok(Response::text("ok"))
}

/// Echoes `?v=` into a header: giong-http's `Response::header` panics on a
/// value with a control character, which `%0A` decodes to.
async fn echo_header(req: Req) -> Result<Response, Error> {
    let v: String = req.query_param("v")?.unwrap_or_default();
    Ok(Response::text("ok").header("X-Echo", &v))
}

async fn slow(_req: Req) -> Result<Response, Error> {
    giong::time::sleep(Duration::from_millis(300)).await;
    Ok(Response::text("slow done"))
}

/// Panics on requests to `/mw-panic`, after the middleware before it ran.
struct PanicsOnPath;

impl<S: 'static> Middleware<S> for PanicsOnPath {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        if req.path() == "/mw-panic" {
            let n: Option<&str> = req.header_str("x-missing");
            let _ = n.expect("the middleware's own bug");
        }
        Ok(next.run(req).await)
    }
}

fn app(lines: Rc<RefCell<Vec<String>>>) -> App {
    Router::new()
        .get("/panic", boom)
        .get("/ok", ok)
        .get("/mw-panic", ok)
        .get("/echo-header", echo_header)
        .get("/slow", slow)
        // A closure that panics before it even returns its future.
        .get("/sync-panic", |req: Req| {
            let n: u32 = req.query_param("n").unwrap().expect("a query parameter n");
            async move { Ok(Response::text(n.to_string())) }
        })
        .layer(RequestId::new())
        .layer(Logger::new(Sink::Custom(Rc::new(move |line: &str| {
            lines.borrow_mut().push(line.to_string());
        }))))
        .layer(PanicsOnPath)
        .build(())
}

/// The response is the problem+json `500` with the request id, and the
/// logger wrote one line for it, with `cause` in it.
fn assert_answered_500(resp: &Response, lines: &[String], cause: &str) {
    assert_eq!(resp.status(), 500);
    assert_eq!(
        resp.get_header("content-type"),
        Some("application/problem+json")
    );
    let id = resp.get_header("x-request-id").expect("a request id");
    let text = body(resp);
    assert!(text.contains("\"code\":\"GIONG-E1210\""), "{text}");
    assert!(text.contains(&format!("\"request_id\":\"{id}\"")), "{text}");
    assert!(!text.contains("panicked"), "the cause is not sent: {text}");
    let err = Error::of(resp).expect("made from an Error");
    let chain = err.source().unwrap().to_string();
    assert!(chain.contains(cause), "{chain}");

    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.contains("\"status\":500"), "{line}");
    assert!(line.contains(&format!("\"request_id\":\"{id}\"")), "{line}");
    assert!(line.contains(cause), "{line}");
}

#[test]
fn a_panicking_handler_is_answered_with_a_500_and_logged_once() {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let app = app(lines.clone());
    let resp = get(&app, "/panic");
    assert_answered_500(&resp, &lines.borrow(), "the handler panicked: boom");
    assert!(
        lines.borrow()[0].contains("\"route\":\"/panic\""),
        "{:?}",
        lines.borrow()
    );

    // The app goes on answering.
    lines.borrow_mut().clear();
    assert_eq!(body(&get(&app, "/ok")), "ok");
}

#[test]
fn a_handler_that_panics_before_returning_its_future_is_caught_too() {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let app = app(lines.clone());
    let resp = get(&app, "/sync-panic");
    assert_answered_500(
        &resp,
        &lines.borrow(),
        "the handler panicked: a query parameter n",
    );
    lines.borrow_mut().clear();
    assert_eq!(body(&get(&app, "/sync-panic?n=7")), "7");
}

#[test]
fn a_panicking_middleware_is_answered_with_a_500_and_logged_once() {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let app = app(lines.clone());
    let resp = get(&app, "/mw-panic");
    assert_answered_500(
        &resp,
        &lines.borrow(),
        "a middleware panicked: the middleware's own bug",
    );
}

#[test]
fn a_panic_with_no_logger_still_answers_500() {
    // No middleware at all: App::handle itself writes the cause to stderr.
    let app = Router::new().get("/panic", boom).build(());
    let resp = get(&app, "/panic");
    assert_eq!(resp.status(), 500);
    assert!(body(&resp).contains("\"code\":\"GIONG-E1210\""));
    // A payload that is not a string still gives a cause.
    let app = Router::new()
        .get("/any", |_req: Req| async {
            std::panic::panic_any(42_u8);
            #[allow(unreachable_code)]
            Ok(Response::new(204))
        })
        .build(());
    let resp = get(&app, "/any");
    assert_eq!(resp.status(), 500);
    let cause = Error::of(&resp).unwrap().source().unwrap().to_string();
    assert_eq!(
        cause,
        "the handler panicked (with a payload that is not a string)"
    );
}

#[test]
fn a_header_value_from_the_request_that_panics_is_a_500() {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let app = app(lines.clone());
    let resp = get(&app, "/echo-header?v=a%0Ab");
    assert_answered_500(&resp, &lines.borrow(), "invalid value for header");
}

/// Serves `app()` on a free port of this worker; returns the address.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app(Rc::new(RefCell::new(Vec::new())));
    giong::spawn(async move { Server::new().serve(listener, app).await });
    addr.to_string()
}

#[test]
fn a_panic_keeps_the_http1_connection_open() {
    giong::block_on(async {
        let addr = serve();
        let stream = TcpStream::connect(&addr).await.unwrap();
        let requests = "GET /echo-header?v=a%0Ab HTTP/1.1\r\nHost: t\r\n\r\n\
                        GET /ok HTTP/1.1\r\nHost: t\r\n\r\n";
        let (res, _) = stream.write_all(requests.as_bytes().to_vec()).await;
        res.unwrap();
        let mut got = Vec::new();
        while !String::from_utf8_lossy(&got).ends_with("\r\n\r\nok") {
            let chunk = giong::time::timeout(Duration::from_secs(10), stream.recv())
                .await
                .expect("an answer within 10 s")
                .unwrap();
            assert!(
                !chunk.is_empty(),
                "the server closed the connection: {}",
                String::from_utf8_lossy(&got)
            );
            got.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&got);
        assert!(
            text.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{text}"
        );
        assert_eq!(text.matches("HTTP/1.1 200 OK\r\n").count(), 1, "{text}");
    });
}

#[test]
fn a_panic_in_one_http2_stream_leaves_the_others_alone() {
    giong::block_on(async {
        let base = format!("http://{}", serve());
        let client = Client::builder().http2_prior_knowledge(true).build();
        // Open the connection, so that both requests below share it.
        let first = client.get(&format!("{base}/ok")).send().await.unwrap();
        assert_eq!(first.version(), Version::Http2);
        let other = client.clone();
        let url = format!("{base}/slow");
        let slow = giong::spawn(async move { other.get(&url).send().await });
        giong::time::sleep(Duration::from_millis(50)).await;
        let panicked = client.get(&format!("{base}/panic")).send().await.unwrap();
        assert_eq!(panicked.status(), 500);
        let slow = slow.await.unwrap().expect("the slow request completes");
        assert_eq!(slow.status(), 200);
        assert_eq!(slow.text().unwrap(), "slow done");
        assert_eq!(client.connections(&base).0, 1, "one connection throughout");
    });
}
