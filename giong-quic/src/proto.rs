//! The quinn-proto engine behind the public API.
//!
//! One endpoint per giong worker: a driver task owns the socket and runs
//! quinn-proto; connection and stream handles reach the state through an
//! `Rc<RefCell<..>>`. Everything stays on the worker that made it, so there
//! is no lock or channel anywhere (quinn on Tokio has a mutex per
//! connection and channels between endpoint and connections). The one
//! atomic is the flag of [`Closable`], which quinn-proto requires to be
//! `Send`; only its own worker touches it.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use giong::net::{SendOptions, UdpSocket};
use quinn_proto::{
    ConnectionError, ConnectionHandle, DatagramEvent, Dir, EcnCodepoint, EndpointConfig, Event,
    ReadError, StreamEvent, StreamId, WriteError,
};

use crate::error;
use crate::route::{self, Forwarded, Inbox, Member};

/// Datagrams quinn-proto may put in one GSO send. quinn uses 10.
const GSO_SEGMENTS: usize = 10;
/// Datagrams handled before the driver runs connections again.
const RECV_BATCH: usize = 64;
/// Connections a server endpoint holds that the application has not taken
/// with `accept` yet, handshakes included. More are refused
/// (CONNECTION_REFUSED), so that a flood of handshakes, or an application
/// that stopped accepting (`max_connections` reached), cannot make the
/// endpoint hold connections without bound.
const MAX_UNACCEPTED: usize = 1024;
/// Past this many connections not yet accepted, a client whose address is
/// not validated gets a Retry (RFC 9000, section 8.1.2) before it costs a
/// connection: a flood of Initials from forged addresses, which never see
/// the Retry, then holds at most this many of the `MAX_UNACCEPTED`.
const RETRY_ABOVE: usize = 64;

pub(crate) struct Shared {
    sock: UdpSocket,
    st: RefCell<State>,
}

#[derive(Default)]
struct Wakers {
    read: Option<Waker>,
    write: Option<Waker>,
    /// The handles of the stream still held: two for a bidirectional
    /// stream, one for a unidirectional one. The entry goes with the last.
    halves: u8,
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
    /// In `State::accepted`, which holds a reference to it, waiting for the
    /// application to accept it.
    queued: bool,
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
    /// Handed over early (0-RTT) and its handshake not over yet: the
    /// client's address is not validated (counted in `State::unvalidated`).
    unvalidated: bool,
    /// The deadline of this connection's live entry in `State::timers`.
    armed: Option<Instant>,
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

