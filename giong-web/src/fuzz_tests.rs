//! Random inputs for the route table and the whole app: the checks of
//! `fuzzing.rs`, on every `cargo test`.

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

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

#[test]
fn the_route_table_finds_what_the_reference_finds() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..30_000 {
        let len = rng.below(48) as usize;
        let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        crate::fuzzing::check_routes(&data);
    }
}

#[test]
fn chosen_route_sets_agree_with_the_reference() {
    // count, then per route: method and length, then a segment choice each
    // (0-3 text, 4 parameter, 5 catch-all, 6 `/` when last); then the
    // request's method and path pieces (see `route/model.rs`).
    let cases: &[&[u8]] = &[
        // GET /a/a/a and GET /{p0}/a/b: GET /a/a/b backtracks to the second.
        &[2, 10, 0, 0, 0, 10, 4, 0, 1, 0, 3, 0, 3, 0, 4],
        // GET /{*rest} and GET /: `/` is not a catch-all match.
        &[2, 0, 5, 0, 6, 0],
        // POST /a and GET /a: DELETE /a is a 405.
        &[2, 2, 0, 0, 0, 4, 3],
        // `%61` is `a`.
        &[1, 0, 0, 0, 5],
    ];
    for case in cases {
        crate::fuzzing::check_routes(case);
    }
}

#[cfg(feature = "serde")]
#[test]
fn every_request_gets_a_well_formed_answer() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..4_000 {
        let raw = request(&mut rng);
        crate::fuzzing::check_request(&raw);
    }
}

