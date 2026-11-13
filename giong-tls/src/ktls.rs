//! Kernel TLS (kTLS): rustls does the handshake, then the kernel encrypts and
//! decrypts the records of the connection.
//!
//! The parts, in the order a connection goes through them:
//!
//! 1. [`support`] probes once per process what the kernel can do: whether a
//!    socket takes the `tls` upper-layer protocol, which cipher suites it
//!    encrypts and decrypts, and whether it accepts new TLS 1.3 keys on a live
//!    connection (Linux 6.14). [`Plan`] decides from that, once per acceptor or
//!    connector, whether its connections may use kTLS.
//! 2. [`handshake`] attaches the `tls` protocol to the socket, runs the
//!    handshake with rustls' unbuffered API (which leaves every byte it has not
//!    used in our buffer), reads exactly up to the end of the record the
//!    handshake ended in, then hands the keys to the kernel.
//! 3. [`Kernel`] is what the stream keeps afterwards. Application data goes
//!    through the ordinary socket calls; the kernel returns other records
//!    (alerts, `NewSessionTicket`, `KeyUpdate`) one at a time with their type,
//!    and rustls' `KernelConnection` derives new keys and stores tickets.
//!
//! Every unsafe block here is a system call on the stream's descriptor with
//! buffers that live on the stack or the heap for the duration of the call.

use std::cell::RefMut;
use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use giong::error::Code;
use giong::net::TcpStream;
use giong::time::timeout;
use rustls::client::{ClientConnectionData, UnbufferedClientConnection};
use rustls::crypto::CryptoProvider;
use rustls::kernel::KernelConnection;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::server::{ServerConnectionData, UnbufferedServerConnection};
use rustls::unbuffered::{
    ConnectionState, EncodeError, UnbufferedConnectionCommon, UnbufferedStatus,
};
use rustls::{
    CipherSuite, ClientConfig, ConnectionTrafficSecrets, ExtractedSecrets, HandshakeKind,
    ServerConfig, SupportedCipherSuite,
};

use crate::{coded, tls_error, Engine, TlsStream};

/// Whether TLS connections hand their records to the kernel after the
/// handshake: see [`TlsAcceptor::ktls`](crate::TlsAcceptor::ktls).
///
/// With kernel TLS (kTLS), rustls still does the handshake and checks the
/// certificates; afterwards the kernel encrypts what the program sends and
/// decrypts what it receives, and the program uses plain socket calls. That
/// saves a copy of every byte and lets the kernel send straight from its own
/// buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Ktls {
    /// rustls encrypts and decrypts in the process. The default.
    #[default]
    Off,
    /// The kernel takes a connection when it can do everything rustls would
    /// have done for it; otherwise the connection stays in the process.
    /// Never fails because of kTLS.
    ///
    /// "Everything" means: the kernel has the `tls` module, encrypts and
    /// decrypts every cipher suite the configuration allows, and, when TLS
    /// 1.3 is allowed, takes new keys on a live connection (Linux 6.14 or
    /// newer). Without that last part a TLS 1.3 connection could not follow
    /// a peer's `KeyUpdate`, so on older kernels connections stay in the
    /// process. A stream on which `TcpStream::recv` was already used (a
    /// database client's plaintext greeting) also stays in the process.
    Auto,
    /// The kernel takes every connection, or the handshake fails with
    /// `GIONG-E2009`. Also allows TLS 1.3 on kernels older than 6.14, where
    /// a peer's `KeyUpdate` ends the connection (`GIONG-E2011`).
    Required,
}

// ------------------------------------------------------------ kernel ABI

// From <linux/tcp.h> and <linux/tls.h>; stable kernel ABI.
const TCP_ULP: libc::c_int = 31;
const SOL_TLS: libc::c_int = 282;
const TLS_TX: libc::c_int = 1;
const TLS_RX: libc::c_int = 2;
const TLS_SET_RECORD_TYPE: libc::c_int = 1;
const TLS_GET_RECORD_TYPE: libc::c_int = 2;
/// Linux 6.0+: the peer does not pad TLS 1.3 records, so the kernel may
/// decrypt straight into the receiver's buffer (`TLS_RX_EXPECT_NO_PAD`).
const TLS_RX_EXPECT_NO_PAD: libc::c_int = 4;

const TLS_1_2_VERSION: u16 = 0x0303;
const TLS_1_3_VERSION: u16 = 0x0304;
const TLS_CIPHER_AES_GCM_128: u16 = 51;
const TLS_CIPHER_AES_GCM_256: u16 = 52;
const TLS_CIPHER_CHACHA20_POLY1305: u16 = 54;

// TLS record content types (RFC 8446, section 5.1).
const CHANGE_CIPHER_SPEC: u8 = 20;
const ALERT: u8 = 21;
const HANDSHAKE: u8 = 22;
const APPLICATION_DATA: u8 = 23;

// Handshake message types used after the handshake.
const HELLO_REQUEST: u8 = 0;
const CLIENT_HELLO: u8 = 1;
const NEW_SESSION_TICKET: u8 = 4;
const KEY_UPDATE: u8 = 24;

// Alert descriptions giong-tls sends or recognises.
const ALERT_WARNING: u8 = 1;
const ALERT_FATAL: u8 = 2;
const CLOSE_NOTIFY: u8 = 0;
const UNEXPECTED_MESSAGE: u8 = 10;
const ILLEGAL_PARAMETER: u8 = 47;
const DECODE_ERROR: u8 = 50;
const USER_CANCELED: u8 = 90;
const NO_RENEGOTIATION: u8 = 100;

/// The largest plaintext a record carries (2^14 bytes).
const MAX_PLAINTEXT: usize = 1 << 14;
/// The largest record on the wire: header, TLS 1.2's allowance for
/// expansion (RFC 5246, section 6.2.3), plaintext.
const MAX_RECORD: usize = 5 + MAX_PLAINTEXT + 2048;
/// The most a handshake may leave in the incoming buffer: a whole handshake
/// message (at most 64 KiB in rustls) and the records around it.
const MAX_INCOMING: usize = (64 << 10) + 2 * MAX_RECORD;
/// The largest post-handshake handshake message accepted, as in rustls.
const MAX_HANDSHAKE_MESSAGE: usize = 0xffff;

/// The protocol version, as the kernel names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Version {
    Tls12,
    Tls13,
}

impl Version {
    fn wire(self) -> u16 {
        match self {
            Version::Tls12 => TLS_1_2_VERSION,
            Version::Tls13 => TLS_1_3_VERSION,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Version::Tls12 => "TLS 1.2",
            Version::Tls13 => "TLS 1.3",
        }
    }
}

/// The record ciphers the kernel and rustls' `ring` provider have in common.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cipher {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
}

const CIPHERS: [Cipher; 3] = [
    Cipher::Aes128Gcm,
    Cipher::Aes256Gcm,
    Cipher::Chacha20Poly1305,
];

impl Cipher {
    fn of_suite(suite: CipherSuite) -> Option<(Version, Cipher)> {
        use CipherSuite::*;
        Some(match suite {
            TLS13_AES_128_GCM_SHA256 => (Version::Tls13, Cipher::Aes128Gcm),
            TLS13_AES_256_GCM_SHA384 => (Version::Tls13, Cipher::Aes256Gcm),
            TLS13_CHACHA20_POLY1305_SHA256 => (Version::Tls13, Cipher::Chacha20Poly1305),
            TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 | TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => {
                (Version::Tls12, Cipher::Aes128Gcm)
            }
            TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 | TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => {
                (Version::Tls12, Cipher::Aes256Gcm)
            }
            TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
            | TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
                (Version::Tls12, Cipher::Chacha20Poly1305)
            }
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Cipher::Aes128Gcm => "AES-128-GCM",
            Cipher::Aes256Gcm => "AES-256-GCM",
            Cipher::Chacha20Poly1305 => "ChaCha20-Poly1305",
        }
    }
}

/// `struct tls_crypto_info`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InfoHeader {
    version: u16,
    cipher_type: u16,
}

/// `struct tls12_crypto_info_aes_gcm_128`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AesGcm128Info {
    header: InfoHeader,
    iv: [u8; 8],
    key: [u8; 16],
    salt: [u8; 4],
    rec_seq: [u8; 8],
}

/// `struct tls12_crypto_info_aes_gcm_256`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AesGcm256Info {
    header: InfoHeader,
    iv: [u8; 8],
    key: [u8; 32],
    salt: [u8; 4],
    rec_seq: [u8; 8],
}

/// `struct tls12_crypto_info_chacha20_poly1305` (its salt is empty).
#[repr(C)]
#[derive(Clone, Copy)]
struct Chacha20Poly1305Info {
    header: InfoHeader,
    iv: [u8; 12],
    key: [u8; 32],
    rec_seq: [u8; 8],
}

// The kernel checks `optlen` against these exact sizes.
const _: () = assert!(mem::size_of::<AesGcm128Info>() == 40);
const _: () = assert!(mem::size_of::<AesGcm256Info>() == 56);
const _: () = assert!(mem::size_of::<Chacha20Poly1305Info>() == 56);

/// Key material for one direction, as `setsockopt(SOL_TLS, TLS_TX/TLS_RX)`
/// takes it. Wiped when dropped.
enum CryptoInfo {
    Aes128(AesGcm128Info),
    Aes256(AesGcm256Info),
    Chacha(Chacha20Poly1305Info),
}

