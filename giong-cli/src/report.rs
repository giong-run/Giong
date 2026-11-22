//! What every command prints: findings with a code, a cause and a fix, as text
//! for people or as one JSON document for agents (`--json`).
//!
//! The JSON document is the same shape for every command:
//!
//! ```json
//! {"command": "doctor", "version": "1.3.0", "ok": false,
//!  "findings": [{"code": "GIONG-E5003", "severity": "error", "title": "...",
//!                "detail": "...", "fix": "...", "url": "...",
//!                "file": "src/main.rs", "line": 12, "column": 5, "fixable": true,
//!                "edits": [{"file": "src/main.rs", "start_byte": 210, "end_byte": 236,
//!                           "replacement": "..."}]}],
//!  "data": {}}
//! ```
//!
//! When the command cannot do its job at all, the document has `"ok": false`, no
//! findings, and an `error` object instead:
//!
//! ```json
//! {"command": "clean", "version": "1.3.0", "ok": false,
//!  "error": {"code": "GIONG-E5000", "message": "...", "fix": "...", "url": "..."},
//!  "findings": [], "data": {}}
//! ```
//!
//! `file`, `line`, `column` appear when the finding is about a place in the code;
//! `edits` when `giong fix` can apply the fix (all of them, or none);
//! `data` holds what the command found besides problems (the driver it probed, the
//! bytes it freed...). Fields are only ever added. Exit status: 0 when no finding
//! is an error, 1 when one is, 2 when the command could not do its job
//! ([`Failure`]).

use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{json, Map, Value};

use crate::code::Code;

/// How bad a finding is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Works, but is slower, riskier or will break elsewhere.
    Warning,
    /// Does not work; the command exits with 1.
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// A place in a file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// A replacement `giong fix` can apply: bytes `start..end` of `file` (relative
/// to the project root) become `replacement`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Edit {
    pub file: String,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// One problem (or fact) a command found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    pub code: Code,
    pub severity: Severity,
    /// One line: what is wrong.
    pub title: String,
    /// What was seen, with the numbers.
    pub detail: String,
    /// What to do about it.
    pub fix: String,
    pub place: Option<Place>,
    /// Whether `giong fix` can apply the fix by itself.
    pub fixable: bool,
    /// The edits that apply it, when `fixable` (all or none).
    pub edits: Vec<Edit>,
}

impl Finding {
    pub fn new(code: Code, severity: Severity, title: impl Into<String>) -> Finding {
        Finding {
            code,
            severity,
            title: title.into(),
            detail: String::new(),
            fix: String::new(),
            place: None,
            fixable: false,
            edits: Vec::new(),
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Finding {
        self.detail = detail.into();
        self
    }

    pub fn fix(mut self, fix: impl Into<String>) -> Finding {
        self.fix = fix.into();
        self
    }

    pub fn at(mut self, file: impl Into<String>, line: u32, column: u32) -> Finding {
        self.place = Some(Place {
            file: file.into(),
            line,
            column,
        });
        self
    }

    /// Marks the finding as fixable by `giong fix` with these edits.
    pub fn edits(mut self, edits: Vec<Edit>) -> Finding {
        self.fixable = !edits.is_empty();
        self.edits = edits;
        self
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code.to_string()));
        m.insert("severity".into(), json!(self.severity.as_str()));
        m.insert("title".into(), json!(self.title));
        m.insert("detail".into(), json!(self.detail));
        m.insert("fix".into(), json!(self.fix));
        m.insert("url".into(), json!(self.code.url()));
        if let Some(p) = &self.place {
            m.insert("file".into(), json!(p.file));
            m.insert("line".into(), json!(p.line));
            m.insert("column".into(), json!(p.column));
        }
        m.insert("fixable".into(), json!(self.fixable));
        if !self.edits.is_empty() {
            m.insert(
                "edits".into(),
                json!(self
                    .edits
                    .iter()
                    .map(
                        |e| json!({"file": e.file, "start_byte": e.start, "end_byte": e.end,
                                    "replacement": e.replacement})
                    )
                    .collect::<Vec<_>>()),
            );
        }
        Value::Object(m)
    }
}

/// The command could not do its job at all (exit status 2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Failure {
    pub code: Code,
    pub what: String,
    pub fix: String,
}

impl Failure {
    pub fn new(code: Code, what: impl Into<String>, fix: impl Into<String>) -> Failure {
        Failure {
            code,
            what: what.into(),
            fix: fix.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}. Fix: {}{} See {}",
            self.code,
            self.what,
            self.fix,
            if self.fix.ends_with(['.', '?', '!']) {
                ""
            } else {
                "."
            },
            self.code.url()
        )
    }
}

