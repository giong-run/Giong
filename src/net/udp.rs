//! UDP sockets.

use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::ptr;
use std::task::{Context, Poll};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::addr::{resolved, ToSocketAddrs};
use super::sendq::{self, Sent};
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

/// Room for control messages in front of the payload when GRO or ECN is on
/// (`unstable-udp-offload`): one `int` for `UDP_GRO`, one for `IP_TOS` or
/// `IPV6_TCLASS` (each `CMSG_SPACE(4)`: 24 bytes on 64-bit Linux, 16 on
/// macOS), and the address the datagram was sent to (`IP_PKTINFO` or
/// `IP_RECVDSTADDR`, or `IPV6_PKTINFO` which is the larger:
/// `CMSG_SPACE(20)`, 40 bytes on 64-bit Linux, 32 on macOS).
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const CONTROL_SPACE: usize = 2 * CMSG_INT_SPACE + CMSG_PKTINFO6_SPACE;
/// A control message's header (`struct cmsghdr`): 16 bytes on 64-bit Linux,
/// where `cmsg_len` is a `size_t`, and 12 on macOS, where it is a
/// `socklen_t`.
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const CMSG_HDR: usize = mem::size_of::<libc::cmsghdr>();
/// What control messages are aligned to (`CMSG_ALIGN`): `size_t` on Linux,
/// 4 bytes on macOS. Both are the alignment of the header itself.
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const CMSG_ALIGN: usize = mem::align_of::<libc::cmsghdr>();
// The data of a message starts right after its header (`CMSG_DATA`), which
// holds where the header's size is a multiple of the alignment.
const _: () = assert!(CMSG_HDR % CMSG_ALIGN == 0);
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const fn cmsg_align(len: usize) -> usize {
    (len + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
}
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const CMSG_INT_SPACE: usize = cmsg_align(CMSG_HDR + 4);
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
const CMSG_PKTINFO6_SPACE: usize = cmsg_align(CMSG_HDR + mem::size_of::<libc::in6_pktinfo>());
/// Largest payload a GRO receive can hand over: 64 KiB of coalesced
/// datagrams.
#[cfg(feature = "unstable-udp-offload")]
const GRO_MAX: usize = 65_535;

/// The receive buffer size [`UdpSocket::set_gro`] needs: pass it to
/// [`Builder::recv_buffers`](crate::Builder::recv_buffers). 64 KiB of
/// coalesced datagrams plus what giong keeps in front of them.
/// `unstable-udp-offload`.
#[cfg(feature = "unstable-udp-offload")]
pub const GRO_RECV_BUFFER: usize = GRO_MAX + OVERHEAD + CONTROL_SPACE;

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

/// The header of a multishot receive that also takes control messages
/// (GRO segment size, ECN bits).
#[cfg(feature = "unstable-udp-offload")]
static RECV_HEADER_CONTROL: RecvHeader = {
    // SAFETY: as above.
    let mut hdr: libc::msghdr = unsafe { mem::zeroed() };
    hdr.msg_namelen = NAME_LEN as libc::socklen_t;
    hdr.msg_controllen = CONTROL_SPACE as _;
    RecvHeader(hdr)
};

/// Everything a one-off `sendmsg` / `recvmsg` points at, in a [`KernelBox`]
/// so that the addresses stay put, and stay valid, while the kernel uses
/// them.
struct Message {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: libc::sockaddr_storage,
    /// Control messages of a send (`UDP_SEGMENT`, `IP_TOS`), aligned for
    /// `cmsghdr`. Unused unless `unstable-udp-offload` sets them.
    #[cfg(feature = "unstable-udp-offload")]
    control: [u64; CONTROL_SPACE.div_ceil(8)],
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

/// The GRO segment size, the ECN bits and the address the datagram was sent
/// to, from the control messages of a receive. Unknown or malformed
/// messages are skipped.
#[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
pub(crate) fn parse_control(mut control: &[u8]) -> (u16, u8, Option<IpAddr>) {
    // GRO segment sizes come on Linux only.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut segment = 0;
    let (mut ecn, mut local) = (0, None);
    while control.len() >= CMSG_HDR {
        // SAFETY: `control` holds at least `CMSG_HDR` bytes, read without
        // assuming they are aligned; `cmsghdr` is plain data.
        let hdr = unsafe { control.as_ptr().cast::<libc::cmsghdr>().read_unaligned() };
        #[allow(clippy::unnecessary_cast)] // a `size_t` on Linux, a `socklen_t` on macOS
        let len = hdr.cmsg_len as usize;
        let (level, kind) = (hdr.cmsg_level, hdr.cmsg_type);
        if len < CMSG_HDR || len > control.len() {
            break;
        }
        let data = &control[CMSG_HDR..len];
        let int = || {
            data.get(..4)
                .map(|b| i32::from_ne_bytes(b.try_into().expect("four")))
        };
        match (level, kind) {
            #[cfg(target_os = "linux")]
            (libc::SOL_UDP, libc::UDP_GRO) => {
                segment = int().map_or(0, |v| u16::try_from(v).unwrap_or(0));
            }
            // IPv4 hands the TOS byte over as one byte (as `IP_RECVTOS` on
            // macOS), IPv6 the traffic class as an int (as one byte on some
            // macOS versions).
            (libc::IPPROTO_IP, libc::IP_TOS) => ecn = data.first().map_or(0, |b| b & 3),
            #[cfg(target_os = "macos")]
            (libc::IPPROTO_IP, libc::IP_RECVTOS) => ecn = data.first().map_or(0, |b| b & 3),
            (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) => {
                ecn = match data.len() {
                    1 => data[0] & 3,
                    _ => int().map_or(0, |v| (v & 3) as u8),
                };
            }
            // `struct in_pktinfo`: interface index, the local address to
            // answer from (`ipi_spec_dst`, read here), then the header's
            // destination, which for a broadcast is not an address of ours.
            // When `ipi_spec_dst` is empty (always on macOS, and possible
            // on Linux), the destination is the local address instead.
            (libc::IPPROTO_IP, libc::IP_PKTINFO) => {
                if let Some(d) = data.get(..12) {
                    let spec: [u8; 4] = d[4..8].try_into().expect("four bytes");
                    let dst: [u8; 4] = d[8..12].try_into().expect("four bytes");
                    let ip = if spec == [0; 4] { dst } else { spec };
                    local = Some(IpAddr::V4(Ipv4Addr::from(ip)));
                }
            }
            // macOS: the header's destination address (`struct in_addr`).
            #[cfg(target_os = "macos")]
            (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) => {
                if let Some(b) = data.get(..4) {
                    let ip: [u8; 4] = b.try_into().expect("four bytes");
                    local = Some(IpAddr::V4(Ipv4Addr::from(ip)));
                }
            }
            // `struct in6_pktinfo`: the destination address, then the
            // interface index.
            (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                if let Some(b) = data.get(..20).map(|d| &d[..16]) {
                    let ip: [u8; 16] = b.try_into().expect("sixteen bytes");
                    let ip = Ipv6Addr::from(ip);
                    // A dual-stack socket reports IPv4 destinations mapped.
                    local = Some(match ip.to_ipv4_mapped() {
                        Some(v4) => IpAddr::V4(v4),
                        None => IpAddr::V6(ip),
                    });
                }
            }
            _ => {}
        }
        control = control.get(cmsg_align(len)..).unwrap_or(&[]);
    }
    (segment, ecn, local)
}

fn parse_addr(name: &[u8]) -> Option<SocketAddr> {
    // Linux's `sa_family` is a `u16`; the BSDs, macOS among them, start
    // with a `u8` length and a `u8` family.
    #[cfg(target_os = "linux")]
    let family = i32::from(u16::from_ne_bytes([*name.first()?, *name.get(1)?]));
    #[cfg(target_os = "macos")]
    let family = i32::from(*name.get(1)?);
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
    // The fields below exist only with the feature, so that a datagram
    // costs what it did without it (it is moved through every `Poll`).
    /// Where the payload starts in `buf`.
    #[cfg(feature = "unstable-udp-offload")]
    payload_at: u16,
    /// GRO: the size of every coalesced datagram but the last (0: one
    /// datagram).
    #[cfg(feature = "unstable-udp-offload")]
    segment: u16,
    /// The ECN bits of the IP header (0 when not asked for).
    #[cfg(feature = "unstable-udp-offload")]
    ecn: u8,
    /// The address the datagram was sent to (when asked for).
    #[cfg(feature = "unstable-udp-offload")]
    local: Option<IpAddr>,
}

impl Datagram {
    /// Splits what a multishot `recvmsg` wrote into a buffer.
    #[cfg(not(feature = "unstable-udp-offload"))]
    fn parse(buf: RecvBuf) -> io::Result<(Self, SocketAddr)> {
        let malformed = || unexpected("the driver returned a malformed datagram");
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

    /// Splits what a multishot `recvmsg` wrote into a buffer, armed with
    /// `control_space` bytes of room for control messages.
    #[cfg(feature = "unstable-udp-offload")]
    fn parse(buf: RecvBuf, control_space: usize) -> io::Result<(Self, SocketAddr)> {
        let malformed = || unexpected("io_uring returned a malformed datagram");
        let payload_at = OVERHEAD + control_space;
        if buf.len() < payload_at {
            return Err(malformed());
        }
        let field = |i: usize| {
            let bytes: [u8; 4] = buf[i * 4..i * 4 + 4].try_into().expect("four bytes");
            u32::from_ne_bytes(bytes) as usize
        };
        let (name_len, control_len, payload_len, flags) =
            (field(0), field(1), field(2), field(3) as i32);
        let from = parse_addr(&buf[OUT_HEADER..OUT_HEADER + name_len.min(NAME_LEN)])
            .ok_or_else(malformed)?;
        let truncated = flags & libc::MSG_TRUNC != 0 || payload_len > buf.len() - payload_at;
        let (segment, ecn, local) = if control_space > 0 {
            parse_control(&buf[OVERHEAD..OVERHEAD + control_len.min(control_space)])
        } else {
            (0, 0, None)
        };
        let payload_at = payload_at as u16;
        Ok((
            Self {
                buf,
                truncated,
                payload_at,
                segment,
                ecn,
                local,
            },
            from,
        ))
    }

    /// The payload.
    pub fn as_slice(&self) -> &[u8] {
        #[cfg(feature = "unstable-udp-offload")]
        let at = usize::from(self.payload_at);
        #[cfg(not(feature = "unstable-udp-offload"))]
        let at = OVERHEAD;
        &self.buf[at..]
    }

    /// With GRO on ([`UdpSocket::set_gro`]), several datagrams of one sender
    /// can arrive coalesced in one: this is the size of each of them but the
    /// last, which may be shorter. `None` for a single datagram.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn segment_size(&self) -> Option<usize> {
        (self.segment != 0 && usize::from(self.segment) < self.as_slice().len())
            .then_some(usize::from(self.segment))
    }

    /// The datagrams in this one: one, or several coalesced by GRO.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn segments(&self) -> std::slice::Chunks<'_, u8> {
        let payload = self.as_slice();
        payload.chunks(self.segment_size().unwrap_or(payload.len()).max(1))
    }

    /// The two ECN bits of the IP header the datagram came with (0 to 3),
    /// once [`UdpSocket::set_recv_ecn`] is on; 0 otherwise.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn ecn(&self) -> u8 {
        self.ecn
    }

    /// The local address the datagram was sent to, once
    /// [`UdpSocket::set_recv_local_addr`] is on; `None` otherwise. On a
    /// socket bound to `0.0.0.0` or `[::]` this is how a server learns which
    /// of the machine's addresses a client used, to answer from the same one
    /// ([`SendOptions::source`]).
    #[cfg(feature = "unstable-udp-offload")]
    pub fn local_ip(&self) -> Option<IpAddr> {
        self.local
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
/// A send returns once the kernel has taken the datagram, with the result
/// `sendto(2)` gives. When other tasks of the worker are waiting to run, the
/// datagram waits for the end of that turn of the event loop and goes to the
/// kernel with the others sent in it, in one `sendmmsg(2)`; a task that is
/// the only one with work sends at once. Datagrams of one socket leave in
/// the order they were sent. Receives stay queued in the kernel's ring on
/// io_uring and come up to 16 per `recvmmsg(2)` on epoll, so a busy server
/// makes a fraction of a system call per datagram.
pub struct UdpSocket {
    sock: ManuallyDrop<Socket>,
    recv: MultiSlot,
    /// Receives take control messages (GRO, ECN): set before the first
    /// `recv_from`.
    #[cfg(feature = "unstable-udp-offload")]
    control: std::cell::Cell<bool>,
    /// The socket's hold on its port, for one that [`bind`](Self::bind)
    /// made on a port of the caller's choice (see `share`).
    held: Option<share::Held>,
    _not_send: PhantomData<*const ()>,
}

/// A send's answer (bytes, or a negative errno) as an `io::Result`.
fn sent_result(res: isize) -> io::Result<usize> {
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res as i32))
    } else {
        Ok(res as usize)
    }
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
    /// On macOS every worker can bind the address too, and no other process
    /// can: the sockets of one address in a process are duplicates of one
    /// socket bound without `SO_REUSEPORT`, so each datagram goes to the
    /// worker that reads first, and [`connect`](Self::connect) on one of
    /// them connects them all (bind a port of its own for a connected
    /// socket). giong also raises the
    /// socket's send buffer there to 65,535 bytes when it is smaller (the
    /// system's default refuses datagrams over 9,216 bytes), so a datagram
    /// of any length UDP allows can be sent, as on Linux.
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
            Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        })?;
        // macOS refuses a datagram longer than the socket's send buffer,
        // 9216 bytes unless changed (`net.inet.udp.maxdgram`): raised to the
        // longest datagram UDP has, so the limit is the protocol's, as on
        // Linux. A failure leaves the system's limit.
        #[cfg(target_os = "macos")]
        if sock.send_buffer_size().is_ok_and(|size| size < 65_535) {
            drop(sock.set_send_buffer_size(65_535));
        }
        let mut socket = Self::from_socket(sock);
        socket.held = held;
        Ok(socket)
    }

    /// Wraps a socket created by the standard library, with the options it
    /// was given, and does not check who else holds its port. giong adds
    /// one option on macOS, `SO_NOSIGPIPE`, and the readiness drivers (epoll
    /// and kqueue) make the socket non-blocking when they first use it:
    /// `O_NONBLOCK`, which every descriptor of the socket shares, such as
    /// one from `try_clone`.
    pub fn from_std(socket: std::net::UdpSocket) -> Self {
        // SAFETY: we take over the descriptor that `socket` owned.
        Self::from_socket(unsafe { Socket::from_raw_fd(socket.into_raw_fd()) })
    }

    fn from_socket(sock: Socket) -> Self {
        // macOS has no per-send flag against `SIGPIPE`: the socket carries
        // the option. socket2 sets it on the sockets it creates; this covers
        // one that came from somewhere else (`from_std`).
        #[cfg(target_os = "macos")]
        drop(sock.set_nosigpipe(true));
        Self {
            sock: ManuallyDrop::new(sock),
            recv: MultiSlot::new(MultiKind::Recv),
            #[cfg(feature = "unstable-udp-offload")]
            control: std::cell::Cell::new(false),
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
    /// ([`Builder::recv_buffers`](crate::Builder::recv_buffers), 16 KiB by
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
    ///
    /// Cancel safe, like [`recv_from`](Self::recv_from).
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
                    #[cfg(not(feature = "unstable-udp-offload"))]
                    MultiPoll::Data(buf) => return Poll::Ready(Datagram::parse(buf)),
                    #[cfg(feature = "unstable-udp-offload")]
                    MultiPoll::Data(buf) => {
                        let space = if self.control.get() { CONTROL_SPACE } else { 0 };
                        return Poll::Ready(Datagram::parse(buf, space));
                    }
                    MultiPoll::NeedArm => {
                        if !wait_for_pool(&mut driver, cx, true) {
                            return Poll::Pending;
                        }
                        #[cfg(not(feature = "unstable-udp-offload"))]
                        let hdr = &RECV_HEADER.0;
                        #[cfg(feature = "unstable-udp-offload")]
                        let hdr = if self.control.get() {
                            &RECV_HEADER_CONTROL.0
                        } else {
                            &RECV_HEADER.0
                        };
                        let io = MultiIo::RecvMsg {
                            fd: self.sock.as_raw_fd(),
                            hdr,
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
    /// For datagrams that do not fit a pool buffer. It must not be mixed
    /// with [`recv_from`](Self::recv_from) on one socket: once `recv_from`
    /// has been used, `read_from` fails with `InvalidInput`.
    ///
    /// Not cancel safe, unlike `recv_from`: a datagram that arrives while the
    /// future is being dropped is lost.
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
            flags: 0,
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
    /// apply (not after `recv` or `recv_from`).
    ///
    /// Not cancel safe, like `read_from`.
    pub async fn read<B: IoBufMut>(&self, buf: B) -> (io::Result<usize>, B) {
        let (res, buf) = self.read_from(buf).await;
        (res.map(|(len, _from)| len), buf)
    }

    /// Sends `buf` as one datagram to `addr`, returning the number of bytes
    /// sent. The buffer is handed back so it can be reused.
    ///
    /// Not cancel safe: if the future is dropped, the datagram may have been
    /// sent.
    pub async fn send_to<B: IoBuf>(&self, buf: B, addr: SocketAddr) -> (io::Result<usize>, B) {
        let addr = SockAddr::from(addr);
        let fd = self.sock.as_raw_fd();
        // At once where the socket has room (see `Driver::try_send_to`),
        // unless other tasks may send in this turn too: then with their
        // datagrams (see `sendq`).
        let try_now = |w: &runtime::Worker| {
            let mut driver = w.driver.borrow_mut();
            // SAFETY: an `IoBuf` has `len()` initialised bytes at `as_ptr()`.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
            (driver.try_send_to(fd, bytes, &addr), driver.sends_to())
        };
        let mut now = runtime::with_worker(|w| (!sendq::wanted(w)).then(|| try_now(w)));
        if now.is_none() {
            let queued = {
                // SAFETY: an `IoBuf`'s bytes stay put, and `buf` is held
                // until the future is done.
                unsafe { sendq::send(fd, buf.as_ptr(), buf.len(), Some(&addr)) }
            };
            if let Sent::Done(res) = queued.await {
                return (sent_result(res), buf);
            }
            now = Some(runtime::with_worker(try_now));
        }
        let (sent, sends_to) = now.expect("tried");
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
                flags: crate::sys::MSG_NOSIGNAL as u32,
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
    ///
    /// Not cancel safe: if the future is dropped, the datagram may have been
    /// sent.
    pub async fn send<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        let fd = self.sock.as_raw_fd();
        let try_now = |w: &runtime::Worker| {
            // SAFETY: an `IoBuf` has `len()` initialised bytes at `as_ptr()`.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
            w.driver.borrow_mut().try_send(fd, bytes)
        };
        // As in `send_to`.
        let mut now = runtime::with_worker(|w| (!sendq::wanted(w)).then(|| try_now(w)));
        if now.is_none() {
            let queued = {
                // SAFETY: as in `send_to`.
                unsafe { sendq::send(fd, buf.as_ptr(), buf.len(), None) }
            };
            if let Sent::Done(res) = queued.await {
                return (sent_result(res), buf);
            }
            now = Some(runtime::with_worker(try_now));
        }
        let sent = now.expect("tried");
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
            flags: crate::sys::MSG_NOSIGNAL,
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

    /// Turns UDP generic receive offload on or off (Linux 5.0+,
    /// `unstable-udp-offload`): the kernel may hand over several datagrams
    /// of one sender, of one size, as one [`Datagram`], whose
    /// [`segment_size`](Datagram::segment_size) says where to cut it. One
    /// receive then costs what one datagram did, for up to 64 KiB of them.
    /// Elsewhere (macOS), and on older kernels, turning it on fails with
    /// `ENOPROTOOPT` and datagrams come one by one.
    ///
    /// Call it before the first [`recv_from`](Self::recv_from). The worker's
    /// receive buffers must hold 64 KiB of payload (see
    /// [`Builder::recv_buffers`](crate::Builder::recv_buffers)): the kernel
    /// cuts a coalesced datagram that does not fit and drops the rest.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn set_gro(&self, on: bool) -> io::Result<()> {
        self.before_recv("set_gro")?;
        if on {
            let size = runtime::with_worker(|w| w.driver.borrow().pool().buf_len());
            let need = GRO_RECV_BUFFER;
            if size < need {
                return Err(raise(
                    Code::RecvBufferTooSmall,
                    format!(
                        "set_gro needs receive buffers of {need} bytes and this worker's are \
                         {size}: the kernel would cut coalesced datagrams and drop the rest"
                    ),
                    format!(
                        "build the runtime with Builder::recv_buffers(count, \
                         giong::net::GRO_RECV_BUFFER) ({need} bytes, or more), or leave GRO off."
                    ),
                ));
            }
        }
        #[cfg(target_os = "linux")]
        setsockopt_int(&self.sock, libc::SOL_UDP, libc::UDP_GRO, on as libc::c_int)?;
        #[cfg(not(target_os = "linux"))]
        if on {
            return Err(io::Error::from_raw_os_error(libc::ENOPROTOOPT));
        }
        if on {
            self.control.set(true);
        }
        Ok(())
    }

    /// Reports the ECN bits of received datagrams ([`Datagram::ecn`]):
    /// `IP_RECVTOS`, and `IPV6_RECVTCLASS` on an IPv6 socket. QUIC uses them
    /// for congestion control (RFC 9000, section 13.4). Call it before the
    /// first [`recv_from`](Self::recv_from). `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn set_recv_ecn(&self, on: bool) -> io::Result<()> {
        self.before_recv("set_recv_ecn")?;
        let v = on as libc::c_int;
        if self.local_addr()?.is_ipv6() {
            setsockopt_int(&self.sock, libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS, v)?;
            // A dual-stack socket gets IPv4 datagrams too; refusal is fine
            // on an IPv6-only one.
            let _ = setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_RECVTOS, v);
        } else {
            setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_RECVTOS, v)?;
        }
        if on {
            self.control.set(true);
        }
        Ok(())
    }

    /// Reports the local address each datagram was sent to
    /// ([`Datagram::local_ip`]): `IP_PKTINFO` (`IP_RECVDSTADDR` on macOS),
    /// and `IPV6_RECVPKTINFO` on an IPv6 socket. For a socket bound to `0.0.0.0` or `[::]` on a machine
    /// with several addresses. Call it before the first
    /// [`recv_from`](Self::recv_from). `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn set_recv_local_addr(&self, on: bool) -> io::Result<()> {
        self.before_recv("set_recv_local_addr")?;
        let v = on as libc::c_int;
        if self.local_addr()?.is_ipv6() {
            setsockopt_int(&self.sock, libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO, v)?;
            // A dual-stack socket gets IPv4 datagrams too; they are
            // reported through the IPv6 message, mapped.
        } else {
            #[cfg(target_os = "linux")]
            setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_PKTINFO, v)?;
            #[cfg(target_os = "macos")]
            setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_RECVDSTADDR, v)?;
        }
        if on {
            self.control.set(true);
        }
        Ok(())
    }

    /// Sends datagrams with "don't fragment" set and without the kernel's
    /// own path MTU estimate cutting them (`IP_MTU_DISCOVER` =
    /// `IP_PMTUDISC_PROBE`; on IPv6 also `IPV6_DONTFRAG`): a datagram larger
    /// than a link on the path is dropped there instead of being fragmented,
    /// which is what a protocol that probes the path MTU itself needs (QUIC,
    /// RFC 9000 section 14). A datagram larger than the local interface's
    /// MTU fails with `EMSGSIZE`. macOS has no path MTU setting:
    /// `IP_DONTFRAG` and `IPV6_DONTFRAG` there. `unstable-udp-offload`.
    #[cfg(all(feature = "unstable-udp-offload", target_os = "macos"))]
    pub fn set_dont_fragment(&self, on: bool) -> io::Result<()> {
        let v = on as libc::c_int;
        if self.local_addr()?.is_ipv6() {
            setsockopt_int(&self.sock, libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG, v)?;
            // The IPv4 side of a dual-stack socket; refused on IPv6-only.
            let _ = setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_DONTFRAG, v);
            Ok(())
        } else {
            setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_DONTFRAG, v)
        }
    }

    /// Sends datagrams with "don't fragment" set and without the kernel's
    /// own path MTU estimate cutting them (`IP_MTU_DISCOVER` =
    /// `IP_PMTUDISC_PROBE`; on IPv6 also `IPV6_DONTFRAG`): a datagram larger
    /// than a link on the path is dropped there instead of being fragmented,
    /// which is what a protocol that probes the path MTU itself needs (QUIC,
    /// RFC 9000 section 14). A datagram larger than the local interface's
    /// MTU fails with `EMSGSIZE`. `unstable-udp-offload`.
    #[cfg(all(feature = "unstable-udp-offload", target_os = "linux"))]
    pub fn set_dont_fragment(&self, on: bool) -> io::Result<()> {
        let v4 = if on {
            libc::IP_PMTUDISC_PROBE
        } else {
            libc::IP_PMTUDISC_WANT
        };
        if self.local_addr()?.is_ipv6() {
            let v6 = if on {
                libc::IPV6_PMTUDISC_PROBE
            } else {
                libc::IPV6_PMTUDISC_WANT
            };
            setsockopt_int(&self.sock, libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER, v6)?;
            setsockopt_int(
                &self.sock,
                libc::IPPROTO_IPV6,
                libc::IPV6_DONTFRAG,
                on as libc::c_int,
            )?;
            // The IPv4 side of a dual-stack socket; refused on IPv6-only.
            let _ = setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, v4);
            Ok(())
        } else {
            setsockopt_int(&self.sock, libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, v4)
        }
    }

    /// Asks for a socket receive buffer of `bytes` (`SO_RCVBUF`). The
    /// kernel doubles the value for its own bookkeeping and caps it at
    /// `net.core.rmem_max`; [`recv_buffer_size`](Self::recv_buffer_size)
    /// says what it gave. A server that receives bursts (QUIC) drops
    /// datagrams with the default of about 200 KiB.
    /// `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn set_recv_buffer_size(&self, bytes: usize) -> io::Result<()> {
        self.sock.set_recv_buffer_size(bytes)
    }

    /// The socket receive buffer the kernel gave, in bytes.
    /// `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn recv_buffer_size(&self) -> io::Result<usize> {
        self.sock.recv_buffer_size()
    }

    /// Asks for a socket send buffer of `bytes` (`SO_SNDBUF`), capped at
    /// `net.core.wmem_max`. `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn set_send_buffer_size(&self, bytes: usize) -> io::Result<()> {
        self.sock.set_send_buffer_size(bytes)
    }

    /// The socket send buffer the kernel gave, in bytes.
    /// `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn send_buffer_size(&self) -> io::Result<usize> {
        self.sock.send_buffer_size()
    }

    #[cfg(feature = "unstable-udp-offload")]
    fn before_recv(&self, what: &str) -> io::Result<()> {
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            return Err(raise(
                Code::RecvAlreadyStarted,
                format!("{what} was called after recv_from had started receiving on this socket"),
                format!("call {what} right after binding the socket, before the first recv_from."),
            ));
        }
        Ok(())
    }

    /// `true` if the kernel can split one send into several datagrams
    /// (`UDP_SEGMENT`, Linux 4.18+), as
    /// [`send_segments_to`](Self::send_segments_to) asks; `false` on macOS.
    /// `unstable-udp-offload`.
    #[cfg(feature = "unstable-udp-offload")]
    pub fn gso_supported(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut v: libc::c_int = 0;
            let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: `v` and `len` are valid for the kernel to write.
            unsafe {
                libc::getsockopt(
                    self.sock.as_raw_fd(),
                    libc::SOL_UDP,
                    libc::UDP_SEGMENT,
                    ptr::addr_of_mut!(v).cast(),
                    &mut len,
                ) == 0
            }
        }
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Sends `buf` to `addr` as datagrams of `segment_size` bytes each (the
    /// last may be shorter), in one system call: UDP generic segmentation
    /// offload (`UDP_SEGMENT`). With `segment_size` equal to or above the
    /// length it is one datagram. `ecn` (0 to 3) goes into the IP header's
    /// ECN bits. Short for [`send_to_with`](Self::send_to_with).
    ///
    /// Linux takes at most 64 segments and 64 KiB in one send, and refuses
    /// GSO on a route without checksum offload with `EIO`; see
    /// [`gso_supported`](Self::gso_supported). Where there is no GSO
    /// (macOS), a send of more than one segment fails with `EOPNOTSUPP` and
    /// sends nothing. `unstable-udp-offload`.
    ///
    /// Not cancel safe: if the future is dropped, the datagrams may have
    /// been sent.
    #[cfg(feature = "unstable-udp-offload")]
    pub async fn send_segments_to<B: IoBuf>(
        &self,
        buf: B,
        addr: SocketAddr,
        segment_size: usize,
        ecn: u8,
    ) -> (io::Result<usize>, B) {
        let options = SendOptions::new().segment_size(segment_size).ecn(ecn);
        self.send_to_with(buf, addr, &options).await
    }

    /// Sends `buf` to `addr` with what `options` asks for: split into
    /// datagrams of one size by the kernel (GSO), ECN bits, and the local
    /// address to send from. With default options it is
    /// [`send_to`](Self::send_to). `unstable-udp-offload`.
    ///
    /// Not cancel safe: if the future is dropped, the datagrams may have
    /// been sent.
    #[cfg(feature = "unstable-udp-offload")]
    pub async fn send_to_with<B: IoBuf>(
        &self,
        buf: B,
        addr: SocketAddr,
        options: &SendOptions,
    ) -> (io::Result<usize>, B) {
        let (segment_size, ecn) = (options.segment_size, options.ecn);
        #[cfg(not(target_os = "linux"))]
        if segment_size > 0 && segment_size < buf.len() {
            return (Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)), buf);
        }
        let sa = SockAddr::from(addr);
        let msg = Message::new(buf.as_ptr().cast_mut(), buf.len(), Some(&sa));
        let m = msg.as_ptr();
        // SAFETY: `m` is the live `Message` that `msg` owns; the control
        // buffer is `CONTROL_SPACE` bytes: room for the two `int` messages
        // (`CMSG_INT_SPACE` each) and one address message (at most
        // `CMSG_PKTINFO6_SPACE`) written here.
        unsafe {
            let control = ptr::addr_of_mut!((*m).control).cast::<u8>();
            let mut at = 0;
            let mut put = |level: i32, kind: i32, data: &[u8]| {
                let len = CMSG_HDR + data.len();
                let p = control.add(at);
                // All-zero is a valid `cmsghdr` (musl pads it).
                let mut hdr: libc::cmsghdr = mem::zeroed();
                hdr.cmsg_len = len as _;
                hdr.cmsg_level = level;
                hdr.cmsg_type = kind;
                p.cast::<libc::cmsghdr>().write_unaligned(hdr);
                ptr::copy_nonoverlapping(data.as_ptr(), p.add(CMSG_HDR), data.len());
                at += cmsg_align(len);
            };
            #[cfg(target_os = "linux")]
            if segment_size > 0 && segment_size < buf.len() {
                let seg = u16::try_from(segment_size).unwrap_or(u16::MAX);
                put(libc::SOL_UDP, libc::UDP_SEGMENT, &seg.to_ne_bytes());
            }
            if ecn & 3 != 0 {
                let v = libc::c_int::from(ecn & 3);
                // To an IPv4-mapped address a dual-stack socket sends IPv4,
                // where the kernel reads IP_TOS and ignores IPV6_TCLASS.
                let mapped = matches!(addr, SocketAddr::V6(a) if a.ip().to_ipv4_mapped().is_some());
                if addr.is_ipv6() && !mapped {
                    put(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, &v.to_ne_bytes());
                } else {
                    put(libc::IPPROTO_IP, libc::IP_TOS, &v.to_ne_bytes());
                }
            }
            match (options.source, addr.is_ipv6()) {
                (Some(IpAddr::V4(ip)), false) => {
                    // `struct in_pktinfo`: interface 0 (the route decides),
                    // `ipi_spec_dst` = the source address, `ipi_addr` unused.
                    // macOS reads it the same way.
                    let mut info = [0u8; 12];
                    info[4..8].copy_from_slice(&ip.octets());
                    put(libc::IPPROTO_IP, libc::IP_PKTINFO, &info);
                }
                (Some(ip), true) => {
                    // `struct in6_pktinfo`: the source address, interface 0.
                    let ip = match ip {
                        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                        IpAddr::V6(v6) => v6,
                    };
                    let mut info = [0u8; 20];
                    info[..16].copy_from_slice(&ip.octets());
                    put(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, &info);
                }
                // An IPv6 source for an IPv4 destination: the kernel picks.
                _ => {}
            }
            if at > 0 {
                (*m).hdr.msg_control = control.cast();
                (*m).hdr.msg_controllen = at as _;
            }
        }
        // At once where the socket has room (see `Driver::try_send_to`).
        let fd = self.sock.as_raw_fd();
        // SAFETY: as above; the header lives until the end of this function.
        let hdr = unsafe { ptr::addr_of!((*m).hdr) };
        if let Some(res) = runtime::with_worker(|w| w.driver.borrow_mut().try_sendmsg(fd, hdr)) {
            return if res < 0 {
                (Err(os_err(res)), buf)
            } else {
                (Ok(res as usize), buf)
            };
        }
        let io = Io::SendMsg {
            fd: self.sock.as_raw_fd(),
            // SAFETY: `msg` owns a live `Message`; the field pointer is taken
            // without making a reference.
            msg: unsafe { ptr::addr_of!((*msg.as_ptr()).hdr) },
            flags: crate::sys::MSG_NOSIGNAL as u32,
            zero_copy: false,
        };
        // SAFETY: the header is in a `KernelBox` and points at its own fields
        // and at `buf`'s bytes; an `IoBuf`'s bytes stay put when it moves.
        let (res, _, (_msg, buf)) = unsafe { Op::new(io, (msg, buf)) }.await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Allows sending to broadcast addresses (`SO_BROADCAST`).
    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        self.sock.set_broadcast(broadcast)
    }
}

