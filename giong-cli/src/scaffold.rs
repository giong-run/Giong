//! The projects `giong new` writes: which templates exist, what may be asked of
//! them, and the files they become. Writing them to disk is `cmd::new`'s job;
//! this module only computes them, so every combination can be tested.
//!
//! The templates live in `templates/` (embedded by `build.rs`):
//! `templates/common/` goes into every project, `templates/<template>/` into
//! projects of that template, and a file of the template replaces a common file
//! of the same path. A path segment `dot.x` becomes `.x`, and `.tmpl` is dropped.

use std::fmt;
use std::path::Path;

use crate::code::Code;
use crate::report::Failure;
use crate::template::{self, Context};

mod files {
    include!(concat!(env!("OUT_DIR"), "/templates.rs"));
}

/// What a project starts as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Template {
    /// An HTTP API on giong-web.
    Web,
    /// A gRPC service on giong-grpc.
    Grpc,
    /// A background worker: jobs from a queue, or on a timer.
    Worker,
}

impl Template {
    pub const ALL: [Template; 3] = [Template::Web, Template::Grpc, Template::Worker];

    pub fn name(self) -> &'static str {
        match self {
            Template::Web => "web",
            Template::Grpc => "grpc",
            Template::Worker => "worker",
        }
    }

    pub fn parse(s: &str) -> Option<Template> {
        Template::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The port it listens on unless `LISTEN` says otherwise.
    fn default_port(self, tls: bool) -> u16 {
        match (self, tls) {
            (Template::Grpc, _) => 50051,
            (_, true) => 8443,
            _ => 8080,
        }
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A store a project uses.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Db {
    Postgres,
    Mysql,
    /// A file in the project, through rusqlite on the blocking lane: no
    /// server to start.
    Sqlite,
    Redis,
}

impl Db {
    pub const ALL: [Db; 4] = [Db::Postgres, Db::Mysql, Db::Sqlite, Db::Redis];

    pub fn name(self) -> &'static str {
        match self {
            Db::Postgres => "postgres",
            Db::Mysql => "mysql",
            Db::Sqlite => "sqlite",
            Db::Redis => "redis",
        }
    }

    /// Whether `giong db up` has a server to start for it.
    pub fn has_server(self) -> bool {
        self != Db::Sqlite
    }

    pub fn parse(s: &str) -> Option<Db> {
        match s {
            "postgres" | "postgresql" | "pg" => Some(Db::Postgres),
            "mysql" | "mariadb" => Some(Db::Mysql),
            "sqlite" | "sqlite3" => Some(Db::Sqlite),
            "redis" | "valkey" => Some(Db::Redis),
            _ => None,
        }
    }
}

/// Everything `giong new` was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The package name (`api`, `user-service`).
    pub name: String,
    pub template: Template,
    /// Sorted, without repeats.
    pub dbs: Vec<Db>,
    pub tls: bool,
    /// Depend on the crates of a giong checkout by path instead of crates.io:
    /// for giong's own CI and for people working on giong.
    pub giong_path: Option<String>,
}

impl Options {
    pub fn new(name: &str, template: Template) -> Options {
        Options {
            name: name.to_string(),
            template,
            dbs: Vec::new(),
            tls: false,
            giong_path: None,
        }
    }

    fn has(&self, db: Db) -> bool {
        self.dbs.contains(&db)
    }
}

/// The stores of a template when `--db` is not given: SQLite for web, so a
/// new project serves a real API with nothing to install or start; none for
/// the others (a worker's queue is shared, which a file cannot be).
pub fn default_dbs(template: Template) -> Vec<Db> {
    match template {
        Template::Web => vec![Db::Sqlite],
        Template::Grpc | Template::Worker => Vec::new(),
    }
}

/// Parses `--db postgres,redis` into a sorted list without repeats; `none`
/// is the empty list.
pub fn parse_dbs(list: &str) -> Result<Vec<Db>, Failure> {
    if list.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut dbs = Vec::new();
    for word in list.split(',').map(str::trim) {
        if word.is_empty() {
            continue;
        }
        let db = Db::parse(&word.to_ascii_lowercase()).ok_or_else(|| {
            Failure::new(
                Code::BAD_TEMPLATE_CHOICE,
                format!("--db {word:?} is not a store giong new knows"),
                "use sqlite, postgres, mysql or redis, several separated by commas (--db \
                 postgres,redis), or none",
            )
        })?;
        if !dbs.contains(&db) {
            dbs.push(db);
        }
    }
    dbs.sort();
    Ok(dbs)
}

