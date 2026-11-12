//! RESP, Redis's wire format: reading replies as they arrive.
//!
//! A reply is found in two passes. [`ReadBuf::next`] scans what has arrived
//! for the end of the next reply without building anything, and remembers
//! where it got to, so a reply that arrives in many pieces is scanned once.
//! Then [`ReadBuf::take`] builds the [`Value`] from bytes known to be whole,
//! or [`ReadBuf::skip`] passes over a reply nobody waits for.
//!
//! A reply larger than the limit is thrown away as it arrives, without
//! being kept, and reported as [`Next::TooLarge`]: the connection stays in
//! step with the server.

use crate::error::{Error, ServerError};
use crate::value::Value;

/// Aggregates nested deeper than this are refused.
pub(crate) const MAX_DEPTH: usize = 128;

/// The longest line (a simple string, an error, a number) accepted.
const MAX_LINE: usize = 1 << 20;

/// An aggregate claiming more elements than this is refused.
const MAX_COUNT: u64 = 1 << 32;

/// The buffer's size to begin with, and after a large reply.
const INITIAL_CAPACITY: usize = 16 * 1024;

/// A buffer grown past this by a large reply shrinks back once it is empty.
const SHRINK_ABOVE: usize = 1 << 20;

/// What [`ReadBuf::next`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The next reply has not arrived whole yet.
    Pending,
    /// The next reply is here, this many bytes long: take or skip it.
    Ready(usize),
    /// A reply larger than the limit arrived and was thrown away.
    TooLarge,
}

/// Bytes received and not yet taken apart into replies.
pub(crate) struct ReadBuf {
    data: Vec<u8>,
    start: usize,
    scan: Scan,
}

#[derive(Default)]
struct Scan {
    /// How far the reply in progress has been scanned, from `start`.
    pos: usize,
    /// For each aggregate open around `pos`, how many elements it still
    /// needs.
    stack: Vec<u64>,
    /// The reply in progress is over the limit and being thrown away.
    discarding: bool,
    /// Bytes of a string being thrown away that have not arrived yet.
    skip: usize,
    /// A string being thrown away has ended: one element is complete.
    skipped_one: bool,
}

/// Finds `\r\n` at or after `from`; returns the index of `\r`.
fn line_end(buf: &[u8], from: usize) -> Option<usize> {
    let rest = buf.get(from..)?;
    let mut i = 0;
    while let Some(off) = rest[i..].iter().position(|&b| b == b'\r') {
        let at = i + off;
        match rest.get(at + 1) {
            Some(b'\n') => return Some(from + at),
            Some(_) => i = at + 1,
            None => return None,
        }
    }
    None
}

/// Parses the decimal number of a header line (`-1` allowed): what
/// `str::parse::<i64>` accepts, except a leading `+`.
fn number(line: &[u8]) -> Option<i64> {
    let (negative, digits) = match line.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, line),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &b in digits {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(d))?;
    }
    if negative {
        // -2^63 fits, as i64::MIN.
        (n <= 1 << 63).then(|| (n as i64).wrapping_neg())
    } else {
        i64::try_from(n).ok()
    }
}

fn bad(what: impl std::fmt::Display) -> Error {
    Error::protocol(format!("the server sent {what}"))
}

