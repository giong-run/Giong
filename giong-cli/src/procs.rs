//! `.giong/processes.json`: what giong started for a project besides
//! `giong dev` itself (a database container from `giong db up`, ...), so that
//! `giong dev status` lists it and `giong dev stop` stops it. Nothing here
//! starts or stops anything; the commands that do call [`add`] and
//! [`remove`].
//!
//! ```json
//! {"processes": [{"kind": "db", "name": "giong-shop-postgres-3fa2c1",
//!   "engine": "docker", "started_by": "giong db up", "started_at": 1764000000000}]}
//! ```

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::state;

/// One thing giong started for the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it is: `db` (a container).
    pub kind: String,
    /// How to find it: the container's name.
    pub name: String,
    /// What runs it: `docker` or `podman` for a container.
    pub engine: String,
    /// The command that started it, for people: `giong db up`.
    pub started_by: String,
    /// Milliseconds since 1970.
    pub started_at: u64,
}

impl Entry {
    /// Whether giong may act on it. The file is in the project folder, which
    /// a cloned repository can fill: only `docker` or `podman` (by name,
    /// looked up on PATH, never a path) and a container name that cannot be
    /// read as an option are run.
    pub fn is_safe(&self) -> bool {
        matches!(self.engine.as_str(), "docker" | "podman")
            && self
                .name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    }
}

fn path(root: &Path) -> PathBuf {
    root.join(".giong").join("processes.json")
}

/// The entries, oldest first; none when the file is missing or unreadable.
pub fn list(root: &Path) -> Vec<Entry> {
    let Ok(text) = state::read_to_string(&path(root)) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    doc["processes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Entry {
                kind: p["kind"].as_str()?.to_string(),
                name: p["name"].as_str()?.to_string(),
                engine: p["engine"].as_str().unwrap_or("").to_string(),
                started_by: p["started_by"].as_str().unwrap_or("").to_string(),
                started_at: p["started_at"].as_u64().unwrap_or(0),
            })
        })
        .collect()
}

/// Records `entry`, replacing one of the same kind and name.
#[allow(dead_code)] // called by `giong db up`
pub fn add(root: &Path, entry: Entry) -> std::io::Result<()> {
    let mut all = list(root);
    all.retain(|e| !(e.kind == entry.kind && e.name == entry.name));
    all.push(entry);
    save(root, &all)
}

/// Forgets the entry of this kind and name, if there is one.
pub fn remove(root: &Path, kind: &str, name: &str) -> std::io::Result<()> {
    let mut all = list(root);
    let before = all.len();
    all.retain(|e| !(e.kind == kind && e.name == name));
    if all.len() == before {
        return Ok(());
    }
    save(root, &all)
}

/// As JSON, for `giong dev status`.
pub fn to_json(e: &Entry) -> Value {
    json!({"kind": e.kind, "name": e.name, "engine": e.engine,
           "started_by": e.started_by, "started_at": e.started_at})
}

/// Written aside, then renamed: a reader never sees half a file.
fn save(root: &Path, all: &[Entry]) -> std::io::Result<()> {
    state::ensure(root)?;
    let doc = json!({"processes": all.iter().map(to_json).collect::<Vec<_>>()});
    state::replace(&path(root), format!("{doc:#}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn entries_are_added_replaced_and_removed() {
        let root = std::env::temp_dir().join(format!("giong-procs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(list(&root).is_empty());
        let db = |name: &str, at| Entry {
            kind: "db".into(),
            name: name.into(),
            engine: "docker".into(),
            started_by: "giong db up".into(),
            started_at: at,
        };
        add(&root, db("a", 1)).unwrap();
        add(&root, db("b", 2)).unwrap();
        add(&root, db("a", 3)).unwrap();
        assert_eq!(list(&root), [db("b", 2), db("a", 3)]);
        remove(&root, "db", "b").unwrap();
        remove(&root, "db", "nope").unwrap();
        assert_eq!(list(&root), [db("a", 3)]);
        assert!(db("giong-shop-postgres-3fa2c1", 0).is_safe());
        for (engine, name) in [
            ("./x.sh", "a"),
            ("/usr/bin/docker", "a"),
            ("docker", "-v"),
            ("docker", "a;b"),
            ("docker", ""),
            ("sh", "a"),
        ] {
            let e = Entry {
                engine: engine.into(),
                ..db(name, 0)
            };
            assert!(!e.is_safe(), "{engine} {name}");
        }
        fs::write(path(&root), "not json").unwrap();
        assert!(list(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
