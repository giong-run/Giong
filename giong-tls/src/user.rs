//! rustls in the process, through its unbuffered API, so that giong-tls owns
//! every buffer a connection uses.
//!
//! rustls' buffered `Connection` keeps a 4 KiB receive buffer for the life
//! of the connection, idle or not; it is most of what an idle HTTPS
//! connection costs. Here received bytes are processed from a per-thread
//! scratch buffer, and a connection keeps a buffer of its own only while it
//! holds part of a record: an idle connection holds none.
//!
//! [`User`] offers what the stream needs of a buffered connection: records
//! to send ([`wants_write`](User::wants_write), [`write_tls`](User::write_tls)),
//! received bytes in and plaintext out ([`read_tls`](User::read_tls)),
//! plaintext in and records out ([`encrypt`](User::encrypt)), and
//! `close_notify`.

use std::cell::RefCell;
use std::mem;

use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::CertificateDer;
use rustls::server::UnbufferedServerConnection;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{HandshakeKind, ProtocolVersion};

/// The most a connection holds of records rustls has not used up yet: a
/// handshake message of up to 64 KiB spread over records (what rustls'
/// buffered connection allows), and the records around it.
pub(crate) const MAX_PARTIAL: usize = (64 << 10) + 2 * MAX_RECORD;

/// The largest TLS record on the wire: a header, 16 KiB of plaintext and
/// what encryption adds (RFC 8446 section 5.2 allows 256 bytes, TLS 1.2
/// 2048).
const MAX_RECORD: usize = 5 + (16 << 10) + 2048;

/// Room made in the output buffer for each record of plaintext encrypted:
/// a header, a tag, the TLS 1.3 content type or the TLS 1.2 explicit nonce.
/// rustls says how much it needs if this is too little.
const RECORD_OVERHEAD: usize = 64;

/// The largest scratch buffer a thread keeps between receives.
const SCRATCH_KEPT: usize = 64 << 10;

