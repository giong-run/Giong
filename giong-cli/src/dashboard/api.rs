//! What the dashboard page asks for: one handler per path.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::http::{Req, Resp};
use super::Target;
use crate::cmd::{explain, openapi, routes, top};
use crate::record::base64;
use crate::report::Failure;
use crate::service::{self, DevState};
use crate::web_client::{self, Base, Outgoing, SendError};

/// The page itself.
pub const PAGE: &str = include_str!("index.html");

/// The most log bytes one `/api/log` answer reads.
const LOG_CHUNK: u64 = 1 << 20;
/// Where a first `/api/log` (no `after`) starts: this far before the end.
const LOG_TAIL: u64 = 256 * 1024;

/// What the page may load: inline script and style, calls to itself.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                   connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

pub fn handle(req: &Req, target: &Target) -> Resp {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", "/") | ("GET" | "HEAD", "/index.html") => {
            let mut r = Resp {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: PAGE.as_bytes().to_vec(),
                headers: Vec::new(),
            };
            r.headers.push(("Content-Security-Policy", CSP.to_string()));
            r
        }
        ("GET", "/api/state") => state(target),
        ("GET", "/api/log") => log(req, target),
        ("GET", "/api/routes") => api_routes(target),
        ("GET", "/api/openapi") => api_openapi(target),
        ("GET", "/api/tasks") => mcp(target, "tasks", json!({"limit": 100})),
        ("GET", "/api/workers") => mcp(target, "workers", json!({})),
        ("GET", "/api/sources") => mcp(target, "sources", json!({})),
        ("POST", "/api/profile") => profile(req, target),
        ("GET", "/api/explain") => api_explain(req),
        ("POST", "/api/send") => send(req, target),
        ("POST", "/api/record/save") => record_save(req, target),
        (_, p) if p.starts_with("/api/") => Resp::error(
            404,
            "GIONG-E5000",
            format!("no API {p}"),
            "reload the page: the dashboard may be newer than it",
        ),
        _ => Resp::text(404, "not found"),
    }
}

fn failure(status: u16, f: &Failure) -> Resp {
    Resp::error(status, &f.code.to_string(), f.what.clone(), f.fix.clone())
}

fn dev_state(target: &Target) -> Option<DevState> {
    target.root.as_deref().and_then(DevState::read)
}

