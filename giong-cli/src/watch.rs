//! Watching a project for changes, by looking at modification times.
//!
//! Polling keeps the command free of platform code and of `unsafe`, and is
//! cheap enough: a stat per file every few hundred milliseconds, a few
//! milliseconds for a few thousand files. Build output, version control and
//! dependency folders are skipped, as is anything under a directory cargo
//! marked as its own.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory names never watched.
pub const SKIP: &[&str] = &[
    "target", ".git", ".hg", ".svn", ".giong", "node_modules", ".idea", ".vscode", ".direnv",
];

/// The most files watched; past it the rest are ignored (and said so).
pub const MAX_FILES: usize = 50_000;

/// What a file looked like.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// Every watched file and its stamp.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<PathBuf, Stamp>,
    /// More files than [`MAX_FILES`] were found.
    pub truncated: bool,
}

/// What kind of change a set of paths is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Only `.env`: restart without building.
    Env,
    /// Anything else: build, then restart.
    Code,
}

impl Snapshot {
    /// Looks at every file under `roots` (files or directories).
    pub fn take(roots: &[PathBuf]) -> Snapshot {
        let mut snap = Snapshot::default();
        for root in roots {
            snap.walk(root);
        }
        snap
    }

    fn walk(&mut self, path: &Path) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP.contains(&name) || path.join("CACHEDIR.TAG").exists() {
                return;
            }
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            children.sort();
            for c in children {
                if self.truncated {
                    return;
                }
                self.walk(&c);
            }
        } else if meta.is_file() {
            if self.files.len() >= MAX_FILES {
                self.truncated = true;
                return;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Editors' swap and backup files.
            if name.ends_with('~')
                || name.ends_with(".swp")
                || name.ends_with(".swx")
                || name.starts_with(".#")
                || name == "4913"
            {
                return;
            }
            self.files.insert(
                path.to_path_buf(),
                Stamp {
                    modified: meta.modified().ok(),
                    len: meta.len(),
                },
            );
        }
    }

    /// Files added, removed or changed since `before`.
    pub fn changed_since(&self, before: &Snapshot) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (path, stamp) in &self.files {
            if before.files.get(path) != Some(stamp) {
                out.push(path.clone());
            }
        }
        for path in before.files.keys() {
            if !self.files.contains_key(path) {
                out.push(path.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// `.env` alone needs a restart; anything else a build.
pub fn classify(changed: &[PathBuf]) -> Option<Change> {
    if changed.is_empty() {
        return None;
    }
    let env_only = changed
        .iter()
        .all(|p| p.file_name().is_some_and(|n| n == ".env"));
    Some(if env_only { Change::Env } else { Change::Code })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_seen_and_noise_is_not() {
        let d = std::env::temp_dir().join(format!("giong-cli-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("target/debug")).unwrap();
        fs::create_dir_all(d.join("nested-target")).unwrap();
        fs::write(d.join("nested-target/CACHEDIR.TAG"), "x").unwrap();
        fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(d.join(".env"), "A=1").unwrap();
        let a = Snapshot::take(&[d.clone()]);
        assert_eq!(a.files.len(), 2, "{:?}", a.files.keys());
        // Build output and editor files change nothing.
        fs::write(d.join("target/debug/app"), "bin").unwrap();
        fs::write(d.join("src/.main.rs.swp"), "swap").unwrap();
        fs::write(d.join("src/main.rs~"), "backup").unwrap();
        assert!(Snapshot::take(&[d.clone()]).changed_since(&a).is_empty());
        // A different length is a change even within one time tick.
        fs::write(d.join(".env"), "A=12").unwrap();
        let b = Snapshot::take(&[d.clone()]);
        let changed = b.changed_since(&a);
        assert_eq!(changed, [d.join(".env")]);
        assert_eq!(classify(&changed), Some(Change::Env));
        // New and removed files.
        fs::write(d.join("src/lib.rs"), "").unwrap();
        fs::remove_file(d.join("src/main.rs")).unwrap();
        let c = Snapshot::take(&[d.clone()]);
        let changed = c.changed_since(&b);
        assert_eq!(changed, [d.join("src/lib.rs"), d.join("src/main.rs")]);
        assert_eq!(classify(&changed), Some(Change::Code));
        assert_eq!(classify(&[]), None);
        fs::remove_dir_all(&d).unwrap();
    }
}
