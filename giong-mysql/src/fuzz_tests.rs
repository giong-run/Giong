//! Randomised robustness tests: whatever a server (or a URL, or a key file)
//! sends, the parsers return a verdict without panicking.

use std::cell::RefCell;
use std::rc::Rc;

use crate::auth::{self, RsaPublicKey};
use crate::client::Client;
use crate::config::Config;
use crate::error::ErrorKind;
use crate::fuzzing::{
    all_columns, as_reads, check_payload, framing, pieces, reads, rows, server, session, Memory,
};
use crate::protocol;
use crate::protocol::status;
use crate::row::Column;

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
        check_payload(&input);
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
        // Fed in pieces of random size, as the network might.
        framing(&input, limit, |left| 1 + rng.below(left));
    }
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
        rows(&payload, &cols);
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

/// Scripts a real server could send, named, with the two bytes that choose
/// what the client asks: the seeds of the `session` fuzz target.
fn session_scripts() -> Vec<(&'static str, [u8; 2], Vec<u8>)> {
    let ac = status::AUTOCOMMIT;
    let in_tx = status::AUTOCOMMIT | status::IN_TRANS;
    let more = status::AUTOCOMMIT | status::MORE_RESULTS_EXISTS;
    let mut out = Vec::new();

    let mut prepare = Vec::new();
    server::prepared(&mut prepare, 1, 1);
    let mut result = Vec::new();
    server::result(&mut result, &[1], 42);
    out.push((
        "query",
        [0x01, 0],
        as_reads(&[prepare.clone(), result.clone()]),
    ));

    let mut batch = Vec::new();
    server::packet(&mut batch, 1, &server::ok(more));
    server::text_result(&mut batch, 2, "1", ac);
    out.push(("batch", [0x04, 0], as_reads(&[batch.clone()])));
    // On a shared connection, a reset follows the batch.
    out.push((
        "batch shared",
        [0x14, 0],
        as_reads(&[
            batch,
            server::single(&server::ok(ac)),
            server::single(&server::ok(ac)),
        ]),
    ));

    let mut delete = Vec::new();
    server::prepared(&mut delete, 2, 1);
    out.push((
        "transaction",
        [0x00, 0x01],
        as_reads(&[
            server::single(&server::ok(in_tx)),
            delete.clone(),
            server::single(&server::ok(in_tx)),
            server::single(&server::ok(ac)),
        ]),
    ));
    out.push((
        "deadlock",
        [0x80, 0x01],
        as_reads(&[
            server::single(&server::ok(in_tx)),
            delete,
            server::single(&server::error(1213, "Deadlock found")),
        ]),
    ));

    let mut wide = Vec::new();
    server::result(&mut wide, &[0xFD, 0x70, 0x11, 0x01], 1);
    out.push((
        "70000 columns",
        [0x01, 0],
        as_reads(&[prepare.clone(), wide]),
    ));
    out.push((
        "everything",
        [0xFF, 0x07],
        as_reads(&[prepare, result, server::single(&server::ok(ac))]),
    ));
    out
}

#[test]
fn a_scripted_server_is_read_like_a_real_one() {
    giong::block_on(async {
        let (_, _, script) = &session_scripts()[0];
        let output = Rc::new(RefCell::new(Vec::new()));
        let memory = Memory::new(reads(script), output.clone());
        let client = Client::over_memory(memory, Config::new().host("m").user("u"), false);
        let rows = client.query("SELECT ?", &[&1i64]).await.unwrap();
        assert_eq!(rows[0].get::<i64>(0).unwrap(), 42);
        // A prepare, then an execute.
        let written = output.borrow();
        assert_eq!(written[4], 0x16);
        assert_eq!(written[4 + 4 + "SELECT ?".len() + 1], 0x17);
    });
}

#[test]
fn a_result_with_more_columns_than_mysql_allows_is_refused() {
    // MySQL allows 4,096 columns; a count above 65,535 would make the
    // client keep definitions for as long as the server sends them.
    for count in [
        &[0xFD, 0x70, 0x11, 0x01][..],
        &[0xFE, 0, 0, 0, 0, 1, 0, 0, 0],
    ] {
        giong::block_on(async {
            let mut prepare = Vec::new();
            server::prepared(&mut prepare, 1, 0);
            let mut wide = Vec::new();
            server::result(&mut wide, count, 1);
            let memory = Memory::new(
                reads(&as_reads(&[prepare, wide])),
                Rc::new(RefCell::new(Vec::new())),
            );
            let client = Client::over_memory(memory, Config::new().host("m").user("u"), false);
            let e = client.query("SELECT 1", &[]).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Protocol, "{e}");
            assert!(e.to_string().contains("columns"), "{e}");
        });
    }
}

#[test]
fn sessions_with_a_hostile_server_end_every_request() {
    let scripts = session_scripts();
    giong::block_on(async {
        // As written: the scripts are what a server would send.
        for (_, ops, script) in &scripts {
            session(*ops, reads(script)).await;
        }
        // Mutated, cut anywhere, with any requests.
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        for _ in 0..3_000 {
            let (_, ops, script) = &scripts[rng.below(scripts.len())];
            let mut bytes = reads(script).into_iter().flatten().collect::<Vec<u8>>();
            mutate(&mut rng, &mut bytes);
            let ops = if rng.below(4) == 0 {
                [rng.next() as u8, rng.next() as u8]
            } else {
                *ops
            };
            let seed = if rng.below(3) == 0 {
                0
            } else {
                rng.next() as u8 | 1
            };
            session(ops, pieces(seed, &bytes)).await;
        }
    });
    // The seeds of the fuzz target are these scripts.
    if let Some(dir) = std::env::var_os("GIONG_WRITE_SEEDS") {
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        for (name, ops, script) in &scripts {
            let mut seed = ops.to_vec();
            seed.extend_from_slice(script);
            std::fs::write(dir.join(name.replace(' ', "-")), seed).unwrap();
        }
    }
}
