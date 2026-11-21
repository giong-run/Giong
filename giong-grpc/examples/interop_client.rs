//! The client of gRPC's interoperability tests
//! (github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md):
//! runs one test case, or all of them, against a server of any
//! implementation. `giong-grpc/interop/run.sh` runs it against grpc-go's.
//!
//! ```text
//! cargo run -p giong-grpc --example interop_client -- \
//!     --server_port=10000 --test_case=large_unary
//! ```
//!
//! Exits with 0 if every case passed.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Duration;

use common::pb::grpc::testing::unimplemented_service::UnimplementedServiceClient;
use common::*;
use giong_grpc::{CallOptions, Channel, Code, Metadata};

/// The cases, in the order of the specification. The compressed ones need
/// a server that implements them: grpc-go's does not.
const CASES: &[&str] = &[
    "empty_unary",
    "large_unary",
    "client_compressed_unary",
    "server_compressed_unary",
    "client_streaming",
    "client_compressed_streaming",
    "server_streaming",
    "server_compressed_streaming",
    "ping_pong",
    "empty_stream",
    "custom_metadata",
    "status_code_and_message",
    "special_status_message",
    "unimplemented_method",
    "unimplemented_service",
    "cancel_after_begin",
    "cancel_after_first_response",
    "timeout_on_sleeping_server",
];

type Outcome = Result<(), String>;

macro_rules! check {
    ($cond:expr, $($what:tt)*) => {
        if !$cond {
            return Err(format!($($what)*));
        }
    };
}

fn fail<T>(what: &str) -> impl FnOnce(giong_grpc::Status) -> Result<T, String> + '_ {
    move |status| Err(format!("{what}: {status}"))
}

fn payload(body: &[u8]) -> Payload<'_> {
    Payload {
        body,
        ..Default::default()
    }
}

fn flag(value: bool) -> BoolValue<'static> {
    BoolValue {
        value,
        ..Default::default()
    }
}

async fn empty_unary(client: &TestServiceClient) -> Outcome {
    let reply = client
        .empty_call(&Empty::default(), CallOptions::new())
        .await
        .or_else(fail("EmptyCall"))?;
    check!(reply.as_bytes().is_empty(), "the response is not empty");
    Ok(())
}

