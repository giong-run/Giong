//! The server side of an HTTP/3 connection (RFC 9114) over giong-quic.
//!
//! QUIC gives every request a stream of its own, with its own flow control,
//! so a connection here is a set of small tasks and no shared state machine:
//! one task per request stream, one per unidirectional stream the client
//! opens (its control stream and its two QPACK streams), and the task that
//! accepts them. Handlers see the same [`Request`] and return the same
//! [`Response`] as over HTTP/1.1 and HTTP/2, and the limits of the server
//! (`max_head_size`, `max_body_size`, the timeouts, the worker's budget for
//! buffered requests) apply the same way.
//!
//! The QPACK dynamic table is not used in either direction: the server
//! announces a capacity of zero and encodes with static references and
//! literals, which RFC 9204 allows in full.

use std::cell::{Cell, RefCell};
use std::future::{poll_fn, Future};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use giong::time::timeout;
use giong_quic::{Connection, RecvStream, SendStream};

use super::frame::{
    self, kind, read_varint, write_varint, Code, Error, Header, Need, On, Settings, StreamType,
};
use super::qpack::{self, Encoder};
use crate::body::{Failure, Next, Pipe};
use crate::budget::{self, Charge};
use crate::h2::hpack::Index;
use crate::h2::request::{Builder, Built, TrailerBlock};
use crate::request::BodyBytes;
use crate::response::{Body, Response};
use crate::server::{catching, drop_quietly, panicked, Config, Handler};
use crate::trailers::Trailers;

/// The largest SETTINGS frame read. The three settings HTTP/3 defines take
/// under 30 bytes.
const MAX_SETTINGS_FRAME: u64 = 4 * 1024;
/// Frames of unknown types (RFC 9114, section 9: to be ignored) and empty
/// DATA frames tolerated on one stream. A client that sends more only
/// keeps the server busy.
const MAX_IDLE_FRAMES: u32 = 1024;
/// Bytes handed to QUIC in one write, each under the write timeout.
const WRITE_PIECE: usize = 16 * 1024;
/// Bytes read from a file at a time.
const FILE_PIECE: u64 = 64 * 1024;
/// A DATA frame up to this size is written together with its header.
const SMALL_BODY: usize = 1024;

/// Why work on a stream stops early.
enum Stop {
    /// A connection error: the connection is closed with its code.
    Conn(Error),
    /// A stream error: the stream is reset with this code, and the
    /// connection goes on.
    Stream(Code),
    /// The peer reset the stream or the connection is gone: nothing to say.
    Gone,
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Stop::Conn(e)
    }
}

fn fatal(code: Code, what: &'static str) -> Stop {
    Stop::Conn(Error::new(code, what))
}

/// What the tasks of one connection share.
struct Conn {
    quic: Connection,
    cfg: Config,
    /// A connection error was sent, or the connection was closed.
    closed: Cell<bool>,
    /// Which of the client's critical streams exist: control (1), QPACK
    /// encoder (2), QPACK decoder (4).
    critical: Cell<u8>,
    /// The largest MAX_PUSH_ID the client sent.
    max_push_id: Cell<Option<u64>>,
    /// The identifier in the client's last GOAWAY.
    goaway: Cell<Option<u64>>,
    /// Request streams being served, and who waits for there to be none.
    active: Cell<usize>,
    idle: Cell<Option<Waker>>,
    /// Response bytes the streams hold that QUIC has not taken yet, and how
    /// many they may hold before handlers wait: the rule of HTTP/2 (see
    /// `h2::budget`). Every handler of a connection runs at once, each in
    /// its stream's task, and a client that gives no flow-control credit
    /// would otherwise make the connection hold every answer it asked for
    /// until the write timeout.
    unsent: Cell<u64>,
    unsent_budget: u64,
    /// The tasks of handlers waiting for `unsent` to fall below the budget.
    parked: RefCell<Vec<Waker>>,
}

impl Conn {
    fn fail(&self, e: Error) {
        if !self.closed.replace(true) {
            self.quic.close(e.code as u32, e.what.as_bytes());
        }
    }

    /// Handlers wait: the connection holds as many response bytes as it
    /// may, and the client has not taken them yet. A waiting handler is
    /// not polled (so not started either) until the bytes are taken.
    fn handlers_wait(&self, cx: &Context<'_>) -> bool {
        if self.unsent.get() < self.unsent_budget {
            return false;
        }
        let mut parked = self.parked.borrow_mut();
        if !parked.iter().any(|w| w.will_wake(cx.waker())) {
            parked.push(cx.waker().clone());
        }
        true
    }
}

/// Response bytes a stream holds, counted against its connection's budget
/// until they are handed to QUIC or the stream ends.
struct Held<'a> {
    conn: &'a Conn,
    bytes: u64,
}

