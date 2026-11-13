//! A complete REST API on giong: users kept in PostgreSQL, read through a
//! Redis cache, served over HTTPS, with nothing from the Tokio ecosystem.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /health` | `ok` while the process runs |
//! | `GET /ready` | `ready` when PostgreSQL and Redis answer, `503` otherwise |
//! | `GET /users?limit=50&after=0` | a page of users, by id |
//! | `POST /users` | creates a user: `{"name": .., "email": ..}` |
//! | `GET /users/{id}` | one user, from the cache when it is there |
//! | `PUT /users/{id}` | replaces a user's name and email |
//! | `DELETE /users/{id}` | deletes a user |
//! | `GET /users/export` | every user, one JSON object per line, streamed |
//!
//! `main.rs` reads the configuration and starts the server; this file is
//! the API, which the tests build too.

use std::borrow::Cow;
use std::time::Duration;

use giong_postgres::{Pool as Db, Row};
use giong_redis::Pool as Cache;
use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
use giong_web::{json, App, Body, Error, Req, Response, Router, Sink};
use serde::{Deserialize, Serialize};

/// What every handler of a worker shares: its own pools, nothing locked.
pub struct State {
    pub db: Db,
    pub cache: Cache,
    /// How long a user stays in the cache.
    pub cache_ttl: Duration,
}

/// The table, created at startup when it is missing.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS users (
    id bigserial PRIMARY KEY,
    name text NOT NULL,
    email text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
)";

/// The columns a user is read with; `created_at` as RFC 3339 text.
macro_rules! columns {
    () => {
        "id, name, email, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"
    };
}

const SELECT_ONE: &str = concat!("SELECT ", columns!(), " FROM users WHERE id = $1");
const SELECT_PAGE: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM users WHERE id > $1 ORDER BY id LIMIT $2"
);
const INSERT: &str = concat!(
    "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING ",
    columns!()
);
const UPDATE: &str = concat!(
    "UPDATE users SET name = $2, email = $3 WHERE id = $1 RETURNING ",
    columns!()
);
const DELETE: &str = "DELETE FROM users WHERE id = $1";

/// The largest page `GET /users` gives, and the default.
const MAX_PAGE: u32 = 1000;
const DEFAULT_PAGE: u32 = 50;

#[derive(Serialize)]
struct User<'a> {
    id: i64,
    name: &'a str,
    email: &'a str,
    created_at: &'a str,
}

impl<'a> User<'a> {
    fn from_row(row: &'a Row) -> Result<Self, Error> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            email: row.get(2)?,
            created_at: row.get(3)?,
        })
    }
}

/// The body of `POST /users` and `PUT /users/{id}`.
#[derive(Deserialize)]
struct UserInput<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    #[serde(borrow)]
    email: Cow<'a, str>,
}

