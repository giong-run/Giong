//! What `giong upgrade` reads and edits: a Cargo.toml's dependencies, a
//! Cargo.lock, and version requirements. Any text reads without a panic;
//! every requirement span is inside the text and holds the requirement; an
//! edit of every requirement changes only those bytes, and reading the
//! result again finds the same dependencies with the new requirements; a
//! moved requirement allows the release it was moved to.
#![no_main]

use giong_cli_fuzz::host::Version;
use giong_cli_fuzz::lockfile::parse_lock;
use giong_cli_fuzz::manifest::{allows, apply, moved_req, parse_req, scan, Change};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_lock(text);
    for line in text.lines().take(64) {
        if let Some(req) = parse_req(line) {
            for target in [Version(1, 5, 0), Version(0, 3, 9), Version(2, 0, 0)] {
                let _ = allows(&req, target);
                if let Some(moved) = moved_req(line, target) {
                    let again = parse_req(&moved).expect("a moved requirement reads back");
                    // `1` stays `1` and allows every 1.x; else it names the target.
                    assert!(allows(&again, target), "{line:?} -> {moved:?} refuses {target}");
                }
            }
        }
    }

    let deps = scan(text);
    for d in &deps {
        if let Some(span) = &d.req_span {
            assert!(span.end <= text.len() && span.start <= span.end);
            assert!(text.is_char_boundary(span.start) && text.is_char_boundary(span.end));
            assert_eq!(Some(&text[span.clone()]), d.req.as_deref());
        }
    }
    // Every requirement replaced by one of a known shape.
    let changes: Vec<Change> = deps
        .iter()
        .filter(|d| d.req_span.is_some())
        .map(|d| Change {
            dep: d.clone(),
            new_req: "1.5.0".to_string(),
        })
        .collect();
    // Two dependencies cannot share a span; if the reader ever gave the same
    // bytes twice, the edit below would not be well defined.
    let mut spans: Vec<_> = changes
        .iter()
        .map(|c| c.dep.req_span.clone().unwrap())
        .collect();
    spans.sort_by_key(|s| s.start);
    for w in spans.windows(2) {
        assert!(w[0].end <= w[1].start, "overlapping requirements {spans:?}");
    }
    let out = apply(text, &changes);
    let removed: usize = spans.iter().map(|s| s.len()).sum();
    assert_eq!(out.len(), text.len() - removed + 5 * spans.len());
    let again = scan(&out);
    assert_eq!(again.len(), deps.len(), "{text:?} -> {out:?}");
    for (a, b) in again.iter().zip(&deps) {
        assert_eq!((&a.key, &a.table, a.line), (&b.key, &b.table, b.line));
        if b.req_span.is_some() {
            assert_eq!(a.req.as_deref(), Some("1.5.0"), "{text:?} -> {out:?}");
        }
    }
});