impl<'a> Held<'a> {
    fn new(conn: &'a Conn) -> Self {
        Self { conn, bytes: 0 }
    }

    fn add(&mut self, n: usize) {
        self.bytes += n as u64;
        self.conn.unsent.set(self.conn.unsent.get() + n as u64);
    }

    fn release(&mut self, n: usize) {
        let n = (n as u64).min(self.bytes);
        self.bytes -= n;
        let conn = self.conn;
        conn.unsent.set(conn.unsent.get() - n);
        if conn.unsent.get() < conn.unsent_budget {
            for waker in conn.parked.take() {
                waker.wake();
            }
        }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.release(usize::try_from(self.bytes).unwrap_or(usize::MAX));
    }
}

/// One request stream being served.
struct Active(Rc<Conn>);

impl Drop for Active {
    fn drop(&mut self) {
        let conn = &self.0;
        conn.active.set(conn.active.get() - 1);
        if let Some(waker) = conn.idle.take() {
            waker.wake();
        }
    }
}

/// A stream being read: frames are cut out of what has arrived.
struct Reader {
    recv: RecvStream,
    /// What was read and not used yet starts at `at`.
    buf: Vec<u8>,
    at: usize,
    /// The stream's end was read.
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

    fn append(&mut self, bytes: &[u8]) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Reads more of the stream; `false` at its end.
    async fn fill(&mut self) -> Result<bool, Stop> {
        if self.ended {
            return Ok(false);
        }
        match self.recv.read().await {
            Ok(Some(bytes)) => {
                self.append(&bytes);
                Ok(true)
            }
            Ok(None) => {
                self.ended = true;
                Ok(false)
            }
            Err(_) => Err(Stop::Gone),
        }
    }

    /// Takes what has arrived already, without waiting: after it, `ended`
    /// says whether the stream is known to be over.
    async fn peek(&mut self) -> Result<(), Stop> {
        if self.ended {
            return Ok(());
        }
        let arrived = {
            // Cancel safe: bytes leave the stream only when `read` completes.
            let mut read = pin!(self.recv.read());
            poll_fn(|cx| match read.as_mut().poll(cx) {
                Poll::Ready(read) => Poll::Ready(Some(read)),
                Poll::Pending => Poll::Ready(None),
            })
            .await
        };
        match arrived {
            Some(Ok(Some(bytes))) => self.append(&bytes),
            Some(Ok(None)) => self.ended = true,
            Some(Err(_)) => return Err(Stop::Gone),
            None => {}
        }
        Ok(())
    }

    /// The next frame header; `None` if the stream ends where a frame
    /// would start.
    async fn header(&mut self, on: On) -> Result<Option<Header>, Stop> {
        loop {
            if let Need::Got(header) = Header::read(self.rest(), on)? {
                self.consume(header.size);
                return Ok(Some(header));
            }
            if !self.fill().await? {
                if self.rest().is_empty() {
                    return Ok(None);
                }
                return Err(cut());
            }
        }
    }

    /// The whole payload of a frame, `len` bytes.
    async fn exact(&mut self, len: usize) -> Result<Vec<u8>, Stop> {
        while self.rest().len() < len {
            if !self.fill().await? {
                return Err(cut());
            }
        }
        let payload = self.rest()[..len].to_vec();
        self.consume(len);
        Ok(payload)
    }

    /// Reads past `len` bytes of payload without keeping them.
    async fn skip(&mut self, mut len: u64) -> Result<(), Stop> {
        loop {
            let n = (self.rest().len() as u64).min(len) as usize;
            self.consume(n);
            len -= n as u64;
            if len == 0 {
                return Ok(());
            }
            if !self.fill().await? {
                return Err(cut());
            }
        }
    }

    /// Some of the `left` bytes that remain of a DATA frame: at least one.
    async fn data(&mut self, left: &mut u64) -> Result<Vec<u8>, Stop> {
        while self.rest().is_empty() {
            if !self.fill().await? {
                return Err(cut());
            }
        }
        let n = (self.rest().len() as u64).min(*left) as usize;
        let piece = self.rest()[..n].to_vec();
        self.consume(n);
        *left -= n as u64;
        Ok(piece)
    }
}

/// A stream that ends inside a frame (RFC 9114, section 7.1).
fn cut() -> Stop {
    fatal(Code::FrameError, "a stream ends inside a frame")
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Whichever of the two completes first; the other is dropped.
async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let (mut a, mut b) = (pin!(a), pin!(b));
    poll_fn(|cx| {
        if let Poll::Ready(value) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(value));
        }
        if let Poll::Ready(value) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(value));
        }
        Poll::Pending
    })
    .await
}

