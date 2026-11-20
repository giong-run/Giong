use super::frame::{
    self, kind, parse_single_id, read_varint, write_goaway, write_varint, Code, Header, Need, On,
    Settings, StreamType, VARINT_MAX,
};
use super::qpack::{self, Encoder, STATIC};
use crate::h2::hpack::Index;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn fields(block: &[u8]) -> Result<Vec<(String, String)>, frame::Error> {
    let mut out = Vec::new();
    qpack::decode(block, |n, v| {
        out.push((
            String::from_utf8_lossy(n).into_owned(),
            String::from_utf8_lossy(v).into_owned(),
        ));
    })?;
    Ok(out)
}

/// The examples of RFC 9000, appendix A.1, and the bounds of each length.
#[test]
fn varints_follow_rfc_9000() {
    for (bytes, value) in [
        ("c2197c5eff14e88c", 151_288_809_941_952_652u64),
        ("9d7f3e7d", 494_878_333),
        ("7bbd", 15_293),
        ("25", 37),
        // The same value in two bytes: not the shortest form, still valid.
        ("4025", 37),
    ] {
        let bytes = hex(bytes);
        assert_eq!(read_varint(&bytes), Some((value, bytes.len())));
        for cut in 0..bytes.len() {
            assert_eq!(read_varint(&bytes[..cut]), None);
        }
    }
    for (value, len) in [
        (0u64, 1),
        (63, 1),
        (64, 2),
        (16_383, 2),
        (16_384, 4),
        ((1 << 30) - 1, 4),
        (1 << 30, 8),
        (VARINT_MAX, 8),
    ] {
        let mut out = Vec::new();
        write_varint(&mut out, value);
        assert_eq!(out.len(), len, "{value}");
        assert_eq!(read_varint(&out), Some((value, len)));
    }
}

#[test]
#[should_panic(expected = "varint out of range")]
fn a_varint_above_62_bits_is_a_bug() {
    write_varint(&mut Vec::new(), VARINT_MAX + 1);
}

/// RFC 9114, section 7.2: which frame may arrive where, as a server sees it.
#[test]
fn frames_are_checked_against_the_stream_they_are_on() {
    let header = |kind: u64, on| {
        let mut buf = Vec::new();
        Header::write(&mut buf, kind, 5);
        Header::read(&buf, on)
    };
    for k in [kind::DATA, kind::HEADERS] {
        assert_eq!(
            header(k, On::Request),
            Ok(Need::Got(Header {
                kind: k,
                len: 5,
                size: 2
            }))
        );
        assert_eq!(
            header(k, On::Control).unwrap_err().code,
            Code::FrameUnexpected
        );
    }
    for k in [
        kind::SETTINGS,
        kind::GOAWAY,
        kind::MAX_PUSH_ID,
        kind::CANCEL_PUSH,
    ] {
        assert!(matches!(header(k, On::Control), Ok(Need::Got(_))));
        assert_eq!(
            header(k, On::Request).unwrap_err().code,
            Code::FrameUnexpected
        );
    }
    // A response, as the client reads it: control frames are as wrong
    // there as on a request, and a PUSH_PROMISE names a push that was
    // never allowed.
    for k in [
        kind::CANCEL_PUSH,
        kind::SETTINGS,
        kind::GOAWAY,
        kind::MAX_PUSH_ID,
        0x06,
    ] {
        assert_eq!(
            header(k, On::Response).unwrap_err().code,
            Code::FrameUnexpected
        );
    }
    assert_eq!(
        header(kind::PUSH_PROMISE, On::Response).unwrap_err().code,
        Code::IdError
    );
    for k in [kind::DATA, kind::HEADERS, 0x21] {
        assert!(matches!(header(k, On::Response), Ok(Need::Got(_))));
    }
    for on in [On::Control, On::Request] {
        // A client must not push, and HTTP/2's leftovers are refused.
        for k in [kind::PUSH_PROMISE, 0x02, 0x06, 0x08, 0x09] {
            assert_eq!(
                header(k, on).unwrap_err().code,
                Code::FrameUnexpected,
                "{k}"
            );
        }
        // Reserved (0x1f * N + 0x21) and unknown types pass: to be skipped.
        for k in [0x21, 0x21 + 0x1f * 7, 0x0e, 0xff_ffff] {
            assert!(matches!(header(k, on), Ok(Need::Got(h)) if h.kind == k && h.len == 5));
        }
    }
    // A header cut anywhere asks for more.
    let mut buf = Vec::new();
    Header::write(&mut buf, 0x2040, 70_000);
    assert_eq!(buf.len(), 6);
    for cut in 0..buf.len() {
        assert_eq!(Header::read(&buf[..cut], On::Request), Ok(Need::More));
    }
    assert_eq!(
        Header::read(&buf, On::Request),
        Ok(Need::Got(Header {
            kind: 0x2040,
            len: 70_000,
            size: 6
        }))
    );
}