impl CryptoInfo {
    /// The kernel's form of rustls' secrets for one direction.
    ///
    /// `seq` is the sequence number of the next record. For TLS 1.2 with
    /// AES-GCM in the sending direction the 8-byte explicit nonce matters
    /// too: see [`tls12_gcm_explicit_iv`]. `sending` says which direction
    /// this is.
    fn new(
        version: Version,
        secrets: &ConnectionTrafficSecrets,
        seq: u64,
        sending: bool,
    ) -> io::Result<Self> {
        let rec_seq = seq.to_be_bytes();
        let gcm = |iv: &[u8]| -> ([u8; 4], [u8; 8]) {
            let mut salt = [0; 4];
            salt.copy_from_slice(&iv[..4]);
            let mut explicit = [0; 8];
            explicit.copy_from_slice(&iv[4..12]);
            if sending && version == Version::Tls12 {
                explicit = tls12_gcm_explicit_iv(explicit, seq);
            }
            (salt, explicit)
        };
        Ok(match secrets {
            ConnectionTrafficSecrets::Aes128Gcm { key, iv } => {
                let (salt, explicit) = gcm(iv.as_ref());
                let mut info = AesGcm128Info {
                    header: InfoHeader {
                        version: version.wire(),
                        cipher_type: TLS_CIPHER_AES_GCM_128,
                    },
                    iv: explicit,
                    key: [0; 16],
                    salt,
                    rec_seq,
                };
                info.key.copy_from_slice(key.as_ref());
                CryptoInfo::Aes128(info)
            }
            ConnectionTrafficSecrets::Aes256Gcm { key, iv } => {
                let (salt, explicit) = gcm(iv.as_ref());
                let mut info = AesGcm256Info {
                    header: InfoHeader {
                        version: version.wire(),
                        cipher_type: TLS_CIPHER_AES_GCM_256,
                    },
                    iv: explicit,
                    key: [0; 32],
                    salt,
                    rec_seq,
                };
                info.key.copy_from_slice(key.as_ref());
                CryptoInfo::Aes256(info)
            }
            ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => {
                let mut info = Chacha20Poly1305Info {
                    header: InfoHeader {
                        version: version.wire(),
                        cipher_type: TLS_CIPHER_CHACHA20_POLY1305,
                    },
                    iv: [0; 12],
                    key: [0; 32],
                    rec_seq,
                };
                info.iv.copy_from_slice(iv.as_ref());
                info.key.copy_from_slice(key.as_ref());
                CryptoInfo::Chacha(info)
            }
            _ => {
                return Err(ktls_setup(
                    "rustls produced keys for a cipher the kernel is not given".into(),
                ))
            }
        })
    }

    /// A zero key for `cipher`, for the probe.
    fn dummy(version: Version, cipher: Cipher, key_byte: u8) -> Self {
        let header = |cipher_type| InfoHeader {
            version: version.wire(),
            cipher_type,
        };
        match cipher {
            Cipher::Aes128Gcm => CryptoInfo::Aes128(AesGcm128Info {
                header: header(TLS_CIPHER_AES_GCM_128),
                iv: [0; 8],
                key: [key_byte; 16],
                salt: [0; 4],
                rec_seq: [0; 8],
            }),
            Cipher::Aes256Gcm => CryptoInfo::Aes256(AesGcm256Info {
                header: header(TLS_CIPHER_AES_GCM_256),
                iv: [0; 8],
                key: [key_byte; 32],
                salt: [0; 4],
                rec_seq: [0; 8],
            }),
            Cipher::Chacha20Poly1305 => CryptoInfo::Chacha(Chacha20Poly1305Info {
                header: header(TLS_CIPHER_CHACHA20_POLY1305),
                iv: [0; 12],
                key: [key_byte; 32],
                rec_seq: [0; 8],
            }),
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        let (ptr, len): (*mut u8, usize) = match self {
            CryptoInfo::Aes128(i) => ((i as *mut AesGcm128Info).cast(), mem::size_of_val(i)),
            CryptoInfo::Aes256(i) => ((i as *mut AesGcm256Info).cast(), mem::size_of_val(i)),
            CryptoInfo::Chacha(i) => ((i as *mut Chacha20Poly1305Info).cast(), mem::size_of_val(i)),
        };
        // SAFETY: the structs are `repr(C)`, made of integers and byte arrays
        // with no padding (the sizes are asserted above), so every byte is
        // initialised and any byte pattern is valid.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    /// The record sequence number in the struct.
    fn rec_seq(&mut self) -> u64 {
        let seq = match self {
            CryptoInfo::Aes128(i) => i.rec_seq,
            CryptoInfo::Aes256(i) => i.rec_seq,
            CryptoInfo::Chacha(i) => i.rec_seq,
        };
        u64::from_be_bytes(seq)
    }

    /// `setsockopt(fd, SOL_TLS, TLS_TX or TLS_RX, self)`.
    fn install(&mut self, fd: RawFd, sending: bool) -> io::Result<()> {
        let name = if sending { TLS_TX } else { TLS_RX };
        let bytes = self.bytes_mut();
        setsockopt(fd, SOL_TLS, name, bytes)
    }
}

impl Drop for CryptoInfo {
    fn drop(&mut self) {
        for b in self.bytes_mut() {
            // SAFETY: `b` is a valid, aligned `u8` in this struct. A volatile
            // write so that wiping the key is not optimised away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

/// The explicit nonce the kernel should start from when it takes over
/// sending on a TLS 1.2 AES-GCM connection on which rustls has already sent
/// `used` records with the same key.
///
/// The two count nonces differently. rustls builds the 8-byte explicit part
/// of each record's nonce as `iv XOR seq`; the kernel takes the value it is
/// given and adds one per record. Starting the kernel at rustls' `iv` would
/// therefore reuse the nonce of the first record rustls sent under this key
/// (the `Finished` message): with AES-GCM, two records under one key and one
/// nonce reveal the authentication key, after which anyone on the path can
/// forge records.
///
/// rustls' nonces `iv XOR 0 .. iv XOR (used - 1)` all differ from `iv` only
/// in the lowest `b` bits, where `2^b >= used`, so they lie in the aligned
/// block of `2^b` values that holds `iv`. The kernel starts at the first value
/// after that block and counts up, and would only come back to it after
/// `2^64 - 2^b` records; giong-tls stops at the cipher's limit of 2^24 long
/// before that. The explicit nonce is sent in every record, so the peer needs
/// nothing else.
fn tls12_gcm_explicit_iv(iv: [u8; 8], used: u64) -> [u8; 8] {
    if used == 0 {
        return iv;
    }
    // The smallest b with 2^b >= used.
    let bits = 64 - (used - 1).leading_zeros();
    let block = 1u64.checked_shl(bits).unwrap_or(0);
    let start = u64::from_be_bytes(iv) & !block.wrapping_sub(1);
    start.wrapping_add(block).to_be_bytes()
}

fn setsockopt(fd: RawFd, level: libc::c_int, name: libc::c_int, value: &[u8]) -> io::Result<()> {
    // SAFETY: `value` is valid for reads of its length for the whole call.
    let res = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            value.as_ptr().cast(),
            value.len() as libc::socklen_t,
        )
    };
    if res == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Attaches the kernel's `tls` upper-layer protocol to a connected TCP
/// socket. Until keys are installed it behaves like plain TCP.
fn attach_ulp(fd: RawFd) -> io::Result<()> {
    setsockopt(fd, libc::IPPROTO_TCP, TCP_ULP, b"tls")
}

/// The sequence number of the next record the kernel will send.
fn sent_records(fd: RawFd, cipher: Cipher, version: Version) -> io::Result<u64> {
    let mut info = CryptoInfo::dummy(version, cipher, 0);
    let bytes = info.bytes_mut();
    let mut len = bytes.len() as libc::socklen_t;
    // SAFETY: `bytes` is writable for `len` bytes for the whole call, and
    // `len` is a valid `socklen_t`. The kernel copies the key back too; the
    // struct is wiped when `info` is dropped.
    let res = unsafe { libc::getsockopt(fd, SOL_TLS, TLS_TX, bytes.as_mut_ptr().cast(), &mut len) };
    if res != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.rec_seq())
}

/// A control message that sets the type of the record `sendmsg` sends.
fn record_type_cmsg(record_type: u8) -> Vec<u8> {
    // SAFETY: CMSG_SPACE and CMSG_LEN only compute sizes.
    let (space, len) = unsafe { (libc::CMSG_SPACE(1) as usize, libc::CMSG_LEN(1) as usize) };
    // Aligned for `cmsghdr`, then copied out as bytes.
    let mut buf = vec![0u64; space.div_ceil(8)];
    // SAFETY: all-zero is a valid `msghdr`.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_control = buf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `buf` is aligned and holds `space` bytes, enough for one header
    // and one byte of data, so CMSG_FIRSTHDR is not null and both writes
    // stay inside it.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = SOL_TLS;
        (*cmsg).cmsg_type = TLS_SET_RECORD_TYPE;
        (*cmsg).cmsg_len = len as _;
        *libc::CMSG_DATA(cmsg) = record_type;
    }
    // SAFETY: `buf` holds at least `space` initialised bytes.
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), space) }.to_vec()
}

/// Reads the record the kernel holds back after a receive failed with `EIO`
/// (a record that is not application data, which the kernel only returns to
/// a `recvmsg` with room for its type).
///
/// It does not wait: the kernel has already decrypted the record and keeps
/// it at the head of the stream. No other receive can run at the same time:
/// a multishot receive ends with the error that brought us here, and only
/// one task receives on a `TlsStream`. Returns `WouldBlock` if there is no
/// such record after all, in which case the `EIO` was something else.
fn recv_record(fd: RawFd) -> io::Result<(u8, Vec<u8>)> {
    let mut data = vec![0u8; MAX_PLAINTEXT + 256];
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // SAFETY: all-zero is a valid `msghdr`.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(&control) as _;
    let n = loop {
        // SAFETY: `msg` points at `iov`, `data` and `control`, which live
        // until the call returns, with their true lengths.
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(unexpected("the kernel cut the TLS record type short"));
    }
    let mut record_type = APPLICATION_DATA;
    // SAFETY: the kernel filled `msg_controllen` bytes of `control` with
    // complete control messages; the CMSG_* macros walk them within it.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == SOL_TLS && (*cmsg).cmsg_type == TLS_GET_RECORD_TYPE {
                record_type = *libc::CMSG_DATA(cmsg);
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    data.truncate(n);
    Ok((record_type, data))
}

