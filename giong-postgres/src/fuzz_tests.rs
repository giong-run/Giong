//! Randomised robustness tests: whatever a server (or a URL) sends, the
//! parsers return a verdict without panicking.

use crate::auth::Scram;
use crate::config::Config;
use crate::connect::ReadBuf;
use crate::error::DbError;
use crate::protocol::{data_row, parse_backend};
use crate::row::RowsBuilder;

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

const INTERESTING: &[u8] = b"\0\xff\x7f\x80\x01\x02ZTDCEISRK=,:/?@%[]";

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

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

/// A plausible stream of server messages to start mutating from.
fn seed_stream() -> Vec<u8> {
    let mut s = Vec::new();
    s.extend(message(b'R', &[0, 0, 0, 0]));
    s.extend(message(b'S', b"server_version\x0016.4\0"));
    s.extend(message(b'K', &[0, 0, 1, 2, 3, 4, 5, 6]));
    s.extend(message(b'Z', b"I"));
    s.extend(message(b'1', b""));
    s.extend(message(b't', &[0, 1, 0, 0, 0, 23]));
    let mut row_desc = vec![0, 2];
    for name in ["id", "name"] {
        row_desc.extend_from_slice(name.as_bytes());
        row_desc.push(0);
        row_desc.extend_from_slice(&[
            0, 0, 64, 1, 0, 1, 0, 0, 0, 23, 0, 4, 255, 255, 255, 255, 0, 0,
        ]);
    }
    s.extend(message(b'T', &row_desc));
    s.extend(message(b'2', b""));
    s.extend(message(
        b'D',
        &[0, 2, 0, 0, 0, 4, 0, 0, 0, 7, 255, 255, 255, 255],
    ));
    s.extend(message(b'C', b"SELECT 1\0"));
    s.extend(message(
        b'E',
        b"SERROR\0VERROR\0C23505\0Mduplicate key\0Dkey (id)=(1)\0\0",
    ));
    s.extend(message(b'Z', b"T"));
    s
}

#[test]
fn the_message_parser_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let seed = seed_stream();
    for _ in 0..20_000 {
        let mut input = seed.clone();
        mutate(&mut rng, &mut input);
        // Split the input at a random point, as the network might.
        let cut = rng.below(input.len() + 1);
        let mut buf = ReadBuf::new();
        buf.extend(&input[..cut]);
        let mut fed_rest = false;
        loop {
            let Ok(Some((tag, len))) = buf.peek() else {
                if fed_rest {
                    break;
                }
                buf.extend(&input[cut..]);
                fed_rest = true;
                continue;
            };
            if len > 1 << 20 {
                buf.skip(len);
                continue;
            }
            let Some(body) = buf.body(len) else {
                if fed_rest {
                    break;
                }
                buf.extend(&input[cut..]);
                fed_rest = true;
                continue;
            };
            let bytes = buf.bytes(body).to_vec();
            buf.consume(len);
            if let Ok(msg) = parse_backend(tag, &bytes) {
                let _ = format!("{msg:?}");
            }
            let _ = DbError::parse(&bytes);
            let mut rows = RowsBuilder::default();
            let _ = rows.push(&bytes, 2, 1 << 16);
            let _ = data_row(&bytes, 1, |_| {});
        }
    }
}

#[test]
fn the_url_parser_never_panics() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let seeds = [
        "postgres://app:secret@db.internal:5432/shop?sslmode=disable&application_name=api",
        "postgresql://u@[::1]:5433/db?connect_timeout=3",
        "postgres:///shop?host=/var/run/postgresql&user=u",
        "postgres://u%40x:p%3A@h/d%20b",
    ];
    for _ in 0..20_000 {
        let mut input = seeds[rng.below(seeds.len())].as_bytes().to_vec();
        mutate(&mut rng, &mut input);
        if let Ok(url) = std::str::from_utf8(&input) {
            let _ = Config::from_url(url);
        }
    }
}

#[test]
fn scram_never_panics_on_server_messages() {
    let mut rng = Rng(0x94D0_49BB_1331_11EB);
    let seed = b"r=abcdefXYZ,s=QUJDREVGR0g=,i=16".to_vec();
    for _ in 0..2_000 {
        let mut input = seed.clone();
        mutate(&mut rng, &mut input);
        // Keep the key derivation cheap: the iteration limit itself is
        // tested in the auth module.
        let text = String::from_utf8_lossy(&input);
        let iterations = text.rsplit("i=").next().and_then(|i| i.parse::<u64>().ok());
        if iterations.is_some_and(|i| i > 4096) {
            continue;
        }
        let mut scram = Scram::with_nonce("pw", "", "abcdef");
        let _ = scram.client_final(&input);
        let mut fin = b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=".to_vec();
        mutate(&mut rng, &mut fin);
        let _ = scram.finish(&fin);
    }
}
