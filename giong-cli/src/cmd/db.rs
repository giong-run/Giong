//! `giong db`: a development database in one command, and plain SQL
//! migrations.
//!
//! `giong db up` starts PostgreSQL, MySQL or Redis in Docker or Podman
//! (whichever answers; `GIONG_CONTAINER_RUNTIME` picks one), waits until it
//! accepts queries over TCP, and writes `DATABASE_URL` / `REDIS_URL` into
//! `.env`. What it started is kept in `.giong/db.json`, so `down`, `status`
//! and `reset` act on that and nothing else. SQLite (a project on rusqlite)
//! has no server: `up` only makes sure `.env` names its file
//! (`sqlite:data/<crate>.db`), and `reset` empties that file when it is in
//! the project.
//!
//! Migrations: `migrations/NNNN_name.sql` (at least four digits, unique,
//! applied in order), plain SQL, forward only. Each file runs once, in one
//! transaction on PostgreSQL (unless its first line is
//! `-- giong:no-transaction`); MySQL commits DDL statement by statement, so
//! there a file is recorded only after its last statement succeeded. Applied
//! files are recorded in `giong_migrations` with a SHA-256 checksum, and a
//! changed applied file is refused. The SQL goes through the database's own
//! client: `psql` / `mysql` inside the container giong started, or installed
//! locally for any other `DATABASE_URL`; `sqlite3` for SQLite, where a file
//! runs in one transaction too.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dotenv;
use crate::project::find_manifest_dir;
use crate::report::{Failure, Finding, Report, Severity};
use crate::sha256;
use crate::state;

pub const COMMAND: Command = Command {
    name: "db",
    summary: "start a development database and apply SQL migrations",
    usage:
        "giong db up [--engine sqlite|postgres|mysql|redis] [--port <n>] [--image <ref>] [--timeout <s>]\n\
            giong db down [--volumes]\n\
            giong db status\n\
            giong db new <name>\n\
            giong db migrate\n\
            giong db reset [--yes]\n\n\
            up: starts the database the project uses (from its giong-postgres, giong-mysql or \
            giong-redis dependency, or --engine) in Docker or Podman, waits until it answers \
            queries, and writes DATABASE_URL or REDIS_URL into .env (other keys untouched). Run \
            again, it reuses what it started. SQLite (a rusqlite dependency) starts nothing: up \
            writes DATABASE_URL=sqlite:data/<crate>.db into .env unless it names a file \
            already, and migrate, status and reset use the sqlite3 shell. down: stops and removes it; --volumes also \
            deletes its data. status: what runs, where, and which migrations are applied or \
            pending. new: writes migrations/NNNN_<name>.sql with the next number. migrate: \
            applies the pending migrations in order, each once, each in a transaction on \
            PostgreSQL; an applied file that was changed is refused. reset: drops and \
            recreates the database giong started, then migrates (only that one, never \
            another DATABASE_URL; for SQLite, only a file inside the project; --yes is \
            required).",
    flags: &[
        Flag {
            name: "engine",
            kind: Kind::Value,
            help: "sqlite, postgres, mysql or redis (default: from the project's dependencies)",
        },
        Flag {
            name: "port",
            kind: Kind::Value,
            help: "the port on 127.0.0.1 (default: the engine's usual port when free, else any)",
        },
        Flag {
            name: "image",
            kind: Kind::Value,
            help: "the container image (default postgres:17-alpine, mysql:8.4, redis:7-alpine)",
        },
        Flag {
            name: "timeout",
            kind: Kind::Value,
            help: "seconds to wait for the database to answer (default 120)",
        },
        Flag {
            name: "volumes",
            kind: Kind::Switch,
            help: "down: also delete the database's data",
        },
        Flag {
            name: "yes",
            kind: Kind::Switch,
            help: "reset: confirm that the data may be deleted",
        },
    ],
    max_words: 2,
    run,
};

/// A database engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Postgres,
    Mysql,
    /// A file, through the application's own SQLite: nothing to start.
    Sqlite,
    Redis,
}

impl Engine {
    pub fn parse(s: &str) -> Option<Engine> {
        match s.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(Engine::Postgres),
            "mysql" | "mariadb" => Some(Engine::Mysql),
            "sqlite" | "sqlite3" => Some(Engine::Sqlite),
            "redis" => Some(Engine::Redis),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
            Engine::Sqlite => "sqlite",
            Engine::Redis => "redis",
        }
    }

    /// Whether `giong db up` runs a server for it. The container settings
    /// below are those of the engines that have one; SQLite's are empty and
    /// never used.
    pub fn has_server(self) -> bool {
        self != Engine::Sqlite
    }

    fn default_image(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres:17-alpine",
            Engine::Mysql => "mysql:8.4",
            Engine::Redis => "redis:7-alpine",
            Engine::Sqlite => "",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mysql => 3306,
            Engine::Redis => 6379,
            Engine::Sqlite => 0,
        }
    }

    fn data_dir(self) -> &'static str {
        match self {
            Engine::Postgres => "/var/lib/postgresql/data",
            Engine::Mysql => "/var/lib/mysql",
            Engine::Redis => "/data",
            Engine::Sqlite => "",
        }
    }

    /// The `.env` key its URL goes into.
    pub fn env_key(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::Mysql | Engine::Sqlite => "DATABASE_URL",
            Engine::Redis => "REDIS_URL",
        }
    }

    fn sql(self) -> bool {
        self != Engine::Redis
    }
}

/// What `giong db up` started for one engine, kept in `.giong/db.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub engine: Engine,
    pub runtime: String,
    pub container: String,
    pub volume: String,
    pub image: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl Started {
    pub fn url(&self) -> String {
        match self.engine {
            Engine::Postgres => format!(
                "postgres://{}:{}@127.0.0.1:{}/{}",
                self.user, self.password, self.port, self.database
            ),
            Engine::Mysql => format!(
                "mysql://{}:{}@127.0.0.1:{}/{}",
                self.user, self.password, self.port, self.database
            ),
            Engine::Redis => format!("redis://127.0.0.1:{}", self.port),
            Engine::Sqlite => String::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "runtime": self.runtime, "container": self.container, "volume": self.volume,
            "image": self.image, "port": self.port, "user": self.user,
            "password": self.password, "database": self.database,
        })
    }

    /// Why `giong db up` could not have written this entry for the checkout
    /// at `root`, or `None` when it could have.
    fn problem(&self, project: &str, root: &Path) -> Option<String> {
        let base = container_base(project, root, self.engine);
        let word = |s: &str, max: usize| {
            s.len() <= max
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        if !known_runtime(&self.runtime) {
            Some(format!(
                "its runtime {:?} is neither docker nor podman",
                self.runtime
            ))
        } else if self.container != base {
            Some(format!(
                "its container {:?} is not this checkout's ({base})",
                self.container
            ))
        } else if self.volume != format!("{base}-data") {
            Some(format!(
                "its volume {:?} is not this checkout's ({base}-data)",
                self.volume
            ))
        } else if !plain_image(&self.image) {
            Some(format!(
                "its image {:?} is not a plain image name",
                self.image
            ))
        } else if !word(&self.user, 63) || !word(&self.database, 63) {
            Some("its user or database is not a plain name (a-z, 0-9, _)".to_string())
        } else if self.password.len() > 128
            || !self.password.chars().all(|c| c.is_ascii_alphanumeric())
        {
            Some("its password is not letters and digits".to_string())
        } else {
            None
        }
    }

    fn from_json(engine: Engine, v: &Value) -> Option<Started> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        Some(Started {
            engine,
            runtime: s("runtime")?,
            container: s("container")?,
            volume: s("volume")?,
            image: s("image")?,
            port: u16::try_from(v["port"].as_u64()?).ok()?,
            user: s("user")?,
            password: s("password")?,
            database: s("database")?,
        })
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join(".giong").join("db.json")
}

/// Everything `giong db up` started in this project: the entries of
/// `.giong/db.json` that it could have written for this checkout. Others are
/// left out (see [`rejected_state`]).
pub fn read_state(root: &Path) -> Vec<Started> {
    load_state(root).0
}

/// The entries of `.giong/db.json` that are left out, with why. A cloned
/// repository can commit that file, and its fields name the program run, the
/// volume mounted, the image started and the database dropped.
pub fn rejected_state(root: &Path) -> Vec<(Option<Engine>, String)> {
    load_state(root).1
}

fn load_state(root: &Path) -> (Vec<Started>, Vec<(Option<Engine>, String)>) {
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    let text = match state::read_to_string(&state_path(root)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (good, bad),
        Err(e) => {
            bad.push((None, format!("it cannot be read: {e}")));
            return (good, bad);
        }
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (good, bad);
    };
    let project = project_name(root);
    for e in [Engine::Postgres, Engine::Mysql, Engine::Redis] {
        if v[e.name()].is_null() {
            continue;
        }
        match Started::from_json(e, &v[e.name()]) {
            None => bad.push((Some(e), "it lacks fields giong db up writes".to_string())),
            Some(s) => match s.problem(&project, root) {
                None => good.push(s),
                Some(why) => bad.push((Some(e), why)),
            },
        }
    }
    (good, bad)
}

/// The runtimes `giong db up` writes: `docker`, `podman`, or the one
/// `GIONG_CONTAINER_RUNTIME` names now (the user's own setting).
fn known_runtime(runtime: &str) -> bool {
    matches!(runtime, "docker" | "podman")
        || std::env::var("GIONG_CONTAINER_RUNTIME").is_ok_and(|r| !r.is_empty() && r == runtime)
}

/// A plain image reference (`postgres:17-alpine`, `ghcr.io/org/pg@sha256:...`):
/// nothing a container runtime could read as an option.
fn plain_image(image: &str) -> bool {
    image.len() <= 255
        && image
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '@' | '-'))
}

/// One of the official images of `engine` (any tag): safe to start for a
/// project whatever its files say, since the repository chose only a version.
fn official_image(engine: Engine, image: &str) -> bool {
    let name = image.strip_prefix("docker.io/").unwrap_or(image);
    let name = name.strip_prefix("library/").unwrap_or(name);
    let (repo, tag) = name.split_once(':').unwrap_or((name, "latest"));
    let repos: &[&str] = match engine {
        Engine::Postgres => &["postgres"],
        Engine::Mysql => &["mysql", "mariadb"],
        Engine::Redis => &["redis", "valkey/valkey"],
        Engine::Sqlite => &[],
    };
    repos.contains(&repo)
        && !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `.giong/processes.json`, for the message of a failed write.
fn procs_path(root: &Path) -> PathBuf {
    root.join(".giong").join("processes.json")
}

fn write_state(root: &Path, all: &[Started]) -> Result<(), Failure> {
    let mut m = Map::new();
    for s in all {
        m.insert(s.engine.name().into(), s.to_json());
    }
    let path = state_path(root);
    // Never through a link: the project can come from a cloned repository.
    let write = || -> std::io::Result<()> {
        state::ensure(root)?;
        if all.is_empty() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => return Ok(()),
            }
        }
        let text = serde_json::to_string_pretty(&Value::Object(m))? + "\n";
        state::replace_private(&path, text.as_bytes())
    };
    write().map_err(|e| io_failure(&path, e))
}

fn io_failure(path: &Path, e: std::io::Error) -> Failure {
    Failure::new(
        Code::USAGE,
        format!("cannot write {}: {e}", path.display()),
        "check that the project directory is writable",
    )
}

