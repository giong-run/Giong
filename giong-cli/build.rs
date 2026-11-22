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

fn main() {
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
