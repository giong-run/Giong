//! Password authentication: SCRAM-SHA-256 (RFC 5802, RFC 7677) and the older
//! MD5 scheme.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};

use giong::sync::oneshot;
use ring::{digest, hmac, pbkdf2, rand::SecureRandom};

use crate::error::{Error, ErrorKind};

/// More iterations than this would take seconds of CPU for each new key: a
/// server asking for them is misconfigured or hostile. (PostgreSQL's default
/// is 4096; guides for PBKDF2-SHA-256 ask for 600,000.)
const MAX_ITERATIONS: u32 = 1_000_000;

fn auth_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Auth, message)
}

/// The client side of a SCRAM-SHA-256 exchange.
pub(crate) struct Scram {
    password: Vec<u8>,
    client_first_bare: String,
    nonce: String,
    state: State,
}

enum State {
    Started,
    /// After the server's first message, which the client's final one
    /// answers once the key is derived.
    Received {
        server_first: String,
        nonce: String,
    },
    /// After the client's final message: the signature the server must send.
    Sent {
        server_signature: [u8; 32],
    },
    Done,
}

impl Scram {
    /// Starts an exchange with a fresh random nonce.
    pub(crate) fn new(password: &str) -> Result<Self, Error> {
        let mut raw = [0u8; 18];
        ring::rand::SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| auth_error("the system's random number generator failed"))?;
        Ok(Self::with_nonce(password, "", &base64_encode(&raw)))
    }

    /// PostgreSQL ignores the SCRAM user name (it uses the one from the
    /// startup message), so `user` is empty except in the RFC's test vector.
    pub(crate) fn with_nonce(password: &str, user: &str, nonce: &str) -> Self {
        Self {
            // SASLprep would normalize non-ASCII passwords first; PostgreSQL
            // uses the raw bytes when a password is not valid SASLprep input,
            // and for passwords already in NFKC form the two agree.
            password: password.as_bytes().to_vec(),
            client_first_bare: format!("n={user},r={nonce}"),
            nonce: nonce.to_owned(),
            state: State::Started,
        }
    }

    /// Whether the server proved that it knows the password.
    pub(crate) fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// The client's first message, with the GS2 header (no channel binding).
    pub(crate) fn client_first(&self) -> Vec<u8> {
        format!("n,,{}", self.client_first_bare).into_bytes()
    }

    /// Answers the server's first message with the client's final one,
    /// deriving the key on this thread.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, Error> {
        let (salt, iterations) = self.server_first(server_first)?;
        let salted = salted_password(&self.password, &salt, iterations);
        self.client_final_salted(&salted)
    }

    /// The password, for deriving the key.
    pub(crate) fn password(&self) -> &[u8] {
        &self.password
    }

    /// Reads the server's first message: the salt and the iteration count to
    /// derive the key with ([`salted_password`]).
    pub(crate) fn server_first(
        &mut self,
        server_first: &[u8],
    ) -> Result<(Vec<u8>, NonZeroU32), Error> {
        if !matches!(self.state, State::Started) {
            return Err(Error::protocol(
                "the server sent SCRAM messages out of order",
            ));
        }
        let server_first = std::str::from_utf8(server_first)
            .map_err(|_| Error::protocol("the server's SCRAM message is not UTF-8"))?;
        let bad = || {
            Error::protocol(format!(
                "the server's SCRAM message {server_first:?} is malformed"
            ))
        };
        let mut parts = server_first.split(',');
        let nonce = parts
            .next()
            .and_then(|p| p.strip_prefix("r="))
            .ok_or_else(bad)?;
        let salt = parts
            .next()
            .and_then(|p| p.strip_prefix("s="))
            .ok_or_else(bad)?;
        let iterations = parts
            .next()
            .and_then(|p| p.strip_prefix("i="))
            .ok_or_else(bad)?;
        if !nonce.starts_with(&self.nonce) || nonce.len() == self.nonce.len() {
            return Err(auth_error(
                "the server's SCRAM nonce does not extend the client's; refusing to continue",
            ));
        }
        let salt = base64_decode(salt).ok_or_else(bad)?;
        let iterations: u32 = iterations.parse().map_err(|_| bad())?;
        let iterations = NonZeroU32::new(iterations).ok_or_else(bad)?;
        if iterations.get() > MAX_ITERATIONS {
            return Err(auth_error(format!(
                "the server asks for {iterations} SCRAM iterations, more than the {MAX_ITERATIONS} \
                 this client accepts; lower scram_iterations on the server and set the password again"
            )));
        }

        self.state = State::Received {
            server_first: server_first.to_owned(),
            nonce: nonce.to_owned(),
        };
        Ok((salt, iterations))
    }

    /// The client's final message, from the key [`salted_password`] derived.
    pub(crate) fn client_final_salted(&mut self, salted: &[u8; 32]) -> Result<Vec<u8>, Error> {
        let State::Received {
            server_first,
            nonce,
        } = std::mem::replace(&mut self.state, State::Started)
        else {
            return Err(Error::protocol(
                "the server sent SCRAM messages out of order",
            ));
        };
        let salted_key = hmac::Key::new(hmac::HMAC_SHA256, salted);
        let client_key = hmac::sign(&salted_key, b"Client Key");
        let stored_key = digest::digest(&digest::SHA256, client_key.as_ref());

        let without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);

        let signature = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, stored_key.as_ref()),
            auth_message.as_bytes(),
        );
        let mut proof = [0u8; 32];
        for (p, (k, s)) in proof
            .iter_mut()
            .zip(client_key.as_ref().iter().zip(signature.as_ref()))
        {
            *p = k ^ s;
        }

        let server_key = hmac::sign(&salted_key, b"Server Key");
        let server_signature = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, server_key.as_ref()),
            auth_message.as_bytes(),
        );
        let mut expected = [0u8; 32];
        expected.copy_from_slice(server_signature.as_ref());
        self.state = State::Sent {
            server_signature: expected,
        };
        Ok(format!("{without_proof},p={}", base64_encode(&proof)).into_bytes())
    }

    /// Checks the server's final message, which proves that it knows the
    /// password too.
    pub(crate) fn finish(&mut self, server_final: &[u8]) -> Result<(), Error> {
        let State::Sent { server_signature } = self.state else {
            return Err(Error::protocol(
                "the server sent SCRAM messages out of order",
            ));
        };
        let server_final = std::str::from_utf8(server_final)
            .map_err(|_| Error::protocol("the server's SCRAM message is not UTF-8"))?;
        if let Some(error) = server_final.strip_prefix("e=") {
            return Err(auth_error(format!("SCRAM authentication failed: {error}")));
        }
        let verifier = server_final
            .split(',')
            .next()
            .and_then(|p| p.strip_prefix("v="))
            .and_then(base64_decode)
            .ok_or_else(|| Error::protocol("the server's final SCRAM message is malformed"))?;
        // Compared without stopping at the first difference.
        let differs = verifier.len() != server_signature.len()
            || verifier
                .iter()
                .zip(&server_signature)
                .fold(0, |acc, (a, b)| acc | (a ^ b))
                != 0;
        if differs {
            return Err(auth_error(
                "the server's SCRAM signature is wrong: it does not know the password, so it is \
                 not the server it claims to be; refusing to continue",
            ));
        }
        self.state = State::Done;
        Ok(())
    }
}

