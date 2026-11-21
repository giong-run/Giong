//! Every giong-proto error code has its page under docs/errors/, titled with
//! the code and its summary, naming its `ErrorKind`, and listed in the index.

use std::fs;
use std::path::Path;

use giong_proto::ErrorKind;

#[test]
fn every_code_has_its_page() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/errors");
    if !dir.exists() {
        return;
    }
    let index = fs::read_to_string(dir.join("README.md")).unwrap();
    for &kind in ErrorKind::ALL {
        let file = format!("E{:04}.md", kind.number());
        let page = fs::read_to_string(dir.join(&file))
            .unwrap_or_else(|e| panic!("no page docs/errors/{file}: {e}"));
        let title = format!("# GIONG-E{:04}: {}", kind.number(), kind.summary());
        assert_eq!(
            page.lines().next(),
            Some(title.as_str()),
            "docs/errors/{file}"
        );
        assert!(
            page.contains(&format!("`giong_proto::ErrorKind::{kind:?}`")),
            "docs/errors/{file} does not name ErrorKind::{kind:?}"
        );
        let row = format!(
            "| [GIONG-E{:04}]({file}) | `ErrorKind::{kind:?}` | {} |",
            kind.number(),
            kind.summary()
        );
        assert!(index.contains(&row), "docs/errors/README.md lacks {row}");
    }
}

#[test]
fn messages_name_code_and_page() {
    let err = giong_proto::wire::Reader::new(&[0x80])
        .varint()
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("GIONG-E4001: the message ends in the middle of a field"),
        "{msg}"
    );
    assert!(msg.contains("Fix: "), "{msg}");
    assert!(msg.ends_with("docs/errors/E4001.md"), "{msg}");
    assert_eq!(err.code(), "GIONG-E4001");
}