/// Serves one HTTP/3 connection until it ends.
pub(crate) async fn serve<H: Handler>(quic: Connection, cfg: Config, handler: Rc<H>) {
    let conn = Rc::new(Conn {
        quic,
        cfg,
        closed: Cell::new(false),
        critical: Cell::new(0),
        max_push_id: Cell::new(None),
        goaway: Cell::new(None),
        active: Cell::new(0),
        idle: Cell::new(None),
        unsent: Cell::new(0),
        unsent_budget: crate::h2::budget(&cfg),
        parked: RefCell::new(Vec::new()),
    });
    // The control stream, which stays open for as long as the connection
    // does: closing it is a connection error (section 6.2.1).
    // A client that gives no credit for it (no unidirectional stream, or
    // no bytes on one) would hold the connection without being served:
    // it must allow three (section 6.2), so it is closed after the write
    // timeout.
    let mut out = Vec::with_capacity(16);
    write_varint(&mut out, StreamType::CONTROL);
    Settings::write_frame(&mut out, cfg.max_head_size as u64);
    let opened = timeout(cfg.write_timeout, async {
        let mut control = conn.quic.open_uni().await?;
        control.write_all(&out).await?;
        std::io::Result::Ok(control)
    })
    .await;
    let mut control = match opened {
        Ok(Ok(control)) => control,
        Ok(Err(_)) => return,
        Err(_) => {
            conn.closed.set(true);
            conn.quic.close(
                Code::ExcessiveLoad as u32,
                b"no credit for the control stream",
            );
            return;
        }
    };
    drop(giong::spawn(peer_streams(conn.clone())));

    // The lowest request stream not accepted yet: what GOAWAY announces.
    let mut next_id = 0;
    loop {
        match giong::until_shutdown(conn.quic.accept_bi()).await {
            Some(Ok((send, recv))) => {
                next_id = send.id() + 4;
                conn.active.set(conn.active.get() + 1);
                let active = Active(conn.clone());
                let handler = handler.clone();
                drop(giong::spawn(request(active, send, recv, handler)));
            }
            // The connection is over.
            Some(Err(_)) => return,
            None => break,
        }
    }

    // Shutting down (section 5.2): the requests accepted are finished, the
    // client is told which those are, and later ones are refused in a way
    // that lets it retry them elsewhere.
    out.clear();
    frame::write_goaway(&mut out, next_id);
    // Bounded too, or a client that gives no credit would hold shutdown.
    match timeout(cfg.write_timeout, control.write_all(&out)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return,
        Err(_) => {
            if !conn.closed.replace(true) {
                conn.quic.close(Code::NoError as u32, b"");
            }
            return;
        }
    }
    loop {
        let idle = poll_fn(|cx| {
            if conn.active.get() == 0 {
                Poll::Ready(())
            } else {
                conn.idle.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        });
        match race(idle, conn.quic.accept_bi()).await {
            Either::Left(()) => break,
            Either::Right(Ok((mut send, mut recv))) => {
                send.reset(Code::RequestRejected as u32);
                recv.stop(Code::RequestRejected as u32);
            }
            Either::Right(Err(_)) => return,
        }
    }
    if !conn.closed.replace(true) {
        conn.quic.close(Code::NoError as u32, b"");
    }
}

/// Takes the unidirectional streams the client opens.
async fn peer_streams(conn: Rc<Conn>) {
    while let Ok(recv) = conn.quic.accept_uni().await {
        let conn = conn.clone();
        drop(giong::spawn(async move {
            let mut reader = Reader::new(recv);
            if let Err(Stop::Conn(e)) = peer_stream(&conn, &mut reader).await {
                conn.fail(e);
            }
        }));
    }
}

/// Reads one unidirectional stream of the client for as long as it lasts.
async fn peer_stream(conn: &Conn, r: &mut Reader) -> Result<(), Stop> {
    let kind = loop {
        if let Some((kind, used)) = read_varint(r.rest()) {
            r.consume(used);
            break kind;
        }
        if !r.fill().await? {
            // Closed before saying what it is: nothing was asked.
            return Ok(());
        }
    };
    let critical = |bit: u8| {
        let seen = conn.critical.get();
        if seen & bit != 0 {
            return Err(fatal(
                Code::StreamCreationError,
                "a second control or QPACK stream",
            ));
        }
        conn.critical.set(seen | bit);
        Ok(())
    };
    match StreamType::from_id(kind) {
        StreamType::Control => {
            critical(1)?;
            control_stream(conn, r).await
        }
        StreamType::QpackEncoder => {
            critical(2)?;
            qpack_stream(r, qpack::encoder_stream).await
        }
        StreamType::QpackDecoder => {
            critical(4)?;
            qpack_stream(r, qpack::decoder_stream).await
        }
        // Only a server pushes (section 6.2.2).
        StreamType::Push => Err(fatal(
            Code::StreamCreationError,
            "a push stream opened by a client",
        )),
        // Section 6.2.3: a type we do not know is not an error; we just do
        // not read it.
        StreamType::Unknown(_) => {
            r.recv.stop(Code::StreamCreationError as u32);
            Ok(())
        }
    }
}

/// The end of a control or QPACK stream, which must stay open (sections
/// 6.2.1 of RFC 9114 and 4.2 of RFC 9204).
fn closed_critical() -> Stop {
    fatal(
        Code::ClosedCriticalStream,
        "the control stream or a QPACK stream was closed",
    )
}

/// Reads a QPACK stream of a client that was told there is no dynamic
/// table: `check` accepts the few instructions that remain legal.
async fn qpack_stream(
    r: &mut Reader,
    check: fn(&[u8]) -> Result<usize, Error>,
) -> Result<(), Stop> {
    loop {
        let used = check(r.rest())?;
        r.consume(used);
        match r.fill().await {
            Ok(true) => {}
            _ => return Err(closed_critical()),
        }
    }
}

/// Reads the client's control stream: SETTINGS first, then what it has to
/// say for the rest of the connection.
async fn control_stream(conn: &Conn, r: &mut Reader) -> Result<(), Stop> {
    let mut first = true;
    let mut unknown = frame::Budget::new(4 * MAX_IDLE_FRAMES);
    loop {
        let header = match r.header(On::Control).await {
            Ok(Some(header)) => header,
            Ok(None) | Err(Stop::Gone) => return Err(closed_critical()),
            Err(stop) => return Err(stop),
        };
        if first && header.kind != kind::SETTINGS {
            return Err(fatal(
                Code::MissingSettings,
                "the control stream does not start with SETTINGS",
            ));
        }
        match header.kind {
            kind::SETTINGS if !first => {
                return Err(fatal(Code::FrameUnexpected, "a second SETTINGS frame"));
            }
            kind::SETTINGS => {
                if header.len > MAX_SETTINGS_FRAME {
                    return Err(fatal(Code::ExcessiveLoad, "a SETTINGS frame too large"));
                }
                let payload = critical_payload(r, header.len as usize).await?;
                // Nothing in them changes what this server sends: it has
                // no dynamic table to fill, and its field sections are
                // small. They are checked all the same.
                Settings::parse(&payload)?;
            }
            // The client is going away: it will open no more requests.
            // The identifier it names may only go down (section 5.2).
            kind::GOAWAY => {
                let id = single_id(r, header.len).await?;
                if conn.goaway.get().is_some_and(|last| id > last) {
                    return Err(fatal(Code::IdError, "the identifier in GOAWAY went up"));
                }
                conn.goaway.set(Some(id));
            }
            kind::MAX_PUSH_ID => {
                let id = single_id(r, header.len).await?;
                if conn.max_push_id.get().is_some_and(|max| id < max) {
                    return Err(fatal(Code::IdError, "MAX_PUSH_ID went down"));
                }
                conn.max_push_id.set(Some(id));
            }
            kind::CANCEL_PUSH => {
                // This server never pushes, so no push id was ever used:
                // one the client did not allow is an error, any other is
                // a push that will never come.
                let id = single_id(r, header.len).await?;
                if conn.max_push_id.get().map_or(true, |max| id > max) {
                    return Err(fatal(
                        Code::IdError,
                        "CANCEL_PUSH for a push id that was not allowed",
                    ));
                }
            }
            _ => {
                if !unknown.take() {
                    return Err(fatal(
                        Code::ExcessiveLoad,
                        "too many unknown frames in a second",
                    ));
                }
                match r.skip(header.len).await {
                    Err(Stop::Gone) => return Err(closed_critical()),
                    other => other?,
                }
            }
        }
        first = false;
    }
}

/// The integer that GOAWAY, MAX_PUSH_ID and CANCEL_PUSH hold.
async fn single_id(r: &mut Reader, len: u64) -> Result<u64, Stop> {
    // A length no control frame of this server's could have is refused for
    // what it asks the server to hold; one that is merely wrong for an
    // integer is a frame error.
    if len > MAX_SETTINGS_FRAME {
        return Err(fatal(Code::ExcessiveLoad, "a control frame too large"));
    }
    if len > 8 {
        return Err(fatal(
            Code::FrameError,
            "a frame that holds one integer has another length",
        ));
    }
    let payload = critical_payload(r, len as usize).await?;
    Ok(frame::parse_single_id(&payload)?)
}

/// The payload of a control frame; the stream going away under it is the
/// loss of a critical stream.
async fn critical_payload(r: &mut Reader, len: usize) -> Result<Vec<u8>, Stop> {
    match r.exact(len).await {
        Err(Stop::Gone) => Err(closed_critical()),
        other => other,
    }
}

/// Serves one request stream.
async fn request<H: Handler>(
    active: Active,
    mut send: SendStream,
    recv: RecvStream,
    handler: Rc<H>,
) {
    let conn = &*active.0;
    let mut r = Reader::new(recv);
    match exchange(conn, &mut send, &mut r, &*handler).await {
        // Answered. What the client still sends of a request that was
        // answered early is not wanted (section 4.1.2).
        Ok(()) => r.recv.stop(Code::NoError as u32),
        Err(Stop::Conn(e)) => conn.fail(e),
        Err(Stop::Stream(code)) => {
            send.reset(code as u32);
            r.recv.stop(code as u32);
        }
        // Nobody may be left to hear it, but if the client still reads,
        // it must not take what was written for a whole response:
        // dropping the stream would end it cleanly.
        Err(Stop::Gone) => send.reset(Code::RequestCancelled as u32),
    }
}

/// Where the body of a request has got to.
#[derive(Default)]
struct BodyState {
    /// Bytes left of the DATA frame being read.
    left: u64,
    /// Body bytes so far.
    received: u64,
    /// The trailers were read: nothing but unknown frames may follow.
    trailers: bool,
    /// The trailer fields are kept, up to this many bytes (a body the
    /// handler streams); `None`: checked, not kept.
    keep: Option<usize>,
    /// The trailer fields kept.
    kept: Option<Trailers>,
    /// Unknown frames and empty DATA frames so far.
    idle: u32,
}

/// The next piece of a request body; `None` at the end of the request.
async fn body_piece(
    r: &mut Reader,
    st: &mut BodyState,
    max_head: usize,
) -> Result<Option<Vec<u8>>, Stop> {
    loop {
        if st.left > 0 {
            let piece = r.data(&mut st.left).await?;
            st.received += piece.len() as u64;
            return Ok(Some(piece));
        }
        let Some(header) = r.header(On::Request).await? else {
            return Ok(None);
        };
        match header.kind {
            kind::DATA | kind::HEADERS if st.trailers => {
                return Err(fatal(
                    Code::FrameUnexpected,
                    "DATA or HEADERS after the trailers",
                ));
            }
            kind::DATA if header.len > 0 => st.left = header.len,
            kind::HEADERS => {
                st.trailers = true;
                if header.len > max_head as u64 {
                    return Err(Stop::Stream(Code::ExcessiveLoad));
                }
                let block = r.exact(header.len as usize).await?;
                let mut trailers = TrailerBlock::new(st.keep);
                qpack::decode(&block, |name, value| trailers.field(name, value))?;
                if trailers.error.is_some() {
                    return Err(Stop::Stream(Code::MessageError));
                }
                if st.keep.is_some() {
                    st.kept = Some(trailers.fields);
                }
            }
            _ => {
                st.idle += 1;
                if st.idle > MAX_IDLE_FRAMES {
                    return Err(fatal(
                        Code::ExcessiveLoad,
                        "too many empty or unknown frames on a request stream",
                    ));
                }
                r.skip(header.len).await?;
            }
        }
    }
}

/// Reads the HEADERS frame a request starts with and makes the request of
/// it.
async fn read_head(conn: &Conn, r: &mut Reader) -> Result<Built, Stop> {
    let max_head = conn.cfg.max_head_size;
    let mut unknown = 0;
    let header = loop {
        let Some(header) = r.header(On::Request).await? else {
            return Err(Stop::Stream(Code::RequestIncomplete));
        };
        match header.kind {
            kind::HEADERS => break header,
            kind::DATA => {
                return Err(fatal(Code::FrameUnexpected, "DATA before HEADERS"));
            }
            _ => {
                unknown += 1;
                if unknown > MAX_IDLE_FRAMES {
                    return Err(fatal(
                        Code::ExcessiveLoad,
                        "too many unknown frames before a request",
                    ));
                }
                r.skip(header.len).await?;
            }
        }
    };
    // An encoded section this large cannot decode to a list under the
    // limit that was announced. Answered at once: the rest of the stream,
    // which a client may announce as a gigabyte, is not read (the stream
    // is stopped once the answer is out).
    if header.len > max_head as u64 {
        return Ok(Built::Status(431));
    }
    let block = r.exact(header.len as usize).await?;
    let mut builder = Builder::with_scratch(max_head, Vec::new()).http3();
    qpack::decode(&block, |name, value| builder.field(name, value))?;
    // Read for each request: a client that moves (a NAT rebinding, or a
    // new network) is seen where it is now. For a request that came in
    // 0-RTT data the address is not validated yet and may be forged.
    Ok(builder.finish(Some(conn.quic.remote_addr())))
}

/// A handler's future, on the heap. A panic of the handler becomes the
/// `500` answer, and a panic of the future's destructor is swallowed, so
/// that neither takes the connection's other requests with it.
struct Call<F>(Option<Pin<Box<F>>>);

impl<F: Future<Output = Response>> Call<F> {
    fn new(future: F) -> Self {
        Self(Some(Box::pin(future)))
    }

    /// Polls the handler, unless handlers of `conn` wait (see
    /// [`Conn::handlers_wait`]): its answer is counted against the
    /// connection's budget (by `respond`) before another handler is polled.
    fn poll(&mut self, conn: &Conn, cx: &mut Context<'_>) -> Poll<Response> {
        if conn.handlers_wait(cx) {
            return Poll::Pending;
        }
        let Some(future) = self.0.as_mut() else {
            return Poll::Ready(panicked());
        };
        match catching(|| future.as_mut().poll(cx)) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(response)) => {
                self.0 = None;
                Poll::Ready(response)
            }
            Err(()) => {
                if let Some(future) = self.0.take() {
                    drop_quietly(future);
                }
                Poll::Ready(panicked())
            }
        }
    }
}

