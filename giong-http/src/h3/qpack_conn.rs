//! The QPACK decoder of one connection, when it allows the peer a dynamic
//! table (RFC 9204): the table, the streams that wait for insertions, and
//! our decoder stream, which tells the peer's encoder what arrived.
//!
//! The parsing is in [`qpack`](super::qpack) and works on bytes in memory;
//! this is what needs the connection: waking a request whose field section
//! refers to insertions not received yet, and writing Section
//! Acknowledgments, Stream Cancellations and Insert Count Increments on a
//! unidirectional stream of our own, opened when there is first something
//! to say.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::task::{Poll, Waker};

use giong_quic::Connection;

use super::frame::{write_varint, Code, Error, StreamType};
use super::qpack::{self, Table, BLOCKED_STREAMS};

/// A connection's QPACK decoder with a dynamic table.
pub(crate) struct Decoder {
    table: RefCell<Table>,
    /// Streams waiting for insertions now.
    blocked: Cell<u64>,
    /// Their tasks.
    waiting: RefCell<Vec<Waker>>,
    /// What our decoder stream has to say and has not said yet.
    out: RefCell<Vec<u8>>,
    /// The task that writes it.
    writer: Cell<Option<Waker>>,
    /// The connection is over: nothing waits any more.
    over: Cell<bool>,
}

fn failed(what: &'static str) -> Error {
    Error::new(Code::QpackDecompressionFailed, what)
}

impl Decoder {
    /// A decoder that allows the peer `capacity` bytes of dynamic table.
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            table: RefCell::new(Table::new(u64::from(capacity))),
            blocked: Cell::new(0),
            waiting: RefCell::new(Vec::new()),
            out: RefCell::new(Vec::new()),
            writer: Cell::new(None),
            over: Cell::new(false),
        }
    }

    fn said(&self) {
        if let Some(waker) = self.writer.take() {
            waker.wake();
        }
    }

    /// Applies what the peer's encoder stream says (see
    /// [`Table::encoder_stream`]), wakes the streams that waited for it and
    /// tells the encoder what arrived.
    pub(crate) fn encoder_stream(&self, buf: &[u8]) -> Result<usize, Error> {
        let mut table = self.table.borrow_mut();
        let before = table.inserted();
        let used = table.encoder_stream(buf)?;
        if table.inserted() != before {
            // Told at once, before any section that needs these insertions
            // can be acknowledged: an acknowledgment then never tells the
            // encoder more than an increment already did.
            table.increment(&mut self.out.borrow_mut());
            drop(table);
            self.said();
            for waker in self.waiting.borrow_mut().drain(..) {
                waker.wake();
            }
        }
        Ok(used)
    }

    /// Decodes the field section `block` of request stream `stream`,
    /// waiting for the insertions it needs, and acknowledges it if it used
    /// the table. A section that would make more than [`BLOCKED_STREAMS`]
    /// wait is a connection error (RFC 9204, section 2.1.2).
    pub(crate) async fn decode(
        &self,
        stream: u64,
        block: &[u8],
        field: impl FnMut(&[u8], &[u8]),
    ) -> Result<(), Error> {
        let prefix = self.table.borrow().prefix(block)?;
        if prefix.required > self.table.borrow().inserted() {
            if self.blocked.get() >= BLOCKED_STREAMS {
                return Err(failed("more streams wait for insertions than allowed"));
            }
            let mut waiting = Waiting {
                decoder: self,
                stream,
                done: false,
            };
            self.blocked.set(self.blocked.get() + 1);
            poll_fn(|cx| {
                if self.over.get() || self.table.borrow().inserted() >= prefix.required {
                    return Poll::Ready(());
                }
                let mut list = self.waiting.borrow_mut();
                if !list.iter().any(|w| w.will_wake(cx.waker())) {
                    list.push(cx.waker().clone());
                }
                Poll::Pending
            })
            .await;
            waiting.done = true;
            if self.over.get() {
                return Err(failed("the connection ended while a section waited"));
            }
        }
        let mut table = self.table.borrow_mut();
        table.decode(block, prefix, field)?;
        table.acknowledge(stream, prefix.required, &mut self.out.borrow_mut());
        drop(table);
        if prefix.required > 0 {
            self.said();
        }
        Ok(())
    }

    /// Stream `stream` is given up with a field section never decoded (too
    /// large, or the stream cancelled before it came): the encoder is told,
    /// so that it stops counting the section's references (RFC 9204,
    /// section 4.4.2). Telling it about a stream whose sections did not use
    /// the table, or that sent none, is harmless.
    pub(crate) fn cancel(&self, stream: u64) {
        if !self.over.get() {
            qpack::cancel_stream(stream, &mut self.out.borrow_mut());
            self.said();
        }
    }

    /// The connection is over: streams that wait stop waiting, and the
    /// writer ends.
    pub(crate) fn end(&self) {
        self.over.set(true);
        self.said();
        for waker in self.waiting.borrow_mut().drain(..) {
            waker.wake();
        }
    }

    /// Writes our decoder stream for as long as the connection lasts:
    /// opens it when there is first something to say. Ends at
    /// [`end`](Self::end) or when the connection is gone.
    pub(crate) async fn write(&self, quic: &Connection) {
        let mut stream = None;
        let mut buf = Vec::new();
        loop {
            poll_fn(|cx| {
                if self.over.get() || !self.out.borrow().is_empty() {
                    return Poll::Ready(());
                }
                self.writer.set(Some(cx.waker().clone()));
                Poll::Pending
            })
            .await;
            if self.over.get() {
                return;
            }
            buf.clear();
            if stream.is_none() {
                write_varint(&mut buf, StreamType::QPACK_DECODER);
                match quic.open_uni().await {
                    Ok(s) => stream = Some(s),
                    Err(_) => return,
                }
            }
            buf.append(&mut self.out.borrow_mut());
            let send = stream.as_mut().expect("opened above");
            if send.write_all(&buf).await.is_err() {
                return;
            }
        }
    }
}

/// A stream waiting for insertions: one fewer waits when it is done, and
/// one dropped while it waits is cancelled (RFC 9204, section 4.4.2), so
/// that the encoder stops counting its references.
struct Waiting<'a> {
    decoder: &'a Decoder,
    stream: u64,
    done: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let d = self.decoder;
        d.blocked.set(d.blocked.get() - 1);
        if !self.done {
            d.cancel(self.stream);
        }
    }
}
