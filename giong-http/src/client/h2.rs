//! HTTP/2 connections for the client (RFC 9113).
//!
//! One connection carries many requests at once, one stream each. The
//! connection has a task of its own, on the worker that opened it, which
//! writes the frames the requests queue and reads the frames the server
//! sends; a request holds a [`Stream`], which waits for the head of its
//! response and reads its body as the task delivers it. Everything stays on
//! one worker, so the state they share sits behind one `RefCell`.
//!
//! What a server can make the client hold is bounded: response bodies by
//! the windows the client grants (1 MiB for each stream, 4 MiB for the
//! connection, granted again only as bodies are read), header blocks by
//! `max_head_size`, and frames the client must answer (PING, SETTINGS) by
//! not reading while too much output waits.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::mem;
use std::ops::Range;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use giong::time::sleep_until;

use super::conn::Stream as Socket;
use crate::h2::frame::{self, Head, Reason, PREFACE};
use crate::h2::hpack::{Decoder, Encoder, Index, TABLE_SIZE};
use crate::parse::{is_field_value, is_token_str};

/// The window each stream starts with: how much of a response the server
/// may send before the client has read any of it.
const STREAM_WINDOW: u32 = 1 << 20;
/// The window of the connection: what all the responses together may have
/// sent and not been read.
const CONN_WINDOW: u32 = 4 << 20;
/// Largest frame accepted (`SETTINGS_MAX_FRAME_SIZE`, left at its default).
const MAX_FRAME: usize = frame::DEFAULT_FRAME_SIZE;
/// Output beyond which no more request bodies are queued in one turn.
const OUT_HIGH: usize = 64 * 1024;
/// Output beyond which the connection stops reading: a server that sends
/// PINGs and SETTINGS without reading the answers cannot make it buffer.
const OUT_PAUSE: usize = 256 * 1024;
/// Streams assumed to be allowed until the server's SETTINGS says.
const INITIAL_MAX_STREAMS: u32 = 100;
/// Informational (1xx) responses accepted before the final one.
const MAX_INFORMATIONAL: u32 = 32;
/// Highest stream identifier.
const MAX_STREAM_ID: u32 = (1 << 31) - 1;

/// What a connection needs to know from the client's settings.
#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub(crate) max_head_size: usize,
    pub(crate) idle_timeout: Duration,
    /// How long a write may take before the server is taken to be stuck.
    pub(crate) write_timeout: Duration,
}

/// What a connection tells its pool.
pub(crate) enum Event {
    /// A stream ended: there is room for another request.
    StreamClosed,
    /// The connection ended; it carries no more requests.
    Closed,
}

/// A request, as the client sends it on a stream.
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) scheme: &'a str,
    pub(crate) authority: &'a str,
    pub(crate) path: &'a str,
    /// Lowercase names, without the fields HTTP/2 forbids.
    pub(crate) fields: &'a [(String, String)],
    pub(crate) body: Option<&'a [u8]>,
    pub(crate) head_request: bool,
}

/// Why a stream failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamError {
    /// The server did not process the request (`REFUSED_STREAM`, or a
    /// stream beyond the last one a GOAWAY names): it can be sent again,
    /// whatever its method.
    Refused(String),
    /// The connection ended before the response did. `received` says
    /// whether anything of the response had arrived.
    Closed { message: String, received: bool },
    /// The server reset the stream.
    Reset(String),
    /// The server broke the protocol on this stream or the connection.
    Protocol(String),
    /// The response head is larger than `max_head_size`.
    TooLarge(String),
}

/// The head of a response.
pub(crate) struct ResponseHead {
    pub(crate) status: u16,
    /// Names and values one after the other, which `fields` points into.
    pub(crate) bytes: Vec<u8>,
    pub(crate) fields: Vec<(Range<usize>, Range<usize>)>,
    pub(crate) content_length: Option<u64>,
}

/// A connection, shared by the requests on it. Cloning gives another
/// handle to the same connection.
#[derive(Clone)]
pub(crate) struct Connection {
    inner: Rc<Inner>,
}

struct Inner {
    state: RefCell<State>,
    notify: Box<dyn Fn(Event)>,
}

impl Inner {
    fn notify(&self, event: Event) {
        (self.notify)(event);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // The connection's own task holds one reference: when only it and
        // the streams are left, it may have nothing more to do.
        if Rc::strong_count(&self.inner) <= 2 {
            if let Ok(mut st) = self.inner.state.try_borrow_mut() {
                st.poke();
            }
        }
    }
}

impl Connection {
    /// Starts HTTP/2 on `socket` (after TLS chose `h2`, or with prior
    /// knowledge) and spawns the connection's task. `notify` is told when
    /// a stream ends and when the connection does.
    pub(crate) fn start(socket: Socket, settings: Settings, notify: Box<dyn Fn(Event)>) -> Self {
        let mut state = State::new(settings.max_head_size);
        state.start();
        let inner = Rc::new(Inner {
            state: RefCell::new(state),
            notify,
        });
        drop(giong::spawn(drive(socket, inner.clone(), settings)));
        Self { inner }
    }

    /// Whether another request can be sent on it now.
    pub(crate) fn has_room(&self) -> bool {
        self.inner.state.borrow().has_room()
    }

    /// Streams in progress.
    pub(crate) fn streams(&self) -> usize {
        self.inner.state.borrow().streams.len()
    }

    /// Sends `request` on a new stream, or returns `None` when the
    /// connection has no room for one.
    pub(crate) fn send(&self, request: &Request<'_>) -> Option<Stream> {
        let mut st = self.inner.state.borrow_mut();
        if !st.has_room() {
            return None;
        }
        let id = st.open(request);
        Some(Stream {
            inner: self.inner.clone(),
            id,
            head: None,
            head_request: request.head_request,
            done: false,
        })
    }
}

/// One request and its response. Dropped before the response has ended,
/// it resets the stream (`CANCEL`).
pub(crate) struct Stream {
    inner: Rc<Inner>,
    id: u32,
    head: Option<ResponseHead>,
    head_request: bool,
    /// The response ended and was read, or failed: the stream is gone from
    /// the connection.
    done: bool,
}

