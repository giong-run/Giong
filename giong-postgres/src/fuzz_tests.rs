//! Randomised robustness tests: whatever a server (or a URL) sends, the
//! parsers return a verdict without panicking.

use crate::config::Config;
use crate::fuzzing::{flags, messages, scram, session, split_answers};

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
        messages(&input, cut);
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
        let mut fin = b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=".to_vec();
        mutate(&mut rng, &mut fin);
        scram(&input, &fin);
    }
}

/// The inverse of [`split_answers`], for building seeds.
fn join_answers(answers: &[Vec<u8>]) -> Vec<u8> {
    let mut data = Vec::new();
    for answer in answers {
        let n = u16::try_from(answer.len()).expect("an answer under 64 KiB");
        data.extend_from_slice(&n.to_le_bytes());
        data.extend_from_slice(answer);
    }
    data
}

fn column(name: &str, oid: u32) -> Vec<u8> {
    let mut c = name.as_bytes().to_vec();
    c.push(0);
    c.extend_from_slice(&[0, 0, 64, 1, 0, 1]);
    c.extend_from_slice(&oid.to_be_bytes());
    c.extend_from_slice(&[255, 255, 255, 255, 255, 255, 0, 0]);
    c
}

fn row_description(columns: &[(&str, u32)]) -> Vec<u8> {
    let mut body = (columns.len() as u16).to_be_bytes().to_vec();
    for (name, oid) in columns {
        body.extend(column(name, *oid));
    }
    message(b'T', &body)
}

fn parameters(oids: &[u32]) -> Vec<u8> {
    let mut body = (oids.len() as u16).to_be_bytes().to_vec();
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    message(b't', &body)
}

fn data_row(values: &[&[u8]]) -> Vec<u8> {
    let mut body = (values.len() as u16).to_be_bytes().to_vec();
    for v in values {
        body.extend_from_slice(&(v.len() as i32).to_be_bytes());
        body.extend_from_slice(v);
    }
    message(b'D', &body)
}

fn done(tag: &str) -> Vec<u8> {
    message(b'C', format!("{tag}\0").as_bytes())
}

fn ready(status: u8) -> Vec<u8> {
    message(b'Z', &[status])
}

/// What a real server answers to [`session`] with these flags, request by
/// request, in the order the client sends them.
fn session_seed() -> (u8, Vec<Vec<u8>>) {
    let flags = flags::DROPPED | flags::TRANSACTION | flags::RESET;
    let answers = vec![
        // Preparing `SELECT $1::int4 AS a, $2 AS b`.
        [
            message(b'1', b""),
            parameters(&[23, 25]),
            row_description(&[("a", 23), ("b", 25)]),
            ready(b'I'),
        ]
        .concat(),
        // Preparing `UPDATE t SET a = 1`.
        [
            message(b'1', b""),
            parameters(&[]),
            message(b'n', b""),
            ready(b'I'),
        ]
        .concat(),
        // `SET x = 1; SELECT 1` as a simple query.
        [
            done("SET"),
            row_description(&[("?column?", 23)]),
            data_row(&[b"1"]),
            done("SELECT 1"),
            ready(b'I'),
        ]
        .concat(),
        // Preparing the statement whose future is dropped.
        [
            message(b'1', b""),
            parameters(&[23]),
            row_description(&[("pg_sleep", 2278)]),
            ready(b'I'),
        ]
        .concat(),
        // Running the SELECT (int4 in binary, text as text).
        [
            message(b'2', b""),
            data_row(&[&7i32.to_be_bytes(), b"x"]),
            done("SELECT 1"),
            ready(b'I'),
        ]
        .concat(),
        // Running the UPDATE.
        [message(b'2', b""), done("UPDATE 3"), ready(b'I')].concat(),
        // The transaction: BEGIN, the cached UPDATE, which fails, and the
        // ROLLBACK sent when the transaction is dropped.
        [done("BEGIN"), ready(b'T')].concat(),
        [
            message(b'2', b""),
            message(
                b'E',
                b"SERROR\0VERROR\0C23505\0Mduplicate key value violates unique constraint\0\0",
            ),
            ready(b'E'),
        ]
        .concat(),
        [done("ROLLBACK"), ready(b'I')].concat(),
        // The session reset, as one simple query of several statements.
        [
            done("SET"),
            done("RESET"),
            done("CLOSE CURSOR ALL"),
            done("UNLISTEN"),
            row_description(&[("pg_advisory_unlock_all", 2278)]),
            data_row(&[b""]),
            done("SELECT 1"),
            done("DISCARD TEMP"),
            done("DISCARD SEQUENCES"),
            ready(b'I'),
        ]
        .concat(),
        // Preparing and running `SELECT 1`.
        [
            message(b'1', b""),
            parameters(&[]),
            row_description(&[("?column?", 23)]),
            ready(b'I'),
        ]
        .concat(),
        [
            message(b'2', b""),
            data_row(&[&1i32.to_be_bytes()]),
            done("SELECT 1"),
            ready(b'I'),
        ]
        .concat(),
    ];
    (flags, answers)
}

#[test]
fn the_session_seed_is_a_conversation_that_works() {
    // Otherwise the mutations below would start from a session that fails
    // early, and never reach the later states.
    let (flags, answers) = session_seed();
    let outcomes = giong::block_on(session(flags, answers.clone()));
    assert_eq!(outcomes.len(), 6, "{outcomes:?}");
    for (i, outcome) in outcomes.iter().enumerate() {
        match i {
            // The transaction's statement.
            4 => assert_eq!(outcome.as_ref().unwrap_err().code(), Some("23505")),
            _ => assert!(outcome.is_ok(), "{outcomes:?}"),
        }
    }
    assert_eq!(split_answers(&join_answers(&answers)), answers);
}

#[test]
fn a_connection_settles_every_statement_whatever_the_server_answers() {
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    let (seed_flags, answers) = session_seed();
    let seed = join_answers(&answers);
    giong::block_on(async {
        for _ in 0..300 {
            let mut input = seed.clone();
            mutate(&mut rng, &mut input);
            let flags = if rng.below(4) == 0 {
                rng.next() as u8
            } else {
                seed_flags
            };
            session(flags, split_answers(&input)).await;
        }
    });
}
