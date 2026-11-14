//! [`Timeout`]: a limit on how long a handler may take.

use std::time::Duration;

use giong::error::Code;

use crate::{Error, Middleware, Next, Req, Response};

/// Answers `503 Service Unavailable` (`GIONG-E1209`) when the rest of the
/// chain takes longer than the limit to produce a response, and drops the
/// handler's future.
///
/// Dropping the future cancels what the handler was waiting for. Dropping is
/// always memory-safe, but not every operation can be stopped halfway
/// without a trace: giong's database clients and giong-http's client close a
/// connection whose query or request was abandoned (an HTTP/2 request
/// resets its stream) instead of reusing it, while some of giong's own I/O,
/// such as a vectored write or `send_file` on a `TcpStream`, may have done
/// part of its work when it is dropped. For your own I/O in a handler, see
/// each operation's note on cancel safety.
///
/// The limit covers producing the response, not receiving the request
/// (giong-http's `request_timeout`) or sending it (`write_timeout`). So a
/// request whose body is read as a stream
/// ([`Router::stream_bodies`](crate::Router::stream_bodies)) has no limit
/// here, since the handler runs while the body arrives; and for a response
/// body written as it is produced, the limit ends when the handler returns
/// the response.
///
/// A streaming route that does more work once the body is in (a call to a
/// database or another service) should put its own limit on that work, or
/// a dependency that hangs holds the request until the client gives up:
///
/// ```no_run
/// use std::time::Duration;
///
/// use giong_web::{Error, Req, Response};
///
/// # async fn store(_body: Vec<u8>) -> Result<u64, Error> { Ok(1) }
/// async fn upload(req: Req) -> Result<Response, Error> {
///     let stream = req.body_stream().expect("a streaming route");
///     let mut body = Vec::new();
///     while let Some(piece) = stream.chunk().await? {
///         body.extend_from_slice(&piece);
///     }
///     // The body is in: from here on, at most 10 seconds (a 503 after).
///     let id = giong::time::timeout(Duration::from_secs(10), store(body))
///         .await
///         .map_err(Error::unavailable)??;
///     Ok(Response::text(format!("stored {id}")))
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Timeout {
    limit: Duration,
}

impl Timeout {
    /// A limit of `limit`.
    ///
    /// # Panics
    ///
    /// Panics if `limit` is zero (`GIONG-E1213`).
    #[track_caller]
    pub fn new(limit: Duration) -> Self {
        if limit.is_zero() {
            super::invalid_config(
                "Timeout::new: the limit is zero, so every request would time out".into(),
                "give a limit such as Duration::from_secs(10).",
            );
        }
        Self { limit }
    }
}

impl<S: 'static> Middleware<S> for Timeout {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        if req.body_stream().is_some() {
            return Ok(next.run(req).await);
        }
        match giong::time::timeout(self.limit, next.run(req)).await {
            Ok(resp) => Ok(resp),
            Err(_) => Err(Error::coded(
                503,
                Code::WebTimeout,
                format!(
                    "the server did not answer within {:?}; try again later",
                    self.limit
                ),
            )),
        }
    }
}