/// The project's root and package name.
fn project() -> Result<(PathBuf, String), Failure> {
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("cannot read the current directory: {e}"),
            "run giong db inside a project",
        )
    })?;
    let root = find_manifest_dir(&cwd).ok_or_else(|| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("no Cargo.toml in {} or above it", cwd.display()),
            "run giong db inside a project, or start one with giong new <name>",
        )
    })?;
    let name = project_name(&root);
    Ok((root, name))
}

/// The package name of the project at `root`, else its directory's name.
fn project_name(root: &Path) -> String {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    package_name(&manifest).unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into())
    })
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "name" {
                    return Some(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

/// The container's name for `engine` in the project at `root`:
/// `giong-<package>-<engine>-<6 hex digits of the root's path>`, so two
/// checkouts of one project never share a database. The volume adds
/// `-data`.
fn container_base(project: &str, root: &Path, engine: Engine) -> String {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let hash = sha256::hex(root.to_string_lossy().as_bytes());
    format!(
        "giong-{}-{}-{}",
        slug(project).replace('_', "-"),
        engine.name(),
        &hash[..6]
    )
}

/// A name a container, a volume and a database accept: lower case letters,
/// digits and `_`, starting with a letter, at most 48 characters.
pub fn slug(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_');
    let mut s = if s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        s.to_string()
    } else {
        format!("app_{s}").trim_end_matches('_').to_string()
    };
    s.truncate(48);
    s
}

/// The engines the project's dependencies name.
pub fn engines_in(manifest: &str) -> Vec<Engine> {
    let mut out = Vec::new();
    for (krate, engine) in [
        ("giong-postgres", Engine::Postgres),
        ("giong-mysql", Engine::Mysql),
        ("rusqlite", Engine::Sqlite),
        ("giong-redis", Engine::Redis),
    ] {
        let named = manifest.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with('#')
                && (l.starts_with(&format!("{krate} "))
                    || l.starts_with(&format!("{krate}="))
                    || l.contains(&format!("dependencies.{krate}]")))
        });
        if named {
            out.push(engine);
        }
    }
    out
}

/// A password of 24 hex characters, from the system's random source.
fn password() -> String {
    let mut seed = Vec::new();
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = [0u8; 32];
        if f.read_exact(&mut buf).is_ok() {
            seed.extend_from_slice(&buf);
        }
    }
    // Elsewhere (Windows): enough for a local development database.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    seed.extend_from_slice(&now.as_nanos().to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let stack = 0u8;
    seed.extend_from_slice(&(std::ptr::addr_of!(stack) as usize).to_le_bytes());
    sha256::hex(&seed)[..24].to_string()
}

/// The container runtime: `GIONG_CONTAINER_RUNTIME`, else Docker, else
/// Podman, whichever answers.
fn container_runtime() -> Result<String, Failure> {
    let forced = std::env::var("GIONG_CONTAINER_RUNTIME")
        .ok()
        .filter(|s| !s.is_empty());
    let candidates: Vec<String> = match &forced {
        Some(f) => vec![f.clone()],
        None => vec!["docker".into(), "podman".into()],
    };
    let mut tried = Vec::new();
    let mut windows = false;
    for c in &candidates {
        let out = Proc::new(c)
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdin(Stdio::null())
            .output();
        match out {
            // Docker Desktop on Windows answers in Windows-container mode
            // too, where no database image pulls: say so instead of failing
            // later on a pull.
            Ok(o) if o.status.success() && windows_containers(c) => {
                windows = true;
                tried.push(format!("{c}: runs Windows containers"));
            }
            Ok(o) if o.status.success() => return Ok(c.clone()),
            Ok(o) => tried.push(format!(
                "{c}: {}",
                String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .next()
                    .unwrap_or("does not answer")
            )),
            Err(e) => tried.push(format!("{c}: {e}")),
        }
    }
    if windows {
        return Err(Failure::new(
            Code::NO_CONTAINER_RUNTIME,
            format!(
                "no container runtime runs Linux containers ({})",
                tried.join("; ")
            ),
            "switch Docker Desktop to Linux containers (its tray icon: Switch to Linux \
             containers), or set DATABASE_URL to a database you run yourself and use giong db \
             migrate",
        ));
    }
    Err(Failure::new(
        Code::NO_CONTAINER_RUNTIME,
        format!("no container runtime answers ({})", tried.join("; ")),
        "install and start Docker or Podman (on a Mac: Docker Desktop, OrbStack or podman \
         machine start), or set DATABASE_URL to a database you run yourself and use giong db \
         migrate",
    ))
}

/// Whether `runtime` runs Windows containers. Podman has no `OSType` and runs
/// only Linux containers, so a failed query counts as no.
fn windows_containers(runtime: &str) -> bool {
    Proc::new(runtime)
        .args(["info", "--format", "{{.OSType}}"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && is_windows_os_type(&o.stdout))
}

fn is_windows_os_type(out: &[u8]) -> bool {
    String::from_utf8_lossy(out).trim() == "windows"
}

fn runtime_cmd(runtime: &str, args: &[&str]) -> Result<std::process::Output, Failure> {
    Proc::new(runtime)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            Failure::new(
                Code::NO_CONTAINER_RUNTIME,
                format!("cannot run {runtime}: {e}"),
                "install and start Docker or Podman",
            )
        })
}

/// `running`, `exited`, ... or `None` when the container does not exist.
fn container_state(runtime: &str, name: &str) -> Option<String> {
    let out = Proc::new(runtime)
        .args(["inspect", "--format", "{{.State.Status}}", name])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn any_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// How the SQL of a database is run.
#[derive(Clone, Debug)]
pub enum Runner {
    /// Through the client inside the container giong started.
    Container(Started),
    /// Through the client installed here, for any other URL.
    Local { engine: Engine, url: String },
}

/// The pieces of a `mysql://user:password@host:port/db` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlParts {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Bytes, not `&s[..]`: the two after a '%' may cut a character. And
        // two hex digits, not from_str_radix alone, which also takes "+5".
        let hex = b.get(i + 1..i + 3).and_then(|h| {
            let (hi, lo) = ((h[0] as char).to_digit(16)?, (h[1] as char).to_digit(16)?);
            Some((hi * 16 + lo) as u8)
        });
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A URL's `scheme`, the user and password before its host (as written,
/// percent-encoded; the password `None` without a `:`), and what follows
/// the `@`; `None` without an `@` before the host.
///
/// Clients get the password in their environment, never in their command
/// line, which every user of the machine can read (`ps`,
/// `/proc/PID/cmdline`): a `DATABASE_URL` is often a shared server's.
fn credentials(url: &str) -> Option<(&str, &str, Option<&str>, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (auth, _) = rest[..end].rsplit_once('@')?;
    let host = &rest[auth.len() + 1..];
    Some(match auth.split_once(':') {
        Some((user, password)) => (scheme, user, Some(password), host),
        None => (scheme, auth, None, host),
    })
}

/// The URL without its password, and the password (decoded; `None` when
/// the URL has none). The user stays: `postgres://user@host/db`. libpq
/// also reads a `password=` in the query, which wins over the one before
/// the host, as it does there.
fn without_password(url: &str) -> (String, Option<String>) {
    let (bare, before_host) = match credentials(url) {
        Some((scheme, "", Some(password), host)) => {
            (format!("{scheme}://{host}"), Some(percent_decode(password)))
        }
        Some((scheme, user, Some(password), host)) => (
            format!("{scheme}://{user}@{host}"),
            Some(percent_decode(password)),
        ),
        _ => (url.to_string(), None),
    };
    let (bare, in_query) = without_query_password(&bare);
    (bare, in_query.or(before_host))
}

/// The URL without the `password=` pairs of its query, and the last one's
/// value (decoded).
fn without_query_password(url: &str) -> (String, Option<String>) {
    let Some(q) = url.find('?') else {
        return (url.to_string(), None);
    };
    let (base, rest) = (&url[..q], &url[q + 1..]);
    let (query, fragment) = rest.split_at(rest.find('#').unwrap_or(rest.len()));
    let mut password = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| match pair.split_once('=') {
            Some(("password", v)) => {
                password = Some(percent_decode(v));
                false
            }
            _ => !pair.is_empty(),
        })
        .collect();
    if password.is_none() {
        return (url.to_string(), None);
    }
    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(fragment);
    (out, password)
}

/// The URL without its user and password, and those two (decoded; `None`
/// when the URL has neither). redis-cli reads `redis://secret@host`, with
/// no `:`, as a password, so that form gives an empty user.
fn without_credentials(url: &str) -> (String, Option<(String, String)>) {
    match credentials(url) {
        Some((scheme, user, Some(password), host)) => (
            format!("{scheme}://{host}"),
            Some((percent_decode(user), percent_decode(password))),
        ),
        Some((scheme, password, None, host)) => (
            format!("{scheme}://{host}"),
            Some((String::new(), percent_decode(password))),
        ),
        None => (url.to_string(), None),
    }
}

/// Splits a database URL (`scheme://user:password@host:port/db?query`).
pub fn parse_url(url: &str) -> Option<(Engine, UrlParts)> {
    let (scheme, rest) = url.split_once("://")?;
    let engine = match scheme {
        "postgres" | "postgresql" => Engine::Postgres,
        "mysql" | "mariadb" => Engine::Mysql,
        "redis" | "rediss" => Engine::Redis,
        _ => return None,
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (auth_host, database) = match rest.split_once('/') {
        Some((a, d)) => (a, percent_decode(d)),
        None => (rest, String::new()),
    };
    let (auth, host_port) = match auth_host.rsplit_once('@') {
        Some((a, h)) => (a, h),
        None => ("", auth_host),
    };
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (percent_decode(u), percent_decode(p)),
        None => (percent_decode(auth), String::new()),
    };
    let (host, port) = if let Some(h) = host_port.strip_prefix('[') {
        let (h, after) = h.split_once(']')?;
        (
            h.to_string(),
            after.strip_prefix(':').and_then(|p| p.parse().ok()),
        )
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse().ok()?)),
            None => (host_port.to_string(), None),
        }
    };
    Some((
        engine,
        UrlParts {
            user,
            password,
            host,
            port,
            database,
        },
    ))
}

/// What running some SQL gave.
struct SqlOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Runner {
    pub fn engine(&self) -> Engine {
        match self {
            Runner::Container(s) => s.engine,
            Runner::Local { engine, .. } => *engine,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Runner::Container(s) => format!(
                "{} in {} container {}",
                s.engine.name(),
                s.runtime,
                s.container
            ),
            Runner::Local { engine, url } => format!("{} at {}", engine.name(), redact(url)),
        }
    }

