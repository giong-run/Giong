//! The four kinds of call, end to end: giong-grpc's client against
//! giong-grpc's server, on one giong worker.

mod common;

use std::time::{Duration, Instant};

use common::*;
use giong::time::sleep;
use giong_grpc::{CallOptions, Channel, Code, Metadata, Server};

fn channel(base: &str) -> Channel {
    Channel::new(base).unwrap()
}

fn sized(size: i32) -> ResponseParameters<'static> {
    ResponseParameters {
        size,
        ..Default::default()
    }
}

#[test]
fn unary_calls_carry_messages_metadata_and_statuses() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let reply = client
            .empty_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap();
        assert!(reply.as_bytes().is_empty());
        reply.message().unwrap();

        // A large payload each way, with metadata echoed around it.
        let body = vec![7u8; 271_828];
        let payload = Payload {
            body: &body,
            ..Default::default()
        };
        let mut metadata = Metadata::new();
        metadata
            .add("x-grpc-test-echo-initial", "test_initial_metadata_value")
            .unwrap();
        metadata
            .add("x-grpc-test-echo-trailing-bin", [0xab, 0xab, 0xab])
            .unwrap();
        let reply = client
            .unary_call(
                &SimpleRequest {
                    response_size: 314_159,
                    payload: Some(&payload),
                    ..Default::default()
                },
                CallOptions::new().metadata(metadata),
            )
            .await
            .unwrap();
        let message = reply.message().unwrap();
        assert_eq!(message.payload().unwrap().body().len(), 314_159);
        assert_eq!(
            reply.metadata().get_str("x-grpc-test-echo-initial"),
            Some("test_initial_metadata_value")
        );
        assert_eq!(
            reply.trailers().get("x-grpc-test-echo-trailing-bin"),
            Some(&[0xab, 0xab, 0xab][..])
        );

        // A status with a message beyond ASCII.
        let text = "\t\ntest with whitespace\r\nand Unicode BMP \u{263a} and non-BMP \u{1f608}\t\n";
        let asked = EchoStatus {
            code: 2,
            message: text,
            ..Default::default()
        };
        let status = client
            .unary_call(
                &SimpleRequest {
                    response_status: Some(&asked),
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unknown);
        assert_eq!(status.message(), text);
        assert_eq!(status.error_code(), None);

        let status = client
            .unimplemented_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unimplemented);
    });
}

#[test]
fn server_streaming_sends_each_message_as_it_is_ready() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let sizes = [31_415, 9, 2_653, 58_979];
        let parameters: Vec<_> = sizes.iter().map(|&s| sized(s)).collect();
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
        let mut got = Vec::new();
        while let Some(received) = responses.message().await.unwrap() {
            got.push(received.message().unwrap().payload().unwrap().body().len() as i32);
        }
        assert_eq!(got, sizes);
        // The end stays the end.
        assert!(responses.message().await.unwrap().is_none());

        // No message at all: the status is the whole response.
        let mut responses = client
            .streaming_output_call(&StreamingOutputCallRequest::default(), CallOptions::new())
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_none());
    });
}

#[test]
fn client_streaming_reads_requests_until_the_client_finishes() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let mut call = client.streaming_input_call(CallOptions::new());
        let sizes = [27_182, 8, 1_828, 45_904];
        for size in sizes {
            let body = vec![1u8; size];
            let payload = Payload {
                body: &body,
                ..Default::default()
            };
            call.send(&StreamingInputCallRequest {
                payload: Some(&payload),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let reply = call.finish().await.unwrap();
        assert_eq!(
            reply.message().unwrap().aggregated_payload_size(),
            sizes.iter().sum::<usize>() as i32
        );

        // No request at all.
        let reply = client
            .streaming_input_call(CallOptions::new())
            .finish()
            .await
            .unwrap();
        assert_eq!(reply.message().unwrap().aggregated_payload_size(), 0);
    });
}

