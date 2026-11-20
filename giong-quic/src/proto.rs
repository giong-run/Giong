//! The quinn-proto engine behind the public API.
//!
//! One endpoint per giong worker: a driver task owns the socket and runs
//! quinn-proto; connection and stream handles reach the state through an
//! `Rc<RefCell<..>>`. Everything stays on the worker that made it, so there
//! is no lock, channel or atomic anywhere (quinn on Tokio has a mutex per
//! connection and channels between endpoint and connections).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use giong::net::UdpSocket;
use quinn_proto::{
    ConnectionError, ConnectionHandle, DatagramEvent, Dir, EcnCodepoint, EndpointConfig, Event,
    ReadError, StreamEvent, StreamId, WriteError,
};

use crate::error;

/// Datagrams quinn-proto may put in one GSO send. quinn uses 10.
const GSO_SEGMENTS: usize = 10;
/// Datagrams handled before the driver runs connections again.
const RECV_BATCH: usize = 64;

pub(crate) struct Shared {
    sock: UdpSocket,
    st: RefCell<State>,
}

#[derive(Default)]
struct Wakers {
    read: Option<Waker>,
    write: Option<Waker>,
}

struct Conn {
    c: quinn_proto::Connection,
    connected: bool,
    lost: Option<ConnectionError>,
    /// Waiting for the handshake, a stream to accept or one to open.
    waiters: Vec<Waker>,
    streams: HashMap<StreamId, Wakers>,
    /// Live handles (the `Connection` and its streams).
    refs: usize,
    dirty: bool,
    /// Server side: handed to `accept` once connected.
    incoming: bool,
}

struct Send {
    data: Vec<u8>,
    to: SocketAddr,
    segment: usize,
    ecn: u8,
}

struct State {
    ep: quinn_proto::Endpoint,
    conns: Vec<Option<Conn>>,
    dirty: Vec<usize>,
    accepted: VecDeque<usize>,
    accept_waker: Option<Waker>,
    driver_waker: Option<Waker>,
    wake: bool,
    /// The `Endpoint` handle is gone: no new connections.
    closed: bool,
    scratch: Vec<u8>,
    sends: Vec<Send>,
    segments: usize,
    spare: Vec<Vec<u8>>,
}

fn wake_all(w: &mut Vec<Waker>) {
    for w in w.drain(..) {
        w.wake();
    }
}

impl State {
    fn conn(&mut self, h: usize) -> &mut Conn {
        self.conns[h]
            .as_mut()
            .expect("a handle keeps its connection")
    }

    fn mark(&mut self, h: usize) {
        if let Some(Some(c)) = self.conns.get_mut(h) {
            if !c.dirty {
                c.dirty = true;
                self.dirty.push(h);
            }
        }
        if !self.wake {
            self.wake = true;
            if let Some(w) = self.driver_waker.take() {
                w.wake();
            }
        }
    }

    fn insert(&mut self, ch: ConnectionHandle, c: quinn_proto::Connection, incoming: bool) {
        if self.conns.len() <= ch.0 {
            self.conns.resize_with(ch.0 + 1, || None);
        }
        self.conns[ch.0] = Some(Conn {
            c,
            connected: false,
            lost: None,
            waiters: Vec::new(),
            streams: HashMap::new(),
            refs: usize::from(!incoming),
            dirty: false,
            incoming,
        });
        self.mark(ch.0);
    }

    fn out(&mut self, to: SocketAddr, data: &[u8], segment: usize, ecn: u8) {
        let mut v = self.spare.pop().unwrap_or_default();
        v.extend_from_slice(data);
        self.sends.push(Send {
            data: v,
            to,
            segment,
            ecn,
        });
    }

    fn handle(
        &mut self,
        now: Instant,
        from: SocketAddr,
        ecn: u8,
        mut data: BytesMut,
        seg: Option<usize>,
    ) {
        let seg = seg.unwrap_or(data.len()).max(1);
        let ecn = EcnCodepoint::from_bits(ecn);
        while !data.is_empty() {
            let one = data.split_to(seg.min(data.len()));
            self.scratch.clear();
            match self.ep.handle(now, from, None, ecn, one, &mut self.scratch) {
                Some(DatagramEvent::NewConnection(inc)) => {
                    if self.closed {
                        self.ep.ignore(inc);
                        continue;
                    }
                    self.scratch.clear();
                    match self.ep.accept(inc, now, &mut self.scratch, None) {
                        Ok((ch, c)) => self.insert(ch, c, true),
                        Err(e) => {
                            if let Some(t) = e.response {
                                let d = std::mem::take(&mut self.scratch);
                                self.out(t.destination, &d[..t.size], 0, 0);
                                self.scratch = d;
                            }
                        }
                    }
                }
                Some(DatagramEvent::ConnectionEvent(ch, ev)) => {
                    if let Some(Some(c)) = self.conns.get_mut(ch.0) {
                        c.c.handle_event(ev);
                        self.mark(ch.0);
                    }
                }
                Some(DatagramEvent::Response(t)) => {
                    let d = std::mem::take(&mut self.scratch);
                    self.out(t.destination, &d[..t.size], 0, 0);
                    self.scratch = d;
                }
                None => {}
            }
        }
    }

