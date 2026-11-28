//! `giong cache`: the build store, which keeps one copy on disk of each
//! dependency build for every project on the machine (see src/store.rs).
//!
//! `giong dev` shares each project's dependency builds after it builds, and
//! before a dev build takes the ones another project already made;
//! `giong cache share` and `giong cache fill` do the same by hand (any cargo
//! project, giong or not), `giong cache gc` frees what nothing uses and keeps
//! the builds under a budget, and `giong cache` says what the store saves.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use super::clean::{find_target_dirs, human};
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::project::{find_manifest_dir, Project};
use crate::report::{Failure, Finding, Report, Severity};
use crate::store::{self, Store};

pub const COMMAND: Command = Command {
    name: "cache",
    summary: "keep one copy of each dependency build for all your projects",
    usage: "giong cache [status]\n\
            giong cache share [dir...]\n\
            giong cache fill\n\
            giong cache gc [--budget SIZE] [--dry-run]\n\n\
            Rust projects each build their own copy of every dependency, and cargo never removes \
            one. The build store keeps one copy on disk: files a dependency's build wrote that are \
            byte for byte the same in several projects become hard links to one file in \
            ~/.giong/store. Each project keeps its own target/, so cargo works as before. giong \
            dev shares each build on its own, and before a dev build takes the dependency builds \
            another project already made, so a new project compiles only its own code. status: \
            what the store saves. share: share the builds of this project, or of every cargo \
            project under the directories given (giong cache share ~/code). fill: give this \
            project's dev build (target/debug) the dependency builds another project made; cargo \
            checks each one and builds again what differs. Release builds are never filled. gc: remove what no project uses, and keep the builds of \
            the projects giong knows under the budget (GIONG_STORE_BUDGET, default 30G): what an \
            older Rust built first, then whole builds of projects not built for 14 days. \
            GIONG_STORE=off turns the store off; GIONG_STORE_DIR moves it (it must be on the \
            same drive as the projects).",
    flags: &[
        Flag {
            name: "budget",
            kind: Kind::Value,
            help: "gc: the size the builds may use, such as 30G or 500M, or off",
        },
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "gc: say what would be removed, remove nothing",
        },
    ],
    max_words: 17,
    run,
};

fn run(args: &Args) -> Result<Report, Failure> {
    let (verb, rest) = match args.words.split_first() {
        None => ("status", &[][..]),
        Some((v, rest)) => (v.as_str(), rest),
    };
    let known = matches!(verb, "status" | "gc" | "fill") && rest.is_empty() || verb == "share";
    if !known {
        return Err(Failure::new(
            Code::USAGE,
            format!("giong cache {} is not a command", args.words.join(" ")),
            "giong cache status, giong cache share [dir...], giong cache fill or giong cache gc",
        ));
    }
    // Checked before the store is opened (and made): a mistake writes nothing.
    let budget = match args.value("budget") {
        Some(v) => store::parse_size(v).ok_or_else(|| {
            Failure::new(
                Code::USAGE,
                format!("--budget {v:?} is not a size"),
                "give a size such as 30G, 500M or off",
            )
        })?,
        None => store::budget_from_env(),
    };
    let store = Store::open().map_err(|u| Failure::new(Code::STORE_UNUSABLE, u.why(), u.fix()))?;
    match verb {
        "share" => share(&store, rest),
        "fill" => fill(&store),
        "gc" => Ok(gc(&store, budget, args.has("dry-run"))),
        _ => Ok(status(&store)),
    }
}

