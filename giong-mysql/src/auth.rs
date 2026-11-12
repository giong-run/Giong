//! Password authentication: the scrambles of `caching_sha2_password` and
//! `mysql_native_password`, and the RSA encryption `caching_sha2_password`
//! uses to send a password over a connection without TLS.

use ring::digest::{digest, Context, SHA1_FOR_LEGACY_USE_ONLY as SHA1, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{Error, ErrorKind};

/// The scramble `caching_sha2_password` sends when the server may have the
/// password cached: `SHA256(p) XOR SHA256(SHA256(SHA256(p)) || nonce)`.
pub(crate) fn caching_sha2(password: &str, nonce: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = digest(&SHA256, password.as_bytes());
    let stage2 = digest(&SHA256, stage1.as_ref());
    let mut ctx = Context::new(&SHA256);
    ctx.update(stage2.as_ref());
    ctx.update(nonce);
    let mix = ctx.finish();
    xor(stage1.as_ref(), mix.as_ref())
}

/// The scramble of `mysql_native_password`:
/// `SHA1(p) XOR SHA1(nonce || SHA1(SHA1(p)))`.
pub(crate) fn native(password: &str, nonce: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = digest(&SHA1, password.as_bytes());
    let stage2 = digest(&SHA1, stage1.as_ref());
    let mut ctx = Context::new(&SHA1);
    ctx.update(nonce);
    ctx.update(stage2.as_ref());
    let mix = ctx.finish();
    xor(stage1.as_ref(), mix.as_ref())
}

fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x ^ y).collect()
}

/// The password, NUL-terminated and XORed with the nonce, encrypted with the
/// server's RSA public key (PEM) under RSA-OAEP with SHA-1, as MySQL 8
/// expects for a full `caching_sha2_password` login without TLS.
pub(crate) fn rsa_encrypt_password(
    password: &str,
    nonce: &[u8],
    pem: &[u8],
) -> Result<Vec<u8>, Error> {
    let key = RsaPublicKey::from_pem(pem)?;
    let mut message = password.as_bytes().to_vec();
    message.push(0);
    if !nonce.is_empty() {
        for (i, b) in message.iter_mut().enumerate() {
            *b ^= nonce[i % nonce.len()];
        }
    }
    key.encrypt_oaep_sha1(&message, &SystemRandom::new())
}

fn key_error(message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Auth,
        format!(
            "the server's RSA public key cannot be used: {}",
            message.into()
        ),
    )
}

/// An RSA public key, as big-endian magnitudes.
pub(crate) struct RsaPublicKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

impl RsaPublicKey {
    /// Reads `-----BEGIN PUBLIC KEY-----` (SubjectPublicKeyInfo) or
    /// `-----BEGIN RSA PUBLIC KEY-----` (PKCS #1).
    pub(crate) fn from_pem(pem: &[u8]) -> Result<Self, Error> {
        let text = std::str::from_utf8(pem).map_err(|_| key_error("the PEM is not text"))?;
        let (label, body) = pem_body(text).ok_or_else(|| key_error("no PEM block found"))?;
        let der = base64_decode(&body).ok_or_else(|| key_error("the PEM is not base64"))?;
        let mut d = Der(&der);
        let mut outer = d.read(0x30)?;
        let rsa = if label == "PUBLIC KEY" {
            // SEQUENCE { SEQUENCE { OID, params }, BIT STRING { RSAPublicKey } }
            let mut alg = outer.read(0x30)?;
            let oid = alg.read(0x06)?;
            if oid.0 != [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01] {
                return Err(key_error("it is not an RSA key"));
            }
            let bits = outer.read(0x03)?;
            let (&unused, key) = bits
                .0
                .split_first()
                .ok_or_else(|| key_error("the key is empty"))?;
            if unused != 0 {
                return Err(key_error("the key's bit string is not whole bytes"));
            }
            Der(key).read(0x30)?
        } else if label == "RSA PUBLIC KEY" {
            outer
        } else {
            return Err(key_error(format!(
                "a PEM block of {label:?} is not a public key"
            )));
        };
        let mut rsa = rsa;
        let n = strip_zeros(rsa.read(0x02)?.0);
        let e = strip_zeros(rsa.read(0x02)?.0);
        if n.len() < 128 || n.len() > 1024 {
            return Err(key_error(format!(
                "a {}-bit modulus is outside 1024 to 8192 bits",
                n.len() * 8
            )));
        }
        if e.is_empty() || e.len() > 4 || n[n.len() - 1] & 1 == 0 {
            return Err(key_error("the exponent or the modulus is not valid"));
        }
        Ok(Self { n, e })
    }

