//! The simulated kernel objects: sockets (TCP, UDP, Unix streams), the
//! links between them, epoll instances and eventfds.
//!
//! Every object has a virtual id, given out in order, so iteration and the
//! trace never depend on real descriptor numbers. A descriptor of the
//! simulated process that refers to an object is a real descriptor number
//! (a duplicate of `/dev/null`) mapped to the object, so numbers never
//! collide with the real files the process opens.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::world::World;

pub(crate) type ObjId = u64;

pub(crate) const IN: u32 = libc::EPOLLIN as u32;
pub(crate) const OUT: u32 = libc::EPOLLOUT as u32;
pub(crate) const ERR: u32 = libc::EPOLLERR as u32;
pub(crate) const HUP: u32 = libc::EPOLLHUP as u32;
pub(crate) const RDHUP: u32 = libc::EPOLLRDHUP as u32;
pub(crate) const ET: u32 = libc::EPOLLET as u32;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Addr {
    Inet(SocketAddr),
    Unix(Vec<u8>),
}

impl std::fmt::Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Addr::Inet(a) => write!(f, "{a}"),
            Addr::Unix(p) => write!(f, "unix:{}", String::from_utf8_lossy(p)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Event {
    /// A connection attempt reaches `to`.
    Syn {
        client: ObjId,
        to: Addr,
        attempt: u32,
    },
    /// Segments of a stream may have arrived.
    Deliver { conn: usize, dir: usize },
    /// A reset arrives at side `side` of `conn`.
    Rst { conn: usize, side: usize },
    /// A datagram arrives.
    Datagram { to: Addr, from: Addr, data: Vec<u8> },
}

#[derive(Debug)]
enum Seg {
    Data(Vec<u8>),
    Fin,
}

#[derive(Default, Debug)]
pub(crate) struct Flow {
    /// Arrived, not yet read by the receiver.
    pub(crate) buf: VecDeque<u8>,
    flight: VecDeque<(u64, Seg)>,
    flight_bytes: usize,
    last_at: u64,
    fin_sent: bool,
    pub(crate) fin_arrived: bool,
}

#[derive(Debug)]
pub(crate) struct Conn {
    /// The socket object at each side, `None` once that side has closed.
    socks: [Option<ObjId>; 2],
    /// `flows[i]`: data from side `i` to side `1 - i`.
    flows: [Flow; 2],
    /// A reset has reached side `i`.
    reset: [bool; 2],
    addrs: [Addr; 2],
}

#[derive(Debug)]
pub(crate) enum State {
    Fresh,
    Listening {
        queue: VecDeque<ObjId>,
        backlog: usize,
    },
    Connecting,
    Connected {
        conn: usize,
        side: usize,
    },
    /// Connect failed, or the connection is gone.
    Dead,
    Dgram {
        peer: Option<Addr>,
        queue: VecDeque<(Addr, Vec<u8>)>,
    },
}

#[derive(Debug)]
pub(crate) struct Sock {
    pub(crate) domain: i32,
    pub(crate) stream: bool,
    pub(crate) local: Option<Addr>,
    pub(crate) state: State,
    pub(crate) so_error: i32,
    pub(crate) reuse_port: bool,
    pub(crate) reuse_addr: bool,
    pub(crate) v6only: bool,
    pub(crate) nodelay: bool,
    pub(crate) shut_rd: bool,
    pub(crate) shut_wr: bool,
    /// A reset was reported once; later reads see end of stream.
    pub(crate) reset_reported: bool,
}

#[derive(Debug)]
pub(crate) struct Interest {
    pub(crate) events: u32,
    pub(crate) data: u64,
    pub(crate) seen: u64,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Sock(Sock),
    EventFd { count: u64, semaphore: bool },
    Epoll { list: BTreeMap<ObjId, Interest> },
}

#[derive(Debug)]
pub(crate) struct Obj {
    pub(crate) kind: Kind,
    /// Descriptors that refer to it.
    pub(crate) refs: usize,
    pub(crate) nonblock: bool,
    /// Bumped on every change that can make it ready (for edge triggering).
    pub(crate) ev: u64,
}

#[derive(Default)]
pub(crate) struct Net {
    next_id: ObjId,
    pub(crate) objs: BTreeMap<ObjId, Obj>,
    pub(crate) fds: HashMap<i32, ObjId>,
    pub(crate) conns: Vec<Conn>,
    /// Bound addresses (TCP and UDP kept apart) and the sockets on them.
    bound: BTreeMap<(bool, Addr), Vec<ObjId>>,
    next_port: u16,
    /// Hosts cut off from each other: a pair of IPs.
    pub(crate) partitions: Vec<(IpAddr, IpAddr)>,
    /// When the last datagram on each (from, to) path arrives: a path
    /// keeps its order unless a reorder fault moves one.
    pub(crate) dgram_last: BTreeMap<(Addr, Addr), u64>,
}

pub(crate) fn sock_kind(domain: i32, stream: bool) -> Kind {
    Kind::Sock(Sock {
        domain,
        stream,
        local: None,
        state: if stream {
            State::Fresh
        } else {
            State::Dgram {
                peer: None,
                queue: VecDeque::new(),
            }
        },
        so_error: 0,
        reuse_port: false,
        reuse_addr: false,
        v6only: false,
        nodelay: false,
        shut_rd: false,
        shut_wr: false,
        reset_reported: false,
    })
}

fn loopback_for(domain: i32) -> IpAddr {
    if domain == libc::AF_INET6 {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

impl Net {
    pub(crate) fn add(&mut self, kind: Kind, nonblock: bool) -> ObjId {
        self.next_id += 1;
        let id = self.next_id;
        self.objs.insert(
            id,
            Obj {
                kind,
                refs: 0,
                nonblock,
                ev: 1,
            },
        );
        id
    }

    pub(crate) fn obj(&self, fd: i32) -> Option<ObjId> {
        self.fds.get(&fd).copied()
    }

    pub(crate) fn sock(&self, id: ObjId) -> Option<&Sock> {
        match self.objs.get(&id).map(|o| &o.kind) {
            Some(Kind::Sock(s)) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn sock_mut(&mut self, id: ObjId) -> Option<&mut Sock> {
        match self.objs.get_mut(&id).map(|o| &mut o.kind) {
            Some(Kind::Sock(s)) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn bump(&mut self, id: ObjId) {
        if let Some(o) = self.objs.get_mut(&id) {
            o.ev += 1;
        }
    }

    fn window(&self) -> usize {
        256 * 1024
    }

    /// What `epoll` would report for the object now.
    pub(crate) fn readiness(&self, id: ObjId) -> u32 {
        let Some(o) = self.objs.get(&id) else {
            return HUP | ERR;
        };
        match &o.kind {
            Kind::EventFd { count, .. } => (if *count > 0 { IN } else { 0 }) | OUT,
            Kind::Epoll { .. } => {
                if self.epoll_has_ready(id) {
                    IN
                } else {
                    0
                }
            }
            Kind::Sock(s) => {
                let err = if s.so_error != 0 { ERR } else { 0 };
                match &s.state {
                    State::Fresh => OUT | HUP | err,
                    State::Listening { queue, .. } => {
                        if queue.is_empty() {
                            0
                        } else {
                            IN
                        }
                    }
                    State::Connecting => err,
                    State::Dead => IN | OUT | HUP | err,
                    State::Dgram { queue, .. } => {
                        (if queue.is_empty() { 0 } else { IN }) | OUT | err
                    }
                    State::Connected { conn, side } => {
                        let c = &self.conns[*conn];
                        let incoming = &c.flows[1 - side];
                        let outgoing = &c.flows[*side];
                        let mut r = err;
                        if c.reset[*side] {
                            r |= IN | OUT | HUP | ERR;
                        }
                        if !incoming.buf.is_empty() || incoming.fin_arrived {
                            r |= IN;
                        }
                        if incoming.fin_arrived {
                            r |= RDHUP;
                        }
                        if incoming.fin_arrived && outgoing.fin_sent {
                            r |= HUP;
                        }
                        if outgoing.buf.len() + outgoing.flight_bytes < self.window() {
                            r |= OUT;
                        }
                        r
                    }
                }
            }
        }
    }

    pub(crate) fn epoll_ready(&self, ep: ObjId) -> Vec<(ObjId, u32, u64)> {
        let mut out = Vec::new();
        if let Some(Obj {
            kind: Kind::Epoll { list },
            ..
        }) = self.objs.get(&ep)
        {
            for (id, i) in list {
                let r = self.readiness(*id) & (i.events | ERR | HUP);
                if r == 0 {
                    continue;
                }
                let ev = self.objs.get(id).map_or(0, |o| o.ev);
                if i.events & ET != 0 && ev == i.seen {
                    continue;
                }
                out.push((*id, r, i.data));
            }
        }
        out
    }

    pub(crate) fn epoll_has_ready(&self, ep: ObjId) -> bool {
        !self.epoll_ready(ep).is_empty()
    }

    /// Marks the events of `id` in `ep` as reported (edge triggering).
    pub(crate) fn epoll_seen(&mut self, ep: ObjId, id: ObjId) {
        let ev = self.objs.get(&id).map_or(0, |o| o.ev);
        if let Some(Obj {
            kind: Kind::Epoll { list },
            ..
        }) = self.objs.get_mut(&ep)
        {
            if let Some(i) = list.get_mut(&id) {
                i.seen = ev;
            }
        }
    }

    // --------------------------------------------------------------- binding

    pub(crate) fn ephemeral(&mut self, stream: bool, ip: IpAddr) -> Option<SocketAddr> {
        for _ in 0..30000 {
            if self.next_port < 32768 {
                self.next_port = 32768;
            }
            let port = self.next_port;
            self.next_port = self.next_port.wrapping_add(1);
            let addr = SocketAddr::new(ip, port);
            if !self.port_used(stream, port) {
                return Some(addr);
            }
        }
        None
    }

    fn port_used(&self, stream: bool, port: u16) -> bool {
        self.bound
            .keys()
            .any(|(s, a)| *s == stream && matches!(a, Addr::Inet(x) if x.port() == port))
    }

    /// Binds socket `id` to `addr`; port 0 picks a free port.
    pub(crate) fn bind(&mut self, id: ObjId, addr: Addr) -> Result<(), i32> {
        let s = self.sock(id).ok_or(libc::ENOTSOCK)?;
        if s.local.is_some() {
            return Err(libc::EINVAL);
        }
        let stream = s.stream;
        let addr = match addr {
            Addr::Inet(a) if a.port() == 0 => {
                Addr::Inet(self.ephemeral(stream, a.ip()).ok_or(libc::EADDRINUSE)?)
            }
            other => other,
        };
        if let Addr::Inet(a) = &addr {
            for other in self.overlapping(stream, a) {
                if !self.may_share(id, other, false) {
                    return Err(libc::EADDRINUSE);
                }
            }
        } else if let Addr::Unix(path) = &addr {
            if self.bound.contains_key(&(stream, addr.clone())) {
                return Err(libc::EADDRINUSE);
            }
            // A path name: the socket file appears in the file system, as
            // on Linux (abstract names, starting with a zero byte, do not).
            if !path.is_empty() && path[0] != 0 {
                crate::raw::make_socket_file(path)?;
            }
        }
        self.bound
            .entry((stream, addr.clone()))
            .or_default()
            .push(id);
        self.sock_mut(id).unwrap().local = Some(addr);
        Ok(())
    }

    /// Sockets bound to the same port as `a`, on an address that overlaps.
    pub(crate) fn overlapping(&self, stream: bool, a: &SocketAddr) -> Vec<ObjId> {
        let mut out = Vec::new();
        for ((st, b), ids) in &self.bound {
            let Addr::Inet(b) = b else { continue };
            if *st != stream || b.port() != a.port() || b.is_ipv4() != a.is_ipv4() {
                continue;
            }
            if b.ip() == a.ip() || b.ip().is_unspecified() || a.ip().is_unspecified() {
                out.extend(ids.iter().copied());
            }
        }
        out
    }

    /// Linux's rules for two sockets on one port: both set SO_REUSEPORT;
    /// or both set SO_REUSEADDR and (for TCP) neither listens.
    /// `listening` says `a` is about to listen.
    pub(crate) fn may_share(&self, a: ObjId, b: ObjId, listening: bool) -> bool {
        if a == b {
            return true;
        }
        let (Some(x), Some(y)) = (self.sock(a), self.sock(b)) else {
            return true;
        };
        if x.reuse_port && y.reuse_port {
            return true;
        }
        let y_listens = matches!(y.state, State::Listening { .. });
        x.reuse_addr && y.reuse_addr && (!x.stream || (!listening && !y_listens))
    }

    fn unbind(&mut self, id: ObjId) {
        let Some(s) = self.sock(id) else { return };
        let Some(local) = s.local.clone() else { return };
        let key = (s.stream, local);
        if let Some(v) = self.bound.get_mut(&key) {
            v.retain(|o| *o != id);
            if v.is_empty() {
                self.bound.remove(&key);
            }
        }
    }

    /// Sockets bound where a packet for `to` lands.
    fn lookup(&self, stream: bool, to: &Addr) -> Vec<ObjId> {
        let mut out = Vec::new();
        for ((st, b), ids) in &self.bound {
            if *st != stream {
                continue;
            }
            let hit = match (b, to) {
                (Addr::Unix(x), Addr::Unix(y)) => x == y,
                (Addr::Inet(b), Addr::Inet(t)) => {
                    b.port() == t.port()
                        && b.is_ipv4() == t.is_ipv4()
                        && (b.ip() == t.ip() || b.ip().is_unspecified())
                }
                _ => false,
            };
            if hit {
                out.extend(ids.iter().copied());
            }
        }
        out
    }

    pub(crate) fn partitioned(&self, a: &Addr, b: &Addr) -> bool {
        let (Addr::Inet(a), Addr::Inet(b)) = (a, b) else {
            return false;
        };
        self.partitions
            .iter()
            .any(|(x, y)| (*x == a.ip() && *y == b.ip()) || (*x == b.ip() && *y == a.ip()))
    }
}

/// Picks the address a connecting socket gets: its bound address, or the
/// loopback address with a fresh port.
fn local_for(w: &mut World, id: ObjId, to: &Addr) -> Result<Addr, i32> {
    let s = w.net.sock(id).ok_or(libc::ENOTSOCK)?;
    if let Some(l) = &s.local {
        return Ok(l.clone());
    }
    let domain = s.domain;
    let stream = s.stream;
    let addr = match to {
        Addr::Unix(_) => Addr::Unix(Vec::new()),
        Addr::Inet(t) => {
            let ip = if t.ip().is_loopback() || t.ip().is_unspecified() {
                loopback_for(domain)
            } else {
                t.ip()
            };
            let a = w.net.ephemeral(stream, ip).ok_or(libc::EADDRNOTAVAIL)?;
            let a = Addr::Inet(a);
            w.net.bound.entry((stream, a.clone())).or_default().push(id);
            a
        }
    };
    w.net.sock_mut(id).unwrap().local = Some(addr.clone());
    Ok(addr)
}

fn normalise(to: Addr) -> Addr {
    match to {
        Addr::Inet(a) if a.ip().is_unspecified() => Addr::Inet(SocketAddr::new(
            if a.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            },
            a.port(),
        )),
        other => other,
    }
}

impl World {
    pub(crate) fn latency(&mut self) -> u64 {
        let (lo, hi) = self.faults.latency_ns;
        self.between(lo, hi)
    }

    // ------------------------------------------------------------ streams

    pub(crate) fn listen(&mut self, id: ObjId, backlog: i32) -> Result<(), i32> {
        let needs_bind = match self.net.sock(id) {
            None => return Err(libc::ENOTSOCK),
            Some(s) if !s.stream => return Err(libc::EOPNOTSUPP),
            Some(s) => s.local.is_none(),
        };
        if needs_bind {
            let domain = self.net.sock(id).unwrap().domain;
            self.net
                .bind(id, Addr::Inet(SocketAddr::new(loopback_for(domain), 0)))?;
        }
        if let Some(Addr::Inet(a)) = self.net.sock(id).and_then(|s| s.local.clone()) {
            for other in self.net.overlapping(true, &a) {
                let other_listens = matches!(
                    self.net.sock(other).map(|s| &s.state),
                    Some(State::Listening { .. })
                );
                if other != id && other_listens && !self.net.may_share(id, other, true) {
                    return Err(libc::EADDRINUSE);
                }
            }
        }
        let s = self.net.sock_mut(id).unwrap();
        let backlog = backlog.clamp(1, 4096) as usize;
        match &mut s.state {
            State::Listening { backlog: b, .. } => *b = backlog,
            st @ State::Fresh => {
                *st = State::Listening {
                    queue: VecDeque::new(),
                    backlog,
                }
            }
            _ => return Err(libc::EINVAL),
        }
        Ok(())
    }

    /// Starts a connect. `Ok(false)`: in progress; `Ok(true)`: connected
    /// (Unix sockets connect at once, as on Linux).
    pub(crate) fn connect_stream(&mut self, id: ObjId, to: Addr) -> Result<bool, i32> {
        let to = normalise(to);
        match self.net.sock(id).map(|s| &s.state) {
            None => return Err(libc::ENOTSOCK),
            Some(State::Fresh) => {}
            Some(State::Connecting) => return Err(libc::EALREADY),
            Some(State::Connected { .. }) => return Err(libc::EISCONN),
            Some(_) => return Err(libc::EINVAL),
        }
        if let Addr::Unix(path) = &to {
            let listening = self.net.lookup(true, &to).into_iter().any(|l| {
                matches!(
                    self.net.sock(l).map(|s| &s.state),
                    Some(State::Listening { .. })
                )
            });
            if !listening {
                let exists = path.first() == Some(&0) || crate::raw::path_exists(path);
                return Err(if exists {
                    libc::ECONNREFUSED
                } else {
                    libc::ENOENT
                });
            }
            local_for(self, id, &to)?;
            self.net.sock_mut(id).unwrap().state = State::Connecting;
            self.syn_arrives(id, to, 6);
            return match self.net.sock(id).map(|s| &s.state) {
                Some(State::Connected { .. }) => Ok(true),
                _ => {
                    // The backlog is full: Linux says EAGAIN for Unix sockets.
                    let s = self.net.sock_mut(id).unwrap();
                    s.state = State::Fresh;
                    s.so_error = 0;
                    Err(libc::EAGAIN)
                }
            };
        }
        local_for(self, id, &to)?;
        self.net.sock_mut(id).unwrap().state = State::Connecting;
        let lat = self.latency();
        let at = self.now + lat;
        self.schedule(
            at,
            Event::Syn {
                client: id,
                to,
                attempt: 0,
            },
        );
        Ok(false)
    }

    fn syn_arrives(&mut self, client: ObjId, to: Addr, attempt: u32) {
        let Some(State::Connecting) = self.net.sock(client).map(|s| &s.state) else {
            return;
        };
        let from = self.net.sock(client).and_then(|s| s.local.clone()).unwrap();
        let unix = matches!(to, Addr::Unix(_));
        let lost = !unix && self.net.partitioned(&from, &to)
            || !unix && {
                let p = self.faults.connect_drop;
                self.fault(p, "syn-lost")
            };
        if lost {
            // The SYN is lost: Linux retries after 1, 2, 4, ... seconds and
            // gives up after six retries.
            if attempt >= 6 {
                self.connect_failed(client, libc::ETIMEDOUT);
            } else {
                let at = self.now + (1_000_000_000u64 << attempt);
                self.schedule(
                    at,
                    Event::Syn {
                        client,
                        to,
                        attempt: attempt + 1,
                    },
                );
            }
            return;
        }
        let p = if unix {
            0.0
        } else {
            self.faults.connect_refuse
        };
        if self.fault(p, "connect-refused") {
            self.connect_failed(client, libc::ECONNREFUSED);
            return;
        }
        let listeners: Vec<ObjId> = self
            .net
            .lookup(true, &to)
            .into_iter()
            .filter(|l| {
                matches!(
                    self.net.sock(*l).map(|s| &s.state),
                    Some(State::Listening { .. })
                )
            })
            .collect();
        if listeners.is_empty() {
            self.connect_failed(client, libc::ECONNREFUSED);
            return;
        }
        let pick = listeners[(self.next_u64() % listeners.len() as u64) as usize];
        let (full, domain, server_addr) = {
            let s = self.net.sock(pick).unwrap();
            let full = match &s.state {
                State::Listening { queue, backlog } => queue.len() >= *backlog,
                _ => true,
            };
            (full, s.domain, s.local.clone().unwrap())
        };
        if full && unix {
            return;
        }
        if full {
            // A full backlog drops the SYN; the client retries.
            let at = self.now + (1_000_000_000u64 << attempt.min(5));
            self.schedule(
                at,
                Event::Syn {
                    client,
                    to,
                    attempt: attempt + 1,
                },
            );
            return;
        }
        let server_local = match (&server_addr, &to) {
            (Addr::Inet(b), Addr::Inet(t)) if b.ip().is_unspecified() => {
                Addr::Inet(SocketAddr::new(t.ip(), b.port()))
            }
            _ => server_addr.clone(),
        };
        let conn = self.net.conns.len();
        let server = self.net.add(sock_kind(domain, true), true);
        {
            let s = self.net.sock_mut(server).unwrap();
            s.local = Some(server_local.clone());
            s.state = State::Connected { conn, side: 1 };
        }
        self.net.conns.push(Conn {
            socks: [Some(client), Some(server)],
            flows: [Flow::default(), Flow::default()],
            reset: [false, false],
            addrs: [from, server_local],
        });
        self.net.sock_mut(client).unwrap().state = State::Connected { conn, side: 0 };
        if let Some(State::Listening { queue, .. }) = self.net.sock_mut(pick).map(|s| &mut s.state)
        {
            queue.push_back(server);
        }
        self.net.bump(pick);
        self.net.bump(client);
    }

    fn connect_failed(&mut self, client: ObjId, err: i32) {
        self.net.unbind(client);
        if let Some(s) = self.net.sock_mut(client) {
            s.so_error = err;
            s.state = State::Dead;
            s.local = None;
        }
        self.net.bump(client);
    }

    /// Takes the next connection from a listener's queue.
    pub(crate) fn accept(&mut self, id: ObjId) -> Result<ObjId, i32> {
        match self.net.sock_mut(id).map(|s| &mut s.state) {
            None => Err(libc::ENOTSOCK),
            Some(State::Listening { queue, .. }) => queue.pop_front().ok_or(libc::EAGAIN),
            Some(_) => Err(libc::EINVAL),
        }
    }

    pub(crate) fn peer_addr(&self, id: ObjId) -> Result<Addr, i32> {
        match self.net.sock(id).map(|s| &s.state) {
            Some(State::Connected { conn, side }) => {
                Ok(self.net.conns[*conn].addrs[1 - side].clone())
            }
            Some(State::Dgram { peer: Some(p), .. }) => Ok(p.clone()),
            Some(_) => Err(libc::ENOTCONN),
            None => Err(libc::ENOTSOCK),
        }
    }

    /// Writes into a stream. Returns bytes accepted.
    pub(crate) fn send_stream(&mut self, id: ObjId, data: &[u8]) -> Result<usize, i32> {
        let s = self.net.sock_mut(id).ok_or(libc::ENOTSOCK)?;
        if s.so_error != 0 {
            return Err(std::mem::take(&mut s.so_error));
        }
        let (conn, side) = match s.state {
            State::Connected { conn, side } => (conn, side),
            State::Connecting => return Err(libc::EAGAIN),
            State::Dead => return Err(libc::EPIPE),
            _ => return Err(libc::ENOTCONN),
        };
        if s.shut_wr {
            return Err(libc::EPIPE);
        }
        if self.net.conns[conn].reset[side] {
            let s = self.net.sock_mut(id).unwrap();
            if !s.reset_reported {
                s.reset_reported = true;
                return Err(libc::ECONNRESET);
            }
            return Err(libc::EPIPE);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let window = self.net.window();
        let flow = &self.net.conns[conn].flows[side];
        let room = window.saturating_sub(flow.buf.len() + flow.flight_bytes);
        if room == 0 {
            return Err(libc::EAGAIN);
        }
        let mut n = data.len().min(room);
        let p = self.faults.short_io;
        if n > 1 && self.fault(p, "short-send") {
            n = self.between(1, n as u64 - 1) as usize;
        }
        let mut off = 0;
        while off < n {
            let piece = if self.faults.split_segments {
                let max = (n - off) as u64;
                self.between(1, max.min(16 * 1024)) as usize
            } else {
                n - off
            };
            let data = data[off..off + piece].to_vec();
            self.push_seg(conn, side, Seg::Data(data));
            off += piece;
        }
        // Reset injected by the network: the peer goes away mid-stream.
        // The data is sent first, so the stream never has a gap.
        let p = self.faults.tcp_reset;
        if self.fault(p, "tcp-reset") {
            self.reset_conn(conn, None);
        }
        Ok(n)
    }

    fn push_seg(&mut self, conn: usize, side: usize, seg: Seg) {
        let mut at = self.now + self.latency();
        let p = self.faults.tcp_loss;
        if self.fault(p, "tcp-loss") {
            // Retransmitted after the minimum RTO.
            at += 200_000_000;
        }
        let blocked = {
            let c = &self.net.conns[conn];
            self.net.partitioned(&c.addrs[0], &c.addrs[1])
        };
        if blocked {
            // Retransmissions keep failing: model as a long stall.
            at += 30_000_000_000;
        }
        let f = &mut self.net.conns[conn].flows[side];
        at = at.max(f.last_at);
        f.last_at = at;
        if let Seg::Data(d) = &seg {
            f.flight_bytes += d.len();
        } else {
            f.fin_sent = true;
        }
        f.flight.push_back((at, seg));
        self.schedule(at, Event::Deliver { conn, dir: side });
    }

    fn deliver(&mut self, conn: usize, dir: usize) {
        let now = self.now;
        let to_side = 1 - dir;
        let mut arrived = false;
        loop {
            let c = &mut self.net.conns[conn];
            let f = &mut c.flows[dir];
            match f.flight.front() {
                Some((at, _)) if *at <= now => {}
                _ => break,
            }
            let (_, seg) = f.flight.pop_front().unwrap();
            if c.reset[to_side] {
                if let Seg::Data(d) = &seg {
                    f.flight_bytes -= d.len();
                }
                continue;
            }
            match seg {
                Seg::Data(d) => {
                    f.flight_bytes -= d.len();
                    if c.socks[to_side].is_none() {
                        // Data for a closed socket: answered with a reset.
                        let at = now + self.latency();
                        self.schedule(at, Event::Rst { conn, side: dir });
                        continue;
                    }
                    f.buf.extend(d);
                }
                Seg::Fin => f.fin_arrived = true,
            }
            arrived = true;
        }
        if arrived {
            if let Some(id) = self.net.conns[conn].socks[to_side] {
                self.net.bump(id);
            }
        }
    }

    /// Resets the connection: `from` is the side that sent the reset
    /// (`None` for one injected by the network, seen by both sides).
    pub(crate) fn reset_conn(&mut self, conn: usize, from: Option<usize>) {
        let sides: Vec<usize> = match from {
            Some(s) => vec![1 - s],
            None => vec![0, 1],
        };
        for side in sides {
            let at = self.now + self.latency();
            self.schedule(at, Event::Rst { conn, side });
        }
    }

    fn rst_arrives(&mut self, conn: usize, side: usize) {
        let c = &mut self.net.conns[conn];
        if c.reset[side] {
            return;
        }
        c.reset[side] = true;
        // Linux keeps what already arrived readable: a read returns it
        // first, then ECONNRESET. Data the peer sent before the reset is
        // ahead of it on the wire, so it lands first.
        let incoming = &mut c.flows[1 - side];
        for (_, seg) in incoming.flight.drain(..) {
            if let Seg::Data(d) = seg {
                incoming.buf.extend(d);
            }
        }
        incoming.flight_bytes = 0;
        c.flows[side].flight.clear();
        c.flows[side].flight_bytes = 0;
        if let Some(id) = c.socks[side] {
            if let Some(s) = self.net.sock_mut(id) {
                s.so_error = libc::ECONNRESET;
            }
            self.net.bump(id);
        }
    }

    /// Reads from a stream. `Ok(0)` is end of stream.
    pub(crate) fn recv_stream(
        &mut self,
        id: ObjId,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<usize, i32> {
        let s = self.net.sock_mut(id).ok_or(libc::ENOTSOCK)?;
        let (conn, side) = match s.state {
            State::Connected { conn, side } => (conn, side),
            State::Dead => {
                if s.so_error != 0 {
                    return Err(std::mem::take(&mut s.so_error));
                }
                return Ok(0);
            }
            State::Connecting => return Err(libc::EAGAIN),
            _ => return Err(libc::ENOTCONN),
        };
        if self.net.conns[conn].reset[side]
            && (self.net.conns[conn].flows[1 - side].buf.is_empty() || buf.is_empty())
        {
            let s = self.net.sock_mut(id).unwrap();
            s.so_error = 0;
            if !s.reset_reported {
                s.reset_reported = true;
                return Err(libc::ECONNRESET);
            }
            return Ok(0);
        }
        if self.net.sock(id).unwrap().shut_rd {
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let flow = &mut self.net.conns[conn].flows[1 - side];
        if flow.buf.is_empty() {
            return if flow.fin_arrived {
                Ok(0)
            } else {
                Err(libc::EAGAIN)
            };
        }
        let mut n = buf.len().min(flow.buf.len());
        let p = self.faults.short_io;
        if n > 1 && self.fault(p, "short-recv") {
            n = self.between(1, n as u64 - 1) as usize;
        }
        let flow = &mut self.net.conns[conn].flows[1 - side];
        for (i, b) in flow.buf.iter().take(n).enumerate() {
            buf[i] = *b;
        }
        if !peek {
            flow.buf.drain(..n);
            // Room in the window again: the sender may write.
            if let Some(other) = self.net.conns[conn].socks[1 - side] {
                self.net.bump(other);
            }
        }
        Ok(n)
    }

    pub(crate) fn shutdown(&mut self, id: ObjId, how: i32) -> Result<(), i32> {
        let s = self.net.sock_mut(id).ok_or(libc::ENOTSOCK)?;
        let (conn, side) = match s.state {
            State::Connected { conn, side } => (conn, side),
            State::Listening { .. } | State::Dgram { .. } => return Ok(()),
            _ => return Err(libc::ENOTCONN),
        };
        if how == libc::SHUT_RD || how == libc::SHUT_RDWR {
            s.shut_rd = true;
        }
        if (how == libc::SHUT_WR || how == libc::SHUT_RDWR) && !s.shut_wr {
            s.shut_wr = true;
            if !self.net.conns[conn].flows[side].fin_sent && !self.net.conns[conn].reset[side] {
                self.push_seg(conn, side, Seg::Fin);
            }
        }
        self.net.bump(id);
        Ok(())
    }

    // ------------------------------------------------------------ datagrams

    pub(crate) fn send_dgram(
        &mut self,
        id: ObjId,
        to: Option<Addr>,
        data: &[u8],
    ) -> Result<usize, i32> {
        let s = self.net.sock(id).ok_or(libc::ENOTSOCK)?;
        let to = match (to, &s.state) {
            (Some(t), _) => normalise(t),
            (None, State::Dgram { peer: Some(p), .. }) => p.clone(),
            (None, _) => return Err(libc::EDESTADDRREQ),
        };
        if data.len() > 65507 {
            return Err(libc::EMSGSIZE);
        }
        let from = local_for(self, id, &to)?;
        if self.net.partitioned(&from, &to) {
            return Ok(data.len());
        }
        let p = self.faults.udp_loss;
        if self.fault(p, "udp-loss") {
            return Ok(data.len());
        }
        let copies = if self.fault(self.faults.udp_dup, "udp-dup") {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let mut at = self.now + self.latency();
            let p = self.faults.udp_reorder;
            if self.fault(p, "udp-reorder") {
                at += self.between(1, 50) * 1_000_000;
            } else {
                let last = self
                    .net
                    .dgram_last
                    .entry((from.clone(), to.clone()))
                    .or_insert(0);
                at = at.max(*last);
                *last = at;
            }
            self.schedule(
                at,
                Event::Datagram {
                    to: to.clone(),
                    from: from.clone(),
                    data: data.to_vec(),
                },
            );
        }
        Ok(data.len())
    }

    fn datagram_arrives(&mut self, to: Addr, from: Addr, data: Vec<u8>) {
        let targets = self.net.lookup(false, &to);
        // SO_REUSEPORT spreads datagrams by source; keep one target per
        // source so a flow stays on one socket.
        let target = match targets.len() {
            0 => {
                // Port unreachable: a connected sender learns it (ICMP), as
                // on Linux.
                for sender in self.net.lookup(false, &from) {
                    let connected_here = matches!(
                        self.net.sock(sender).map(|s| &s.state),
                        Some(State::Dgram { peer: Some(p), .. }) if *p == to
                    );
                    if connected_here {
                        self.net.sock_mut(sender).unwrap().so_error = libc::ECONNREFUSED;
                        self.net.bump(sender);
                    }
                }
                return;
            }
            1 => targets[0],
            n => {
                let mut h: u64 = 1469598103934665603;
                for b in from.to_string().bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(1099511628211);
                }
                targets[(h % n as u64) as usize]
            }
        };
        if let Some(State::Dgram { peer, queue }) = self.net.sock_mut(target).map(|s| &mut s.state)
        {
            if peer.as_ref().is_some_and(|p| *p != from) {
                return;
            }
            if queue.len() < 1024 {
                queue.push_back((from, data));
            }
        }
        self.net.bump(target);
    }

    pub(crate) fn recv_dgram(&mut self, id: ObjId, peek: bool) -> Result<(Addr, Vec<u8>), i32> {
        if let Some(s) = self.net.sock_mut(id) {
            if s.so_error != 0 {
                return Err(std::mem::take(&mut s.so_error));
            }
        }
        match self.net.sock_mut(id).map(|s| &mut s.state) {
            Some(State::Dgram { queue, .. }) => {
                if peek {
                    queue.front().cloned().ok_or(libc::EAGAIN)
                } else {
                    queue.pop_front().ok_or(libc::EAGAIN)
                }
            }
            Some(_) => Err(libc::EINVAL),
            None => Err(libc::ENOTSOCK),
        }
    }

    pub(crate) fn connect_dgram(&mut self, id: ObjId, to: Addr) -> Result<(), i32> {
        let to = normalise(to);
        local_for(self, id, &to)?;
        if let Some(State::Dgram { peer, .. }) = self.net.sock_mut(id).map(|s| &mut s.state) {
            *peer = Some(to);
        }
        Ok(())
    }

    // ---------------------------------------------------------- unix pairs

    pub(crate) fn socketpair(&mut self, domain: i32, nonblock: bool) -> (ObjId, ObjId) {
        let a = self.net.add(sock_kind(domain, true), nonblock);
        let b = self.net.add(sock_kind(domain, true), nonblock);
        let conn = self.net.conns.len();
        self.net.conns.push(Conn {
            socks: [Some(a), Some(b)],
            flows: [Flow::default(), Flow::default()],
            reset: [false, false],
            addrs: [Addr::Unix(Vec::new()), Addr::Unix(Vec::new())],
        });
        self.net.sock_mut(a).unwrap().state = State::Connected { conn, side: 0 };
        self.net.sock_mut(b).unwrap().state = State::Connected { conn, side: 1 };
        (a, b)
    }

    // --------------------------------------------------------------- closing

    /// The last descriptor of `id` is closed.
    pub(crate) fn destroy(&mut self, id: ObjId) {
        // Leaves every epoll set.
        let eps: Vec<ObjId> = self
            .net
            .objs
            .iter()
            .filter(|(_, o)| matches!(o.kind, Kind::Epoll { .. }))
            .map(|(k, _)| *k)
            .collect();
        for ep in eps {
            if let Some(Obj {
                kind: Kind::Epoll { list },
                ..
            }) = self.net.objs.get_mut(&ep)
            {
                list.remove(&id);
            }
        }
        self.net.unbind(id);
        let Some(obj) = self.net.objs.remove(&id) else {
            return;
        };
        let Kind::Sock(s) = obj.kind else { return };
        match s.state {
            State::Listening { queue, .. } => {
                // Connections never accepted are reset.
                for server in queue {
                    if let Some(State::Connected { conn, side }) =
                        self.net.sock(server).map(|s| &s.state)
                    {
                        let (conn, side) = (*conn, *side);
                        self.net.conns[conn].socks[side] = None;
                        self.reset_conn(conn, Some(side));
                    }
                    self.net.objs.remove(&server);
                }
            }
            State::Connected { conn, side } => {
                self.net.conns[conn].socks[side] = None;
                let unread = !self.net.conns[conn].flows[1 - side].buf.is_empty();
                if self.net.conns[conn].reset[side] {
                    // Nothing to tell the peer.
                } else if unread {
                    // Closing with unread data sends a reset (as Linux does).
                    self.reset_conn(conn, Some(side));
                } else if !self.net.conns[conn].flows[side].fin_sent {
                    self.push_seg(conn, side, Seg::Fin);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn apply(&mut self, ev: Event) {
        match ev {
            Event::Syn {
                client,
                to,
                attempt,
            } => self.syn_arrives(client, to, attempt),
            Event::Deliver { conn, dir } => self.deliver(conn, dir),
            Event::Rst { conn, side } => self.rst_arrives(conn, side),
            Event::Datagram { to, from, data } => self.datagram_arrives(to, from, data),
        }
    }
}
