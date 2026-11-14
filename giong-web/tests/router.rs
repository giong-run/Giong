//! Routing, errors and middleware, through `App::handle` without a socket.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use giong_web::{App, Error, Middleware, Next, Req, Request, Response, Router};

fn request(raw: &str) -> Request {
    Request::from_bytes(raw.as_bytes()).unwrap_or_else(|s| panic!("{s}: {raw:?}"))
}

fn call<S: 'static>(app: &App<S>, method: &str, target: &str) -> Response {
    let raw = format!("{method} {target} HTTP/1.1\r\nHost: test\r\n\r\n");
    giong::block_on(app.handle(request(&raw)))
}

fn body(resp: &Response) -> String {
    String::from_utf8(resp.body_bytes().to_vec()).unwrap()
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

#[test]
fn routes_match_by_text_then_parameter_then_catch_all() {
    let app = Router::new()
        .get("/users/{id}", user)
        .get("/users/me", me)
        .get("/files/{*path}", file)
        .build(());
    assert_eq!(body(&call(&app, "GET", "/users/me")), "me");
    assert_eq!(body(&call(&app, "GET", "/users/42")), "user 42");
    assert_eq!(body(&call(&app, "GET", "/files/a/b%20c.txt")), "a/b c.txt");
    let resp = call(&app, "HEAD", "/users/42");
    assert_eq!(resp.status(), 200, "HEAD falls back to GET");
}

#[test]
fn a_path_without_a_route_is_404_with_a_code_and_a_hint() {
    let app = Router::new().get("/users", me).build(());
    let resp = call(&app, "GET", "/nothing");
    assert_eq!(resp.status(), 404);
    assert_eq!(
        resp.get_header("content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        body(&resp),
        r#"{"status":404,"code":"GIONG-E1203","title":"Not Found","detail":"no route for GET /nothing"}"#
    );
    let resp = call(&app, "GET", "/users/");
    assert!(
        body(&resp).contains("did you mean /users?"),
        "{}",
        body(&resp)
    );
}

#[test]
fn a_known_path_with_another_method_is_405_with_allow() {
    let app = Router::new()
        .get("/items", me)
        .delete("/items", me)
        .build(());
    let resp = call(&app, "PUT", "/items");
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.get_header("allow"), Some("GET, DELETE, HEAD, OPTIONS"));
    assert!(body(&resp).contains("\"code\":\"GIONG-E1204\""));

    let resp = call(&app, "OPTIONS", "/items");
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.get_header("allow"), Some("GET, DELETE, HEAD, OPTIONS"));
}

#[test]
fn allow_lists_every_method_that_routes_to_the_url() {
    async fn deleted(req: Req) -> Result<Response, Error> {
        let id: String = req.param("id")?;
        Ok(Response::text(format!("deleted {id}")))
    }
    // The method picks among the paths that match: DELETE /users/me goes to
    // /users/{id}, so `Allow` for /users/me names it.
    let app = Router::new()
        .get("/users/me", me)
        .delete("/users/{id}", deleted)
        .route("PUT", "/{*any}", me)
        .build(());
    assert_eq!(body(&call(&app, "DELETE", "/users/me")), "deleted me");
    let resp = call(&app, "OPTIONS", "/users/me");
    assert_eq!(resp.status(), 204);
    assert_eq!(
        resp.get_header("allow"),
        Some("GET, DELETE, PUT, HEAD, OPTIONS")
    );
    let resp = call(&app, "POST", "/users/me");
    assert_eq!(resp.status(), 405);
    assert_eq!(
        resp.get_header("allow"),
        Some("GET, DELETE, PUT, HEAD, OPTIONS")
    );
    assert!(
        body(&resp).contains("allowed: GET, DELETE, PUT, HEAD, OPTIONS"),
        "{}",
        body(&resp)
    );
    // Only the paths that match count: /users/7 has no GET.
    let resp = call(&app, "OPTIONS", "/users/7");
    assert_eq!(resp.get_header("allow"), Some("DELETE, PUT, OPTIONS"));
}

