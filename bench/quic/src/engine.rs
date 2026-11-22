//! The server side of the benchmark protocol on quinn-proto, with no I/O.
//!
//! The same engine runs under every I/O layer measured (giong, Tokio), so
//! that a difference between them is the I/O layer and nothing else. The
//! application (answer each stream with N bytes) runs inline in the event
//! loop: no task, channel or lock per connection or stream.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use quinn_proto::{
    ConnectionHandle, DatagramEvent, Dir, EcnCodepoint, Endpoint, EndpointConfig, Event, ReadError,
    ServerConfig, StreamEvent, StreamId, Transmit, WriteError,
};

static ZEROS: [u8; 65536] = [0; 65536];

#[derive(Default)]
struct Stream {
    req: [u8; 8],
    got: usize,
    left: u64,
}

struct Conn {
    c: quinn_proto::Connection,
    streams: HashMap<StreamId, Stream>,
    dirty: bool,
}

pub struct Engine {
    endpoint: Endpoint,
    conns: Vec<Option<Conn>>,
    dirty: Vec<usize>,
    scratch: Vec<u8>,
    /// Datagrams quinn-proto may put in one transmit (1 without GSO).
    pub max_segments: usize,
    body: Bytes,
}

impl Engine {
    pub fn new(max_segments: usize) -> Self {
        let server: Arc<ServerConfig> = Arc::new(crate::server_config());
        Self {
            endpoint: Endpoint::new(
                Arc::new(EndpointConfig::default()),
                Some(server),
                true,
                None,
            ),
            conns: Vec::new(),
            dirty: Vec::new(),
            scratch: Vec::with_capacity(65536),
            max_segments,
            body: Bytes::from_static(&ZEROS),
        }
    }

    pub fn connections(&self) -> usize {
        self.conns.iter().filter(|c| c.is_some()).count()
    }

    fn mark(&mut self, i: usize) {
        if let Some(Some(c)) = self.conns.get_mut(i) {
            if !c.dirty {
                c.dirty = true;
                self.dirty.push(i);
            }
        }
    }

    /// Feeds one received datagram (or a GRO batch of them: `segment` is
    /// the size of each but the last).
    #[allow(clippy::too_many_arguments)]
    pub fn handle(
        &mut self,
        now: Instant,
        from: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        mut data: BytesMut,
        segment: Option<usize>,
        out: &mut impl FnMut(&Transmit, &[u8]),
    ) {
        let seg = segment.unwrap_or(data.len()).max(1);
        while !data.is_empty() {
            let one = data.split_to(seg.min(data.len()));
            self.handle_one(now, from, local_ip, ecn, one, out);
        }
    }

    fn handle_one(
        &mut self,
        now: Instant,
        from: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
        out: &mut impl FnMut(&Transmit, &[u8]),
    ) {
        self.scratch.clear();
        match self
            .endpoint
            .handle(now, from, local_ip, ecn, data, &mut self.scratch)
        {
            Some(DatagramEvent::NewConnection(incoming)) => {
                self.scratch.clear();
                match self.endpoint.accept(incoming, now, &mut self.scratch, None) {
                    Ok((ch, c)) => {
                        if self.conns.len() <= ch.0 {
                            self.conns.resize_with(ch.0 + 1, || None);
                        }
                        self.conns[ch.0] = Some(Conn {
                            c,
                            streams: HashMap::new(),
                            dirty: false,
                        });
                        self.mark(ch.0);
                    }
                    Err(e) => {
                        if let Some(t) = e.response {
                            out(&t, &self.scratch[..t.size]);
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
            Some(DatagramEvent::Response(t)) => out(&t, &self.scratch[..t.size]),
            None => {}
        }
    }

    /// Runs timers that are due, the application, and hands every datagram
    /// to send to `out`. Returns the next deadline.
    pub fn drive(
        &mut self,
        now: Instant,
        out: &mut impl FnMut(&Transmit, &[u8]),
    ) -> Option<Instant> {
        // Timers first: they mark their connections dirty.
        for i in 0..self.conns.len() {
            let due = match &mut self.conns[i] {
                Some(c) => c.c.poll_timeout().is_some_and(|t| t <= now),
                None => false,
            };
            if due {
                self.conns[i].as_mut().unwrap().c.handle_timeout(now);
                self.mark(i);
            }
        }
        while let Some(i) = self.dirty.pop() {
            self.process(i, now, out);
        }
        self.conns
            .iter_mut()
            .flatten()
            .filter_map(|c| c.c.poll_timeout())
            .min()
    }

    fn process(&mut self, i: usize, now: Instant, out: &mut impl FnMut(&Transmit, &[u8])) {
        let Some(conn) = self.conns[i].as_mut() else {
            return;
        };
        conn.dirty = false;
        let ch = ConnectionHandle(i);
        while let Some(ev) = conn.c.poll_endpoint_events() {
            if let Some(cev) = self.endpoint.handle_event(ch, ev) {
                conn.c.handle_event(cev);
            }
        }
        while let Some(ev) = conn.c.poll() {
            match ev {
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    while let Some(id) = conn.c.streams().accept(Dir::Bi) {
                        conn.streams.insert(id, Stream::default());
                        read(conn, id, &self.body);
                    }
                }
                Event::Stream(StreamEvent::Readable { id }) => read(conn, id, &self.body),
                Event::Stream(StreamEvent::Writable { id }) => write(conn, id, &self.body),
                _ => {}
            }
        }
        loop {
            self.scratch.clear();
            match conn
                .c
                .poll_transmit(now, self.max_segments, &mut self.scratch)
            {
                Some(t) => out(&t, &self.scratch[..t.size]),
                None => break,
            }
        }
        while let Some(ev) = conn.c.poll_endpoint_events() {
            if let Some(cev) = self.endpoint.handle_event(ch, ev) {
                conn.c.handle_event(cev);
                if !conn.dirty {
                    conn.dirty = true;
                    self.dirty.push(i);
                }
            }
        }
        if conn.c.is_drained() {
            self.conns[i] = None;
        }
    }
}

fn read(conn: &mut Conn, id: StreamId, body: &Bytes) {
    let Some(st) = conn.streams.get_mut(&id) else {
        return;
    };
    let mut rs = conn.c.recv_stream(id);
    let Ok(mut chunks) = rs.read(true) else {
        return;
    };
    let mut fin = false;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                let take = chunk.bytes.len().min(8 - st.got);
                st.req[st.got..st.got + take].copy_from_slice(&chunk.bytes[..take]);
                st.got += take;
            }
            Ok(None) => {
                fin = true;
                break;
            }
            Err(ReadError::Blocked) => break,
            Err(_) => break,
        }
    }
    let _ = chunks.finalize();
    if fin && st.got == 8 {
        st.left = u64::from_be_bytes(st.req);
        write(conn, id, body);
    }
}

fn write(conn: &mut Conn, id: StreamId, body: &Bytes) {
    let Some(st) = conn.streams.get_mut(&id) else {
        return;
    };
    if st.got < 8 {
        return;
    }
    let mut s = conn.c.send_stream(id);
    while st.left > 0 {
        let n = st.left.min(body.len() as u64) as usize;
        match s.write_chunks(&mut [body.slice(..n)]) {
            Ok(w) if w.bytes > 0 => st.left -= w.bytes as u64,
            Ok(_) | Err(WriteError::Blocked) => return,
            Err(_) => {
                conn.streams.remove(&id);
                return;
            }
        }
    }
    let _ = s.finish();
    conn.streams.remove(&id);
}
