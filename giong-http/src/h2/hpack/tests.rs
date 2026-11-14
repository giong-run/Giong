use super::*;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn decode_all(decoder: &mut Decoder, block: &[u8]) -> Result<Vec<(String, String)>, Error> {
    let mut fields = Vec::new();
    decoder.decode(block, |n, v| {
        fields.push((
            String::from_utf8(n.to_vec()).unwrap(),
            String::from_utf8(v.to_vec()).unwrap(),
        ))
    })?;
    Ok(fields)
}

fn table(decoder: &Decoder) -> Vec<(String, String)> {
    decoder
        .table
        .entries
        .iter()
        .map(|e| {
            (
                String::from_utf8(e.name().to_vec()).unwrap(),
                String::from_utf8(e.value().to_vec()).unwrap(),
            )
        })
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

struct Step<'a> {
    block: &'a str,
    fields: &'a [(&'a str, &'a str)],
    table: &'a [(&'a str, &'a str)],
    size: usize,
}

fn run(limit: usize, steps: &[Step]) {
    let mut decoder = Decoder::new(limit);
    for (i, step) in steps.iter().enumerate() {
        let fields = decode_all(&mut decoder, &hex(step.block)).unwrap();
        assert_eq!(fields, pairs(step.fields), "fields after step {i}");
        assert_eq!(table(&decoder), pairs(step.table), "table after step {i}");
        assert_eq!(decoder.table.size, step.size, "size after step {i}");
    }
}

const REQUESTS: [&[(&str, &str)]; 3] = [
    &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
    ],
    &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
        ("cache-control", "no-cache"),
    ],
    &[
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/index.html"),
        (":authority", "www.example.com"),
        ("custom-key", "custom-value"),
    ],
];

const REQUEST_TABLES: [&[(&str, &str)]; 3] = [
    &[(":authority", "www.example.com")],
    &[
        ("cache-control", "no-cache"),
        (":authority", "www.example.com"),
    ],
    &[
        ("custom-key", "custom-value"),
        ("cache-control", "no-cache"),
        (":authority", "www.example.com"),
    ],
];

#[test]
fn decodes_rfc_7541_c3_requests_without_huffman() {
    let blocks = [
        "8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d",
        "8286 84be 5808 6e6f 2d63 6163 6865",
        "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
    ];
    let sizes = [57, 110, 164];
    let steps: Vec<Step> = (0..3)
        .map(|i| Step {
            block: blocks[i],
            fields: REQUESTS[i],
            table: REQUEST_TABLES[i],
            size: sizes[i],
        })
        .collect();
    run(4096, &steps);
}

#[test]
fn decodes_rfc_7541_c4_requests_with_huffman() {
    let blocks = [
        "8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff",
        "8286 84be 5886 a8eb 1064 9cbf",
        "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf",
    ];
    let sizes = [57, 110, 164];
    let steps: Vec<Step> = (0..3)
        .map(|i| Step {
            block: blocks[i],
            fields: REQUESTS[i],
            table: REQUEST_TABLES[i],
            size: sizes[i],
        })
        .collect();
    run(4096, &steps);
}

const DATE1: &str = "Mon, 21 Oct 2013 20:13:21 GMT";
const DATE2: &str = "Mon, 21 Oct 2013 20:13:22 GMT";
const LOCATION: &str = "https://www.example.com";
const COOKIE: &str = "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1";

fn responses(blocks: [&str; 3]) {
    run(
        256,
        &[
            Step {
                block: blocks[0],
                fields: &[
                    (":status", "302"),
                    ("cache-control", "private"),
                    ("date", DATE1),
                    ("location", LOCATION),
                ],
                table: &[
                    ("location", LOCATION),
                    ("date", DATE1),
                    ("cache-control", "private"),
                    (":status", "302"),
                ],
                size: 222,
            },
            Step {
                block: blocks[1],
                fields: &[
                    (":status", "307"),
                    ("cache-control", "private"),
                    ("date", DATE1),
                    ("location", LOCATION),
                ],
                table: &[
                    (":status", "307"),
                    ("location", LOCATION),
                    ("date", DATE1),
                    ("cache-control", "private"),
                ],
                size: 222,
            },
            Step {
                block: blocks[2],
                fields: &[
                    (":status", "200"),
                    ("cache-control", "private"),
                    ("date", DATE2),
                    ("location", LOCATION),
                    ("content-encoding", "gzip"),
                    ("set-cookie", COOKIE),
                ],
                table: &[
                    ("set-cookie", COOKIE),
                    ("content-encoding", "gzip"),
                    ("date", DATE2),
                ],
                size: 215,
            },
        ],
    );
}

#[test]
fn decodes_rfc_7541_c5_responses_with_evictions() {
    responses([
        "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133
         2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77 7777 2e65 7861 6d70
         6c65 2e63 6f6d",
        "4803 3330 37c1 c0bf",
        "88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32 3220 474d
         54c0 5a04 677a 6970 7738 666f 6f3d 4153 444a 4b48 514b 425a 584f 5157 454f 5049
         5541 5851 5745 4f49 553b 206d 6178 2d61 6765 3d33 3630 303b 2076 6572 7369 6f6e
         3d31",
    ]);
}

#[test]
fn decodes_rfc_7541_c6_responses_with_huffman() {
    responses([
        "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6
         2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
        "4883 640e ffc1 c0bf",
        "88c1 6196 d07a be94 1054 d444 a820 0595 040b 8166 e084 a62d 1bff c05a 839b d9ab
         77ad 94e7 821d d7f2 e6c7 b335 dfdf cd5b 3960 d5af 2708 7f36 72c1 ab27 0fb5 291f
         9587 3160 65c0 03ed 4ee5 b106 3d50 07",
    ]);
}