/// What a command found, printed once at the end.
#[derive(Debug)]
pub struct Report {
    command: String,
    findings: Vec<Finding>,
    data: Map<String, Value>,
    /// Lines for people, printed before the findings (text mode only).
    lines: Vec<String>,
    /// Nothing is printed at the end (a command that spoke on its own,
    /// such as `mcp`).
    quiet: bool,
}

impl Report {
    pub fn new(command: &str) -> Report {
        Report {
            command: command.to_string(),
            findings: Vec::new(),
            data: Map::new(),
            lines: Vec::new(),
            quiet: false,
        }
    }

    /// A report that prints nothing when the command ends.
    pub fn quiet(mut self) -> Report {
        self.quiet = true;
        self
    }

    pub fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }

    /// A fact for the `data` object (JSON) .
    pub fn data(&mut self, key: &str, value: Value) {
        self.data.insert(key.to_string(), value);
    }

    /// A line for people (text mode).
    pub fn line(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn data_value(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    pub fn ok(&self) -> bool {
        !self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "command": self.command,
            "version": env!("CARGO_PKG_VERSION"),
            "ok": self.ok(),
            "findings": self.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
            "data": Value::Object(self.data.clone()),
        })
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(l);
            out.push('\n');
        }
        let mut sorted: Vec<&Finding> = self.findings.iter().collect();
        sorted.sort_by_key(|f| std::cmp::Reverse(f.severity));
        for f in sorted {
            if !out.is_empty() {
                out.push('\n');
            }
            let place = f
                .place
                .as_ref()
                .map(|p| format!("{}:{}:{}: ", p.file, p.line, p.column))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}{} {}: {}\n",
                place,
                f.severity.as_str(),
                f.code,
                f.title
            ));
            if !f.detail.is_empty() {
                out.push_str(&format!("  {}\n", f.detail));
            }
            if !f.fix.is_empty() {
                out.push_str(&format!("  Fix: {}\n", f.fix));
            }
            out.push_str(&format!("  See {}\n", f.code.url()));
        }
        out
    }

    /// Prints the report and gives the exit status.
    pub fn finish(self, json: bool) -> ExitCode {
        if self.quiet {
            return ExitCode::SUCCESS;
        }
        let text = if json {
            format!("{}\n", self.to_json())
        } else {
            self.to_text()
        };
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        if self.ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }
}

/// Prints a failure (to stdout as JSON, or to stderr as text) and gives exit
/// status 2.
pub fn fail(command: &str, failure: &Failure, json: bool) -> ExitCode {
    if json {
        let doc = json!({
            "command": command,
            "version": env!("CARGO_PKG_VERSION"),
            "ok": false,
            "error": {
                "code": failure.code.to_string(),
                "message": failure.what,
                "fix": failure.fix,
                "url": failure.code.url(),
            },
            "findings": [],
            "data": {},
        });
        // A closed pipe is not worth a panic: the exit status still says it.
        let _ = writeln!(io::stdout().lock(), "{doc}");
    } else {
        let _ = writeln!(io::stderr().lock(), "giong: {failure}");
    }
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_is_stable() {
        let mut r = Report::new("doctor");
        r.push(
            Finding::new(Code::RUST_TOO_OLD, Severity::Error, "Rust 1.70 is too old")
                .detail("giong needs 1.80")
                .fix("rustup update stable")
                .at("Cargo.toml", 3, 1),
        );
        r.data("driver", json!("io_uring"));
        let v = r.to_json();
        assert_eq!(v["command"], "doctor");
        assert_eq!(v["ok"], false);
        let f = &v["findings"][0];
        assert_eq!(f["code"], "GIONG-E5003");
        assert_eq!(f["severity"], "error");
        assert_eq!(f["line"], 3);
        assert_eq!(f["fixable"], false);
        assert_eq!(v["data"]["driver"], "io_uring");
        let t = r.to_text();
        assert!(t.starts_with("Cargo.toml:3:1: error GIONG-E5003: Rust 1.70 is too old"));
        assert!(t.contains("  Fix: rustup update stable"));
    }

    #[test]
    fn warnings_do_not_fail() {
        let mut r = Report::new("doctor");
        r.push(Finding::new(Code::DISK_USED, Severity::Warning, "big"));
        assert!(r.ok());
    }
}
