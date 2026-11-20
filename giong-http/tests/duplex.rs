//! Trailer fields and streamed request bodies, end to end: giong-http's
//! client against giong-http's server over HTTP/1.1 and HTTP/2, on one
//! giong worker. Raw sockets where the bytes on the wire are the point.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};
use giong_http::client::{Client, ErrorKind};
use giong_http::{Body, Handler, Request, Response, Server, Trailers, Version};

// ------------------------------------------------------------------ harness

#[derive(Clone, Default)]
struct App {
    /// What handlers saw, for the tests to read.
    log: Rc<RefCell<Vec<String>>>,
}

impl App {
    fn record(&self, what: impl Into<String>) {
        self.log.borrow_mut().push(what.into());
    }

    fn seen(&self, what: &str) -> bool {
        self.log.borrow().iter().any(|l| l == what)
    }
}

impl Handler for App {
    fn stream_body(&self, req: &Request) -> Option<u64> {
        (req.path() != "/whole").then_some(1 << 20)
    }

    async fn call(&self, req: Request) -> Response {
        match req.path() {
            // Reads the body, then answers with its size in the body, and
            // its size and the request's trailers in trailers.
            "/sum" => {
                let body = req.body_stream().expect("a streamed body");
                assert!(body.trailers().is_none(), "no trailers before the end");
                // An HTTP/2 stream the client resets drops its handler.
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
                let app = self.clone();
                giong::spawn(async move {
                    loop {
                        match body.chunk().await {
                            Ok(Some(piece)) => {
                                if sender.send(piece.to_ascii_uppercase()).await.is_err() {
                                    app.record("shout: the client went away");
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                app.record(format!("shout failed: {e}"));
                                sender.abort();
                                return;
                            }
                        }
                    }
                    let _ = sender.send_trailers(Trailers::new().field("x-done", "yes").unwrap());
                });
                Response::new(200).body(out)
            }
            // Answers without reading the body.
            "/refuse" => Response::new(403).body("no"),
            "/moved" => Response::new(307).header("location", "/sum"),
            "/whole" => Response::text(format!("whole {}", req.body().len())),
            // A body in memory with trailers has no way to be written: the
            // trailers of a response to HEAD are not sent either.
            "/large" => {
                let (sender, out) = Body::channel();
                giong::spawn(async move {
                    for _ in 0..8 {
                        if sender.send(vec![b'x'; 100_000]).await.is_err() {
                            return;
                        }
                    }
                    let big = "v".repeat(3000);
                    let mut trailers = Trailers::new();
                    for i in 0..10 {
                        trailers = trailers.field(&format!("x-big-{i}"), &big).unwrap();
                    }
                    let _ = sender.send_trailers(trailers);
                });
                Response::new(200).body(out)
            }
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

fn serve(app: App) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move { Server::new().serve(listener, app).await });
    format!("http://{addr}")
}

fn clients() -> [(Version, Client); 2] {
    [
        (Version::Http11, Client::new()),
        (
            Version::Http2,
            Client::builder().http2_prior_knowledge(true).build(),
        ),
    ]
}

async fn read_all(response: &mut giong_http::client::StreamingResponse) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(piece) = response.chunk().await.unwrap() {
        all.extend_from_slice(&piece);
    }
    all
}

async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        sleep(Duration::from_millis(5)).await;
    }
    panic!("{what}");
}

// ------------------------------------------------------------------- tests

