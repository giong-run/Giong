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

    // The same bytes passed on as they arrive, as a streamed request body
    // is: the input dropped and the body taken out after every piece.
    let mut streamed = Decoder::new(max);
    let mut body = Vec::new();
    let (mut start, mut end) = (0, 0);
    let got = loop {
        end = (end + 1 + splits() % 6).min(input.len());
        let progress = streamed.feed(&input[start..end]);
        body.append(&mut streamed.body);
        match progress {
            Ok(Progress::NeedMore) if end < input.len() => start += streamed.consume(),
            Ok(Progress::Done(used)) => break Ok(Progress::Done(start + used)),
            other => break other,
        }
    };
    assert_eq!(
        got,
        expected,
        "streamed {:?}",
        String::from_utf8_lossy(input)
    );

    if let Ok(Progress::Done(used)) = expected {
        assert!(used <= input.len());
        assert!(whole.body.len() <= max);
        assert_eq!(whole.body, pieces.body);
        assert_eq!(whole.body, body);
        true
    } else {
        false
    }
}

// ------------------------------------------------------------------ HTTP/2

use crate::h2::frame::{self, Head};
use crate::h2::hpack::{Decoder as HeaderDecoder, Encoder, Index, TABLE_SIZE};

/// Checks HPACK decoding and encoding on `input` (see `h2::hpack::check`).
pub fn check_hpack(input: &[u8]) -> bool {
    crate::h2::hpack::check(input)
}

/// Checks Huffman coding on `input` (see `h2::hpack::check_huffman`).
pub fn check_huffman(input: &[u8]) -> bool {
    crate::h2::hpack::check_huffman(input)
}

/// Reads bytes off the front of fuzz data; zeros once it runs out.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn byte(&mut self) -> u8 {
        let Some((&b, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        b
    }

    fn take(&mut self, n: usize) -> &[u8] {
        let (taken, rest) = self.0.split_at(n.min(self.0.len()));
        self.0 = rest;
        taken
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.byte() as usize % from.len()]
    }
}

