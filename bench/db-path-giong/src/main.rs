//! The HTTP -> database -> HTTP path on giong, for bench/db-path.sh.
//!
//!   bench-db-path-giong <listen>
//!
//! `GET /pg/{id}` and `GET /my/{id}` read one user by primary key from
//! PostgreSQL (`PG_URL`) or MySQL (`MY_URL`) and answer it as JSON.
//! `WORKERS` workers (1 by default); `PG_POOL` and `MY_POOL` connections
//! per worker (2 each by default; the pools pipeline statements over their
//! connections). `PG_BATCH=1` turns on `Config::batch_reads`.

use giong::net::TcpListener;
use giong_web::{Error, Req, Response, Router, Server};
use serde::Serialize;

const PG_SELECT: &str = "SELECT id, name, email FROM users WHERE id = $1";
const MY_SELECT: &str = "SELECT id, name, email FROM users WHERE id = ?";

struct State {
    pg: Option<giong_postgres::Pool>,
    my: Option<giong_mysql::Pool>,
}

#[derive(Serialize)]
struct User<'a> {
    id: i64,
    name: &'a str,
    email: &'a str,
}

fn not_found() -> Error {
    Error::not_found("no_user", "no user with this id")
}

async fn pg(req: Req<State>) -> Result<Response, Error> {
    let id: i64 = req.param("id")?;
    let pool = req.state().pg.as_ref().ok_or_else(not_found)?;
    let row = pool
        .query_opt(PG_SELECT, &[&id])
        .await?
        .ok_or_else(not_found)?;
    let user = User {
        id: row.get(0)?,
        name: row.get(1)?,
        email: row.get(2)?,
    };
    Ok(Response::json(serde_json::to_vec(&user)?))
}

async fn my(req: Req<State>) -> Result<Response, Error> {
    let id: i64 = req.param("id")?;
    let pool = req.state().my.as_ref().ok_or_else(not_found)?;
    let row = pool
        .query_opt(MY_SELECT, &[&id])
        .await?
        .ok_or_else(not_found)?;
    let user = User {
        id: row.get(0)?,
        name: row.get(1)?,
        email: row.get(2)?,
    };
    Ok(Response::json(serde_json::to_vec(&user)?))
}

fn size(name: &str) -> usize {
    std::env::var(name).map_or(2, |v| v.parse().expect("a number"))
}

fn main() {
    let listen = std::env::args().nth(1).expect("listen address");
    let pg_url = std::env::var("PG_URL").ok();
    let my_url = std::env::var("MY_URL").ok();
    giong::Builder::new()
        .workers(std::env::var("WORKERS").map_or(1, |v| v.parse().expect("a number")))
        .build()
        .unwrap()
        .run(move |_| {
            let state = State {
                pg: pg_url.as_deref().map(|u| {
                    let batch = std::env::var("PG_BATCH").is_ok_and(|v| v == "1");
                    let config = giong_postgres::Config::from_url(u)
                        .unwrap()
                        .batch_reads(batch);
                    giong_postgres::Pool::new(config, size("PG_POOL"))
                }),
                my: my_url.as_deref().map(|u| {
                    giong_mysql::Pool::new(
                        giong_mysql::Config::from_url(u).unwrap(),
                        size("MY_POOL"),
                    )
                }),
            };
            let app = Router::new()
                .get("/pg/{id}", pg)
                .get("/my/{id}", my)
                .build(state);
            let listen = listen.clone();
            async move {
                let listener = TcpListener::bind(listen.as_str()).unwrap();
                Server::new().serve(listener, app).await;
            }
        })
        .unwrap();
}
