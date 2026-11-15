//! Runs the differential check against prost on generated inputs.
//! `GIONG_DIFF_CASES` sets how many (default 200 000); a failure prints the
//! seed, and `GIONG_DIFF_SEED` replays one.

use giong_proto_conformance::diff::{check_proto2, check_proto3, Outcome};
use giong_proto_conformance::gen;

#[test]
fn differential_against_prost() {
    let cases: u64 = std::env::var("GIONG_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let start: u64 = std::env::var("GIONG_DIFF_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let cases = if std::env::var("GIONG_DIFF_SEED").is_ok() {
        1
    } else {
        cases
    };
    let mut stats = std::collections::BTreeMap::<String, u64>::new();
    for seed in start..start + cases {
        let untyped = gen::input(seed);
        let t3 = gen::typed_input(".protobuf_test_messages.proto3.TestAllTypesProto3", seed);
        let t2 = gen::typed_input(".protobuf_test_messages.proto2.TestAllTypesProto2", seed);
        let runs: [(&str, &Vec<u8>, fn(&[u8]) -> _); 4] = [
            ("proto3", &untyped, check_proto3),
            ("proto2", &untyped, check_proto2),
            ("typed proto3", &t3, check_proto3),
            ("typed proto2", &t2, check_proto2),
        ];
        for (name, input, check) in runs {
            match check(input) {
                Ok(o) => {
                    let key = match o {
                        Outcome::Known(d) => format!("{name} known {d:?}"),
                        o => format!("{name} {o:?}"),
                    };
                    *stats.entry(key).or_default() += 1;
                }
                Err(e) => panic!("seed {seed} ({name}): {e}\ninput: {input:02x?}"),
            }
        }
    }
    eprintln!("{cases} inputs: {stats:#?}");
}
