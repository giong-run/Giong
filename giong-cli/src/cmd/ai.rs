//! `giong ai init`: teaches the coding agents of a project how to write Gióng
//! code: `AGENTS.md` (read by most agents), `CLAUDE.md` (which imports it),
//! a Cursor rule, the Gióng skill of this release, and the MCP servers in
//! `.mcp.json`.
//!
//! Safe to run again and again: in a file someone else wrote it only adds or
//! replaces its own marked block (`<!-- giong:begin -->` ... `<!-- giong:end -->`),
//! a whole file it rewrites only when the file carries its `<!-- giong:owned -->`
//! line near its top, and in `.mcp.json` it adds its server when it is missing
//! and touches nothing else. A file it cannot read (not UTF-8, no permission)
//! is never treated as missing, and `CLAUDE.md` linked to `AGENTS.md` is left
//! to the link.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::project::Project;
use crate::report::{Failure, Finding, Report, Severity};
use crate::scaffold::GIONG_VERSION;

pub const COMMAND: Command = Command {
    name: "ai",
    summary: "teach coding agents this project: AGENTS.md, CLAUDE.md, Cursor, skill, MCP",
    usage: "giong ai init [--dry-run]\n\n\
            Writes, at the root of the project: AGENTS.md (the Gióng rules and this project's \
            commands, read by Codex, Copilot, Cursor, Gemini and others), CLAUDE.md (imports \
            AGENTS.md for Claude Code), .cursor/rules/giong.mdc, the Gióng skill of this release \
            in .claude/skills/giong/, and the giong MCP server in .mcp.json (giong mcp over stdio, \
            which also reaches the service giong dev runs; written once this giong has the mcp \
            command). The files name only the giong commands this build has. Run it again at \
            any time: it replaces only what it wrote (a marked block, or a file with its \
            giong:owned line) and never changes anything else. giong new runs it.",
    flags: &[Flag {
        name: "dry-run",
        kind: Kind::Switch,
        help: "say what would change without writing",
    }],
    max_words: 1,
    run,
};

/// The skill of this release (`skills/giong/SKILL.md`, embedded).
pub const SKILL: &str = include_str!("../../assets/skill/SKILL.md");

const BEGIN: &str = "<!-- giong:begin";
const END: &str = "<!-- giong:end -->";
const OWNED: &str = "<!-- giong:owned";

/// What `ai init` did in a directory.
#[derive(Debug, Default)]
pub struct Outcome {
    pub findings: Vec<Finding>,
    /// Files created or changed, relative to the project, with `/`.
    pub written: Vec<String>,
    /// Files that were already current.
    pub unchanged: Vec<String>,
}

/// What should happen to one file.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    Write(String),
    Same,
    /// Left as it is, with the finding that says why.
    Keep(Finding),
}

/// The giong commands this build has: the agent files name only those, so an
/// agent is never told to run a command that does not exist yet.
#[derive(Clone, Copy, Debug)]
pub struct Offer {
    pub mcp: bool,
    pub dev: bool,
    /// `giong dev --background` with `status`, `logs` and `stop`: the one
    /// runner an agent starts once and keeps using.
    pub dev_background: bool,
    pub check: bool,
    pub fix: bool,
    pub test: bool,
    pub doctor: bool,
    pub db: bool,
    pub upgrade: bool,
}

impl Offer {
    pub fn here() -> Offer {
        let has = |name: &str| super::COMMANDS.iter().any(|c| c.name == name);
        Offer {
            mcp: has("mcp"),
            dev: has("dev"),
            dev_background: super::COMMANDS
                .iter()
                .any(|c| c.name == "dev" && c.flags.iter().any(|f| f.name == "background")),
            check: has("check"),
            fix: has("fix"),
            test: has("test"),
            doctor: has("doctor"),
            db: has("db"),
            upgrade: has("upgrade"),
        }
    }

    #[cfg(test)]
    pub fn all() -> Offer {
        Offer {
            mcp: true,
            dev: true,
            dev_background: true,
            check: true,
            fix: true,
            test: true,
            doctor: true,
            db: true,
            upgrade: true,
        }
    }
}

/// The rows of the commands table, for the commands this build has.
fn command_rows(offer: &Offer) -> String {
    let mut rows = String::from("| To | Run |\n|---|---|\n");
    let mut row = |when: bool, to: &str, run: &str| {
        if when {
            rows.push_str(&format!("| {to} | {run} |\n"));
        }
    };
    row(
        offer.dev,
        "Run the service, rebuilt on every change",
        if offer.dev_background {
            "`giong dev --background`, once (see Local development)"
        } else {
            "`giong dev`"
        },
    );
    row(
        offer.check,
        "Check a change before finishing",
        if offer.fix {
            "`giong check --json`, then `giong fix` for what it can fix"
        } else {
            "`giong check --json`"
        },
    );
    row(
        true,
        "Run the tests",
        if offer.test {
            "`giong test` (or `cargo test`, then `GIONG_DRIVER=epoll cargo test`)"
        } else {
            "`cargo test`, then `GIONG_DRIVER=epoll cargo test`"
        },
    );
    row(true, "Explain an error code", "`giong explain <code>`");
    row(
        offer.doctor,
        "Check the machine and the project",
        "`giong doctor`",
    );
    row(
        offer.db,
        "Start the database, apply migrations",
        "`giong db up`, `giong db migrate`",
    );
    row(
        offer.upgrade,
        "Move to the newest giong release",
        "`giong upgrade` (`--dry-run` first; never edit giong versions by hand)",
    );
    rows
}