    /// Timers, events and transmits of every connection that needs it.
    /// Returns the next deadline.
    fn process(&mut self, now: Instant) -> Option<Instant> {
        self.wake = false;
        for i in 0..self.conns.len() {
            if let Some(c) = &mut self.conns[i] {
                if c.c.poll_timeout().is_some_and(|t| t <= now) {
                    c.c.handle_timeout(now);
                    self.mark(i);
                }
            }
        }
        self.wake = false;
        while let Some(i) = self.dirty.pop() {
            self.process_one(i, now);
        }
        self.conns
            .iter_mut()
            .flatten()
            .filter_map(|c| c.c.poll_timeout())
            .min()
    }

    fn process_one(&mut self, i: usize, now: Instant) {
        let ch = ConnectionHandle(i);
        let Some(conn) = self.conns[i].as_mut() else {
            return;
        };
        conn.dirty = false;
        loop {
            let mut again = false;
            while let Some(ev) = conn.c.poll_endpoint_events() {
                if let Some(cev) = self.ep.handle_event(ch, ev) {
                    conn.c.handle_event(cev);
                    again = true;
                }
            }
            while let Some(ev) = conn.c.poll() {
                match ev {
                    Event::Connected => {
                        conn.connected = true;
                        if conn.incoming {
                            conn.refs += 1;
                            self.accepted.push_back(i);
                            if let Some(w) = self.accept_waker.take() {
                                w.wake();
                            }
                        }
                        wake_all(&mut conn.waiters);
                    }
                    Event::ConnectionLost { reason } => {
                        conn.lost = Some(reason);
                        wake_all(&mut conn.waiters);
                        for w in conn.streams.values_mut() {
                            if let Some(w) = w.read.take() {
                                w.wake();
                            }
                            if let Some(w) = w.write.take() {
                                w.wake();
                            }
                        }
                    }
                    Event::Stream(StreamEvent::Opened { .. } | StreamEvent::Available { .. }) => {
                        wake_all(&mut conn.waiters)
                    }
                    Event::Stream(StreamEvent::Readable { id }) => {
                        if let Some(w) = conn.streams.get_mut(&id).and_then(|w| w.read.take()) {
                            w.wake();
                        }
                    }
                    Event::Stream(
                        StreamEvent::Writable { id }
                        | StreamEvent::Finished { id }
                        | StreamEvent::Stopped { id, .. },
                    ) => {
                        if let Some(w) = conn.streams.get_mut(&id).and_then(|w| w.write.take()) {
                            w.wake();
                        }
                    }
                    _ => {}
                }
            }
            loop {
                self.scratch.clear();
                match conn.c.poll_transmit(now, self.segments, &mut self.scratch) {
                    Some(t) => {
                        let mut v = self.spare.pop().unwrap_or_default();
                        v.extend_from_slice(&self.scratch[..t.size]);
                        self.sends.push(Send {
                            data: v,
                            to: t.destination,
                            segment: t.segment_size.unwrap_or(0),
                            ecn: t.ecn.map_or(0, |e| e as u8),
                        });
                    }
                    None => break,
                }
            }
            if !again {
                break;
            }
        }
        if conn.c.is_drained() && conn.refs == 0 {
            self.conns[i] = None;
        }
    }
}