impl Stream {
    /// Waits for the head of the final response (1xx responses are
    /// skipped).
    pub(crate) async fn response(&mut self) -> Result<(), StreamError> {
        let id = self.id;
        let inner = self.inner.clone();
        let head = poll_fn(|cx| {
            let mut st = inner.state.borrow_mut();
            let Some(s) = st.streams.get_mut(&id) else {
                return Poll::Ready(Err(StreamError::Closed {
                    message: "the stream is gone".into(),
                    received: false,
                }));
            };
            if let Some(head) = s.head.take() {
                return Poll::Ready(Ok(head));
            }
            if let Some(e) = &s.error {
                return Poll::Ready(Err(e.clone()));
            }
            s.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        match head {
            Ok(head) => {
                self.head = Some(head);
                Ok(())
            }
            Err(e) => {
                self.finish();
                Err(e)
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

    /// The length of the body, when known: none for a `HEAD` request or
    /// a `204` or `304` answer, otherwise what `content-length` says.
    pub(crate) fn body_length(&self) -> Option<u64> {
        let head = self.head();
        if self.head_request || matches!(head.status, 204 | 304) {
            Some(0)
        } else {
            head.content_length
        }
    }

    /// Appends what has arrived of the body to `out`, waiting for some if
    /// nothing has: whole frames, and no more than `limit` bytes unless
    /// one frame is larger. Returns `false`, with nothing appended, at the
    /// end. Cancel safe: what is appended is taken off the stream.
    pub(crate) async fn read(
        &mut self,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<bool, StreamError> {
        if self.done {
            return Ok(false);
        }
        let id = self.id;
        let inner = self.inner.clone();
        let result = poll_fn(|cx| {
            let mut guard = inner.state.borrow_mut();
            let st = &mut *guard;
            let Some(s) = st.streams.get_mut(&id) else {
                return Poll::Ready(Ok(false));
            };
            if !s.data.is_empty() {
                let mut n = 0;
                while let Some(chunk) = s.data.front() {
                    if n > 0 && n + chunk.len() > limit {
                        break;
                    }
                    let chunk = s.data.pop_front().expect("the front was there");
                    n += chunk.len();
                    if out.is_empty() && out.capacity() < chunk.len() {
                        *out = chunk;
                    } else {
                        out.extend_from_slice(&chunk);
                    }
                }
                s.buffered -= n;
                st.consumed(id, n);
                return Poll::Ready(Ok(true));
            }
            if let Some(e) = &s.error {
                return Poll::Ready(Err(e.clone()));
            }
            if s.ended {
                return Poll::Ready(Ok(false));
            }
            s.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        if !matches!(result, Ok(true)) {
            self.finish();
        }
        result
    }

    /// Whether the body has been read to its end (or failed).
    pub(crate) fn is_done(&self) -> bool {
        if self.done {
            return true;
        }
        let st = self.inner.state.borrow();
        st.streams
            .get(&self.id)
            .map_or(true, |s| s.ended && s.data.is_empty() && s.error.is_none())
    }

    /// The response ended or failed, and the stream is off the connection.
    pub(crate) fn is_finished(&self) -> bool {
        self.done
    }

    /// Stops reading: the stream is reset if the response has not ended.
    pub(crate) fn cancel(&mut self) {
        self.finish();
    }

    /// Takes the stream off the connection: reset it if it is still
    /// going, and tell the pool there is room.
    fn finish(&mut self) {
        if mem::replace(&mut self.done, true) {
            return;
        }
        let freed = self.inner.state.borrow_mut().abandon(self.id);
        if freed {
            self.inner.notify(Event::StreamClosed);
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The state of one stream.
struct StreamState {
    /// The request body, and how much of it is sent.
    body: Vec<u8>,
    sent: usize,
    /// Every frame of the request is written.
    request_done: bool,
    /// In `State::sending`.
    queued: bool,
    send_window: i64,
    /// What we let the server send on it, and what was read since we last
    /// said so.
    recv_window: i64,
    consumed: usize,
    head_request: bool,
    /// The final head arrived (it may have been taken already).
    got_head: bool,
    head: Option<ResponseHead>,
    status: u16,
    informational: u32,
    content_length: Option<u64>,
    received: u64,
    received_any: bool,
    data: VecDeque<Vec<u8>>,
    buffered: usize,
    /// END_STREAM arrived.
    ended: bool,
    error: Option<StreamError>,
    /// The stream is closed on the wire (reset by either side).
    reset: bool,
    waker: Option<Waker>,
}

impl StreamState {
    fn wake(&mut self) {
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

/// A header block being received.
struct Block {
    stream: u32,
    end_stream: bool,
    data: Vec<u8>,
}

/// A header block, decoded and checked.
#[derive(Default)]
struct Decoded {
    status: Option<u16>,
    bytes: Vec<u8>,
    fields: Vec<(Range<usize>, Range<usize>)>,
    /// Decoded size, counted as for `SETTINGS_MAX_HEADER_LIST_SIZE`.
    size: usize,
    content_length: Option<u64>,
    malformed: Option<&'static str>,
}

/// A connection error: what the GOAWAY says, and the message requests get.
struct ConnError {
    reason: Reason,
    detail: String,
}

fn protocol(detail: &str) -> ConnError {
    ConnError {
        reason: Reason::Protocol,
        detail: detail.to_owned(),
    }
}

fn reason_name(code: u32) -> String {
    match Reason::from_u32(code) {
        Some(r) => r.name().to_owned(),
        None => format!("error code {code:#x}"),
    }
}

/// The protocol state of a connection, without I/O: frames in, frames out.
struct State {
    encoder: Encoder,
    decoder: Decoder,
    /// Frames to write.
    out: Vec<u8>,
    next_id: u32,
    streams: HashMap<u32, StreamState>,
    /// Streams with request body to send, served in turn.
    sending: VecDeque<u32>,
    peer_max_streams: u32,
    /// The server's `SETTINGS_INITIAL_WINDOW_SIZE`.
    peer_window: i64,
    peer_frame: usize,
    /// What the connection may send.
    send_window: i64,
    /// What the server may send on the connection, and what was read (or
    /// dropped) since we last said so.
    recv_window: i64,
    consumed: usize,
    settings_seen: bool,
    /// The last stream the server will process, from its GOAWAY.
    goaway: Option<u32>,
    /// Why the connection can carry nothing more; set when it ends.
    dead: Option<String>,
    block: Option<Block>,
    max_head_size: usize,
    /// No stream open since then.
    idle_since: Instant,
    /// The connection's task: woken when there is something for it to do.
    driver: Option<Waker>,
    poked: bool,
}

impl State {
    fn new(max_head_size: usize) -> Self {
        Self {
            encoder: Encoder::new(),
            decoder: Decoder::new(TABLE_SIZE),
            out: Vec::new(),
            next_id: 1,
            streams: HashMap::new(),
            sending: VecDeque::new(),
            peer_max_streams: INITIAL_MAX_STREAMS,
            peer_window: i64::from(frame::DEFAULT_WINDOW),
            peer_frame: frame::DEFAULT_FRAME_SIZE,
            send_window: i64::from(frame::DEFAULT_WINDOW),
            recv_window: i64::from(CONN_WINDOW),
            consumed: 0,
            settings_seen: false,
            goaway: None,
            dead: None,
            block: None,
            max_head_size,
            idle_since: Instant::now(),
            driver: None,
            poked: false,
        }
    }

    /// The preface, our SETTINGS, and the connection window beyond its
    /// default.
    fn start(&mut self) {
        self.out.extend_from_slice(PREFACE);
        frame::write_settings(
            &mut self.out,
            &[
                (frame::ENABLE_PUSH, 0),
                (frame::INITIAL_WINDOW_SIZE, STREAM_WINDOW),
                (
                    frame::MAX_HEADER_LIST_SIZE,
                    u32::try_from(self.max_head_size).unwrap_or(u32::MAX),
                ),
            ],
        );
        frame::write_window_update(&mut self.out, 0, CONN_WINDOW - frame::DEFAULT_WINDOW);
    }

    fn poke(&mut self) {
        self.poked = true;
        if let Some(w) = self.driver.take() {
            w.wake();
        }
    }

    fn has_room(&self) -> bool {
        self.dead.is_none()
            && self.goaway.is_none()
            && self.next_id <= MAX_STREAM_ID
            && self.streams.len() < self.peer_max_streams as usize
    }

    fn reading(&self) -> bool {
        self.dead.is_none() && self.out.len() < OUT_PAUSE
    }

    // ----------------------------------------------------------- requests

    /// Opens a stream for `req`, and wakes the connection's task to send
    /// it; returns its identifier.
    fn open(&mut self, req: &Request<'_>) -> u32 {
        let id = self.next_id;
        self.next_id += 2;
        let mut block = Vec::with_capacity(64 + req.path.len());
        let e = &mut self.encoder;
        e.begin(&mut block);
        e.field(b":method", req.method.as_bytes(), Index::Yes, &mut block);
        e.field(b":scheme", req.scheme.as_bytes(), Index::Yes, &mut block);
        e.field(
            b":authority",
            req.authority.as_bytes(),
            Index::Yes,
            &mut block,
        );
        let path_index = if req.path.len() > 256 {
            Index::No
        } else {
            Index::Yes
        };
        e.field(b":path", req.path.as_bytes(), path_index, &mut block);
        for (name, value) in req.fields {
            let index = match name.as_str() {
                "authorization" | "cookie" | "proxy-authorization" => Index::Never,
                "content-length" => Index::No,
                _ if value.len() > 256 => Index::No,
                _ => Index::Yes,
            };
            e.field(name.as_bytes(), value.as_bytes(), index, &mut block);
        }
        let body = req.body.unwrap_or_default();
        let end_stream = body.is_empty();
        frame::write_headers(&mut self.out, id, &block, end_stream, self.peer_frame);
        self.streams.insert(
            id,
            StreamState {
                body: body.to_vec(),
                sent: 0,
                request_done: end_stream,
                queued: !end_stream,
                send_window: self.peer_window,
                recv_window: i64::from(STREAM_WINDOW),
                consumed: 0,
                head_request: req.head_request,
                got_head: false,
                head: None,
                status: 0,
                informational: 0,
                content_length: None,
                received: 0,
                received_any: false,
                data: VecDeque::new(),
                buffered: 0,
                ended: false,
                error: None,
                reset: false,
                waker: None,
            },
        );
        if !end_stream {
            self.sending.push_back(id);
        }
        self.poke();
        id
    }

    /// `n` bytes of stream `id`'s body were read: let the server send more
    /// once half a window has been read.
    fn consumed(&mut self, id: u32, n: usize) {
        if let Some(s) = self.streams.get_mut(&id) {
            s.consumed += n;
            if !s.ended && !s.reset && s.consumed >= STREAM_WINDOW as usize / 2 {
                frame::write_window_update(&mut self.out, id, s.consumed as u32);
                s.recv_window += s.consumed as i64;
                s.consumed = 0;
                // The connection's task may be asleep, with everything the
                // server could send received: wake it to send this.
                self.poke();
            }
        }
        self.give_back(n);
    }

    /// `n` bytes the connection's window counted are read or dropped.
    fn give_back(&mut self, n: usize) {
        self.consumed += n;
        if self.dead.is_none() && self.consumed >= CONN_WINDOW as usize / 2 {
            frame::write_window_update(&mut self.out, 0, self.consumed as u32);
            self.recv_window += self.consumed as i64;
            self.consumed = 0;
            self.poke();
        }
    }

    /// The request of stream `id` is given up, or its response read to the
    /// end: forget it, resetting it if it is still going. Returns whether
    /// there was such a stream.
    fn abandon(&mut self, id: u32) -> bool {
        let Some(s) = self.streams.remove(&id) else {
            return false;
        };
        let complete = s.ended && s.request_done;
        if !complete && !s.reset && self.dead.is_none() {
            frame::write_rst(&mut self.out, id, Reason::Cancel);
        }
        if s.queued {
            self.sending.retain(|&q| q != id);
        }
        // What the server sent and nobody read still counts against the
        // connection's window.
        self.give_back(s.buffered);
        if self.streams.is_empty() {
            self.idle_since = Instant::now();
        }
        self.poke();
        true
    }

    /// Fails stream `id`, resetting it with `rst` unless it is closed
    /// already.
    fn fail_stream(&mut self, id: u32, error: StreamError, rst: Option<Reason>) {
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        if s.error.is_none() {
            s.error = Some(error);
        }
        if let Some(reason) = rst {
            if !s.reset && self.dead.is_none() {
                frame::write_rst(&mut self.out, id, reason);
            }
        }
        s.reset = true;
        s.body = Vec::new();
        s.request_done = true;
        let dropped = mem::take(&mut s.buffered);
        s.data.clear();
        s.wake();
        if s.queued {
            s.queued = false;
            self.sending.retain(|&q| q != id);
        }
        self.give_back(dropped);
    }

    /// Writes request bodies, in turn, within the flow-control windows.
    fn fill(&mut self) {
        if self.dead.is_some() {
            return;
        }
        while self.out.len() < OUT_HIGH && self.send_window > 0 {
            let Some(id) = self.sending.pop_front() else {
                break;
            };
            let Some(s) = self.streams.get_mut(&id) else {
                continue;
            };
            let left = s.body.len() - s.sent;
            let window = s.send_window.min(self.send_window).max(0) as usize;
            let n = left.min(window).min(self.peer_frame);
            if n == 0 {
                // Waits for the server's WINDOW_UPDATE.
                s.queued = false;
                continue;
            }
            let end = n == left;
            frame::write_head(
                &mut self.out,
                n,
                frame::DATA,
                if end { frame::END_STREAM } else { 0 },
                id,
            );
            self.out.extend_from_slice(&s.body[s.sent..s.sent + n]);
            s.sent += n;
            s.send_window -= n as i64;
            self.send_window -= n as i64;
            if end {
                s.body = Vec::new();
                s.sent = 0;
                s.request_done = true;
                s.queued = false;
            } else {
                self.sending.push_back(id);
            }
        }
    }

    /// Puts stream `id` back in turn to send, if it has body left.
    fn enqueue(&mut self, id: u32) {
        if let Some(s) = self.streams.get_mut(&id) {
            if !s.queued && !s.request_done && s.send_window > 0 {
                s.queued = true;
                self.sending.push_back(id);
            }
        }
    }

    // ------------------------------------------------------------- input

    /// Handles every complete frame at the start of `input`; returns how
    /// many bytes that was.
    fn receive(&mut self, input: &[u8]) -> usize {
        let mut at = 0;
        while self.reading() {
            let Some(head) = input.get(at..at + frame::HEAD_LEN) else {
                break;
            };
            let head = Head::parse(head);
            if head.len > MAX_FRAME {
                self.fail(ConnError {
                    reason: Reason::FrameSize,
                    detail: format!(
                        "the server sent a frame of {} bytes, more than the {MAX_FRAME} allowed",
                        head.len
                    ),
                });
                break;
            }
            let end = at + frame::HEAD_LEN + head.len;
            let Some(payload) = input.get(at + frame::HEAD_LEN..end) else {
                break;
            };
            at = end;
            if let Err(e) = self.frame(head, payload) {
                self.fail(e);
            }
        }
        at
    }

    /// Ends the connection with a GOAWAY saying why; every stream fails.
    fn fail(&mut self, e: ConnError) {
        if self.dead.is_some() {
            return;
        }
        frame::write_goaway(&mut self.out, 0, e.reason, e.detail.as_bytes());
        let message = format!("the server broke the HTTP/2 protocol: {}", e.detail);
        self.end(message, true);
    }

    /// The connection is over: every stream still going fails with
    /// `message`.
    fn end(&mut self, message: String, protocol: bool) {
        if self.dead.is_some() {
            return;
        }
        // Set first: nothing is written once the connection is over, not
        // even the window the failed streams give back.
        self.dead = Some(message.clone());
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            let s = self.streams.get_mut(&id).expect("listed");
            if s.ended && s.error.is_none() {
                // Its response arrived whole: it can still be read.
                continue;
            }
            let error = if protocol {
                StreamError::Protocol(message.clone())
            } else {
                StreamError::Closed {
                    message: message.clone(),
                    received: s.received_any,
                }
            };
            self.fail_stream(id, error, None);
        }
        self.block = None;
        self.sending.clear();
    }

    fn frame(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if let Some(block) = &self.block {
            if head.kind != frame::CONTINUATION || head.stream != block.stream {
                return Err(protocol("a header block was interrupted by another frame"));
            }
        }
        if !self.settings_seen && (head.kind != frame::SETTINGS || head.has(frame::ACK)) {
            return Err(protocol("the server's first frame was not SETTINGS"));
        }
        match head.kind {
            frame::DATA => self.on_data(head, payload),
            frame::HEADERS => self.on_headers(head, payload),
            frame::PRIORITY => {
                if head.stream == 0 {
                    return Err(protocol("PRIORITY on stream 0"));
                }
                if payload.len() != 5 {
                    return Err(ConnError {
                        reason: Reason::FrameSize,
                        detail: "PRIORITY with a length other than 5".into(),
                    });
                }
                Ok(())
            }
            frame::RST_STREAM => self.on_rst(head, payload),
            frame::SETTINGS => self.on_settings(head, payload),
            frame::PUSH_PROMISE => Err(protocol(
                "the server sent PUSH_PROMISE, though the client turned push off",
            )),
            frame::PING => self.on_ping(head, payload),
            frame::GOAWAY => self.on_goaway(head, payload),
            frame::WINDOW_UPDATE => self.on_window_update(head, payload),
            frame::CONTINUATION => self.on_continuation(head, payload),
            // Unknown types are ignored (RFC 9113, section 5.5).
            _ => Ok(()),
        }
    }

    /// Whether `id` names a stream the client opened at some point.
    fn opened(&self, id: u32) -> bool {
        id % 2 == 1 && id < self.next_id
    }

    fn on_data(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        let id = head.stream;
        if id == 0 {
            return Err(protocol("DATA on stream 0"));
        }
        let len = payload.len();
        self.recv_window -= len as i64;
        if self.recv_window < 0 {
            return Err(ConnError {
                reason: Reason::FlowControl,
                detail: "the server sent more than the connection's window".into(),
            });
        }
        frame::unpad(&head, payload).map_err(|()| protocol("DATA with more padding than data"))?;
        let data = frame::unpadded(&head, payload);
        let Some(s) = self.streams.get_mut(&id) else {
            if !self.opened(id) {
                return Err(protocol("DATA on a stream the client never opened"));
            }
            // A stream given up: the bytes are dropped.
            self.give_back(len);
            return Ok(());
        };
        if s.reset {
            self.give_back(len);
            return Ok(());
        }
        // Padding is read as soon as it arrives.
        let padding = len - data.len();
        s.recv_window -= len as i64;
        let problem = if s.recv_window < 0 {
            Some((
                "the server sent more than the stream's window",
                Reason::FlowControl,
            ))
        } else if s.ended {
            Some(("DATA after the end of the stream", Reason::StreamClosed))
        } else if !s.got_head {
            Some(("DATA before the response head", Reason::Protocol))
        } else if !data.is_empty() && (s.head_request || matches!(s.status, 204 | 304)) {
            Some((
                "a body on a response that cannot have one",
                Reason::Protocol,
            ))
        } else {
            None
        };
        if let Some((message, reason)) = problem {
            self.give_back(len);
            self.fail_stream(id, StreamError::Protocol(message.into()), Some(reason));
            return Ok(());
        }
        s.received += data.len() as u64;
        s.received_any = true;
        if s.content_length.is_some_and(|cl| s.received > cl) {
            self.give_back(len);
            self.fail_stream(
                id,
                StreamError::Protocol("a body longer than its content-length".into()),
                Some(Reason::Protocol),
            );
            return Ok(());
        }
        if !data.is_empty() {
            s.buffered += data.len();
            // Small frames are joined: a server sending one byte a frame
            // cannot make the client hold a buffer for each.
            match s.data.back_mut() {
                Some(last) if last.len() + data.len() <= MAX_FRAME => {
                    last.extend_from_slice(data);
                }
                _ => s.data.push_back(data.to_vec()),
            }
        }
        if padding > 0 {
            s.consumed += padding;
        }
        let end_stream = head.has(frame::END_STREAM);
        if end_stream {
            self.end_of_response(id);
        } else if let Some(s) = self.streams.get_mut(&id) {
            s.wake();
        }
        if padding > 0 {
            self.give_back(padding);
        }
        Ok(())
    }

    /// END_STREAM arrived on stream `id`.
    fn end_of_response(&mut self, id: u32) {
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        let bodiless = s.head_request || matches!(s.status, 204 | 304);
        if !bodiless && s.content_length.is_some_and(|cl| cl != s.received) {
            self.fail_stream(
                id,
                StreamError::Protocol("a body shorter than its content-length".into()),
                Some(Reason::Protocol),
            );
            return;
        }
        s.ended = true;
        if !s.request_done {
            // The server answered before the whole request was sent: stop
            // sending it (RFC 9113, section 8.1).
            s.body = Vec::new();
            s.request_done = true;
            s.reset = true;
            frame::write_rst(&mut self.out, id, Reason::NoError);
            if s.queued {
                s.queued = false;
                self.sending.retain(|&q| q != id);
            }
        }
        if let Some(s) = self.streams.get_mut(&id) {
            s.wake();
        }
    }

    fn on_headers(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        let id = head.stream;
        if id == 0 {
            return Err(protocol("HEADERS on stream 0"));
        }
        if !self.opened(id) {
            return Err(protocol("HEADERS on a stream the client never opened"));
        }
        frame::unpad(&head, payload)
            .map_err(|()| protocol("HEADERS with more padding than data"))?;
        let mut fragment = frame::unpadded(&head, payload);
        if head.has(frame::PRIORITY_FLAG) {
            if fragment.len() < 5 {
                return Err(ConnError {
                    reason: Reason::FrameSize,
                    detail: "HEADERS too short for its priority fields".into(),
                });
            }
            fragment = &fragment[5..];
        }
        let end_stream = head.has(frame::END_STREAM);
        if head.has(frame::END_HEADERS) {
            return self.header_block(id, end_stream, fragment);
        }
        let mut data = Vec::with_capacity(fragment.len() * 2);
        data.extend_from_slice(fragment);
        self.check_block_size(data.len())?;
        self.block = Some(Block {
            stream: id,
            end_stream,
            data,
        });
        Ok(())
    }

    fn on_continuation(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        let Some(mut block) = self.block.take() else {
            return Err(protocol("CONTINUATION without a header block before it"));
        };
        block.data.extend_from_slice(payload);
        self.check_block_size(block.data.len())?;
        if head.has(frame::END_HEADERS) {
            return self.header_block(block.stream, block.end_stream, &block.data);
        }
        self.block = Some(block);
        Ok(())
    }

    /// An encoded header block may be twice the decoded limit (Huffman
    /// codes can be longer than the bytes they stand for), not more.
    fn check_block_size(&self, len: usize) -> Result<(), ConnError> {
        let limit = 2 * self.max_head_size.max(16 * 1024);
        if len > limit {
            return Err(ConnError {
                reason: Reason::EnhanceYourCalm,
                detail: format!("the server sent a header block of more than {limit} bytes"),
            });
        }
        Ok(())
    }

    /// Decodes a complete header block. The dynamic table changes whatever
    /// the block is for, so every block is decoded.
    fn decode(&mut self, block: &[u8]) -> Result<Decoded, ConnError> {
        let max = self.max_head_size;
        let mut d = Decoded::default();
        self.decoder
            .decode(block, |name, value| check_field(&mut d, max, name, value))
            .map_err(|e| ConnError {
                reason: Reason::Compression,
                detail: format!("a header block could not be decoded: {}", e.0),
            })?;
        Ok(d)
    }

    fn header_block(&mut self, id: u32, end_stream: bool, block: &[u8]) -> Result<(), ConnError> {
        let d = self.decode(block)?;
        let max = self.max_head_size;
        let Some(s) = self.streams.get_mut(&id) else {
            // A stream given up: the block only mattered to the table.
            return Ok(());
        };
        if s.reset {
            return Ok(());
        }
        s.received_any = true;
        let fail = |st: &mut Self, error: StreamError, reason: Reason| {
            st.fail_stream(id, error, Some(reason));
            Ok(())
        };
        if s.ended {
            return fail(
                self,
                StreamError::Protocol("a header block after the end of the stream".into()),
                Reason::StreamClosed,
            );
        }
        if d.size > max {
            return fail(
                self,
                StreamError::TooLarge(format!(
                    "the response head is larger than {max} bytes (ClientBuilder::max_head_size)"
                )),
                Reason::Cancel,
            );
        }
        if let Some(m) = d.malformed {
            return fail(self, StreamError::Protocol(m.into()), Reason::Protocol);
        }
        if s.got_head {
            // Trailers: they end the stream, and carry no pseudo-header.
            if !end_stream {
                return fail(
                    self,
                    StreamError::Protocol(
                        "a second header block that does not end the stream".into(),
                    ),
                    Reason::Protocol,
                );
            }
            if d.status.is_some() {
                return fail(
                    self,
                    StreamError::Protocol("trailers with a :status".into()),
                    Reason::Protocol,
                );
            }
            self.end_of_response(id);
            return Ok(());
        }
        let Some(status) = d.status else {
            return fail(
                self,
                StreamError::Protocol("a response without :status".into()),
                Reason::Protocol,
            );
        };
        if status == 101 {
            return fail(
                self,
                StreamError::Protocol("101 (Switching Protocols), which HTTP/2 forbids".into()),
                Reason::Protocol,
            );
        }
        if status < 200 {
            s.informational += 1;
            if end_stream || s.informational > MAX_INFORMATIONAL {
                let m = if end_stream {
                    "a 1xx response that ends the stream"
                } else {
                    "more than 32 1xx responses"
                };
                return fail(self, StreamError::Protocol(m.into()), Reason::Protocol);
            }
            return Ok(());
        }
        s.got_head = true;
        s.status = status;
        s.content_length = d.content_length;
        s.head = Some(ResponseHead {
            status,
            bytes: d.bytes,
            fields: d.fields,
            content_length: d.content_length,
        });
        if end_stream {
            self.end_of_response(id);
        } else {
            s.wake();
        }
        Ok(())
    }

    fn on_rst(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if payload.len() != 4 {
            return Err(ConnError {
                reason: Reason::FrameSize,
                detail: "RST_STREAM with a length other than 4".into(),
            });
        }
        let id = head.stream;
        if id == 0 {
            return Err(protocol("RST_STREAM on stream 0"));
        }
        if !self.opened(id) {
            return Err(protocol("RST_STREAM on a stream the client never opened"));
        }
        let code = frame::read_u32(payload);
        let Some(s) = self.streams.get_mut(&id) else {
            return Ok(());
        };
        s.reset = true;
        if s.ended && code == Reason::NoError as u32 {
            // The response is whole; the server only stops the request.
            s.body = Vec::new();
            s.request_done = true;
            if s.queued {
                s.queued = false;
                self.sending.retain(|&q| q != id);
            }
            return Ok(());
        }
        let error = if code == Reason::RefusedStream as u32 {
            StreamError::Refused("the server refused the stream (REFUSED_STREAM)".into())
        } else {
            StreamError::Reset(format!(
                "the server reset the stream ({})",
                reason_name(code)
            ))
        };
        self.fail_stream(id, error, None);
        Ok(())
    }

    fn on_settings(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("SETTINGS on a stream other than 0"));
        }
        if head.has(frame::ACK) {
            if !payload.is_empty() {
                return Err(ConnError {
                    reason: Reason::FrameSize,
                    detail: "SETTINGS acknowledgement with a payload".into(),
                });
            }
            return Ok(());
        }
        if payload.len() % 6 != 0 {
            return Err(ConnError {
                reason: Reason::FrameSize,
                detail: "SETTINGS with a length that is not a multiple of 6".into(),
            });
        }
        self.settings_seen = true;
        for setting in payload.chunks_exact(6) {
            let id = u16::from_be_bytes([setting[0], setting[1]]);
            let value = frame::read_u32(&setting[2..]);
            match id {
                frame::HEADER_TABLE_SIZE => self.encoder.set_limit(value as usize),
                frame::ENABLE_PUSH if value != 0 => {
                    return Err(protocol(
                        "the server sent SETTINGS_ENABLE_PUSH other than 0",
                    ))
                }
                frame::MAX_CONCURRENT_STREAMS => self.peer_max_streams = value,
                frame::INITIAL_WINDOW_SIZE => {
                    let value = i64::from(value);
                    if value > frame::MAX_WINDOW {
                        return Err(ConnError {
                            reason: Reason::FlowControl,
                            detail: "SETTINGS_INITIAL_WINDOW_SIZE above 2^31-1".into(),
                        });
                    }
                    let delta = value - self.peer_window;
                    self.peer_window = value;
                    let ids: Vec<u32> = self.streams.keys().copied().collect();
                    for id in ids {
                        let s = self.streams.get_mut(&id).expect("listed");
                        s.send_window += delta;
                        if s.send_window > frame::MAX_WINDOW {
                            return Err(ConnError {
                                reason: Reason::FlowControl,
                                detail: "a stream window grew above 2^31-1".into(),
                            });
                        }
                        self.enqueue(id);
                    }
                }
                frame::MAX_FRAME_SIZE => {
                    if !(frame::DEFAULT_FRAME_SIZE as u32..=frame::MAX_FRAME_SIZE_LIMIT)
                        .contains(&value)
                    {
                        return Err(protocol("SETTINGS_MAX_FRAME_SIZE out of range"));
                    }
                    // Larger frames save little: keep ours at the default.
                    self.peer_frame = (value as usize).min(MAX_FRAME);
                }
                // MAX_HEADER_LIST_SIZE is advice; unknown settings are
                // ignored (RFC 9113, section 6.5.2).
                _ => {}
            }
        }
        frame::write_settings_ack(&mut self.out);
        Ok(())
    }

    fn on_ping(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("PING on a stream other than 0"));
        }
        if payload.len() != 8 {
            return Err(ConnError {
                reason: Reason::FrameSize,
                detail: "PING with a length other than 8".into(),
            });
        }
        if !head.has(frame::ACK) {
            frame::write_ping(&mut self.out, true, payload);
        }
        Ok(())
    }

    fn on_goaway(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("GOAWAY on a stream other than 0"));
        }
        if payload.len() < 8 {
            return Err(ConnError {
                reason: Reason::FrameSize,
                detail: "GOAWAY shorter than 8 bytes".into(),
            });
        }
        let last = frame::read_u32(payload) & 0x7fff_ffff;
        let code = frame::read_u32(&payload[4..]);
        let debug = String::from_utf8_lossy(&payload[8..payload.len().min(8 + 200)]).into_owned();
        let last = self.goaway.map_or(last, |l| l.min(last));
        self.goaway = Some(last);
        let why = if code == Reason::NoError as u32 && debug.is_empty() {
            "the server is closing the connection (GOAWAY)".to_owned()
        } else if debug.is_empty() {
            format!(
                "the server is closing the connection (GOAWAY, {})",
                reason_name(code)
            )
        } else {
            format!(
                "the server is closing the connection (GOAWAY, {}: {debug:?})",
                reason_name(code)
            )
        };
        let refused: Vec<u32> = self
            .streams
            .keys()
            .copied()
            .filter(|&id| id > last)
            .collect();
        for id in refused {
            if let Some(s) = self.streams.get_mut(&id) {
                s.reset = true;
            }
            self.fail_stream(
                id,
                StreamError::Refused(format!("{why} and did not process the request")),
                None,
            );
        }
        Ok(())
    }

    fn on_window_update(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if payload.len() != 4 {
            return Err(ConnError {
                reason: Reason::FrameSize,
                detail: "WINDOW_UPDATE with a length other than 4".into(),
            });
        }
        let increment = i64::from(frame::read_u32(payload) & 0x7fff_ffff);
        let id = head.stream;
        if id == 0 {
            if increment == 0 {
                return Err(protocol("WINDOW_UPDATE of 0 on the connection"));
            }
            self.send_window += increment;
            if self.send_window > frame::MAX_WINDOW {
                return Err(ConnError {
                    reason: Reason::FlowControl,
                    detail: "the connection window grew above 2^31-1".into(),
                });
            }
            return Ok(());
        }
        if !self.opened(id) {
            return Err(protocol(
                "WINDOW_UPDATE on a stream the client never opened",
            ));
        }
        let Some(s) = self.streams.get_mut(&id) else {
            return Ok(());
        };
        if increment == 0 {
            self.fail_stream(
                id,
                StreamError::Protocol("WINDOW_UPDATE of 0 on the stream".into()),
                Some(Reason::Protocol),
            );
            return Ok(());
        }
        s.send_window += increment;
        if s.send_window > frame::MAX_WINDOW {
            self.fail_stream(
                id,
                StreamError::Protocol("the stream window grew above 2^31-1".into()),
                Some(Reason::FlowControl),
            );
            return Ok(());
        }
        self.enqueue(id);
        Ok(())
    }
}

/// Checks one decoded field of a response head and keeps it.
fn check_field(d: &mut Decoded, max: usize, name: &[u8], value: &[u8]) {
    d.size += name.len() + value.len() + 32;
    if d.malformed.is_some() {
        return;
    }
    if let Some(pseudo) = name.strip_prefix(b":") {
        if !d.fields.is_empty() {
            d.malformed = Some("a pseudo-header after a regular header field");
        } else if pseudo != b"status" {
            d.malformed = Some("a pseudo-header other than :status in a response");
        } else if d.status.is_some() {
            d.malformed = Some("two :status pseudo-headers");
        } else {
            match value {
                [a, b, c] if value.iter().all(u8::is_ascii_digit) => {
                    let status =
                        u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0');
                    if (100..=599).contains(&status) {
                        d.status = Some(status);
                    } else {
                        d.malformed = Some("a :status outside 100 to 599");
                    }
                }
                _ => d.malformed = Some("a :status that is not three digits"),
            }
        }
        return;
    }
    if !is_token_str(name) || name.iter().any(u8::is_ascii_uppercase) {
        d.malformed = Some("a header name that is not a lowercase token");
        return;
    }
    if matches!(
        name,
        b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding" | b"upgrade"
    ) {
        d.malformed = Some("a connection-specific header field, which HTTP/2 forbids");
        return;
    }
    if !is_field_value(value)
        || value.first().is_some_and(|&b| b == b' ' || b == b'\t')
        || value.last().is_some_and(|&b| b == b' ' || b == b'\t')
    {
        d.malformed = Some("a header value with a control character or surrounding space");
        return;
    }
    if name == b"content-length" {
        let n = match value {
            v if !v.is_empty() && v.len() <= 19 && v.iter().all(u8::is_ascii_digit) => {
                v.iter().fold(0u64, |n, &b| n * 10 + u64::from(b - b'0'))
            }
            _ => {
                d.malformed = Some("a content-length that is not a number");
                return;
            }
        };
        if d.content_length.is_some_and(|cl| cl != n) {
            d.malformed = Some("two different content-length values");
            return;
        }
        d.content_length = Some(n);
    }
    if d.size <= max {
        let start = d.bytes.len();
        d.bytes.extend_from_slice(name);
        let mid = d.bytes.len();
        d.bytes.extend_from_slice(value);
        d.fields.push((start..mid, mid..d.bytes.len()));
    }
}

/// What woke the connection's task.
enum Wake<R, W> {
    Read(R),
    Wrote(W),
    Poked,
    Timer,
}

/// The connection's task: writes what the state queues, reads frames into
/// it, and closes the connection when it is over or idle.
async fn drive(socket: Socket, inner: Rc<Inner>, settings: Settings) {
    let mut input: Vec<u8> = Vec::new();
    let mut recv = pin!(Some(socket.recv()));
    let mut write = pin!(None);
    let mut write_deadline: Option<Instant> = None;
    let mut timer = pin!(sleep_until(Instant::now() + settings.idle_timeout));
    if false {
        // Names the type of the write future for `write`.
        write.set(Some(socket.write_all(Vec::new())));
    }
    loop {
        let (out, over, reading, idle_since) = {
            let mut st = inner.state.borrow_mut();
            if !input.is_empty() && st.reading() {
                let used = st.receive(&input);
                input.drain(..used);
            }
            st.fill();
            if st.dead.is_none() && st.streams.is_empty() {
                if Rc::strong_count(&inner) == 1 {
                    // Nobody holds the connection any more.
                    frame::write_goaway(&mut st.out, 0, Reason::NoError, b"");
                    st.end("the client was dropped".into(), false);
                } else if st.goaway.is_some() {
                    st.end("the server closed the connection (GOAWAY)".into(), false);
                }
            }
            let out = (write.is_none() && !st.out.is_empty()).then(|| mem::take(&mut st.out));
            st.poked = false;
            // Once the connection is over, only the last write is waited for.
            let idle_since = (st.dead.is_none() && st.streams.is_empty()).then_some(st.idle_since);
            (out, st.dead.is_some(), st.reading(), idle_since)
        };
        if let Some(out) = out {
            write.set(Some(socket.write_all(out)));
            write_deadline = Some(Instant::now() + settings.write_timeout);
        }
        if over && write.is_none() {
            break;
        }

        let idle_deadline = idle_since.map(|t| t + settings.idle_timeout);
        let deadline = [idle_deadline, write_deadline]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        if timer.deadline() != deadline {
            timer.as_mut().get_mut().reset(deadline);
        }
        let woke = poll_fn(|cx| {
            if let Some(w) = write.as_mut().as_pin_mut() {
                if let Poll::Ready(done) = w.poll(cx) {
                    return Poll::Ready(Wake::Wrote(done));
                }
            }
            if reading && !over {
                if let Some(r) = recv.as_mut().as_pin_mut() {
                    if let Poll::Ready(got) = r.poll(cx) {
                        return Poll::Ready(Wake::Read(got));
                    }
                }
            }
            {
                let mut st = inner.state.borrow_mut();
                if st.poked {
                    st.poked = false;
                    return Poll::Ready(Wake::Poked);
                }
                st.driver = Some(cx.waker().clone());
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Timer);
            }
            Poll::Pending
        })
        .await;
        match woke {
            Wake::Wrote(Ok(())) => {
                write.set(None);
                write_deadline = None;
            }
            Wake::Wrote(Err(e)) => {
                write.set(None);
                inner
                    .state
                    .borrow_mut()
                    .end(format!("writing to the connection failed: {e}"), false);
                break;
            }
            Wake::Read(Ok(chunk)) if !chunk.is_empty() => {
                input.extend_from_slice(&chunk);
                drop(chunk);
                recv.set(Some(socket.recv()));
            }
            Wake::Read(Ok(_)) => {
                recv.set(None);
                inner
                    .state
                    .borrow_mut()
                    .end("the server closed the connection".into(), false);
            }
            Wake::Read(Err(e)) => {
                recv.set(None);
                inner
                    .state
                    .borrow_mut()
                    .end(format!("reading from the connection failed: {e}"), false);
            }
            Wake::Poked => {}
            Wake::Timer => {
                let now = Instant::now();
                let mut st = inner.state.borrow_mut();
                if write_deadline.is_some_and(|d| d <= now) {
                    st.end(
                        format!(
                            "the server did not read what the client sent for {:?}",
                            settings.write_timeout
                        ),
                        false,
                    );
                    break;
                }
                if st.streams.is_empty()
                    && st.dead.is_none()
                    && st.idle_since + settings.idle_timeout <= now
                {
                    frame::write_goaway(&mut st.out, 0, Reason::NoError, b"");
                    st.end("the connection was idle for too long".into(), false);
                }
            }
        }
    }
    inner.notify(Event::Closed);
    // A write still going is abandoned: the connection is closed anyway.
    write.set(None);
    recv.set(None);
    let _ = giong::time::timeout(Duration::from_secs(1), socket.close()).await;
}

/// The check behind the `h2_client` fuzz target and its randomised test:
/// what a server sends, built from fuzz data, fed in pieces to the state
/// of a connection while requests come and go.
#[cfg(any(test, fuzzing))]
pub(crate) mod fuzzing {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::Wake;

    /// Stands for the connection's task, asleep: records being woken.
    struct Asleep(AtomicBool);

    impl Wake for Asleep {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Runs `call`, one of the calls a request's owner makes, while the
    /// connection's task waits for something to do: whatever the call
    /// queues must wake the task, or it waits forever.
    fn awake<R>(st: &mut State, call: impl FnOnce(&mut State) -> R) -> R {
        let asleep = Arc::new(Asleep(AtomicBool::new(false)));
        st.driver = Some(Waker::from(asleep.clone()));
        st.poked = false;
        let queued = st.out.len();
        let r = call(st);
        assert!(
            st.out.len() == queued || asleep.0.load(Ordering::Relaxed),
            "{} bytes queued without waking the connection's task",
            st.out.len() - queued
        );
        r
    }

    struct Bytes<'a>(&'a [u8]);

    impl Bytes<'_> {
        fn byte(&mut self) -> u8 {
            let Some((&b, rest)) = self.0.split_first() else {
                return 0;
            };
            self.0 = rest;
            b
        }

        fn take(&mut self, n: usize) -> &[u8] {
            let (taken, rest) = self.0.split_at(n.min(self.0.len()));
            self.0 = rest;
            taken
        }

        fn pick<T: Copy>(&mut self, from: &[T]) -> T {
            from[self.byte() as usize % from.len()]
        }
    }

    /// What a server sends, built from fuzz data: SETTINGS (usually), then
    /// frames that are well delimited but otherwise anything, mostly for
    /// the first streams a client opens, with header blocks a real encoder
    /// made and now and then raw bytes instead.
    fn server_frames(d: &mut Bytes<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = Encoder::new();
        let mut block = Vec::new();
        if d.byte() % 16 != 0 {
            let mut settings = Vec::new();
            for _ in 0..d.byte() % 4 {
                settings.push(d.pick(&[
                    (frame::INITIAL_WINDOW_SIZE, 0),
                    (frame::INITIAL_WINDOW_SIZE, 10),
                    (frame::INITIAL_WINDOW_SIZE, 70_000),
                    (frame::MAX_FRAME_SIZE, 20_000),
                    (frame::HEADER_TABLE_SIZE, 0),
                    (frame::HEADER_TABLE_SIZE, 100),
                    (frame::MAX_CONCURRENT_STREAMS, 1),
                    (frame::MAX_CONCURRENT_STREAMS, 0),
                ]));
            }
            frame::write_settings(&mut out, &settings);
        }
        // Enough to fill every window, not so much that a run is slow.
        while !d.0.is_empty() && out.len() < 6 << 20 {
            let op = d.byte() % 12;
            let stream = match d.byte() {
                0 => 0,
                b @ 1..=239 => u32::from(b % 4) * 2 + 1,
                b @ 240..=251 => u32::from(b % 16) * 2 + 1,
                b => u32::from(b),
            };
            let flags = d.byte();
            match op {
                0..=3 | 10 => {
                    // A response head, or trailers.
                    let mut fields = Vec::new();
                    if op != 10 {
                        let status = d.pick(&[
                            "200", "200", "200", "200", "204", "304", "404", "100", "103", "101",
                            "abc", "", "2000",
                        ]);
                        if d.byte() % 16 != 0 {
                            fields.push((":status", status));
                        }
                    }
                    for _ in 0..d.byte() % 4 {
                        let valid = d.byte() % 4 != 0;
                        fields.push(if valid {
                            d.pick(&[
                                ("content-length", "3"),
                                ("content-length", "0"),
                                ("content-type", "text/plain"),
                                ("set-cookie", "a=1"),
                                ("x-long", "0123456789012345678901234567890123456789"),
                            ])
                        } else {
                            d.pick(&[
                                ("content-length", "x"),
                                ("connection", "close"),
                                ("Upper", "x"),
                                (":path", "/"),
                                (":status", "200"),
                                ("x-ctl", "a\x01b"),
                                ("x-space", " a"),
                            ])
                        });
                    }
                    block.clear();
                    encoder.begin(&mut block);
                    for (name, value) in fields {
                        let index = d.pick(&[Index::Yes, Index::No, Index::Never]);
                        encoder.field(name.as_bytes(), value.as_bytes(), index, &mut block);
                    }
                    let end_stream = flags & frame::END_STREAM;
                    if flags & 0xc0 != 0xc0 {
                        frame::write_head(
                            &mut out,
                            block.len(),
                            frame::HEADERS,
                            end_stream | frame::END_HEADERS,
                            stream,
                        );
                        out.extend_from_slice(&block);
                    } else {
                        // Split over CONTINUATION.
                        let half = block.len() / 2;
                        frame::write_head(&mut out, half, frame::HEADERS, end_stream, stream);
                        out.extend_from_slice(&block[..half]);
                        let cont = if d.byte() != 0 { stream } else { stream + 2 };
                        frame::write_head(
                            &mut out,
                            block.len() - half,
                            frame::CONTINUATION,
                            frame::END_HEADERS,
                            cont,
                        );
                        out.extend_from_slice(&block[half..]);
                    }
                }
                4 | 5 => {
                    // Now and then many full frames at once, around the
                    // windows the client grants (1 MiB a stream, half of
                    // which read grants it again, 4 MiB in all), unpadded
                    // so that they are not too large, and sometimes with a
                    // valid head before them.
                    let bulk = flags & 0x40 != 0;
                    let (n, times) = if bulk {
                        (16_384, d.pick(&[8, 32, 63, 64, 65, 255, 256, 257]))
                    } else {
                        (d.pick(&[0usize, 1, 3, 40, 1000, 16_384]), 1)
                    };
                    if bulk && flags & 0x20 != 0 {
                        block.clear();
                        encoder.begin(&mut block);
                        encoder.field(b":status", b"200", Index::Yes, &mut block);
                        frame::write_head(
                            &mut out,
                            block.len(),
                            frame::HEADERS,
                            frame::END_HEADERS,
                            stream,
                        );
                        out.extend_from_slice(&block);
                    }
                    let mut payload = Vec::new();
                    let mut f = flags & (frame::END_STREAM | frame::PADDED);
                    if bulk {
                        f &= !frame::PADDED;
                    }
                    if f & frame::PADDED != 0 {
                        payload.push(d.byte() % 8);
                    }
                    let fill = d.byte();
                    payload.resize(payload.len() + n, fill);
                    if f & frame::PADDED != 0 {
                        let pad = payload[0] as usize;
                        payload.resize(payload.len() + pad, 0);
                    }
                    if payload.is_empty() && f & frame::PADDED != 0 {
                        f &= !frame::PADDED;
                    }
                    for i in 1..=times {
                        // Only the last may end the stream.
                        let f = if i < times { f & !frame::END_STREAM } else { f };
                        frame::write_head(&mut out, payload.len(), frame::DATA, f, stream);
                        out.extend_from_slice(&payload);
                    }
                }
                6 => {
                    let code = d.pick(&[0u32, 1, 2, 7, 8, 0xffff]);
                    frame::write_head(&mut out, 4, frame::RST_STREAM, 0, stream);
                    out.extend_from_slice(&code.to_be_bytes());
                }
                7 => {
                    let increment = d.pick(&[0u32, 1, 100, 65_535, 1 << 20, 0x7fff_ffff]);
                    frame::write_head(&mut out, 4, frame::WINDOW_UPDATE, 0, stream % 8);
                    out.extend_from_slice(&increment.to_be_bytes());
                }
                8 => {
                    if flags & 1 != 0 {
                        frame::write_settings_ack(&mut out);
                    } else {
                        let setting = d.pick(&[
                            (frame::INITIAL_WINDOW_SIZE, 0),
                            (frame::INITIAL_WINDOW_SIZE, 1 << 20),
                            (frame::INITIAL_WINDOW_SIZE, 0x8000_0000),
                            (frame::MAX_FRAME_SIZE, 100),
                            (frame::HEADER_TABLE_SIZE, 50),
                            (frame::MAX_CONCURRENT_STREAMS, 2),
                            (frame::ENABLE_PUSH, 1),
                            (0x99, 1),
                        ]);
                        frame::write_settings(&mut out, &[setting]);
                    }
                }
                9 => match flags % 4 {
                    0 => frame::write_ping(&mut out, flags & 0x10 != 0, &[d.byte(); 8]),
                    1 => {
                        frame::write_head(&mut out, 5, frame::PRIORITY, 0, stream);
                        out.extend_from_slice(&[0, 0, 0, d.byte() % 8, 16]);
                    }
                    2 => {
                        let last = d.pick(&[0u32, 1, 3, 0x7fff_ffff]);
                        let code = d.pick(&[0u32, 2, 0xb]);
                        frame::write_goaway(&mut out, last, Reason::NoError, b"");
                        let at = out.len() - 4;
                        out[at..].copy_from_slice(&code.to_be_bytes());
                    }
                    _ => {
                        frame::write_head(&mut out, 4, frame::PUSH_PROMISE, 4, stream);
                        out.extend_from_slice(&2u32.to_be_bytes());
                    }
                },
                _ => {
                    // Anything at all.
                    let kind = d.byte() % 12;
                    let n = d.byte() as usize % 40;
                    let payload = d.take(n).to_vec();
                    frame::write_head(&mut out, payload.len(), kind, flags, stream);
                    out.extend_from_slice(&payload);
                }
            }
        }
        out
    }

    /// The largest request body the check sends.
    const BODIES: [usize; 4] = [0, 10, 70_000, 200_000];

    fn open(st: &mut State, a: &mut Bytes<'_>, bodies: &mut HashMap<u32, usize>) {
        if !st.has_room() {
            return;
        }
        let method = a.pick(&["GET", "POST", "HEAD", "PUT"]);
        let body = vec![b'b'; a.pick(&BODIES)];
        let fields = [
            ("user-agent".to_owned(), "fuzz".to_owned()),
            ("content-length".to_owned(), body.len().to_string()),
        ];
        let path = a.pick(&["/", "/a?b=c"]);
        let id = awake(st, |st| {
            st.open(&Request {
                method,
                scheme: "https",
                authority: "example.com",
                path,
                fields: &fields,
                body: (!body.is_empty()).then_some(&body[..]),
                head_request: method == "HEAD",
            })
        });
        bodies.insert(id, body.len());
    }

    /// What a stream's owner does: takes the head, reads some of the body,
    /// and gives the stream up once it has ended or failed.
    fn read(st: &mut State, id: u32, limit: usize) -> usize {
        let Some(s) = st.streams.get_mut(&id) else {
            return 0;
        };
        let mut responses = 0;
        if let Some(head) = s.head.take() {
            assert!((200..=599).contains(&head.status), "{}", head.status);
            for (name, value) in &head.fields {
                let name = &head.bytes[name.clone()];
                assert!(is_token_str(name) && !name.iter().any(u8::is_ascii_uppercase));
                assert!(is_field_value(&head.bytes[value.clone()]));
            }
            responses += 1;
        }
        let mut n = 0;
        while let Some(chunk) = s.data.front() {
            if n > 0 && n + chunk.len() > limit {
                break;
            }
            n += chunk.len();
            s.data.pop_front();
        }
        if n > 0 {
            s.buffered -= n;
            awake(st, |st| st.consumed(id, n));
        }
        let s = &st.streams[&id];
        if s.error.is_some() || (s.ended && s.data.is_empty()) {
            awake(st, |st| st.abandon(id));
        }
        responses
    }

    /// What a connection holds stays within the windows it grants.
    fn check_state(st: &State) {
        let mut total = 0;
        for s in st.streams.values() {
            assert_eq!(s.buffered, s.data.iter().map(Vec::len).sum::<usize>());
            assert!(s.buffered <= STREAM_WINDOW as usize, "{}", s.buffered);
            assert!(
                s.data.len() <= 1 + 2 * s.buffered / MAX_FRAME,
                "{} pieces for {} bytes",
                s.data.len(),
                s.buffered
            );
            if s.error.is_some() {
                assert!(s.data.is_empty());
            }
            total += s.buffered;
        }
        assert!(total <= CONN_WINDOW as usize, "{total}");
        if let Some(b) = &st.block {
            assert!(b.data.len() <= 2 * st.max_head_size.max(16 * 1024));
        }
    }

    /// The server's frames in `input`, for what they allow the client to
    /// send: the largest initial window, and window increments per stream
    /// (0 for the connection). Also the PINGs and SETTINGS to answer.
    fn allowance(input: &[u8]) -> (u64, HashMap<u32, u64>, usize, usize) {
        let mut initial = u64::from(frame::DEFAULT_WINDOW);
        let mut increments = HashMap::new();
        let (mut pings, mut settings) = (0, 0);
        let mut at = 0;
        while let Some(head) = input.get(at..at + frame::HEAD_LEN).map(Head::parse) {
            let Some(payload) = input.get(at + frame::HEAD_LEN..at + frame::HEAD_LEN + head.len)
            else {
                break;
            };
            at += frame::HEAD_LEN + head.len;
            match head.kind {
                frame::SETTINGS if !head.has(frame::ACK) => {
                    settings += 1;
                    for s in payload.chunks_exact(6) {
                        if u16::from_be_bytes([s[0], s[1]]) == frame::INITIAL_WINDOW_SIZE {
                            initial = initial.max(u64::from(frame::read_u32(&s[2..])));
                        }
                    }
                }
                frame::WINDOW_UPDATE if payload.len() == 4 => {
                    *increments.entry(head.stream).or_insert(0) +=
                        u64::from(frame::read_u32(payload) & 0x7fff_ffff);
                }
                frame::PING if !head.has(frame::ACK) => pings += 1,
                _ => {}
            }
        }
        (initial, increments, pings, settings)
    }

    /// What the client wrote: the preface and SETTINGS first, then frames
    /// no larger than the default size; requests whose header blocks
    /// decode, pseudo-headers first; DATA only within the windows the
    /// server gave and the bodies, never after END_STREAM or RST_STREAM;
    /// an answer for each PING and SETTINGS at most; nothing after GOAWAY.
    fn check_output(input: &[u8], out: &[u8], bodies: &HashMap<u32, usize>) {
        assert!(out.starts_with(PREFACE));
        let (initial, increments, pings, settings) = allowance(input);
        let mut decoder = Decoder::new(TABLE_SIZE);
        let mut at = PREFACE.len();
        let mut first = true;
        let mut last_id = 0;
        let mut block: Option<(u32, Vec<u8>)> = None;
        let mut closed = std::collections::HashSet::new();
        let mut sent: HashMap<u32, u64> = HashMap::new();
        let mut conn_sent = 0u64;
        let (mut ping_acks, mut settings_acks) = (0, 0);
        let mut goaway = false;
        while at < out.len() {
            let head = Head::parse(&out[at..at + frame::HEAD_LEN]);
            let payload = &out[at + frame::HEAD_LEN..at + frame::HEAD_LEN + head.len];
            at += frame::HEAD_LEN + head.len;
            assert!(!goaway, "a frame after GOAWAY");
            assert!(head.len <= MAX_FRAME);
            if first {
                assert_eq!(head.kind, frame::SETTINGS);
                assert!(!head.has(frame::ACK));
                first = false;
                continue;
            }
            if block.is_some() {
                assert_eq!(head.kind, frame::CONTINUATION);
            }
            match head.kind {
                frame::HEADERS | frame::CONTINUATION => {
                    if head.kind == frame::HEADERS {
                        assert!(head.stream % 2 == 1 && head.stream > last_id);
                        last_id = head.stream;
                        if head.has(frame::END_STREAM) {
                            closed.insert(head.stream);
                        }
                        block = Some((head.stream, Vec::new()));
                    }
                    let (stream, data) = block.as_mut().expect("a block was begun");
                    assert_eq!(*stream, head.stream);
                    data.extend_from_slice(payload);
                    if head.has(frame::END_HEADERS) {
                        let (_, data) = block.take().expect("there is a block");
                        let mut names = Vec::new();
                        decoder
                            .decode(&data, |name, _| names.push(name.to_vec()))
                            .expect("the client's header blocks decode");
                        let pseudo: Vec<&[u8]> = names.iter().take(4).map(Vec::as_slice).collect();
                        assert_eq!(
                            pseudo,
                            [&b":method"[..], b":scheme", b":authority", b":path"]
                        );
                        for name in &names[4..] {
                            assert!(!name.starts_with(b":"));
                            assert!(!name.iter().any(u8::is_ascii_uppercase));
                        }
                    }
                }
                frame::DATA => {
                    assert!(!closed.contains(&head.stream), "DATA after the end");
                    let n = sent.entry(head.stream).or_insert(0);
                    *n += head.len as u64;
                    conn_sent += head.len as u64;
                    let allowed = initial + increments.get(&head.stream).copied().unwrap_or(0);
                    assert!(*n <= allowed, "{n} sent on a window of {allowed}");
                    assert!(*n <= bodies[&head.stream] as u64);
                    if head.has(frame::END_STREAM) {
                        assert_eq!(*n, bodies[&head.stream] as u64);
                        closed.insert(head.stream);
                    }
                }
                frame::RST_STREAM => {
                    assert!(head.stream % 2 == 1 && head.stream <= last_id);
                    closed.insert(head.stream);
                }
                frame::WINDOW_UPDATE => {
                    let increment = frame::read_u32(payload);
                    assert!(increment > 0 && increment <= 0x7fff_ffff);
                }
                frame::PING => {
                    assert!(head.has(frame::ACK));
                    ping_acks += 1;
                }
                frame::SETTINGS => {
                    assert!(head.has(frame::ACK));
                    settings_acks += 1;
                }
                frame::GOAWAY => goaway = true,
                kind => panic!("the client wrote a frame of type {kind}"),
            }
        }
        let allowed = u64::from(frame::DEFAULT_WINDOW) + increments.get(&0).copied().unwrap_or(0);
        assert!(
            conn_sent <= allowed,
            "{conn_sent} sent on a window of {allowed}"
        );
        assert!(ping_acks <= pings && settings_acks <= settings);
    }

    /// Feeds a server's frames, built from `data`, to a connection with
    /// requests in progress; panics if a property does not hold. Returns
    /// the number of responses read.
    pub(crate) fn check_server_frames(data: &[u8]) -> usize {
        let (frames, actions) = data.split_at(data.len() * 3 / 4);
        let mut a = Bytes(actions);
        let input = server_frames(&mut Bytes(frames));
        let mut st = State::new(a.pick(&[200, 4096, 16 * 1024]));
        st.start();
        let mut bodies = HashMap::new();
        // Streams 1, 3, 5 and 7, which most frames are for, and maybe more.
        for _ in 0..4 + a.byte() % 4 {
            open(&mut st, &mut a, &mut bodies);
        }
        let mut out = Vec::new();
        let mut pending = Vec::new();
        let mut at = 0;
        let mut responses = 0;
        loop {
            // Once the actions run out, the rest arrives at once, and the
            // owners read everything there is until nothing more can be
            // taken in, granting windows again as they go.
            let step = match a.byte() {
                0 if a.0.is_empty() => input.len(),
                b => 1 + b as usize * 97,
            };
            let end = (at + step).min(input.len());
            pending.extend_from_slice(&input[at..end]);
            at = end;
            let mut used = 0;
            if st.reading() {
                used = st.receive(&pending);
                assert!(used <= pending.len());
                pending.drain(..used);
            }
            check_state(&st);
            // The requests' owners act, in a deterministic order.
            let mut ids: Vec<u32> = st.streams.keys().copied().collect();
            ids.sort_unstable();
            let action = if a.0.is_empty() { 2 } else { a.byte() % 8 };
            match action {
                0 => open(&mut st, &mut a, &mut bodies),
                1 => {
                    if let Some(&id) = ids.first() {
                        awake(&mut st, |st| st.abandon(id));
                    }
                }
                2..=4 => {
                    for id in ids {
                        responses += read(&mut st, id, a.pick(&[usize::MAX, 1, 16_384]));
                    }
                }
                // Nobody reads for a while.
                _ => {}
            }
            check_state(&st);
            st.fill();
            out.append(&mut st.out);
            if at == input.len() && (pending.is_empty() || (a.0.is_empty() && used == 0)) {
                break;
            }
        }
        check_output(&input, &out, &bodies);
        responses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_with(max: usize, fields: &[(&[u8], &[u8])]) -> Decoded {
        let mut d = Decoded::default();
        for (n, v) in fields {
            check_field(&mut d, max, n, v);
        }
        d
    }

    #[test]
    fn response_heads_are_checked_as_rfc_9113_says() {
        let d = decode_with(
            1024,
            &[
                (b":status", b"200"),
                (b"content-type", b"text/plain"),
                (b"content-length", b"5"),
                (b"content-length", b"5"),
            ],
        );
        assert_eq!(d.malformed, None);
        assert_eq!(d.status, Some(200));
        assert_eq!(d.content_length, Some(5));
        assert_eq!(&d.bytes[d.fields[0].0.clone()], b"content-type");
        assert_eq!(&d.bytes[d.fields[0].1.clone()], b"text/plain");

        for bad in [
            &[(&b"content-type"[..], &b"x"[..]), (b":status", b"200")][..],
            &[(b":status", b"200"), (b":status", b"200")],
            &[(b":status", b"20")],
            &[(b":status", b"600")],
            &[(b":path", b"/")],
            &[(b":status", b"200"), (b"Content-Type", b"x")],
            &[(b":status", b"200"), (b"connection", b"close")],
            &[(b":status", b"200"), (b"transfer-encoding", b"chunked")],
            &[(b":status", b"200"), (b"x", b" padded")],
            &[(b":status", b"200"), (b"x", b"a\nb")],
            &[
                (b":status", b"200"),
                (b"content-length", b"1"),
                (b"content-length", b"2"),
            ],
            &[(b":status", b"200"), (b"content-length", b"-1")],
        ] {
            let d = decode_with(1024, bad);
            assert!(d.malformed.is_some(), "{bad:?}");
        }

        // Over the limit: counted, not kept.
        let d = decode_with(40, &[(b":status", b"200"), (b"x", b"1234567890")]);
        assert!(d.size > 40);
        assert!(d.fields.is_empty());
    }
}
