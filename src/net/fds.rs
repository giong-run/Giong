//! Passing descriptors between processes over Unix sockets (`SCM_RIGHTS`),
//! and the sends and receives with addresses that Unix datagram sockets
//! share with it.
//!
//! Two things make this more than a `sendmsg` with control data:
//!
//! * **A send keeps its own copies of the descriptors.** The kernel reads
//!   the descriptor numbers when the send runs, which on io_uring can be
//!   after the future is dropped. Had the caller closed a descriptor by then
//!   and the number been reused, the peer would get an unrelated file. So a
//!   send duplicates each descriptor (`F_DUPFD_CLOEXEC`) and keeps the
//!   duplicates with the operation until the kernel is done.
//! * **A receive that nobody collects closes what it received.** Once the
//!   kernel has delivered descriptors into the control buffer they belong to
//!   this process; if the future was dropped meanwhile, [`Incoming`] closes
//!   them when the driver lets the buffers go, so no descriptor leaks.

use std::ffi::OsStr;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::SocketAddr;
use std::ptr;

use socket2::SockAddr;

use super::os_err;
use crate::buf::{IoBuf, IoBufMut};
use crate::driver::Io;
use crate::error::{raise, Code};
use crate::kbox::KernelBox;
use crate::op::Op;

/// The most descriptors one message carries (`SCM_MAX_FD` in Linux).
pub const MAX_FDS: usize = 253;

/// Bytes of control buffer for `n` descriptors (`CMSG_SPACE`).
fn space_for(n: usize) -> usize {
    // SAFETY: `CMSG_SPACE` is arithmetic.
    unsafe { libc::CMSG_SPACE((n * mem::size_of::<RawFd>()) as u32) as usize }
}

/// A control buffer aligned for `cmsghdr`, `len` bytes or a little more.
fn control_buffer(len: usize) -> Vec<u64> {
    vec![0u64; len.div_ceil(mem::size_of::<u64>())]
}

/// The error for a message the kernel had to cut short.
fn truncated() -> io::Error {
    raise(
        Code::FdPassing,
        "the kernel cut off the control data of a received message, so some of the descriptors \
         sent with it were closed before they arrived",
        "send at most 253 descriptors per message (giong::net::MAX_FDS), and do not turn on \
         SO_PASSCRED or other control data on a socket that receives descriptors.",
    )
}

/// What a send with descriptors points at, kept in one heap allocation that
/// never moves.
#[repr(C)]
struct Outgoing {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: libc::sockaddr_storage,
}

/// Sends the bytes of `buf` on `fd`, with the descriptors `fds` (may be
/// empty) and to `to` (for an unconnected datagram socket).
pub(crate) async fn send<B: IoBuf>(
    fd: RawFd,
    buf: B,
    fds: &[BorrowedFd<'_>],
    to: Option<&SockAddr>,
) -> (io::Result<usize>, B) {
    if fds.len() > MAX_FDS {
        let err = raise(
            Code::FdPassing,
            format!(
                "{} descriptors were given for one message; Linux passes at most {MAX_FDS}",
                fds.len()
            ),
            "send the descriptors in several messages of at most 253 (giong::net::MAX_FDS) each.",
        );
        return (Err(err), buf);
    }
    if !fds.is_empty() && buf.is_empty() {
        let err = raise(
            Code::FdPassing,
            "descriptors were given with no bytes to carry them",
            "send at least one byte along with the descriptors: a stream socket passes nothing \
             with an empty send.",
        );
        return (Err(err), buf);
    }
    // Our own copies, alive until the kernel has read them (see the module
    // docs). Taken before anything else, so a bad descriptor fails early.
    let mut dups = Vec::with_capacity(fds.len());
    for fd in fds {
        match fd.try_clone_to_owned() {
            Ok(dup) => dups.push(dup),
            Err(e) => return (Err(e), buf),
        }
    }
    let mut control = if dups.is_empty() {
        Vec::new()
    } else {
        control_buffer(space_for(dups.len()))
    };
    // SAFETY: all-zero is valid for these plain C structs.
    let out: KernelBox<Outgoing> = KernelBox::from_box(Box::new(unsafe { mem::zeroed() }));
    let m = out.as_ptr();
    // SAFETY: `m` points at the live `Outgoing` that `out` owns; nothing else
    // uses it yet. Every pointer stored in it is to its own fields (taken
    // through `m`), to `buf`'s bytes, or to `control`'s heap buffer, none of
    // which moves when its owner moves.
    unsafe {
        (*m).iov.iov_base = buf.as_ptr().cast_mut().cast();
        (*m).iov.iov_len = buf.len();
        (*m).hdr.msg_iov = ptr::addr_of_mut!((*m).iov);
        (*m).hdr.msg_iovlen = 1;
        if let Some(to) = to {
            // A `SockAddr` holds `len()` valid bytes, which fit in
            // `sockaddr_storage`.
            ptr::copy_nonoverlapping(
                to.as_ptr().cast::<u8>(),
                ptr::addr_of_mut!((*m).addr).cast::<u8>(),
                to.len() as usize,
            );
            (*m).hdr.msg_name = ptr::addr_of_mut!((*m).addr).cast();
            (*m).hdr.msg_namelen = to.len();
        }
        if !dups.is_empty() {
            let bytes = dups.len() * mem::size_of::<RawFd>();
            (*m).hdr.msg_control = control.as_mut_ptr().cast();
            (*m).hdr.msg_controllen = space_for(dups.len()) as _;
            let cmsg = libc::CMSG_FIRSTHDR(ptr::addr_of!((*m).hdr));
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, dup) in dups.iter().enumerate() {
                data.add(i).write_unaligned(dup.as_raw_fd());
            }
        }
    }
    let io = Io::SendMsg {
        fd,
        // SAFETY: a field of the live `Outgoing`, taken without a reference.
        msg: unsafe { ptr::addr_of_mut!((*out.as_ptr()).hdr) },
        flags: libc::MSG_NOSIGNAL as u32,
        zero_copy: false,
    };
    // SAFETY: the header and what it points at are in `out` (a `KernelBox`),
    // `control` (a `Vec`'s heap buffer) and `buf` (an `IoBuf`), all owned by
    // the operation; the descriptors the control data names are `dups`,
    // owned by it too.
    let (res, _, (buf, _control, _dups, _out)) =
        unsafe { Op::new(io, (buf, control, dups, out)) }.await;
    if res < 0 {
        (Err(os_err(res)), buf)
    } else {
        (Ok(res as usize), buf)
    }
}

