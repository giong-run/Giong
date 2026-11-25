//! `giong routes`: the HTTP routes a running service serves, read from its
//! giong-mcp `routes` tool, with a curl line for each.

use serde_json::{json, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::report::{Failure, Report};
use crate::service::{self, DevState};

pub const MCP_FLAG: Flag = Flag {
    name: "mcp",
    kind: Kind::Value,
    help: "the service's giong-mcp address (127.0.0.1:7777 or unix:/path); default: GIONG_MCP, \
           then the service giong dev runs here",
};

pub const BASE_FLAG: Flag = Flag {
    name: "base-url",
    kind: Kind::Value,
    help: "where the service answers HTTP, for the curl lines (default: what giong dev listens \
           on, else http://127.0.0.1:8080)",
};

pub const COMMAND: Command = Command {
    name: "routes",
    summary: "list the HTTP routes of the running service, with a curl line each",
    usage: "giong routes [--mcp <address>] [--base-url <url>]\n\n\
            Asks the running service's giong-mcp for the routes of the giong-web apps its \
            workers serve: method, pattern, path parameters, streamed bodies, middleware and \
            the line that added each route. The service needs giong-mcp with the `web` \
            feature. Workers serving different routes are listed separately.",
    flags: &[MCP_FLAG, BASE_FLAG],
    max_words: 0,
    run,
};

/// One route as the tool reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub method: String,
    pub pattern: String,
    pub params: Vec<String>,
    pub catch_all: bool,
    pub stream_limit: Option<u64>,
    pub middleware: u64,
    pub added_at: String,
}

/// The workers that serve an app, and its routes.
pub type App = (Vec<u64>, Vec<Route>);

/// The routes of each distinct app, with the workers that serve it.
pub fn fetch(mcp: Option<&str>) -> Result<Vec<App>, Failure> {
    let svc = service::locate(mcp)?;
    let answer = svc.call("routes", json!({}))?;
    parse(&answer)
}

