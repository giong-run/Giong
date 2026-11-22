//! A generator of protobuf-shaped bytes for the differential tests: mostly
//! well-formed fields with the field numbers and wire types of
//! `TestAllTypesProto3`/`Proto2`, with some unknown fields, wrong wire types,
//! nested messages, packed runs, groups and plain corruption mixed in.

/// A small deterministic PRNG (xorshift64*), so a failing seed replays.
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 random bits.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// True one time in `n`.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

/// Field numbers the test messages define, and some they do not.
const FIELDS: &[u32] = &[
    1,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    9,
    10,
    11,
    12,
    13,
    14,
    15,
    16,
    17,
    18,
    19,
    20,
    21,
    22,
    23,
    24,
    25,
    26,
    27,
    31,
    32,
    33,
    34,
    35,
    36,
    37,
    38,
    39,
    40,
    41,
    42,
    43,
    44,
    45,
    46,
    47,
    48,
    49,
    50,
    51,
    52,
    53,
    54,
    55,
    56,
    57,
    58,
    59,
    60,
    61,
    62,
    63,
    64,
    65,
    66,
    67,
    68,
    69,
    70,
    71,
    72,
    73,
    74,
    75,
    76,
    77,
    78,
    79,
    80,
    81,
    82,
    83,
    84,
    85,
    86,
    87,
    88,
    89,
    90,
    91,
    92,
    93,
    94,
    95,
    96,
    97,
    98,
    99,
    100,
    101,
    102,
    103,
    104,
    105,
    106,
    111,
    112,
    113,
    114,
    115,
    116,
    117,
    118,
    119,
    120,
    201,
    202,
    203,
    204,
    205,
    206,
    241,
    242,
    243,
    244,
    245,
    246,
    247,
    248,
    249,
    250,
    251,
    252,
    253,
    254,
    255,
    256,
    301,
    302,
    303,
    304,
    305,
    306,
    307,
    308,
    309,
    310,
    311,
    312,
    313,
    314,
    315,
    316,
    317,
    318,
    401,
    402,
    403,
    404,
    405,
    406,
    407,
    408,
    409,
    410,
    411,
    412,
    413,
    414,
    415,
    416,
    417,
    418,
    500,
    501,
    502,
    503,
    504,
    505,
    1000,
    1001,
    5000,
    536_870_911,
];

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_value(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 => 0,
        1 => 1,
        2 => rng.below(128),
        3 => rng.below(1 << 14),
        4 => u64::MAX - rng.below(4),
        5 => (rng.next() as i32) as i64 as u64, // sign-extended negative int32
        6 => rng.next() >> rng.below(64),
        _ => rng.next() & 0xffff_ffff,
    }
}

fn string_value(rng: &mut Rng, out: &mut Vec<u8>) {
    let n = rng.below(12) as usize;
    for _ in 0..n {
        match rng.below(10) {
            0 => out.extend_from_slice("é".as_bytes()),
            1 => out.extend_from_slice("中".as_bytes()),
            2 => out.extend_from_slice("😀".as_bytes()),
            3 if rng.one_in(8) => out.push(0xff), // not UTF-8
            3 => out.push(0xc3),                  // usually cut UTF-8
            _ => out.push(b'a' + rng.below(26) as u8),
        }
    }
}

/// Appends one message body: `fields` fields, nested `depth` deep at most.
pub fn message(rng: &mut Rng, out: &mut Vec<u8>, depth: u32) {
    let fields = rng.below(14);
    for _ in 0..fields {
        field(rng, out, depth);
    }
    if rng.one_in(40) {
        // Plain corruption: flip or cut a byte.
        if !out.is_empty() {
            let i = rng.below(out.len() as u64) as usize;
            if rng.one_in(2) {
                out[i] ^= 1 << rng.below(8);
            } else {
                out.truncate(i);
            }
        }
    }
}

fn field(rng: &mut Rng, out: &mut Vec<u8>, depth: u32) {
    let num = FIELDS[rng.below(FIELDS.len() as u64) as usize] as u64;
    let wire = match rng.below(20) {
        0..=6 => 0,
        7..=9 => 2,
        10..=11 => 5,
        12..=13 => 1,
        14..=17 => 2,
        18 => 3,
        _ => rng.below(8),
    };
    put_varint(out, (num << 3) | wire);
    match wire {
        0 => put_varint(out, varint_value(rng)),
        1 => out.extend_from_slice(&rng.next().to_le_bytes()),
        5 => out.extend_from_slice(&(rng.next() as u32).to_le_bytes()),
        2 => {
            let mut body = Vec::new();
            match rng.below(4) {
                0 if depth > 0 => message(rng, &mut body, depth - 1),
                1 => {
                    // A packed run of varints or fixed values.
                    let n = rng.below(6);
                    let fixed = rng.below(3);
                    for _ in 0..n {
                        match fixed {
                            0 => put_varint(&mut body, varint_value(rng)),
                            1 => body.extend_from_slice(&(rng.next() as u32).to_le_bytes()),
                            _ => body.extend_from_slice(&rng.next().to_le_bytes()),
                        }
                    }
                }
                2 if depth > 0 => {
                    // A map entry: key 1, value 2.
                    if rng.one_in(4) {
                        message(rng, &mut body, depth - 1);
                    } else {
                        field_with(rng, &mut body, 1, depth);
                        field_with(rng, &mut body, 2, depth);
                    }
                }
                _ => string_value(rng, &mut body),
            }
            put_varint(out, body.len() as u64);
            out.extend_from_slice(&body);
        }
        3 => {
            if depth > 0 {
                message(rng, out, depth - 1);
            }
            if !rng.one_in(30) {
                put_varint(out, (num << 3) | 4);
            }
        }
        _ => {}
    }
}