#[test]
fn bidi_calls_alternate_requests_and_responses() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let mut metadata = Metadata::new();
        metadata.add("x-grpc-test-echo-initial", "hello").unwrap();
        metadata
            .add("x-grpc-test-echo-trailing-bin", [1, 2, 3])
            .unwrap();
        let (mut requests, mut responses) =
            client.full_duplex_call(CallOptions::new().metadata(metadata));
        for (response_size, request_size) in
            [(31_415, 27_182), (9, 8), (2_653, 1_828), (58_979, 45_904)]
        {
            let body = vec![2u8; request_size];
            let payload = Payload {
                body: &body,
                ..Default::default()
            };
            requests
                .send(&StreamingOutputCallRequest {
                    response_parameters: &[sized(response_size)],
                    payload: Some(&payload),
                    ..Default::default()
                })
                .await
                .unwrap();
            let received = responses.message().await.unwrap().expect("a response");
            assert_eq!(
                received.message().unwrap().payload().unwrap().body().len(),
                response_size as usize
            );
        }
        assert_eq!(
            responses
                .metadata()
                .await
                .unwrap()
                .get_str("x-grpc-test-echo-initial"),
            Some("hello")
        );
        assert!(responses.trailers().is_empty());
        requests.finish();
        assert!(responses.message().await.unwrap().is_none());
        assert_eq!(
            responses.trailers().get("x-grpc-test-echo-trailing-bin"),
            Some(&[1, 2, 3][..])
        );

        // A status in the middle of the stream ends it for both sides.
        let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
        let asked = EchoStatus {
            code: 9,
            message: "as asked",
            ..Default::default()
        };
        requests
            .send(&StreamingOutputCallRequest {
                response_parameters: &[sized(5)],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(responses.message().await.unwrap().is_some());
        requests
            .send(&StreamingOutputCallRequest {
                response_status: Some(&asked),
                ..Default::default()
            })
            .await
            .unwrap();
        let status = responses.message().await.unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "as asked");
        assert!(responses.message().await.unwrap().is_none());
        // The sender is told, too.
        let mut told = false;
        for _ in 0..100 {
            if requests
                .send(&StreamingOutputCallRequest::default())
                .await
                .is_err()
            {
                told = true;
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(told, "the request sink was never told that the call ended");
        assert!(requests.is_closed());
    });
}

#[test]
fn deadlines_end_calls_on_both_sides() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        // The server sleeps longer than the call allows.
        let slow = ResponseParameters {
            size: 10,
            interval_us: 2_000_000,
            ..Default::default()
        };
        let started = Instant::now();
        let (mut requests, mut responses) =
            client.full_duplex_call(CallOptions::new().timeout(Duration::from_millis(100)));
        requests
            .send(&StreamingOutputCallRequest {
                response_parameters: std::slice::from_ref(&slow),
                ..Default::default()
            })
            .await
            .unwrap();
        let status = responses.message().await.unwrap_err();
        assert_eq!(status.code(), Code::DeadlineExceeded, "{status}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );

        // Server streaming: the head arrives late too.
        let status = client
            .streaming_output_call(
                &StreamingOutputCallRequest {
                    response_parameters: &[slow],
                    ..Default::default()
                },
                CallOptions::new().timeout(Duration::from_millis(50)),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::DeadlineExceeded, "{status}");
        assert_eq!(status.error_code(), Some(giong::error::Code::GrpcDeadline));
    });
}

