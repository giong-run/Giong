//! `giong new`, `giong ai init`, `giong fix` and `giong test`, run as an
//! agent runs them. Each test returns early (with a note) while its command
//! is not registered in this build.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::harness::{giong, has_command, new_package, TempDir};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()));
    c.env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTFLAGS")
        // CI sets CARGO_TERM_COLOR=always; these tests read cargo's words.
        .env("CARGO_TERM_COLOR", "never");
    c
}

fn cargo_reads(dir: &Path) -> Result<(), String> {
    let out = cargo()
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(dir)
        .output()
        .unwrap();
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Every file under `dir` with its bytes, for before/after comparisons.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_name() == "target" || e.file_name() == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(dir).unwrap().to_path_buf(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

fn codes(v: &Value) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

fn new_local(dir: &Path, path: &str, extra: &[&str]) -> crate::harness::Out {
    let mut run = giong(["new", path, "--no-git", "--json", "--giong-path"])
        .arg(workspace())
        .dir(dir)
        .timeout(Duration::from_secs(120));
    for e in extra {
        run = run.arg(e);
    }
    run.run()
}

#[test]
fn new_dry_run_writes_nothing() {
    if !has_command("new") {
        return;
    }
    let dir = TempDir::new("new-dry");
    let before = snapshot(dir.path());
    let out = giong(["new", "app", "--dry-run", "--json"])
        .dir(dir.path())
        .run();
    out.report("new");
    out.expect_code(0);
    assert_eq!(snapshot(dir.path()), before, "--dry-run wrote files");
    assert!(!dir.join("app").exists());
}

/// Every template and store combination writes a project cargo reads, and
/// the project passes its own `giong doctor` (fast dev profile, no Tokio).
#[test]
fn every_template_writes_a_project_cargo_reads_and_doctor_accepts() {
    if !has_command("new") {
        return;
    }
    let combos: &[&[&str]] = &[
        &["--template", "web"],
        &["--template", "web", "--tls"],
        &["--template", "web", "--db", "postgres,redis"],
        &["--template", "web", "--db", "mysql"],
        &["--template", "grpc"],
        &["--template", "worker", "--db", "redis"],
        &["--template", "worker", "--db", "postgres"],
    ];
    for combo in combos {
        let dir = TempDir::new("new-combo");
        let out = new_local(dir.path(), "svc", combo);
        out.report("new");
        out.expect_code(0);
        let p = dir.join("svc");
        if let Err(e) = cargo_reads(&p) {
            panic!("giong new {combo:?}: cargo cannot read the project:\n{e}");
        }
        for f in ["Cargo.toml", ".gitignore", ".env.example", "AGENTS.md"] {
            assert!(p.join(f).exists(), "giong new {combo:?} wrote no {f}");
        }
        let gi = std::fs::read_to_string(p.join(".gitignore")).unwrap();
        assert!(
            gi.lines()
                .any(|l| l.trim() == ".env" || l.trim() == "/.env"),
            "giong new {combo:?}: .env is not in .gitignore"
        );
        if p.join(".mcp.json").exists() {
            let mcp = std::fs::read_to_string(p.join(".mcp.json")).unwrap();
            assert!(
                serde_json::from_str::<Value>(&mcp).is_ok(),
                "giong new {combo:?}: .mcp.json is not JSON"
            );
            assert!(
                !mcp.contains("GIONG_MCP_TOKEN=") && !mcp.to_lowercase().contains("bearer "),
                "giong new {combo:?}: a token is written into the committed .mcp.json"
            );
        }
        let v = giong(["doctor", "--json"])
            .dir(&p)
            .timeout(Duration::from_secs(120))
            .run()
            .report("doctor");
        let c = codes(&v);
        assert!(
            !c.contains(&"GIONG-E5012".into()) && !c.contains(&"GIONG-E5004".into()),
            "giong new {combo:?}: the project fails its own doctor: {v:#}"
        );
    }
}

/// Whatever name is asked for, `giong new` either refuses it with a code and
/// writes nothing, or writes a project cargo reads. Never half a project.
#[test]
fn odd_names_are_refused_or_give_a_valid_package() {
    if !has_command("new") {
        return;
    }
    for name in [
        "a b",
        "self",
        "test",
        "core",
        "std",
        "Café",
        "x-y_z",
        "9lives",
        "-dash",
        "a\"b",
        "{{name}}",
        "con",
        "very-long-name-very-long-name-very-long-name-very-long-name-xx",
    ] {
        let dir = TempDir::new("new-name");
        let before = snapshot(dir.path());
        let out = new_local(dir.path(), "proj", &["--name", name]);
        let v = out.report("new");
        if v.get("error").is_some() {
            assert_eq!(
                snapshot(dir.path()),
                before,
                "--name {name:?} failed but wrote files"
            );
            continue;
        }
        if let Err(e) = cargo_reads(&dir.join("proj")) {
            panic!("--name {name:?} was accepted but cargo cannot read the project:\n{e}");
        }
    }
}

#[test]
fn new_never_writes_over_a_directory_with_files() {
    if !has_command("new") {
        return;
    }
    let dir = TempDir::new("new-existing");
    dir.write("app/notes.txt", "mine");
    dir.write("app/Cargo.toml", "[package]\nname = \"mine\"\n");
    let before = snapshot(dir.path());
    let out = new_local(dir.path(), "app", &[]);
    let v = out.report("new");
    assert!(
        v.get("error").is_some(),
        "giong new wrote into a directory with files: {v:#}"
    );
    assert_eq!(snapshot(dir.path()), before);
    // A file with the project's name.
    let dir = TempDir::new("new-file");
    dir.write("app", "a file");
    let v = new_local(dir.path(), "app", &[]).report("new");
    assert!(v.get("error").is_some(), "{v:#}");
    assert_eq!(dir.read("app"), "a file");
    // An empty directory is fine.
    let dir = TempDir::new("new-empty");
    std::fs::create_dir_all(dir.join("app")).unwrap();
    new_local(dir.path(), "app", &[]).expect_code(0);
    assert!(dir.join("app/Cargo.toml").exists());
}

/// `giong ai init` twice changes nothing the second time, keeps what people
/// wrote, and merges into an `.mcp.json` that has other servers.
#[test]
fn ai_init_is_idempotent_and_keeps_what_people_wrote() {
    if !has_command("ai") {
        return;
    }
    let dir = TempDir::new("ai-init");
    new_package(&dir, "agents", "");
    dir.write("AGENTS.md", "# House rules\n\nKeep me exactly.\n");
    dir.write("CLAUDE.md", "Personal note: keep me too.\n");
    dir.write(
        ".mcp.json",
        "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"other-server\", \"args\": [\"--x\"]}\n  }\n}\n",
    );
    let out = giong(["ai", "init", "--json"]).dir(dir.path()).run();
    out.report("ai");
    out.expect_code(0);
    let agents = dir.read("AGENTS.md");
    assert!(
        agents.contains("# House rules\n\nKeep me exactly.\n"),
        "{agents}"
    );
    assert!(dir
        .read("CLAUDE.md")
        .contains("Personal note: keep me too."));
    let mcp: Value = serde_json::from_str(&dir.read(".mcp.json")).expect(".mcp.json is JSON");
    assert_eq!(
        mcp["mcpServers"]["other"]["command"], "other-server",
        "another MCP server was dropped: {mcp:#}"
    );
    // giong's own server only once `giong mcp` exists: a server entry that
    // exits at once would break the agent's MCP connection.
    let has_giong = mcp["mcpServers"].get("giong").is_some();
    assert_eq!(
        has_giong,
        has_command("mcp"),
        "giong's server is listed exactly when `giong mcp` exists: {mcp:#}"
    );
    let first = snapshot(dir.path());
    giong(["ai", "init", "--json"])
        .dir(dir.path())
        .run()
        .expect_code(0);
    assert_eq!(
        snapshot(dir.path()),
        first,
        "a second giong ai init changed files"
    );
    // Dry run on a fresh package writes nothing.
    let fresh = TempDir::new("ai-dry");
    new_package(&fresh, "dry", "");
    let before = snapshot(fresh.path());
    giong(["ai", "init", "--dry-run", "--json"])
        .dir(fresh.path())
        .run()
        .expect_code(0);
    assert_eq!(
        snapshot(fresh.path()),
        before,
        "ai init --dry-run wrote files"
    );
}

/// A `.mcp.json` that is not JSON is the person's file: it is not replaced.
#[test]
fn ai_init_does_not_replace_a_broken_mcp_json() {
    if !has_command("ai") {
        return;
    }
    let dir = TempDir::new("ai-broken-mcp");
    new_package(&dir, "broken", "");
    let broken = "{ \"mcpServers\": { \"mine\": { \"command\": \"x\" }, }\n";
    dir.write(".mcp.json", broken);
    let out = giong(["ai", "init", "--json"]).dir(dir.path()).run();
    let v = out.report("ai");
    assert_eq!(
        dir.read(".mcp.json"),
        broken,
        "a broken .mcp.json was replaced: {v:#}"
    );
    assert!(
        v.get("error").is_some() || !v["findings"].as_array().unwrap().is_empty(),
        "the broken .mcp.json was skipped without a word: {v:#}"
    );
}

/// A file giong cannot read as UTF-8 is someone's file: left byte for byte.
#[test]
fn ai_init_leaves_files_it_cannot_read_alone() {
    if !has_command("ai") {
        return;
    }
    let dir = TempDir::new("ai-latin1");
    new_package(&dir, "latin", "");
    let agents: &[u8] = b"# Mes r\xe8gles\nNe pas effacer.\n";
    let mcp: &[u8] = b"{\"mcpServers\":{\"db\":{\"command\":\"psql\",\"note\":\"caf\xe9\"}}}";
    std::fs::write(dir.join("AGENTS.md"), agents).unwrap();
    std::fs::write(dir.join(".mcp.json"), mcp).unwrap();
    let out = giong(["ai", "init", "--json"]).dir(dir.path()).run();
    out.report("ai");
    assert_eq!(
        std::fs::read(dir.join("AGENTS.md")).unwrap(),
        agents,
        "{}",
        out.dump()
    );
    assert_eq!(
        std::fs::read(dir.join(".mcp.json")).unwrap(),
        mcp,
        "{}",
        out.dump()
    );
}

/// `CLAUDE.md -> AGENTS.md` is a common setup: one file, written once, the
/// same on every later run, and never importing itself.
#[cfg(unix)]
#[test]
fn ai_init_with_claude_md_linked_to_agents_md() {
    if !has_command("ai") {
        return;
    }
    let dir = TempDir::new("ai-symlink");
    new_package(&dir, "linked", "");
    dir.write("AGENTS.md", "# My rules\nKeep this.\n");
    std::os::unix::fs::symlink("AGENTS.md", dir.join("CLAUDE.md")).unwrap();
    giong(["ai", "init"]).dir(dir.path()).run().expect_code(0);
    let first = dir.read("AGENTS.md");
    assert!(first.starts_with("# My rules\nKeep this.\n"), "{first}");
    assert!(first.contains("giong:begin"), "no giong block: {first}");
    assert!(
        !first.contains("@AGENTS.md"),
        "AGENTS.md imports itself: {first}"
    );
    for _ in 0..2 {
        giong(["ai", "init"]).dir(dir.path()).run().expect_code(0);
        assert_eq!(dir.read("AGENTS.md"), first, "not idempotent");
    }
    assert!(std::fs::symlink_metadata(dir.join("CLAUDE.md"))
        .unwrap()
        .file_type()
        .is_symlink());
}

/// Every `giong <command>` the agent files name is a command this build has:
/// an agent told to run a missing command wastes a turn on E5000.
#[test]
fn agent_files_name_only_commands_that_exist() {
    if !has_command("ai") {
        return;
    }
    let dir = TempDir::new("ai-commands");
    new_package(&dir, "cmds", "");
    giong(["ai", "init"]).dir(dir.path()).run().expect_code(0);
    let have = crate::harness::commands();
    let mut text = String::new();
    for f in [
        "AGENTS.md",
        "CLAUDE.md",
        ".mcp.json",
        ".cursor/rules/giong.mdc",
    ] {
        if let Ok(t) = std::fs::read_to_string(dir.join(f)) {
            text.push_str(&t);
        }
    }
    let mut missing = Vec::new();
    for (i, _) in text.match_indices("giong ") {
        let word: String = text[i + 6..]
            .chars()
            .take_while(|c| c.is_ascii_lowercase())
            .collect();
        // Prose like "giong is" is not a command; a known command word is.
        if word.len() > 1 && !have.contains(&word) && looks_like_command(&text, i) {
            missing.push(word);
        }
    }
    let mcp: Value = std::fs::read_to_string(dir.join(".mcp.json"))
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or(Value::Null);
    if let Some(args) = mcp["mcpServers"]["giong"]["args"].as_array() {
        let w = args[0].as_str().unwrap().to_string();
        if !have.contains(&w) {
            missing.push(w);
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "the agent files name missing commands {missing:?}\n{text}"
    );
}

/// `giong x` inside backticks or at the start of a code line.
fn looks_like_command(text: &str, i: usize) -> bool {
    let before = &text[..i];
    before.ends_with('`') || before.ends_with("\n") || before.ends_with("$ ")
}

/// `giong fix` applies the compiler's machine-applicable suggestion, writes
/// nothing with --dry-run, and has nothing left to do on a second run.
#[test]
fn fix_applies_what_the_compiler_suggests_and_dry_run_writes_nothing() {
    if !has_command("fix") {
        return;
    }
    let dir = TempDir::new("fix");
    new_package(&dir, "fixme", "");
    let src = "use std::collections::HashMap;\n\nfn main() {\n    println!(\"hi\");\n}\n";
    dir.write("src/main.rs", src);
    let out = giong(["fix", "--dry-run", "--json"])
        .dir(dir.path())
        .timeout(Duration::from_secs(300))
        .run();
    out.report("fix");
    assert_eq!(
        dir.read("src/main.rs"),
        src,
        "fix --dry-run changed the file"
    );
    let out = giong(["fix", "--json"])
        .dir(dir.path())
        .timeout(Duration::from_secs(300))
        .run();
    out.report("fix");
    let fixed = dir.read("src/main.rs");
    assert!(
        !fixed.contains("HashMap"),
        "the unused import stayed:\n{fixed}"
    );
    assert!(fixed.contains("println!(\"hi\")"));
    let out = giong(["fix", "--json"])
        .dir(dir.path())
        .timeout(Duration::from_secs(300))
        .run();
    out.report("fix");
    assert_eq!(
        dir.read("src/main.rs"),
        fixed,
        "a second giong fix changed the file again"
    );
}

/// `giong test`: a failing test is a finding with its name and exit 1, a
/// green suite exit 0, and .env reaches the tests (values with spaces too).
#[test]
fn test_reports_failures_by_name_and_loads_dotenv() {
    if !has_command("test") {
        return;
    }
    let dir = TempDir::new("gtest");
    new_package(&dir, "tested", "");
    dir.write(".env", "GIONG_E2E_FROM_DOTENV=\"two words\"\n");
    dir.write(
        "src/main.rs",
        "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn sees_dotenv() {\n        \
         assert_eq!(std::env::var(\"GIONG_E2E_FROM_DOTENV\").unwrap(), \"two words\");\n    }\n\n    \
         #[test]\n    fn this_one_fails() {\n        panic!(\"on purpose\");\n    }\n}\n",
    );
    let out = giong(["test", "--json"])
        .dir(dir.path())
        .env_remove("GIONG_E2E_FROM_DOTENV")
        .timeout(Duration::from_secs(300))
        .run();
    let v = out.report("test");
    out.expect_code(1);
    let text = v.to_string();
    assert!(text.contains("this_one_fails"), "{v:#}");
    assert!(
        !text.contains("sees_dotenv\""),
        "the .env test failed: {v:#}"
    );
    let src = dir
        .read("src/main.rs")
        .replace("panic!(\"on purpose\");", "");
    dir.write("src/main.rs", &src);
    let out = giong(["test", "--json"])
        .dir(dir.path())
        .env_remove("GIONG_E2E_FROM_DOTENV")
        .timeout(Duration::from_secs(300))
        .run();
    out.report("test");
    out.expect_code(0);
}