impl<F> Drop for Call<F> {
    fn drop(&mut self) {
        if let Some(future) = self.0.take() {
            drop_quietly(future);
        }
    }
}

/// One request and its response.
async fn exchange<H: Handler>(
    conn: &Conn,
    send: &mut SendStream,
    r: &mut Reader,
    handler: &H,
) -> Result<(), Stop> {
    let cfg = &conn.cfg;
    let built = match timeout(cfg.request_timeout, read_head(conn, r)).await {
        Ok(built) => built?,
        Err(_) => return respond(conn, send, Response::new(408), false).await,
    };
    let mut req = match built {
        // Early data can be replayed (RFC 8470): only the methods meant
        // to be safe get through, marked as early.
        Built::Request(mut req) if r.recv.is_early() => {
            if !matches!(req.method(), "GET" | "HEAD" | "OPTIONS") {
                return respond(conn, send, Response::new(425), false).await;
            }
            req.early_data = true;
            req
        }
        Built::Request(req) => req,
        Built::Status(status) => {
            return respond(conn, send, Response::new(status), false).await;
        }
        // Section 4.1.2: a malformed request is a stream error.
        Built::Malformed(_) => return Err(Stop::Stream(Code::MessageError)),
    };
    let head_only = req.method() == "HEAD";
    let content_length = req.head.content_length;
    let expect_continue = req.head.expect_continue;

    // A request without a body ends with its HEADERS frame, and the end
    // usually arrives with it.
    if r.rest().is_empty() {
        r.peek().await?;
    }
    if r.ended && r.rest().is_empty() {
        if content_length.is_some_and(|cl| cl != 0) {
            return Err(Stop::Stream(Code::MessageError));
        }
        let mut call = Call::new(handler.call(req));
        let response = poll_fn(|cx| call.poll(conn, cx)).await;
        return respond(conn, send, response, head_only).await;
    }

    let Ok(streamed) = catching(|| handler.stream_body(&req)) else {
        return respond(conn, send, panicked(), head_only).await;
    };
    let max_head = cfg.max_head_size;
    let Some(limit) = streamed else {
        // The whole body first, then the handler.
        let max_body = cfg.max_body_size as u64;
        let buffered_limit = cfg.buffered_limit();
        if content_length.is_some_and(|cl| cl > max_body || cl > buffered_limit) {
            return respond(conn, send, Response::new(413), false).await;
        }
        // Refused before its body is read if the worker's budget for
        // requests cannot take it, so that the client can retry.
        if !budget::fits(content_length.unwrap_or(1), buffered_limit) {
            budget::note_refused(buffered_limit);
            return Err(Stop::Stream(Code::RequestRejected));
        }
        if expect_continue {
            interim(conn, send, 100).await?;
        }
        let mut charge = Charge::none();
        let mut st = BodyState::default();
        let read = async {
            while let Some(piece) = body_piece(r, &mut st, max_head).await? {
                if content_length.is_some_and(|cl| st.received > cl) {
                    return Err(Stop::Stream(Code::MessageError));
                }
                if st.received > max_body {
                    return Ok(Some(413));
                }
                if !charge.grow_to(st.received, buffered_limit) {
                    budget::note_refused(buffered_limit);
                    return Err(Stop::Stream(Code::RequestRejected));
                }
                if let Some(buf) = Rc::get_mut(&mut req.buf) {
                    buf.extend_from_slice(&piece);
                }
            }
            if content_length.is_some_and(|cl| cl != st.received) {
                return Err(Stop::Stream(Code::MessageError));
            }
            Ok(None)
        };
        let refused = match timeout(cfg.request_timeout, read).await {
            Ok(read) => read?,
            Err(_) => Some(408),
        };
        if let Some(status) = refused {
            return respond(conn, send, Response::new(status), false).await;
        }
        let start = req.head.len;
        req.body = BodyBytes::InBuffer(start..req.buf.len());
        req.charge = charge;
        let mut call = Call::new(handler.call(req));
        let response = poll_fn(|cx| call.poll(conn, cx)).await;
        return respond(conn, send, response, head_only).await;
    };

    // The handler reads the body as it arrives.
    if content_length.is_some_and(|cl| cl > limit) {
        return respond(conn, send, Response::new(413), false).await;
    }
    let pipe = Rc::new(Pipe::new(content_length));
    req.body = BodyBytes::Stream(pipe.clone());
    let request_timeout = cfg.request_timeout;
    let pump = async {
        let mut st = BodyState {
            keep: Some(max_head),
            ..BodyState::default()
        };
        loop {
            let piece = match timeout(request_timeout, body_piece(r, &mut st, max_head)).await {
                Ok(Ok(Some(piece))) => piece,
                Ok(Ok(None)) => break,
                Ok(Err(stop)) => {
                    pipe.finish(Err(match stop {
                        Stop::Gone => Failure::Reset,
                        _ => Failure::Malformed,
                    }));
                    return Err(stop);
                }
                Err(_) => {
                    pipe.finish(Err(Failure::Cut));
                    return Err(Stop::Stream(Code::RequestCancelled));
                }
            };
            if content_length.is_some_and(|cl| st.received > cl) {
                pipe.finish(Err(Failure::Malformed));
                return Err(Stop::Stream(Code::MessageError));
            }
            if st.received > limit {
                // The handler learns of it and answers.
                pipe.finish(Err(Failure::TooLarge(limit)));
                return Ok(());
            }
            // Waits until the handler has taken the piece before: a slow
            // handler slows the client down through QUIC's flow control.
            pipe.put(piece).await;
        }
        if content_length.is_some_and(|cl| cl != st.received) {
            pipe.finish(Err(Failure::Malformed));
            return Err(Stop::Stream(Code::MessageError));
        }
        pipe.finish_with(Ok(()), st.kept.take());
        Ok(())
    };
    let answer = async {
        let mut call = Call::new(handler.call(req));
        let mut continued = !expect_continue;
        let response = loop {
            // `100 Continue` goes out when the handler first asks for the
            // body, as over HTTP/1.1.
            let step = poll_fn(|cx| {
                if let Poll::Ready(response) = call.poll(conn, cx) {
                    return Poll::Ready(Some(response));
                }
                if !continued && pipe.poll_asked(cx.waker()) {
                    return Poll::Ready(None);
                }
                Poll::Pending
            })
            .await;
            match step {
                Some(response) => break response,
                None => {
                    continued = true;
                    interim(conn, send, 100).await?;
                }
            }
        };
        respond(conn, send, response, head_only).await
    };
    let (mut pump, mut answer) = (pin!(pump), pin!(answer));
    let mut pumping = true;
    let result = poll_fn(|cx| {
        if pumping {
            if let Poll::Ready(end) = pump.as_mut().poll(cx) {
                pumping = false;
                if let Err(stop) = end {
                    return Poll::Ready(Err(stop));
                }
            }
        }
        answer.as_mut().poll(cx)
    })
    .await;
    // The response is out (or the stream is lost): what is left of the
    // body is not read. The first end of a pipe is the one that counts.
    pipe.finish(Err(Failure::Abandoned));
    result
}

