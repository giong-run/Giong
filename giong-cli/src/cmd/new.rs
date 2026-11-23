//! `giong new <path>`: a project that builds, passes its tests and follows the
//! best practices from the first minute.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use serde_json::json;

use super::{ai, Command};
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::report::{Failure, Finding, Report, Severity};
use crate::scaffold::{self, Options, Template};

pub const COMMAND: Command = Command {
    name: "new",
    summary: "start a project: an HTTP API, a gRPC service or a worker",
    usage: "giong new <path> [--template web|grpc|worker] [--db postgres,mysql,redis] [--tls]\n\n\
            Writes a project that builds and passes its tests: Cargo.toml with fast dev \
            builds, the service (one worker per core, graceful shutdown, giong-mcp), an end to \
            end test, .env.example, a Dockerfile, a GitHub Actions workflow, and the files \
            coding agents read (giong ai init). The directory must not exist or be empty; its \
            last part is the package name unless --name gives one.\n\n\
            web: routes and JSON on giong-web; with --db a users API (postgres or mysql; with \
            redis as a cache too), or counters in redis. grpc: a service from a .proto, no \
            protoc needed. worker: jobs from a postgres or mysql table or a redis list, or a \
            job on a timer.",
    flags: &[
        Flag {
            name: "template",
            kind: Kind::Value,
            help: "web (default), grpc or worker",
        },
        Flag {
            name: "db",
            kind: Kind::Value,
            help: "the stores, separated by commas: postgres or mysql, and redis",
        },
        Flag {
            name: "tls",
            kind: Kind::Switch,
            help: "serve HTTPS when TLS_CERT and TLS_KEY are set (web, grpc)",
        },
        Flag {
            name: "name",
            kind: Kind::Value,
            help: "the package name, when it differs from the directory's",
        },
        Flag {
            name: "no-git",
            kind: Kind::Switch,
            help: "do not start a git repository",
        },
        Flag {
            name: "no-ai",
            kind: Kind::Switch,
            help: "do not write the agent files (giong ai init does it later)",
        },
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "list the files without writing anything",
        },
        Flag {
            name: "giong-path",
            kind: Kind::Value,
            help: "depend on a giong checkout by path instead of crates.io (working on giong)",
        },
    ],
    max_words: 1,
    run,
};

/// The choices of a command line, checked.
fn options(args: &Args) -> Result<(PathBuf, Options), Failure> {
    let Some(word) = args.words.first() else {
        return Err(Failure::new(
            Code::USAGE,
            "giong new needs the directory of the project",
            "write giong new api (or giong new path/to/api)",
        ));
    };
    let dir = without_dots(Path::new(word));
    let name = match args.value("name") {
        Some(n) => n.to_string(),
        None => project_name(&resolved(&dir)).ok_or_else(|| {
            Failure::new(
                Code::BAD_NAME,
                format!("{word:?} does not end in a name the project could have"),
                "give the directory of the project, such as api, or name it with --name",
            )
        })?,
    };
    let template = match args.value("template") {
        None => Template::Web,
        Some(t) => Template::parse(t).ok_or_else(|| {
            Failure::new(
                Code::BAD_TEMPLATE_CHOICE,
                format!("there is no template {t:?}"),
                "use --template web, grpc or worker",
            )
        })?,
    };
    let mut o = Options::new(&name, template);
    o.dbs = scaffold::parse_dbs(args.value("db").unwrap_or(""))?;
    o.tls = args.has("tls");
    if let Some(path) = args.value("giong-path") {
        let checkout = fs::canonicalize(path)
            .ok()
            .filter(|p| scaffold::is_checkout(p))
            .ok_or_else(|| {
                Failure::new(
                    Code::BAD_GIONG_PATH,
                    format!("{path:?} is not a checkout of giong"),
                    "give the directory that holds giong's root Cargo.toml (git clone \
                     https://github.com/giong-run/Giong)",
                )
            })?;
        o.giong_path = Some(checkout.display().to_string());
    }
    scaffold::check_name(&o.name)?;
    scaffold::check_choices(&o)?;
    Ok((dir, o))
}

/// The last part of the path, as the name a project in it would have.
/// The path without its `.` parts (`yy/.` is `yy`), `.` when nothing is left.
fn without_dots(path: &Path) -> PathBuf {
    let clean: PathBuf = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if clean.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        clean
    }
}