fn unexpected(what: &str) -> io::Error {
    coded(
        Code::Unexpected,
        what.to_string(),
        "this is a bug in giong-tls or the kernel; please report it at \
         https://github.com/giong-run/Giong/issues with giong_tls::ktls_support().",
    )
}

fn ktls_unavailable(why: String) -> io::Error {
    coded(
        Code::KtlsUnavailable,
        format!("kernel TLS cannot be used: {why}"),
        "load the kernel's tls module (`modprobe tls` on the host), or use Ktls::Auto, \
         which keeps such connections in the process.",
    )
}

fn ktls_setup(why: String) -> io::Error {
    coded(
        Code::KtlsSetup,
        format!("the kernel refused the TLS keys after the handshake: {why}"),
        "the connection is closed; open a new one. If it keeps happening, use Ktls::Off and \
         report it with giong_tls::ktls_support().",
    )
}

// ----------------------------------------------------------------- probe

/// What the running kernel can do for kernel TLS, probed once per process:
/// see [`ktls_support`].
///
/// Its [`Display`](fmt::Display) form is one line for a log:
///
/// ```text
/// kTLS available: TLS 1.2 AES-128-GCM AES-256-GCM ChaCha20-Poly1305, TLS 1.3 AES-128-GCM AES-256-GCM ChaCha20-Poly1305, key updates yes
/// kTLS unavailable: the kernel has no tls module and cannot load one here (ENOENT); load it with `modprobe tls`
/// ```
#[derive(Clone, Debug)]
pub struct KtlsSupport {
    unavailable: Option<String>,
    /// `[version][cipher]`: the kernel takes keys in both directions.
    ciphers: [[bool; 3]; 2],
    key_update: bool,
}

impl KtlsSupport {
    /// `true` if sockets take the kernel's `tls` protocol.
    pub fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// Why kernel TLS is not available, if it is not.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// `true` if the kernel encrypts and decrypts `suite`.
    pub fn supports(&self, suite: CipherSuite) -> bool {
        match Cipher::of_suite(suite) {
            Some((version, cipher)) => self.has(version, cipher),
            None => false,
        }
    }

    /// `true` if the kernel takes new TLS 1.3 keys on a live connection, as
    /// a `KeyUpdate` requires (Linux 6.14 and newer).
    pub fn key_update(&self) -> bool {
        self.key_update
    }

    fn has(&self, version: Version, cipher: Cipher) -> bool {
        self.ciphers[(version == Version::Tls13) as usize][cipher.index()]
    }
}

impl fmt::Display for KtlsSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(why) = &self.unavailable {
            return write!(f, "kTLS unavailable: {why}");
        }
        f.write_str("kTLS available:")?;
        for (i, version) in [Version::Tls12, Version::Tls13].into_iter().enumerate() {
            f.write_str(if i == 0 { " " } else { ", " })?;
            f.write_str(version.name())?;
            let mut any = false;
            for cipher in CIPHERS {
                if self.has(version, cipher) {
                    write!(f, " {}", cipher.name())?;
                    any = true;
                }
            }
            if !any {
                f.write_str(" none")?;
            }
        }
        write!(
            f,
            ", key updates {}",
            if self.key_update {
                "yes"
            } else {
                "no (Linux 6.14+)"
            }
        )
    }
}

/// What the running kernel can do for kernel TLS. Probed the first time it
/// is needed, on throwaway loopback sockets, and remembered for the process.
pub fn ktls_support() -> &'static KtlsSupport {
    static SUPPORT: OnceLock<KtlsSupport> = OnceLock::new();
    SUPPORT.get_or_init(probe)
}

fn probe() -> KtlsSupport {
    let mut support = KtlsSupport {
        unavailable: None,
        ciphers: [[false; 3]; 2],
        key_update: false,
    };
    let listener = match std::net::TcpListener::bind("127.0.0.1:0")
        .or_else(|_| std::net::TcpListener::bind("[::1]:0"))
    {
        Ok(l) => l,
        Err(e) => {
            support.unavailable = Some(format!(
                "cannot open a loopback socket to probe the kernel ({e})"
            ));
            return support;
        }
    };
    let pair = || -> io::Result<(std::net::TcpStream, std::net::TcpStream)> {
        let a = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (b, _) = listener.accept()?;
        Ok((a, b))
    };
    // One fresh connection per attempt: keys can be set once per socket
    // (before Linux 6.14), and a failure may leave the socket unusable.
    let attempt = |version: Version, cipher: Cipher| -> io::Result<std::net::TcpStream> {
        let (a, _b) = pair()?;
        let fd = a.as_raw_fd();
        attach_ulp(fd)?;
        CryptoInfo::dummy(version, cipher, 1).install(fd, true)?;
        CryptoInfo::dummy(version, cipher, 1).install(fd, false)?;
        Ok(a)
    };
    match pair().and_then(|(a, _b)| attach_ulp(a.as_raw_fd())) {
        Ok(()) => {}
        Err(e) => {
            support.unavailable = Some(match e.raw_os_error() {
                Some(libc::ENOENT) => "the kernel has no tls module and cannot load one here \
                                       (ENOENT); load it on the host with `modprobe tls`"
                    .to_string(),
                Some(libc::ENOPROTOOPT) => {
                    "this kernel has no TCP upper-layer protocols (ENOPROTOOPT)".to_string()
                }
                _ => format!("the socket refused the tls protocol ({e})"),
            });
            return support;
        }
    }
    for (v, version) in [Version::Tls12, Version::Tls13].into_iter().enumerate() {
        for cipher in CIPHERS {
            support.ciphers[v][cipher.index()] = attempt(version, cipher).is_ok();
        }
    }
    // Linux 6.14 accepts a second TLS 1.3 key on a socket (a key update);
    // older kernels answer EBUSY.
    for cipher in CIPHERS {
        if support.has(Version::Tls13, cipher) {
            if let Ok(a) = attempt(Version::Tls13, cipher) {
                let fd = a.as_raw_fd();
                support.key_update = CryptoInfo::dummy(Version::Tls13, cipher, 2)
                    .install(fd, true)
                    .and_then(|()| CryptoInfo::dummy(Version::Tls13, cipher, 2).install(fd, false))
                    .is_ok();
            }
            break;
        }
    }
    support
}

/// Kernel-side counters for this process: see [`ktls_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KtlsStats {
    /// Connections whose keys went to the kernel after the handshake.
    pub offloaded: u64,
    /// Connections set up with `Ktls::Auto` that stayed in the process
    /// because the kernel or the stream could not take them.
    pub stayed_in_process: u64,
    /// Key updates on connections encrypted by the kernel, in either
    /// direction.
    pub key_updates: u64,
}

static OFFLOADED: AtomicU64 = AtomicU64::new(0);
static STAYED: AtomicU64 = AtomicU64::new(0);
static KEY_UPDATES: AtomicU64 = AtomicU64::new(0);

/// How many connections in this process went to kernel TLS, how many
/// stayed in the process under `Ktls::Auto`, and how many key updates the
/// kernel took.
pub fn ktls_stats() -> KtlsStats {
    KtlsStats {
        offloaded: OFFLOADED.load(Ordering::Relaxed),
        stayed_in_process: STAYED.load(Ordering::Relaxed),
        key_updates: KEY_UPDATES.load(Ordering::Relaxed),
    }
}

// ------------------------------------------------------------------ plan

/// What an acceptor or connector does about kTLS, decided once from its
/// configuration and [`ktls_support`].
#[derive(Clone, Debug)]
pub(crate) enum Plan {
    /// Every connection stays in the process.
    Off,
    /// Try kTLS on each connection; `required` turns a stream that cannot
    /// take it into an error instead of a connection in the process.
    Try { required: bool },
    /// `Ktls::Auto` on a machine or configuration kTLS cannot serve.
    Unavailable(Arc<str>),
}

impl Plan {
    /// `early_data`: the configuration allows 0-RTT data, which the
    /// handshake for kTLS does not handle.
    pub(crate) fn new(mode: Ktls, provider: &CryptoProvider, early_data: bool) -> io::Result<Self> {
        if mode == Ktls::Off {
            return Ok(Plan::Off);
        }
        let required = mode == Ktls::Required;
        let checked = if early_data {
            Err(
                "the configuration allows 0-RTT (early) data; set max_early_data_size to 0 \
                 (server) or enable_early_data to false (client)"
                    .to_string(),
            )
        } else {
            eligible(provider, ktls_support(), required)
        };
        match checked {
            Ok(()) => Ok(Plan::Try { required }),
            Err(why) if required => Err(ktls_unavailable(why)),
            Err(why) => Ok(Plan::Unavailable(why.into())),
        }
    }

    /// Why `Ktls::Auto` keeps every connection in the process, if it does.
    pub(crate) fn unavailable(&self) -> Option<&str> {
        match self {
            Plan::Unavailable(why) => Some(why),
            _ => None,
        }
    }