/// Words that cannot name a Rust crate.
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Names taken by the standard library, by the crates a project depends on,
/// or by Cargo itself.
const TAKEN: &[&str] = &[
    "std",
    "core",
    "alloc",
    "proc_macro",
    "proc-macro",
    "test",
    "build",
    "deps",
    "examples",
    "incremental",
    "serde",
    "serde_json",
    // SQLite, the default database of `giong new`.
    "rusqlite",
    "giong",
    "giong-web",
    "giong-http",
    "giong-tls",
    "giong-postgres",
    "giong-mysql",
    "giong-redis",
    "giong-mcp",
    "giong-grpc",
    "giong-proto",
    "giong-grpc-build",
    "giong-compat",
    "giong-quic",
    "giong-cli",
];

/// Names Windows keeps for devices: a directory cannot have them there.
fn windows_device(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || ((lower.starts_with("com") || lower.starts_with("lpt"))
            && lower.len() == 4
            && lower.as_bytes()[3].is_ascii_digit()
            && lower.as_bytes()[3] != b'0')
}

/// Checks a package name the way cargo does, and a little stricter: lower
/// case, so the one name works as the package, the binary, the directory and
/// (with `_`) the crate.
pub fn check_name(name: &str) -> Result<(), Failure> {
    let bad = |what: String, fix: String| Err(Failure::new(Code::BAD_NAME, what, fix));
    if name.is_empty() {
        return bad(
            "the project name is empty".into(),
            "give a name such as api or user-service".into(),
        );
    }
    if name.len() > 64 {
        return bad(
            format!("the project name is {} bytes long", name.len()),
            "use at most 64 characters".into(),
        );
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return bad(
            format!("the project name {name:?} has the character {c:?}"),
            "use letters a-z, digits, - and _ only".into(),
        );
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return bad(
            format!("the project name {name:?} has capital letters"),
            format!("use lower case: {}", name.to_ascii_lowercase()),
        );
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return bad(
            format!("the project name {name:?} does not start with a letter"),
            "start it with a letter a-z".into(),
        );
    }
    if name.ends_with(['-', '_']) {
        return bad(
            format!(
                "the project name {name:?} ends with {:?}",
                &name[name.len() - 1..]
            ),
            "end it with a letter or a digit".into(),
        );
    }
    let crate_name = name.replace('-', "_");
    if KEYWORDS.contains(&crate_name.as_str()) {
        return bad(
            format!("{name:?} is a Rust keyword, which cannot name a crate"),
            format!("add a word: {name}-service, or my-{name}"),
        );
    }
    if TAKEN.iter().any(|t| t.replace('-', "_") == crate_name) {
        return bad(
            format!("{name:?} is the name of a crate the project depends on, or one cargo keeps"),
            format!("add a word: {name}-service, or my-{name}"),
        );
    }
    if windows_device(name) {
        return bad(
            format!("{name:?} names a device on Windows, so no directory can have it there"),
            format!("add a word: {name}-service"),
        );
    }
    Ok(())
}

/// Checks that the template offers what was asked for.
pub fn check_choices(o: &Options) -> Result<(), Failure> {
    let refuse = |what: String, fix: &str| {
        Err(Failure::new(
            Code::BAD_TEMPLATE_CHOICE,
            what,
            fix.to_string(),
        ))
    };
    let sql: Vec<&str> = o
        .dbs
        .iter()
        .filter(|d| **d != Db::Redis)
        .map(|d| d.name())
        .collect();
    if sql.len() > 1 {
        return refuse(
            format!("--db asks for {}", sql.join(" and ")),
            "pick one SQL database: --db sqlite, --db postgres or --db mysql (redis goes with \
             any of them)",
        );
    }
    match o.template {
        Template::Web => Ok(()),
        Template::Grpc if !o.dbs.is_empty() => refuse(
            "the grpc template has no database example yet".into(),
            "drop --db; a pool goes into the service struct, built in each worker (the web \
             template shows the pattern), or start from --template web",
        ),
        Template::Grpc => Ok(()),
        Template::Worker if o.tls => refuse(
            "the worker template serves nothing, so --tls has nothing to do".into(),
            "drop --tls",
        ),
        Template::Worker if o.dbs.len() > 1 => refuse(
            "the worker template takes its jobs from one store".into(),
            "pick one of --db postgres, --db mysql or --db redis (or none, for a timer)",
        ),
        Template::Worker if o.has(Db::Sqlite) => refuse(
            "the worker template has no sqlite queue: its jobs are shared by every copy of \
             the worker, which a file on one machine cannot be"
                .into(),
            "use --db postgres, mysql or redis for the queue (or none, for a timer); for \
             SQLite, start from --template web",
        ),
        Template::Worker => Ok(()),
    }
}