/// The driver task of one endpoint.
async fn drive(sh: Rc<Shared>) {
    loop {
        let now = Instant::now();
        let (deadline, sends, done) = {
            let mut st = sh.st.borrow_mut();
            let deadline = st.process(now);
            let done = st.closed && st.conns.iter().all(Option::is_none);
            (deadline, std::mem::take(&mut st.sends), done)
        };
        for s in sends {
            let sh = sh.clone();
            giong::spawn(async move {
                let (_, mut v) = sh
                    .sock
                    .send_segments_to(s.data, s.to, s.segment, s.ecn)
                    .await;
                v.clear();
                sh.st.borrow_mut().spare.push(v);
            });
        }
        if done {
            return;
        }
        // Wait for a datagram, the next deadline, or a handle's call.
        let recv = pin!(sh.sock.recv_from());
        let mut recv = recv;
        let sleep = deadline.map(giong::time::sleep_until);
        let mut sleep = pin!(sleep);
        let first = poll_fn(|cx| {
            if let Poll::Ready(r) = recv.as_mut().poll(cx) {
                return Poll::Ready(Some(r));
            }
            if let Some(s) = sleep.as_mut().as_pin_mut() {
                if s.poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
            }
            let mut st = sh.st.borrow_mut();
            if st.wake {
                return Poll::Ready(None);
            }
            st.driver_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        let Some(first) = first else { continue };
        let now = Instant::now();
        let mut next = Some(first);
        let mut n = 0;
        while let Some(r) = next {
            if let Ok((dg, from)) = r {
                let data = BytesMut::from(dg.as_slice());
                let (seg, ecn) = (dg.segment_size(), dg.ecn());
                drop(dg);
                sh.st.borrow_mut().handle(now, from, ecn, data, seg);
            }
            n += 1;
            next = if n < RECV_BATCH {
                let mut f = pin!(sh.sock.recv_from());
                poll_fn(|cx| match f.as_mut().poll(cx) {
                    Poll::Ready(r) => Poll::Ready(Some(r)),
                    Poll::Pending => Poll::Ready(None),
                })
                .await
            } else {
                None
            };
        }
    }
}

pub(crate) fn bind(
    addr: SocketAddr,
    server: Option<Arc<quinn_proto::ServerConfig>>,
) -> io::Result<Rc<Shared>> {
    let sock = UdpSocket::bind(addr)?;
    // GRO needs 64 KiB receive buffers; without them datagrams come one by
    // one, which is correct, only slower.
    let _ = sock.set_gro(true);
    sock.set_recv_ecn(true)?;
    let segments = if sock.gso_supported() {
        GSO_SEGMENTS
    } else {
        1
    };
    let ep = quinn_proto::Endpoint::new(Arc::new(EndpointConfig::default()), server, true, None);
    let sh = Rc::new(Shared {
        sock,
        st: RefCell::new(State {
            ep,
            conns: Vec::new(),
            dirty: Vec::new(),
            accepted: VecDeque::new(),
            accept_waker: None,
            driver_waker: None,
            wake: false,
            closed: false,
            scratch: Vec::with_capacity(65_536),
            sends: Vec::new(),
            segments,
            spare: Vec::new(),
        }),
    });
    giong::spawn(drive(sh.clone()));
    Ok(sh)
}

impl Shared {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub(crate) fn close(&self) {
        let mut st = self.st.borrow_mut();
        st.closed = true;
        if let Some(w) = st.accept_waker.take() {
            w.wake();
        }
        st.mark(usize::MAX);
    }

    pub(crate) async fn accept(self: &Rc<Self>) -> Option<usize> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            if let Some(h) = st.accepted.pop_front() {
                return Poll::Ready(Some(h));
            }
            if st.closed {
                return Poll::Ready(None);
            }
            st.accept_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    pub(crate) async fn connect(
        self: &Rc<Self>,
        cfg: quinn_proto::ClientConfig,
        to: SocketAddr,
        name: &str,
    ) -> io::Result<usize> {
        let h = {
            let mut st = self.st.borrow_mut();
            let (ch, c) = st
                .ep
                .connect(Instant::now(), cfg, to, name)
                .map_err(error::connect)?;
            st.insert(ch, c, false);
            ch.0
        };
        // Releases the connection if the handshake fails or the caller
        // gives up on it (drops the future), so that it is closed and
        // forgotten instead of staying in the table without an owner.
        struct Attempt<'a>(&'a Shared, usize, bool);
        impl Drop for Attempt<'_> {
            fn drop(&mut self) {
                if !self.2 {
                    self.0.release(self.1);
                }
            }
        }
        let mut attempt = Attempt(self, h, false);
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if let Some(e) = &c.lost {
                return Poll::Ready(Err(error::connect(e)));
            }
            if c.connected {
                return Poll::Ready(Ok(()));
            }
            c.waiters.push(cx.waker().clone());
            Poll::Pending
        })
        .await?;
        attempt.2 = true;
        Ok(h)
    }

    /// Connections this endpoint still holds (open, closing or draining).
    pub(crate) fn connections(&self) -> usize {
        self.st.borrow().conns.iter().flatten().count()
    }

    pub(crate) fn remote_addr(&self, h: usize) -> SocketAddr {
        self.st.borrow_mut().conn(h).c.remote_address()
    }

    pub(crate) async fn stream(
        self: &Rc<Self>,
        h: usize,
        open: bool,
        dir: Dir,
    ) -> io::Result<StreamId> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if let Some(e) = &c.lost {
                return Poll::Ready(Err(error::lost(e)));
            }
            let id = if open {
                c.c.streams().open(dir)
            } else {
                c.c.streams().accept(dir)
            };
            match id {
                Some(id) => {
                    c.streams.insert(id, Wakers::default());
                    // One handle per half.
                    c.refs += if dir == Dir::Bi { 2 } else { 1 };
                    st.mark(h);
                    Poll::Ready(Ok(id))
                }
                None => {
                    c.waiters.push(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }

    pub(crate) async fn read(self: &Rc<Self>, h: usize, id: StreamId) -> io::Result<Option<Bytes>> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            let mut rs = c.c.recv_stream(id);
            let mut chunks = match rs.read(true) {
                Ok(chunks) => chunks,
                Err(_) => return Poll::Ready(Ok(None)),
            };
            let res = chunks.next(usize::MAX);
            let transmit = chunks.finalize().should_transmit();
            let out = match res {
                Ok(Some(chunk)) => Poll::Ready(Ok(Some(chunk.bytes))),
                Ok(None) => Poll::Ready(Ok(None)),
                Err(ReadError::Reset(code)) => Poll::Ready(Err(error::reset(code.into_inner()))),
                Err(ReadError::Blocked) => match &c.lost {
                    Some(e) => Poll::Ready(Err(error::lost(e))),
                    None => {
                        c.streams.entry(id).or_default().read = Some(cx.waker().clone());
                        Poll::Pending
                    }
                },
            };
            if transmit {
                st.mark(h);
            }
            out
        })
        .await
    }

    pub(crate) async fn write(
        self: &Rc<Self>,
        h: usize,
        id: StreamId,
        data: &[u8],
    ) -> io::Result<()> {
        let mut off = 0;
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            let mut wrote = false;
            let out = loop {
                if off == data.len() {
                    break Poll::Ready(Ok(()));
                }
                match c.c.send_stream(id).write(&data[off..]) {
                    Ok(n) => {
                        off += n;
                        wrote = true;
                    }
                    Err(WriteError::Blocked) => match &c.lost {
                        Some(e) => break Poll::Ready(Err(error::lost(e))),
                        None => {
                            c.streams.entry(id).or_default().write = Some(cx.waker().clone());
                            break Poll::Pending;
                        }
                    },
                    Err(WriteError::Stopped(code)) => {
                        break Poll::Ready(Err(error::reset(code.into_inner())))
                    }
                    Err(WriteError::ClosedStream) => {
                        break Poll::Ready(Err(match &c.lost {
                            Some(e) => error::lost(e),
                            None => error::lost(&ConnectionError::LocallyClosed),
                        }))
                    }
                }
            };
            if wrote {
                st.mark(h);
            }
            out
        })
        .await
    }

    pub(crate) fn finish(&self, h: usize, id: StreamId) -> io::Result<()> {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        let r = c.c.send_stream(id).finish();
        st.mark(h);
        r.map_err(|_| error::lost(&ConnectionError::LocallyClosed))
    }

    pub(crate) fn reset(&self, h: usize, id: StreamId, code: u32) {
        let mut st = self.st.borrow_mut();
        let _ = st.conn(h).c.send_stream(id).reset(code.into());
        st.mark(h);
    }

    pub(crate) fn stop(&self, h: usize, id: StreamId, code: u32) {
        let mut st = self.st.borrow_mut();
        let _ = st.conn(h).c.recv_stream(id).stop(code.into());
        st.mark(h);
    }

    pub(crate) fn close_conn(&self, h: usize, code: u32, reason: &[u8]) {
        let mut st = self.st.borrow_mut();
        st.conn(h)
            .c
            .close(Instant::now(), code.into(), Bytes::copy_from_slice(reason));
        st.mark(h);
    }

    /// A handle (connection or stream) is gone. The last one closes the
    /// connection if it is still open.
    pub(crate) fn release(&self, h: usize) {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        c.refs -= 1;
        if c.refs == 0 && c.lost.is_none() {
            c.c.close(Instant::now(), 0u32.into(), Bytes::new());
        }
        st.mark(h);
    }

    pub(crate) fn forget_stream(&self, h: usize, id: StreamId) {
        if let Some(Some(c)) = self.st.borrow_mut().conns.get_mut(h) {
            c.streams.remove(&id);
        }
    }
}