impl Default for ReadBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadBuf {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::with_capacity(INITIAL_CAPACITY),
            start: 0,
            scan: Scan::default(),
        }
    }

    /// Adds bytes that arrived.
    pub(crate) fn extend(&mut self, mut chunk: &[u8]) {
        if self.scan.skip > 0 {
            let n = self.scan.skip.min(chunk.len());
            self.scan.skip -= n;
            chunk = &chunk[n..];
        }
        if self.start == self.data.len() {
            self.data.clear();
            self.start = 0;
            if self.data.capacity() > SHRINK_ABOVE {
                // A large reply has gone: give its memory back.
                self.data = Vec::with_capacity(INITIAL_CAPACITY);
            }
        } else if self.start > 0 && self.data.capacity() - self.data.len() < chunk.len() {
            // No room at the end. Moving the part of a reply that is left
            // to the front costs a copy of it; growing instead costs a copy
            // of everything, once. Move when what is left is small, so that
            // each byte is moved a third of a time at most, on average.
            let live = self.data.len() - self.start;
            if live * 4 <= self.data.capacity() {
                self.data.drain(..self.start);
                self.start = 0;
            }
        }
        self.data.extend_from_slice(chunk);
    }

    /// Whether nothing is waiting to be read.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.data.len() && self.scan.skip == 0 && self.scan.stack.is_empty()
    }

    /// Looks for the end of the next reply. A reply longer than `limit`
    /// bytes is thrown away as it arrives and reported as
    /// [`Next::TooLarge`].
    pub(crate) fn next(&mut self, limit: usize) -> Result<Next, Error> {
        let scan = &mut self.scan;
        loop {
            if scan.skip > 0 {
                return Ok(Next::Pending);
            }
            if std::mem::take(&mut scan.skipped_one) && scan.complete_one() {
                return Ok(scan.finish(&mut self.start, limit));
            }
            let buf = &self.data[self.start..];
            if !scan.discarding && scan.pos > limit {
                scan.discard(&mut self.start);
                continue;
            }
            let pos = scan.pos;
            let Some(end) = line_end(buf, pos + 1) else {
                if buf.len() - pos > MAX_LINE {
                    return Err(bad(format!("a line of more than {MAX_LINE} bytes")));
                }
                if scan.discarding {
                    // Keep only the line in progress.
                    self.start += pos;
                    scan.pos = 0;
                }
                return Ok(Next::Pending);
            };
            let Some(&kind) = buf.get(pos) else {
                return Ok(Next::Pending);
            };
            let line = &buf[pos + 1..end];
            let after = end + 2;
            match kind {
                b'+' | b'-' | b':' | b',' | b'#' | b'_' | b'(' => {
                    scan.pos = after;
                    if scan.complete_one() {
                        return Ok(scan.finish(&mut self.start, limit));
                    }
                }
                b'$' | b'!' | b'=' => {
                    let len = match number(line) {
                        Some(-1) if kind == b'$' => {
                            scan.pos = after;
                            if scan.complete_one() {
                                return Ok(scan.finish(&mut self.start, limit));
                            }
                            continue;
                        }
                        Some(n) if n >= 0 => n as usize,
                        _ => return Err(bad(format!("a bad length {:?}", lossy(line)))),
                    };
                    let total = len
                        .checked_add(2)
                        .ok_or_else(|| bad("an impossible length"))?;
                    let have = buf.len() - after;
                    if have >= total {
                        if !scan.discarding && &buf[after + len..after + total] != b"\r\n" {
                            return Err(bad("a string not followed by CRLF"));
                        }
                        scan.pos = after + total;
                        if scan.complete_one() {
                            return Ok(scan.finish(&mut self.start, limit));
                        }
                    } else if scan.discarding || after + total > limit {
                        // Throw away what is here, and the rest as it comes.
                        if !scan.discarding {
                            scan.discarding = true;
                        }
                        self.start += buf.len();
                        scan.pos = 0;
                        scan.skip = total - have;
                        scan.skipped_one = true;
                        return Ok(Next::Pending);
                    } else {
                        return Ok(Next::Pending);
                    }
                }
                b'*' | b'~' | b'>' | b'%' | b'|' => {
                    let n = match number(line) {
                        Some(-1) if kind == b'*' => {
                            scan.pos = after;
                            if scan.complete_one() {
                                return Ok(scan.finish(&mut self.start, limit));
                            }
                            continue;
                        }
                        Some(n) if n >= 0 && (n as u64) < MAX_COUNT => n as u64,
                        _ => return Err(bad(format!("a bad element count {:?}", lossy(line)))),
                    };
                    let elements = match kind {
                        b'%' => 2 * n,
                        // An attribute's pairs, then the value they describe.
                        b'|' => 2 * n + 1,
                        _ => n,
                    };
                    scan.pos = after;
                    if elements == 0 {
                        if scan.complete_one() {
                            return Ok(scan.finish(&mut self.start, limit));
                        }
                    } else {
                        if scan.stack.len() >= MAX_DEPTH {
                            return Err(bad(format!(
                                "aggregates nested more than {MAX_DEPTH} deep"
                            )));
                        }
                        scan.stack.push(elements);
                    }
                }
                _ => {
                    return Err(bad(format!(
                        "a reply starting with {:?}, which is not a RESP type",
                        char::from(kind)
                    )))
                }
            }
        }
    }

    /// Whether the reply found by [`next`](Self::next) is a push message.
    pub(crate) fn is_push(&self) -> bool {
        let mut pos = self.start;
        // Attributes come before what they describe.
        while self.data.get(pos) == Some(&b'|') {
            pos += 1;
            let Some(pairs) = line(&self.data, &mut pos).ok().and_then(number) else {
                return false;
            };
            for _ in 0..2 * pairs {
                if parse(&self.data, &mut pos, 0).is_err() {
                    return false;
                }
            }
        }
        self.data.get(pos) == Some(&b'>')
    }

    /// Builds the reply of `len` bytes found by [`next`](Self::next) and
    /// moves past it.
    pub(crate) fn take(&mut self, len: usize) -> Result<Value, Error> {
        let bytes = &self.data[self.start..self.start + len];
        let mut pos = 0;
        let value = parse(bytes, &mut pos, 0)?;
        if pos != len {
            return Err(bad("a reply whose parts do not add up"));
        }
        self.start += len;
        Ok(value)
    }

    /// Moves past the reply of `len` bytes found by [`next`](Self::next).
    pub(crate) fn skip(&mut self, len: usize) {
        self.start += len;
    }
}

