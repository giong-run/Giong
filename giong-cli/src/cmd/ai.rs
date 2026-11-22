//! `giong ai init`: teaches the coding agents of a project how to write Gióng
//! code: `AGENTS.md` (read by most agents), `CLAUDE.md` (which imports it),
//! a Cursor rule, the Gióng skill of this release, and the MCP servers in
//! `.mcp.json`.
//!
//! Safe to run again and again: in a file someone else wrote it only adds or
//! replaces its own marked block (`<!-- giong:begin -->` ... `<!-- giong:end -->`),
//! a whole file it rewrites only when the file carries its `<!-- giong:owned -->`
//! line, and in `.mcp.json` it adds its server when it is missing and touches
//! nothing else.

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
            in .claude/skills/giong/, and the giong MCP server in .mcp.json (this command over \
            stdio, which also reaches the service giong dev runs). Run it again at \
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

fn agents_block(version: &str) -> String {
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
- **`bind` takes IP addresses** (`0.0.0.0:8080`), never `localhost`.
- **Every wait for another party has a deadline**: `giong::time::timeout(duration, fut)`.
- **Errors carry a code** (`GIONG-E0102: ... Fix: ...`). Match on `giong::error::code(&err)`,
  never on the text; `giong explain GIONG-E0102` prints the cause and the fix.
- **The giong crate for the job**: giong-web (not axum), giong-http's client (not reqwest),
  giong-postgres and giong-mysql (not sqlx), giong-redis (not redis-rs), giong-grpc (not
  tonic). A crate that needs the Tokio runtime panics on a worker.

| To | Run |
|---|---|
| Run the service, restarting on every change | `giong dev` |
| Check a change before finishing | `giong check --json`, then `giong fix` for what it can fix |
| Run the tests | `giong test` (or `cargo test`, then `GIONG_DRIVER=epoll cargo test`) |
| Explain an error code | `giong explain <code>` |
| Check the machine and the project | `giong doctor` |
| Start the database, apply migrations | `giong db up`, `giong db migrate` |

The `giong` MCP server in `.mcp.json` serves these commands to agents and, while `giong dev`
runs the service, the service's own view of itself (workers, tasks by spawn line, pools, a
blocked worker): ask it rather than guess why the service is slow or stuck.
{END}
"#
    )
}

const CLAUDE_BLOCK: &str =
    "<!-- giong:begin (written by giong ai init: edit outside this block, it is replaced) -->\n\
@AGENTS.md\n\
<!-- giong:end -->\n";

fn cursor_rule() -> String {
    "---\n\
     description: Gióng rules for this Rust project\n\
     alwaysApply: true\n\
     ---\n\
     <!-- giong:owned (giong ai init rewrites this file; delete this line to keep your edits) -->\n\n\
     This project runs on Gióng, a thread-per-core async runtime for Rust. Read `AGENTS.md` at\n\
     the root before changing code: it has the rules and the commands. Before finishing a\n\
     change, run `giong check --json` and fix what it reports (`giong fix` applies the\n\
     mechanical fixes).\n"
        .to_string()
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

/// Puts `block` into `existing`: in place of the marked block, or at the end.
fn merge_block(existing: Option<&str>, block: &str, new_file: &str, path: &str) -> Change {
    let Some(text) = existing else {
        return Change::Write(new_file.to_string());
    };
    let begin = text.find(BEGIN);
    let end = text.find(END);
    let merged = match (begin, end) {
        (Some(b), Some(e)) if b < e => {
            let mut after = e + END.len();
            if text[after..].starts_with("\r\n") {
                after += 2;
            } else if text[after..].starts_with('\n') {
                after += 1;
            }
            format!("{}{block}{}", &text[..b], &text[after..])
        }
        (None, None) => {
            let sep = if text.is_empty() || text.ends_with("\n\n") {
                ""
            } else if text.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{text}{sep}{block}")
        }
        _ => {
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
            )
        }
    };
    if merged == text {
        Change::Same
    } else {
        Change::Write(merged)
    }
}

