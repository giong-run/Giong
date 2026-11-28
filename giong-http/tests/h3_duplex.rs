//! Streamed request bodies over HTTP/3: giong-http's client against
//! giong-http's `Server::serve_h3`, on one giong worker. Trailer fields
//! both ways, a response read while its request is still being written,
//! a body the producer aborts, a server that answers without reading, and
//! a response given up while its request is still being written.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use giong::time::{sleep, timeout};
use giong_http::client::{Client, ErrorKind};
use giong_http::{Body, Handler, Request, Response, Server, Trailers, Version};
use giong_quic::{ClientConfig, Endpoint, ServerConfig};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"h3".to_vec()];
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h3".to_vec()];
    (server, client)
}

#[derive(Clone, Default)]
struct App {
    /// What handlers saw, for the tests to read.
    log: Rc<RefCell<Vec<String>>>,
}

impl App {
    fn record(&self, what: impl Into<String>) {
        self.log.borrow_mut().push(what.into());
    }

    fn seen(&self, prefix: &str) -> bool {
        self.log.borrow().iter().any(|l| l.starts_with(prefix))
    }
}

impl Handler for App {
    fn stream_body(&self, req: &Request) -> Option<u64> {
        (req.path() != "/whole").then_some(1 << 20)
    }

    async fn call(&self, req: Request) -> Response {
        assert_eq!(req.version(), Version::Http3);
        match req.path() {
            // Reads the body, then answers with its size in the body, and
            // its size and the request's trailers in trailers.
            "/sum" => {
                // A request whose stream ended with its head has no body to
                // stream (as over HTTP/2): its end may arrive in the same
                // packet when the producer is done before it is sent.
                let Some(body) = req.body_stream() else {
                    return Response::text("0 bytes");
                };
                self.record("sum started");
                // A stream the client resets may drop its handler.
                let mut cancelled = Cancelled(Some(self.clone()));
                let mut size = 0;
                loop {
                    match body.chunk().await {
                        Ok(Some(piece)) => size += piece.len(),
                        Ok(None) => break,
                        Err(e) => {
                            cancelled.0 = None;
                            self.record(format!("sum failed: {e}"));
                            return Response::new(400).body(e.to_string());
                        }
                    }
                }
                cancelled.0 = None;
                let mut trailers = Trailers::new().field("x-size", size.to_string()).unwrap();
                if let Some(sent) = body.trailers() {
                    for (name, value) in sent.iter() {
                        trailers = trailers.field(&format!("x-echo-{name}"), value).unwrap();
                    }
                }
                let (sender, out) = Body::channel();
                giong::spawn(async move {
                    sender.send(format!("{size} bytes")).await.unwrap();
                    sender.send_trailers(trailers).unwrap();
                });
                Response::new(200).body(out)
            }
            // Answers at once, then sends every piece back in upper case as
            // it arrives: both directions at the same time.
            "/shout" => {
                let body = req.body_stream().expect("a streamed body");
                let (sender, out) = Body::channel();
                giong::spawn(async move {
                    while let Ok(Some(piece)) = body.chunk().await {
                        if sender.send(piece.to_ascii_uppercase()).await.is_err() {
                            return;
                        }
                    }
                });
                Response::new(200).body(out)
            }
            // Answers at once with a body that never comes, and goes on
            // reading the request: the client gives up on the response.
            "/hold" => {
                let body = req.body_stream().expect("a streamed body");
                let app = self.clone();
                let (sender, out) = Body::channel();
                giong::spawn(async move {
                    let _never = sender;
                    loop {
                        match body.chunk().await {
                            Ok(Some(piece)) => app.record(format!("hold got {}", piece.len())),
                            Ok(None) => return app.record("hold ended"),
                            Err(e) => return app.record(format!("hold cut: {e}")),
                        }
                    }
                });
                Response::new(200).body(out)
            }
            // Answers without reading the body.
            "/refuse" => Response::new(403).body("no"),
            "/whole" => Response::text(format!("whole {}", req.body().len())),
            _ => Response::new(404),
        }
    }
}

/// Records that a handler was dropped before it finished.
struct Cancelled(Option<App>);

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(app) = self.0.take() {
            app.record("sum cancelled");
        }
    }
}

/// An HTTP/3 server for `app` on this worker, and a client that speaks
/// HTTP/3 to it by prior knowledge.
fn serve(app: App) -> (String, Client) {
    let (server_tls, client_tls) = tls();
    let endpoint = Endpoint::server(
        "127.0.0.1:0".parse().unwrap(),
        ServerConfig::new(server_tls).unwrap(),
    )
    .unwrap();
    let addr: SocketAddr = endpoint.local_addr().unwrap();
    giong::spawn(async move { Server::new().serve_h3(endpoint, app).await });
    let client = Client::builder()
        .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
        .timeout(Duration::from_secs(10))
        .build();
    // An address, not `localhost`: Windows resolves that to ::1 first,
    // where nothing listens, and the client gives each address its share
    // of the connect timeout.
    (format!("https://{addr}"), client)
}

fn run<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F) {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(test())
        .unwrap();
}

async fn read_all(response: &mut giong_http::client::StreamingResponse) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(piece) = response.chunk().await.unwrap() {
        all.extend_from_slice(&piece);
    }
    all
}

