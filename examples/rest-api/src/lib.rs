//! A REST API on giong: users kept in PostgreSQL, read through a Redis
//! cache, with nothing from the Tokio ecosystem.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /health` | `ok` while the process runs |
//! | `GET /ready` | `ready` when the database and Redis answer, `503` otherwise |
//! | `GET /users?limit=50&after=0` | a page of users, by id |
//! | `POST /users` | creates a user: `{"name": .., "email": ..}` |
//! | `GET /users/{id}` | one user, from the cache when it is there |
//! | `PUT /users/{id}` | replaces a user's name and email |
//! | `DELETE /users/{id}` | deletes a user |
//! | `GET /users/export` | every user, one JSON object per line, streamed |
//!
//! `main.rs` reads the configuration and starts the server; this file is
//! the API, which the tests build too.

use std::time::Duration;

use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
use giong_web::{App, Error, Req, Response, Router, Sink};

mod users;

/// What every handler of a worker shares: its own pools, nothing locked.
pub struct State {
    pub db: giong_postgres::Pool,
    pub cache: giong_redis::Pool,
    /// How long a user stays in the cache.
    pub cache_ttl: Duration,
}

/// The tables, created at startup when they are missing.
pub const SCHEMA: &str = include_str!("../migrations/0001_init.sql");

async fn health(_req: Req<State>) -> Result<Response, Error> {
    Ok(Response::text("ok"))
}

/// `503` (with the cause in the log) when either store does not answer.
async fn ready(req: Req<State>) -> Result<Response, Error> {
    let state = req.state();
    state.db.query_one("SELECT 1", &[]).await?;
    state.cache.ping().await?;
    Ok(Response::text("ready"))
}

/// The access log: one JSON line per request, or none.
pub enum AccessLog {
    Stdout,
    Off,
}

/// The API, with its middleware, for one worker.
pub fn app(state: State, log: AccessLog) -> App<State> {
    let mut router = Router::new()
        .get("/health", health)
        .get("/ready", ready)
        .nest("/users", users::routes())
        .layer(RequestId::new());
    if let AccessLog::Stdout = log {
        router = router.layer(Logger::new(Sink::Stdout));
    }
    router
        .layer(Compression::new())
        .layer(Timeout::new(Duration::from_secs(10)))
        .build(state)
}

// Not from the template: the rest of this file is examples/rest-api's own (the
// part above is what `giong new` writes, checked by giong-cli's tests).

/// The Rust samples in the repository's Markdown, compiled by
/// `cargo test --doc -p rest-api` so that none of them drifts from the API.
/// The ones that need a server, a database or a certificate are marked
/// `no_run` and only compiled; the rest run.
#[cfg(doctest)]
mod markdown {
    #[doc = include_str!("../../../README.md")]
    mod readme {}
    #[doc = include_str!("../../../docs/DEPLOYMENT.md")]
    mod deployment {}
    #[doc = include_str!("../../../docs/errors/README.md")]
    mod errors {}
    #[doc = include_str!("../../../giong-compat/README.md")]
    mod giong_compat {}
    #[doc = include_str!("../../../giong-http/README.md")]
    mod giong_http {}
    #[doc = include_str!("../../../giong-mcp/README.md")]
    mod giong_mcp {}
    #[doc = include_str!("../../../giong-mysql/README.md")]
    mod giong_mysql {}
    #[doc = include_str!("../../../giong-postgres/README.md")]
    mod giong_postgres {}
    #[doc = include_str!("../../../giong-redis/README.md")]
    mod giong_redis {}
    #[doc = include_str!("../../../giong-tls/README.md")]
    mod giong_tls {}
    #[doc = include_str!("../../../giong-web/README.md")]
    mod giong_web {}
    #[doc = include_str!("../README.md")]
    mod rest_api {}
}