    /// Counts a connection that stays in the process although kTLS was
    /// asked for.
    pub(crate) fn stayed(&self) {
        if !matches!(self, Plan::Off) {
            STAYED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `Ok` if the kernel can take every connection made with a configuration
/// using `provider`, otherwise why not.
fn eligible(
    provider: &CryptoProvider,
    support: &KtlsSupport,
    required: bool,
) -> Result<(), String> {
    if let Some(why) = support.unavailable_reason() {
        return Err(why.to_string());
    }
    let mut tls13 = false;
    for suite in &provider.cipher_suites {
        let name = suite.suite();
        match Cipher::of_suite(name) {
            Some((version, cipher)) if support.has(version, cipher) => {
                tls13 |= version == Version::Tls13;
            }
            Some((version, cipher)) => {
                return Err(format!(
                    "the kernel cannot encrypt {} for {} ({name:?} is allowed by the \
                     configuration)",
                    cipher.name(),
                    version.name()
                ))
            }
            None => {
                return Err(format!(
                    "the kernel has no cipher for {name:?}, which the configuration allows"
                ))
            }
        }
    }
    if tls13 && !support.key_update && !required {
        return Err(
            "this kernel cannot take new TLS 1.3 keys on a live connection (that \
                    needs Linux 6.14), so a peer's KeyUpdate would end the connection; \
                    Ktls::Required accepts that"
                .to_string(),
        );
    }
    Ok(())
}

// ------------------------------------------------------------- handshake

/// The two sides of rustls' unbuffered API, driven by one handshake loop.
trait Unbuffered {
    type Data;
    fn common(&self) -> &UnbufferedConnectionCommon<Self::Data>;
    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, Self::Data>;
    fn into_kernel(self) -> Result<(ExtractedSecrets, Side), rustls::Error>;
}

impl Unbuffered for UnbufferedServerConnection {
    type Data = ServerConnectionData;
    fn common(&self) -> &UnbufferedConnectionCommon<ServerConnectionData> {
        self
    }
    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, ServerConnectionData> {
        self.process_tls_records(incoming)
    }
    fn into_kernel(self) -> Result<(ExtractedSecrets, Side), rustls::Error> {
        let (secrets, conn) = self.dangerous_into_kernel_connection()?;
        Ok((secrets, Side::Server(conn)))
    }
}

impl Unbuffered for UnbufferedClientConnection {
    type Data = ClientConnectionData;
    fn common(&self) -> &UnbufferedConnectionCommon<ClientConnectionData> {
        self
    }
    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, ClientConnectionData> {
        self.process_tls_records(incoming)
    }
    fn into_kernel(self) -> Result<(ExtractedSecrets, Side), rustls::Error> {
        let (secrets, conn) = self.dangerous_into_kernel_connection()?;
        Ok((secrets, Side::Client(conn)))
    }
}

/// rustls' half of a connection the kernel encrypts.
enum Side {
    Client(KernelConnection<ClientConnectionData>),
    Server(KernelConnection<ServerConnectionData>),
}

impl Side {
    fn update_secret(
        &mut self,
        sending: bool,
    ) -> Result<(u64, ConnectionTrafficSecrets), rustls::Error> {
        match (self, sending) {
            (Side::Client(c), true) => c.update_tx_secret(),
            (Side::Client(c), false) => c.update_rx_secret(),
            (Side::Server(c), true) => c.update_tx_secret(),
            (Side::Server(c), false) => c.update_rx_secret(),
        }
    }
}

/// What the handshake settled, kept for the accessors of `TlsStream`.
pub(crate) struct Negotiated {
    pub(crate) alpn: Option<Vec<u8>>,
    pub(crate) peer_certificates: Option<Vec<CertificateDer<'static>>>,
    pub(crate) resumed: bool,
}

/// What the stream owns once the kernel encrypts the connection.
pub(crate) struct Kernel {
    side: Side,
    pub(crate) negotiated: Negotiated,
    version: Version,
    cipher: Cipher,
    can_update_keys: bool,
    /// Rekey (TLS 1.3) or stop (TLS 1.2) once the kernel's sending sequence
    /// number reaches this.
    record_limit: u64,
    /// The kernel's sequence number when it was last read, and an upper
    /// bound on the records sent since.
    sent_base: u64,
    sent_since: u64,
    /// Part of a post-handshake handshake message spread over records.
    handshake: Vec<u8>,
    warnings_left: u8,
    renegotiations_left: u8,
    key_update_requests_left: u8,
    /// The peer asked for a `KeyUpdate` and has not had it yet.
    key_update_owed: bool,
    /// What ended receiving, for every later `recv`.
    recv_failed: Option<String>,
    /// Sending is over: `close_notify` went out, or the keys are used up.
    sending_closed: Option<Code>,
}

/// Records of slack before the confidentiality limit, so that one write
/// (at most this many records, see `records_for`) never crosses it.
const LIMIT_MARGIN: u64 = 1 << 16;
/// rustls' limit on consecutive key update requests (from BoringSSL).
const KEY_UPDATE_REQUESTS: u8 = 32;

/// An upper bound on the records the kernel makes of one `send` of `len`
/// bytes: full records, a short one, and one more if the send is split.
fn records_for(len: usize) -> u64 {
    (len / MAX_PLAINTEXT) as u64 + 2
}

/// A byte buffer `TcpStream::read` fills up to `limit` bytes, no more.
struct Window {
    buf: Vec<u8>,
    limit: usize,
}

// SAFETY: `buf` is a heap allocation of at least `limit` bytes that does not
// move with the `Window`; the kernel writes at most `limit` bytes, and
// `set_init(n)` only marks bytes it wrote as initialised.
unsafe impl giong::IoBuf for Window {
    fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }
    fn len(&self) -> usize {
        self.buf.len()
    }
}

// SAFETY: see above.
unsafe impl giong::IoBufMut for Window {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr()
    }
    fn capacity(&self) -> usize {
        self.limit
    }
    unsafe fn set_init(&mut self, n: usize) {
        debug_assert!(n <= self.limit);
        self.buf.set_len(n);
    }
}

impl Window {
    fn new(limit: usize) -> Self {
        Self {
            buf: Vec::with_capacity(limit),
            limit,
        }
    }
}

/// How the handshake ended.
pub(crate) enum Outcome {
    /// The kernel encrypts the connection.
    Kernel(Box<TlsStream>),
    /// The stream cannot take kTLS (`recv` was already used on it, or the
    /// socket refused the `tls` protocol); nothing was sent or received.
    /// `Auto` goes on with TLS in the process.
    Declined(TcpStream, io::Error),
}

/// The server or client connection, before the handshake.
pub(crate) enum Start {
    Server(Arc<ServerConfig>),
    Client(Arc<ClientConfig>, ServerName<'static>),
}

/// Runs the handshake for kTLS, then hands the keys to the kernel.
pub(crate) async fn handshake(
    tcp: TcpStream,
    start: Start,
    limit: Duration,
) -> io::Result<Outcome> {
    if recv_was_used(&tcp).await? {
        return Ok(Outcome::Declined(
            tcp,
            ktls_unavailable("TcpStream::recv was used on this stream before the handshake".into()),
        ));
    }
    if let Err(e) = attach_ulp(tcp.as_raw_fd()) {
        return Ok(Outcome::Declined(
            tcp,
            ktls_unavailable(format!("the socket refused the tls protocol ({e})")),
        ));
    }
    let run = async {
        match start {
            Start::Server(config) => {
                let conn = UnbufferedServerConnection::new(config).map_err(tls_error)?;
                Handshake::new(&tcp).run(conn).await
            }
            Start::Client(config, name) => {
                let conn = UnbufferedClientConnection::new(config, name).map_err(tls_error)?;
                Handshake::new(&tcp).run(conn).await
            }
        }
    };
    let done = match timeout(limit, run).await {
        Ok(done) => done?,
        Err(_) => return Err(crate::handshake_timed_out(limit)),
    };
    let Done {
        kernel,
        plain,
        peer_closed,
    } = done;
    OFFLOADED.fetch_add(1, Ordering::Relaxed);
    Ok(Outcome::Kernel(Box::new(TlsStream::kernel(
        tcp,
        kernel,
        plain,
        peer_closed,
    ))))
}

/// `true` if `TcpStream::recv` was used on `tcp`: the runtime may then hold
/// bytes the kernel would never see, so the kernel cannot take the stream.
///
/// The handshake reads with one-off reads, so that nothing is taken from the
/// socket after the last record rustls needs; `read` refuses to run after
/// `recv`, and says so the first time it is polled. That first poll is all
/// this does: a read of nothing that has not finished by then is dropped,
/// which takes nothing from the socket (waiting for it would wait for the
/// peer, which on a client waits for us).
async fn recv_was_used(tcp: &TcpStream) -> io::Result<bool> {
    let mut read = std::pin::pin!(tcp.read(Window::new(0)));
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(read.as_mut().poll(cx))).await;
    match first {
        std::task::Poll::Ready((Err(e), _)) if giong::error::code(&e) == Some(Code::MixedReads) => {
            Ok(true)
        }
        std::task::Poll::Ready((Err(e), _)) => Err(e),
        _ => Ok(false),
    }
}

struct Handshake<'a> {
    tcp: &'a TcpStream,
    /// Received bytes rustls has not used up yet.
    incoming: Vec<u8>,
    /// Encoded records waiting to be sent.
    out: Vec<u8>,
    /// Application data that arrived before the keys went to the kernel.
    plain: Vec<u8>,
    peer_closed: bool,
}

struct Done {
    kernel: Kernel,
    plain: Vec<u8>,
    peer_closed: bool,
}

/// What to do after handling one state, once the borrows of it are over.
enum Step {
    Again,
    Send,
    /// Read whatever arrives (during the handshake).
    Read,
    Finished,
}

impl<'a> Handshake<'a> {
    fn new(tcp: &'a TcpStream) -> Self {
        Self {
            tcp,
            incoming: Vec::new(),
            out: Vec::new(),
            plain: Vec::new(),
            peer_closed: false,
        }
    }