    /// RSAES-OAEP-ENCRYPT (RFC 8017, 7.1.1) with SHA-1, MGF1-SHA-1 and an
    /// empty label.
    pub(crate) fn encrypt_oaep_sha1(
        &self,
        message: &[u8],
        rng: &dyn SecureRandom,
    ) -> Result<Vec<u8>, Error> {
        const H: usize = 20;
        let k = self.n.len();
        if message.len() + 2 * H + 2 > k {
            return Err(key_error(format!(
                "the password is too long for a {}-bit key; use TLS",
                k * 8
            )));
        }
        let mut db = Vec::with_capacity(k - H - 1);
        db.extend_from_slice(digest(&SHA1, b"").as_ref());
        db.resize(k - message.len() - H - 2, 0);
        db.push(1);
        db.extend_from_slice(message);
        let mut seed = [0u8; H];
        rng.fill(&mut seed)
            .map_err(|_| key_error("no random bytes for the padding"))?;
        let db_mask = mgf1(&seed, db.len());
        let masked_db = xor(&db, &db_mask);
        let seed_mask = mgf1(&masked_db, H);
        let masked_seed = xor(&seed, &seed_mask);
        let mut em = Vec::with_capacity(k);
        em.push(0);
        em.extend_from_slice(&masked_seed);
        em.extend_from_slice(&masked_db);
        let c =
            BigUint::from_be(&em).pow_mod(&BigUint::from_be(&self.e), &BigUint::from_be(&self.n));
        Ok(c.to_be(k))
    }
}

fn mgf1(seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 20);
    let mut counter = 0u32;
    while out.len() < len {
        let mut ctx = Context::new(&SHA1);
        ctx.update(seed);
        ctx.update(&counter.to_be_bytes());
        out.extend_from_slice(ctx.finish().as_ref());
        counter += 1;
    }
    out.truncate(len);
    out
}

fn strip_zeros(b: &[u8]) -> Vec<u8> {
    let start = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    b[start..].to_vec()
}

/// The label and base64 body of the first PEM block.
fn pem_body(text: &str) -> Option<(String, String)> {
    let begin = text.find("-----BEGIN ")?;
    let rest = &text[begin + 11..];
    let label_end = rest.find("-----")?;
    let label = rest[..label_end].to_owned();
    let body_start = label_end + 5;
    let end = rest.find(&format!("-----END {label}-----"))?;
    let body: String = rest[body_start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Some((label, body))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// A cursor over DER, enough for an RSA public key.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    /// The contents of the next element, which must have tag `tag`.
    fn read(&mut self, tag: u8) -> Result<Der<'a>, Error> {
        let bad = || key_error("its DER encoding is malformed");
        let (&t, rest) = self.0.split_first().ok_or_else(bad)?;
        if t != tag {
            return Err(bad());
        }
        let (&first, mut rest) = rest.split_first().ok_or_else(bad)?;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let n = usize::from(first & 0x7F);
            if n == 0 || n > 4 || rest.len() < n {
                return Err(bad());
            }
            let len = rest[..n]
                .iter()
                .fold(0usize, |a, &b| (a << 8) | usize::from(b));
            rest = &rest[n..];
            len
        };
        if rest.len() < len {
            return Err(bad());
        }
        let (body, tail) = rest.split_at(len);
        self.0 = tail;
        Ok(Der(body))
    }
}

