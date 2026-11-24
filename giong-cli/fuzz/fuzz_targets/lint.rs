//! giong's source checks (`giong check`, E5006-E5008) read any text a
//! project holds: no panic, the mask keeps every byte offset and line break,
//! every place is inside the file, and every edit replaces whole characters
//! inside the file without overlapping another edit of the same finding.
#![no_main]

use giong_cli_fuzz::lint::{check_file, mask};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let m = mask(text);
    assert_eq!(m.len(), text.len(), "mask changed the length");
    for (a, b) in text.bytes().zip(m.bytes()) {
        assert_eq!(a == b'\n', b == b'\n', "mask moved a line break");
    }
    let lines = text.split('\n').count();
    for f in check_file(text, "src/x.rs") {
        if let Some(p) = &f.place {
            assert!(p.line >= 1 && p.line as usize <= lines, "line {} of {lines}", p.line);
            assert!(p.column >= 1);
        }
        assert_eq!(f.fixable, !f.edits.is_empty());
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for e in &f.edits {
            assert_eq!(e.file, "src/x.rs");
            assert!(e.start <= e.end && e.end <= text.len(), "{e:?} in {} bytes", text.len());
            assert!(text.is_char_boundary(e.start) && text.is_char_boundary(e.end), "{e:?}");
            assert!(
                spans.iter().all(|&(s, en)| e.end <= s || e.start >= en),
                "edits of one finding overlap: {:?}",
                f.edits
            );
            spans.push((e.start, e.end));
        }
    }
});