/// What a receive points at: the header, the iovec, room for the sender's
/// address, and the control buffer. Closes received descriptors nobody took
/// (see the module docs).
pub(crate) struct Incoming {
    parts: KernelBox<IncomingParts>,
    control: Vec<u64>,
    /// Set once the received descriptors have been taken out.
    taken: bool,
}

#[repr(C)]
struct IncomingParts {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: libc::sockaddr_un,
}

impl Incoming {
    fn new(base: *mut u8, len: usize, fds: bool) -> Self {
        let mut control = if fds {
            // Room for the most descriptors one message carries, and for
            // credentials, so that the kernel never has to cut it short.
            control_buffer(space_for(MAX_FDS) + space_for(4))
        } else {
            Vec::new()
        };
        // SAFETY: all-zero is valid for these plain C structs.
        let parts: KernelBox<IncomingParts> =
            KernelBox::from_box(Box::new(unsafe { mem::zeroed() }));
        let m = parts.as_ptr();
        // SAFETY: as in `send`: the pointers are to `parts`' own fields,
        // to the caller's buffer and to `control`'s heap buffer.
        unsafe {
            (*m).iov.iov_base = base.cast();
            (*m).iov.iov_len = len;
            (*m).hdr.msg_iov = ptr::addr_of_mut!((*m).iov);
            (*m).hdr.msg_iovlen = 1;
            (*m).hdr.msg_name = ptr::addr_of_mut!((*m).addr).cast();
            (*m).hdr.msg_namelen = mem::size_of::<libc::sockaddr_un>() as _;
            if !control.is_empty() {
                (*m).hdr.msg_control = control.as_mut_ptr().cast();
                (*m).hdr.msg_controllen = (control.len() * mem::size_of::<u64>()) as _;
            }
        }
        Self {
            parts,
            control,
            taken: false,
        }
    }

    /// The descriptors in the control data, owned from here on. Only after
    /// the receive completed: before, the control data is all zero, and
    /// nothing is found.
    fn take_fds(&mut self) -> Vec<OwnedFd> {
        self.taken = true;
        let mut fds = Vec::new();
        if self.control.is_empty() {
            return fds;
        }
        let hdr = ptr::addr_of!(self.parts.get().hdr);
        let start = self.control.as_ptr() as usize;
        let end = start + self.control.len() * mem::size_of::<u64>();
        // SAFETY: the header is the one the kernel filled in; its control
        // pointer and length describe `self.control`, which the kernel wrote
        // `msg_controllen` bytes of. Every header is checked to lie within
        // the buffer before it is read.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(hdr);
            while !cmsg.is_null() {
                let at = cmsg as usize;
                let len = (*cmsg).cmsg_len as usize;
                if at + mem::size_of::<libc::cmsghdr>() > end
                    || len < libc::CMSG_LEN(0) as usize
                    || at + len > end
                {
                    break;
                }
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg);
                    let n = (len - (data as usize - at)) / mem::size_of::<RawFd>();
                    for i in 0..n {
                        let fd = data.cast::<RawFd>().add(i).read_unaligned();
                        if fd >= 0 {
                            fds.push(OwnedFd::from_raw_fd(fd));
                        }
                    }
                }
                cmsg = libc::CMSG_NXTHDR(hdr, cmsg);
            }
        }
        fds
    }

    fn flags(&self) -> i32 {
        self.parts.get().hdr.msg_flags
    }

    /// The sender's address, for a datagram socket.
    fn addr(&self) -> SocketAddr {
        let parts = self.parts.get();
        let len = parts.hdr.msg_namelen as usize;
        unix_addr(&parts.addr, len)
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if !self.taken {
            // Closes whatever the kernel delivered that nobody collected.
            drop(self.take_fds());
        }
    }
}

