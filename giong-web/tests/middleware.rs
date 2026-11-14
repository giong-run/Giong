//! The middleware in `giong_web::middleware`, through `App::handle`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use giong_web::middleware::{Compression, Cors, Logger, RequestId, Timeout};
use giong_web::{App, Error, Req, Request, Response, Router, Sink};

fn send<S: 'static>(app: &App<S>, raw: &str) -> Response {
    let req = Request::from_bytes(raw.as_bytes()).unwrap_or_else(|s| panic!("{s}: {raw:?}"));
    giong::block_on(app.handle(req))
}

fn get<S: 'static>(app: &App<S>, target: &str, headers: &str) -> Response {
    send(
        app,
        &format!("GET {target} HTTP/1.1\r\nHost: t\r\n{headers}\r\n"),
    )
}

fn body(resp: &Response) -> String {
    String::from_utf8_lossy(resp.body_bytes()).into_owned()
}

async fn ok(_req: Req) -> Result<Response, Error> {
    Ok(Response::text("ok"))
}

async fn fail(_req: Req) -> Result<Response, Error> {
    Err(std::io::Error::other("database exploded"))?
}

/// A sink that keeps the lines.
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

#[test]
fn request_ids_reach_the_response_the_error_body_and_the_handler() {
    async fn echo_id(req: Req) -> Result<Response, Error> {
        Ok(Response::text(
            req.request_id().unwrap_or("none").to_string(),
        ))
    }
    let app = Router::new()
        .get("/id", echo_id)
        .layer(RequestId::new())
        .build(());
    let resp = get(&app, "/id", "");
    let id = resp.get_header("x-request-id").unwrap().to_string();
    assert_eq!(id.len(), 20);
    assert_eq!(body(&resp), id);
    let other = get(&app, "/id", "")
        .get_header("x-request-id")
        .unwrap()
        .to_string();
    assert_ne!(id, other);

    let resp = get(&app, "/missing", "");
    let id = resp.get_header("x-request-id").unwrap();
    assert!(body(&resp).contains(&format!("\"request_id\":\"{id}\"")));

    // A client's id is ignored unless trusted, and checked when trusted.
    let resp = get(&app, "/id", "X-Request-Id: client-chosen\r\n");
    assert_ne!(resp.get_header("x-request-id"), Some("client-chosen"));
    let app = Router::new()
        .get("/id", echo_id)
        .layer(
            RequestId::new()
                .header("X-Correlation-Id")
                .trust_incoming(true),
        )
        .build(());
    let resp = get(&app, "/id", "X-Correlation-Id: abc-123\r\n");
    assert_eq!(resp.get_header("x-correlation-id"), Some("abc-123"));
    let resp = get(&app, "/id", "X-Correlation-Id: has spaces\r\n");
    assert_ne!(resp.get_header("x-correlation-id"), Some("has spaces"));
}

#[test]
fn the_logger_writes_one_json_line_per_request() {
    let (sink, lines) = memory();
    let app = Router::new()
        .get("/users/{id}", ok)
        .get("/fail", fail)
        .layer(RequestId::new())
        .layer(Logger::new(sink))
        .build(());
    let resp = get(&app, "/users/7?token=secret", "");
    let id = resp.get_header("x-request-id").unwrap().to_string();
    get(&app, "/fail", "");
    get(&app, "/nope", "");

    let lines = lines.borrow();
    assert_eq!(lines.len(), 3);
    let first = &lines[0];
    for part in [
        "\"level\":\"info\"",
        "\"event\":\"request\"",
        "\"method\":\"GET\"",
        "\"path\":\"/users/7\"",
        "\"route\":\"/users/{id}\"",
        "\"status\":200",
        "\"bytes\":2",
        &format!("\"request_id\":\"{id}\""),
        "\"peer\":null",
        "\"worker\":0",
        "\"duration_us\":",
    ] {
        assert!(first.contains(part), "{part} missing from {first}");
    }
    assert!(first.starts_with("{\"ts\":\"20"), "{first}");
    assert!(
        !first.contains("secret"),
        "query strings are left out by default"
    );
    assert!(!first.contains("\"error\""));

    let second = &lines[1];
    assert!(second.contains("\"level\":\"error\""), "{second}");
    assert!(second.contains("\"status\":500"), "{second}");
    assert!(
        second.contains("\"error\":{\"code\":\"GIONG-E1210\",\"detail\":"),
        "{second}"
    );
    assert!(
        second.contains("\"cause\":\"database exploded\""),
        "{second}"
    );

    let third = &lines[2];
    assert!(third.contains("\"route\":null"), "{third}");
    assert!(third.contains("\"code\":\"GIONG-E1203\""), "{third}");
}