/// `.giong/dev.json` as written, for the fields this command does not model
/// (`build`, `restarts`, `started_at`).
fn dev_raw(target: &Target) -> Option<Value> {
    let root = target.root.as_deref()?;
    let text = crate::state::read_to_string(&root.join(".giong").join("dev.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a process is alive, where that can be told cheaply.
fn alive(pid: u64) -> Option<bool> {
    if cfg!(target_os = "linux") {
        Some(Path::new(&format!("/proc/{pid}")).exists())
    } else {
        None
    }
}

/// The giong-mcp of the service: `--mcp`, `GIONG_MCP`, then `.giong/dev.json`.
fn locate(target: &Target) -> Result<service::Service, Failure> {
    if target.mcp.is_some() {
        return service::locate(target.mcp.as_deref());
    }
    if let Some(dev) = dev_state(target) {
        if dev.pid.and_then(alive) == Some(false) {
            return Err(Failure::new(
                crate::code::Code::NO_SERVICE,
                "giong dev is not running any more (its .giong/dev.json is stale)",
                "start the service with giong dev",
            ));
        }
        if let Some(sock) = dev.mcp_socket {
            return Ok(service::Service {
                endpoint: service::Endpoint::Unix(sock),
                token: dev.mcp_token,
                found_in: ".giong/dev.json".into(),
            });
        }
    }
    service::locate(None)
}

/// Where the service answers HTTP.
fn base_url(target: &Target) -> String {
    if let Some(b) = &target.base_url {
        return b.trim_end_matches('/').to_string();
    }
    // Loopback host:port entries only (DevState drops the rest).
    dev_state(target)
        .and_then(|d| d.listen.into_iter().next())
        .map(|addr| format!("http://{addr}"))
        .unwrap_or_else(|| routes::base_url(None))
}

fn log_path(target: &Target) -> Option<PathBuf> {
    target
        .log
        .clone()
        .or_else(|| dev_state(target).and_then(|d| d.log))
}

fn state(target: &Target) -> Resp {
    let dev = dev_raw(target);
    let dev_alive = dev.as_ref().and_then(|d| d["pid"].as_u64()).and_then(alive);
    let mut out = json!({
        "project": target.root.as_ref().map(|r| r.display().to_string()),
        "dev": dev,
        "dev_alive": dev_alive,
        "base_url": base_url(target),
        "log": log_path(target).map(|p| p.display().to_string()),
        "service": null,
        "service_error": null,
        "overview": null,
        "findings": [],
    });
    match locate(target) {
        Ok(svc) => {
            out["service"] =
                json!({"endpoint": svc.endpoint.to_string(), "found_in": svc.found_in});
            match svc.call("overview", json!({})) {
                Ok(o) => {
                    out["findings"] = json!(top::findings(&o)
                        .iter()
                        .map(|f| f.to_json())
                        .collect::<Vec<_>>());
                    out["overview"] = o;
                }
                Err(f) => {
                    out["service_error"] =
                        json!({"code": f.code.to_string(), "message": f.what, "fix": f.fix})
                }
            }
        }
        Err(f) => {
            out["service_error"] =
                json!({"code": f.code.to_string(), "message": f.what, "fix": f.fix})
        }
    }
    Resp::json(200, &out)
}

fn mcp(target: &Target, tool: &str, args: Value) -> Resp {
    match locate(target).and_then(|s| s.call(tool, args)) {
        Ok(v) => Resp::json(200, &v),
        Err(f) => failure(503, &f),
    }
}

fn profile(req: &Req, target: &Target) -> Resp {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
    let ms = body["duration_ms"]
        .as_u64()
        .unwrap_or(1000)
        .clamp(100, 5000);
    mcp(target, "profile", json!({"duration_ms": ms, "limit": 30}))
}

fn route_json(r: &routes::Route, base: &str) -> Value {
    json!({
        "method": r.method,
        "pattern": r.pattern,
        "params": r.params,
        "catch_all": r.catch_all,
        "stream_limit": r.stream_limit,
        "middleware": r.middleware,
        "added_at": r.added_at,
        "sample_path": routes::sample_path(&r.pattern),
        "curl": routes::curl(base, r),
    })
}

fn fetch_routes(target: &Target) -> Result<Vec<routes::App>, Failure> {
    let svc = locate(target)?;
    routes::parse(&svc.call("routes", json!({}))?)
}

fn api_routes(target: &Target) -> Resp {
    let base = base_url(target);
    match fetch_routes(target) {
        Ok(apps) => Resp::json(
            200,
            &json!({
                "base_url": base,
                "apps": apps.iter().map(|(workers, rs)| json!({
                    "workers": workers,
                    "routes": rs.iter().map(|r| route_json(r, &base)).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }),
        ),
        Err(f) => failure(503, &f),
    }
}

fn api_openapi(target: &Target) -> Resp {
    let base = base_url(target);
    match fetch_routes(target) {
        Ok(apps) => {
            let (name, version) = target
                .root
                .as_deref()
                .and_then(openapi::package_at)
                .unwrap_or_else(|| ("service".into(), "0.0.0".into()));
            let doc = openapi::document(&apps, &name, &version, &base);
            let mut r = Resp::json(200, &doc);
            r.headers.push((
                "Content-Disposition",
                "attachment; filename=\"openapi.json\"".to_string(),
            ));
            r
        }
        Err(f) => failure(503, &f),
    }
}

fn api_explain(req: &Req) -> Resp {
    let word = req.param("code").unwrap_or("");
    match explain::parse_code(word).and_then(|n| explain::page(n).map(|p| (n, p))) {
        Some((n, page)) => Resp::json(
            200,
            &json!({"code": format!("GIONG-E{n:04}"), "page": page}),
        ),
        None => Resp::error(
            404,
            "GIONG-E5005",
            format!("no page for {word:?}"),
            "giong explain lists the codes",
        ),
    }
}

/// Log lines with the byte offset each starts at.
pub type Lines = Vec<(u64, String)>;

/// Reads complete lines of the log from `start`, at most [`LOG_CHUNK`]
/// bytes; gives the lines with their byte offsets and where to go on.
pub fn read_lines(path: &Path, after: Option<u64>) -> std::io::Result<(u64, u64, Lines)> {
    // Never through a link: a repository can commit .giong/dev.log as one.
    let mut f = crate::state::open_read(path)?;
    let len = f.metadata()?.len();
    // Unknown or past the end (giong dev truncated it on a new start):
    // begin with the tail.
    let mut start = match after {
        Some(a) if a <= len => a,
        _ => len.saturating_sub(LOG_TAIL),
    };
    let fresh = after.map_or(true, |a| a > len);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(LOG_CHUNK).read_to_end(&mut buf)?;
    let mut from = 0usize;
    if fresh && start > 0 {
        // Starting mid-file: skip to the first whole line.
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => from = i + 1,
            None => return Ok((start, len, Vec::new())),
        }
    }
    let end = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) if i + 1 >= from => i + 1,
        _ => {
            // No complete line yet; a single line longer than a chunk is
            // passed over so the log can go on.
            if buf.len() as u64 == LOG_CHUNK {
                return Ok((start + LOG_CHUNK, len, Vec::new()));
            }
            return Ok((start + from as u64, len, Vec::new()));
        }
    };
    let mut lines = Vec::new();
    let mut at = from;
    for line in buf[from..end].split(|&b| b == b'\n') {
        let off = start + at as u64;
        at += line.len() + 1;
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        if !text.trim().is_empty() {
            lines.push((off, text.to_string()));
        }
    }
    start += end as u64;
    Ok((start, len, lines))
}

/// A log line as the page shows it.
pub fn entry(at: u64, line: &str) -> Value {
    let trimmed = line.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if v.is_object() {
                let kind = match v["event"].as_str() {
                    Some("request") | Some("request cancelled") => "request",
                    Some("exchange") => "exchange",
                    Some("error") => "error",
                    _ => "log",
                };
                return json!({"at": at, "kind": kind, "data": v});
            }
        }
    }
    json!({"at": at, "kind": "text", "text": line})
}

fn log(req: &Req, target: &Target) -> Resp {
    let Some(path) = log_path(target) else {
        return Resp::json(200, &json!({"log": null, "offset": 0, "entries": []}));
    };
    let after = req.param("after").and_then(|a| a.parse::<u64>().ok());
    match read_lines(&path, after) {
        Ok((offset, len, lines)) => Resp::json(
            200,
            &json!({
                "log": path.display().to_string(),
                "offset": offset,
                "size": len,
                "more": offset < len,
                "entries": lines.iter().map(|(at, l)| entry(*at, l)).collect::<Vec<_>>(),
            }),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Resp::json(
            200,
            &json!({"log": path.display().to_string(), "offset": 0, "entries": [], "missing": true}),
        ),
        Err(e) => Resp::error(
            500,
            "GIONG-E5054",
            format!("{} cannot be read: {e}", path.display()),
            "check the file's permissions",
        ),
    }
}

fn send(req: &Req, target: &Target) -> Resp {
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
        return Resp::error(
            400,
            "GIONG-E5000",
            "the request to send is not JSON",
            "reload the page",
        );
    };
    let base_text = body["base_url"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| base_url(target));
    let base = match Base::parse(&base_text) {
        Ok(b) => b,
        Err(e) => {
            return Resp::error(
                400,
                "GIONG-E5000",
                e,
                "give the plain-HTTP address the service listens on",
            )
        }
    };
    // Only loopback: the box is for the service on this machine, not a proxy.
    let host = base.authority.rsplit_once(':').map_or("", |(h, _)| h);
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") && !host.starts_with("127.") {
        return Resp::error(
            400,
            "GIONG-E5000",
            format!("{} is not on this machine", base.url()),
            "the dashboard sends requests only to the service on 127.0.0.1",
        );
    }
    let headers = body["headers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| Some((h[0].as_str()?.to_string(), h[1].as_str()?.to_string())))
                .filter(|(n, _)| !n.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let out = Outgoing {
        method: body["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase(),
        target: body["path"].as_str().unwrap_or("/").to_string(),
        headers,
        body: body["body"].as_str().unwrap_or("").as_bytes().to_vec(),
    };
    match web_client::send(&base, &out, Duration::from_secs(30)) {
        Ok(a) => {
            let text = std::str::from_utf8(&a.body).ok();
            Resp::json(
                200,
                &json!({
                    "status": a.status,
                    "headers": a.headers.iter().map(|(n, v)| json!([n, v])).collect::<Vec<_>>(),
                    "body": text,
                    "body_base64": if text.is_none() { Some(base64::encode(&a.body)) } else { None },
                    "body_len": a.body.len(),
                    "elapsed_ms": a.elapsed.as_secs_f64() * 1000.0,
                }),
            )
        }
        Err(SendError::Invalid(why)) => {
            Resp::error(400, "GIONG-E5000", why, "fix the method, path or headers")
        }
        Err(SendError::Connect(why)) | Err(SendError::Answer(why)) => Resp::error(
            502,
            "GIONG-E5069",
            why,
            "start the service (giong dev), or check the address it listens on",
        ),
    }
}

/// The exchange number of the `exchange` line at byte `at` of the log.
pub fn exchange_number(path: &Path, at: u64) -> std::io::Result<Option<usize>> {
    use std::io::BufRead;
    let f = crate::state::open_read(path)?;
    let mut r = std::io::BufReader::new(f);
    let mut offset = 0u64;
    let mut count = 0usize;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 || offset > at {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&line);
        let is_exchange = matches!(
            crate::record::exchange::parse_line(&text, 0),
            Ok(Some(_)) | Err(_)
        );
        if is_exchange {
            count += 1;
            if offset == at {
                return Ok(Some(count));
            }
        } else if offset == at {
            return Ok(None);
        }
        offset += n as u64;
    }
}

fn record_save(req: &Req, target: &Target) -> Resp {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
    let Some(at) = body["at"].as_u64() else {
        return Resp::error(
            400,
            "GIONG-E5000",
            "which exchange? (no `at`)",
            "reload the page",
        );
    };
    let name = body["name"].as_str().unwrap_or("dashboard");
    let Some(log) = log_path(target) else {
        return Resp::error(
            404,
            "GIONG-E5065",
            "no log to save from",
            "run the service with giong dev",
        );
    };
    let number = match exchange_number(&log, at) {
        Ok(Some(n)) => n,
        Ok(None) => {
            return Resp::error(
                404,
                "GIONG-E5065",
                "that exchange is not in the log any more",
                "giong dev starts a new log on each start; record it again",
            )
        }
        Err(e) => {
            return Resp::error(
                500,
                "GIONG-E5054",
                format!("{} cannot be read: {e}", log.display()),
                "check the file",
            )
        }
    };
    let Some(root) = target.root.clone() else {
        return Resp::error(
            400,
            "GIONG-E5001",
            "no project to save the case in",
            "run giong dashboard inside the project",
        );
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return Resp::error(
                500,
                "GIONG-E5054",
                format!("cannot find the giong command: {e}"),
                "run giong record save by hand",
            )
        }
    };
    let file = root
        .join("tests")
        .join("recorded")
        .join(format!("{name}.json"));
    let mut cmd = std::process::Command::new(exe);
    cmd.current_dir(&root)
        .args(["record", "save", &number.to_string(), "--from"])
        .arg(&log)
        .args(["--name", name, "--json"]);
    if file.exists() {
        cmd.arg("--append");
    }
    match cmd.output() {
        Ok(out) => match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) => Resp::json(if out.status.success() { 200 } else { 400 }, &v),
            Err(_) => Resp::error(
                500,
                "GIONG-E5054",
                "giong record save gave no report",
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ),
        },
        Err(e) => Resp::error(
            500,
            "GIONG-E5054",
            format!("giong record save did not run: {e}"),
            "run it by hand",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giong-dash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn log_lines_come_whole_with_offsets() {
        let d = temp("log");
        let p = d.join("dev.log");
        std::fs::write(&p, "one\n{\"event\":\"request\",\"status\":200}\npartial").unwrap();
        let (off, len, lines) = read_lines(&p, Some(0)).unwrap();
        assert_eq!(len, 44);
        assert_eq!(off, 37);
        assert_eq!(
            lines,
            vec![
                (0, "one".to_string()),
                (4, "{\"event\":\"request\",\"status\":200}".to_string())
            ]
        );
        // Nothing new until the line ends.
        let (off2, _, lines) = read_lines(&p, Some(off)).unwrap();
        assert_eq!((off2, lines.len()), (37, 0));
        std::fs::write(
            &p,
            "one\n{\"event\":\"request\",\"status\":200}\npartial line\n",
        )
        .unwrap();
        let (_, _, lines) = read_lines(&p, Some(off)).unwrap();
        assert_eq!(lines, vec![(37, "partial line".to_string())]);
        // Truncated (a new giong dev run): the offset is past the end.
        std::fs::write(&p, "new\n").unwrap();
        let (off, _, lines) = read_lines(&p, Some(500)).unwrap();
        assert_eq!((off, lines), (4, vec![(0, "new".to_string())]));
    }

    #[test]
    fn a_first_read_starts_with_the_tail_on_a_line_boundary() {
        let d = temp("tail");
        let p = d.join("dev.log");
        let mut text = String::new();
        for i in 0..40_000 {
            text.push_str(&format!("line {i:06}\n"));
        }
        std::fs::write(&p, &text).unwrap();
        let (off, len, lines) = read_lines(&p, None).unwrap();
        assert_eq!(off, len);
        assert!(
            lines.len() < 40_000 && lines.len() > 20_000,
            "{}",
            lines.len()
        );
        assert!(lines[0].1.starts_with("line "), "{:?}", lines[0]);
        assert_eq!(lines.last().unwrap().1, "line 039999");
        // Each offset points at its line.
        let (at, l) = &lines[0];
        assert_eq!(&text[*at as usize..*at as usize + l.len()], l);
    }

    #[test]
    fn an_endless_line_does_not_stall_the_log() {
        let d = temp("long");
        let p = d.join("dev.log");
        std::fs::write(&p, "x".repeat(LOG_CHUNK as usize + 10)).unwrap();
        let (off, _, lines) = read_lines(&p, Some(0)).unwrap();
        assert_eq!(off, LOG_CHUNK);
        assert!(lines.is_empty());
    }

    #[test]
    fn entries_are_classified() {
        assert_eq!(
            entry(0, r#"{"event":"request","status":200}"#)["kind"],
            "request"
        );
        assert_eq!(
            entry(0, r#"{"event":"request cancelled"}"#)["kind"],
            "request"
        );
        assert_eq!(entry(0, r#"{"event":"exchange"}"#)["kind"], "exchange");
        assert_eq!(entry(0, r#"{"event":"error"}"#)["kind"], "error");
        assert_eq!(entry(0, r#"{"msg":"hi"}"#)["kind"], "log");
        assert_eq!(entry(0, "plain {")["kind"], "text");
        assert_eq!(entry(0, "[1,2]")["kind"], "text");
    }

    #[test]
    fn exchange_numbers_follow_the_log() {
        let d = temp("num");
        let p = d.join("dev.log");
        let ex = r#"{"event":"exchange","method":"GET","path":"/","status":200,"request":{"headers":[],"body":""},"response":{"headers":[],"body":""}}"#;
        let text = format!("hello\n{ex}\n{{\"event\":\"request\"}}\n{ex}\n");
        std::fs::write(&p, &text).unwrap();
        let second = text.rfind(ex).unwrap() as u64;
        assert_eq!(exchange_number(&p, 6).unwrap(), Some(1));
        assert_eq!(exchange_number(&p, second).unwrap(), Some(2));
        assert_eq!(exchange_number(&p, 0).unwrap(), None);
        assert_eq!(exchange_number(&p, 3).unwrap(), None);
        assert_eq!(exchange_number(&p, 99_999).unwrap(), None);
    }

    #[test]
    fn the_send_box_reaches_only_this_machine() {
        let req = Req {
            method: "POST".into(),
            path: "/api/send".into(),
            body: br#"{"base_url":"http://10.0.0.1:80","method":"GET","path":"/"}"#.to_vec(),
            ..Req::default()
        };
        let r = send(&req, &Target::default());
        assert_eq!(r.status, 400);
        assert!(String::from_utf8_lossy(&r.body).contains("not on this machine"));
    }

    #[test]
    fn the_page_quotes_the_method_of_a_copied_curl_line() {
        // A recorded method comes from any client; `GET|sh` must not reach
        // a shell as a pipe when the line is pasted.
        let uses: Vec<&str> = PAGE
            .match_indices("\" -X \" + ")
            .map(|(i, _)| &PAGE[i..])
            .collect();
        assert!(
            !uses.is_empty(),
            "curlFor no longer sets the method with -X"
        );
        for rest in uses {
            assert!(
                rest.starts_with("\" -X \" + shWord("),
                "{}",
                &rest[..40.min(rest.len())]
            );
        }
        assert!(PAGE.contains("/^[A-Za-z0-9_-]+$/.test(s) ? s : shQuote(s)"));
    }

    #[test]
    fn the_page_is_served_with_a_policy() {
        let req = Req {
            method: "GET".into(),
            path: "/".into(),
            ..Req::default()
        };
        let r = handle(&req, &Target::default());
        assert_eq!(r.status, 200);
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| *k == "Content-Security-Policy" && v.contains("frame-ancestors 'none'")));
        assert!(String::from_utf8_lossy(&r.body).contains("<title>"));
    }
}
