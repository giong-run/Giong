//! The quinn-proto engine behind the public API.
//!
//! One endpoint per giong worker: a driver task owns the socket and runs
//! quinn-proto; connection and stream handles reach the state through an
//! `Rc<RefCell<..>>`. Everything stays on the worker that made it, so there
//! is no lock or channel anywhere (quinn on Tokio has a mutex per
//! connection and channels between endpoint and connections). The one
//! atomic is the flag of [`Closable`], which quinn-proto requires to be
//! `Send`; only its own worker touches it.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::route::{self, Forwarded, Member};

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
    /// The name quinn-proto has for it. It gives the name to a new
    /// connection as soon as this one has drained, which can be before
    /// the last handle to this one is dropped: handles are slots of
    /// `State::conns` instead, which are only reused without handles.
    ch: ConnectionHandle,
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
    /// Handed to `accept` already (a server endpoint with early data hands
    /// a connection over before its handshake ends).
    handed: bool,
    /// Client: the connection started with 0-RTT keys. Server: the
    /// endpoint accepts early data.
    early: bool,
    /// Bumped when the server rejects the client's early data: quinn-proto
    /// forgets the streams opened in it and gives their numbers out again,
    /// so handles from before must no longer touch them.
    epoch: u32,
    /// Server: bidirectional streams the peer opened before the handshake
    /// ended, so in 0-RTT data, taken ahead of `accept_bi` to remember it.
    early_bi: VecDeque<StreamId>,
    /// Set once the connection is over, so that its CONNECTION_CLOSE is
    /// not held back by congestion control (see [`Closable`]).
    closing: Arc<AtomicBool>,
    /// Server: the peer opened a stream in 0-RTT data.
    saw_early: bool,
}

/// A stream as a handle knows it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sid {
    pub(crate) id: StreamId,
    /// The connection's `epoch` when the stream was opened.
    epoch: u32,
    /// Its first bytes may have come in 0-RTT data (server), or went out
    /// in it (client).
    pub(crate) early: bool,
}

impl Conn {
    /// A stream of the early data the server rejected.
    fn stale(&self, s: Sid) -> bool {
        s.epoch != self.epoch
    }

    /// Takes the flag of a congestion controller that quinn-proto just
    /// built for this connection, which it does when the peer moves to
    /// another address.
    fn adopt(&mut self) {
        if let Some(flag) = Closable::built() {
            flag.store(self.lost.is_some(), Ordering::Relaxed);
            self.closing = flag;
        }
    }

    /// Server: takes the bidirectional streams that the datagram just
    /// handled opened, when it came before the end of the handshake.
    fn take_early(&mut self, handshaking: bool) {
        if self.incoming && self.early && handshaking {
            while let Some(id) = self.c.streams().accept(Dir::Bi) {
                self.early_bi.push_back(id);
                self.saw_early = true;
                wake_all(&mut self.waiters);
            }
        }
    }
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
    /// The slot in `conns` of the connection quinn-proto knows by each
    /// of its handles.
    slots: Vec<usize>,
    /// Empty slots of `conns`.
    free: Vec<usize>,
    dirty: Vec<usize>,
    accepted: VecDeque<usize>,
    accept_waker: Option<Waker>,
    driver_waker: Option<Waker>,
    wake: bool,
    /// The `Endpoint` handle is gone: no new connections.
    closed: bool,
    /// Server: the TLS configuration accepts early data.
    early_data: bool,
    scratch: Vec<u8>,
    sends: Vec<Send>,
    segments: usize,
    spare: Vec<Vec<u8>>,
}

impl Conn {
    /// The connection is over: whoever waits on it or on one of its
    /// streams is told so. The first reason stays.
    fn lose(&mut self, reason: ConnectionError) {
        if self.lost.is_none() {
            self.lost = Some(reason);
        }
        self.closing.store(true, Ordering::Relaxed);
        wake_all(&mut self.waiters);
        for w in self.streams.values_mut() {
            if let Some(w) = w.read.take() {
                w.wake();
            }
            if let Some(w) = w.write.take() {
                w.wake();
            }
        }
    }
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

