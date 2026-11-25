//! `giong upgrade`: moves the project to the newest giong release (or the one
//! `--to` names), and `giong upgrade --self` moves this command.
//!
//! The giong crates are released together with one version, and a release
//! within 1.x only adds (docs/STABILITY.md), so moving a project is: every
//! giong requirement in every Cargo.toml of the workspace to the new version,
//! written the way the person wrote it ([`crate::manifest`]); `cargo update`
//! of the giong crates; the new release's `rust-version` against rustc and the
//! project's own; `cargo check`. If any step fails, every file is put back as
//! it was. Then the agent files `giong ai init` wrote are refreshed, so they
//! describe the release the project is on.
//!
//! This command writes the agent files for its own release, so it moves itself
//! first when the project is to move past it: the new command then moves the
//! project (it is run again with the same arguments).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::build;
use crate::code::Code;
use crate::host::{self, Version};
use crate::manifest::{self, Change, Dep};
use crate::project::{cargo, first_error, Project};
use crate::report::{Failure, Finding, Report, Severity};
use crate::selfupdate::{self, Method};
use crate::versions::{self, Latest};

pub const COMMAND: Command = Command {
    name: "upgrade",
    summary: "move the project (or this command, --self) to the newest giong release",
    usage: "giong upgrade [--to <version>] [--dry-run] [--offline]\n\
            giong upgrade --self [--to <version>] [--dry-run]\n\n\
            Moves every giong crate of the project to the newest release on crates.io (or \
            --to): the versions in every Cargo.toml of the workspace, written as you wrote them \
            (\"1.4\" becomes \"1.5\"), then cargo update, a check of the release's rust-version, \
            and cargo check. If a step fails, the files are put back as they were. Then it \
            refreshes the agent files giong ai init wrote. A release newer than this command \
            updates the command first. --offline moves to this command's own release without \
            asking crates.io. --self updates only this command, the way it was installed (the \
            install script or cargo install).",
    flags: &[
        Flag {
            name: "to",
            kind: Kind::Value,
            help: "the release to move to, such as 1.5.0 (default: the newest)",
        },
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "say what would change, change nothing",
        },
        Flag {
            name: "offline",
            kind: Kind::Switch,
            help: "do not ask crates.io: move to this command's release, with cargo offline",
        },
        Flag {
            name: "self",
            kind: Kind::Switch,
            help: "update this giong command instead of the project",
        },
    ],
    max_words: 0,
    run,
};

/// Set for the command run again after it updated itself: its version then.
const CLI_FROM_ENV: &str = "GIONG_UPGRADE_CLI_FROM";

fn run(args: &Args) -> Result<Report, Failure> {
    if args.has("self") {
        if args.has("offline") {
            return Err(Failure::new(
                Code::USAGE,
                "--self downloads a release: it cannot be --offline",
                "leave out --offline, or use giong upgrade --offline for the project",
            ));
        }
        return self_upgrade(args);
    }
    project_upgrade(args)
}

/// `--to`, checked: a release of this major version.
fn asked_version(args: &Args) -> Result<Option<Version>, Failure> {
    let Some(text) = args.value("to") else {
        return Ok(None);
    };
    let v = text.trim().trim_start_matches('v');
    let parsed =
        host::parse_version(v).filter(|p| p.to_string() == v || format!("{}.{}", p.0, p.1) == v);
    let Some(parsed) = parsed else {
        return Err(Failure::new(
            Code::UPGRADE_TARGET,
            format!("--to {text:?} is not a version"),
            "give a release such as --to 1.5.0",
        ));
    };
    let cli = versions::cli();
    if parsed.0 != cli.0 {
        return Err(Failure::new(
            Code::UPGRADE_TARGET,
            format!("--to {parsed} is not a {}.x release", cli.0),
            format!(
                "giong upgrade moves within major version {}, where every release only adds",
                cli.0
            ),
        ));
    }
    Ok(Some(parsed))
}

