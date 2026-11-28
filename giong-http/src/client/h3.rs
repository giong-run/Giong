//! The client side of an HTTP/3 connection (RFC 9114) on giong-quic.
//!
//! A request is one bidirectional stream: the client writes the head and
//! the body and ends its side, then reads the response from the other.
//! Nothing is shared between requests but the connection, so a slow
//! response holds back no other, and QUIC's flow control bounds what the
//! server can make the client hold for each of them.
//!
//! One task reads what the server opens: its control stream (SETTINGS,
//! then GOAWAY when it wants no more requests) and its QPACK streams. By
//! default the client allows no dynamic table, so those have nothing to
//! say; with `ClientBuilder::qpack_table_size` the server's encoder stream
//! fills one, a response that refers to insertions not received yet waits
//! for them, and the client's decoder stream (`qpack_conn::Decoder`)
//! acknowledges what was decoded. A server that breaks the protocol there
//! or on a response has the connection closed with the error code RFC 9114
//! or RFC 9204 names.

use std::cell::{Cell, RefCell};
use std::future::{poll_fn, Future};
use std::io;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use giong_quic::{RecvStream, SendStream};

use super::h2::{check_field, Decoded, Request, ResponseHead, StreamError};
use crate::body::{Next, StreamBody};
use crate::h2::hpack::Index;
use crate::h3::frame::{self, kind, Code, Header, Need, On, Settings, StreamType};
use crate::h3::qpack::{self, Encoder};
use crate::h3::qpack_conn::Decoder;
use crate::trailers::Trailers;

/// The largest SETTINGS frame read. HTTP/3 defines three settings.
const MAX_SETTINGS_FRAME: u64 = 4 * 1024;

/// Frames of unknown types a server may send on its control stream, a
/// second.
const MAX_UNKNOWN_FRAMES: u32 = 1024;

/// Informational (1xx) responses accepted before the final one, as over
/// HTTP/1.1 and HTTP/2.
const MAX_INFORMATIONAL: u32 = 32;

/// What the connection's streams and its task share.
struct State {
    /// Why the connection carries no more requests, once it does not.
    closed: RefCell<Option<String>>,
    /// The first request stream the server said it will not handle.
    goaway: Cell<Option<u64>>,
    /// Which of the server's critical streams exist: control (1), QPACK
    /// encoder (2), QPACK decoder (4).
    critical: Cell<u8>,
    /// The client's control stream stays open for as long as the
    /// connection: closing it is a connection error (section 6.2.1).
    control: RefCell<Option<SendStream>>,
    /// The decoder of the server's field sections, when the client allows
    /// the server a dynamic table (`ClientBuilder::qpack_table_size`).
    qpack: Option<Decoder>,
}

impl State {
    /// Nothing more is decoded: requests waiting for insertions stop.
    fn end_qpack(&self) {
        if let Some(decoder) = &self.qpack {
            decoder.end();
        }
    }

    /// Closes the connection for an error of the server's, once.
    fn fail(&self, quic: &giong_quic::Connection, error: frame::Error) {
        let mut closed = self.closed.borrow_mut();
        if closed.is_none() {
            *closed = Some(format!("the server broke HTTP/3: {error}"));
            quic.close(error.code as u32, error.what.as_bytes());
        }
        drop(closed);
        self.end_qpack();
    }

    /// Notes that the connection is gone.
    fn lost(&self, why: &io::Error) {
        let mut closed = self.closed.borrow_mut();
        if closed.is_none() {
            *closed = Some(why.to_string());
        }
        drop(closed);
        self.end_qpack();
    }
}

struct Shared {
    quic: Rc<giong_quic::Connection>,
    state: Rc<State>,
    max_head_size: usize,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The last request is over and the client let the connection go.
        self.quic.close(Code::NoError as u32, b"");
        self.state.end_qpack();
    }
}

/// A connection, shared by the requests on it. Cloning gives another
/// handle to the same connection, which closes when the last one is
/// dropped.
#[derive(Clone)]
pub(crate) struct Connection {
    shared: Rc<Shared>,
}

impl Connection {
    /// Speaks HTTP/3 on a QUIC connection just made: opens the control
    /// stream with the client's settings and starts reading the server's.
    /// `qpack_table_size` is the dynamic table the server may fill for its
    /// field sections, 0 for none.
    pub(crate) async fn start(
        quic: giong_quic::Connection,
        max_head_size: usize,
        qpack_table_size: u32,
    ) -> io::Result<Self> {
        let quic = Rc::new(quic);
        let control = open_control(&quic, max_head_size, qpack_table_size).await?;
        let early = control.is_early();
        let state = Rc::new(State {
            closed: RefCell::new(None),
            goaway: Cell::new(None),
            critical: Cell::new(0),
            control: RefCell::new(Some(control)),
            qpack: (qpack_table_size > 0).then(|| Decoder::new(qpack_table_size)),
        });
        drop(giong::spawn(server_streams(quic.clone(), state.clone())));
        if state.qpack.is_some() {
            let (quic, state) = (quic.clone(), state.clone());
            drop(giong::spawn(async move {
                if let Some(decoder) = &state.qpack {
                    decoder.write(&quic).await;
                }
            }));
        }
        if early {
            drop(giong::spawn(settle(
                quic.clone(),
                state.clone(),
                max_head_size,
                qpack_table_size,
            )));
        }
        Ok(Self {
            shared: Rc::new(Shared {
                quic,
                state,
                max_head_size,
            }),
        })
    }