#[test]
fn path_parameters_refuse_dot_segments_nul_and_slashes() {
    async fn raw(req: Req) -> Result<Response, Error> {
        Ok(Response::text(req.param_raw("name")?.to_string()))
    }
    let app = Router::new()
        .get("/files/{*path}", file)
        .get("/one/{name}", |req: Req| async move {
            let name: String = req.param("name")?;
            Ok(Response::text(name))
        })
        .get("/raw/{name}", raw)
        .build(());
    for (target, why) in [
        ("/files/../../etc/passwd", "`..`"),
        ("/files/..%2F..%2Fetc%2Fpasswd", "`..`"),
        ("/files/a/%2E%2E/%2E%2E/etc/passwd", "`..`"),
        ("/files/a/./b", "`.`"),
        ("/files/x%00y", "NUL"),
        ("/files//etc/passwd", "starts with `/`"),
        ("/files/%2Fetc%2Fpasswd", "starts with `/`"),
        ("/one/..%2F..%2Fsecret", "`/`"),
        ("/one/a%2Fb", "`/`"),
        ("/one/..", "`..`"),
        ("/one/%2e", "`.`"),
    ] {
        let resp = call(&app, "GET", target);
        let text = body(&resp);
        assert_eq!(resp.status(), 400, "{target}: {text}");
        assert!(
            text.contains("\"code\":\"GIONG-E1205\""),
            "{target}: {text}"
        );
        assert!(text.contains(why), "{target}: {text}");
    }
    for (target, value) in [
        ("/files/a/b%20c.txt", "a/b c.txt"),
        ("/files/.hidden/a..b", ".hidden/a..b"),
        ("/files/dir/", "dir/"),
        ("/one/v1.2", "v1.2"),
        ("/one/...", "..."),
    ] {
        let resp = call(&app, "GET", target);
        assert_eq!(
            (resp.status(), body(&resp).as_str()),
            (200, value),
            "{target}"
        );
    }
    // The value as sent, for the route that wants it.
    assert_eq!(body(&call(&app, "GET", "/raw/..%2Fx")), "..%2Fx");
}

#[test]
fn bad_parameters_are_400_and_name_the_parameter() {
    let app = Router::new().get("/users/{id}", user).build(());
    let resp = call(&app, "GET", "/users/abc");
    assert_eq!(resp.status(), 400);
    let text = body(&resp);
    assert!(text.contains("\"code\":\"GIONG-E1205\""), "{text}");
    assert!(
        text.contains("path parameter `id`: cannot read \\\"abc\\\""),
        "{text}"
    );
    assert!(text.contains("expected an unsigned integer"), "{text}");
}

#[test]
fn asking_for_a_parameter_the_route_lacks_is_a_500_bug() {
    async fn typo(req: Req) -> Result<Response, Error> {
        let _: u64 = req.param("idd")?;
        Ok(Response::new(204))
    }
    let app = Router::new().get("/users/{id}", typo).build(());
    let resp = call(&app, "GET", "/users/1");
    assert_eq!(resp.status(), 500);
    let err = Error::of(&resp).unwrap();
    assert_eq!(err.code(), "GIONG-E1206");
    let cause = err.source().unwrap().to_string();
    assert!(
        cause.contains("/users/{id} has no parameter `idd`"),
        "{cause}"
    );
    assert!(!body(&resp).contains("idd"), "the cause is not sent");
}

#[test]
fn query_parameters_are_decoded_and_parsed() {
    async fn search(req: Req) -> Result<Response, Error> {
        let q: Option<String> = req.query_param("q")?;
        let page = req.query_param::<u32>("page")?.unwrap_or(1);
        Ok(Response::text(format!("{q:?} {page}")))
    }
    let app = Router::new().get("/search", search).build(());
    assert_eq!(
        body(&call(&app, "GET", "/search?q=caf%C3%A9+au+lait&page=3")),
        "Some(\"café au lait\") 3"
    );
    assert_eq!(body(&call(&app, "GET", "/search")), "None 1");
    let resp = call(&app, "GET", "/search?page=-1");
    assert_eq!(resp.status(), 400);
    assert!(body(&resp).contains("query parameter `page`"));
}

