//! Calls that do not go well: cancelled, late, panicking, malformed. The
//! server's handlers must stop when nobody listens, and each failure must
//! reach the other side as the status gRPC specifies.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use common::*;
#[path = "../../giong-http/tests/support/quiet_panics.rs"]
mod quiet_panics;
use giong::time::{sleep, timeout};
use giong_grpc::{CallOptions, Channel, Code, Reply, Request, RequestStream, Sink, Status};
use giong_http::client::Client;

/// A test service whose handlers wait until they are told to go on, and
/// record what happens to them.
#[derive(Clone, Default)]
struct Watched {
    log: Rc<RefCell<Vec<String>>>,
}

impl Watched {
    fn record(&self, what: &str) {
        self.log.borrow_mut().push(what.to_owned());
    }

    fn seen(&self, what: &str) -> bool {
        self.log.borrow().iter().any(|l| l == what)
    }

    async fn eventually(&self, what: &str) {
        for _ in 0..1000 {
            if self.seen(what) {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("never saw {what:?}; saw {:?}", self.log.borrow());
    }
}

/// Records `0` when it is dropped.
struct OnDrop(Watched, &'static str);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.record(self.1);
    }
}

impl TestService for Watched {
    async fn empty_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        panic!("as the test asks (expected by this test)");
    }

    /// Never answers.
    async fn unary_call(
        &self,
        _request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        let _guard = OnDrop(self.clone(), "unary dropped");
        self.record("unary started");
        sleep(Duration::from_secs(3600)).await;
        unreachable!()
    }

    async fn cacheable_unary_call(
        &self,
        _request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    /// Sends one message, then never ends. With a payload in the request,
    /// sends nothing at all.
    async fn streaming_output_call(
        &self,
        request: Request<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let _guard = OnDrop(self.clone(), "output dropped");
        if request.message()?.payload().is_none() {
            responses
                .send(&StreamingOutputCallResponse::default())
                .await?;
        }
        self.record("output started");
        sleep(Duration::from_secs(3600)).await;
        unreachable!()
    }

    /// Reads requests until they end or fail, and says which.
    async fn streaming_input_call(
        &self,
        mut requests: RequestStream<StreamingInputCallRequest<'static>>,
    ) -> Result<Reply<StreamingInputCallResponse<'static>>, Status> {
        let _guard = OnDrop(self.clone(), "input dropped");
        self.record("input started");
        loop {
            match requests.message().await {
                Ok(Some(_)) => self.record("input message"),
                Ok(None) => break,
                Err(status) => {
                    self.record(&format!("input failed: {}", status.code()));
                    return Err(status);
                }
            }
        }
        Reply::new(&StreamingInputCallResponse::default())
    }

    /// Echoes one response for each request; panics on a request with a
    /// status in it.
    async fn full_duplex_call(
        &self,
        mut requests: RequestStream<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let _guard = OnDrop(self.clone(), "duplex dropped");
        self.record("duplex started");
        loop {
            match requests.message().await {
                Ok(Some(received)) => {
                    if received.message()?.response_status().is_some() {
                        panic!("as the test asks (expected by this test)");
                    }
                    responses
                        .send(&StreamingOutputCallResponse::default())
                        .await?;
                }
                Ok(None) => return Ok(()),
                Err(status) => {
                    self.record(&format!("duplex failed: {}", status.code()));
                    return Err(status);
                }
            }
        }
    }

    /// Hands its sink to another task and returns: the task's sends fail
    /// from then on.
    async fn half_duplex_call(
        &self,
        _requests: RequestStream<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        responses
            .send(&StreamingOutputCallResponse::default())
            .await?;
        let watched = self.clone();
        giong::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            match responses
                .send(&StreamingOutputCallResponse::default())
                .await
            {
                Ok(()) => watched.record("stray send worked"),
                Err(status) => watched.record(&format!("stray send: {}", status.code())),
            }
            assert!(responses.is_closed());
        });
        Ok(())
    }

    async fn unimplemented_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }
}

fn watched() -> (Watched, TestServiceClient, String) {
    quiet_panics::quiet_expected_panics();
    let watched = Watched::default();
    let base = serve(giong_grpc::Server::new().add(TestServiceServer::new(watched.clone())));
    let client = TestServiceClient::new(Channel::new(&base).unwrap());
    (watched, client, base)
}

#[test]
fn a_cancelled_unary_call_drops_its_handler() {
    giong::block_on(async {
        let (watched, client, _) = watched();
        let request = SimpleRequest::default();
        let call = client.unary_call(&request, CallOptions::new());
        // Dropped while the handler waits.
        assert!(timeout(Duration::from_millis(100), call).await.is_err());
        assert!(watched.seen("unary started"));
        watched.eventually("unary dropped").await;
    });
}

