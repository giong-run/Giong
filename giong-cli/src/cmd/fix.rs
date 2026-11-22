//! `giong fix`: applies the fixes that need no judgement: the compiler's and
//! clippy's machine-applicable suggestions, and the edits `giong check` attaches
//! to its own findings. Nothing else: a fix that needs a decision stays a
//! finding of `giong check`.
//!
//! The suggestions are read from `cargo clippy --message-format=json` and
//! applied by this command, all of a suggestion's edits or none, so that
//! `--dry-run` shows exactly what a run would change, overlapping fixes are
//! never both applied, and a file that changed meanwhile is left alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command as Process;

use serde_json::{json, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::project::{cargo, cargo_missing, first_error, Project};
use crate::report::{Edit, Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "fix",
    summary: "apply the fixes that need no judgement (compiler, clippy, giong check)",
    usage: "giong fix [--dry-run] [--allow-dirty] [-- <cargo flags>]\n\n\
            Applies the compiler's and clippy's machine-applicable suggestions and the edits \
            giong check attaches to its findings, then looks again (fixes can reveal more), up \
            to three rounds. Each fix is applied whole or not at all; two fixes that touch the \
            same code are never both applied. --dry-run shows the changes without writing. \
            It refuses to edit a git repository with changes that are not committed, unless \
            --allow-dirty. Flags after -- go to cargo (--features x, -p name).",
    flags: &[
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "show what would change, write nothing",
        },
        Flag {
            name: "allow-dirty",
            kind: Kind::Switch,
            help: "edit even with changes that are not committed",
        },
    ],
    max_words: 0,
    run,
};

/// One fix: edits applied together, or not at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    /// What it fixes: `clippy::len_zero`, `unused_imports`, `GIONG-E5007`.
    pub what: String,
    /// The message, for people.
    pub title: String,
    pub edits: Vec<Edit>,
}

/// What became of a set of fixes.
#[derive(Debug, Default)]
pub struct Applied {
    pub done: Vec<Fix>,
    /// Fixes left out, with the reason.
    pub skipped: Vec<(Fix, String)>,
    /// The new text of each changed file (relative path).
    pub files: BTreeMap<String, String>,
}

/// Applies fixes to the files under `root` (in memory): a fix whose edits
/// overlap an earlier fix, repeat one exactly (dropped quietly), fall outside
/// the file or between the bytes of a character is skipped.
pub fn apply(root: &Path, fixes: Vec<Fix>) -> Applied {
    let mut out = Applied::default();
    let mut originals: BTreeMap<String, Option<String>> = BTreeMap::new();
    // Byte ranges taken so far, per file (in original offsets).
    let mut taken: BTreeMap<String, Vec<(usize, usize, String)>> = BTreeMap::new();
    let mut accepted: Vec<Fix> = Vec::new();
    'fixes: for fix in fixes {
        if fix.edits.is_empty() {
            continue;
        }
        // The same fix twice (lib and test builds both report it): once.
        if accepted.iter().any(|a| a.edits == fix.edits) {
            continue;
        }
        for e in &fix.edits {
            let text = originals
                .entry(e.file.clone())
                .or_insert_with(|| fs::read_to_string(root.join(&e.file)).ok());
            let Some(text) = text else {
                out.skipped
                    .push((fix.clone(), format!("{} cannot be read", e.file)));
                continue 'fixes;
            };
            if e.start > e.end
                || e.end > text.len()
                || !text.is_char_boundary(e.start)
                || !text.is_char_boundary(e.end)
            {
                out.skipped.push((
                    fix.clone(),
                    format!("{} no longer has the code this fix was made for", e.file),
                ));
                continue 'fixes;
            }
            for (s, end, repl) in taken.get(&e.file).into_iter().flatten() {
                let same = *s == e.start && *end == e.end && *repl == e.replacement;
                let overlap = e.start < *end && *s < e.end
                    // Two insertions at one place would land in an order nobody chose.
                    || (e.start == e.end && *s == *end && e.start == *s);
                if overlap && !same {
                    out.skipped.push((
                        fix.clone(),
                        format!("another fix changes the same code in {}", e.file),
                    ));
                    continue 'fixes;
                }
            }
        }
        for e in &fix.edits {
            taken
                .entry(e.file.clone())
                .or_default()
                .push((e.start, e.end, e.replacement.clone()));
        }
        accepted.push(fix);
    }
    // Apply per file from the end, so earlier offsets stay right.
    let mut per_file: BTreeMap<String, Vec<&Edit>> = BTreeMap::new();
    for fix in &accepted {
        for e in &fix.edits {
            let list = per_file.entry(e.file.clone()).or_default();
            if !list.contains(&e) {
                list.push(e);
            }
        }
    }
    for (file, mut edits) in per_file {
        let mut text = originals[&file].clone().expect("checked above");
        edits.sort_by_key(|e| std::cmp::Reverse((e.start, e.end)));
        for e in edits {
            text.replace_range(e.start..e.end, &e.replacement);
        }
        out.files.insert(file, text);
    }
    out.done = accepted;
    out
}