#[cfg(feature = "unstable-udp-offload")]
fn setsockopt_int(sock: &Socket, level: i32, name: i32, v: libc::c_int) -> io::Result<()> {
    // SAFETY: `v` is a valid `int` for the kernel to read.
    let r = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            level,
            name,
            ptr::addr_of!(v).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What [`UdpSocket::send_to_with`] does besides sending: nothing by
/// default. `unstable-udp-offload`.
///
/// ```
/// # #[cfg(feature = "unstable-udp-offload")] {
/// use giong::net::SendOptions;
/// // Ten 1200-byte datagrams from one 12,000-byte buffer, marked ECT(0).
/// let options = SendOptions::new().segment_size(1200).ecn(2);
/// # let _ = options;
/// # }
/// ```
#[cfg(feature = "unstable-udp-offload")]
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SendOptions {
    segment_size: usize,
    ecn: u8,
    source: Option<IpAddr>,
}

#[cfg(feature = "unstable-udp-offload")]
impl SendOptions {
    /// No options: one datagram, no ECN bits, the source address the route
    /// gives.
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits the buffer into datagrams of `bytes` each (the last may be
    /// shorter) in the kernel (`UDP_SEGMENT`); 0, or the buffer's length or
    /// more, is one datagram.
    pub fn segment_size(mut self, bytes: usize) -> Self {
        self.segment_size = bytes;
        self
    }

    /// The two ECN bits of the IP header (0 to 3; higher bits are ignored).
    pub fn ecn(mut self, ecn: u8) -> Self {
        self.ecn = ecn & 3;
        self
    }

    /// The local address to send from, for a socket bound to `0.0.0.0` or
    /// `[::]` on a machine with several addresses: answer from the address
    /// the request was sent to ([`Datagram::local_ip`]). The send fails with
    /// `EINVAL` if the machine does not have the address.
    pub fn source(mut self, ip: IpAddr) -> Self {
        self.source = Some(ip);
        self
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

impl std::os::fd::AsFd for UdpSocket {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&*self.sock)
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

    use super::{
        cmsg_align, parse_addr, parse_control, Message, CMSG_HDR, CMSG_INT_SPACE,
        CMSG_PKTINFO6_SPACE, CONTROL_SPACE,
    };
    #[cfg(target_os = "linux")]
    use std::net::Ipv4Addr;
    use std::net::{IpAddr, Ipv6Addr};

    /// A control message as the kernel lays it out, padded to its space.
    fn cmsg(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
        // SAFETY: all-zero is a valid `cmsghdr`.
        let mut hdr: libc::cmsghdr = unsafe { mem::zeroed() };
        hdr.cmsg_len = (CMSG_HDR + data.len()) as _;
        hdr.cmsg_level = level;
        hdr.cmsg_type = kind;
        // SAFETY: `cmsghdr` is plain data of `CMSG_HDR` bytes.
        let mut v = unsafe {
            std::slice::from_raw_parts(ptr::addr_of!(hdr).cast::<u8>(), CMSG_HDR).to_vec()
        };
        v.extend(data);
        v.resize(cmsg_align(v.len()), 0);
        v
    }

    /// Sets the `cmsg_len` of the message at the start of `v`.
    fn set_len(v: &mut [u8], len: usize) {
        // SAFETY: `v` holds at least one header, read unaligned.
        let mut hdr: libc::cmsghdr = unsafe { v.as_ptr().cast::<libc::cmsghdr>().read_unaligned() };
        hdr.cmsg_len = len as _;
        // SAFETY: `v` holds at least one header.
        unsafe { v.as_mut_ptr().cast::<libc::cmsghdr>().write_unaligned(hdr) };
    }

    /// macOS: the TOS byte comes as `IP_RECVTOS`, the IPv4 destination as
    /// `IP_RECVDSTADDR` or as an `IP_PKTINFO` without `ipi_spec_dst`, and
    /// the traffic class sometimes as one byte.
    #[test]
    #[cfg(target_os = "macos")]
    fn control_messages_of_macos_are_read() {
        let tos = cmsg(libc::IPPROTO_IP, libc::IP_RECVTOS, &[0b1011_1101]);
        let dst = cmsg(libc::IPPROTO_IP, libc::IP_RECVDSTADDR, &[192, 0, 2, 9]);
        assert_eq!(tos.len(), CMSG_INT_SPACE);
        assert_eq!(
            parse_control(&[tos, dst].concat()),
            (0, 1, Some(IpAddr::from([192, 0, 2, 9])))
        );
        let mut info = [0u8; 12];
        info[8..12].copy_from_slice(&[10, 0, 0, 9]);
        let info = cmsg(libc::IPPROTO_IP, libc::IP_PKTINFO, &info);
        assert_eq!(
            parse_control(&info),
            (0, 0, Some(IpAddr::from([10, 0, 0, 9])))
        );
        let tclass = cmsg(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, &[2]);
        assert_eq!(parse_control(&tclass), (0, 2, None));
        let mut v6 = [0u8; 20];
        v6[..16].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        let v6 = cmsg(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, &v6);
        assert_eq!(v6.len(), CMSG_PKTINFO6_SPACE);
        assert!(2 * CMSG_INT_SPACE + v6.len() <= CONTROL_SPACE);
        assert_eq!(
            parse_control(&v6),
            (0, 0, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        let mut bad = v6.clone();
        set_len(&mut bad, 999);
        assert_eq!(parse_control(&bad), (0, 0, None));
    }

    /// GRO and ECN control messages are read, whatever their order; a
    /// malformed or unknown one ends or is skipped, never read past.
    #[test]
    #[cfg(target_os = "linux")]
    fn control_messages_give_segment_size_and_ecn() {
        let gro = cmsg(libc::SOL_UDP, libc::UDP_GRO, &1200i32.to_ne_bytes());
        let tos = cmsg(libc::IPPROTO_IP, libc::IP_TOS, &[0b1011_1110]);
        let tclass = cmsg(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, &3i32.to_ne_bytes());
        assert_eq!(gro.len(), CMSG_INT_SPACE);
        assert_eq!(
            parse_control(&[gro.clone(), tos.clone()].concat()),
            (1200, 2, None)
        );
        assert_eq!(
            parse_control(&[tclass, gro.clone()].concat()),
            (1200, 3, None)
        );
        assert_eq!(parse_control(&tos), (0, 2, None));
        assert_eq!(parse_control(&[]), (0, 0, None));
        // A length past the end, or shorter than a header, stops parsing.
        let mut bad = gro.clone();
        set_len(&mut bad, 999);
        assert_eq!(parse_control(&bad), (0, 0, None));
        let mut bad = gro.clone();
        set_len(&mut bad, 3);
        assert_eq!(parse_control(&bad), (0, 0, None));
        // An unknown message is skipped; a GRO size above u16 is ignored.
        let other = cmsg(libc::SOL_SOCKET, libc::SO_TIMESTAMP, &[0; 16]);
        assert_eq!(
            parse_control(&[other, gro.clone()].concat()),
            (1200, 0, None)
        );
        let huge = cmsg(libc::SOL_UDP, libc::UDP_GRO, &70_000i32.to_ne_bytes());
        assert_eq!(parse_control(&huge), (0, 0, None));
        // The address a datagram was sent to: IPv4 (`ipi_spec_dst`), IPv6,
        // and IPv4 through a dual-stack socket. All three kinds fit the
        // room a receive has for them.
        let mut v4 = [0u8; 12];
        v4[4..8].copy_from_slice(&[127, 0, 0, 2]);
        v4[8..12].copy_from_slice(&[10, 0, 0, 9]);
        let v4 = cmsg(libc::IPPROTO_IP, libc::IP_PKTINFO, &v4);
        let local = Some(IpAddr::from([127, 0, 0, 2]));
        assert_eq!(
            parse_control(&[gro.clone(), tos, v4].concat()),
            (1200, 2, local)
        );
        let mut v6 = [0u8; 20];
        v6[..16].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        let v6 = cmsg(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, &v6);
        let all = [gro.clone(), v6.clone()].concat();
        assert!(all.len() + CMSG_INT_SPACE <= CONTROL_SPACE);
        assert_eq!(v6.len(), CMSG_PKTINFO6_SPACE);
        assert_eq!(
            parse_control(&all),
            (1200, 0, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        let mut mapped = [0u8; 20];
        mapped[..16].copy_from_slice(&Ipv4Addr::new(192, 0, 2, 7).to_ipv6_mapped().octets());
        let mapped = cmsg(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, &mapped);
        assert_eq!(
            parse_control(&mapped),
            (0, 0, Some(IpAddr::from([192, 0, 2, 7])))
        );
        // Too short to hold an address: skipped.
        let short = cmsg(libc::IPPROTO_IP, libc::IP_PKTINFO, &[0; 8]);
        assert_eq!(parse_control(&short), (0, 0, None));
    }

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
