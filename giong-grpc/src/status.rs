//! How a call ended: a [`Code`], a message, and optional details.

use std::fmt;

use giong::error::{Code as ErrorCode, Error};

use crate::metadata::Metadata;

/// The status codes of gRPC.
///
/// A number that is not one of these (a peer may send any) is read as
/// [`Code::Unknown`], as the gRPC specification asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Code {
    /// The call succeeded.
    Ok = 0,
    /// The call was cancelled, usually by the caller.
    Cancelled = 1,
    /// An error that fits no other code, or a status from another system
    /// that could not be mapped.
    Unknown = 2,
    /// The client sent an argument that is wrong whatever the state of the
    /// system.
    InvalidArgument = 3,
    /// The deadline passed before the call ended.
    DeadlineExceeded = 4,
    /// Something the call asked for does not exist.
    NotFound = 5,
    /// Something the call tried to create exists already.
    AlreadyExists = 6,
    /// The caller is known and is not allowed to do this.
    PermissionDenied = 7,
    /// A quota, a rate limit or a size limit was reached.
    ResourceExhausted = 8,
    /// The system is not in the state the call needs.
    FailedPrecondition = 9,
    /// The call was aborted, typically by a concurrency conflict.
    Aborted = 10,
    /// The call went past a valid range (reading past the end of a file).
    OutOfRange = 11,
    /// The method is not implemented or not supported.
    Unimplemented = 12,
    /// An invariant of the system is broken.
    Internal = 13,
    /// The service is not available right now; retrying may work.
    Unavailable = 14,
    /// Data was lost or is corrupted beyond repair.
    DataLoss = 15,
    /// The caller is not known: no valid credentials.
    Unauthenticated = 16,
}

impl Code {
    /// Every code, in numeric order.
    pub const ALL: [Code; 17] = [
        Code::Ok,
        Code::Cancelled,
        Code::Unknown,
        Code::InvalidArgument,
        Code::DeadlineExceeded,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::ResourceExhausted,
        Code::FailedPrecondition,
        Code::Aborted,
        Code::OutOfRange,
        Code::Unimplemented,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
        Code::Unauthenticated,
    ];

    /// The code's number, as sent in `grpc-status`.
    pub const fn number(self) -> u8 {
        self as u8
    }

    /// The code for a number; [`Code::Unknown`] for a number that is not a
    /// gRPC status code.
    pub const fn from_number(number: u32) -> Code {
        if number < 17 {
            Code::ALL[number as usize]
        } else {
            Code::Unknown
        }
    }

