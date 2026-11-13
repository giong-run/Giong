//! [`RequestId`]: an id for every request, in the response and the logs.

use std::cell::Cell;
use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

use crate::req::IdBuf;
use crate::{Error, Middleware, Next, Req, Response};

/// Gives every request an id: in [`Req::request_id`], in error responses, in
/// the [`Logger`](super::Logger)'s lines, and in an `X-Request-Id` response
/// header, so that a client can quote it and an operator can find the
/// request in the log.
///
/// Ids are 20 hexadecimal digits: 8 chosen at random when the process
/// starts, 2 for the worker and 10 counting that worker's requests. Making
/// one takes no lock and no system call.
///
/// An id the client sent is ignored unless
/// [`trust_incoming`](Self::trust_incoming) says otherwise, because clients
/// can send anything.
#[derive(Clone, Debug)]
pub struct RequestId {
    header: Box<str>,
    trust_incoming: bool,
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestId {
    /// Ids in the `X-Request-Id` header, always made here.
    pub fn new() -> Self {
        Self {
            header: "X-Request-Id".into(),
            trust_incoming: false,
        }
    }

    /// The header the id goes in (and, with
    /// [`trust_incoming`](Self::trust_incoming), comes from). Default:
    /// `X-Request-Id`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid header name (`GIONG-E1213`).
    #[track_caller]
    pub fn header(mut self, name: &str) -> Self {
        if !crate::header::is_token(name) {
            super::invalid_config(
                format!("RequestId::header: {name:?} is not a header name"),
                "use a name such as \"X-Request-Id\".",
            );
        }
        self.header = name.into();
        self
    }

    /// Keeps the id a request arrives with, when it has one that is 1 to 64
    /// characters from `A-Z a-z 0-9 - _ . : / + =` (default: `false`).
    ///
    /// Turn this on behind a proxy or gateway you run that sets the header,
    /// so that one id follows the request through every service.
    pub fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }
}

impl<S: 'static> Middleware<S> for RequestId {
    async fn call(&self, mut req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let incoming = if self.trust_incoming {
            req.header_str(&self.header).and_then(IdBuf::new)
        } else {
            None
        };
        let id = incoming.unwrap_or_else(generate);
        req.set_id_buf(id);
        let resp = next.run(req).await;
        Ok(resp
            .remove_header(&self.header)
            .header(&self.header, id.as_str()))
    }
}

thread_local! {
    static COUNTER: Cell<u64> = const { Cell::new(0) };
}

/// A new id: process prefix, worker, counter.
pub(crate) fn generate() -> IdBuf {
    static PREFIX: OnceLock<u32> = OnceLock::new();
    let prefix = *PREFIX.get_or_init(|| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(t.as_nanos());
        }
        h.finish() as u32
    });
    let worker = giong::try_worker_id().unwrap_or(0xff) as u64 & 0xff;
    let count = COUNTER.with(|c| {
        let n = c.get().wrapping_add(1);
        c.set(n);
        n
    });
    let mut digits = [0u8; 20];
    hex(&mut digits[..8], u64::from(prefix));
    hex(&mut digits[8..10], worker);
    hex(&mut digits[10..], count);
    // Hexadecimal digits are always a valid id.
    IdBuf::new(std::str::from_utf8(&digits).unwrap_or("0")).expect("hex digits")
}

/// Writes the low `out.len()` hex digits of `n` into `out`.
fn hex(out: &mut [u8], mut n: u64) {
    for slot in out.iter_mut().rev() {
        *slot = b"0123456789abcdef"[(n & 0xf) as usize];
        n >>= 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_hex_with_a_shared_prefix() {
        let a = generate();
        let b = generate();
        assert_eq!(a.as_str().len(), 20);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str()[..10], b.as_str()[..10]);
    }
}