#[test]
fn stream_types() {
    assert_eq!(StreamType::from_id(0), StreamType::Control);
    assert_eq!(StreamType::from_id(1), StreamType::Push);
    assert_eq!(StreamType::from_id(2), StreamType::QpackEncoder);
    assert_eq!(StreamType::from_id(3), StreamType::QpackDecoder);
    assert_eq!(StreamType::from_id(0x21), StreamType::Unknown(0x21));
}

fn settings(pairs: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(id, value) in pairs {
        write_varint(&mut out, id);
        write_varint(&mut out, value);
    }
    out
}

/// RFC 9114, section 7.2.4.
#[test]
fn settings_are_parsed_strictly() {
    assert_eq!(Settings::parse(&[]), Ok(Settings::default()));
    let s = Settings::parse(&settings(&[
        (1, 4096),
        (6, 16_384),
        (7, 100),
        (0x21 + 0x1f, 9),
    ]));
    assert_eq!(
        s,
        Ok(Settings {
            qpack_max_table_capacity: 4096,
            max_field_section_size: 16_384,
            qpack_blocked_streams: 100,
        })
    );
    // Sent twice.
    for id in [1, 6, 7, 0x33, 0x40, 0x41, 1 << 40] {
        let twice = Settings::parse(&settings(&[(id, 1), (8, 0), (id, 1)]));
        assert_eq!(twice.unwrap_err().code, Code::SettingsError, "{id}");
    }
    // HTTP/2's settings.
    for id in [0, 2, 3, 4, 5] {
        let h2 = Settings::parse(&settings(&[(id, 1)]));
        assert_eq!(h2.unwrap_err().code, Code::SettingsError, "{id}");
    }
    // Cut inside a setting.
    let whole = settings(&[(6, 70_000)]);
    for cut in 1..whole.len() {
        let err = Settings::parse(&whole[..cut]).unwrap_err();
        assert_eq!(err.code, Code::FrameError);
    }
    // A frame full of settings nobody defined.
    let many: Vec<(u64, u64)> = (0..65).map(|i| (1000 + i, 0)).collect();
    assert_eq!(
        Settings::parse(&settings(&many)).unwrap_err().code,
        Code::ExcessiveLoad
    );
    assert!(Settings::parse(&settings(&many[..64])).is_ok());

    // Ours reads back.
    let mut out = Vec::new();
    Settings::write_frame(&mut out, 65_536);
    let Ok(Need::Got(h)) = Header::read(&out, On::Control) else {
        panic!("our SETTINGS frame has a header");
    };
    assert_eq!(
        (h.kind, h.size + h.len as usize),
        (kind::SETTINGS, out.len())
    );
    let ours = Settings::parse(&out[h.size..]).unwrap();
    assert_eq!(ours.max_field_section_size, 65_536);
    assert_eq!(ours.qpack_max_table_capacity, 0);
    assert_eq!(ours.qpack_blocked_streams, 0);
}