/// Reads the `routes` tool's answer.
pub fn parse(answer: &Value) -> Result<Vec<App>, Failure> {
    let bad = |what: &str| {
        Failure::new(
            Code::MCP_TOOL_ERROR,
            format!("the routes answer is not what this command expects: {what}"),
            "use giong-mcp and giong-cli of the same version",
        )
    };
    let apps = answer["apps"].as_array().ok_or_else(|| bad("no apps"))?;
    let mut out = Vec::new();
    for app in apps {
        let workers = app["workers"]
            .as_array()
            .ok_or_else(|| bad("no workers"))?
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        let mut routes = Vec::new();
        for r in app["routes"].as_array().ok_or_else(|| bad("no routes"))? {
            let s = |k: &str| r[k].as_str().map(str::to_string).ok_or_else(|| bad(k));
            routes.push(Route {
                method: s("method")?,
                pattern: s("pattern")?,
                params: r["params"]
                    .as_array()
                    .ok_or_else(|| bad("params"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                catch_all: r["catch_all"].as_bool().unwrap_or(false),
                stream_limit: r["stream_limit"].as_u64(),
                middleware: r["middleware"].as_u64().unwrap_or(0),
                added_at: s("added_at")?,
            });
        }
        out.push((workers, routes));
    }
    Ok(out)
}

/// The base URL for curl lines: `--base-url`, else the first address
/// `giong dev` listens on, else `http://127.0.0.1:8080`.
pub fn base_url(flag: Option<&str>) -> String {
    if let Some(b) = flag {
        return b.trim_end_matches('/').to_string();
    }
    service::project_root()
        .and_then(|root| DevState::read(&root))
        .and_then(|d| d.listen.into_iter().next())
        // DevState keeps only loopback host:port entries: a committed
        // dev.json cannot send requests (and replay's env values) elsewhere.
        .map(|addr| format!("http://{addr}"))
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string())
}

/// A path to try: each parameter replaced by a sample value.
pub fn sample_path(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix("{*").and_then(|s| s.strip_suffix('}')) {
                format!("{name}/example")
            } else if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                let lower = name.to_ascii_lowercase();
                if lower == "id" || lower.ends_with("_id") || lower.ends_with("id") {
                    "1".to_string()
                } else {
                    name.to_string()
                }
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A curl line for a route, single-quoted for POSIX shells.
pub fn curl(base: &str, r: &Route) -> String {
    let url = format!("{base}{}", sample_path(&r.pattern));
    let mut line = String::from("curl -i");
    if r.method != "GET" {
        line.push_str(&format!(" -X {}", shell_word(&r.method)));
    }
    if matches!(r.method.as_str(), "POST" | "PUT" | "PATCH") {
        if r.stream_limit.is_some() {
            line.push_str(" --data-binary @file");
        } else {
            line.push_str(" -H 'Content-Type: application/json' -d '{}'");
        }
    }
    line.push_str(&format!(" '{}'", url.replace('\'', "'\\''")));
    line
}

/// `s` as one word for a POSIX shell: as it is when it holds only letters,
/// digits, `-` and `_` (every method people write), else single-quoted. The
/// method comes from the service, which may be someone else's (`--mcp`), and
/// HTTP allows `|`, `&`, `$` and a backquote in one.
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn run(args: &Args) -> Result<Report, Failure> {
    let apps = fetch(args.value("mcp"))?;
    let base = base_url(args.value("base-url"));
    let mut r = Report::new("routes");
    if apps.is_empty() {
        r.line("No giong-web app is alive on any worker.");
    }
    let mut json_apps = Vec::new();
    for (workers, routes) in &apps {
        let list = workers
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        r.line(format!(
            "{} route{} on worker{} {list}:",
            routes.len(),
            if routes.len() == 1 { "" } else { "s" },
            if workers.len() == 1 { "" } else { "s" }
        ));
        let width = routes.iter().map(|x| x.method.len()).max().unwrap_or(0);
        let mut json_routes = Vec::new();
        for route in routes {
            let mut notes = Vec::new();
            if let Some(n) = route.stream_limit {
                notes.push(format!("streamed body up to {n} bytes"));
            }
            notes.push(format!("{} middleware", route.middleware));
            r.line(format!(
                "  {:width$}  {}  ({}; {})",
                route.method,
                route.pattern,
                notes.join(", "),
                route.added_at
            ));
            let c = curl(&base, route);
            r.line(format!("  {:width$}  {c}", ""));
            json_routes.push(json!({
                "method": route.method,
                "pattern": route.pattern,
                "params": route.params,
                "catch_all": route.catch_all,
                "stream_limit": route.stream_limit,
                "middleware": route.middleware,
                "added_at": route.added_at,
                "curl": c,
            }));
        }
        json_apps.push(json!({ "workers": workers, "routes": json_routes }));
    }
    r.data("apps", json!(json_apps));
    r.data("base_url", json!(base));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, pattern: &str) -> Route {
        Route {
            method: method.into(),
            pattern: pattern.into(),
            params: vec![],
            catch_all: false,
            stream_limit: None,
            middleware: 0,
            added_at: "src/main.rs:1:1".into(),
        }
    }

    #[test]
    fn sample_paths_fill_parameters() {
        assert_eq!(sample_path("/users/{id}"), "/users/1");
        assert_eq!(
            sample_path("/teams/{team}/users/{user_id}"),
            "/teams/team/users/1"
        );
        assert_eq!(sample_path("/files/{*path}"), "/files/path/example");
        assert_eq!(sample_path("/"), "/");
        assert_eq!(sample_path("/health"), "/health");
    }

    #[test]
    fn curl_lines_carry_method_body_and_quoting() {
        let b = "http://127.0.0.1:8080";
        assert_eq!(
            curl(b, &route("GET", "/a")),
            "curl -i 'http://127.0.0.1:8080/a'"
        );
        assert_eq!(
            curl(b, &route("POST", "/users")),
            "curl -i -X POST -H 'Content-Type: application/json' -d '{}' 'http://127.0.0.1:8080/users'"
        );
        let mut up = route("PUT", "/files/{*path}");
        up.stream_limit = Some(10);
        assert_eq!(
            curl(b, &up),
            "curl -i -X PUT --data-binary @file 'http://127.0.0.1:8080/files/path/example'"
        );
        assert_eq!(
            curl(b, &route("DELETE", "/it's")),
            "curl -i -X DELETE 'http://127.0.0.1:8080/it'\\''s'"
        );
        // A method is a token, and tokens may hold what a shell runs.
        assert_eq!(
            curl(b, &route("GET|sh", "/a")),
            "curl -i -X 'GET|sh' 'http://127.0.0.1:8080/a'"
        );
        assert_eq!(
            curl(b, &route("X'`id`", "/a")),
            "curl -i -X 'X'\\''`id`' 'http://127.0.0.1:8080/a'"
        );
        assert_eq!(
            curl(b, &route("PURGE", "/a")),
            "curl -i -X PURGE 'http://127.0.0.1:8080/a'"
        );
    }

    #[test]
    fn answers_parse_and_bad_ones_are_refused() {
        let answer = json!({"apps":[{"workers":[0,1],"routes":[{"method":"GET","pattern":"/u/{id}",
            "params":["id"],"catch_all":false,"stream_limit":null,"middleware":2,"added_at":"a:1:2"}]}],
            "unanswered":[]});
        let apps = parse(&answer).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0, [0, 1]);
        assert_eq!(apps[0].1[0].params, ["id"]);
        assert_eq!(apps[0].1[0].middleware, 2);
        assert_eq!(parse(&json!({})).unwrap_err().code, Code::MCP_TOOL_ERROR);
        assert!(parse(&json!({"apps":[{"workers":[0],"routes":[{"method":1}]}]})).is_err());
    }

    #[test]
    fn base_urls_prefer_the_flag() {
        assert_eq!(base_url(Some("http://x:1/")), "http://x:1");
    }
}
