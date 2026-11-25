//! How a streaming handler's messages leave: together when they are sent
//! in a row, in order, no faster than the client takes them, and from
//! whichever task holds the sink.

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use giong::time::sleep;
use giong_grpc::{CallOptions, Channel, Code, Reply, Request, RequestStream, Sink, Status};

/// What the request's first response parameter asks for, by its size.
const IN_A_ROW: i32 = 1;
const FLOOD: i32 = 2;
const FROM_A_TASK: i32 = 3;

#[derive(Clone, Default)]
struct Sender {
    /// Messages the flood has sent.
    sent: Rc<Cell<u64>>,
    /// The flood's handler is gone.
    dropped: Rc<Cell<bool>>,
}

struct OnDrop(Rc<Cell<bool>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

async fn send_numbered(
    responses: &mut Sink<StreamingOutputCallResponse<'static>>,
    i: u32,
    padding: usize,
) -> Result<(), Status> {
    let mut body = i.to_be_bytes().to_vec();
    body.resize(4 + padding, 0);
    let payload = Payload {
        body: &body,
        ..Default::default()
    };
    responses
        .send(&StreamingOutputCallResponse {
            payload: Some(&payload),
            ..Default::default()
        })
        .await
}

impl TestService for Sender {
    async fn empty_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn unary_call(
        &self,
        _request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn cacheable_unary_call(
        &self,
        _request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn streaming_output_call(
        &self,
        request: Request<StreamingOutputCallRequest<'static>>,
        mut responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let message = request.message()?;
        let mut parameters = message.response_parameters().iter().map(|p| p.size());
        let mode = parameters.next().unwrap_or(0);
        let count = parameters.next().unwrap_or(0) as u32;
        match mode {
            IN_A_ROW => {
                for i in 0..count {
                    send_numbered(&mut responses, i, 0).await?;
                }
                Ok(())
            }
            FLOOD => {
                let _guard = OnDrop(self.dropped.clone());
                loop {
                    send_numbered(&mut responses, 0, 1020).await?;
                    self.sent.set(self.sent.get() + 1);
                }
            }
            FROM_A_TASK => {
                let task = giong::spawn(async move {
                    for i in 0..count {
                        sleep(Duration::from_millis(10)).await;
                        send_numbered(&mut responses, i, 0).await.unwrap();
                    }
                });
                task.await.unwrap();
                Ok(())
            }
            _ => Err(Status::new(Code::InvalidArgument, "an unknown mode")),
        }
    }

    async fn streaming_input_call(
        &self,
        _requests: RequestStream<StreamingInputCallRequest<'static>>,
    ) -> Result<Reply<StreamingInputCallResponse<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn full_duplex_call(
        &self,
        _requests: RequestStream<StreamingOutputCallRequest<'static>>,
        _responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn half_duplex_call(
        &self,
        _requests: RequestStream<StreamingOutputCallRequest<'static>>,
        _responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }

    async fn unimplemented_call(
        &self,
        _request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        Err(Status::new(Code::Unimplemented, ""))
    }
}

fn sender() -> (Sender, TestServiceClient) {
    let sender = Sender::default();
    let base = serve(giong_grpc::Server::new().add(TestServiceServer::new(sender.clone())));
    (sender, TestServiceClient::new(Channel::new(&base).unwrap()))
}

fn asking(mode: i32, count: i32) -> [ResponseParameters<'static>; 2] {
    [
        ResponseParameters {
            size: mode,
            ..Default::default()
        },
        ResponseParameters {
            size: count,
            ..Default::default()
        },
    ]
}

fn number(received: &giong_grpc::Received<StreamingOutputCallResponse<'static>>) -> u32 {
    let message = received.message().unwrap();
    let body = message.payload().unwrap().body();
    u32::from_be_bytes([body[0], body[1], body[2], body[3]])
}

#[test]
fn messages_sent_in_a_row_arrive_in_order() {
    giong::block_on(async {
        let (_, client) = sender();
        // Many times what leaves together.
        let parameters = asking(IN_A_ROW, 20_000);
        let mut responses = client
            .streaming_output_call(
                &StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap();
        let mut next = 0;
        while let Some(received) = responses.message().await.unwrap() {
            assert_eq!(number(&received), next);
            next += 1;
        }
        assert_eq!(next, 20_000);
    });
}

#[test]
fn a_client_that_does_not_read_holds_the_handler_back() {
    giong::block_on(async {
        let (sender, client) = sender();
        let parameters = asking(FLOOD, 0);
        let mut responses = client
            .streaming_output_call(
                &StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_some());
        // Nothing more is read: the handler stops once the windows and the
        // buffers on the way are full. How long filling them takes depends on
        // the machine (under TSan it was still going after 300 ms), so wait
        // until the count stops moving, then check where it stopped.
        let mut sent = sender.sent.get();
        for _ in 0..300 {
            sleep(Duration::from_millis(100)).await;
            let now = sender.sent.get();
            if now == sent {
                break;
            }
            sent = now;
        }
        assert!(
            sent < 4096,
            "{sent} messages of 1 KiB sent to a client that reads none"
        );
        sleep(Duration::from_millis(300)).await;
        assert_eq!(sender.sent.get(), sent, "the handler kept sending");
        // The client gives up: the handler, waiting to send, is dropped.
        drop(responses);
        for _ in 0..1000 {
            if sender.dropped.get() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(sender.dropped.get());
    });
}

#[test]
fn a_sink_in_another_task_sends_while_the_handler_waits() {
    giong::block_on(async {
        let (_, client) = sender();
        let parameters = asking(FROM_A_TASK, 3);
        let mut responses = client
            .streaming_output_call(
                &StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap();
        for i in 0..3 {
            assert_eq!(number(&responses.message().await.unwrap().unwrap()), i);
        }
        assert!(responses.message().await.unwrap().is_none());
    });
}
