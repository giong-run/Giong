//! `giong check`: compiler, clippy and giong's own source checks in one
//! report. The edits it offers are applied by `giong fix` and by agents
//! directly, so a byte offset that is off by one corrupts someone's code:
//! the tests here apply them and build again.

use std::path::Path;

use serde_json::Value;

use crate::harness::*;

const SOURCE: &str = r#"use std::cell::RefCell;
use std::time::Duration;

async fn tick() {}

async fn blocking() -> String {
    std::thread::sleep(Duration::from_millis(1));
    std::fs::read_to_string("x").unwrap_or_default()
}

fn fine_sync() {
    // std::thread::sleep(Duration::from_millis(1)) in a comment
    let _s = "std::thread::sleep in a string";
    std::thread::sleep(Duration::from_millis(1));
}

async fn borrow(c: &RefCell<u32>) {
    let g = c.borrow_mut();
    tick().await;
    drop(g);
}

async fn bind() {
    let _ = std::net::TcpListener::bind("localhost:8080");
}

fn main() {
    let v = vec![1, 2, 3];
    let _n = v.iter().count();
    fine_sync();
    let _ = (blocking(), borrow(&RefCell::new(0)), bind());
}
"#;

fn project(tag: &str, main: &str) -> TempDir {
    let dir = TempDir::new(tag);
    new_package(&dir, "chk", "");
    std::fs::write(dir.join("src/main.rs"), main).unwrap();
    dir
}

fn check(dir: &Path, extra: &[&str]) -> (Out, Value) {
    let mut run = giong(["check"]);
    for a in extra {
        run = run.arg(*a);
    }
    let out = run
        .arg("--json")
        .dir(dir)
        .env("CARGO_TERM_COLOR", "never")
        .timeout(std::time::Duration::from_secs(300))
        .run();
    let v = out.report("check");
    (out, v)
}

fn findings(v: &Value) -> &Vec<Value> {
    v["findings"].as_array().unwrap()
}

fn lines_of(v: &Value, code: &str) -> Vec<u64> {
    let mut l: Vec<u64> = findings(v)
        .iter()
        .filter(|f| f["code"] == code)
        .map(|f| f["line"].as_u64().expect("a check finding has a line"))
        .collect();
    l.sort();
    l
}

#[test]
fn giongs_checks_find_what_blocks_a_worker_and_nothing_in_comments_or_sync_code() {
    if !crate::harness::toolchain(
        "giongs_checks_find_what_blocks_a_worker_and_nothing_in_comments_or_sync_code",
    ) {
        return;
    }
    if !has_command("check") {
        return;
    }
    let dir = project("check-giong", SOURCE);
    let (out, v) = check(dir.path(), &["--no-clippy"]);
    // Line 7 sleep and line 8 std::fs, in the async fn; not 12-14.
    assert_eq!(lines_of(&v, "GIONG-E5006"), vec![7, 8], "{}", out.dump());
    assert_eq!(lines_of(&v, "GIONG-E5008"), vec![18], "{}", out.dump());
    assert_eq!(lines_of(&v, "GIONG-E5007"), vec![24], "{}", out.dump());
    // "/" on every system, Windows too, for giong's checks and rustc's.
    for f in findings(&v) {
        assert_eq!(f["file"], "src/main.rs", "{f:#}");
    }
}