    /// The code's name as the gRPC specification writes it: `NOT_FOUND`.
    pub const fn name(self) -> &'static str {
        match self {
            Code::Ok => "OK",
            Code::Cancelled => "CANCELLED",
            Code::Unknown => "UNKNOWN",
            Code::InvalidArgument => "INVALID_ARGUMENT",
            Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
            Code::NotFound => "NOT_FOUND",
            Code::AlreadyExists => "ALREADY_EXISTS",
            Code::PermissionDenied => "PERMISSION_DENIED",
            Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
            Code::FailedPrecondition => "FAILED_PRECONDITION",
            Code::Aborted => "ABORTED",
            Code::OutOfRange => "OUT_OF_RANGE",
            Code::Unimplemented => "UNIMPLEMENTED",
            Code::Internal => "INTERNAL",
            Code::Unavailable => "UNAVAILABLE",
            Code::DataLoss => "DATA_LOSS",
            Code::Unauthenticated => "UNAUTHENTICATED",
        }
    }

    /// The code a gRPC client reports for an HTTP status that came without
    /// a `grpc-status` (a proxy's answer, a server that does not speak
    /// gRPC), as the gRPC specification maps them.
    pub(crate) const fn from_http(status: u16) -> Code {
        match status {
            400 => Code::Internal,
            401 => Code::Unauthenticated,
            403 => Code::PermissionDenied,
            404 => Code::Unimplemented,
            429 | 502 | 503 | 504 => Code::Unavailable,
            _ => Code::Unknown,
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a call ended when it did not succeed: what a handler returns to
/// fail a call, and what a client gets when a call fails.
///
/// ```
/// use giong_grpc::{Code, Status};
///
/// let status = Status::new(Code::NotFound, "no order 17");
/// assert_eq!(status.code(), Code::NotFound);
/// assert_eq!(status.message(), "no order 17");
/// assert_eq!(status.to_string(), "NOT_FOUND: no order 17");
/// ```
///
/// A status that giong-grpc itself raised (a deadline that passed, a
/// connection that broke, a message past the size limit) also carries a
/// `GIONG-E41xx` code, [`Status::error_code`], and its message says what
/// went wrong and how to fix it.
#[derive(Clone, PartialEq, Eq)]
pub struct Status {
    code: Code,
    message: String,
    details: Vec<u8>,
    metadata: Metadata,
    error: Option<ErrorCode>,
}

impl Status {
    /// A status with a code and a message for the developer on the other
    /// side. The message is sent in `grpc-message`, percent-encoded, so it
    /// can hold any text.
    ///
    /// [`Code::Ok`] is not an error: a handler returns `Ok(..)` for it. A
    /// status built with it is sent as it is.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Vec::new(),
            metadata: Metadata::new(),
            error: None,
        }
    }

    /// A status giong-grpc raises itself: the message is the coded error's,
    /// with the cause and the fix.
    pub(crate) fn raised(
        code: Code,
        error: ErrorCode,
        what: impl Into<String>,
        fix: &'static str,
    ) -> Self {
        let mut status = Self::new(code, Error::new(error, what.into(), fix).to_string());
        status.error = Some(error);
        status
    }

    /// Adds the details: an encoded `google.rpc.Status` message, sent in
    /// `grpc-status-details-bin`. Its code and message should repeat this
    /// status's; giong-grpc sends the bytes as they are.
    pub fn with_details(mut self, details: impl Into<Vec<u8>>) -> Self {
        self.details = details.into();
        self
    }

    /// Adds metadata sent with the status, in the trailers of the response.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// The code.
    pub fn code(&self) -> Code {
        self.code
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The details: an encoded `google.rpc.Status`, or empty.
    pub fn details(&self) -> &[u8] {
        &self.details
    }

    /// The metadata that came (or goes) with the status.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The `GIONG-E41xx` code, for a status giong-grpc raised on this side
    /// of the call; `None` for one a handler built or the other side sent.
    pub fn error_code(&self) -> Option<ErrorCode> {
        self.error
    }

    pub(crate) fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Status");
        d.field("code", &self.code).field("message", &self.message);
        if !self.details.is_empty() {
            d.field("details", &self.details.len());
        }
        if !self.metadata.is_empty() {
            d.field("metadata", &self.metadata);
        }
        d.finish()
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            f.write_str(self.code.name())
        } else {
            write!(f, "{}: {}", self.code.name(), self.message)
        }
    }
}

impl std::error::Error for Status {}

impl From<Status> for std::io::Error {
    fn from(status: Status) -> Self {
        std::io::Error::other(status)
    }
}

impl From<giong_proto::Error> for Status {
    /// A message that is not valid protobuf for its type: `INTERNAL`, as
    /// other gRPC implementations report it, with `GIONG-E4105`.
    fn from(error: giong_proto::Error) -> Self {
        Status::raised(
            Code::Internal,
            ErrorCode::GrpcDecode,
            format!("a message is not valid protobuf for its type: {error}"),
            "check that both sides were generated from the same .proto.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_unknown_numbers_are_unknown() {
        for (i, code) in Code::ALL.into_iter().enumerate() {
            assert_eq!(code.number() as usize, i);
            assert_eq!(Code::from_number(i as u32), code);
            assert!(!code.name().is_empty());
        }
        assert_eq!(Code::from_number(17), Code::Unknown);
        assert_eq!(Code::from_number(u32::MAX), Code::Unknown);
        assert_eq!(Code::from_http(503), Code::Unavailable);
        assert_eq!(Code::from_http(418), Code::Unknown);
    }

    #[test]
    fn a_raised_status_names_its_code_and_fix() {
        let status = Status::raised(
            Code::DeadlineExceeded,
            ErrorCode::GrpcDeadline,
            "the call took longer than 1s",
            "raise the deadline.",
        );
        assert_eq!(status.error_code(), Some(ErrorCode::GrpcDeadline));
        assert!(status.message().starts_with("GIONG-E4108: "), "{status}");
        assert!(status.message().contains("Fix: raise the deadline."));
        assert_eq!(Status::new(Code::Ok, "").to_string(), "OK");
    }
}
