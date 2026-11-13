//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-mysql/fuzz`): whatever a server
//! (or a URL) sends, the parsers return a verdict without panicking, and
//! framing respects its limit.

use std::time::SystemTime;

use crate::config::Config;
use crate::connect::{Message, ReadBuf};
use crate::error::DbError;
use crate::protocol;
use crate::row::{Column, RowsBuilder};
use crate::types::{Bits, ColumnType};

/// Runs one packet's payload through every parser of server packets.
pub fn check_payload(input: &[u8]) {
    let _ = protocol::handshake(input);
    let _ = protocol::ok(input);
    let _ = protocol::err(input);
    let _ = DbError::parse(input);
    let _ = protocol::column_def(input);
    let _ = protocol::prepare_ok(input);
    let _ = protocol::is_end_of_rows(input);
}

/// Frames `input` into packets, fed in pieces whose sizes `piece` picks.
pub(crate) fn framing(input: &[u8], limit: usize, mut piece: impl FnMut(usize) -> usize) {
    let mut buf = ReadBuf::new();
    let mut at = 0;
    while at < input.len() {
        let n = piece(input.len() - at).clamp(1, input.len() - at);
        buf.extend(&input[at..at + n]);
        at += n;
        while let Some(m) = buf.next(limit) {
            if let Message::Here { payload, .. } = m {
                assert!(payload.len() <= limit);
                let _ = protocol::ok(buf.bytes(payload.clone()));
                buf.consume(&payload);
            }
        }
    }
}

/// Fuzz entry: a byte choosing the size limit, a byte setting how the
/// input is cut, then the bytes a server sent.
pub fn check_framing(data: &[u8]) {
    let [limit, cut, input @ ..] = data else {
        return;
    };
    let limit = [16, 100, 1 << 20][usize::from(*limit) % 3];
    let mut state = u64::from(*cut) | 1;
    framing(input, limit, move |left| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        1 + (state as usize) % left
    });
}

/// Columns of every type, signed and unsigned, text and binary.
pub(crate) fn all_columns() -> Vec<Column> {
    let mut cols = Vec::new();
    for code in [
        0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 242, 245, 246, 247, 248, 249, 250,
        251, 252, 253, 254, 255, 100,
    ] {
        for (flags, collation) in [(0, 255), (32, 63)] {
            cols.push(Column::for_tests(
                "c",
                ColumnType::from_code(code),
                flags,
                collation,
            ));
        }
    }
    cols
}

/// Reads a row payload with the given columns, and every value in it as
/// every type.
pub(crate) fn rows(payload: &[u8], cols: &[Column]) {
    let n = cols.len();
    let mut b = RowsBuilder::default();
    if b.push(payload, cols, 1 << 20).is_err() {
        return;
    }
    for row in b.finish(cols.to_vec().into()).into_rows() {
        for i in 0..n {
            let _ = row.get::<i64>(i);
            let _ = row.get::<u64>(i);
            let _ = row.get::<i8>(i);
            let _ = row.get::<bool>(i);
            let _ = row.get::<f64>(i);
            let _ = row.get::<f32>(i);
            let _ = row.get::<&str>(i);
            let _ = row.get::<String>(i);
            let _ = row.get::<&[u8]>(i);
            let _ = row.get::<Bits>(i);
            let _ = row.get::<SystemTime>(i);
            let _ = row.get::<Option<String>>(i);
        }
        let _ = format!("{row:?}");
    }
}

/// Fuzz entry: a byte with the number of columns (1 to 4), a byte for each
/// column's type, then a row payload.
pub fn check_rows(data: &[u8]) {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let n = 1 + usize::from(n) % 4;
    if rest.len() < n {
        return;
    }
    let all = all_columns();
    let cols: Vec<Column> = rest[..n]
        .iter()
        .map(|&b| all[usize::from(b) % all.len()].clone())
        .collect();
    rows(&rest[n..], &cols);
}

/// Fuzz entry: a connection URL.
pub fn check_url(data: &[u8]) {
    if let Ok(config) = Config::from_url(&String::from_utf8_lossy(data)) {
        let _ = format!("{config:?}");
    }
}
