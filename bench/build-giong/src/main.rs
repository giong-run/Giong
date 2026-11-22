use std::time::Duration;
use giong::net::TcpListener;
use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
use giong_web::{json, Error, Req, Response, Router, Server, Sink};
use serde::{Deserialize, Serialize};

struct State {
    greeting: &'static str,
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
}

#[derive(Serialize)]
struct User {
    id: u64,
    name: String,
}

async fn hello(req: Req<State>) -> Result<Response, Error> {
    let name: String = req.param("name")?;
    Ok(Response::text(format!("{}, {name}!", req.state().greeting)))
}

async fn get_user(req: Req<State>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?; // "abc" is a 400 that names `id`
    if id == 0 {
        return Err(Error::not_found("user_not_found", format!("no user {id}")));
    }
    json(200, &User { id, name: "Ada".into() })
}

async fn create_user(req: Req<State>) -> Result<Response, Error> {
    let new: NewUser = req.json()?; // 415, 400 or 422 with what is wrong
    json(201, &User { id: 1, name: new.name })
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(10))
        .build()?
        .run(|_worker| async {
            // Every worker builds its own app: nothing is locked, nothing needs `Send`.
            let app = Router::new()
                .get("/hello/{name}", hello)
                .get("/users/{id}", get_user)
                .post("/users", create_user)
                .layer(RequestId::new())
                .layer(Logger::new(Sink::Stdout))
                .layer(Compression::new())
                .layer(Timeout::new(Duration::from_secs(10)))
                .build(State { greeting: "Hello" });
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, app).await;
        })
}