/// Every edit, applied to the file, replaces exactly the text it names, and
/// the project still builds with the edits of each finding applied.
#[test]
fn edits_replace_the_right_bytes_and_the_result_builds() {
    if !crate::harness::toolchain("edits_replace_the_right_bytes_and_the_result_builds") {
        return;
    }
    if !has_command("check") {
        return;
    }
    // Multi-byte text and CRLF line ends before the edits: offsets are bytes
    // of the file as it is on disk.
    let crlf = format!("// Gióng “quotes” é ё\n{SOURCE}").replace('\n', "\r\n");
    for (tag, text) in [
        ("check-edits-lf", SOURCE.to_string()),
        ("check-edits-crlf", crlf),
    ] {
        let dir = project(tag, &text);
        let (out, v) = check(dir.path(), &[]);
        let bytes = std::fs::read(dir.join("src/main.rs")).unwrap();
        let fixable: Vec<&Value> = findings(&v)
            .iter()
            .filter(|f| f["fixable"] == true)
            .collect();
        assert!(
            fixable.iter().any(|f| f["code"] == "GIONG-E5007"),
            "{tag}: the host-name bind has no edit\n{}",
            out.dump()
        );
        for f in &fixable {
            for e in f["edits"].as_array().unwrap() {
                let (s, en) = (
                    e["start_byte"].as_u64().unwrap() as usize,
                    e["end_byte"].as_u64().unwrap() as usize,
                );
                assert!(en <= bytes.len(), "{tag}: edit past the end: {f:#}");
                let old = std::str::from_utf8(&bytes[s..en])
                    .unwrap_or_else(|_| panic!("{tag}: edit splits a character: {f:#}"));
                assert!(
                    !old.contains('\r'),
                    "{tag}: an edit swallows a line end: {f:#}"
                );
                if f["code"] == "GIONG-E5007" {
                    assert_eq!(old, "localhost", "{tag}: {f:#}");
                }
            }
            // This finding's edits alone, applied from the end.
            let mut edits: Vec<&Value> = f["edits"].as_array().unwrap().iter().collect();
            edits.sort_by_key(|e| std::cmp::Reverse(e["start_byte"].as_u64().unwrap()));
            let mut fixed = bytes.clone();
            for e in edits {
                let (s, en) = (
                    e["start_byte"].as_u64().unwrap() as usize,
                    e["end_byte"].as_u64().unwrap() as usize,
                );
                fixed.splice(s..en, e["replacement"].as_str().unwrap().bytes());
            }
            std::fs::write(dir.join("src/main.rs"), &fixed).unwrap();
            let build =
                std::process::Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()))
                    .args(["check", "--offline", "--quiet"])
                    .current_dir(dir.path())
                    .env_remove("CARGO_TARGET_DIR")
                    .env_remove("RUSTFLAGS")
                    .output()
                    .unwrap();
            assert!(
                build.status.success(),
                "{tag}: {} applied breaks the build:\n{}",
                f["title"],
                String::from_utf8_lossy(&build.stderr)
            );
            std::fs::write(dir.join("src/main.rs"), &bytes).unwrap();
        }
    }
}

#[test]
fn a_compile_error_is_an_error_at_its_place() {
    if !crate::harness::toolchain("a_compile_error_is_an_error_at_its_place") {
        return;
    }
    if !has_command("check") {
        return;
    }
    let dir = project("check-broken", "fn main() {\n    let x: u32 = \"no\";\n}\n");
    let (out, v) = check(dir.path(), &[]);
    out.expect_code(1);
    let e = findings(&v)
        .iter()
        .find(|f| f["code"] == "GIONG-E5009")
        .unwrap_or_else(|| panic!("no E5009\n{}", out.dump()));
    assert_eq!(e["severity"], "error");
    assert_eq!(e["file"], "src/main.rs", "{e:#}");
    assert_eq!(e["line"], 2);
}

#[test]
fn a_clean_project_is_ok() {
    if !crate::harness::toolchain("a_clean_project_is_ok") {
        return;
    }
    if !has_command("check") {
        return;
    }
    let dir = project("check-clean", "fn main() {\n    println!(\"hello\");\n}\n");
    let (out, v) = check(dir.path(), &[]);
    out.expect_code(0);
    assert!(findings(&v).is_empty(), "{}", out.dump());
}

/// One problem, one finding: clippy's `await_holding_refcell_ref` and
/// giong's E5008 describe the same borrow.
#[test]
fn one_problem_is_reported_once() {
    if !crate::harness::toolchain("one_problem_is_reported_once") {
        return;
    }
    if !has_command("check") {
        return;
    }
    let dir = project("check-dup", SOURCE);
    let (out, v) = check(dir.path(), &[]);
    let at_18: Vec<&Value> = findings(&v).iter().filter(|f| f["line"] == 18).collect();
    assert_eq!(at_18.len(), 1, "{at_18:#?}\n{}", out.dump());
}

/// A file in the middle of an edit (unbalanced braces, half a statement) is
/// what `giong check` meets most often while someone types: it reports what
/// the compiler says and never panics. Found by the `lint` fuzz target.
#[test]
fn a_half_typed_file_does_not_panic() {
    if !crate::harness::toolchain("a_half_typed_file_does_not_panic") {
        return;
    }
    if !has_command("check") {
        return;
    }
    let dir = project(
        "check-half-typed",
        "u:D;\n\nasync frig {\n g{\n let _n = vln((Durat8`)dy\n}\n\nac.borrow_mut();OG\n/",
    );
    let (out, v) = check(dir.path(), &["--no-clippy"]);
    assert!(!out.stderr.contains("panicked"), "{}", out.dump());
    assert_eq!(v["ok"], false, "{}", out.dump());
}
