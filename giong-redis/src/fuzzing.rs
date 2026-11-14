//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-redis/fuzz`): whatever bytes a
//! server sends, reading them ends in replies or an error, never a panic,
//! the size limit holds, and a reply thrown away for its size is called a
//! push message exactly when it is one.

use crate::resp::{Next, ReadBuf, ELEMENT_COST};
use crate::value::Value;
use crate::Config;

/// One reply as [`read_replies`] found it: whole, with what it counted for
/// against the limit, or thrown away for its size, saying whether it was a
/// push message.
pub(crate) type Reply = Result<(Value, usize), bool>;

/// Reads `input` delivered in pieces whose sizes `piece` picks; returns
/// the replies read, and the error that stopped the reading, if one did.
pub(crate) fn read_replies(
    input: &[u8],
    limit: usize,
    mut piece: impl FnMut() -> usize,
) -> (Vec<Reply>, Option<crate::Error>) {
    let mut buf = ReadBuf::new();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let n = piece().clamp(1, input.len() - pos);
        buf.extend(&input[pos..pos + n]);
        pos += n;
        loop {
            match buf.next(limit) {
                Err(e) => return (out, Some(e)),
                Ok(Next::Pending) => break,
                Ok(Next::Ready(len)) => {
                    let cost = cost(buf.peek(len));
                    let pushed = buf.is_push();
                    let v = match buf.take(len) {
                        Ok(v) => v,
                        Err(e) => return (out, Some(e)),
                    };
                    assert_eq!(pushed, matches!(v, Value::Push(_)));
                    out.push(Ok((v, cost)));
                }
                Ok(Next::TooLarge { push }) => out.push(Err(push)),
            }
        }
    }
    (out, None)
}

/// What a whole reply counts for against the size limit: its bytes, and
/// [`ELEMENT_COST`] for each element (attributes and their pairs included),
/// counted here by walking its lines, apart from the reader's own count.
pub(crate) fn cost(reply: &[u8]) -> usize {
    let mut elements = 0;
    let mut pos = 0;
    while pos < reply.len() {
        let end = pos
            + reply[pos..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .expect("a whole reply");
        elements += 1;
        let kind = reply[pos];
        let line = &reply[pos + 1..end];
        pos = end + 2;
        if matches!(kind, b'$' | b'!' | b'=') {
            // As the reader reads it: `-0` and `007` are numbers too, and
            // `-1` is nil.
            let len: i64 = std::str::from_utf8(line).unwrap().parse().unwrap();
            if let Ok(len) = usize::try_from(len) {
                pos += len + 2;
            }
        }
    }
    reply.len() + elements * ELEMENT_COST
}

/// Whether two values are the same, a NaN being the same as a NaN.
pub(crate) fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Double(x), Value::Double(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Array(x), Value::Array(y))
        | (Value::Set(x), Value::Set(y))
        | (Value::Push(x), Value::Push(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((ka, va), (kb, vb))| same(ka, kb) && same(va, vb))
        }
        _ => a == b,
    }
}

/// Reads `input` with `limit`, and again with no limit, each cut in its own
/// way, and checks that they agree: a reply read whole under the limit is
/// the same and within it; one thrown away is over it, and called a push
/// message exactly when it is one.
pub(crate) fn check_against_no_limit(
    input: &[u8],
    limit: usize,
    piece: impl FnMut() -> usize,
    reference_piece: impl FnMut() -> usize,
) {
    let (reference, reference_err) = read_replies(input, usize::MAX, reference_piece);
    let (got, err) = read_replies(input, limit, piece);
    for (i, (r, g)) in reference.iter().zip(&got).enumerate() {
        let Ok((v, cost)) = r else {
            unreachable!("nothing is over no limit")
        };
        match g {
            Ok((w, _)) => {
                assert!(same(v, w), "reply {i}: {v:?} != {w:?}");
                assert!(
                    *cost <= limit,
                    "reply {i} costs {cost} > {limit} and was kept"
                );
            }
            Err(push) => {
                assert!(
                    *cost > limit,
                    "reply {i} costs {cost} <= {limit} and was thrown away"
                );
                assert_eq!(*push, matches!(v, Value::Push(_)), "reply {i}: {v:?}");
            }
        }
    }
    if reference_err.is_none() {
        assert!(err.is_none(), "{err:?} with a limit, none without");
        assert_eq!(got.len(), reference.len());
    }
}

/// A small deterministic generator (xorshift) for the cuts.
fn cuts(seed: u8) -> impl FnMut() -> usize {
    let mut state = u64::from(seed) | 1;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if state % 4 == 0 {
            4096
        } else {
            1 + (state % 9) as usize
        }
    }
}

/// Fuzz entry: two bytes of limit, one byte that sets how the input is
/// cut, then the bytes a server sent.
pub fn check_replies(data: &[u8]) {
    let [a, b, cut, input @ ..] = data else {
        return;
    };
    let limit = match u16::from_le_bytes([*a, *b]) {
        0 => 64 << 20,
        n => usize::from(n),
    };
    check_against_no_limit(input, limit, cuts(*cut), cuts(cut.wrapping_add(1)));
}

/// Fuzz entry: a script for a connection's reply matching (see
/// `client::check_matching`): every command gets its own replies.
pub fn check_connection(data: &[u8]) {
    crate::client::check_matching(data);
}

/// Fuzz entry: a URL. What parses can be shown and used.
pub fn check_url(data: &[u8]) {
    let Ok(url) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(config) = Config::from_url(url) {
        let _ = format!("{config:?}");
    }
}
