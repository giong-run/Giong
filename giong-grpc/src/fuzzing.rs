//! What the fuzz targets check about the code that reads bytes from the
//! other end of a call. Each function panics when a property does not
//! hold.

use std::time::Duration;

use crate::codec::{
    decode_message, encode_message, format_timeout, gzip_compress, gzip_decompress, parse_timeout,
    Deframer,
};
use crate::metadata::{base64_decode, base64_encode, Metadata};
use crate::{Code, Status};

/// Every message of `input` under the limit `max`, or the status the
/// stream fails with, however `splits` cuts the input into pieces.
fn deframe(
    input: &[u8],
    max: usize,
    gzip: bool,
    mut splits: impl FnMut() -> usize,
) -> (Vec<(Vec<u8>, bool)>, Option<Status>) {
    let mut deframer = Deframer::new(max, gzip);
    let mut messages = Vec::new();
    let mut at = 0;
    loop {
        loop {
            match deframer.next() {
                Ok(Some(payload)) => {
                    assert!(payload.as_slice().len() <= max);
                    messages.push((payload.as_slice().to_vec(), payload.was_compressed()));
                }
                Ok(None) => break,
                Err(status) => return (messages, Some(status)),
            }
        }
        if at == input.len() {
            return (messages, deframer.finish().err());
        }
        let end = (at + 1 + splits() % 64).min(input.len());
        deframer.push(input[at..end].to_vec());
        at = end;
    }
}

/// The message framing: never panics, gives no message past the limit,
/// and the same messages and the same failure whether the stream arrives
/// whole or in pieces. Returns the number of messages.
pub fn check_framing(input: &[u8], max: usize, gzip: bool, splits: impl FnMut() -> usize) -> usize {
    let (whole, whole_end) = deframe(input, max, gzip, || usize::MAX - 1);
    let (pieces, pieces_end) = deframe(input, max, gzip, splits);
    assert_eq!(whole, pieces);
    assert_eq!(
        whole_end.as_ref().map(|s| (s.code(), s.error_code())),
        pieces_end.as_ref().map(|s| (s.code(), s.error_code())),
    );
    if let Some(status) = &whole_end {
        assert!(matches!(
            status.code(),
            Code::Internal | Code::ResourceExhausted
        ));
        assert!(status.error_code().is_some());
    }
    whole.len()
}

/// gzip: reading never panics and never gives more than `max` bytes, and
/// what this side writes it reads back.
pub fn check_gzip(input: &[u8], max: usize) -> bool {
    let read = gzip_decompress(input, max);
    if let Ok(plain) = &read {
        assert!(plain.len() <= max);
    }
    let packed = gzip_compress(input);
    let back = gzip_decompress(&packed, input.len()).expect("our own gzip");
    assert_eq!(back, input);
    if !input.is_empty() {
        // One byte short of what it holds: refused as too large.
        let short = gzip_decompress(&packed, input.len() - 1).expect_err("past the limit");
        assert_eq!(short.code(), Code::ResourceExhausted);
    }
    read.is_ok()
}

/// `grpc-timeout`: parsing never panics, accepts only digits and a unit,
/// and a timeout written for sending reads back as no less than it was.
pub fn check_timeout(input: &[u8]) -> bool {
    let Some(parsed) = parse_timeout(input) else {
        return false;
    };
    assert!((2..=9).contains(&input.len()));
    assert!(input[..input.len() - 1].iter().all(u8::is_ascii_digit));
    let written = format_timeout(parsed);
    assert!(written.len() <= 9, "{written}");
    let back = parse_timeout(written.as_bytes()).expect("our own timeout");
    assert!(back >= parsed, "{parsed:?} written as {written}");
    // Rounded up by less than one unit of what was written.
    assert!(back - parsed < Duration::from_secs(3600));
    true
}

/// `grpc-message`: decoding never panics and always gives text, and any
/// text survives being encoded and decoded.
pub fn check_status_message(input: &[u8]) -> bool {
    let decoded = decode_message(input);
    let encoded = encode_message(&decoded);
    assert!(encoded.bytes().all(|b| (0x20..=0x7e).contains(&b)));
    assert_eq!(decode_message(encoded.as_bytes()), decoded);
    match std::str::from_utf8(input) {
        Ok(text) => {
            assert_eq!(decode_message(encode_message(text).as_bytes()), text);
            true
        }
        Err(_) => false,
    }
}

/// Metadata as it arrives: base64 never panics and reads back what this
/// side writes, and a received field is kept under the name it came with
/// or refused.
pub fn check_metadata(name: &[u8], value: &[u8]) -> bool {
    assert_eq!(
        base64_decode(base64_encode(value).as_bytes()).as_deref(),
        Some(value)
    );
    if let Some(bytes) = base64_decode(value) {
        // Padded or not, it holds three bytes for every four characters.
        assert!(bytes.len() <= value.len() * 3 / 4 + 3);
    }
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    let mut metadata = Metadata::new();
    match metadata.received(name, value) {
        Ok(()) => {
            // One value, or the several a `-bin` field may carry.
            assert!(metadata.len() <= 1 || name.ends_with("-bin"));
            for (kept, bytes) in metadata.iter() {
                assert_eq!(kept, name);
                if !name.ends_with("-bin") {
                    assert_eq!(bytes, value);
                }
            }
            // What was read can be sent on.
            for (kept, wire) in metadata.wire() {
                assert_eq!(kept, name);
                let _ = wire.as_bytes();
            }
            true
        }
        Err(refused) => {
            assert_eq!(refused, name);
            assert!(name.ends_with("-bin"));
            false
        }
    }
}
