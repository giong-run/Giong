//! Randomised robustness tests for the parsers: whatever the input, they must
//! return a verdict without panicking, and agree with themselves however the
//! input is split.

use crate::invariants::{check_chunked, check_head};

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

const SEEDS: &[&[u8]] = &[
    b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
    b"POST /submit?a=1 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
    b"PUT /u HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
    b"GET http://h/p HTTP/1.0\r\nHost: h\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
    b"OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n",
    b"GET HTTPS://Example.com:8443/a?b=/c HTTP/1.1\r\nHost: example.com:8443\r\n\r\n",
];

const INTERESTING: &[u8] = b"\r\n\r\n: ,;\t\0\x7f\xff0123456789abcdefHTTP/1.1";

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

#[test]
fn the_head_parser_never_panics_and_accepts_only_what_it_can_serve() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut accepted = 0;
    for round in 0..300_000 {
        let mut input = SEEDS[round % SEEDS.len()].to_vec();
        mutate(&mut rng, &mut input);
        let from = rng.below(usize::MAX);
        accepted += usize::from(check_head(&input, from));
    }
    // The mutations must not be so destructive that nothing gets through.
    assert!(accepted > 1_000, "only {accepted} inputs were accepted");
}

#[test]
fn the_chunked_decoder_never_panics_and_ignores_how_input_is_split() {
    let seeds: &[&[u8]] = &[
        b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n",
        b"1\r\na\r\n1\r\nb\r\nA\r\n0123456789\r\n0\r\nTrailer: x\r\n\r\n",
        b"0\r\n\r\n",
    ];
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut completed = 0;
    for round in 0..200_000 {
        let mut input = seeds[round % seeds.len()].to_vec();
        if round % 4 != 0 {
            mutate(&mut rng, &mut input);
        }
        let max = [0, 5, 11, 64, 1 << 20][rng.below(5)];
        completed += usize::from(check_chunked(&input, max, || rng.below(6)));
    }
    assert!(completed > 1_000, "only {completed} inputs decoded");
}

#[cfg(feature = "client")]
#[test]
fn the_response_head_parser_never_panics_and_its_ranges_hold() {
    use crate::client::fuzzing::{check_response_head, check_url};
    let seeds: &[&[u8]] = &[
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 12\r\n\r\n",
        b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\nX-Empty:\r\n\r\n",
        b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n",
    ];
    let mut rng = Rng(0x94D0_49BB_1331_11EB);
    let mut accepted = 0;
    for round in 0..200_000 {
        let mut input = seeds[round % seeds.len()].to_vec();
        mutate(&mut rng, &mut input);
        accepted += usize::from(check_response_head(&input));
    }
    assert!(accepted > 1_000, "only {accepted} heads were accepted");

    let urls: &[&[u8]] = &[
        b"https://api.example.com:8443/v1/users?id=7#top",
        b"http://[::1]:3000/a/./b/../c?x=1",
        b"http://127.0.0.1/",
    ];
    let locations: &[&[u8]] = &[
        b"../g?y",
        b"//other/x",
        b"/abs",
        b"?q",
        b"https://h:1/p",
        b"a b/\xc3\xa9",
    ];
    let mut parsed = 0;
    for round in 0..100_000 {
        let mut url = urls[round % urls.len()].to_vec();
        let mut location = locations[rng.below(locations.len())].to_vec();
        mutate(&mut rng, &mut url);
        if round % 2 == 0 {
            mutate(&mut rng, &mut location);
        }
        parsed += usize::from(check_url(
            &String::from_utf8_lossy(&url),
            &String::from_utf8_lossy(&location),
        ));
    }
    assert!(parsed > 1_000, "only {parsed} URLs were parsed");
}