#[test]
fn trailers_travel_both_ways_after_streamed_bodies() {
    run(|| async {
        let (base, client) = serve(App::default());
        let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
        let upload = giong::spawn(async move {
            for piece in [&b"one"[..], b"", b"three", &[b'z'; 70_000]] {
                body.send(piece).await.unwrap();
            }
            body.send_trailers(
                Trailers::new()
                    .field("X-Checksum", "abc")
                    .unwrap()
                    .field("x-bin", [0xfeu8, 0xff])
                    .unwrap(),
            )
            .unwrap();
        });
        let mut response = request.send_streaming().await.unwrap();
        upload.await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), Version::Http3);
        assert_eq!(read_all(&mut response).await, b"70008 bytes");
        let trailers = response.trailers().expect("trailers").clone();
        assert_eq!(trailers.get_str("x-size"), Some("70008"));
        assert_eq!(trailers.get_str("x-echo-x-checksum"), Some("abc"));
        assert_eq!(trailers.get("x-echo-x-bin"), Some(&[0xfe, 0xff][..]));
        assert_eq!(trailers.len(), 3);

        // Without trailers: the body ends when its sender is dropped; and
        // an empty body.
        let (request, body) = client.put(&format!("{base}/sum")).streamed_body();
        body.send("hello").await.unwrap();
        drop(body);
        let response = request.send().await.unwrap();
        assert_eq!(
            (response.version(), response.body()),
            (Version::Http3, &b"5 bytes"[..])
        );
        let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
        drop(body);
        assert_eq!(request.send().await.unwrap().body(), b"0 bytes");

        // The connection carries whole bodies as before.
        let again = client
            .post(&format!("{base}/whole"))
            .body("abc")
            .send()
            .await;
        assert_eq!(again.unwrap().body(), b"whole 3");
    });
}

#[test]
fn the_response_is_read_while_the_request_is_written() {
    run(|| async {
        let (base, client) = serve(App::default());
        let (request, body) = client.post(&format!("{base}/shout")).streamed_body();
        body.send("first").await.unwrap();
        // The head of the response comes before the body is over.
        let mut response = timeout(Duration::from_secs(5), request.send_streaming())
            .await
            .expect("the response before the end of the request")
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut echoed = Vec::new();
        while echoed.len() < 5 {
            echoed.extend_from_slice(&response.chunk().await.unwrap().unwrap());
        }
        assert_eq!(echoed, b"FIRST");
        // Each piece comes back before the next one is sent.
        for word in ["second", "third"] {
            body.send(word).await.unwrap();
            let piece = response.chunk().await.unwrap().unwrap();
            assert_eq!(piece, word.to_ascii_uppercase().as_bytes());
        }
        drop(body);
        assert_eq!(read_all(&mut response).await, b"");
    });
}

#[test]
fn an_aborted_request_body_cancels_the_request() {
    run(|| async {
        let app = App::default();
        let (base, client) = serve(app.clone());
        let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
        let seen = app.clone();
        let producer = giong::spawn(async move {
            body.send("part").await.unwrap();
            // Aborted once the server has the request: an abort before the
            // head went out (the handshake is slow under TSan) cancels a
            // request the server never sees.
            for _ in 0..1000 {
                if seen.seen("sum started") {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            body.abort();
        });
        let err = request.send().await.unwrap_err();
        producer.await.unwrap();
        assert_eq!(err.kind(), ErrorKind::Closed, "{err}");
        assert!(err.to_string().contains("aborted"), "{err}");
        // The server saw its request cut short.
        let cut = || app.seen("sum failed") || app.seen("sum cancelled");
        for _ in 0..200 {
            if cut() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(cut(), "{:?}", app.log.borrow());
        // The connection carries the next request.
        let again = client
            .post(&format!("{base}/whole"))
            .body("ab")
            .send()
            .await;
        assert_eq!(again.unwrap().body(), b"whole 2");
    });
}

#[test]
fn a_server_that_answers_without_reading_gets_no_more_of_the_body() {
    run(|| async {
        let (base, client) = serve(App::default());
        let (request, body) = client.post(&format!("{base}/refuse")).streamed_body();
        body.send("start").await.unwrap();
        let response = timeout(Duration::from_secs(5), request.send())
            .await
            .expect("an answer although the body never ends")
            .unwrap();
        assert_eq!((response.status(), response.body()), (403, &b"no"[..]));
        // The producer learns that nothing more is taken.
        for _ in 0..200 {
            if body.is_closed() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(body.is_closed(), "the producer was not told");
    });
}

/// Polls `done` for up to a second.
async fn eventually(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        sleep(Duration::from_millis(5)).await;
    }
    done()
}

#[test]
fn a_response_given_up_mid_upload_ends_the_upload() {
    run(|| async {
        let app = App::default();
        let (base, client) = serve(app.clone());
        let (request, body) = client.post(&format!("{base}/hold")).streamed_body();
        body.send("first").await.unwrap();
        let response = timeout(Duration::from_secs(5), request.send_streaming())
            .await
            .expect("the response before the end of the request")
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            eventually(|| app.seen("hold got")).await,
            "{:?}",
            app.log.borrow()
        );
        // Given up (as a request timeout would), with the body still open.
        drop(response);
        // The server sees its request cut short, not ended...
        assert!(
            eventually(|| app.seen("hold cut")).await,
            "{:?}",
            app.log.borrow()
        );
        assert!(!app.seen("hold ended"), "{:?}", app.log.borrow());
        // ...and the producer's next send fails: nothing more goes out.
        let mut refused = false;
        for _ in 0..200 {
            if body.send("more").await.is_err() {
                refused = true;
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(refused, "the producer was not told: {:?}", app.log.borrow());
        // The connection carries the next request.
        let again = client
            .post(&format!("{base}/whole"))
            .body("ab")
            .send()
            .await;
        assert_eq!(again.unwrap().body(), b"whole 2");
    });
}
