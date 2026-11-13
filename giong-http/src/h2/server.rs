//! HTTP/2 server connections (RFC 9113).
//!
//! One task per connection, as for HTTP/1.1. The task reads frames, keeps
//! the state of every stream, runs the handlers of the streams itself (each
//! on the heap, polled when its own waker fires) and writes frames, one
//! write per turn of its loop.
//!
//! The protocol lives in [`Session`], which turns received bytes into
//! frames to send and handler calls to make; [`serve`] is the loop around
//! it that does the I/O, runs the handlers and keeps time.
//!
//! Every resource a client can make the server hold is bounded:
//!
//! * streams at once: `max_concurrent_streams` (beyond it, `REFUSED_STREAM`);
//! * request bodies: by the flow-control windows the server grants. A
//!   stream whose handler reads its body as it arrives gets more only as
//!   the handler takes it, the others up to `max_body_size`; and together
//!   the bodies a connection holds stay within a budget (the larger of
//!   `max_body_size` and 1 MiB) beyond which only the oldest stream still
//!   receiving gets more, so that bodies are served in turn instead of
//!   piling up `max_concurrent_streams` times over;
//! * header blocks: `max_head_size`, encoded and decoded, and no frame of
//!   another stream may interleave (the CONTINUATION flood);
//! * frames that do no work (PING, SETTINGS, PRIORITY, empty DATA or
//!   CONTINUATION, WINDOW_UPDATE with nothing to send, frames for streams
//!   already closed, unknown types) and streams reset by the client or
//!   refused: two budgets that refill over time. A client that overdraws
//!   one gets `ENHANCE_YOUR_CALM` (the rapid-reset and flood attacks);
//! * answers to PING and SETTINGS: no input is read while too much output
//!   waits, so a client that does not read cannot make the server buffer;
//! * time: `request_timeout` for a request (and for each piece of a
//!   streamed body), `write_timeout` for each write and for a stream that
//!   the client's flow control keeps from sending, `idle_timeout` with no
//!   stream open.

use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::mem;
use std::net::SocketAddr;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use giong::error::Code;
use giong::time::sleep_until;

use super::frame::{self, Head, Reason, PREFACE};
use super::hpack::{Decoder, Encoder, Index, TABLE_SIZE};
use super::request::{Builder, Built, Trailers};
use super::wake::Wakers;
use crate::body::{Failure, Next, Pipe, StreamBody};
use crate::heap::HeapFuture;
use crate::request::{BodyBytes, Request};
use crate::response::{Body, Response};
use crate::server::{give_buffer, take_buffer, Config, Handler, Transport};

/// The window each stream starts with (`SETTINGS_INITIAL_WINDOW_SIZE`):
/// the protocol's default, which a stream cannot be denied.
const INITIAL_WINDOW: u32 = frame::DEFAULT_WINDOW;
/// The window a stream receiving a body is topped up to, while the
/// connection's body budget allows.
const STREAM_WINDOW: u32 = 256 * 1024;
/// The connection's receive window.
const CONN_WINDOW: u32 = 1024 * 1024;
/// Largest frame we accept (`SETTINGS_MAX_FRAME_SIZE`, left at its default).
const MAX_FRAME: usize = frame::DEFAULT_FRAME_SIZE;
/// Largest DATA frame we send.
const DATA_FRAME: usize = 16 * 1024;
/// DATA frames are added to the output until it holds this much.
const OUT_HIGH: usize = 64 * 1024;
/// With this much output waiting, input is not read: a client that sends
/// PINGs and SETTINGS without reading the answers stops being read.
const OUT_PAUSE: usize = 256 * 1024;
/// Streams recently closed, remembered to answer late frames correctly.
const CLOSED_MEMORY: usize = 64;
/// Frames that do no work: a burst, and the rate at which it refills.
const CHEAP_BURST: u32 = 1_000;
const CHEAP_PER_SEC: u32 = 500;
/// After an error, how long the connection keeps reading (and discarding)
/// so that the client sees the GOAWAY rather than a reset.
const ERROR_LINGER: Duration = Duration::from_secs(1);

/// An error that ends the connection: answered with GOAWAY.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnError {
    reason: Reason,
    code: Code,
    detail: &'static str,
}

fn protocol(detail: &'static str) -> ConnError {
    ConnError {
        reason: Reason::Protocol,
        code: Code::Http2Protocol,
        detail,
    }
}

fn frame_size(detail: &'static str) -> ConnError {
    ConnError {
        reason: Reason::FrameSize,
        code: Code::Http2Protocol,
        detail,
    }
}

fn flow_control(detail: &'static str) -> ConnError {
    ConnError {
        reason: Reason::FlowControl,
        code: Code::Http2Protocol,
        detail,
    }
}

/// A budget of events that refills over time.
struct Budget {
    tokens: u32,
    burst: u32,
    per_sec: u32,
    last: Instant,
}

impl Budget {
    fn new(burst: u32, per_sec: u32) -> Self {
        Self {
            tokens: burst,
            burst,
            per_sec: per_sec.max(1),
            last: Instant::now(),
        }
    }