#[test]
fn errors_from_anywhere_convert_with_question_mark() {
    async fn broken(_req: Req) -> Result<Response, Error> {
        Err(io::Error::other("disk on fire"))?
    }
    let app = Router::new().get("/broken", broken).build(());
    let resp = call(&app, "GET", "/broken");
    assert_eq!(resp.status(), 500);
    let text = body(&resp);
    assert!(text.contains("\"code\":\"GIONG-E1210\""), "{text}");
    assert!(!text.contains("disk"), "{text}");
    assert_eq!(
        Error::of(&resp).unwrap().source().unwrap().to_string(),
        "disk on fire"
    );
}

#[test]
fn state_and_nested_routers() {
    struct State {
        name: &'static str,
    }
    async fn team(req: Req<State>) -> Result<Response, Error> {
        let team: String = req.param("team")?;
        let id: u32 = req.param("id")?;
        Ok(Response::text(format!(
            "{} {team} {id} {}",
            req.state().name,
            req.route().unwrap()
        )))
    }
    async fn root(req: Req<State>) -> Result<Response, Error> {
        Ok(Response::text(req.route().unwrap().to_string()))
    }
    let members = Router::new().get("/members/{id}", team).get("/", root);
    let app = Router::new()
        .nest("/teams/{team}", members)
        .build(State { name: "acme" });
    assert_eq!(
        body(&call(&app, "GET", "/teams/red/members/7")),
        "acme red 7 /teams/{team}/members/{id}"
    );
    assert_eq!(body(&call(&app, "GET", "/teams/red")), "/teams/{team}");
}

/// Records the order middleware run in.
struct Mark(&'static str, Rc<RefCell<Vec<String>>>);

impl<S: 'static> Middleware<S> for Mark {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        self.1.borrow_mut().push(format!("{} in", self.0));
        let resp = next.run(req).await;
        self.1.borrow_mut().push(format!("{} out", self.0));
        Ok(resp.header("X-Mark", self.0))
    }
}

#[test]
fn middleware_run_in_the_order_added_and_nested_ones_only_for_their_routes() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let api = Router::new().get("/x", me).layer(Mark("api", log.clone()));
    let app = Router::new()
        .get("/top", me)
        .nest("/api", api)
        .layer(Mark("outer", log.clone()))
        .layer(Mark("inner", log.clone()))
        .build(());

    call(&app, "GET", "/api/x");
    assert_eq!(
        *log.borrow(),
        [
            "outer in",
            "inner in",
            "api in",
            "api out",
            "inner out",
            "outer out"
        ]
    );
    log.borrow_mut().clear();
    call(&app, "GET", "/top");
    assert_eq!(
        *log.borrow(),
        ["outer in", "inner in", "inner out", "outer out"]
    );
    log.borrow_mut().clear();
    let resp = call(&app, "GET", "/missing");
    assert_eq!(resp.status(), 404);
    assert_eq!(
        *log.borrow(),
        ["outer in", "inner in", "inner out", "outer out"],
        "the outermost router's middleware see 404s too"
    );
    let marks: Vec<_> = resp
        .headers()
        .filter(|(n, _)| *n == "X-Mark")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(marks, ["inner", "outer"]);
}