impl UserInput<'_> {
    /// `422` with a code and a detail for each field that is not acceptable.
    fn validate(&self) -> Result<(), Error> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(Error::unprocessable(
                "invalid_name",
                "name: between 1 and 100 characters, not only spaces",
            ));
        }
        let email = self.email.trim();
        let well_formed = email
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'));
        if !well_formed || email.len() > 254 {
            return Err(Error::unprocessable(
                "invalid_email",
                "email: an address such as ada@example.com, at most 254 bytes",
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
    /// The `after` of the next page; `null` on the last one.
    next_after: Option<i64>,
}

fn cache_key(id: i64) -> String {
    format!("user:{id}")
}

/// A user already taken: PostgreSQL's unique violation, as a `409`.
fn conflict_or(err: giong_postgres::Error, email: &str) -> Error {
    if err.code() == Some("23505") {
        Error::conflict(
            "email_taken",
            format!("a user with the email {email:?} exists already"),
        )
    } else {
        err.into()
    }
}

fn not_found(id: i64) -> Error {
    Error::not_found("user_not_found", format!("no user has the id {id}"))
}

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

async fn list_users(req: Req<State>) -> Result<Response, Error> {
    let page: Page = req.query()?;
    let limit = page.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(Error::bad_request(
            "invalid_limit",
            format!("limit: between 1 and {MAX_PAGE}"),
        ));
    }
    let after = page.after.unwrap_or(0);
    let rows = req
        .state()
        .db
        .query(SELECT_PAGE, &[&after, &i64::from(limit)])
        .await?;
    let users = rows
        .iter()
        .map(User::from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let next_after = (users.len() == limit as usize)
        .then(|| users.last().map(|u| u.id))
        .flatten();
    json(200, &PageBody { users, next_after })
}

async fn create_user(req: Req<State>) -> Result<Response, Error> {
    let input: UserInput = req.json()?;
    input.validate()?;
    let (name, email) = (input.name.trim(), input.email.trim());
    let row = req
        .state()
        .db
        .query_one(INSERT, &[&name, &email])
        .await
        .map_err(|e| conflict_or(e, email))?;
    let user = User::from_row(&row)?;
    let location = format!("/users/{}", user.id);
    Ok(json(201, &user)?.header("Location", &location))
}

/// Cache-aside: Redis first; on a miss, PostgreSQL, then the answer is
/// cached for `cache_ttl`. A cache that is down slows reads down but does
/// not fail them.
async fn get_user(req: Req<State>) -> Result<Response, Error> {
    let id: i64 = req.param("id")?;
    let state = req.state();
    let key = cache_key(id);
    if let Ok(Some(cached)) = state.cache.get::<Option<Vec<u8>>>(&key).await {
        return Ok(Response::json(cached).header("X-Cache", "hit"));
    }
    let row = state
        .db
        .query_opt(SELECT_ONE, &[&id])
        .await?
        .ok_or_else(|| not_found(id))?;
    let body = serde_json::to_vec(&User::from_row(&row)?)?;
    // Not worth failing the request for: the next read tries again.
    let _ = state.cache.set_ex(&key, &body, state.cache_ttl).await;
    Ok(Response::json(body).header("X-Cache", "miss"))
}

async fn update_user(req: Req<State>) -> Result<Response, Error> {
    let id: i64 = req.param("id")?;
    let input: UserInput = req.json()?;
    input.validate()?;
    let (name, email) = (input.name.trim(), input.email.trim());
    let state = req.state();
    let row = state
        .db
        .query_opt(UPDATE, &[&id, &name, &email])
        .await
        .map_err(|e| conflict_or(e, email))?
        .ok_or_else(|| not_found(id))?;
    // After the write, so a read in between caches at most the old row
    // until `cache_ttl` runs out. Failing here would leave the old row
    // cached: report it.
    state.cache.del(cache_key(id)).await?;
    json(200, &User::from_row(&row)?)
}

async fn delete_user(req: Req<State>) -> Result<Response, Error> {
    let id: i64 = req.param("id")?;
    let state = req.state();
    if state.db.execute(DELETE, &[&id]).await? == 0 {
        return Err(not_found(id));
    }
    state.cache.del(cache_key(id)).await?;
    Ok(Response::new(204))
}

/// Every user as NDJSON, read a page at a time and sent as it is read:
/// memory stays at one page whatever the table's size, and a client that
/// reads slowly slows the reading down.
async fn export_users(req: Req<State>) -> Result<Response, Error> {
    let state = req.state_rc();
    let (tx, body) = Body::channel();
    drop(giong::spawn(async move {
        let mut after = 0i64;
        loop {
            let limit = i64::from(MAX_PAGE);
            let rows = match state.db.query(SELECT_PAGE, &[&after, &limit]).await {
                Ok(rows) => rows,
                // The status is gone already: cut the body short, so the
                // client sees that it is incomplete.
                Err(_) => return tx.abort(),
            };
            let mut lines = Vec::with_capacity(rows.len() * 96);
            for row in &rows {
                let Ok(user) = User::from_row(row) else {
                    return tx.abort();
                };
                after = user.id;
                if serde_json::to_writer(&mut lines, &user).is_err() {
                    return tx.abort();
                }
                lines.push(b'\n');
            }
            if tx.send(lines).await.is_err() {
                return; // The client went away.
            }
            if rows.len() < MAX_PAGE as usize {
                return;
            }
        }
    }));
    Ok(Response::new(200)
        .header("Content-Type", "application/x-ndjson")
        .body(body))
}

/// The access log: one JSON line per request, or none.
pub enum AccessLog {
    Stdout,
    Off,
}

/// The API, with its middleware, for one worker.
pub fn app(state: State, log: AccessLog) -> App<State> {
    let users = Router::new()
        .get("/", list_users)
        .post("/", create_user)
        // Text beats a parameter, so /users/export is not a user id.
        .get("/export", export_users)
        .get("/{id}", get_user)
        .put("/{id}", update_user)
        .delete("/{id}", delete_user);
    let mut router = Router::new()
        .get("/health", health)
        .get("/ready", ready)
        .nest("/users", users)
        .layer(RequestId::new());
    if let AccessLog::Stdout = log {
        router = router.layer(Logger::new(Sink::Stdout));
    }
    router
        .layer(Compression::new())
        .layer(Timeout::new(Duration::from_secs(10)))
        .build(state)
}