    /// Waits for the end of the QUIC handshake, after which nothing more
    /// goes out in 0-RTT data. Cancel safe: it only waits.
    pub(crate) async fn handshake(&self) {
        let shared = &self.shared;
        if let Err(e) = shared.quic.handshake().await {
            shared.state.lost(&e);
        }
    }

    /// Whether a new request may go on this connection.
    pub(crate) fn is_open(&self) -> bool {
        let state = &self.shared.state;
        state.closed.borrow().is_none() && state.goaway.get().is_none()
    }

    /// Sends `req` on a stream of its own, waiting while the server allows
    /// no more streams. `Refused` means nothing of it was processed.
    pub(crate) async fn send(&self, req: &Request<'_>) -> Result<Stream, StreamError> {
        let (stream, cancel) = self.open(req, false).await?;
        let mut cancel = cancel;
        let send = cancel.0.as_mut().expect("held until finished");
        match send.finish() {
            Ok(()) => {}
            Err(e) if giong_quic::reset_code(&e) == Some(Code::NoError as u64) => {}
            Err(e) => {
                send.reset(Code::RequestCancelled as u32);
                let mut stream = stream;
                return Err(stream.failed(e));
            }
        }
        // Finished: nothing to cancel.
        drop(cancel.0.take());
        Ok(stream)
    }

    /// Sends `req`, whose body comes from a source as it is produced, and
    /// waits for the head of the response. The body is sent meanwhile, so
    /// a server may answer before the body is over; what is left of it
    /// then is sent by a task of its own. A body the producer aborts
    /// resets the stream with H3_REQUEST_CANCELLED and fails the request.
    pub(crate) async fn send_streamed(
        &self,
        req: &Request<'_>,
        body: &Rc<BodySource>,
    ) -> Result<Stream, StreamError> {
        let (mut stream, cancel) = self.open(req, true).await?;
        let mut pump = Some(Box::pin(pump(cancel, body.clone())));
        {
            let mut response = pin!(stream.response());
            poll_fn(|cx| {
                if let Some(p) = pump.as_mut() {
                    if let Poll::Ready(pumped) = p.as_mut().poll(cx) {
                        pump = None;
                        if let Pumped::Aborted = pumped {
                            return Poll::Ready(Err(StreamError::Reset(
                                "the request body was aborted (RequestBody::abort), so the \
                                 request was cancelled"
                                    .into(),
                            )));
                        }
                    }
                }
                response.as_mut().poll(cx)
            })
            .await?;
        }
        if let Some(mut pump) = pump {
            // HTTP/3 halves are independent (unlike HTTP/2's RST_STREAM):
            // a response given up must also end the upload, or the rest of
            // the body would still go out.
            let abandon = Rc::new(Abandon::default());
            stream.abandon = Some(abandon.clone());
            let body = body.clone();
            drop(giong::spawn(async move {
                poll_fn(|cx| {
                    if abandon.set.get() {
                        return Poll::Ready(());
                    }
                    abandon.waker.set(Some(cx.waker().clone()));
                    pump.as_mut().poll(cx).map(drop)
                })
                .await;
                if abandon.set.get() {
                    // Dropping the pump resets the stream with
                    // H3_REQUEST_CANCELLED (`Cancel`); without the source
                    // the producer's next send fails.
                    drop(pump);
                    drop(body.source.borrow_mut().take());
                }
            }));
        }
        Ok(stream)
    }