/// How to work on the service: one `giong dev` per project, started once and
/// kept, the way `npm run dev` is.
fn dev_flow(offer: &Offer) -> &'static str {
    if !offer.dev_background {
        return "";
    }
    "\n### Local development\n\n\
     One project folder, one runner, on this machine, like `npm run dev`:\n\n\
     1. At the start, run `giong dev --background`. It builds, starts the service, waits until\n   \
     it serves, prints its address (`http://127.0.0.1:PORT`) and returns; if one already runs\n   \
     for this project, it reports that one. A failed build says why (`GIONG-E5009`) and keeps\n   \
     watching: fix the code.\n\
     2. Edit the code. `giong dev` rebuilds and swaps the service by itself, without refusing\n   \
     a connection.\n\
     3. Try it with `curl` on the printed address.\n\
     4. `giong dev status --json` says whether it is building, serving or failed (`state`), its\n   \
     `urls` and the last build's first error (`build.error`); `giong dev logs --lines 100` shows\n   \
     the service's output and requests.\n\
     5. `giong dev stop` when you are done. `--all` on `status` and `stop` covers every project\n   \
     on this machine.\n\n\
     Do not run the service with `cargo run`, `cargo build --release`, Docker or a second\n\
     terminal. One `giong dev` per project.\n"
}

fn agents_block(version: &str, offer: &Offer) -> String {
    let rows = command_rows(offer);
    let dev = dev_flow(offer);
    let mcp = if offer.mcp {
        "\nThe `giong` MCP server in `.mcp.json` serves the commands that only read (`check`, `doctor`,\n\
         `explain`, ...) as tools; `fix` and the other changes join them only under\n\
         `giong mcp --allow-writes`. While `giong dev` runs the service, it also serves the service's\n\
         own view of itself (workers, tasks by spawn line, pools, a blocked worker): ask it rather\n\
         than guess why the service is slow or stuck.\n"
    } else {
        ""
    };
    format!(
        r#"{BEGIN} (written by giong ai init: edit outside this block, it is replaced) -->
## Gióng

This project runs on [Gióng](https://giong.run) {version}, a thread-per-core async runtime
for Rust: one worker per core, nothing shared between workers, no Tokio underneath. The
skill in `.claude/skills/giong/SKILL.md` has every rule; the ones that matter most:

- **No macros.** `giong::Builder::new().shutdown_on_signals(true).build()?.run(|_worker| async {{ .. }})`
  starts a future per worker; tests use `giong::block_on(async {{ .. }})` in a plain `#[test]`.
- **Per worker, inside the closure**: listeners, routers, database pools, HTTP clients.
  Futures need not be `Send`; per-worker state is `Rc<RefCell<T>>`, never borrowed across
  an `.await`.
- **Never block a worker**: `giong::fs`, `giong::net`, `giong::time`, never `std::fs`,
  `std::net` or `std::thread::sleep` on a worker. Heavy work goes to `std::thread::spawn`
  and comes back through `giong::sync::oneshot`.
- **`bind` takes IP addresses** (`127.0.0.1:8080`), never `localhost`.
- **Every wait for another party has a deadline**: `giong::time::timeout(duration, fut)`.
- **Errors carry a code** (`GIONG-E0102: ... Fix: ...`). Match on `giong::error::code(&err)`,
  never on the text; `giong explain GIONG-E0102` prints the cause and the fix.
- **The giong crate for the job**: giong-web (not axum), giong-http's client (not reqwest),
  giong-postgres and giong-mysql (not sqlx), giong-redis (not redis-rs), giong-grpc (not
  tonic). A crate that needs the Tokio runtime panics on a worker.

{rows}{dev}{mcp}{END}
"#
    )
}

const CLAUDE_BLOCK: &str =
    "<!-- giong:begin (written by giong ai init: edit outside this block, it is replaced) -->\n\
@AGENTS.md\n\
<!-- giong:end -->\n";

fn cursor_rule(offer: &Offer) -> String {
    let finish = match (offer.check, offer.fix) {
        (true, true) => {
            " Before finishing a\nchange, run `giong check --json` and fix what it reports (`giong fix` applies the\nmechanical fixes).\n"
        }
        (true, false) => {
            " Before finishing a\nchange, run `giong check --json` and fix what it reports.\n"
        }
        _ => "\n",
    };
    let dev = if offer.dev_background {
        "Run the service with `giong dev --background`, once, and keep it: it rebuilds on\nevery change. Never `cargo run`, Docker or a release build for it.\n"
    } else {
        ""
    };
    format!(
        "---\n\
         description: Gióng rules for this Rust project\n\
         alwaysApply: true\n\
         ---\n\
         <!-- giong:owned (giong ai init rewrites this file; delete this line to keep your edits) -->\n\n\
         This project runs on Gióng, a thread-per-core async runtime for Rust. Read `AGENTS.md` at\n\
         the root before changing code: it has the rules and the commands.{finish}{dev}"
    )
}

/// The skill, marked as written by `ai init` and stamped with the version.
fn skill(version: &str) -> String {
    let stamp = format!(
        "{OWNED} (giong ai init installed this skill from giong {version}; delete this line to \
         keep your edits) -->\n"
    );
    // After the front matter, which must stay the first thing in the file.
    match SKILL
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n"))
    {
        Some(end) => {
            let split = 4 + end + 5;
            let built = format!("\nBuilt from giong {version}.\n");
            let body = SKILL[split..].replacen("\n# giong\n", &format!("\n# giong\n{built}"), 1);
            format!("{}{stamp}{body}", &SKILL[..split])
        }
        None => format!("{stamp}{SKILL}"),
    }
}

/// The byte ranges of giong's blocks in `text` (each from its begin marker to
/// just past its end marker and line break), or `None` when a block is not
/// closed or ends before it begins.
fn blocks(text: &str) -> Option<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut from = 0;
    loop {
        let begin = text[from..].find(BEGIN).map(|i| from + i);
        let end = text[from..].find(END).map(|i| from + i);
        match (begin, end) {
            (None, None) => return Some(out),
            (Some(b), Some(e)) if b < e => {
                // No second begin inside this block.
                if text[b + BEGIN.len()..e].contains(BEGIN) {
                    return None;
                }
                let mut after = e + END.len();
                if text[after..].starts_with("\r\n") {
                    after += 2;
                } else if text[after..].starts_with('\n') {
                    after += 1;
                }
                out.push((b, after));
                from = after;
            }
            _ => return None,
        }
    }
}