#[test]
fn goaway_holds_one_integer() {
    let mut out = Vec::new();
    write_goaway(&mut out, 400);
    let Ok(Need::Got(h)) = Header::read(&out, On::Control) else {
        panic!("our GOAWAY frame has a header");
    };
    assert_eq!(h.kind, kind::GOAWAY);
    assert_eq!(parse_single_id(&out[h.size..]), Ok(400));
    assert_eq!(parse_single_id(&[]).unwrap_err().code, Code::FrameError);
    assert_eq!(parse_single_id(&[0x40]).unwrap_err().code, Code::FrameError);
    assert_eq!(parse_single_id(&[1, 2]).unwrap_err().code, Code::FrameError);
}

/// Entries spot-checked against RFC 9204, appendix A, and the shape of the
/// whole table: lowercase names, pseudo-headers where the RFC has them.
#[test]
fn the_static_table_is_rfc_9204s() {
    for (index, name, value) in [
        (0, ":authority", ""),
        (1, ":path", "/"),
        (4, "content-length", "0"),
        (14, "set-cookie", ""),
        (15, ":method", "CONNECT"),
        (17, ":method", "GET"),
        (21, ":method", "PUT"),
        (23, ":scheme", "https"),
        (24, ":status", "103"),
        (25, ":status", "200"),
        (28, ":status", "503"),
        (31, "accept-encoding", "gzip, deflate, br"),
        (46, "content-type", "application/json"),
        (52, "content-type", "text/html; charset=utf-8"),
        (54, "content-type", "text/plain;charset=utf-8"),
        (
            58,
            "strict-transport-security",
            "max-age=31536000; includesubdomains; preload",
        ),
        (62, "x-xss-protection", "1; mode=block"),
        (63, ":status", "100"),
        (71, ":status", "500"),
        (73, "access-control-allow-credentials", "FALSE"),
        (83, "alt-svc", "clear"),
        (
            85,
            "content-security-policy",
            "script-src 'none'; object-src 'none'; base-uri 'none'",
        ),
        (90, "origin", ""),
        (95, "user-agent", ""),
        (98, "x-frame-options", "sameorigin"),
    ] {
        assert_eq!(STATIC[index], (name, value), "index {index}");
    }
    for (name, _) in STATIC {
        assert!(!name.is_empty());
        assert!(!name.bytes().any(|b| b.is_ascii_uppercase()), "{name}");
    }
    let pseudo: Vec<usize> = (0..99).filter(|&i| STATIC[i].0.starts_with(':')).collect();
    let mut expected: Vec<usize> = vec![0, 1];
    expected.extend(15..=28);
    expected.extend(63..=71);
    assert_eq!(pseudo, expected);
}

/// RFC 9204, appendix B.1, and one of each representation a peer without
/// a dynamic table can send.
#[test]
fn field_sections_decode() {
    // B.1: literal field line with name reference, static.
    assert_eq!(
        fields(&hex("0000 510b 2f69 6e64 6578 2e68 746d 6c")).unwrap(),
        [(":path".to_string(), "/index.html".to_string())]
    );
    // Indexed static (:method GET = 17), literal name and value, a
    // Huffman-coded value ("www.example.com", RFC 7541 C.4.1) under a
    // static name (:authority = 0), and a never-indexed literal.
    let mut block = hex("0000 d1");
    block.extend(hex("23 782d61 03 616263")); // x-a: abc
    block.extend(hex("50 8c f1e3 c2e5 f23a 6ba0 ab90 f4ff"));
    block.extend(hex("7f 45 01 78")); // N=1, static name 84 (authorization): x
    assert_eq!(
        fields(&block).unwrap(),
        [
            (":method".to_string(), "GET".to_string()),
            ("x-a".to_string(), "abc".to_string()),
            (":authority".to_string(), "www.example.com".to_string()),
            ("authorization".to_string(), "x".to_string()),
        ]
    );
    // A Huffman-coded literal name: "x-a" does not shrink, so take a long
    // one written by our encoder.
    let mut out = Vec::new();
    Encoder::begin(&mut out);
    Encoder::field(b"x-request-identifier", b"0", Index::No, &mut out);
    assert_eq!(out[2] & 0xf8, 0x28, "literal name, Huffman coded");
    assert_eq!(
        fields(&out).unwrap(),
        [("x-request-identifier".to_string(), "0".to_string())]
    );
    // An empty section is a prefix and nothing else.
    assert_eq!(fields(&[0, 0]).unwrap(), []);
}

