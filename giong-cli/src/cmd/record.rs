//! `giong record`: list the requests the service recorded, save chosen ones
//! as cases, and replay case files against the running service.
//!
//! The service records with giong-web's `Record` middleware (one `exchange`
//! JSON line per answered request); `giong dev` keeps the service's output
//! in `.giong/dev.log`, which is where `list` and `save` read by default.

use std::collections::BTreeSet;
use std::io::{BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::routes::{self, BASE_FLAG};
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::record::case::{self, Case};
use crate::record::exchange::{self, Exchange, Filter};
use crate::record::matcher::Missing;
use crate::record::replay::{self, Outcome};
use crate::record::secret;
use crate::report::{Failure, Finding, Report, Severity};
use crate::service::{self, DevState};
use crate::web_client::Base;

pub const COMMAND: Command = Command {
    name: "record",
    summary: "turn recorded requests into cases and replay them against the service",
    usage: "giong record list [--route <text>] [--method <m>] [--status <404|4xx>] [--limit <n>] [--from <log>]\n\
            giong record show <number> [--from <log>]\n\
            giong record save <numbers|3-7|last|all> [--name <name> | --out <file>] [--append | --force] [--keep-secrets]\n\
            giong record replay [<file>...] [--base-url <url>] [--timeout-ms <n>]\n\n\
            The service records with giong-web's Record middleware: one `exchange` line per \
            answered request, headers and bodies included, secret headers redacted. `list` \
            numbers the exchanges in the log (default: the log of `giong dev` here, \
            .giong/dev.log; --from reads another file, - reads standard input). `save` writes \
            the chosen ones to tests/recorded/<name>.json (default name: recorded) as cases: \
            the request, and the status, content type, Location and body expected, with dates, \
            UUIDs, tokens and ids the service made replaced by placeholders, and ids that a \
            later request uses captured and reused. Secrets stay out of the file: redacted \
            headers and request fields whose names say secret (password, token, api_key, \
            card, ...) in the query, a form or a JSON body are read from \
            GIONG_RECORD_<NAME> when replayed, and `save` refuses to write a value that \
            still looks like one (a card number, a JWT, a value under such a name) unless \
            --keep-secrets is given. `replay` sends every case of the files given \
            (default: tests/recorded/*.json) in order to the running service and checks each \
            answer; exit status 1 when a case fails, so it runs in CI. Replay against a fresh \
            service: cases that create things expect the state they were recorded in.",
    flags: &[
        Flag {
            name: "from",
            kind: Kind::Value,
            help: "read exchanges from this file (- for standard input) instead of giong dev's log",
        },
        Flag {
            name: "route",
            kind: Kind::Value,
            help: "only exchanges whose route pattern or path contains this text",
        },
        Flag {
            name: "method",
            kind: Kind::Value,
            help: "only exchanges with this method",
        },
        Flag {
            name: "status",
            kind: Kind::Value,
            help: "only exchanges with this status (404) or class (4xx)",
        },
        Flag {
            name: "limit",
            kind: Kind::Value,
            help: "list at most this many, the newest (default 50)",
        },
        Flag {
            name: "name",
            kind: Kind::Value,
            help: "save to tests/recorded/<name>.json (default: recorded)",
        },
        Flag {
            name: "out",
            kind: Kind::Value,
            help: "save to this file instead",
        },
        Flag {
            name: "append",
            kind: Kind::Switch,
            help: "add the cases to the end of an existing file",
        },
        Flag {
            name: "force",
            kind: Kind::Switch,
            help: "replace an existing file",
        },
        Flag {
            name: "keep-secrets",
            kind: Kind::Switch,
            help: "save values that look like secrets instead of refusing (case files are meant to be committed)",
        },
        BASE_FLAG,
        Flag {
            name: "timeout-ms",
            kind: Kind::Value,
            help: "how long one replayed request may take (default 10000)",
        },
    ],
    max_words: 1000,
    run,
};

fn usage_failure(what: impl Into<String>) -> Failure {
    Failure::new(
        Code::USAGE,
        what,
        "giong record list, show <n>, save <numbers> or replay; giong record --help shows all",
    )
}

fn run(args: &Args) -> Result<Report, Failure> {
    let Some(sub) = args.words.first() else {
        return Err(usage_failure(
            "giong record needs list, show, save or replay",
        ));
    };
    let rest = &args.words[1..];
    let allowed: &[&str] = match sub.as_str() {
        "list" => &["from", "route", "method", "status", "limit"],
        "show" => &["from"],
        "save" => &[
            "from",
            "route",
            "method",
            "status",
            "name",
            "out",
            "append",
            "force",
            "keep-secrets",
        ],
        "replay" => &["base-url", "timeout-ms"],
        other => {
            let close = ["list", "show", "save", "replay"]
                .into_iter()
                .find(|c| crate::args::distance(c, other) <= 2);
            return Err(Failure::new(
                Code::USAGE,
                format!("giong record has no {other:?}"),
                match close {
                    Some(c) => format!("did you mean giong record {c}?"),
                    None => "use list, show, save or replay".into(),
                },
            ));
        }
    };
    for f in COMMAND.flags {
        if (args.has(f.name) || args.value(f.name).is_some()) && !allowed.contains(&f.name) {
            return Err(usage_failure(format!(
                "--{} does not go with giong record {sub}",
                f.name
            )));
        }
    }
    match sub.as_str() {
        "list" => list(args, rest),
        "show" => show(args, rest),
        "save" => save(args, rest),
        _ => replay_files(args, rest),
    }
}

/// Where exchanges are read from, for messages.
struct Source {
    label: String,
    read: exchange::Read,
}

fn source(args: &Args) -> Result<Source, Failure> {
    let open_failed = |p: &Path, e: std::io::Error| {
        Failure::new(
            Code::NO_RECORDING,
            format!("{} cannot be read: {e}", p.display()),
            "give --from a log file that exists, or run the service with giong dev",
        )
    };
    let (label, read) = match args.value("from") {
        Some("-") => {
            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                return Err(usage_failure(
                    "--from - reads standard input, but it is a terminal: pipe a log in",
                ));
            }
            ("standard input".to_string(), exchange::read(stdin.lock()))
        }
        Some(path) => {
            let p = PathBuf::from(path);
            let f = std::fs::File::open(&p).map_err(|e| open_failed(&p, e))?;
            (p.display().to_string(), exchange::read(BufReader::new(f)))
        }
        None => {
            let root = service::project_root().ok_or_else(|| {
                Failure::new(
                    Code::NO_RECORDING,
                    "no project here, so no giong dev log to read",
                    "run giong record inside the project, or pass --from <log file>",
                )
            })?;
            let log = DevState::read(&root).and_then(|d| d.log).ok_or_else(|| {
                Failure::new(
                    Code::NO_RECORDING,
                    "giong dev is not running here, so there is no log to read",
                    "start the service with giong dev (it keeps its output in .giong/dev.log), \
                     or pass --from <log file>",
                )
            })?;
            let f = std::fs::File::open(&log).map_err(|e| open_failed(&log, e))?;
            (log.display().to_string(), exchange::read(BufReader::new(f)))
        }
    };
    let read = read.map_err(|e| {
        Failure::new(
            Code::NO_RECORDING,
            format!("reading {label} failed: {e}"),
            "check the file, or pass another with --from",
        )
    })?;
    Ok(Source { label, read })
}