    async fn run<C: Unbuffered>(mut self, mut conn: C) -> io::Result<Done> {
        loop {
            let step = match self.step(&mut conn) {
                Ok(step) => step,
                Err(e) => {
                    self.send_alert(&mut conn).await;
                    return Err(e);
                }
            };
            match step {
                Step::Again => {}
                Step::Send => self.send().await?,
                Step::Read => self.read(MAX_RECORD).await?,
                Step::Finished => {
                    // The handshake is over. Bytes left over are the start
                    // of a record the handshake did not need (or a handshake
                    // message spread over records): read exactly up to its
                    // end, so that the kernel starts at a record boundary.
                    let need = record_boundary(&self.incoming)?;
                    if need == 0 {
                        break;
                    }
                    self.read(need).await?;
                }
            }
        }
        self.into_kernel(conn)
    }

    /// Handles one state of the connection.
    fn step<C: Unbuffered>(&mut self, conn: &mut C) -> io::Result<Step> {
        let status = conn.process(&mut self.incoming);
        let mut discard = status.discard;
        let state = status.state.map_err(tls_error)?;
        let step = match state {
            ConnectionState::EncodeTlsData(mut data) => {
                let at = self.out.len();
                self.out.resize(at + MAX_RECORD, 0);
                let encoded = match data.encode(&mut self.out[at..]) {
                    // A flight bigger than a record (a long certificate
                    // chain): rustls says how much room it needs.
                    Err(EncodeError::InsufficientSize(s)) => {
                        self.out.resize(at + s.required_size, 0);
                        data.encode(&mut self.out[at..])
                    }
                    other => other,
                };
                match encoded {
                    Ok(n) => self.out.truncate(at + n),
                    Err(e) => {
                        self.out.truncate(at);
                        return Err(unexpected(&format!("rustls: {e}")));
                    }
                }
                Step::Again
            }
            ConnectionState::TransmitTlsData(data) => {
                // The bytes are in `out` and go out before anything else.
                data.done();
                Step::Send
            }
            ConnectionState::ReadTraffic(mut data) => {
                while let Some(record) = data.next_record() {
                    let record = record.map_err(tls_error)?;
                    discard += record.discard;
                    self.plain.extend_from_slice(record.payload);
                }
                Step::Again
            }
            ConnectionState::BlockedHandshake => Step::Read,
            ConnectionState::WriteTraffic(_) => Step::Finished,
            ConnectionState::PeerClosed => {
                self.peer_closed = true;
                Step::Again
            }
            ConnectionState::Closed => Step::Finished,
            ConnectionState::ReadEarlyData(_) => {
                return Err(coded(
                    Code::TlsProtocol,
                    "the client sent 0-RTT (early) data, which giong-tls does not accept",
                    "set max_early_data_size to 0 in the rustls ServerConfig given to \
                     TlsAcceptor::new: 0-RTT data can be replayed.",
                ))
            }
            other => return Err(unexpected(&format!("rustls state {other:?}"))),
        };
        self.incoming.drain(..discard);
        // rustls 0.23 offers WriteTraffic as soon as this side may send, which
        // for a TLS 1.3 server is before the client's Finished.
        if matches!(step, Step::Finished) && conn.common().is_handshaking() {
            return Ok(Step::Read);
        }
        Ok(step)
    }

    async fn send(&mut self) -> io::Result<()> {
        let out = mem::take(&mut self.out);
        let (res, mut out) = self.tcp.write_all(out).await;
        res?;
        out.clear();
        self.out = out;
        Ok(())
    }

    /// One read of at most `limit` bytes, appended to `incoming`.
    async fn read(&mut self, limit: usize) -> io::Result<()> {
        if self.incoming.len() + limit > MAX_INCOMING {
            return Err(coded(
                Code::TlsProtocol,
                "the peer sent more handshake data than a handshake needs",
                "check that the other side speaks TLS on this port.",
            ));
        }
        let (res, window) = self.tcp.read(Window::new(limit)).await;
        if res? == 0 {
            return Err(crate::handshake_eof());
        }
        self.incoming.extend_from_slice(&window.buf);
        Ok(())
    }

    /// Sends the alert rustls queued for the error that ended the
    /// handshake, if it did. Best effort.
    async fn send_alert<C: Unbuffered>(&mut self, conn: &mut C) {
        self.out.clear();
        for _ in 0..4 {
            match self.step(conn) {
                Ok(Step::Again) => {}
                Ok(Step::Send) => {
                    let _ = self.send().await;
                    return;
                }
                _ => return,
            }
        }
    }

    fn into_kernel<C: Unbuffered>(self, conn: C) -> io::Result<Done> {
        let common = conn.common();
        let negotiated = Negotiated {
            alpn: common.alpn_protocol().map(<[u8]>::to_vec),
            peer_certificates: common.peer_certificates().map(<[_]>::to_vec),
            resumed: common.handshake_kind() == Some(HandshakeKind::Resumed),
        };
        let suite = common
            .negotiated_cipher_suite()
            .ok_or_else(|| unexpected("no cipher suite after the handshake"))?;
        let (version, cipher) = Cipher::of_suite(suite.suite())
            .ok_or_else(|| ktls_setup(format!("no kernel cipher for {:?}", suite.suite())))?;
        let record_limit = confidentiality_limit(&suite).saturating_sub(LIMIT_MARGIN);
        let (secrets, side) = conn
            .into_kernel()
            .map_err(|e| ktls_setup(format!("rustls would not hand over the keys: {e}")))?;
        let fd = self.tcp.as_raw_fd();
        let (tx_seq, tx) = &secrets.tx;
        let (rx_seq, rx) = &secrets.rx;
        let tx_seq = *tx_seq;
        CryptoInfo::new(version, tx, tx_seq, true)?
            .install(fd, true)
            .map_err(|e| ktls_setup(format!("TLS_TX: {e}")))?;
        CryptoInfo::new(version, rx, *rx_seq, false)?
            .install(fd, false)
            .map_err(|e| ktls_setup(format!("TLS_RX: {e}")))?;
        drop(secrets);
        if version == Version::Tls13 {
            // A hint, not a promise: rustls and the common TLS libraries do
            // not pad records, and when a record is padded, or is not data,
            // the kernel decrypts it again the slow way. Older kernels do
            // not know the option, which changes nothing.
            let _ = setsockopt(fd, SOL_TLS, TLS_RX_EXPECT_NO_PAD, &1u32.to_ne_bytes());
        }
        Ok(Done {
            kernel: Kernel {
                side,
                negotiated,
                version,
                cipher,
                can_update_keys: version == Version::Tls13 && ktls_support().key_update,
                record_limit,
                sent_base: tx_seq,
                sent_since: 0,
                handshake: Vec::new(),
                warnings_left: 4,
                renegotiations_left: 1,
                key_update_requests_left: KEY_UPDATE_REQUESTS,
                key_update_owed: false,
                recv_failed: None,
                sending_closed: None,
            },
            plain: self.plain,
            peer_closed: self.peer_closed,
        })
    }
}

/// How many more bytes to read so that `buf`, which starts at a record
/// boundary, ends at one: the rest of a record cut short, or the header of
/// the next one when every record in it is whole (a handshake message spread
/// over records). 0 when it is empty.
fn record_boundary(buf: &[u8]) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        let rest = &buf[at..];
        if rest.len() < 5 {
            return Ok(5 - rest.len());
        }
        let len = 5 + u16::from_be_bytes([rest[3], rest[4]]) as usize;
        if len > MAX_RECORD {
            return Err(coded(
                Code::TlsProtocol,
                format!("the peer sent a TLS record of {len} bytes, more than TLS allows"),
                "check that the other side speaks TLS on this port.",
            ));
        }
        if rest.len() < len {
            return Ok(len - rest.len());
        }
        at += len;
    }
    Ok(if buf.is_empty() { 0 } else { 5 })
}

// ---------------------------------------------------- the kernel's stream

/// Maps what the kernel's TLS layer returns to an error that says what
/// happened.
fn kernel_error(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(libc::EBADMSG) => coded(
            Code::TlsProtocol,
            "a TLS record failed to decrypt or authenticate (bad_record_mac): it was changed \
             on the way, or the peer encrypts with other keys",
            "the connection cannot go on; open a new one. If it happens on every connection, \
             check what sits between the two sides.",
        ),
        Some(libc::EMSGSIZE) => coded(
            Code::TlsProtocol,
            "the peer sent a TLS record larger than TLS allows (record_overflow)",
            "check that the other side speaks TLS on this port.",
        ),
        Some(libc::EKEYEXPIRED) => coded(
            Code::KtlsKeyUpdate,
            "the kernel waits for new TLS keys after the peer's KeyUpdate",
            "this is a bug in giong-tls; please report it at \
             https://github.com/giong-run/Giong/issues.",
        ),
        _ => e,
    }
}

/// What a record that is not application data asks of the stream.
enum Control {
    /// Nothing more.
    Done,
    /// Send this warning alert (a refused renegotiation).
    Warn(u8),
    /// The peer asked for a `KeyUpdate` back; it is owed until sent.
    KeyUpdateOwed,
    /// The connection cannot go on; send this fatal alert first, if any.
    Fail(io::Error, Option<u8>),
}

fn peer_misbehaved(what: String) -> io::Error {
    coded(
        Code::TlsProtocol,
        what,
        "check that the other side speaks TLS correctly; the connection cannot go on.",
    )
}

