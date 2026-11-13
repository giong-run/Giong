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

use giong::error::{Code, Error};

/// How a request body read as a stream ended badly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Past the handler's limit.
    TooLarge(u64),
    /// The client closed the connection or went quiet.
    Cut,
    /// The handler answered and the connection stopped reading the body.
    Abandoned,
    /// Broken chunked framing.
    Malformed,
}

impl Failure {
    fn into_io(self) -> io::Error {
        match self {
            Failure::TooLarge(limit) => Error::new(
                Code::HttpBodyTooLarge,
                format!("the request body is larger than the {limit} bytes this handler accepts"),
                "answer 413 Content Too Large, or raise the limit `Handler::stream_body` returns",
            )
            .into(),
            Failure::Cut => Error::new(
                Code::HttpBodyIncomplete,
                "the request body was cut short: the client closed the connection or sent \
                 nothing for the request timeout",
                "nothing to fix on the server; the client may retry. A slow client gets \
                 `Server::request_timeout` between two pieces of the body",
            )
            .into(),
            Failure::Abandoned => Error::new(
                Code::HttpBodyIncomplete,
                "the handler answered before reading the whole request body, so the \
                 connection stopped reading it",
                "read the body to its end before returning the response, or stop reading \
                 it once the response is returned",
            )
            .into(),
            Failure::Malformed => Error::new(
                Code::HttpBodyMalformed,
                "the request body's chunked framing is broken",
                "answer 400 Bad Request; the client sent an invalid chunked body",
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

    /// Ends the body, well or badly. The first end counts.
    pub(crate) fn finish(&self, end: Result<(), Failure>) {
        let mut inlet = self.inlet.borrow_mut();
        if inlet.end.is_none() {
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
    /// The sender is done: `Ok` for a complete body, `Err` for an aborted
    /// one.
    end: Option<Result<(), ()>>,
    /// The connection stopped taking pieces (client gone, `HEAD`, 204).
    closed: bool,
    sender: Option<Waker>,
    receiver: Option<Waker>,
}

/// What the connection gets from a streamed response body.
pub(crate) enum Next {
    Piece(Vec<u8>),
    End,
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
        poll_fn(|cx| {
            let mut outlet = self.outlet.borrow_mut();
            if let Some(piece) = outlet.piece.take() {
                if let Some(sender) = outlet.sender.take() {
                    sender.wake();
                }
                return Poll::Ready(Next::Piece(piece));
            }
            match outlet.end {
                Some(Ok(())) => Poll::Ready(Next::End),
                Some(Err(())) => Poll::Ready(Next::Aborted),
                None => {
                    outlet.receiver = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
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

    /// `true` once the body can no longer be sent (see [`send`](Self::send)).
    pub fn is_closed(&self) -> bool {
        self.outlet.borrow().closed
    }

    fn end(&self, end: Result<(), ()>) {
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
        self.end(Ok(()));
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
        "stop producing the body; nothing more will reach the client",
    )
    .into()
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
