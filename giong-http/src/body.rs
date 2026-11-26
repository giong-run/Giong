//! Bodies that do not fit in memory, or are not known in full when the
//! answer starts: a request body read as it arrives ([`BodyStream`]), and a
//! response body written as it is produced ([`BodySender`]).
//!
//! Both sides are a one-piece pipe between the connection and the code that
//! reads or writes the body. Everything stays on the connection's worker,
//! so the pipes are `Rc<RefCell<..>>` with a waker on each side.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::io;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use giong::error::{Code, Error};

use crate::trailers::Trailers;

/// How a request body read as a stream ended badly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Past the handler's limit.
    TooLarge(u64),
    /// The client closed the connection or went quiet.
    Cut,
    /// The handler answered and the connection stopped reading the body.
    Abandoned,
    /// Broken chunked framing, or an HTTP/2 stream that broke the protocol
    /// (a body that does not match its `content-length`, data past the
    /// flow-control window), which the server reset.
    Malformed,
    /// The client reset the HTTP/2 stream.
    Reset,
    /// The body arrived slower than `Server::min_body_rate` (bytes per
    /// second) once its grace period was over.
    TooSlow(u32),
}

/// `Server::min_body_rate`: the slowest average rate, in bytes per second,
/// at which a streamed request body may arrive once its grace period is
/// over. Only the time the server spends waiting for the client counts, not
/// the time a handler takes with a piece. `per_sec == 0` is off, the
/// default. Two `u32`s: with `Config::http2_budget` no longer an `Option`,
/// `Config` stays the size it was (120 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MinRate {
    pub(crate) per_sec: u32,
    pub(crate) grace_ms: u32,
}

impl MinRate {
    pub(crate) fn is_on(self) -> bool {
        self.per_sec != 0
    }

    /// How much longer the server may wait for the client, once `got`
    /// bytes have arrived over `waited` of waiting: the grace period plus
    /// what `got` bytes earn at the rate, less what was waited already.
    /// Zero when the body is too slow. Only called when on.
    pub(crate) fn left(self, got: u64, waited: Duration) -> Duration {
        let earned = u128::from(self.grace_ms) * 1_000_000
            + u128::from(got) * 1_000_000_000 / u128::from(self.per_sec.max(1));
        let earned = Duration::from_nanos(u64::try_from(earned).unwrap_or(u64::MAX));
        earned.saturating_sub(waited)
    }

    /// How long to wait for the next piece: the request timeout, or less
    /// when the rate runs out first. The second value says whether the rate
    /// sets it, so that running out of it means `TooSlow`, not `Cut`.
    pub(crate) fn wait(self, clock: &RateClock, request_timeout: Duration) -> (Duration, bool) {
        if !self.is_on() {
            return (request_timeout, false);
        }
        let left = self.left(clock.got, clock.waited);
        if left < request_timeout {
            (left, true)
        } else {
            (request_timeout, false)
        }
    }
}

/// What a streamed request body has received, and how long the server has
/// waited for it, for `MinRate`. Kept only while the rate is on.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RateClock {
    pub(crate) got: u64,
    pub(crate) waited: Duration,
    /// Since when the server has been waiting for the next piece (HTTP/2,
    /// where the wait is not one `await`).
    pub(crate) since: Option<Instant>,
}