impl TlsStream {
    /// `recv` once the kernel decrypts.
    ///
    /// Cancel safe: what a record changes (keys, tickets, counters, a
    /// failure) is settled before the first `.await` that follows it, and a
    /// `KeyUpdate` the peer asked for stays owed until something is sent.
    pub(crate) async fn kernel_recv(&self) -> io::Result<Vec<u8>> {
        loop {
            {
                let mut plain = self.plain.borrow_mut();
                if !plain.is_empty() {
                    return Ok(mem::take(&mut *plain));
                }
            }
            if let Some(what) = self.kernel_mut().recv_failed.clone() {
                return Err(coded(
                    Code::TlsProtocol,
                    format!("this TLS connection already failed: {what}"),
                    "open a new connection; nothing more can be received on this one.",
                ));
            }
            if self.eof.get() {
                return Ok(Vec::new());
            }
            let control = match self.tcp.recv().await {
                Ok(data) if data.is_empty() => {
                    self.eof.set(true);
                    return Ok(Vec::new());
                }
                Ok(data) => {
                    self.kernel_mut().key_update_requests_left = KEY_UPDATE_REQUESTS;
                    return Ok(data.to_vec());
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    // A record that is not application data, which the
                    // kernel only hands to a receive with room for its type.
                    match recv_record(self.tcp.as_raw_fd()) {
                        Ok(record) => self.control_record(record),
                        Err(r) if r.kind() == io::ErrorKind::WouldBlock => return Err(e),
                        Err(r) => Control::Fail(kernel_error(r), None),
                    }
                }
                Err(e) => match e.raw_os_error() {
                    Some(libc::EBADMSG | libc::EMSGSIZE | libc::EKEYEXPIRED) => {
                        Control::Fail(kernel_error(e), None)
                    }
                    _ => return Err(e),
                },
            };
            match control {
                Control::Done => {}
                Control::Warn(description) => self.send_alert(ALERT_WARNING, description).await,
                Control::KeyUpdateOwed => {
                    // Answer now, so the peer can move on. If the answer
                    // cannot be sent (the peer may already have closed), the
                    // stream can no longer send, and later sends say so; what
                    // the peer sent before closing is still received.
                    let sending = self.lock_send().await;
                    let _ = self.pay_key_update().await;
                    sending.done();
                }
                Control::Fail(e, alert) => {
                    let what = e
                        .get_ref()
                        .and_then(|r| r.downcast_ref::<giong::error::Error>())
                        .map_or_else(|| e.to_string(), |g| g.message().into_owned());
                    self.kernel_mut().recv_failed = Some(what);
                    if let Some(description) = alert {
                        self.send_alert(ALERT_FATAL, description).await;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// The kernel state. Never held across an `.await`.
    fn kernel_mut(&self) -> RefMut<'_, Kernel> {
        RefMut::map(self.engine.borrow_mut(), |e| match e {
            Engine::Kernel(k) => k,
            Engine::User(_) => unreachable!("not a kernel TLS stream"),
        })
    }

    /// A record the kernel returned with a type other than application data.
    fn control_record(&self, (record_type, data): (u8, Vec<u8>)) -> Control {
        match record_type {
            APPLICATION_DATA => {
                self.plain.borrow_mut().extend_from_slice(&data);
                Control::Done
            }
            ALERT => self.alert(&data),
            HANDSHAKE => self.handshake_record(data),
            CHANGE_CIPHER_SPEC => Control::Fail(
                peer_misbehaved("the peer sent change_cipher_spec after the handshake".into()),
                Some(UNEXPECTED_MESSAGE),
            ),
            other => Control::Fail(
                peer_misbehaved(format!(
                    "the peer sent a TLS record of unknown type {other}"
                )),
                Some(UNEXPECTED_MESSAGE),
            ),
        }
    }

    /// An alert from the peer, handled as rustls would.
    fn alert(&self, data: &[u8]) -> Control {
        let &[level, description] = data else {
            return Control::Fail(
                peer_misbehaved(format!("the peer sent an alert of {} bytes", data.len())),
                Some(DECODE_ERROR),
            );
        };
        if level != ALERT_WARNING && level != ALERT_FATAL {
            return Control::Fail(
                peer_misbehaved(format!("the peer sent an alert of level {level}")),
                Some(ILLEGAL_PARAMETER),
            );
        }
        if description == CLOSE_NOTIFY {
            self.close_notify.set(true);
            self.eof.set(true);
            return Control::Done;
        }
        let alert = tls_error(rustls::Error::AlertReceived(description.into()));
        if level == ALERT_FATAL {
            return Control::Fail(alert, None);
        }
        let mut k = self.kernel_mut();
        if k.warnings_left == 0 {
            return Control::Fail(
                peer_misbehaved("the peer sent too many warning alerts".into()),
                None,
            );
        }
        k.warnings_left -= 1;
        // TLS 1.3 has no warnings, except, for no good reason, user_canceled.
        if k.version == Version::Tls13 && description != USER_CANCELED {
            return Control::Fail(alert, Some(DECODE_ERROR));
        }
        Control::Done
    }

    /// Handshake messages after the handshake: session tickets, key updates
    /// and, in TLS 1.2, requests to renegotiate (refused, as rustls does).
    fn handshake_record(&self, data: Vec<u8>) -> Control {
        if data.is_empty() {
            return Control::Fail(
                peer_misbehaved("the peer sent an empty handshake record".into()),
                Some(UNEXPECTED_MESSAGE),
            );
        }
        let mut messages = {
            let mut k = self.kernel_mut();
            k.handshake.extend_from_slice(&data);
            mem::take(&mut k.handshake)
        };
        let mut result = Control::Done;
        let mut at = 0;
        while messages.len() - at >= 4 {
            let typ = messages[at];
            let len = u32::from_be_bytes([0, messages[at + 1], messages[at + 2], messages[at + 3]])
                as usize;
            if len > MAX_HANDSHAKE_MESSAGE {
                return Control::Fail(
                    peer_misbehaved(format!("the peer sent a handshake message of {len} bytes")),
                    Some(DECODE_ERROR),
                );
            }
            if messages.len() - at - 4 < len {
                break;
            }
            let body = &messages[at + 4..at + 4 + len];
            at += 4 + len;
            let last_in_record = at == messages.len();
            match self.post_handshake(typ, body, last_in_record) {
                Control::Done => {}
                fail @ Control::Fail(..) => return fail,
                other => result = other,
            }
        }
        messages.drain(..at);
        if messages.len() > MAX_HANDSHAKE_MESSAGE + 4 {
            return Control::Fail(
                peer_misbehaved("the peer sent a handshake message that never ends".into()),
                Some(DECODE_ERROR),
            );
        }
        self.kernel_mut().handshake = messages;
        result
    }

    fn post_handshake(&self, typ: u8, body: &[u8], last_in_record: bool) -> Control {
        let mut k = self.kernel_mut();
        let client = matches!(k.side, Side::Client(_));
        match (k.version, typ) {
            (Version::Tls13, NEW_SESSION_TICKET) if client => {
                if let Side::Client(c) = &mut k.side {
                    if let Err(e) = c.handle_new_session_ticket(body) {
                        return Control::Fail(tls_error(e), Some(DECODE_ERROR));
                    }
                }
                Control::Done
            }
            (Version::Tls13, KEY_UPDATE) => {
                // The keys change after this message, so it has to end its
                // record (RFC 8446, section 5.1).
                if !last_in_record {
                    return Control::Fail(
                        peer_misbehaved("the peer's KeyUpdate does not end its record".into()),
                        Some(UNEXPECTED_MESSAGE),
                    );
                }
                let requested = match body {
                    [0] => false,
                    [1] => true,
                    [other] => {
                        return Control::Fail(
                            peer_misbehaved(format!(
                                "the peer sent a KeyUpdate with request_update {other}"
                            )),
                            Some(ILLEGAL_PARAMETER),
                        )
                    }
                    _ => {
                        return Control::Fail(
                            peer_misbehaved("the peer sent a malformed KeyUpdate".into()),
                            Some(DECODE_ERROR),
                        )
                    }
                };
                drop(k);
                self.peer_key_update(requested)
            }
            (Version::Tls12, HELLO_REQUEST) if client => Self::refuse_renegotiation(&mut k),
            (Version::Tls12, CLIENT_HELLO) if !client => Self::refuse_renegotiation(&mut k),
            _ => Control::Fail(
                peer_misbehaved(format!(
                    "the peer sent handshake message {typ} after the handshake"
                )),
                Some(UNEXPECTED_MESSAGE),
            ),
        }
    }

    /// Refuses a TLS 1.2 renegotiation with a warning, once, as rustls does.
    fn refuse_renegotiation(k: &mut Kernel) -> Control {
        if k.renegotiations_left == 0 {
            return Control::Fail(
                peer_misbehaved("the peer asked to renegotiate again after being refused".into()),
                None,
            );
        }
        k.renegotiations_left -= 1;
        Control::Warn(NO_RENEGOTIATION)
    }

    /// The peer's TLS 1.3 `KeyUpdate`: from the next record on it encrypts
    /// with new keys. If it asks for one back, ours is owed, and goes out
    /// before anything else this side sends.
    fn peer_key_update(&self, requested: bool) -> Control {
        let fd = self.tcp.as_raw_fd();
        let mut k = self.kernel_mut();
        if k.key_update_requests_left == 0 {
            return Control::Fail(
                peer_misbehaved("the peer sent too many KeyUpdate messages".into()),
                Some(UNEXPECTED_MESSAGE),
            );
        }
        k.key_update_requests_left -= 1;
        if !k.can_update_keys {
            return Control::Fail(
                coded(
                    Code::KtlsKeyUpdate,
                    "the peer changed its TLS 1.3 keys (KeyUpdate), and this kernel cannot take \
                     new keys on a live connection",
                    "run Linux 6.14 or newer, or use Ktls::Auto, which keeps TLS 1.3 in the \
                     process on older kernels.",
                ),
                None,
            );
        }
        let version = k.version;
        let installed =
            k.side
                .update_secret(false)
                .map_err(tls_error)
                .and_then(|(seq, secrets)| {
                    CryptoInfo::new(version, &secrets, seq, false)?
                        .install(fd, false)
                        .map_err(|e| ktls_setup(format!("new TLS_RX key: {e}")))
                });
        if let Err(e) = installed {
            return Control::Fail(e, None);
        }
        KEY_UPDATES.fetch_add(1, Ordering::Relaxed);
        if requested {
            k.key_update_owed = true;
            Control::KeyUpdateOwed
        } else {
            Control::Done
        }
    }

    /// Sends the `KeyUpdate` the peer asked for, if it is still owed and
    /// the stream can still send. The caller holds the send lock.
    async fn pay_key_update(&self) -> io::Result<()> {
        let owed = {
            let k = self.kernel_mut();
            k.key_update_owed && k.sending_closed.is_none()
        };
        if owed && !self.broken.get() {
            self.update_sending_keys().await?;
        }
        Ok(())
    }

    /// Sends a `KeyUpdate` (not asking for one back) and switches the
    /// kernel to the next sending key. The caller holds the send lock. On
    /// failure the stream cannot send any more: the peer may already expect
    /// the new key.
    async fn update_sending_keys(&self) -> io::Result<()> {
        // KeyUpdate, 1 byte of body: update_not_requested.
        let message = vec![KEY_UPDATE, 0, 0, 1, 0];
        let (res, _, _) = self
            .tcp
            .send_with_control(message, record_type_cmsg(HANDSHAKE))
            .await;
        match res {
            Ok(5) => {}
            Ok(n) => {
                self.broken.set(true);
                return Err(unexpected(&format!(
                    "the kernel sent {n} of the 5 bytes of a KeyUpdate"
                )));
            }
            Err(e) => {
                self.broken.set(true);
                return Err(kernel_error(e));
            }
        }
        let fd = self.tcp.as_raw_fd();
        let mut k = self.kernel_mut();
        let version = k.version;
        let installed = k
            .side
            .update_secret(true)
            .map_err(tls_error)
            .and_then(|(seq, secrets)| {
                CryptoInfo::new(version, &secrets, seq, true)?
                    .install(fd, true)
                    .map_err(|e| ktls_setup(format!("new TLS_TX key: {e}")))?;
                Ok(seq)
            });
        match installed {
            Ok(seq) => {
                k.sent_base = seq;
                k.sent_since = 0;
                k.key_update_owed = false;
                drop(k);
                KEY_UPDATES.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                drop(k);
                self.broken.set(true);
                Err(e)
            }
        }
    }

    /// Sends one alert through the kernel. Best effort: the connection is
    /// usually ending.
    async fn send_alert(&self, level: u8, description: u8) {
        let sending = self.lock_send().await;
        if self.broken.get() || self.kernel_mut().sending_closed.is_some() {
            sending.done();
            return;
        }
        let (res, _, _) = self
            .tcp
            .send_with_control(vec![level, description], record_type_cmsg(ALERT))
            .await;
        if res.is_err() || level == ALERT_FATAL {
            self.kernel_mut().sending_closed = Some(Code::TlsBroken);
        }
        sending.done();
    }

    /// `write_all` once the kernel encrypts.
    pub(crate) async fn kernel_write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        let sending = self.lock_send().await;
        if let Err(e) = self.before_kernel_send(buf.len()).await {
            // Nothing of `buf` was sent; a failed key update has already
            // marked the stream broken.
            sending.done();
            return (Err(e), buf);
        }
        let (res, buf) = self.tcp.write_all(buf).await;
        match res {
            Ok(()) => {
                sending.done();
                (Ok(()), buf)
            }
            // Part of `buf` may be gone: the stream is cut, as in process.
            Err(e) => (Err(kernel_error(e)), buf),
        }
    }

    /// `write_all_vectored` once the kernel encrypts.
    pub(crate) async fn kernel_write_all_vectored<B: giong::IoBuf>(
        &self,
        bufs: Vec<B>,
    ) -> (io::Result<()>, Vec<B>) {
        let len = bufs.iter().map(giong::IoBuf::len).sum();
        let sending = self.lock_send().await;
        if let Err(e) = self.before_kernel_send(len).await {
            sending.done();
            return (Err(e), bufs);
        }
        let (res, bufs) = self.tcp.write_all_vectored(bufs).await;
        match res {
            Ok(()) => {
                sending.done();
                (Ok(()), bufs)
            }
            Err(e) => (Err(kernel_error(e)), bufs),
        }
    }

    /// Refuses to send on a closed or broken stream, sends a `KeyUpdate` the
    /// peer asked for, and keeps the number of records under one key below
    /// the cipher's limit: a key update where the kernel can take new keys,
    /// `close_notify` and an error elsewhere (as rustls does for TLS 1.2).
    /// The caller holds the send lock.
    async fn before_kernel_send(&self, len: usize) -> io::Result<()> {
        if self.broken.get() {
            return Err(crate::broken());
        }
        if let Some(code) = self.kernel_mut().sending_closed {
            return Err(sending_closed(code));
        }
        self.pay_key_update().await?;
        let add = records_for(len);
        let fd = self.tcp.as_raw_fd();
        let near_limit = {
            let mut k = self.kernel_mut();
            if k.sent_base + k.sent_since + add < k.record_limit {
                k.sent_since += add;
                false
            } else {
                // Close to the limit by the estimate: ask the kernel.
                k.sent_base = sent_records(fd, k.cipher, k.version).map_err(kernel_error)?;
                k.sent_since = 0;
                if k.sent_base + add < k.record_limit {
                    k.sent_since = add;
                    false
                } else {
                    true
                }
            }
        };
        if !near_limit {
            return Ok(());
        }
        if self.kernel_mut().can_update_keys {
            self.update_sending_keys().await?;
            self.kernel_mut().sent_since = add;
            return Ok(());
        }
        // No new keys to be had: end the stream cleanly, as rustls does.
        let (res, _, _) = self
            .tcp
            .send_with_control(vec![ALERT_WARNING, CLOSE_NOTIFY], record_type_cmsg(ALERT))
            .await;
        // The connection ends either way; the error below says why.
        drop(res);
        self.kernel_mut().sending_closed = Some(Code::TlsKeysExhausted);
        Err(sending_closed(Code::TlsKeysExhausted))
    }

    /// `shutdown` once the kernel encrypts: `close_notify`, then FIN.
    pub(crate) async fn kernel_shutdown(&self) -> io::Result<()> {
        let sending = self.lock_send().await;
        let open = !self.broken.get() && self.kernel_mut().sending_closed.is_none();
        if open {
            let (res, _, _) = self
                .tcp
                .send_with_control(vec![ALERT_WARNING, CLOSE_NOTIFY], record_type_cmsg(ALERT))
                .await;
            self.kernel_mut().sending_closed = Some(Code::TlsBroken);
            if let Err(e) = res {
                sending.done();
                return Err(kernel_error(e));
            }
        }
        sending.done();
        self.tcp.shutdown(std::net::Shutdown::Write)
    }
}

fn sending_closed(code: Code) -> io::Error {
    match code {
        Code::TlsKeysExhausted => coded(
            Code::TlsKeysExhausted,
            "this connection has sent as many TLS records as its cipher allows under one key, \
             and cannot change keys (TLS 1.2, or TLS 1.3 on a kernel older than 6.14); \
             close_notify was sent",
            "open a new connection. TLS 1.3 on Linux 6.14 or newer changes keys instead.",
        ),
        _ => coded(
            Code::TlsBroken,
            "this TLS stream has been shut down or ended with an alert; nothing more can be sent",
            "open a new connection.",
        ),
    }
}

/// The most records one key may encrypt (RFC 8446, section 5.5; the same
/// bound is applied to TLS 1.2 AES-GCM).
fn confidentiality_limit(suite: &SupportedCipherSuite) -> u64 {
    match suite {
        SupportedCipherSuite::Tls12(s) => s.common.confidentiality_limit,
        SupportedCipherSuite::Tls13(s) => s.common.confidentiality_limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_never_reuses_a_tls12_gcm_nonce_rustls_used() {
        let ivs = [
            0u64,
            1,
            0x0123_4567_89ab_cdef,
            u64::MAX,
            u64::MAX - 1,
            0x8000_0000_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0xffff_ffff_0000_0000,
        ];
        let used_counts = (1..300u64).chain([1 << 20, (1 << 24) - 1, 1 << 24, u64::MAX >> 1]);
        for iv in ivs {
            for used in used_counts.clone() {
                let start = u64::from_be_bytes(tls12_gcm_explicit_iv(iv.to_be_bytes(), used));
                // rustls used `iv ^ 0 .. iv ^ (used - 1)`, which all lie in
                // the aligned block of `block` values holding `iv`. The
                // kernel counts up from `start`: it reaches that block again
                // only after 2^64 - block records, so never under the 2^24
                // limit. Checked for every nonce rustls used, when there are
                // few enough to list.
                let block = used.next_power_of_two().max(1);
                let base = iv & !(block - 1);
                assert_eq!(start, base.wrapping_add(block), "iv {iv:#x} used {used}");
                if used <= 300 {
                    for seq in 0..used {
                        let distance = (iv ^ seq).wrapping_sub(start);
                        assert!(distance >= 0u64.wrapping_sub(block), "iv {iv:#x} seq {seq}");
                        assert!(distance > 1 << 24);
                    }
                }
            }
        }
        assert_eq!(tls12_gcm_explicit_iv([9; 8], 0), [9; 8]);
        // The common case: only Finished was sent (sequence number 0).
        assert_eq!(
            u64::from_be_bytes(tls12_gcm_explicit_iv(41u64.to_be_bytes(), 1)),
            42
        );
    }

    #[test]
    fn crypto_info_has_the_kernel_layout() {
        let key = |n: u8| -> Vec<u8> { (0..n).collect() };
        let iv: [u8; 12] = [
            0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
        ];
        let make = |cipher: u8| -> ConnectionTrafficSecrets {
            let k = rustls::crypto::cipher::AeadKey::from(<[u8; 32]>::try_from(key(32)).unwrap());
            let iv = rustls::crypto::cipher::Iv::from(iv);
            match cipher {
                0 => ConnectionTrafficSecrets::Aes256Gcm { key: k, iv },
                _ => ConnectionTrafficSecrets::Chacha20Poly1305 { key: k, iv },
            }
        };
        let mut info = CryptoInfo::new(Version::Tls13, &make(0), 7, true).unwrap();
        let b = info.bytes_mut().to_vec();
        assert_eq!(b.len(), 56);
        assert_eq!(&b[0..2], &TLS_1_3_VERSION.to_ne_bytes());
        assert_eq!(&b[2..4], &TLS_CIPHER_AES_GCM_256.to_ne_bytes());
        assert_eq!(&b[4..12], &iv[4..12], "iv: the last 8 bytes");
        assert_eq!(&b[12..44], &key(32)[..], "key");
        assert_eq!(&b[44..48], &iv[..4], "salt: the first 4 bytes");
        assert_eq!(&b[48..56], &7u64.to_be_bytes(), "rec_seq, big-endian");

        let mut info = CryptoInfo::new(Version::Tls12, &make(1), 1, true).unwrap();
        let b = info.bytes_mut().to_vec();
        assert_eq!(&b[2..4], &TLS_CIPHER_CHACHA20_POLY1305.to_ne_bytes());
        assert_eq!(&b[4..16], &iv, "ChaCha takes the whole nonce, untouched");
        assert_eq!(&b[16..48], &key(32)[..]);
        assert_eq!(&b[48..56], &1u64.to_be_bytes());

        // TLS 1.2 AES-GCM: the sending direction moves the explicit nonce
        // past what rustls used; the receiving one leaves it alone.
        let gcm = || ConnectionTrafficSecrets::Aes256Gcm {
            key: rustls::crypto::cipher::AeadKey::from(<[u8; 32]>::try_from(key(32)).unwrap()),
            iv: rustls::crypto::cipher::Iv::from(iv),
        };
        let mut tx = CryptoInfo::new(Version::Tls12, &gcm(), 1, true).unwrap();
        let mut rx = CryptoInfo::new(Version::Tls12, &gcm(), 1, false).unwrap();
        let (tx, rx) = (tx.bytes_mut().to_vec(), rx.bytes_mut().to_vec());
        assert_eq!(&rx[4..12], &iv[4..12]);
        let explicit = u64::from_be_bytes(iv[4..12].try_into().unwrap());
        assert_eq!(&tx[4..12], &(explicit + 1).to_be_bytes());
    }

    #[test]
    fn records_are_read_up_to_a_boundary() {
        let record = |len: u16| -> Vec<u8> {
            let mut r = vec![23, 3, 3];
            r.extend_from_slice(&len.to_be_bytes());
            r.extend(std::iter::repeat(0).take(len as usize));
            r
        };
        assert_eq!(record_boundary(&[]).unwrap(), 0);
        assert_eq!(
            record_boundary(&[23, 3]).unwrap(),
            3,
            "the rest of the header"
        );
        let mut two = record(10);
        assert_eq!(record_boundary(&two).unwrap(), 5, "whole: the next header");
        two.extend_from_slice(&record(100)[..20]);
        assert_eq!(record_boundary(&two).unwrap(), 85, "the rest of the second");
        let mut huge = vec![23, 3, 3];
        huge.extend_from_slice(&u16::MAX.to_be_bytes());
        assert!(record_boundary(&huge).is_err());
    }

    #[test]
    fn the_record_type_control_message_is_well_formed() {
        let c = record_type_cmsg(ALERT);
        // SAFETY: test only; `c` holds one control message.
        let space = unsafe { libc::CMSG_SPACE(1) } as usize;
        assert_eq!(c.len(), space);
        let mut aligned = vec![0u64; space.div_ceil(8)];
        // SAFETY: both buffers hold `space` bytes.
        unsafe { std::ptr::copy_nonoverlapping(c.as_ptr(), aligned.as_mut_ptr().cast(), space) };
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_control = aligned.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: walks the one message in `aligned`.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            assert_eq!((*cmsg).cmsg_level, SOL_TLS);
            assert_eq!((*cmsg).cmsg_type, TLS_SET_RECORD_TYPE);
            assert_eq!(*libc::CMSG_DATA(cmsg), ALERT);
        }
    }

    /// `true` if the kernel does kTLS; otherwise the test says why and
    /// passes, or fails if `GIONG_KTLS_REQUIRED` is set.
    fn kernel_tls() -> bool {
        let support = ktls_support();
        if support.is_available() {
            return true;
        }
        assert!(
            std::env::var_os("GIONG_KTLS_REQUIRED").is_none(),
            "GIONG_KTLS_REQUIRED is set, but {support}"
        );
        eprintln!("skipped: {support}");
        false
    }

    /// An acceptor whose connections go to the kernel and a connector that
    /// stays in the process, both allowing `suite` only.
    fn kernel_server(suite: SupportedCipherSuite) -> (crate::TlsAcceptor, crate::TlsConnector) {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites = vec![suite];
        let provider = Arc::new(provider);
        let der = rustls::pki_types::PrivatePkcs8KeyDer::from(key.key_pair.serialize_der());
        let server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[suite.version()])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![key.cert.der().clone()], der.into())
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(key.cert.der().clone()).unwrap();
        let client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[suite.version()])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (
            crate::TlsAcceptor::new(Arc::new(server))
                .ktls(Ktls::Required)
                .unwrap(),
            crate::TlsConnector::new(Arc::new(client)),
        )
    }

