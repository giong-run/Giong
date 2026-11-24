//! `.env` files: any text parses without a panic; every variable has a valid
//! name and the line it came from; a value written back in single quotes
//! reads back the same.
#![no_main]

use giong_cli_fuzz::dotenv::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (vars, bad) = parse(text);
    let lines = text.lines().count() as u32;
    let mut last = 0;
    for v in &vars {
        assert!(!v.key.is_empty());
        assert!(!v.key.starts_with(|c: char| c.is_ascii_digit()));
        assert!(v.key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        assert!(v.line >= 1 && v.line <= lines, "line {} of {lines}", v.line);
        assert!(v.line > last, "lines out of order");
        last = v.line;
        assert!(text.lines().nth(v.line as usize - 1).unwrap().contains(&v.key));
    }
    for b in &bad {
        assert!(b.line >= 1 && b.line <= lines);
        assert!(!vars.iter().any(|v| v.line == b.line), "a line is both good and bad");
    }
    // Round trip: KEY='value' for every value that can be single-quoted.
    let mut out = String::new();
    let mut kept = Vec::new();
    for v in &vars {
        if !v.value.contains('\'') && !v.value.contains('\n') && !v.value.contains('\r') {
            out.push_str(&format!("{}='{}'\n", v.key, v.value));
            kept.push(v);
        }
    }
    let (again, bad_again) = parse(&out);
    assert!(bad_again.is_empty(), "{out:?}: {bad_again:?}");
    assert_eq!(again.len(), kept.len(), "{out:?}");
    for (a, b) in again.iter().zip(kept) {
        assert_eq!((&a.key, &a.value), (&b.key, &b.value), "{out:?}");
    }
});
