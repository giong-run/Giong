//! Unix domain sockets on Windows: Winsock's `AF_UNIX`, stream sockets
//! only, from Windows 10 version 1803 (Server 2019).
//!
//! Connected and accepted through the completion port like TCP: `AcceptEx`
//! works on them as it is, and `ConnectEx` once the socket is bound to the
//! unnamed address (see `driver::iocp`). What Windows lacks is handled here:
//! the standard library has no address type for them, Winsock takes paths
//! in UTF-8 only, and there is no `socketpair`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use socket2::{Domain, SockAddr, Socket, Type};
use windows_sys::Win32::Networking::WinSock::{
    SOCKADDR_UN, WSAEAFNOSUPPORT, WSAEPROTONOSUPPORT, WSAESOCKTNOSUPPORT,
};

use crate::error::{raise, Code};

/// The longest path Winsock takes, in bytes of UTF-8: `sun_path` holds 108
/// bytes, the last of them the terminating NUL.
const MAX_PATH_LEN: usize = 107;

/// The address of a Unix domain socket on Windows, as
/// [`UnixListener::local_addr`](super::UnixListener::local_addr),
/// [`UnixStream::local_addr`](super::UnixStream::local_addr) and
/// [`UnixStream::peer_addr`](super::UnixStream::peer_addr) return it. It
/// mirrors the standard library's Unix `SocketAddr`: a path, or unnamed
/// (the client end of most connections). Windows has no abstract names.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct UnixSocketAddr {
    path: Option<PathBuf>,
}

impl UnixSocketAddr {
    /// The path the socket is bound to, or `None` if it is unnamed.
    pub fn as_pathname(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the socket is bound to no path.
    pub fn is_unnamed(&self) -> bool {
        self.path.is_none()
    }

    /// Reads an `AF_UNIX` address as Winsock wrote it.
    fn from_sockaddr(addr: &SockAddr) -> Self {
        // SAFETY: a `SockAddr` holds room for any address, `SOCKADDR_UN`
        // included; its length says how much of it the call wrote.
        let un = unsafe { &*addr.as_ptr().cast::<SOCKADDR_UN>() };
        let room = (addr.len() as usize)
            .saturating_sub(std::mem::size_of::<u16>())
            .min(un.sun_path.len());
        let bytes: Vec<u8> = un.sun_path[..room]
            .iter()
            .map(|&b| b as u8)
            .take_while(|&b| b != 0)
            .collect();
        let path = (!bytes.is_empty()).then(|| {
            // Winsock reads and writes UTF-8; anything else is shown as
            // the standard library shows invalid UTF-8.
            PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
        });
        Self { path }
    }
}

impl fmt::Debug for UnixSocketAddr {
    /// As the standard library shows a Unix address: `"C:\run\app.sock"
    /// (pathname)` or `(unnamed)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{path:?} (pathname)"),
            None => f.write_str("(unnamed)"),
        }
    }
}

pub(super) fn local_addr(sock: &Socket) -> io::Result<UnixSocketAddr> {
    Ok(UnixSocketAddr::from_sockaddr(&sock.local_addr()?))
}

pub(super) fn peer_addr(sock: &Socket) -> io::Result<UnixSocketAddr> {
    Ok(UnixSocketAddr::from_sockaddr(&sock.peer_addr()?))
}

/// A new `AF_UNIX` stream socket, or a coded error on a Windows without
/// them.
pub(super) fn new_socket() -> io::Result<Socket> {
    Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|e| match e.raw_os_error() {
        Some(WSAEAFNOSUPPORT | WSAEPROTONOSUPPORT | WSAESOCKTNOSUPPORT) => raise(
            Code::UnixSocketsUnavailable,
            format!("this Windows has no Unix domain sockets ({e})"),
            "run on Windows 10 version 1803 or later, or Windows Server 2019 or later; \
                 on older systems use TCP on a loopback address instead.",
        ),
        _ => e,
    })
}