#[test]
fn a_dropped_response_stream_drops_its_handler() {
    giong::block_on(async {
        let (watched, client, _) = watched();
        // After the first response.
        let mut responses = client
            .streaming_output_call(&StreamingOutputCallRequest::default(), CallOptions::new())
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_some());
        watched.eventually("output started").await;
        assert!(!watched.seen("output dropped"));
        drop(responses);
        watched.eventually("output dropped").await;

        // Before any response: the head has not been sent yet.
        watched.log.borrow_mut().clear();
        let payload = Payload::default();
        let request = StreamingOutputCallRequest {
            payload: Some(&payload),
            ..Default::default()
        };
        let call = client.streaming_output_call(&request, CallOptions::new());
        assert!(timeout(Duration::from_millis(100), call).await.is_err());
        assert!(watched.seen("output started"));
        watched.eventually("output dropped").await;
    });
}

#[test]
fn cancelling_the_requests_ends_the_handler() {
    giong::block_on(async {
        // Client streaming: cancel_after_begin.
        let (watched, client, _) = watched();
        let mut call = client.streaming_input_call(CallOptions::new());
        call.send(&StreamingInputCallRequest::default())
            .await
            .unwrap();
        watched.eventually("input message").await;
        call.cancel();
        // The handler runs inside the HTTP request, which is dropped.
        watched.eventually("input dropped").await;

        // Bidirectional: cancel_after_first_response.
        let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
        requests
            .send(&StreamingOutputCallRequest::default())
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_some());
        requests.cancel();
        watched.eventually("duplex dropped").await;
        let status = responses.message().await.unwrap_err();
        assert!(
            matches!(status.code(), Code::Cancelled | Code::Unavailable),
            "{status}"
        );
    });
}

#[test]
fn a_handler_that_panics_ends_its_call_with_internal() {
    giong::block_on(async {
        let (_, client, _) = watched();
        let status = client
            .empty_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Internal, "{status}");
        // In a streaming call, after a response.
        let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
        requests
            .send(&StreamingOutputCallRequest::default())
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_some());
        let asked = EchoStatus::default();
        requests
            .send(&StreamingOutputCallRequest {
                response_status: Some(&asked),
                ..Default::default()
            })
            .await
            .unwrap();
        let status = responses.message().await.unwrap_err();
        assert_eq!(status.code(), Code::Internal, "{status}");
        // The server goes on.
        let status = client
            .unimplemented_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unimplemented);
    });
}

#[test]
fn a_sink_that_outlives_its_handler_cannot_send() {
    giong::block_on(async {
        let (watched, client, _) = watched();
        let (requests, mut responses) = client.half_duplex_call(CallOptions::new());
        requests.finish();
        assert!(responses.message().await.unwrap().is_some());
        assert!(responses.message().await.unwrap().is_none());
        watched.eventually("stray send: CANCELLED").await;
    });
}

// ------------------------------------------- the wire, written by hand

fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// One gRPC request made with the HTTP client: the status, the fields of
/// the response's head and trailers, and its body.
async fn raw(
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let client = Client::builder().http2_prior_knowledge(true).build();
    let mut request = client.post(&format!("{base}{path}")).body(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let mut response = request.send_streaming().await.unwrap();
    let mut fields: Vec<(String, String)> = response
        .headers()
        .map(|(n, v)| (n.to_owned(), String::from_utf8_lossy(v).into_owned()))
        .collect();
    let mut body = Vec::new();
    while let Some(piece) = response.chunk().await.unwrap() {
        body.extend_from_slice(&piece);
    }
    if let Some(trailers) = response.trailers() {
        fields.extend(
            trailers
                .iter()
                .map(|(n, v)| (n.to_owned(), String::from_utf8_lossy(v).into_owned())),
        );
    }
    (response.status(), fields, body)
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(n, _)| n == name).map(|(_, v)| &**v)
}

const GRPC: (&str, &str) = ("content-type", "application/grpc");