/// What a client sends, built from fuzz data: the preface, then frames
/// that are well delimited but otherwise anything, with header blocks a
/// real encoder made (so that requests get through to the handlers) and
/// now and then raw bytes instead.
pub fn h2_input(data: &[u8]) -> Vec<u8> {
    let mut d = Bytes(data);
    let mut out = frame::PREFACE.to_vec();
    let mut encoder = Encoder::new();
    let mut block = Vec::new();
    if d.byte() % 16 != 0 {
        let mut settings = Vec::new();
        for _ in 0..d.byte() % 4 {
            settings.push(d.pick(&[
                (frame::INITIAL_WINDOW_SIZE, 0),
                (frame::INITIAL_WINDOW_SIZE, 10),
                (frame::INITIAL_WINDOW_SIZE, 70_000),
                (frame::MAX_FRAME_SIZE, 20_000),
                (frame::HEADER_TABLE_SIZE, 0),
                (frame::HEADER_TABLE_SIZE, 100),
                (frame::ENABLE_PUSH, 0),
                (frame::MAX_CONCURRENT_STREAMS, 1),
            ]));
        }
        frame::write_settings(&mut out, &settings);
    }
    // The stream most recently opened.
    let mut last = 0u32;
    while !d.0.is_empty() {
        // Mostly requests, and frames for the stream opened last.
        let mut op = d.byte() % 12;
        if last == 0 {
            op = 0;
        }
        let stream = match d.byte() {
            0 => 0,
            1..=223 if op < 4 => (last + 1) | 1,
            1..=223 => last,
            b @ 224..=247 => u32::from(b % 16) * 2 + 1,
            b => u32::from(b),
        };
        if op < 4 {
            last = last.max(stream);
        }
        let flags = d.byte();
        match op {
            0..=3 => {
                // A request head.
                let method = d.pick(&["GET", "POST", "HEAD", "PUT", "CONNECT", "OPTIONS", "get"]);
                let path = d.pick(&["/", "/s", "/r", "/n", "/sr", "/big?q", "/rx", "*", "", "x"]);
                let mut fields = vec![(":method", method)];
                if d.byte() % 8 != 0 {
                    fields.push((":scheme", "https"));
                }
                if d.byte() % 8 != 0 {
                    fields.push((":path", path));
                }
                fields.push((":authority", "example.com"));
                for _ in 0..d.byte() % 4 {
                    fields.push(d.pick(&[
                        ("content-length", "3"),
                        ("content-length", "0"),
                        ("expect", "100-continue"),
                        ("cookie", "a=1"),
                        ("te", "trailers"),
                        ("te", "gzip"),
                        ("connection", "close"),
                        ("Upper", "x"),
                        (":path", "/again"),
                        ("host", "other"),
                        ("x-long", "0123456789012345678901234567890123456789"),
                        ("x-ctl", "a\x01b"),
                    ]));
                }
                block.clear();
                for (name, value) in fields {
                    let index = d.pick(&[Index::Yes, Index::No, Index::Never]);
                    encoder.field(name.as_bytes(), value.as_bytes(), index, &mut block);
                }
                let end_stream = flags & frame::END_STREAM;
                if flags & 0xc0 != 0xc0 {
                    frame::write_head(
                        &mut out,
                        block.len(),
                        frame::HEADERS,
                        end_stream | frame::END_HEADERS,
                        stream,
                    );
                    out.extend_from_slice(&block);
                } else {
                    // Split over CONTINUATION.
                    let half = block.len() / 2;
                    frame::write_head(&mut out, half, frame::HEADERS, end_stream, stream);
                    out.extend_from_slice(&block[..half]);
                    let cont = if d.byte() != 0 { stream } else { stream + 2 };
                    frame::write_head(
                        &mut out,
                        block.len() - half,
                        frame::CONTINUATION,
                        frame::END_HEADERS,
                        cont,
                    );
                    out.extend_from_slice(&block[half..]);
                }
            }
            4 | 5 => {
                let n = d.byte() as usize % 70;
                let mut payload = Vec::new();
                let mut f = flags & (frame::END_STREAM | frame::PADDED);
                if f & frame::PADDED != 0 {
                    payload.push(d.byte() % 8);
                }
                payload.extend_from_slice(d.take(n));
                if f & frame::PADDED != 0 {
                    let pad = payload[0] as usize;
                    payload.resize(payload.len() + pad, 0);
                }
                if payload.is_empty() && f & frame::PADDED != 0 {
                    f &= !frame::PADDED;
                }
                frame::write_head(&mut out, payload.len(), frame::DATA, f, stream);
                out.extend_from_slice(&payload);
            }
            6 => {
                let code = d.pick(&[0u32, 1, 8, 0xb, 0xffff]);
                frame::write_head(&mut out, 4, frame::RST_STREAM, 0, stream);
                out.extend_from_slice(&code.to_be_bytes());
            }
            7 => {
                let increment = d.pick(&[0u32, 1, 100, 65_535, 1 << 20, 0x7fff_ffff]);
                frame::write_head(&mut out, 4, frame::WINDOW_UPDATE, 0, stream % 64);
                out.extend_from_slice(&increment.to_be_bytes());
            }
            8 => {
                if flags & 1 != 0 {
                    frame::write_settings_ack(&mut out);
                } else {
                    let setting = d.pick(&[
                        (frame::INITIAL_WINDOW_SIZE, 0),
                        (frame::INITIAL_WINDOW_SIZE, 1 << 20),
                        (frame::INITIAL_WINDOW_SIZE, 0x8000_0000),
                        (frame::MAX_FRAME_SIZE, 100),
                        (frame::HEADER_TABLE_SIZE, 50),
                        (0x99, 1),
                    ]);
                    frame::write_settings(&mut out, &[setting]);
                }
            }
            9 => match flags % 3 {
                0 => frame::write_ping(&mut out, flags & 0x10 != 0, &[d.byte(); 8]),
                1 => {
                    frame::write_head(&mut out, 5, frame::PRIORITY, 0, stream);
                    out.extend_from_slice(&[0, 0, 0, d.byte() % 8, 16]);
                }
                _ => {
                    frame::write_head(&mut out, 8, frame::GOAWAY, 0, 0);
                    out.extend_from_slice(&[0; 8]);
                }
            },
            10 => {
                // Trailers.
                block.clear();
                encoder.field(b"x-trailer", b"1", Index::No, &mut block);
                frame::write_head(
                    &mut out,
                    block.len(),
                    frame::HEADERS,
                    flags & frame::END_STREAM | frame::END_HEADERS,
                    stream,
                );
                out.extend_from_slice(&block);
            }
            _ => {
                // Anything at all.
                let kind = d.byte() % 12;
                let n = d.byte() as usize % 40;
                let payload = d.take(n).to_vec();
                frame::write_head(&mut out, payload.len(), kind, flags, stream);
                out.extend_from_slice(&payload);
            }
        }
    }
    out
}