/// The machine-applicable suggestions in cargo's JSON messages, for files of
/// the project (never the registry's or the standard library's). Also returns
/// the first compiler error, if any: suggestions are only taken from code that
/// compiles.
pub fn suggestions(cargo_json: &str, root: &Path) -> (Vec<Fix>, Option<String>) {
    let mut fixes = Vec::new();
    let mut first_err = None;
    for line in cargo_json.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-message" {
            continue;
        }
        let m = &msg["message"];
        if m["level"] == "error" {
            if first_err.is_none() {
                first_err = m["rendered"]
                    .as_str()
                    .or(m["message"].as_str())
                    .map(|s| s.lines().next().unwrap_or("").to_string());
            }
            continue;
        }
        let what = m["code"]["code"].as_str().unwrap_or("").to_string();
        let title = m["message"].as_str().unwrap_or("").to_string();
        // The message's own spans, then each child's, are one suggestion each.
        let groups = std::iter::once(m).chain(m["children"].as_array().into_iter().flatten());
        for g in groups {
            let mut edits = Vec::new();
            let mut whole = true;
            for span in g["spans"].as_array().into_iter().flatten() {
                let Some(replacement) = span["suggested_replacement"].as_str() else {
                    continue;
                };
                if span["suggestion_applicability"] != "MachineApplicable" {
                    whole = false;
                    continue;
                }
                let file = span["file_name"].as_str().unwrap_or("");
                let Some(rel) = project_file(root, file) else {
                    whole = false;
                    continue;
                };
                let (Some(start), Some(end)) =
                    (span["byte_start"].as_u64(), span["byte_end"].as_u64())
                else {
                    whole = false;
                    continue;
                };
                edits.push(Edit {
                    file: rel,
                    start: start as usize,
                    end: end as usize,
                    replacement: replacement.to_string(),
                });
            }
            if whole && !edits.is_empty() {
                fixes.push(Fix {
                    what: what.clone(),
                    title: title.clone(),
                    edits,
                });
            }
        }
    }
    (fixes, first_err)
}