#[test]
fn the_server_enforces_the_clients_deadline() {
    giong::block_on(async {
        let (watched, _, base) = watched();
        let started = Instant::now();
        let (status, fields, body) = raw(
            &base,
            "/grpc.testing.TestService/UnaryCall",
            &[GRPC, ("grpc-timeout", "100m")],
            frame(b""),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(field(&fields, "grpc-status"), Some("4"), "{fields:?}");
        assert!(field(&fields, "grpc-message")
            .unwrap()
            .starts_with("GIONG-E4108"));
        assert!(body.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(watched.seen("unary dropped"));

        // A streaming handler too, after its first message.
        let (_, fields, body) = raw(
            &base,
            "/grpc.testing.TestService/StreamingOutputCall",
            &[GRPC, ("grpc-timeout", "100m")],
            frame(b""),
        )
        .await;
        assert_eq!(body, frame(b""));
        assert_eq!(field(&fields, "grpc-status"), Some("4"), "{fields:?}");
        assert!(watched.seen("output dropped"));
    });
}

#[test]
fn requests_that_are_not_grpc_are_answered_as_the_protocol_says() {
    giong::block_on(async {
        let (_, _, base) = watched();
        let unary = "/grpc.testing.TestService/CacheableUnaryCall";
        // Not gRPC at all.
        let (status, _, body) = raw(
            &base,
            unary,
            &[("content-type", "application/json")],
            vec![],
        )
        .await;
        assert_eq!(status, 415);
        assert!(String::from_utf8_lossy(&body).contains("gRPC"));
        // Every one of these is a gRPC response: 200, the status in the
        // head, no body.
        for (path, headers, body, code, giong) in [
            (
                "/grpc.testing.Nope/Call",
                vec![GRPC],
                frame(b""),
                "12",
                "GIONG-E4110",
            ),
            (
                "/grpc.testing.TestService/Nope",
                vec![GRPC],
                frame(b""),
                "12",
                "GIONG-E4110",
            ),
            ("/nope", vec![GRPC], frame(b""), "12", "GIONG-E4110"),
            (
                unary,
                vec![GRPC, ("grpc-timeout", "soon")],
                frame(b""),
                "13",
                "GIONG-E4103",
            ),
            (
                unary,
                vec![GRPC, ("grpc-encoding", "snappy")],
                frame(b""),
                "12",
                "GIONG-E4104",
            ),
            (
                unary,
                vec![GRPC, ("x-key-bin", "not base64!")],
                frame(b""),
                "13",
                "GIONG-E4103",
            ),
            // No message, two messages, a bad flag, a message cut short, a
            // compressed flag without an encoding.
            (unary, vec![GRPC], vec![], "12", "GIONG-E4103"),
            (
                unary,
                vec![GRPC],
                [frame(b""), frame(b"")].concat(),
                "12",
                "GIONG-E4103",
            ),
            (unary, vec![GRPC], vec![7, 0, 0, 0, 0], "13", "GIONG-E4103"),
            (
                unary,
                vec![GRPC],
                vec![0, 0, 0, 0, 9, 1, 2],
                "13",
                "GIONG-E4103",
            ),
            (
                unary,
                vec![GRPC],
                vec![1, 0, 0, 0, 1, 0],
                "13",
                "GIONG-E4103",
            ),
            // Not a SimpleRequest: field 2 is a varint that never ends.
            (
                "/grpc.testing.TestService/UnaryCallX",
                vec![GRPC],
                frame(b""),
                "12",
                "GIONG-E4110",
            ),
        ] {
            let (status, fields, answer) = raw(&base, path, &headers, body.clone()).await;
            assert_eq!(status, 200, "{path} {headers:?}");
            assert_eq!(field(&fields, "content-type"), Some("application/grpc"));
            assert_eq!(
                field(&fields, "grpc-status"),
                Some(code),
                "{path} {headers:?} {body:?}: {fields:?}"
            );
            let message = field(&fields, "grpc-message").unwrap_or_default();
            assert!(message.starts_with(giong), "{path} {headers:?}: {message}");
            assert!(answer.is_empty());
        }
    });
}

#[test]
fn a_message_that_is_not_its_type_is_internal() {
    giong::block_on(async {
        // The interop service decodes its request.
        let base = serve(test_server());
        let (_, fields, _) = raw(
            &base,
            "/grpc.testing.TestService/UnaryCall",
            &[GRPC],
            frame(&[
                0x10, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ]),
        )
        .await;
        assert_eq!(field(&fields, "grpc-status"), Some("13"), "{fields:?}");
        let message = field(&fields, "grpc-message").unwrap();
        assert!(message.starts_with("GIONG-E4105"), "{message}");
        assert!(message.contains("GIONG-E4002"), "{message}");
    });
}

#[test]
fn a_client_that_stops_mid_stream_is_seen_as_cancelled() {
    giong::block_on(async {
        let (watched, _, base) = watched();
        let client = Client::builder().http2_prior_knowledge(true).build();
        let (request, body) = client
            .post(&format!("{base}/grpc.testing.TestService/FullDuplexCall"))
            .header("content-type", "application/grpc")
            .streamed_body();
        // The service answers once it has a request.
        body.send(frame(b"")).await.unwrap();
        let mut response = request.send_streaming().await.unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap(), frame(b""));
        // Half a message, then a reset.
        body.send(vec![0, 0, 0, 0, 9, 1]).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        body.abort();
        watched.eventually("duplex dropped").await;
        assert!(response.chunk().await.is_err());
    });
}
