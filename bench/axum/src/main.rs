use axum::{extract::{Path, Query}, http::{HeaderName, StatusCode}, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::time::Duration;
use tower_http::compression::{predicate::SizeAbove, CompressionLayer, CompressionLevel};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;

#[derive(Serialize)]
struct Msg { message: &'static str }

#[derive(Serialize)]
struct User { id: u32, name: String, email: String, active: bool, score: f64 }

#[derive(Deserialize)]
struct N { n: Option<u32> }

#[derive(Deserialize)]
struct Ms { ms: Option<u64> }

fn fib(n: u32) -> u64 { if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) } }

fn make_user(i: u32) -> User {
    User { id: i, name: format!("User {i}"), email: format!("user{i}@example.com"),
        active: i % 2 == 0, score: i as f64 * 1.5 }
}

async fn json() -> Json<Msg> { Json(Msg { message: "Hello, World!" }) }

async fn users() -> Json<Vec<User>> { Json((0..100).map(make_user).collect()) }

async fn user(Path(id): Path<u32>) -> Json<User> { Json(make_user(id)) }

async fn compute(Query(q): Query<N>) -> Json<serde_json::Value> {
    let n = black_box(q.n.unwrap_or(25));
    Json(serde_json::json!({ "result": fib(n) }))
}

/// `/wait?ms=N` answers after N milliseconds, like a handler waiting for a
/// database or another service (bench/memory.sh).
async fn wait(Query(q): Query<Ms>) -> Json<Msg> {
    tokio::time::sleep(Duration::from_millis(q.ms.unwrap_or(0))).await;
    Json(Msg { message: "Hello, World!" })
}

#[tokio::main]
async fn main() {
    let mut app = Router::new()
        .route("/json", get(json))
        .route("/users", get(users))
        .route("/users/{id}", get(user))
        .route("/compute", get(compute))
        .route("/wait", get(wait));
    // MIDDLEWARE=1: what bench/giong-web adds with RequestId, Compression
    // (gzip level 1 from 1 KiB) and Timeout, from tower-http. Layers listed
    // last run first.
    if std::env::var_os("MIDDLEWARE").is_some() {
        let id = HeaderName::from_static("x-request-id");
        app = app
            .layer(TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, Duration::from_secs(10)))
            .layer(CompressionLayer::new().quality(CompressionLevel::Fastest).compress_when(SizeAbove::new(1024)))
            .layer(PropagateRequestIdLayer::new(id.clone()))
            .layer(SetRequestIdLayer::new(id, MakeRequestUuid));
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:3003").await.unwrap();
    axum::serve(l, app).await.unwrap();
}