#[test]
fn trailers_travel_both_ways_after_streamed_bodies() {
    giong::block_on(async {
        let base = serve(App::default());
        for (version, client) in clients() {
            let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
            let upload = giong::spawn(async move {
                for piece in [&b"one"[..], b"", b"three", &[b'z'; 70_000]] {
                    body.send(piece).await.unwrap();
                }
                assert!(!body.is_closed());
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
            assert_eq!(response.version(), version);
            assert!(response.trailers().is_none(), "no trailers before the end");
            assert_eq!(read_all(&mut response).await, b"70008 bytes");
            let trailers = response.trailers().expect("trailers").clone();
            assert_eq!(trailers.get_str("x-size"), Some("70008"), "{version:?}");
            assert_eq!(trailers.get_str("x-echo-x-checksum"), Some("abc"));
            assert_eq!(trailers.get("x-echo-x-bin"), Some(&[0xfe, 0xff][..]));
            assert_eq!(trailers.len(), 3);
            // The connection carries the next request.
            let again = client
                .post(&format!("{base}/whole"))
                .body("abc")
                .send()
                .await;
            assert_eq!(again.unwrap().body(), b"whole 3");
        }
    });
}

#[test]
fn a_streamed_body_without_trailers_ends_when_its_sender_is_dropped() {
    giong::block_on(async {
        let base = serve(App::default());
        for (_, client) in clients() {
            let (request, body) = client.put(&format!("{base}/sum")).streamed_body();
            // Written from the same task, before the request is polled:
            // one piece waits in the pipe.
            body.send("hello").await.unwrap();
            drop(body);
            let mut response = request.send_streaming().await.unwrap();
            assert_eq!(read_all(&mut response).await, b"5 bytes");
            let trailers = response.trailers().expect("the server's trailers");
            assert_eq!(trailers.len(), 1);
            // An empty body too.
            let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
            drop(body);
            let response = request.send().await.unwrap();
            assert_eq!(response.body(), b"0 bytes");
        }
    });
}

#[test]
fn requests_and_responses_alternate_on_one_http2_stream() {
    giong::block_on(async {
        let app = App::default();
        let base = serve(app.clone());
        let client = Client::builder().http2_prior_knowledge(true).build();
        let (request, body) = client.post(&format!("{base}/shout")).streamed_body();
        // The head of the response arrives before any of the body is sent.
        let mut response = timeout(Duration::from_secs(5), request.send_streaming())
            .await
            .expect("the response head arrives while the request is open")
            .unwrap();
        assert_eq!(response.status(), 200);
        for word in ["alpha", "beta", "gamma"] {
            body.send(word).await.unwrap();
            let piece = response.chunk().await.unwrap().expect("an answer");
            assert_eq!(piece, word.to_ascii_uppercase().into_bytes());
        }
        // Larger than the flow-control window, so it goes out in turns.
        let big = vec![b'q'; 300_000];
        let sending = {
            let big = big.clone();
            giong::spawn(async move {
                body.send(big).await.unwrap();
                drop(body);
            })
        };
        let mut all = Vec::new();
        while let Some(piece) = response.chunk().await.unwrap() {
            all.extend_from_slice(&piece);
        }
        sending.await.unwrap();
        assert_eq!(all, big.to_ascii_uppercase());
        assert_eq!(response.trailers().unwrap().get_str("x-done"), Some("yes"));
    });
}

#[test]
fn an_aborted_request_body_cancels_the_request() {
    giong::block_on(async {
        for (version, client) in clients() {
            let app = App::default();
            let base = serve(app.clone());
            let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
            let upload = giong::spawn(async move {
                body.send("partial").await.unwrap();
                sleep(Duration::from_millis(20)).await;
                body.abort();
            });
            let error = request.send().await.unwrap_err();
            upload.await.unwrap();
            assert_eq!(error.kind(), ErrorKind::Closed, "{version:?}: {error}");
            assert!(error.to_string().contains("aborted"), "{error}");
            eventually(
                &format!("{version:?}: the handler sees the body cut short"),
                || {
                    // Over HTTP/1.1 the body ends with an error; over HTTP/2
                    // the reset stream's handler is dropped.
                    app.log.borrow().iter().any(|l| l.contains("GIONG-E1112"))
                        || (version == Version::Http2 && app.seen("sum cancelled"))
                },
            )
            .await;
        }
    });
}

#[test]
fn the_sender_is_told_when_the_request_is_over() {
    giong::block_on(async {
        let base = serve(App::default());
        for (version, client) in clients() {
            // The server answers without reading the body.
            let (request, body) = client.post(&format!("{base}/refuse")).streamed_body();
            let upload = giong::spawn(async move {
                let mut sent = 0;
                loop {
                    if let Err(e) = body.send(vec![0u8; 10_000]).await {
                        assert_eq!(e.kind(), ErrorKind::Closed);
                        assert!(body.is_closed());
                        assert!(body.send_trailers(Trailers::new()).is_err());
                        return sent;
                    }
                    sent += 1;
                    if version == Version::Http11 && sent == 3 {
                        // Over HTTP/1.1 the answer is read after the body.
                        return sent;
                    }
                    assert!(sent < 10_000, "the sender is never told");
                }
            });
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 403, "{version:?}");
            assert_eq!(response.body(), b"no");
            timeout(Duration::from_secs(5), upload)
                .await
                .unwrap()
                .unwrap();

            // The request is dropped before it is sent.
            let (request, body) = client.post(&format!("{base}/sum")).streamed_body();
            drop(request);
            assert!(body.is_closed());
            assert!(body.send("x").await.is_err());
        }
    });
}

#[test]
fn a_streamed_body_is_not_sent_again_after_a_redirect() {
    giong::block_on(async {
        let base = serve(App::default());
        for (_, client) in clients() {
            let (request, body) = client.post(&format!("{base}/moved")).streamed_body();
            drop(body);
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 307);
            assert_eq!(response.header("location"), Some("/sum"));
        }
    });
}

#[test]
fn a_request_has_one_body() {
    giong::block_on(async {
        let client = Client::new();
        let (request, _body) = client.post("http://127.0.0.1:1/").body("a").streamed_body();
        let error = request.send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Request);
        let (request, _body) = client.post("http://127.0.0.1:1/").streamed_body();
        let error = request.json("{}").send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Request);
        let (request, _first) = client.post("http://127.0.0.1:1/").streamed_body();
        let (request, _second) = request.streamed_body();
        let error = request.send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Request);
    });
}

