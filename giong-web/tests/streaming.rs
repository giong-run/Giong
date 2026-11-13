//! Streamed bodies through an app: routes that read their body as it
//! arrives (`Router::stream_bodies`), responses written as they are
//! produced (`Body::channel`), and what the middleware does with both.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::sleep;
use giong_http::client::Client;
use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
use giong_web::{App, Body, Error, Req, Response, Router, Server, Sink};

/// The largest body the streaming routes accept.
const LIMIT: u64 = 1 << 20;

async fn upload(req: Req) -> Result<Response, Error> {
    let body = req.body_stream().expect("a streaming route");
    assert!(req.body().is_empty());
    let mut total = 0;
    while let Some(piece) = body.chunk().await? {
        total += piece.len();
    }
    Ok(Response::text(format!("{total} bytes")))
}

#[cfg(feature = "serde")]
async fn parse(req: Req) -> Result<Response, Error> {
    let value: serde_json::Value = req.json()?;
    Ok(Response::text(value.to_string()))
}

async fn echo(req: Req) -> Result<Response, Error> {
    assert!(req.body_stream().is_none());
    Ok(Response::text(format!("{} bytes", req.body().len())))
}

async fn events(_req: Req) -> Result<Response, Error> {
    let (tx, body) = Body::channel();
    drop(giong::spawn(async move {
        for n in 0..3 {
            // Long enough for the Compression middleware to want it.
            let event = format!("data: {}\n\n", "event ".repeat(300 + n));
            if tx.send(event).await.is_err() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    }));
    Ok(Response::new(200)
        .header("Content-Type", "text/event-stream")
        .body(body))
}

fn app(log: Rc<RefCell<Vec<String>>>) -> App {
    let mut uploads = Router::new()
        .put("/files/{*path}", upload)
        .stream_bodies(LIMIT);
    #[cfg(feature = "serde")]
    {
        uploads = uploads.post("/json", parse);
    }
    Router::new()
        .post("/echo", echo)
        .get("/events", events)
        .nest("/api", uploads)
        .layer(RequestId::new())
        .layer(Logger::new(Sink::Custom(Rc::new(move |line| {
            log.borrow_mut().push(line.to_string());
        }))))
        .layer(Compression::new().min_size(64))
        .layer(Timeout::new(Duration::from_millis(200)))
        .build(())
}

/// Serves `app()` on a free port of this worker, with a small
/// `max_body_size`; returns the address and the log lines.
fn serve() -> (String, Rc<RefCell<Vec<String>>>) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = app(log.clone());
    giong::spawn(async move { Server::new().max_body_size(1024).serve(listener, app).await });
    (addr, log)
}

/// Sends `pieces` on a new connection, `pause` apart, and reads until the
/// server closes or a complete response with a `Content-Length` arrived.
async fn raw(addr: &str, pieces: &[&[u8]], pause: Duration) -> String {
    let stream = TcpStream::connect(addr).await.unwrap();
    for piece in pieces {
        let (res, _) = stream.write_all(piece.to_vec()).await;
        res.unwrap();
        sleep(pause).await;
    }
    let mut got = Vec::new();
    loop {
        let chunk = stream.recv().await.unwrap();
        if chunk.is_empty() {
            break;
        }
        got.extend_from_slice(&chunk);
        let text = String::from_utf8_lossy(&got);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|n| n.parse::<usize>().ok());
            if length.is_some_and(|n| body.len() >= n) {
                break;
            }
        }
    }
    String::from_utf8(got).unwrap()
}

#[test]
fn streaming_routes_take_bodies_past_the_servers_limit() {
    giong::block_on(async {
        let (addr, log) = serve();
        let base = format!("http://{addr}");
        let client = Client::new();

        let big = vec![b'x'; 300_000];
        let resp = client
            .put(&format!("{base}/api/files/a/b.bin"))
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().unwrap(), "300000 bytes");

        // Other routes keep the server's limit.
        let resp = client
            .post(&format!("{base}/echo"))
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
        let resp = client
            .post(&format!("{base}/echo"))
            .body("small")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.text().unwrap(), "5 bytes");

        // Announced past the route's limit: refused before the handler.
        let resp = client
            .put(&format!("{base}/api/files/huge"))
            .body(vec![b'x'; LIMIT as usize + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);

        let lines = log.borrow();
        assert!(
            lines[0].contains("\"route\":\"/api/files/{*path}\""),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("\"status\":200"), "{}", lines[0]);
    });
}

#[test]
fn a_chunked_body_past_the_limit_is_413_with_a_code() {
    giong::block_on(async {
        let (addr, _log) = serve();
        let head = b"PUT /api/files/x HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n";
        let first = b"4\r\nabcd\r\n".as_slice();
        let too_big = format!("{:x}\r\n", LIMIT);
        let reply = raw(
            &addr,
            &[head, first, too_big.as_bytes()],
            Duration::from_millis(5),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 413 "), "{reply}");
        assert!(reply.contains("\"code\":\"GIONG-E1111\""), "{reply}");
        assert!(reply.contains("Connection: close"), "{reply}");

        let broken = b"3\r\nabcX\r\n".as_slice();
        let reply = raw(&addr, &[head, broken], Duration::ZERO).await;
        assert!(reply.starts_with("HTTP/1.1 400 "), "{reply}");
        assert!(reply.contains("\"code\":\"GIONG-E1113\""), "{reply}");
    });
}

#[test]
fn the_timeout_does_not_cut_a_slow_upload() {
    giong::block_on(async {
        let (addr, _log) = serve();
        // Ten pieces 50 ms apart: half a second, past the 200 ms timeout.
        let head = b"PUT /api/files/slow HTTP/1.1\r\nHost: t\r\nContent-Length: 100\r\n\r\n";
        let piece = [b'y'; 10];
        let mut pieces: Vec<&[u8]> = vec![&piece[..]; 11];
        pieces[0] = head;
        let reply = raw(&addr, &pieces, Duration::from_millis(50)).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert!(reply.ends_with("100 bytes"), "{reply}");
    });
}

#[cfg(feature = "serde")]
#[test]
fn json_on_a_streaming_route_is_a_500_with_the_cause_in_the_log() {
    giong::block_on(async {
        let (addr, log) = serve();
        let resp = Client::new()
            .post(&format!("http://{addr}/api/json"))
            .header("Content-Type", "application/json")
            .body("{\"a\":1}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        assert!(resp.text().unwrap().contains("GIONG-E1210"));
        let lines = log.borrow();
        assert!(lines[0].contains("GIONG-E1213"), "{}", lines[0]);
        assert!(lines[0].contains("Req::body_stream"), "{}", lines[0]);
    });
}

#[test]
fn a_streamed_response_passes_through_the_middleware_untouched() {
    giong::block_on(async {
        let (addr, log) = serve();
        let client = Client::builder().decompress(false).build();
        let mut resp = client
            .get(&format!("http://{addr}/events"))
            .header("Accept-Encoding", "gzip")
            .send_streaming()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.header("transfer-encoding"), Some("chunked"));
        assert_eq!(resp.header("content-encoding"), None);
        assert_eq!(resp.header("x-request-id").map(str::len), Some(20));
        let mut body = Vec::new();
        while let Some(piece) = resp.chunk().await.unwrap() {
            body.extend_from_slice(&piece);
        }
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.matches("data: ").count(), 3);
        let lines = log.borrow();
        assert!(lines[0].contains("\"bytes\":null"), "{}", lines[0]);
    });
}