fn filter(args: &Args) -> Result<Filter, Failure> {
    let f = Filter {
        route: args.value("route").map(str::to_string),
        method: args.value("method").map(str::to_string),
        status: args.value("status").map(str::to_string),
    };
    f.check().map_err(usage_failure)?;
    Ok(f)
}

/// Findings for exchange lines that could not be read.
fn bad_lines(r: &mut Report, src: &Source) {
    for (n, why) in src.read.bad.iter().take(5) {
        r.push(
            Finding::new(
                Code::RECORDING_INVALID,
                Severity::Warning,
                format!("exchange {n} in {} cannot be read", src.label),
            )
            .detail(why.clone())
            .fix("the line was cut or changed; record the request again"),
        );
    }
}

fn no_exchanges(src: &Source) -> Finding {
    Finding::new(
        Code::NO_RECORDING,
        Severity::Warning,
        format!("{} holds no recorded exchanges", src.label),
    )
    .fix(
        "add giong-web's Record middleware to the router (for example when GIONG_RECORD is \
         set: `router.layer(Record::new(Sink::Stdout))`), restart, and send some requests",
    )
}

fn list(args: &Args, rest: &[String]) -> Result<Report, Failure> {
    if !rest.is_empty() {
        return Err(usage_failure(
            "giong record list takes no words; use the flags",
        ));
    }
    let limit: usize = args.number("limit")?.unwrap_or(50);
    if limit == 0 {
        return Err(usage_failure("--limit must be 1 or more"));
    }
    let src = source(args)?;
    let f = filter(args)?;
    let matching: Vec<&Exchange> = src.read.exchanges.iter().filter(|x| f.matches(x)).collect();
    let shown = &matching[matching.len().saturating_sub(limit)..];
    let mut r = Report::new("record");
    if src.read.total == 0 {
        r.push(no_exchanges(&src));
    } else {
        r.line(format!(
            "{} of {} exchanges in {}{}:",
            shown.len(),
            src.read.total,
            src.label,
            if matching.len() > shown.len() {
                format!(
                    " (the newest; {} match, --limit shows more)",
                    matching.len()
                )
            } else {
                String::new()
            }
        ));
        for x in shown {
            let mark = if x.replayable().is_ok() {
                ""
            } else {
                "  (not replayable)"
            };
            r.line(format!(
                "{:>6}  {:<7} {:<40} {}  {:>8.2} ms{}",
                x.number,
                x.method,
                truncate(&x.target(), 40),
                x.status,
                x.duration_us as f64 / 1000.0,
                mark
            ));
        }
        if !shown.is_empty() {
            r.line("Save some as cases: giong record save <numbers> --name <name>");
        }
    }
    bad_lines(&mut r, &src);
    r.data("source", json!(src.label));
    r.data("total", json!(src.read.total));
    r.data("matching", json!(matching.len()));
    r.data(
        "exchanges",
        Value::Array(shown.iter().map(|x| x.summary()).collect()),
    );
    Ok(r)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 3).collect();
        t.push_str("...");
        t
    }
}

