//! UDP sockets.

use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::ptr;
use std::task::{Context, Poll};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::addr::{resolved, ToSocketAddrs};
use super::{
    close_socket, inet_addr, no_addresses, os_err, share, unexpected, wait_for_pool, MultiSlot,
};
use crate::buf::{IoBuf, IoBufMut, RecvBuf};
use crate::driver::{Io, MultiIo, MultiKind, MultiPoll};
use crate::error::{raise, Code};
use crate::kbox::KernelBox;
use crate::op::Op;
use crate::runtime;

/// What the kernel puts in front of every datagram of a multishot receive
/// (`struct io_uring_recvmsg_out`: name length, control length, payload
/// length, flags — four `u32`s) ...
const OUT_HEADER: usize = 16;
/// ... followed by the sender's address in a field of this fixed size, large
/// enough for IPv4 and IPv6.
const NAME_LEN: usize = mem::size_of::<libc::sockaddr_in6>();
/// Bytes of every pool buffer that are not available for payload.
const OVERHEAD: usize = OUT_HEADER + NAME_LEN;

/// The `msghdr` of every multishot receive. The kernel only reads the two
/// length fields from it: room for the sender's address, no control data.
struct RecvHeader(libc::msghdr);

// SAFETY: never written to after initialisation, and its pointers are null.
unsafe impl Sync for RecvHeader {}

static RECV_HEADER: RecvHeader = {
    // SAFETY: all-zero is a valid `msghdr` (null pointers, zero lengths).
    let mut hdr: libc::msghdr = unsafe { mem::zeroed() };
    hdr.msg_namelen = NAME_LEN as libc::socklen_t;
    RecvHeader(hdr)
};

/// Everything a one-off `sendmsg` / `recvmsg` points at, in a [`KernelBox`]
/// so that the addresses stay put, and stay valid, while the kernel uses
/// them.
struct Message {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: libc::sockaddr_storage,
}

impl Message {
    /// A message for `len` bytes at `base`, sent to `addr`, or (with `None`)
    /// with room for the sender's address of a received one.
    fn new(base: *mut u8, len: usize, addr: Option<&SockAddr>) -> KernelBox<Self> {
        // SAFETY: all-zero is valid for these plain C structs.
        let msg: KernelBox<Self> = KernelBox::from_box(Box::new(unsafe { mem::zeroed() }));
        let m = msg.as_ptr();
        // SAFETY: `m` points at the live, initialised `Message` that `msg`
        // owns; nothing else uses it yet. The pointers stored in it are taken
        // through `m`, so they stay valid when `msg` moves.
        unsafe {
            (*m).iov.iov_base = base.cast();
            (*m).iov.iov_len = len;
            (*m).hdr.msg_iov = ptr::addr_of_mut!((*m).iov);
            (*m).hdr.msg_iovlen = 1;
            (*m).hdr.msg_name = ptr::addr_of_mut!((*m).addr).cast();
            (*m).hdr.msg_namelen = match addr {
                Some(addr) => {
                    // A `SockAddr` holds `len()` valid bytes, which fit in
                    // `sockaddr_storage`.
                    ptr::copy_nonoverlapping(
                        addr.as_ptr().cast::<u8>(),
                        ptr::addr_of_mut!((*m).addr).cast::<u8>(),
                        addr.len() as usize,
                    );
                    addr.len()
                }
                None => mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            };
        }
        msg
    }
}

fn parse_addr(name: &[u8]) -> Option<SocketAddr> {
    let family = i32::from(u16::from_ne_bytes([*name.first()?, *name.get(1)?]));
    let port = u16::from_be_bytes([*name.get(2)?, *name.get(3)?]);
    match family {
        libc::AF_INET => {
            let ip: [u8; 4] = name.get(4..8)?.try_into().ok()?;
            Some(SocketAddrV4::new(Ipv4Addr::from(ip), port).into())
        }
        libc::AF_INET6 => {
            let flowinfo = u32::from_ne_bytes(name.get(4..8)?.try_into().ok()?);
            let ip: [u8; 16] = name.get(8..24)?.try_into().ok()?;
            let scope_id = u32::from_ne_bytes(name.get(24..28)?.try_into().ok()?);
            Some(SocketAddrV6::new(Ipv6Addr::from(ip), port, flowinfo, scope_id).into())
        }
        _ => None,
    }
}

