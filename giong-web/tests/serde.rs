//! JSON bodies, JSON responses and typed query strings (feature `serde`).

use std::borrow::Cow;

use giong_web::{json, App, Error, Req, Request, Response, Router};
use serde::{Deserialize, Serialize};

fn send(app: &App, raw: &str) -> Response {
    let req = Request::from_bytes(raw.as_bytes()).unwrap_or_else(|s| panic!("{s}: {raw:?}"));
    giong::block_on(app.handle(req))
}

fn post(app: &App, content_type: Option<&str>, body: &str) -> Response {
    let ct = content_type.map_or(String::new(), |t| format!("Content-Type: {t}\r\n"));
    send(
        app,
        &format!(
            "POST /users HTTP/1.1\r\nHost: t\r\n{ct}Content-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn text(resp: &Response) -> String {
    String::from_utf8(resp.body_bytes().to_vec()).unwrap()
}

#[derive(Deserialize)]
struct NewUser<'a> {
    // Borrowed from the body when the string has no escape, owned when it
    // has one: a `&str` field would fail on `"Ad\"a"`.
    #[serde(borrow)]
    name: Cow<'a, str>,
    age: u8,
}

#[derive(Serialize)]
struct User<'a> {
    id: u64,
    name: &'a str,
    age: u8,
}

