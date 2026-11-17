//! Every public async fn of the workspace's crates says in its documentation
//! whether it is cancel safe: a paragraph that starts with "Cancel safe" or
//! "Not cancel safe" (see docs/STABILITY.md, which counts it as part of the
//! API). Code written against the crates, by people or by agents, relies on
//! it when it puts a timeout or a `select` around a call.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_public_async_fn_says_whether_it_is_cancel_safe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !root.join("src").is_dir() {
        // The kernel jobs run the built test in a VM without the sources.
        return;
    }
    let mut dirs = vec![root.join("src")];
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("giong-") && path.join("src").is_dir() {
            dirs.push(path.join("src"));
        }
    }
    let mut files = Vec::new();
    for dir in &dirs {
        rust_files(dir, &mut files);
    }
    let mut missing = Vec::new();
    let mut seen = 0;
    for file in &files {
        let text = fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.trim_start().starts_with("pub async fn ") {
                continue;
            }
            // The doc comment and attributes right above the function.
            let mut doc = Vec::new();
            for above in lines[..i].iter().rev() {
                let above = above.trim_start();
                if let Some(text) = above.strip_prefix("///") {
                    doc.push(text.trim());
                } else if !above.starts_with("#[") {
                    break;
                } else if above.contains("doc(hidden)") {
                    doc.push("Cancel safe (hidden)");
                }
            }
            seen += 1;
            let says = doc
                .iter()
                .any(|l| l.starts_with("Cancel safe") || l.starts_with("Not cancel safe"));
            if !says {
                let at = file.strip_prefix(root).unwrap_or(file).display();
                missing.push(format!("{at}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    if seen == 0 {
        // The published crate has no sibling crates and may lay out its
        // sources differently; nothing to check.
        return;
    }
    assert!(
        missing.is_empty(),
        "{} public async fns do not say whether they are cancel safe (start a paragraph of \
         their documentation with \"Cancel safe\" or \"Not cancel safe\", and say what a \
         dropped future leaves behind):\n{}",
        missing.len(),
        missing.join("\n")
    );
}