impl Scan {
    /// Counts one element as complete; returns whether the whole reply is.
    fn complete_one(&mut self) -> bool {
        loop {
            match self.stack.last_mut() {
                None => return true,
                Some(left) => {
                    *left -= 1;
                    if *left > 0 {
                        return false;
                    }
                    self.stack.pop();
                }
            }
        }
    }

    /// Ends the reply: what to report, and resets for the next one.
    fn finish(&mut self, start: &mut usize, limit: usize) -> Next {
        let pos = std::mem::take(&mut self.pos);
        if std::mem::take(&mut self.discarding) || pos > limit {
            *start += pos;
            Next::TooLarge
        } else {
            Next::Ready(pos)
        }
    }

    /// Starts throwing away the reply in progress.
    fn discard(&mut self, start: &mut usize) {
        self.discarding = true;
        *start += self.pos;
        self.pos = 0;
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.len().min(40)]).into_owned()
}

/// The line at `*pos` (after the type byte), moving past its CRLF.
fn line<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8], Error> {
    let end = line_end(buf, *pos).ok_or_else(|| bad("a line without CRLF"))?;
    let line = &buf[*pos..end];
    *pos = end + 2;
    Ok(line)
}

fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

/// Builds the value at `*pos` from bytes known to hold it whole.
fn parse(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Value, Error> {
    if depth > MAX_DEPTH {
        return Err(bad("aggregates nested too deep"));
    }
    let kind = *buf.get(*pos).ok_or_else(|| bad("a reply cut short"))?;
    *pos += 1;
    let head = line(buf, pos)?;
    Ok(match kind {
        b'+' => Value::Status(text(head)),
        b'-' => Value::Error(ServerError::new(text(head))),
        b':' => {
            Value::Int(number(head).ok_or_else(|| bad(format!("a bad integer {:?}", lossy(head))))?)
        }
        b',' => {
            let s = std::str::from_utf8(head).map_err(|_| bad("a double that is not text"))?;
            Value::Double(s.parse().map_err(|_| bad(format!("a bad double {s:?}")))?)
        }
        b'#' => match head {
            b"t" => Value::Bool(true),
            b"f" => Value::Bool(false),
            _ => return Err(bad(format!("a bad boolean {:?}", lossy(head)))),
        },
        b'_' => {
            if !head.is_empty() {
                return Err(bad("a null with something after it"));
            }
            Value::Nil
        }
        b'(' => {
            let digits = head.strip_prefix(b"-").unwrap_or(head);
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return Err(bad(format!("a bad big number {:?}", lossy(head))));
            }
            Value::BigNumber(text(head))
        }
        b'$' | b'!' | b'=' => {
            let n = number(head).ok_or_else(|| bad("a bad length"))?;
            if n == -1 && kind == b'$' {
                return Ok(Value::Nil);
            }
            let n = usize::try_from(n).map_err(|_| bad("a bad length"))?;
            let body = buf
                .get(*pos..*pos + n)
                .ok_or_else(|| bad("a string cut short"))?;
            if buf.get(*pos + n..*pos + n + 2) != Some(b"\r\n") {
                return Err(bad("a string not followed by CRLF"));
            }
            *pos += n + 2;
            match kind {
                b'$' => Value::Bulk(body.to_vec()),
                b'!' => Value::Error(ServerError::new(text(body))),
                _ => {
                    if body.len() < 4 || body[3] != b':' {
                        return Err(bad("a verbatim string without its format"));
                    }
                    Value::Verbatim {
                        format: text(&body[..3]),
                        text: text(&body[4..]),
                    }
                }
            }
        }
        b'*' | b'~' | b'>' | b'%' | b'|' => {
            let n = number(head).ok_or_else(|| bad("a bad element count"))?;
            if n == -1 && kind == b'*' {
                return Ok(Value::Nil);
            }
            let n = usize::try_from(n).map_err(|_| bad("a bad element count"))?;
            // Never trust the count for the allocation: each element takes
            // at least three bytes.
            let room = (buf.len() - *pos) / 3;
            match kind {
                b'%' | b'|' => {
                    let mut pairs = Vec::with_capacity(n.min(room / 2));
                    for _ in 0..n {
                        let k = parse(buf, pos, depth + 1)?;
                        let v = parse(buf, pos, depth + 1)?;
                        pairs.push((k, v));
                    }
                    if kind == b'|' {
                        // The attribute is dropped; the value it describes
                        // is the reply.
                        return parse(buf, pos, depth);
                    }
                    Value::Map(pairs)
                }
                _ => {
                    let mut items = Vec::with_capacity(n.min(room));
                    for _ in 0..n {
                        items.push(parse(buf, pos, depth + 1)?);
                    }
                    match kind {
                        b'*' => Value::Array(items),
                        b'~' => Value::Set(items),
                        _ => Value::Push(items),
                    }
                }
            }
        }
        _ => {
            return Err(bad(format!(
                "a reply starting with {:?}, which is not a RESP type",
                char::from(kind)
            )))
        }
    })
}