/// A received message.
pub(crate) struct Received {
    pub(crate) len: usize,
    pub(crate) fds: Vec<OwnedFd>,
    pub(crate) from: Option<SocketAddr>,
}

/// Receives into `buf` from `fd`, with room for descriptors when `fds`, and
/// with the sender's address when `from`.
pub(crate) async fn recv<B: IoBufMut>(
    fd: RawFd,
    mut buf: B,
    fds: bool,
    from: bool,
) -> (io::Result<Received>, B) {
    let len = buf.capacity();
    let incoming = Incoming::new(buf.as_mut_ptr(), len, fds);
    let io = Io::RecvMsg {
        fd,
        // SAFETY: a field of the live parts, taken without a reference.
        msg: unsafe { ptr::addr_of_mut!((*incoming.parts.as_ptr()).hdr) },
        flags: libc::MSG_CMSG_CLOEXEC,
    };
    // SAFETY: the header points at its own `KernelBox`, at `incoming`'s
    // control buffer (a `Vec`'s heap buffer) and at `buf`'s bytes; an
    // `IoBufMut`'s bytes stay put when it moves.
    let (res, _, (mut incoming, mut buf)) = unsafe { Op::new(io, (incoming, buf)) }.await;
    if res < 0 {
        return (Err(os_err(res)), buf);
    }
    let got = incoming.take_fds();
    if incoming.flags() & libc::MSG_CTRUNC != 0 {
        drop(got);
        return (Err(truncated()), buf);
    }
    let n = res as usize;
    // SAFETY: the kernel wrote `n` bytes at the start of the buffer (at most
    // its capacity; a longer datagram is cut to fit).
    unsafe { buf.set_init(n.min(len)) };
    let from = from.then(|| incoming.addr());
    (
        Ok(Received {
            len: n.min(len),
            fds: got,
            from,
        }),
        buf,
    )
}

// ------------------------------------------------------------------ addresses

/// The address of an unnamed socket, which the standard library has no
/// constructor for: read once from an unbound socket.
fn unnamed() -> SocketAddr {
    thread_local! {
        static UNNAMED: Option<SocketAddr> = std::os::unix::net::UnixDatagram::unbound()
            .and_then(|s| s.local_addr())
            .ok();
    }
    UNNAMED
        .with(Clone::clone)
        .unwrap_or_else(|| SocketAddr::from_pathname("").expect("an empty path is unnamed"))
}

/// The standard library's address for the first `len` bytes of `addr`.
pub(crate) fn unix_addr(addr: &libc::sockaddr_un, len: usize) -> SocketAddr {
    let offset = mem::size_of::<libc::sa_family_t>();
    if len <= offset || i32::from(addr.sun_family) != libc::AF_UNIX {
        return unnamed();
    }
    let path_len = (len - offset).min(addr.sun_path.len());
    // SAFETY: `sun_path` is an array of `c_char`, which has the size and
    // alignment of `u8`.
    let path: &[u8] =
        unsafe { std::slice::from_raw_parts(addr.sun_path.as_ptr().cast(), path_len) };
    match path.split_first() {
        Some((0, name)) => SocketAddr::from_abstract_name(name).unwrap_or_else(|_| unnamed()),
        _ => {
            // A path, possibly with its terminating NUL counted.
            let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
            SocketAddr::from_pathname(OsStr::from_bytes(&path[..end])).unwrap_or_else(|_| unnamed())
        }
    }
}

/// The kernel's form of a standard library address: a path or an abstract
/// name. An unnamed address cannot be bound or sent to.
pub(crate) fn sock_addr(addr: &SocketAddr) -> io::Result<SockAddr> {
    if let Some(path) = addr.as_pathname() {
        return SockAddr::unix(path);
    }
    if let Some(name) = addr.as_abstract_name() {
        let mut bytes = Vec::with_capacity(name.len() + 1);
        bytes.push(0);
        bytes.extend_from_slice(name);
        // socket2 reads a leading NUL as an abstract name.
        return SockAddr::unix(OsStr::from_bytes(&bytes));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "an unnamed Unix socket address cannot be bound, connected or sent to",
    ))
}

/// The credentials of the process at the other end of a Unix socket, as the
/// kernel recorded them when the connection was made (or the pair created).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerCred {
    /// The peer's process ID, in this process's PID namespace (0 if it is
    /// not visible from here).
    pub pid: i32,
    /// The peer's effective user ID.
    pub uid: u32,
    /// The peer's effective group ID.
    pub gid: u32,
}

/// `SO_PEERCRED` of `fd`.
pub(crate) fn peer_cred(fd: RawFd) -> io::Result<PeerCred> {
    // SAFETY: all-zero is a valid `ucred`.
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid pointer to a `ucred` and its length.
    let res = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if res != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}