    #[test]
    fn the_sending_key_changes_or_the_stream_ends_before_the_cipher_limit() {
        if !kernel_tls() {
            return;
        }
        use rustls::crypto::ring::cipher_suite as suites;
        for suite in [
            suites::TLS13_AES_128_GCM_SHA256,
            suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ] {
            let rekeys = suite.tls13().is_some() && ktls_support().key_update();
            let (acceptor, connector) = kernel_server(suite);
            giong::block_on(async move {
                let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap();
                let accept = giong::spawn(async move {
                    let tcp = listener.accept().await.unwrap();
                    acceptor.accept(tcp).await.unwrap()
                });
                let tcp = TcpStream::connect(addr).await.unwrap();
                let client = connector.connect("localhost", tcp).await.unwrap();
                let server = accept.await.unwrap();
                assert!(server.is_ktls() && !client.is_ktls());
                // Pretend the cipher allows 30 more records under one key.
                {
                    let mut k = server.kernel_mut();
                    k.record_limit = k.sent_base + 30;
                }
                let updates = ktls_stats().key_updates;
                let reader = giong::spawn(async move {
                    let mut got = Vec::new();
                    loop {
                        let data = client.recv().await.unwrap();
                        if data.is_empty() {
                            break (got, client.received_close_notify());
                        }
                        got.extend_from_slice(&data);
                    }
                });
                let mut sent = 0u8;
                let mut ended = None;
                for i in 0..100u8 {
                    match server.write_all(vec![i; 1000]).await.0 {
                        Ok(()) => sent += 1,
                        Err(e) => {
                            ended = Some(e);
                            break;
                        }
                    }
                }
                if rekeys {
                    // TLS 1.3 on Linux 6.14+: new keys, and the data goes on.
                    assert!(ended.is_none(), "{ended:?}");
                    server.shutdown().await.unwrap();
                } else {
                    // Otherwise close_notify, then an error that says why.
                    let e = ended.expect("sending stops at the limit");
                    assert_eq!(giong::error::code(&e), Some(Code::TlsKeysExhausted), "{e}");
                    assert!((10..30).contains(&sent), "{sent} writes");
                    let again = server.write_all(vec![0]).await.0.unwrap_err();
                    assert_eq!(giong::error::code(&again), Some(Code::TlsKeysExhausted));
                }
                let (got, close_notify) = reader.await.unwrap();
                assert!(close_notify);
                let expected: Vec<u8> = (0..sent).flat_map(|i| vec![i; 1000]).collect();
                assert!(got == expected, "{} of {} bytes", got.len(), expected.len());
                if rekeys {
                    assert!(ktls_stats().key_updates >= updates + 2);
                }
            });
        }
    }

