//! Reading the `exchange` lines giong-web's `Record` middleware writes.
//!
//! A log holds other lines too (giong-web's `Logger`, the service's own
//! output, panics): only JSON objects with `"event":"exchange"` count. Each
//! exchange keeps its number in the log (1 for the first exchange), which is
//! what `giong record list` shows and `giong record save` takes.

use std::collections::VecDeque;
use std::io::BufRead;

use serde_json::{json, Value};

use super::base64;

/// The most exchanges kept from one log: the newest win.
pub const KEEP: usize = 10_000;

/// One side of an exchange.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    /// In order, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The bytes recorded (at most `Record::max_body`).
    pub body: Vec<u8>,
    /// The body was recorded as text (`body`), not `body_base64`.
    pub text: bool,
    /// The whole body's length; `None` when streamed.
    pub body_len: Option<u64>,
    pub truncated: bool,
    pub streamed: bool,
}

impl Side {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// One recorded request and its answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exchange {
    /// Its number among the log's exchanges, from 1.
    pub number: usize,
    pub ts: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    /// The route pattern that answered, if one did.
    pub route: Option<String>,
    pub status: u16,
    pub duration_us: u64,
    pub request_id: Option<String>,
    pub request: Side,
    pub response: Side,
}

impl Exchange {
    /// `/users?x=1`.
    pub fn target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    /// A short summary for lists, as JSON.
    pub fn summary(&self) -> Value {
        json!({
            "number": self.number,
            "ts": self.ts,
            "method": self.method,
            "target": self.target(),
            "route": self.route,
            "status": self.status,
            "duration_us": self.duration_us,
            "request_id": self.request_id,
            "request_bytes": self.request.body_len,
            "response_bytes": self.response.body_len,
            "replayable": self.replayable().err(),
        })
    }

    /// `Ok` when a case can be made from it, else why not.
    pub fn replayable(&self) -> Result<(), String> {
        if self.request.streamed {
            return Err("the request body was streamed and not recorded".into());
        }
        if self.request.truncated {
            return Err(format!(
                "the request body was cut at {} of {} bytes (raise Record::max_body)",
                self.request.body.len(),
                self.request.body_len.unwrap_or(0)
            ));
        }
        Ok(())
    }
}

/// Why a line that says it is an exchange cannot be read.
fn side(v: &Value, what: &str) -> Result<Side, String> {
    if !v.is_object() {
        return Err(format!("`{what}` is not an object"));
    }
    let mut headers = Vec::new();
    for h in v["headers"]
        .as_array()
        .ok_or(format!("`{what}.headers` is not a list"))?
    {
        match h.as_array().map(Vec::as_slice) {
            Some([Value::String(n), Value::String(val)]) => {
                headers.push((n.to_ascii_lowercase(), val.clone()))
            }
            _ => return Err(format!("`{what}.headers` holds {h}, not [name, value]")),
        }
    }
    let streamed = v["streamed"].as_bool().unwrap_or(false);
    let (body, text) = if let Some(t) = v["body"].as_str() {
        (t.as_bytes().to_vec(), true)
    } else if let Some(b) = v["body_base64"].as_str() {
        (
            base64::decode(b).ok_or(format!("`{what}.body_base64` is not base64"))?,
            false,
        )
    } else if streamed {
        (Vec::new(), true)
    } else {
        return Err(format!("`{what}` has neither `body` nor `body_base64`"));
    };
    Ok(Side {
        headers,
        body,
        text,
        body_len: v["body_len"].as_u64(),
        truncated: v["truncated"].as_bool().unwrap_or(false),
        streamed,
    })
}