async fn create(req: Req) -> Result<Response, Error> {
    let new: NewUser = req.json()?;
    if new.name.is_empty() {
        return Err(Error::unprocessable("empty_name", "name must not be empty"));
    }
    json(
        201,
        &User {
            id: 7,
            name: &new.name,
            age: new.age,
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Sort {
    Name,
    Age,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    page: Option<u32>,
    sort: Option<Sort>,
}

async fn list(req: Req) -> Result<Response, Error> {
    let q: ListQuery = req.query()?;
    let sort = match q.sort {
        Some(Sort::Name) => "name",
        Some(Sort::Age) => "age",
        None => "id",
    };
    json(
        200,
        &serde_json::json!({ "page": q.page.unwrap_or(1), "sort": sort }),
    )
}

fn app() -> App {
    Router::new()
        .post("/users", create)
        .get("/users", list)
        .build(())
}

#[test]
fn a_json_body_in_a_json_response_out() {
    let app = app();
    let resp = post(
        &app,
        Some("application/json; charset=utf-8"),
        r#"{"name":"Ada","age":36}"#,
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.get_header("content-type"), Some("application/json"));
    assert_eq!(text(&resp), r#"{"id":7,"name":"Ada","age":36}"#);

    // Escapes need an owned string, which `Cow` turns into.
    let resp = post(
        &app,
        Some("application/json"),
        r#"{"name":"Ad\"a \u00e9","age":36}"#,
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(text(&resp), r#"{"id":7,"name":"Ad\"a é","age":36}"#);
}

#[test]
fn bodies_that_do_not_fit_say_why() {
    let app = app();

    let resp = post(&app, None, r#"{"name":"Ada","age":36}"#);
    assert_eq!(resp.status(), 415);
    let body = text(&resp);
    assert!(body.contains("\"code\":\"GIONG-E1208\""), "{body}");
    assert!(body.contains("the request has no Content-Type"), "{body}");

    let resp = post(&app, Some("text/plain"), "hi");
    assert_eq!(resp.status(), 415);
    assert!(text(&resp).contains("got `text/plain`"));

    let resp = post(&app, Some("application/json"), r#"{"name":"Ada","#);
    assert_eq!(resp.status(), 400);
    let body = text(&resp);
    assert!(body.contains("\"code\":\"GIONG-E1207\""), "{body}");
    assert!(
        body.contains("EOF while parsing a value at line 1 column 14"),
        "{body}"
    );

    let resp = post(
        &app,
        Some("application/json"),
        r#"{"name":"Ada","age":"old"}"#,
    );
    assert_eq!(resp.status(), 422);
    let body = text(&resp);
    assert!(
        body.contains("invalid type: string \\\"old\\\", expected u8"),
        "{body}"
    );

    let resp = post(&app, Some("application/json"), r#"{"name":"Ada"}"#);
    assert_eq!(resp.status(), 422);
    assert!(text(&resp).contains("missing field `age`"));

    let resp = post(&app, Some("application/json"), r#"{"name":"","age":1}"#);
    assert_eq!(resp.status(), 422);
    assert!(text(&resp).contains("\"code\":\"empty_name\""));
}

#[test]
fn query_strings_become_structs() {
    let app = app();
    let get = |target: &str| send(&app, &format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"));

    assert_eq!(text(&get("/users")), r#"{"page":1,"sort":"id"}"#);
    assert_eq!(
        text(&get("/users?page=3&sort=age")),
        r#"{"page":3,"sort":"age"}"#
    );

    for (target, why) in [
        (
            "/users?page=x",
            "parameter `page`: cannot read \\\"x\\\" as an unsigned integer",
        ),
        ("/users?sort=size", "unknown variant `size`"),
        ("/users?limit=5", "unknown field `limit`"),
        ("/users?page=1&page=2", "duplicate field `page`"),
    ] {
        let resp = get(target);
        assert_eq!(resp.status(), 400, "{target}");
        let body = text(&resp);
        assert!(body.contains("\"code\":\"GIONG-E1205\""), "{body}");
        assert!(body.contains(why), "{target}: {body}");
    }
}

/// A body or query string that serde cannot lend to a `&str` field is the
/// program's mistake, not the client's: a `500` whose cause says what to
/// use instead.
#[test]
fn a_str_field_that_cannot_borrow_is_a_500_that_says_what_to_use() {
    #[derive(Deserialize)]
    struct Borrowed<'a> {
        name: &'a str,
    }
    #[derive(Deserialize)]
    struct Tagged<'a> {
        tag: Option<&'a str>,
    }
    async fn body(req: Req) -> Result<Response, Error> {
        let b: Borrowed = req.json()?;
        Ok(Response::text(b.name.to_string()))
    }
    async fn query(req: Req) -> Result<Response, Error> {
        let t: Tagged = req.query()?;
        Ok(Response::text(t.tag.unwrap_or("").to_string()))
    }
    let app = Router::new()
        .post("/users", body)
        .get("/tags", query)
        .build(());
    let get = |target: &str| send(&app, &format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"));

    // Plain values borrow, so the mistake hides in tests with plain values.
    let resp = post(&app, Some("application/json"), r#"{"name":"Ada"}"#);
    assert_eq!(text(&resp), "Ada");
    assert_eq!(text(&get("/tags?tag=rust")), "rust");

    for resp in [
        post(&app, Some("application/json"), r#"{"name":"Ad\"a"}"#),
        post(&app, Some("application/json"), r#"{"name":"caf\u00e9"}"#),
        get("/tags?tag=rust+lang"),
        get("/tags?tag=caf%C3%A9"),
    ] {
        assert_eq!(resp.status(), 500);
        let err = Error::of(&resp).unwrap();
        assert_eq!(err.code(), "GIONG-E1210");
        let cause = err.source().unwrap().to_string();
        assert!(cause.contains("GIONG-E1213"), "{cause}");
        assert!(cause.contains("Cow<'a, str>"), "{cause}");
        assert!(cause.contains("#[serde(borrow)]"), "{cause}");
        assert!(!text(&resp).contains("borrowed"), "{}", text(&resp));
    }
}

#[test]
fn both_query_apis_refuse_a_repeated_key_and_a_bad_escape() {
    use std::collections::HashMap;
    async fn one(req: Req) -> Result<Response, Error> {
        let page: Option<u32> = req.query_param("page")?;
        Ok(Response::text(format!("{page:?}")))
    }
    async fn map(req: Req) -> Result<Response, Error> {
        let all: HashMap<String, String> = req.query()?;
        Ok(Response::text(all.len().to_string()))
    }
    let app = Router::new().get("/one", one).get("/map", map).build(());
    let get = |target: &str| send(&app, &format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"));

    // Only a key that is read must be there once.
    assert_eq!(text(&get("/one?page=2&q=a&q=b")), "Some(2)");
    assert_eq!(text(&get("/map?a=1&b=2")), "2");
    for (target, why) in [
        ("/one?page=1&page=2", "`page` appears more than once"),
        ("/map?a=1&a=2", "`a` appears more than once"),
        (
            "/one?%zz=1&page=2",
            "not followed by two hexadecimal digits",
        ),
        ("/one?page=2&q=%ff", "not UTF-8"),
        ("/map?a=%zz", "not followed by two hexadecimal digits"),
    ] {
        let resp = get(target);
        let body = text(&resp);
        assert_eq!(resp.status(), 400, "{target}: {body}");
        assert!(body.contains("\"code\":\"GIONG-E1205\""), "{body}");
        assert!(body.contains(why), "{target}: {body}");
    }
}