/// The client frames in `input` (after the preface), for what they allow
/// the server to send: the largest initial window and frame size, and the
/// sum of window increments per stream (0 for the connection).
fn client_allowance(input: &[u8]) -> (u64, usize, std::collections::HashMap<u32, u64>) {
    let mut initial = u64::from(frame::DEFAULT_WINDOW);
    let mut max_frame = frame::DEFAULT_FRAME_SIZE;
    let mut increments = std::collections::HashMap::new();
    let mut at = frame::PREFACE.len().min(input.len());
    while let Some(head) = input.get(at..at + frame::HEAD_LEN).map(Head::parse) {
        let Some(payload) = input.get(at + frame::HEAD_LEN..at + frame::HEAD_LEN + head.len) else {
            break;
        };
        at += frame::HEAD_LEN + head.len;
        match head.kind {
            frame::SETTINGS if !head.has(frame::ACK) => {
                for s in payload.chunks_exact(6) {
                    let value = frame::read_u32(&s[2..]);
                    match u16::from_be_bytes([s[0], s[1]]) {
                        frame::INITIAL_WINDOW_SIZE => initial = initial.max(u64::from(value)),
                        frame::MAX_FRAME_SIZE => max_frame = max_frame.max(value as usize),
                        _ => {}
                    }
                }
            }
            frame::WINDOW_UPDATE if payload.len() == 4 => {
                *increments.entry(head.stream).or_insert(0) +=
                    u64::from(frame::read_u32(payload) & 0x7fff_ffff);
            }
            _ => {}
        }
    }
    (initial, max_frame, increments)
}