/// The release to move to, and the lookup when one was made.
fn target(args: &Args, offline: bool) -> Result<(Version, Option<Latest>), Failure> {
    if let Some(v) = asked_version(args)? {
        return Ok((v, None));
    }
    let cli = versions::cli();
    if offline {
        return Ok((cli, None));
    }
    match versions::latest(false) {
        Ok(l) => Ok((l.version, Some(l))),
        Err(why) => Err(Failure::new(
            Code::RELEASE_UNKNOWN,
            format!("the newest giong release could not be looked up: {why}"),
            format!(
                "giong upgrade --offline moves to {cli}, the release of this command; --to <version> \
                 names one; GIONG_LATEST=<version> tells giong which release is the newest"
            ),
        )),
    }
}

fn this_exe() -> Result<PathBuf, Failure> {
    std::env::current_exe().map_err(|e| {
        Failure::new(
            Code::SELF_UPDATE,
            format!("giong cannot find its own binary: {e}"),
            selfupdate::by_hand(versions::cli()),
        )
    })
}

fn self_upgrade(args: &Args) -> Result<Report, Failure> {
    let dry = args.has("dry-run");
    let exe = this_exe()?;
    selfupdate::tidy(&exe);
    let method = selfupdate::detect(&exe);
    let cli = versions::cli();
    let asked = asked_version(args)?;
    let (target, latest) = target(args, false)?;
    let mut r = Report::new("upgrade");
    r.data("self", json!(true));
    r.data("from", json!(cli.to_string()));
    r.data("to", json!(target.to_string()));
    r.data("dry_run", json!(dry));
    r.data("install", method.to_json());
    if let Some(l) = &latest {
        r.data("latest", l.to_json());
    }
    // Without --to, only forward: an older "newest" is a stale mirror.
    if target == cli || (asked.is_none() && target < cli) {
        r.line(format!("This giong command is {cli}, the newest release."));
        return Ok(r);
    }
    if dry {
        r.line(format!("Would {}.", method.plan(target)));
        if let Method::Unknown { .. } = method {
            r.push(unknown_install(&method, target));
        }
        return Ok(r);
    }
    selfupdate::install(&exe, &method, target)?;
    check_installed(&exe, target)?;
    r.line(format!(
        "Updated giong {cli} to {target} ({}).",
        method.name()
    ));
    r.line(format!(
        "What is new: {}",
        super::doctor::changelog_url(target)
    ));
    Ok(r)
}

fn unknown_install(method: &Method, target: Version) -> Finding {
    Finding::new(
        Code::SELF_UPDATE,
        Severity::Error,
        "giong cannot tell how this command was installed",
    )
    .detail(method.plan(target))
    .fix(selfupdate::by_hand(target))
}

fn check_installed(exe: &Path, target: Version) -> Result<(), Failure> {
    match selfupdate::installed_version(exe) {
        Some(v) if v == target => Ok(()),
        other => Err(Failure::new(
            Code::SELF_UPDATE,
            format!(
                "after the update, {} reports {}, not {target}",
                exe.display(),
                other.map_or("no version".to_string(), |v| v.to_string())
            ),
            format!(
                "another giong may come first on the PATH (which giong / where giong); {}",
                selfupdate::by_hand(target)
            ),
        )),
    }
}

/// One Cargo.toml of the workspace: relative name, path, text.
struct File {
    rel: String,
    path: PathBuf,
    text: String,
}

