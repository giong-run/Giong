//! JSON bodies, JSON responses and typed query strings (feature `serde`).

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
    name: &'a str,
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
            name: new.name,
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
