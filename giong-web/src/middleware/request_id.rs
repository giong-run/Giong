//! [`RequestId`]: an id for every request, in the response and the logs.

use std::cell::Cell;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::req::IdBuf;
use crate::{Error, Middleware, Next, Req, Response};

/// Gives every request an id: in [`Req::request_id`], in error responses, in
/// the [`Logger`](super::Logger)'s lines, and in an `X-Request-Id` response
/// header, so that a client can quote it and an operator can find the
/// request in the log.
///
/// Ids are 32 lowercase hexadecimal digits, 128 bits like a UUID: 16 chosen
/// at random when the process starts and 16 counting the process's ids, so
/// that no two ids of one process are the same, whatever thread or runtime
/// made them. Each thread reserves 65,536 numbers of the count at a time,
/// so the count is shared without a lock: two threads' ids interleave in
/// blocks rather than in the order they were made. Two processes share the
/// random part with even odds only after some five billion starts, and the
/// count does not wrap in practice. Making one takes no lock and no
/// system call. The length and the digits are part of the format, changed
/// only in a breaking release; what the parts mean is for reading a log,
/// not for parsing.
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

/// How many numbers of the count a thread reserves at a time.
const BLOCK: u64 = 1 << 16;

/// The first number of the next block a thread reserves.
static NEXT_BLOCK: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The next number this thread gives and the end of its block (both 0
    /// before its first id, which reserves one).
    static COUNTER: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// A new id: process prefix, then the process's count.
pub(crate) fn generate() -> IdBuf {
    static PREFIX: OnceLock<u64> = OnceLock::new();
    let prefix = *PREFIX.get_or_init(prefix);
    // One count for the process, not one per thread or worker: two
    // runtimes in one process each have a worker 0, and per-thread counters
    // all start at the same number.
    let count = COUNTER.with(|c| {
        let (mut next, mut end) = c.get();
        if next == end {
            next = NEXT_BLOCK.fetch_add(BLOCK, Ordering::Relaxed);
            end = next + BLOCK;
        }
        c.set((next + 1, end));
        next
    });
    let mut digits = [0u8; 32];
    hex(&mut digits[..16], prefix);
    hex(&mut digits[16..], count);
    // Hexadecimal digits are always a valid id.
    IdBuf::new(std::str::from_utf8(&digits).unwrap_or("0")).expect("hex digits")
}

/// 64 random bits: the process id and the time, hashed with SipHash under
/// the random key the standard library draws from the operating system.
/// Two processes share a prefix with even odds only after some five
/// billion starts.
fn prefix() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.finish()
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
        assert_eq!(a.as_str().len(), 32);
        assert!(a
            .as_str()
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(a.as_str(), b.as_str());
        // The same process: the same 64 random bits; the same thread: the
        // next number.
        assert_eq!(a.as_str()[..16], b.as_str()[..16]);
        let count = |id: &IdBuf| u64::from_str_radix(&id.as_str()[16..], 16).unwrap();
        assert_eq!(count(&b), count(&a) + 1);
    }

    /// Threads that are not giong workers, and the workers of two runtimes
    /// (each has a worker 0), make ids of their own.
    #[test]
    fn ids_made_on_different_threads_differ() {
        let ids: Vec<IdBuf> = (0..8)
            .map(|_| std::thread::spawn(generate))
            .chain((0..4).map(|_| {
                std::thread::spawn(|| {
                    giong::Builder::new()
                        .workers(1)
                        .block_on(async { generate() })
                        .unwrap()
                })
            }))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let mut seen: Vec<&str> = ids.iter().map(IdBuf::as_str).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ids.len(), "{seen:?}");
    }

    #[test]
    fn the_prefix_has_64_bits_that_differ_from_process_to_process() {
        // Two processes are two seeds: what `prefix` makes of each.
        let (a, b) = (prefix(), prefix());
        assert_ne!(a, b);
        assert!(a >> 32 != 0 || b >> 32 != 0, "{a:x} {b:x}: 64 bits");
    }
}