    /// Opens a stream for `req` and writes its head, and its body when the
    /// request holds it whole. `streamed`: a body follows from a source.
    async fn open(
        &self,
        req: &Request<'_>,
        streamed: bool,
    ) -> Result<(Stream, Cancel), StreamError> {
        let shared = &self.shared;
        let state = &shared.state;
        if let Some(why) = state.closed.borrow().as_ref() {
            return Err(StreamError::Refused(why.clone()));
        }
        let (send, recv) = match shared.quic.open_bi().await {
            Ok(pair) => pair,
            Err(e) => {
                state.lost(&e);
                return Err(StreamError::Refused(e.to_string()));
            }
        };
        // A request cut short (its future dropped mid-body) is cancelled
        // with H3_REQUEST_CANCELLED, never ended with a FIN inside a frame.
        let mut cancel = Cancel(Some(send));
        let send = cancel.0.as_mut().expect("just put there");
        if state.goaway.get().is_some_and(|first| send.id() >= first) {
            send.reset(Code::RequestCancelled as u32);
            return Err(StreamError::Refused(
                "the server is going away (GOAWAY)".into(),
            ));
        }
        // Early data can be replayed (RFC 8470), so only a request that is
        // harmless to repeat goes out in it; others wait for the handshake.
        let mut early = send.is_early();
        let bodiless = req.body.is_none() && !streamed;
        if early && !(bodiless && matches!(req.method, "GET" | "HEAD" | "OPTIONS")) {
            if let Err(e) = shared.quic.handshake().await {
                state.lost(&e);
                return Err(StreamError::Refused(e.to_string()));
            }
            // Nothing of it goes out in 0-RTT data now: a 425 for it is
            // the server's answer, never a reason to send it again.
            early = false;
        }
        let mut stream = Stream {
            conn: self.clone(),
            id: send.id(),
            r: Reader::new(recv),
            head: None,
            head_request: req.head_request,
            left: 0,
            received: 0,
            received_any: false,
            trailers: false,
            trailer_fields: None,
            done: false,
            early,
            qpack_waiting: false,
            complete: false,
            abandon: None,
        };
        let body = req.body.unwrap_or_default();
        let mut out = Vec::with_capacity(128 + req.path.len());
        write_head(&mut out, req);
        if !body.is_empty() {
            Header::write(&mut out, kind::DATA, body.len() as u64);
        }
        let mut sent = send.write_all(&out).await;
        if sent.is_ok() && !body.is_empty() {
            sent = send.write_all(body).await;
        }
        match sent {
            Ok(()) => {}
            // A server that has its answer before the request ends (a 413,
            // say) may stop reading: the answer is what matters then.
            Err(e) if giong_quic::reset_code(&e) == Some(Code::NoError as u64) => {}
            Err(e) => {
                send.reset(Code::RequestCancelled as u32);
                return Err(stream.failed(e));
            }
        }
        Ok((stream, cancel))
    }
}

/// The source of a streamed request body, shared by [`pump`] and the
/// request: the request takes the source back when the server refused it
/// before anything of the body was taken, so that it can be sent again.
pub(crate) struct BodySource {
    source: RefCell<Option<StreamBody>>,
    pulled: Cell<bool>,
}

impl BodySource {
    pub(crate) fn new(source: StreamBody) -> Self {
        Self {
            source: RefCell::new(Some(source)),
            pulled: Cell::new(false),
        }
    }

    /// The source, if nothing was taken from it.
    pub(crate) fn unused(&self) -> Option<StreamBody> {
        if self.pulled.get() {
            return None;
        }
        self.source.borrow_mut().take()
    }
}