/// Found by the fuzzer: serde_json reads `2e-75` one unit in the last place
/// off, and what `json()` writes for that reads back as `2e-75`.
#[cfg(feature = "serde")]
#[test]
fn a_number_that_does_not_read_back_the_same_is_echoed_as_read() {
    let body = br#"{"a":[2e-75,{"b":null}],"c":"x\fe\r\\\rtu"}"#;
    let mut raw = format!(
        "POST /api/json HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
         Accept-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    let req = giong_http::Request::from_bytes(&raw).expect("the request parses");
    assert_eq!(crate::fuzzing::app::check(req).status(), 200);
}

#[cfg(feature = "serde")]
#[test]
fn large_bodies_are_compressed_in_slices_and_decode_to_the_original() {
    let mut rng = Rng(77);
    for len in [64 * 1024 - 1, 64 * 1024, 64 * 1024 + 1, 200_000] {
        let body: Vec<u8> = (0..len)
            .map(|i| {
                if rng.below(4) == 0 {
                    rng.next() as u8
                } else {
                    b"giong "[i % 6]
                }
            })
            .collect();
        let mut raw = format!(
            "POST /api/echo HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\nContent-Length: {len}\r\n\r\n"
        )
        .into_bytes();
        raw.extend_from_slice(&body);
        let resp = crate::fuzzing::app::check(giong_http::Request::from_bytes(&raw).unwrap());
        assert_eq!(resp.get_header("content-encoding"), Some("gzip"), "{len}");
    }
}

/// A request aimed at the app in `fuzzing.rs`, often valid and often not
/// quite.
#[cfg(feature = "serde")]
fn request(rng: &mut Rng) -> Vec<u8> {
    const METHODS: &[&str] = &["GET", "HEAD", "POST", "DELETE", "OPTIONS", "PUT", "BREW"];
    const SLOTS: &[&str] = &[
        "42",
        "abc",
        "%41",
        "%",
        "%ff",
        "a%2Fb",
        "18446744073709551616",
        "-1",
        "%C3%A9",
        "0",
        "me",
        "",
        "a/b",
    ];
    let (mut method, mut target) = match rng.below(10) {
        0 | 1 => ("GET", format!("/users/{}", rng.pick(SLOTS))),
        2 => (
            "GET",
            format!("/files/{}/{}", rng.pick(SLOTS), rng.pick(SLOTS)),
        ),
        3 | 4 => ("GET", "/search".to_string()),
        5 => ("POST", "/api/echo".to_string()),
        6 => ("POST", "/api/json".to_string()),
        7 => ("POST", "/api/items".to_string()),
        8 => ("DELETE", format!("/api/items/{}", rng.pick(SLOTS))),
        _ => (
            rng.pick(METHODS),
            rng.pick(&[
                "/",
                "/users",
                "/users/",
                "/users/me/",
                "/files",
                "/api",
                "/nothing",
                "*",
            ])
            .to_string(),
        ),
    };
    if rng.below(6) == 0 {
        method = rng.pick(METHODS);
    }
    let json_route = target.starts_with("/api/json") || target == "/api/items";
    if rng.below(3) == 0 {
        target.push('?');
        for i in 0..rng.below(4) {
            if i > 0 {
                target.push('&');
            }
            target.push_str(rng.pick(&[
                "q=caf%C3%A9+au+lait",
                "q",
                "q=%zz",
                "page=3",
                "page=-1",
                "page=x",
                "sort=asc",
                "sort=up",
                "exact=true",
                "exact=1",
                "n=-7",
                "n=99999999999",
                "page=1&page=2",
                "=",
                "&&",
            ]));
        }
    }
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: t\r\n");
    let headers: &[&[&str]] = &[
        &[
            "Origin: https://app.example",
            "Origin: https://evil.example",
            "Origin: null",
        ],
        &[
            "Access-Control-Request-Method: DELETE",
            "Access-Control-Request-Method: PUT",
            "Access-Control-Request-Method: GET",
        ],
        &[
            "Access-Control-Request-Headers: content-type",
            "Access-Control-Request-Headers: content-type, x-other",
            "Access-Control-Request-Headers: Accept-Language",
        ],
        &[
            "Accept-Encoding: gzip",
            "Accept-Encoding: gzip;q=0",
            "Accept-Encoding: *",
            "Accept-Encoding: br",
        ],
        &[
            "X-Request-Id: abc-123",
            "X-Request-Id: bad id with spaces",
            "X-Request-Id: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        ],
    ];
    let content_types = [
        "Content-Type: application/json",
        "Content-Type: application/problem+json; charset=utf-8",
        "Content-Type: text/plain",
    ];
    if json_route && rng.below(5) != 0 {
        head.push_str("Content-Type: application/json\r\n");
    } else if rng.below(2) == 0 {
        head.push_str(rng.pick(&content_types));
        head.push_str("\r\n");
    }
    for options in headers {
        if rng.below(2) == 0 {
            head.push_str(rng.pick(options));
            head.push_str("\r\n");
        }
    }
    let body: Vec<u8> = match rng.below(if json_route { 8 } else { 5 }) {
        0 => Vec::new(),
        1 => (0..rng.below(300)).map(|_| rng.next() as u8).collect(),
        2 => "giong ".repeat(rng.below(200) as usize).into_bytes(),
        _ => rng
            .pick(&[
                r#"{"a":[1,2.5,{"b":null}],"c":"é\n","d":true}"#,
                r#"{"a":"#,
                "[1,2",
                r#""text""#,
                "123",
                "1e400",
                r#"{"a":"\ud800"}"#,
                "",
                r#"{"name":"pen","n":3,"tags":["a","b"]}"#,
                r#"{"name":"pen","tags":[]}"#,
                r#"{"name":1,"tags":[]}"#,
                r#"{"name":"pen","n":"3","tags":[]}"#,
                r#"{"name":"pen"}"#,
                r#"{"name":"caf\u00e9","n":-9223372036854775808,"tags":["\n"],"extra":{}}"#,
            ])
            .as_bytes()
            .to_vec(),
    };
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut raw = head.into_bytes();
    raw.extend_from_slice(&body);
    raw
}

#[cfg(feature = "serde")]
#[test]
fn the_fuzz_seeds_are_valid_requests() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fuzz/seeds/request");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // The published crate leaves fuzz/ out.
    };
    let mut statuses = Vec::new();
    for entry in entries {
        let raw = std::fs::read(entry.unwrap().path()).unwrap();
        let req = giong_http::Request::from_bytes(&raw).expect("a seed parses");
        statuses.push(crate::fuzzing::app::check(req).status());
    }
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 200, 200, 200, 201, 204, 405]);
}