#[test]
fn the_logger_can_include_the_query_string() {
    let (sink, lines) = memory();
    let app = Router::new()
        .get("/s", ok)
        .layer(Logger::new(sink).include_query(true))
        .build(());
    get(&app, "/s?q=rust", "");
    assert!(lines.borrow()[0].contains("\"query\":\"q=rust\""));
}

#[test]
fn a_slow_handler_gets_503_from_the_timeout() {
    async fn slow(_req: Req) -> Result<Response, Error> {
        giong::time::sleep(Duration::from_secs(30)).await;
        Ok(Response::text("late"))
    }
    let app = Router::new()
        .get("/slow", slow)
        .get("/fast", ok)
        .layer(Timeout::new(Duration::from_millis(50)))
        .build(());
    let started = std::time::Instant::now();
    let resp = get(&app, "/slow", "");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(resp.status(), 503);
    assert!(body(&resp).contains("\"code\":\"GIONG-E1209\""));
    assert_eq!(get(&app, "/fast", "").status(), 200);
}

fn cors_app(cors: Cors) -> App {
    Router::new()
        .get("/data", ok)
        .route("PUT", "/data", ok)
        .layer(cors)
        .build(())
}

fn preflight(app: &App, origin: &str, method: &str, headers: &str) -> Response {
    send(
        app,
        &format!(
            "OPTIONS /data HTTP/1.1\r\nHost: t\r\nOrigin: {origin}\r\n\
             Access-Control-Request-Method: {method}\r\n\
             Access-Control-Request-Headers: {headers}\r\n\r\n"
        ),
    )
}

#[test]
fn cors_answers_preflights_for_allowed_origins_only() {
    let app = cors_app(
        Cors::new()
            .allow_origin("https://App.example.com")
            .allow_methods(&["GET", "PUT"])
            .allow_headers(&["Content-Type", "authorization"])
            .allow_credentials(true)
            .max_age(Duration::from_secs(600)),
    );
    let resp = preflight(
        &app,
        "https://app.example.com",
        "PUT",
        "content-type, Accept",
    );
    assert_eq!(resp.status(), 204);
    assert_eq!(
        resp.get_header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        resp.get_header("access-control-allow-methods"),
        Some("GET, PUT")
    );
    assert_eq!(
        resp.get_header("access-control-allow-headers"),
        Some("content-type, authorization")
    );
    assert_eq!(
        resp.get_header("access-control-allow-credentials"),
        Some("true")
    );
    assert_eq!(resp.get_header("access-control-max-age"), Some("600"));
    let vary: Vec<_> = resp
        .headers()
        .filter(|(n, _)| n.eq_ignore_ascii_case("vary"))
        .map(|(_, v)| v)
        .collect();
    assert_eq!(
        vary,
        [
            "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers"
        ]
    );

    for (origin, method, headers, why) in [
        (
            "https://evil.example",
            "PUT",
            "",
            "the origin https://evil.example is not allowed",
        ),
        (
            "https://app.example.com",
            "DELETE",
            "",
            "the method DELETE is not allowed; allowed: GET, PUT",
        ),
        (
            "https://app.example.com",
            "PUT",
            "x-secret",
            "the header x-secret is not allowed",
        ),
    ] {
        let resp = preflight(&app, origin, method, headers);
        assert_eq!(resp.status(), 403, "{why}");
        let text = body(&resp);
        assert!(text.contains("\"code\":\"GIONG-E1212\""), "{text}");
        assert!(text.contains(why), "{text}");
        assert!(resp.get_header("access-control-allow-origin").is_none());
    }
}

