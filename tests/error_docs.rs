//! Every error code has its page under docs/errors/, titled with the code and
//! its summary, and listed in the index; no page is left without a code.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use giong::error::Code;

#[test]
fn every_code_has_its_page() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/errors");
    if !dir.exists() {
        // The published crate leaves docs/ out.
        return;
    }
    let index = fs::read_to_string(dir.join("README.md")).unwrap();
    let mut pages = HashSet::new();
    for &code in Code::ALL {
        let file = format!("E{:04}.md", code.number());
        let page = fs::read_to_string(dir.join(&file))
            .unwrap_or_else(|e| panic!("{code} has no page docs/errors/{file}: {e}"));
        let title = format!("# {code}: {}", code.summary());
        assert_eq!(
            page.lines().next(),
            Some(title.as_str()),
            "docs/errors/{file}"
        );
        assert!(
            page.contains(&format!("`giong::error::Code::{code:?}`")),
            "docs/errors/{file} does not name Code::{code:?}"
        );
        let row = format!("| [{code}]({file}) | `{code:?}` | {} |", code.summary());
        assert!(index.contains(&row), "docs/errors/README.md lacks {row}");
        assert!(code.url().ends_with(&format!("/docs/errors/{file}")));
        pages.insert(file);
    }
    for entry in fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            name == "README.md" || pages.contains(&name),
            "docs/errors/{name} belongs to no code"
        );
    }
}
