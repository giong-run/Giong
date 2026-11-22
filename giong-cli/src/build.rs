//! Runs `cargo build` / `cargo check` / `cargo clippy` with JSON messages and
//! turns the compiler's diagnostics into findings.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::code::Code;
use crate::project::{cargo, cargo_missing};
use crate::report::{Edit, Failure, Finding, Severity};

/// What one cargo run produced.
#[derive(Debug, Default)]
pub struct Outcome {
    pub ok: bool,
    pub findings: Vec<Finding>,
    /// Rendered diagnostics, for people, errors first.
    pub rendered: Vec<String>,
    /// Executables built (path, target name).
    pub executables: Vec<(PathBuf, String)>,
    pub took: Duration,
    /// The last lines cargo itself wrote to stderr (dependency resolution,
    /// a manifest error), for when it failed before compiling.
    pub stderr_tail: Vec<String>,
}

/// `cargo <subcommand> --message-format=json <args>` in `dir`.
pub fn run(dir: &Path, subcommand: &str, args: &[String]) -> Result<Outcome, Failure> {
    let start = Instant::now();
    let mut child = cargo()
        .arg(subcommand)
        .arg("--message-format=json-diagnostic-rendered-ansi")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(cargo_missing)?;
    let stderr = child.stderr.take().expect("piped");
    let err_thread = std::thread::spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tail.push(line);
            if tail.len() > 40 {
                tail.remove(0);
            }
        }
        tail
    });
    let mut out = Outcome::default();
    let stdout = child.stdout.take().expect("piped");
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        absorb(&v, dir, &mut out);
    }
    let status = child.wait().map_err(cargo_missing)?;
    out.stderr_tail = err_thread.join().unwrap_or_default();
    out.ok = status.success();
    out.took = start.elapsed();
    if !out.ok && !out.findings.iter().any(|f| f.severity == Severity::Error) {
        // cargo failed before the compiler said anything: a manifest error,
        // a dependency that cannot be fetched.
        let msg = out
            .stderr_tail
            .iter()
            .find(|l| l.contains("error"))
            .cloned()
            .unwrap_or_else(|| "cargo failed".into());
        out.findings.push(
            Finding::new(Code::CARGO_FAILED, Severity::Error, strip_ansi(&msg))
                .detail(strip_ansi(&out.stderr_tail.join("\n")))
                .fix(
                    "fix what cargo reports (run the same cargo command by hand to see all of it)",
                ),
        );
    }
    Ok(out)
}

/// One JSON message of cargo.
pub fn absorb(v: &Value, dir: &Path, out: &mut Outcome) {
    match v["reason"].as_str() {
        Some("compiler-message") => {
            if let Some((finding, rendered)) = diagnostic(&v["message"], dir) {
                // Errors first, in the order they came; `rendered` stays
                // parallel to `findings`.
                let at = if finding.severity == Severity::Error {
                    out.findings
                        .iter()
                        .take_while(|f| f.severity == Severity::Error)
                        .count()
                } else {
                    out.findings.len()
                };
                out.findings.insert(at, finding);
                out.rendered.insert(at, rendered);
            }
        }
        Some("compiler-artifact") => {
            if let Some(exe) = v["executable"].as_str() {
                let name = v["target"]["name"].as_str().unwrap_or("").to_string();
                out.executables.push((PathBuf::from(exe), name));
            }
        }
        _ => {}
    }
}