/// Hands `data` to QUIC, each piece of it under the write timeout: a
/// client that takes nothing for that long loses the stream.
async fn write(conn: &Conn, send: &mut SendStream, data: &[u8]) -> Result<(), Stop> {
    write_held(conn, send, data, None).await
}

/// [`write`], for bytes counted in `held`: each piece no longer counts
/// once QUIC has it.
async fn write_held(
    conn: &Conn,
    send: &mut SendStream,
    data: &[u8],
    mut held: Option<&mut Held<'_>>,
) -> Result<(), Stop> {
    for piece in data.chunks(WRITE_PIECE) {
        match timeout(conn.cfg.write_timeout, send.write_all(piece)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Stop::Gone),
            Err(_) => return Err(Stop::Stream(Code::RequestCancelled)),
        }
        if let Some(held) = held.as_deref_mut() {
            held.release(piece.len());
        }
    }
    Ok(())
}

/// Writes an interim response: a HEADERS frame with the status alone.
async fn interim(conn: &Conn, send: &mut SendStream, status: u16) -> Result<(), Stop> {
    let mut block = Vec::with_capacity(8);
    Encoder::begin(&mut block);
    Encoder::status(status, &mut block);
    let mut out = Vec::with_capacity(block.len() + 4);
    Header::write(&mut out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
    write(conn, send, &out).await
}

/// The decimal digits of `n`, written at the end of `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[at..];
        }
    }
}