/// A file `ai init` owns entirely: written when missing or still marked.
fn owned_file(existing: Option<&str>, content: String, path: &str) -> Change {
    match existing {
        None => Change::Write(content),
        Some(text) if text == content => Change::Same,
        Some(text) if text.contains(OWNED) => Change::Write(content),
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
    let mut text = serde_json::to_string_pretty(&doc).expect("a Value always serialises");
    text.push('\n');
    Change::Write(text)
}

/// The changes for a project root, without writing anything.
fn plan(root: &Path, version: &str) -> Vec<(String, Change)> {
    let read = |rel: &str| fs::read_to_string(root.join(rel)).ok();
    let block = agents_block(version);
    let agents_new = format!(
        "# AGENTS.md\n\nInstructions for coding agents working on this project.\n\n{block}\n\
         ## This project\n\nAdd what an agent should know about this project here, outside the \
         giong block.\n"
    );
    vec![
        (
            "AGENTS.md".into(),
            merge_block(
                read("AGENTS.md").as_deref(),
                &block,
                &agents_new,
                "AGENTS.md",
            ),
        ),
        (
            "CLAUDE.md".into(),
            merge_block(
                read("CLAUDE.md").as_deref(),
                CLAUDE_BLOCK,
                &format!("# CLAUDE.md\n\n{CLAUDE_BLOCK}"),
                "CLAUDE.md",
            ),
        ),
        (
            ".cursor/rules/giong.mdc".into(),
            owned_file(
                read(".cursor/rules/giong.mdc").as_deref(),
                cursor_rule(),
                ".cursor/rules/giong.mdc",
            ),
        ),
        (
            ".claude/skills/giong/SKILL.md".into(),
            owned_file(
                read(".claude/skills/giong/SKILL.md").as_deref(),
                skill(version),
                ".claude/skills/giong/SKILL.md",
            ),
        ),
        (".mcp.json".into(), merge_mcp(read(".mcp.json").as_deref())),
    ]
}

/// Applies the changes. `version` is the giong version the project uses, when
/// known (giong new); otherwise it is read from the project.
pub fn init_dir(root: &Path, version: Option<&str>) -> Outcome {
    apply(root, version.unwrap_or(GIONG_VERSION), false)
}

fn apply(root: &Path, version: &str, dry_run: bool) -> Outcome {
    let mut out = Outcome::default();
    for (rel, change) in plan(root, version) {
        match change {
            Change::Same => out.unchanged.push(rel),
            Change::Keep(finding) => out.findings.push(finding),
            Change::Write(text) => {
                if !dry_run {
                    let path: PathBuf = root.join(&rel);
                    let result = path
                        .parent()
                        .map_or(Ok(()), fs::create_dir_all)
                        .and_then(|()| fs::write(&path, text));
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
    let out = apply(&project.root, GIONG_VERSION, args.has("dry-run"));
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

    #[test]
    fn writes_everything_then_nothing() {
        let dir = scratch("fresh");
        let first = init_dir(&dir, None);
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

        let second = init_dir(&dir, None);
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
        let out = init_dir(&dir, None);
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
        init_dir(&dir, None);
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
        let out = init_dir(&dir, None);
        assert_eq!(out.findings[0].code, Code::AGENT_FILE_FOREIGN);
        assert_eq!(read(&dir, ".cursor/rules/giong.mdc"), mine);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_files_are_left_alone() {
        let dir = scratch("broken");
        for bad in ["{not json", "[1, 2]", r#"{"mcpServers": 3}"#] {
            fs::write(dir.join(".mcp.json"), bad).unwrap();
            let out = init_dir(&dir, None);
            assert!(
                out.findings
                    .iter()
                    .any(|f| f.code == Code::MCP_JSON_INVALID),
                "{bad}"
            );
            assert_eq!(read(&dir, ".mcp.json"), bad);
        }
        fs::write(dir.join(".mcp.json"), "  \n").unwrap();
        assert!(init_dir(&dir, None)
            .written
            .contains(&".mcp.json".to_string()));

        fs::write(dir.join("AGENTS.md"), "x\n<!-- giong:begin\nhalf a block\n").unwrap();
        let out = init_dir(&dir, None);
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
            Change::Write(t) => assert_eq!(t, format!("a\r\n{block}b\r\n")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = scratch("dry");
        let out = apply(&dir, GIONG_VERSION, true);
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