async fn large_unary(client: &TestServiceClient) -> Outcome {
    let body = vec![0u8; 271_828];
    let request = SimpleRequest {
        response_size: 314_159,
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    let reply = client
        .unary_call(&request, CallOptions::new())
        .await
        .or_else(fail("UnaryCall"))?;
    let size = reply
        .message()
        .or_else(fail("decoding"))?
        .payload()
        .map_or(0, |p| p.body().len());
    check!(size == 314_159, "the response payload has {size} bytes");
    Ok(())
}

async fn client_compressed_unary(client: &TestServiceClient) -> Outcome {
    let body = vec![0u8; 271_828];
    let (yes, no) = (flag(true), flag(false));
    let payload = payload(&body);
    let request = |expect| SimpleRequest {
        expect_compressed: Some(expect),
        response_size: 314_159,
        payload: Some(&payload),
        ..Default::default()
    };
    // The probe: an uncompressed message that claims to be compressed.
    let probe = client
        .unary_call(&request(&yes), CallOptions::new().compress(false))
        .await;
    match probe {
        Err(status) if status.code() == Code::InvalidArgument => {}
        other => return Err(format!("the probe was answered {other:?}")),
    }
    for (expect, compress) in [(&yes, true), (&no, false)] {
        let reply = client
            .unary_call(&request(expect), CallOptions::new().compress(compress))
            .await
            .or_else(fail("UnaryCall"))?;
        let size = reply
            .message()
            .or_else(fail("decoding"))?
            .payload()
            .map_or(0, |p| p.body().len());
        check!(size == 314_159, "the response payload has {size} bytes");
    }
    Ok(())
}

async fn server_compressed_unary(client: &TestServiceClient) -> Outcome {
    let body = vec![0u8; 271_828];
    for compressed in [true, false] {
        let request = SimpleRequest {
            response_compressed: Some(&flag(compressed)),
            response_size: 314_159,
            payload: Some(&payload(&body)),
            ..Default::default()
        };
        let reply = client
            .unary_call(&request, CallOptions::new())
            .await
            .or_else(fail("UnaryCall"))?;
        check!(
            reply.was_compressed() == compressed,
            "asked for compressed = {compressed}, the response says otherwise"
        );
        let size = reply
            .message()
            .or_else(fail("decoding"))?
            .payload()
            .map_or(0, |p| p.body().len());
        check!(size == 314_159, "the response payload has {size} bytes");
    }
    Ok(())
}

/// Not one of the specification's cases: gzip both ways with a server
/// that does not implement the `*_compressed_*` cases (grpc-go's). The
/// request is compressed; a server that answers in kind is read too.
async fn gzip_unary(client: &TestServiceClient) -> Outcome {
    let body: Vec<u8> = (0..271_828u32).map(|i| (i % 251) as u8).collect();
    let request = SimpleRequest {
        response_size: 314_159,
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    let reply = client
        .unary_call(&request, CallOptions::new().compress(true))
        .await
        .or_else(fail("UnaryCall"))?;
    let size = reply
        .message()
        .or_else(fail("decoding"))?
        .payload()
        .map_or(0, |p| p.body().len());
    check!(size == 314_159, "the response payload has {size} bytes");
    println!(
        "gzip_unary: the response came {}",
        if reply.was_compressed() {
            "compressed"
        } else {
            "plain"
        }
    );
    Ok(())
}

async fn client_streaming(client: &TestServiceClient) -> Outcome {
    let mut call = client.streaming_input_call(CallOptions::new());
    for size in [27_182, 8, 1_828, 45_904] {
        let body = vec![0u8; size];
        let request = StreamingInputCallRequest {
            payload: Some(&payload(&body)),
            ..Default::default()
        };
        call.send(&request).await.or_else(fail("sending"))?;
    }
    let reply = call.finish().await.or_else(fail("StreamingInputCall"))?;
    let total = reply
        .message()
        .or_else(fail("decoding"))?
        .aggregated_payload_size();
    check!(total == 74_922, "the server counted {total} bytes");
    Ok(())
}

async fn client_compressed_streaming(client: &TestServiceClient) -> Outcome {
    let (yes, no) = (flag(true), flag(false));
    let body = vec![0u8; 27_182];
    // The probe: an uncompressed message that claims to be compressed.
    let mut call = client.streaming_input_call(CallOptions::new().compress(false));
    let probe = StreamingInputCallRequest {
        expect_compressed: Some(&yes),
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    // The server may end the call before the message is all sent.
    let _ = call.send(&probe).await;
    match call.finish().await {
        Err(status) if status.code() == Code::InvalidArgument => {}
        other => return Err(format!("the probe was answered {other:?}")),
    }
    let mut call = client.streaming_input_call(CallOptions::new().compress(true));
    for (expect, compress, size) in [(&yes, true, 27_182), (&no, false, 45_904)] {
        let body = vec![0u8; size];
        let request = StreamingInputCallRequest {
            expect_compressed: Some(expect),
            payload: Some(&payload(&body)),
            ..Default::default()
        };
        call.compress(compress).or_else(fail("compress"))?;
        call.send(&request).await.or_else(fail("sending"))?;
    }
    let reply = call.finish().await.or_else(fail("StreamingInputCall"))?;
    let total = reply
        .message()
        .or_else(fail("decoding"))?
        .aggregated_payload_size();
    check!(total == 73_086, "the server counted {total} bytes");
    Ok(())
}

async fn server_streaming(client: &TestServiceClient) -> Outcome {
    let sizes = [31_415, 9, 2_653, 58_979];
    let parameters: Vec<_> = sizes
        .iter()
        .map(|&size| ResponseParameters {
            size,
            ..Default::default()
        })
        .collect();
    let request = StreamingOutputCallRequest {
        response_parameters: &parameters,
        ..Default::default()
    };
    let mut responses = client
        .streaming_output_call(&request, CallOptions::new())
        .await
        .or_else(fail("StreamingOutputCall"))?;
    let mut got = Vec::new();
    while let Some(received) = responses.message().await.or_else(fail("reading"))? {
        let message = received.message().or_else(fail("decoding"))?;
        got.push(message.payload().map_or(0, |p| p.body().len()) as i32);
    }
    check!(got == sizes, "the responses have sizes {got:?}");
    Ok(())
}

async fn server_compressed_streaming(client: &TestServiceClient) -> Outcome {
    let (yes, no) = (flag(true), flag(false));
    let asked = [(31_415, true), (92_653, false)];
    let parameters: Vec<_> = asked
        .iter()
        .map(|&(size, compressed)| ResponseParameters {
            size,
            compressed: Some(if compressed { &yes } else { &no }),
            ..Default::default()
        })
        .collect();
    let request = StreamingOutputCallRequest {
        response_parameters: &parameters,
        ..Default::default()
    };
    let mut responses = client
        .streaming_output_call(&request, CallOptions::new())
        .await
        .or_else(fail("StreamingOutputCall"))?;
    let mut got = Vec::new();
    while let Some(received) = responses.message().await.or_else(fail("reading"))? {
        let message = received.message().or_else(fail("decoding"))?;
        got.push((
            message.payload().map_or(0, |p| p.body().len()) as i32,
            received.was_compressed(),
        ));
    }
    check!(got == asked, "the responses are {got:?}");
    Ok(())
}

async fn ping_pong(client: &TestServiceClient) -> Outcome {
    let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
    for (response, request) in [(31_415, 27_182), (9, 8), (2_653, 1_828), (58_979, 45_904)] {
        let body = vec![0u8; request];
        let parameters = [ResponseParameters {
            size: response,
            ..Default::default()
        }];
        let message = StreamingOutputCallRequest {
            response_parameters: &parameters,
            payload: Some(&payload(&body)),
            ..Default::default()
        };
        requests.send(&message).await.or_else(fail("sending"))?;
        let Some(received) = responses.message().await.or_else(fail("reading"))? else {
            return Err("the server ended the call early".into());
        };
        let message = received.message().or_else(fail("decoding"))?;
        let size = message.payload().map_or(0, |p| p.body().len());
        check!(size == response as usize, "a response has {size} bytes");
    }
    requests.finish();
    let end = responses.message().await.or_else(fail("ending"))?;
    check!(end.is_none(), "the server sent a response nobody asked for");
    Ok(())
}

async fn empty_stream(client: &TestServiceClient) -> Outcome {
    let (requests, mut responses) = client.full_duplex_call(CallOptions::new());
    requests.finish();
    let end = responses.message().await.or_else(fail("ending"))?;
    check!(end.is_none(), "the server sent a response nobody asked for");
    Ok(())
}

async fn custom_metadata(client: &TestServiceClient) -> Outcome {
    const INITIAL: (&str, &str) = ("x-grpc-test-echo-initial", "test_initial_metadata_value");
    const TRAILING: (&str, [u8; 3]) = ("x-grpc-test-echo-trailing-bin", [0xab, 0xab, 0xab]);
    let options = || {
        let mut metadata = Metadata::new();
        metadata.add(INITIAL.0, INITIAL.1).expect("valid");
        metadata.add(TRAILING.0, TRAILING.1).expect("valid");
        CallOptions::new().metadata(metadata)
    };
    let echoed = |initial: &Metadata, trailing: &Metadata| {
        check!(
            initial.get_str(INITIAL.0) == Some(INITIAL.1),
            "the initial metadata came back as {initial:?}"
        );
        check!(
            trailing.get(TRAILING.0) == Some(&TRAILING.1[..]),
            "the trailing metadata came back as {trailing:?}"
        );
        Ok(())
    };
    let body = vec![0u8; 271_828];
    let request = SimpleRequest {
        response_size: 314_159,
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    let reply = client
        .unary_call(&request, options())
        .await
        .or_else(fail("UnaryCall"))?;
    echoed(reply.metadata(), reply.trailers())?;

    let (mut requests, mut responses) = client.full_duplex_call(options());
    let parameters = [ResponseParameters {
        size: 314_159,
        ..Default::default()
    }];
    let message = StreamingOutputCallRequest {
        response_parameters: &parameters,
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    requests.send(&message).await.or_else(fail("sending"))?;
    requests.finish();
    while responses
        .message()
        .await
        .or_else(fail("reading"))?
        .is_some()
    {}
    let initial = responses
        .metadata()
        .await
        .or_else(fail("metadata"))?
        .clone();
    echoed(&initial, responses.trailers())
}

async fn status_code_and_message(client: &TestServiceClient) -> Outcome {
    const TEXT: &str = "test status message";
    let asked = EchoStatus {
        code: 2,
        message: TEXT,
        ..Default::default()
    };
    let matches = |status: giong_grpc::Status| {
        check!(
            status.code() == Code::Unknown && status.message() == TEXT,
            "the status came back as {status}"
        );
        Ok(())
    };
    let request = SimpleRequest {
        response_status: Some(&asked),
        ..Default::default()
    };
    match client.unary_call(&request, CallOptions::new()).await {
        Err(status) => matches(status)?,
        Ok(_) => return Err("UnaryCall succeeded".into()),
    }
    let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
    let message = StreamingOutputCallRequest {
        response_status: Some(&asked),
        ..Default::default()
    };
    requests.send(&message).await.or_else(fail("sending"))?;
    requests.finish();
    match responses.message().await {
        Err(status) => matches(status),
        Ok(_) => Err("FullDuplexCall did not fail".into()),
    }
}

async fn special_status_message(client: &TestServiceClient) -> Outcome {
    const TEXT: &str =
        "\t\ntest with whitespace\r\nand Unicode BMP \u{263a} and non-BMP \u{1f608}\t\n";
    let asked = EchoStatus {
        code: 2,
        message: TEXT,
        ..Default::default()
    };
    let request = SimpleRequest {
        response_status: Some(&asked),
        ..Default::default()
    };
    match client.unary_call(&request, CallOptions::new()).await {
        Err(status) => {
            check!(
                status.code() == Code::Unknown && status.message() == TEXT,
                "the status came back as {status}"
            );
            Ok(())
        }
        Ok(_) => Err("UnaryCall succeeded".into()),
    }
}

async fn unimplemented_method(client: &TestServiceClient) -> Outcome {
    match client
        .unimplemented_call(&Empty::default(), CallOptions::new())
        .await
    {
        Err(status) if status.code() == Code::Unimplemented => Ok(()),
        other => Err(format!("UnimplementedCall was answered {other:?}")),
    }
}

async fn unimplemented_service(channel: &Channel) -> Outcome {
    match UnimplementedServiceClient::new(channel.clone())
        .unimplemented_call(&Empty::default(), CallOptions::new())
        .await
    {
        Err(status) if status.code() == Code::Unimplemented => Ok(()),
        other => Err(format!("UnimplementedCall was answered {other:?}")),
    }
}

async fn cancel_after_begin(client: &TestServiceClient) -> Outcome {
    let call = client.streaming_input_call(CallOptions::new());
    // Let the call reach the server first.
    giong::time::sleep(Duration::from_millis(50)).await;
    call.cancel();
    // The call is over for this side; what is left to check is that the
    // connection still serves calls.
    empty_unary(client).await
}

async fn cancel_after_first_response(client: &TestServiceClient) -> Outcome {
    let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
    let body = vec![0u8; 27_182];
    let parameters = [ResponseParameters {
        size: 31_415,
        ..Default::default()
    }];
    let message = StreamingOutputCallRequest {
        response_parameters: &parameters,
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    requests.send(&message).await.or_else(fail("sending"))?;
    let first = responses.message().await.or_else(fail("reading"))?;
    check!(first.is_some(), "the server ended the call early");
    requests.cancel();
    match responses.message().await {
        Err(status) if status.code() == Code::Cancelled => {}
        other => return Err(format!("after the cancel, the call gave {other:?}")),
    }
    empty_unary(client).await
}

async fn timeout_on_sleeping_server(client: &TestServiceClient) -> Outcome {
    let options = CallOptions::new().timeout(Duration::from_millis(1));
    let (mut requests, mut responses) = client.full_duplex_call(options);
    let body = vec![0u8; 27_182];
    let message = StreamingOutputCallRequest {
        payload: Some(&payload(&body)),
        ..Default::default()
    };
    // The deadline may pass before the message is sent.
    let _ = requests.send(&message).await;
    match responses.message().await {
        Err(status) if status.code() == Code::DeadlineExceeded => Ok(()),
        other => Err(format!("the call gave {other:?}")),
    }
}

async fn run(case: &str, channel: &Channel) -> Outcome {
    let client = &TestServiceClient::new(channel.clone());
    match case {
        "empty_unary" => empty_unary(client).await,
        "large_unary" => large_unary(client).await,
        "client_compressed_unary" => client_compressed_unary(client).await,
        "server_compressed_unary" => server_compressed_unary(client).await,
        "gzip_unary" => gzip_unary(client).await,
        "client_streaming" => client_streaming(client).await,
        "client_compressed_streaming" => client_compressed_streaming(client).await,
        "server_streaming" => server_streaming(client).await,
        "server_compressed_streaming" => server_compressed_streaming(client).await,
        "ping_pong" => ping_pong(client).await,
        "empty_stream" => empty_stream(client).await,
        "custom_metadata" => custom_metadata(client).await,
        "status_code_and_message" => status_code_and_message(client).await,
        "special_status_message" => special_status_message(client).await,
        "unimplemented_method" => unimplemented_method(client).await,
        "unimplemented_service" => unimplemented_service(channel).await,
        "cancel_after_begin" => cancel_after_begin(client).await,
        "cancel_after_first_response" => cancel_after_first_response(client).await,
        "timeout_on_sleeping_server" => timeout_on_sleeping_server(client).await,
        _ => Err(format!(
            "no such test case; the cases are: {} gzip_unary",
            CASES.join(" ")
        )),
    }
}

fn main() {
    let (mut host, mut port, mut cases) = (String::from("127.0.0.1"), 10000u16, Vec::new());
    for arg in std::env::args().skip(1) {
        let Some((name, value)) = arg.split_once('=') else {
            eprintln!("usage: interop_client [--server_host=127.0.0.1] [--server_port=10000] [--test_case=NAME,... | --test_case=all]");
            std::process::exit(2);
        };
        match name {
            "--server_host" => host = value.to_owned(),
            "--server_port" => port = value.parse().expect("--server_port is a number"),
            "--test_case" if value == "all" => cases.extend(CASES.iter().map(|c| c.to_string())),
            "--test_case" => cases.extend(value.split(',').map(str::to_owned)),
            _ => {
                eprintln!("unknown option {name}");
                std::process::exit(2);
            }
        }
    }
    if cases.is_empty() {
        cases.push("large_unary".to_owned());
    }
    let failed = giong::block_on(async move {
        let channel = Channel::new(&format!("http://{host}:{port}")).expect("address");
        let mut failed = 0;
        for case in &cases {
            match run(case, &channel).await {
                Ok(()) => println!("{case}: ok"),
                Err(what) => {
                    println!("{case}: FAILED: {what}");
                    failed += 1;
                }
            }
        }
        failed
    });
    std::process::exit(i32::from(failed > 0));
}