/// Puts `block` into `existing`: in place of the first marked block (later
/// copies are dropped), or at the end. A file with CRLF line ends gets the
/// block with CRLF too.
fn merge_block(existing: Option<&str>, block: &str, new_file: &str, path: &str) -> Change {
    let Some(text) = existing else {
        return Change::Write(new_file.to_string());
    };
    let block = if text.contains("\r\n") {
        block.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        block.to_string()
    };
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let Some(found) = blocks(text) else {
        return Change::Keep(
            Finding::new(
                Code::AGENT_FILE_FOREIGN,
                Severity::Warning,
                format!("{path} has a giong block that is not closed"),
            )
            .detail(format!(
                "{path} has `{BEGIN}` without `{END}` after it (or the other way round), so \
                 giong cannot tell which part is its own; the file was left as it is."
            ))
            .fix(format!(
                "delete the giong lines from {path}, or close the block, and run giong ai \
                 init again"
            ))
            .at(path, 1, 1),
        );
    };
    let merged = if found.is_empty() {
        let sep = if text.is_empty() || text.ends_with(&format!("{nl}{nl}")) {
            String::new()
        } else if text.ends_with('\n') {
            nl.to_string()
        } else {
            format!("{nl}{nl}")
        };
        format!("{text}{sep}{block}")
    } else {
        let mut merged = String::with_capacity(text.len() + block.len());
        let mut at = 0;
        for (i, (b, e)) in found.iter().enumerate() {
            merged.push_str(&text[at..*b]);
            if i == 0 {
                merged.push_str(&block);
            }
            at = *e;
        }
        merged.push_str(&text[at..]);
        merged
    };
    if merged == text {
        Change::Same
    } else {
        Change::Write(merged)
    }
}

/// Whether a file carries the owned marker at the start of a line near its
/// top (after a front matter at most), not just anywhere in its text.
fn is_owned(text: &str) -> bool {
    text.lines().take(12).any(|l| l.starts_with(OWNED))
}

/// A file `ai init` owns entirely: written when missing or still marked.
fn owned_file(existing: Option<&str>, content: String, path: &str) -> Change {
    match existing {
        None => Change::Write(content),
        Some(text) if text == content => Change::Same,
        Some(text) if is_owned(text) => Change::Write(content),
        Some(_) => Change::Keep(
            Finding::new(
                Code::AGENT_FILE_FOREIGN,
                Severity::Warning,
                format!("{path} was not written by giong and was left as it is"),
            )
            .detail(format!(
                "{path} exists without the `{OWNED}` line, so it is yours: giong does not \
                 replace it."
            ))
            .fix(format!(
                "to get giong's version, move {path} away and run giong ai init again"
            ))
            .at(path, 1, 1),
        ),
    }
}

/// Adds giong's MCP servers to `.mcp.json`, keeping everything else.
fn merge_mcp(existing: Option<&str>) -> Change {
    // One server: `giong mcp` serves this project's commands and, while
    // `giong dev` runs the service, forwards the service's own giong-mcp tools.
    let servers = [("giong", json!({"command": "giong", "args": ["mcp"]}))];
    let invalid = |why: String| {
        Change::Keep(
            Finding::new(
                Code::MCP_JSON_INVALID,
                Severity::Error,
                ".mcp.json could not be read, so the giong MCP server was not added",
            )
            .detail(why)
            .fix(
                "fix .mcp.json (it must be one JSON object, with the servers under \
                 \"mcpServers\"), or move it away, then run giong ai init again",
            )
            .at(".mcp.json", 1, 1),
        )
    };
    // A byte order mark (some Windows editors write one) is kept as it was.
    let bom = existing.is_some_and(|t| t.starts_with('\u{feff}'));
    let existing = existing.map(|t| t.strip_prefix('\u{feff}').unwrap_or(t));
    let mut doc = match existing {
        None => Value::Object(Map::new()),
        Some(text) if text.trim().is_empty() => Value::Object(Map::new()),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(v) => v,
            Err(e) => return invalid(format!("it is not valid JSON: {e}")),
        },
    };
    let Some(root) = doc.as_object_mut() else {
        return invalid("it holds JSON, but not an object".into());
    };
    let list = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(list) = list.as_object_mut() else {
        return invalid("\"mcpServers\" is not an object".into());
    };
    let mut added = false;
    for (name, server) in servers {
        if !list.contains_key(name) {
            list.insert(name.to_string(), server);
            added = true;
        }
    }
    if !added {
        return Change::Same;
    }
    let mut text = if bom {
        "\u{feff}".to_string()
    } else {
        String::new()
    };
    text.push_str(&serde_json::to_string_pretty(&doc).expect("a Value always serialises"));
    text.push('\n');
    Change::Write(text)
}