thread_local! {
    /// Received bytes being processed: rustls decrypts records in place, so
    /// they need a buffer it may write to. Shared by the thread's
    /// connections, since processing never waits.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Why [`User::encrypt`] refused data.
pub(crate) enum Refused {
    /// The key may encrypt no more records and cannot be changed;
    /// `close_notify` is queued when rustls sends one.
    Exhausted,
    /// The connection cannot send application data (handshake not over, or
    /// `close_notify` sent).
    Closed,
    Tls(rustls::Error),
}

enum Side {
    Server(UnbufferedServerConnection),
    Client(UnbufferedClientConnection),
}

/// What [`User::process`] stopped at.
enum Stop {
    /// Every complete record has been processed; more input is needed to go
    /// on, and application data may be sent if the handshake allows it.
    Idle,
    /// Application data arrived before the handshake ended (0-RTT).
    EarlyData,
}

pub(crate) struct User {
    side: Side,
    /// Records rustls has emitted that wait to be sent: handshake messages,
    /// session tickets, key updates, alerts. No capacity when empty.
    queued: Vec<u8>,
    /// Received bytes rustls has not used up: part of a record, or records
    /// of a handshake message whose rest has not arrived. No capacity when
    /// empty.
    partial: Vec<u8>,
    /// rustls has handed over the peer's `close_notify`.
    peer_closed: bool,
    /// The error that ended the connection: every later call returns it.
    failed: Option<rustls::Error>,
}

impl User {
    pub(crate) fn server(conn: UnbufferedServerConnection) -> Result<Self, rustls::Error> {
        Self::new(Side::Server(conn))
    }

    pub(crate) fn client(conn: UnbufferedClientConnection) -> Result<Self, rustls::Error> {
        Self::new(Side::Client(conn))
    }

    fn new(side: Side) -> Result<Self, rustls::Error> {
        let mut user = Self {
            side,
            queued: Vec::new(),
            partial: Vec::new(),
            peer_closed: false,
            failed: None,
        };
        // A client's first flight (its ClientHello) is ready at once.
        user.process_partial(&mut Vec::new())?;
        Ok(user)
    }

    fn common_state(&self) -> &rustls::CommonState {
        match &self.side {
            Side::Server(c) => c,
            Side::Client(c) => c,
        }
    }

    pub(crate) fn is_server(&self) -> bool {
        matches!(self.side, Side::Server(_))
    }

    pub(crate) fn is_handshaking(&self) -> bool {
        self.common_state().is_handshaking()
    }

    pub(crate) fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.common_state().protocol_version()
    }

    pub(crate) fn alpn_protocol(&self) -> Option<&[u8]> {
        self.common_state().alpn_protocol()
    }

    pub(crate) fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.common_state().peer_certificates()
    }

    pub(crate) fn handshake_kind(&self) -> Option<HandshakeKind> {
        self.common_state().handshake_kind()
    }

    /// `true` if records wait to be sent.
    pub(crate) fn wants_write(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Moves the records waiting to be sent to the end of `out`.
    pub(crate) fn write_tls(&mut self, out: &mut Vec<u8>) {
        if out.is_empty() && out.capacity() < self.queued.capacity() {
            mem::swap(out, &mut self.queued);
        } else {
            out.extend_from_slice(&self.queued);
        }
        // An idle connection keeps no buffer for these.
        self.queued = Vec::new();
    }

    /// Processes received bytes: handshake messages move the handshake on
    /// (and may queue records to send), application data is appended to
    /// `plain`. Bytes of a record not complete yet are kept for the next
    /// call. After the peer's `close_notify` nothing more is read.
    pub(crate) fn read_tls(&mut self, input: &[u8], plain: &mut Vec<u8>) -> Result<(), ReadError> {
        if let Some(e) = &self.failed {
            return Err(ReadError::Tls(e.clone()));
        }
        if self.peer_closed {
            return Ok(());
        }
        if !self.partial.is_empty() {
            let mut buf = mem::take(&mut self.partial);
            buf.extend_from_slice(input);
            let res = self.process_buffer(&mut buf, plain);
            return res.and_then(|()| self.keep_partial(&buf));
        }
        SCRATCH.with(|scratch| {
            // Processing never waits, so nothing else on this thread is
            // using the scratch buffer; `try_borrow_mut` guards it anyway.
            let mut owned;
            let mut guard = scratch.try_borrow_mut();
            let buf: &mut Vec<u8> = match guard.as_deref_mut() {
                Ok(buf) => buf,
                Err(_) => {
                    owned = Vec::new();
                    &mut owned
                }
            };
            buf.clear();
            buf.extend_from_slice(input);
            let res = self
                .process_buffer(buf, plain)
                .and_then(|()| self.keep_partial(buf));
            buf.clear();
            if buf.capacity() > SCRATCH_KEPT {
                *buf = Vec::new();
            }
            res
        })
    }

    /// Keeps `rest`, what processing left of the received bytes, for the
    /// next receive: the start of a record, or records of a handshake
    /// message whose end has not arrived. Nothing is read after the peer's
    /// `close_notify`.
    fn keep_partial(&mut self, rest: &[u8]) -> Result<(), ReadError> {
        if rest.is_empty() || self.peer_closed {
            return Ok(());
        }
        if rest.len() > MAX_PARTIAL {
            return Err(ReadError::TooLarge);
        }
        self.partial = rest.to_vec();
        Ok(())
    }

    /// Processes every complete record in `buf`, removing from its front
    /// what rustls used up.
    fn process_buffer(&mut self, buf: &mut Vec<u8>, plain: &mut Vec<u8>) -> Result<(), ReadError> {
        let mut used = 0;
        let res = match self.process(buf, &mut used, plain) {
            Ok(Stop::Idle) => Ok(()),
            Ok(Stop::EarlyData) => Err(ReadError::EarlyData),
            Err(e) => {
                self.collect_alert(buf, &mut used);
                self.failed = Some(e.clone());
                Err(ReadError::Tls(e))
            }
        };
        buf.drain(..used);
        res
    }

    /// After rustls failed on `buf`: queues the alert it has for the
    /// failure. rustls hands a queued record over before it looks at the
    /// input again, and that input must be the same bytes (`buf[*used..]`).
    /// No call is made once the alert is out: rustls would process the
    /// record that failed a second time.
    fn collect_alert(&mut self, buf: &mut [u8], used: &mut usize) {
        // A second failure means no alert was queued by the first; one may
        // be now.
        for _ in 0..2 {
            let incoming = &mut buf[*used..];
            let (discard, step) = match &mut self.side {
                Side::Server(c) => alert_step(c.process_tls_records(incoming), &mut self.queued),
                Side::Client(c) => alert_step(c.process_tls_records(incoming), &mut self.queued),
            };
            *used += discard;
            if step != AlertStep::FailedAgain {
                return;
            }
        }
    }

    /// Runs rustls over `buf[*used..]` until it needs more input, queueing
    /// the records it emits and appending application data to `plain`.
    /// `used` grows by what rustls is done with.
    fn process(
        &mut self,
        buf: &mut [u8],
        used: &mut usize,
        plain: &mut Vec<u8>,
    ) -> Result<Stop, rustls::Error> {
        loop {
            let incoming = &mut buf[*used..];
            let (discard, step) = match &mut self.side {
                Side::Server(c) => {
                    let status = c.process_tls_records(incoming);
                    handle(status, &mut self.queued, plain)
                }
                Side::Client(c) => {
                    let status = c.process_tls_records(incoming);
                    handle(status, &mut self.queued, plain)
                }
            };
            *used += discard;
            match step? {
                Step::Again => {}
                Step::PeerClosed => self.peer_closed = true,
                Step::Stop(stop) => return Ok(stop),
            }
        }
    }

    /// Processes what `partial` holds (nothing, usually) to collect the
    /// records rustls emitted since the last call, as after `close_notify`
    /// or an error.
    fn process_partial(&mut self, plain: &mut Vec<u8>) -> Result<(), rustls::Error> {
        let mut buf = mem::take(&mut self.partial);
        let mut used = 0;
        let res = self.process(&mut buf, &mut used, plain);
        buf.drain(..used);
        if !buf.is_empty() {
            self.partial = buf;
        }
        res.map(|_| ())
    }

    /// Encrypts all of `data`, appending the records to `out` after any
    /// record waiting to be sent (key updates come first). Returns the
    /// number of bytes appended.
    pub(crate) fn encrypt(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<usize, Refused> {
        if let Some(e) = &self.failed {
            return Err(Refused::Tls(e.clone()));
        }
        let start = out.len();
        let mut partial = mem::take(&mut self.partial);
        let res = self.encrypt_with(&mut partial, data, out);
        self.partial = partial;
        match res {
            Ok(()) => Ok(out.len() - start),
            Err(Refused::Exhausted) => {
                // rustls queued close_notify (TLS 1.2): collect it.
                let _ = self.process_partial(&mut Vec::new());
                Err(Refused::Exhausted)
            }
            Err(e) => Err(e),
        }
    }

    fn encrypt_with(
        &mut self,
        partial: &mut Vec<u8>,
        data: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Refused> {
        let mut plain = Vec::new();
        loop {
            let used;
            let mut extra = 0;
            let step = match &mut self.side {
                Side::Server(c) => {
                    let status = c.process_tls_records(partial);
                    used = status.discard;
                    write_step(status, &mut extra, &mut self.queued, &mut plain, data, out)
                }
                Side::Client(c) => {
                    let status = c.process_tls_records(partial);
                    used = status.discard;
                    write_step(status, &mut extra, &mut self.queued, &mut plain, data, out)
                }
            };
            partial.drain(..used + extra);
            debug_assert!(plain.is_empty(), "plaintext is drained as it arrives");
            match step {
                WriteStep::Again => {}
                WriteStep::PeerClosed => self.peer_closed = true,
                WriteStep::Done => return Ok(()),
                WriteStep::Refused(Refused::Tls(e)) => {
                    self.failed = Some(e.clone());
                    return Err(Refused::Tls(e));
                }
                WriteStep::Refused(r) => return Err(r),
            }
        }
    }

    /// Queues `close_notify`, after any record waiting to be sent. Does
    /// nothing if application data cannot be sent (the handshake is not
    /// over, or `close_notify` was sent already).
    pub(crate) fn send_close_notify(&mut self) {
        if self.failed.is_some() {
            return;
        }
        let mut partial = mem::take(&mut self.partial);
        let mut queued = mem::take(&mut self.queued);
        let mut plain = Vec::new();
        for _ in 0..8 {
            let used;
            let mut extra = 0;
            let step = match &mut self.side {
                Side::Server(c) => {
                    let status = c.process_tls_records(&mut partial);
                    used = status.discard;
                    close_step(status, &mut extra, &mut queued, &mut plain)
                }
                Side::Client(c) => {
                    let status = c.process_tls_records(&mut partial);
                    used = status.discard;
                    close_step(status, &mut extra, &mut queued, &mut plain)
                }
            };
            partial.drain(..used + extra);
            match step {
                Some(true) => continue,
                Some(false) | None => break,
            }
        }
        self.partial = partial;
        self.queued = queued;
    }

    /// `true` once rustls has handed over the peer's `close_notify`.
    pub(crate) fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Bytes held for records not complete yet (for tests).
    #[cfg(test)]
    pub(crate) fn partial_capacity(&self) -> usize {
        self.partial.capacity()
    }
}

/// Why [`User::read_tls`] failed.
pub(crate) enum ReadError {
    Tls(rustls::Error),
    /// More bytes of incomplete records than [`MAX_PARTIAL`].
    TooLarge,
    /// The client sent 0-RTT data, which is not accepted.
    EarlyData,
}

enum Step {
    Again,
    PeerClosed,
    Stop(Stop),
}

/// Handles one state of a connection that is reading: records emitted go to
/// `queued`, application data to `plain`.
fn handle<Data>(
    status: UnbufferedStatus<'_, '_, Data>,
    queued: &mut Vec<u8>,
    plain: &mut Vec<u8>,
) -> (usize, Result<Step, rustls::Error>) {
    let mut discard = status.discard;
    let state = match status.state {
        Ok(state) => state,
        Err(e) => return (discard, Err(e)),
    };
    let step = match state {
        ConnectionState::EncodeTlsData(mut data) => {
            encode(queued, |out| data.encode(out));
            Step::Again
        }
        ConnectionState::TransmitTlsData(data) => {
            // The records are in `queued`, and go out with the stream's
            // next send.
            data.done();
            Step::Again
        }
        ConnectionState::ReadTraffic(mut data) => {
            while let Some(record) = data.next_record() {
                match record {
                    Ok(record) => {
                        discard += record.discard;
                        plain.extend_from_slice(record.payload);
                    }
                    Err(e) => return (discard, Err(e)),
                }
            }
            Step::Again
        }
        ConnectionState::PeerClosed => Step::PeerClosed,
        ConnectionState::ReadEarlyData(_) => Step::Stop(Stop::EarlyData),
        // Every complete record is processed.
        ConnectionState::BlockedHandshake
        | ConnectionState::WriteTraffic(_)
        | ConnectionState::Closed => Step::Stop(Stop::Idle),
        // A state added by a later rustls: nothing more to process here.
        _ => Step::Stop(Stop::Idle),
    };
    (discard, Ok(step))
}

#[derive(PartialEq)]
enum AlertStep {
    Done,
    FailedAgain,
}

/// Handles the state rustls is in after a failure: the alert it queued is
/// encoded into `queued`.
fn alert_step<Data>(
    status: UnbufferedStatus<'_, '_, Data>,
    queued: &mut Vec<u8>,
) -> (usize, AlertStep) {
    let step = match status.state {
        Ok(ConnectionState::EncodeTlsData(mut data)) => {
            encode(queued, |out| data.encode(out));
            AlertStep::Done
        }
        Ok(_) => AlertStep::Done,
        Err(_) => AlertStep::FailedAgain,
    };
    (status.discard, step)
}

enum WriteStep {
    Again,
    PeerClosed,
    Done,
    Refused(Refused),
}

/// Handles one state of a connection that is about to encrypt `data` into
/// `out`.
fn write_step<Data>(
    status: UnbufferedStatus<'_, '_, Data>,
    discard: &mut usize,
    queued: &mut Vec<u8>,
    plain: &mut Vec<u8>,
    data: &[u8],
    out: &mut Vec<u8>,
) -> WriteStep {
    let state = match status.state {
        Ok(state) => state,
        Err(e) => return WriteStep::Refused(Refused::Tls(e)),
    };
    match state {
        ConnectionState::EncodeTlsData(mut d) => {
            encode(queued, |buf| d.encode(buf));
            WriteStep::Again
        }
        ConnectionState::TransmitTlsData(d) => {
            d.done();
            WriteStep::Again
        }
        ConnectionState::ReadTraffic(mut d) => {
            while let Some(record) = d.next_record() {
                match record {
                    Ok(record) => {
                        *discard += record.discard;
                        plain.extend_from_slice(record.payload);
                    }
                    Err(e) => return WriteStep::Refused(Refused::Tls(e)),
                }
            }
            WriteStep::Again
        }
        ConnectionState::PeerClosed => WriteStep::PeerClosed,
        ConnectionState::WriteTraffic(mut w) => {
            // Records waiting go first, as rustls' own queue would send them.
            if !queued.is_empty() {
                out.extend_from_slice(queued);
                *queued = Vec::new();
            }
            let start = out.len();
            let records = data.len().div_ceil(16 << 10).max(1);
            out.resize(start + data.len() + records * RECORD_OVERHEAD, 0);
            let res = match w.encrypt(data, &mut out[start..]) {
                Err(EncryptError::InsufficientSize(s)) => {
                    out.resize(start + s.required_size, 0);
                    w.encrypt(data, &mut out[start..])
                }
                other => other,
            };
            match res {
                Ok(n) => {
                    out.truncate(start + n);
                    WriteStep::Done
                }
                Err(EncryptError::EncryptExhausted) => {
                    out.truncate(start);
                    WriteStep::Refused(Refused::Exhausted)
                }
                Err(e) => {
                    out.truncate(start);
                    WriteStep::Refused(Refused::Tls(rustls::Error::General(e.to_string())))
                }
            }
        }
        _ => WriteStep::Refused(Refused::Closed),
    }
}

/// Handles one state of a connection that is to send `close_notify`:
/// `Some(true)` to go on, `Some(false)` when it is queued, `None` when it
/// cannot be.
fn close_step<Data>(
    status: UnbufferedStatus<'_, '_, Data>,
    discard: &mut usize,
    queued: &mut Vec<u8>,
    plain: &mut Vec<u8>,
) -> Option<bool> {
    match status.state.ok()? {
        ConnectionState::EncodeTlsData(mut d) => {
            encode(queued, |buf| d.encode(buf));
            Some(true)
        }
        ConnectionState::TransmitTlsData(d) => {
            d.done();
            Some(true)
        }
        ConnectionState::ReadTraffic(mut d) => {
            while let Some(Ok(record)) = d.next_record() {
                *discard += record.discard;
                plain.extend_from_slice(record.payload);
            }
            Some(true)
        }
        ConnectionState::PeerClosed => Some(true),
        ConnectionState::WriteTraffic(mut w) => {
            let start = queued.len();
            queued.resize(start + MAX_RECORD, 0);
            let res = match w.queue_close_notify(&mut queued[start..]) {
                Err(EncryptError::InsufficientSize(s)) => {
                    queued.resize(start + s.required_size, 0);
                    w.queue_close_notify(&mut queued[start..])
                }
                other => other,
            };
            match res {
                Ok(n) => queued.truncate(start + n),
                Err(_) => queued.truncate(start),
            }
            Some(false)
        }
        _ => None,
    }
}

/// Appends a record rustls emits to `queued`, making room as it asks.
fn encode(queued: &mut Vec<u8>, mut encode: impl FnMut(&mut [u8]) -> Result<usize, EncodeError>) {
    let start = queued.len();
    queued.resize(start + MAX_RECORD, 0);
    let res = match encode(&mut queued[start..]) {
        // A flight bigger than a record (a long certificate chain): rustls
        // says how much room it needs.
        Err(EncodeError::InsufficientSize(s)) => {
            queued.resize(start + s.required_size, 0);
            encode(&mut queued[start..])
        }
        other => other,
    };
    match res {
        Ok(n) => queued.truncate(start + n),
        Err(_) => queued.truncate(start),
    }
}
