//! hyper's HTTP/1 connection API over `Compat` (through hyper-util's
//! `TokioIo`), against giong-http on the other side: hyper's client asks a
//! giong-http server, giong-http's client asks a hyper server. And hyper's
//! header timeout, on a timer built on giong's (the README's).

mod common;

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use giong::time::timeout;
use giong_compat::Compat;
use giong_http::client::Client;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

#[test]
fn hyper_client_against_a_giong_http_server() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            giong_http::Server::new()
                .serve(l, |req: giong_http::Request| async move {
                    match req.path() {
                        "/hello" => giong_http::Response::text("hello from giong-http"),
                        _ => giong_http::Response::text(req.body().to_vec()),
                    }
                })
                .await
        });

        let tcp = giong::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(Compat::new(tcp)))
                .await
                .unwrap();
        let conn = giong::spawn(conn);

        let req = http::Request::get("/hello")
            .header("host", "localhost")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let res = timeout(LIMIT, sender.send_request(req))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res.status(), 200);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello from giong-http");

        // Keep-alive: more requests on the same connection, with bodies.
        for len in [1usize, 10_000, 500_000] {
            let sent = Bytes::from(pattern(len));
            let req = http::Request::post("/echo")
                .header("host", "localhost")
                .body(Full::new(sent.clone()))
                .unwrap();
            sender.ready().await.unwrap();
            let res = timeout(LIMIT, sender.send_request(req))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(res.status(), 200);
            let body = res.into_body().collect().await.unwrap().to_bytes();
            assert!(body == sent, "{len} bytes came back different");
        }
        drop(sender);
        timeout(LIMIT, conn).await.unwrap().unwrap().unwrap();
        drop(server);
    });
}

#[test]
fn giong_http_client_against_a_hyper_server() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let tcp = l.accept().await.unwrap();
            let service = service_fn(|req: http::Request<Incoming>| async move {
                let path = req.uri().path().to_owned();
                let body = req.into_body().collect().await.unwrap().to_bytes();
                let reply = if path == "/hello" {
                    Bytes::from_static(b"hello from hyper")
                } else {
                    body
                };
                Ok::<_, Infallible>(http::Response::new(Full::new(reply)))
            });
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(Compat::new(tcp)), service)
                .await
                .unwrap();
        });

        let client = Client::builder().max_connections_per_host(1).build();
        let base = format!("http://{addr}");
        let res = timeout(LIMIT, client.get(&format!("{base}/hello")).send())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.body(), b"hello from hyper");
        for len in [1usize, 10_000, 500_000] {
            let sent = pattern(len);
            let res = timeout(
                LIMIT,
                client
                    .post(&format!("{base}/echo"))
                    .body(sent.clone())
                    .send(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(res.body() == &sent[..], "{len} bytes came back different");
        }
        // One connection carried all of them.
        assert_eq!(client.connections(&base).0, 1);
        drop(client);
        timeout(LIMIT, server).await.unwrap().unwrap();
    });
}

/// hyper's timer on giong's timers, as the README shows it.
#[derive(Clone, Copy, Debug)]
struct GiongTimer;

struct GiongSleep(giong::time::Sleep);

impl std::future::Future for GiongSleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl hyper::rt::Sleep for GiongSleep {}

impl hyper::rt::Timer for GiongTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(GiongSleep(giong::time::sleep(duration)))
    }
    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(GiongSleep(giong::time::sleep_until(deadline)))
    }
}

#[test]
fn a_hyper_server_with_giongs_timer_drops_a_request_head_that_never_ends() {
    giong::block_on(async {
        let (l, addr) = listener();
        // Half a request head, then nothing: the client waits for the server
        // to close the connection.
        let client = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut tcp = std::net::TcpStream::connect(addr).unwrap();
            tcp.write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\n")
                .unwrap();
            tcp.set_read_timeout(Some(LIMIT)).unwrap();
            let start = Instant::now();
            let mut buf = [0; 512];
            let mut got = Vec::new();
            loop {
                match tcp.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("the server kept the connection open: {e}"),
                }
            }
            (start.elapsed(), got)
        });
        let tcp = l.accept().await.unwrap();
        let service = service_fn(|_: http::Request<Incoming>| async {
            Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::new())))
        });
        let start = Instant::now();
        let res = timeout(
            LIMIT,
            hyper::server::conn::http1::Builder::new()
                .timer(GiongTimer)
                .header_read_timeout(Duration::from_millis(500))
                .serve_connection(TokioIo::new(Compat::new(tcp)), service),
        )
        .await
        .expect("hyper's header timeout did not fire");
        let err = res.expect_err("the connection ended without an error");
        assert!(err.is_timeout(), "{err}");
        assert!(start.elapsed() >= Duration::from_millis(500));
        // The connection is closed by the runtime: wait without blocking it.
        while !client.is_finished() {
            giong::time::sleep(Duration::from_millis(10)).await;
        }
        let (waited, _) = client.join().unwrap();
        assert!(waited < LIMIT);
    });
}
