//! The HTTP -> database -> HTTP path on axum, for bench/db-path.sh.
//!
//!   bench-db-path-tokio <listen> <deadpool|sqlx>
//!
//! `GET /pg/{id}` and `GET /my/{id}` read one user by primary key from
//! PostgreSQL (`PG_URL`) or MySQL (`MY_URL`, SQLx only) and answer it as
//! JSON. A current-thread runtime; `PG_POOL` and `MY_POOL` connections (32
//! each by default: these pools give each statement a connection).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

const PG_SELECT: &str = "SELECT id, name, email FROM users WHERE id = $1";
const MY_SELECT: &str = "SELECT id, name, email FROM users WHERE id = ?";

enum Pg {
    Deadpool(deadpool_postgres::Pool),
    Sqlx(sqlx::PgPool),
}

struct Db {
    pg: Option<Pg>,
    my: Option<sqlx::MySqlPool>,
}

#[derive(Serialize)]
struct User {
    id: i64,
    name: String,
    email: String,
}

fn fail(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

async fn pg(State(db): State<Arc<Db>>, Path(id): Path<i64>) -> Response {
    let user = match db.pg.as_ref() {
        Some(Pg::Deadpool(pool)) => {
            let client = match pool.get().await {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let statement = match client.prepare_cached(PG_SELECT).await {
                Ok(s) => s,
                Err(e) => return fail(e),
            };
            match client.query_opt(&statement, &[&id]).await {
                Ok(Some(row)) => User {
                    id: row.get(0),
                    name: row.get(1),
                    email: row.get(2),
                },
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                Err(e) => return fail(e),
            }
        }
        Some(Pg::Sqlx(pool)) => {
            match sqlx::query_as::<_, (i64, String, String)>(PG_SELECT)
                .bind(id)
                .fetch_optional(pool)
                .await
            {
                Ok(Some((id, name, email))) => User { id, name, email },
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                Err(e) => return fail(e),
            }
        }
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(user).into_response()
}

async fn my(State(db): State<Arc<Db>>, Path(id): Path<i64>) -> Response {
    let Some(pool) = db.my.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match sqlx::query_as::<_, (i64, String, String)>(MY_SELECT)
        .bind(id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some((id, name, email))) => Json(User { id, name, email }).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

fn size(name: &str) -> u32 {
    std::env::var(name).map_or(32, |v| v.parse().expect("a number"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let listen = args.next().expect("listen address");
    let driver = args.next().expect("deadpool or sqlx");
    let pg_pool = match std::env::var("PG_URL").ok() {
        None => None,
        Some(url) if driver == "deadpool" => {
            let config: tokio_postgres::Config = url.parse().unwrap();
            let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
            let pool = deadpool_postgres::Pool::builder(manager)
                .max_size(size("PG_POOL") as usize)
                .build()
                .unwrap();
            Some(Pg::Deadpool(pool))
        }
        Some(url) => Some(Pg::Sqlx(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(size("PG_POOL"))
                .connect(&url)
                .await
                .unwrap(),
        )),
    };
    let my_pool = match std::env::var("MY_URL").ok() {
        Some(url) => Some(
            sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(size("MY_POOL"))
                .connect(&url)
                .await
                .unwrap(),
        ),
        None => None,
    };
    let app = Router::new()
        .route("/pg/{id}", get(pg))
        .route("/my/{id}", get(my))
        .with_state(Arc::new(Db {
            pg: pg_pool,
            my: my_pool,
        }));
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
