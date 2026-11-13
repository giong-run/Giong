//! Random inputs for the reply reader and the URL parser.

use crate::resp::{encode, Next, ReadBuf};
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

/// Reads `input` delivered in random pieces; returns the replies
/// (`Err(push)` for one over the limit) or the first error.
fn read(
    rng: &mut Rng,
    input: &[u8],
    limit: usize,
) -> Result<Vec<Result<Value, bool>>, crate::Error> {
    let mut buf = ReadBuf::new();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let max = if rng.below(4) == 0 { 4000 } else { 9 };
        let n = (1 + rng.below(max) as usize).min(input.len() - pos);
        buf.extend(&input[pos..pos + n]);
        pos += n;
        loop {
            match buf.next(limit)? {
                Next::Pending => break,
                Next::Ready(len) => {
                    let pushed = buf.is_push();
                    let v = buf.take(len)?;
                    assert_eq!(pushed, matches!(v, Value::Push(_)));
                    out.push(Ok(v));
                }
                Next::TooLarge { push } => out.push(Err(push)),
            }
        }
    }
    Ok(out)
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Double(x), Value::Double(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Array(x), Value::Array(y))
        | (Value::Set(x), Value::Set(y))
        | (Value::Push(x), Value::Push(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((ka, va), (kb, vb))| same(ka, kb) && same(va, vb))
        }
        _ => a == b,
    }
}

#[test]
fn replies_read_back_whole_however_they_are_cut_and_whatever_the_limit() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..30_000 {
        let values: Vec<Value> = (0..1 + rng.below(4)).map(|_| value(&mut rng, 0)).collect();
        let mut input = Vec::new();
        let mut sizes = Vec::new();
        for v in &values {
            let before = input.len();
            encode(v, &mut input);
            sizes.push(input.len() - before);
        }
        let limit = if rng.below(3) == 0 {
            rng.below(200) as usize
        } else {
            1 << 20
        };
        let got = read(&mut rng, &input, limit).expect("valid RESP");
        assert_eq!(got.len(), values.len());
        for ((g, v), size) in got.iter().zip(&values).zip(&sizes) {
            match g {
                Ok(g) => {
                    assert!(same(g, v), "{g:?} != {v:?}");
                    assert!(*size <= limit, "{size} > {limit} was kept");
                }
                // Thrown away only when it is over the limit, and said to
                // be a push message only when it is one.
                Err(push) => {
                    assert!(*size > limit, "{size} <= {limit}");
                    assert_eq!(*push, matches!(v, Value::Push(_)), "{v:?}");
                }
            }
        }
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
        let _ = read(&mut rng, &input, limit);
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