    /// Runs `sql` on `database` (`None`: the configured one), stopping at
    /// the first error; rows come back tab-separated, one per line.
    fn run(&self, sql: &str, database: Option<&str>, admin: bool) -> Result<SqlOut, Failure> {
        let mut cmd;
        match self {
            Runner::Container(s) => {
                cmd = Proc::new(&s.runtime);
                cmd.args(["exec", "-i"]);
                match s.engine {
                    Engine::Postgres => {
                        cmd.args(["-e", &format!("PGPASSWORD={}", s.password), &s.container]);
                        cmd.args(psql_args());
                        cmd.args(["-h", "127.0.0.1", "-U", &s.user, "-d"]);
                        cmd.arg(database.unwrap_or(&s.database));
                    }
                    Engine::Mysql => {
                        cmd.args(["-e", &format!("MYSQL_PWD={}", s.password), &s.container]);
                        cmd.args(["mysql", "-h127.0.0.1", "-N", "-B"]);
                        cmd.arg(if admin {
                            "-uroot".to_string()
                        } else {
                            format!("-u{}", s.user)
                        });
                        if let Some(db) = database.or(Some(&s.database)).filter(|d| !d.is_empty()) {
                            cmd.arg(db);
                        }
                    }
                    Engine::Redis => {
                        cmd.arg(&s.container);
                        cmd.args(["redis-cli"]);
                    }
                    Engine::Sqlite => return Err(bad_url("sqlite in a container")),
                }
            }
            Runner::Local { engine, url } => match engine {
                Engine::Postgres => {
                    cmd = Proc::new("psql");
                    cmd.args(psql_args());
                    let target = match database {
                        Some(db) => replace_database(url, db),
                        None => url.clone(),
                    };
                    let (target, password) = without_password(&target);
                    if let Some(p) = password.filter(|p| !p.is_empty()) {
                        cmd.env("PGPASSWORD", p);
                    }
                    cmd.args(["-d", &target]);
                }
                Engine::Mysql => {
                    let (_, p) = parse_url(url).ok_or_else(|| bad_url(url))?;
                    cmd = Proc::new("mysql");
                    cmd.env("MYSQL_PWD", &p.password);
                    cmd.args([
                        "-N",
                        "-B",
                        "-h",
                        if p.host.is_empty() {
                            "127.0.0.1"
                        } else {
                            &p.host
                        },
                    ]);
                    cmd.arg(format!("-P{}", p.port.unwrap_or(3306)));
                    if !p.user.is_empty() {
                        cmd.arg(format!("-u{}", p.user));
                    }
                    let db = database.unwrap_or(&p.database).to_string();
                    if !db.is_empty() {
                        cmd.arg(db);
                    }
                }
                Engine::Redis => {
                    cmd = Proc::new("redis-cli");
                    // redis-cli reads `redis://name@host` as a password, so
                    // the user goes in --user and the password in
                    // REDISCLI_AUTH, which it reads when -u gave none.
                    let target = match without_credentials(url) {
                        (bare, Some((user, password))) => {
                            if !user.is_empty() {
                                cmd.args(["--user", &user]);
                            }
                            if !password.is_empty() {
                                cmd.env("REDISCLI_AUTH", password);
                            }
                            bare
                        }
                        (target, None) => target,
                    };
                    cmd.args(["-u", &target]);
                }
                Engine::Sqlite => {
                    let path = sqlite_path(url).ok_or_else(|| bad_url(url))?;
                    cmd = Proc::new("sqlite3");
                    // Stop at the first error, rows tab-separated without a
                    // header, and wait for a lock the service holds.
                    cmd.args(["-bail", "-batch", "-noheader", "-separator", "\t"]);
                    cmd.args(["-cmd", ".timeout 5000"]);
                    cmd.arg(path);
                }
            },
        }
        let program = format!("{:?}", cmd.get_program());
        let out = feed(&mut cmd, sql).map_err(|e| client_missing(self, &program, e))?;
        Ok(SqlOut {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Runs `cmd` with `input` on its standard input and its output captured.
/// The input is written on its own thread while the output is read: a
/// client prints as it reads (rows, notices), and once both pipes are full a
/// writer that waits for the whole script to go in first never finishes.
fn feed(cmd: &mut Proc, input: &str) -> std::io::Result<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.to_owned();
        std::thread::spawn(move || {
            // A client that exits early (bad credentials) closes its end:
            // the error is in its output, not in this write.
            let _ = stdin.write_all(input.as_bytes());
        })
    });
    let out = child.wait_with_output();
    if let Some(w) = writer {
        let _ = w.join();
    }
    out
}

/// psql reading the script from standard input, stopping at the first
/// error, printing rows unaligned and tab-separated.
fn psql_args() -> [&'static str; 9] {
    [
        "psql",
        "-X",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "-At",
        "-F",
        "\t",
        "-f-",
    ]
}

/// What the service of a project on rusqlite opens without `DATABASE_URL`
/// (the template's default), so `giong db migrate` works before `.env` exists.
fn default_sqlite_url(root: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let engines = engines_in(&manifest);
    (engines.contains(&Engine::Sqlite) && !engines.iter().any(|e| e.sql() && e.has_server()))
        .then(|| format!("sqlite:data/{}.db", project_name(root).replace('-', "_")))
}

/// The file of a `sqlite:path` or `sqlite://path` URL (`sqlite:///abs`
/// for an absolute one), as the template's service reads it.
pub fn sqlite_path(url: &str) -> Option<PathBuf> {
    url.strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .filter(|p| !p.is_empty() && !p.starts_with(':'))
        .map(PathBuf::from)
}

/// A URL with the password replaced by `***`.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((auth, host)) = rest.rsplit_once('@') else {
        return url.to_string();
    };
    match auth.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

/// The URL with its database (the path) replaced.
fn replace_database(url: &str, db: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (before_query, query) = match rest.find(['?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = before_query.split('/').next().unwrap_or("");
    format!("{scheme}://{host}/{db}{query}")
}

fn bad_url(url: &str) -> Failure {
    Failure::new(
        Code::DB_URL,
        format!("{} is not a database URL giong db understands", redact(url)),
        "write it as postgres://user:password@host:5432/db, mysql://user:password@host:3306/db, \
         sqlite:path/to/file.db or redis://host:6379",
    )
}

fn client_missing(r: &Runner, program: &str, e: std::io::Error) -> Failure {
    let client =
        match r.engine() {
            Engine::Postgres => "psql (postgresql-client)",
            Engine::Mysql => "mysql (mysql-client or mariadb-client)",
            Engine::Redis => "redis-cli (redis-tools)",
            Engine::Sqlite => return Failure::new(
                Code::DB_CLIENT_MISSING,
                format!("cannot run {program} for {}: {e}", r.describe()),
                "install the sqlite3 shell (apt install sqlite3, brew install sqlite; macOS has \
                 it); the service itself needs nothing, it creates its tables at start",
            ),
        };
    Failure::new(
        Code::DB_CLIENT_MISSING,
        format!("cannot run {program} for {}: {e}", r.describe()),
        format!(
            "install {client}, or let giong db up start the database (its container has the \
             client)"
        ),
    )
}

/// The runner for an engine: the container giong started, else the URL in
/// the environment or `.env`.
fn runner(root: &Path, want: Option<Engine>) -> Result<Runner, Failure> {
    let started = read_state(root);
    let sql_started: Vec<&Started> = started
        .iter()
        .filter(|s| want.map_or(s.engine.sql(), |w| s.engine == w))
        .collect();
    if let Some(s) = sql_started.first() {
        let st = container_state(&s.runtime, &s.container);
        if st.as_deref() != Some("running") {
            return Err(Failure::new(
                Code::DB_NOT_READY,
                format!(
                    "the {} container {} that giong db up started is {}",
                    s.engine.name(),
                    s.container,
                    st.as_deref().unwrap_or("removed")
                ),
                "giong db up starts it again, with its data",
            ));
        }
        return Ok(Runner::Container((*s).clone()));
    }
    let key = want.map_or("DATABASE_URL", Engine::env_key);
    let url = std::env::var(key)
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            state::read_to_string(&root.join(".env"))
                .ok()
                .map(|t| dotenv::parse(&t))
                .and_then(|(vars, _)| vars.into_iter().find(|v| v.key == key).map(|v| v.value))
                .filter(|u| !u.is_empty())
        })
        .or_else(|| default_sqlite_url(root).filter(|_| key == "DATABASE_URL"));
    let Some(url) = url else {
        return Err(Failure::new(
            Code::DB_URL,
            format!("no database to use: giong db up started none here and {key} is not set"),
            format!("run giong db up, or set {key} in the environment or in .env"),
        ));
    };
    if let Some(path) = sqlite_path(&url) {
        if want.is_some_and(|w| w != Engine::Sqlite) {
            return Err(bad_url(&url));
        }
        // Relative to the project, where giong dev starts the service.
        let path = root.join(path);
        return Ok(Runner::Local {
            engine: Engine::Sqlite,
            url: format!("sqlite:{}", path.display()),
        });
    }
    let (engine, _) = parse_url(&url).ok_or_else(|| bad_url(&url))?;
    if let Some(w) = want {
        if w != engine {
            return Err(bad_url(&url));
        }
    }
    Ok(Runner::Local { engine, url })
}

/// One file of `migrations/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    pub version: u64,
    pub name: String,
    pub file: String,
    pub sql: String,
    pub checksum: String,
    pub transaction: bool,
}

/// Reads `migrations/` in order. A `.sql` file whose name does not follow
/// `NNNN_name.sql`, or two files with one version, is an error.
pub fn read_migrations(dir: &Path) -> Result<Vec<Migration>, Failure> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Failure::new(
                Code::MIGRATION_NAME,
                format!("cannot read {}: {e}", dir.display()),
                "check the permissions of migrations/",
            ))
        }
    };
    let mut out: Vec<Migration> = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.ends_with(".sql") || !entry.path().is_file() {
            continue;
        }
        let (version, name) = parse_migration_name(&file).ok_or_else(|| {
            Failure::new(
                Code::MIGRATION_NAME,
                format!("migrations/{file} is not named NNNN_name.sql"),
                "name it with at least four digits, an underscore and lower-case letters, \
                 digits or _ (giong db new <name> writes the next one)",
            )
        })?;
        let bytes = std::fs::read(entry.path()).map_err(|e| {
            Failure::new(
                Code::MIGRATION_NAME,
                format!("cannot read migrations/{file}: {e}"),
                "check the file's permissions",
            )
        })?;
        let sql = String::from_utf8(bytes.clone()).map_err(|_| {
            Failure::new(
                Code::MIGRATION_NAME,
                format!("migrations/{file} is not UTF-8"),
                "save the file as UTF-8",
            )
        })?;
        if let Some(other) = out.iter().find(|m| m.version == version) {
            return Err(Failure::new(
                Code::MIGRATION_NAME,
                format!(
                    "migrations/{file} and migrations/{} have the same version {version}",
                    other.file
                ),
                "renumber one of them (two branches each added a migration: give the newer one \
                 the next free number)",
            ));
        }
        let transaction = !sql
            .lines()
            .next()
            .is_some_and(|l| l.trim() == "-- giong:no-transaction");
        out.push(Migration {
            version,
            name,
            file,
            checksum: sha256::hex(&bytes),
            sql,
            transaction,
        });
    }
    out.sort_by_key(|m| m.version);
    Ok(out)
}

/// `0001_create_users.sql` → (1, "create_users").
pub fn parse_migration_name(file: &str) -> Option<(u64, String)> {
    let stem = file.strip_suffix(".sql")?;
    let (digits, name) = stem.split_once('_')?;
    if digits.len() < 4 || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    Some((digits.parse().ok()?, name.to_string()))
}

const TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS giong_migrations (\
    version BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, checksum CHAR(64) NOT NULL, \
    applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP);\n";

/// An applied migration as `giong_migrations` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub version: u64,
    pub name: String,
    pub checksum: String,
}

fn sql_failure(r: &Runner, what: &str, out: &SqlOut) -> Failure {
    Failure::new(
        Code::DB_NOT_READY,
        format!(
            "{what} failed on {}: {}",
            r.describe(),
            first_lines(&out.stderr, 3)
        ),
        "check that the database runs and that the URL's user, password and database are \
         right (giong db status)",
    )
}

