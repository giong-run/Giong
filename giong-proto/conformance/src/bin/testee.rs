//! The testee that Google's `conformance_test_runner` drives: reads
//! `ConformanceRequest`s on stdin, answers `ConformanceResponse`s on stdout.
//!
//! Binary input and output only; JSON and text format are answered with
//! `skipped` (giong-proto has neither yet).

use std::io::{self, Read, Write};

use giong_proto::wire;
use giong_proto::View;
use giong_proto_conformance::pb;
use pb::conformance::conformance_request::PayloadView as Payload;
use pb::conformance::{ConformanceRequestView, WireFormat};

enum Answer {
    ParseError(String),
    SerializeError(String),
    Payload(Vec<u8>),
    Skipped(&'static str),
}

fn answer(req: &ConformanceRequestView<'_>) -> Answer {
    let Some(Payload::ProtobufPayload(payload)) = req.payload() else {
        return Answer::Skipped("giong-proto reads the binary format only");
    };
    if req.requested_output_format() != WireFormat::Protobuf {
        return Answer::Skipped("giong-proto writes the binary format only");
    }
    macro_rules! round_trip {
        ($view:ty) => {
            match <$view>::decode(payload) {
                Ok(v) => match {
                    let mut b = Vec::new();
                    v.encode_canonical(&mut b);
                    Ok::<_, giong_proto::Error>(b)
                } {
                    Ok(out) => Answer::Payload(out),
                    Err(e) => Answer::SerializeError(e.to_string()),
                },
                Err(e) => Answer::ParseError(e.to_string()),
            }
        };
    }
    match req.message_type() {
        "protobuf_test_messages.proto3.TestAllTypesProto3" => {
            round_trip!(pb::protobuf_test_messages::proto3::TestAllTypesProto3View)
        }
        "protobuf_test_messages.proto2.TestAllTypesProto2" => {
            round_trip!(pb::protobuf_test_messages::proto2::TestAllTypesProto2View)
        }
        "conformance.FailureSet" => round_trip!(pb::conformance::FailureSetView),
        _ => Answer::Skipped("message type not built into the testee (editions)"),
    }
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut buf = Vec::new();
    loop {
        let mut len = [0u8; 4];
        match input.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        buf.resize(u32::from_le_bytes(len) as usize, 0);
        input.read_exact(&mut buf)?;
        let resp = match ConformanceRequestView::decode(&buf) {
            Ok(req) => answer(&req),
            Err(e) => Answer::ParseError(format!("bad request: {e}")),
        };
        // ConformanceResponse, written by hand: a oneof of one field.
        let mut out = Vec::new();
        match resp {
            Answer::ParseError(s) => wire::put_bytes(&mut out, 1, s.as_bytes()),
            Answer::SerializeError(s) => wire::put_bytes(&mut out, 6, s.as_bytes()),
            Answer::Payload(p) => wire::put_bytes(&mut out, 3, &p),
            Answer::Skipped(s) => wire::put_bytes(&mut out, 5, s.as_bytes()),
        }
        output.write_all(&(out.len() as u32).to_le_bytes())?;
        output.write_all(&out)?;
        output.flush()?;
    }
}