/// `file` (as rustc names it, relative to the workspace or absolute) relative
/// to `root`, if it is inside it and not in its target directory.
fn project_file(root: &Path, file: &str) -> Option<String> {
    let path = Path::new(file);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let full = fs::canonicalize(&full).ok()?;
    let root = fs::canonicalize(root).ok()?;
    let rel = full.strip_prefix(&root).ok()?;
    if rel.starts_with("target") {
        return None;
    }
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// The fixes `giong check` offers: its findings that carry edits.
fn check_fixes(findings: &[Finding]) -> Vec<Fix> {
    findings
        .iter()
        .filter(|f| f.fixable && !f.edits.is_empty())
        .map(|f| Fix {
            what: f.code.to_string(),
            title: f.title.clone(),
            edits: f.edits.clone(),
        })
        .collect()
}

/// giong check's own findings for the project. Until `giong check` lands in
/// this branch it has none.
fn giong_findings(_project: &Project, _cargo_args: &[String]) -> Vec<Finding> {
    Vec::new()
}

/// Whether a git repository around `root` has changes that are not committed.
/// `None` when it is not a repository or git is missing.
fn dirty(root: &Path) -> Option<Vec<String>> {
    let out = Process::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Some(lines)
}

/// Runs clippy (or, without it, `cargo check`) with JSON messages.
fn compiler_messages(root: &Path, cargo_args: &[String]) -> Result<(String, bool), Failure> {
    let run = |sub: &str| {
        cargo()
            .arg(sub)
            .args(["--all-targets", "--message-format=json", "--quiet"])
            .args(cargo_args)
            .current_dir(root)
            .output()
    };
    let out = run("clippy").map_err(cargo_missing)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && stderr.contains("no such command") {
        let out = run("check").map_err(cargo_missing)?;
        return Ok((String::from_utf8_lossy(&out.stdout).into_owned(), false));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() && !stdout.contains("\"reason\":\"compiler-message\"") {
        return Err(Failure::new(
            Code::FIX_FAILED,
            format!("cargo clippy failed: {}", first_error(&stderr)),
            "run cargo clippy to see why, fix that, then giong fix again",
        ));
    }
    Ok((stdout, true))
}

/// The line (1-based) of a byte offset.
fn line_of(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].matches('\n').count() + 1
}

fn run(args: &Args) -> Result<Report, Failure> {
    let project = Project::here()?;
    let root = project.root.clone();
    let dry_run = args.has("dry-run");
    if !dry_run && !args.has("allow-dirty") {
        if let Some(changed) = dirty(&root).filter(|c| !c.is_empty()) {
            return Err(Failure::new(
                Code::FIX_DIRTY,
                format!(
                    "{} file(s) have changes that are not committed ({})",
                    changed.len(),
                    changed
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "commit or stash them first, or pass --allow-dirty (giong fix --dry-run shows \
                 the edits)",
            ));
        }
    }
    let mut r = Report::new("fix");
    let mut done: Vec<(Fix, usize)> = Vec::new();
    let mut skipped: Vec<(Fix, String)> = Vec::new();
    let mut clippy = true;
    // Fixes can reveal more (an import unused once a call is gone): look again,
    // a few times. A dry run cannot see past its first round.
    let rounds = if dry_run { 1 } else { 3 };
    for _ in 0..rounds {
        let (json, with_clippy) = compiler_messages(&root, &args.rest)?;
        clippy = with_clippy;
        let (mut fixes, error) = suggestions(&json, &root);
        if let Some(error) = error {
            r.push(
                Finding::new(
                    Code::FIX_FAILED,
                    Severity::Error,
                    "the code does not compile",
                )
                .detail(format!(
                    "{error}. The compiler's and clippy's suggestions are only applied to \
                         code that compiles."
                ))
                .fix("giong check shows every error; fix them, then run giong fix again"),
            );
            fixes.clear();
        }
        fixes.extend(check_fixes(&giong_findings(&project, &args.rest)));
        let applied = apply(&root, fixes);
        if applied.done.is_empty() {
            skipped.extend(applied.skipped);
            break;
        }
        for fix in &applied.done {
            let e = &fix.edits[0];
            let text = fs::read_to_string(root.join(&e.file)).unwrap_or_default();
            done.push((fix.clone(), line_of(&text, e.start)));
        }
        skipped = applied.skipped;
        if dry_run {
            for fix in &applied.done {
                for e in &fix.edits {
                    let text = fs::read_to_string(root.join(&e.file)).unwrap_or_default();
                    let old = &text[e.start..e.end];
                    r.line(format!(
                        "{}:{}: {}",
                        e.file,
                        line_of(&text, e.start),
                        fix.title
                    ));
                    for l in old.lines() {
                        r.line(format!("  - {l}"));
                    }
                    for l in e.replacement.lines() {
                        r.line(format!("  + {l}"));
                    }
                }
            }
            break;
        }
        for (file, text) in &applied.files {
            fs::write(root.join(file), text).map_err(|e| {
                Failure::new(
                    Code::WRITE_FAILED,
                    format!("{file} could not be written: {e}"),
                    "check that you may write to the project",
                )
            })?;
        }
    }
    for (fix, why) in &skipped {
        let e = &fix.edits[0];
        r.push(
            Finding::new(
                Code::FIX_CONFLICT,
                Severity::Warning,
                format!("not applied: {}", fix.title),
            )
            .detail(why.clone())
            .fix("run giong fix again; if it stays, apply it by hand")
            .at(&e.file, 0, 0),
        );
    }
    if !clippy {
        r.push(
            Finding::new(Code::FIX_FAILED, Severity::Info, "clippy is not installed")
                .detail("Only the compiler's suggestions were applied.")
                .fix("rustup component add clippy"),
        );
    }
    let files: std::collections::BTreeSet<&str> = done
        .iter()
        .flat_map(|(f, _)| f.edits.iter().map(|e| e.file.as_str()))
        .collect();
    r.data("dry_run", json!(dry_run));
    r.data(
        "fixes",
        json!(done
            .iter()
            .map(
                |(f, line)| json!({"what": f.what, "title": f.title, "file": f.edits[0].file,
                                    "line": line, "edits": f.edits.len()})
            )
            .collect::<Vec<_>>()),
    );
    r.data("files", json!(files));
    let verb = if dry_run { "Would fix" } else { "Fixed" };
    if done.is_empty() {
        r.line("Nothing to fix by itself. giong check lists what needs a decision.");
    } else {
        if !dry_run {
            for (fix, line) in &done {
                r.line(format!("{}:{line}: {}", fix.edits[0].file, fix.title));
            }
        }
        r.line(format!(
            "{verb} {} problem(s) in {} file(s).",
            done.len(),
            files.len()
        ));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("giong-fix-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn edit(file: &str, start: usize, end: usize, r: &str) -> Edit {
        Edit {
            file: file.into(),
            start,
            end,
            replacement: r.into(),
        }
    }

    fn fix(what: &str, edits: Vec<Edit>) -> Fix {
        Fix {
            what: what.into(),
            title: what.into(),
            edits,
        }
    }

    #[test]
    fn applies_from_the_end_and_skips_overlaps() {
        let dir = scratch("apply");
        fs::write(dir.join("a.rs"), "let a = 1; let b = 2; let c = 3;").unwrap();
        let out = apply(
            &dir,
            vec![
                fix("one", vec![edit("a.rs", 4, 5, "alpha")]),
                fix(
                    "two",
                    vec![edit("a.rs", 26, 27, "gamma"), edit("a.rs", 15, 16, "beta")],
                ),
                // Overlaps "one": skipped whole.
                fix(
                    "three",
                    vec![edit("a.rs", 0, 6, "x"), edit("a.rs", 30, 31, "4")],
                ),
                // The same as "one", reported twice: once.
                fix("one again", vec![edit("a.rs", 4, 5, "alpha")]),
            ],
        );
        assert_eq!(
            out.files["a.rs"],
            "let alpha = 1; let beta = 2; let gamma = 3;"
        );
        assert_eq!(out.done.len(), 2);
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].0.what, "three");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn insertions_and_deletions() {
        let dir = scratch("insert");
        fs::write(dir.join("a.rs"), "use x;\nfn f() {}\n").unwrap();
        let out = apply(
            &dir,
            vec![
                fix("del", vec![edit("a.rs", 0, 7, "")]),
                fix("ins", vec![edit("a.rs", 7, 7, "#[inline]\n")]),
                // A second insertion at the same place: order unknown, skipped.
                fix("ins2", vec![edit("a.rs", 7, 7, "#[cold]\n")]),
            ],
        );
        assert_eq!(out.files["a.rs"], "#[inline]\nfn f() {}\n");
        assert_eq!(out.skipped.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_or_broken_edits_are_skipped() {
        let dir = scratch("stale");
        fs::write(dir.join("a.rs"), "é = 1;").unwrap();
        let out = apply(
            &dir,
            vec![
                fix("past the end", vec![edit("a.rs", 3, 99, "")]),
                fix("inside a char", vec![edit("a.rs", 1, 2, "e")]),
                fix("backwards", vec![edit("a.rs", 3, 2, "")]),
                fix("no file", vec![edit("nope.rs", 0, 0, "x")]),
                fix("good", vec![edit("a.rs", 0, 2, "e")]),
            ],
        );
        assert_eq!(out.files["a.rs"], "e = 1;");
        assert_eq!(out.skipped.len(), 4);
        assert!(!out.files.contains_key("nope.rs"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_suggestions_from_cargo_json() {
        let dir = scratch("json");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let span = |file: &str, app: &str, repl: &str| {
            json!({"file_name": file, "byte_start": 0, "byte_end": 2, "suggested_replacement": repl,
                   "suggestion_applicability": app})
        };
        let msg = |level: &str, children: Value| {
            json!({"reason": "compiler-message", "message": {"level": level, "message": "m",
                   "code": {"code": "clippy::x"}, "spans": [], "children": children,
                   "rendered": "error[E0425]: cannot find value\n  --> here"}})
            .to_string()
        };
        let lines = [
            msg(
                "warning",
                json!([{"spans": [span("src/main.rs", "MachineApplicable", "fx")]}]),
            ),
            msg(
                "warning",
                json!([{"spans": [span("src/main.rs", "MaybeIncorrect", "no")]}]),
            ),
            // Half machine-applicable: not taken at all.
            msg(
                "warning",
                json!([{"spans": [span("src/main.rs", "MachineApplicable", "a"),
                                             span("src/main.rs", "HasPlaceholders", "b")]}]),
            ),
            msg(
                "warning",
                json!([{"spans": [span("/rustc/library/core/src/x.rs", "MachineApplicable", "no")]}]),
            ),
            "not json".to_string(),
            json!({"reason": "build-finished", "success": true}).to_string(),
        ];
        let (fixes, err) = suggestions(&lines.join("\n"), &dir);
        assert_eq!(fixes.len(), 1, "{fixes:?}");
        assert_eq!(fixes[0].edits[0].file, "src/main.rs");
        assert_eq!(fixes[0].edits[0].replacement, "fx");
        assert_eq!(fixes[0].what, "clippy::x");
        assert!(err.is_none());

        let with_error = format!("{}\n{}", lines[0], msg("error", json!([])));
        let (_, err) = suggestions(&with_error, &dir);
        assert_eq!(err.as_deref(), Some("error[E0425]: cannot find value"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn findings_with_edits_become_fixes() {
        let f = Finding::new(Code::HOST_NAME_BIND, Severity::Error, "binding localhost")
            .edits(vec![edit("src/main.rs", 1, 2, "0.0.0.0")]);
        let plain = Finding::new(
            Code::BLOCKING_CALL,
            Severity::Warning,
            "std::fs in a worker",
        );
        let fixes = check_fixes(&[f, plain]);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].what, "GIONG-E5007");
    }

    /// A real project with an unused import and a `len() == 0`: giong fix's
    /// own loop (clippy's JSON, then the edits) leaves it clean and compiling.
    #[test]
    fn fixes_a_real_project() {
        let dir = scratch("real");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"fixme\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        let before = "use std::collections::HashMap;\n\nfn main() {\n    let v = vec![1];\n    if v.len() == 0 {\n        println!(\"empty\");\n    }\n}\n";
        fs::write(dir.join("src/main.rs"), before).unwrap();
        let has_clippy = cargo()
            .args(["clippy", "--version"])
            .output()
            .is_ok_and(|o| o.status.success());
        let (json, _) = compiler_messages(&dir, &[]).unwrap();
        let (fixes, err) = suggestions(&json, &dir);
        assert!(err.is_none(), "{err:?}");
        let applied = apply(&dir, fixes);
        let after = &applied.files["src/main.rs"];
        assert!(!after.contains("HashMap"), "{after}");
        if has_clippy {
            assert!(after.contains("v.is_empty()"), "{after}");
        }
        fs::write(dir.join("src/main.rs"), after).unwrap();
        let (json, _) = compiler_messages(&dir, &[]).unwrap();
        let (fixes, err) = suggestions(&json, &dir);
        assert!(err.is_none() && fixes.is_empty(), "{err:?} {fixes:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines() {
        assert_eq!(line_of("a\nb\nc", 0), 1);
        assert_eq!(line_of("a\nb\nc", 2), 2);
        assert_eq!(line_of("a\nb\nc", 99), 3);
    }
}