fn first_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).take(n).collect();
    if lines.is_empty() {
        "no message".into()
    } else {
        lines.join(" / ")
    }
}

/// Creates the table if needed and reads it.
fn applied(r: &Runner) -> Result<Vec<Applied>, Failure> {
    let sql = format!(
        "{TABLE_SQL}SELECT version, name, checksum FROM giong_migrations ORDER BY version;\n"
    );
    let out = r.run(&sql, None, false)?;
    if !out.ok {
        return Err(sql_failure(r, "reading giong_migrations", &out));
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let version = f.next()?.trim().parse().ok()?;
            let name = f.next()?.trim().to_string();
            let checksum = f.next()?.trim().to_string();
            Some(Applied {
                version,
                name,
                checksum,
            })
        })
        .collect())
}

/// What migrating would do, checked before anything runs.
#[derive(Debug, Default)]
pub struct Plan {
    pub pending: Vec<Migration>,
    pub changed: Vec<(Migration, Applied)>,
    pub unknown: Vec<Applied>,
    pub applied: usize,
}

pub fn plan(files: &[Migration], done: &[Applied]) -> Plan {
    let mut p = Plan::default();
    for m in files {
        match done.iter().find(|a| a.version == m.version) {
            Some(a) if a.checksum == m.checksum => p.applied += 1,
            Some(a) => p.changed.push((m.clone(), a.clone())),
            None => p.pending.push(m.clone()),
        }
    }
    for a in done {
        if !files.iter().any(|m| m.version == a.version) {
            p.unknown.push(a.clone());
        }
    }
    p
}

/// The script that applies one migration and records it.
pub fn apply_script(engine: Engine, m: &Migration) -> String {
    let record = format!(
        "INSERT INTO giong_migrations (version, name, checksum) VALUES ({}, '{}', '{}');\n",
        m.version, m.name, m.checksum
    );
    let body = format!("{}\n;\n", m.sql.trim_end());
    match engine {
        Engine::Postgres | Engine::Sqlite if m.transaction => {
            format!("BEGIN;\n{body}{record}COMMIT;\n")
        }
        _ => format!("{body}{record}"),
    }
}

fn migrate(r: &Runner, root: &Path, rep: &mut Report) -> Result<(), Failure> {
    let files = read_migrations(&root.join("migrations"))?;
    let done = applied(r)?;
    let p = plan(&files, &done);
    for a in &p.unknown {
        rep.push(
            Finding::new(
                Code::MIGRATION_UNKNOWN,
                Severity::Warning,
                format!("the database has migration {} ({}) that migrations/ does not", a.version, a.name),
            )
            .detail("it was applied from another branch or a deleted file")
            .fix("restore the file, or leave it if the branch it came from is gone (giong db reset starts over)"),
        );
    }
    if let Some((m, _)) = p.changed.first() {
        for (m, a) in &p.changed {
            rep.push(
                Finding::new(
                    Code::MIGRATION_CHANGED,
                    Severity::Error,
                    format!("migrations/{} changed after it was applied", m.file),
                )
                .detail(format!("checksum now {}, applied {}", &m.checksum[..12], a.checksum.get(..12).unwrap_or("")))
                .fix("undo the change and write the new SQL as a new migration (giong db new), or giong db reset in development")
                .at(format!("migrations/{}", m.file), 1, 1),
            );
        }
        rep.line(format!(
            "Nothing applied: migrations/{} changed after it was applied.",
            m.file
        ));
        rep.data("applied", json!([]));
        return Ok(());
    }
    let mut applied_now = Vec::new();
    for m in &p.pending {
        let started = Instant::now();
        let out = r.run(&apply_script(r.engine(), m), None, false)?;
        if !out.ok {
            let wrapped = matches!(r.engine(), Engine::Postgres | Engine::Sqlite) && m.transaction;
            let line = migration_error_line(&out.stderr, &m.sql, wrapped);
            rep.push(
                Finding::new(
                    Code::MIGRATION_FAILED,
                    Severity::Error,
                    format!("migrations/{} failed", m.file),
                )
                .detail(first_lines(&out.stderr, 6))
                .fix(match r.engine() {
                    Engine::Postgres | Engine::Sqlite if m.transaction => {
                        "fix the SQL and run giong db migrate again: the file ran in a \
                         transaction, so nothing of it was applied"
                    }
                    _ => {
                        "fix the SQL; statements before the failing one were applied (MySQL \
                         commits DDL at once, and no-transaction files have no transaction), so \
                         make the file safe to run again (IF NOT EXISTS) or undo them by hand"
                    }
                })
                .at(format!("migrations/{}", m.file), line, 1),
            );
            break;
        }
        rep.line(format!(
            "applied migrations/{} ({} ms)",
            m.file,
            started.elapsed().as_millis()
        ));
        applied_now.push(json!(m.file));
    }
    if p.pending.is_empty() {
        rep.line(format!("Up to date: {} migrations applied.", p.applied));
    }
    rep.data("applied", json!(applied_now));
    rep.data("already_applied", json!(p.applied));
    Ok(())
}

/// The line of the migration file an error is reported at. The script that
/// applies it has a `BEGIN;` line before the file when it is `wrapped` in a
/// transaction, and a `;` line after it. psql names the line where the
/// failing statement ends: for a last statement without its `;` that is the
/// added line, and for one with an unclosed parenthesis (psql reads on to the
/// end of the script) a line past the file. Either is kept inside the file.
pub fn migration_error_line(stderr: &str, sql: &str, wrapped: bool) -> u32 {
    let last = u32::try_from(sql.trim_end().lines().count())
        .unwrap_or(u32::MAX)
        .max(1);
    error_line(stderr)
        .map(|l| l.saturating_sub(u32::from(wrapped)).clamp(1, last))
        .unwrap_or(1)
}

/// The script line an error names: psql's `psql:<stdin>:8: ERROR`,
/// mysql's `ERROR 1146 (42S02) at line 2:` or sqlite3's `Parse error near
/// line 4:`.
pub fn error_line(stderr: &str) -> Option<u32> {
    for l in stderr.lines() {
        if let Some(rest) = l.strip_prefix("psql:<stdin>:") {
            if let Some(n) = rest.split(':').next().and_then(|n| n.parse().ok()) {
                return Some(n);
            }
        }
        if let Some(i) = l
            .find(" at line ")
            .map(|i| i + 9)
            .or_else(|| l.find("near line ").map(|i| i + 10))
        {
            let n: String = l[i..].chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = n.parse() {
                return Some(n);
            }
        }
    }
    None
}

/// Sets `key=value` in `.env`, creating it from `.env.example` when missing,
/// and leaves every other line as it is.
pub fn set_env(root: &Path, key: &str, value: &str) -> Result<(), Failure> {
    let path = root.join(".env");
    // Neither file is read or written through a link: a cloned repository
    // can commit .env -> ~/.bashrc.
    let text = match state::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            state::read_to_string(&root.join(".env.example")).unwrap_or_default()
        }
        Err(e) => return Err(io_failure(&path, e)),
    };
    let new = set_env_text(&text, key, value);
    state::replace_private(&path, new.as_bytes()).map_err(|e| io_failure(&path, e))
}

pub fn set_env_text(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={value}");
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            let t = l.trim_start();
            let t = t.strip_prefix("export ").map(str::trim_start).unwrap_or(t);
            let is_key = t.split_once('=').is_some_and(|(k, _)| k.trim() == key);
            if is_key && !found {
                found = true;
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(line);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Adds `.env` and `.giong/` to `.gitignore` when they are not there: both
/// hold the database's password.
pub(crate) fn ignore_secrets(root: &Path) -> Result<Vec<String>, Failure> {
    let path = root.join(".gitignore");
    let text = match state::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_failure(&path, e)),
    };
    let has = |p: &str| {
        let bare = p.trim_end_matches('/');
        text.lines().any(|l| {
            [p, bare]
                .iter()
                .any(|p| l.trim() == *p || l.trim() == format!("/{p}"))
        })
    };
    let mut added = Vec::new();
    let mut new = text.clone();
    for p in [".env", ".giong/"] {
        if !has(p) {
            if !new.is_empty() && !new.ends_with('\n') {
                new.push('\n');
            }
            new.push_str(p);
            new.push('\n');
            added.push(p.to_string());
        }
    }
    if !added.is_empty() {
        state::replace(&path, new.as_bytes()).map_err(|e| io_failure(&path, e))?;
    }
    Ok(added)
}