impl RateClock {
    /// A wait for the client starts.
    pub(crate) fn start(&mut self, now: Instant) {
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    /// `n` bytes arrived: the wait in progress, if any, ends.
    pub(crate) fn got(&mut self, n: usize, now: Instant) {
        if let Some(since) = self.since.take() {
            self.waited += now.saturating_duration_since(since);
        }
        self.got += n as u64;
    }

    /// Whether the body has run out of `rate` at `now`.
    pub(crate) fn too_slow(&self, rate: MinRate, now: Instant) -> bool {
        let waiting = self
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        rate.is_on() && rate.left(self.got, self.waited + waiting).is_zero()
    }
}

impl Failure {
    fn into_io(self) -> io::Error {
        match self {
            Failure::TooLarge(limit) => Error::new(
                Code::HttpBodyTooLarge,
                format!("the request body is larger than the {limit} bytes this handler accepts"),
                "answer 413 Content Too Large, or raise the limit `Handler::stream_body` returns.",
            )
            .into(),
            Failure::Cut => Error::new(
                Code::HttpBodyIncomplete,
                "the request body was cut short: the client closed the connection or sent \
                 nothing for the request timeout",
                "nothing to fix on the server; the client may retry. A slow client gets \
                 `Server::request_timeout` between two pieces of the body.",
            )
            .into(),
            Failure::Abandoned => Error::new(
                Code::HttpBodyIncomplete,
                "the handler answered before reading the whole request body, so the \
                 connection stopped reading it",
                "read the body to its end before returning the response, or stop reading \
                 it once the response is returned.",
            )
            .into(),
            Failure::Malformed => Error::new(
                Code::HttpBodyMalformed,
                "the request body's framing is broken: invalid chunked encoding, or an \
                 HTTP/2 stream that broke the protocol (a body longer or shorter than its \
                 content-length, data past the flow-control window), which the server reset",
                "answer 400 Bad Request; the client sent an invalid body.",
            )
            .into(),
            Failure::TooSlow(per_sec) => Error::new(
                Code::HttpBodyIncomplete,
                format!(
                    "the request body was cut short: it arrived slower than the {per_sec} \
                     bytes per second `Server::min_body_rate` asks for, once its grace period \
                     was over"
                ),
                "nothing to fix on the server if the client is hostile or broken. If real \
                 clients are that slow (a mobile upload, a stream that sends little), lower \
                 `min_body_rate` or lengthen its grace period.",
            )
            .into(),
            Failure::Reset => Error::new(
                Code::HttpBodyIncomplete,
                "the request body was cut short: the client cancelled the HTTP/2 stream \
                 (RST_STREAM)",
                "nothing to fix on the server: the client gave up on the request; stop \
                 working on it.",
            )
            .into(),
        }
    }
}

#[derive(Default)]
struct Inlet {
    /// A piece the connection received and the handler has not taken yet.
    piece: Option<Vec<u8>>,
    /// `None` until the body ends; then how it ended.
    end: Option<Result<(), Failure>>,
    /// The handler has asked for a piece at least once: the connection may
    /// read the body (and answer `Expect: 100-continue`) from then on.
    asked: bool,
    /// Bytes handed to the handler so far.
    taken: u64,
    /// The trailer fields a body that ended well ended with.
    trailers: Option<Trailers>,
    reader: Option<Waker>,
    writer: Option<Waker>,
}

/// The connection's side of a request body read as a stream.
pub(crate) struct Pipe {
    inlet: RefCell<Inlet>,
    /// The length the request announced, if it did.
    length: Option<u64>,
}

impl Pipe {
    pub(crate) fn new(length: Option<u64>) -> Self {
        Self {
            inlet: RefCell::default(),
            length,
        }
    }