/// The root Cargo.toml (for `[workspace.dependencies]`) and every member's.
fn manifests(project: &Project) -> Vec<File> {
    let mut paths = vec![project.root.join("Cargo.toml")];
    for p in project.packages() {
        if let Some(m) = p["manifest_path"].as_str() {
            let m = PathBuf::from(m);
            if !paths.iter().any(|q| q == &m) {
                paths.push(m);
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let text = fs::read_to_string(&path).ok()?;
            let rel = path
                .strip_prefix(&project.root)
                .map(|r| r.display().to_string().replace('\\', "/"))
                .unwrap_or_else(|_| path.display().to_string());
            Some(File { rel, path, text })
        })
        .collect()
}

/// The files changed, kept as they were until the upgrade is done: in memory,
/// and in `.giong/upgrade/<time>/` in case giong is stopped half way.
struct Saved {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    dir: Option<PathBuf>,
}

impl Saved {
    fn keep(root: &Path, paths: &[PathBuf]) -> Saved {
        let files: Vec<(PathBuf, Option<Vec<u8>>)> = paths
            .iter()
            .map(|p| (p.clone(), fs::read(p).ok()))
            .collect();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let dir = crate::state::ensure_sub(root, &["upgrade", &stamp]).ok();
        if let Some(d) = &dir {
            let mut index = String::new();
            for (i, (path, bytes)) in files.iter().enumerate() {
                if let Some(b) = bytes {
                    let name = format!("{i}.orig");
                    let _ = crate::state::replace(&d.join(&name), b);
                    index.push_str(&format!("{name}\t{}\n", path.display()));
                }
            }
            let _ = crate::state::replace(&d.join("index.txt"), index.as_bytes());
        }
        Saved { files, dir }
    }

    /// Puts every file back; the errors of the files that could not be.
    fn restore(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (path, bytes) in &self.files {
            let result = match bytes {
                Some(b) => fs::write(path, b),
                None => match fs::remove_file(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other,
                },
            };
            if let Err(e) = result {
                errors.push(format!("{}: {e}", path.display()));
            }
        }
        if errors.is_empty() {
            self.discard();
        }
        errors
    }

    fn discard(&self) {
        if let Some(d) = &self.dir {
            let _ = fs::remove_dir_all(d);
            // `upgrade/`, then `.giong/`, each only when empty.
            for parent in d.ancestors().skip(1).take(2) {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

/// Puts the files back and turns `failure` into what the command reports.
fn undo(saved: &Saved, mut failure: Failure) -> Failure {
    let errors = saved.restore();
    if errors.is_empty() {
        failure
            .what
            .push_str(" (nothing was changed: the files are as they were)");
    } else {
        failure.what.push_str(&format!(
            "; and these files could not be put back: {} (the originals are in {})",
            errors.join(", "),
            saved
                .dir
                .as_ref()
                .map_or("no copy".to_string(), |d| d.display().to_string())
        ));
    }
    failure
}

fn cargo_step(root: &Path, args: &[String], offline: bool) -> Result<String, String> {
    let mut cmd = cargo();
    cmd.args(args).current_dir(root);
    if offline {
        cmd.arg("--offline");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cargo could not be started: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(first_error(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn read_lock(root: &Path) -> Vec<versions::Locked> {
    fs::read_to_string(root.join("Cargo.lock"))
        .map(|t| versions::parse_lock(&t))
        .unwrap_or_default()
}

/// The giong crates of `lock` that depend on `name`, directly or not.
fn dependents_first(lock: &[versions::Locked], names: &[String]) -> Vec<String> {
    // A crate comes after every giong crate that depends on it.
    let mut order: Vec<String> = Vec::new();
    let mut left: Vec<String> = names.to_vec();
    while !left.is_empty() {
        let pos = left
            .iter()
            .position(|n| {
                !left
                    .iter()
                    .any(|o| o != n && lock.iter().any(|p| &p.name == o && p.deps.contains(n)))
            })
            .unwrap_or(0);
        order.push(left.remove(pos));
    }
    order
}

/// Whether the project has agent files `giong ai init` wrote.
fn has_agent_files(root: &Path) -> bool {
    root.join(".claude/skills/giong/SKILL.md").is_file()
        || root.join(".cursor/rules/giong.mdc").is_file()
        || fs::read_to_string(root.join("AGENTS.md")).is_ok_and(|t| t.contains("<!-- giong:begin"))
}

fn project_upgrade(args: &Args) -> Result<Report, Failure> {
    let dry = args.has("dry-run");
    let offline = args.has("offline") || versions::cargo_offline();
    let project = Project::here()?;
    let root = project.root.clone();
    let files = manifests(&project);
    let giong_deps: Vec<(usize, Dep)> = files
        .iter()
        .enumerate()
        .flat_map(|(i, f)| {
            manifest::scan(&f.text)
                .into_iter()
                .filter(|d| versions::is_giong_crate(&d.crate_name))
                .map(move |d| (i, d))
        })
        .collect();
    let lock = read_lock(&root);
    let locked = versions::giong_from_registry(&lock);
    let registry: Vec<&(usize, Dep)> = giong_deps
        .iter()
        .filter(|(_, d)| d.is_registry() && d.req.is_some())
        .collect();
    if registry.is_empty() && locked.is_empty() {
        let local = giong_deps.iter().find(|(_, d)| d.path || d.git);
        return Err(match local {
            Some((i, d)) => Failure::new(
                Code::NOT_GIONG_PROJECT,
                format!(
                    "the project takes {} from a {} ({}), not from crates.io",
                    d.crate_name,
                    if d.path { "path" } else { "git repository" },
                    files[*i].rel
                ),
                "update that checkout (git pull); giong upgrade moves versions from crates.io",
            ),
            None => Failure::new(
                Code::NOT_GIONG_PROJECT,
                "the project does not depend on giong",
                "cargo add giong, or start a project with giong new",
            ),
        });
    }
    // Where the project is: its lock, or the lowest version its
    // requirements name when it was never built.
    let from = locked.iter().map(|c| c.version).min().or_else(|| {
        registry
            .iter()
            .filter_map(|(_, d)| versions::parse_req(d.req.as_deref()?))
            .filter_map(|r| {
                r.first()
                    .map(|c| Version(c.parts[0], c.parts[1], c.parts[2]))
            })
            .min()
    });
    let cli = versions::cli();
    let (target, latest) = target(args, offline)?;
    if let Some(f) = from.filter(|f| target < *f) {
        return Err(Failure::new(
            Code::UPGRADE_TARGET,
            format!("giong {target} is older than the project's giong {f}"),
            format!(
                "giong upgrade only moves forward: --to {f} or newer. To go back, restore \
                 Cargo.toml and Cargo.lock from version control"
            ),
        ));
    }

    let mut r = Report::new("upgrade");
    r.data("self", json!(false));
    r.data("from", json!(from.map(|v| v.to_string())));
    r.data("to", json!(target.to_string()));
    r.data("dry_run", json!(dry));
    if let Some(l) = &latest {
        r.data("latest", l.to_json());
    }

    // This command first, when the project is to move past it.
    let cli_from = std::env::var(CLI_FROM_ENV).ok();
    if target > cli {
        if cli_from.is_some() {
            return Err(Failure::new(
                Code::CLI_OUTDATED,
                format!("the giong command just installed is {cli}, not {target}"),
                format!(
                    "another giong may come first on the PATH (which giong / where giong); or \
                     giong upgrade --to {cli}"
                ),
            ));
        }
        let exe = this_exe()?;
        let method = selfupdate::detect(&exe);
        let mut cli_data = method.to_json();
        cli_data["from"] = json!(cli.to_string());
        cli_data["to"] = json!(target.to_string());
        if dry {
            r.data("cli", cli_data);
            r.line(format!(
                "Would first update this giong command from {cli} to {target}: {}.",
                method.plan(target)
            ));
            if let Method::Unknown { .. } = method {
                r.push(unknown_install(&method, target).fix(format!(
                    "{}; or giong upgrade --to {cli} moves the project to this command's release",
                    selfupdate::by_hand(target)
                )));
            }
        } else {
            if std::env::var_os(super::mcp::TOOL_ENV).is_some() {
                return Err(Failure::new(
                    Code::SELF_UPDATE,
                    format!(
                        "giong {target} is newer than this command ({cli}); giong mcp does not \
                         replace the giong command"
                    ),
                    format!(
                        "ask the person to run giong upgrade; or move the project to this \
                         command's release with --to {cli}"
                    ),
                ));
            }
            if let Method::Unknown { .. } = method {
                return Err(Failure::new(
                    Code::SELF_UPDATE,
                    format!(
                        "giong {target} is newer than this command ({cli}), which giong cannot \
                         update: it does not know how it was installed"
                    ),
                    format!(
                        "{}, then run giong upgrade again; or giong upgrade --to {cli} moves the \
                         project to this command's release",
                        selfupdate::by_hand(target)
                    ),
                ));
            }
            selfupdate::tidy(&exe);
            selfupdate::install(&exe, &method, target)?;
            check_installed(&exe, target)?;
            eprintln!("giong: updated this command to {target}; it moves the project now");
            let status = std::process::Command::new(&exe)
                .args(std::env::args_os().skip(1))
                .env(CLI_FROM_ENV, cli.to_string())
                .status();
            match status {
                Ok(s) => std::process::exit(s.code().unwrap_or(2)),
                Err(e) => {
                    return Err(Failure::new(
                        Code::SELF_UPDATE,
                        format!("the new giong command could not be started: {e}"),
                        "run giong upgrade again",
                    ))
                }
            }
        }
    }
    if let Some(f) = &cli_from {
        let exe = this_exe()?;
        let mut cli_data = selfupdate::detect(&exe).to_json();
        cli_data["from"] = json!(f);
        cli_data["to"] = json!(cli.to_string());
        r.data("cli", cli_data);
        r.line(format!("Updated the giong command from {f} to {cli}."));
    }

    // What to change.
    let mut pinned: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut changes: Vec<(usize, Change)> = Vec::new();
    for (i, d) in &registry {
        let req = d.req.as_deref().expect("filtered");
        match versions::parse_req(req) {
            None => unreadable.push(format!(
                "{} = \"{req}\" ({}:{})",
                d.key, files[*i].rel, d.line
            )),
            Some(parsed) => match versions::moved_req(req, target) {
                Some(new) if new != req => changes.push((
                    *i,
                    Change {
                        dep: d.clone(),
                        new_req: new,
                    },
                )),
                Some(_) => {}
                None if versions::allows(&parsed, target) => {}
                None => pinned.push(format!(
                    "{} = \"{req}\" ({}:{})",
                    d.key, files[*i].rel, d.line
                )),
            },
        }
    }
    if !unreadable.is_empty() {
        return Err(Failure::new(
            Code::MANIFEST_SHAPE,
            format!(
                "giong cannot read these giong requirements: {}",
                unreadable.join(", ")
            ),
            format!("write them as plain versions (\"{target}\"), then giong upgrade again"),
        ));
    }
    if !pinned.is_empty() {
        return Err(Failure::new(
            Code::PINNED_VERSION,
            format!(
                "these giong dependencies are pinned and do not allow {target}: {}",
                pinned.join(", ")
            ),
            format!(
                "a pin is a decision giong does not undo: change them to \"{target}\" by hand, or \
                 remove the pin, then giong upgrade again"
            ),
        ));
    }
    let edits: Vec<Value> = changes
        .iter()
        .map(|(i, c)| {
            json!({"file": files[*i].rel, "line": c.dep.line, "crate": c.dep.crate_name,
                   "key": c.dep.key, "table": c.dep.table,
                   "old": c.dep.req, "new": c.new_req})
        })
        .collect();
    r.data("edits", json!(edits));
    let lock_needs = locked.iter().any(|c| c.version != target) || locked.is_empty();
    if changes.is_empty() && !lock_needs {
        r.data("lock", lock_json(&lock));
        r.line(format!("The project is on giong {target} already."));
        return Ok(r);
    }
    for (i, c) in &changes {
        r.line(format!(
            "{} {}:{}: {} \"{}\" -> \"{}\"",
            if dry { "Would change" } else { "Changed" },
            files[*i].rel,
            c.dep.line,
            c.dep.key,
            c.dep.req.as_deref().unwrap_or(""),
            c.new_req
        ));
    }
    if dry {
        let crates: Vec<String> = locked.iter().map(|c| c.name.clone()).collect();
        r.line(format!(
            "Would then run cargo update for {} and cargo check.",
            if crates.is_empty() {
                "the giong crates".to_string()
            } else {
                crates.join(", ")
            }
        ));
        r.data("lock", lock_json(&lock));
        return Ok(r);
    }

    // Change them, keeping the originals.
    let mut touched: Vec<PathBuf> = changes
        .iter()
        .map(|(i, _)| files[*i].path.clone())
        .collect();
    touched.dedup();
    touched.push(root.join("Cargo.lock"));
    let saved = Saved::keep(&root, &touched);
    for (i, file) in files.iter().enumerate() {
        let mine: Vec<Change> = changes
            .iter()
            .filter(|(j, _)| *j == i)
            .map(|(_, c)| c.clone())
            .collect();
        if mine.is_empty() {
            continue;
        }
        let text = manifest::apply(&file.text, &mine);
        if let Err(e) = crate::state::replace(&file.path, text.as_bytes()) {
            return Err(undo(
                &saved,
                Failure::new(
                    Code::WRITE_FAILED,
                    format!("{} could not be written: {e}", file.rel),
                    "check that you may write to the project",
                ),
            ));
        }
    }

    // Every giong requirement, as cargo reads it now, allows the target.
    let after = match Project::at(&root) {
        Ok(p) => p,
        Err(f) => {
            return Err(undo(
                &saved,
                Failure::new(Code::MANIFEST_SHAPE, f.what, f.fix),
            ))
        }
    };
    let mut missed = Vec::new();
    for p in after.packages() {
        for d in p["dependencies"].as_array().into_iter().flatten() {
            let name = d["name"].as_str().unwrap_or("");
            let from_registry = d["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("registry+"))
                || (d["path"].is_null() && d["source"].is_null() && d["git"].is_null());
            if !versions::is_giong_crate(name) || !from_registry || !d["path"].is_null() {
                continue;
            }
            let req = d["req"].as_str().unwrap_or("*");
            if !versions::parse_req(req).is_some_and(|r| versions::allows(&r, target)) {
                missed.push(format!(
                    "{name} \"{req}\" in {}",
                    p["manifest_path"].as_str().unwrap_or("?")
                ));
            }
        }
    }
    if !missed.is_empty() {
        return Err(undo(
            &saved,
            Failure::new(
                Code::MANIFEST_SHAPE,
                format!(
                    "giong could not find where these are written: {}",
                    missed.join(", ")
                ),
                format!("change them to \"{target}\" by hand (giong upgrade --dry-run lists what it can edit)"),
            ),
        ));
    }

    // The lock: every giong crate to the target, exactly.
    let mut update = vec!["update".to_string()];
    if locked.is_empty() {
        update = vec!["generate-lockfile".to_string()];
    } else {
        for c in &locked {
            update.push("-p".into());
            update.push(format!("{}@{}", c.name, c.version));
        }
    }
    if let Err(e) = cargo_step(&root, &update, offline) {
        return Err(undo(
            &saved,
            Failure::new(
                Code::CARGO_FAILED,
                format!("cargo {} failed: {e}", update[0]),
                if offline {
                    format!("without the network, cargo needs giong {target} in its cache: run without --offline")
                } else {
                    "fix what cargo reports, then giong upgrade again".to_string()
                },
            ),
        ));
    }
    let now = read_lock(&root);
    let above: Vec<String> = versions::giong_from_registry(&now)
        .iter()
        .filter(|c| c.version > target)
        .map(|c| c.name.clone())
        .collect();
    for name in dependents_first(&now, &above) {
        let current = read_lock(&root);
        let Some(c) = current
            .iter()
            .find(|c| c.name == name && c.version > target)
        else {
            continue;
        };
        let step = vec![
            "update".to_string(),
            "-p".into(),
            format!("{}@{}", c.name, c.version),
            "--precise".into(),
            target.to_string(),
        ];
        if let Err(e) = cargo_step(&root, &step, offline) {
            return Err(undo(
                &saved,
                Failure::new(
                    Code::CARGO_FAILED,
                    format!("cargo update -p {name} --precise {target} failed: {e}"),
                    "fix what cargo reports, then giong upgrade again",
                ),
            ));
        }
    }
    let now = read_lock(&root);
    let off: Vec<String> = versions::giong_from_registry(&now)
        .iter()
        .filter(|c| c.version != target)
        .map(|c| format!("{} {}", c.name, c.version))
        .collect();
    if !off.is_empty() {
        let name = off[0].split(' ').next().unwrap_or("giong").to_string();
        return Err(undo(
            &saved,
            Failure::new(
                Code::GIONG_MIXED,
                format!(
                    "after cargo update, these giong crates are not on {target}: {}",
                    off.join(", ")
                ),
                format!(
                    "another dependency asks for them (cargo tree -i {name} shows which): update \
                     it, then giong upgrade again"
                ),
            ),
        ));
    }

    // The release's rust-version: rustc, and the project's own promise.
    if let Err(f) = msrv(&root, target, offline) {
        return Err(undo(&saved, f));
    }

    // It builds.
    let mut check_args = vec!["--workspace".to_string(), "--all-targets".to_string()];
    if offline {
        check_args.push("--offline".into());
    }
    let outcome = match build::run(&root, "check", &check_args) {
        Ok(o) => o,
        Err(f) => return Err(undo(&saved, f)),
    };
    if !outcome.ok {
        let errors = saved.restore();
        for f in outcome
            .findings
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
        {
            r.push(f);
        }
        r.data("rolled_back", json!(errors.is_empty()));
        r.line(format!(
            "The project does not build on giong {target}: the files are put back as they were{}.",
            if errors.is_empty() {
                String::new()
            } else {
                format!(
                    " except {} (originals kept in .giong/upgrade/)",
                    errors.join(", ")
                )
            }
        ));
        r.line("If it did not build before either, giong check shows why.");
        return Ok(r);
    }
    saved.discard();
    r.data("lock", lock_json(&now));
    r.data("rolled_back", json!(false));
    r.line(format!(
        "The project is on giong {target} (from {}); cargo check passes.",
        from.map_or("no lock".to_string(), |f| f.to_string())
    ));

    // The agent files describe the release the project is on.
    if has_agent_files(&root) {
        let out = super::ai::init_dir(&root, None);
        if !out.written.is_empty() {
            r.line(format!(
                "Refreshed the agent files: {}",
                out.written.join(", ")
            ));
        }
        r.data("agent_files", json!(out.written));
        for f in out.findings {
            r.push(f);
        }
    }
    r.line(format!(
        "What is new: {}",
        super::doctor::changelog_url(target)
    ));
    Ok(r)
}

fn lock_json(lock: &[versions::Locked]) -> Value {
    let mut m = serde_json::Map::new();
    for c in versions::giong_from_registry(lock) {
        m.insert(c.name.clone(), json!(c.version.to_string()));
    }
    Value::Object(m)
}

/// The highest `rust-version` of the giong crates at `target` against this
/// rustc (in the project, so a rust-toolchain file counts) and against the
/// `rust-version` of the project's own packages.
fn msrv(root: &Path, target: Version, offline: bool) -> Result<(), Failure> {
    let args = vec![
        "metadata".to_string(),
        "--format-version".into(),
        "1".into(),
    ];
    let meta: Value = match cargo_step(root, &args, offline) {
        Ok(out) => serde_json::from_str(&out).unwrap_or(Value::Null),
        Err(e) => {
            return Err(Failure::new(
                Code::CARGO_FAILED,
                format!("cargo metadata failed: {e}"),
                "fix what cargo reports, then giong upgrade again",
            ))
        }
    };
    let packages = meta["packages"].as_array().cloned().unwrap_or_default();
    let need = packages
        .iter()
        .filter(|p| {
            versions::is_giong_crate(p["name"].as_str().unwrap_or(""))
                && p["version"].as_str() == Some(target.to_string().as_str())
        })
        .filter_map(|p| host::parse_version(p["rust_version"].as_str()?))
        .max();
    let Some(need) = need else { return Ok(()) };
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    if let Some(have) =
        host::output_in(root, &rustc, &["--version"]).and_then(|o| host::parse_version(&o))
    {
        if have < need {
            return Err(Failure::new(
                Code::RUST_TOO_OLD,
                format!("giong {target} needs Rust {need}; this rustc is {have}"),
                "rustup update stable (or change the project's rust-toolchain file), then giong \
                 upgrade again",
            ));
        }
    }
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for p in &packages {
        if !members.contains(&p["id"].as_str().unwrap_or("")) {
            continue;
        }
        if let Some(theirs) = p["rust_version"].as_str().and_then(host::parse_version) {
            if theirs < need {
                return Err(Failure::new(
                    Code::MSRV_BELOW_TARGET,
                    format!(
                        "{} promises Rust {theirs} (rust-version), and giong {target} needs {need}",
                        p["manifest_path"].as_str().unwrap_or("a package")
                    ),
                    format!(
                        "raise its rust-version to \"{}.{}\" if you can drop Rust {theirs}, then \
                         giong upgrade again; or stay on the release you have",
                        need.0, need.1
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependents_move_before_what_they_depend_on() {
        let lock = versions::parse_lock(
            "[[package]]\nname = \"giong\"\nversion = \"1.6.0\"\nsource = \"registry+x\"\n\n\
             [[package]]\nname = \"giong-http\"\nversion = \"1.6.0\"\nsource = \"registry+x\"\ndependencies = [\n \"giong\",\n]\n\n\
             [[package]]\nname = \"giong-web\"\nversion = \"1.6.0\"\nsource = \"registry+x\"\ndependencies = [\n \"giong\",\n \"giong-http\",\n]\n",
        );
        let names: Vec<String> = ["giong", "giong-http", "giong-web"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            dependents_first(&lock, &names),
            ["giong-web", "giong-http", "giong"]
        );
    }
}