/// Reads one line: `Ok(None)` when it is not an exchange at all, `Err`
/// when it says it is one but is malformed.
pub fn parse_line(line: &str, number: usize) -> Result<Option<Exchange>, String> {
    let line = line.trim();
    // Cheap test first: most lines of a busy log are access log lines.
    if !line.starts_with('{') || !line.contains("\"exchange\"") {
        return Ok(None);
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    if v["event"] != "exchange" {
        return Ok(None);
    }
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let status = v["status"]
        .as_u64()
        .filter(|s| (100..1000).contains(s))
        .ok_or("`status` is not an HTTP status")? as u16;
    Ok(Some(Exchange {
        number,
        ts: s("ts").unwrap_or_default(),
        method: s("method")
            .filter(|m| !m.is_empty())
            .ok_or("`method` is missing")?,
        path: s("path")
            .filter(|p| p.starts_with('/') || p == "*")
            .ok_or("`path` is missing or does not start with /")?,
        query: s("query"),
        route: s("route"),
        status,
        duration_us: v["duration_us"].as_u64().unwrap_or(0),
        request_id: s("request_id"),
        request: side(&v["request"], "request")?,
        response: side(&v["response"], "response")?,
    }))
}

/// What reading a log gave.
#[derive(Debug, Default)]
pub struct Read {
    /// The newest [`KEEP`] exchanges, oldest first.
    pub exchanges: Vec<Exchange>,
    /// How many exchanges the log holds in all.
    pub total: usize,
    /// Lines that said they were exchanges but could not be read:
    /// (exchange number, why).
    pub bad: Vec<(usize, String)>,
}

/// Reads every exchange of a log.
pub fn read(reader: impl BufRead) -> std::io::Result<Read> {
    let mut kept = VecDeque::new();
    let mut out = Read::default();
    let mut buf = Vec::new();
    let mut reader = reader;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        // A service may print anything: never fail on bytes that are not UTF-8.
        let line = String::from_utf8_lossy(&buf);
        match parse_line(&line, out.total + 1) {
            Ok(None) => {}
            Ok(Some(x)) => {
                out.total += 1;
                if kept.len() == KEEP {
                    kept.pop_front();
                }
                kept.push_back(x);
            }
            Err(why) => {
                out.total += 1;
                if out.bad.len() < 100 {
                    out.bad.push((out.total, why));
                }
            }
        }
    }
    out.exchanges = kept.into();
    Ok(out)
}

/// Which exchanges a command means: `--route`, `--method`, `--status`.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// A substring of the route pattern or of the path.
    pub route: Option<String>,
    pub method: Option<String>,
    /// `404`, or a class such as `4xx`.
    pub status: Option<String>,
}

impl Filter {
    /// Checks the status filter's spelling.
    pub fn check(&self) -> Result<(), String> {
        if let Some(s) = &self.status {
            let ok = match s.as_bytes() {
                [c, b'x' | b'X', b'x' | b'X'] => (b'1'..=b'5').contains(c),
                _ => s.parse::<u16>().is_ok_and(|n| (100..1000).contains(&n)),
            };
            if !ok {
                return Err(format!("--status {s:?} is not a status such as 404 or 4xx"));
            }
        }
        Ok(())
    }

    pub fn matches(&self, x: &Exchange) -> bool {
        if let Some(r) = &self.route {
            let in_route = x.route.as_deref().is_some_and(|p| p.contains(r.as_str()));
            if !in_route && !x.path.contains(r.as_str()) {
                return false;
            }
        }
        if let Some(m) = &self.method {
            if !x.method.eq_ignore_ascii_case(m) {
                return false;
            }
        }
        if let Some(s) = &self.status {
            let b = s.as_bytes();
            if b.len() == 3 && (b[1] | 0x20) == b'x' {
                if x.status / 100 != u16::from(b[0] - b'0') {
                    return false;
                }
            } else if s.parse::<u16>().ok() != Some(x.status) {
                return false;
            }
        }
        true
    }
}

