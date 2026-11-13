//! examples/rest-api on axum: the same routes, SQL, validation, cache-aside
//! reads and middleware (request ids, gzip from 1 KiB at the fastest level,
//! a 10 s timeout), with tokio-postgres + deadpool-postgres, redis-rs and
//! rustls. Configured by the same environment variables; for
//! bench/rest-api.sh.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use tokio_postgres::error::SqlState;
use tokio_postgres::{NoTls, Row};
use tower_http::compression::predicate::SizeAbove;
use tower_http::compression::{CompressionLayer, CompressionLevel};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;

const COLUMNS: &str = "id, name, email, \
     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')";
const MAX_PAGE: u32 = 1000;
const DEFAULT_PAGE: u32 = 50;

struct Queries {
    select_one: String,
    select_page: String,
    insert: String,
    update: String,
}

struct AppState {
    db: Pool,
    cache: ConnectionManager,
    cache_ttl_ms: u64,
    sql: Queries,
}

type Shared = std::sync::Arc<AppState>;

#[derive(Serialize)]
struct User<'a> {
    id: i64,
    name: &'a str,
    email: &'a str,
    created_at: &'a str,
}

impl<'a> User<'a> {
    fn from_row(row: &'a Row) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get(0)?,
            name: row.try_get(1)?,
            email: row.try_get(2)?,
            created_at: row.try_get(3)?,
        })
    }
}

#[derive(Deserialize)]
struct UserInput {
    name: String,
    email: String,
}

impl UserInput {
    fn validate(&self) -> Result<(), ApiError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(ApiError::new(
                422,
                "invalid_name",
                "name: between 1 and 100 characters, not only spaces".into(),
            ));
        }
        let email = self.email.trim();
        let well_formed = email
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'));
        if !well_formed || email.len() > 254 {
            return Err(ApiError::new(
                422,
                "invalid_email",
                "email: an address such as ada@example.com, at most 254 bytes".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Page {
    limit: Option<u32>,
    after: Option<i64>,
}

#[derive(Serialize)]
struct PageBody<'a> {
    users: Vec<User<'a>>,
    next_after: Option<i64>,
}

/// A problem+json answer, as giong-web gives.
struct ApiError {
    status: u16,
    code: &'static str,
    detail: String,
}

impl ApiError {
    fn new(status: u16, code: &'static str, detail: String) -> Self {
        Self {
            status,
            code,
            detail,
        }
    }

    fn internal(cause: impl std::fmt::Display) -> Self {
        eprintln!("error: {cause}");
        Self::new(
            500,
            "internal",
            "the server failed to answer this request; the cause is in its log".into(),
        )
    }
}

impl From<tokio_postgres::Error> for ApiError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::internal(e)
    }
}

impl From<deadpool_postgres::PoolError> for ApiError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        Self::internal(e)
    }
}

impl From<redis::RedisError> for ApiError {
    fn from(e: redis::RedisError) -> Self {
        Self::internal(e)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::json!({
            "status": self.status,
            "code": self.code,
            "title": status.canonical_reason().unwrap_or(""),
            "detail": self.detail,
        });
        (
            status,
            [
                (header::CONTENT_TYPE, "application/problem+json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body.to_string(),
        )
            .into_response()
    }
}

fn conflict_or(e: tokio_postgres::Error, email: &str) -> ApiError {
    if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        ApiError::new(
            409,
            "email_taken",
            format!("a user with the email {email:?} exists already"),
        )
    } else {
        e.into()
    }
}

fn not_found(id: i64) -> ApiError {
    ApiError::new(404, "user_not_found", format!("no user has the id {id}"))
}

fn json_body(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(s): State<Shared>) -> Result<&'static str, ApiError> {
    s.db.get().await?.query_one("SELECT 1", &[]).await?;
    let mut cache = s.cache.clone();
    redis::cmd("PING").query_async::<()>(&mut cache).await?;
    Ok("ready")
}

async fn list_users(
    State(s): State<Shared>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    let limit = page.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(ApiError::new(
            400,
            "invalid_limit",
            format!("limit: between 1 and {MAX_PAGE}"),
        ));
    }
    let after = page.after.unwrap_or(0);
    let client = s.db.get().await?;
    let stmt = client.prepare_cached(&s.sql.select_page).await?;
    let rows = client.query(&stmt, &[&after, &i64::from(limit)]).await?;
    let users = rows
        .iter()
        .map(User::from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let next_after = (users.len() == limit as usize)
        .then(|| users.last().map(|u| u.id))
        .flatten();
    Ok(json_body(
        StatusCode::OK,
        serde_json::to_vec(&PageBody { users, next_after })?,
    ))
}

async fn create_user(
    State(s): State<Shared>,
    Json(input): Json<UserInput>,
) -> Result<Response, ApiError> {
    input.validate()?;
    let (name, email) = (input.name.trim(), input.email.trim());
    let client = s.db.get().await?;
    let stmt = client.prepare_cached(&s.sql.insert).await?;
    let row = client
        .query_one(&stmt, &[&name, &email])
        .await
        .map_err(|e| conflict_or(e, email))?;
    let user = User::from_row(&row)?;
    let location = format!("/users/{}", user.id);
    let mut resp = json_body(StatusCode::CREATED, serde_json::to_vec(&user)?);
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_str(&location).unwrap());
    Ok(resp)
}