/// Just enough unsigned big-integer arithmetic for one RSA public-key
/// operation: little-endian 32-bit limbs, schoolbook multiplication and
/// shift-and-subtract reduction. The exponent is public and short, so a
/// 2048-bit encryption takes a few milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BigUint(Vec<u32>);

impl BigUint {
    fn from_be(bytes: &[u8]) -> Self {
        let mut limbs: Vec<u32> = bytes
            .rchunks(4)
            .map(|c| c.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b)))
            .collect();
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        Self(limbs)
    }

    fn to_be(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, limb) in self.0.iter().enumerate() {
            for j in 0..4 {
                let pos = i * 4 + j;
                if pos < len {
                    out[len - 1 - pos] = (limb >> (8 * j)) as u8;
                }
            }
        }
        out
    }

    fn bits(&self) -> usize {
        match self.0.last() {
            Some(&top) => self.0.len() * 32 - top.leading_zeros() as usize,
            None => 0,
        }
    }

    fn bit(&self, i: usize) -> bool {
        self.0.get(i / 32).is_some_and(|l| l >> (i % 32) & 1 == 1)
    }

    fn mul(&self, other: &Self) -> Self {
        let mut out = vec![0u32; self.0.len() + other.0.len()];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.0.iter().enumerate() {
                let t = u64::from(a) * u64::from(b) + u64::from(out[i + j]) + carry;
                out[i + j] = t as u32;
                carry = t >> 32;
            }
            out[i + other.0.len()] = carry as u32;
        }
        let mut r = Self(out);
        r.trim();
        r
    }

    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    /// `self mod m`, bit by bit.
    fn rem(&self, m: &Self) -> Self {
        let mut r = Self(Vec::with_capacity(m.0.len() + 1));
        for i in (0..self.bits()).rev() {
            r.shl1(self.bit(i));
            if r.ge(m) {
                r.sub(m);
            }
        }
        r
    }

    fn shl1(&mut self, low: bool) {
        let mut carry = u32::from(low);
        for limb in &mut self.0 {
            let next = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            self.0.push(carry);
        }
    }

    fn ge(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return self.0.len() > other.0.len();
        }
        for (a, b) in self.0.iter().rev().zip(other.0.iter().rev()) {
            if a != b {
                return a > b;
            }
        }
        true
    }

    /// `self -= other`, for `self >= other`.
    fn sub(&mut self, other: &Self) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let b = i64::from(other.0.get(i).copied().unwrap_or(0));
            let mut t = i64::from(self.0[i]) - b - borrow;
            borrow = 0;
            if t < 0 {
                t += 1 << 32;
                borrow = 1;
            }
            self.0[i] = t as u32;
        }
        self.trim();
    }

    fn pow_mod(&self, exponent: &Self, m: &Self) -> Self {
        let base = self.rem(m);
        let mut result = Self(vec![1]).rem(m);
        for i in (0..exponent.bits()).rev() {
            result = result.mul(&result).rem(m);
            if exponent.bit(i) {
                result = result.mul(&base).rem(m);
            }
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn scrambles_can_be_checked_as_the_server_does() {
        let nonce =
            b"\x3d\x46\x2a\x6c\x15\x4f\x31\x17\x35\x01\x3c\x34\x7a\x4f\x3e\x2b\x0f\x7c\x59\x5e";
        assert!(native("", nonce).is_empty());
        assert!(caching_sha2("", nonce).is_empty());
        // The server keeps SHA1(SHA1(p)) (or SHA256(SHA256(p))): XORing the
        // scramble with the hash it can compute gives back SHA1(p), whose
        // hash must be what it keeps. The integration tests log in to a real
        // server with both plugins.
        let s = native("secret", nonce);
        let stage1 = digest(&SHA1, b"secret");
        let stage2 = digest(&SHA1, stage1.as_ref());
        let mut ctx = Context::new(&SHA1);
        ctx.update(nonce);
        ctx.update(stage2.as_ref());
        let recovered = xor(&s, ctx.finish().as_ref());
        assert_eq!(digest(&SHA1, &recovered).as_ref(), stage2.as_ref());

        let s = caching_sha2("secret", nonce);
        let stage1 = digest(&SHA256, b"secret");
        let stage2 = digest(&SHA256, stage1.as_ref());
        let mut ctx = Context::new(&SHA256);
        ctx.update(stage2.as_ref());
        ctx.update(nonce);
        let recovered = xor(&s, ctx.finish().as_ref());
        assert_eq!(digest(&SHA256, &recovered).as_ref(), stage2.as_ref());
    }

    #[test]
    fn big_integers_do_modular_arithmetic() {
        let (a, m) = (
            0x0123_4567_89ab_cdef_0123_4567_89ab_cdefu128,
            0x7123_4567_89ab_cdef_0123_4567_89ab_cdefu128,
        );
        // The same power with u128 arithmetic (double-and-add, so nothing
        // overflows).
        let mulmod = |x: u128, mut y: u128| -> u128 {
            let (mut acc, mut x) = (0u128, x % m);
            while y > 0 {
                if y & 1 == 1 {
                    acc = (acc + x) % m;
                }
                x = (x << 1) % m;
                y >>= 1;
            }
            acc
        };
        let (mut expected, mut base, mut e) = (1u128, a % m, 65_537u32);
        while e > 0 {
            if e & 1 == 1 {
                expected = mulmod(expected, base);
            }
            base = mulmod(base, base);
            e >>= 1;
        }
        let r = BigUint::from_be(&a.to_be_bytes()).pow_mod(
            &BigUint::from_be(&[1, 0, 1]),
            &BigUint::from_be(&m.to_be_bytes()),
        );
        assert_eq!(r.to_be(16), expected.to_be_bytes());
        assert_eq!(BigUint::from_be(&[0, 0, 5]).to_be(2), vec![0, 5]);
    }

    /// A throwaway 1024-bit key made with `openssl genpkey` for this test.
    pub(crate) const PUBLIC_PEM: &str = r#"-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDsFcI2Wg4CsgOqyYKG5G2SkvIo
dPnd336HImlHLbnnMO49lJtwDtGGdV/+5l9ipkZg8ArEExW/zrkXROyEKeAjEti6
mmj2dqr9qor9OKxmnHa3ZSb44eMKyY7Ku9FxuNa8AHoDaendC1bs9miH/RuFzydU
3BgcfW839dNRNwbAcwIDAQAB
-----END PUBLIC KEY-----
"#;
    const PRIVATE_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBAOwVwjZaDgKyA6rJ
gobkbZKS8ih0+d3ffociaUctuecw7j2Um3AO0YZ1X/7mX2KmRmDwCsQTFb/OuRdE
7IQp4CMS2LqaaPZ2qv2qiv04rGacdrdlJvjh4wrJjsq70XG41rwAegNp6d0LVuz2
aIf9G4XPJ1TcGBx9bzf101E3BsBzAgMBAAECgYAaw+S+0Lrh51ivFiVsMZ2SjoJR
v+FMHDh2DZqt7Ss0YMHBzYVJ0QREIC5/8ZJC1uMHvm8aB6fhlQg68R93ZF/6ojvX
+/culOWdG+3WWn222LhbFLE+/0L+iLiqg6MxTPX7q2c+srY9xZjdBKlW3rfGRHdI
OsNKuZ0hx2LxvR61MQJBAPc0bM590IxqkGFqseigWOnC36YClY6pD6abHy8HkIgH
TxK4sIGZPNKxDtcvh+e0xcjyy17H8LkjtJpuPG59tJ0CQQD0fA2URKSjMuuc9PCv
WNeFHJexfJpvetUKCLbFlJR9HjyTPrx6lADE/s8dsb8Zg4U8YkTsNJ/q46/a2mVW
MtRPAkA4sHAHRnjtyn5DqMkVtLfPG55YHp2XcpOhPE+BZHWzjCGJBIuNnMuC8ZGa
rU64Rw9YTr1uheFhQ5vmEkOPfwpNAkA5v3OVqgEZa1Wr5U9Um6+MiYxrJ/tyiZj4
ncZONSI9y36TaSYr7fmgjm9tnCZ/aEl4Ud3lzShY2x8Wmq+FBMyRAkAyoya34I/2
rARKCc+k8az3RNCE5hImBghT4RYEKJAyWJqNNvpsOEGnAKk9BiFlQT0u9w4t+tkS
fTch7m+1IYlE
-----END PRIVATE KEY-----
"#;

    #[test]
    fn a_pem_public_key_is_read() {
        let key = RsaPublicKey::from_pem(PUBLIC_PEM.as_bytes()).unwrap();
        assert_eq!(key.n.len(), 128);
        assert_eq!(key.e, vec![1, 0, 1]);
        assert!(RsaPublicKey::from_pem(b"nothing").is_err());
        assert!(RsaPublicKey::from_pem(PRIVATE_PEM.as_bytes()).is_err());
    }

    /// Encrypts with this code and decrypts with OpenSSL, when its command
    /// line tool is installed.
    #[test]
    fn openssl_decrypts_what_is_encrypted() {
        use std::process::Command;
        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("skipped: no openssl command");
            return;
        }
        let dir = std::env::temp_dir().join(format!("giong-mysql-rsa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key.pem");
        std::fs::write(&key, PRIVATE_PEM).unwrap();
        let nonce = b"0123456789abcdefghij";
        let sealed = rsa_encrypt_password("päss wörd", nonce, PUBLIC_PEM.as_bytes()).unwrap();
        let input = dir.join("in.bin");
        std::fs::write(&input, &sealed).unwrap();
        let out = Command::new("openssl")
            .args(["pkeyutl", "-decrypt", "-inkey"])
            .arg(&key)
            .args([
                "-pkeyopt",
                "rsa_padding_mode:oaep",
                "-pkeyopt",
                "rsa_oaep_md:sha1",
                "-in",
            ])
            .arg(&input)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut plain = out.stdout;
        for (i, b) in plain.iter_mut().enumerate() {
            *b ^= nonce[i % nonce.len()];
        }
        assert_eq!(plain, "päss wörd\0".as_bytes());
    }

    #[test]
    fn oaep_padding_decodes() {
        // Encrypt with e = 1 and a modulus larger than any message, so the
        // ciphertext is the padded message itself, then check the padding as
        // a decryptor would.
        let key = RsaPublicKey {
            n: {
                let mut n = vec![0xFF; 128];
                n[127] = 0xFF;
                n
            },
            e: vec![1],
        };
        let message = b"hunter2\0";
        let em = key
            .encrypt_oaep_sha1(message, &SystemRandom::new())
            .unwrap();
        assert_eq!(em.len(), 128);
        assert_eq!(em[0], 0);
        let (masked_seed, masked_db) = em[1..].split_at(20);
        let seed = xor(masked_seed, &mgf1(masked_db, 20));
        let db = xor(masked_db, &mgf1(&seed, masked_db.len()));
        assert_eq!(&db[..20], digest(&SHA1, b"").as_ref());
        let one = db[20..].iter().position(|&b| b != 0).unwrap() + 20;
        assert_eq!(db[one], 1);
        assert_eq!(&db[one + 1..], message);
        // Two encryptions differ: the seed is random.
        let again = key
            .encrypt_oaep_sha1(message, &SystemRandom::new())
            .unwrap();
        assert_ne!(em, again);
        let long = vec![b'x'; 100];
        assert!(key.encrypt_oaep_sha1(&long, &SystemRandom::new()).is_err());
    }
}
