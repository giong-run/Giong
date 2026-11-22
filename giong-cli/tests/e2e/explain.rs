//! `giong explain`: every page in docs/errors, offline, in every spelling of
//! its code, and the same text the repository holds.

use std::path::Path;

use crate::harness::giong;

fn pages() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/errors");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let code = name.strip_suffix(".md")?;
            (code.len() == 5 && code.starts_with('E'))
                .then(|| (code.to_string(), std::fs::read_to_string(e.path()).unwrap()))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_page_in_docs_errors_is_served_offline_and_matches() {
    let pages = pages();
    assert!(pages.len() > 50, "only {} pages found", pages.len());
    for (code, text) in &pages {
        let out = giong(["explain", code.as_str(), "--json"])
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .run();
        let v = out.report("explain");
        assert_eq!(v["data"]["code"], format!("GIONG-{code}"));
        assert_eq!(
            v["data"]["page"].as_str().unwrap(),
            text,
            "{code}: the embedded page differs from docs/errors/{code}.md"
        );
        assert!(text.starts_with(&format!("# GIONG-{code}")), "{code}");
    }
}

#[test]
fn spellings_of_a_code_agree() {
    let want = giong(["explain", "GIONG-E0001", "--json"])
        .run()
        .report("explain");
    for w in ["E0001", "e0001", "0001", "1", "giong-e0001", "GIONG-E0001:"] {
        let v = giong(["explain", w, "--json"]).run().report("explain");
        assert_eq!(v["data"], want["data"], "{w}");
    }
}

#[test]
fn bad_and_unknown_codes_are_coded_failures() {
    for w in [
        "",
        "E",
        "E99999",
        "x12",
        "GIONG-",
        "-1",
        "E00 01",
        "١٢٣",
        "E0001E0002",
    ] {
        let out = giong(["explain", w, "--json"]).run();
        let v = out.report("explain");
        let code = v["error"]["code"].as_str().unwrap_or("");
        assert!(
            code == "GIONG-E5005" || code == "GIONG-E5000",
            "giong explain {w:?}: {}",
            out.dump()
        );
    }
    let out = giong(["explain", "E9999", "--json"]).run();
    let v = out.report("explain");
    assert_eq!(v["error"]["code"], "GIONG-E5005");
    let out = giong(["explain", "--json"]).run();
    assert_eq!(out.report("explain")["error"]["code"], "GIONG-E5000");
}

#[test]
fn text_output_is_the_page_and_its_link() {
    let out = giong(["explain", "E5000"]).run();
    out.expect_code(0);
    assert!(out.stdout.starts_with("# GIONG-E5000"), "{}", out.dump());
    assert!(
        out.stdout.contains("/docs/errors/E5000.md"),
        "{}",
        out.dump()
    );
}
