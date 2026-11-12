//! What the parsers must guarantee for any input. Shared by the randomised
//! tests in `fuzz_tests.rs` and by the coverage-guided fuzz targets in
//! `giong-http/fuzz` (through the hidden `fuzzing` module).

use crate::chunked::{Decoder, Progress};
use crate::parse::{find_head_end, header_lines, parse_head};

/// Checks the head parser on `input`. `from` picks an earlier offset to search
/// from (taken modulo the possible range). Returns `true` if a complete head
/// was found and accepted.
pub fn check_head(input: &[u8], from: usize) -> bool {
    let Some(end) = find_head_end(input, 0) else {
        return false;
    };
    // Searching from any earlier offset finds the same terminator.
    let from = from % (end.saturating_sub(3) + 1);
    assert_eq!(find_head_end(input, from), Some(end));

    let head = &input[..end];
    match parse_head(head) {
        Ok(parsed) => {
            assert_eq!(parsed.len, end);
            // Everything a `Request` later slices out must be in bounds and
            // valid UTF-8 where it is exposed as `&str`.
            assert!(std::str::from_utf8(&head[parsed.method.clone()]).is_ok());
            assert!(std::str::from_utf8(&head[parsed.target.clone()]).is_ok());
            for (name, value) in header_lines(&head[parsed.headers.clone()]) {
                assert!(std::str::from_utf8(name).is_ok());
                assert!(!value.contains(&b'\r') && !value.contains(&b'\n'));
            }
            // An accepted head never has a stray line break or NUL.
            let lines = &head[..end - 4];
            assert!(!lines.contains(&0));
            assert_eq!(
                lines.iter().filter(|&&b| b == b'\r').count(),
                lines.iter().filter(|&&b| b == b'\n').count()
            );
            assert!(!(parsed.chunked && parsed.content_length.is_some()));
            true
        }
        Err(status) => {
            assert!(matches!(status, 400 | 417 | 501 | 505), "{status}");
            false
        }
    }
}

/// Checks the chunked decoder on `input` with body limit `max`: decoding it in
/// one piece and in the pieces `splits` produces (sizes, taken modulo 6, plus
/// one) must give the same answer. Returns `true` if the body was complete.
pub fn check_chunked(input: &[u8], max: usize, mut splits: impl FnMut() -> usize) -> bool {
    let mut whole = Decoder::new(max);
    let expected = whole.feed(input);

    // The same bytes, delivered in pieces.
    let mut pieces = Decoder::new(max);
    let mut end = 0;
    let got = loop {
        end = (end + 1 + splits() % 6).min(input.len());
        match pieces.feed(&input[..end]) {
            Ok(Progress::NeedMore) if end < input.len() => continue,
            other => break other,
        }
    };
    assert_eq!(got, expected, "input {:?}", String::from_utf8_lossy(input));
    if let Ok(Progress::Done(used)) = expected {
        assert!(used <= input.len());
        assert!(whole.body.len() <= max);
        assert_eq!(whole.body, pieces.body);
        true
    } else {
        false
    }
}
