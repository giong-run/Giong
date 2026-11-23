//! One UDP port for several workers, on Windows.
//!
//! Windows has no `SO_REUSEPORT`: a port has one socket, and a socket
//! reports its completions to one completion port, so to one worker. Every
//! worker can still bind its own [`UdpSocket`](super::UdpSocket) to the
//! same address, as on Linux:
//!
//! - The first socket of an address binds it and receives through its
//!   worker's driver, as a socket nobody shares with does.
//! - When a second socket binds the address, it gets a handle to the same
//!   socket, and from then on one thread takes the datagrams off the socket
//!   and hands each to a socket chosen by a hash of the sender's address,
//!   so that all datagrams of one sender go to the same worker, as on
//!   Linux. Each socket's receives return the datagrams handed to it.
//! - Sends go out on the one socket, from every worker, so replies come
//!   from the port the datagrams went to.
//!
//! A socket whose waiting datagrams pass [`INBOX_MAX`] of them or
//! [`INBOX_BYTES`] loses the next ones, as the kernel drops datagrams for a
//! socket that is not read fast enough. The thread ends with the last
//! socket.
//!
//! With `unstable-udp-offload`, the thread receives with room for control
//! messages (`WSARecvMsg`), and every datagram it hands out carries them
//! (the URO segment size, the ECN bits, the local address), in the room a
//! receive of the driver gives them ([`INBOX_CONTROL`] bytes); that room
//! counts towards [`INBOX_BYTES`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use socket2::Socket;

use super::{INBOX_CONTROL, NAME_LEN, OUT_HEADER, OVERHEAD};
use crate::runtime::lock;
use crate::sys::{recv_from_nowait, AcceptEvents, AcceptWake, RawSock, SockAddrStorage, SockLen};

/// Datagrams that may wait for one socket before the next ones for it are
/// dropped.
const INBOX_MAX: usize = 4096;

/// Bytes that may wait for one socket, headers included, before the next
/// datagrams for it are dropped. The count alone would let 4096 datagrams
/// of up to 64 KiB each (UDP over IP fragments) hold 256 MiB for a socket
/// whose worker is slow, where the kernel holds a socket's receive buffer.
const INBOX_BYTES: usize = 4 << 20;

/// The longest datagram UDP carries.
const DATAGRAM_MAX: usize = 65_536;

/// The addresses that sockets of this process share.
static GROUPS: Mutex<Option<HashMap<SocketAddr, Arc<Group>>>> = Mutex::new(None);

/// One shared address.
struct Group {
    addr: SocketAddr,
    /// A handle to the socket that lives as long as any member, whichever
    /// of them goes first.
    master: Socket,
    /// Set once a second socket joined: the thread receives from then on.
    dispatching: AtomicBool,
    state: Mutex<State>,
}

struct State {
    inboxes: Vec<Arc<Inbox>>,
    dispatcher: Option<(Arc<AcceptEvents>, JoinHandle<()>)>,
}

/// The datagrams handed to one socket.
#[derive(Default)]
struct Inbox {
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    /// Each laid out as the driver lays out a received datagram (see
    /// [`super::Datagram::parse`]).
    datagrams: VecDeque<Vec<u8>>,
    /// The bytes of `datagrams`.
    bytes: usize,
    /// The tasks waiting on the socket, each once.
    waiters: Vec<Waker>,
    /// `false` once the socket is gone.
    open: bool,
}

/// One socket's share of an address.
pub(crate) struct Member {
    group: Arc<Group>,
    inbox: Arc<Inbox>,
    /// The socket that bound the address receives on it by itself until
    /// another one joins.
    first: bool,
}