fn field_with(rng: &mut Rng, out: &mut Vec<u8>, num: u64, depth: u32) {
    let mut tmp = Vec::new();
    field(rng, &mut tmp, depth.saturating_sub(1));
    // Rewrite the field number, keeping the wire type.
    let mut r = giong_proto::wire::Reader::new(&tmp);
    if let Ok(Some((_, w))) = r.tag() {
        put_varint(out, (num << 3) | w as u64);
        out.extend_from_slice(&tmp[r.pos()..]);
    }
}

/// One random input.
pub fn input(seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let depth = rng.below(4) as u32;
    message(&mut rng, &mut out, depth);
    out
}

/// One random input shaped by the schema of message `full`: known fields
/// with their own wire types and value types most of the time, so most
/// inputs are valid and reach deep into nested messages, maps and groups.
pub fn typed_input(full: &str, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let depth = rng.below(5) as u32;
    typed_message(&mut rng, &mut out, full, depth);
    if rng.one_in(50) && !out.is_empty() {
        let i = rng.below(out.len() as u64) as usize;
        if rng.one_in(2) {
            out[i] ^= 1 << rng.below(8);
        } else {
            out.truncate(i);
        }
    }
    out
}

fn typed_message(rng: &mut Rng, out: &mut Vec<u8>, full: &str, depth: u32) {
    use prost_types::field_descriptor_proto::{Label, Type};
    let s = crate::schema::schema();
    let Some(m) = s.messages.get(full) else {
        message(rng, out, depth);
        return;
    };
    let n = rng.below(10);
    for _ in 0..n {
        if m.field.is_empty() || rng.one_in(12) {
            field(rng, out, depth.min(1));
            continue;
        }
        let f = &m.field[rng.below(m.field.len() as u64) as usize];
        let num = f.number() as u64;
        let (want, packable) = crate::schema::expected(f);
        if rng.one_in(60) {
            // Wrong wire type on a known field.
            put_varint(out, (num << 3) | rng.below(6));
            put_varint(out, rng.below(4));
            continue;
        }
        let ty = f.r#type();
        if packable && rng.one_in(2) {
            let mut body = Vec::new();
            for _ in 0..rng.below(6) {
                typed_value(rng, &mut body, ty, f.type_name());
            }
            put_varint(out, (num << 3) | 2);
            put_varint(out, body.len() as u64);
            out.extend_from_slice(&body);
            continue;
        }
        let repeat = if f.label() == Label::Repeated {
            1 + rng.below(3)
        } else {
            1 + (rng.one_in(10) as u64)
        };
        for _ in 0..repeat {
            put_varint(out, (num << 3) | want as u64);
            match ty {
                Type::Message => {
                    let mut body = Vec::new();
                    if depth > 0 {
                        typed_message(rng, &mut body, f.type_name(), depth - 1);
                    }
                    put_varint(out, body.len() as u64);
                    out.extend_from_slice(&body);
                }
                Type::Group => {
                    if depth > 0 {
                        typed_message(rng, out, f.type_name(), depth - 1);
                    }
                    put_varint(out, (num << 3) | 4);
                }
                Type::String | Type::Bytes => {
                    let mut body = Vec::new();
                    if ty == Type::String {
                        string_value(rng, &mut body);
                    } else {
                        for _ in 0..rng.below(8) {
                            body.push(rng.next() as u8);
                        }
                    }
                    put_varint(out, body.len() as u64);
                    out.extend_from_slice(&body);
                }
                _ => typed_value(rng, out, ty, f.type_name()),
            }
        }
    }
}

fn typed_value(
    rng: &mut Rng,
    out: &mut Vec<u8>,
    ty: prost_types::field_descriptor_proto::Type,
    type_name: &str,
) {
    use prost_types::field_descriptor_proto::Type;
    match ty {
        Type::Double | Type::Fixed64 | Type::Sfixed64 => {
            let v = match rng.below(6) {
                0 => f64::NAN.to_bits(),
                1 => (-0.0f64).to_bits(),
                2 => 0,
                _ => rng.next(),
            };
            out.extend_from_slice(&v.to_le_bytes());
        }
        Type::Float | Type::Fixed32 | Type::Sfixed32 => {
            let v = match rng.below(6) {
                0 => f32::NAN.to_bits(),
                1 => (-0.0f32).to_bits(),
                2 => 0,
                _ => rng.next() as u32,
            };
            out.extend_from_slice(&v.to_le_bytes());
        }
        Type::Enum => {
            let s = crate::schema::schema();
            let v = match s.enums.get(type_name) {
                Some((_, values)) if !values.is_empty() && !rng.one_in(8) => {
                    values[rng.below(values.len() as u64) as usize] as i64 as u64
                }
                _ => varint_value(rng),
            };
            put_varint(out, v);
        }
        _ => put_varint(out, varint_value(rng)),
    }
}