/// SCRAM's SaltedPassword: PBKDF2-HMAC-SHA-256 of the password. This is the
/// expensive step (about 0.5 µs per iteration), which a [`Config`] caches.
///
/// [`Config`]: crate::Config
pub(crate) fn salted_password(password: &[u8], salt: &[u8], iterations: NonZeroU32) -> [u8; 32] {
    let mut salted = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password,
        &mut salted,
    );
    salted
}

/// How many derived SCRAM keys a [`KeyCache`] keeps.
const CACHED_KEYS: usize = 8;

/// The SCRAM keys derived for one [`Config`] and its clones, by password,
/// salt and iteration count. A role's salt and iteration count stay the same
/// until its password is set again, so only the first connection of a
/// process pays for the derivation, and it pays on a thread of its own:
/// every connection after a failover would otherwise stall its worker's
/// tasks for 2 ms at PostgreSQL's default of 4096 iterations, and for 175 ms
/// at a million.
///
/// [`Config`]: crate::Config
#[derive(Default)]
pub(crate) struct KeyCache {
    entries: Mutex<Vec<(KeyId, KeyState)>>,
}

#[derive(Clone, PartialEq, Eq)]
struct KeyId {
    /// SHA-256 of the password.
    password: [u8; 32],
    salt: Vec<u8>,
    iterations: u32,
}

