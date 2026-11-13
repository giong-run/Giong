//! [`Timeout`]: a limit on how long a handler may take.

use std::time::Duration;

use giong::error::Code;

use crate::{Error, Middleware, Next, Req, Response};

/// Answers `503 Service Unavailable` (`GIONG-E1209`) when the rest of the
/// chain takes longer than the limit to produce a response, and drops the
/// handler's future.
///
/// Dropping the future cancels what the handler was waiting for: giong's
/// operations are safe to cancel, and its database clients close or reset a
/// connection whose query was abandoned instead of reusing it.
///
/// The limit covers producing the response, not receiving the request
/// (giong-http's `request_timeout`) or sending it (`write_timeout`). So a
/// request whose body is read as a stream
/// ([`Router::stream_bodies`](crate::Router::stream_bodies)) has no limit
/// here, since the handler runs while the body arrives; and for a response
/// body written as it is produced, the limit ends when the handler returns
/// the response.
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
