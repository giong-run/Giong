//! Randomised robustness tests: whatever a server (or a URL, or a key file)
//! sends, the parsers return a verdict without panicking.

use std::time::SystemTime;

use crate::auth::{self, RsaPublicKey};
use crate::config::Config;
use crate::connect::{Message, ReadBuf};
use crate::error::DbError;
use crate::protocol;
use crate::row::{Column, RowsBuilder};
use crate::types::{Bits, ColumnType};

/// xorshift64*: deterministic, so a failure can be reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const INTERESTING: &[u8] = b"\0\xff\xfe\xfd\xfc\xfb\x7f\x80\x01\x02\x03\x04\x0a\x0c=&:/?@%[]-";

fn mutate(rng: &mut Rng, input: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(4) {
        if input.is_empty() {
            input.push(rng.next() as u8);
            continue;
        }
        let at = rng.below(input.len());
        match rng.below(6) {
            0 => input[at] = rng.next() as u8,
            1 => input[at] = INTERESTING[rng.below(INTERESTING.len())],
            2 => {
                input.remove(at);
            }
            3 => input.insert(at, INTERESTING[rng.below(INTERESTING.len())]),
            4 => {
                let end = (at + 1 + rng.below(8)).min(input.len());
                let piece = input[at..end].to_vec();
                let to = rng.below(input.len());
                input.splice(to..to, piece);
            }
            _ => input.truncate(at),
        }
    }
}

fn packet(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    protocol::packet(&mut out, seq, |o| o.extend_from_slice(payload));
    out
}

fn column_def(name: &str, ty: u8, flags: u16, collation: u16) -> Vec<u8> {
    let mut p = Vec::new();
    for part in ["def", "giong", "t", "t", name, name] {
        protocol::put_lenenc_bytes(&mut p, part.as_bytes());
    }
    p.push(0x0C);
    p.extend_from_slice(&collation.to_le_bytes());
    p.extend_from_slice(&255u32.to_le_bytes());
    p.push(ty);
    p.extend_from_slice(&flags.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0]);
    p
}

/// The payloads of a plausible conversation, to start mutating from.
fn seed_payloads() -> Vec<Vec<u8>> {
    let mut greeting = vec![10];
    greeting.extend_from_slice(b"8.0.46\0");
    greeting.extend_from_slice(&9u32.to_le_bytes());
    greeting.extend_from_slice(b"abcdefgh\0");
    greeting.extend_from_slice(&0xFFFFu16.to_le_bytes());
    greeting.push(255);
    greeting.extend_from_slice(&2u16.to_le_bytes());
    greeting.extend_from_slice(&0xDFFFu16.to_le_bytes());
    greeting.push(21);
    greeting.extend_from_slice(&[0; 10]);
    greeting.extend_from_slice(b"ijklmnopqrst\0caching_sha2_password\0");
    vec![
        greeting,
        vec![0, 1, 7, 2, 0, 0, 0],
        vec![0xFE, 0, 0, 0x22, 0, 0, 0],
        b"\xff\x26\x04#23000Duplicate entry '1' for key 'PRIMARY'".to_vec(),
        b"\xff\x26\x04Duplicate entry without a SQLSTATE".to_vec(),
        vec![0, 1, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0],
        column_def("id", 8, 32, 63),
        column_def("name", 253, 0, 255),
        column_def("at", 12, 0, 63),
    ]
}

#[test]
fn the_packet_parsers_never_panic() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let seeds = seed_payloads();
    for _ in 0..20_000 {
        let mut input = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut input);
        let _ = protocol::handshake(&input);
        let _ = protocol::ok(&input);
        let _ = protocol::err(&input);
        let _ = DbError::parse(&input);
        let _ = protocol::column_def(&input);
        let _ = protocol::prepare_ok(&input);
        let _ = protocol::is_end_of_rows(&input);
    }
}

#[test]
fn packets_are_framed_whatever_the_bytes() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut stream = Vec::new();
    for (i, p) in seed_payloads().iter().enumerate() {
        stream.extend(packet(i as u8, p));
    }
    for _ in 0..5_000 {
        let mut input = stream.clone();
        mutate(&mut rng, &mut input);
        let limit = [16, 100, 1 << 20][rng.below(3)];
        let mut buf = ReadBuf::new();
        let mut at = 0;
        // Fed in pieces of random size, as the network might.
        while at < input.len() {
            let n = 1 + rng.below(input.len() - at);
            buf.extend(&input[at..at + n]);
            at += n;
            while let Some(m) = buf.next(limit) {
                if let Message::Here { payload, .. } = m {
                    assert!(payload.len() <= limit);
                    let _ = protocol::ok(buf.bytes(payload.clone()));
                    buf.consume(&payload);
                }
            }
        }
    }
}

/// Columns of every type, signed and unsigned, text and binary.
fn all_columns() -> Vec<Column> {
    let mut cols = Vec::new();
    for code in [
        0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 242, 245, 246, 247, 248, 249, 250,
        251, 252, 253, 254, 255, 100,
    ] {
        for (flags, collation) in [(0, 255), (32, 63)] {
            cols.push(Column::for_tests(
                "c",
                ColumnType::from_code(code),
                flags,
                collation,
            ));
        }
    }
    cols
}

#[test]
fn rows_and_values_never_panic() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let all = all_columns();
    for _ in 0..10_000 {
        let n = 1 + rng.below(4);
        let cols: Vec<Column> = (0..n).map(|_| all[rng.below(all.len())].clone()).collect();
        let mut payload = vec![0, 0];
        payload.resize(2 + (n + 9) / 8, 0);
        for _ in 0..rng.below(40) {
            payload.push(rng.next() as u8);
        }
        mutate(&mut rng, &mut payload);
        let mut b = RowsBuilder::default();
        if b.push(&payload, &cols, 1 << 20).is_err() {
            continue;
        }
        for row in b.finish(cols.clone().into()).into_rows() {
            for i in 0..n {
                let _ = row.get::<i64>(i);
                let _ = row.get::<u64>(i);
                let _ = row.get::<i8>(i);
                let _ = row.get::<bool>(i);
                let _ = row.get::<f64>(i);
                let _ = row.get::<f32>(i);
                let _ = row.get::<&str>(i);
                let _ = row.get::<String>(i);
                let _ = row.get::<&[u8]>(i);
                let _ = row.get::<Bits>(i);
                let _ = row.get::<SystemTime>(i);
                let _ = row.get::<Option<String>>(i);
            }
            let _ = format!("{row:?}");
        }
    }
}

#[test]
fn urls_and_key_files_never_panic() {
    let mut rng = Rng(0x94D0_49BB_1331_11EB);
    let urls = [
        "mysql://app:s%40cret@db.internal:3306/shop?ssl-mode=VERIFY_IDENTITY&connect-timeout=5",
        "mysql://app@[::1]:3307/shop?ssl-mode=DISABLED&application-name=x",
        "mysql://app@/shop?socket=/var/run/mysqld/mysqld.sock",
    ];
    for _ in 0..10_000 {
        let mut input = urls[rng.below(urls.len())].as_bytes().to_vec();
        mutate(&mut rng, &mut input);
        let _ = Config::from_url(&String::from_utf8_lossy(&input));
    }
    let pem = auth::tests::PUBLIC_PEM.as_bytes();
    for _ in 0..2_000 {
        let mut input = pem.to_vec();
        mutate(&mut rng, &mut input);
        if RsaPublicKey::from_pem(&input).is_ok() {
            let _ = auth::rsa_encrypt_password("secret", b"0123456789abcdefghij", &input);
        }
    }
}