/// A rustc or clippy diagnostic as a finding (errors and warnings; notes and
/// the "aborting due to" summary are left out).
pub fn diagnostic(m: &Value, dir: &Path) -> Option<(Finding, String)> {
    let level = m["level"].as_str()?;
    let severity = match level {
        "error" | "error: internal compiler error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let text = m["message"].as_str().unwrap_or("").to_string();
    if text.starts_with("aborting due to")
        || text.ends_with("warnings emitted")
        || text.ends_with("warning emitted")
    {
        return None;
    }
    let lint = m["code"]["code"].as_str().unwrap_or("");
    let code = if severity == Severity::Error {
        Code::BUILD_FAILED
    } else if lint.starts_with("clippy::") {
        Code::LINT
    } else {
        Code::BUILD_FAILED
    };
    let rendered = m["rendered"].as_str().unwrap_or(&text).to_string();
    let plain = strip_ansi(&rendered);
    let title = if lint.is_empty() {
        text.clone()
    } else {
        format!("{text} [{lint}]")
    };
    let mut f = Finding::new(code, severity, title).detail(plain.trim_end().to_string());
    f = f.fix(if lint.starts_with('E') {
        format!(
            "rustc --explain {lint} explains it; the compiler's message above says what to change"
        )
    } else if lint.starts_with("clippy::") {
        format!(
            "follow clippy's suggestion (https://rust-lang.github.io/rust-clippy/master/#{})",
            lint.trim_start_matches("clippy::")
        )
    } else {
        "the compiler's message above says what to change".to_string()
    });
    let spans = m["spans"].as_array().cloned().unwrap_or_default();
    if let Some(span) = spans.iter().find(|s| s["is_primary"] == true) {
        let file = span["file_name"].as_str().unwrap_or("").to_string();
        f = f.at(
            file,
            span["line_start"].as_u64().unwrap_or(1) as u32,
            span["column_start"].as_u64().unwrap_or(1) as u32,
        );
    }
    // Machine-applicable suggestions become edits giong fix can apply.
    let mut edits = Vec::new();
    collect_edits(m, dir, &mut edits);
    if !edits.is_empty() {
        f = f.edits(edits);
    }
    Some((f, rendered))
}

fn collect_edits(m: &Value, dir: &Path, edits: &mut Vec<Edit>) {
    for s in m["spans"].as_array().into_iter().flatten() {
        if s["suggestion_applicability"] == "MachineApplicable" {
            if let (Some(file), Some(start), Some(end), Some(rep)) = (
                s["file_name"].as_str(),
                s["byte_start"].as_u64(),
                s["byte_end"].as_u64(),
                s["suggested_replacement"].as_str(),
            ) {
                // Only files of the project, never of a dependency.
                let path = Path::new(file);
                if path.is_absolute() && !path.starts_with(dir) {
                    continue;
                }
                edits.push(Edit {
                    file: file.to_string(),
                    start: start as usize,
                    end: end as usize,
                    replacement: rep.to_string(),
                });
            }
        }
    }
    for child in m["children"].as_array().into_iter().flatten() {
        collect_edits(child, dir, edits);
    }
}

/// Removes ANSI colour sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostics_become_findings_with_edits() {
        let m = json!({
            "level": "warning",
            "message": "unused variable: `x`",
            "code": {"code": "unused_variables"},
            "rendered": "\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n --> src/main.rs:2:9\n",
            "spans": [{"file_name": "src/main.rs", "line_start": 2, "column_start": 9,
                       "is_primary": true, "byte_start": 20, "byte_end": 21,
                       "suggestion_applicability": null, "suggested_replacement": null}],
            "children": [{"level": "help", "message": "prefix it", "spans": [
                {"file_name": "src/main.rs", "byte_start": 20, "byte_end": 21, "is_primary": true,
                 "suggestion_applicability": "MachineApplicable", "suggested_replacement": "_x"}]}]
        });
        let (f, rendered) = diagnostic(&m, Path::new("/p")).unwrap();
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.code, Code::BUILD_FAILED);
        assert_eq!(f.title, "unused variable: `x` [unused_variables]");
        assert!(f.detail.starts_with("warning: unused variable"));
        assert_eq!(f.place.as_ref().unwrap().line, 2);
        assert!(f.fixable);
        assert_eq!(f.edits[0].replacement, "_x");
        assert!(rendered.contains('\u{1b}'));
    }

    #[test]
    fn errors_come_first_and_summaries_are_dropped() {
        let mut out = Outcome::default();
        let warn = json!({"reason": "compiler-message", "message": {"level": "warning", "message": "w", "spans": []}});
        let err = json!({"reason": "compiler-message", "message": {"level": "error", "message": "mismatched types", "code": {"code": "E0308"}, "spans": []}});
        let summary = json!({"reason": "compiler-message", "message": {"level": "error", "message": "aborting due to 1 previous error", "spans": []}});
        let exe = json!({"reason": "compiler-artifact", "executable": "/p/target/debug/app", "target": {"name": "app"}});
        for v in [&warn, &err, &summary, &exe] {
            absorb(v, Path::new("/p"), &mut out);
        }
        assert_eq!(out.findings.len(), 2);
        assert_eq!(out.findings[0].title, "mismatched types [E0308]");
        assert!(out.findings[0].fix.contains("rustc --explain E0308"));
        assert_eq!(
            out.executables,
            [(PathBuf::from("/p/target/debug/app"), "app".to_string())]
        );
    }

    #[test]
    fn edits_outside_the_project_are_ignored() {
        let m = json!({"level": "warning", "message": "m", "spans": [
            {"file_name": "/home/u/.cargo/registry/x.rs", "byte_start": 1, "byte_end": 2, "is_primary": true,
             "suggestion_applicability": "MachineApplicable", "suggested_replacement": "y"}]});
        let (f, _) = diagnostic(&m, Path::new("/p")).unwrap();
        assert!(!f.fixable);
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[31merror\u{1b}[0m: x"),
            "error: x"
        );
    }
}