/// `path = '...'` of a giong crate in a checkout, as TOML.
fn path_source(checkout: &str, krate: &str) -> String {
    let dir = if krate == "giong" {
        checkout.trim_end_matches(['/', '\\']).to_string()
    } else {
        format!("{}/{krate}", checkout.trim_end_matches(['/', '\\']))
    };
    format!("path = {}", toml_string(&dir))
}

/// A TOML string: literal (`'...'`, no escapes, right for Windows paths) when
/// it can be, basic with escapes otherwise.
pub fn toml_string(s: &str) -> String {
    if !s.contains('\'') && !s.chars().any(char::is_control) {
        return format!("'{s}'");
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The crates a template may name as `{{src_<crate>}}`.
const GIONG_CRATES: &[&str] = &[
    "giong",
    "giong-web",
    "giong-http",
    "giong-tls",
    "giong-postgres",
    "giong-mysql",
    "giong-redis",
    "giong-mcp",
    "giong-grpc",
    "giong-proto",
    "giong-grpc-build",
];

/// The Cargo profiles of every project (`giong doctor` checks the dev one).
const PROFILES: &str = r#"# Faster rebuilds while you work: backtraces keep file and line, the
# dependencies carry no debug information, and there is much less to write
# and link after each change (`giong doctor` checks this).
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
debug = false

# A smaller binary to ship.
[profile.release]
lto = "thin"
strip = true"#;

/// The version of the giong crates the project depends on.
pub const GIONG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The context a project's templates are rendered with.
pub fn context(o: &Options) -> Context {
    let mut cx = Context::new();
    let crate_name = o.name.replace('-', "_");
    cx.var("name", o.name.as_str())
        .var("crate", crate_name)
        .var("giong_version", GIONG_VERSION)
        .var("default_port", o.template.default_port(o.tls).to_string())
        .var("profiles", PROFILES);
    for krate in GIONG_CRATES {
        let source = match &o.giong_path {
            Some(checkout) => path_source(checkout, krate),
            None => format!("version = \"{GIONG_VERSION}\""),
        };
        cx.var(&format!("src_{}", krate.replace('-', "_")), source);
    }
    for t in Template::ALL {
        cx.flag(t.name(), t == o.template);
    }
    for db in Db::ALL {
        cx.flag(db.name(), o.has(db));
    }
    let sql_server = o.has(Db::Postgres) || o.has(Db::Mysql);
    cx.flag("sql", sql_server || o.has(Db::Sqlite))
        // A SQL database reached over the network, through a pool per worker.
        .flag("sql_server", sql_server)
        .flag("db", !o.dbs.is_empty())
        // A store `giong db up` starts (and CI runs as a service).
        .flag("servers", o.dbs.iter().any(|d| d.has_server()))
        .flag("tls", o.tls)
        .flag("local", o.giong_path.is_some())
        .flag(
            "dev_background",
            crate::cmd::ai::Offer::here().dev_background,
        );
    cx
}

/// The path a template file is written to: `dot.` segments become `.`, and
/// `.tmpl` is dropped.
pub fn output_path(template_path: &str) -> String {
    let path = template_path.strip_suffix(".tmpl").unwrap_or(template_path);
    path.split('/')
        .map(|seg| match seg.strip_prefix("dot.") {
            Some(rest) => format!(".{rest}"),
            None => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// One file of a new project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// Relative to the project, with `/`.
    pub path: String,
    pub text: String,
}

/// The files of a project, sorted by path. Fails only on a template bug.
pub fn render(o: &Options) -> Result<Vec<File>, Failure> {
    check_name(&o.name)?;
    check_choices(o)?;
    let cx = context(o);
    let template_dir = format!("{}/", o.template.name());
    let mut out: Vec<File> = Vec::new();
    // Common files first, so a template's own file of the same path wins.
    for (prefix, overrides) in [("common/", false), (template_dir.as_str(), true)] {
        for (path, text) in files::FILES {
            let Some(rel) = path.strip_prefix(prefix) else {
                continue;
            };
            let rendered = template::render(text, &cx).map_err(|e| {
                Failure::new(
                    Code::WRITE_FAILED,
                    format!("templates/{path}: {e}"),
                    "this is a bug in giong-cli's templates: report it at \
                     https://github.com/giong-run/Giong/issues",
                )
            })?;
            let Some(text) = rendered else { continue };
            let path = output_path(rel);
            if overrides {
                out.retain(|f| f.path != path);
            }
            out.push(File { path, text });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Every template file, for the tests.
#[cfg(test)]
pub fn template_files() -> &'static [(&'static str, &'static str)] {
    files::FILES
}

/// Whether `dir` looks like a giong checkout (for `--giong-path`).
pub fn is_checkout(dir: &Path) -> bool {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    manifest.contains("name = \"giong\"") && dir.join("giong-web/Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination the templates accept, and only those.
    pub fn every_choice() -> Vec<Options> {
        let mut all = Vec::new();
        let db_sets: Vec<Vec<Db>> = (0u8..1 << Db::ALL.len())
            .map(|bits| {
                Db::ALL
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| bits & (1 << i) != 0)
                    .map(|(_, d)| d)
                    .collect()
            })
            .collect();
        for template in Template::ALL {
            for dbs in &db_sets {
                for tls in [false, true] {
                    for local in [false, true] {
                        let mut o = Options::new("my-api", template);
                        o.dbs = dbs.clone();
                        o.tls = tls;
                        o.giong_path = local.then(|| "/src/giong".to_string());
                        if check_choices(&o).is_ok() {
                            all.push(o);
                        }
                    }
                }
            }
        }
        all
    }

    #[test]
    fn every_template_renders_with_every_choice() {
        let all = every_choice();
        // web: 8 db sets (at most one SQL database, redis or not) x tls x
        // local; grpc: 1 x 2 x 2; worker: 4 (none, pg, mysql, redis) x 1 x 2.
        assert_eq!(all.len(), 8 * 2 * 2 + 2 * 2 + 4 * 2);
        for o in all {
            let files = render(&o).unwrap_or_else(|f| panic!("{o:?}: {f}"));
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            for must in [
                "Cargo.toml",
                "src/main.rs",
                "src/lib.rs",
                ".gitignore",
                ".env.example",
                "Dockerfile",
                "README.md",
                ".github/workflows/ci.yml",
                ".dockerignore",
            ] {
                assert!(paths.contains(&must), "{o:?} lacks {must}: {paths:?}");
            }
            assert!(paths.iter().any(|p| p.starts_with("tests/")), "{o:?}");
            let sql = o.has(Db::Postgres) || o.has(Db::Mysql) || o.has(Db::Sqlite);
            assert_eq!(paths.contains(&"migrations/0001_init.sql"), sql, "{o:?}");
            for f in &files {
                let tag_left = f.text.match_indices("{{").any(|(i, _)| {
                    f.text[i + 2..]
                        .starts_with(|c: char| c.is_ascii_alphabetic() || c == '#' || c == '/')
                });
                assert!(!tag_left, "{o:?} {}: a tag is left", f.path);
                assert!(
                    !f.path.contains(".tmpl") && !f.path.contains("dot."),
                    "{}",
                    f.path
                );
                assert!(
                    !f.text.contains("\n\n\n"),
                    "{o:?} {}: two blank lines",
                    f.path
                );
                assert!(
                    f.text.ends_with('\n'),
                    "{o:?} {}: no newline at the end",
                    f.path
                );
                assert!(
                    !f.text.lines().any(|l| l.ends_with(' ')),
                    "{o:?} {}: trailing space",
                    f.path
                );
            }
            let cargo = &files.iter().find(|f| f.path == "Cargo.toml").unwrap().text;
            assert!(cargo.contains("name = \"my-api\""));
            assert!(cargo.contains("debug = \"line-tables-only\""));
            // giong routes and giong openapi ask giong-mcp's routes tool,
            // which its `web` feature adds.
            if o.template == Template::Web {
                assert!(
                    cargo
                        .lines()
                        .any(|l| l.starts_with("giong-mcp =") && l.contains("features = [\"web\"]")),
                    "{o:?}: {cargo}"
                );
            }
            for db in Db::ALL {
                let dep = match db {
                    Db::Sqlite => "rusqlite =".to_string(),
                    _ => format!("giong-{} =", db.name()),
                };
                assert_eq!(cargo.contains(&dep), o.has(db), "{o:?}: {dep}");
            }
            // SQLite needs no server: nothing to start, in development or CI.
            let ci = &files
                .iter()
                .find(|f| f.path == ".github/workflows/ci.yml")
                .unwrap()
                .text;
            let servers = o.dbs.iter().any(|d| d.has_server());
            assert_eq!(ci.contains("services:"), servers, "{o:?}");
            let readme = &files.iter().find(|f| f.path == "README.md").unwrap().text;
            assert_eq!(readme.contains("giong db up"), servers, "{o:?}");
            if o.giong_path.is_some() {
                assert!(cargo.contains("giong = { path = '/src/giong' }"), "{cargo}");
                assert!(!cargo.contains(GIONG_VERSION), "{cargo}");
            } else {
                assert!(cargo.contains(&format!("giong = {{ version = \"{GIONG_VERSION}\" }}")));
            }
        }
    }

    /// The README teaches the one `giong dev` an agent keeps running when this
    /// giong has it, and plain `giong dev` when it does not.
    #[test]
    fn readme_teaches_the_dev_runner_this_giong_has() {
        let readme = files::FILES
            .iter()
            .find(|(p, _)| *p == "common/README.md.tmpl")
            .unwrap()
            .1;
        for template in Template::ALL {
            let o = Options::new("my-api", template);
            for background in [true, false] {
                let mut cx = context(&o);
                cx.flag("dev_background", background);
                let text = template::render(readme, &cx).unwrap().unwrap();
                assert_eq!(text.contains("giong dev --background"), background);
                assert_eq!(text.contains("giong dev stop"), background);
                assert_eq!(text.contains("`cargo run`"), !background);
                assert_eq!(text.contains("giong dev                "), !background);
                assert_eq!(
                    text.contains("Try the service on the address it prints"),
                    background && template != Template::Worker
                );
            }
        }
    }

    #[test]
    fn every_template_file_is_used() {
        let mut used = std::collections::BTreeSet::new();
        for o in every_choice() {
            let cx = context(&o);
            for (path, text) in template_files() {
                let dir = path.split('/').next().unwrap();
                if (dir == "common" || dir == o.template.name())
                    && template::render(text, &cx).unwrap().is_some()
                {
                    used.insert(*path);
                }
            }
        }
        for (path, _) in template_files() {
            assert!(used.contains(path), "templates/{path} is never written");
        }
    }

    #[test]
    fn names() {
        for good in [
            "api",
            "user-service",
            "a1",
            "my_api",
            "x",
            "giong-shop",
            "web2",
        ] {
            check_name(good).unwrap_or_else(|f| panic!("{good}: {f}"));
        }
        for (bad, says) in [
            ("", "empty"),
            ("Api", "capital"),
            ("1api", "start with a letter"),
            ("-api", "start with a letter"),
            ("_api", "start with a letter"),
            ("api-", "ends with"),
            ("my api", "character ' '"),
            ("api.rs", "character '.'"),
            ("ăpi", "character"),
            ("../x", "character '.'"),
            ("a/b", "character '/'"),
            ("fn", "keyword"),
            ("async", "keyword"),
            ("self", "keyword"),
            ("std", "depends on"),
            ("test", "depends on"),
            ("giong", "depends on"),
            ("giong-web", "depends on"),
            ("giong_web", "depends on"),
            ("serde", "depends on"),
            ("rusqlite", "depends on"),
            ("con", "device"),
            ("com1", "device"),
            ("lpt9", "device"),
        ] {
            let f = check_name(bad).expect_err(bad);
            assert_eq!(f.code, Code::BAD_NAME);
            assert!(f.what.contains(says), "{bad:?}: {}", f.what);
            assert!(!f.fix.is_empty());
        }
        check_name("com0").unwrap();
        check_name("console").unwrap();
        check_name(&"a".repeat(64)).unwrap();
        assert!(check_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn choices() {
        assert_eq!(
            parse_dbs("redis,postgres,redis").unwrap(),
            [Db::Postgres, Db::Redis]
        );
        assert_eq!(
            parse_dbs(" PG , valkey ").unwrap(),
            [Db::Postgres, Db::Redis]
        );
        assert_eq!(parse_dbs("").unwrap(), []);
        let f = parse_dbs("mongo").unwrap_err();
        assert_eq!(f.code, Code::BAD_TEMPLATE_CHOICE);
        assert!(f.what.contains("mongo"));

        let refused = |t, dbs: &[Db], tls| {
            let mut o = Options::new("x", t);
            o.dbs = dbs.to_vec();
            o.tls = tls;
            check_choices(&o).expect_err("refused").code
        };
        assert_eq!(
            refused(Template::Web, &[Db::Postgres, Db::Mysql], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Grpc, &[Db::Redis], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Worker, &[], true),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Worker, &[Db::Postgres, Db::Redis], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Web, &[Db::Postgres, Db::Sqlite], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Worker, &[Db::Sqlite], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(
            refused(Template::Grpc, &[Db::Sqlite], false),
            Code::BAD_TEMPLATE_CHOICE
        );
        assert_eq!(parse_dbs("redis,sqlite").unwrap(), [Db::Sqlite, Db::Redis]);
        assert_eq!(parse_dbs("SQLite3").unwrap(), [Db::Sqlite]);
        assert_eq!(Template::parse("web"), Some(Template::Web));
        assert_eq!(Template::parse("Web"), None);
    }

    #[test]
    fn paths_and_toml() {
        assert_eq!(
            output_path("dot.github/workflows/ci.yml.tmpl"),
            ".github/workflows/ci.yml"
        );
        assert_eq!(output_path("dot.env.example.tmpl"), ".env.example");
        assert_eq!(output_path("src/main.rs.tmpl"), "src/main.rs");
        assert_eq!(output_path("a/dotty.tmpl"), "a/dotty");
        assert_eq!(toml_string("/src/giong"), "'/src/giong'");
        assert_eq!(toml_string(r"C:\src\giong"), r"'C:\src\giong'");
        assert_eq!(toml_string("/it's"), "\"/it's\"");
        assert_eq!(toml_string(r"C:\it's"), r#""C:\\it's""#);
        assert_eq!(toml_string("a\"b'c\n"), r#""a\"b'c\u000A""#);
        assert_eq!(path_source("/g/", "giong"), "path = '/g'");
        assert_eq!(
            path_source(r"C:\g\", "giong-web"),
            r"path = 'C:\g/giong-web'"
        );
    }

    /// examples/rest-api is what `giong new rest-api --db postgres,redis --tls`
    /// writes, plus its own test (tests/api.rs, with throwaway servers) and the
    /// doctests of the repository's Markdown at the end of src/lib.rs.
    /// `GIONG_CLI_REGENERATE=1` rewrites it from the template.
    #[test]
    fn rest_api_is_the_web_template() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/rest-api");
        if !example.exists() {
            return; // the published crate
        }
        let mut o = Options::new("rest-api", Template::Web);
        o.dbs = vec![Db::Postgres, Db::Redis];
        o.tls = true;
        let files = render(&o).unwrap();
        let regenerate = std::env::var_os("GIONG_CLI_REGENERATE").is_some();
        let mut stale = Vec::new();
        for path in [
            "src/main.rs",
            "src/lib.rs",
            "src/users.rs",
            "migrations/0001_init.sql",
        ] {
            let mut want = files.iter().find(|f| f.path == path).unwrap().text.clone();
            let file = example.join(path);
            let have = std::fs::read_to_string(&file).unwrap_or_default();
            if path == "src/lib.rs" {
                let tail = have
                    .find(REST_API_TAIL)
                    .map(|i| have[i..].to_string())
                    .expect("examples/rest-api/src/lib.rs keeps its doctests after the marker");
                want = format!("{want}\n{tail}");
            }
            if regenerate {
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, &want).unwrap();
            } else if have != want {
                stale.push(path);
            }
        }
        assert!(
            stale.is_empty(),
            "examples/rest-api differs from the web template in {stale:?}: run \
             GIONG_CLI_REGENERATE=1 cargo test -p giong-cli rest_api (and change the template, \
             not the example)"
        );
    }

    /// Where the part of examples/rest-api/src/lib.rs that is not the template's
    /// begins.
    const REST_API_TAIL: &str = "// Not from the template:";

    #[test]
    fn ports() {
        let port = |t, tls| {
            let mut o = Options::new("x", t);
            o.tls = tls;
            render(&o)
                .unwrap()
                .into_iter()
                .find(|f| f.path == "src/main.rs")
                .unwrap()
                .text
        };
        assert!(port(Template::Web, false).contains("0.0.0.0:8080"));
        assert!(port(Template::Web, true).contains("0.0.0.0:8443"));
        assert!(port(Template::Grpc, false).contains("0.0.0.0:50051"));
    }
}