/// Writes a value in RESP3, for tests and for the fuzz tests' round trips.
#[cfg(test)]
pub(crate) fn encode(v: &Value, out: &mut Vec<u8>) {
    fn bulk(out: &mut Vec<u8>, kind: u8, b: &[u8]) {
        out.push(kind);
        out.extend_from_slice(format!("{}\r\n", b.len()).as_bytes());
        out.extend_from_slice(b);
        out.extend_from_slice(b"\r\n");
    }
    match v {
        Value::Nil => out.extend_from_slice(b"_\r\n"),
        Value::Int(n) => out.extend_from_slice(format!(":{n}\r\n").as_bytes()),
        Value::Bulk(b) => bulk(out, b'$', b),
        Value::Status(s) => out.extend_from_slice(format!("+{s}\r\n").as_bytes()),
        Value::Array(a) | Value::Set(a) | Value::Push(a) => {
            let kind = match v {
                Value::Array(_) => '*',
                Value::Set(_) => '~',
                _ => '>',
            };
            out.extend_from_slice(format!("{kind}{}\r\n", a.len()).as_bytes());
            for e in a {
                encode(e, out);
            }
        }
        Value::Map(m) => {
            out.extend_from_slice(format!("%{}\r\n", m.len()).as_bytes());
            for (k, e) in m {
                encode(k, out);
                encode(e, out);
            }
        }
        Value::Double(d) => out.extend_from_slice(format!(",{d}\r\n").as_bytes()),
        Value::Bool(b) => out.extend_from_slice(if *b { b"#t\r\n" } else { b"#f\r\n" }),
        Value::BigNumber(n) => out.extend_from_slice(format!("({n}\r\n").as_bytes()),
        Value::Verbatim { format, text } => bulk(out, b'=', format!("{format}:{text}").as_bytes()),
        Value::Error(e) => bulk(out, b'!', e.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8], limit: usize, piece: usize) -> Result<Vec<Result<Value, ()>>, Error> {
        let mut buf = ReadBuf::new();
        let mut out = Vec::new();
        for chunk in input.chunks(piece.max(1)) {
            buf.extend(chunk);
            loop {
                match buf.next(limit)? {
                    Next::Pending => break,
                    Next::Ready(n) => out.push(Ok(buf.take(n)?)),
                    Next::TooLarge => out.push(Err(())),
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn header_numbers_read_as_str_parse_reads_them() {
        let cases = [
            "0",
            "-0",
            "1",
            "-1",
            "007",
            "-007",
            "12345",
            "",
            "-",
            "+1",
            "1-",
            "--1",
            "1 ",
            " 1",
            "1a",
            "a",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775808",
            "-9223372036854775809",
            "18446744073709551616",
            "000000000000000000000000001",
        ];
        for c in cases {
            let want = if c.starts_with('+') {
                None
            } else {
                c.parse::<i64>().ok()
            };
            assert_eq!(number(c.as_bytes()), want, "{c:?}");
        }
        assert_eq!(number(b"\xff1"), None);
    }

    #[test]
    fn the_buffer_does_not_grow_without_bound_or_copy_much() {
        // Replies a little larger than the chunks they arrive in, as 8 KiB
        // values do in 8 KiB receive buffers.
        let value = vec![b'x'; 8192];
        let mut reply = format!("${}\r\n", value.len()).into_bytes();
        reply.extend_from_slice(&value);
        reply.extend_from_slice(b"\r\n");
        let stream: Vec<u8> = reply
            .iter()
            .copied()
            .cycle()
            .take(reply.len() * 200)
            .collect();
        let mut buf = ReadBuf::new();
        let mut got = 0;
        for chunk in stream.chunks(8192) {
            buf.extend(chunk);
            while let Next::Ready(n) = buf.next(1 << 20).unwrap() {
                assert_eq!(buf.take(n).unwrap(), Value::Bulk(value.clone()));
                got += 1;
            }
            assert!(buf.data.capacity() <= 128 * 1024, "{}", buf.data.capacity());
        }
        assert_eq!(got, 200);

        // A large reply's memory is given back once it is read.
        let big = format!("${}\r\n{}\r\n", 3 << 20, "y".repeat(3 << 20));
        buf.extend(big.as_bytes());
        let Next::Ready(n) = buf.next(4 << 20).unwrap() else {
            panic!()
        };
        buf.take(n).unwrap();
        buf.extend(b"+OK\r\n");
        assert!(
            buf.data.capacity() <= INITIAL_CAPACITY,
            "{}",
            buf.data.capacity()
        );
        assert_eq!(buf.next(1 << 20).unwrap(), Next::Ready(5));
    }

    fn one(input: &str) -> Value {
        let mut got = read_all(input.as_bytes(), 1 << 20, input.len()).unwrap();
        assert_eq!(got.len(), 1, "{input:?}");
        got.pop().unwrap().unwrap()
    }

    #[test]
    fn every_resp3_type_is_read() {
        assert_eq!(one("+OK\r\n"), Value::Status("OK".into()));
        assert_eq!(one(":-12\r\n"), Value::Int(-12));
        assert_eq!(one("$5\r\nhello\r\n"), Value::Bulk(b"hello".to_vec()));
        assert_eq!(one("$0\r\n\r\n"), Value::Bulk(vec![]));
        assert_eq!(one("$-1\r\n"), Value::Nil);
        assert_eq!(one("*-1\r\n"), Value::Nil);
        assert_eq!(one("_\r\n"), Value::Nil);
        assert_eq!(one(",3.25\r\n"), Value::Double(3.25));
        assert_eq!(one(",inf\r\n"), Value::Double(f64::INFINITY));
        assert_eq!(one(",-inf\r\n"), Value::Double(f64::NEG_INFINITY));
        assert!(matches!(one(",nan\r\n"), Value::Double(d) if d.is_nan()));
        assert_eq!(one("#t\r\n"), Value::Bool(true));
        assert_eq!(
            one("(3492890328409238509324850943850943825024385\r\n"),
            Value::BigNumber("3492890328409238509324850943850943825024385".into())
        );
        assert_eq!(
            one("=15\r\ntxt:Some string\r\n"),
            Value::Verbatim {
                format: "txt".into(),
                text: "Some string".into()
            }
        );
        assert_eq!(
            one("-WRONGTYPE Operation\r\n"),
            Value::Error(ServerError::new("WRONGTYPE Operation".into()))
        );
        assert_eq!(
            one("!21\r\nSYNTAX invalid syntax\r\n"),
            Value::Error(ServerError::new("SYNTAX invalid syntax".into()))
        );
        assert_eq!(
            one("*2\r\n:1\r\n*1\r\n$1\r\na\r\n"),
            Value::Array(vec![
                Value::Int(1),
                Value::Array(vec![Value::Bulk(b"a".to_vec())])
            ])
        );
        assert_eq!(
            one("%1\r\n+k\r\n:2\r\n"),
            Value::Map(vec![(Value::Status("k".into()), Value::Int(2))])
        );
        assert_eq!(one("~0\r\n"), Value::Set(vec![]));
        assert_eq!(
            one(">2\r\n+message\r\n$1\r\nx\r\n"),
            Value::Push(vec![
                Value::Status("message".into()),
                Value::Bulk(b"x".to_vec())
            ])
        );
    }

    #[test]
    fn attributes_are_dropped_wherever_they_are() {
        assert_eq!(one("|1\r\n+ttl\r\n:3600\r\n:7\r\n"), Value::Int(7));
        assert_eq!(
            one("*2\r\n|1\r\n+a\r\n+b\r\n:1\r\n:2\r\n"),
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn replies_split_anywhere_read_the_same() {
        let mut input = Vec::new();
        let values = [
            Value::Array(vec![
                Value::Bulk(vec![b'x'; 3000]),
                Value::Map(vec![(Value::Int(1), Value::Set(vec![Value::Nil]))]),
                Value::Bulk(b"a\r\nb".to_vec()),
            ]),
            Value::Status("PONG".into()),
            Value::Bulk(vec![]),
        ];
        for v in &values {
            encode(v, &mut input);
        }
        for piece in [1, 2, 3, 7, 64, 1000, input.len()] {
            let got: Vec<Value> = read_all(&input, 1 << 20, piece)
                .unwrap()
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(got, values, "in pieces of {piece}");
        }
    }

    #[test]
    fn a_reply_over_the_limit_is_thrown_away_and_the_next_one_read() {
        let mut input = Vec::new();
        encode(&Value::Bulk(vec![b'y'; 10_000]), &mut input);
        encode(
            &Value::Array(vec![Value::Bulk(vec![b'z'; 600]); 20]),
            &mut input,
        );
        encode(&Value::Int(5), &mut input);
        for piece in [1, 5, 100, 4096, input.len()] {
            let got = read_all(&input, 1000, piece).unwrap();
            assert_eq!(
                got,
                [Err(()), Err(()), Ok(Value::Int(5))],
                "in pieces of {piece}"
            );
        }
        // Nothing of a thrown-away reply is kept.
        let mut buf = ReadBuf::new();
        buf.extend(b"$1000000\r\n");
        assert_eq!(buf.next(1000).unwrap(), Next::Pending);
        buf.extend(&vec![b'q'; 500_000]);
        assert_eq!(buf.next(1000).unwrap(), Next::Pending);
        assert!(buf.data.len() - buf.start < 1000);
        buf.extend(&vec![b'q'; 500_000]);
        buf.extend(b"\r\n:1\r\n");
        assert_eq!(buf.next(1000).unwrap(), Next::TooLarge);
        assert_eq!(buf.next(1000).unwrap(), Next::Ready(4));
        assert_eq!(buf.take(4).unwrap(), Value::Int(1));
        assert!(buf.is_empty());
    }

    #[test]
    fn what_is_not_resp_is_an_error() {
        for bad in [
            "?\r\n",
            "$x\r\n",
            "$-2\r\n",
            "*-2\r\n",
            "$3\r\nabcde\r\n",
            ":12a\r\n",
            ":+1\r\n",
            "#x\r\n",
            ",abc\r\n",
            "(12x\r\n",
            "=3\r\ntxt\r\n",
            "_x\r\n",
            "*99999999999\r\n",
        ] {
            let r = read_all(bad.as_bytes(), 1 << 20, bad.len());
            assert!(r.is_err(), "{bad:?} was read as {r:?}");
            assert_eq!(r.unwrap_err().kind(), crate::ErrorKind::Protocol);
        }
        let deep = "*1\r\n".repeat(MAX_DEPTH + 1) + ":1\r\n";
        assert!(read_all(deep.as_bytes(), 1 << 20, 7).is_err());
        let fine = "*1\r\n".repeat(MAX_DEPTH) + ":1\r\n";
        assert_eq!(read_all(fine.as_bytes(), 1 << 20, 7).unwrap().len(), 1);
    }

    #[test]
    fn a_push_is_told_apart() {
        let mut buf = ReadBuf::new();
        buf.extend(b">1\r\n+x\r\n|1\r\n+a\r\n+b\r\n>0\r\n:1\r\n");
        let Next::Ready(n) = buf.next(100).unwrap() else {
            panic!()
        };
        assert!(buf.is_push());
        buf.skip(n);
        let Next::Ready(n) = buf.next(100).unwrap() else {
            panic!()
        };
        assert!(buf.is_push());
        buf.skip(n);
        let Next::Ready(_) = buf.next(100).unwrap() else {
            panic!()
        };
        assert!(!buf.is_push());
    }
}