/// Everything that needs a dynamic table is refused, as is every way a
/// section can be cut or point outside the static table.
#[test]
fn field_sections_that_break_rfc_9204_are_refused() {
    for (block, why) in [
        ("", "empty"),
        ("00", "prefix cut"),
        ("0100", "Required Insert Count above zero"),
        ("0080", "Base below zero"),
        ("0000 80", "indexed, dynamic"),
        ("0000 10", "indexed, post-base"),
        ("0000 40 0178", "literal, dynamic name"),
        ("0000 00 0178", "literal, post-base name"),
        ("0000 ff24", "static index 99"),
        ("0000 5f 55 0178", "static name 100"),
        ("0000 51", "value missing"),
        ("0000 5105 2f69", "value cut"),
        ("0000 23 782d", "name cut"),
        ("0000 23 782d61", "value missing after a literal name"),
        ("0000 ff ffffffffff 7f", "integer too large"),
        (
            "0000 5181 ff",
            "Huffman: EOS in the padding position of a full byte",
        ),
        ("0000 517f ffffffff0f", "string length too large"),
    ] {
        let err = fields(&hex(block)).expect_err(why);
        assert_eq!(err.code, Code::QpackDecompressionFailed, "{why}");
    }
}

/// What our encoder writes: the shortest of indexed, name reference and
/// literal, and the never-index bit for secrets.
#[test]
fn the_encoder_uses_the_static_table() {
    let enc = |name: &str, value: &str, index| {
        let mut out = Vec::new();
        Encoder::field(name.as_bytes(), value.as_bytes(), index, &mut out);
        out
    };
    assert_eq!(enc(":method", "GET", Index::Yes), [0xc0 | 17]);
    assert_eq!(
        enc("content-type", "application/json", Index::No),
        [0xc0 | 46]
    );
    // Index 63 and above need the integer's second byte.
    assert_eq!(enc("x-frame-options", "deny", Index::No), [0xff, 97 - 63]);
    // A name in the table with another value: the first entry of that name.
    assert_eq!(enc("content-length", "7", Index::No), [0x50 | 4, 1, b'7']);
    assert_eq!(enc("user-agent", "a", Index::No), [0x5f, 95 - 15, 1, b'a']);
    // Unknown name.
    assert_eq!(
        enc("x-a", "b", Index::No),
        [0x23, b'x', b'-', b'a', 1, b'b']
    );
    // Never indexed: a literal even when the static table has the pair.
    assert_eq!(enc("age", "0", Index::Never), [0x70 | 2, 1, b'0']);
    assert_eq!(enc("x-a", "b", Index::Never)[0], 0x33);

    for (status, bytes) in [
        (200u16, vec![0xc0 | 25]),
        (404, vec![0xc0 | 27]),
        (500, vec![0xff, 71 - 63]),
        // "201" is 15 bits in the Huffman code: two bytes, padded with a one.
        (201, vec![0x5f, 24 - 15, 0x82, 0x10, 0x03]),
    ] {
        let mut out = Vec::new();
        Encoder::status(status, &mut out);
        assert_eq!(out, bytes, "{status}");
    }

    // Every static entry, and a mix of other fields, comes back.
    let mut out = Vec::new();
    Encoder::begin(&mut out);
    let mut sent: Vec<(String, String)> = Vec::new();
    for (i, (name, value)) in STATIC.iter().enumerate() {
        let index = [Index::Yes, Index::No, Index::Never][i % 3];
        Encoder::field(name.as_bytes(), value.as_bytes(), index, &mut out);
        sent.push((name.to_string(), value.to_string()));
    }
    let long = "v".repeat(5000);
    for (name, value) in [
        ("x-empty", ""),
        ("x-long", long.as_str()),
        ("cookie", "a=b; c=d"),
        ("x-bytes", "\u{1}\u{7f}~"),
    ] {
        Encoder::field(name.as_bytes(), value.as_bytes(), Index::No, &mut out);
        sent.push((name.to_string(), value.to_string()));
    }
    assert_eq!(fields(&out).unwrap(), sent);
}

