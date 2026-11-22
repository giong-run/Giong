//! `giong check`: everything wrong with the project, in one report — the
//! compiler's errors and warnings, clippy's lints when clippy is installed,
//! and giong's own checks of the source ([`crate::lint`]).
//!
//! Machine-applicable suggestions become edits that `giong fix` applies.
//! While `giong dev` runs, its last build is reported too.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::build;
use crate::host;
use crate::lint;
use crate::project::Project;
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "check",
    summary: "compiler errors, clippy lints and giong's own checks, in one report",
    usage: "giong check [--no-clippy] [-- cargo arguments]\n\n\
            Runs cargo clippy (or cargo check without clippy, or with --no-clippy) on every \
            target of the project, and giong's checks of the source: blocking calls in async \
            code (GIONG-E5006), listeners bound to a host name (GIONG-E5007), RefCell borrows \
            held across an .await (GIONG-E5008). Fixes that can be applied as they are come as \
            edits (giong fix applies them). Arguments after -- go to cargo, such as \
            -- --features x.",
    flags: &[Flag {
        name: "no-clippy",
        kind: Kind::Switch,
        help: "run cargo check even when clippy is installed",
    }],
    max_words: 0,
    run,
};

fn run(args: &Args) -> Result<Report, Failure> {
    let project = Project::here()?;
    let clippy = !args.has("no-clippy") && clippy_installed(&project.root);
    let mut r = Report::new("check");
    let (cargo_findings, took) = cargo_findings(
        &project,
        if clippy { "clippy" } else { "check" },
        &args.rest,
    )?;
    let source = source_findings(&project);
    let compiler = cargo_findings.len();
    for f in cargo_findings.into_iter().chain(source.iter().cloned()) {
        r.push(f);
    }
    r.data(
        "tool",
        json!(if clippy {
            "cargo clippy"
        } else {
            "cargo check"
        }),
    );
    r.data("took_ms", json!(took.as_millis() as u64));
    r.data("compiler_findings", json!(compiler));
    r.data("giong_findings", json!(source.len()));
    if let Some(dev) = dev_build(&project.root) {
        r.data("dev", dev);
    }
    let errors = r
        .findings()
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let others = r.findings().len() - errors;
    r.line(if r.findings().is_empty() {
        format!(
            "No problems found ({}, giong's checks).",
            if clippy {
                "cargo clippy"
            } else {
                "cargo check"
            }
        )
    } else {
        format!(
            "{errors} {}, {others} other {}:",
            if errors == 1 { "error" } else { "errors" },
            if others == 1 { "finding" } else { "findings" }
        )
    });
    if !clippy && !args.has("no-clippy") {
        r.line("(clippy is not installed: rustup component add clippy adds its lints)");
    }
    Ok(r)
}

/// cargo check's findings and giong's own, with their edits, printing
/// nothing: what `giong fix` applies (it runs clippy itself).
/// `cargo_args` go to cargo (`--features x`).
pub fn findings(project: &Project, cargo_args: &[String]) -> Result<Vec<Finding>, Failure> {
    let (mut out, _) = cargo_findings(project, "check", cargo_args)?;
    out.extend(source_findings(project));
    Ok(out)
}

fn cargo_findings(
    project: &Project,
    subcommand: &str,
    cargo_args: &[String],
) -> Result<(Vec<Finding>, std::time::Duration), Failure> {
    let mut args = vec!["--workspace".to_string(), "--all-targets".to_string()];
    args.extend(cargo_args.iter().cloned());
    let outcome = build::run(&project.root, subcommand, &args)?;
    // The same warning comes once per target that compiles the file (lib and
    // test builds): keep one.
    let mut seen = std::collections::HashSet::new();
    let findings = outcome
        .findings
        .into_iter()
        .filter(|f| {
            let place = f.place.as_ref().map(|p| (p.file.clone(), p.line, p.column));
            seen.insert((f.title.clone(), place))
        })
        .collect();
    Ok((findings, outcome.took))
}

/// giong's checks over every package's source.
fn source_findings(project: &Project) -> Vec<Finding> {
    let dirs: Vec<PathBuf> = project
        .packages()
        .filter_map(|p| p["manifest_path"].as_str())
        .filter_map(|m| Path::new(m).parent().map(Path::to_path_buf))
        .collect();
    let mut out = Vec::new();
    for file in lint::source_files(&dirs) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let shown = file
            .strip_prefix(&project.root)
            .unwrap_or(&file)
            .display()
            .to_string();
        out.extend(lint::check_file(&text, &shown));
    }
    out
}

fn clippy_installed(dir: &Path) -> bool {
    host::output_in(dir, "cargo", &["clippy", "--version"]).is_some()
}

/// The last build of a `giong dev` that runs in this project.
fn dev_build(root: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(root.join(".giong").join("dev.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(json!({"build": v["build"], "service_pid": v["service_pid"], "listen": v["listen"]}))
}