    /// Takes a new connection in; gives its slot.
    fn insert(
        &mut self,
        ch: ConnectionHandle,
        c: quinn_proto::Connection,
        incoming: bool,
        early: bool,
    ) -> usize {
        let slot = self.free.pop().unwrap_or_else(|| {
            self.conns.push(None);
            self.conns.len() - 1
        });
        if self.slots.len() <= ch.0 {
            self.slots.resize(ch.0 + 1, usize::MAX);
        }
        self.slots[ch.0] = slot;
        self.conns[slot] = Some(Conn {
            c,
            ch,
            connected: false,
            lost: None,
            waiters: Vec::new(),
            streams: HashMap::new(),
            refs: usize::from(!incoming),
            dirty: false,
            incoming,
            handed: false,
            early,
            epoch: 0,
            early_bi: VecDeque::new(),
            closing: Closable::built().unwrap_or_default(),
            saw_early: false,
        });
        if let Some(c) = &mut self.conns[slot] {
            // The first datagram may hold 0-RTT packets after the Initial.
            c.take_early(true);
        }
        self.mark(slot);
        slot
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
                        Ok((ch, c)) => {
                            let early = self.early_data;
                            self.insert(ch, c, true, early);
                        }
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
                    let slot = self.slots.get(ch.0).copied().unwrap_or(usize::MAX);
                    if let Some(Some(c)) = self.conns.get_mut(slot) {
                        if c.ch == ch {
                            let handshaking = c.c.is_handshaking();
                            c.c.handle_event(ev);
                            c.adopt();
                            // A datagram that ends the handshake may also
                            // hold 1-RTT packets: their streams count as
                            // early too, which errs on the safe side.
                            c.take_early(handshaking);
                            self.mark(slot);
                        }
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
                    c.adopt();
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
        let Some(conn) = self.conns[i].as_mut() else {
            return;
        };
        let ch = conn.ch;
        conn.dirty = false;
        loop {
            let mut again = false;
            while let Some(ev) = conn.c.poll_endpoint_events() {
                if let Some(cev) = self.ep.handle_event(ch, ev) {
                    conn.c.handle_event(cev);
                    conn.adopt();
                    again = true;
                }
            }
            while let Some(ev) = conn.c.poll() {
                match ev {
                    Event::HandshakeDataReady if conn.incoming && conn.early => {
                        // Early data on: the application gets the
                        // connection now, to answer what came in 0-RTT.
                        conn.handed = true;
                        conn.refs += 1;
                        self.accepted.push_back(i);
                        if let Some(w) = self.accept_waker.take() {
                            w.wake();
                        }
                    }
                    Event::Connected => {
                        conn.connected = true;
                        if conn.incoming && !conn.handed {
                            conn.handed = true;
                            conn.refs += 1;
                            self.accepted.push_back(i);
                            if let Some(w) = self.accept_waker.take() {
                                w.wake();
                            }
                        }
                        if !conn.incoming && conn.early && !conn.c.accepted_0rtt() {
                            // The server refused the early data: what was
                            // sent in it is gone, and the stream numbers
                            // start again.
                            conn.epoch += 1;
                            for w in conn.streams.values_mut() {
                                if let Some(w) = w.read.take() {
                                    w.wake();
                                }
                                if let Some(w) = w.write.take() {
                                    w.wake();
                                }
                            }
                            conn.streams.clear();
                        }
                        wake_all(&mut conn.waiters);
                    }
                    Event::ConnectionLost { reason } => conn.lose(reason),
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
            self.free.push(i);
        }
    }
}

/// What woke the driver.
enum Woke {
    Datagram(io::Result<(giong::net::Datagram, SocketAddr)>),
    Forwarded(Forwarded),
    Other,
}

/// The driver task of one endpoint. A server endpoint has a `route`: its
/// place among the endpoints of the other workers on the same address,
/// and where the datagrams they pass on arrive.
async fn drive(sh: Rc<Shared>, route: Option<(Member, giong::sync::mpsc::Receiver<Forwarded>)>) {
    let (mut member, mut inbox) = match route {
        Some((member, inbox)) => (Some(member), Some(inbox)),
        None => (None, None),
    };
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
        // Wait for a datagram, one that another worker passes on, the
        // next deadline, or a handle's call.
        let first = {
            let mut recv = pin!(sh.sock.recv_from());
            let passed = async {
                match inbox.as_mut() {
                    Some(inbox) => inbox.recv().await,
                    None => std::future::pending().await,
                }
            };
            let mut passed = pin!(passed);
            let sleep = deadline.map(giong::time::sleep_until);
            let mut sleep = pin!(sleep);
            poll_fn(|cx| {
                if let Poll::Ready(r) = recv.as_mut().poll(cx) {
                    return Poll::Ready(Woke::Datagram(r));
                }
                if let Poll::Ready(Some(datagram)) = passed.as_mut().poll(cx) {
                    return Poll::Ready(Woke::Forwarded(datagram));
                }
                if let Some(s) = sleep.as_mut().as_pin_mut() {
                    if s.poll(cx).is_ready() {
                        return Poll::Ready(Woke::Other);
                    }
                }
                let mut st = sh.st.borrow_mut();
                if st.wake {
                    return Poll::Ready(Woke::Other);
                }
                st.driver_waker = Some(cx.waker().clone());
                Poll::Pending
            })
            .await
        };
        let now = Instant::now();
        let first = match first {
            Woke::Datagram(r) => r,
            Woke::Forwarded(first) => {
                // What the other workers passed on, a batch at most.
                let mut next = Some(first);
                let mut n = 0;
                while let Some(d) = next {
                    let data = BytesMut::from(&d.data[..]);
                    sh.st
                        .borrow_mut()
                        .handle(now, d.from, d.ecn, data, d.segment);
                    n += 1;
                    next = match inbox.as_mut() {
                        Some(inbox) if n < RECV_BATCH => inbox.try_recv().ok(),
                        _ => None,
                    };
                }
                continue;
            }
            Woke::Other => continue,
        };
        let mut next = Some(first);
        let mut n = 0;
        while let Some(r) = next {
            if let Ok((dg, from)) = r {
                let (seg, ecn) = (dg.segment_size(), dg.ecn());
                match member.as_mut() {
                    Some(member) => {
                        deliver(&sh, member, now, from, ecn, dg.as_slice(), seg);
                    }
                    None => {
                        let data = BytesMut::from(dg.as_slice());
                        drop(dg);
                        sh.st.borrow_mut().handle(now, from, ecn, data, seg);
                    }
                }
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

/// A receive of a server endpoint: handled here, or passed on to the
/// worker that owns its connection (see [`route`]).
fn deliver(
    sh: &Shared,
    member: &mut Member,
    now: Instant,
    from: SocketAddr,
    ecn: u8,
    data: &[u8],
    seg: Option<usize>,
) {
    let me = member.worker();
    let whole = seg.unwrap_or(data.len()).max(1);
    // Datagrams received as one mostly go the same way; when they do not,
    // each goes its own.
    let (step, seg) = match route::owner_of_all(data, whole, me) {
        Ok(None) => {
            let data = BytesMut::from(data);
            sh.st.borrow_mut().handle(now, from, ecn, data, seg);
            return;
        }
        Ok(Some(_)) => (data.len().max(1), seg),
        Err(route::Mixed) => (whole, None),
    };
    for part in data.chunks(step) {
        let mut here = Some(part);
        if let Some(worker) = crate::native::cid::owner(part, me) {
            let passed = Forwarded {
                data: part.to_vec(),
                from,
                ecn,
                segment: seg,
            };
            // Given back when no endpoint of that worker is there: the
            // packet is then answered as any that is not known.
            if member.forward(worker, passed).is_none() {
                here = None;
            }
        }
        if let Some(part) = here {
            let data = BytesMut::from(part);
            sh.st.borrow_mut().handle(now, from, ecn, data, seg);
        }
    }
}

pub(crate) fn bind(
    addr: SocketAddr,
    server: Option<Arc<quinn_proto::ServerConfig>>,
    early_data: bool,
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
    // Connection IDs name this worker, so that another worker that gets
    // one of this endpoint's packets knows where it belongs.
    let worker = giong::worker_id() as u16;
    let mut config = EndpointConfig::default();
    config.cid_generator(move || Box::new(route::WorkerCids { worker }));
    let route = match &server {
        Some(_) => Some(Member::join(sock.local_addr()?, worker)),
        None => None,
    };
    let ep = quinn_proto::Endpoint::new(Arc::new(config), server, true, None);
    let sh = Rc::new(Shared {
        sock,
        st: RefCell::new(State {
            ep,
            conns: Vec::new(),
            slots: Vec::new(),
            free: Vec::new(),
            dirty: Vec::new(),
            accepted: VecDeque::new(),
            accept_waker: None,
            driver_waker: None,
            wake: false,
            closed: false,
            early_data,
            scratch: Vec::with_capacity(65_536),
            sends: Vec::new(),
            segments,
            spare: Vec::new(),
        }),
    });
    giong::spawn(drive(sh.clone(), route));
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

    /// Connects; with `early`, returns at once when the client has 0-RTT
    /// keys for the server (a ticket from an earlier connection).
    pub(crate) async fn connect(
        self: &Rc<Self>,
        cfg: quinn_proto::ClientConfig,
        to: SocketAddr,
        name: &str,
        early: bool,
    ) -> io::Result<usize> {
        let h = {
            let mut st = self.st.borrow_mut();
            let (ch, c) = st
                .ep
                .connect(Instant::now(), cfg, to, name)
                .map_err(error::connect)?;
            let early = early && c.has_0rtt();
            let h = st.insert(ch, c, false, early);
            if early {
                return Ok(h);
            }
            h
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

    pub(crate) fn stats(&self, h: usize) -> crate::ConnectionStats {
        let stats = self.st.borrow_mut().conn(h).c.stats();
        crate::ConnectionStats {
            rtt: stats.path.rtt,
            congestion_window: stats.path.cwnd,
            congestion_events: stats.path.congestion_events,
            sent_packets: stats.path.sent_packets,
            lost_packets: stats.path.lost_packets,
            lost_bytes: stats.path.lost_bytes,
            sent_datagrams: stats.udp_tx.datagrams,
            sent_bytes: stats.udp_tx.bytes,
            received_datagrams: stats.udp_rx.datagrams,
            received_bytes: stats.udp_rx.bytes,
            mtu: stats.path.current_mtu,
        }
    }

    /// Waits for the end of the handshake. `true` if the server took the
    /// early data this client sent (client), or if the peer opened a
    /// request stream in early data (server).
    pub(crate) async fn handshake(self: &Rc<Self>, h: usize) -> io::Result<bool> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if c.connected {
                let took = if c.incoming {
                    c.saw_early
                } else {
                    c.early && c.c.accepted_0rtt()
                };
                return Poll::Ready(Ok(took));
            }
            if let Some(e) = &c.lost {
                return Poll::Ready(Err(error::lost(e)));
            }
            c.waiters.push(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    pub(crate) async fn stream(self: &Rc<Self>, h: usize, open: bool, dir: Dir) -> io::Result<Sid> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if let Some(e) = &c.lost {
                return Poll::Ready(Err(error::lost(e)));
            }
            let (id, early) = if open {
                // Opened before the handshake ends: its bytes go in 0-RTT.
                (c.c.streams().open(dir), c.early && !c.connected)
            } else if dir == Dir::Bi && !c.early_bi.is_empty() {
                (c.early_bi.pop_front(), true)
            } else {
                (c.c.streams().accept(dir), false)
            };
            match id {
                Some(id) => {
                    c.streams.insert(id, Wakers::default());
                    // One handle per half.
                    c.refs += if dir == Dir::Bi { 2 } else { 1 };
                    let sid = Sid {
                        id,
                        epoch: c.epoch,
                        early,
                    };
                    st.mark(h);
                    Poll::Ready(Ok(sid))
                }
                None => {
                    c.waiters.push(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }

    pub(crate) async fn read(self: &Rc<Self>, h: usize, s: Sid) -> io::Result<Option<Bytes>> {
        let id = s.id;
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if c.stale(s) {
                return Poll::Ready(Err(error::early_rejected()));
            }
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

    pub(crate) async fn write(self: &Rc<Self>, h: usize, s: Sid, data: &[u8]) -> io::Result<()> {
        let id = s.id;
        let mut off = 0;
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if c.stale(s) {
                return Poll::Ready(Err(error::early_rejected()));
            }
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

    pub(crate) fn finish(&self, h: usize, s: Sid) -> io::Result<()> {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        if c.stale(s) {
            return Err(error::early_rejected());
        }
        let r = c.c.send_stream(s.id).finish();
        st.mark(h);
        r.map_err(|_| error::lost(&ConnectionError::LocallyClosed))
    }

    pub(crate) fn reset(&self, h: usize, s: Sid, code: u32) {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        if !c.stale(s) {
            let _ = c.c.send_stream(s.id).reset(code.into());
        }
        st.mark(h);
    }

    pub(crate) fn stop(&self, h: usize, s: Sid, code: u32) {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        if !c.stale(s) {
            let _ = c.c.recv_stream(s.id).stop(code.into());
        }
        st.mark(h);
    }

    pub(crate) fn close_conn(&self, h: usize, code: u32, reason: &[u8]) {
        let mut st = self.st.borrow_mut();
        let c = st.conn(h);
        c.c.close(Instant::now(), code.into(), Bytes::copy_from_slice(reason));
        // quinn-proto has no event for a close of our own: the tasks that
        // wait on this connection's streams would wait for ever.
        c.lose(ConnectionError::LocallyClosed);
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

    pub(crate) fn forget_stream(&self, h: usize, s: Sid) {
        if let Some(Some(c)) = self.st.borrow_mut().conns.get_mut(h) {
            if !c.stale(s) {
                c.streams.remove(&s.id);
            }
        }
    }
}

thread_local! {
    /// The flag of the controller [`Closable`] built last on this thread,
    /// until the connection it was built for takes it.
    static BUILT: Cell<Option<Arc<AtomicBool>>> = const { Cell::new(None) };
}

/// Builds quinn-proto's default congestion controller (CUBIC) behind a
/// flag that lifts the congestion window once the connection is over.
///
/// quinn-proto holds a CONNECTION_CLOSE back like data when the
/// connection still has stream data or frames to send and its window is
/// full, and it stops the timers that would free the window: the close is
/// then never sent and the peer waits for its idle timeout. Seen when a
/// server closed a connection right after writing a large response.
pub(crate) struct Closable(Arc<quinn_proto::congestion::CubicConfig>);

impl Closable {
    pub(crate) fn factory() -> Arc<Self> {
        Arc::new(Closable(Arc::default()))
    }

    /// The flag of the controller built since the last call, if any.
    /// quinn-proto builds controllers inside the calls that make a
    /// connection or handle its datagrams, on the endpoint's thread.
    fn built() -> Option<Arc<AtomicBool>> {
        BUILT.with(Cell::take)
    }
}

impl quinn_proto::congestion::ControllerFactory for Closable {
    fn build(
        self: Arc<Self>,
        now: Instant,
        current_mtu: u16,
    ) -> Box<dyn quinn_proto::congestion::Controller> {
        let closing = Arc::new(AtomicBool::new(false));
        BUILT.with(|b| b.set(Some(closing.clone())));
        Box::new(ClosableController {
            inner: self.0.clone().build(now, current_mtu),
            closing,
        })
    }
}

struct ClosableController {
    inner: Box<dyn quinn_proto::congestion::Controller>,
    closing: Arc<AtomicBool>,
}

impl quinn_proto::congestion::Controller for ClosableController {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &quinn_proto::RttEstimator,
    ) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        if self.closing.load(Ordering::Relaxed) {
            u64::MAX / 2
        } else {
            self.inner.window()
        }
    }

    fn metrics(&self) -> quinn_proto::congestion::ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn quinn_proto::congestion::Controller> {
        Box::new(ClosableController {
            inner: self.inner.clone_box(),
            closing: self.closing.clone(),
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}
