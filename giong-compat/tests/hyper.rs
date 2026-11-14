//! hyper's HTTP/1 connection API over `Compat` (through hyper-util's
//! `TokioIo`), against giong-http on the other side: hyper's client asks a
//! giong-http server, giong-http's client asks a hyper server.

mod common;

use std::convert::Infallible;

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