enum KeyState {
    Ready([u8; 32]),
    /// Being derived; these connections wait for it.
    Deriving(Vec<oneshot::Sender<[u8; 32]>>),
}

impl KeyCache {
    /// [`salted_password`], derived once on another thread and kept.
    pub(crate) async fn salted_password(
        self: &Arc<Self>,
        password: &[u8],
        salt: &[u8],
        iterations: NonZeroU32,
    ) -> [u8; 32] {
        let mut digest_of_password = [0u8; 32];
        digest_of_password.copy_from_slice(digest::digest(&digest::SHA256, password).as_ref());
        let id = KeyId {
            password: digest_of_password,
            salt: salt.to_vec(),
            iterations: iterations.get(),
        };
        let (tx, rx) = oneshot::channel();
        let start = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            match entries.iter_mut().find(|(k, _)| *k == id) {
                Some((_, KeyState::Ready(key))) => return *key,
                Some((_, KeyState::Deriving(waiting))) => {
                    waiting.push(tx);
                    false
                }
                None => {
                    if entries.len() >= CACHED_KEYS {
                        let ready = entries
                            .iter()
                            .position(|(_, s)| matches!(s, KeyState::Ready(_)));
                        if let Some(i) = ready {
                            entries.remove(i);
                        }
                    }
                    entries.push((id.clone(), KeyState::Deriving(vec![tx])));
                    true
                }
            }
        };
        if start {
            let job = Derive {
                cache: Arc::clone(self),
                id,
                password: password.to_vec(),
                salt: salt.to_vec(),
                iterations,
            };
            let spawned = std::thread::Builder::new()
                .name("giong-postgres-scram".to_owned())
                .spawn(move || job.run());
            if spawned.is_err() {
                // No thread to be had: derive here (the job was dropped,
                // which took its entry out).
                return salted_password(password, salt, iterations);
            }
        }
        match rx.await {
            Ok(key) => key,
            Err(_) => salted_password(password, salt, iterations),
        }
    }
}

/// A key derivation on its thread. If it ends without a key (it cannot,
/// short of a panic), its entry goes, so that nobody waits for it.
struct Derive {
    cache: Arc<KeyCache>,
    id: KeyId,
    password: Vec<u8>,
    salt: Vec<u8>,
    iterations: NonZeroU32,
}

impl Derive {
    fn run(self) {
        let key = salted_password(&self.password, &self.salt, self.iterations);
        let waiting = {
            let mut entries = self
                .cache
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match entries.iter_mut().find(|(k, _)| *k == self.id) {
                Some((_, state)) => match std::mem::replace(state, KeyState::Ready(key)) {
                    KeyState::Deriving(waiting) => waiting,
                    KeyState::Ready(_) => Vec::new(),
                },
                None => Vec::new(),
            }
        };
        for tx in waiting {
            let _ = tx.send(key);
        }
    }
}

impl Drop for Derive {
    fn drop(&mut self) {
        let mut entries = self
            .cache
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(k, s)| !(*k == self.id && matches!(s, KeyState::Deriving(_))));
    }
}