    /// Server: takes the bidirectional streams that the datagram just
    /// handled opened, when it came before the end of the handshake.
    fn take_early(&mut self, handshaking: bool) {
        // Without 0-RTT keys nothing came in early data.
        if self.incoming && self.early && handshaking && self.c.has_0rtt() {
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
    /// The local address to send from: the one the peer sent to, on a
    /// wildcard bind (quinn-proto's `Transmit::src_ip`).
    src: Option<IpAddr>,
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
    /// Incoming connections the application has not taken yet.
    unaccepted: usize,
    /// Connections handed over early whose clients' addresses are not
    /// validated yet. They count with `unaccepted` towards Retry and
    /// refusal: a recorded 0-RTT flight replayed from spoofed addresses
    /// would otherwise take connections without a limit.
    unvalidated: usize,
    /// Server: the TLS configuration accepts early data.
    early_data: bool,
    scratch: Vec<u8>,
    sends: Vec<Send>,
    segments: usize,
    spare: Vec<Vec<u8>>,
    /// When connections next want `handle_timeout`, earliest first, by
    /// slot. A wake of the driver then costs what the connections it
    /// touches cost, not a look at every connection the endpoint holds:
    /// with 3,000 idle connections on one worker, two scans of them per
    /// wake made a stream round trip 30 times as expensive.
    ///
    /// An entry is live while its connection's `armed` names its
    /// deadline; others are left behind when a deadline moves, and are
    /// skipped when they come up (see [`State::arm`]).
    timers: BinaryHeap<Reverse<(Instant, usize)>>,
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

/// Gives a server connection to `accept`, or closes it when the endpoint is
/// closed and no one will accept it.
fn hand_over(
    conn: &mut Conn,
    i: usize,
    closed: bool,
    accepted: &mut VecDeque<usize>,
    accept_waker: &mut Option<Waker>,
    now: Instant,
) {
    if closed {
        conn.c.close(now, 0u32.into(), Bytes::new());
        conn.closing.store(true, Ordering::Relaxed);
        return;
    }
    conn.handed = true;
    conn.queued = true;
    conn.refs += 1;
    accepted.push_back(i);
    if let Some(w) = accept_waker.take() {
        w.wake();
    }
}

/// Adds `w`, unless the last one added wakes the same task: a future polled
/// again and again before anything happens would otherwise add itself
/// each time.
fn add_waker(list: &mut Vec<Waker>, w: &Waker) {
    if !list.last().is_some_and(|last| last.will_wake(w)) {
        list.push(w.clone());
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
        closing: Arc<AtomicBool>,
    ) -> usize {
        self.unaccepted += usize::from(incoming);
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
            queued: false,
            early,
            epoch: 0,
            early_bi: VecDeque::new(),
            closing,
            saw_early: false,
            unvalidated: false,
            armed: None,
        });
        if let Some(c) = &mut self.conns[slot] {
            // The first datagram may hold 0-RTT packets after the Initial.
            c.take_early(true);
        }
        self.mark(slot);
        slot
    }

    fn out(&mut self, t: &quinn_proto::Transmit, data: &[u8]) {
        let mut v = self.spare.pop().unwrap_or_default();
        v.extend_from_slice(data);
        self.sends.push(Send {
            data: v,
            to: t.destination,
            segment: 0,
            ecn: 0,
            src: t.src_ip,
        });
    }

    fn handle(
        &mut self,
        now: Instant,
        from: SocketAddr,
        local: Option<IpAddr>,
        ecn: u8,
        mut data: BytesMut,
        seg: Option<usize>,
    ) {
        let seg = seg.unwrap_or(data.len()).max(1);
        let ecn = EcnCodepoint::from_bits(ecn);
        while !data.is_empty() {
            let mut one = data.split_to(seg.min(data.len()));
            // With early data on, the 1-RTT packets that follow the
            // client's Finished in one datagram are handled apart, after
            // the handshake has ended, so that their streams do not count
            // as opened in 0-RTT data.
            let rest = if self.early_data {
                handshake_end(&one).map(|end| one.split_off(end))
            } else {
                None
            };
            self.handle_one(now, from, local, ecn, one);
            if let Some(rest) = rest {
                self.handle_one(now, from, local, ecn, rest);
            }
        }
    }

    fn handle_one(
        &mut self,
        now: Instant,
        from: SocketAddr,
        local: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        one: BytesMut,
    ) {
        {
            self.scratch.clear();
            match self
                .ep
                .handle(now, from, local, ecn, one, &mut self.scratch)
            {
                Some(DatagramEvent::NewConnection(inc)) => {
                    self.scratch.clear();
                    let pending = self.unaccepted + self.unvalidated;
                    // A closed endpoint, still draining its connections,
                    // refuses at once rather than leave the client to its
                    // timeout.
                    let answer = if self.closed || pending >= MAX_UNACCEPTED {
                        Some(self.ep.refuse(inc, &mut self.scratch))
                    } else if pending >= RETRY_ABOVE && !inc.remote_address_validated() {
                        match self.ep.retry(inc, &mut self.scratch) {
                            Ok(t) => Some(t),
                            Err(e) => {
                                self.ep.ignore(e.into_incoming());
                                None
                            }
                        }
                    } else {
                        let closing = Arc::new(AtomicBool::new(false));
                        // Only the answer to send is kept of a refusal.
                        let accepted = Closable::building_for(&closing, || {
                            self.ep
                                .accept(inc, now, &mut self.scratch, None)
                                .map_err(|e| e.response)
                        });
                        match accepted {
                            Ok((ch, c)) => {
                                let early = self.early_data;
                                self.insert(ch, c, true, early, closing);
                                None
                            }
                            Err(response) => response,
                        }
                    };
                    if let Some(t) = answer {
                        let d = std::mem::take(&mut self.scratch);
                        self.out(&t, &d[..t.size]);
                        self.scratch = d;
                    }
                }
                Some(DatagramEvent::ConnectionEvent(ch, ev)) => {
                    let slot = self.slots.get(ch.0).copied().unwrap_or(usize::MAX);
                    if let Some(Some(c)) = self.conns.get_mut(slot) {
                        if c.ch == ch {
                            let handshaking = c.c.is_handshaking();
                            let closing = c.closing.clone();
                            Closable::building_for(&closing, || c.c.handle_event(ev));
                            c.take_early(handshaking);
                            self.mark(slot);
                        }
                    }
                }
                Some(DatagramEvent::Response(t)) => {
                    let d = std::mem::take(&mut self.scratch);
                    self.out(&t, &d[..t.size]);
                    self.scratch = d;
                }
                None => {}
            }
        }
    }

    /// Timers, events and transmits of every connection that needs it.
    /// Returns the next deadline.
    fn process(&mut self, now: Instant) -> Option<Instant> {
        #[cfg(test)]
        tests::TURNS.with(|n| n.set(n.get() + 1));
        self.wake = false;
        while let Some(&Reverse((at, i))) = self.timers.peek() {
            if at > now {
                break;
            }
            self.timers.pop();
            let Some(Some(c)) = self.conns.get_mut(i) else {
                continue;
            };
            if c.armed != Some(at) {
                // Left behind by a deadline that moved, or by the slot's
                // previous connection.
                continue;
            }
            c.armed = None;
            #[cfg(test)]
            tests::LOOKS.with(|n| n.set(n.get() + 1));
            if c.c.poll_timeout().is_some_and(|t| t <= now) {
                let closing = c.closing.clone();
                Closable::building_for(&closing, || c.c.handle_timeout(now));
                self.mark(i);
            } else {
                // The deadline moved later since it was armed.
                self.arm(i);
            }
        }
        self.wake = false;
        while let Some(i) = self.dirty.pop() {
            self.process_one(i, now);
        }
        self.timers.peek().map(|&Reverse((at, _))| at)
    }

    /// Puts the deadline of the connection in slot `i` in `timers`, after
    /// anything that may have changed it: everything that touches a
    /// connection marks it, and [`process_one`](Self::process_one) arms
    /// it. A deadline later than the armed one waits for that entry to
    /// come up, which arms the connection again; so a busy connection
    /// leaves an entry behind only when its deadline comes earlier. The
    /// heap is rebuilt from the live entries when those left behind
    /// outnumber them.
    fn arm(&mut self, i: usize) {
        let Some(Some(c)) = self.conns.get_mut(i) else {
            return;
        };
        #[cfg(test)]
        tests::LOOKS.with(|n| n.set(n.get() + 1));
        let Some(at) = c.c.poll_timeout() else {
            return;
        };
        if c.armed.is_some_and(|armed| armed <= at) {
            return;
        }
        c.armed = Some(at);
        self.timers.push(Reverse((at, i)));
        if self.timers.len() > 2 * self.conns.len() + 64 {
            self.timers = self
                .conns
                .iter()
                .enumerate()
                .filter_map(|(i, c)| Some(Reverse((c.as_ref()?.armed?, i))))
                .collect();
        }
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
                    let closing = conn.closing.clone();
                    Closable::building_for(&closing, || conn.c.handle_event(cev));
                    again = true;
                }
            }
            while let Some(ev) = conn.c.poll() {
                match ev {
                    Event::HandshakeDataReady
                        if conn.incoming && conn.early && conn.c.has_0rtt() =>
                    {
                        // Early data on and taken: the application gets
                        // the connection now, to answer what came in 0-RTT.
                        // Without 0-RTT keys it waits for the handshake,
                        // so that a forged Initial never reaches it.
                        hand_over(
                            conn,
                            i,
                            self.closed,
                            &mut self.accepted,
                            &mut self.accept_waker,
                            now,
                        );
                        if conn.handed && !conn.connected && !conn.unvalidated {
                            conn.unvalidated = true;
                            self.unvalidated += 1;
                        }
                    }
                    Event::Connected => {
                        conn.connected = true;
                        if conn.unvalidated {
                            conn.unvalidated = false;
                            self.unvalidated -= 1;
                        }
                        if conn.incoming && !conn.handed {
                            hand_over(
                                conn,
                                i,
                                self.closed,
                                &mut self.accepted,
                                &mut self.accept_waker,
                                now,
                            );
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
                        // The packets go out from the buffer they were
                        // built in: it is swapped for a spare one rather
                        // than copied (a GSO batch is up to 10 datagrams).
                        let spare = self.spare.pop().unwrap_or_default();
                        let mut v = std::mem::replace(&mut self.scratch, spare);
                        v.truncate(t.size);
                        self.sends.push(Send {
                            data: v,
                            to: t.destination,
                            segment: t.segment_size.unwrap_or(0),
                            ecn: t.ecn.map_or(0, |e| e as u8),
                            src: t.src_ip,
                        });
                    }
                    None => break,
                }
            }
            if !again {
                break;
            }
        }
        if conn.queued && conn.lost.is_some() {
            // Lost while waiting to be accepted (idle timeout, the peer
            // closed or reset it): it leaves the accept queue and its place
            // in the bound, so that dead connections neither keep new
            // clients out nor come out of `accept`.
            conn.queued = false;
            conn.refs -= 1;
            self.unaccepted -= 1;
            if let Some(at) = self.accepted.iter().position(|&h| h == i) {
                self.accepted.remove(at);
            }
        }
        if conn.c.is_drained() && conn.refs == 0 {
            if conn.incoming && !conn.handed {
                self.unaccepted -= 1;
            }
            if conn.unvalidated {
                self.unvalidated -= 1;
            }
            self.conns[i] = None;
            self.free.push(i);
            return;
        }
        self.arm(i);
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
async fn drive(sh: Rc<Shared>, route: Option<(Member, Inbox)>) {
    let (mut member, mut inbox) = match route {
        Some((member, inbox)) => (Some(member), Some(inbox)),
        None => (None, None),
    };
    // The next deadline of the connections. It moves on almost every turn
    // (acknowledgement and loss timers); an alarm stays armed from one wait
    // to the next and only retargets its entry, where a new sleep per wait
    // inserted a timer and removed it again.
    let mut alarm = giong::time::Alarm::new();
    loop {
        let now = Instant::now();
        let (deadline, sends, done) = {
            let mut st = sh.st.borrow_mut();
            let deadline = st.process(now);
            let done = st.closed && st.conns.iter().all(Option::is_none);
            (deadline, std::mem::take(&mut st.sends), done)
        };
        for s in sends {
            // Polled once here: a send the socket has room for completes in
            // that poll (giong sends a datagram at once when it can), which
            // spares a task per datagram batch. One that has to wait goes on
            // in a task of its own; that task's first poll registers its
            // waker, so the no-op one used here is never needed.
            let mut send = Box::pin(send(sh.clone(), s));
            let mut cx = std::task::Context::from_waker(Waker::noop());
            if send.as_mut().poll(&mut cx).is_pending() {
                #[cfg(test)]
                tests::SPAWNED_SENDS.with(|n| n.set(n.get() + 1));
                giong::spawn(send);
            }
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
            poll_fn(|cx| {
                if let Poll::Ready(r) = recv.as_mut().poll(cx) {
                    return Poll::Ready(Woke::Datagram(r));
                }
                if let Poll::Ready(Some(datagram)) = passed.as_mut().poll(cx) {
                    return Poll::Ready(Woke::Forwarded(datagram));
                }
                // Without a deadline an armed alarm may still go off at
                // its old one: a spare turn, nothing more.
                if let Some(at) = deadline {
                    if alarm.poll_deadline(at, cx).is_ready() {
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
                    sh.st
                        .borrow_mut()
                        .handle(now, d.from, d.local, d.ecn, d.data, d.segment);
                    n += 1;
                    next = match inbox.as_mut() {
                        Some(inbox) if n < RECV_BATCH => inbox.try_recv(),
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
                let (seg, ecn, local) = (dg.segment_size(), dg.ecn(), dg.local_ip());
                match member.as_mut() {
                    Some(member) => {
                        deliver(&sh, member, now, (from, local), ecn, dg.as_slice(), seg);
                    }
                    None => {
                        let data = BytesMut::from(dg.as_slice());
                        drop(dg);
                        sh.st.borrow_mut().handle(now, from, local, ecn, data, seg);
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
        // What other workers passed on gets a batch every round too: the
        // wait above looks at it only when the socket has nothing, so
        // under load the connections that moved here would starve.
        if let Some(inbox) = inbox.as_mut() {
            for _ in 0..RECV_BATCH {
                let Some(d) = inbox.try_recv() else { break };
                sh.st
                    .borrow_mut()
                    .handle(now, d.from, d.local, d.ecn, d.data, d.segment);
            }
        }
    }
}

/// Sends one transmit, then gives its buffer back to the spare ones.
async fn send(sh: Rc<Shared>, s: Send) {
    let mut options = SendOptions::new().segment_size(s.segment).ecn(s.ecn);
    if let Some(src) = s.src {
        options = options.source(src);
    }
    let (sent, mut v) = sh.sock.send_to_with(s.data, s.to, &options).await;
    if s.segment > 0 && s.segment < v.len() && gso_refused(&sent) {
        // The route may refuse segmentation offload (no
        // checksum offload on a tun, VPN or veth device). EINVAL
        // also comes from a source address the machine no
        // longer has, so one datagram is sent alone first: if
        // that goes, it was the offload, and every send is one
        // datagram from now on.
        let options = options.segment_size(0);
        let mut ones = v.chunks(s.segment);
        let first = ones.next().unwrap_or_default().to_vec();
        let (alone, _) = sh.sock.send_to_with(first, s.to, &options).await;
        if alone.is_ok() {
            sh.st.borrow_mut().segments = 1;
            for one in ones {
                let _ = sh.sock.send_to_with(one.to_vec(), s.to, &options).await;
            }
        }
    }
    // Other send errors are losses, which QUIC recovers from.
    v.clear();
    sh.st.borrow_mut().spare.push(v);
}

/// A receive of a server endpoint: handled here, or passed on to the
/// worker that owns its connection (see [`route`]).
fn deliver(
    sh: &Shared,
    member: &mut Member,
    now: Instant,
    (from, local): (SocketAddr, Option<IpAddr>),
    ecn: u8,
    data: &[u8],
    seg: Option<usize>,
) {
    let me = member.worker();
    // Where the kernel does not keep a client on one worker (macOS), the
    // handshake's packets go where their connection is too.
    #[cfg(not(target_os = "linux"))]
    if let Some(worker) = member.long_owner(data) {
        if member.forward(worker, data, from, ecn, seg, local) {
            return;
        }
    }
    let whole = seg.unwrap_or(data.len()).max(1);
    // Datagrams received as one mostly go the same way; when they do not,
    // each goes its own.
    let (step, seg) = match route::owner_of_all(data, whole, me) {
        Ok(None) => {
            let data = BytesMut::from(data);
            sh.st.borrow_mut().handle(now, from, local, ecn, data, seg);
            return;
        }
        Ok(Some(_)) => (data.len().max(1), seg),
        Err(route::Mixed) => (whole, None),
    };
    for part in data.chunks(step) {
        let mut here = Some(part);
        if let Some(worker) = crate::native::cid::owner(part, me) {
            // Kept here when no endpoint of that worker is there: the
            // packet is then answered as any that is not known.
            if member.forward(worker, part, from, ecn, seg, local) {
                here = None;
            }
        }
        if let Some(part) = here {
            let data = BytesMut::from(part);
            sh.st.borrow_mut().handle(now, from, local, ecn, data, seg);
        }
    }
}

pub(crate) fn bind(
    addr: SocketAddr,
    server: Option<(Arc<quinn_proto::ServerConfig>, [u8; 32])>,
    early_data: bool,
) -> io::Result<Rc<Shared>> {
    let (server, reset_key) = match server {
        Some((config, key)) => (Some(config), Some(key)),
        None => (None, None),
    };
    let sock = UdpSocket::bind(addr)?;
    // GRO needs 64 KiB receive buffers; without them datagrams come one by
    // one, which is correct, only slower.
    let _ = sock.set_gro(true);
    // Windows before 11 and Server 2022 has no ECN: QUIC works without it,
    // and its ECN validation (RFC 9000, section 13.4.2) sees that none comes.
    if let Err(e) = sock.set_recv_ecn(true) {
        if !cfg!(windows) {
            return Err(e);
        }
    }
    // On a wildcard bind, each datagram's local address, so that the answer
    // leaves from the address the peer sent to: on a machine with several
    // addresses the route may pick another, which the peer drops.
    // Where the platform cannot report it, the route picks the source, as
    // before: the bind itself must not fail for that.
    if addr.ip().is_unspecified() {
        let _ = sock.set_recv_local_addr(true);
    }
    let segments = if sock.gso_supported() {
        GSO_SEGMENTS
    } else {
        1
    };
    // Connection IDs name this worker, so that another worker that gets
    // one of this endpoint's packets knows where it belongs.
    let worker = giong::worker_id() as u16;
    let mut config = match reset_key {
        Some(key) => EndpointConfig::new(Arc::new(ring::hmac::Key::new(
            ring::hmac::HMAC_SHA256,
            &key,
        ))),
        None => EndpointConfig::default(),
    };
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
            unaccepted: 0,
            unvalidated: 0,
            early_data,
            scratch: Vec::with_capacity(65_536),
            sends: Vec::new(),
            segments,
            spare: Vec::new(),
            timers: BinaryHeap::new(),
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
        // Connections no one will accept now are closed, instead of being
        // held until their peers time out.
        let now = Instant::now();
        while let Some(h) = st.accepted.pop_front() {
            st.unaccepted -= 1;
            let c = st.conn(h);
            c.queued = false;
            c.refs -= 1;
            if c.lost.is_none() {
                c.c.close(now, 0u32.into(), Bytes::new());
                c.closing.store(true, Ordering::Relaxed);
            }
            st.mark(h);
        }
        if let Some(w) = st.accept_waker.take() {
            w.wake();
        }
        st.mark(usize::MAX);
    }

    pub(crate) async fn accept(self: &Rc<Self>) -> Option<usize> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            if let Some(h) = st.accepted.pop_front() {
                st.unaccepted -= 1;
                st.conn(h).queued = false;
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
            let closing = Arc::new(AtomicBool::new(false));
            let (ch, c) =
                Closable::building_for(&closing, || st.ep.connect(Instant::now(), cfg, to, name))
                    .map_err(error::connect)?;
            let early = early && c.has_0rtt();
            let h = st.insert(ch, c, false, early, closing);
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
            // A handshake that finished is a connection, even if the peer
            // closed it before this task ran (a server that closes right
            // after accepting): the caller gets it, and its first use
            // reports the close, as `handshake` does.
            if c.connected {
                return Poll::Ready(Ok(()));
            }
            if let Some(e) = &c.lost {
                return Poll::Ready(Err(error::connect(e)));
            }
            add_waker(&mut c.waiters, cx.waker());
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
            add_waker(&mut c.waiters, cx.waker());
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
                (
                    c.c.streams().open(dir),
                    !c.incoming && c.early && !c.connected,
                )
            } else if dir == Dir::Bi && !c.early_bi.is_empty() {
                (c.early_bi.pop_front(), true)
            } else {
                (c.c.streams().accept(dir), false)
            };
            match id {
                Some(id) => {
                    let halves = if dir == Dir::Bi { 2 } else { 1 };
                    c.streams.insert(
                        id,
                        Wakers {
                            halves,
                            ..Wakers::default()
                        },
                    );
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
                    add_waker(&mut c.waiters, cx.waker());
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

    pub(crate) async fn stopped(self: &Rc<Self>, h: usize, s: Sid) -> io::Result<u64> {
        poll_fn(|cx| {
            let mut st = self.st.borrow_mut();
            let c = st.conn(h);
            if c.stale(s) {
                return Poll::Ready(Err(error::early_rejected()));
            }
            match c.c.send_stream(s.id).stopped() {
                Ok(Some(code)) => Poll::Ready(Ok(code.into_inner())),
                Ok(None) => match &c.lost {
                    Some(e) => Poll::Ready(Err(error::lost(e))),
                    None => {
                        // `StreamEvent::Stopped` wakes the writer.
                        c.streams.entry(s.id).or_default().write = Some(cx.waker().clone());
                        Poll::Pending
                    }
                },
                Err(_) => Poll::Ready(Err(match &c.lost {
                    Some(e) => error::lost(e),
                    None => error::lost(&ConnectionError::LocallyClosed),
                })),
            }
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
        r.map_err(|e| match e {
            // The peer asked to stop sending: its code, as `write` gives it.
            quinn_proto::FinishError::Stopped(code) => error::reset(code.into_inner()),
            quinn_proto::FinishError::ClosedStream => error::lost(&ConnectionError::LocallyClosed),
        })
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
            // Like any close, not held back by congestion control.
            c.closing.store(true, Ordering::Relaxed);
        }
        st.mark(h);
    }

    /// One half of a stream is dropped: its waker goes, and the entry
    /// with the last half, so that the other half is still woken.
    pub(crate) fn forget_stream(&self, h: usize, s: Sid, write: bool) {
        if let Some(Some(c)) = self.st.borrow_mut().conns.get_mut(h) {
            if !c.stale(s) {
                if let Some(w) = c.streams.get_mut(&s.id) {
                    if write {
                        w.write = None;
                    } else {
                        w.read = None;
                    }
                    w.halves = w.halves.saturating_sub(1);
                    if w.halves == 0 {
                        c.streams.remove(&s.id);
                    }
                }
            }
        }
    }
}

thread_local! {
    /// The flag of the connection whose call into quinn-proto is running on
    /// this thread, for the controllers [`Closable`] builds during it.
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
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

    /// Runs a call into quinn-proto for the connection whose flag is
    /// `closing`: every controller built during it (the first one, and the
    /// new ones of a path change or of its revert) shares that one flag, so
    /// that setting it covers whichever controller the connection ends with.
    /// quinn-proto builds controllers only inside such calls, on the
    /// endpoint's thread.
    fn building_for<R>(closing: &Arc<AtomicBool>, call: impl FnOnce() -> R) -> R {
        CURRENT.with(|c| *c.borrow_mut() = Some(closing.clone()));
        let r = call();
        CURRENT.with(|c| *c.borrow_mut() = None);
        r
    }
}

impl quinn_proto::congestion::ControllerFactory for Closable {
    fn build(
        self: Arc<Self>,
        now: Instant,
        current_mtu: u16,
    ) -> Box<dyn quinn_proto::congestion::Controller> {
        // Outside a call of `building_for` (none is known) the controller
        // gets a flag of its own, which nothing sets.
        let closing = CURRENT
            .with(|c| c.borrow().clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
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

/// Whether a send failed because the route refuses `UDP_SEGMENT`: Linux
/// answers `EIO` where the device has no checksum offload, `EINVAL` where
/// the segments do not fit it.
fn gso_refused(sent: &io::Result<usize>) -> bool {
    #[cfg(unix)]
    const REFUSED: [i32; 2] = [5, 22]; // EIO, EINVAL
                                       // WSAEINVAL, and WSAEOPNOTSUPP from a route that cannot segment.
    #[cfg(windows)]
    const REFUSED: [i32; 2] = [10022, 10045];
    matches!(sent, Err(e) if e.raw_os_error().is_some_and(|c| REFUSED.contains(&c)))
}

/// Where the last long-header Handshake packet of a datagram ends, when
/// short-header (1-RTT) packets follow it (RFC 9000, sections 12.2 and
/// 17.2). `None` when there is no such boundary or the datagram cannot be
/// read; quinn-proto then gets it whole.
fn handshake_end(d: &[u8]) -> Option<usize> {
    fn varint(d: &[u8], at: &mut usize) -> Option<u64> {
        let first = *d.get(*at)?;
        let len = 1usize << (first >> 6);
        let bytes = d.get(*at..*at + len)?;
        let mut v = u64::from(first & 0x3f);
        for b in &bytes[1..] {
            v = v << 8 | u64::from(*b);
        }
        *at += len;
        Some(v)
    }
    let mut at = 0;
    let mut end = None;
    while at < d.len() {
        let first = d[at];
        if first & 0x80 == 0 {
            // A short header runs to the end of the datagram.
            return end.filter(|&e| e == at);
        }
        let version = u32::from_be_bytes(d.get(at + 1..at + 5)?.try_into().ok()?);
        if version != 1 {
            return None;
        }
        let ty = (first >> 4) & 0x03;
        let mut p = at + 5;
        let dcid = usize::from(*d.get(p)?);
        p += 1 + dcid;
        let scid = usize::from(*d.get(p)?);
        p += 1 + scid;
        if ty == 0 {
            let token = usize::try_from(varint(d, &mut p)?).ok()?;
            p = p.checked_add(token)?;
        }
        if ty == 3 {
            // Retry: no length, and nothing follows it.
            return None;
        }
        let len = usize::try_from(varint(d, &mut p)?).ok()?;
        at = p.checked_add(len)?;
        if at > d.len() {
            return None;
        }
        if ty == 2 {
            end = Some(at);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{add_waker, handshake_end, Arc, Waker};
    use std::cell::Cell;
    use std::time::Duration;

    /// Driver turns of both endpoints for one stream round trip, at most.
    const TURNS_A_ROUND_TRIP: u64 = 4;

    thread_local! {
        /// How many times the drivers of this thread asked a connection
        /// for its next deadline.
        pub(super) static LOOKS: Cell<u64> = const { Cell::new(0) };
        /// How many times the drivers of this thread ran their connections.
        pub(super) static TURNS: Cell<u64> = const { Cell::new(0) };
        /// Sends that could not complete at once and went on in a task.
        pub(super) static SPAWNED_SENDS: Cell<u64> = const { Cell::new(0) };
    }

    /// TLS configurations of a server and a client that trusts it.
    fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = cert.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key.into())
            .unwrap();
        server_tls.alpn_protocols = vec![b"t".to_vec()];
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"t".to_vec()];
        (server_tls, client_tls)
    }

    /// An echo server on this worker: its address.
    fn echo_server(server_tls: rustls::ServerConfig) -> std::net::SocketAddr {
        let server = crate::Endpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            crate::ServerConfig::new(server_tls).unwrap(),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        giong::spawn(async move {
            while let Some(conn) = server.accept().await {
                giong::spawn(async move {
                    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let data = recv.read_to_end(64).await.unwrap();
                        send.write_all(&data).await.unwrap();
                        send.finish().unwrap();
                    }
                });
            }
        });
        addr
    }

    #[test]
    fn a_busy_connection_costs_no_look_at_the_idle_ones() {
        let (server_tls, client_tls) = tls();
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let addr = echo_server(server_tls);
                let client = crate::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                let config = crate::ClientConfig::new(client_tls).unwrap();
                let mut idle = Vec::new();
                for _ in 0..100 {
                    idle.push(client.connect(addr, "localhost", &config).await.unwrap());
                }
                let busy = client.connect(addr, "localhost", &config).await.unwrap();
                giong::time::sleep(Duration::from_millis(100)).await;
                let before = LOOKS.with(Cell::get);
                for _ in 0..100 {
                    let (mut send, mut recv) = busy.open_bi().await.unwrap();
                    send.write_all(b"ping").await.unwrap();
                    send.finish().unwrap();
                    assert_eq!(recv.read_to_end(64).await.unwrap(), b"ping");
                }
                // Both endpoints, 101 connections each: looking at all of
                // them at every wake of the drivers was over 400 looks a
                // round trip.
                let looks = (LOOKS.with(Cell::get) - before) / 100;
                assert!(looks < 40, "{looks} looks at deadlines a round trip");
                drop(idle);
            })
            .unwrap();
    }

    #[test]
    fn a_round_trip_costs_few_driver_turns_and_sends_at_once() {
        let (server_tls, client_tls) = tls();
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let addr = echo_server(server_tls);
                let client = crate::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                let config = crate::ClientConfig::new(client_tls).unwrap();
                let conn = client.connect(addr, "localhost", &config).await.unwrap();
                giong::time::sleep(Duration::from_millis(100)).await;
                let turns = TURNS.with(Cell::get);
                let spawned = SPAWNED_SENDS.with(Cell::get);
                for _ in 0..100 {
                    let (mut send, mut recv) = conn.open_bi().await.unwrap();
                    send.write_all(b"ping").await.unwrap();
                    send.finish().unwrap();
                    assert_eq!(recv.read_to_end(64).await.unwrap(), b"ping");
                }
                // Both endpoints' drivers: two turns each (the datagram
                // in, the stream's answer out).
                let turns = (TURNS.with(Cell::get) - turns) / 100;
                assert!(
                    turns <= TURNS_A_ROUND_TRIP,
                    "{turns} driver turns a round trip"
                );
                // A socket with room takes every send in the driver's own
                // turn: none needs a task of its own.
                let spawned = SPAWNED_SENDS.with(Cell::get) - spawned;
                assert!(
                    spawned < 10,
                    "{spawned} of about 400 sends went on in a task"
                );
            })
            .unwrap();
    }

    fn long(ty: u8, payload: usize) -> Vec<u8> {
        let mut p = vec![0xc0 | ty << 4, 0, 0, 0, 1, 4, 1, 2, 3, 4, 0];
        if ty == 0 {
            p.push(0);
        }
        p.extend_from_slice(&[0x40 | (payload >> 8) as u8, payload as u8]);
        p.extend(std::iter::repeat_n(0, payload));
        p
    }

    #[test]
    // Two different wakers are needed, which `Waker::noop` cannot give.
    #[allow(clippy::manual_noop_waker)]
    fn a_task_polled_again_is_not_added_again() {
        struct Task;
        impl std::task::Wake for Task {
            fn wake(self: Arc<Self>) {}
        }
        let a = Waker::from(Arc::new(Task));
        let b = Waker::from(Arc::new(Task));
        let mut list = Vec::new();
        for _ in 0..100 {
            add_waker(&mut list, &a);
        }
        assert_eq!(list.len(), 1);
        add_waker(&mut list, &b);
        add_waker(&mut list, &b);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn a_refused_segmentation_offload_is_told_from_other_errors() {
        use std::io::{Error, ErrorKind};
        #[cfg(unix)]
        let (refused, other) = ([5, 22], 11);
        // WSAEINVAL and WSAEOPNOTSUPP; WSAEWOULDBLOCK is something else.
        #[cfg(windows)]
        let (refused, other) = ([10022, 10045], 10035);
        for code in refused {
            assert!(super::gso_refused(&Err(Error::from_raw_os_error(code))));
        }
        assert!(!super::gso_refused(&Err(Error::from_raw_os_error(other))));
        assert!(!super::gso_refused(&Err(Error::from(ErrorKind::Other))));
        assert!(!super::gso_refused(&Ok(1200)));
    }

    #[test]
    fn the_boundary_after_a_handshake_packet_is_found() {
        let mut d = long(0, 30);
        d.extend(long(2, 50));
        let end = d.len();
        d.extend_from_slice(&[0x40, 1, 2, 3, 4, 9, 9, 9]);
        assert_eq!(handshake_end(&d), Some(end));
        // Nothing to split: no 1-RTT packet, or no Handshake packet before it.
        assert_eq!(handshake_end(&d[..end]), None);
        let mut zero_rtt = long(0, 30);
        zero_rtt.extend(long(1, 40));
        zero_rtt.extend_from_slice(&[0x40, 1, 2]);
        assert_eq!(handshake_end(&zero_rtt), None);
        // A length past the end, or another version.
        assert_eq!(handshake_end(&d[..end - 1]), None);
        let mut other = d.clone();
        other[4] = 2;
        assert_eq!(handshake_end(&other), None);
        assert_eq!(handshake_end(&[]), None);
    }
}
