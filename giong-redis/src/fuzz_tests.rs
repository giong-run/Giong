//! Random inputs for the reply reader and the URL parser.

use crate::fuzzing::{check_against_no_limit, cost, read_replies, same};
use crate::resp::encode;
use crate::value::Value;
use crate::Config;

/// A small deterministic generator (xorshift), so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn value(rng: &mut Rng, depth: usize) -> Value {
    let pick = if depth > 3 {
        rng.below(9)
    } else {
        rng.below(13)
    };
    match pick {
        0 => Value::Nil,
        1 => Value::Int(rng.next() as i64 >> rng.below(64)),
        2 => {
            let max = if rng.below(10) == 0 { 3000 } else { 20 };
            let len = rng.below(max) as usize;
            Value::Bulk((0..len).map(|_| rng.below(256) as u8).collect())
        }
        3 => Value::Status("OK".repeat(rng.below(3) as usize)),
        4 => Value::Double((rng.next() as i64 >> 20) as f64 / 7.0),
        5 => Value::Bool(rng.below(2) == 0),
        6 => Value::BigNumber(format!("-{}", rng.next())),
        7 => Value::Verbatim {
            format: "txt".into(),
            text: "x".repeat(rng.below(30) as usize),
        },
        8 => Value::Error(crate::ServerError::new(format!("ERR e{}", rng.below(100)))),
        9 => Value::Array((0..rng.below(5)).map(|_| value(rng, depth + 1)).collect()),
        10 => Value::Set((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
        11 => Value::Push((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
        _ => Value::Map(
            (0..rng.below(4))
                .map(|_| (value(rng, depth + 1), value(rng, depth + 1)))
                .collect(),
        ),
    }
}

/// Random cut sizes: mostly a few bytes, sometimes a few thousand.
fn piece(rng: &mut Rng) -> usize {
    let max = if rng.below(4) == 0 { 4000 } else { 9 };
    1 + rng.below(max) as usize
}

/// Writes `v` as `resp::encode` does, with random attributes in front of it
/// and of its elements (which the reader drops), some of them large.
fn encode_attributed(rng: &mut Rng, v: &Value, out: &mut Vec<u8>) {
    if rng.below(6) == 0 {
        let pairs = rng.below(3);
        out.extend_from_slice(format!("|{pairs}\r\n").as_bytes());
        for _ in 0..2 * pairs {
            let len = if rng.below(4) == 0 { 300 } else { 3 };
            encode(&Value::Bulk(vec![b'a'; rng.below(len) as usize]), out);
        }
    }
    match v {
        Value::Array(a) | Value::Set(a) | Value::Push(a) => {
            let kind = match v {
                Value::Array(_) => '*',
                Value::Set(_) => '~',
                _ => '>',
            };
            out.extend_from_slice(format!("{kind}{}\r\n", a.len()).as_bytes());
            for e in a {
                encode_attributed(rng, e, out);
            }
        }
        Value::Map(m) => {
            out.extend_from_slice(format!("%{}\r\n", m.len()).as_bytes());
            for (k, e) in m {
                encode_attributed(rng, k, out);
                encode_attributed(rng, e, out);
            }
        }
        _ => encode(v, out),
    }
}

#[test]
fn replies_read_back_whole_however_they_are_cut_and_whatever_the_limit() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..30_000 {
        let values: Vec<Value> = (0..1 + rng.below(4)).map(|_| value(&mut rng, 0)).collect();
        let mut input = Vec::new();
        let mut costs = Vec::new();
        for v in &values {
            let before = input.len();
            if rng.below(2) == 0 {
                encode(v, &mut input);
            } else {
                encode_attributed(&mut rng, v, &mut input);
            }
            costs.push(cost(&input[before..]));
        }
        let limit = match rng.below(3) {
            0 => rng.below(200) as usize,
            1 => rng.below(2000) as usize,
            _ => 1 << 20,
        };
        let (got, err) = read_replies(&input, limit, || piece(&mut rng));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(got.len(), values.len());
        for ((g, v), cost) in got.iter().zip(&values).zip(&costs) {
            match g {
                Ok((g, _)) => {
                    assert!(same(g, v), "{g:?} != {v:?}");
                    assert!(*cost <= limit, "{cost} > {limit} was kept");
                }
                // Thrown away only when it is over the limit, and said to
                // be a push message only when it is one, attributes in
                // front of it or not.
                Err(push) => {
                    assert!(*cost > limit, "{cost} <= {limit}");
                    assert_eq!(*push, matches!(v, Value::Push(_)), "{v:?}");
                }
            }
        }
        // The same, against a reading with no limit.
        let seed = rng.next();
        let mut other = Rng(seed | 1);
        check_against_no_limit(&input, limit, || piece(&mut rng), || piece(&mut other));
    }
}

#[test]
fn every_command_gets_its_own_replies_whatever_comes_between() {
    // The connection's reply matching, driven with no socket by random
    // scripts: commands of every shape, waiters dropped, deadlines passed,
    // and push messages, attributes and replies over the limit among the
    // replies, which arrive cut anywhere.
    let mut rng = Rng(0x5851_f42d_4c95_7f2d);
    for _ in 0..20_000 {
        let script: Vec<u8> = (0..rng.below(400)).map(|_| rng.next() as u8).collect();
        crate::client::check_matching(&script);
    }
}

#[test]
fn damaged_replies_are_errors_never_panics() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..200_000 {
        let mut input = Vec::new();
        encode(&value(&mut rng, 0), &mut input);
        for _ in 0..1 + rng.below(4) {
            if input.is_empty() {
                break;
            }
            let at = rng.below(input.len() as u64) as usize;
            match rng.below(3) {
                0 => input[at] = rng.below(256) as u8,
                1 => {
                    input.remove(at);
                }
                _ => input.insert(at, b"\r\n*$:-1"[rng.below(7) as usize]),
            }
        }
        let limit = 1 + rng.below(1 << 16) as usize;
        // Whatever the bytes, reading them ends: with replies, or an error.
        let _ = read_replies(&input, limit, || piece(&mut rng));
    }
}

#[test]
fn urls_parse_or_fail_without_panicking() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let pieces = [
        "redis://",
        "redis+unix://",
        "unix://",
        "/",
        ":",
        "@",
        "[",
        "]",
        "?",
        "&",
        "=",
        "%",
        "%4",
        "%41",
        "host",
        "6379",
        "0",
        "99999",
        "db",
        "client-name",
        "cacert",
        "connect-timeout",
        "0.5",
        "-1",
        "::1",
        "user",
        "pw",
        "nan",
        "inf",
    ];
    for _ in 0..100_000 {
        let url: String = (0..rng.below(10))
            .map(|_| pieces[rng.below(pieces.len() as u64) as usize])
            .collect();
        if let Ok(c) = Config::from_url(&url) {
            // What parsed is usable.
            let _ = c.target();
            assert!(c.port > 0);
        }
    }
}
