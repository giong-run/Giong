//! Errors. Every error giong-quic returns is an `io::Error` carrying a giong
//! error code (`GIONG-E36xx`) with what happened and how to fix it.

use std::io;

use giong::error::{Code, Error};

fn raise(code: Code, message: String, fix: String) -> io::Error {
    Error::new(code, message, fix).into()
}

pub(crate) fn config(what: impl std::fmt::Display, fix: impl std::fmt::Display) -> io::Error {
    raise(Code::QuicConfig, what.to_string(), fix.to_string())
}

pub(crate) fn connect(e: impl std::fmt::Display) -> io::Error {
    raise(
        Code::QuicConnect,
        format!("the QUIC connection could not be set up: {e}"),
        "check that the server listens on that address over UDP, that its certificate is valid \
         for the server name, and that both sides offer a common ALPN protocol."
            .to_string(),
    )
}

pub(crate) fn lost(e: &quinn_proto::ConnectionError) -> io::Error {
    let fix = match e {
        quinn_proto::ConnectionError::TimedOut => {
            "the peer stopped answering: check the network path, or raise the idle timeout with \
             TransportConfig::idle_timeout."
        }
        quinn_proto::ConnectionError::LocallyClosed => {
            "this side closed the connection; open a new one."
        }
        _ => "the connection is gone; open a new one, and see the peer's logs for its reason.",
    };
    raise(
        Code::QuicConnectionLost,
        format!("the QUIC connection was lost: {e}"),
        fix.to_string(),
    )
}

pub(crate) fn reset(code: u64) -> io::Error {
    raise(
        Code::QuicStreamReset,
        format!("the peer reset or stopped the stream with application error code {code}"),
        "the peer gave up on this stream; its error code says why. Retry on a new stream if \
         the request is idempotent."
            .to_string(),
    )
}
