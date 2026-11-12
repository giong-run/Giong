//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-postgres/fuzz`): whatever a server
//! (or a URL) sends, the parsers return a verdict without panicking.

use crate::auth::Scram;
use crate::config::Config;
use crate::connect::ReadBuf;
use crate::error::DbError;
use crate::protocol::{data_row, parse_backend};
use crate::row::RowsBuilder;

/// Frames `input` into messages, delivered in two pieces cut at `cut`, and
/// runs every message through every parser that reads server messages.
pub(crate) fn messages(input: &[u8], cut: usize) {
    let cut = cut.min(input.len());
    let mut buf = ReadBuf::new();
    buf.extend(&input[..cut]);
    let mut fed_rest = false;
    loop {
        let Ok(Some((tag, len))) = buf.peek() else {
            if fed_rest {
                break;
            }
            buf.extend(&input[cut..]);
            fed_rest = true;
            continue;
        };
        if len > 1 << 20 {
            buf.skip(len);
            continue;
        }
        let Some(body) = buf.body(len) else {
            if fed_rest {
                break;
            }
            buf.extend(&input[cut..]);
            fed_rest = true;
            continue;
        };
        let bytes = buf.bytes(body).to_vec();
        buf.consume(len);
        if let Ok(msg) = parse_backend(tag, &bytes) {
            let _ = format!("{msg:?}");
        }
        let _ = DbError::parse(&bytes);
        let mut rows = RowsBuilder::default();
        let _ = rows.push(&bytes, 2, 1 << 16);
        let _ = data_row(&bytes, 1, |_| {});
    }
}

/// Fuzz entry: two bytes saying where the input is cut, then the bytes a
/// server sent.
pub fn check_messages(data: &[u8]) {
    let [a, b, input @ ..] = data else {
        return;
    };
    messages(input, usize::from(u16::from_le_bytes([*a, *b])));
}

/// Fuzz entry: a connection URL.
pub fn check_url(data: &[u8]) {
    if let Ok(url) = std::str::from_utf8(data) {
        if let Ok(config) = Config::from_url(url) {
            let _ = format!("{config:?}");
        }
    }
}

/// Runs a SCRAM exchange against a server's first and final messages.
pub(crate) fn scram(server_first: &[u8], server_final: &[u8]) {
    // Keep the key derivation cheap: the iteration limit itself is tested
    // in the auth module.
    let text = String::from_utf8_lossy(server_first);
    let iterations = text.rsplit("i=").next().and_then(|i| i.parse::<u64>().ok());
    if iterations.is_some_and(|i| i > 4096) {
        return;
    }
    let mut scram = Scram::with_nonce("pw", "", "abcdef");
    let _ = scram.client_final(server_first);
    let _ = scram.finish(server_final);
}

/// Fuzz entry: the server's first SCRAM message, a 0xff byte, and its
/// final message.
pub fn check_scram(data: &[u8]) {
    let (first, last) = match data.iter().position(|&b| b == 0xff) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    scram(first, last);
}
