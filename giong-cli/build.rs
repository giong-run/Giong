//! Embeds `templates/` (the projects `giong new` writes) in the binary, so the
//! command needs no network and every file matches the release it comes with.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The minor version of the compiler building this (`rustc 1.89.0` -> 89).
fn rustc_minor() -> Option<u32> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.split_whitespace()
        .nth(1)?
        .split('.')
        .nth(1)?
        .parse()
        .ok()
}

fn main() {
    // File locks (std, Rust 1.89) guard the build store (src/store.rs); a
    // giong built with an older compiler has the store turned off.
    println!("cargo::rustc-check-cfg=cfg(giong_file_lock)");
    if rustc_minor().is_some_and(|m| m >= 89) {
        println!("cargo::rustc-cfg=giong_file_lock");
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut code = String::from("/// Every template file: (path under templates/, text).\npub const FILES: &[(&str, &str)] = &[\n");
    for file in &files {
        let rel = file.strip_prefix(&root).unwrap();
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_str().expect("template paths are UTF-8"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(
            rel.ends_with(".tmpl"),
            "templates/{rel}: every template file ends with .tmpl"
        );
        writeln!(
            code,
            "    ({rel:?}, include_str!({:?})),",
            file.display().to_string()
        )
        .unwrap();
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("templates.rs");
    std::fs::write(out, code).unwrap();
}