/// A datagram received by [`UdpSocket::recv_from`].
///
/// Like a [`RecvBuf`], it normally borrows a buffer from the worker's pool:
/// copy out what you need and drop it promptly.
pub struct Datagram {
    buf: RecvBuf,
    truncated: bool,
}

impl Datagram {
    /// Splits what a multishot `recvmsg` wrote into a buffer.
    fn parse(buf: RecvBuf) -> io::Result<(Self, SocketAddr)> {
        let malformed = || unexpected("io_uring returned a malformed datagram");
        if buf.len() < OVERHEAD {
            return Err(malformed());
        }
        let field = |i: usize| {
            let bytes: [u8; 4] = buf[i * 4..i * 4 + 4].try_into().expect("four bytes");
            u32::from_ne_bytes(bytes) as usize
        };
        let (name_len, payload_len, flags) = (field(0), field(2), field(3) as i32);
        let from = parse_addr(&buf[OUT_HEADER..OUT_HEADER + name_len.min(NAME_LEN)])
            .ok_or_else(malformed)?;
        let truncated = flags & libc::MSG_TRUNC != 0 || payload_len > buf.len() - OVERHEAD;
        Ok((Self { buf, truncated }, from))
    }

    /// The payload.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[OVERHEAD..]
    }

    /// `true` if the datagram was longer than a receive buffer and its end
    /// was cut off. See [`UdpSocket::recv_from`].
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl Deref for Datagram {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Datagram {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::fmt::Debug for Datagram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Datagram")
            .field("len", &self.as_slice().len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// A UDP socket.
///
/// All methods take `&self`, so a socket can be shared (e.g. in an `Rc`)
/// between tasks; any number of them may send and receive at the same time.
/// Each datagram goes to one of the tasks waiting in
/// [`recv_from`](Self::recv_from), woken for it alone.
///
/// A send goes out within the call when the socket has room, as it nearly
/// always does: datagrams wait for no peer. Receives stay queued in the
/// kernel's ring on io_uring, so a datagram echoed back costs one system
/// call.
pub struct UdpSocket {
    sock: ManuallyDrop<Socket>,
    recv: MultiSlot,
    /// The socket's hold on its port, for one that [`bind`](Self::bind)
    /// made on a port of the caller's choice (see `share`).
    held: Option<share::Held>,
    _not_send: PhantomData<*const ()>,
}

impl UdpSocket {
    /// Binds to `addr`, an IP address and a port.
    ///
    /// Every worker of a process can bind its own socket to the same
    /// address, and datagrams are then spread across the workers: on Linux
    /// by a hash of the sender's address, so that all datagrams of one
    /// sender go to the same worker. No other process can: an address that
    /// a socket of another process holds is refused with
    /// [`AddrInUse`](io::ErrorKind::AddrInUse).
    ///
    /// On Linux the workers share the port through `SO_REUSEPORT`, which by
    /// itself would let any process of the same user bind it too; giong
    /// checks the port before the first socket of a process binds it. To
    /// share a port between processes on purpose, create the sockets
    /// yourself and pass them to [`from_std`](Self::from_std).
    ///
    /// Port 0 asks the kernel for a port of the socket's own, which no other
    /// socket can share.
    ///
    /// `addr` is an IP address with a port (see
    /// [`ToSocketAddrs`]): a host name is refused
    /// with `GIONG-E0100`, which says what to write instead.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let mut last_err = None;
        for addr in resolved(&addr, "UdpSocket::bind")? {
            match Self::bind_addr(addr) {
                Ok(socket) => return Ok(socket),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(no_addresses))
    }

    fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        // Not for port 0: with `SO_REUSEPORT` set, Linux may pick a port
        // that another UDP socket of the same user already holds with it,
        // and join the two. Datagrams for either socket would then be split
        // between them by a hash of the sender's address.
        let share = addr.port() != 0;
        let (sock, held) = share::bind(share::Proto::Udp, addr, share, || {
            let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
            if share {
                // `SO_REUSEPORT` only. With `SO_REUSEADDR` as well, any
                // user's socket that sets `SO_REUSEADDR` could bind the
                // port and receive its datagrams; `SO_REUSEPORT` is for
                // sockets of the same user.
                sock.set_reuse_port(true)?;
            }
            sock.bind(&addr.into())?;
            Ok(sock)
        })?;
        let mut socket = Self::from_socket(sock);
        socket.held = held;
        Ok(socket)
    }

    /// Wraps a socket created by the standard library, with the options it
    /// was given: giong sets none, and does not check who else holds its
    /// port.
    pub fn from_std(socket: std::net::UdpSocket) -> Self {
        // SAFETY: we take over the descriptor that `socket` owned.
        Self::from_socket(unsafe { Socket::from_raw_fd(socket.into_raw_fd()) })
    }

    fn from_socket(sock: Socket) -> Self {
        Self {
            sock: ManuallyDrop::new(sock),
            recv: MultiSlot::new(MultiKind::Recv),
            held: None,
            _not_send: PhantomData,
        }
    }

    /// Sets the peer: [`send`](Self::send) goes to `addr`, and only datagrams
    /// from `addr` are received.
    ///
    /// `addr` is an IP address with a port: a host name is refused with
    /// `GIONG-E0100`. Resolve one first with
    /// [`lookup_host`](crate::net::lookup_host).
    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let mut last_err = None;
        for addr in resolved(&addr, "UdpSocket::connect")? {
            // Connecting a UDP socket only records the address: it never blocks.
            match self.sock.connect(&addr.into()) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(no_addresses))
    }

    /// Receives the next datagram and the address it came from.
    ///
    /// The datagram borrows a buffer from the worker's pool, so the longest
    /// payload that fits is the pool's buffer size
    /// ([`Builder::recv_buffers`](crate::Builder::recv_buffers), 8 KiB by
    /// default) minus 44 bytes. Longer datagrams are cut off and marked
    /// [`is_truncated`](Datagram::is_truncated); use
    /// [`read_from`](Self::read_from) with a buffer of your own for those.
    ///
    /// Cancel safe: dropping the future (for instance through
    /// [`timeout`](crate::time::timeout)) loses no datagram — whatever arrives
    /// in the meantime is returned by the next call. As always with UDP, the
    /// kernel drops datagrams when the socket is not read fast enough.
    pub async fn recv_from(&self) -> io::Result<(Datagram, SocketAddr)> {
        let mut waiting = super::Waiting::new(&self.recv);
        poll_fn(|cx| {
            let poll = self.poll_recv_from(cx);
            waiting.polled(&poll, cx);
            poll
        })
        .await
    }

    /// Like [`recv_from`](Self::recv_from), without the sender's address.
    pub async fn recv(&self) -> io::Result<Datagram> {
        Ok(self.recv_from().await?.0)
    }

    fn poll_recv_from(&self, cx: &mut Context<'_>) -> Poll<io::Result<(Datagram, SocketAddr)>> {
        runtime::with_worker(|w| {
            let key = self.recv.key(w);
            let mut driver = w.driver.borrow_mut();
            loop {
                match driver.poll_multi(key, cx.waker()) {
                    MultiPoll::Item(res) => {
                        return Poll::Ready(if res < 0 {
                            Err(os_err(res))
                        } else {
                            Err(unexpected("io_uring returned a datagram without a buffer"))
                        });
                    }
                    MultiPoll::Data(buf) => return Poll::Ready(Datagram::parse(buf)),
                    MultiPoll::NeedArm => {
                        if !wait_for_pool(&mut driver, cx, true) {
                            return Poll::Pending;
                        }
                        let io = MultiIo::RecvMsg {
                            fd: self.sock.as_raw_fd(),
                            hdr: &RECV_HEADER.0,
                        };
                        driver.arm_multi(key, io);
                    }
                    MultiPoll::Pending => return Poll::Pending,
                }
            }
        })
    }

    /// Receives one datagram into `buf` (from its start, up to its capacity),
    /// returning the buffer with its length set to the bytes received, and the
    /// sender's address. A datagram longer than the buffer is cut off.
    ///
    /// For datagrams that do not fit a pool buffer. Unlike
    /// [`recv_from`](Self::recv_from) this is **not** cancel safe (a datagram
    /// that arrives while the future is being dropped is lost), and the two
    /// must not be mixed on one socket: once `recv_from` has been used,
    /// `read_from` fails with `InvalidInput`.
    ///
    /// A buffer with no capacity (such as `Vec::new()`) is refused with
    /// `GIONG-E0109`: the kernel would take the datagram and keep none of it.
    pub async fn read_from<B: IoBufMut>(&self, mut buf: B) -> (io::Result<(usize, SocketAddr)>, B) {
        if buf.capacity() == 0 {
            let err = raise(
                Code::EmptyBuffer,
                "read_from was given a buffer with no room, and a datagram received into it \
                 would be lost",
                "pass a buffer with room for the largest datagram expected, such as \
                 Vec::with_capacity(65_536); Vec::new() has none.",
            );
            return (Err(err), buf);
        }
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            let err = raise(
                Code::MixedReads,
                "read_from cannot be used after recv_from on the same socket",
                "use either recv_from (buffers from the runtime's pool) or read_from (your own \
                 buffer) on one socket, not both.",
            );
            return (Err(err), buf);
        }
        let msg = Message::new(buf.as_mut_ptr(), buf.capacity(), None);
        let io = Io::RecvMsg {
            fd: self.sock.as_raw_fd(),
            // SAFETY: `msg` owns a live `Message`; the field pointer is taken
            // without making a reference.
            msg: unsafe { ptr::addr_of_mut!((*msg.as_ptr()).hdr) },
        };
        // SAFETY: the header is in a `KernelBox` and points at its own fields
        // and at `buf`'s bytes; an `IoBufMut`'s bytes stay put when it moves.
        let (res, _, (msg, mut buf)) = unsafe { Op::new(io, (msg, buf)) }.await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        let msg = msg.get();
        // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
        unsafe { buf.set_init(res as usize) };
        // SAFETY: `sockaddr_storage` is plain data of exactly this size.
        let name = unsafe {
            std::slice::from_raw_parts(
                (&msg.addr as *const libc::sockaddr_storage).cast::<u8>(),
                mem::size_of::<libc::sockaddr_storage>(),
            )
        };
        match parse_addr(&name[..(msg.hdr.msg_namelen as usize).min(name.len())]) {
            Some(from) => (Ok((res as usize, from)), buf),
            None => (
                Err(unexpected(
                    "io_uring returned a datagram from an unknown address family",
                )),
                buf,
            ),
        }
    }

    /// Like [`read_from`](Self::read_from), without the sender's address: for
    /// a socket that has been [`connect`](Self::connect)ed. The same limits
    /// apply (not cancel safe, not after `recv` or `recv_from`).
    pub async fn read<B: IoBufMut>(&self, buf: B) -> (io::Result<usize>, B) {
        let (res, buf) = self.read_from(buf).await;
        (res.map(|(len, _from)| len), buf)
    }

    /// Sends `buf` as one datagram to `addr`, returning the number of bytes
    /// sent. The buffer is handed back so it can be reused.
    pub async fn send_to<B: IoBuf>(&self, buf: B, addr: SocketAddr) -> (io::Result<usize>, B) {
        let addr = SockAddr::from(addr);
        let fd = self.sock.as_raw_fd();
        // At once where the socket has room (see `Driver::try_send_to`).
        let (sent, sends_to) = runtime::with_worker(|w| {
            let mut driver = w.driver.borrow_mut();
            // SAFETY: an `IoBuf` has `len()` initialised bytes at `as_ptr()`.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
            (driver.try_send_to(fd, bytes, &addr), driver.sends_to())
        });
        if let Some(res) = sent {
            return if res < 0 {
                (Err(os_err(res)), buf)
            } else {
                (Ok(res as usize), buf)
            };
        }
        let (res, buf) = if sends_to {
            let addr = KernelBox::new(addr);
            let io = Io::SendTo {
                fd,
                buf: buf.as_ptr(),
                len: buf.len().min(u32::MAX as usize) as u32,
                // SAFETY: `addr` owns a live `SockAddr`; the pointer is taken
                // through the box, so it stays valid when `addr` moves.
                addr: unsafe { (*addr.as_ptr()).as_ptr().cast() },
                addr_len: addr.get().len(),
                flags: libc::MSG_NOSIGNAL,
            };
            // SAFETY: the address is in a `KernelBox`; an `IoBuf`'s bytes
            // stay put when it moves.
            let (res, _, (_addr, buf)) = unsafe { Op::new(io, (addr, buf)) }.await;
            (res, buf)
        } else {
            let msg = Message::new(buf.as_ptr().cast_mut(), buf.len(), Some(&addr));
            let io = Io::SendMsg {
                fd,
                // SAFETY: `msg` owns a live `Message`; the field pointer is
                // taken without making a reference.
                msg: unsafe { ptr::addr_of!((*msg.as_ptr()).hdr) },
                flags: libc::MSG_NOSIGNAL as u32,
                zero_copy: false,
            };
            // SAFETY: the header is in a `KernelBox` and points at its own
            // fields and at `buf`'s bytes; an `IoBuf`'s bytes stay put when
            // it moves.
            let (res, _, (_msg, buf)) = unsafe { Op::new(io, (msg, buf)) }.await;
            (res, buf)
        };
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Sends `buf` as one datagram to the peer set with
    /// [`connect`](Self::connect).
    pub async fn send<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        let fd = self.sock.as_raw_fd();
        let sent = runtime::with_worker(|w| {
            // SAFETY: an `IoBuf` has `len()` initialised bytes at `as_ptr()`.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
            w.driver.borrow_mut().try_send(fd, bytes)
        });
        if let Some(res) = sent {
            return if res < 0 {
                (Err(os_err(res)), buf)
            } else {
                (Ok(res as usize), buf)
            };
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let io = Io::Send {
            fd: self.sock.as_raw_fd(),
            buf: buf.as_ptr(),
            len,
            flags: libc::MSG_NOSIGNAL,
            zero_copy: false,
        };
        // SAFETY: an `IoBuf`'s bytes stay put when it moves.
        let (res, _, buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// The address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        inet_addr(self.sock.local_addr()?)
    }

    /// The peer set with [`connect`](Self::connect).
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        inet_addr(self.sock.peer_addr()?)
    }

    /// Allows sending to broadcast addresses (`SO_BROADCAST`).
    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        self.sock.set_broadcast(broadcast)
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            held.release(&self.sock);
        }
        close_socket(&mut self.sock, &self.recv);
    }
}