    #[test]
    fn checking_for_recv_neither_waits_nor_takes_data() {
        giong::block_on(async {
            let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let server = listener.accept().await.unwrap();
            // Nothing to read: the check must not wait for the peer.
            let check = timeout(Duration::from_secs(5), recv_was_used(&client)).await;
            assert!(!check.expect("returned at once").unwrap());
            // Data waiting: the check leaves it in the socket.
            let (res, _) = server.write_all(b"kept".to_vec()).await;
            res.unwrap();
            giong::time::sleep(Duration::from_millis(20)).await;
            assert!(!recv_was_used(&client).await.unwrap());
            let (res, buf) = client.read(Vec::with_capacity(16)).await;
            assert_eq!(res.unwrap(), 4);
            assert_eq!(buf, b"kept");
            // After recv: declined.
            let (res, _) = server.write_all(b"more".to_vec()).await;
            res.unwrap();
            assert_eq!(&client.recv().await.unwrap()[..], b"more");
            assert!(recv_was_used(&client).await.unwrap());
            drop((client, server, listener));
            giong::time::sleep(Duration::from_millis(50)).await;
            let stats = giong::stats();
            assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        });
    }

    #[test]
    fn the_probe_explains_itself() {
        let support = ktls_support();
        let line = support.to_string();
        assert!(line.starts_with("kTLS "), "{line}");
        if let Some(why) = support.unavailable_reason() {
            assert!(!support.is_available());
            assert!(line.contains(why));
        }
    }
}