    /// Takes one token; `false` when there is none left.
    fn take(&mut self) -> bool {
        if self.tokens < self.burst {
            let now = Instant::now();
            let earned =
                now.duration_since(self.last).as_millis() * u128::from(self.per_sec) / 1000;
            if earned > 0 {
                self.tokens = self.burst.min(
                    self.tokens
                        .saturating_add(earned.min(u128::from(u32::MAX)) as u32),
                );
                self.last = now;
            }
        } else {
            self.last = Instant::now();
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// How a request body goes to its handler.
enum Recv {
    /// Received in full before the handler runs: the request waits here,
    /// its body growing at the end of its buffer.
    Buffer(Request),
    /// Read by the handler as it arrives.
    Pipe {
        pipe: Rc<Pipe>,
        /// Received, not handed over yet.
        pending: Vec<u8>,
        limit: u64,
        /// Bytes the handler had taken at the last look.
        taken: u64,
    },
    /// Nothing more to hand over: the body is complete, was refused, or has
    /// no reader any more. Late DATA is counted and dropped.
    Done,
}

/// What is left to send on a stream.
enum Send {
    /// The handler has not answered yet.
    Waiting,
    /// A body in memory, sent from `at`.
    Bytes { body: Body, at: usize },
    /// A body written as it is produced: the current piece, sent from `at`.
    Stream {
        body: StreamBody,
        piece: Vec<u8>,
        at: usize,
    },
    /// Only END_STREAM: an empty DATA frame.
    End,
    /// END_STREAM sent.
    Done,
}

struct Stream {
    id: u32,
    /// END_STREAM received.
    remote_done: bool,
    /// What the client may still send on this stream.
    recv_window: i64,
    /// Bytes received and dealt with, not granted back yet.
    unacked: u32,
    content_length: Option<u64>,
    received: u64,
    /// Body bytes held for this stream: the buffered body (until the
    /// stream ends) or what its handler has not taken yet.
    held: u64,
    recv: Recv,
    send_window: i64,
    send: Send,
    head_only: bool,
    expect_continue: bool,
    continue_sent: bool,
    /// In the queue of streams with something to send.
    queued: bool,
    /// When the request (or the next piece of a streamed body) is due.
    deadline: Option<Instant>,
    /// Since when the stream has had data that flow control holds back.
    blocked_since: Option<Instant>,
}

impl Stream {
    /// Data to send and a window to send it in (or only the end).
    fn sendable(&self) -> bool {
        match &self.send {
            Send::Bytes { body, at } => *at < body.len() && self.send_window > 0,
            Send::Stream { piece, at, .. } => *at < piece.len() && self.send_window > 0,
            Send::End => true,
            Send::Waiting | Send::Done => false,
        }
    }

    fn has_data(&self) -> bool {
        match &self.send {
            Send::Bytes { body, at } => *at < body.len(),
            Send::Stream { piece, at, .. } => *at < piece.len(),
            _ => false,
        }
    }
}

/// What a header block is for.
#[derive(Clone, Copy)]
enum Purpose {
    /// Opens a stream. `refuse` says to reset it at once (self-dependency).
    Open { refuse: bool },
    /// Trailers of the stream in that slot.
    Trailers(usize),
    /// A HEADERS frame where none may come: answer after decoding.
    Misplaced(usize),
    /// Decode for the table's sake only.
    Ignore,
}

/// A header block spread over HEADERS and CONTINUATION frames.
struct Block {
    stream: u32,
    end_stream: bool,
    purpose: Purpose,
    fragment: Vec<u8>,
    deadline: Instant,
}

/// What the loop must do for the handlers.
pub(crate) enum Event {
    /// Run the handler for this request in this slot.
    Start(usize, Request),
    /// Drop the handler of this slot: its stream is gone.
    Stop(usize),
}

/// The protocol state of one connection.
pub(crate) struct Session {
    cfg: Config,
    peer: Option<SocketAddr>,
    decoder: Decoder,
    encoder: Encoder,
    /// The client's `SETTINGS_INITIAL_WINDOW_SIZE` and `MAX_FRAME_SIZE`.
    peer_window: i64,
    peer_frame: usize,
    conn_send: i64,
    conn_recv: i64,
    conn_unacked: u32,
    /// Body bytes the streams hold, and how many they may.
    held: u64,
    body_budget: u64,
    /// Window may be due: data was dealt with, or the budget freed.
    grant_due: bool,
    streams: Vec<Option<Stream>>,
    free: Vec<usize>,
    /// `(id, slot)` of the open streams, by id.
    ids: Vec<(u32, usize)>,
    /// The highest stream id the client has used.
    last_id: u32,
    /// Recently closed streams, and whether we reset them.
    closed: VecDeque<(u32, bool)>,
    block: Option<Block>,
    /// Frames to send.
    pub(crate) out: Vec<u8>,
    queue: VecDeque<usize>,
    settings_seen: bool,
    /// The last stream id of the GOAWAY we sent.
    goaway: Option<u32>,
    failed: bool,
    resets: Budget,
    cheap: Budget,
    events: Vec<Event>,
    pub(crate) wakers: Wakers,
    /// Since when no stream has been open.
    idle_since: Instant,
    /// Lowercased header names, reused.
    name: Vec<u8>,
}

impl Session {
    pub(crate) fn new(cfg: Config, peer: Option<SocketAddr>) -> Self {
        let max = cfg.max_concurrent_streams;
        Self {
            cfg,
            peer,
            decoder: Decoder::new(TABLE_SIZE),
            encoder: Encoder::new(),
            peer_window: i64::from(frame::DEFAULT_WINDOW),
            peer_frame: frame::DEFAULT_FRAME_SIZE,
            conn_send: i64::from(frame::DEFAULT_WINDOW),
            conn_recv: i64::from(CONN_WINDOW),
            conn_unacked: 0,
            held: 0,
            body_budget: (cfg.max_body_size as u64).max(u64::from(CONN_WINDOW)),
            grant_due: false,
            streams: Vec::new(),
            free: Vec::new(),
            ids: Vec::new(),
            last_id: 0,
            closed: VecDeque::new(),
            block: None,
            out: Vec::new(),
            queue: VecDeque::new(),
            settings_seen: false,
            goaway: None,
            failed: false,
            resets: Budget::new(max.saturating_mul(2).max(100), max.max(50)),
            cheap: Budget::new(CHEAP_BURST, CHEAP_PER_SEC),
            events: Vec::new(),
            wakers: Wakers::default(),
            idle_since: Instant::now(),
            name: Vec::new(),
        }
    }

    /// Our SETTINGS, and the connection window we grant beyond the default.
    pub(crate) fn start(&mut self) {
        self.reserve_out();
        frame::write_settings(
            &mut self.out,
            &[
                (
                    frame::MAX_CONCURRENT_STREAMS,
                    self.cfg.max_concurrent_streams,
                ),
                (frame::INITIAL_WINDOW_SIZE, INITIAL_WINDOW),
                (
                    frame::MAX_HEADER_LIST_SIZE,
                    u32::try_from(self.cfg.max_head_size).unwrap_or(u32::MAX),
                ),
                // Streams are sent in turn; priority signals are ignored.
                (frame::NO_RFC7540_PRIORITIES, 1),
            ],
        );
        frame::write_window_update(&mut self.out, 0, CONN_WINDOW - frame::DEFAULT_WINDOW);
    }

    fn reserve_out(&mut self) {
        if self.out.capacity() == 0 {
            self.out = take_buffer();
        }
    }

    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        mem::take(&mut self.events)
    }

    /// No more frames will be read: an error, or a GOAWAY with every
    /// stream finished.
    pub(crate) fn finished(&self) -> bool {
        self.failed || (self.goaway.is_some() && self.ids.is_empty() && self.block.is_none())
    }

    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// Too much output waits: stop reading.
    pub(crate) fn paused(&self) -> bool {
        self.out.len() >= OUT_PAUSE
    }

    pub(crate) fn open_streams(&self) -> usize {
        self.ids.len()
    }

    // ------------------------------------------------------------ input

    /// Handles every complete frame at the start of `input`; returns how
    /// many bytes that was. A connection error is answered here (GOAWAY)
    /// and ends the reading.
    pub(crate) fn receive<H: Handler>(&mut self, input: &[u8], handler: &H) -> usize {
        let mut at = 0;
        while !self.failed && !self.paused() {
            let Some(head) = input.get(at..at + frame::HEAD_LEN) else {
                break;
            };
            let head = Head::parse(head);
            if head.len > MAX_FRAME {
                self.fail(frame_size("a frame larger than SETTINGS_MAX_FRAME_SIZE"));
                break;
            }
            let end = at + frame::HEAD_LEN + head.len;
            let Some(payload) = input.get(at + frame::HEAD_LEN..end) else {
                break;
            };
            at = end;
            if let Err(e) = self.frame(head, payload, handler) {
                self.fail(e);
            }
        }
        self.reserve_out();
        self.grant();
        at
    }

    /// Ends the connection with a GOAWAY saying why.
    pub(crate) fn fail(&mut self, e: ConnError) {
        if self.failed {
            return;
        }
        self.failed = true;
        let debug = format!("{}: {}", e.code, e.detail);
        self.reserve_out();
        frame::write_goaway(&mut self.out, self.last_id, e.reason, debug.as_bytes());
        let slots: Vec<usize> = self.ids.iter().map(|&(_, slot)| slot).collect();
        for slot in slots {
            self.remove(slot, Failure::Cut, false);
        }
        self.block = None;
        self.queue.clear();
    }

    /// The client closed its side of the connection: no frame comes any
    /// more. Streams still waiting for their request are dropped; the
    /// others are answered, then the connection ends.
    pub(crate) fn on_eof(&mut self) {
        self.go_away();
        self.block = None;
        let waiting: Vec<usize> = self
            .ids
            .iter()
            .map(|&(_, slot)| slot)
            .filter(|&slot| {
                self.streams[slot]
                    .as_ref()
                    .is_some_and(|st| !st.remote_done)
            })
            .collect();
        for slot in waiting {
            self.remove(slot, Failure::Cut, true);
        }
    }

    /// Stops taking new streams: GOAWAY with the last one taken. The
    /// connection ends once the open streams are finished.
    pub(crate) fn go_away(&mut self) {
        if self.goaway.is_some() || self.failed {
            return;
        }
        self.goaway = Some(self.last_id);
        self.reserve_out();
        frame::write_goaway(&mut self.out, self.last_id, Reason::NoError, b"");
    }

    fn frame<H: Handler>(
        &mut self,
        head: Head,
        payload: &[u8],
        handler: &H,
    ) -> Result<(), ConnError> {
        if let Some(block) = &self.block {
            if head.kind != frame::CONTINUATION || head.stream != block.stream {
                return Err(protocol("a header block was interrupted by another frame"));
            }
        }
        if !self.settings_seen && (head.kind != frame::SETTINGS || head.has(frame::ACK)) {
            return Err(protocol("the client's first frame was not SETTINGS"));
        }
        match head.kind {
            frame::DATA => self.on_data(head, payload),
            frame::HEADERS => self.on_headers(head, payload, handler),
            frame::PRIORITY => self.on_priority(head, payload),
            frame::RST_STREAM => self.on_rst(head, payload),
            frame::SETTINGS => self.on_settings(head, payload),
            frame::PUSH_PROMISE => Err(protocol("a client sent PUSH_PROMISE")),
            frame::PING => self.on_ping(head, payload),
            frame::GOAWAY => self.on_goaway(head),
            frame::WINDOW_UPDATE => self.on_window_update(head, payload),
            frame::CONTINUATION => self.on_continuation(head, payload, handler),
            // Unknown types are ignored (RFC 9113, section 5.5).
            _ => self.cheap(),
        }
    }

    fn cheap(&mut self) -> Result<(), ConnError> {
        if self.cheap.take() {
            Ok(())
        } else {
            Err(ConnError {
                reason: Reason::EnhanceYourCalm,
                code: Code::Http2Flood,
                detail: "too many frames that do no work (PING, SETTINGS, PRIORITY, empty \
                         DATA or CONTINUATION, WINDOW_UPDATE with nothing to send, frames \
                         for closed streams, unknown types)",
            })
        }
    }

    fn reset_budget(&mut self) -> Result<(), ConnError> {
        if self.resets.take() {
            Ok(())
        } else {
            Err(ConnError {
                reason: Reason::EnhanceYourCalm,
                code: Code::Http2TooManyResets,
                detail: "too many streams cancelled or refused in a short time",
            })
        }
    }

    fn slot_of(&self, id: u32) -> Option<usize> {
        self.ids
            .binary_search_by_key(&id, |&(id, _)| id)
            .ok()
            .map(|i| self.ids[i].1)
    }

    fn stream(&mut self, slot: usize) -> Option<&mut Stream> {
        self.streams.get_mut(slot).and_then(Option::as_mut)
    }

    /// `Some(by_us)` for a stream closed recently.
    fn closed_by_us(&self, id: u32) -> Option<bool> {
        self.closed
            .iter()
            .rev()
            .find(|&&(closed, _)| closed == id)
            .map(|&(_, by_us)| by_us)
    }

    fn on_data(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream == 0 {
            return Err(protocol("DATA on stream 0"));
        }
        frame::unpad(&head, payload).map_err(|()| protocol("DATA with more padding than data"))?;
        // Flow control counts the whole payload, padding included.
        let len = payload.len();
        if len as i64 > self.conn_recv {
            return Err(flow_control(
                "DATA past the connection's flow-control window",
            ));
        }
        self.conn_recv -= len as i64;
        self.conn_unacked += len as u32;
        let data = frame::unpadded(&head, payload);
        let end = head.has(frame::END_STREAM);
        let Some(slot) = self.slot_of(head.stream) else {
            if head.stream > self.last_id {
                return Err(protocol("DATA on a stream that was never opened"));
            }
            if self.closed_by_us(head.stream) != Some(true) {
                self.reset_closed(head.stream, Reason::StreamClosed)?;
            }
            return Ok(());
        };
        let limit = self.cfg.max_body_size as u64;
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        if st.remote_done {
            return self.reset(slot, Reason::StreamClosed);
        }
        if len as i64 > st.recv_window {
            return self.reset(slot, Reason::FlowControl);
        }
        st.recv_window -= len as i64;
        // Padding is granted back at once; the data when it is dealt with.
        st.unacked += (len - data.len()) as u32;
        st.received += data.len() as u64;
        if st.content_length.is_some_and(|cl| st.received > cl) {
            return self.reset(slot, Reason::Protocol);
        }
        if data.is_empty() && !end {
            self.cheap()?;
        }
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        let over = match &st.recv {
            Recv::Buffer(_) => (st.received > limit).then_some(None),
            Recv::Pipe { limit, .. } => (st.received > *limit).then_some(Some(*limit)),
            Recv::Done => None,
        };
        match over {
            Some(None) => {
                // Too large to hold: answer now, without the handler.
                st.recv = Recv::Done;
                st.unacked += data.len() as u32;
                self.drop_held(slot);
                return self.answer_status(slot, 413);
            }
            Some(Some(limit)) => {
                st.unacked += data.len() as u32;
                self.end_pipe(slot, Err(Failure::TooLarge(limit)));
                return Ok(());
            }
            None => {}
        }
        match &mut st.recv {
            Recv::Buffer(req) => {
                if let Some(buf) = Rc::get_mut(&mut req.buf) {
                    buf.extend_from_slice(data);
                }
                st.unacked += data.len() as u32;
                st.held += data.len() as u64;
                self.held += data.len() as u64;
            }
            Recv::Pipe { pending, .. } => {
                pending.extend_from_slice(data);
                st.held += data.len() as u64;
                self.held += data.len() as u64;
            }
            Recv::Done => st.unacked += data.len() as u32,
        }
        if end {
            self.remote_end(slot)?;
        } else {
            self.service(slot);
        }
        Ok(())
    }

    /// END_STREAM arrived on the stream in `slot`.
    fn remote_end(&mut self, slot: usize) -> Result<(), ConnError> {
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        st.remote_done = true;
        st.deadline = None;
        if st.content_length.is_some_and(|cl| cl != st.received) {
            return self.reset(slot, Reason::Protocol);
        }
        match mem::replace(&mut st.recv, Recv::Done) {
            Recv::Buffer(mut req) => {
                let start = req.head.len;
                req.body = BodyBytes::InBuffer(start..req.buf.len());
                self.events.push(Event::Start(slot, req));
            }
            pipe @ Recv::Pipe { .. } => {
                st.recv = pipe;
                self.service(slot);
            }
            Recv::Done => {}
        }
        self.close_if_done(slot);
        Ok(())
    }

    fn on_headers<H: Handler>(
        &mut self,
        head: Head,
        payload: &[u8],
        handler: &H,
    ) -> Result<(), ConnError> {
        let id = head.stream;
        if id == 0 {
            return Err(protocol("HEADERS on stream 0"));
        }
        if id % 2 == 0 {
            return Err(protocol("HEADERS on an even-numbered stream"));
        }
        frame::unpad(&head, payload)
            .map_err(|()| protocol("HEADERS with more padding than data"))?;
        let mut fragment = frame::unpadded(&head, payload);
        let mut self_dependent = false;
        if head.has(frame::PRIORITY_FLAG) {
            if fragment.len() < 5 {
                return Err(frame_size("HEADERS too short for its priority fields"));
            }
            self_dependent = frame::dependency(fragment) == id;
            fragment = &fragment[5..];
        }
        let purpose = match self.slot_of(id) {
            Some(slot) => {
                let remote_done = self.stream(slot).is_some_and(|st| st.remote_done);
                if remote_done || self_dependent {
                    Purpose::Misplaced(slot)
                } else {
                    Purpose::Trailers(slot)
                }
            }
            None if id > self.last_id => {
                self.last_id = id;
                if self.goaway.is_some() {
                    Purpose::Ignore
                } else {
                    Purpose::Open {
                        refuse: self_dependent,
                    }
                }
            }
            None => match self.closed_by_us(id) {
                Some(true) => Purpose::Ignore,
                Some(false) => {
                    return Err(ConnError {
                        reason: Reason::StreamClosed,
                        code: Code::Http2Protocol,
                        detail: "HEADERS on a closed stream",
                    })
                }
                None => return Err(protocol("a stream identifier lower than one used before")),
            },
        };
        let end_stream = head.has(frame::END_STREAM);
        if head.has(frame::END_HEADERS) {
            return self.header_block(id, end_stream, purpose, fragment, handler);
        }
        if fragment.is_empty() {
            self.cheap()?;
        }
        self.check_block_size(fragment.len())?;
        self.block = Some(Block {
            stream: id,
            end_stream,
            purpose,
            fragment: fragment.to_vec(),
            deadline: Instant::now() + self.cfg.request_timeout,
        });
        Ok(())
    }

    fn check_block_size(&self, len: usize) -> Result<(), ConnError> {
        if len > self.cfg.max_head_size.max(16 * 1024) {
            return Err(ConnError {
                reason: Reason::EnhanceYourCalm,
                code: Code::Http2HeadersTooLarge,
                detail: "a header block larger than the server's max_head_size",
            });
        }
        Ok(())
    }

    fn on_continuation<H: Handler>(
        &mut self,
        head: Head,
        payload: &[u8],
        handler: &H,
    ) -> Result<(), ConnError> {
        let Some(block) = &mut self.block else {
            return Err(protocol("CONTINUATION without a header block to continue"));
        };
        block.fragment.extend_from_slice(payload);
        let len = block.fragment.len();
        if !head.has(frame::END_HEADERS) {
            if payload.is_empty() {
                self.cheap()?;
            }
            return self.check_block_size(len);
        }
        let block = self.block.take().expect("a header block");
        self.check_block_size(len)?;
        self.header_block(
            block.stream,
            block.end_stream,
            block.purpose,
            &block.fragment,
            handler,
        )
    }

    /// A complete header block.
    fn header_block<H: Handler>(
        &mut self,
        id: u32,
        end_stream: bool,
        purpose: Purpose,
        fragment: &[u8],
        handler: &H,
    ) -> Result<(), ConnError> {
        let compression = |_| ConnError {
            reason: Reason::Compression,
            code: Code::Http2Protocol,
            detail: "a header block HPACK cannot decode",
        };
        match purpose {
            Purpose::Ignore => {
                self.decoder
                    .decode(fragment, |_, _| {})
                    .map_err(compression)?;
                self.cheap()
            }
            Purpose::Misplaced(slot) => {
                self.decoder
                    .decode(fragment, |_, _| {})
                    .map_err(compression)?;
                let reason = if self.stream(slot).is_some_and(|st| st.remote_done) {
                    Reason::StreamClosed
                } else {
                    Reason::Protocol
                };
                self.reset(slot, reason)
            }
            Purpose::Trailers(slot) => {
                let mut trailers = Trailers::new();
                self.decoder
                    .decode(fragment, |n, v| trailers.field(n, v))
                    .map_err(compression)?;
                if trailers.error.is_some() || !end_stream {
                    return self.reset(slot, Reason::Protocol);
                }
                self.remote_end(slot)
            }
            Purpose::Open { refuse } => {
                let mut builder = Builder::new(self.cfg.max_head_size);
                self.decoder
                    .decode(fragment, |n, v| builder.field(n, v))
                    .map_err(compression)?;
                if refuse {
                    return self.reset_closed(id, Reason::Protocol);
                }
                if self.ids.len() >= self.cfg.max_concurrent_streams as usize {
                    return self.reset_closed(id, Reason::RefusedStream);
                }
                match builder.finish(self.peer) {
                    Built::Malformed(_) => self.reset_closed(id, Reason::Protocol),
                    Built::Status(status) => {
                        let slot = self.open(id, end_stream, None, false);
                        self.answer_status(slot, status)
                    }
                    Built::Request(req) => {
                        self.open_request(id, end_stream, req, handler);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Adds a stream.
    fn open(
        &mut self,
        id: u32,
        end_stream: bool,
        content_length: Option<u64>,
        head_only: bool,
    ) -> usize {
        let stream = Stream {
            id,
            remote_done: end_stream,
            recv_window: i64::from(INITIAL_WINDOW),
            // A stream with a body is owed the rest of a full window, which
            // `grant` gives when the budget allows.
            unacked: if end_stream {
                0
            } else {
                STREAM_WINDOW - INITIAL_WINDOW
            },
            content_length,
            received: 0,
            held: 0,
            recv: Recv::Done,
            send_window: self.peer_window,
            send: Send::Waiting,
            head_only,
            expect_continue: false,
            continue_sent: false,
            queued: false,
            deadline: None,
            blocked_since: None,
        };
        let slot = match self.free.pop() {
            Some(slot) => {
                self.streams[slot] = Some(stream);
                slot
            }
            None => {
                self.streams.push(Some(stream));
                self.streams.len() - 1
            }
        };
        // Ids only grow, so the list stays sorted.
        self.ids.push((id, slot));
        slot
    }

    fn open_request<H: Handler>(
        &mut self,
        id: u32,
        end_stream: bool,
        mut req: Request,
        handler: &H,
    ) {
        let head_only = req.method() == "HEAD";
        let content_length = req.head.content_length;
        let expect_continue = req.head.expect_continue;
        let slot = self.open(id, end_stream, content_length, head_only);
        let request_timeout = self.cfg.request_timeout;
        let max_body = self.cfg.max_body_size as u64;
        if end_stream {
            if content_length.is_some_and(|cl| cl != 0) {
                let _ = self.reset(slot, Reason::Protocol);
                return;
            }
            self.events.push(Event::Start(slot, req));
            return;
        }
        match handler.stream_body(&req) {
            Some(limit) => {
                if content_length.is_some_and(|cl| cl > limit) {
                    let _ = self.answer_status(slot, 413);
                    return;
                }
                let pipe = Rc::new(Pipe::new(content_length));
                req.body = BodyBytes::Stream(pipe.clone());
                let st = self.stream(slot).expect("just opened");
                st.recv = Recv::Pipe {
                    pipe,
                    pending: Vec::new(),
                    limit,
                    taken: 0,
                };
                st.expect_continue = expect_continue;
                self.events.push(Event::Start(slot, req));
            }
            None => {
                if content_length.is_some_and(|cl| cl > max_body) {
                    let _ = self.answer_status(slot, 413);
                    return;
                }
                let st = self.stream(slot).expect("just opened");
                st.recv = Recv::Buffer(req);
                st.deadline = Some(Instant::now() + request_timeout);
                if expect_continue {
                    st.continue_sent = true;
                    self.write_status(id, 100, false);
                }
            }
        }
    }

    fn on_priority(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream == 0 {
            return Err(protocol("PRIORITY on stream 0"));
        }
        if payload.len() != 5 {
            return Err(frame_size("PRIORITY with a length other than 5"));
        }
        self.cheap()?;
        if frame::dependency(payload) == head.stream {
            return match self.slot_of(head.stream) {
                Some(slot) => self.reset(slot, Reason::Protocol),
                None if head.stream > self.last_id => {
                    // Never opened: nothing to reset, but the stream is
                    // closed from now on.
                    self.last_id = head.stream;
                    self.reset_closed(head.stream, Reason::Protocol)
                }
                None => Ok(()),
            };
        }
        // Priorities are advice; giong sends streams in turn.
        Ok(())
    }

    fn on_rst(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if payload.len() != 4 {
            return Err(frame_size("RST_STREAM with a length other than 4"));
        }
        if head.stream == 0 {
            return Err(protocol("RST_STREAM on stream 0"));
        }
        match self.slot_of(head.stream) {
            Some(slot) => {
                let answered = self
                    .stream(slot)
                    .is_some_and(|st| matches!(st.send, Send::Done));
                self.remove(slot, Failure::Reset, false);
                if !answered {
                    // A stream cancelled before its answer: the rapid reset.
                    self.reset_budget()?;
                }
                Ok(())
            }
            None if head.stream > self.last_id => {
                Err(protocol("RST_STREAM on a stream that was never opened"))
            }
            None => self.cheap(),
        }
    }

    fn on_settings(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("SETTINGS on a stream other than 0"));
        }
        if head.has(frame::ACK) {
            if !payload.is_empty() {
                return Err(frame_size("SETTINGS acknowledgement with a payload"));
            }
            return self.cheap();
        }
        if payload.len() % 6 != 0 {
            return Err(frame_size(
                "SETTINGS with a length that is not a multiple of 6",
            ));
        }
        if self.settings_seen {
            self.cheap()?;
        }
        self.settings_seen = true;
        for setting in payload.chunks_exact(6) {
            let id = u16::from_be_bytes([setting[0], setting[1]]);
            let value = frame::read_u32(&setting[2..]);
            match id {
                frame::HEADER_TABLE_SIZE => self.encoder.set_limit(value as usize),
                frame::ENABLE_PUSH if value > 1 => {
                    return Err(protocol("SETTINGS_ENABLE_PUSH other than 0 or 1"))
                }
                frame::INITIAL_WINDOW_SIZE => {
                    let value = i64::from(value);
                    if value > frame::MAX_WINDOW {
                        return Err(flow_control("SETTINGS_INITIAL_WINDOW_SIZE above 2^31-1"));
                    }
                    let delta = value - self.peer_window;
                    self.peer_window = value;
                    let slots: Vec<usize> = self.ids.iter().map(|&(_, slot)| slot).collect();
                    for slot in slots {
                        let Some(st) = self.stream(slot) else {
                            continue;
                        };
                        st.send_window += delta;
                        if st.send_window > frame::MAX_WINDOW {
                            return Err(flow_control("a stream window grew above 2^31-1"));
                        }
                        self.enqueue(slot);
                    }
                }
                frame::MAX_FRAME_SIZE => {
                    if !(frame::DEFAULT_FRAME_SIZE as u32..=frame::MAX_FRAME_SIZE_LIMIT)
                        .contains(&value)
                    {
                        return Err(protocol("SETTINGS_MAX_FRAME_SIZE out of range"));
                    }
                    self.peer_frame = value as usize;
                }
                // Advisory, or not for a server; unknown ones are ignored.
                _ => {}
            }
        }
        self.reserve_out();
        frame::write_settings_ack(&mut self.out);
        Ok(())
    }

    fn on_ping(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("PING on a stream other than 0"));
        }
        if payload.len() != 8 {
            return Err(frame_size("PING with a length other than 8"));
        }
        self.cheap()?;
        if head.has(frame::ACK) {
            return Ok(());
        }
        self.reserve_out();
        frame::write_ping(&mut self.out, true, payload);
        Ok(())
    }

    fn on_goaway(&mut self, head: Head) -> Result<(), ConnError> {
        if head.stream != 0 {
            return Err(protocol("GOAWAY on a stream other than 0"));
        }
        if head.len < 8 {
            return Err(frame_size("GOAWAY shorter than 8 bytes"));
        }
        // The client opens no more streams; those open are finished.
        self.go_away();
        Ok(())
    }

    fn on_window_update(&mut self, head: Head, payload: &[u8]) -> Result<(), ConnError> {
        if payload.len() != 4 {
            return Err(frame_size("WINDOW_UPDATE with a length other than 4"));
        }
        let increment = i64::from(frame::read_u32(payload) & 0x7fff_ffff);
        if head.stream == 0 {
            if increment == 0 {
                return Err(protocol("WINDOW_UPDATE of 0 on the connection"));
            }
            self.conn_send += increment;
            if self.conn_send > frame::MAX_WINDOW {
                return Err(flow_control("the connection window grew above 2^31-1"));
            }
            // Streams the connection window holds back stay in the queue,
            // so there is nothing to wake; with none, the frame did nothing.
            if self.queue.is_empty() {
                self.cheap()?;
            }
            return Ok(());
        }
        let Some(slot) = self.slot_of(head.stream) else {
            if head.stream > self.last_id {
                return Err(protocol("WINDOW_UPDATE on a stream that was never opened"));
            }
            return self.cheap();
        };
        if increment == 0 {
            return self.reset(slot, Reason::Protocol);
        }
        if !self.stream(slot).is_some_and(|st| st.has_data()) {
            self.cheap()?;
        }
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        st.send_window += increment;
        if st.send_window > frame::MAX_WINDOW {
            return self.reset(slot, Reason::FlowControl);
        }
        self.enqueue(slot);
        Ok(())
    }

    // ---------------------------------------------------------- streams

    /// Resets an open stream: a stream error.
    fn reset(&mut self, slot: usize, reason: Reason) -> Result<(), ConnError> {
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        let id = st.id;
        self.reserve_out();
        frame::write_rst(&mut self.out, id, reason);
        // What a handler still reading the body is told.
        let failure = match reason {
            // The client stopped reading the response.
            Reason::Cancel => Failure::Cut,
            // The response broke off on our side.
            Reason::Internal => Failure::Abandoned,
            // The client broke a rule of the stream.
            _ => Failure::Malformed,
        };
        self.remove(slot, failure, true);
        self.reset_budget()
    }

    /// Resets a stream that has no slot (refused, or closed already).
    fn reset_closed(&mut self, id: u32, reason: Reason) -> Result<(), ConnError> {
        self.reserve_out();
        frame::write_rst(&mut self.out, id, reason);
        self.remember_closed(id, true);
        self.reset_budget()
    }

    fn remember_closed(&mut self, id: u32, by_us: bool) {
        if self.closed.len() == CLOSED_MEMORY {
            self.closed.pop_front();
        }
        self.closed.push_back((id, by_us));
    }

    /// Drops a stream and its handler. A body still being read ends with
    /// `failure`.
    fn remove(&mut self, slot: usize, failure: Failure, by_us: bool) {
        let Some(st) = self.streams.get_mut(slot).and_then(Option::take) else {
            return;
        };
        self.held -= st.held;
        self.grant_due |= st.held > 0;
        if let Recv::Pipe { pipe, .. } = &st.recv {
            pipe.finish(Err(failure));
        }
        if let Ok(i) = self.ids.binary_search_by_key(&st.id, |&(id, _)| id) {
            self.ids.remove(i);
        }
        self.free.push(slot);
        self.events.push(Event::Stop(slot));
        self.remember_closed(st.id, by_us);
        if self.ids.is_empty() {
            self.idle_since = Instant::now();
        }
    }

    /// Closes the stream if both sides are done. One whose response went
    /// out before its request ended is told to stop sending.
    fn close_if_done(&mut self, slot: usize) {
        let Some(st) = self.stream(slot) else {
            return;
        };
        if !matches!(st.send, Send::Done) {
            return;
        }
        if st.remote_done {
            self.remove(slot, Failure::Abandoned, false);
        } else {
            let id = st.id;
            self.reserve_out();
            frame::write_rst(&mut self.out, id, Reason::NoError);
            self.remove(slot, Failure::Abandoned, true);
        }
    }

    fn end_pipe(&mut self, slot: usize, end: Result<(), Failure>) {
        if let Some(st) = self.stream(slot) {
            if let Recv::Pipe { pipe, .. } = mem::replace(&mut st.recv, Recv::Done) {
                pipe.finish(end);
            }
            st.deadline = None;
        }
        self.drop_held(slot);
    }

    /// The body bytes of the stream in `slot` are no longer held.
    fn drop_held(&mut self, slot: usize) {
        if let Some(st) = self.stream(slot) {
            let held = mem::take(&mut st.held);
            self.held -= held;
            self.grant_due |= held > 0;
        }
    }

    fn enqueue(&mut self, slot: usize) {
        let Some(st) = self.stream(slot) else {
            return;
        };
        if !st.queued && st.sendable() {
            st.queued = true;
            self.queue.push_back(slot);
        } else if st.has_data() && st.send_window <= 0 {
            // Held back by the client's flow control from the start.
            st.blocked_since.get_or_insert_with(Instant::now);
        }
    }

    /// Writes a response that is only a status, without calling the
    /// handler (413, 431, 417, 408).
    fn answer_status(&mut self, slot: usize, status: u16) -> Result<(), ConnError> {
        let Some(st) = self.stream(slot) else {
            return Ok(());
        };
        if !matches!(st.send, Send::Waiting) {
            return Ok(());
        }
        st.send = Send::Done;
        st.deadline = None;
        let id = st.id;
        self.write_status(id, status, true);
        self.end_pipe(slot, Err(Failure::Abandoned));
        self.close_if_done(slot);
        Ok(())
    }

    fn write_status(&mut self, id: u32, status: u16, end_stream: bool) {
        let mut block = take_buffer();
        self.encoder.begin(&mut block);
        self.encoder.status(status, &mut block);
        if status >= 200 {
            self.encoder
                .field(b"date", &crate::date::now(), Index::Yes, &mut block);
            if end_stream && status != 204 && status != 304 {
                self.encoder
                    .field(b"content-length", b"0", Index::Yes, &mut block);
            }
        }
        self.reserve_out();
        frame::write_headers(&mut self.out, id, &block, end_stream, self.peer_frame);
        give_buffer(block);
    }

    /// The handler of the stream in `slot` answered.
    pub(crate) fn respond(&mut self, slot: usize, mut resp: Response) {
        let Some(st) = self.stream(slot) else {
            return;
        };
        if !matches!(st.send, Send::Waiting) {
            return;
        }
        let (id, head_only) = (st.id, st.head_only);
        let status = resp.status();
        let bodiless = matches!(status, 204 | 304);
        let body = resp.take_body();
        let streaming = body.is_stream();
        let end_stream = head_only || bodiless || (!streaming && body.is_empty());

        let mut block = take_buffer();
        self.encoder.begin(&mut block);
        self.encoder.status(status, &mut block);
        self.encoder
            .field(b"date", &crate::date::now(), Index::Yes, &mut block);
        if !bodiless && !streaming {
            let mut digits = itoa(body.len());
            self.encoder
                .field(b"content-length", digits.as_bytes(), Index::No, &mut block);
            digits.clear();
        }
        let mut name = mem::take(&mut self.name);
        for (n, v) in resp.headers() {
            name.clear();
            name.extend(n.bytes().map(|b| b.to_ascii_lowercase()));
            if matches!(
                &name[..],
                b"keep-alive" | b"proxy-connection" | b"upgrade" | b"te"
            ) {
                continue;
            }
            let index = if name == b"set-cookie" {
                Index::Never
            } else if v.len() > 256 {
                Index::No
            } else {
                Index::Yes
            };
            self.encoder.field(&name, v.as_bytes(), index, &mut block);
        }
        self.name = name;
        self.reserve_out();
        frame::write_headers(&mut self.out, id, &block, end_stream, self.peer_frame);
        give_buffer(block);

        let Some(st) = self.stream(slot) else {
            return;
        };
        if end_stream {
            // A streamed body that is not sent is dropped here, which tells
            // its sender.
            drop(body);
            st.send = Send::Done;
            self.close_if_done(slot);
            return;
        }
        st.send = match body {
            Body::Stream(body) => Send::Stream {
                body,
                piece: Vec::new(),
                at: 0,
            },
            body => Send::Bytes { body, at: 0 },
        };
        self.service(slot);
        self.enqueue(slot);
    }

    /// Moves what can move on the stream in `slot` without the handler:
    /// request body pieces into its pipe, response pieces out of its body.
    /// Called when the slot's waker fires.
    pub(crate) fn service(&mut self, slot: usize) {
        let waker = self.wakers.get(slot).clone();
        let request_timeout = self.cfg.request_timeout;
        let mut interim = None;
        let mut ended = false;
        let mut aborted = false;
        let mut released = 0;
        let Some(st) = self.stream(slot) else {
            return;
        };
        if let Recv::Pipe {
            pipe,
            pending,
            taken,
            ..
        } = &mut st.recv
        {
            if st.expect_continue && !st.continue_sent && pipe.poll_asked(&waker) {
                st.continue_sent = true;
                interim = Some(st.id);
            }
            if !pending.is_empty() {
                if let Err(back) = pipe.offer(mem::take(pending), &waker) {
                    *pending = back;
                }
            }
            let now_taken = pipe.poll_taken(&waker);
            let newly = now_taken - *taken;
            st.unacked += newly as u32;
            released = newly.min(st.held);
            st.held -= released;
            *taken = now_taken;
            if st.remote_done && pending.is_empty() {
                pipe.finish(Ok(()));
                st.recv = Recv::Done;
                st.deadline = None;
            } else if pending.is_empty() && !st.remote_done {
                // The handler waits for the client.
                if st.deadline.is_none() {
                    st.deadline = Some(Instant::now() + request_timeout);
                }
            } else {
                st.deadline = None;
            }
        }
        if let Send::Stream { body, piece, at } = &mut st.send {
            if *at >= piece.len() {
                match body.poll_next(&waker) {
                    Poll::Ready(Next::Piece(next)) => {
                        *piece = next;
                        *at = 0;
                    }
                    Poll::Ready(Next::End) => ended = true,
                    Poll::Ready(Next::Aborted) => aborted = true,
                    Poll::Pending => {}
                }
            }
        }
        if ended {
            st.send = Send::End;
        }
        self.held -= released;
        if released > 0 {
            self.grant_due = true;
        }
        if let Some(id) = interim {
            self.write_status(id, 100, false);
        }
        if aborted {
            // Without END_STREAM, so the client sees an incomplete body.
            let _ = self.reset(slot, Reason::Internal);
            return;
        }
        self.enqueue(slot);
    }

    /// Grants back flow-control window for data dealt with.
    ///
    /// The connection window comes back as soon as the data is received;
    /// what bounds the bodies held is the windows of the streams. A stream
    /// gets more only while the bodies held, and the windows open for more
    /// of them, stay within the budget; except the oldest stream still
    /// receiving, so that one body always makes progress.
    fn grant(&mut self) {
        self.grant_due = false;
        if self.failed {
            return;
        }
        if self.conn_unacked >= CONN_WINDOW / 4 {
            frame::write_window_update(&mut self.out, 0, self.conn_unacked);
            self.conn_recv += i64::from(self.conn_unacked);
            self.conn_unacked = 0;
        }
        let max_body = self.cfg.max_body_size as u64;
        let receiving = |st: &Stream| !st.remote_done && !matches!(st.recv, Recv::Done);
        let mut committed = self.held;
        for &(_, slot) in &self.ids {
            if let Some(st) = self.streams[slot].as_ref().filter(|st| receiving(st)) {
                committed += st.recv_window.max(0) as u64;
            }
        }
        let mut oldest = true;
        for &(id, slot) in &self.ids {
            let Some(st) = self.streams[slot].as_mut() else {
                continue;
            };
            if st.remote_done {
                continue;
            }
            let holds = receiving(st);
            let first = holds && oldest;
            if holds {
                oldest = false;
            }
            if st.unacked == 0 {
                continue;
            }
            // A body received in full before the handler runs gets no more
            // than its limit.
            if matches!(st.recv, Recv::Buffer(_)) && st.received >= max_body {
                continue;
            }
            if st.unacked < STREAM_WINDOW / 4 && st.recv_window >= i64::from(STREAM_WINDOW / 2) {
                continue;
            }
            let more = u64::from(st.unacked);
            if holds && !first && committed + more > self.body_budget {
                continue;
            }
            frame::write_window_update(&mut self.out, id, st.unacked);
            st.recv_window += i64::from(st.unacked);
            st.unacked = 0;
            if holds {
                committed += more;
            }
        }
    }

    /// Adds DATA frames to the output, a frame per stream in turn, until
    /// the output is full or no stream can send.
    pub(crate) fn fill(&mut self) {
        if self.grant_due {
            self.reserve_out();
            self.grant();
        }
        let now = Instant::now();
        while self.out.len() < OUT_HIGH && !self.failed {
            let Some(&slot) = self.queue.front() else {
                break;
            };
            let conn_send = self.conn_send;
            let peer_frame = self.peer_frame;
            let Some(st) = self.streams[slot].as_mut() else {
                self.queue.pop_front();
                continue;
            };
            let needs_window = st.has_data();
            if needs_window && conn_send <= 0 {
                // Everything waits for the connection window.
                for &slot in &self.queue {
                    if let Some(st) = self.streams[slot].as_mut().filter(|st| st.has_data()) {
                        st.blocked_since.get_or_insert(now);
                    }
                }
                break;
            }
            self.queue.pop_front();
            st.queued = false;
            let window = st.send_window.min(conn_send).max(0) as usize;
            let limit = DATA_FRAME.min(peer_frame).min(window);
            let id = st.id;
            let mut sent = 0;
            let mut end = false;
            match &mut st.send {
                Send::Bytes { body, at } => {
                    let bytes = body.as_bytes();
                    let n = (bytes.len() - *at).min(limit);
                    end = *at + n == bytes.len();
                    if n > 0 || end {
                        let flags = if end { frame::END_STREAM } else { 0 };
                        frame::write_head(&mut self.out, n, frame::DATA, flags, id);
                        self.out.extend_from_slice(&bytes[*at..*at + n]);
                        *at += n;
                        sent = n;
                    }
                }
                Send::Stream { piece, at, .. } => {
                    let n = (piece.len() - *at).min(limit);
                    if n > 0 {
                        frame::write_head(&mut self.out, n, frame::DATA, 0, id);
                        self.out.extend_from_slice(&piece[*at..*at + n]);
                        *at += n;
                        sent = n;
                    }
                }
                Send::End => {
                    frame::write_head(&mut self.out, 0, frame::DATA, frame::END_STREAM, id);
                    end = true;
                }
                Send::Waiting | Send::Done => {}
            }
            st.send_window -= sent as i64;
            self.conn_send -= sent as i64;
            if sent > 0 {
                st.blocked_since = None;
            }
            if end {
                st.send = Send::Done;
                self.close_if_done(slot);
                continue;
            }
            if st.has_data() {
                if st.send_window > 0 {
                    st.queued = true;
                    self.queue.push_back(slot);
                } else {
                    st.blocked_since.get_or_insert(now);
                }
            } else if matches!(st.send, Send::Stream { .. }) {
                // The piece is out: ask the producer for the next one.
                self.service(slot);
            }
        }
    }

    // ------------------------------------------------------------- time

    /// The next moment something times out.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut at = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        if let Some(block) = &self.block {
            at(block.deadline);
        }
        if self.ids.is_empty() && self.goaway.is_none() {
            at(self.idle_since + self.cfg.idle_timeout);
        }
        for &(_, slot) in &self.ids {
            if let Some(st) = &self.streams[slot] {
                if let Some(deadline) = st.deadline {
                    at(deadline);
                }
                if let Some(since) = st.blocked_since {
                    at(since + self.cfg.write_timeout);
                }
            }
        }
        next
    }

    /// Handles what timed out by `now`.
    pub(crate) fn on_timer(&mut self, now: Instant) {
        if self.block.as_ref().is_some_and(|b| b.deadline <= now) {
            self.fail(protocol(
                "a header block was not finished within the request timeout",
            ));
            return;
        }
        if self.ids.is_empty()
            && self.goaway.is_none()
            && self.idle_since + self.cfg.idle_timeout <= now
        {
            self.go_away();
            return;
        }
        let write_timeout = self.cfg.write_timeout;
        let slots: Vec<usize> = self.ids.iter().map(|&(_, slot)| slot).collect();
        for slot in slots {
            let Some(st) = self.stream(slot) else {
                continue;
            };
            if st
                .blocked_since
                .is_some_and(|since| since + write_timeout <= now)
            {
                // The client's flow control has held the response back
                // for too long.
                let _ = self.reset(slot, Reason::Cancel);
                continue;
            }
            let Some(st) = self.stream(slot) else {
                continue;
            };
            if !st.deadline.is_some_and(|deadline| deadline <= now) {
                continue;
            }
            st.deadline = None;
            match st.recv {
                Recv::Buffer(_) => {
                    st.recv = Recv::Done;
                    self.drop_held(slot);
                    let _ = self.answer_status(slot, 408);
                }
                Recv::Pipe { .. } => self.end_pipe(slot, Err(Failure::Cut)),
                Recv::Done => {}
            }
        }
    }

    /// Bytes of state this connection keeps between requests: for the
    /// memory measurements.
    #[allow(dead_code)]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.decoder.heap_bytes()
            + self.encoder.heap_bytes()
            + self.out.capacity()
            + self.streams.capacity() * mem::size_of::<Option<Stream>>()
    }