#[test]
fn nested_middleware_see_the_answers_the_router_gives_for_its_paths() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let users = Router::new()
        .get("/{id}", user)
        .layer(Mark("users", log.clone()));
    let api = Router::new()
        .get("/x", me)
        .nest("/teams/{team}/users", users)
        .layer(Mark("api", log.clone()));
    let app = Router::new()
        .get("/top", me)
        .nest("/api", api)
        .layer(Mark("outer", log.clone()))
        .build(());
    let seen = |method: &str, target: &str| {
        log.borrow_mut().clear();
        let status = call(&app, method, target).status();
        let ins: Vec<String> = log
            .borrow()
            .iter()
            .filter_map(|l| l.strip_suffix(" in").map(str::to_string))
            .collect();
        (status, ins)
    };
    // The automatic OPTIONS and 405 of a nested route's path.
    assert_eq!(
        seen("OPTIONS", "/api/x"),
        (204, vec!["outer".into(), "api".into()])
    );
    assert_eq!(
        seen("POST", "/api/x"),
        (405, vec!["outer".into(), "api".into()])
    );
    // A 404 under the prefix a router was nested at, the most specific one.
    assert_eq!(
        seen("GET", "/api/nothing"),
        (404, vec!["outer".into(), "api".into()])
    );
    assert_eq!(
        seen("GET", "/api"),
        (404, vec!["outer".into(), "api".into()])
    );
    assert_eq!(
        seen("GET", "/api/teams/red/users/7/more"),
        (404, vec!["outer".into(), "api".into(), "users".into()])
    );
    // Elsewhere, only the outermost router's.
    assert_eq!(seen("GET", "/nothing"), (404, vec!["outer".into()]));
    assert_eq!(seen("GET", "/apix"), (404, vec!["outer".into()]));
    assert_eq!(seen("POST", "/top"), (405, vec!["outer".into()]));
}

#[test]
fn a_middleware_can_answer_by_itself_and_pass_values_in() {
    struct User(&'static str);
    struct Auth;
    impl<S: 'static> Middleware<S> for Auth {
        async fn call(&self, mut req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
            match req.header_str("authorization") {
                Some("Bearer good") => {
                    req.set_extension(User("ada"));
                    Ok(next.run(req).await)
                }
                _ => Err(Error::unauthorized("no_token", "send a bearer token")
                    .with_header("WWW-Authenticate", "Bearer")),
            }
        }
    }
    async fn whoami(req: Req) -> Result<Response, Error> {
        let user = req.extension::<User>().map_or("?", |u| u.0);
        Ok(Response::text(user))
    }
    let app = Router::new().get("/me", whoami).layer(Auth).build(());
    let resp = call(&app, "GET", "/me");
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.get_header("www-authenticate"), Some("Bearer"));
    let raw = "GET /me HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer good\r\n\r\n";
    let resp = giong::block_on(app.handle(request(raw)));
    assert_eq!(body(&resp), "ada");
}

#[test]
fn closures_capture_what_they_need() {
    let greeting = Rc::new(String::from("hi"));
    let app = Router::new()
        .get("/", move |_req: Req| {
            let greeting = greeting.clone();
            async move { Ok(Response::text(greeting.to_string())) }
        })
        .build(());
    assert_eq!(body(&call(&app, "GET", "/")), "hi");
}

fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let err = std::panic::catch_unwind(f).unwrap_err();
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap()
}

#[test]
fn mistakes_in_the_routes_panic_with_a_code_and_where() {
    let msg = panic_message(|| {
        let _ = Router::<()>::new().get("users/{id}", user);
    });
    assert!(
        msg.starts_with(
            "GIONG-E1201: invalid route pattern \"users/{id}\": a pattern starts with `/`"
        ),
        "{msg}"
    );
    assert!(msg.contains("docs/errors/E1201.md"), "{msg}");

    let msg = panic_message(|| {
        let _ = Router::<()>::new()
            .get("/users/{id}", user)
            .get("/users/{name}", user);
    });
    assert!(msg.starts_with("GIONG-E1202: GET /users/{name} would answer the same requests as GET /users/{id}, added at "), "{msg}");
    assert!(msg.contains("tests/router.rs:"), "{msg}");

    let msg = panic_message(|| {
        let api = Router::<()>::new().get("/users", me);
        let _ = Router::new().get("/api/users", me).nest("/api", api);
    });
    assert!(msg.starts_with("GIONG-E1202"), "{msg}");

    let msg = panic_message(|| {
        let _ = Router::<()>::new().nest("/api/", Router::new());
    });
    assert!(msg.contains("a prefix cannot end with `/`"), "{msg}");
}