/// A socket for `addr` and its share of the address.
///
/// If sockets of this process hold the address already, the socket is a
/// handle to theirs. Otherwise `bind` makes it, and later sockets join it;
/// what else `bind` returns is handed on. The registry is locked from the
/// look to the entry, so that of two workers that bind an address at the
/// same moment one binds and the other joins.
pub(crate) fn bind<T>(
    addr: SocketAddr,
    bind: impl FnOnce() -> io::Result<(Socket, T)>,
) -> io::Result<(Socket, Option<T>, Option<Member>)> {
    let mut groups = lock(&GROUPS);
    let known = groups.as_ref().and_then(|g| g.get(&addr)).cloned();
    let Some(group) = known else {
        let (sock, other) = bind()?;
        let member = first(groups.get_or_insert_with(HashMap::new), &sock);
        return Ok((sock, Some(other), member));
    };
    let sock = group.master.try_clone()?;
    // Its sends must not wait: they run on a worker.
    sock.set_nonblocking(true)?;
    let inbox = Arc::new(Inbox::default());
    lock(&inbox.slot).open = true;
    {
        let mut state = lock(&group.state);
        if state.dispatcher.is_none() {
            // This also makes the socket non-blocking, which the sends of
            // the members rely on.
            let events = Arc::new(AcceptEvents::new(group.master.as_raw_socket() as RawSock)?);
            let (for_thread, shared) = (events.clone(), group.clone());
            let thread = std::thread::Builder::new()
                .name("giong-udp".into())
                .spawn(move || shared.dispatch(&for_thread))?;
            state.dispatcher = Some((events, thread));
            group.dispatching.store(true, Ordering::Release);
            // The first socket may be waiting in its own receive: it has
            // to look again to see that the thread took over.
            for inbox in &state.inboxes {
                let waiters = mem::take(&mut lock(&inbox.slot).waiters);
                waiters.into_iter().for_each(Waker::wake);
            }
        }
        state.inboxes.push(inbox.clone());
    }
    drop(groups);
    let member = Member {
        group,
        inbox,
        first: false,
    };
    Ok((sock, None, Some(member)))
}

/// Makes the address `sock` was just bound to one that later sockets join.
/// `None` if the socket's address cannot be read or duplicated, and the
/// socket then keeps the port to itself.
fn first(groups: &mut HashMap<SocketAddr, Arc<Group>>, sock: &Socket) -> Option<Member> {
    let addr = sock.local_addr().ok()?.as_socket()?;
    let master = sock.try_clone().ok()?;
    let inbox = Arc::new(Inbox::default());
    lock(&inbox.slot).open = true;
    let group = Arc::new(Group {
        addr,
        master,
        dispatching: AtomicBool::new(false),
        state: Mutex::new(State {
            inboxes: vec![inbox.clone()],
            dispatcher: None,
        }),
    });
    groups.insert(addr, group.clone());
    Some(Member {
        group,
        inbox,
        first: true,
    })
}

impl Member {
    /// `true` for the socket that bound the address.
    pub(crate) fn is_first(&self) -> bool {
        self.first
    }

    /// `true` once the thread receives for every socket of the address.
    /// The first socket then receives no more by itself: what its driver
    /// still has in flight is its last.
    pub(crate) fn dispatching(&self) -> bool {
        self.group.dispatching.load(Ordering::Acquire)
    }

    /// `true` if this socket's datagrams come from its inbox only.
    pub(crate) fn receives_from_inbox(&self) -> bool {
        !self.first || self.dispatching()
    }

    /// For a datagram that the first socket's own receive, started before
    /// the thread took over, brought in: `true` if its sender maps to
    /// another socket, which then has it, so that every datagram of one
    /// sender reaches the same socket. `name` is the sender's address as
    /// the system wrote it.
    pub(crate) fn pass_on(
        &self,
        name: &[u8],
        control: &[u8],
        payload: &[u8],
        truncated: bool,
    ) -> bool {
        if !self.first || !self.dispatching() {
            return false;
        }
        let name = &name[..name.len().min(NAME_LEN)];
        match self.group.inbox_for(name) {
            Some(inbox) if !Arc::ptr_eq(&inbox, &self.inbox) => {
                deliver(&inbox, name, control, payload, truncated);
                true
            }
            _ => false,
        }
    }