fn show(args: &Args, rest: &[String]) -> Result<Report, Failure> {
    let [word] = rest else {
        return Err(usage_failure("giong record show takes one exchange number"));
    };
    let n: usize = word
        .parse()
        .map_err(|_| usage_failure(format!("{word:?} is not an exchange number")))?;
    let src = source(args)?;
    let x = src
        .read
        .exchanges
        .iter()
        .find(|x| x.number == n)
        .ok_or_else(|| {
            usage_failure(format!(
                "no exchange {n} in {} (giong record list shows the numbers)",
                src.label
            ))
        })?;
    let side = |s: &exchange::Side| {
        json!({
            "headers": s.headers.iter().map(|(n, v)| json!([n, v])).collect::<Vec<_>>(),
            "body": if s.text { json!(String::from_utf8_lossy(&s.body)) } else { Value::Null },
            "body_base64": if s.text { Value::Null } else { json!(crate::record::base64::encode(&s.body)) },
            "body_len": s.body_len,
            "truncated": s.truncated,
            "streamed": s.streamed,
        })
    };
    let mut r = Report::new("record");
    let text_side = |s: &exchange::Side, out: &mut Vec<String>| {
        for (n, v) in &s.headers {
            out.push(format!("{n}: {v}"));
        }
        out.push(String::new());
        if s.streamed {
            out.push("(streamed, not recorded)".into());
        } else if s.text {
            out.push(String::from_utf8_lossy(&s.body).into_owned());
        } else {
            out.push(format!("({} bytes, not text)", s.body.len()));
        }
        if s.truncated {
            out.push(format!(
                "(cut: {} of {} bytes)",
                s.body.len(),
                s.body_len.unwrap_or(0)
            ));
        }
    };
    let mut lines = vec![format!("{} {}", x.method, x.target())];
    text_side(&x.request, &mut lines);
    lines.push(String::new());
    lines.push(format!(
        "=> {} in {:.2} ms (route {}, request id {})",
        x.status,
        x.duration_us as f64 / 1000.0,
        x.route.as_deref().unwrap_or("none"),
        x.request_id.as_deref().unwrap_or("none")
    ));
    text_side(&x.response, &mut lines);
    for l in lines {
        r.line(l);
    }
    let mut summary = x.summary();
    summary["request"] = side(&x.request);
    summary["response"] = side(&x.response);
    r.data("exchange", summary);
    Ok(r)
}