    /// Waits until the handler first asks for the body.
    pub(crate) async fn asked(&self) {
        poll_fn(|cx| {
            let mut inlet = self.inlet.borrow_mut();
            if inlet.asked {
                Poll::Ready(())
            } else {
                inlet.writer = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    /// Hands `piece` to the handler, after waiting until it has taken the
    /// previous one: one piece in flight, so a slow handler slows the
    /// client down instead of filling memory.
    pub(crate) async fn put(&self, piece: Vec<u8>) {
        let mut piece = Some(piece);
        poll_fn(|cx| {
            let mut inlet = self.inlet.borrow_mut();
            if inlet.piece.is_some() {
                inlet.writer = Some(cx.waker().clone());
                return Poll::Pending;
            }
            inlet.piece = piece.take();
            if let Some(reader) = inlet.reader.take() {
                reader.wake();
            }
            Poll::Ready(())
        })
        .await
    }

    /// Hands `piece` to the handler if it has taken the previous one, or
    /// gives it back; `waker` is then woken once the handler takes it. For
    /// HTTP/2, where one connection feeds many streams and cannot wait for
    /// one of them.
    pub(crate) fn offer(&self, piece: Vec<u8>, waker: &Waker) -> Result<(), Vec<u8>> {
        let mut inlet = self.inlet.borrow_mut();
        if inlet.piece.is_some() {
            set_waker(&mut inlet.writer, waker);
            return Err(piece);
        }
        inlet.piece = Some(piece);
        if let Some(reader) = inlet.reader.take() {
            reader.wake();
        }
        Ok(())
    }

    /// Whether the handler has asked for the body; if not, `waker` is woken
    /// when it does.
    pub(crate) fn poll_asked(&self, waker: &Waker) -> bool {
        let mut inlet = self.inlet.borrow_mut();
        if !inlet.asked {
            set_waker(&mut inlet.writer, waker);
        }
        inlet.asked
    }

    /// Bytes the handler has taken so far; `waker` is woken when it takes
    /// more.
    pub(crate) fn poll_taken(&self, waker: &Waker) -> u64 {
        let mut inlet = self.inlet.borrow_mut();
        if inlet.piece.is_some() {
            set_waker(&mut inlet.writer, waker);
        }
        inlet.taken
    }

    /// Ends the body, well or badly. The first end counts.
    pub(crate) fn finish(&self, end: Result<(), Failure>) {
        self.finish_with(end, None);
    }

    /// Ends the body as [`finish`](Self::finish), with the trailer fields
    /// that came after it.
    pub(crate) fn finish_with(&self, end: Result<(), Failure>, trailers: Option<Trailers>) {
        let mut inlet = self.inlet.borrow_mut();
        if inlet.end.is_none() {
            if end.is_ok() {
                inlet.trailers = trailers.filter(|t| !t.is_empty());
            }
            inlet.end = Some(end);
        }
        if let Some(reader) = inlet.reader.take() {
            reader.wake();
        }
    }
}

/// A request body read as it arrives, for a handler whose
/// [`Handler::stream_body`](crate::Handler::stream_body) asked for one.
/// [`Request::body_stream`](crate::Request::body_stream) returns it.
///
/// ```no_run
/// use giong_http::{Handler, Request, Response};
///
/// /// Counts the bytes of uploads of up to 10 GiB, without holding them.
/// struct Upload;
///
/// impl Handler for Upload {
///     fn stream_body(&self, _req: &Request) -> Option<u64> {
///         Some(10 << 30)
///     }
///
///     async fn call(&self, req: Request) -> Response {
///         let Some(body) = req.body_stream() else {
///             return Response::new(400);
///         };
///         let mut total = 0;
///         loop {
///             match body.chunk().await {
///                 Ok(Some(piece)) => total += piece.len(),
///                 Ok(None) => break,
///                 Err(e) => return Response::new(400).body(e.to_string()),
///             }
///         }
///         Response::text(format!("{total} bytes"))
///     }
/// }
/// ```
///
/// Nothing of the body is read from the connection until the first call to
/// [`chunk`](Self::chunk), so a handler can refuse a request (an upload it
/// does not want, a client without credentials) before the client sends
/// the body: a client that sent `Expect: 100-continue` gets its `100
/// Continue` only then.
///
/// Each piece must arrive within the server's
/// [`request_timeout`](crate::Server::request_timeout) of the one before;
/// there is no deadline for the whole body. A handler that answers before
/// reading the body to its end makes the connection close after the
/// response.
#[derive(Clone)]
pub struct BodyStream {
    pipe: Rc<Pipe>,
}

impl BodyStream {
    pub(crate) fn new(pipe: Rc<Pipe>) -> Self {
        Self { pipe }
    }

    /// The next piece of the body, as received (and decoded, for a chunked
    /// body), or `Ok(None)` at its end.
    ///
    /// Cancel safe: a piece is only taken when it is returned, so dropping
    /// the future (a `timeout` around it) loses nothing.
    ///
    /// # Errors
    ///
    /// `GIONG-E1111` past the handler's limit, `GIONG-E1112` if the body was
    /// cut short, `GIONG-E1113` for broken chunked framing. After an error,
    /// the connection closes once the response is sent.
    pub async fn chunk(&self) -> io::Result<Option<Vec<u8>>> {
        poll_fn(|cx| {
            let mut inlet = self.pipe.inlet.borrow_mut();
            if !inlet.asked {
                inlet.asked = true;
                if let Some(writer) = inlet.writer.take() {
                    writer.wake();
                }
            }
            if let Some(piece) = inlet.piece.take() {
                inlet.taken += piece.len() as u64;
                if let Some(writer) = inlet.writer.take() {
                    writer.wake();
                }
                return Poll::Ready(Ok(Some(piece)));
            }
            match inlet.end {
                Some(Ok(())) => Poll::Ready(Ok(None)),
                Some(Err(failure)) => Poll::Ready(Err(failure.into_io())),
                None => {
                    inlet.reader = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// Reads the rest of the body into memory.
    ///
    /// Cancel safe in the same way as [`chunk`](Self::chunk), except that
    /// the pieces read so far are dropped with the future.
    ///
    /// # Errors
    ///
    /// As [`chunk`](Self::chunk), and `GIONG-E1111` once the body passes
    /// `limit` bytes.
    pub async fn read_to_end(&self, limit: usize) -> io::Result<Vec<u8>> {
        let mut all = Vec::new();
        while let Some(piece) = self.chunk().await? {
            if all.len() + piece.len() > limit {
                return Err(Failure::TooLarge(limit as u64).into_io());
            }
            if all.is_empty() {
                all = piece;
            } else {
                all.extend_from_slice(&piece);
            }
        }
        Ok(all)
    }

    /// The length the request announced with `Content-Length`; `None` for a
    /// chunked body.
    pub fn content_length(&self) -> Option<u64> {
        self.pipe.length
    }

    /// Bytes returned by [`chunk`](Self::chunk) so far.
    pub fn bytes_read(&self) -> u64 {
        self.pipe.inlet.borrow().taken
    }

    /// The trailer fields the client sent after the body: in a last HEADERS
    /// frame over HTTP/2, after the last chunk of a chunked HTTP/1.1 body.
    ///
    /// `None` until [`chunk`](Self::chunk) has returned `Ok(None)`, and
    /// after that if the client sent none. Fields that mean nothing in
    /// trailers (`content-length`, `host` and the like) are dropped. The
    /// trailers of one request are limited to 16 KiB over HTTP/1.1 and to
    /// the server's `max_head_size` over HTTP/2.
    pub fn trailers(&self) -> Option<Trailers> {
        let inlet = self.pipe.inlet.borrow();
        if inlet.piece.is_some() {
            return None;
        }
        inlet.trailers.clone()
    }
}

impl fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inlet = self.pipe.inlet.borrow();
        f.debug_struct("BodyStream")
            .field("content_length", &self.pipe.length)
            .field("bytes_read", &inlet.taken)
            .field("ended", &inlet.end.is_some())
            .finish()
    }
}

// ------------------------------------------------------------- responses

#[derive(Default)]
struct Outlet {
    piece: Option<Vec<u8>>,
    /// The sender is done: `Ok` for a complete body (with the trailer
    /// fields to send after it, until the connection takes them), `Err`
    /// for an aborted one.
    end: Option<Result<Option<Trailers>, ()>>,
    /// The connection stopped taking pieces (client gone, `HEAD`, 204).
    closed: bool,
    sender: Option<Waker>,
    receiver: Option<Waker>,
    /// Woken when the connection stops taking pieces: the task waiting in
    /// [`BodySender::closed`], which may not be the one that sends.
    watcher: Option<Waker>,
}

/// What the connection gets from a streamed response body.
pub(crate) enum Next {
    Piece(Vec<u8>),
    /// The end of the body, and the trailer fields to send after it.
    End(Option<Trailers>),
    Aborted,
}

/// The receiving half of [`Body::channel`](crate::Body::channel), held by
/// the response as [`Body::Stream`](crate::Body::Stream): the server takes
/// the pieces out of it as it writes them. Dropping it (with the response)
/// makes the sender's [`send`](BodySender::send) fail.
pub struct StreamBody {
    outlet: Rc<RefCell<Outlet>>,
}

impl StreamBody {
    /// The next piece the producer sent.
    pub(crate) async fn next(&self) -> Next {
        poll_fn(|cx| self.poll_next(cx.waker())).await
    }

    /// The next piece the producer sent, if there is one yet; `waker` is
    /// woken when there is.
    pub(crate) fn poll_next(&self, waker: &Waker) -> Poll<Next> {
        let mut outlet = self.outlet.borrow_mut();
        if let Some(piece) = outlet.piece.take() {
            if let Some(sender) = outlet.sender.take() {
                sender.wake();
            }
            return Poll::Ready(Next::Piece(piece));
        }
        match &mut outlet.end {
            Some(Ok(trailers)) => Poll::Ready(Next::End(trailers.take())),
            Some(Err(())) => Poll::Ready(Next::Aborted),
            None => {
                set_waker(&mut outlet.receiver, waker);
                Poll::Pending
            }
        }
    }
}

impl StreamBody {
    /// Appends the piece that waits to `into`, if it is no more than
    /// `room` bytes: small pieces sent one after the other leave together.
    pub(crate) fn take_small(&self, into: &mut Vec<u8>, room: usize) -> bool {
        let mut outlet = self.outlet.borrow_mut();
        if !outlet.piece.as_ref().is_some_and(|p| p.len() <= room) {
            return false;
        }
        if let Some(piece) = outlet.piece.take() {
            into.extend_from_slice(&piece);
        }
        if let Some(sender) = outlet.sender.take() {
            sender.wake();
        }
        true
    }
}

impl Drop for StreamBody {
    fn drop(&mut self) {
        let mut outlet = self.outlet.borrow_mut();
        outlet.closed = true;
        outlet.piece = None;
        if let Some(sender) = outlet.sender.take() {
            sender.wake();
        }
        if let Some(watcher) = outlet.watcher.take() {
            watcher.wake();
        }
    }
}

impl fmt::Debug for StreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamBody")
    }
}

/// Writes a response body piece by piece: the other half of
/// [`Body::channel`](crate::Body::channel).
///
/// The response goes out with `Transfer-Encoding: chunked` (to an HTTP/1.0
/// client, without a length, and the connection closes after it). Dropping
/// the sender ends the body; [`abort`](Self::abort) ends it as broken
/// instead, so the client can tell it is incomplete.
pub struct BodySender {
    outlet: Rc<RefCell<Outlet>>,
}

impl BodySender {
    /// Sends one piece of the body, after waiting until the previous one
    /// has been written to the connection: one piece in flight, so a slow
    /// client slows the producer down instead of filling memory. Empty
    /// pieces are skipped.
    ///
    /// Cancel safe: if the future is dropped before it returns, the piece
    /// was not sent.
    ///
    /// # Errors
    ///
    /// `GIONG-E1114` when the body can no longer be sent: the client went
    /// away, the connection timed out writing, or the response has no body
    /// (`HEAD`, 204, 304). Stop producing then.
    pub async fn send(&self, piece: impl Into<Vec<u8>>) -> io::Result<()> {
        let piece = piece.into();
        if piece.is_empty() {
            return if self.outlet.borrow().closed {
                Err(not_sent())
            } else {
                Ok(())
            };
        }
        let mut piece = Some(piece);
        poll_fn(|cx| {
            let mut outlet = self.outlet.borrow_mut();
            if outlet.closed {
                return Poll::Ready(Err(not_sent()));
            }
            if outlet.piece.is_some() {
                outlet.sender = Some(cx.waker().clone());
                return Poll::Pending;
            }
            outlet.piece = piece.take();
            if let Some(receiver) = outlet.receiver.take() {
                receiver.wake();
            }
            Poll::Ready(Ok(()))
        })
        .await
    }

    /// Ends the body as broken: the connection closes without the end of
    /// the chunked body, so the client sees an incomplete response instead
    /// of a short one. For a producer that fails halfway.
    pub fn abort(self) {
        self.end(Err(()));
    }

    /// Ends the body, then sends `trailers` after it: in a last HEADERS
    /// frame over HTTP/2, after the last chunk over HTTP/1.1. Pieces
    /// already sent go out first. Empty trailers end the body as dropping
    /// the sender does.
    ///
    /// An HTTP/1.0 client gets the body without the trailers, since its
    /// response is not chunked. Over HTTP/1.1 the trailers are sent whether
    /// or not the request had `TE: trailers`; a client that does not expect
    /// them ignores them.
    ///
    /// # Errors
    ///
    /// `GIONG-E1114` when the body can no longer be sent (see
    /// [`send`](Self::send)): the trailers were not sent either.
    pub fn send_trailers(self, trailers: Trailers) -> io::Result<()> {
        if self.outlet.borrow().closed {
            return Err(not_sent());
        }
        self.end(Ok(Some(trailers).filter(|t| !t.is_empty())));
        Ok(())
    }

    /// `true` once the body can no longer be sent (see [`send`](Self::send)).
    pub fn is_closed(&self) -> bool {
        self.outlet.borrow().closed
    }

    /// Waits until the body can no longer be sent (see
    /// [`send`](Self::send)): for a producer that waits on something else
    /// (a queue, a timer, the request's own body) and should stop when the
    /// client resets its HTTP/2 stream or its connection closes. It does
    /// not return when the body ends well: the sender is gone by then.
    ///
    /// Over HTTP/1.1 a connection that is writing a streamed body does not
    /// read, so a client that goes away is only noticed when the next
    /// piece is written: there, [`send`](Self::send) is what tells.
    ///
    /// May be awaited in another task than the one that sends, by one task
    /// at a time.
    ///
    /// Cancel safe: a dropped future leaves nothing behind.
    pub async fn closed(&self) {
        poll_fn(|cx| {
            let mut outlet = self.outlet.borrow_mut();
            if outlet.closed {
                return Poll::Ready(());
            }
            set_waker(&mut outlet.watcher, cx.waker());
            Poll::Pending
        })
        .await
    }

    fn end(&self, end: Result<Option<Trailers>, ()>) {
        let mut outlet = self.outlet.borrow_mut();
        if outlet.end.is_none() {
            outlet.end = Some(end);
        }
        if let Some(receiver) = outlet.receiver.take() {
            receiver.wake();
        }
    }
}

impl Drop for BodySender {
    fn drop(&mut self) {
        self.end(Ok(None));
    }
}

impl fmt::Debug for BodySender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodySender")
            .field("closed", &self.is_closed())
            .finish()
    }
}

fn not_sent() -> io::Error {
    Error::new(
        Code::HttpBodyNotSent,
        "the response body cannot be sent: the client went away, or the response has no body",
        "stop producing the body; nothing more will reach the client.",
    )
    .into()
}

/// Stores `waker` unless the one stored already wakes the same task.
fn set_waker(slot: &mut Option<Waker>, waker: &Waker) {
    if !slot.as_ref().is_some_and(|w| w.will_wake(waker)) {
        *slot = Some(waker.clone());
    }
}

/// A connected sender and body.
pub(crate) fn channel() -> (BodySender, StreamBody) {
    let outlet = Rc::new(RefCell::new(Outlet::default()));
    (
        BodySender {
            outlet: outlet.clone(),
        },
        StreamBody { outlet },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fix is a sentence: the message reads "... Fix: <fix>. See <url>".
    #[test]
    fn every_fix_is_a_sentence() {
        let failures = [
            Failure::TooLarge(10),
            Failure::Cut,
            Failure::Abandoned,
            Failure::Malformed,
            Failure::Reset,
        ];
        for io in failures
            .into_iter()
            .map(Failure::into_io)
            .chain([not_sent()])
        {
            let error = io
                .get_ref()
                .and_then(|e| e.downcast_ref::<Error>())
                .expect("a giong error");
            assert!(error.fix().ends_with('.'), "{error}");
        }
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn the_allowance_is_the_grace_plus_what_the_bytes_earn() {
        let rate = MinRate {
            per_sec: 1000,
            grace_ms: 200,
        };
        let ms = Duration::from_millis;
        assert_eq!(rate.left(0, Duration::ZERO), ms(200));
        assert_eq!(rate.left(500, Duration::ZERO), ms(700));
        assert_eq!(rate.left(500, ms(650)), ms(50));
        assert_eq!(rate.left(500, ms(700)), Duration::ZERO);
        assert_eq!(rate.left(500, ms(5000)), Duration::ZERO);
        // No overflow at the ends of the ranges.
        let slow = MinRate {
            per_sec: 1,
            grace_ms: u32::MAX,
        };
        assert!(slow.left(u64::MAX, Duration::ZERO) > Duration::from_secs(1 << 30));
        // The request timeout decides while the rate leaves more time.
        let clock = RateClock::default();
        assert_eq!(rate.wait(&clock, ms(100)), (ms(100), false));
        assert_eq!(rate.wait(&clock, ms(30_000)), (ms(200), true));
        assert_eq!(
            MinRate::default().wait(&clock, ms(30_000)),
            (ms(30_000), false)
        );
    }

    #[test]
    fn a_clock_counts_only_the_waits() {
        let rate = MinRate {
            per_sec: 1000,
            grace_ms: 100,
        };
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut clock = RateClock::default();
        clock.start(t0);
        clock.got(50, t0 + ms(80));
        // Not waiting: the handler's time is not counted.
        assert!(!clock.too_slow(rate, t0 + ms(10_000)));
        clock.start(t0 + ms(10_000));
        assert!(!clock.too_slow(rate, t0 + ms(10_000) + ms(60)));
        assert!(clock.too_slow(rate, t0 + ms(10_000) + ms(70)));
        assert!(!clock.too_slow(MinRate::default(), t0 + ms(99_999)));
    }
}