/// The address of the socket file `path`, which Winsock takes in UTF-8
/// and up to [`MAX_PATH_LEN`] bytes.
pub(super) fn socket_addr(path: &Path) -> io::Result<SockAddr> {
    let Some(text) = path.to_str() else {
        return Err(raise(
            Code::InvalidPath,
            format!("the Unix socket path {path:?} is not valid Unicode"),
            "Windows takes Unix socket paths in UTF-8: use a path made of valid Unicode \
             characters.",
        ));
    };
    if text.len() > MAX_PATH_LEN {
        return Err(raise(
            Code::UnixPathTooLong,
            format!(
                "the Unix socket path {path:?} is {} bytes long; Windows takes at most \
                 {MAX_PATH_LEN}",
                text.len()
            ),
            "use a shorter path: a relative one, or a directory near the root of the drive \
             (C:\\run\\app.sock).",
        ));
    }
    SockAddr::unix(path)
}

/// Binds a new socket to `path`. A file already there is not removed: it
/// may belong to a server still running.
pub(super) fn bind_socket(path: &Path) -> io::Result<Socket> {
    let addr = socket_addr(path)?;
    let sock = new_socket()?;
    match sock.bind(&addr) {
        Ok(()) => Ok(sock),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse && path.exists() => Err(io::Error::from(
            crate::error::Error::new(
                Code::UnixSocketInUse,
                format!("cannot listen on {path:?}: a file is already there"),
                "if no server listens on it any more (a listener does not remove its \
                     file when it closes), remove the file with std::fs::remove_file and \
                     bind again; otherwise pick another path.",
            )
            .with_source(e),
        )),
        Err(e) => Err(e),
    }
}

/// Two connected sockets, made through a listener bound to a fresh path
/// in the temporary directory, whose file is removed before returning.
pub(super) fn pair() -> io::Result<(Socket, Socket)> {
    let dir = std::env::temp_dir();
    let mut tries = 0;
    loop {
        let mut random = [0u8; 8];
        crate::sys::random_bytes(&mut random);
        let path = dir.join(format!("giong-{:016x}.sock", u64::from_ne_bytes(random)));
        let addr = socket_addr(&path)?;
        let listener = new_socket()?;
        if let Err(e) = listener.bind(&addr) {
            // Another file of that name: draw again.
            if e.kind() == io::ErrorKind::AddrInUse && tries < 8 {
                tries += 1;
                continue;
            }
            return Err(e);
        }
        // The sockets block here: the connect is to a listener with room
        // in its queue, so it finishes at once, and the accept takes it.
        let made = (|| {
            listener.listen(1)?;
            let a = new_socket()?;
            a.connect(&addr)?;
            let (b, _) = listener.accept()?;
            Ok((a, b))
        })();
        drop(listener);
        let _ = std::fs::remove_file(&path);
        return made;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_over_107_bytes_and_invalid_unicode_are_refused_with_a_code() {
        let long = PathBuf::from("a".repeat(MAX_PATH_LEN + 1));
        let err = socket_addr(&long).unwrap_err();
        assert_eq!(crate::error::code(&err), Some(Code::UnixPathTooLong));
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(socket_addr(&PathBuf::from("a".repeat(MAX_PATH_LEN))).is_ok());
    }

    #[test]
    fn addresses_read_as_the_standard_library_shows_them() {
        let named = UnixSocketAddr::from_sockaddr(&SockAddr::unix(r"C:\run\app.sock").unwrap());
        assert_eq!(named.as_pathname(), Some(Path::new(r"C:\run\app.sock")));
        assert!(!named.is_unnamed());
        assert_eq!(format!("{named:?}"), r#""C:\\run\\app.sock" (pathname)"#);
        let unnamed = UnixSocketAddr { path: None };
        assert!(unnamed.is_unnamed());
        assert_eq!(format!("{unnamed:?}"), "(unnamed)");
    }
}