#[cfg(feature = "client")]
#[test]
fn compressed_bodies_decode_or_fail_cleanly_whatever_the_bytes() {
    use crate::client::fuzzing::check_inflate;
    // A gzip member and a zlib stream of the same text, made by hand from
    // a stored (uncompressed) deflate block, so this needs no encoder.
    let text = b"{\"id\":7,\"name\":\"giong\"}";
    let mut stored = vec![1u8];
    stored.extend((text.len() as u16).to_le_bytes());
    stored.extend((!(text.len() as u16)).to_le_bytes());
    stored.extend(text);
    let mut gzip = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    gzip.extend(&stored);
    let mut crc = !0u32;
    for &b in text {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    gzip.extend((!crc).to_le_bytes());
    gzip.extend((text.len() as u32).to_le_bytes());
    let (mut a, mut b) = (1u32, 0u32);
    for &x in text {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    let mut zlib = vec![0x78, 0x01];
    zlib.extend(&stored);
    zlib.extend(((b << 16) | a).to_be_bytes());
    // Untouched, both decode.
    assert!(check_inflate(&[[0u8, 9, 9].as_slice(), &gzip].concat()));
    assert!(check_inflate(&[[1u8, 9, 9].as_slice(), &zlib].concat()));

    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut decoded = 0;
    for round in 0..50_000 {
        let mut input = if round % 2 == 0 {
            gzip.clone()
        } else {
            zlib.clone()
        };
        if round % 7 != 0 {
            mutate(&mut rng, &mut input);
        }
        let how = (rng.below(256) as u8 & !1) | (round % 2) as u8;
        let limit = (rng.below(64) as u16).to_le_bytes();
        let data = [[how, limit[0], limit[1]].as_slice(), &input].concat();
        decoded += usize::from(check_inflate(&data));
    }
    assert!(decoded > 1_000, "only {decoded} bodies decoded");
}

#[test]
fn hpack_decodes_or_fails_cleanly_and_round_trips_whatever_the_blocks() {
    use crate::invariants::{check_hpack, check_huffman};
    // RFC 7541, C.3 and C.4: three requests on one connection, without and
    // with Huffman coding.
    let c3: &[&[u8]] = &[
        b"\x82\x86\x84\x41\x0fwww.example.com",
        b"\x82\x86\x84\xbe\x58\x08no-cache",
        b"\x82\x87\x85\xbf\x40\x0acustom-key\x0ccustom-value",
    ];
    let c4: &[&[u8]] = &[
        b"\x82\x86\x84\x41\x8c\xf1\xe3\xc2\xe5\xf2\x3a\x6b\xa0\xab\x90\xf4\xff",
        b"\x82\x86\x84\xbe\x58\x86\xa8\xeb\x10\x64\x9c\xbf",
        b"\x82\x87\x85\xbf\x40\x88\x25\xa8\x49\xe9\x5b\xa9\x7d\x7f\x89\x25\xa8\x49\xe9\x5b\xb8\xe8\xb4\xbf",
    ];
    let seeds: Vec<Vec<u8>> = [c3, c4]
        .iter()
        .map(|blocks| {
            let mut input = vec![4];
            for block in *blocks {
                input.push(block.len() as u8);
                input.extend_from_slice(block);
            }
            input
        })
        .collect();
    for seed in &seeds {
        assert!(check_hpack(seed));
    }
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    let mut decoded = 0;
    for round in 0..100_000 {
        let mut input = seeds[round % seeds.len()].clone();
        mutate(&mut rng, &mut input);
        decoded += usize::from(check_hpack(&input));
    }
    assert!(decoded > 1_000, "only {decoded} inputs decoded");

    let mut huffman = 0;
    for round in 0..100_000 {
        let mut input = c4[round % 3][5..].to_vec();
        mutate(&mut rng, &mut input);
        huffman += usize::from(check_huffman(&input));
    }
    assert!(huffman > 1_000, "only {huffman} inputs decoded");
}

#[test]
fn http2_connections_answer_with_well_formed_frames_whatever_the_client_sends() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::invariants::h2_connection::{pieces, server, Fuzzed, Memory};

    use crate::invariants::{check_h2_output, h2_input};

    const ROUNDS: usize = 3_000;
    let seed = std::env::var("GIONG_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut rng = Rng(seed.unwrap_or(0xE703_7ED1_A0B4_28DB));
    let responses = giong::Builder::new()
        .workers(1)
        .block_on(async move {
            let handler = Rc::new(Fuzzed);
            let mut responses = 0;
            for round in 0..ROUNDS {
                let len = 1 + rng.below(200);
                let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                let input = h2_input(&data);
                let output = Rc::new(RefCell::new(Vec::new()));
                let stream = Memory {
                    input: RefCell::new(pieces(
                        &input,
                        if round % 3 == 0 { 0 } else { rng.next() as u8 },
                    )),
                    output: output.clone(),
                };
                server().serve_connection(stream, handler.clone()).await;
                responses += check_h2_output(&input, &output.borrow());
            }
            responses
        })
        .unwrap();
    assert!(responses > ROUNDS / 10, "only {responses} responses");
}

#[cfg(feature = "client")]
#[test]
fn the_http2_client_stays_within_its_windows_whatever_the_server_sends() {
    use crate::client::fuzzing::check_h2_client;

    const ROUNDS: usize = 3_000;
    let seed = std::env::var("GIONG_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut rng = Rng(seed.unwrap_or(0x5DEE_CE66_D1CE_4E5B));
    let mut responses = 0;
    for _ in 0..ROUNDS {
        let len = 1 + rng.below(300);
        let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        responses += check_h2_client(&data);
    }
    assert!(responses > ROUNDS / 20, "only {responses} responses");
}
