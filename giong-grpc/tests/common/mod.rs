//! The service of gRPC's interoperability tests, implemented as their
//! specification describes it
//! (github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md), and
//! helpers to serve it. Shared by the tests and the interop programs.

#![allow(dead_code, unused_imports, clippy::all)]

use std::time::Duration;

use giong::net::TcpListener;
use giong_grpc::{Code, Metadata, Reply, Request, RequestStream, Sink, Status};

#[allow(missing_docs, unused, unreachable_pub)]
pub mod pb {
    include!("../generated/grpc_testing.rs");
}

pub use pb::grpc::testing::test_service::{TestService, TestServiceClient, TestServiceServer};
pub use pb::grpc::testing::*;

const ECHO_INITIAL: &str = "x-grpc-test-echo-initial";
const ECHO_TRAILING: &str = "x-grpc-test-echo-trailing-bin";

/// The test service.
#[derive(Clone, Default)]
pub struct Interop;

/// The metadata the tests ask to be echoed: before the response, after it.
fn echoes(metadata: &Metadata) -> (Metadata, Metadata) {
    let (mut initial, mut trailing) = (Metadata::new(), Metadata::new());
    for value in metadata.get_all(ECHO_INITIAL) {
        initial.add(ECHO_INITIAL, value).unwrap();
    }
    for value in metadata.get_all(ECHO_TRAILING) {
        trailing.add(ECHO_TRAILING, value).unwrap();
    }
    (initial, trailing)
}

fn asked_status(status: Option<&EchoStatusView<'_>>) -> Result<(), Status> {
    match status {
        Some(status) if status.code() != 0 => Err(Status::new(
            Code::from_number(status.code() as u32),
            status.message(),
        )),
        _ => Ok(()),
    }
}

/// A payload of `size` zero bytes.
fn zeros(size: i32) -> Vec<u8> {
    vec![0; size.max(0) as usize]
}

impl TestService for Interop {
    async fn empty_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        Reply::new(&Empty::default())
    }

    async fn unary_call(
        &self,
        request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        let (initial, trailing) = echoes(request.metadata());
        let message = request.message()?;
        if let Err(status) = asked_status(message.response_status()) {
            return Err(status.with_metadata(trailing));
        }
        if let Some(expect) = message.expect_compressed() {
            if expect.value() != request.was_compressed() {
                return Err(Status::new(
                    Code::InvalidArgument,
                    format!(
                        "expect_compressed is {} and the request was {}compressed",
                        expect.value(),
                        if request.was_compressed() { "" } else { "not " }
                    ),
                ));
            }
        }
        let body = zeros(message.response_size());
        let payload = Payload {
            body: &body,
            ..Default::default()
        };
        let mut reply = Reply::new(&SimpleResponse {
            payload: Some(&payload),
            ..Default::default()
        })?
        .with_metadata(initial)
        .with_trailers(trailing);
        if let Some(compressed) = message.response_compressed() {
            reply = reply.compress(compressed.value());
        }
        Ok(reply)
    }

    async fn cacheable_unary_call(
        &self,
        request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        self.unary_call(request).await
    }

    async fn streaming_output_call(
        &self,
        request: Request<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let (initial, trailing) = echoes(request.metadata());
        responses.set_metadata(initial)?;
        responses.set_trailers(trailing);
        let message = request.message()?;
        // Announced before the first message, so that each one can choose.
        if message
            .response_parameters()
            .iter()
            .any(|p| p.compressed().is_some_and(|c| c.value()))
        {
            responses.compress(true)?;
        }
        respond(&message, &mut responses).await?;
        asked_status(message.response_status())
    }

    async fn streaming_input_call(
        &self,
        mut requests: RequestStream<StreamingInputCallRequest<'static>>,
    ) -> Result<Reply<StreamingInputCallResponse<'static>>, Status> {
        let mut total = 0;
        while let Some(received) = requests.message().await? {
            let message = received.message()?;
            if let Some(expect) = message.expect_compressed() {
                if expect.value() != received.was_compressed() {
                    return Err(Status::new(
                        Code::InvalidArgument,
                        "a request was not compressed as expect_compressed says",
                    ));
                }
            }
            total += message.payload().map_or(0, |p| p.body().len());
        }
        Reply::new(&StreamingInputCallResponse {
            aggregated_payload_size: total as i32,
            ..Default::default()
        })
    }

    async fn full_duplex_call(
        &self,
        mut requests: RequestStream<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let (initial, trailing) = echoes(requests.metadata());
        responses.set_metadata(initial)?;
        responses.set_trailers(trailing);
        while let Some(received) = requests.message().await? {
            let message = received.message()?;
            asked_status(message.response_status())?;
            respond(&message, &mut responses).await?;
        }
        Ok(())
    }

    async fn half_duplex_call(
        &self,
        mut requests: RequestStream<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        // Every request first, then every response.
        let mut all = Vec::new();
        while let Some(received) = requests.message().await? {
            all.push(received);
        }
        for received in &all {
            respond(&received.message()?, &mut responses).await?;
        }
        Ok(())
    }

    async fn unimplemented_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        Err(Status::new(
            Code::Unimplemented,
            "not implemented, as the test asks",
        ))
    }
}

/// Sends the responses one request asks for.
async fn respond(
    message: &StreamingOutputCallRequestView<'_>,
    responses: &mut Sink<StreamingOutputCallResponse<'static>>,
) -> Result<(), Status> {
    for parameters in message.response_parameters().iter() {
        if parameters.interval_us() > 0 {
            giong::time::sleep(Duration::from_micros(parameters.interval_us() as u64)).await;
        }
        if let Some(compressed) = parameters.compressed() {
            responses.compress(compressed.value())?;
        }
        let body = zeros(parameters.size());
        let payload = Payload {
            body: &body,
            ..Default::default()
        };
        responses
            .send(&StreamingOutputCallResponse {
                payload: Some(&payload),
                ..Default::default()
            })
            .await?;
    }
    Ok(())
}

/// Serves the test service on a loopback port of this worker; returns the
/// address for a channel.
pub fn serve(server: giong_grpc::Server) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move { giong_http::Server::new().serve(listener, server).await });
    format!("http://{addr}")
}

/// A server with the test service.
pub fn test_server() -> giong_grpc::Server {
    giong_grpc::Server::new().add(TestServiceServer::new(Interop))
}
