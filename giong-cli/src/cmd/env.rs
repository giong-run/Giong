//! `giong env`: the service's configuration, which comes from the
//! environment only (12-factor). `.env` is a development convenience that
//! `giong dev`, `giong test` and `giong db` load; a service in production
//! reads its real environment and never a file.
//!
//! `.env.example` (committed) lists every key the service reads; `.env`
//! (ignored by git) holds this machine's values.

use std::path::Path;

use serde_json::json;

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dotenv::{self, Var};
use crate::project::find_manifest_dir;
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "env",
    summary: "show and check the service's configuration (.env, .env.example, environment)",
    usage: "giong env [show]\n\
            giong env check\n\
            giong env init\n\
            giong env get <KEY>\n\n\
            show: every key from .env.example, .env and the environment that the project \
            uses, where its value comes from, secrets masked. check: a key of .env.example \
            with no value anywhere is an error (GIONG-E5016), a key only in .env is a \
            warning (add it to .env.example so others know), a line .env cannot parse is an \
            error. init: writes .env from .env.example when there is none. get: one value, \
            the environment first, then .env (for scripts: giong env get DATABASE_URL). \
            show prints a value that only the environment sets as set, without the value, \
            unless --reveal is given.",
    flags: &[Flag {
        name: "reveal",
        kind: Kind::Switch,
        help: "show: print secret values, and values only the environment sets, instead of masking them",
    }],
    max_words: 2,
    run,
};

/// Words in a key's name that say its value is secret (`DB_PASS`,
/// `MYSQL_PWD`, `SESSION_SIGNING_SALT`, `API_KEY`...).
const SECRET_WORDS: &[&str] = &[
    "PASS",
    "PWD",
    "SECRET",
    "TOKEN",
    "KEY",
    "PRIVATE",
    "CREDENTIAL",
    "AUTH",
    "SALT",
    "SIGNING",
    "COOKIE",
    "SESSION",
];

/// Query parameters of a URL whose values are secret.
const SECRET_PARAMS: &[&str] = &[
    "password",
    "pass",
    "passwd",
    "pwd",
    "token",
    "secret",
    "key",
    "sig",
    "signature",
    "access_token",
    "api_key",
    "apikey",
];

/// Whether a key's value should be masked: names that say secret (a word
/// above, or a `_SK` part as in `STRIPE_SK`), and URLs that carry a
/// password or a secret query parameter.
pub fn secret(key: &str, value: &str) -> bool {
    let k = key.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|w| k.contains(w))
        || k.split('_').any(|part| part == "SK")
        || url_password(value)
        || secret_query(value).is_some()
}

/// The value with the secret query parameters of a URL masked, if it has
/// any.
fn secret_query(value: &str) -> Option<String> {
    if !value.contains("://") {
        return None;
    }
    let (base, query) = value.split_once('?')?;
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let mut hit = false;
    let parts: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if SECRET_PARAMS.contains(&k.to_ascii_lowercase().as_str()) => {
                hit = true;
                format!("{k}=***")
            }
            _ => kv.to_string(),
        })
        .collect();
    if !hit {
        return None;
    }
    let mut out = format!("{base}?{}", parts.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

fn url_password(value: &str) -> bool {
    value
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|auth| auth.rsplit_once('@'))
        .is_some_and(|(cred, _)| cred.contains(':'))
}

/// The value as shown: secrets masked (without their length), URL
/// passwords and secret query parameters replaced.
pub fn shown(key: &str, value: &str, reveal: bool) -> String {
    if reveal || value.is_empty() {
        return value.to_string();
    }
    if url_password(value) || secret_query(value).is_some() {
        let v = if url_password(value) {
            super::db::redact(value)
        } else {
            value.to_string()
        };
        return secret_query(&v).unwrap_or(v);
    }
    if secret(key, value) {
        return "*** (set)".to_string();
    }
    value.to_string()
}

/// What `show` prints for a value only the process environment sets.
const ENV_ONLY: &str = "*** (set in the environment)";

/// Where a key's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The process environment (wins over `.env`, as `giong dev` does).
    Environment(String),
    DotEnv(String),
    /// Listed in `.env.example` with an empty value or not at all.
    Missing,
}

pub struct Row {
    pub key: String,
    pub in_example: bool,
    pub source: Source,
    /// The line of `.env.example` that lists the key (1 when it is not there).
    pub example_line: u32,
}

/// Every key of `.env.example` and `.env`, with its source.
pub fn rows(example: &[Var], dot: &[Var], env: &dyn Fn(&str) -> Option<String>) -> Vec<Row> {
    let mut keys: Vec<String> = Vec::new();
    for v in example.iter().chain(dot) {
        if !keys.contains(&v.key) {
            keys.push(v.key.clone());
        }
    }
    keys.into_iter()
        .map(|key| {
            let ex = example.iter().find(|v| v.key == key);
            // The last definition in .env wins, as the loaders do.
            let d = dot.iter().rev().find(|v| v.key == key);
            let source = match (env(&key), d) {
                (Some(v), _) => Source::Environment(v),
                (None, Some(d)) if !d.value.is_empty() => Source::DotEnv(d.value.clone()),
                _ => Source::Missing,
            };
            Row {
                in_example: ex.is_some(),
                example_line: ex.map_or(1, |v| v.line),
                key,
                source,
            }
        })
        .collect()
}