/// What the server wrote back to a client that sent `input` must be a
/// well-formed sequence of frames: SETTINGS first; header blocks that
/// decode, to responses with a `:status` and nothing an HTTP/2 response
/// may not carry; DATA only inside a response and within the windows the
/// client gave; nothing after an error GOAWAY. Returns the number of
/// final responses.
pub fn check_h2_output(input: &[u8], output: &[u8]) -> usize {
    use std::collections::HashMap;
    #[derive(Default)]
    struct Seen {
        head: bool,
        ended: bool,
        data: u64,
    }
    let (initial, max_frame, increments) = client_allowance(input);
    let mut decoder = HeaderDecoder::new(TABLE_SIZE);
    let mut streams: HashMap<u32, Seen> = HashMap::new();
    let mut conn_data = 0u64;
    let mut block: Option<(u32, bool, Vec<u8>)> = None;
    let mut responses = 0;
    let mut first = true;
    let mut error_goaway = false;
    let mut at = 0;
    while at < output.len() {
        let head = Head::parse(
            output
                .get(at..at + frame::HEAD_LEN)
                .expect("a whole frame header"),
        );
        let payload = output
            .get(at + frame::HEAD_LEN..at + frame::HEAD_LEN + head.len)
            .expect("a whole frame");
        at += frame::HEAD_LEN + head.len;
        assert!(!error_goaway, "a frame after an error GOAWAY: {head:?}");
        assert!(head.len <= max_frame, "{head:?}");
        if first {
            assert!(
                head.kind == frame::SETTINGS && !head.has(frame::ACK),
                "{head:?}"
            );
            first = false;
        }
        if let Some((stream, _, _)) = &block {
            assert!(
                head.kind == frame::CONTINUATION && head.stream == *stream,
                "{head:?}"
            );
        }
        match head.kind {
            frame::SETTINGS => {
                assert_eq!(head.stream, 0);
                assert_eq!(payload.len() % 6, 0);
                assert!(!head.has(frame::ACK) || payload.is_empty());
            }
            frame::HEADERS | frame::CONTINUATION => {
                assert!(head.stream % 2 == 1, "{head:?}");
                if head.kind == frame::HEADERS {
                    assert!(head.flags & (frame::PADDED | frame::PRIORITY_FLAG) == 0);
                    block = Some((head.stream, head.has(frame::END_STREAM), Vec::new()));
                }
                let (stream, end, bytes) = block.as_mut().expect("CONTINUATION after HEADERS");
                bytes.extend_from_slice(payload);
                if head.has(frame::END_HEADERS) {
                    let (stream, end, bytes) = (*stream, *end, std::mem::take(bytes));
                    block = None;
                    let mut fields = Vec::new();
                    decoder
                        .decode(&bytes, |n, v| fields.push((n.to_vec(), v.to_vec())))
                        .expect("a header block of ours decodes");
                    let (name, status) = fields.first().expect("a :status");
                    assert_eq!(name, b":status");
                    assert!(status.len() == 3 && status.iter().all(u8::is_ascii_digit));
                    let interim = status[0] == b'1';
                    for (name, value) in &fields[1..] {
                        assert!(!name.starts_with(b":"), "{name:?}");
                        assert!(!name.iter().any(u8::is_ascii_uppercase), "{name:?}");
                        assert!(!matches!(
                            &name[..],
                            b"connection"
                                | b"keep-alive"
                                | b"transfer-encoding"
                                | b"upgrade"
                                | b"te"
                        ));
                        assert!(!value.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0));
                    }
                    let seen = streams.entry(stream).or_default();
                    assert!(
                        !seen.ended && !seen.head,
                        "a second response head on {stream}"
                    );
                    if interim {
                        assert!(!end, "END_STREAM on an interim response");
                    } else {
                        seen.head = true;
                        responses += 1;
                    }
                    seen.ended |= end;
                }
            }
            frame::DATA => {
                let seen = streams.entry(head.stream).or_default();
                assert!(
                    seen.head && !seen.ended,
                    "DATA outside a response on {}",
                    head.stream
                );
                assert!(head.flags & frame::PADDED == 0);
                seen.data += payload.len() as u64;
                seen.ended |= head.has(frame::END_STREAM);
                conn_data += payload.len() as u64;
                let stream_allowed = initial + increments.get(&head.stream).copied().unwrap_or(0);
                assert!(
                    seen.data <= stream_allowed,
                    "stream {} past its window",
                    head.stream
                );
                let conn_allowed =
                    u64::from(frame::DEFAULT_WINDOW) + increments.get(&0).copied().unwrap_or(0);
                assert!(conn_data <= conn_allowed, "past the connection window");
            }
            frame::RST_STREAM => {
                assert!(head.stream != 0 && payload.len() == 4);
                streams.entry(head.stream).or_default().ended = true;
            }
            frame::WINDOW_UPDATE => {
                assert_eq!(payload.len(), 4);
                let increment = frame::read_u32(payload);
                assert!(increment > 0 && increment <= 0x7fff_ffff);
            }
            frame::PING => {
                assert!(head.stream == 0 && head.has(frame::ACK) && payload.len() == 8);
            }
            frame::GOAWAY => {
                assert!(head.stream == 0 && payload.len() >= 8);
                let code = frame::read_u32(&payload[4..8]);
                if code != 0 {
                    error_goaway = true;
                    let debug = std::str::from_utf8(&payload[8..]).expect("UTF-8 debug data");
                    assert!(debug.starts_with("GIONG-E"), "{debug}");
                }
            }
            kind => panic!("the server sent a frame of type {kind}"),
        }
    }
    assert!(block.is_none(), "a header block cut short");
    responses
}