/// Waits until the database answers a query over TCP inside its container
/// (the images' first start runs a server without TCP while they set up,
/// which a socket check would take for ready).
fn wait_ready(s: &Started, timeout: Duration) -> Result<Duration, Failure> {
    let start = Instant::now();
    let r = Runner::Container(s.clone());
    let probe = match s.engine {
        Engine::Redis => "PING\n",
        _ => "SELECT 1;\n",
    };
    let mut last = String::new();
    while start.elapsed() < timeout {
        if container_state(&s.runtime, &s.container).as_deref() != Some("running") {
            let logs = runtime_cmd(&s.runtime, &["logs", "--tail", "5", &s.container])
                .map(|o| {
                    first_lines(
                        &(String::from_utf8_lossy(&o.stderr).into_owned()
                            + &String::from_utf8_lossy(&o.stdout)),
                        5,
                    )
                })
                .unwrap_or_default();
            return Err(Failure::new(
                Code::DB_START_FAILED,
                format!("the {} container {} stopped while starting: {logs}", s.engine.name(), s.container),
                format!("read {} logs {}; a volume from another version of the engine is a common cause (giong db down --volumes starts fresh)", s.runtime, s.container),
            ));
        }
        if let Ok(out) = r.run(probe, None, false) {
            let ok = match s.engine {
                Engine::Redis => out.stdout.contains("PONG"),
                _ => out.ok && out.stdout.trim() == "1",
            };
            if ok && std::net::TcpStream::connect(("127.0.0.1", s.port)).is_ok() {
                return Ok(start.elapsed());
            }
            last = first_lines(&out.stderr, 2);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(Failure::new(
        Code::DB_NOT_READY,
        format!(
            "the {} container {} did not answer within {} s (last: {last})",
            s.engine.name(),
            s.container,
            timeout.as_secs()
        ),
        format!(
            "give it longer with --timeout, or read {} logs {}",
            s.runtime, s.container
        ),
    ))
}

fn up(args: &Args, root: &Path, project: &str, rep: &mut Report) -> Result<(), Failure> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    let engines = match args.value("engine") {
        Some(e) => vec![Engine::parse(e).ok_or_else(|| {
            Failure::new(
                Code::USAGE,
                format!("--engine {e:?} is not sqlite, postgres, mysql or redis"),
                "give --engine sqlite, postgres, mysql or redis",
            )
        })?],
        None => {
            let found = engines_in(&manifest);
            if found.is_empty() {
                return Err(Failure::new(
                    Code::USAGE,
                    "the project uses no giong database crate, so giong db up does not know \
                     which database to start",
                    "pass --engine postgres, mysql, redis or sqlite, or add giong-postgres, \
                     giong-mysql, giong-redis or rusqlite to Cargo.toml",
                ));
            }
            found
        }
    };
    let sql: Vec<&str> = engines
        .iter()
        .filter(|e| e.sql())
        .map(|e| e.name())
        .collect();
    if sql.len() > 1 {
        return Err(Failure::new(
            Code::USAGE,
            format!(
                "the project uses {}, and each would use DATABASE_URL",
                sql.join(" and ")
            ),
            format!("start one with --engine {}", sql.join(" or --engine ")),
        ));
    }
    let added = ignore_secrets(root)?;
    if !added.is_empty() {
        rep.line(format!(
            "added {} to .gitignore (they hold the password)",
            added.join(" and ")
        ));
    }
    let mut out = Vec::new();
    let (engines, files): (Vec<Engine>, Vec<Engine>) =
        engines.into_iter().partition(|e| e.has_server());
    if !files.is_empty() {
        out.push(sqlite_up(root, project, rep)?);
    }
    if engines.is_empty() {
        if args.value("port").is_some() || args.value("image").is_some() {
            return Err(Failure::new(
                Code::USAGE,
                "SQLite has no server, so --port and --image have nothing to set",
                "drop them: the database is a file the service opens",
            ));
        }
        rep.data("databases", json!(out));
        return Ok(());
    }
    if args.value("port").is_some() && engines.len() > 1 {
        return Err(Failure::new(
            Code::USAGE,
            "--port names one port, and the project uses several databases",
            "start them one at a time with --engine and --port",
        ));
    }
    let timeout = Duration::from_secs(args.number::<u64>("timeout")?.unwrap_or(120).max(1));
    if let Some(image) = args.value("image").filter(|i| !plain_image(i)) {
        return Err(Failure::new(
            Code::USAGE,
            format!("{image:?} is not an image name"),
            "give --image a plain reference: postgres:16-alpine, mysql:8.4, \
             ghcr.io/org/image:tag",
        ));
    }
    let runtime = container_runtime()?;
    let mut state = read_state(root);
    for engine in engines {
        let existing = state.iter().find(|s| s.engine == engine).cloned();
        let mut s = match existing {
            Some(s) if s.runtime == runtime => s,
            _ => {
                let port = match args.number::<u16>("port")? {
                    Some(p) => {
                        if !port_free(p) {
                            return Err(Failure::new(
                                Code::PORT_IN_USE,
                                format!("port {p} on 127.0.0.1 is in use"),
                                "pick another --port, or leave it out to get a free one",
                            ));
                        }
                        p
                    }
                    None if port_free(engine.default_port()) => engine.default_port(),
                    None => any_free_port(),
                };
                let base = container_base(project, root, engine);
                Started {
                    engine,
                    runtime: runtime.clone(),
                    container: base.clone(),
                    volume: format!("{base}-data"),
                    image: args
                        .value("image")
                        .unwrap_or(engine.default_image())
                        .to_string(),
                    port,
                    user: "giong".into(),
                    password: password(),
                    database: slug(project),
                }
            }
        };
        let status = container_state(&runtime, &s.container);
        // A container of this name that .giong/db.json does not list: its
        // password is not known here, and writing a new one into .env would
        // not match it.
        if status.is_some() && !read_state(root).iter().any(|x| x.engine == engine) {
            return Err(Failure::new(
                Code::DB_START_FAILED,
                format!(
                    "a container named {} exists, but .giong/db.json does not list it, so its \
                     password is not known",
                    s.container
                ),
                "giong db down removes it (--volumes also deletes its data) and giong db up \
                 starts a fresh one; to keep it, restore .giong/db.json from where it was",
            ));
        }
        let action = match status.as_deref() {
            Some("running") => "already running",
            Some(_) => {
                let o = runtime_cmd(&runtime, &["start", &s.container])?;
                if !o.status.success() {
                    return Err(start_failed(&s, &o));
                }
                "started again"
            }
            None => {
                // A new container: an image the project's files name is used
                // only when it is one of the engine's official images or the
                // one --image gives now, since .giong/db.json can come from
                // a cloned repository.
                if !official_image(engine, &s.image)
                    && args.value("image") != Some(s.image.as_str())
                {
                    let image = args.value("image").unwrap_or(engine.default_image());
                    rep.line(format!(
                        "{}: .giong/db.json names image {}, which is not an official {} image; \
                         starting {image} (--image {} keeps it)",
                        engine.name(),
                        s.image,
                        engine.name(),
                        s.image
                    ));
                    s.image = image.to_string();
                }
                // Data left by a container this project no longer has the
                // credentials of: the engine would keep the old password.
                let fresh = !read_state(root).iter().any(|x| x.engine == engine);
                if fresh
                    && runtime_cmd(&runtime, &["volume", "inspect", &s.volume])
                        .is_ok_and(|o| o.status.success())
                {
                    return Err(Failure::new(
                        Code::DB_START_FAILED,
                        format!(
                            "volume {} holds data from an earlier giong db up whose password is \
                             no longer known (.giong/db.json was removed)",
                            s.volume
                        ),
                        "giong db down --volumes deletes it and starts fresh; to keep the data, \
                         restore .giong/db.json from where it was",
                    ));
                }
                // A labelled volume, so that giong db down --volumes finds it
                // even after .giong/db.json is gone.
                let label = format!("run.giong.project={}", root.display());
                let _ = runtime_cmd(
                    &runtime,
                    &["volume", "create", "--label", &label, &s.volume],
                );
                // The port was free when chosen, but another giong db up (or
                // anything else) can take it before the container binds it:
                // pick another, unless the user named this one.
                let mut attempts = 0;
                loop {
                    let o = runtime_cmd(
                        &runtime,
                        &docker_run_args(&s, root)
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                    )?;
                    if o.status.success() {
                        break;
                    }
                    let err = String::from_utf8_lossy(&o.stderr);
                    let taken = err.contains("port is already allocated")
                        || err.contains("address already in use");
                    attempts += 1;
                    if !taken || args.value("port").is_some() || attempts >= 4 {
                        let _ = runtime_cmd(&runtime, &["rm", "-f", &s.container]);
                        return Err(start_failed(&s, &o));
                    }
                    let _ = runtime_cmd(&runtime, &["rm", "-f", &s.container]);
                    s.port = any_free_port();
                }
                "created"
            }
        };
        // Recorded before waiting, so that giong db down can remove a
        // container that never became ready.
        state.retain(|x| x.engine != engine);
        state.push(s.clone());
        write_state(root, &state)?;
        // For giong dev status and giong dev stop.
        let entry = crate::procs::Entry {
            kind: "db".into(),
            name: s.container.clone(),
            engine: s.runtime.clone(),
            started_by: "giong db up".into(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        };
        crate::procs::add(root, entry).map_err(|e| io_failure(&procs_path(root), e))?;
        let took = wait_ready(&s, timeout)?;
        set_env(root, engine.env_key(), &s.url())?;
        rep.line(format!(
            "{}: {} ({action}, ready in {:.1} s) at 127.0.0.1:{}; {}={} in .env",
            engine.name(),
            s.container,
            took.as_secs_f64(),
            s.port,
            engine.env_key(),
            redact(&s.url())
        ));
        out.push(json!({
            "engine": engine.name(), "container": s.container, "runtime": s.runtime,
            "image": s.image, "port": s.port, "action": action,
            "env_key": engine.env_key(), "url": redact(&s.url()),
            "ready_ms": took.as_millis() as u64,
        }));
    }
    rep.data("databases", json!(out));
    Ok(())
}

/// SQLite: nothing to start. Makes sure `.env` names the file (a `sqlite:`
/// URL there is kept) and that its directory exists.
fn sqlite_up(root: &Path, project: &str, rep: &mut Report) -> Result<Value, Failure> {
    let have = state::read_to_string(&root.join(".env"))
        .ok()
        .and_then(|t| {
            dotenv::parse(&t)
                .0
                .into_iter()
                .find(|v| v.key == "DATABASE_URL")
                .map(|v| v.value)
        })
        .filter(|u| sqlite_path(u).is_some());
    let url = match have {
        Some(url) => url,
        None => {
            let url = format!("sqlite:data/{}.db", project.replace('-', "_"));
            set_env(root, "DATABASE_URL", &url)?;
            url
        }
    };
    let file = sqlite_path(&url).unwrap_or_default();
    let path = root.join(&file);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_failure(dir, e))?;
    }
    let made = path.is_file();
    rep.line(format!(
        "sqlite: {} ({}; a file, nothing to start); DATABASE_URL={url} in .env",
        file.display(),
        if made {
            "there already"
        } else {
            "made by the service at its first start"
        }
    ));
    Ok(json!({
        "engine": "sqlite", "file": file.display().to_string(), "exists": made,
        "action": "none", "env_key": "DATABASE_URL", "url": url,
    }))
}

fn docker_run_args(s: &Started, root: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        s.container.clone(),
        "--label".into(),
        format!("run.giong.project={}", root.display()),
        "-p".into(),
        format!("127.0.0.1:{}:{}", s.port, s.engine.default_port()),
        "-v".into(),
        format!("{}:{}", s.volume, s.engine.data_dir()),
    ];
    let env = |a: &mut Vec<String>, k: &str, v: &str| {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    };
    match s.engine {
        Engine::Postgres => {
            env(&mut a, "POSTGRES_USER", &s.user);
            env(&mut a, "POSTGRES_PASSWORD", &s.password);
            env(&mut a, "POSTGRES_DB", &s.database);
        }
        Engine::Mysql => {
            env(&mut a, "MYSQL_USER", &s.user);
            env(&mut a, "MYSQL_PASSWORD", &s.password);
            env(&mut a, "MYSQL_ROOT_PASSWORD", &s.password);
            env(&mut a, "MYSQL_DATABASE", &s.database);
        }
        Engine::Redis | Engine::Sqlite => {}
    }
    a.push(s.image.clone());
    a
}

fn start_failed(s: &Started, o: &std::process::Output) -> Failure {
    let err = first_lines(&String::from_utf8_lossy(&o.stderr), 3);
    let fix = if err.contains("port is already allocated") || err.contains("address already in use")
    {
        "another program took the port: giong db down, then giong db up picks a free one"
            .to_string()
    } else if err.contains("pull") || err.contains("manifest") || err.contains("not found") {
        format!(
            "check the image name {} and the network (or pull it first)",
            s.image
        )
    } else {
        format!("read the message; {} ps -a shows the container", s.runtime)
    };
    Failure::new(
        Code::DB_START_FAILED,
        format!(
            "{} could not start {} ({}): {err}",
            s.runtime, s.container, s.image
        ),
        fix,
    )
}

