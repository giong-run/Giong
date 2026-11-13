//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-redis/fuzz`): whatever bytes a
//! server sends, reading them ends in replies or an error, never a panic,
//! and the size limit holds.

use crate::resp::{Next, ReadBuf};
use crate::value::Value;
use crate::Config;

/// Reads `input` delivered in pieces whose sizes `piece` picks; returns
/// the replies (`Err(push)` for one over `limit`, `push` saying whether it
/// was a push message) or the first error.
pub(crate) fn read_replies(
    input: &[u8],
    limit: usize,
    mut piece: impl FnMut() -> usize,
) -> Result<Vec<Result<Value, bool>>, crate::Error> {
    let mut buf = ReadBuf::new();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let n = piece().clamp(1, input.len() - pos);
        buf.extend(&input[pos..pos + n]);
        pos += n;
        loop {
            match buf.next(limit)? {
                Next::Pending => break,
                Next::Ready(len) => {
                    let pushed = buf.is_push();
                    let v = buf.take(len)?;
                    assert_eq!(pushed, matches!(v, Value::Push(_)));
                    out.push(Ok(v));
                }
                Next::TooLarge { push } => out.push(Err(push)),
            }
        }
    }
    Ok(out)
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
    let mut state = u64::from(*cut) | 1;
    let piece = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if state % 4 == 0 {
            4096
        } else {
            1 + (state % 9) as usize
        }
    };
    if let Ok(replies) = read_replies(input, limit, piece) {
        for v in replies.into_iter().flatten() {
            let _ = format!("{v:?}");
        }
    }
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