#[test]
fn cors_marks_responses_for_allowed_origins() {
    let app = cors_app(
        Cors::new()
            .allow_origin("https://app.example.com")
            .expose_headers(&["X-Request-Id"]),
    );
    let resp = get(&app, "/data", "Origin: https://app.example.com\r\n");
    assert_eq!(body(&resp), "ok");
    assert_eq!(
        resp.get_header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        resp.get_header("access-control-expose-headers"),
        Some("x-request-id")
    );
    assert_eq!(resp.get_header("vary"), Some("Origin"));

    let resp = get(&app, "/data", "Origin: https://evil.example\r\n");
    assert_eq!(resp.status(), 200, "the browser, not the server, blocks it");
    assert!(resp.get_header("access-control-allow-origin").is_none());
    assert_eq!(resp.get_header("vary"), Some("Origin"));

    let resp = get(&app, "/data", "");
    assert_eq!(resp.get_header("vary"), Some("Origin"));
    assert!(resp.get_header("access-control-allow-origin").is_none());

    // Errors from further in get the headers too, so pages can read them.
    let resp = get(&app, "/missing", "Origin: https://app.example.com\r\n");
    assert_eq!(resp.status(), 404);
    assert_eq!(
        resp.get_header("access-control-allow-origin"),
        Some("https://app.example.com")
    );

    let app = cors_app(Cors::new().allow_any_origin());
    let resp = get(&app, "/data", "Origin: https://anyone.example\r\n");
    assert_eq!(resp.get_header("access-control-allow-origin"), Some("*"));
    assert!(resp.get_header("vary").is_none());
}

#[test]
fn cors_on_a_nested_router_answers_its_preflights_and_errors() {
    let api = Router::new()
        .get("/items", ok)
        .post("/items", ok)
        .delete("/items/{id}", ok)
        .layer(
            Cors::new()
                .allow_origin("https://app.example.com")
                .allow_methods(&["GET", "POST", "DELETE"])
                .allow_headers(&["content-type"]),
        );
    let app = Router::new()
        .get("/health", ok)
        .nest("/api", api)
        .layer(RequestId::new())
        .build(());
    let origin = "Origin: https://app.example.com\r\n";

    let resp = send(
        &app,
        &format!(
            "OPTIONS /api/items HTTP/1.1\r\nHost: t\r\n{origin}\
             Access-Control-Request-Method: POST\r\n\
             Access-Control-Request-Headers: content-type\r\n\r\n"
        ),
    );
    assert_eq!(resp.status(), 204);
    assert_eq!(
        resp.get_header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        resp.get_header("access-control-allow-methods"),
        Some("GET, POST, DELETE")
    );
    assert_eq!(
        resp.get_header("access-control-allow-headers"),
        Some("content-type")
    );
    assert!(resp.get_header("x-request-id").is_some());

    // A preflight for a path whose route has a parameter.
    let resp = send(
        &app,
        &format!(
            "OPTIONS /api/items/7 HTTP/1.1\r\nHost: t\r\n{origin}\
             Access-Control-Request-Method: DELETE\r\n\r\n"
        ),
    );
    assert_eq!(resp.status(), 204);
    assert!(resp.get_header("access-control-allow-methods").is_some());

    // Errors under the prefix carry the header too, so the page can read them.
    for (method, target, status) in [("GET", "/api/nothing", 404), ("PUT", "/api/items", 405)] {
        let resp = send(
            &app,
            &format!("{method} {target} HTTP/1.1\r\nHost: t\r\n{origin}\r\n"),
        );
        assert_eq!(resp.status(), status, "{method} {target}");
        assert_eq!(
            resp.get_header("access-control-allow-origin"),
            Some("https://app.example.com"),
            "{method} {target}"
        );
    }

    // Outside the prefix, the nested Cors does not run.
    let resp = get(&app, "/health", origin);
    assert!(resp.get_header("access-control-allow-origin").is_none());
    let resp = send(
        &app,
        &format!(
            "OPTIONS /health HTTP/1.1\r\nHost: t\r\n{origin}\
             Access-Control-Request-Method: GET\r\n\r\n"
        ),
    );
    assert_eq!(resp.status(), 204);
    assert!(resp.get_header("access-control-allow-methods").is_none());
}

fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let err = std::panic::catch_unwind(f).unwrap_err();
    err.downcast_ref::<String>().cloned().unwrap_or_default()
}

#[test]
fn settings_that_cannot_work_panic_with_a_code() {
    type Case = (Box<dyn FnOnce() + std::panic::UnwindSafe>, &'static str);
    let cases: Vec<Case> = vec![
        (
            Box::new(|| {
                let _ = Cors::new().allow_any_origin().allow_credentials(true);
            }),
            "allow_any_origin together with allow_credentials",
        ),
        (
            Box::new(|| {
                let _ = Cors::new().allow_origin("https://a.example/");
            }),
            "no path, query or trailing slash",
        ),
        (
            Box::new(|| {
                let _ = Cors::new().allow_origin("null");
            }),
            "`null`",
        ),
        (
            Box::new(|| {
                let _ = Compression::new().level(0);
            }),
            "the level is 1 to 9",
        ),
        (
            Box::new(|| {
                let _ = Timeout::new(Duration::ZERO);
            }),
            "the limit is zero",
        ),
        (
            Box::new(|| {
                let _ = RequestId::new().header("bad header");
            }),
            "not a header name",
        ),
    ];
    for (f, why) in cases {
        let msg = panic_message(f);
        assert!(msg.starts_with("GIONG-E1213: "), "{msg}");
        assert!(msg.contains(why), "{msg}");
    }
}

fn big_json() -> String {
    let items: Vec<String> = (0..200)
        .map(|i| format!("{{\"id\":{i},\"name\":\"user {i}\",\"email\":\"user{i}@example.com\"}}"))
        .collect();
    format!("[{}]", items.join(","))
}

fn gunzip(gz: &[u8]) -> Vec<u8> {
    assert_eq!(&gz[..2], &[0x1f, 0x8b]);
    miniz_oxide::inflate::decompress_to_vec(&gz[10..gz.len() - 8]).unwrap()
}

#[test]
fn compression_gzips_text_for_clients_that_accept_it() {
    let json = big_json();
    let page = json.clone();
    let app = Router::new()
        .get("/users", move |_req: Req| {
            let page = page.clone();
            async move { Ok(Response::json(page).header("ETag", "\"v1\"")) }
        })
        .get("/small", ok)
        .get("/png", |_req: Req| async {
            Ok(Response::new(200)
                .header("Content-Type", "image/png")
                .body(vec![7u8; 5000]))
        })
        .get("/done", |_req: Req| async {
            Ok(Response::json(vec![b' '; 5000]).header("Content-Encoding", "br"))
        })
        .get("/no-transform", |_req: Req| async {
            Ok(Response::json(vec![b' '; 5000]).header("Cache-Control", "public, no-transform"))
        })
        .layer(Compression::new())
        .build(());

    let resp = get(&app, "/users", "Accept-Encoding: br, gzip\r\n");
    assert_eq!(resp.get_header("content-encoding"), Some("gzip"));
    assert_eq!(resp.get_header("vary"), Some("Accept-Encoding"));
    assert_eq!(resp.get_header("etag"), Some("W/\"v1\""));
    assert!(
        resp.body_bytes().len() * 4 < json.len(),
        "{} of {}",
        resp.body_bytes().len(),
        json.len()
    );
    assert_eq!(gunzip(resp.body_bytes()), json.as_bytes());

    let resp = get(&app, "/users", "");
    assert_eq!(resp.get_header("content-encoding"), None);
    assert_eq!(
        resp.get_header("vary"),
        Some("Accept-Encoding"),
        "caches must still vary"
    );
    assert_eq!(resp.get_header("etag"), Some("\"v1\""));
    assert_eq!(body(&resp), json);

    let resp = send(
        &app,
        "HEAD /users HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\n\r\n",
    );
    assert_eq!(resp.get_header("content-encoding"), None);

    for path in ["/small", "/png", "/done", "/no-transform"] {
        let resp = get(&app, path, "Accept-Encoding: gzip\r\n");
        assert_ne!(resp.get_header("content-encoding"), Some("gzip"), "{path}");
        assert_eq!(resp.get_header("vary"), None, "{path}");
    }
}
