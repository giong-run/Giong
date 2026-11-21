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

pub(crate) fn early_rejected() -> io::Error {
    raise(
        Code::QuicEarlyDataRejected,
        "the server refused the 0-RTT data this stream was opened in; nothing of it was \
         processed"
            .to_string(),
        "send the request again on a new stream: the handshake has ended, so a stream opened \
         now is not early data. Connection::handshake tells when that is."
            .to_string(),
    )
}

/// The application error code of a stream the peer reset or stopped
/// (`GIONG-E3604`); `None` for any other error.
pub(crate) fn reset_code(error: &io::Error) -> Option<u64> {
    let ours = error.get_ref()?.downcast_ref::<Error>()?;
    if ours.code() != Code::QuicStreamReset {
        return None;
    }
    // The message `reset` wrote ends with the code.
    ours.message().rsplit(' ').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_of_a_reset_reads_back() {
        for code in [0, 0x10b, 0x10c, (1 << 62) - 1] {
            assert_eq!(reset_code(&reset(code)), Some(code));
        }
        assert_eq!(reset_code(&connect("refused")), None);
        assert_eq!(reset_code(&io::Error::other("code 7")), None);
    }
}
