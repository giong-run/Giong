//! Watching a project for changes, by looking at modification times.
//!
//! Polling keeps the command free of platform code and of `unsafe`, and is
//! cheap enough: a stat per file every few hundred milliseconds, a few
//! milliseconds for a few thousand files. Build output, version control and
//! dependency folders are skipped, as is anything under a directory cargo
//! marked as its own, what the project's `.gitignore` ignores (but `.env`),
//! and SQLite databases, which a running service writes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory names never watched.
pub const SKIP: &[&str] = &[
    "target",
    ".git",
    ".hg",
    ".svn",
    ".giong",
    "node_modules",
    ".idea",
    ".vscode",
    ".direnv",
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
    /// Looks at every file under `roots` (files or directories). Under a
    /// directory root, what its `.gitignore` ignores is not looked at: the
    /// file is read again each time, so an edit to it counts at once.
    pub fn take(roots: &[PathBuf]) -> Snapshot {
        let mut snap = Snapshot::default();
        for root in roots {
            let ignore = Ignore::read(root);
            snap.walk(root, root, &ignore);
        }
        snap
    }

    fn walk(&mut self, root: &Path, path: &Path, ignore: &Ignore) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        let rel = path
            .strip_prefix(root)
            .ok()
            .filter(|r| !r.as_os_str().is_empty());
        if meta.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP.contains(&name) || path.join("CACHEDIR.TAG").exists() {
                return;
            }
            if rel.is_some_and(|r| ignore.ignores(r, true)) {
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
                self.walk(root, &c, ignore);
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
            // A service's own data (#514): its SQLite database changes
            // while it runs, and a restart for that would write it again.
            if is_sqlite(name) {
                return;
            }
            // `.env` is ignored by git but restarts the service on purpose.
            if name != ".env" && rel.is_some_and(|r| ignore.ignores(r, false)) {
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

/// A SQLite database or one of the files SQLite keeps beside it.
fn is_sqlite(name: &str) -> bool {
    let base = ["-wal", "-shm", "-journal"]
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .unwrap_or(name);
    [".db", ".sqlite", ".sqlite3"]
        .iter()
        .any(|ext| base.len() > ext.len() && base.ends_with(ext))
}

/// The patterns of a project's top-level `.gitignore`, in the parts of git's
/// syntax projects use: `#` comments, `!` to take a path back, a trailing `/`
/// for directories only, a leading or inner `/` to anchor a pattern at the
/// root (else it matches a name at any depth), and `*`, `?`, `**` and `[...]`.
/// The last pattern that matches decides, and nothing under an ignored
/// directory is looked at, as in git. `.gitignore` files further down are not
/// read.
#[derive(Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    glob: String,
    negated: bool,
    dir_only: bool,
    anchored: bool,
}

impl Ignore {
    /// The patterns in `root/.gitignore`; none when it cannot be read.
    pub fn read(root: &Path) -> Ignore {
        match fs::read_to_string(root.join(".gitignore")) {
            Ok(text) => Ignore::parse(&text),
            Err(_) => Ignore::default(),
        }
    }

    pub fn parse(text: &str) -> Ignore {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches([' ', '\t', '\r']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line.strip_prefix('\\').unwrap_or(line)),
            };
            let (dir_only, line) = match line.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let anchored = line.contains('/');
            let glob = line.strip_prefix('/').unwrap_or(line);
            if glob.is_empty() {
                continue;
            }
            rules.push(Rule {
                glob: glob.to_string(),
                negated,
                dir_only,
                anchored,
            });
        }
        Ignore { rules }
    }

    /// Whether `rel`, a path relative to the root, is ignored.
    pub fn ignores(&self, rel: &Path, is_dir: bool) -> bool {
        let Some(rel) = rel.to_str() else {
            return false;
        };
        // Windows separates components with `\`; elsewhere it is part of a name.
        let rel = if cfg!(windows) {
            rel.replace('\\', "/")
        } else {
            rel.to_string()
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let hit = if rule.anchored {
                glob(rule.glob.as_bytes(), rel.as_bytes())
            } else {
                glob(rule.glob.as_bytes(), name.as_bytes())
            };
            if hit {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

/// Matches `text` against a gitignore glob: `*` and `?` stop at `/`, `**`
/// as a whole component crosses any number of them.
fn glob(p: &[u8], t: &[u8]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            // `**/x` also matches `x` at this level; `a/**` everything below.
            let rest_after_slash = rest.strip_prefix(b"/").unwrap_or(rest);
            if rest.is_empty() {
                return true;
            }
            (0..=t.len()).any(|i| (i == 0 || t[i - 1] == b'/') && glob(rest_after_slash, &t[i..]))
        }
        Some(b'*') => (0..=t.len())
            .take_while(|&i| i == 0 || t[i - 1] != b'/')
            .any(|i| glob(&p[1..], &t[i..])),
        Some(b'?') => t.first().is_some_and(|&c| c != b'/') && glob(&p[1..], &t[1..]),
        Some(b'[') => {
            let Some(end) = p.iter().skip(2).position(|&c| c == b']').map(|i| i + 2) else {
                return t.first() == Some(&b'[') && glob(&p[1..], &t[1..]);
            };
            let Some(&c) = t.first() else { return false };
            if c == b'/' {
                return false;
            }
            let (negate, set) = match p[1] {
                b'!' | b'^' => (true, &p[2..end]),
                _ => (false, &p[1..end]),
            };
            let mut found = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    found |= set[i] <= c && c <= set[i + 2];
                    i += 3;
                } else {
                    found |= set[i] == c;
                    i += 1;
                }
            }
            found != negate && glob(&p[end + 1..], &t[1..])
        }
        Some(&c) => t.first() == Some(&c) && glob(&p[1..], &t[1..]),
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
        let a = Snapshot::take(std::slice::from_ref(&d));
        assert_eq!(a.files.len(), 2, "{:?}", a.files.keys());
        // Build output and editor files change nothing.
        fs::write(d.join("target/debug/app"), "bin").unwrap();
        fs::write(d.join("src/.main.rs.swp"), "swap").unwrap();
        fs::write(d.join("src/main.rs~"), "backup").unwrap();
        assert!(Snapshot::take(std::slice::from_ref(&d))
            .changed_since(&a)
            .is_empty());
        // A different length is a change even within one time tick.
        fs::write(d.join(".env"), "A=12").unwrap();
        let b = Snapshot::take(std::slice::from_ref(&d));
        let changed = b.changed_since(&a);
        assert_eq!(changed, [d.join(".env")]);
        assert_eq!(classify(&changed), Some(Change::Env));
        // New and removed files.
        fs::write(d.join("src/lib.rs"), "").unwrap();
        fs::remove_file(d.join("src/main.rs")).unwrap();
        let c = Snapshot::take(std::slice::from_ref(&d));
        let changed = c.changed_since(&b);
        assert_eq!(changed, [d.join("src/lib.rs"), d.join("src/main.rs")]);
        assert_eq!(classify(&changed), Some(Change::Code));
        assert_eq!(classify(&[]), None);
        fs::remove_dir_all(&d).unwrap();
    }

    /// #514: in a project from `giong new` (SQLite), the service writes
    /// `data/app.db-wal` and `data/app.db-shm` while it runs; `giong dev`
    /// took that for a change and restarted it, which wrote them again.
    #[test]
    fn a_running_services_database_and_gitignored_paths_are_not_changes() {
        let d = std::env::temp_dir().join(format!("giong-cli-watch-ignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("data")).unwrap();
        fs::create_dir_all(d.join("logs")).unwrap();
        // The template's .gitignore.
        fs::write(
            d.join(".gitignore"),
            "/target\n/.giong\n.env\n/data\n*.log\n",
        )
        .unwrap();
        fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(d.join(".env"), "A=1").unwrap();
        let a = Snapshot::take(std::slice::from_ref(&d));
        let mut seen: Vec<_> = a
            .files
            .keys()
            .map(|p| p.strip_prefix(&d).unwrap().to_path_buf())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                PathBuf::from(".env"),
                ".gitignore".into(),
                "src/main.rs".into()
            ]
        );
        for f in [
            "data/app.db",
            "data/app.db-wal",
            "data/app.db-shm",
            "logs/today.log",
        ] {
            fs::write(d.join(f), "x").unwrap();
        }
        // SQLite files outside an ignored directory, in a project whose
        // .gitignore does not name them.
        fs::write(d.join("local.sqlite3"), "x").unwrap();
        fs::write(d.join("local.sqlite3-journal"), "x").unwrap();
        assert_eq!(
            Snapshot::take(std::slice::from_ref(&d)).changed_since(&a),
            Vec::<PathBuf>::new()
        );
        // .env still counts, though git ignores it.
        fs::write(d.join(".env"), "A=12").unwrap();
        assert_eq!(
            Snapshot::take(std::slice::from_ref(&d)).changed_since(&a),
            [d.join(".env")]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn gitignore_patterns_match_as_git_does() {
        let ig = Ignore::parse(
            "# comment\n\n/target\n*.log\nbuild/\ndocs/out\n**/cache\nvendor/**\n!keep.log\n\\#hash\ntmp[0-9]\n",
        );
        let yes = |p: &str, dir: bool| ig.ignores(Path::new(p), dir);
        assert!(yes("target", true));
        assert!(!yes("src/target", true), "anchored at the root");
        assert!(yes("a.log", false) && yes("src/deep/b.log", false));
        assert!(!yes("keep.log", false), "taken back by !");
        assert!(yes("build", true) && yes("src/build", true));
        assert!(!yes("build", false), "a trailing / is for directories");
        assert!(yes("docs/out", true) && !yes("x/docs/out", true));
        assert!(yes("cache", true) && yes("a/b/cache", true));
        assert!(yes("vendor/x", false) && yes("vendor/x/y.rs", false) && !yes("vendor", true));
        assert!(yes("#hash", false));
        assert!(yes("tmp3", false) && !yes("tmpx", false));
        assert!(!yes("src/main.rs", false));
        assert!(is_sqlite("shop.db") && is_sqlite("shop.db-wal") && is_sqlite("x.sqlite-shm"));
        assert!(!is_sqlite(".db") && !is_sqlite("db.rs") && !is_sqlite("schema.sql"));
    }
}
