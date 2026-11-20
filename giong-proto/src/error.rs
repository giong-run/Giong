//! The errors this crate raises, each with a `GIONG-E40xx` code.

use std::fmt;

/// What went wrong while decoding or encoding a message.
///
/// Every kind has a code (`GIONG-E4001` and up) and a page under
/// `docs/errors/` that says what causes it and how to fix it. Match on the
/// kind, never on the message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// `GIONG-E4001`: the input ends in the middle of a field.
    Truncated,
    /// `GIONG-E4002`: a varint is longer than 10 bytes or does not fit in
    /// 64 bits.
    BadVarint,
    /// `GIONG-E4003`: a field tag has field number 0, a number above
    /// 536 870 911, wire type 6 or 7, or more than 5 bytes.
    BadTag,
    /// `GIONG-E4004`: an end-group tag without its start, with another
    /// field number, or a group the input ends inside.
    BadGroup,
    /// `GIONG-E4005`: a `string` field holds bytes that are not UTF-8.
    BadUtf8,
    /// `GIONG-E4006`: messages are nested deeper than the limit (100).
    TooDeep,
    /// `GIONG-E4007`: a packed field of fixed-width values whose length is
    /// not a multiple of the value size, or whose varints run past its end.
    BadPacked,
    /// `GIONG-E4008`: a message or field is larger than 2 GiB, the most
    /// protobuf can frame.
    TooLarge,
}

impl ErrorKind {
    /// Every kind, in numeric order.
    pub const ALL: &'static [ErrorKind] = &[
        ErrorKind::Truncated,
        ErrorKind::BadVarint,
        ErrorKind::BadTag,
        ErrorKind::BadGroup,
        ErrorKind::BadUtf8,
        ErrorKind::TooDeep,
        ErrorKind::BadPacked,
        ErrorKind::TooLarge,
    ];

    /// The number in the code: 4001 for `GIONG-E4001`.
    pub const fn number(self) -> u16 {
        match self {
            ErrorKind::Truncated => 4001,
            ErrorKind::BadVarint => 4002,
            ErrorKind::BadTag => 4003,
            ErrorKind::BadGroup => 4004,
            ErrorKind::BadUtf8 => 4005,
            ErrorKind::TooDeep => 4006,
            ErrorKind::BadPacked => 4007,
            ErrorKind::TooLarge => 4008,
        }
    }

    /// What the code means, in a few words: the title of its page.
    pub const fn summary(self) -> &'static str {
        match self {
            ErrorKind::Truncated => "the message ends in the middle of a field",
            ErrorKind::BadVarint => "malformed varint",
            ErrorKind::BadTag => "invalid field tag",
            ErrorKind::BadGroup => "unbalanced group",
            ErrorKind::BadUtf8 => "string field is not UTF-8",
            ErrorKind::TooDeep => "messages nested too deeply",
            ErrorKind::BadPacked => "malformed packed field",
            ErrorKind::TooLarge => "message larger than 2 GiB",
        }
    }

    const fn fix(self) -> &'static str {
        match self {
            ErrorKind::Truncated => {
                "pass the whole message: check that the length prefix in front of it \
                 (gRPC framing, a file record) was read in full and that the sender \
                 did not cut it off"
            }
            ErrorKind::BadVarint | ErrorKind::BadTag | ErrorKind::BadGroup => {
                "the bytes are not a protobuf message of this type, or they are \
                 corrupted: check that both sides use the same .proto and that no \
                 framing bytes were left in front of the message"
            }
            ErrorKind::BadUtf8 => {
                "the sender put non-UTF-8 bytes in a `string` field: declare the \
                 field `bytes` in the .proto if it carries binary data"
            }
            ErrorKind::TooDeep => {
                "flatten the data or send it in several messages; the limit stops \
                 hostile input from exhausting the stack"
            }
            ErrorKind::BadPacked => {
                "the sender's encoder is broken or the bytes are corrupted: compare \
                 the .proto on both sides"
            }
            ErrorKind::TooLarge => {
                "split the data over several messages, or stream it (a gRPC \
                 streaming call) instead of one message"
            }
        }
    }
}

/// An error from decoding or encoding a message.
///
/// The message names the code, the byte offset where the input went wrong,
/// the field being read when there was one, and a fix:
///
/// ```text
/// GIONG-E4001: the message ends in the middle of a field (field 3, at byte 17).
/// Fix: pass the whole message: ... See https://github.com/giong-run/Giong/blob/main/docs/errors/E4001.md
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    offset: u32,
    field: u32,
}

impl Error {
    #[cold]
    #[inline(never)]
    pub(crate) fn new(kind: ErrorKind, offset: usize, field: u32) -> Error {
        Error {
            kind,
            offset: offset.min(u32::MAX as usize) as u32,
            field,
        }
    }

    /// What went wrong.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The code, such as `"GIONG-E4001"`.
    pub fn code(&self) -> String {
        format!("GIONG-E{:04}", self.kind.number())
    }

    /// The offset in the input (from the start of the innermost message
    /// being read) where decoding stopped.
    pub fn offset(&self) -> usize {
        self.offset as usize
    }

    /// The field number being read, or 0 when the error is not inside a
    /// field.
    pub fn field(&self) -> u32 {
        self.field
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.kind.number();
        write!(f, "GIONG-E{n:04}: {}", self.kind.summary())?;
        if self.field != 0 {
            write!(f, " (field {}, at byte {})", self.field, self.offset)?;
        } else {
            write!(f, " (at byte {})", self.offset)?;
        }
        write!(
            f,
            ". Fix: {}. See https://github.com/giong-run/Giong/blob/main/docs/errors/E{n:04}.md",
            self.kind.fix()
        )
    }
}

impl std::error::Error for Error {}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    }
}