/// The answer to an MD5 password request: `"md5" + md5(md5(password + user)
/// + salt)` in hex.
pub(crate) fn md5_password(user: &str, password: &str, salt: [u8; 4]) -> String {
    let mut inner = Vec::with_capacity(password.len() + user.len());
    inner.extend_from_slice(password.as_bytes());
    inner.extend_from_slice(user.as_bytes());
    let mut outer = hex(&md5(&inner)).into_bytes();
    outer.extend_from_slice(&salt);
    format!("md5{}", hex(&md5(&outer)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// MD5 (RFC 1321), for the MD5 password scheme only: `ring` leaves it out
/// because it is broken for anything that needs collision resistance.
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] =
        std::array::from_fn(|i| (((i + 1) as f64).sin().abs() * 4_294_967_296.0) as u32);
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());

    for block in data.chunks_exact(64) {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ])
        });
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(m[g])
                .rotate_left(S[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (chunk, word) in out.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    out
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.as_bytes();
    if input.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for (i, chunk) in input.chunks(4).enumerate() {
        let last = i == input.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            let v = BASE64.iter().position(|&b| b == c)? as u32;
            n = n << 6 | v;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scram_matches_the_rfc_7677_example() {
        let mut scram = Scram::with_nonce("pencil", "user", "rOprNGfwEbeRWgbNEkqO");
        assert_eq!(scram.client_first(), b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let client_final = scram
            .client_final(
                b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                  s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            )
            .unwrap();
        assert_eq!(
            String::from_utf8(client_final).unwrap(),
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        scram
            .finish(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .unwrap();
    }

    #[test]
    fn scram_refuses_a_server_that_does_not_know_the_password() {
        let mut scram = Scram::with_nonce("pencil", "user", "rOprNGfwEbeRWgbNEkqO");
        scram
            .client_final(
                b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                  s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            )
            .unwrap();
        let e = scram
            .finish(b"v=AAAATRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Auth);
        assert!(e.to_string().contains("does not know the password"), "{e}");
    }

    #[test]
    fn scram_refuses_bad_server_messages() {
        let first = |msg: &[u8]| {
            Scram::with_nonce("p", "", "abc")
                .client_final(msg)
                .unwrap_err()
        };
        // The nonce must extend ours.
        assert_eq!(first(b"r=xyz123,s=QUJD,i=4096").kind(), ErrorKind::Auth);
        assert_eq!(first(b"r=abc,s=QUJD,i=4096").kind(), ErrorKind::Auth);
        assert_eq!(first(b"r=abcd,s=QUJD,i=0").kind(), ErrorKind::Protocol);
        assert_eq!(first(b"r=abcd,s=Q,i=4096").kind(), ErrorKind::Protocol);
        assert_eq!(first(b"r=abcd,i=4096").kind(), ErrorKind::Protocol);
        let e = first(b"r=abcd,s=QUJD,i=100000000");
        assert!(e.to_string().contains("scram_iterations"), "{e}");
        let mut scram = Scram::with_nonce("p", "", "abc");
        assert!(scram.finish(b"v=AAAA").is_err());
    }

    #[test]
    fn md5_matches_rfc_1321() {
        let cases: [(&[u8], &str); 4] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, digest) in cases {
            assert_eq!(hex(&md5(input)), digest);
        }
    }

    #[test]
    fn md5_password_has_the_server_format() {
        // md5("secret" + "app") = 7a3a5e8e..., checked against PostgreSQL in
        // the integration tests; here the shape and determinism.
        let a = md5_password("app", "secret", [1, 2, 3, 4]);
        assert!(a.starts_with("md5") && a.len() == 35);
        assert_eq!(a, md5_password("app", "secret", [1, 2, 3, 4]));
        assert_ne!(a, md5_password("app", "secret", [1, 2, 3, 5]));
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..40u8 {
            let data: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37)).collect();
            let text = base64_encode(&data);
            assert_eq!(base64_decode(&text).unwrap(), data, "{text}");
        }
        assert_eq!(base64_encode(b"n,,"), "biws");
        assert!(base64_decode("abc").is_none());
        assert!(base64_decode("a===").is_none());
        assert!(base64_decode("ab=c").is_none());
        assert!(base64_decode("ab==abcd").is_none());
        assert!(base64_decode("ab!c").is_none());
    }
}