impl std::fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpSocket")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::{mem, ptr};

    use socket2::SockAddr;

    use super::{parse_addr, Message};

    /// The header of a `Message` stays valid while the message moves, as it
    /// does into the operation and back, and the kernel's writes through it
    /// are seen afterwards. Miri runs this one: it is how `recvmsg` and
    /// `sendmsg` use the memory, and Miri cannot call them.
    #[test]
    fn a_message_header_survives_moves() {
        // Received: the "kernel" writes the payload and the sender through
        // the header pointer taken before the move.
        let mut buf = vec![0u8; 8];
        let msg = Message::new(buf.as_mut_ptr(), buf.len(), None);
        // SAFETY: the field pointer of a live `Message`, as `read_from` takes it.
        let hdr = unsafe { ptr::addr_of_mut!((*msg.as_ptr()).hdr) };
        let moved = (msg, 1);
        let from: SocketAddr = "192.0.2.7:4242".parse().unwrap();
        let name = SockAddr::from(from);
        // SAFETY: what the kernel does with a `msghdr`: the iovec and the
        // name point into memory the message and `buf` own.
        unsafe {
            let iov = &*(*hdr).msg_iov;
            ptr::copy_nonoverlapping(b"datagram".as_ptr(), iov.iov_base.cast::<u8>(), 8);
            assert!(name.len() <= (*hdr).msg_namelen);
            ptr::copy_nonoverlapping(
                name.as_ptr().cast::<u8>(),
                (*hdr).msg_name.cast::<u8>(),
                name.len() as usize,
            );
            (*hdr).msg_namelen = name.len();
        }
        let msg = moved.0;
        let msg = msg.get();
        assert_eq!(buf, b"datagram");
        // SAFETY: `sockaddr_storage` is plain data of exactly this size.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&msg.addr as *const libc::sockaddr_storage).cast::<u8>(),
                mem::size_of::<libc::sockaddr_storage>(),
            )
        };
        assert_eq!(
            parse_addr(&bytes[..msg.hdr.msg_namelen as usize]),
            Some(from)
        );

        // Sent: the "kernel" reads the payload and the address.
        let to: SocketAddr = "[2001:db8::1]:53".parse().unwrap();
        let payload = *b"query";
        let msg = Message::new(
            payload.as_ptr().cast_mut(),
            payload.len(),
            Some(&SockAddr::from(to)),
        );
        // SAFETY: as `send_to` takes it.
        let hdr = unsafe { ptr::addr_of!((*msg.as_ptr()).hdr) };
        let moved = (msg, 1);
        // SAFETY: the header and what it points at are alive in `moved`.
        let (sent, addr) = unsafe {
            let iov = &*(*hdr).msg_iov;
            (
                std::slice::from_raw_parts(iov.iov_base.cast::<u8>(), iov.iov_len),
                std::slice::from_raw_parts(
                    (*hdr).msg_name.cast::<u8>(),
                    (*hdr).msg_namelen as usize,
                ),
            )
        };
        assert_eq!(sent, b"query");
        assert_eq!(parse_addr(addr), Some(to));
        drop(moved);
    }
}
