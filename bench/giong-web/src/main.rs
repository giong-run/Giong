//! The benchmark routes on giong-web: the same handlers as bench/axum, with
//! routing, path and query parameters and serde JSON.
//!
//! MIDDLEWARE=1 adds request ids, gzip and a timeout, as bench/axum does
//! with tower-http's equivalents.
use std::hint::black_box;
use std::time::Duration;

use giong::net::TcpListener;
use giong_web::middleware::{Compression, RequestId, Timeout};
use giong_web::{json, Error, Req, Response, Router, Server};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Msg {
    message: &'static str,
}

#[derive(Serialize)]
struct User {
    id: u32,
    name: String,
    email: String,
    active: bool,
    score: f64,
}

#[derive(Deserialize)]
struct N {
    n: Option<u32>,
}

fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

fn make_user(i: u32) -> User {
    User {
        id: i,
        name: format!("User {i}"),
        email: format!("user{i}@example.com"),
        active: i % 2 == 0,
        score: i as f64 * 1.5,
    }
}

async fn hello(_req: Req) -> Result<Response, Error> {
    json(200, &Msg { message: "Hello, World!" })
}

async fn users(_req: Req) -> Result<Response, Error> {
    let list: Vec<User> = (0..100).map(make_user).collect();
    json(200, &list)
}

async fn user(req: Req) -> Result<Response, Error> {
    let id: u32 = req.param("id")?;
    json(200, &make_user(id))
}

async fn compute(req: Req) -> Result<Response, Error> {
    let q: N = req.query()?;
    let n = black_box(q.n.unwrap_or(25).min(40));
    json(200, &serde_json::json!({ "result": fib(n) }))
}

fn main() -> std::io::Result<()> {
    let middleware = std::env::var_os("MIDDLEWARE").is_some();
    giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(5))
        .build()?
        .run(move |_| async move {
            let mut router = Router::new()
                .get("/json", hello)
                .get("/users", users)
                .get("/users/{id}", user)
                .get("/compute", compute);
            if middleware {
                router = router
                    .layer(RequestId::new())
                    .layer(Compression::new())
                    .layer(Timeout::new(Duration::from_secs(10)));
            }
            let listener = TcpListener::bind("127.0.0.1:3011").expect("bind");
            Server::new().serve(listener, router.build(())).await;
        })
}
