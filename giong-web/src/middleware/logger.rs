//! [`Logger`]: one JSON line per request.

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use crate::error::Rendered;
use crate::json::JsonWriter;
use crate::log::{clip, error_fields, timestamp, worker, Sink};
use crate::{Error, Middleware, Next, Req, Response};

/// Writes one JSON line per request, after the response is ready:
///
/// ```json
/// {"ts":"2026-11-13T08:40:01.123456Z","level":"info","event":"request","method":"GET","path":"/users/7","route":"/users/{id}","status":200,"duration_us":153,"bytes":512,"request_id":"5f3a9c2e8b41d706000100000000002a","peer":"10.0.0.7:51234","worker":1}
/// ```
///
/// | Field | Value |
/// |---|---|
/// | `ts` | when the request reached the logger: RFC 3339, UTC, microseconds |
/// | `level` | `error` for a `5xx` status, `info` otherwise |
/// | `event` | `request`, or `request cancelled` (see below) |
/// | `method`, `path` | as sent; the path still percent-encoded, and cut to 4 KiB when longer (ending `...(+N bytes)`) |
/// | `query` | the query string, only with [`include_query`](Self::include_query); cut like the path |
/// | `route` | the pattern that matched, or `null` |
/// | `status` | the response status |
/// | `duration_us` | microseconds until the response was ready (sending it is not counted) |
/// | `bytes` | the length of the response body |
/// | `request_id` | from [`RequestId`](super::RequestId), when it runs before the logger |
/// | `peer` | the client's address, or `null` over a Unix socket |
/// | `worker` | the giong worker that served the request |
/// | `error` | for a response made from an [`Error`]: `code`, `detail` and, when there is one, `cause` |
///
/// When the request is dropped before its response is ready (the client
/// reset its HTTP/2 stream, or a [`Timeout`](super::Timeout) layered
/// outside the logger gave up), the line is written at the drop instead,
/// with `"event":"request cancelled"`, `"status":null`, `"bytes":null` and
/// the time until then in `duration_us`; nginx logs the same case as `499`.
///
/// The cause of a `5xx` error is in the line and nowhere else: the client
/// only gets the code and a generic detail. Lines go through the background
/// writer described at [`Sink`], so logging costs a worker no system call.
#[derive(Clone, Debug)]
pub struct Logger {
    sink: Sink,
    query: bool,
}

impl Logger {
    /// A logger writing to `sink`.
    pub fn new(sink: Sink) -> Self {
        Self { sink, query: false }
    }

    /// Adds the query string to each line (default: `false`, since query
    /// strings often carry tokens and personal data).
    pub fn include_query(mut self, include: bool) -> Self {
        self.query = include;
        self
    }
}

impl<S: 'static> Middleware<S> for Logger {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let mut pending = Pending {
            sink: &self.sink,
            start: Instant::now(),
            ts: SystemTime::now(),
            method: req.method().to_owned(),
            path: clip(req.path()).into_owned(),
            query: if self.query {
                req.query_string().map(|q| clip(q).into_owned())
            } else {
                None
            },
            route: req.route().map(str::to_owned),
            peer: req.peer_addr(),
            id: req.request_id().map(str::to_owned),
            done: false,
        };

        let resp = next.run(req).await;
        pending.done = true;
        let elapsed = pending.start.elapsed();

        let status = resp.status();
        let mut line = pending.head(if status >= 500 { "error" } else { "info" }, "request");
        line.u64("status", u64::from(status));
        line.u64("duration_us", elapsed.as_micros() as u64);
        if resp.is_streaming() {
            // Written after this line, piece by piece.
            line.null("bytes");
        } else {
            line.u64("bytes", resp.body_len() as u64);
        }
        let rendered = resp.extension::<Rendered>();
        let id = pending.id.take().or_else(|| {
            rendered
                .and_then(|r| r.request_id.as_ref())
                .map(|i| i.as_str().to_owned())
        });
        pending.tail(&mut line, id.as_deref());
        if let Some(r) = rendered {
            error_fields(&mut line, &r.error);
            r.logged.set(true);
        }
        self.sink.write(&line.finish_line());
        Ok(resp)
    }
}

/// What the line needs from the request, kept until the response is ready
/// and written by `Drop` if it never is.
struct Pending<'a> {
    sink: &'a Sink,
    start: Instant,
    ts: SystemTime,
    method: String,
    path: String,
    query: Option<String>,
    route: Option<String>,
    peer: Option<SocketAddr>,
    id: Option<String>,
    done: bool,
}

impl Pending<'_> {
    /// The fields before `status`.
    fn head(&self, level: &str, event: &str) -> JsonWriter {
        let mut line = JsonWriter::with_capacity(256 + self.path.len());
        line.str("ts", &timestamp(self.ts));
        line.str("level", level);
        line.str("event", event);
        line.str("method", &self.method);
        line.str("path", &self.path);
        if let Some(q) = &self.query {
            line.str("query", q);
        }
        match &self.route {
            Some(r) => line.str("route", r),
            None => line.null("route"),
        }
        line
    }

    /// The fields after `bytes`.
    fn tail(&self, line: &mut JsonWriter, id: Option<&str>) {
        match id {
            Some(id) => line.str("request_id", id),
            None => line.null("request_id"),
        }
        match self.peer {
            Some(addr) => line.str("peer", &addr.to_string()),
            None => line.null("peer"),
        }
        if let Some(n) = worker() {
            line.u64("worker", n as u64);
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        // A panic is caught further in and answered as a `500`; one that
        // gets here comes from the logger's own sink.
        if self.done || std::thread::panicking() {
            return;
        }
        // The request was dropped before its response was ready: say so,
        // or it would leave no trace at all.
        let mut line = self.head("info", "request cancelled");
        line.null("status");
        line.u64("duration_us", self.start.elapsed().as_micros() as u64);
        line.null("bytes");
        self.tail(&mut line, self.id.as_deref());
        self.sink.write(&line.finish_line());
    }
}