/// The project root, or the current directory outside a project.
fn base_dir() -> PathBuf {
    service::project_root()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn save(args: &Args, rest: &[String]) -> Result<Report, Failure> {
    if args.has("append") && args.has("force") {
        return Err(usage_failure("--append and --force do not go together"));
    }
    if args.value("name").is_some() && args.value("out").is_some() {
        return Err(usage_failure("give --name or --out, not both"));
    }
    let out: PathBuf = match (args.value("out"), args.value("name")) {
        (Some(o), _) => PathBuf::from(o),
        (None, name) => {
            let name = name.unwrap_or("recorded");
            if !valid_file_name(name) {
                return Err(usage_failure(format!(
                    "--name {name:?} is not a file name: use letters, digits, - and _"
                )));
            }
            base_dir()
                .join("tests")
                .join("recorded")
                .join(format!("{}.json", name.trim_end_matches(".json")))
        }
    };
    let src = source(args)?;
    let f = filter(args)?;
    let matching: Vec<Exchange> = src
        .read
        .exchanges
        .iter()
        .filter(|x| f.matches(x))
        .cloned()
        .collect();
    if matching.is_empty() {
        let mut fail = Failure::new(
            Code::NO_RECORDING,
            format!("no recorded exchanges in {} to save", src.label),
            no_exchanges(&src).fix,
        );
        if src.read.total > 0 {
            fail.what = format!(
                "none of the {} exchanges in {} match the filters",
                src.read.total, src.label
            );
            fail.fix = "loosen --route, --method or --status (giong record list shows them)".into();
        }
        return Err(fail);
    }
    let chosen = exchange::select(&matching, rest).map_err(usage_failure)?;

    let mut existing: Vec<Value> = Vec::new();
    let mut names = BTreeSet::new();
    if out.exists() {
        if args.has("append") {
            let text = std::fs::read_to_string(&out).map_err(|e| {
                Failure::new(
                    Code::RECORDING_INVALID,
                    format!("{} cannot be read: {e}", out.display()),
                    "check the file, or save to another with --name",
                )
            })?;
            let cases = case::load(&text).map_err(|e| {
                Failure::new(
                    Code::RECORDING_INVALID,
                    format!("{} cannot be appended to: {e}", out.display()),
                    "fix the file, or save to another with --name",
                )
            })?;
            names = case::names_in(&cases);
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            existing = v["cases"].as_array().cloned().unwrap_or_default();
        } else if !args.has("force") {
            return Err(Failure::new(
                Code::USAGE,
                format!("{} already exists", out.display()),
                "pass --append to add to it, --force to replace it, or --name for another file",
            ));
        }
    }

    let built = case::build(&chosen, &mut names);
    let mut r = Report::new("record");
    for (n, why) in &built.skipped {
        r.push(
            Finding::new(
                Code::RECORDING_INVALID,
                Severity::Warning,
                format!("exchange {n} was not saved"),
            )
            .detail(why.clone())
            .fix("record it again with a larger Record::max_body, or write that case by hand"),
        );
    }
    for (n, note) in &built.notes {
        r.push(
            Finding::new(
                Code::RECORDING_INVALID,
                Severity::Info,
                format!("exchange {n}"),
            )
            .detail(note.clone()),
        );
    }
    if built.cases.is_empty() {
        return Err(Failure::new(
            Code::RECORDING_INVALID,
            "none of the chosen exchanges can be replayed",
            "record them again with a larger Record::max_body (see the findings of giong record list)",
        ));
    }
    let secrets: Vec<String> = built
        .cases
        .iter()
        .flat_map(|c| {
            let name = c["name"].as_str().unwrap_or("?").to_string();
            secret::scan(c)
                .into_iter()
                .map(move |(at, what)| format!("{name}: {at} ({what})"))
        })
        .collect();
    if !secrets.is_empty() {
        if !args.has("keep-secrets") {
            let shown: Vec<&str> = secrets.iter().take(5).map(String::as_str).collect();
            let more = if secrets.len() > 5 {
                format!(" and {} more", secrets.len() - 5)
            } else {
                String::new()
            };
            return Err(Failure::new(
                Code::RECORD_SECRET,
                format!(
                    "nothing saved: {} value{} look{} like a secret: {}{more}",
                    secrets.len(),
                    if secrets.len() == 1 { "" } else { "s" },
                    if secrets.len() == 1 { "s" } else { "" },
                    shown.join("; ")
                ),
                "leave those exchanges out of the numbers given to save, or record them again \
                 with test values; when the values already are test values (a 4242 4242 4242 \
                 4242 test card, a token made for tests), pass --keep-secrets",
            ));
        }
        for s in &secrets {
            r.push(
                Finding::new(
                    Code::RECORD_SECRET,
                    Severity::Warning,
                    "saved a value that looks like a secret (--keep-secrets)",
                )
                .detail(s.clone()),
            );
        }
    }
    let added = built.cases.len();
    existing.extend(built.cases);
    let text = serde_json::to_string_pretty(&case::file_json(&existing)).expect("JSON");
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| {
            Failure::new(
                Code::USAGE,
                format!("cannot make {}: {e}", dir.display()),
                "save somewhere you can write, with --out",
            )
        })?;
    }
    // Write next to it, then rename: an interrupted save never leaves half a file.
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))
        .and_then(|()| std::fs::rename(&tmp, &out))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Failure::new(
                Code::USAGE,
                format!("cannot write {}: {e}", out.display()),
                "save somewhere you can write, with --out",
            )
        })?;
    r.line(format!(
        "Saved {added} case{} to {} ({} in the file).",
        if added == 1 { "" } else { "s" },
        out.display(),
        existing.len()
    ));
    if !built.env.is_empty() {
        r.line(format!(
            "Replaying needs {} (secret values are not saved: set them to the values to send).",
            built.env.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    r.line(format!("Replay: giong record replay {}", out.display()));
    r.data("out", json!(out.display().to_string()));
    r.data("added", json!(added));
    r.data("cases", json!(existing.len()));
    r.data("env", json!(built.env));
    Ok(r)
}

fn case_files(rest: &[String]) -> Result<Vec<PathBuf>, Failure> {
    if !rest.is_empty() {
        return Ok(rest.iter().map(PathBuf::from).collect());
    }
    let dir = base_dir().join("tests").join("recorded");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.is_empty() {
        return Err(Failure::new(
            Code::NO_RECORDING,
            format!("no case files in {}", dir.display()),
            "save some first: giong record list, then giong record save <numbers>",
        ));
    }
    Ok(files)
}

fn replay_files(args: &Args, rest: &[String]) -> Result<Report, Failure> {
    let files = case_files(rest)?;
    let timeout = Duration::from_millis(args.number::<u64>("timeout-ms")?.unwrap_or(10_000).max(1));
    let base_text = routes::base_url(args.value("base-url"));
    let base = Base::parse(&base_text).map_err(|e| {
        Failure::new(
            Code::USAGE,
            e,
            "give --base-url the plain-HTTP address the service listens on, such as http://127.0.0.1:8080",
        )
    })?;
    let env = |k: &str| std::env::var(k).ok();
    let mut r = Report::new("record");
    let (mut passed, mut failed) = (0usize, 0usize);
    let mut files_json = Vec::new();
    for file in &files {
        let shown = file.display().to_string();
        let cases: Vec<Case> = match std::fs::read_to_string(file)
            .map_err(|e| e.to_string())
            .and_then(|t| case::load(&t))
        {
            Ok(c) => c,
            Err(e) => {
                r.push(
                    Finding::new(
                        Code::RECORDING_INVALID,
                        Severity::Error,
                        format!("{shown} cannot be replayed"),
                    )
                    .detail(e)
                    .fix("fix the file (giong explain E5067 shows the format), or save it again")
                    .at(shown.clone(), 1, 1),
                );
                files_json.push(json!({"file": shown, "error": "unreadable", "cases": []}));
                continue;
            }
        };
        let (results, stop) = match replay::run(&cases, &base, timeout, &env) {
            Ok(res) => (res, None),
            Err((res, stop)) => (res, Some(stop)),
        };
        let mut file_failed = 0;
        for res in &results {
            match &res.outcome {
                Outcome::Passed => passed += 1,
                outcome => {
                    failed += 1;
                    file_failed += 1;
                    let finding = match outcome {
                        Outcome::Failed(diffs) => Finding::new(
                            Code::RECORDED_CASE_FAILED,
                            Severity::Error,
                            format!("case {} answers differently", res.name),
                        )
                        .detail(diffs.iter().take(5).cloned().collect::<Vec<_>>().join("; "))
                        .fix(
                            "if the service changed on purpose, save the case again (or edit \
                             the file); if not, the service has a regression",
                        ),
                        Outcome::Missing(m) => {
                            let fix = match m {
                                Missing::Env(v) => format!(
                                    "set {v} to the value the request needs (it was redacted when recorded)"
                                ),
                                Missing::Var(_) => "the earlier answer that should give it did not have the field: fix that case first".into(),
                                Missing::Class(_) => "put a real value in the request; class placeholders are for answers".into(),
                            };
                            Finding::new(
                                Code::RECORD_VALUE_MISSING,
                                Severity::Error,
                                format!("case {} cannot be sent", res.name),
                            )
                            .detail(m.to_string())
                            .fix(fix)
                        }
                        Outcome::Invalid(why) => Finding::new(
                            Code::RECORDING_INVALID,
                            Severity::Error,
                            format!("case {} cannot be sent", res.name),
                        )
                        .detail(why.clone())
                        .fix("fix the request in the file"),
                        Outcome::Passed => unreachable!(),
                    };
                    r.push(finding.at(shown.clone(), res.line as u32, 1));
                }
            }
        }
        r.line(format!(
            "{shown}: {} passed, {file_failed} failed{}",
            results.len() - file_failed,
            if stop.is_some() { ", stopped" } else { "" }
        ));
        files_json.push(json!({
            "file": shown,
            "cases": results.iter().map(|x| x.to_json()).collect::<Vec<_>>(),
        }));
        if let Some(stop) = stop {
            return Err(Failure::new(
                Code::REPLAY_UNREACHABLE,
                format!(
                    "the service at {} does not answer (case {} of {shown}): {}",
                    base.url(),
                    stop.case,
                    stop.why
                ),
                "start the service (giong dev), or give --base-url the address it listens on",
            ));
        }
    }
    r.line(format!(
        "{passed} passed, {failed} failed, against {}.",
        base.url()
    ));
    r.data("base_url", json!(base.url()));
    r.data("passed", json!(passed));
    r.data("failed", json!(failed));
    r.data("files", Value::Array(files_json));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert!(valid_file_name("users"));
        assert!(valid_file_name("users-v2.json"));
        assert!(!valid_file_name("../x"));
        assert!(!valid_file_name(".hidden"));
        assert!(!valid_file_name("a/b"));
        assert!(!valid_file_name(""));
    }

    #[test]
    fn truncation_keeps_characters_whole() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("ééééééé", 5), "éé...");
    }
}