fn ago(secs: u64) -> String {
    let d = store::now().saturating_sub(secs);
    match d {
        0..=119 => "just now".into(),
        120..=7199 => format!("{} minutes ago", d / 60),
        7200..=172_799 => format!("{} hours ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

fn status(store: &Store) -> Report {
    let mut r = Report::new("cache");
    let u = store::usage(store);
    let reg = store::registry(store);
    r.line(format!("Build store: {}", store.root.display()));
    r.line(format!(
        "{} saved: {} files ({}) shared by the projects instead of a copy each",
        human(u.saved_bytes),
        u.objects - u.unused,
        human(u.object_bytes - u.unused_bytes)
    ));
    if u.unused > 0 {
        r.line(format!(
            "{} files ({}) no project uses any more: giong cache gc removes them",
            u.unused,
            human(u.unused_bytes)
        ));
    }
    let mut rows = Vec::new();
    let dirs: Vec<PathBuf> = reg.projects.iter().map(|p| p.build_dir.clone()).collect();
    for p in &reg.projects {
        let exists = p.build_dir.is_dir();
        let bytes = if exists {
            store::disk_used(std::slice::from_ref(&p.build_dir))
        } else {
            0
        };
        r.line(format!(
            "  {}: {}, built {}{}",
            p.build_dir.display(),
            human(bytes),
            ago(p.last_built),
            if exists { "" } else { " (gone)" }
        ));
        rows.push(json!({
            "build_dir": p.build_dir.display().to_string(),
            "root": p.root.display().to_string(),
            "bytes": bytes,
            "last_built": p.last_built,
            "exists": exists,
        }));
    }
    let total = store::disk_used(&dirs);
    let budget = store::budget_from_env();
    r.line(format!(
        "{} projects use {} on disk{}",
        reg.projects.len(),
        human(total),
        match budget {
            Some(b) => format!(", budget {}", human(b)),
            None => ", no budget".into(),
        }
    ));
    r.data("store", json!(store.root.display().to_string()));
    r.data("objects", json!(u.objects));
    r.data("object_bytes", json!(u.object_bytes));
    r.data("saved_bytes", json!(u.saved_bytes));
    r.data("unused_objects", json!(u.unused));
    r.data("unused_bytes", json!(u.unused_bytes));
    r.data("projects", Value::Array(rows));
    r.data("bytes_used", json!(total));
    r.data("budget", json!(budget));
    r.data("last_gc", json!(reg.last_gc));
    r
}

/// A build directory to share and the project it belongs to.
struct Target {
    build_dir: PathBuf,
    root: PathBuf,
}

fn share(store: &Store, dirs: &[String]) -> Result<Report, Failure> {
    let mut r = Report::new("cache");
    let mut targets = Vec::new();
    if dirs.is_empty() {
        let p = Project::here()?;
        targets.push(Target {
            build_dir: p.build_dir,
            root: p.root,
        });
    } else {
        for w in dirs {
            let dir = Path::new(w);
            if !dir.is_dir() {
                return Err(Failure::new(
                    Code::USAGE,
                    format!("{w} is not a directory"),
                    "give directories that hold projects, such as giong cache share ~/code",
                ));
            }
            let mut found = Vec::new();
            find_target_dirs(dir, 0, &mut found);
            for build_dir in found {
                let Some(root) = build_dir.parent().and_then(find_manifest_dir) else {
                    continue;
                };
                targets.push(Target { build_dir, root });
            }
        }
    }
    let all: Vec<PathBuf> = targets.iter().map(|t| t.build_dir.clone()).collect();
    let before = store::disk_used(&all);
    let never = AtomicBool::new(false);
    let mut rows = Vec::new();
    let (mut linked, mut linked_bytes) = (0u64, 0u64);
    for t in &targets {
        let Some(local) = store::local_packages(&t.root) else {
            r.push(
                Finding::new(
                    Code::STORE_UNUSABLE,
                    Severity::Info,
                    format!(
                        "{} was skipped: its project has no Cargo.lock",
                        t.build_dir.display()
                    ),
                )
                .fix("build the project once (cargo build) so cargo writes Cargo.lock"),
            );
            continue;
        };
        let profiles = store::profiles(&t.build_dir);
        if profiles.is_empty() {
            r.push(
                Finding::new(
                    Code::STORE_UNUSABLE,
                    Severity::Info,
                    format!(
                        "{} was skipped: {}",
                        t.build_dir.display(),
                        store::Skip::OldLayout.why()
                    ),
                )
                .fix("rustup update stable, then build the project again"),
            );
            continue;
        }
        let mut shared_any = false;
        for profile in profiles {
            match store::share(store, &profile, &local, &never) {
                Ok(pass) => {
                    shared_any = true;
                    linked += pass.linked_files;
                    linked_bytes += pass.linked_bytes;
                    rows.push(json!({
                        "profile": profile.display().to_string(),
                        "units": pass.units,
                        "shared_units": pass.shared_units,
                        "linked_files": pass.linked_files,
                        "linked_bytes": pass.linked_bytes,
                        "new_objects": pass.new_objects,
                    }));
                }
                Err(skip) => {
                    r.push(
                        Finding::new(
                            Code::STORE_UNUSABLE,
                            skip_severity(&skip),
                            format!("{} was skipped: {}", profile.display(), skip.why()),
                        )
                        .fix(skip_fix(
                            &skip,
                            "run giong cache share again once the build ends",
                        )),
                    );
                }
            }
        }
        if shared_any {
            let _ = store::note_built(store, &t.build_dir, &t.root);
        }
    }
    let after = store::disk_used(&all);
    r.line(format!(
        "{} build directories: {} files ({}) now linked to the store; {} on disk, was {}",
        targets.len(),
        linked,
        human(linked_bytes),
        human(after),
        human(before)
    ));
    r.data("bytes_before", json!(before));
    r.data("bytes_after", json!(after));
    r.data("linked_files", json!(linked));
    r.data("linked_bytes", json!(linked_bytes));
    r.data("profiles", Value::Array(rows));
    Ok(r)
}

fn fill(store: &Store) -> Result<Report, Failure> {
    let p = Project::here()?;
    let mut r = Report::new("cache");
    let never = AtomicBool::new(false);
    match store::fill_project(store, &p.build_dir, &p.workspace_root(), &never) {
        Ok(f) => {
            if f.units > 0 {
                r.line(format!(
                    "{} dependency builds taken from {} ({} files linked, {} copied): cargo \
                     compiles only what is left",
                    f.units,
                    super::dev::sources(&f.from),
                    f.linked_files,
                    human(f.copied_bytes)
                ));
            } else {
                r.line(
                    "no other project has built these dependencies with this compiler yet \
                     (giong dev and giong cache share record the projects built)"
                        .to_string(),
                );
            }
            r.data("filled_units", json!(f.units));
            r.data("linked_files", json!(f.linked_files));
            r.data("copied_bytes", json!(f.copied_bytes));
            let from: Vec<String> = f.from.iter().map(|p| p.display().to_string()).collect();
            r.data("from", json!(from));
        }
        Err(skip) => r.push(
            Finding::new(
                Code::STORE_UNUSABLE,
                skip_severity(&skip),
                format!("{} was not filled: {}", p.build_dir.display(), skip.why()),
            )
            .fix(skip_fix(
                &skip,
                "run giong cache fill again once the build ends",
            )),
        ),
    }
    Ok(r)
}

/// How much a project the store skipped matters: a build in progress or an
/// old cargo is expected, a store that cannot work is not.
fn skip_severity(skip: &store::Skip) -> Severity {
    match skip {
        store::Skip::CannotLink(_) | store::Skip::Io(_) => Severity::Warning,
        store::Skip::Busy | store::Skip::OldLayout => Severity::Info,
    }
}

/// What to do about a project the store skipped, for each cause; `busy` is
/// the command to run again once the build ends.
fn skip_fix(skip: &store::Skip, busy: &'static str) -> &'static str {
    match skip {
        store::Skip::Busy => busy,
        store::Skip::OldLayout => "rustup update stable, then build the project again",
        store::Skip::CannotLink(_) => {
            "set GIONG_STORE_DIR to a directory on the same drive as the project"
        }
        store::Skip::Io(_) => {
            "check that the project's build directory and the store can be written (the error \
             names the file), and that cargo generate-lockfile works in the project"
        }
    }
}

fn gc(store: &Store, budget: Option<u64>, dry: bool) -> Report {
    let never = AtomicBool::new(false);
    let g = store::gc(store, budget, None, dry, &never);
    let mut r = Report::new("cache");
    let verb = if dry { "would remove" } else { "removed" };
    r.line(format!(
        "{} {} files no project uses ({})",
        verb,
        g.unused_objects,
        human(g.unused_bytes)
    ));
    for (dir, bytes, why) in &g.trimmed {
        r.line(format!(
            "{verb} {} from {} ({why})",
            human(*bytes),
            dir.display()
        ));
    }
    for dir in &g.forgotten {
        r.line(format!("forgot {} (it no longer exists)", dir.display()));
    }
    for (dir, why) in &g.skipped {
        r.line(format!("left {} alone: {why}", dir.display()));
    }
    r.line(match budget {
        Some(b) if g.used_before > b => format!(
            "the projects' builds use {}, over the {} budget{}",
            human(if dry { g.used_before } else { g.used_after }),
            human(b),
            if dry {
                " (before removing anything)"
            } else {
                ""
            }
        ),
        Some(b) => format!(
            "the projects' builds use {}, within the {} budget",
            human(g.used_before),
            human(b)
        ),
        None => format!(
            "the projects' builds use {} (no budget)",
            human(g.used_before)
        ),
    });
    r.data(
        "skipped",
        Value::Array(
            g.skipped
                .iter()
                .map(|(d, why)| json!({"build_dir": d.display().to_string(), "reason": why}))
                .collect(),
        ),
    );
    r.data("dry_run", json!(dry));
    r.data("unused_objects", json!(g.unused_objects));
    r.data("unused_bytes", json!(g.unused_bytes));
    r.data("bytes_before", json!(g.used_before));
    r.data("bytes_after", json!(g.used_after));
    r.data("budget", json!(budget));
    r.data(
        "trimmed",
        Value::Array(
            g.trimmed
                .iter()
                .map(|(d, b, why)| json!({"build_dir": d.display().to_string(), "bytes": b, "reason": why}))
                .collect(),
        ),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skipped_project_gets_the_fix_for_its_cause() {
        let fix = |skip: store::Skip| skip_fix(&skip, "run it again");
        assert_eq!(fix(store::Skip::Busy), "run it again");
        assert!(fix(store::Skip::OldLayout).starts_with("rustup update"));
        assert!(fix(store::Skip::CannotLink("EXDEV".into())).contains("GIONG_STORE_DIR"));
        // A write that failed is not fixed by a newer Rust, as it used to
        // say for `giong cache share`.
        let io = fix(store::Skip::Io("permission denied".into()));
        assert!(
            !io.contains("rustup") && io.contains("can be written"),
            "{io}"
        );
        assert_eq!(skip_severity(&store::Skip::Busy), Severity::Info);
        assert_eq!(
            skip_severity(&store::Skip::Io(String::new())),
            Severity::Warning
        );
    }
}