#[test]
fn decodes_rfc_7541_c2_single_fields() {
    let mut d = Decoder::new(4096);
    let f = decode_all(
        &mut d,
        &hex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572"),
    )
    .unwrap();
    assert_eq!(f, pairs(&[("custom-key", "custom-header")]));
    assert_eq!(d.table.size, 55);

    let mut d = Decoder::new(4096);
    let f = decode_all(&mut d, &hex("040c 2f73 616d 706c 652f 7061 7468")).unwrap();
    assert_eq!(f, pairs(&[(":path", "/sample/path")]));
    assert!(d.table.entries.is_empty());

    let mut d = Decoder::new(4096);
    let f = decode_all(&mut d, &hex("1008 7061 7373 776f 7264 0673 6563 7265 74")).unwrap();
    assert_eq!(f, pairs(&[("password", "secret")]));
    assert!(d.table.entries.is_empty());

    let mut d = Decoder::new(4096);
    assert_eq!(
        decode_all(&mut d, &[0x82]).unwrap(),
        pairs(&[(":method", "GET")])
    );
}

#[test]
fn integers_round_trip_and_match_rfc_7541_c1() {
    let mut out = Vec::new();
    write_int(&mut out, 0, 5, 10);
    assert_eq!(out, [0x0a]);
    out.clear();
    write_int(&mut out, 0, 5, 1337);
    assert_eq!(out, [0x1f, 0x9a, 0x0a]);
    out.clear();
    write_int(&mut out, 0, 8, 42);
    assert_eq!(out, [0x2a]);
    for n in 1..=8u8 {
        for value in [
            0,
            1,
            30,
            31,
            126,
            127,
            128,
            255,
            256,
            1337,
            65_535,
            1 << 24,
            u32::MAX as usize,
        ] {
            let mut out = Vec::new();
            write_int(&mut out, 0, n, value);
            let mut pos = 0;
            assert_eq!(
                read_int(&out, &mut pos, n),
                Ok(value),
                "{value} with {n} bits"
            );
            assert_eq!(pos, out.len());
        }
    }
}

#[test]
fn refuses_what_rfc_7541_forbids() {
    let bad: [(&str, &[u8]); 9] = [
        ("index 0", &[0x80]),
        ("index past the tables", &[0xbf]),
        ("dynamic index into an empty table", &[0xbe]),
        (
            "integer that never ends",
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        ),
        ("string longer than the block", &[0x40, 0x05, b'a']),
        ("block ends in an integer", &[0xff]),
        ("size update above the limit", &[0x3f, 0xe2, 0x1f]),
        ("size update after a field", &[0x82, 0x20]),
        ("bad Huffman padding", &[0x40, 0x81, 0x00, 0x00]),
    ];
    for (what, block) in bad {
        let mut d = Decoder::new(4096);
        assert!(d.decode(block, |_, _| {}).is_err(), "{what}");
    }
    // A size update at the start is fine, and shrinks the table.
    let mut d = Decoder::new(4096);
    d.decode(
        &hex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572"),
        |_, _| {},
    )
    .unwrap();
    d.decode(&[0x20, 0x82], |_, _| {}).unwrap();
    assert!(d.table.entries.is_empty());
}

#[test]
fn the_encoder_round_trips_through_the_decoder() {
    let mut enc = Encoder::new();
    let mut dec = Decoder::new(4096);
    let messages: Vec<Vec<(&str, &str, Index)>> = vec![
        vec![
            ("content-type", "application/json", Index::Yes),
            ("date", DATE1, Index::Yes),
            ("content-length", "17", Index::No),
            ("set-cookie", "a=b", Index::Never),
            ("x-custom", "value", Index::Yes),
        ],
        vec![
            ("content-type", "application/json", Index::Yes),
            ("date", DATE2, Index::Yes),
            ("x-custom", "value", Index::Yes),
            ("accept-encoding", "gzip, deflate", Index::Yes),
        ],
    ];
    let mut sizes = Vec::new();
    for (round, fields) in messages.iter().enumerate() {
        let mut out = Vec::new();
        enc.begin(&mut out);
        enc.status(if round == 0 { 200 } else { 201 }, &mut out);
        for &(n, v, i) in fields {
            enc.field(n.as_bytes(), v.as_bytes(), i, &mut out);
        }
        sizes.push(out.len());
        let got = decode_all(&mut dec, &out).unwrap();
        let status = if round == 0 { "200" } else { "201" };
        let mut want = vec![(":status".to_string(), status.to_string())];
        want.extend(
            fields
                .iter()
                .map(|&(n, v, _)| (n.to_string(), v.to_string())),
        );
        assert_eq!(got, want);
        assert_eq!(table(&dec).len(), enc.table.entries.len());
    }
    // The second block refers back to the first.
    assert!(sizes[1] < sizes[0], "{sizes:?}");

    // A smaller table announced by the peer is announced back.
    enc.set_limit(0);
    enc.set_limit(64);
    let mut out = Vec::new();
    enc.begin(&mut out);
    enc.field(b"x-custom", b"value", Index::Yes, &mut out);
    assert_eq!(&out[..2], &[0x20, 0x3f]);
    let got = decode_all(&mut dec, &out).unwrap();
    assert_eq!(got, pairs(&[("x-custom", "value")]));
}