/// The peer's QPACK streams may say what a table of capacity zero allows.
#[test]
fn the_qpack_streams_carry_nothing() {
    // Encoder stream: "set capacity 0", any number of times, and only that.
    assert_eq!(qpack::encoder_stream(&[]), Ok(0));
    assert_eq!(qpack::encoder_stream(&[0x20, 0x20]), Ok(2));
    for bad in [
        &[0x21][..],         // capacity 1
        &[0x3f, 0x00],       // capacity 31
        &[0xc0, 0x01, b'x'], // insert with static name reference
        &[0x41, b'x', 0x00], // insert with literal name
        &[0x00],             // duplicate
        &[0x20, 0x80],       // a good one, then an insertion
    ] {
        let err = qpack::encoder_stream(bad).unwrap_err();
        assert_eq!(err.code, Code::QpackEncoderStreamError, "{bad:?}");
    }
    // A capacity whose integer is cut: nothing read yet.
    assert_eq!(qpack::encoder_stream(&[0x20, 0x3f]), Ok(1));
    assert_eq!(qpack::encoder_stream(&[0x3f, 0x80]), Ok(0));

    // Decoder stream: stream cancellations and nothing else.
    assert_eq!(qpack::decoder_stream(&[]), Ok(0));
    assert_eq!(qpack::decoder_stream(&[0x44, 0x7f, 0x05]), Ok(3));
    assert_eq!(qpack::decoder_stream(&[0x44, 0x7f]), Ok(1));
    for bad in [
        &[0x84][..],   // section acknowledgment
        &[0x01],       // insert count increment
        &[0x00],       // an increment of zero
        &[0x44, 0x81], // a good one, then an acknowledgment
    ] {
        let err = qpack::decoder_stream(bad).unwrap_err();
        assert_eq!(err.code, Code::QpackDecoderStreamError, "{bad:?}");
    }
}

/// Random and mutated sections through the invariants the fuzz target
/// checks (`qpack::check`).
#[test]
fn random_sections_keep_the_invariants() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut seed = Vec::new();
    Encoder::begin(&mut seed);
    for (name, value) in STATIC.iter().take(40) {
        Encoder::field(name.as_bytes(), value.as_bytes(), Index::No, &mut seed);
        Encoder::field(
            b"x-custom-header",
            value.as_bytes(),
            Index::Never,
            &mut seed,
        );
    }
    assert!(qpack::check(&seed));
    let mut decoded = 0;
    for round in 0..200_000 {
        let mut block = if round % 4 == 0 {
            let len = (next() % 40) as usize;
            (0..len).map(|_| next() as u8).collect::<Vec<u8>>()
        } else {
            let start = (next() as usize % seed.len()).min(seed.len() - 2);
            let len = 2 + next() as usize % 60;
            let mut b = vec![0, 0];
            b.extend_from_slice(&seed[start..seed.len().min(start + len)]);
            b
        };
        for _ in 0..(next() % 3) {
            if !block.is_empty() {
                let at = next() as usize % block.len();
                block[at] ^= 1 << (next() % 8);
            }
        }
        decoded += usize::from(qpack::check(&block));
    }
    assert!(decoded > 1000, "only {decoded} sections decoded");
}