/// The pieces of an HTTP/2 connection driven by fuzz data: shared by the
/// randomised test and the `h2` fuzz target.
pub mod h2_connection {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;
    use std::rc::Rc;
    use std::time::Duration;

    use crate::{Body, Handler, Request, Response, Server, Transport};

    /// A transport in memory: hands out `input` piece by piece, then the
    /// end of the stream; collects what is written in `output`.
    pub struct Memory {
        /// What the client sends, in the pieces `recv` returns.
        pub input: RefCell<VecDeque<Vec<u8>>>,
        /// What the server wrote.
        pub output: Rc<RefCell<Vec<u8>>>,
    }

    impl Transport for Memory {
        type Chunk = Vec<u8>;

        async fn recv(&self) -> io::Result<Vec<u8>> {
            Ok(self.input.borrow_mut().pop_front().unwrap_or_default())
        }

        async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
            self.output.borrow_mut().extend_from_slice(&buf);
            (Ok(()), buf)
        }

        async fn close_write(&self) {}
    }

    /// The path picks the behaviour: `s` reads the body as a stream, `r`
    /// answers with a streamed body (`x`: aborted), `n` answers 204,
    /// `big` answers 3000 bytes.
    pub struct Fuzzed;

    impl Handler for Fuzzed {
        fn stream_body(&self, req: &Request) -> Option<u64> {
            req.path().contains('s').then_some(64)
        }

        async fn call(&self, req: Request) -> Response {
            assert!(req.version() == crate::Version::Http2);
            assert!(req.body().len() <= 64);
            if let Some(body) = req.body_stream() {
                while let Ok(Some(piece)) = body.chunk().await {
                    assert!(!piece.is_empty());
                }
            }
            let path = req.path().to_string();
            if path.contains('n') {
                return Response::new(204);
            }
            if path.contains("big") {
                return Response::new(200).body(vec![b'b'; 3000]);
            }
            if path.contains('r') {
                let (tx, body) = Body::channel();
                let abort = path.contains('x');
                drop(giong::spawn(async move {
                    if tx.send(&b"ab"[..]).await.is_err() {
                        return;
                    }
                    if abort {
                        return tx.abort();
                    }
                    let _ = tx.send(&b"cde"[..]).await;
                }));
                return Response::new(200).body(body);
            }
            Response::text("ok")
        }
    }

    /// The server for fuzzing: short timeouts, so that a stalled stream
    /// costs milliseconds.
    pub fn server() -> Server {
        Server::new()
            .max_head_size(1024)
            .max_body_size(64)
            .request_timeout(Duration::from_millis(20))
            .write_timeout(Duration::from_millis(20))
            .idle_timeout(Duration::from_millis(20))
            .max_concurrent_streams(8)
    }

    /// Splits `bytes` into the pieces the transport delivers.
    pub fn pieces(bytes: &[u8], seed: u8) -> VecDeque<Vec<u8>> {
        let mut input = VecDeque::new();
        if seed == 0 {
            input.push_back(bytes.to_vec());
            return input;
        }
        let mut state = u32::from(seed);
        let mut rest = bytes;
        while !rest.is_empty() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let n = (1 + state as usize % 64).min(rest.len());
            input.push_back(rest[..n].to_vec());
            rest = &rest[n..];
        }
        input
    }
}