/// A file of the plan as it is on disk.
enum OnDisk {
    Missing,
    Text(String),
    /// It exists but could not be read (not UTF-8, no permission): never
    /// treated as missing, so never overwritten.
    Unreadable(String),
}

fn on_disk(path: &Path) -> OnDisk {
    match fs::read_to_string(path) {
        Ok(text) => OnDisk::Text(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OnDisk::Missing,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            OnDisk::Unreadable("it is not UTF-8 text".into())
        }
        Err(e) => OnDisk::Unreadable(e.to_string()),
    }
}

fn unreadable(rel: &str, why: &str) -> Change {
    Change::Keep(
        Finding::new(
            Code::AGENT_FILE_FOREIGN,
            Severity::Warning,
            format!("{rel} could not be read and was left as it is"),
        )
        .detail(format!(
            "{rel} exists, but {why}, so giong cannot tell what is in it and does not touch it."
        ))
        .fix(format!(
            "save {rel} as UTF-8 (or make it readable), then run giong ai init again"
        ))
        .at(rel, 1, 1),
    )
}

/// Whether two paths name the same file (a link to the other, or a hard
/// link), also when the link points at a file that does not exist yet.
fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (fs::canonicalize(a), fs::canonicalize(b)) {
        return a == b;
    }
    let links_to = |link: &Path, target: &Path| {
        let Ok(to) = fs::read_link(link) else {
            return false;
        };
        let to = link.parent().map_or(to.clone(), |dir| dir.join(&to));
        match (
            to.parent().map(fs::canonicalize),
            target.parent().map(fs::canonicalize),
        ) {
            (Some(Ok(d1)), Some(Ok(d2))) => d1 == d2 && to.file_name() == target.file_name(),
            _ => false,
        }
    };
    links_to(a, b) || links_to(b, a)
}

/// A link giong will not write through: a cloned repository can commit
/// `.mcp.json -> ~/some/config`, and git checks it out as a link.
fn linked(rel: &str, what: &str) -> Change {
    Change::Keep(
        Finding::new(
            Code::AGENT_FILE_FOREIGN,
            Severity::Warning,
            format!("{rel} was left as it is: {what} is a symbolic link"),
        )
        .detail(format!(
            "giong writes its agent files only as plain files in the project: a link \
             can name a file outside it, so giong does not write through {what}. The one \
             link it keeps is CLAUDE.md and AGENTS.md naming each other."
        ))
        .fix(format!(
            "replace {what} with a plain file (or folder), then run giong ai init again"
        ))
        .at(rel, 1, 1),
    )
}

/// Whether `path` is a symbolic link (not followed).
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Where `rel` is written, or the link (file or folder) that stops it. Never
/// through a link, except that AGENTS.md and CLAUDE.md may name each other
/// (then the file they share is written, under its own name). A folder on the
/// way may be a link only to a folder inside the project.
fn target(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut folder = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        folder.push(part);
        if is_link(&folder) {
            let inside = fs::canonicalize(&folder)
                .is_ok_and(|to| to.starts_with(&canonical_root) && to.is_dir());
            if !inside {
                let shown = folder.strip_prefix(root).unwrap_or(&folder);
                return Err(shown.display().to_string().replace('\\', "/"));
            }
        }
    }
    let path = root.join(rel);
    if !is_link(&path) {
        return Ok(path);
    }
    let other = match rel {
        "AGENTS.md" => "CLAUDE.md",
        "CLAUDE.md" => "AGENTS.md",
        _ => return Err(rel.to_string()),
    };
    let other_path = root.join(other);
    if same_file(&path, &other_path) && !is_link(&other_path) {
        Ok(other_path)
    } else {
        Err(rel.to_string())
    }
}

/// The changes for a project root, without writing anything: each file's
/// name, the path it is written to, and what happens to it.
fn plan(root: &Path, version: &str, offer: &Offer) -> Vec<(String, PathBuf, Change)> {
    let block = agents_block(version, offer);
    let agents_new = format!(
        "# AGENTS.md\n\nInstructions for coding agents working on this project.\n\n{block}\n\
         ## This project\n\nAdd what an agent should know about this project here, outside the \
         giong block.\n"
    );
    let claude_new = format!("# CLAUDE.md\n\n{CLAUDE_BLOCK}");
    let each = |rel: &str, change: &dyn Fn(Option<&str>) -> Change| {
        let path = match target(root, rel) {
            Ok(path) => path,
            Err(link) => return (rel.to_string(), root.join(rel), linked(rel, &link)),
        };
        let c = match on_disk(&path) {
            OnDisk::Missing => change(None),
            OnDisk::Text(t) => change(Some(&t)),
            OnDisk::Unreadable(why) => unreadable(rel, &why),
        };
        (rel.to_string(), path, c)
    };
    let mut out = vec![each("AGENTS.md", &|t| {
        merge_block(t, &block, &agents_new, "AGENTS.md")
    })];
    if same_file(&root.join("CLAUDE.md"), &root.join("AGENTS.md")) {
        // CLAUDE.md links to AGENTS.md (or the other way round): Claude Code
        // reads the rules through the link, and an import would point the
        // file at itself.
        out.push((
            "CLAUDE.md".to_string(),
            root.join("CLAUDE.md"),
            Change::Same,
        ));
    } else {
        out.push(each("CLAUDE.md", &|t| {
            merge_block(t, CLAUDE_BLOCK, &claude_new, "CLAUDE.md")
        }));
    }
    out.push(each(".cursor/rules/giong.mdc", &|t| {
        owned_file(t, cursor_rule(offer), ".cursor/rules/giong.mdc")
    }));
    out.push(each(".claude/skills/giong/SKILL.md", &|t| {
        owned_file(t, skill(version), ".claude/skills/giong/SKILL.md")
    }));
    if offer.mcp {
        out.push(each(".mcp.json", &merge_mcp));
    }
    out
}

