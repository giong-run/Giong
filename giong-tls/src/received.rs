//! What [`TlsStream::recv`](crate::TlsStream::recv) returns.

use std::fmt;
use std::ops::Deref;

/// Decrypted bytes from [`TlsStream::recv`](crate::TlsStream::recv). Read
/// them as a `&[u8]` (it derefs to one), send them on as they are (it is a
/// [`giong::IoBuf`], so an echo is `tls.write_all(data)`), or take them with
/// [`into_vec`](Self::into_vec).
///
/// A type of its own rather than a `Vec<u8>`, so that where the kernel
/// decrypts (kernel TLS) the bytes can be handed over in the worker's
/// receive buffer, as [`TcpStream::recv`](giong::net::TcpStream::recv)
/// does, without a copy and without changing this API. `into_vec` copies
/// then, and only then.
pub struct Received(pub(crate) Vec<u8>);

impl Received {
    /// The bytes, as a `Vec<u8>`.
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Deref for Received {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Received {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Received> for Vec<u8> {
    fn from(received: Received) -> Self {
        received.into_vec()
    }
}

impl PartialEq<[u8]> for Received {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for Received {
    fn eq(&self, other: &[u8; N]) -> bool {
        **self == other[..]
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for Received {
    fn eq(&self, other: &&[u8; N]) -> bool {
        **self == other[..]
    }
}

impl PartialEq<&[u8]> for Received {
    fn eq(&self, other: &&[u8]) -> bool {
        **self == **other
    }
}

impl PartialEq<Vec<u8>> for Received {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == other[..]
    }
}

impl fmt::Debug for Received {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Received")
            .field("len", &self.0.len())
            .finish()
    }
}

// SAFETY: the bytes are a `Vec`'s, on the heap, so they stay where they are
// when the value moves, and only the owner of the value can reach them
// (`Received` gives out shared references only).
unsafe impl giong::IoBuf for Received {
    fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}
