//! [`Trace`]: a `tracing` span per request (feature `tracing`).

use std::time::Instant;

use tracing::field::Empty;
use tracing::{Instrument, Level};

use crate::error::Rendered;
use crate::log::worker;
use crate::{Error, Middleware, Next, Req, Response};

/// Runs every request inside a [`tracing`] span, and ends it with one
/// event, for services that already collect `tracing` data (to a JSON
/// subscriber, to OpenTelemetry). Events the handler and the code it calls
/// record are inside the span, so they carry the request's route and id.
///
/// ```no_run
/// use giong_web::middleware::{RequestId, Trace};
/// use giong_web::{Error, Req, Response, Router};
///
/// async fn user(req: Req) -> Result<Response, Error> {
///     let id: u64 = req.param("id")?;
///     tracing::debug!(id, "loading the user"); // inside the request's span
///     Ok(Response::text(format!("user {id}")))
/// }
///
/// let app = Router::new()
///     .get("/users/{id}", user)
///     .layer(RequestId::new())
///     .layer(Trace::new())
///     .build(());
/// # let _ = app;
/// ```
///
/// The span is named `request`, at level `INFO`, with target `giong_web`.
/// Its fields follow OpenTelemetry's names for HTTP servers:
///
/// | Field | Value |
/// |---|---|
/// | `http.request.method` | the method, as sent |
/// | `url.path` | the path, still percent-encoded |
/// | `http.route` | the pattern that matched; absent when none did |
/// | `request_id` | from [`RequestId`](super::RequestId), when it runs before |
/// | `client.address` | the client's address; absent over a Unix socket |
/// | `worker` | the giong worker that serves the request |
/// | `http.response.status_code` | recorded once the response is ready |
///
/// The event that ends the span is `request` at `INFO`, with `status` and
/// `duration_us` (until the response was ready; sending it is not counted).
/// For a response made from an [`Error`] it also has `error.code` and
/// `error.detail`, and a `5xx` is at `ERROR`, named `request failed`, with
/// the cause in `error.cause`: the client gets only the code and a generic
/// detail. Such an error then counts as logged, and is not written to
/// stderr as an unlogged one.
///
/// With no subscriber, or one that leaves the target out, the span is
/// disabled and the middleware only passes the request on.
/// [`Logger`](super::Logger) and `Trace` can run side by side.
#[derive(Clone, Copy, Debug, Default)]
pub struct Trace {
    _private: (),
}

impl Trace {
    /// A span per request, as described above.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S: 'static> Middleware<S> for Trace {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let span = tracing::info_span!(
            target: "giong_web",
            "request",
            http.request.method = req.method(),
            url.path = req.path(),
            http.route = req.route(),
            request_id = req.request_id(),
            client.address = req.peer_addr().map(tracing::field::display),
            worker = worker(),
            http.response.status_code = Empty,
        );
        if span.is_disabled() {
            return Ok(next.run(req).await);
        }
        let start = Instant::now();
        let resp = next.run(req).instrument(span.clone()).await;
        let status = resp.status();
        let duration_us = start.elapsed().as_micros() as u64;
        span.record("http.response.status_code", status);
        match resp.extension::<Rendered>() {
            Some(r) if status >= 500 => {
                let cause = r.error.cause_chain();
                tracing::event!(
                    target: "giong_web",
                    parent: &span,
                    Level::ERROR,
                    status,
                    duration_us,
                    error.code = r.error.code(),
                    error.detail = r.error.detail(),
                    error.cause = cause.as_deref(),
                    "request failed"
                );
                r.logged.set(true);
            }
            Some(r) => tracing::event!(
                target: "giong_web",
                parent: &span,
                Level::INFO,
                status,
                duration_us,
                error.code = r.error.code(),
                error.detail = r.error.detail(),
                "request"
            ),
            None => tracing::event!(
                target: "giong_web",
                parent: &span,
                Level::INFO,
                status,
                duration_us,
                "request"
            ),
        }
        Ok(resp)
    }
}