/// The absolute path with `..` taken back, for the name only: `giong new .`
/// in an empty `api` directory makes a project called api.
fn resolved(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn project_name(dir: &Path) -> Option<String> {
    let last = dir.components().next_back()?;
    match last {
        std::path::Component::Normal(s) => s.to_str().map(String::from),
        _ => None,
    }
}

/// Fails unless `dir` is missing or an empty directory. Returns whether it
/// has to be created.
fn check_target(dir: &Path) -> Result<(), Failure> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Failure::new(
                    Code::TARGET_NOT_EMPTY,
                    format!("{} exists and is not empty", dir.display()),
                    "choose a new directory, or empty this one; to add the agent files to an \
                     existing project, run giong ai init in it",
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::new(
            Code::TARGET_NOT_EMPTY,
            format!("{} cannot be used: {e}", dir.display()),
            if dir.is_file() {
                "a file has that name: choose another directory"
            } else {
                "choose a directory you can write to"
            },
        )),
    }
}

/// Writes every file, or nothing: on an error, what was written is removed.
/// Creates `path` and the parents it lacks, recording each directory it made.
fn make_dirs(path: &Path, made: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut at = Some(path);
    while let Some(p) = at {
        if p.as_os_str().is_empty() || p.exists() {
            break;
        }
        missing.push(p.to_path_buf());
        at = p.parent();
    }
    for p in missing.into_iter().rev() {
        match fs::create_dir(&p) {
            Ok(()) => made.push(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Writes every file; on a failure, takes back every file and directory it
/// made (and nothing that was there before).
fn write_all(dir: &Path, files: &[scaffold::File]) -> Result<(), Failure> {
    let failed = |what: String, e: std::io::Error| {
        Failure::new(
            Code::WRITE_FAILED,
            format!("{what}: {e}"),
            "check that the disk has space and that you may write there",
        )
    };
    let mut written: Vec<PathBuf> = Vec::new();
    let mut made: Vec<PathBuf> = Vec::new();
    let result = (|| {
        make_dirs(dir, &mut made).map_err(|e| failed(dir.display().to_string(), e))?;
        for f in files {
            let path = dir.join(&f.path);
            if let Some(parent) = path.parent() {
                make_dirs(parent, &mut made)
                    .map_err(|e| failed(parent.display().to_string(), e))?;
            }
            // create_new: never replace a file, even one that appeared meanwhile.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| failed(path.display().to_string(), e))?;
            written.push(path.clone());
            std::io::Write::write_all(&mut file, f.text.as_bytes())
                .map_err(|e| failed(path.display().to_string(), e))?;
        }
        Ok(())
    })();
    if result.is_err() {
        for path in written.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for d in made.iter().rev() {
            let _ = fs::remove_dir(d);
        }
    }
    result
}

/// `git init`, unless git is missing or the directory is inside a repository
/// already. Returns whether a repository was made.
fn git_init(dir: &Path) -> Result<bool, String> {
    let inside = Process::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output();
    match inside {
        Err(_) => return Err("git is not installed".into()),
        Ok(out) if out.status.success() => return Ok(false),
        Ok(_) => {}
    }
    let out = Process::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(true)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn run(args: &Args) -> Result<Report, Failure> {
    let (dir, o) = options(args)?;
    let files = scaffold::render(&o)?;
    let mut r = Report::new("new");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    r.data("path", json!(dir.display().to_string()));
    r.data("name", json!(o.name));
    r.data("template", json!(o.template.name()));
    r.data(
        "db",
        json!(o.dbs.iter().map(|d| d.name()).collect::<Vec<_>>()),
    );
    r.data("tls", json!(o.tls));
    r.data("giong_version", json!(scaffold::GIONG_VERSION));

    if cfg!(windows) {
        r.push(
            Finding::new(
                Code::WINDOWS_LIMITED,
                Severity::Warning,
                "this project's crates do not build on Windows",
            )
            .detail(
                "On Windows, giong 1.x builds the runtime crate only; giong-web, giong-grpc, \
                 giong-mcp and the database crates need Linux or macOS.",
            )
            .fix("work on it in WSL 2 (wsl --install, then giong new inside it), or on Linux or macOS"),
        );
    }

    check_target(&dir)?;
    if args.has("dry-run") {
        let mut paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        if !args.has("no-ai") {
            paths.extend(ai::files_for_new());
        }
        paths.sort();
        r.data("files", json!(paths));
        r.data("written", json!(false));
        r.line(format!(
            "giong new would write {} files to {}:",
            paths.len(),
            dir.display()
        ));
        for p in &paths {
            r.line(format!("  {p}"));
        }
        return Ok(r);
    }

    write_all(&dir, &files)?;
    let mut all_paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();

    if !args.has("no-ai") {
        let agent = ai::init_dir(&dir, Some(scaffold::GIONG_VERSION));
        for f in agent.findings {
            r.push(f);
        }
        all_paths.extend(agent.written);
    }
    all_paths.sort();
    let git = if args.has("no-git") {
        false
    } else {
        match git_init(&dir) {
            Ok(made) => made,
            Err(why) => {
                r.push(
                    Finding::new(
                        Code::GIT_INIT_FAILED,
                        Severity::Warning,
                        "no git repository was started",
                    )
                    .detail(why)
                    .fix("run git init in the project when you want one (giong fix needs one)"),
                );
                false
            }
        }
    };
    r.data("files", json!(all_paths));
    r.data("written", json!(true));
    r.data("git", json!(git));

    let cd = format!("cd {}", dir.display());
    let mut next = vec![cd];
    let offer = ai::Offer::here();
    if !o.dbs.is_empty() && offer.db {
        next.push("giong db up".into());
    }
    next.push(
        match (offer.dev_background, offer.dev) {
            (true, _) => "giong dev --background",
            (false, true) => "giong dev",
            _ => "cargo run",
        }
        .into(),
    );
    r.data("next", json!(next));

    let what = match (o.template, o.dbs.is_empty()) {
        (Template::Web, true) => "an HTTP service".to_string(),
        (Template::Web, false) => format!("an HTTP service with {}", db_list(&o)),
        (Template::Grpc, _) => "a gRPC service".to_string(),
        (Template::Worker, true) => "a worker that runs a job on a timer".to_string(),
        (Template::Worker, false) => format!("a worker that takes jobs from {}", db_list(&o)),
    };
    r.line(format!(
        "Created {what} in {} ({} files, giong {}).",
        dir.display(),
        all_paths.len(),
        scaffold::GIONG_VERSION
    ));
    r.line(String::new());
    r.line("Next:".to_string());
    for step in next {
        r.line(format!("  {step}"));
    }
    r.line(String::new());
    r.line("Without the giong command: cargo run, cargo test.".to_string());
    Ok(r)
}

fn db_list(o: &Options) -> String {
    let names: Vec<&str> = o
        .dbs
        .iter()
        .map(|d| match d {
            scaffold::Db::Postgres => "PostgreSQL",
            scaffold::Db::Mysql => "MySQL",
            scaffold::Db::Redis => "Redis",
        })
        .collect();
    names.join(" and ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;

    fn parse(line: &str) -> Args {
        let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        args::parse(&argv, COMMAND.flags, COMMAND.max_words).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("giong-new-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn options_from_the_command_line() {
        let (dir, o) = options(&parse("new path/to/user-api --db redis,pg --tls")).unwrap();
        assert_eq!(dir, PathBuf::from("path/to/user-api"));
        assert_eq!(o.name, "user-api");
        assert_eq!(o.template, Template::Web);
        assert_eq!(o.dbs, [scaffold::Db::Postgres, scaffold::Db::Redis]);
        assert!(o.tls);
        let (_, o) = options(&parse("new . --name svc --template worker")).unwrap();
        assert_eq!((o.name.as_str(), o.template), ("svc", Template::Worker));

        let code = |line: &str| options(&parse(line)).unwrap_err().code;
        assert_eq!(code("new"), Code::USAGE);
        assert_eq!(code("new /"), Code::BAD_NAME);
        // `yy/.` is yy; `.` and `..` take the name of the directory they mean.
        let (dir, o) = options(&parse("new yy/.")).unwrap();
        assert_eq!((dir, o.name.as_str()), (PathBuf::from("yy"), "yy"));
        assert_eq!(without_dots(Path::new("./.")), PathBuf::from("."));
        assert!(resolved(Path::new("a/b/../c/.")).ends_with("a/c"));
        assert_eq!(code("new My-Api"), Code::BAD_NAME);
        assert_eq!(code("new x --template rocket"), Code::BAD_TEMPLATE_CHOICE);
        assert_eq!(code("new x --db mongo"), Code::BAD_TEMPLATE_CHOICE);
        assert_eq!(code("new x --db postgres,mysql"), Code::BAD_TEMPLATE_CHOICE);
        assert_eq!(
            code("new x --template grpc --db redis"),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            code("new x --giong-path /nonexistent"),
            Code::BAD_GIONG_PATH
        );
        assert_eq!(code("new x --giong-path /"), Code::BAD_GIONG_PATH);
    }

    #[test]
    fn a_checkout_is_accepted_by_path() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        if !repo.join("giong-web").exists() {
            return; // the published crate
        }
        let line = format!("new x --giong-path {}", repo.display());
        let (_, o) = options(&parse(&line)).unwrap();
        let path = o.giong_path.unwrap();
        assert!(!path.contains(".."), "canonical: {path}");
    }

    #[test]
    fn writes_a_project_and_refuses_to_overwrite_one() {
        let dir = scratch("write");
        let r = run(&parse(&format!("new {} --no-git --no-ai", dir.display()))).unwrap();
        assert!(r.ok());
        assert!(dir.join("Cargo.toml").is_file());
        assert!(dir.join(".github/workflows/ci.yml").is_file());
        assert!(dir.join("src/hello.rs").is_file());
        let files = r.data_value("files").unwrap().as_array().unwrap().len();
        assert_eq!(
            files,
            scaffold::render(&Options::new(&project_name(&dir).unwrap(), Template::Web))
                .unwrap()
                .len()
        );

        let again = run(&parse(&format!("new {} --no-git", dir.display()))).unwrap_err();
        assert_eq!(again.code, Code::TARGET_NOT_EMPTY);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_directory_is_fine_and_a_file_is_not() {
        let dir = scratch("empty");
        fs::create_dir_all(&dir).unwrap();
        run(&parse(&format!(
            "new {} --no-git --no-ai --template worker",
            dir.display()
        )))
        .unwrap();
        assert!(dir.join("tests/jobs.rs").is_file());
        fs::remove_dir_all(&dir).unwrap();

        let file = scratch("file");
        fs::write(&file, "x").unwrap();
        let f = run(&parse(&format!("new {} --no-git", file.display()))).unwrap_err();
        assert_eq!(f.code, Code::TARGET_NOT_EMPTY);
        assert!(f.fix.contains("file"), "{}", f.fix);
        fs::remove_file(&file).unwrap();
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = scratch("dry");
        let r = run(&parse(&format!(
            "new {} --dry-run --db postgres",
            dir.display()
        )))
        .unwrap();
        assert!(!dir.exists());
        let files = r.data_value("files").unwrap().as_array().unwrap();
        assert!(files.iter().any(|f| f == "migrations/0001_init.sql"));
        assert_eq!(r.data_value("written").unwrap(), false);
    }

    #[test]
    fn a_write_that_fails_leaves_nothing_behind() {
        let base = scratch("partial");
        // A file where a directory has to go: a.txt/ cannot be made.
        let files = vec![
            scaffold::File {
                path: "a.txt".into(),
                text: "a".into(),
            },
            scaffold::File {
                path: "src/deep/x.rs".into(),
                text: "x".into(),
            },
            scaffold::File {
                path: "a.txt/b.txt".into(),
                text: "b".into(),
            },
        ];
        // Every directory it made goes, the parents of the project too.
        let dir = base.join("n/a/f3");
        fs::create_dir_all(&base).unwrap();
        let f = write_all(&dir, &files).unwrap_err();
        assert_eq!(f.code, Code::WRITE_FAILED);
        assert_eq!(fs::read_dir(&base).unwrap().count(), 0, "n/ is gone");

        // A directory that was there stays, and is empty again.
        write_all(&base, &files).unwrap_err();
        assert!(base.exists());
        assert_eq!(fs::read_dir(&base).unwrap().count(), 0);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dry_run_refuses_what_a_run_refuses_and_lists_the_agent_files() {
        let dir = scratch("dry-full");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x"), "x").unwrap();
        let f = run(&parse(&format!("new {} --dry-run", dir.display()))).unwrap_err();
        assert_eq!(f.code, Code::TARGET_NOT_EMPTY);
        fs::remove_dir_all(&dir).unwrap();

        let r = run(&parse(&format!("new {} --dry-run", dir.display()))).unwrap();
        let files = r.data_value("files").unwrap().as_array().unwrap().clone();
        for agent in ai::files_for_new() {
            assert!(files.iter().any(|f| f == agent.as_str()), "{agent}");
        }
        assert!(!dir.exists());
    }

    #[test]
    fn git_and_agent_files() {
        let dir = scratch("git");
        let r = run(&parse(&format!("new {} --template grpc", dir.display()))).unwrap();
        assert!(dir.join("AGENTS.md").is_file());
        assert!(dir.join("CLAUDE.md").is_file());
        assert_eq!(dir.join(".mcp.json").is_file(), ai::Offer::here().mcp);
        let git = r.data_value("git").unwrap().as_bool().unwrap();
        assert_eq!(git, dir.join(".git").is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }
}
