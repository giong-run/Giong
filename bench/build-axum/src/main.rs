use std::time::Duration;
use axum::{extract::{Path, State, Json}, http::StatusCode, response::IntoResponse, routing::{get, post}, Router};
use serde::{Deserialize, Serialize};
use tower_http::{compression::CompressionLayer, timeout::TimeoutLayer, trace::TraceLayer,
    request_id::{MakeRequestUuid, SetRequestIdLayer, PropagateRequestIdLayer}};

#[derive(Clone)]
struct AppState { greeting: &'static str }
#[derive(Deserialize)]
struct NewUser { name: String }
#[derive(Serialize)]
struct User { id: u64, name: String }

async fn hello(State(s): State<AppState>, Path(name): Path<String>) -> String {
    format!("{}, {name}!", s.greeting)
}
async fn get_user(Path(id): Path<u64>) -> Result<Json<User>, StatusCode> {
    if id == 0 { return Err(StatusCode::NOT_FOUND); }
    Ok(Json(User { id, name: "Ada".into() }))
}
async fn create_user(Json(new): Json<NewUser>) -> impl IntoResponse {
    (StatusCode::CREATED, Json(User { id: 1, name: new.name }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let app = Router::new()
        .route("/hello/{name}", get(hello))
        .route("/users/{id}", get(get_user))
        .route("/users", post(create_user))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(AppState { greeting: "Hello" });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); }).await.unwrap();
}