    /// With no stream open: gives back the output buffer if it is empty,
    /// and the memory kept for streams, so that an idle connection holds
    /// little more than its HPACK tables.
    pub(crate) fn release(&mut self) {
        if self.out.is_empty() && self.out.capacity() > 0 {
            give_buffer(mem::take(&mut self.out));
        }
        if self.ids.is_empty() && self.events.is_empty() && self.streams.capacity() > 0 {
            self.streams = Vec::new();
            self.free = Vec::new();
            self.ids = Vec::new();
            self.queue = VecDeque::new();
            self.events = Vec::new();
            self.name = Vec::new();
            self.wakers.release();
        }
    }
}

fn itoa(n: usize) -> String {
    n.to_string()
}

/// What woke the connection.
enum Wake<R, W> {
    Read(R),
    Wrote(W),
    Streams,
    Shutdown,
    Timer,
}

/// Serves HTTP/2 on `io` until the connection ends. `input` holds bytes
/// already received, which may include the client's preface.
pub(crate) async fn serve<T: Transport, H: Handler>(
    io: &T,
    cfg: Config,
    peer: Option<SocketAddr>,
    handler: &H,
    mut input: Vec<u8>,
) {
    let mut s = Session::new(cfg, peer);
    s.start();

    // The client's preface, within the request timeout.
    let preface_deadline = Instant::now() + cfg.request_timeout;
    while input.len() < PREFACE.len() {
        if !PREFACE.starts_with(&input) {
            break;
        }
        let left = preface_deadline.saturating_duration_since(Instant::now());
        match giong::time::timeout(left, io.recv()).await {
            Ok(Ok(chunk)) if !chunk.is_empty() => input.extend_from_slice(&chunk),
            _ => return,
        }
    }
    if !input.starts_with(PREFACE) {
        s.fail(protocol(
            "the connection did not start with the HTTP/2 preface",
        ));
        let out = mem::take(&mut s.out);
        let _ = giong::time::timeout(cfg.write_timeout, io.write_all(out)).await;
        return;
    }
    input.drain(..PREFACE.len());

    let mut tasks: Vec<Option<HeapFuture<_>>> = Vec::new();
    let mut polls: Vec<usize> = Vec::new();
    let mut recv = pin!(Some(io.recv()));
    let mut write = pin!(None);
    let mut write_deadline: Option<Instant> = None;
    let mut timer = pin!(sleep_until(Instant::now() + cfg.idle_timeout));
    let mut shutdown = pin!(giong::until_shutdown(std::future::pending::<()>()));
    let mut shutting_down = false;
    if false {
        // Names the type of the write future for `write`.
        write.set(Some(io.write_all(Vec::new())));
    }

    loop {
        // Frames received.
        if !input.is_empty() && !s.failed() && !s.paused() {
            let used = s.receive(&input, handler);
            input.drain(..used);
            if input.is_empty() && input.capacity() > 0 {
                give_buffer(mem::take(&mut input));
            }
        }
        // Handlers to start and stop; then poll the new ones and the woken.
        for event in s.take_events() {
            match event {
                Event::Start(slot, req) => {
                    if tasks.len() <= slot {
                        tasks.resize_with(slot + 1, || None);
                    }
                    tasks[slot] = Some(HeapFuture::new(handler.call(req)));
                    polls.push(slot);
                }
                Event::Stop(slot) => {
                    if let Some(task) = tasks.get_mut(slot) {
                        *task = None;
                    }
                }
            }
        }
        polls.sort_unstable();
        polls.dedup();
        for slot in mem::take(&mut polls) {
            if let Some(Some(task)) = tasks.get_mut(slot) {
                let waker = s.wakers.get(slot).clone();
                let mut cx = Context::from_waker(&waker);
                if let Poll::Ready(resp) = Pin::new(task).poll(&mut cx) {
                    tasks[slot] = None;
                    s.respond(slot, resp);
                }
            }
            s.service(slot);
            for event in s.take_events() {
                if let Event::Stop(slot) = event {
                    if let Some(task) = tasks.get_mut(slot) {
                        *task = None;
                    }
                }
            }
        }
        s.fill();
        for event in s.take_events() {
            if let Event::Stop(slot) = event {
                if let Some(task) = tasks.get_mut(slot) {
                    *task = None;
                }
            }
        }

        if write.is_none() && !s.out.is_empty() {
            let out = mem::replace(&mut s.out, take_buffer());
            write.set(Some(io.write_all(out)));
            write_deadline = Some(Instant::now() + cfg.write_timeout);
        }
        if s.finished() && write.is_none() {
            break;
        }
        if write.is_none() && s.open_streams() == 0 {
            // Idle: hand the buffers back.
            s.release();
            if tasks.capacity() > 0 {
                tasks = Vec::new();
            }
        }

        // Streams woken while the others were polled: let the rest of the
        // worker run first, then poll them.
        if s.wakers.drain(&mut polls) {
            giong::yield_now().await;
            continue;
        }

        let deadline = [s.next_deadline(), write_deadline]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        if timer.deadline() != deadline {
            timer.as_mut().get_mut().reset(deadline);
        }
        let reading = !s.failed() && !s.paused() && !s.finished();
        let woke = poll_fn(|cx| {
            if let Some(w) = write.as_mut().as_pin_mut() {
                if let Poll::Ready(done) = w.poll(cx) {
                    return Poll::Ready(Wake::Wrote(done));
                }
            }
            if reading {
                if let Some(r) = recv.as_mut().as_pin_mut() {
                    if let Poll::Ready(got) = r.poll(cx) {
                        return Poll::Ready(Wake::Read(got));
                    }
                }
            }
            if s.wakers.take(cx.waker(), &mut polls) {
                return Poll::Ready(Wake::Streams);
            }
            if !shutting_down && shutdown.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Shutdown);
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Wake::Timer);
            }
            Poll::Pending
        })
        .await;
        match woke {
            Wake::Wrote((Ok(()), mut buf)) => {
                write.set(None);
                write_deadline = None;
                buf.clear();
                if s.out.is_empty() {
                    give_buffer(mem::replace(&mut s.out, buf));
                } else {
                    give_buffer(buf);
                }
            }
            Wake::Wrote((Err(_), _)) => return,
            Wake::Read(Ok(chunk)) if !chunk.is_empty() => {
                if input.is_empty() && !s.failed() && !s.paused() {
                    // Frames straight from the received buffer; only a
                    // partial frame at its end is copied.
                    let used = s.receive(&chunk, handler);
                    if used < chunk.len() {
                        if input.capacity() == 0 {
                            input = take_buffer();
                        }
                        input.extend_from_slice(&chunk[used..]);
                    }
                } else {
                    if input.capacity() == 0 {
                        input = take_buffer();
                    }
                    input.extend_from_slice(&chunk);
                }
                drop(chunk);
                recv.set(Some(io.recv()));
            }
            // The client closed its side: answer what was asked.
            Wake::Read(Ok(_)) => {
                recv.set(None);
                s.on_eof();
            }
            // The connection broke.
            Wake::Read(Err(_)) => return,
            Wake::Streams => {}
            Wake::Shutdown => {
                shutting_down = true;
                s.go_away();
            }
            Wake::Timer => {
                let now = Instant::now();
                if write_deadline.is_some_and(|d| d <= now) {
                    // The client does not read.
                    return;
                }
                s.on_timer(now);
            }
        }
    }

    // Every stream is finished, or the connection failed: the GOAWAY is
    // out. Close our side, and read what is still on its way for a moment
    // so that the client does not get a reset instead of the GOAWAY.
    drop(tasks);
    recv.set(None);
    let _ = giong::time::timeout(cfg.write_timeout, io.close_write()).await;
    let deadline = Instant::now() + ERROR_LINGER;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match giong::time::timeout(left, io.recv()).await {
            Ok(Ok(chunk)) if !chunk.is_empty() => {}
            _ => return,
        }
    }
}
