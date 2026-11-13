//! An app served by giong-http over real sockets, called with giong-http's
//! client.

use std::time::Duration;

use giong::net::TcpListener;
use giong_http::client::Client;
use giong_web::middleware::{Compression, Cors, RequestId, Timeout};
use giong_web::{App, Error, Req, Response, Router, Server};

async fn users(_req: Req) -> Result<Response, Error> {
    let items: Vec<String> = (0..300)
        .map(|i| format!("{{\"id\":{i},\"name\":\"user {i}\"}}"))
        .collect();
    Ok(Response::json(format!("[{}]", items.join(","))))
}

async fn echo(req: Req) -> Result<Response, Error> {
    let id: u32 = req.param("id")?;
    Ok(Response::new(201)
        .header("Content-Type", "text/plain")
        .body(format!("{id}:{}", String::from_utf8_lossy(req.body()))))
}

fn app() -> App {
    Router::new()
        .get("/users", users)
        .post("/echo/{id}", echo)
        .layer(RequestId::new())
        .layer(Cors::new().allow_origin("https://app.example.com"))
        .layer(Compression::new())
        .layer(Timeout::new(Duration::from_secs(10)))
        .build(())
}

/// Serves `app()` on a free port of this worker; returns the base URL.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move { Server::new().serve(listener, app()).await });
    format!("http://{addr}")
}

#[test]
fn requests_over_a_kept_alive_connection() {
    giong::block_on(async {
        let base = serve();
        let client = Client::new();

        // gzip on the wire...
        let raw = Client::builder().decompress(false).build();
        let resp = raw
            .get(&format!("{base}/users"))
            .header("Accept-Encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.header("content-encoding"), Some("gzip"));
        assert_eq!(&resp.body()[..2], &[0x1f, 0x8b]);
        // ...which the client decompresses.
        let resp = client.get(&format!("{base}/users")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.text().unwrap().starts_with("[{\"id\":0,"));
        assert_eq!(resp.header("x-request-id").map(str::len), Some(20));

        let resp = client
            .post(&format!("{base}/echo/9"))
            .header("Origin", "https://app.example.com")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        assert_eq!(resp.text().unwrap(), "9:hello");
        assert_eq!(
            resp.header("access-control-allow-origin"),
            Some("https://app.example.com")
        );

        let resp = client.get(&format!("{base}/echo/9")).send().await.unwrap();
        assert_eq!(resp.status(), 405);
        assert_eq!(resp.header("allow"), Some("POST, OPTIONS"));
        assert_eq!(
            resp.header("content-type"),
            Some("application/problem+json")
        );

        for i in 0..50 {
            let resp = client
                .post(&format!("{base}/echo/{i}"))
                .body("x")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.text().unwrap(), format!("{i}:x"));
        }
        let (open, idle) = client.connections(&base);
        assert_eq!((open, idle), (1, 1), "every request reused one connection");
    });
}