    /// The next datagram handed to this socket. Remembers the waker either
    /// way, so that the first socket learns when the thread takes over.
    pub(crate) fn poll_inbox(&self, cx: &mut Context<'_>) -> Poll<Vec<u8>> {
        let mut slot = lock(&self.inbox.slot);
        if let Some(datagram) = slot.datagrams.pop_front() {
            slot.bytes -= datagram.len();
            return Poll::Ready(datagram);
        }
        if !slot.waiters.iter().any(|w| w.will_wake(cx.waker())) {
            slot.waiters.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Group {
    /// The thread: takes datagrams off the socket and hands them out,
    /// until it is told to stop.
    fn dispatch(&self, events: &AcceptEvents) {
        let sock = self.master.as_raw_socket() as RawSock;
        let mut scratch = vec![0u8; DATAGRAM_MAX];
        // Aligned for the headers of control messages.
        let mut control = vec![0u64; INBOX_CONTROL.div_ceil(8)];
        let mut wait = None;
        loop {
            // A pause is for work the socket has that cannot be taken yet:
            // waiting on the socket would end at once, and spin.
            let woke = match wait.take() {
                Some(ms) => events.pause(ms),
                None => events.wait(None),
            };
            if let AcceptWake::Stop = woke {
                return;
            }
            loop {
                // SAFETY: all-zero is a valid address buffer.
                let mut from: SockAddrStorage = unsafe { std::mem::zeroed() };
                let mut from_len = std::mem::size_of::<SockAddrStorage>() as SockLen;
                let mut control_len = 0usize;
                let n = if INBOX_CONTROL == 0 {
                    // SAFETY: the buffers are locals of the sizes given; the
                    // socket is open while `self` lives.
                    unsafe {
                        recv_from_nowait(
                            sock,
                            scratch.as_mut_ptr(),
                            scratch.len(),
                            &mut from,
                            &mut from_len,
                        )
                    }
                } else {
                    // SAFETY: as above, with room for control messages.
                    unsafe {
                        recv_msg(
                            sock,
                            &mut scratch,
                            control.as_mut_ptr().cast(),
                            &mut from,
                            &mut from_len,
                            &mut control_len,
                        )
                    }
                };
                let (len, truncated) = if n >= 0 {
                    (n as usize, false)
                } else {
                    let err = io::Error::last_os_error();
                    match err.raw_os_error().unwrap_or(0) {
                        crate::sys::errno::EAGAIN => break,
                        crate::sys::errno::EMSGSIZE => (scratch.len(), true),
                        // An earlier send was refused by its peer
                        // (`WSAECONNRESET`), or the datagram was dropped
                        // on its way in: nothing to hand out, carry on.
                        crate::sys::errno::NO_PEER | crate::sys::errno::ENETRESET => continue,
                        // No memory or buffers right now: try again
                        // shortly rather than spin.
                        _ => {
                            wait = Some(1);
                            break;
                        }
                    }
                };
                // SAFETY: `SOCKADDR_STORAGE` is plain data of this size.
                let name = unsafe {
                    std::slice::from_raw_parts(
                        (&from as *const SockAddrStorage).cast::<u8>(),
                        (from_len.max(0) as usize).min(std::mem::size_of::<SockAddrStorage>()),
                    )
                };
                // SAFETY: `control` holds `INBOX_CONTROL` bytes, of which the
                // receive wrote `control_len`.
                let control = unsafe {
                    std::slice::from_raw_parts(control.as_ptr().cast::<u8>(), control_len)
                };
                self.hand(name, control, &scratch[..len], truncated);
            }
        }
    }

    /// Gives a datagram from `name` to the socket its sender maps to, or
    /// drops it if that socket has no room or every socket is gone.
    fn hand(&self, name: &[u8], control: &[u8], payload: &[u8], truncated: bool) {
        let name = &name[..name.len().min(NAME_LEN)];
        if let Some(inbox) = self.inbox_for(name) {
            deliver(&inbox, name, control, payload, truncated);
        }
    }

    /// The inbox of the socket that datagrams from `name` go to: `None`
    /// once every socket is gone.
    fn inbox_for(&self, name: &[u8]) -> Option<Arc<Inbox>> {
        let state = lock(&self.state);
        let open: Vec<&Arc<Inbox>> = state
            .inboxes
            .iter()
            .filter(|i| lock(&i.slot).open)
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(open[(fnv1a(name) % open.len() as u64) as usize].clone())
    }
}

/// Whether `slot` has room for one more datagram of `size` bytes.
fn has_room(slot: &Slot, size: usize) -> bool {
    slot.open && slot.datagrams.len() < INBOX_MAX && slot.bytes + size <= INBOX_BYTES
}

/// Puts a datagram from `name` in `inbox`, unless it is full or closed,
/// with its control messages (at most [`INBOX_CONTROL`] bytes of them) in
/// front of the payload. A full inbox costs no allocation: room is looked
/// at first, and again when the copy is made (the first socket's worker
/// hands datagrams in too, see [`Member::pass_on`]).
fn deliver(inbox: &Inbox, name: &[u8], control: &[u8], payload: &[u8], truncated: bool) {
    // At most the room there is (none without `unstable-udp-offload`).
    let control = control.get(..INBOX_CONTROL).unwrap_or(control);
    let at = OVERHEAD + INBOX_CONTROL;
    let size = at + payload.len();
    if !has_room(&lock(&inbox.slot), size) {
        return;
    }
    let mut datagram = vec![0u8; size];
    let fields = [
        name.len() as u32,
        control.len() as u32,
        payload.len() as u32,
        if truncated {
            crate::sys::MSG_TRUNC as u32
        } else {
            0
        },
    ];
    for (i, field) in fields.iter().enumerate() {
        datagram[i * 4..i * 4 + 4].copy_from_slice(&field.to_ne_bytes());
    }
    datagram[OUT_HEADER..OUT_HEADER + name.len()].copy_from_slice(name);
    datagram[OVERHEAD..OVERHEAD + control.len()].copy_from_slice(control);
    datagram[at..].copy_from_slice(payload);
    let mut slot = lock(&inbox.slot);
    if !has_room(&slot, size) {
        return;
    }
    slot.bytes += size;
    slot.datagrams.push_back(datagram);
    let waiters = mem::take(&mut slot.waiters);
    drop(slot);
    waiters.into_iter().for_each(Waker::wake);
}

/// One datagram off `sock` without waiting, with its control messages:
/// the payload into `buf`, the sender's address into `from`, and up to
/// [`INBOX_CONTROL`] bytes of control messages at `control`, whose length
/// goes into `control_len`. The bytes received, or -1 with the thread's
/// last error set, as `recv_from_nowait`.
///
/// # Safety
///
/// `control` must be valid for writing [`INBOX_CONTROL`] bytes, aligned for
/// a control message's header.
#[cfg(feature = "unstable-udp-offload")]
unsafe fn recv_msg(
    sock: RawSock,
    buf: &mut [u8],
    control: *mut u8,
    from: &mut SockAddrStorage,
    from_len: &mut SockLen,
    control_len: &mut usize,
) -> isize {
    use windows_sys::Win32::Networking::WinSock::{WSABUF, WSAMSG};
    let mut piece = WSABUF {
        len: buf.len().min(u32::MAX as usize) as u32,
        buf: buf.as_mut_ptr(),
    };
    let mut msg = WSAMSG {
        name: (from as *mut SockAddrStorage).cast(),
        namelen: *from_len,
        lpBuffers: &mut piece,
        dwBufferCount: 1,
        Control: WSABUF {
            len: INBOX_CONTROL as u32,
            buf: control,
        },
        dwFlags: 0,
    };
    // SAFETY: the header points at `buf`, `from` and `control`, valid for
    // the call as the caller guarantees.
    let n = unsafe { crate::sys::recvmsg_nowait(sock, &mut msg) };
    *from_len = msg.namelen;
    *control_len = (msg.Control.len as usize).min(INBOX_CONTROL);
    n
}

/// Without `unstable-udp-offload` there is no room for control messages,
/// and the thread never calls this.
#[cfg(not(feature = "unstable-udp-offload"))]
unsafe fn recv_msg(
    _: RawSock,
    _: &mut [u8],
    _: *mut u8,
    _: &mut SockAddrStorage,
    _: &mut SockLen,
    _: &mut usize,
) -> isize {
    unreachable!("no control messages without unstable-udp-offload")
}

/// FNV-1a over the sender's address (family, port, address): the same
/// sender always maps to the same socket while the members stay the same.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl Drop for Member {
    fn drop(&mut self) {
        let group = &self.group;
        {
            let mut slot = lock(&self.inbox.slot);
            slot.open = false;
            slot.waiters.clear();
            slot.datagrams.clear();
            slot.bytes = 0;
        }
        // The registry first, then the state: the order `bind` takes them.
        let mut groups = lock(&GROUPS);
        let dispatcher = {
            let mut state = lock(&group.state);
            state.inboxes.retain(|i| !Arc::ptr_eq(i, &self.inbox));
            if state.inboxes.is_empty() {
                if let Some(map) = groups.as_mut() {
                    map.remove(&group.addr);
                }
                state.dispatcher.take()
            } else {
                None
            }
        };
        drop(groups);
        if let Some((events, thread)) = dispatcher {
            // The last socket: the thread ends, and with it its use of the
            // socket, before this returns, so that the port is free once
            // the socket is dropped.
            events.stop();
            drop(thread.join());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_inbox() -> Inbox {
        let inbox = Inbox::default();
        lock(&inbox.slot).open = true;
        inbox
    }

    /// Long datagrams (UDP carries up to 64 KiB, over IP fragments) fill a
    /// socket's inbox by bytes long before the count limit: a slow worker
    /// holds a few MiB, not 4096 times 64 KiB.
    #[test]
    fn an_inbox_holds_a_bounded_number_of_bytes() {
        let inbox = open_inbox();
        let name = [0u8; 16];
        let payload = vec![7u8; 65_507];
        for _ in 0..INBOX_MAX {
            deliver(&inbox, &name, &[], &payload, false);
        }
        let slot = lock(&inbox.slot);
        let held: usize = slot.datagrams.iter().map(Vec::len).sum();
        assert_eq!(held, slot.bytes);
        assert!(held <= INBOX_BYTES, "{held} bytes held");
        assert!(
            held + OVERHEAD + INBOX_CONTROL + payload.len() > INBOX_BYTES,
            "{held} bytes held"
        );
        assert!(
            slot.datagrams.len() < 100,
            "{} datagrams",
            slot.datagrams.len()
        );
    }

    /// Short datagrams still stop at the count, and a datagram taken off
    /// the inbox makes room for the next one.
    #[test]
    fn short_datagrams_stop_at_the_count_and_reading_makes_room() {
        let inbox = open_inbox();
        let name = [0u8; 16];
        for _ in 0..INBOX_MAX + 10 {
            deliver(&inbox, &name, &[], b"hi", false);
        }
        assert_eq!(lock(&inbox.slot).datagrams.len(), INBOX_MAX);
        let taken = {
            let mut slot = lock(&inbox.slot);
            let datagram = slot.datagrams.pop_front().unwrap();
            slot.bytes -= datagram.len();
            datagram
        };
        assert_eq!(taken.len(), OVERHEAD + INBOX_CONTROL + 2);
        deliver(&inbox, &name, &[], b"hi", false);
        let slot = lock(&inbox.slot);
        assert_eq!(slot.datagrams.len(), INBOX_MAX);
        assert_eq!(slot.bytes, INBOX_MAX * (OVERHEAD + INBOX_CONTROL + 2));
    }

    /// A closed inbox takes nothing.
    #[test]
    fn a_closed_inbox_takes_nothing() {
        let inbox = Inbox::default();
        deliver(&inbox, &[0u8; 16], &[], b"hi", false);
        let slot = lock(&inbox.slot);
        assert!(slot.datagrams.is_empty());
        assert_eq!(slot.bytes, 0);
    }

    /// The control messages a datagram came with travel with it, in the
    /// room a receive of the driver gives them, and count towards the
    /// inbox's bytes.
    #[test]
    fn control_messages_travel_with_the_datagram() {
        let inbox = open_inbox();
        let control = vec![9u8; INBOX_CONTROL];
        deliver(&inbox, &[2, 0, 0, 80], &control, b"payload", false);
        let slot = lock(&inbox.slot);
        let datagram = &slot.datagrams[0];
        assert_eq!(slot.bytes, datagram.len());
        let field = |i: usize| u32::from_ne_bytes(datagram[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(field(1) as usize, control.len());
        assert_eq!(&datagram[OVERHEAD..OVERHEAD + control.len()], &control[..]);
        assert_eq!(&datagram[OVERHEAD + INBOX_CONTROL..], b"payload");
    }
}