fn down(args: &Args, root: &Path, rep: &mut Report) -> Result<(), Failure> {
    let state = read_state(root);
    let label = format!("label=run.giong.project={}", root.display());
    // What the state file lists, plus anything else labelled for this
    // project (a state file deleted by hand, an earlier down without
    // --volumes).
    let runtime = match state.first() {
        Some(s) => Some(s.runtime.clone()),
        None => container_runtime().ok(),
    };
    let mut containers: Vec<String> = state.iter().map(|s| s.container.clone()).collect();
    let mut volumes: Vec<String> = state.iter().map(|s| s.volume.clone()).collect();
    if let Some(rt) = &runtime {
        let list = |args: &[&str]| -> Vec<String> {
            runtime_cmd(rt, args)
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        for c in list(&["ps", "-a", "--format", "{{.Names}}", "--filter", &label]) {
            if !containers.contains(&c) {
                containers.push(c);
            }
        }
        if args.has("volumes") {
            for v in list(&["volume", "ls", "-q", "--filter", &label]) {
                if !volumes.contains(&v) {
                    volumes.push(v);
                }
            }
        }
    }
    let Some(rt) =
        runtime.filter(|_| !containers.is_empty() || (args.has("volumes") && !volumes.is_empty()))
    else {
        // Containers removed by hand: nothing left for giong dev to stop.
        for e in crate::procs::list(root).iter().filter(|e| e.kind == "db") {
            crate::procs::remove(root, "db", &e.name)
                .map_err(|err| io_failure(&procs_path(root), err))?;
        }
        rep.line("giong db up started nothing in this project.");
        rep.data("removed", json!([]));
        rep.data("volumes_removed", json!([]));
        return Ok(());
    };
    let mut removed = Vec::new();
    for c in &containers {
        let o = runtime_cmd(&rt, &["rm", "-f", c])?;
        if !o.status.success() && container_state(&rt, c).is_some() {
            return Err(Failure::new(
                Code::DB_START_FAILED,
                format!(
                    "{rt} could not remove {c}: {}",
                    first_lines(&String::from_utf8_lossy(&o.stderr), 2)
                ),
                format!("remove it by hand: {rt} rm -f {c}"),
            ));
        }
        crate::procs::remove(root, "db", c).map_err(|e| io_failure(&procs_path(root), e))?;
        rep.line(format!("removed {c}"));
        removed.push(json!(c));
    }
    let mut volumes_removed = Vec::new();
    if args.has("volumes") {
        for v in &volumes {
            let o = runtime_cmd(&rt, &["volume", "rm", "-f", v])?;
            if o.status.success() {
                rep.line(format!("deleted the data in volume {v}"));
                volumes_removed.push(json!(v));
            }
        }
    } else if !volumes.is_empty() {
        rep.line(format!(
            "data kept in {} (giong db down --volumes deletes it)",
            volumes.join(", ")
        ));
    }
    // Without --volumes the data stays, and so must its credentials: the
    // engines set the password only when they create the data, so the next
    // giong db up reuses this entry.
    if args.has("volumes") {
        write_state(root, &[])?;
    }
    rep.data("removed", json!(removed));
    rep.data("volumes_removed", json!(volumes_removed));
    Ok(())
}

fn status(root: &Path, rep: &mut Report) -> Result<(), Failure> {
    let state = read_state(root);
    let mut list = Vec::new();
    for s in &state {
        let st = container_state(&s.runtime, &s.container).unwrap_or_else(|| "removed".into());
        rep.line(format!(
            "{}: container {} {st}, 127.0.0.1:{}, {}",
            s.engine.name(),
            s.container,
            s.port,
            redact(&s.url())
        ));
        if st != "running" {
            rep.push(
                Finding::new(
                    Code::DB_NOT_READY,
                    Severity::Warning,
                    format!("the {} container is {st}", s.engine.name()),
                )
                .fix("giong db up starts it again"),
            );
        }
        list.push(
            json!({ "engine": s.engine.name(), "container": s.container, "state": st,
                          "port": s.port, "url": redact(&s.url()) }),
        );
    }
    rep.data("databases", json!(list));
    let files = read_migrations(&root.join("migrations"))?;
    rep.data(
        "migrations",
        json!(files.iter().map(|m| &m.file).collect::<Vec<_>>()),
    );
    match runner(root, None) {
        Ok(r) if r.engine().sql() => {
            if r.engine() == Engine::Sqlite {
                let path = sqlite_file(&r);
                match std::fs::metadata(&path) {
                    Ok(m) => rep.line(format!(
                        "sqlite: {} ({} KiB)",
                        path.display(),
                        m.len().div_ceil(1024)
                    )),
                    Err(_) => {
                        rep.line(format!(
                            "sqlite: {} is not made yet (the service makes it at its first \
                             start, giong db migrate too)",
                            path.display()
                        ));
                        rep.data(
                            "pending",
                            json!(files.iter().map(|m| &m.file).collect::<Vec<_>>()),
                        );
                        return Ok(());
                    }
                }
            }
            let done = match applied(&r) {
                Ok(done) => done,
                Err(f) if r.engine() == Engine::Sqlite && f.code == Code::DB_CLIENT_MISSING => {
                    rep.line(format!("Migrations not checked: {}", f.what));
                    rep.push(
                        Finding::new(f.code, Severity::Warning, f.what.clone()).fix(f.fix.clone()),
                    );
                    return Ok(());
                }
                Err(f) => return Err(f),
            };
            let p = plan(&files, &done);
            rep.line(format!(
                "migrations on {}: {} applied, {} pending, {} changed, {} unknown",
                r.describe(),
                p.applied,
                p.pending.len(),
                p.changed.len(),
                p.unknown.len()
            ));
            for m in &p.pending {
                rep.line(format!("  pending  migrations/{}", m.file));
            }
            for (m, _) in &p.changed {
                rep.push(
                    Finding::new(
                        Code::MIGRATION_CHANGED,
                        Severity::Error,
                        format!("migrations/{} changed after it was applied", m.file),
                    )
                    .fix("undo the change and write a new migration (giong db new)")
                    .at(format!("migrations/{}", m.file), 1, 1),
                );
            }
            rep.data(
                "pending",
                json!(p.pending.iter().map(|m| &m.file).collect::<Vec<_>>()),
            );
            rep.data("applied_count", json!(p.applied));
        }
        Ok(_) => {}
        Err(f) if state.is_empty() || f.code == Code::DB_NOT_READY => {
            rep.line(format!("Migrations not checked: {}", f.what));
        }
        Err(f) => return Err(f),
    }
    Ok(())
}

fn new_migration(args: &Args, root: &Path, rep: &mut Report) -> Result<(), Failure> {
    let Some(raw) = args.words.get(1) else {
        return Err(Failure::new(
            Code::USAGE,
            "giong db new needs a name",
            "write giong db new create_users",
        ));
    };
    let name = slug(raw);
    let name = name
        .strip_prefix("app_")
        .filter(|_| !raw.starts_with("app"))
        .unwrap_or(&name)
        .to_string();
    if name.is_empty() {
        return Err(Failure::new(
            Code::MIGRATION_NAME,
            format!("{raw:?} has no letters or digits to name a migration"),
            "write giong db new create_users",
        ));
    }
    let dir = root.join("migrations");
    let files = read_migrations(&dir)?;
    let next = files.last().map_or(1, |m| m.version + 1);
    let file = format!("{next:04}_{name}.sql");
    // A linked migrations folder would put the file somewhere else.
    state::check_dir(&dir).map_err(|e| io_failure(&dir, e))?;
    std::fs::create_dir_all(&dir).map_err(|e| io_failure(&dir, e))?;
    let path = dir.join(&file);
    let text = format!(
        "-- {name}\n-- Plain SQL, applied once by giong db migrate, in order. On PostgreSQL the \
         whole file runs in one\n-- transaction; make the first line `-- giong:no-transaction` \
         for statements that cannot\n-- (CREATE INDEX CONCURRENTLY). Never edit it once \
         applied: write a new migration.\n\n"
    );
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io_failure(&path, e))?;
    f.write_all(text.as_bytes())
        .map_err(|e| io_failure(&path, e))?;
    rep.line(format!("wrote migrations/{file}"));
    rep.data("file", json!(format!("migrations/{file}")));
    rep.data("version", json!(next));
    Ok(())
}

fn reset(args: &Args, root: &Path, rep: &mut Report) -> Result<(), Failure> {
    if !args.has("yes") {
        return Err(Failure::new(
            Code::USAGE,
            "giong db reset deletes every row of the development database",
            "run giong db reset --yes to confirm",
        ));
    }
    let state = read_state(root);
    // A SQLite file is the project's own when it is inside the project.
    let file = runner(root, Some(Engine::Sqlite)).ok();
    if state.is_empty() && file.is_none() {
        return Err(Failure::new(
            Code::DB_NOT_MANAGED,
            "giong db reset works only on a database giong db up started, and there is none here",
            "start one with giong db up; giong never drops a database given by DATABASE_URL",
        ));
    }
    if let Some(r) = &file {
        reset_sqlite(r, root, rep)?;
    }
    for s in &state {
        let r = Runner::Container(s.clone());
        let (sql, db, admin) = match s.engine {
            Engine::Postgres => (
                format!(
                    "DROP DATABASE IF EXISTS \"{0}\" WITH (FORCE);\nCREATE DATABASE \"{0}\" OWNER \"{1}\";\n",
                    s.database, s.user
                ),
                Some("postgres"),
                false,
            ),
            Engine::Mysql => (
                format!(
                    "DROP DATABASE IF EXISTS `{0}`;\nCREATE DATABASE `{0}`;\nGRANT ALL ON `{0}`.* TO '{1}'@'%';\n",
                    s.database, s.user
                ),
                Some(""),
                true,
            ),
            Engine::Redis => ("FLUSHALL\n".to_string(), None, false),
            // Never started, so never in the state file.
            Engine::Sqlite => continue,
        };
        let out = r.run(&sql, db, admin)?;
        if !out.ok || out.stderr.contains("ERR") {
            return Err(sql_failure(&r, "resetting", &out));
        }
        rep.line(format!("{}: emptied {}", s.engine.name(), s.container));
    }
    if let Some(sql) = state.iter().find(|s| s.engine.sql()) {
        migrate(&Runner::Container(sql.clone()), root, rep)?;
    } else if let Some(r) = &file {
        migrate(r, root, rep)?;
    }
    Ok(())
}

/// The file a SQLite runner works on (absolute: the runner joined it to the
/// project).
fn sqlite_file(r: &Runner) -> PathBuf {
    match r {
        Runner::Local { url, .. } => sqlite_path(url).unwrap_or_default(),
        Runner::Container(_) => PathBuf::new(),
    }
}

/// Makes the directory of a SQLite file, which `sqlite3` does not.
fn sqlite_dir(r: &Runner) -> Result<(), Failure> {
    if r.engine() != Engine::Sqlite {
        return Ok(());
    }
    match sqlite_file(r).parent() {
        Some(dir) => std::fs::create_dir_all(dir).map_err(|e| io_failure(dir, e)),
        None => Ok(()),
    }
}

/// Drops every table, view and trigger of a SQLite file inside the project.
/// The file stays, so a service that has it open keeps working on the
/// emptied file (deleting it would leave the service on the deleted one).
fn reset_sqlite(r: &Runner, root: &Path, rep: &mut Report) -> Result<(), Failure> {
    let path = sqlite_file(r);
    let Ok(file) = std::fs::canonicalize(&path) else {
        rep.line(format!("sqlite: {} is not made yet", path.display()));
        return sqlite_dir(r);
    };
    let inside = std::fs::canonicalize(root).is_ok_and(|root| file.starts_with(root));
    if !inside {
        return Err(Failure::new(
            Code::DB_NOT_MANAGED,
            format!(
                "giong db reset empties only a SQLite file inside the project, and \
                 DATABASE_URL names {}",
                file.display()
            ),
            "point DATABASE_URL at a file in the project for development; giong never empties \
             a database file kept elsewhere",
        ));
    }
    // Views and triggers first, then virtual tables (which drop their own
    // shadow tables), then the rest.
    let list = "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite\\_%' \
                ESCAPE '\\' AND type IN ('table', 'view', 'trigger') ORDER BY CASE \
                WHEN type = 'view' THEN 0 WHEN type = 'trigger' THEN 1 \
                WHEN sql LIKE 'CREATE VIRTUAL%' THEN 2 ELSE 3 END;\n";
    let out = r.run(list, None, false)?;
    if !out.ok {
        return Err(sql_failure(r, "listing the tables", &out));
    }
    let mut script = String::from("PRAGMA foreign_keys = OFF;\nBEGIN;\n");
    let mut count = 0;
    for (kind, name) in out.stdout.lines().filter_map(|l| l.split_once('\t')) {
        let kind = match kind {
            "view" => "VIEW",
            "trigger" => "TRIGGER",
            _ => "TABLE",
        };
        script.push_str(&format!(
            "DROP {kind} IF EXISTS \"{}\";\n",
            name.replace('"', "\"\"")
        ));
        count += 1;
    }
    script.push_str("COMMIT;\nVACUUM;\n");
    let out = r.run(&script, None, false)?;
    if !out.ok {
        return Err(sql_failure(r, "resetting", &out));
    }
    rep.line(format!(
        "sqlite: emptied {} ({count} tables, views and triggers dropped)",
        path.display()
    ));
    Ok(())
}

