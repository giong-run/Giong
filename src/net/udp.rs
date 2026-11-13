//! UDP sockets.

use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::task::{Context, Poll};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::{close_socket, inet_addr, no_addresses, os_err, unexpected, wait_for_pool, MultiSlot};
use crate::buf::{IoBuf, IoBufMut, RecvBuf};
use crate::driver::{buffer_id, Io, MultiIo, MultiKind, MultiPoll};
use crate::error::{raise, Code};
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

/// Everything a one-off `sendmsg` / `recvmsg` points at, boxed so that the
/// addresses stay put while the kernel uses them.
struct Message {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: libc::sockaddr_storage,
}

impl Message {
    fn new(base: *mut u8, len: usize) -> Box<Self> {
        // SAFETY: all-zero is valid for these plain C structs.
        let mut msg: Box<Self> = Box::new(unsafe { mem::zeroed() });
        msg.iov.iov_base = base.cast();
        msg.iov.iov_len = len;
        msg.hdr.msg_iov = &mut msg.iov;
        msg.hdr.msg_iovlen = 1;
        msg.hdr.msg_name = (&mut msg.addr as *mut libc::sockaddr_storage).cast();
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
/// between tasks; any number of them may send at the same time, one at a time
/// may receive.
pub struct UdpSocket {
    sock: ManuallyDrop<Socket>,
    recv: MultiSlot,
    _not_send: PhantomData<*const ()>,
}

impl UdpSocket {
    /// Binds to `addr` with `SO_REUSEADDR` and `SO_REUSEPORT` set, so every
    /// worker can bind its own socket to the same port and let the kernel
    /// spread incoming datagrams across cores (all datagrams of one sender
    /// address go to the same worker).
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let mut last_err = None;
        for addr in addr.to_socket_addrs()? {
            match Self::bind_addr(addr) {
                Ok(socket) => return Ok(socket),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(no_addresses))
    }

    fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;
        sock.bind(&addr.into())?;
        Ok(Self::from_socket(sock))
    }

    /// Wraps a socket created by the standard library.
    pub fn from_std(socket: std::net::UdpSocket) -> Self {
        // SAFETY: we take over the descriptor that `socket` owned.
        Self::from_socket(unsafe { Socket::from_raw_fd(socket.into_raw_fd()) })
    }

    fn from_socket(sock: Socket) -> Self {
        Self {
            sock: ManuallyDrop::new(sock),
            recv: MultiSlot::new(MultiKind::Recv),
            _not_send: PhantomData,
        }
    }

    /// Sets the peer: [`send`](Self::send) goes to `addr`, and only datagrams
    /// from `addr` are received.
    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let mut last_err = None;
        for addr in addr.to_socket_addrs()? {
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
        poll_fn(|cx| self.poll_recv_from(cx)).await
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
                    MultiPoll::Item(res, flags) => {
                        let bid = buffer_id(flags);
                        return Poll::Ready(match (res, bid) {
                            (n, Some(bid)) if n > 0 => Datagram::parse(RecvBuf::pooled(
                                driver.pool().clone(),
                                bid,
                                n as usize,
                            )),
                            (n, bid) => {
                                if let Some(bid) = bid {
                                    driver.pool().recycle(bid);
                                }
                                if n < 0 {
                                    Err(os_err(n))
                                } else {
                                    Err(unexpected("io_uring returned a datagram without a buffer"))
                                }
                            }
                        });
                    }
                    MultiPoll::Heap(data) => {
                        return Poll::Ready(Datagram::parse(RecvBuf::heap(data)))
                    }
                    MultiPoll::NeedArm => {
                        if !wait_for_pool(&mut driver, cx) {
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
    pub async fn read_from<B: IoBufMut>(&self, mut buf: B) -> (io::Result<(usize, SocketAddr)>, B) {
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            let err = raise(
                Code::MixedReads,
                "read_from cannot be used after recv_from on the same socket",
                "use either recv_from (buffers from the runtime's pool) or read_from (your own \
                 buffer) on one socket, not both.",
            );
            return (Err(err), buf);
        }
        let mut msg = Message::new(buf.as_mut_ptr(), buf.capacity());
        msg.hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let io = Io::RecvMsg {
            fd: self.sock.as_raw_fd(),
            msg: &mut msg.hdr,
        };
        let (res, _, (msg, mut buf)) = Op::new(io, (msg, buf)).await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
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
        let mut msg = Message::new(buf.as_ptr().cast_mut(), buf.len());
        let addr = SockAddr::from(addr);
        // SAFETY: a `SockAddr` holds `len()` valid bytes, which fit in
        // `sockaddr_storage`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                addr.as_ptr().cast::<u8>(),
                (&mut msg.addr as *mut libc::sockaddr_storage).cast::<u8>(),
                addr.len() as usize,
            );
        }
        msg.hdr.msg_namelen = addr.len();
        let io = Io::SendMsg {
            fd: self.sock.as_raw_fd(),
            msg: &msg.hdr,
            flags: libc::MSG_NOSIGNAL as u32,
        };
        let (res, _, (_msg, buf)) = Op::new(io, (msg, buf)).await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Sends `buf` as one datagram to the peer set with
    /// [`connect`](Self::connect).
    pub async fn send<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let io = Io::Send {
            fd: self.sock.as_raw_fd(),
            buf: buf.as_ptr(),
            len,
            flags: libc::MSG_NOSIGNAL,
        };
        let (res, _, buf) = Op::new(io, buf).await;
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