/// Tells the task sending what is left of a streamed request body that
/// the response was given up before it ended (dropped, or the request
/// timed out), so that the body stops too.
#[derive(Default)]
struct Abandon {
    set: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl Abandon {
    fn now(&self) {
        self.set.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// How sending a streamed request body ended.
#[derive(Debug)]
enum Pumped {
    /// All of it, and its trailers, went out; or the server, having its
    /// answer, stopped reading (STOP_SENDING with H3_NO_ERROR).
    Done,
    /// The producer aborted the body: the stream was reset with
    /// H3_REQUEST_CANCELLED.
    Aborted,
    /// The stream or the connection failed: the response says how.
    Failed,
}

/// Sends a streamed request body: each piece as a DATA frame, then the
/// trailer fields in a HEADERS frame if the producer gave any, then the
/// end of the stream.
async fn pump(mut cancel: Cancel, body: Rc<BodySource>) -> Pumped {
    let mut frame = Vec::with_capacity(16);
    let ended: io::Result<()> = loop {
        let send = cancel.0.as_mut().expect("held until the body ends");
        // The next piece; or the server, having answered, asks for no more
        // (RFC 9114, section 4.1.2).
        let next = {
            let mut stopped = pin!(send.stopped());
            poll_fn(|cx| {
                if let Poll::Ready(stop) = stopped.as_mut().poll(cx) {
                    return Poll::Ready(Err(stop));
                }
                match &*body.source.borrow() {
                    Some(source) => source.poll_next(cx.waker()).map(Ok),
                    None => Poll::Ready(Ok(Next::Aborted)),
                }
            })
            .await
        };
        let written = match next {
            Ok(Next::Piece(piece)) => {
                body.pulled.set(true);
                if piece.is_empty() {
                    continue;
                }
                frame.clear();
                Header::write(&mut frame, kind::DATA, piece.len() as u64);
                match send.write_all(&frame).await {
                    Ok(()) => send.write_all(&piece).await,
                    Err(e) => Err(e),
                }
            }
            Ok(Next::End(trailers)) => {
                body.pulled.set(true);
                let mut written = Ok(());
                if let Some(trailers) = trailers.filter(|t| !t.is_empty()) {
                    frame.clear();
                    write_trailers(&mut frame, &trailers);
                    written = send.write_all(&frame).await;
                }
                break written.and_then(|()| send.finish());
            }
            // Dropping `cancel` resets the stream.
            Ok(Next::Aborted) => return Pumped::Aborted,
            // Asked to stop: with H3_NO_ERROR the server has its answer
            // and the request is complete as far as it cares.
            Err(Ok(code)) if code == Code::NoError as u64 => {
                send.reset(Code::NoError as u32);
                drop(cancel.0.take());
                drop(body.source.borrow_mut().take());
                return Pumped::Done;
            }
            // Rejected, say: a source nothing was taken from stays, for
            // the request to be sent again.
            Err(Ok(_)) => {
                if body.pulled.get() {
                    drop(body.source.borrow_mut().take());
                }
                return Pumped::Failed;
            }
            Err(Err(e)) => Err(e),
        };
        if let Err(e) = written {
            break Err(e);
        }
    };
    // Dropped: the producer's next send fails, nothing more is taken.
    if body.pulled.get() {
        drop(body.source.borrow_mut().take());
    }
    match ended {
        Ok(()) => {
            drop(cancel.0.take());
            Pumped::Done
        }
        Err(e) if giong_quic::reset_code(&e) == Some(Code::NoError as u64) => {
            if let Some(send) = cancel.0.as_mut() {
                send.reset(Code::NoError as u32);
            }
            drop(cancel.0.take());
            Pumped::Done
        }
        Err(_) => Pumped::Failed,
    }
}

/// Appends the HEADERS frame of request trailers. HTTP/3 field names are
/// lowercase (RFC 9114, section 4.2).
fn write_trailers(out: &mut Vec<u8>, trailers: &Trailers) {
    let mut block = Vec::with_capacity(64);
    let mut name = Vec::new();
    Encoder::begin(&mut block);
    for (n, value) in trailers.iter() {
        name.clear();
        name.extend(n.bytes().map(|b| b.to_ascii_lowercase()));
        let index = if value.len() > 256 {
            Index::No
        } else {
            Index::Yes
        };
        Encoder::field(&name, value, index, &mut block);
    }
    Header::write(out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
}

/// Holds a request's sending half while it is written: dropped holding
/// it, it resets the stream with H3_REQUEST_CANCELLED.
struct Cancel(Option<SendStream>);

impl Drop for Cancel {
    fn drop(&mut self) {
        if let Some(mut send) = self.0.take() {
            send.reset(Code::RequestCancelled as u32);
        }
    }
}

/// Opens the client's control stream and sends its settings.
async fn open_control(
    quic: &giong_quic::Connection,
    max_head_size: usize,
    qpack_table_size: u32,
) -> io::Result<SendStream> {
    let mut control = quic.open_uni().await?;
    let mut out = vec![StreamType::CONTROL as u8];
    Settings::write_frame(&mut out, max_head_size as u64, u64::from(qpack_table_size));
    control.write_all(&out).await?;
    Ok(control)
}

/// For a control stream opened in 0-RTT data: once the handshake is over,
/// opens it again if the server refused that data, since it then never
/// saw the client's settings.
async fn settle(
    quic: Rc<giong_quic::Connection>,
    state: Rc<State>,
    max_head_size: usize,
    qpack_table_size: u32,
) {
    let reopened = match quic.handshake().await {
        Ok(true) => return,
        Ok(false) => open_control(&quic, max_head_size, qpack_table_size).await,
        Err(e) => Err(e),
    };
    match reopened {
        Ok(control) => *state.control.borrow_mut() = Some(control),
        Err(e) => state.lost(&e),
    }
}

/// Appends the HEADERS frame of `req`.
fn write_head(out: &mut Vec<u8>, req: &Request<'_>) {
    let mut block = Vec::with_capacity(64 + req.path.len());
    Encoder::begin(&mut block);
    Encoder::field(b":method", req.method.as_bytes(), Index::Yes, &mut block);
    Encoder::field(b":scheme", req.scheme.as_bytes(), Index::Yes, &mut block);
    Encoder::field(
        b":authority",
        req.authority.as_bytes(),
        Index::Yes,
        &mut block,
    );
    Encoder::field(b":path", req.path.as_bytes(), Index::Yes, &mut block);
    for (name, value) in req.fields {
        let index = match &**name {
            "authorization" | "cookie" | "proxy-authorization" => Index::Never,
            _ => Index::Yes,
        };
        Encoder::field(name.as_bytes(), value.as_bytes(), index, &mut block);
    }
    Header::write(out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
}

/// What stops the reading of a stream.
enum Fail {
    /// The stream or the connection failed under it.
    Io(io::Error),
    /// The server broke the protocol: a connection error.
    Conn(frame::Error),
}

impl From<frame::Error> for Fail {
    fn from(e: frame::Error) -> Self {
        Fail::Conn(e)
    }
}

/// A stream ends inside a frame (RFC 9114, section 7.1).
const CUT: frame::Error = frame::Error::new(Code::FrameError, "a stream ends inside a frame");

/// A stream being read: frames are cut out of what has arrived.
struct Reader {
    recv: RecvStream,
    /// What was read and not used yet starts at `at`.
    buf: Vec<u8>,
    at: usize,
    ended: bool,
}

impl Reader {
    fn new(recv: RecvStream) -> Self {
        Self {
            recv,
            buf: Vec::new(),
            at: 0,
            ended: false,
        }
    }

    fn rest(&self) -> &[u8] {
        &self.buf[self.at..]
    }

    fn consume(&mut self, n: usize) {
        self.at += n;
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        }
    }

    /// Reads more of the stream; `false` at its end.
    async fn fill(&mut self) -> Result<bool, Fail> {
        if self.ended {
            return Ok(false);
        }
        match self.recv.read().await {
            Ok(Some(bytes)) => {
                if self.at > 0 {
                    self.buf.drain(..self.at);
                    self.at = 0;
                }
                self.buf.extend_from_slice(&bytes);
                Ok(true)
            }
            Ok(None) => {
                self.ended = true;
                Ok(false)
            }
            Err(e) => Err(Fail::Io(e)),
        }
    }

    /// The next frame header; `None` if the stream ends where a frame
    /// would start.
    async fn header(&mut self, on: On) -> Result<Option<Header>, Fail> {
        loop {
            if let Need::Got(header) = Header::read(self.rest(), on)? {
                self.consume(header.size);
                return Ok(Some(header));
            }
            if !self.fill().await? {
                if self.rest().is_empty() {
                    return Ok(None);
                }
                return Err(CUT.into());
            }
        }
    }

    /// The whole payload of a frame, `len` bytes.
    async fn exact(&mut self, len: usize) -> Result<Vec<u8>, Fail> {
        while self.rest().len() < len {
            if !self.fill().await? {
                return Err(CUT.into());
            }
        }
        let payload = self.rest()[..len].to_vec();
        self.consume(len);
        Ok(payload)
    }

    /// Reads past `len` bytes of payload without keeping them.
    async fn skip(&mut self, mut len: u64) -> Result<(), Fail> {
        loop {
            let n = (self.rest().len() as u64).min(len) as usize;
            self.consume(n);
            len -= n as u64;
            if len == 0 {
                return Ok(());
            }
            if !self.fill().await? {
                return Err(CUT.into());
            }
        }
    }
}

/// A request sent, and its response as it arrives.
pub(crate) struct Stream {
    conn: Connection,
    id: u64,
    r: Reader,
    head: Option<ResponseHead>,
    head_request: bool,
    /// Bytes that remain of the DATA frame being read.
    left: u64,
    /// Bytes of body read so far.
    received: u64,
    /// Something of the response has arrived.
    received_any: bool,
    /// The trailers were read: only the end of the stream may follow.
    trailers: bool,
    /// The trailer fields the response ended with, if any.
    trailer_fields: Option<Box<Trailers>>,
    /// The response ended and was read, or failed.
    done: bool,
    /// The stream was opened before the handshake ended, so the request
    /// may have gone out in 0-RTT data.
    early: bool,
    /// A field section waits for insertions on the server's encoder
    /// stream. Given up then, the decoder has told the encoder already.
    qpack_waiting: bool,
    /// The whole response was read: a streamed request body still going
    /// out may finish (RFC 9114, section 4.1.2).
    complete: bool,
    /// The task sending the rest of a streamed request body, told when the
    /// stream goes before the response is complete.
    abandon: Option<Rc<Abandon>>,
}

impl Stream {
    /// What a failure under the stream means for the request.
    fn failed(&mut self, e: io::Error) -> StreamError {
        self.done = true;
        let state = &self.conn.shared.state;
        if giong::error::code(&e) == Some(giong::error::Code::QuicEarlyDataRejected) {
            return StreamError::Refused(
                "the server refused the 0-RTT data the request went out in".into(),
            );
        }
        match giong_quic::reset_code(&e) {
            Some(code) if code == Code::RequestRejected as u64 => StreamError::Refused(
                "the server rejected the request unprocessed (H3_REQUEST_REJECTED)".into(),
            ),
            Some(code) => StreamError::Reset(format!(
                "the server reset the stream with error code {code:#x}"
            )),
            None => {
                state.lost(&e);
                let message = state
                    .closed
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| e.to_string());
                // The server names the streams it did not handle before
                // it goes (section 5.2).
                if state.goaway.get().is_some_and(|first| self.id >= first) {
                    StreamError::Refused(message)
                } else {
                    StreamError::Closed {
                        message,
                        received: self.received_any,
                    }
                }
            }
        }
    }

    /// The server broke the protocol on this stream in a way that ends
    /// the connection.
    fn broke(&mut self, error: frame::Error) -> StreamError {
        self.done = true;
        let shared = &self.conn.shared;
        shared.state.fail(&shared.quic, error);
        StreamError::Protocol(error.what.to_owned())
    }

    fn stop(&mut self, fail: Fail) -> StreamError {
        match fail {
            Fail::Io(e) => self.failed(e),
            Fail::Conn(error) => self.broke(error),
        }
    }

    /// A malformed response (section 4.1.2): the stream alone is given up.
    fn malformed(&mut self, what: &str) -> StreamError {
        self.cancel_as(Code::MessageError);
        StreamError::Protocol(what.to_owned())
    }

    /// Reads and decodes the field section of a HEADERS frame.
    async fn fields(&mut self, len: u64) -> Result<Decoded, StreamError> {
        let max = self.conn.shared.max_head_size;
        // What the client said it accepts counts decoded sizes; an encoded
        // section is never larger than that by more than a little.
        if len > (max as u64).saturating_mul(2).saturating_add(1024) {
            // Given up undecoded: `cancel` tells the server's encoder.
            self.cancel_as(Code::ExcessiveLoad);
            return Err(too_large(max));
        }
        let block = match self.r.exact(len as usize).await {
            Ok(block) => block,
            Err(fail) => return Err(self.stop(fail)),
        };
        let mut d = Decoded::default();
        let field = |name: &[u8], value: &[u8]| check_field(&mut d, max, name, value);
        let state = self.conn.shared.state.clone();
        let decoded = match &state.qpack {
            Some(decoder) => {
                // Dropped while it waits, the decode cancels the stream
                // for the encoder itself.
                self.qpack_waiting = true;
                let decoded = decoder.decode(self.id, &block, field).await;
                self.qpack_waiting = false;
                decoded
            }
            None => qpack::decode(&block, field),
        };
        if let Err(error) = decoded {
            return Err(self.broke(error));
        }
        if d.size > max {
            self.cancel_as(Code::ExcessiveLoad);
            return Err(too_large(max));
        }
        if let Some(what) = d.malformed {
            return Err(self.malformed(what));
        }
        Ok(d)
    }

    /// Waits for the head of the final response (1xx responses are
    /// skipped).
    pub(crate) async fn response(&mut self) -> Result<(), StreamError> {
        let mut informational = 0;
        loop {
            let header = match self.r.header(On::Response).await {
                Ok(Some(header)) => header,
                Ok(None) => {
                    self.done = true;
                    return Err(StreamError::Closed {
                        message: "the server ended the stream without a response".into(),
                        received: self.received_any,
                    });
                }
                Err(fail) => return Err(self.stop(fail)),
            };
            self.received_any = true;
            match header.kind {
                kind::HEADERS => {
                    let d = self.fields(header.len).await?;
                    let Some(status) = d.status else {
                        return Err(self.malformed("a response without :status"));
                    };
                    if status == 101 {
                        return Err(
                            self.malformed("101 (Switching Protocols), which HTTP/3 forbids")
                        );
                    }
                    if status < 200 {
                        // As over HTTP/1.1 and HTTP/2: a server that only
                        // ever sends 1xx answers is told apart from a slow
                        // one, instead of holding the request to its timeout.
                        informational += 1;
                        if informational > MAX_INFORMATIONAL {
                            return Err(self.malformed("more than 32 1xx responses"));
                        }
                        continue;
                    }
                    self.head = Some(ResponseHead {
                        status,
                        bytes: d.bytes,
                        fields: d.fields,
                        content_length: d.content_length,
                    });
                    return Ok(());
                }
                kind::DATA => {
                    return Err(self.broke(frame::Error::new(
                        Code::FrameUnexpected,
                        "DATA before the response head",
                    )));
                }
                _ => {
                    if let Err(fail) = self.r.skip(header.len).await {
                        return Err(self.stop(fail));
                    }
                }
            }
        }
    }

    /// The head of the response, once [`response`](Self::response) has
    /// returned it.
    pub(crate) fn head(&self) -> &ResponseHead {
        self.head
            .as_ref()
            .expect("the head is read before anything asks for it")
    }

    /// The head's field positions, taken once for the response the caller
    /// builds: nothing here reads them after.
    pub(crate) fn take_fields(&mut self) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
        self.head
            .as_mut()
            .map(|head| std::mem::take(&mut head.fields))
            .unwrap_or_default()
    }

    /// The response has no body whatever its fields say.
    fn bodiless(&self) -> bool {
        self.head_request || matches!(self.head().status, 204 | 304)
    }

    /// The length of the body, when known: none for a `HEAD` request or
    /// a `204` or `304` answer, otherwise what `content-length` says.
    /// The trailer fields the response ended with: there once the body
    /// has been read to its end, if the server sent any.
    pub(crate) fn trailers(&self) -> Option<&Trailers> {
        self.trailer_fields.as_deref()
    }

    pub(crate) fn body_length(&self) -> Option<u64> {
        if self.bodiless() {
            Some(0)
        } else {
            self.head().content_length
        }
    }

    /// Appends what has arrived of the body to `out`, waiting for some if
    /// nothing has, about `limit` bytes at most. Returns `false`, with
    /// nothing appended, at the end.
    pub(crate) async fn read(
        &mut self,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<bool, StreamError> {
        loop {
            if self.done {
                return Ok(false);
            }
            if self.left > 0 {
                if self.r.rest().is_empty() {
                    match self.r.fill().await {
                        Ok(true) => {}
                        Ok(false) => return Err(self.broke(CUT)),
                        Err(fail) => return Err(self.stop(fail)),
                    }
                    continue;
                }
                let n = (self.r.rest().len() as u64)
                    .min(self.left)
                    .min(limit.max(1) as u64) as usize;
                out.extend_from_slice(&self.r.rest()[..n]);
                self.r.consume(n);
                self.left -= n as u64;
                self.received += n as u64;
                let length = self.head().content_length;
                if self.bodiless() {
                    return Err(self.malformed("a body in a response that cannot have one"));
                }
                if length.is_some_and(|cl| self.received > cl) {
                    return Err(self.malformed("a body longer than its content-length"));
                }
                return Ok(true);
            }
            let header = match self.r.header(On::Response).await {
                Ok(Some(header)) => header,
                Ok(None) => {
                    let length = self.head().content_length;
                    if !self.bodiless() && length.is_some_and(|cl| cl != self.received) {
                        return Err(self.malformed("a body shorter than its content-length"));
                    }
                    self.done = true;
                    self.complete = true;
                    return Ok(false);
                }
                Err(fail) => return Err(self.stop(fail)),
            };
            match header.kind {
                kind::DATA | kind::HEADERS if self.trailers => {
                    return Err(self.broke(frame::Error::new(
                        Code::FrameUnexpected,
                        "a frame after the trailers",
                    )));
                }
                kind::DATA => self.left = header.len,
                kind::HEADERS => {
                    let d = self.fields(header.len).await?;
                    if d.status.is_some() {
                        return Err(self.malformed("trailers with a :status"));
                    }
                    self.trailers = true;
                    let mut trailers = Trailers::new();
                    for (name, value) in &d.fields {
                        // Checked when the block was decoded.
                        trailers.received(&d.bytes[name.clone()], &d.bytes[value.clone()]);
                    }
                    if !trailers.is_empty() {
                        self.trailer_fields = Some(Box::new(trailers));
                    }
                }
                _ => {
                    if let Err(fail) = self.r.skip(header.len).await {
                        return Err(self.stop(fail));
                    }
                }
            }
        }
    }

    /// Whether the body has been read to its end (or failed).
    /// Whether the request may have gone out in 0-RTT data.
    pub(crate) fn was_early(&self) -> bool {
        self.early
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Stops reading: the server is told the response is not wanted.
    pub(crate) fn cancel(&mut self) {
        self.cancel_as(Code::RequestCancelled);
    }

    /// Stops reading with `code`, unless the stream is over. A field
    /// section of the server's may be on its way, or read and not decoded:
    /// with a dynamic table, its encoder is told not to wait for an
    /// acknowledgment (RFC 9204, section 4.4.2).
    fn cancel_as(&mut self, code: Code) {
        if !std::mem::replace(&mut self.done, true) {
            self.r.recv.stop(code as u32);
            match &self.conn.shared.state.qpack {
                Some(decoder) if !self.qpack_waiting => decoder.cancel(self.id),
                _ => {}
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.complete {
            if let Some(abandon) = &self.abandon {
                abandon.now();
            }
        }
        self.cancel();
    }
}

fn too_large(max: usize) -> StreamError {
    StreamError::TooLarge(format!(
        "the response head is larger than {max} bytes (ClientBuilder::max_head_size)"
    ))
}

/// Takes the streams the server opens, for as long as the connection
/// lives.
async fn server_streams(quic: Rc<giong_quic::Connection>, state: Rc<State>) {
    loop {
        let recv = match quic.accept_uni().await {
            Ok(recv) => recv,
            Err(e) => {
                state.lost(&e);
                return;
            }
        };
        let (quic, state) = (quic.clone(), state.clone());
        drop(giong::spawn(async move {
            let mut r = Reader::new(recv);
            match server_stream(&state, &mut r).await {
                Ok(()) => {}
                Err(Fail::Conn(error)) => state.fail(&quic, error),
                // A stream that is not critical may be reset.
                Err(Fail::Io(e)) if giong_quic::reset_code(&e).is_some() => {}
                Err(Fail::Io(e)) => state.lost(&e),
            }
        }));
    }
}

/// The server's critical stream `bit` exists once.
fn critical(state: &State, bit: u8) -> Result<(), Fail> {
    if state.critical.get() & bit != 0 {
        return Err(frame::Error::new(
            Code::StreamCreationError,
            "a second control or QPACK stream",
        )
        .into());
    }
    state.critical.set(state.critical.get() | bit);
    Ok(())
}

const CLOSED_CRITICAL: frame::Error =
    frame::Error::new(Code::ClosedCriticalStream, "a critical stream was closed");

/// Reads one unidirectional stream of the server's to its end.
async fn server_stream(state: &State, r: &mut Reader) -> Result<(), Fail> {
    let kind = loop {
        if let Some((kind, used)) = frame::read_varint(r.rest()) {
            r.consume(used);
            break kind;
        }
        if !r.fill().await? {
            // Ended before it said what it was: nothing to hold against it.
            return Ok(());
        }
    };
    match StreamType::from_id(kind) {
        StreamType::Control => {
            critical(state, 1)?;
            kept_open(control_stream(state, r).await)
        }
        StreamType::QpackEncoder => {
            critical(state, 2)?;
            kept_open(match &state.qpack {
                Some(decoder) => qpack_stream(r, |buf| decoder.encoder_stream(buf)).await,
                None => qpack_stream(r, qpack::encoder_stream).await,
            })
        }
        StreamType::QpackDecoder => {
            critical(state, 4)?;
            kept_open(qpack_stream(r, qpack::decoder_stream).await)
        }
        // The client allows no push (section 4.6).
        StreamType::Push => Err(frame::Error::new(
            Code::IdError,
            "a push stream although no push was allowed",
        )
        .into()),
        StreamType::Unknown(_) => {
            r.recv.stop(Code::StreamCreationError as u32);
            Ok(())
        }
    }
}

/// A critical stream the server reset is one it closed (section 6.2.1).
fn kept_open(read: Result<(), Fail>) -> Result<(), Fail> {
    match read {
        Err(Fail::Io(e)) if giong_quic::reset_code(&e).is_some() => Err(CLOSED_CRITICAL.into()),
        other => other,
    }
}

/// A QPACK stream of the server's: checked to carry nothing the client did
/// not allow, for as long as the connection lives.
async fn qpack_stream(
    r: &mut Reader,
    mut check: impl FnMut(&[u8]) -> Result<usize, frame::Error>,
) -> Result<(), Fail> {
    loop {
        let used = check(r.rest())?;
        r.consume(used);
        if !r.fill().await? {
            return Err(CLOSED_CRITICAL.into());
        }
    }
}

/// The server's control stream: SETTINGS first, then GOAWAY when it wants
/// no more requests.
async fn control_stream(state: &State, r: &mut Reader) -> Result<(), Fail> {
    let mut first = true;
    let mut unknown = frame::Budget::new(MAX_UNKNOWN_FRAMES);
    loop {
        let Some(header) = r.header(On::Control).await? else {
            return Err(CLOSED_CRITICAL.into());
        };
        if first && header.kind != kind::SETTINGS {
            return Err(frame::Error::new(
                Code::MissingSettings,
                "the control stream does not start with SETTINGS",
            )
            .into());
        }
        match header.kind {
            kind::SETTINGS if !first => {
                return Err(
                    frame::Error::new(Code::FrameUnexpected, "a second SETTINGS frame").into(),
                );
            }
            kind::SETTINGS => {
                if header.len > MAX_SETTINGS_FRAME {
                    return Err(frame::Error::new(
                        Code::ExcessiveLoad,
                        "a SETTINGS frame too large",
                    )
                    .into());
                }
                // Nothing in them changes what this client sends: its
                // encoder uses no dynamic table, and its field sections
                // are what the program gave it. They are checked all the
                // same.
                let payload = r.exact(header.len as usize).await?;
                Settings::parse(&payload)?;
            }
            kind::GOAWAY => {
                let id = single_id(r, header.len).await?;
                // A client-initiated bidirectional stream (section 5.2).
                if id % 4 != 0 {
                    return Err(frame::Error::new(
                        Code::IdError,
                        "GOAWAY names what is not a request stream",
                    )
                    .into());
                }
                if state.goaway.get().is_some_and(|last| id > last) {
                    return Err(frame::Error::new(
                        Code::IdError,
                        "the identifier in GOAWAY went up",
                    )
                    .into());
                }
                state.goaway.set(Some(id));
            }
            // Only a client sends MAX_PUSH_ID (section 7.2.7).
            kind::MAX_PUSH_ID => {
                return Err(
                    frame::Error::new(Code::FrameUnexpected, "MAX_PUSH_ID from a server").into(),
                );
            }
            // No push was allowed, so there is none to cancel.
            kind::CANCEL_PUSH => {
                single_id(r, header.len).await?;
                return Err(frame::Error::new(
                    Code::IdError,
                    "CANCEL_PUSH for a push that was not allowed",
                )
                .into());
            }
            _ => {
                if !unknown.take() {
                    return Err(frame::Error::new(
                        Code::ExcessiveLoad,
                        "too many unknown frames in a second",
                    )
                    .into());
                }
                r.skip(header.len).await?;
            }
        }
        first = false;
    }
}

/// The integer that GOAWAY and CANCEL_PUSH hold.
async fn single_id(r: &mut Reader, len: u64) -> Result<u64, Fail> {
    if len > MAX_SETTINGS_FRAME {
        return Err(frame::Error::new(Code::ExcessiveLoad, "a control frame too large").into());
    }
    if len > 8 {
        return Err(frame::Error::new(
            Code::FrameError,
            "a frame that holds one integer has another length",
        )
        .into());
    }
    let payload = r.exact(len as usize).await?;
    Ok(frame::parse_single_id(&payload)?)
}