#[test]
fn messages_past_the_limit_are_refused_on_each_side() {
    giong::block_on(async {
        let base = serve(test_server().max_message_size(10_000));
        let client = TestServiceClient::new(channel(&base));
        let body = vec![0u8; 20_000];
        let payload = Payload {
            body: &body,
            ..Default::default()
        };
        // The server refuses the request.
        let status = client
            .unary_call(
                &SimpleRequest {
                    payload: Some(&payload),
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted, "{status}");
        assert!(status.message().contains("GIONG-E4102"), "{status}");
        // The server refuses to send its own response.
        let status = client
            .unary_call(
                &SimpleRequest {
                    response_size: 20_000,
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted, "{status}");

        // The client refuses what it would send, and what it receives.
        let base = serve(test_server());
        let small = Channel::builder(&base)
            .max_message_size(10_000)
            .build()
            .unwrap();
        let client = TestServiceClient::new(small);
        let status = client
            .unary_call(
                &SimpleRequest {
                    payload: Some(&payload),
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            status.error_code(),
            Some(giong::error::Code::GrpcMessageTooLarge)
        );
        let status = client
            .unary_call(
                &SimpleRequest {
                    response_size: 20_000,
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted, "{status}");
        assert_eq!(
            status.error_code(),
            Some(giong::error::Code::GrpcMessageTooLarge)
        );
    });
}

#[test]
fn gzip_is_used_when_both_sides_have_it() {
    giong::block_on(async {
        let base = serve(test_server().compress(true));
        let client =
            TestServiceClient::new(Channel::builder(&base).compress(true).build().unwrap());
        let body = vec![0u8; 100_000];
        let payload = Payload {
            body: &body,
            ..Default::default()
        };
        let expect = BoolValue {
            value: true,
            ..Default::default()
        };
        let reply = client
            .unary_call(
                &SimpleRequest {
                    response_size: 50_000,
                    payload: Some(&payload),
                    expect_compressed: Some(&expect),
                    response_compressed: Some(&expect),
                    ..Default::default()
                },
                CallOptions::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            reply.message().unwrap().payload().unwrap().body().len(),
            50_000
        );

        // One call can send plain on a channel that compresses, and the
        // server says so when it expected otherwise.
        let status = client
            .unary_call(
                &SimpleRequest {
                    expect_compressed: Some(&expect),
                    payload: Some(&payload),
                    ..Default::default()
                },
                CallOptions::new().compress(false),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{status}");

        // A stream chooses for each message.
        let on = BoolValue {
            value: true,
            ..Default::default()
        };
        let off = BoolValue::default();
        let parameters = [
            ResponseParameters {
                size: 31_415,
                compressed: Some(&on),
                ..Default::default()
            },
            ResponseParameters {
                size: 92_653,
                compressed: Some(&off),
                ..Default::default()
            },
        ];
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
        let first = responses.message().await.unwrap().unwrap();
        assert!(first.was_compressed());
        assert_eq!(
            first.message().unwrap().payload().unwrap().body().len(),
            31_415
        );
        let second = responses.message().await.unwrap().unwrap();
        assert!(!second.was_compressed());
        assert!(responses.message().await.unwrap().is_none());
    });
}

#[test]
fn unknown_services_methods_and_dead_servers_have_their_statuses() {
    giong::block_on(async {
        // A server without the service.
        let client = TestServiceClient::new(channel(&serve(Server::new())));
        let status = client
            .empty_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unimplemented);
        assert!(status.message().contains("GIONG-E4110"), "{status}");
        assert!(status
            .message()
            .contains("/grpc.testing.TestService/EmptyCall"));

        // Nothing listens.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let client = TestServiceClient::new(channel(&format!("http://127.0.0.1:{port}")));
        let status = client
            .empty_call(&Empty::default(), CallOptions::new())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unavailable, "{status}");
        assert_eq!(status.error_code(), Some(giong::error::Code::GrpcTransport));
        let (_requests, mut responses) = client.full_duplex_call(CallOptions::new());
        let status = responses.message().await.unwrap_err();
        assert_eq!(status.code(), Code::Unavailable, "{status}");

        for bad in [
            "127.0.0.1:1",
            "ftp://x",
            "http://",
            "http://host/path",
            "https://u@host",
        ] {
            let error = Channel::new(bad).unwrap_err();
            assert_eq!(error.code(), giong::error::Code::GrpcEndpoint, "{bad}");
        }
    });
}

#[test]
fn many_calls_share_one_connection() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let mut tasks = Vec::new();
        for i in 0..200 {
            let client = client.clone();
            tasks.push(giong::spawn(async move {
                let reply = client
                    .unary_call(
                        &SimpleRequest {
                            response_size: i,
                            ..Default::default()
                        },
                        CallOptions::new(),
                    )
                    .await
                    .unwrap();
                let size = reply
                    .message()
                    .unwrap()
                    .payload()
                    .map_or(0, |p| p.body().len());
                assert_eq!(size, i as usize);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
}

#[test]
fn a_bidirectional_response_starts_with_its_first_message() {
    giong::block_on(async {
        let client = TestServiceClient::new(channel(&serve(test_server())));
        let mut metadata = Metadata::new();
        metadata.add("x-grpc-test-echo-initial", "early").unwrap();
        let (mut requests, mut responses) =
            client.full_duplex_call(CallOptions::new().metadata(metadata));
        // Like grpc-go's and tonic's servers, nothing is sent while the
        // handler waits for the first request.
        let early = giong::time::timeout(Duration::from_millis(200), responses.metadata()).await;
        assert!(early.is_err(), "the response started before any message");
        let parameters = [sized(3)];
        requests
            .send(&StreamingOutputCallRequest {
                response_parameters: &parameters,
                ..Default::default()
            })
            .await
            .unwrap();
        let initial = responses.metadata().await.unwrap();
        assert_eq!(initial.get_str("x-grpc-test-echo-initial"), Some("early"));
        let received = responses.message().await.unwrap().unwrap();
        assert_eq!(
            received.message().unwrap().payload().unwrap().body().len(),
            3
        );
        requests.finish();
        assert!(responses.message().await.unwrap().is_none());
    });
}