#[test]
fn large_trailers_span_continuation_frames() {
    giong::block_on(async {
        let base = serve(App::default());
        for (version, client) in clients() {
            let mut response = client
                .get(&format!("{base}/large"))
                .send_streaming()
                .await
                .unwrap();
            if version == Version::Http11 {
                // 30 KB of trailers is past the 16 KiB a chunked body may
                // end with: the client refuses the response.
                let error = loop {
                    match response.chunk().await {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("trailers past the limit were accepted"),
                        Err(e) => break e,
                    }
                };
                assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
                assert!(response.trailers().is_none());
                continue;
            }
            let body = read_all(&mut response).await;
            assert_eq!(body.len(), 800_000);
            let trailers = response.trailers().expect("trailers");
            assert_eq!(trailers.len(), 10);
            assert_eq!(trailers.get("x-big-9").map(<[u8]>::len), Some(3000));
        }
    });
}

/// Sends `request` on a new connection and returns everything the server
/// answers until it closes or goes quiet.
async fn raw(base: &str, request: &[u8]) -> String {
    let addr = base.trim_start_matches("http://");
    let stream = TcpStream::connect(addr).await.unwrap();
    let (result, _) = stream.write_all(request.to_vec()).await;
    result.unwrap();
    let mut all = Vec::new();
    while let Ok(Ok(chunk)) = timeout(Duration::from_millis(300), stream.recv()).await {
        if chunk.is_empty() {
            break;
        }
        all.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&all).into_owned()
}

#[test]
fn chunked_trailers_on_the_wire() {
    giong::block_on(async {
        let app = App::default();
        let base = serve(app.clone());
        // Request trailers, with one that means nothing after a body, and
        // the response's trailers after its last chunk.
        let answer = raw(
            &base,
            b"POST /sum HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
              3\r\nabc\r\n0\r\nX-Sum:  6 \r\nContent-Length: 99\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(
            answer.ends_with("7\r\n3 bytes\r\n0\r\nx-size: 3\r\nx-echo-x-sum: 6\r\n\r\n"),
            "{answer}"
        );
        // An HTTP/1.0 client gets the body, not the trailers.
        let answer = raw(&base, b"POST /sum HTTP/1.0\r\nContent-Length: 2\r\n\r\nhi").await;
        assert!(answer.ends_with("\r\n\r\n2 bytes"), "{answer}");
        // A trailer line that is not a field, and more trailers than the
        // limit, fail the body.
        let answer = raw(
            &base,
            b"POST /sum HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
              0\r\nno colon\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
        let mut request =
            b"POST /sum HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
        for i in 0..20 {
            request.extend_from_slice(format!("x-{i}: {}\r\n", "v".repeat(1000)).as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        let answer = raw(&base, &request).await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
        assert!(
            app.seen("sum failed: GIONG-E1113: malformed request body") || {
                app.log.borrow().iter().any(|l| l.contains("GIONG-E1113"))
            }
        );
    });
}

#[test]
fn a_producer_that_waits_is_told_when_the_client_resets_its_stream() {
    giong::block_on(async {
        let told = Rc::new(RefCell::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = {
            let told = told.clone();
            move |_req: Request| {
                let told = told.clone();
                async move {
                    let (sender, body) = Body::channel();
                    giong::spawn(async move {
                        sender.send("first").await.unwrap();
                        // Nothing more to send: only the client can end this.
                        sender.closed().await;
                        assert!(sender.is_closed());
                        *told.borrow_mut() = true;
                    });
                    Response::new(200).body(body)
                }
            }
        };
        giong::spawn(async move { Server::new().serve(listener, handler).await });
        // Over HTTP/1.1 a connection that is writing a streamed body does
        // not read, so a client that goes away is noticed at the next piece.
        for (version, client) in clients().into_iter().skip(1) {
            *told.borrow_mut() = false;
            let mut response = client
                .get(&format!("{base}/"))
                .send_streaming()
                .await
                .unwrap();
            assert_eq!(response.chunk().await.unwrap().unwrap(), b"first");
            sleep(Duration::from_millis(20)).await;
            assert!(!*told.borrow(), "{version:?}: told too early");
            // Resets the HTTP/2 stream.
            drop(response);
            eventually("the producer is told", || *told.borrow()).await;
        }
    });
}

#[test]
fn trailers_are_not_sent_once_the_client_is_gone() {
    giong::block_on(async {
        let (sender, body) = Body::channel();
        drop(body);
        assert!(sender.is_closed());
        let error = sender
            .send_trailers(Trailers::new().field("x-a", "1").unwrap())
            .unwrap_err();
        assert!(error.to_string().starts_with("GIONG-E1114"), "{error}");
    });
}