/// Picks exchanges by the words given to `giong record save`: numbers
/// (`3`), ranges (`3-7`), `last` (the newest) and `all`. Each exchange at
/// most once, in log order.
pub fn select<'a>(
    exchanges: &'a [Exchange],
    words: &[String],
) -> Result<Vec<&'a Exchange>, String> {
    if words.is_empty() {
        return Err(
            "name the exchanges to save: numbers from giong record list, \
                    ranges such as 3-7, last, or all"
                .into(),
        );
    }
    let mut wanted = std::collections::BTreeSet::new();
    for w in words {
        match w.as_str() {
            "all" => {
                wanted.extend(exchanges.iter().map(|x| x.number));
            }
            "last" => {
                if let Some(x) = exchanges.last() {
                    wanted.insert(x.number);
                }
            }
            _ => {
                let range = match w.split_once('-') {
                    Some((a, b)) => a.parse::<usize>().ok().zip(b.parse::<usize>().ok()),
                    None => w.parse::<usize>().ok().map(|n| (n, n)),
                };
                let Some((a, b)) = range.filter(|(a, b)| a <= b && *a > 0) else {
                    return Err(format!(
                        "{w:?} is not an exchange number, a range such as 3-7, last or all"
                    ));
                };
                if b - a > 1_000_000 {
                    return Err(format!("the range {w} is too long"));
                }
                let mut found = false;
                for x in exchanges.iter().filter(|x| (a..=b).contains(&x.number)) {
                    wanted.insert(x.number);
                    found = true;
                }
                if !found {
                    return Err(format!(
                        "no exchange {w} among those listed (giong record list shows the numbers)"
                    ));
                }
            }
        }
    }
    Ok(exchanges
        .iter()
        .filter(|x| wanted.contains(&x.number))
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An exchange line as `Record` writes it.
    pub fn line(method: &str, path: &str, status: u16, req_body: &str, resp_body: &str) -> String {
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, Value::String(q.into())),
            None => (path, Value::Null),
        };
        json!({
            "ts": "2026-11-22T08:40:01.123456Z", "level": "info", "event": "exchange",
            "method": method, "path": path, "query": query, "route": path, "status": status,
            "duration_us": 153, "request_id": "5f3a9c2e8b41d706000100000000002a", "worker": 0,
            "request": {"headers": [["content-type", "application/json"], ["authorization", "[redacted]"]],
                        "body": req_body, "body_len": req_body.len(), "truncated": false, "streamed": false},
            "response": {"headers": [["content-type", "application/json"]],
                         "body": resp_body, "body_len": resp_body.len(), "truncated": false, "streamed": false},
        })
        .to_string()
    }

    #[test]
    fn a_log_mixes_exchanges_with_other_lines() {
        let log = format!(
            "listening on 127.0.0.1:8080\n\
             {{\"event\":\"request\",\"status\":200}}\n\
             {}\n\
             not json {{\"exchange\"\n\
             {{\"event\":\"exchange\",\"status\":\"x\"}}\n\
             \u{fffd}\u{0}garbage\n\
             {}\n",
            line("POST", "/users", 201, "{\"name\":\"ada\"}", "{\"id\":7}"),
            line("GET", "/users/7?full=1", 200, "", "{\"id\":7}"),
        );
        let r = read(log.as_bytes()).unwrap();
        assert_eq!(r.total, 3);
        assert_eq!(r.exchanges.len(), 2);
        assert_eq!(r.bad.len(), 1);
        assert_eq!(r.bad[0].0, 2);
        let (a, b) = (&r.exchanges[0], &r.exchanges[1]);
        assert_eq!((a.number, b.number), (1, 3));
        assert_eq!(a.request.body, b"{\"name\":\"ada\"}");
        assert_eq!(a.request.header("authorization"), Some("[redacted]"));
        assert_eq!(b.target(), "/users/7?full=1");
        assert_eq!(b.query.as_deref(), Some("full=1"));
    }

    #[test]
    fn invalid_utf8_in_a_log_does_not_stop_reading() {
        let mut log = b"\xff\xfe junk\n".to_vec();
        log.extend_from_slice(line("GET", "/", 200, "", "ok").as_bytes());
        let r = read(&log[..]).unwrap();
        assert_eq!(r.exchanges.len(), 1);
    }

    #[test]
    fn base64_bodies_and_streams_are_read() {
        let mut v: Value = serde_json::from_str(&line("POST", "/f", 200, "", "")).unwrap();
        v["request"]["body"] = Value::Null;
        v["request"].as_object_mut().unwrap().remove("body");
        v["request"]["body_base64"] = json!("AP8=");
        v["response"] =
            json!({"headers": [], "body_len": null, "truncated": false, "streamed": true});
        let x = parse_line(&v.to_string(), 1).unwrap().unwrap();
        assert_eq!(x.request.body, vec![0, 255]);
        assert!(!x.request.text);
        assert!(x.response.streamed);
        assert!(x.replayable().is_ok());
        v["request"]["body_base64"] = json!("!!");
        assert!(parse_line(&v.to_string(), 1).is_err());
    }

    #[test]
    fn truncated_or_streamed_requests_are_not_replayable() {
        let mut v: Value = serde_json::from_str(&line("POST", "/f", 200, "abc", "")).unwrap();
        v["request"]["truncated"] = json!(true);
        v["request"]["body_len"] = json!(9);
        let x = parse_line(&v.to_string(), 1).unwrap().unwrap();
        assert!(x.replayable().unwrap_err().contains("3 of 9"));
        v["request"]["truncated"] = json!(false);
        v["request"]["streamed"] = json!(true);
        let x = parse_line(&v.to_string(), 1).unwrap().unwrap();
        assert!(x.replayable().is_err());
    }

    #[test]
    fn only_the_newest_are_kept() {
        let mut log = String::new();
        for _ in 0..KEEP + 5 {
            log.push_str(&line("GET", "/", 200, "", ""));
            log.push('\n');
        }
        let r = read(log.as_bytes()).unwrap();
        assert_eq!(r.total, KEEP + 5);
        assert_eq!(r.exchanges.len(), KEEP);
        assert_eq!(r.exchanges[0].number, 6);
    }

    fn some() -> Vec<Exchange> {
        [
            ("GET", "/users", 200),
            ("POST", "/users", 201),
            ("GET", "/users/7", 404),
            ("DELETE", "/orders/1", 204),
        ]
        .iter()
        .enumerate()
        .map(|(i, (m, p, s))| Exchange {
            number: i + 1,
            method: m.to_string(),
            path: p.to_string(),
            route: Some(p.to_string()),
            status: *s,
            ..Exchange::default()
        })
        .collect()
    }

    #[test]
    fn filters() {
        let xs = some();
        let count = |f: Filter| xs.iter().filter(|x| f.matches(x)).count();
        assert_eq!(count(Filter::default()), 4);
        assert_eq!(
            count(Filter {
                route: Some("users".into()),
                ..Filter::default()
            }),
            3
        );
        assert_eq!(
            count(Filter {
                method: Some("get".into()),
                ..Filter::default()
            }),
            2
        );
        assert_eq!(
            count(Filter {
                status: Some("2xx".into()),
                ..Filter::default()
            }),
            3
        );
        assert_eq!(
            count(Filter {
                status: Some("404".into()),
                ..Filter::default()
            }),
            1
        );
        for bad in ["6xx", "x", "99", "4x"] {
            assert!(
                Filter {
                    status: Some(bad.into()),
                    ..Filter::default()
                }
                .check()
                .is_err(),
                "{bad}"
            );
        }
        assert!(Filter {
            status: Some("4XX".into()),
            ..Filter::default()
        }
        .check()
        .is_ok());
    }

    #[test]
    fn selections() {
        let xs = some();
        let nums = |w: &[&str]| -> Result<Vec<usize>, String> {
            let words: Vec<String> = w.iter().map(|s| s.to_string()).collect();
            select(&xs, &words).map(|v| v.iter().map(|x| x.number).collect())
        };
        assert_eq!(nums(&["all"]).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(nums(&["last"]).unwrap(), vec![4]);
        assert_eq!(nums(&["3", "1", "3"]).unwrap(), vec![1, 3]);
        assert_eq!(nums(&["2-3"]).unwrap(), vec![2, 3]);
        assert!(nums(&[]).is_err());
        assert!(nums(&["9"]).is_err());
        assert!(nums(&["3-2"]).is_err());
        assert!(nums(&["0"]).is_err());
        assert!(nums(&["x"]).is_err());
        assert!(nums(&["1-99999999999"]).is_err());
    }
}