/// Applies the changes. `version` is the giong version the project uses, when
/// known (giong new); otherwise it is read from the project.
pub fn init_dir(root: &Path, version: Option<&str>) -> Outcome {
    apply(
        root,
        version.unwrap_or(GIONG_VERSION),
        false,
        &Offer::here(),
    )
}

/// The files `init_dir` would write in an empty directory.
pub fn files_for_new() -> Vec<String> {
    let mut files: Vec<String> = [
        "AGENTS.md",
        "CLAUDE.md",
        ".cursor/rules/giong.mdc",
        ".claude/skills/giong/SKILL.md",
    ]
    .map(String::from)
    .to_vec();
    if Offer::here().mcp {
        files.push(".mcp.json".into());
    }
    files
}

fn apply(root: &Path, version: &str, dry_run: bool, offer: &Offer) -> Outcome {
    let mut out = Outcome::default();
    for (rel, path, change) in plan(root, version, offer) {
        match change {
            Change::Same => out.unchanged.push(rel),
            Change::Keep(finding) => out.findings.push(finding),
            Change::Write(text) => {
                if !dry_run {
                    // Written aside and renamed over the file: a link that
                    // appeared since the plan is replaced, never followed.
                    let result = path
                        .parent()
                        .map_or(Ok(()), fs::create_dir_all)
                        .and_then(|()| crate::state::replace(&path, text.as_bytes()));
                    if let Err(e) = result {
                        out.findings.push(
                            Finding::new(
                                Code::WRITE_FAILED,
                                Severity::Error,
                                format!("{rel} could not be written"),
                            )
                            .detail(e.to_string())
                            .fix("check that you may write to the project directory"),
                        );
                        continue;
                    }
                }
                out.written.push(rel);
            }
        }
    }
    out
}

/// The `major.minor` of a version requirement such as `"1.5.0"`, `"^1.5"` or
/// `"=1.5.2"`; `None` for `*` and the like.
fn major_minor(req: &str) -> Option<(u64, u64)> {
    let first = req.split(',').next()?.trim();
    let v = first.trim_start_matches(['^', '=', '~', '>', '<', ' ']);
    let mut parts = v.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.trim().parse().ok())?;
    Some((major, minor))
}

/// The giong version the project asks for, from `cargo metadata`: the
/// requirement of the first workspace package that depends on `giong`.
fn giong_requirement(project: &Project) -> Option<String> {
    project.packages().find_map(|p| {
        p["dependencies"].as_array()?.iter().find_map(|d| {
            (d["name"] == "giong" && d["kind"].is_null())
                .then(|| d["req"].as_str().unwrap_or("*").to_string())
        })
    })
}