fn run(args: &Args) -> Result<Report, Failure> {
    let sub = args.words.first().map(String::as_str).unwrap_or("");
    let (root, project) = project()?;
    let mut rep = Report::new("db");
    rep.data("action", json!(sub));
    for (engine, why) in rejected_state(&root) {
        rep.push(
            Finding::new(
                Code::DB_NOT_MANAGED,
                Severity::Warning,
                match engine {
                    Some(e) => {
                        format!("the {} entry of .giong/db.json is ignored: {why}", e.name())
                    }
                    None => format!(".giong/db.json is ignored: {why}"),
                },
            )
            .detail(
                "giong db up never writes such an entry; a cloned repository can commit one to \
                 make giong run another program, mount the host or drop another database",
            )
            .fix(
                "if you did not write it, delete .giong/db.json (and keep .giong/ out of git); \
                 giong db up then starts a fresh database",
            ),
        );
    }
    match sub {
        "up" => up(args, &root, &project, &mut rep)?,
        "down" => down(args, &root, &mut rep)?,
        "status" => status(&root, &mut rep)?,
        "new" => new_migration(args, &root, &mut rep)?,
        "migrate" => {
            let r = runner(&root, None)?;
            if !r.engine().sql() {
                return Err(bad_url("redis"));
            }
            sqlite_dir(&r)?;
            migrate(&r, &root, &mut rep)?;
        }
        "reset" => reset(args, &root, &mut rep)?,
        "" => {
            return Err(Failure::new(
                Code::USAGE,
                "giong db needs an action",
                "write giong db up, down, status, new <name>, migrate or reset",
            ))
        }
        other => {
            return Err(Failure::new(
                Code::USAGE,
                format!("giong db has no action {other:?}"),
                "write giong db up, down, status, new <name>, migrate or reset",
            ))
        }
    }
    if sub != "new" && args.words.len() > 1 {
        return Err(Failure::new(
            Code::USAGE,
            format!("giong db {sub} takes no name"),
            "only giong db new takes a name",
        ));
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that prints as it reads, fed more than the pipes hold: the
    /// script goes in while the output comes out, instead of both waiting.
    #[cfg(unix)]
    #[test]
    fn a_long_script_with_output_does_not_fill_both_pipes() {
        let script = "SELECT 1;\n".repeat(200_000);
        let (tx, rx) = std::sync::mpsc::channel();
        let input = script.clone();
        std::thread::spawn(move || {
            let _ = tx.send(feed(&mut Proc::new("cat"), &input));
        });
        let out = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the client and giong both wait on a full pipe")
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), script.len());
    }

    /// A cloned repository can commit `.env` or the state file's temporary
    /// name as a link: giong db never writes through one.
    #[cfg(unix)]
    #[test]
    fn links_in_the_project_are_not_written_through() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("giongdblinks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("proj");
        std::fs::create_dir_all(root.join(".giong")).unwrap();
        let target = base.join("bashrc");
        std::fs::write(&target, "mine").unwrap();

        symlink(&target, root.join(".env")).unwrap();
        assert!(set_env(&root, "DATABASE_URL", "postgres://h/d").is_err());
        assert!(ignore_secrets(&root).is_ok());
        std::fs::remove_file(root.join(".env")).unwrap();

        std::fs::remove_file(root.join(".gitignore")).unwrap();
        symlink(&target, root.join(".gitignore")).unwrap();
        assert!(ignore_secrets(&root).is_err());
        std::fs::remove_file(root.join(".gitignore")).unwrap();

        let tmp = format!("db.json.{}", std::process::id());
        symlink(&target, root.join(".giong").join(&tmp)).unwrap();
        let s = Started {
            engine: Engine::Postgres,
            runtime: "docker".into(),
            container: "c".into(),
            volume: "v".into(),
            image: "postgres:17".into(),
            port: 5432,
            user: "u".into(),
            password: "p".into(),
            database: "d".into(),
        };
        write_state(&root, &[s]).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
        assert!(!std::fs::symlink_metadata(state_path(&root))
            .unwrap()
            .file_type()
            .is_symlink());

        // A linked .giong folder is refused outright.
        std::fs::remove_dir_all(root.join(".giong")).unwrap();
        symlink(&base, root.join(".giong")).unwrap();
        assert!(write_state(&root, &[]).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn migration_names() {
        assert_eq!(
            parse_migration_name("0001_init.sql"),
            Some((1, "init".into()))
        );
        assert_eq!(
            parse_migration_name("20261122_add_users_2.sql"),
            Some((20261122, "add_users_2".into()))
        );
        for bad in [
            "001_x.sql",
            "0001.sql",
            "0001_.sql",
            "0001_Users.sql",
            "0001-x.sql",
            "x_0001.sql",
            "0001_a b.sql",
            "0001_x.SQL",
        ] {
            assert_eq!(parse_migration_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn state_entries_a_repository_could_plant_are_rejected() {
        let root = std::env::temp_dir().join(format!("giong-db-hostile-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let base = container_base("shop", &root, Engine::Postgres);
        let good = Started {
            engine: Engine::Postgres,
            runtime: "docker".into(),
            container: base.clone(),
            volume: format!("{base}-data"),
            image: "postgres:16-alpine".into(),
            port: 5432,
            user: "giong".into(),
            password: "0123abcd".into(),
            database: "shop".into(),
        };
        assert_eq!(good.problem("shop", &root), None);
        let bad = |f: &dyn Fn(&mut Started)| {
            let mut s = good.clone();
            f(&mut s);
            s.problem("shop", &root)
        };
        for runtime in ["./x.sh", "/usr/bin/docker", "sh", "docker;id", ""] {
            assert!(bad(&|s| s.runtime = runtime.into()).is_some(), "{runtime}");
        }
        for name in ["giong-other-postgres-abcdef", "-v", ""] {
            assert!(bad(&|s| s.container = name.into()).is_some(), "{name}");
        }
        for volume in ["/", "/home/me", "giong-other-postgres-abcdef-data", "-v"] {
            assert!(bad(&|s| s.volume = volume.into()).is_some(), "{volume}");
        }
        for image in ["-v", "--privileged", "x y", "evil;rm", ""] {
            assert!(bad(&|s| s.image = image.into()).is_some(), "{image}");
        }
        for name in ["shop; DROP DATABASE x", "a'b", "Shop", "a-b"] {
            assert!(bad(&|s| s.database = name.into()).is_some(), "{name}");
            assert!(bad(&|s| s.user = name.into()).is_some(), "{name}");
        }
        assert!(bad(&|s| s.password = "a\nB=1".into()).is_some());
        // Another checkout's name is another container.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(good.problem("shop", &other).is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_official_images_come_from_the_state_file() {
        for (e, image) in [
            (Engine::Postgres, "postgres:16-alpine"),
            (Engine::Postgres, "postgres"),
            (Engine::Postgres, "docker.io/library/postgres:17"),
            (Engine::Mysql, "mariadb:11.4"),
            (Engine::Redis, "valkey/valkey:8"),
        ] {
            assert!(official_image(e, image), "{image}");
        }
        for (e, image) in [
            (Engine::Postgres, "evil/postgres:16"),
            (Engine::Postgres, "ghcr.io/x/postgres:16"),
            (Engine::Postgres, "mysql:8.4"),
            (Engine::Redis, "redis:7;id"),
            (Engine::Mysql, "mysql@sha256:abc"),
        ] {
            assert!(!official_image(e, image), "{image}");
        }
        assert!(plain_image("ghcr.io/org/pg@sha256:0a1b"));
        assert!(!plain_image("-v"));
    }

    #[test]
    fn each_checkout_gets_its_own_container() {
        let base = std::env::temp_dir().join(format!("giong-db-names-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let name = container_base("Rest API", &a, Engine::Postgres);
        assert!(name.starts_with("giong-rest-api-postgres-"), "{name}");
        assert_eq!(name.len(), "giong-rest-api-postgres-".len() + 6);
        assert_eq!(name, container_base("Rest API", &a, Engine::Postgres));
        assert_ne!(name, container_base("Rest API", &b, Engine::Postgres));
        assert_ne!(name, container_base("Rest API", &a, Engine::Redis));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn slugs_are_safe_names() {
        assert_eq!(slug("rest-api"), "rest_api");
        assert_eq!(slug("My App!"), "my_app");
        assert_eq!(slug("9lives"), "app_9lives");
        assert_eq!(slug("--"), "app");
        assert_eq!(slug(&"x".repeat(80)).len(), 48);
        assert_eq!(slug("ünï"), "n");
        assert_eq!(slug("ü"), "app");
    }

    #[test]
    fn engines_come_from_dependencies() {
        let m = "[dependencies]\ngiong-postgres = \"1.5\"\n# giong-mysql = \"1\"\ngiong-redis = { version = \"1.5\" }\n";
        assert_eq!(engines_in(m), [Engine::Postgres, Engine::Redis]);
        assert_eq!(
            engines_in("[dependencies.giong-mysql]\nversion = \"1\""),
            [Engine::Mysql]
        );
        assert!(engines_in("[dependencies]\ngiong-postgres-extra = \"1\"").is_empty());
    }

    #[test]
    fn sqlite_files_scripts_and_errors() {
        let m = "[dependencies]\nrusqlite = { version = \"0.43\", features = [\"bundled\"] }\n\
                 giong-redis = \"1.5\"\n";
        assert_eq!(engines_in(m), [Engine::Sqlite, Engine::Redis]);
        assert!(!Engine::Sqlite.has_server() && Engine::Sqlite.sql());
        assert_eq!(Engine::parse("SQLite3"), Some(Engine::Sqlite));
        assert_eq!(Engine::Sqlite.env_key(), "DATABASE_URL");

        // The forms the template's service reads, and nothing else.
        assert_eq!(
            sqlite_path("sqlite:data/app.db"),
            Some("data/app.db".into())
        );
        assert_eq!(
            sqlite_path("sqlite://data/app.db"),
            Some("data/app.db".into())
        );
        assert_eq!(
            sqlite_path("sqlite:///var/lib/app.db"),
            Some("/var/lib/app.db".into())
        );
        assert_eq!(sqlite_path("sqlite::memory:"), None);
        assert_eq!(sqlite_path("sqlite:"), None);
        assert_eq!(sqlite_path("postgres://h/db"), None);

        // A migration runs in one transaction, as on PostgreSQL.
        let m = Migration {
            version: 2,
            name: "add_age".into(),
            file: "0002_add_age.sql".into(),
            sql: "ALTER TABLE users ADD COLUMN age INTEGER;\n".into(),
            checksum: "ab".repeat(32),
            transaction: true,
        };
        let script = apply_script(Engine::Sqlite, &m);
        assert!(script.starts_with("BEGIN;\nALTER TABLE"), "{script}");
        assert!(script.ends_with("COMMIT;\n"), "{script}");
        assert_eq!(
            error_line("Parse error near line 9: no such table: nope"),
            Some(9)
        );
        assert_eq!(
            error_line("Runtime error near line 3: UNIQUE constraint failed: users.email (19)"),
            Some(3)
        );
    }

    /// A project on rusqlite alone has a database before `.env` exists:
    /// the file its service opens without `DATABASE_URL`.
    #[test]
    fn a_sqlite_project_needs_no_env() {
        let root = std::env::temp_dir().join(format!("giongdbsqlite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-shop\"\n\n[dependencies]\nrusqlite = \"0.43\"\n",
        )
        .unwrap();
        // DATABASE_URL from the environment wins, as for every engine.
        let from_env = std::env::var_os("DATABASE_URL").is_some();
        if !from_env {
            let r = runner(&root, None).unwrap();
            assert_eq!(r.engine(), Engine::Sqlite);
            assert_eq!(sqlite_file(&r), root.join("data/my_shop.db"));
        }
        // With a server database too, DATABASE_URL is that one's.
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-shop\"\n\n[dependencies]\nrusqlite = \"0.43\"\n\
             giong-postgres = \"1.5\"\n",
        )
        .unwrap();
        if !from_env {
            assert_eq!(runner(&root, None).unwrap_err().code, Code::DB_URL);
        }
        // .gitignore's `/.giong` counts as `.giong/`.
        std::fs::write(root.join(".gitignore"), "/target\n/.giong\n.env\n").unwrap();
        assert!(ignore_secrets(&root).unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn urls_parse_and_redact() {
        let (e, p) = parse_url("mysql://app:p%40ss@db.local:3307/shop?ssl=1").unwrap();
        assert_eq!(e, Engine::Mysql);
        assert_eq!(
            p,
            UrlParts {
                user: "app".into(),
                password: "p@ss".into(),
                host: "db.local".into(),
                port: Some(3307),
                database: "shop".into()
            }
        );
        let (e, p) = parse_url("postgres://[::1]:5433/x").unwrap();
        assert_eq!(
            (e, p.host.as_str(), p.port),
            (Engine::Postgres, "::1", Some(5433))
        );
        assert_eq!(parse_url("redis://127.0.0.1").unwrap().0, Engine::Redis);
        assert!(parse_url("http://x").is_none());
        assert!(parse_url("mysql://h:notaport/db").is_none());
        assert_eq!(
            redact("postgres://u:secret@h:1/d"),
            "postgres://u:***@h:1/d"
        );
        assert_eq!(redact("redis://h:1"), "redis://h:1");
        assert_eq!(
            replace_database("postgres://u:p@h:1/app?sslmode=disable", "postgres"),
            "postgres://u:p@h:1/postgres?sslmode=disable"
        );
        assert_eq!(replace_database("postgres://h", "x"), "postgres://h/x");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
        // A character cut by the two bytes after '%' (it panicked), and a
        // sign, which from_str_radix takes, are left as written.
        assert_eq!(percent_decode("%€x"), "%€x");
        assert_eq!(percent_decode("p%+5"), "p%+5");
        assert_eq!(parse_url("postgres://u:p%€@h/d").unwrap().1.password, "p%€");
    }

    #[test]
    fn passwords_leave_the_url_a_client_is_given() {
        assert_eq!(
            without_password("postgres://app:p%40ss@db:5432/shop?sslmode=require"),
            (
                "postgres://app@db:5432/shop?sslmode=require".to_string(),
                Some("p@ss".to_string())
            )
        );
        // An `@` after the host is not the end of a password.
        assert_eq!(
            without_password("postgres://u:pw@h/d?application_name=a@b"),
            (
                "postgres://u@h/d?application_name=a@b".to_string(),
                Some("pw".to_string())
            )
        );
        assert_eq!(
            without_password("postgresql://:pw@[::1]:5433"),
            (
                "postgresql://[::1]:5433".to_string(),
                Some("pw".to_string())
            )
        );
        assert_eq!(
            without_password("postgres://u@h/d"),
            ("postgres://u@h/d".to_string(), None)
        );
        assert_eq!(
            without_password("postgres://h/d?x=a:b@c"),
            ("postgres://h/d?x=a:b@c".to_string(), None)
        );
        assert_eq!(
            without_credentials("rediss://default:s%3Acret@cache:6380/2"),
            (
                "rediss://cache:6380/2".to_string(),
                Some(("default".to_string(), "s:cret".to_string()))
            )
        );
        assert_eq!(
            without_credentials("redis://:pw@h:6379"),
            (
                "redis://h:6379".to_string(),
                Some((String::new(), "pw".to_string()))
            )
        );
        assert_eq!(
            without_credentials("redis://h:6379"),
            ("redis://h:6379".to_string(), None)
        );
        // The older form, a password with no user and no `:`.
        assert_eq!(
            without_credentials("redis://s%40cret@h:6379/1"),
            (
                "redis://h:6379/1".to_string(),
                Some((String::new(), "s@cret".to_string()))
            )
        );
        // libpq's password in the query, which wins over the one before
        // the host; the other pairs stay.
        assert_eq!(
            without_password("postgres://u@h/d?sslmode=require&password=p%26w"),
            (
                "postgres://u@h/d?sslmode=require".to_string(),
                Some("p&w".to_string())
            )
        );
        assert_eq!(
            without_password("postgres://u:old@h/d?password=new&application_name=x"),
            (
                "postgres://u@h/d?application_name=x".to_string(),
                Some("new".to_string())
            )
        );
        assert_eq!(
            without_password("postgres://h/d?password=pw"),
            ("postgres://h/d".to_string(), Some("pw".to_string()))
        );
        assert_eq!(
            without_password("postgres://h/d?passwordx=1"),
            ("postgres://h/d?passwordx=1".to_string(), None)
        );
    }

    #[test]
    fn env_lines_are_set_without_touching_others() {
        assert_eq!(set_env_text("", "A", "1"), "A=1\n");
        assert_eq!(
            set_env_text("# c\nA=old\nB=2\nexport A=dup", "A", "new"),
            "# c\nA=new\nB=2\nexport A=dup\n"
        );
        assert_eq!(set_env_text("B=2", "A", "1"), "B=2\nA=1\n");
        assert_eq!(set_env_text("AB=2\n", "A", "1"), "AB=2\nA=1\n");
    }

    fn m(v: u64, sql: &str) -> Migration {
        Migration {
            version: v,
            name: format!("m{v}"),
            file: format!("{v:04}_m{v}.sql"),
            sql: sql.into(),
            checksum: sha256::hex(sql.as_bytes()),
            transaction: true,
        }
    }

    #[test]
    fn plans_find_pending_changed_and_unknown() {
        let files = [m(1, "a"), m(2, "b"), m(3, "c")];
        let done = [
            Applied {
                version: 1,
                name: "m1".into(),
                checksum: sha256::hex(b"a"),
            },
            Applied {
                version: 2,
                name: "m2".into(),
                checksum: sha256::hex(b"B"),
            },
            Applied {
                version: 9,
                name: "m9".into(),
                checksum: "x".into(),
            },
        ];
        let p = plan(&files, &done);
        assert_eq!(p.applied, 1);
        assert_eq!(p.pending.iter().map(|x| x.version).collect::<Vec<_>>(), [3]);
        assert_eq!(p.changed[0].0.version, 2);
        assert_eq!(p.unknown[0].version, 9);
    }

    #[test]
    fn scripts_wrap_postgres_in_a_transaction_only() {
        let s = apply_script(Engine::Postgres, &m(4, "CREATE TABLE t (x int) -- end"));
        assert!(
            s.starts_with("BEGIN;\nCREATE TABLE t (x int) -- end\n;\nINSERT INTO giong_migrations"),
            "{s}"
        );
        assert!(s.ends_with("COMMIT;\n"));
        let mut nt = m(
            5,
            "-- giong:no-transaction\nCREATE INDEX CONCURRENTLY i ON t (x);",
        );
        nt.transaction = false;
        assert!(!apply_script(Engine::Postgres, &nt).contains("BEGIN"));
        assert!(!apply_script(Engine::Mysql, &m(6, "x")).contains("BEGIN"));
    }

    #[test]
    fn migrations_are_read_in_order_and_bad_names_refused() {
        let dir = std::env::temp_dir().join(format!("giong-cli-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_migrations(&dir).unwrap().is_empty());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0002_b.sql"), "-- giong:no-transaction\nSELECT 2;").unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(dir.join("README.md"), "notes").unwrap();
        let ms = read_migrations(&dir).unwrap();
        assert_eq!(ms.iter().map(|m| m.version).collect::<Vec<_>>(), [1, 2]);
        assert!(ms[0].transaction && !ms[1].transaction);
        std::fs::write(dir.join("00001_dup.sql"), "x").unwrap();
        assert_eq!(
            read_migrations(&dir).unwrap_err().code,
            Code::MIGRATION_NAME
        );
        std::fs::remove_file(dir.join("00001_dup.sql")).unwrap();
        std::fs::write(dir.join("schema.sql"), "x").unwrap();
        assert_eq!(
            read_migrations(&dir).unwrap_err().code,
            Code::MIGRATION_NAME
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn error_lines_are_found_for_both_clients() {
        assert_eq!(
            error_line("psql:<stdin>:8: ERROR:  relation \"x\" does not exist"),
            Some(8)
        );
        assert_eq!(
            error_line("ERROR 1146 (42S02) at line 2: Table 'a.b' doesn't exist"),
            Some(2)
        );
        assert_eq!(error_line("FATAL: password authentication failed"), None);
    }

    #[test]
    fn a_migration_error_is_reported_inside_the_file() {
        // psql 17's answers to the script giong sends (BEGIN; + file + ;
        // + INSERT + COMMIT;) for a file of 7 lines.
        let sql = "-- bad\n-- notes\n-- notes\n-- notes\n\nCREATE TABLE t1 (id int);\nCREATE TABLE oops (;\n";
        let unclosed = "psql:<stdin>:11: ERROR:  syntax error at or near \";\"";
        assert_eq!(migration_error_line(unclosed, sql, true), 7);
        let sql = "CREATE TABLE t1 (id int);\nSELECT\n  1\nFROM nope\n";
        let no_semicolon = "psql:<stdin>:6: ERROR:  relation \"nope\" does not exist";
        assert_eq!(migration_error_line(no_semicolon, sql, true), 4);
        let sql = "SELECT 1;\nSELEC 2;\nSELECT 3;\n";
        assert_eq!(
            migration_error_line("psql:<stdin>:3: ERROR:  syntax error", sql, true),
            2
        );
        // mysql names the line the statement starts at; nothing is wrapped.
        assert_eq!(
            migration_error_line("ERROR 1146 (42S02) at line 2: Table", sql, false),
            2
        );
        assert_eq!(migration_error_line("FATAL: no", sql, true), 1);
        assert_eq!(
            migration_error_line("psql:<stdin>:1: ERROR: x", "", true),
            1
        );
    }

    #[test]
    fn passwords_are_hex_and_differ() {
        let a = password();
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, password());
    }
}