fn root() -> Result<std::path::PathBuf, Failure> {
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("cannot read the current directory: {e}"),
            "run giong env inside a project",
        )
    })?;
    find_manifest_dir(&cwd).ok_or_else(|| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("no Cargo.toml in {} or above it", cwd.display()),
            "run giong env inside a project",
        )
    })
}

/// The variables of a `.env`-style file, never read through a link (a
/// cloned repository can commit `.env -> ~/.aws/credentials`).
fn read(path: &Path) -> Result<(Vec<Var>, Vec<dotenv::BadLine>, bool), Failure> {
    match crate::state::read_to_string(path) {
        Ok(text) => {
            let (v, b) = dotenv::parse(&text);
            Ok((v, b, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), Vec::new(), false)),
        Err(e) => Err(Failure::new(
            Code::USAGE,
            format!("cannot read {}: {e}", path.display()),
            "make it a plain file in the project",
        )),
    }
}

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn run(args: &Args) -> Result<Report, Failure> {
    let sub = args.words.first().map(String::as_str).unwrap_or("show");
    let root = root()?;
    let (example, bad_example, has_example) = read(&root.join(".env.example"))?;
    let (dot, bad_dot, has_dot) = read(&root.join(".env"))?;
    let mut r = Report::new("env");
    r.data("action", json!(sub));
    match sub {
        "init" => {
            if has_dot {
                r.line(".env exists; nothing written.");
                r.data("written", json!(false));
            } else if !has_example {
                return Err(Failure::new(
                    Code::ENV_MISSING,
                    "there is no .env.example to start .env from",
                    "write .env.example with every key the service reads (KEY=, one per line) \
                     and commit it",
                ));
            } else {
                let text =
                    crate::state::read_to_string(&root.join(".env.example")).unwrap_or_default();
                let path = root.join(".env");
                // .env will hold secrets: owner-only, never committed, and
                // never written through a link.
                crate::state::replace_private(&path, text.as_bytes()).map_err(|e| {
                    Failure::new(
                        Code::USAGE,
                        format!("cannot write {}: {e}", path.display()),
                        "check that the project directory is writable",
                    )
                })?;
                let ignored = super::db::ignore_secrets(&root)?;
                r.line("wrote .env from .env.example; fill in the empty values.");
                if !ignored.is_empty() {
                    r.line(format!("added {} to .gitignore.", ignored.join(" and ")));
                }
                r.data("written", json!(true));
            }
            return Ok(r);
        }
        "get" => {
            let Some(key) = args.words.get(1) else {
                return Err(Failure::new(
                    Code::USAGE,
                    "giong env get needs a key",
                    "write giong env get DATABASE_URL",
                ));
            };
            let value = process_env(key).or_else(|| {
                dot.iter()
                    .rev()
                    .find(|v| &v.key == key)
                    .map(|v| v.value.clone())
                    .filter(|v| !v.is_empty())
            });
            match value {
                Some(v) => {
                    r.line(v.clone());
                    r.data("key", json!(key));
                    r.data("value", json!(v));
                    return Ok(r);
                }
                None => {
                    return Err(Failure::new(
                        Code::ENV_MISSING,
                        format!("{key} is set neither in the environment nor in .env"),
                        format!("set {key} in .env (giong env init starts one from .env.example)"),
                    ))
                }
            }
        }
        "show" | "check" => {}
        other => {
            return Err(Failure::new(
                Code::USAGE,
                format!("giong env has no action {other:?}"),
                "write giong env, giong env check, giong env init or giong env get <KEY>",
            ))
        }
    }
    if args.words.len() > 1 {
        return Err(Failure::new(
            Code::USAGE,
            format!("giong env {sub} takes no key"),
            "only giong env get takes a key",
        ));
    }
    let reveal = args.has("reveal");
    let list = rows(&example, &dot, &process_env);
    let width = list.iter().map(|x| x.key.len()).max().unwrap_or(0);
    let mut json_rows = Vec::new();
    for row in &list {
        let (from, value) = match &row.source {
            Source::Environment(v) => ("environment", Some(v.as_str())),
            Source::DotEnv(v) => (".env", Some(v.as_str())),
            Source::Missing => ("missing", None),
        };
        // A committed .env.example chooses which keys are listed, so a value
        // that only the process environment holds (AWS_ACCESS_KEY_ID, or
        // anything the name check misses) is not printed unless asked for.
        let env_only =
            matches!(row.source, Source::Environment(_)) && !dot.iter().any(|d| d.key == row.key);
        let show = value.map(|v| {
            if env_only && !reveal {
                ENV_ONLY.to_string()
            } else {
                shown(&row.key, v, reveal)
            }
        });
        if sub == "show" {
            r.line(format!(
                "{:width$}  {:<11}  {}",
                row.key,
                from,
                show.as_deref().unwrap_or("")
            ));
        }
        json_rows.push(json!({
            "key": row.key, "source": from, "value": show,
            "in_example": row.in_example,
            "secret": value.is_some_and(|v| secret(&row.key, v)),
        }));
        if row.in_example && row.source == Source::Missing {
            r.push(
                Finding::new(
                    Code::ENV_MISSING,
                    Severity::Error,
                    format!("{} has no value", row.key),
                )
                .detail(".env.example lists it, and neither the environment nor .env sets it")
                .fix(match row.key.as_str() {
                    "DATABASE_URL" | "REDIS_URL" => "giong db up writes it into .env".to_string(),
                    k => format!("set {k}=... in .env"),
                })
                .at(".env.example", row.example_line, 1),
            );
        }
        if !row.in_example && has_example && sub == "check" {
            r.push(
                Finding::new(
                    Code::ENV_MISSING,
                    Severity::Warning,
                    format!("{} is in .env but not in .env.example", row.key),
                )
                .detail("others who clone the project will not know the service reads it")
                .fix(format!("add {}= to .env.example", row.key)),
            );
        }
    }
    for (file, bad) in [(".env", &bad_dot), (".env.example", &bad_example)] {
        for b in bad {
            r.push(
                Finding::new(
                    Code::ENV_MISSING,
                    Severity::Error,
                    format!("{file} line {} cannot be read", b.line),
                )
                .detail(b.why)
                .fix("write KEY=value, one per line; quote values with spaces or #")
                .at(file, b.line, 1),
            );
        }
    }
    if !has_example && sub == "check" {
        r.push(
            Finding::new(Code::ENV_MISSING, Severity::Warning, "no .env.example")
                .detail("nothing lists the keys the service reads")
                .fix("write .env.example (KEY=, one per line) and commit it"),
        );
    }
    if list.is_empty() && sub == "show" {
        r.line("No .env or .env.example here.");
    }
    if sub == "check" && r.ok() {
        r.line(format!("{} keys, all set.", list.len()));
    }
    r.data("keys", json!(json_rows));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(k: &str, v: &str) -> Var {
        Var {
            key: k.into(),
            value: v.into(),
            line: 1,
        }
    }

    #[test]
    fn secrets_are_masked() {
        assert!(secret("GIONG_MCP_TOKEN", "x"));
        assert!(secret("stripe_api_key", "x"));
        assert!(secret("DATABASE_URL", "postgres://u:p@h/db"));
        assert!(!secret("DATABASE_URL", "postgres://h/db"));
        assert!(!secret("PORT", "8080"));
        assert_eq!(shown("PORT", "8080", false), "8080");
        assert_eq!(shown("API_TOKEN", "abcdef", false), "*** (set)");
        for k in [
            "DB_PASS",
            "MYSQL_PWD",
            "STRIPE_SK",
            "SESSION_SIGNING_SALT",
            "COOKIE_KEY",
        ] {
            assert!(secret(k, "x"), "{k}");
        }
        assert!(!secret("TASK_LIMIT", "x"), "SK only as a whole part");
        assert!(!secret("PORT", "x"));
        assert_eq!(
            shown(
                "DSN",
                "postgres://h/db?user=u&password=pw&sslmode=require#x",
                false
            ),
            "postgres://h/db?user=u&password=***&sslmode=require#x"
        );
        assert_eq!(
            shown("DSN", "postgres://u:pw@h/db?token=t", false),
            "postgres://u:***@h/db?token=***"
        );
        assert_eq!(shown("URL", "https://h/p?q=1", false), "https://h/p?q=1");
        assert_eq!(shown("API_TOKEN", "abcdef", true), "abcdef");
        assert_eq!(
            shown("DATABASE_URL", "postgres://u:pw@h:1/d", false),
            "postgres://u:***@h:1/d"
        );
        assert_eq!(shown("API_TOKEN", "", false), "");
    }

    #[test]
    fn sources_prefer_the_environment_then_the_last_dotenv_line() {
        let example = [
            var("PORT", "8080"),
            var("DATABASE_URL", ""),
            var("EMPTY", ""),
        ];
        let dot = [
            var("PORT", "1"),
            var("PORT", "2"),
            var("EXTRA", "x"),
            var("EMPTY", ""),
        ];
        let env = |k: &str| (k == "DATABASE_URL").then(|| "postgres://h/db".to_string());
        let r = rows(&example, &dot, &env);
        let get = |k: &str| r.iter().find(|x| x.key == k).unwrap();
        assert_eq!(get("PORT").source, Source::DotEnv("2".into()));
        assert_eq!(
            get("DATABASE_URL").source,
            Source::Environment("postgres://h/db".into())
        );
        assert_eq!(get("EMPTY").source, Source::Missing);
        assert!(!get("EXTRA").in_example);
        assert_eq!(r.len(), 4);
    }
}
