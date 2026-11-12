//! Incremental decoder for `Transfer-Encoding: chunked` request bodies.

/// Longest accepted chunk-size line or trailer line.
const MAX_LINE: usize = 4096;

#[derive(Debug)]
enum State {
    /// Reading a `size[;ext]\r\n` line.
    Size,
    /// Reading chunk data; this many bytes are left.
    Data(usize),
    /// Expecting the `\r\n` after chunk data.
    DataEnd,
    /// Reading trailer lines until the empty one.
    Trailers,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// The body is complete; the encoded form took this many bytes in total.
    Done(usize),
    NeedMore,
}

/// Decodes a chunked body as its bytes arrive. Each byte is looked at once, no
/// matter how the input is split up.
#[derive(Debug)]
pub(crate) struct Decoder {
    state: State,
    /// Encoded bytes consumed so far.
    pos: usize,
    trailer_bytes: usize,
    pub(crate) body: Vec<u8>,
    max: usize,
}

impl Decoder {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            state: State::Size,
            pos: 0,
            trailer_bytes: 0,
            body: Vec::new(),
            max,
        }
    }

    /// `data` is everything received after the head so far. Returns the status
    /// code to answer with on failure.
    /// How many bytes of the input given to `feed` have been read, which
    /// the caller may now drop: the next `feed` starts at the input's start.
    /// For a body passed on as it arrives, with `body` taken out each time.
    #[cfg(feature = "client")]
    pub(crate) fn consume(&mut self) -> usize {
        std::mem::take(&mut self.pos)
    }

    pub(crate) fn feed(&mut self, data: &[u8]) -> Result<Progress, u16> {
        loop {
            let rest = &data[self.pos..];
            match self.state {
                State::Size => {
                    let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                        return if rest.len() > MAX_LINE {
                            Err(400)
                        } else {
                            Ok(Progress::NeedMore)
                        };
                    };
                    if nl == 0 || rest[nl - 1] != b'\r' || nl > MAX_LINE {
                        return Err(400);
                    }
                    let line = &rest[..nl - 1];
                    let digits = line.iter().position(|&b| b == b';').unwrap_or(line.len());
                    let hex = &line[..digits];
                    if hex.is_empty() || hex.len() > 16 || !hex.iter().all(u8::is_ascii_hexdigit) {
                        return Err(400);
                    }
                    // Chunk extensions are ignored, but must not hide controls.
                    if !crate::parse::is_field_value(&line[digits..]) {
                        return Err(400);
                    }
                    let size = hex.iter().fold(0u64, |acc, &b| {
                        acc * 16 + (b as char).to_digit(16).unwrap_or(0) as u64
                    });
                    if size > (self.max - self.body.len()) as u64 {
                        return Err(413);
                    }
                    self.pos += nl + 1;
                    self.state = if size == 0 {
                        State::Trailers
                    } else {
                        State::Data(size as usize)
                    };
                }
                State::Data(left) => {
                    if rest.is_empty() {
                        return Ok(Progress::NeedMore);
                    }
                    let n = left.min(rest.len());
                    self.body.extend_from_slice(&rest[..n]);
                    self.pos += n;
                    self.state = if n == left {
                        State::DataEnd
                    } else {
                        State::Data(left - n)
                    };
                }
                State::DataEnd => {
                    if rest.len() < 2 {
                        return Ok(Progress::NeedMore);
                    }
                    if &rest[..2] != b"\r\n" {
                        return Err(400);
                    }
                    self.pos += 2;
                    self.state = State::Size;
                }
                State::Trailers => {
                    let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                        return if rest.len() > MAX_LINE {
                            Err(400)
                        } else {
                            Ok(Progress::NeedMore)
                        };
                    };
                    if nl == 0 || rest[nl - 1] != b'\r' {
                        return Err(400);
                    }
                    self.pos += nl + 1;
                    if nl == 1 {
                        return Ok(Progress::Done(self.pos));
                    }
                    // Trailer fields are read and dropped, within a budget.
                    self.trailer_bytes += nl + 1;
                    if nl > MAX_LINE || self.trailer_bytes > 4 * MAX_LINE {
                        return Err(400);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(input: &[u8], max: usize) -> Result<(Vec<u8>, usize), u16> {
        let mut d = Decoder::new(max);
        match d.feed(input)? {
            Progress::Done(n) => Ok((d.body, n)),
            Progress::NeedMore => Err(0),
        }
    }

    #[test]
    fn decodes_chunks_extensions_and_trailers() {
        let input = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\nNEXT";
        let (body, used) = decode(input, 1024).unwrap();
        assert_eq!(body, b"hello world");
        assert_eq!(&input[used..], b"NEXT");
        assert_eq!(decode(b"0\r\n\r\n", 0).unwrap(), (vec![], 5));
        assert_eq!(
            decode(b"A\r\n0123456789\r\n0\r\n\r\n", 10).unwrap().0,
            b"0123456789"
        );
    }

    #[test]
    fn gives_the_same_answer_however_the_input_is_split() {
        let input = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n";
        for step in 1..input.len() {
            let mut d = Decoder::new(1024);
            let mut end = 0;
            let done = loop {
                end = (end + step).min(input.len());
                match d.feed(&input[..end]).unwrap() {
                    Progress::Done(n) => break n,
                    Progress::NeedMore => assert!(end < input.len(), "step {step}"),
                }
            };
            assert_eq!(done, input.len());
            assert_eq!(d.body, b"hello world");
        }
    }

    #[test]
    fn rejects_bad_framing_and_oversized_bodies() {
        for bad in [
            &b"5\nhello\r\n0\r\n\r\n"[..],
            b"\r\n",
            b"g\r\n",
            b"-1\r\n",
            b"0x5\r\n",
            b"5 \r\nhello\r\n0\r\n\r\n",
            b"5\r\nhelloXX0\r\n\r\n",
            b"11111111111111111\r\n",
            b"1;a\0b\r\nx\r\n0\r\n\r\n",
            b"0\r\nbad\n\r\n",
        ] {
            assert_eq!(
                decode(bad, 1 << 20),
                Err(400),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(decode(b"5\r\nhello\r\n1\r\n!\r\n0\r\n\r\n", 5), Err(413));
        assert_eq!(decode(b"ffffffffffffffff\r\n", 1 << 20), Err(413));
        // Incomplete input is not an error.
        assert_eq!(decode(b"5\r\nhel", 1024), Err(0));
        // An endless size line or trailer section is.
        assert_eq!(decode(&vec![b'1'; MAX_LINE + 2], 1 << 20), Err(400));
        let mut trailers = b"0\r\n".to_vec();
        for _ in 0..2000 {
            trailers.extend_from_slice(b"X-Trailer: aaaaaaaaaaaaaaaa\r\n");
        }
        assert_eq!(decode(&trailers, 1 << 20), Err(400));
    }
}
