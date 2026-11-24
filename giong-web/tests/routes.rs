//! `App::routes` and `RouteList::live`: what a built app tells tools about
//! its routes.

use giong_web::middleware::{Logger, RequestId};
use giong_web::{App, Error, Req, Response, RouteList, Router, Sink};

async fn ok(_req: Req) -> Result<Response, Error> {
    Ok(Response::text("ok"))
}

fn rows(app: &App) -> Vec<(String, String)> {
    app.routes()
        .iter()
        .map(|r| (r.method().to_string(), r.pattern().to_string()))
        .collect()
}

fn pair(m: &str, p: &str) -> (String, String) {
    (m.to_string(), p.to_string())
}

#[test]
fn routes_are_listed_in_the_order_they_were_added_with_full_patterns() {
    let users = Router::new()
        .get("/{id}", ok)
        .delete("/{id}", ok)
        .layer(Logger::new(Sink::Stderr));
    let files = Router::new().put("/{*path}", ok).stream_bodies(1 << 20);
    let api = Router::new()
        .nest("/users", users)
        .nest("/files", files)
        .get("/", ok);
    let app = Router::new()
        .get("/health", ok)
        .route("PROPFIND", "/dav/{*path}", ok)
        .nest("/teams/{team}/api", api)
        .layer(RequestId::new())
        .build(());
    assert_eq!(
        rows(&app),
        vec![
            pair("GET", "/health"),
            pair("PROPFIND", "/dav/{*path}"),
            pair("GET", "/teams/{team}/api/users/{id}"),
            pair("DELETE", "/teams/{team}/api/users/{id}"),
            pair("PUT", "/teams/{team}/api/files/{*path}"),
            pair("GET", "/teams/{team}/api"),
        ]
    );
    let routes: Vec<_> = app.routes().iter().collect();
    assert_eq!(app.routes().len(), 6);
    assert!(!app.routes().is_empty());

    // Parameters, catch-alls, stream limits and middleware.
    assert!(routes[0].params().is_empty());
    assert!(!routes[0].catch_all());
    assert_eq!(routes[0].middleware(), 1);
    assert_eq!(routes[0].stream_limit(), None);

    let names: Vec<&str> = routes[1].params().iter().map(|s| &**s).collect();
    assert_eq!(names, ["path"]);
    assert!(routes[1].catch_all());

    let names: Vec<&str> = routes[2].params().iter().map(|s| &**s).collect();
    assert_eq!(names, ["team", "id"]);
    // RequestId from the top, Logger from `users`.
    assert_eq!(routes[2].middleware(), 2);
    assert_eq!(routes[4].stream_limit(), Some(1 << 20));
    assert!(routes[4].catch_all());
    assert_eq!(routes[5].middleware(), 1);

    // Where each was added: this file.
    for r in &routes {
        assert!(r.added_at().file().ends_with("routes.rs"), "{r:?}");
    }
}

#[test]
fn an_outer_stream_limit_reaches_routes_without_their_own() {
    let inner = Router::new().put("/a", ok).stream_bodies(10);
    let app = Router::new()
        .nest("/in", inner)
        .put("/b", ok)
        .stream_bodies(99)
        .build(());
    let limits: Vec<_> = app.routes().iter().map(|r| r.stream_limit()).collect();
    assert_eq!(limits, [Some(10), Some(99)]);
}

#[test]
fn the_json_form_lists_every_field() {
    let app = Router::new()
        .get("/users/{id}", ok)
        .put("/files/{*path}", ok)
        .stream_bodies(5)
        .build(());
    let json: serde_json::Value = serde_json::from_str(&app.routes().to_json()).unwrap();
    let list = json["routes"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["method"], "GET");
    assert_eq!(list[0]["pattern"], "/users/{id}");
    assert_eq!(list[0]["params"], serde_json::json!(["id"]));
    assert_eq!(list[0]["catch_all"], false);
    assert_eq!(list[0]["stream_limit"], 5);
    assert_eq!(list[0]["middleware"], 0);
    let at = list[0]["added_at"].as_str().unwrap();
    assert!(at.contains("routes.rs:"), "{at}");
    assert_eq!(list[1]["catch_all"], true);
    assert_eq!(list[1]["params"], serde_json::json!(["path"]));
}

#[test]
fn an_app_without_routes_lists_none() {
    let app = Router::<()>::new().build(());
    assert!(app.routes().is_empty());
    assert_eq!(app.routes().to_json(), r#"{"routes":[]}"#);
}

#[test]
fn live_lists_the_apps_alive_on_this_thread_only() {
    // Each test runs on its own thread, so nothing else is listed.
    assert!(RouteList::live().is_empty());
    let a = Router::new().get("/a", ok).build(());
    let b = Router::new().get("/b", ok).post("/b", ok).build(());
    let live = RouteList::live();
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].iter().next().unwrap().pattern(), "/a");
    assert_eq!(live[1].len(), 2);

    // Another thread sees its own apps, not these.
    std::thread::spawn(|| {
        assert!(RouteList::live().is_empty());
        let _c = Router::new().get("/c", ok).build(());
        assert_eq!(RouteList::live().len(), 1);
    })
    .join()
    .unwrap();

    // A list still held does not keep a dropped app listed.
    drop(a);
    drop(live);
    let live = RouteList::live();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].iter().next().unwrap().pattern(), "/b");
    drop(b);
    assert!(RouteList::live().is_empty());
}

#[test]
fn a_list_held_by_a_tool_outlives_its_app_but_is_no_longer_live() {
    let app = Router::new().get("/a", ok).build(());
    let held = RouteList::live().pop().unwrap();
    drop(app);
    // The tool's copy still reads, but the app is no longer listed.
    assert_eq!(held.iter().next().unwrap().pattern(), "/a");
    assert!(RouteList::live().is_empty());
}

#[test]
fn building_many_apps_does_not_grow_the_registry() {
    for _ in 0..10_000 {
        let app = Router::new().get("/x", ok).build(());
        drop(app);
    }
    let kept = Router::new().get("/kept", ok).build(());
    assert_eq!(RouteList::live().len(), 1);
    drop(kept);
}

#[test]
fn listing_is_the_same_on_every_worker() {
    // A service builds its router on each worker: each worker lists its own.
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    rt.run(move |w| {
        let s = s.clone();
        async move {
            let app = Router::new().get("/w", ok).build(());
            let live = RouteList::live();
            s.lock()
                .unwrap()
                .push((w, live.len(), app.routes().to_json()));
        }
    })
    .unwrap();
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, 1);
    assert_eq!(seen[1].1, 1);
    assert_eq!(seen[0].2, seen[1].2);
}
