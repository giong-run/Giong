//! HTTP/2 with the h2 crate over `Compat`: server and client on giong, many
//! streams at once on one connection, request and response bodies, and h2
//! over giong-tls with ALPN.

mod common;

use std::rc::Rc;

use bytes::Bytes;
use common::*;
use giong::time::timeout;
use giong_compat::{Compat, Transport};
use http::{Request, Response};

/// Serves HTTP/2 on `io`: answers `GET /hello` with a greeting and echoes
/// the body of any other request. Returns how many requests it answered.
async fn serve<S: Transport>(io: Compat<S>) -> usize
where
    Compat<S>: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut conn = h2::server::handshake(io).await.unwrap();
    let answered = Rc::new(std::cell::Cell::new(0));
    while let Some(next) = conn.accept().await {
        let (req, mut respond) = next.unwrap();
        let answered = answered.clone();
        // Each stream in a task of its own, like a real server.
        giong::spawn(async move {
            let (head, mut body) = req.into_parts();
            let mut got = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = chunk.unwrap();
                body.flow_control().release_capacity(chunk.len()).unwrap();
                got.extend_from_slice(&chunk);
            }
            let reply = if head.uri.path() == "/hello" {
                Bytes::from_static(b"hello over h2")
            } else {
                Bytes::from(got)
            };
            let mut send = respond.send_response(Response::new(()), false).unwrap();
            send.send_data(reply, true).unwrap();
            answered.set(answered.get() + 1);
        });
    }
    answered.get()
}

/// Sends a request with `body` and returns the response body.
async fn request(client: &h2::client::SendRequest<Bytes>, path: &str, body: Bytes) -> Vec<u8> {
    let mut client = client.clone().ready().await.unwrap();
    let req = Request::post(format!("http://localhost{path}"))
        .body(())
        .unwrap();
    let (response, mut send) = client.send_request(req, body.is_empty()).unwrap();
    if !body.is_empty() {
        // Respect the peer's flow control window.
        let mut rest = body;
        while !rest.is_empty() {
            send.reserve_capacity(rest.len());
            let cap = std::future::poll_fn(|cx| send.poll_capacity(cx))
                .await
                .unwrap()
                .unwrap();
            let piece = rest.split_to(cap.min(rest.len()));
            send.send_data(piece, rest.is_empty()).unwrap();
        }
    }
    let response = response.await.unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut got = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body.flow_control().release_capacity(chunk.len()).unwrap();
        got.extend_from_slice(&chunk);
    }
    got
}

#[test]
fn h2_server_and_client_on_giong() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let server = giong::spawn(serve(Compat::new(s)));

        let (client, connection) = h2::client::handshake(Compat::new(c)).await.unwrap();
        // The connection is driven by a task of its own, as h2 asks.
        let driver = giong::spawn(async move { connection.await.unwrap() });

        assert_eq!(
            request(&client, "/hello", Bytes::new()).await,
            b"hello over h2"
        );
        // 100 streams at once on the one connection, with bodies that need
        // several flow control windows.
        let mut tasks = Vec::new();
        for i in 0..100usize {
            let client = client.clone();
            tasks.push(giong::spawn(async move {
                let body = Bytes::from(pattern(1000 + i * 1500));
                let back = request(&client, "/echo", body.clone()).await;
                assert!(back[..] == body[..], "stream {i}");
            }));
        }
        for task in tasks {
            timeout(LIMIT, task).await.unwrap().unwrap();
        }
        drop(client);
        timeout(LIMIT, driver).await.unwrap().unwrap();
        assert_eq!(timeout(LIMIT, server).await.unwrap().unwrap(), 101);
    });
}

#[cfg(feature = "tls")]
#[test]
fn h2_over_giong_tls_with_alpn() {
    giong::block_on(async {
        let pki = pki();
        let (c, s) = pair().await;
        let acceptor = giong_tls::TlsAcceptor::new(server_config(&pki, &[b"h2", b"http/1.1"]));
        let server = giong::spawn(async move {
            let tls = acceptor.accept(s).await.unwrap();
            assert_eq!(tls.alpn_protocol().as_deref(), Some(&b"h2"[..]));
            serve(Compat::new(tls)).await
        });
        let connector = giong_tls::TlsConnector::new(client_config(&pki, &[b"h2"]));
        let tls = connector.connect("localhost", c).await.unwrap();
        let (client, connection) = h2::client::handshake(Compat::new(tls)).await.unwrap();
        let driver = giong::spawn(async move { connection.await.unwrap() });
        let body = Bytes::from(pattern(200_000));
        assert!(request(&client, "/echo", body.clone()).await[..] == body[..]);
        drop(client);
        timeout(LIMIT, driver).await.unwrap().unwrap();
        assert_eq!(timeout(LIMIT, server).await.unwrap().unwrap(), 1);
    });
}