fn run(args: &Args) -> Result<Report, Failure> {
    match args.words.first().map(String::as_str) {
        Some("init") => {}
        Some(other) => {
            return Err(Failure::new(
                Code::USAGE,
                format!("giong ai has no subcommand {other:?}"),
                "write giong ai init",
            ))
        }
        None => {
            return Err(Failure::new(
                Code::USAGE,
                "giong ai needs a subcommand",
                "write giong ai init",
            ))
        }
    }
    let project = Project::here()?;
    let mut r = Report::new("ai");
    let cli = major_minor(GIONG_VERSION).expect("our own version parses");
    match giong_requirement(&project) {
        None => r.push(
            Finding::new(
                Code::NOT_GIONG_PROJECT,
                Severity::Warning,
                "the project does not depend on giong",
            )
            .detail("No package of the workspace has giong in [dependencies]; the agent files were written anyway.")
            .fix("cargo add giong, or start a project with giong new"),
        ),
        Some(req) => {
            r.data("giong_requirement", json!(req));
            if let Some(theirs) = major_minor(&req).filter(|v| *v != cli) {
                r.push(
                    Finding::new(
                        Code::VERSION_MISMATCH,
                        Severity::Warning,
                        format!(
                            "the project uses giong {}.{}, this command comes with {}",
                            theirs.0, theirs.1, GIONG_VERSION
                        ),
                    )
                    .detail(
                        "The skill and the rules written describe giong ".to_string()
                            + GIONG_VERSION
                            + ": an agent may use what the project's version does not have, or miss what it has.",
                    )
                    .fix(format!(
                        "install the matching command (cargo install giong-cli --version ~{}.{}), \
                         or move the project to giong {GIONG_VERSION}",
                        theirs.0, theirs.1
                    )),
                );
            }
        }
    }
    let out = apply(
        &project.root,
        GIONG_VERSION,
        args.has("dry-run"),
        &Offer::here(),
    );
    for f in out.findings {
        r.push(f);
    }
    r.data("root", json!(project.root.display().to_string()));
    r.data("written", json!(out.written));
    r.data("unchanged", json!(out.unchanged));
    r.data("dry_run", json!(args.has("dry-run")));
    let verb = if args.has("dry-run") {
        "Would write"
    } else {
        "Wrote"
    };
    if out.written.is_empty() {
        r.line("The agent files are current.");
    } else {
        r.line(format!("{verb}: {}", out.written.join(", ")));
    }
    if !out.unchanged.is_empty() {
        r.line(format!("Already current: {}", out.unchanged.join(", ")));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giong-ai-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(dir: &Path, rel: &str) -> String {
        fs::read_to_string(dir.join(rel)).unwrap()
    }

    /// ai init with every command offered, whatever this build has.
    fn every(dir: &Path) -> Outcome {
        apply(dir, GIONG_VERSION, false, &Offer::all())
    }

    /// A file a person saved in Latin-1, and an .mcp.json with a server of
    /// theirs in it: neither is UTF-8, so neither is touched.
    #[test]
    fn files_that_are_not_utf8_are_left_alone() {
        let dir = scratch("latin1");
        let agents = b"# Mes r\xe8gles\nNe pas effacer.\n".to_vec();
        let mcp = b"{\"mcpServers\":{\"db\":{\"command\":\"psql\",\"note\":\"caf\xe9\"}}}".to_vec();
        fs::write(dir.join("AGENTS.md"), &agents).unwrap();
        fs::write(dir.join(".mcp.json"), &mcp).unwrap();
        let out = every(&dir);
        assert_eq!(fs::read(dir.join("AGENTS.md")).unwrap(), agents);
        assert_eq!(fs::read(dir.join(".mcp.json")).unwrap(), mcp);
        let unreadable = out
            .findings
            .iter()
            .filter(|f| f.code == Code::AGENT_FILE_FOREIGN && f.title.contains("could not be read"))
            .count();
        assert_eq!(unreadable, 2, "{:?}", out.findings);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// CLAUDE.md as a link to AGENTS.md: AGENTS.md gets the rules once, no
    /// import of itself, and a second run changes nothing.
    #[cfg(unix)]
    #[test]
    fn claude_md_linked_to_agents_md() {
        for (link, target, exists) in [
            ("CLAUDE.md", "AGENTS.md", true),
            ("AGENTS.md", "CLAUDE.md", true),
            // A link to a file that is not there yet.
            ("CLAUDE.md", "AGENTS.md", false),
        ] {
            let dir = scratch("link");
            if exists {
                fs::write(dir.join(target), "# Rules\n").unwrap();
            }
            std::os::unix::fs::symlink(target, dir.join(link)).unwrap();
            every(&dir);
            let text = read(&dir, "AGENTS.md");
            assert!(text.contains("## Gióng"), "{link}: {text}");
            assert!(!text.contains("@AGENTS.md"), "{link}: {text}");
            assert_eq!(text.matches(BEGIN).count(), 1, "{link}: {text}");
            assert!(!text.contains("# CLAUDE.md"), "{link}: {text}");
            for _ in 0..2 {
                let again = every(&dir);
                assert!(again.written.is_empty(), "{link}: {:?}", again.written);
            }
            assert!(fs::symlink_metadata(dir.join(link))
                .unwrap()
                .file_type()
                .is_symlink());
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A repository can commit an agent file, or a folder on its way, as a
    /// link to something outside the project: giong writes through none of
    /// them, says so, and leaves the links as they are.
    #[cfg(unix)]
    #[test]
    fn links_out_of_the_project_are_not_written_through() {
        use std::os::unix::fs::symlink;
        let outside = scratch("outside");
        let secret = outside.join("config.json");
        fs::write(&secret, "{\"keep\": true}\n").unwrap();
        fs::create_dir_all(outside.join("rules")).unwrap();
        for (link, to) in [
            (".mcp.json", secret.clone()),
            ("CLAUDE.md", secret.clone()),
            (".cursor", outside.clone()),
            (".claude/skills/giong/SKILL.md", secret.clone()),
        ] {
            let dir = scratch("linked");
            let at = dir.join(link);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            symlink(&to, &at).unwrap();
            let out = every(&dir);
            assert_eq!(
                fs::read_to_string(&secret).unwrap(),
                "{\"keep\": true}\n",
                "{link} was written through"
            );
            assert!(!outside.join("rules/giong.mdc").exists(), "{link}");
            assert!(is_link(&at), "{link} is no longer a link");
            let warned = out
                .findings
                .iter()
                .any(|f| f.code == Code::AGENT_FILE_FOREIGN && f.title.contains("symbolic link"));
            assert!(warned, "{link}: {:?}", out.findings);
            // The other files are written as usual.
            assert!(read(&dir, "AGENTS.md").contains("## Gióng"), "{link}");
            fs::remove_dir_all(&dir).unwrap();
        }
        // A folder linked to another folder inside the project is fine.
        let dir = scratch("inside");
        fs::create_dir_all(dir.join("editor")).unwrap();
        symlink("editor", dir.join(".cursor")).unwrap();
        let out = every(&dir);
        assert!(out.findings.is_empty(), "{:?}", out.findings);
        assert!(fs::read_to_string(dir.join("editor/rules/giong.mdc")).is_ok());
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn a_second_copy_of_the_block_is_dropped() {
        let block = "<!-- giong:begin -->\nnew\n<!-- giong:end -->\n";
        let text = "a\n<!-- giong:begin -->\nold\n<!-- giong:end -->\nb\n<!-- giong:begin -->\nolder\n<!-- giong:end -->\nc\n";
        match merge_block(Some(text), block, "", "AGENTS.md") {
            Change::Write(t) => assert_eq!(t, format!("a\n{block}b\nc\n")),
            other => panic!("{other:?}"),
        }
        // A begin inside a block is not a block giong can trust.
        let nested = "<!-- giong:begin -->\n<!-- giong:begin -->\n<!-- giong:end -->\n";
        assert!(matches!(
            merge_block(Some(nested), block, "", "x"),
            Change::Keep(_)
        ));
    }

    #[test]
    fn a_crlf_file_gets_a_crlf_block() {
        let block = "<!-- giong:begin -->\nnew\n<!-- giong:end -->\n";
        match merge_block(Some("# Ours\r\nline\r\n"), block, "", "AGENTS.md") {
            Change::Write(t) => {
                assert_eq!(
                    t,
                    "# Ours\r\nline\r\n\r\n<!-- giong:begin -->\r\nnew\r\n<!-- giong:end -->\r\n"
                );
                assert!(!t.replace("\r\n", "").contains('\n'), "mixed line ends");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_owned_marker_counts_only_at_the_top() {
        assert!(is_owned("---\na: b\n---\n<!-- giong:owned (x) -->\nbody\n"));
        let quoted = format!(
            "# Notes\n\n{}\nWe quote `<!-- giong:owned` here.\n",
            "line\n".repeat(20)
        );
        assert!(!is_owned(&quoted));
        assert!(!is_owned(
            "text that mentions <!-- giong:owned in the middle\n"
        ));
    }

    #[test]
    fn mcp_json_with_a_byte_order_mark() {
        match merge_mcp(Some(
            "\u{feff}{\"mcpServers\": {\"db\": {\"command\": \"psql\"}}}",
        )) {
            Change::Write(t) => {
                assert!(t.starts_with('\u{feff}'));
                let v: Value = serde_json::from_str(&t[3..]).unwrap();
                assert_eq!(v["mcpServers"]["db"]["command"], "psql");
                assert_eq!(v["mcpServers"]["giong"]["args"][0], "mcp");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Commands this build does not have are not offered to the agent.
    #[test]
    fn only_commands_that_exist_are_named() {
        let none = Offer {
            mcp: false,
            dev: false,
            dev_background: false,
            check: false,
            fix: false,
            test: false,
            doctor: false,
            db: false,
            upgrade: false,
        };
        let dir = scratch("offer");
        let out = apply(&dir, GIONG_VERSION, false, &none);
        assert!(!dir.join(".mcp.json").exists());
        assert_eq!(out.written.len(), 4);
        let agents = read(&dir, "AGENTS.md");
        for gone in [
            "giong dev",
            "giong check",
            "giong fix",
            "giong test",
            "giong doctor",
            "giong db",
            "giong upgrade",
            "MCP",
        ] {
            assert!(!agents.contains(gone), "{gone} in {agents}");
        }
        assert!(agents.contains("GIONG_DRIVER=epoll cargo test"));
        assert!(!read(&dir, ".cursor/rules/giong.mdc").contains("giong check"));
        // This build: every command named exists.
        let here = Offer::here();
        assert_eq!(
            here.mcp,
            crate::cmd::COMMANDS.iter().any(|c| c.name == "mcp")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The flow an agent follows to work on the service: one `giong dev`,
    /// started in the background once, and nothing else that runs it.
    #[test]
    fn agents_learn_one_dev_runner() {
        let all = Offer::all();
        let agents = agents_block("1.5.0", &all);
        for step in [
            "giong dev --background",
            "giong dev status",
            "giong dev logs",
            "giong dev stop",
            "http://127.0.0.1:PORT",
            "Do not run the service with `cargo run`",
        ] {
            assert!(agents.contains(step), "{step} missing from {agents}");
        }
        assert!(cursor_rule(&all).contains("giong dev --background"));
        // A giong without the background runner is not taught it.
        let older = Offer {
            dev_background: false,
            ..all
        };
        let agents = agents_block("1.5.0", &older);
        assert!(!agents.contains("--background"), "{agents}");
        assert!(agents.contains("`giong dev`"));
        assert!(!cursor_rule(&older).contains("giong dev"));
        // This build teaches it exactly when `giong dev` has the flag.
        let has = crate::cmd::COMMANDS
            .iter()
            .any(|c| c.name == "dev" && c.flags.iter().any(|f| f.name == "background"));
        assert_eq!(Offer::here().dev_background, has);
    }

    #[test]
    fn writes_everything_then_nothing() {
        let dir = scratch("fresh");
        let first = every(&dir);
        assert!(first.findings.is_empty(), "{:?}", first.findings);
        assert_eq!(first.written.len(), 5, "{:?}", first.written);
        assert!(
            read(&dir, "AGENTS.md").contains(&format!("Gióng](https://giong.run) {GIONG_VERSION}"))
        );
        assert!(read(&dir, "CLAUDE.md").contains("\n@AGENTS.md\n"));
        let skill = read(&dir, ".claude/skills/giong/SKILL.md");
        assert!(
            skill.starts_with("---\nname: giong\n"),
            "front matter first"
        );
        assert!(skill.contains(&format!("Built from giong {GIONG_VERSION}.")));
        assert!(skill.contains(OWNED));
        let mcp: Value = serde_json::from_str(&read(&dir, ".mcp.json")).unwrap();
        assert_eq!(mcp["mcpServers"]["giong"]["args"][0], "mcp");
        assert_eq!(mcp["mcpServers"].as_object().unwrap().len(), 1);

        let second = every(&dir);
        assert!(
            second.written.is_empty(),
            "idempotent: {:?}",
            second.written
        );
        assert_eq!(second.unchanged.len(), 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_what_others_wrote() {
        let dir = scratch("foreign");
        fs::write(dir.join("AGENTS.md"), "# Ours\n\nBuild with make.").unwrap();
        fs::write(dir.join("CLAUDE.md"), "Be brief.\n").unwrap();
        fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        fs::write(dir.join(".cursor/rules/giong.mdc"), "my own rule\n").unwrap();
        fs::write(
            dir.join(".mcp.json"),
            r#"{"zeta": 1, "mcpServers": {"db": {"command": "pg-mcp"}, "giong": {"command": "/opt/giong", "args": ["mcp"]}}, "alpha": true}"#,
        )
        .unwrap();
        let out = every(&dir);
        assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
        assert_eq!(out.findings[0].code, Code::AGENT_FILE_FOREIGN);
        assert_eq!(read(&dir, ".cursor/rules/giong.mdc"), "my own rule\n");

        let agents = read(&dir, "AGENTS.md");
        assert!(
            agents.starts_with("# Ours\n\nBuild with make.\n\n<!-- giong:begin"),
            "{agents}"
        );
        let claude = read(&dir, "CLAUDE.md");
        assert!(
            claude.starts_with("Be brief.\n\n<!-- giong:begin"),
            "{claude}"
        );

        let text = read(&dir, ".mcp.json");
        let mcp: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(mcp["mcpServers"]["db"]["command"], "pg-mcp");
        assert_eq!(
            mcp["mcpServers"]["giong"]["command"], "/opt/giong",
            "theirs is kept"
        );
        assert_eq!(mcp["mcpServers"].as_object().unwrap().len(), 2);
        // The order of the keys stays the user's.
        let zeta = text.find("zeta").unwrap();
        assert!(zeta < text.find("mcpServers").unwrap() && text.find("alpha").unwrap() > zeta);

        // A block edited by hand is put back; the rest of the file stays.
        let edited = agents
            .replace("## Gióng", "## Changed")
            .replace("Build with make.", "Build with just.");
        fs::write(dir.join("AGENTS.md"), &edited).unwrap();
        every(&dir);
        let again = read(&dir, "AGENTS.md");
        assert!(again.contains("## Gióng") && !again.contains("## Changed"));
        assert!(again.contains("Build with just."));
        assert_eq!(again.matches(BEGIN).count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owned_files_are_refreshed_and_freed() {
        let dir = scratch("owned");
        init_dir(&dir, Some("1.0.0"));
        assert!(read(&dir, ".claude/skills/giong/SKILL.md").contains("Built from giong 1.0.0."));
        init_dir(&dir, Some("1.9.0"));
        assert!(read(&dir, ".claude/skills/giong/SKILL.md").contains("Built from giong 1.9.0."));
        // Without the owned line the file is the user's.
        let mine = read(&dir, ".cursor/rules/giong.mdc")
            .lines()
            .filter(|l| !l.contains(OWNED))
            .collect::<Vec<_>>()
            .join("\n")
            + "\nmore of mine\n";
        fs::write(dir.join(".cursor/rules/giong.mdc"), &mine).unwrap();
        let out = every(&dir);
        assert_eq!(out.findings[0].code, Code::AGENT_FILE_FOREIGN);
        assert_eq!(read(&dir, ".cursor/rules/giong.mdc"), mine);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_files_are_left_alone() {
        let dir = scratch("broken");
        for bad in ["{not json", "[1, 2]", r#"{"mcpServers": 3}"#] {
            fs::write(dir.join(".mcp.json"), bad).unwrap();
            let out = every(&dir);
            assert!(
                out.findings
                    .iter()
                    .any(|f| f.code == Code::MCP_JSON_INVALID),
                "{bad}"
            );
            assert_eq!(read(&dir, ".mcp.json"), bad);
        }
        fs::write(dir.join(".mcp.json"), "  \n").unwrap();
        assert!(every(&dir).written.contains(&".mcp.json".to_string()));

        fs::write(dir.join("AGENTS.md"), "x\n<!-- giong:begin\nhalf a block\n").unwrap();
        let out = every(&dir);
        assert!(out
            .findings
            .iter()
            .any(|f| f.code == Code::AGENT_FILE_FOREIGN));
        assert_eq!(
            read(&dir, "AGENTS.md"),
            "x\n<!-- giong:begin\nhalf a block\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn crlf_files_keep_their_lines() {
        let block = "<!-- giong:begin -->\nnew\n<!-- giong:end -->\n";
        let text = "a\r\n<!-- giong:begin -->\r\nold\r\n<!-- giong:end -->\r\nb\r\n";
        match merge_block(Some(text), block, "", "AGENTS.md") {
            Change::Write(t) => assert_eq!(t, format!("a\r\n{}b\r\n", block.replace('\n', "\r\n"))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = scratch("dry");
        let out = apply(&dir, GIONG_VERSION, true, &Offer::all());
        assert_eq!(out.written.len(), 5);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn versions() {
        assert_eq!(major_minor("1.5.0"), Some((1, 5)));
        assert_eq!(major_minor("^1.5"), Some((1, 5)));
        assert_eq!(major_minor("=1.4.2"), Some((1, 4)));
        assert_eq!(major_minor(">=1.3, <2"), Some((1, 3)));
        assert_eq!(major_minor("1"), Some((1, 0)));
        assert_eq!(major_minor("*"), None);
    }

    /// assets/skill/SKILL.md is skills/giong/SKILL.md; `GIONG_CLI_REGENERATE=1`
    /// copies it.
    #[test]
    fn skill_is_current() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = root.join("../skills/giong/SKILL.md");
        let Ok(text) = fs::read_to_string(&source) else {
            return; // the published crate
        };
        let copy = root.join("assets/skill/SKILL.md");
        if std::env::var_os("GIONG_CLI_REGENERATE").is_some() {
            fs::write(&copy, &text).unwrap();
        }
        assert!(
            fs::read_to_string(&copy).unwrap() == text,
            "giong-cli/assets/skill/SKILL.md is out of date with skills/giong/SKILL.md: run \
             GIONG_CLI_REGENERATE=1 cargo test -p giong-cli skill"
        );
    }
}