/// Appends the HEADERS frame of `resp` to `out`.
fn write_head(out: &mut Vec<u8>, resp: &Response, content_length: Option<u64>) {
    let mut block = Vec::with_capacity(256);
    Encoder::begin(&mut block);
    Encoder::status(resp.status(), &mut block);
    Encoder::field(b"date", &crate::date::now(), Index::Yes, &mut block);
    if let Some(length) = content_length {
        let mut digits = [0; 20];
        Encoder::field(
            b"content-length",
            decimal(length, &mut digits),
            Index::No,
            &mut block,
        );
    }
    let mut name = Vec::with_capacity(32);
    for (n, v) in resp.header_fields() {
        name.clear();
        name.extend(n.iter().map(u8::to_ascii_lowercase));
        // Fields about one connection have no meaning here (section 4.2).
        if matches!(
            &name[..],
            b"connection"
                | b"keep-alive"
                | b"proxy-connection"
                | b"transfer-encoding"
                | b"upgrade"
                | b"te"
        ) {
            continue;
        }
        let index = if name == b"set-cookie" {
            Index::Never
        } else {
            Index::Yes
        };
        Encoder::field(&name, v, index, &mut block);
    }
    Header::write(out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
}

/// Appends a HEADERS frame of the trailer fields to `out` (RFC 9114,
/// section 4.1).
fn write_trailers(out: &mut Vec<u8>, trailers: &Trailers) {
    let mut block = Vec::with_capacity(128);
    Encoder::begin(&mut block);
    for (name, value) in trailers.iter() {
        let index = if value.len() > 256 {
            Index::No
        } else {
            Index::Yes
        };
        Encoder::field(name.as_bytes(), value, index, &mut block);
    }
    Header::write(out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
}

/// Writes `resp` and ends the stream.
async fn respond(
    conn: &Conn,
    send: &mut SendStream,
    mut resp: Response,
    head_only: bool,
) -> Result<(), Stop> {
    let bodiless = matches!(resp.status(), 204 | 304);
    let body = resp.take_body();
    let streaming = body.is_stream();
    let length = (!bodiless && !streaming).then(|| match &body {
        Body::File(file) => file.len(),
        body => body.len() as u64,
    });
    // An owned body counts against the connection's budget from now on,
    // before any other handler is polled (`Call::poll`); a `&'static` one
    // costs nothing, and a file or a stream only for the piece in hand.
    let mut held = Held::new(conn);
    if let Body::Owned(bytes) = &body {
        held.add(bytes.len());
    }
    let mut out = Vec::with_capacity(512);
    write_head(&mut out, &resp, length);
    if head_only || bodiless || length == Some(0) {
        // A streamed body that is not sent is dropped here, which tells
        // its sender.
        drop(body);
        write(conn, send, &out).await?;
        return send.finish().map_err(|_| Stop::Gone);
    }
    match body {
        Body::Stream(body) => {
            write(conn, send, &out).await?;
            loop {
                match body.next().await {
                    Next::Piece(piece) if piece.is_empty() => {}
                    Next::Piece(piece) => {
                        held.add(piece.len());
                        out.clear();
                        Header::write(&mut out, kind::DATA, piece.len() as u64);
                        if piece.len() <= SMALL_BODY {
                            out.extend_from_slice(&piece);
                            write(conn, send, &out).await?;
                            held.release(piece.len());
                        } else {
                            write(conn, send, &out).await?;
                            write_held(conn, send, &piece, Some(&mut held)).await?;
                        }
                    }
                    Next::End(None) => break,
                    Next::End(Some(trailers)) => {
                        out.clear();
                        write_trailers(&mut out, &trailers);
                        write(conn, send, &out).await?;
                        break;
                    }
                    // The producer gave up: the response is cut, and the
                    // client must be able to tell.
                    Next::Aborted => return Err(Stop::Stream(Code::InternalError)),
                }
            }
        }
        Body::File(file) => {
            // One DATA frame for the whole range, read and written in
            // pieces.
            let (mut at, mut left) = (file.offset(), file.len());
            Header::write(&mut out, kind::DATA, left);
            write(conn, send, &out).await?;
            let mut buf = Vec::new();
            while left > 0 {
                buf.clear();
                buf.reserve(FILE_PIECE.min(left) as usize);
                let (read, filled) = file.file.read_at(buf, at).await;
                buf = filled;
                let n = match read {
                    Ok(n) if n > 0 => (n as u64).min(left),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // The file is shorter than the length announced, or
                    // cannot be read.
                    _ => return Err(Stop::Stream(Code::InternalError)),
                };
                held.add(n as usize);
                write_held(conn, send, &buf[..n as usize], Some(&mut held)).await?;
                at += n;
                left -= n;
            }
        }
        body => {
            let bytes = body.into_cow();
            Header::write(&mut out, kind::DATA, bytes.len() as u64);
            if bytes.len() <= SMALL_BODY {
                out.extend_from_slice(&bytes);
                write(conn, send, &out).await?;
            } else {
                write(conn, send, &out).await?;
                write_held(conn, send, &bytes, Some(&mut held)).await?;
            }
        }
    }
    send.finish().map_err(|_| Stop::Gone)
}