async fn get_user(State(s): State<Shared>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let key = format!("user:{id}");
    let mut cache = s.cache.clone();
    let cached: Result<Option<Vec<u8>>, _> =
        redis::cmd("GET").arg(&key).query_async(&mut cache).await;
    if let Ok(Some(body)) = cached {
        let mut resp = json_body(StatusCode::OK, body);
        resp.headers_mut()
            .insert("x-cache", HeaderValue::from_static("hit"));
        return Ok(resp);
    }
    let client = s.db.get().await?;
    let stmt = client.prepare_cached(&s.sql.select_one).await?;
    let row = client
        .query_opt(&stmt, &[&id])
        .await?
        .ok_or_else(|| not_found(id))?;
    let body = serde_json::to_vec(&User::from_row(&row)?)?;
    let _: Result<(), _> = redis::cmd("SET")
        .arg(&key)
        .arg(&body)
        .arg("PX")
        .arg(s.cache_ttl_ms)
        .query_async(&mut cache)
        .await;
    let mut resp = json_body(StatusCode::OK, body);
    resp.headers_mut()
        .insert("x-cache", HeaderValue::from_static("miss"));
    Ok(resp)
}

async fn update_user(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    Json(input): Json<UserInput>,
) -> Result<Response, ApiError> {
    input.validate()?;
    let (name, email) = (input.name.trim(), input.email.trim());
    let client = s.db.get().await?;
    let stmt = client.prepare_cached(&s.sql.update).await?;
    let row = client
        .query_opt(&stmt, &[&id, &name, &email])
        .await
        .map_err(|e| conflict_or(e, email))?
        .ok_or_else(|| not_found(id))?;
    let mut cache = s.cache.clone();
    redis::cmd("DEL")
        .arg(format!("user:{id}"))
        .query_async::<()>(&mut cache)
        .await?;
    Ok(json_body(
        StatusCode::OK,
        serde_json::to_vec(&User::from_row(&row)?)?,
    ))
}

async fn delete_user(State(s): State<Shared>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let client = s.db.get().await?;
    let stmt = client
        .prepare_cached("DELETE FROM users WHERE id = $1")
        .await?;
    if client.execute(&stmt, &[&id]).await? == 0 {
        return Err(not_found(id));
    }
    let mut cache = s.cache.clone();
    redis::cmd("DEL")
        .arg(format!("user:{id}"))
        .query_async::<()>(&mut cache)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn main() {
    let workers: usize = var("WORKERS").map_or_else(
        || std::thread::available_parallelism().map_or(1, |n| n.get()),
        |v| v.parse().expect("WORKERS"),
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(serve());
}

async fn serve() {
    let url = var("DATABASE_URL").expect("DATABASE_URL");
    let pg: tokio_postgres::Config = url.parse().expect("DATABASE_URL");
    let manager = Manager::from_config(
        pg,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool_size = var("DB_POOL").map_or(16, |v| v.parse().expect("DB_POOL"));
    let db = Pool::builder(manager).max_size(pool_size).build().unwrap();
    db.get()
        .await
        .expect("connect to PostgreSQL")
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS users (
                id bigserial PRIMARY KEY,
                name text NOT NULL,
                email text NOT NULL UNIQUE,
                created_at timestamptz NOT NULL DEFAULT now())",
        )
        .await
        .expect("create the table");
    let redis_url = var("REDIS_URL").unwrap_or_else(|| "redis://127.0.0.1:6379".into());
    let client = redis::Client::open(redis_url).expect("REDIS_URL");
    let cache = ConnectionManager::new(client)
        .await
        .expect("connect to Redis");
    let ttl: u64 = var("CACHE_TTL_SECS").map_or(60, |v| v.parse().expect("CACHE_TTL_SECS"));
    let state: Shared = std::sync::Arc::new(AppState {
        db,
        cache,
        cache_ttl_ms: ttl * 1000,
        sql: Queries {
            select_one: format!("SELECT {COLUMNS} FROM users WHERE id = $1"),
            select_page: format!("SELECT {COLUMNS} FROM users WHERE id > $1 ORDER BY id LIMIT $2"),
            insert: format!("INSERT INTO users (name, email) VALUES ($1, $2) RETURNING {COLUMNS}"),
            update: format!(
                "UPDATE users SET name = $2, email = $3 WHERE id = $1 RETURNING {COLUMNS}"
            ),
        },
    });

    let x_request_id = HeaderName::from_static("x-request-id");
    let users = Router::new()
        .route("/", get(list_users).post(create_user))
        .route("/{id}", get(get_user).put(update_user).delete(delete_user));
    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .nest("/users", users)
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(10),
        ))
        .layer(
            CompressionLayer::new()
                .quality(CompressionLevel::Fastest)
                .compress_when(SizeAbove::new(1024)),
        )
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid));

    let listen: std::net::SocketAddr = var("LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8443".into())
        .parse()
        .expect("LISTEN");
    match (var("TLS_CERT"), var("TLS_KEY")) {
        (Some(cert), Some(key)) => {
            rustls::crypto::ring::default_provider()
                .install_default()
                .unwrap();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .expect("TLS_CERT/TLS_KEY");
            axum_server::bind_rustls(listen, tls)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        }
    }
}
