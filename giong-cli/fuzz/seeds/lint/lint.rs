//! giong's own checks of a project's source, the ones the compiler cannot
//! make because they are about running on a giong worker: a blocking call in
//! an `async` function (`GIONG-E5006`), a listener bound to a host name
//! (`GIONG-E5007`), a `RefCell` borrow kept across an `.await`
//! (`GIONG-E5008`).
//!
//! They read the source, not the types, so they are heuristics and say so in
//! their fix. Comments and the insides of strings are masked first, so a
//! commented-out call or a word in a message is never reported.

use std::fs;
use std::path::{Path, PathBuf};

use crate::code::Code;
use crate::report::{Edit, Finding, Severity};
use crate::watch::SKIP;

/// The source files of the packages under `dirs` (each package's directory),
/// not descending into build output or into another package.
pub fn source_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            // Tests and benchmarks block on purpose (a std client against
            // the server under test): not reported.
            if SKIP.contains(&name.as_ref())
                || name.starts_with('.')
                || name == "tests"
                || name == "benches"
                || name == "fuzz"
                || path.join("CACHEDIR.TAG").exists()
                || (path != root && path.join("Cargo.toml").is_file())
            {
                continue;
            }
            walk(root, &path, out);
        } else if kind.is_file()
            && name.ends_with(".rs")
            && !name.ends_with("_tests.rs")
            && name != "tests.rs"
        {
            out.push(path);
        }
    }
}

/// Every finding in one file. `shown` is the path to report (relative to the
/// project).
pub fn check_file(text: &str, shown: &str) -> Vec<Finding> {
    let masked = mask(text);
    let m = masked.as_bytes();
    let regions = async_regions(m);
    let mut excluded = blocking_ok_regions(m);
    excluded.extend(test_regions(m));
    let in_async = |at: usize| {
        regions.iter().any(|r| r.0 <= at && at < r.1)
            && !excluded.iter().any(|r| r.0 <= at && at < r.1)
    };
    let mut out = Vec::new();
    let uses_std_fs = masked.contains("use std::fs;") || masked.contains("use std::fs::{");

    // GIONG-E5006: blocking calls.
    let mut blocking: Vec<(&str, &str)> = vec![
        ("std::thread::sleep(", "std::thread::sleep"),
        ("thread::sleep(", "std::thread::sleep"),
        ("std::fs::", "std::fs"),
        (
            "std::net::TcpStream::connect(",
            "std::net::TcpStream::connect",
        ),
        // (Binding an IP address does not block: std's bind is not listed.)
        ("std::process::Command::new(", "std::process::Command"),
        ("reqwest::blocking::", "reqwest::blocking"),
        (".read_line(", "reading a line from standard input"),
    ];
    if uses_std_fs {
        blocking.push(("fs::", "std::fs"));
    }
    let mut seen_at = Vec::new();
    for (pattern, what) in &blocking {
        for at in find_all(&masked, pattern) {
            // `thread::sleep(` also matches inside `std::thread::sleep(`;
            // `fs::` inside `std::fs::` or `giong::fs::`.
            if at > 0 && is_path_char(m[at - 1]) {
                continue;
            }
            if *pattern == "fs::" && preceded_by(m, at, "::") {
                continue;
            }
            if *pattern == ".read_line(" && !masked[line_start(m, at)..at].contains("stdin") {
                continue;
            }
            if !in_async(at) || see