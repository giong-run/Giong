//! Cases: what `giong record save` writes and `giong record replay` reads.
//!
//! A case file (`tests/recorded/<name>.json`) is JSON a person or an agent
//! can read and edit:
//!
//! ```json
//! {
//!   "giong_record": 1,
//!   "about": "...",
//!   "cases": [
//!     {
//!       "name": "post_users_201",
//!       "from": {"ts": "2026-11-22T08:40:01.123456Z", "route": "/users", "request_id": "..."},
//!       "request": {"method": "POST", "path": "/users", "query": null,
//!                   "headers": [["content-type", "application/json"]],
//!                   "body_json": {"name": "ada"}},
//!       "expect": {"status": 201,
//!                  "headers": {"content-type": "application/json"},
//!                  "body_json": {"id": "{{capture:user_id}}", "name": "ada", "created_at": "{{time}}"}}
//!     },
//!     { "name": "get_users_by_id_200",
//!       "request": {"method": "GET", "path": "/users/{{user_id}}", ...}, ... }
//!   ]
//! }
//! ```
//!
//! Bodies are `body_json` (JSON, compared field by field), `body` (text,
//! compared with placeholders), `body_base64` (bytes, compared exactly) or,
//! in an expectation, `body_unchecked` (a reason; the body is not compared).
//! Placeholders are described in [`super::matcher`].
//!
//! Saving masks what changes from run to run: dates and times, UUIDs, long
//! hex ids, tokens, and ids the service made. An id the service made that a
//! later recorded request uses (in its path, query or body) becomes a
//! capture, so the replay sends the id the service makes this time.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use super::exchange::Exchange;
use super::matcher::{self, as_text, Ph};
use super::secret::{self, is_secret_name};
use super::{base64, shape};

/// The version of the case file format.
pub const FORMAT: u64 = 1;

/// Request headers not kept: the client writes them, or they differ every
/// time and the service does not read them.
const DROPPED_REQUEST_HEADERS: [&str; 15] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "upgrade",
    "expect",
    "accept-encoding",
    "user-agent",
    "x-request-id",
    "traceparent",
    "tracestate",
    "postman-token",
    "if-none-match",
];

/// What `Record` writes for a redacted header value.
const REDACTED: &str = "[redacted]";

/// The body of a request or an expectation.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    None,
    Text(String),
    Json(Value),
    Bytes(Vec<u8>),
    /// Expectations only: not compared, for this reason.
    Unchecked(String),
}

impl Body {
    fn write(&self, obj: &mut Map<String, Value>) {
        match self {
            Body::None => {}
            Body::Text(t) => {
                obj.insert("body".into(), json!(t));
            }
            Body::Json(v) => {
                obj.insert("body_json".into(), v.clone());
            }
            Body::Bytes(b) => {
                obj.insert("body_base64".into(), json!(base64::encode(b)));
            }
            Body::Unchecked(why) => {
                obj.insert("body_unchecked".into(), json!(why));
            }
        }
    }

    fn read(obj: &Value, expectation: bool) -> Result<Body, String> {
        let mut found = Vec::new();
        for k in ["body", "body_json", "body_base64", "body_unchecked"] {
            if obj.get(k).is_some() {
                found.push(k);
            }
        }
        if found.len() > 1 {
            return Err(format!("has both {} and {}", found[0], found[1]));
        }
        Ok(match found.first().copied() {
            None => Body::None,
            Some("body") => Body::Text(
                obj["body"]
                    .as_str()
                    .ok_or("`body` is not a string")?
                    .to_string(),
            ),
            Some("body_json") => Body::Json(obj["body_json"].clone()),
            Some("body_base64") => Body::Bytes(
                obj["body_base64"]
                    .as_str()
                    .and_then(base64::decode)
                    .ok_or("`body_base64` is not base64")?,
            ),
            Some(_) => {
                if !expectation {
                    return Err("`body_unchecked` is for expectations only".into());
                }
                Body::Unchecked(
                    obj["body_unchecked"]
                        .as_str()
                        .ok_or("`body_unchecked` is not a string")?
                        .to_string(),
                )
            }
        })
    }
}

/// A request to replay.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// What the answer must be.
#[derive(Clone, Debug, PartialEq)]
pub struct Expect {
    pub status: u16,
    /// Lower-case name to a template (placeholders allowed).
    pub headers: BTreeMap<String, String>,
    pub body: Body,
}

/// One case.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub name: String,
    pub request: Request,
    pub expect: Expect,
    /// Where the case starts in its file (1-based), for messages.
    pub line: usize,
}

impl Case {
    pub fn to_json(&self, from: Option<Value>) -> Value {
        let mut req = Map::new();
        req.insert("method".into(), json!(self.request.method));
        req.insert("path".into(), json!(self.request.path));
        req.insert("query".into(), json!(self.request.query));
        req.insert(
            "headers".into(),
            Value::Array(
                self.request
                    .headers
                    .iter()
                    .map(|(n, v)| json!([n, v]))
                    .collect(),
            ),
        );
        self.request.body.write(&mut req);
        let mut exp = Map::new();
        exp.insert("status".into(), json!(self.expect.status));
        exp.insert("headers".into(), json!(self.expect.headers));
        self.expect.body.write(&mut exp);
        let mut case = Map::new();
        case.insert("name".into(), json!(self.name));
        if let Some(f) = from {
            case.insert("from".into(), f);
        }
        case.insert("request".into(), Value::Object(req));
        case.insert("expect".into(), Value::Object(exp));
        Value::Object(case)
    }
}

/// A whole file's JSON.
pub fn file_json(cases: &[Value]) -> Value {
    json!({
        "giong_record": FORMAT,
        "about": "Recorded requests and the answers expected, written by `giong record save` \
                  and replayed in order by `giong record replay`. Edit freely. Placeholders: \
                  {{any}} {{string}} {{number}} {{uuid}} {{time}} {{hex}}; {{capture:NAME}} \
                  remembers a value and {{NAME}} uses it later; {{env:VAR}} in a request reads \
                  an environment variable. See `giong record --help`.",
        "cases": cases,
    })
}

/// Reads a case file's text.
pub fn load(text: &str) -> Result<Vec<Case>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match v["giong_record"].as_u64() {
        Some(FORMAT) => {}
        Some(n) => {
            return Err(format!(
                "written in format {n}; this giong reads format {FORMAT} (update giong)"
            ))
        }
        None => return Err("not a giong record file (no \"giong_record\": 1)".into()),
    }
    let cases = v["cases"].as_array().ok_or("`cases` is not a list")?;
    let mut out = Vec::with_capacity(cases.len());
    let mut names = BTreeSet::new();
    for (i, c) in cases.iter().enumerate() {
        let case = read_case(c).map_err(|e| {
            let name = c["name"].as_str().unwrap_or("?");
            format!("case {} ({name}): {e}", i + 1)
        })?;
        if !names.insert(case.name.clone()) {
            return Err(format!(
                "case {}: the name {:?} is used twice",
                i + 1,
                case.name
            ));
        }
        out.push(case);
    }
    // Lines: where each `"name": "<name>"` stands, in order.
    let mut from = 0;
    for case in &mut out {
        let needle = format!("\"name\": {}", json!(case.name));
        let compact = format!("\"name\":{}", json!(case.name));
        let at = text[from..]
            .find(&needle)
            .or_else(|| text[from..].find(&compact))
            .map(|o| from + o);
        if let Some(at) = at {
            case.line = text[..at].matches('\n').count() + 1;
            from = at + 1;
        }
    }
    Ok(out)
}

fn read_case(c: &Value) -> Result<Case, String> {
    let name = c["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or("`name` is missing")?
        .to_string();
    let r = &c["request"];
    if !r.is_object() {
        return Err("`request` is missing".into());
    }
    let method = r["method"]
        .as_str()
        .filter(|m| !m.is_empty())
        .ok_or("`request.method` is missing")?
        .to_string();
    let path = r["path"]
        .as_str()
        .filter(|p| p.starts_with('/'))
        .ok_or("`request.path` is missing or does not start with /")?
        .to_string();
    let query = match &r["query"] {
        Value::Null => None,
        Value::String(q) => Some(q.clone()),
        _ => return Err("`request.query` is neither a string nor null".into()),
    };
    let mut headers = Vec::new();
    match &r["headers"] {
        Value::Null => {}
        Value::Array(list) => {
            for h in list {
                match h.as_array().map(Vec::as_slice) {
                    Some([Value::String(n), Value::String(v)]) => {
                        headers.push((n.clone(), v.clone()))
                    }
                    _ => return Err(format!("`request.headers` holds {h}, not [name, value]")),
                }
            }
        }
        _ => return Err("`request.headers` is not a list of [name, value]".into()),
    }
    let body = Body::read(r, false).map_err(|e| format!("request {e}"))?;
    let e = &c["expect"];
    if !e.is_object() {
        return Err("`expect` is missing".into());
    }
    let status = e["status"]
        .as_u64()
        .filter(|s| (100..1000).contains(s))
        .ok_or("`expect.status` is not an HTTP status")? as u16;
    let mut eh = BTreeMap::new();
    match &e["headers"] {
        Value::Null => {}
        Value::Object(o) => {
            for (k, v) in o {
                let v = v
                    .as_str()
                    .ok_or_else(|| format!("`expect.headers.{k}` is not a string"))?;
                eh.insert(k.to_ascii_lowercase(), v.to_string());
            }
        }
        _ => return Err("`expect.headers` is not an object".into()),
    }
    let ebody = Body::read(e, true).map_err(|e| format!("expect {e}"))?;
    Ok(Case {
        name,
        request: Request {
            method,
            path,
            query,
            headers,
            body,
        },
        expect: Expect {
            status,
            headers: eh,
            body: ebody,
        },
        line: 0,
    })
}

/// A value an answer gave that a later request may use.
#[derive(Debug)]
struct Cand {
    value: Value,
    hint: String,
    /// The placeholder written when nothing uses it.
    fallback: &'static str,
    /// Its variable name, once a later exchange uses it.
    name: Option<String>,
}

/// The marker standing for candidate `i` until the end of `build`.
fn marker(i: usize) -> String {
    format!("\u{1}cand{i}\u{1}")
}

/// What `build` made.
#[derive(Debug, Default)]
pub struct Built {
    pub cases: Vec<Value>,
    /// Exchanges left out: (number, why).
    pub skipped: Vec<(usize, String)>,
    /// Things worth telling: (number, note).
    pub notes: Vec<(usize, String)>,
    /// The environment variables the cases read (for redacted headers).
    pub env: BTreeSet<String>,
}

/// The values a request carries, to tell ids the client sent from ids the
/// service made: path segments, query values (by name) and JSON body
/// values (by key).
#[derive(Default)]
struct Sent {
    segments: Vec<String>,
    query: Vec<(String, String)>,
    body: Vec<(Option<String>, Value)>,
}

impl Sent {
    fn has(&self, v: &Value) -> bool {
        let text = as_text(v);
        match v {
            // A number counts as sent only where it stands as an id: a page
            // size of 7 does not make `"id": 7` the client's.
            Value::Number(_) => {
                self.segments.contains(&text)
                    || self
                        .query
                        .iter()
                        .any(|(k, q)| *q == text && shape::is_id_key(k))
                    || self
                        .body
                        .iter()
                        .any(|(k, b)| b == v && k.as_deref().is_some_and(shape::is_id_key))
            }
            Value::String(s) if !s.is_empty() => {
                self.segments.iter().any(|seg| seg == s)
                    || self.query.iter().any(|(_, q)| q == s)
                    || self.body.iter().any(|(_, b)| b == v)
            }
            _ => false,
        }
    }
}

fn collect_body(key: Option<&str>, v: &Value, out: &mut Vec<(Option<String>, Value)>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                collect_body(Some(k), x, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_body(key, x, out);
            }
        }
        Value::Null | Value::Bool(_) => {}
        other => out.push((key.map(str::to_string), other.clone())),
    }
}

fn split_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .collect()
}

/// `users` → `user`, for capture names.
fn singular(word: &str) -> String {
    let w = word.to_ascii_lowercase();
    let w: String = w
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if w.len() > 3 && w.ends_with("ies") {
        format!("{}y", &w[..w.len() - 3])
    } else if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..w.len() - 1].to_string()
    } else {
        w
    }
}

/// The last fixed segment of a path or pattern: `/api/users/{id}` → `users`.
fn resource(path: &str) -> Option<&str> {
    path.split('/')
        .filter(|s| !s.is_empty() && !s.starts_with('{') && !s.bytes().all(|b| b.is_ascii_digit()))
        .rfind(|s| !shape::is_uuid(s) && !shape::is_hex_id(s))
}

fn clean_name(s: &str) -> String {
    let mut n: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    n.truncate(48);
    let n = n.trim_matches('_').to_string();
    if n.is_empty() || Ph::reserved(&n) || n.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v_{n}")
    } else {
        n
    }
}

/// A name not in `used`, which it joins.
fn unique(base: &str, used: &mut BTreeSet<String>) -> String {
    let base = clean_name(base);
    if used.insert(base.clone()) {
        return base;
    }
    for i in 2.. {
        let n = format!("{base}_{i}");
        if used.insert(n.clone()) {
            return n;
        }
    }
    unreachable!()
}

/// `post_users_by_id_201`.
fn case_name(x: &Exchange) -> String {
    let shape = x.route.as_deref().unwrap_or(&x.path);
    let mut n = x.method.to_ascii_lowercase();
    for seg in shape.split('/').filter(|s| !s.is_empty()) {
        n.push('_');
        if let Some(p) = seg
            .strip_prefix("{*")
            .or_else(|| seg.strip_prefix('{'))
            .and_then(|s| s.strip_suffix('}'))
        {
            n.push_str("by_");
            n.push_str(p);
        } else {
            n.push_str(seg);
        }
    }
    if shape.trim_matches('/').is_empty() {
        n.push_str("_root");
    }
    format!("{n}_{}", x.status)
}

struct Builder<'u> {
    cands: Vec<Cand>,
    names: &'u mut BTreeSet<String>,
    /// While building a request: the variables it uses, as (name, text),
    /// so the answer's text can refer to them (`"no user {{user_id}}"`).
    in_request: bool,
    used_now: Vec<(String, String)>,
    /// The environment variables secret request values are read from.
    env: BTreeSet<String>,
}

impl Builder<'_> {
    /// `{{env:GIONG_RECORD_<NAME>}}`, for a secret value under `name`.
    fn secret_env(&mut self, name: &str) -> String {
        let var = env_var(name);
        let ph = format!("{{{{env:{var}}}}}");
        self.env.insert(var);
        ph
    }

    /// Whether a request value under `name` is read from the environment:
    /// `Record` redacted it, or its name says secret. Empty values and
    /// placeholders stay.
    fn is_secret_value(name: &str, value: &str) -> bool {
        value == REDACTED
            || (!value.is_empty() && matcher::whole(value).is_none() && is_secret_name(name))
    }

    /// A query string or form body with its secret values made
    /// `{{env:...}}`, other pairs as they were.
    fn redact_pairs(&mut self, q: &str) -> String {
        q.split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) if Self::is_secret_value(&secret::percent_decode(k), v) => {
                    let ph = self.secret_env(&secret::percent_decode(k));
                    format!("{k}={ph}")
                }
                _ => pair.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A JSON request value under a secret name: strings and numbers are
    /// read from the environment (`card.number` from
    /// GIONG_RECORD_CARD_NUMBER), objects and lists keep their shape.
    fn secret_json(&mut self, name: &str, v: &Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.secret_json(&format!("{name}_{k}"), x)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.secret_json(name, x)).collect()),
            Value::String(s) if s.is_empty() || matcher::whole(s).is_some() => v.clone(),
            Value::String(_) | Value::Number(_) => Value::String(self.secret_env(name)),
            Value::Null | Value::Bool(_) => v.clone(),
        }
    }

    /// The newest candidate holding `v`, if `v` can be one.
    fn find(&self, v: &Value) -> Option<usize> {
        let useful = match v {
            Value::Number(n) => n.is_u64() || n.is_i64(),
            Value::String(s) => !s.is_empty(),
            _ => false,
        };
        if !useful {
            return None;
        }
        self.cands.iter().rposition(|c| &c.value == v)
    }

    /// Candidate `i`'s variable name, naming it on first use.
    fn use_cand(&mut self, i: usize) -> String {
        let n = match &self.cands[i].name {
            Some(n) => n.clone(),
            None => {
                let n = unique(&self.cands[i].hint.clone(), self.names);
                self.cands[i].name = Some(n.clone());
                n
            }
        };
        if self.in_request && !self.used_now.iter().any(|(u, _)| *u == n) {
            self.used_now
                .push((n.clone(), as_text(&self.cands[i].value)));
        }
        n
    }

    /// Text with the values this exchange's request used replaced by their
    /// variables, where they stand as whole words.
    fn refer(&self, text: &str) -> String {
        if self.used_now.is_empty() || !text.bytes().any(|b| b.is_ascii_alphanumeric()) {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        for p in matcher::parts(text) {
            match p {
                matcher::Part::Ph(ph) => out.push_str(&render(ph)),
                matcher::Part::Lit(l) => out.push_str(&replace_words(l, &self.used_now)),
            }
        }
        out
    }

    /// May the number or string `v` at `key` in a request be candidate `i`?
    fn usable(&self, i: usize, key: Option<&str>, in_path: bool) -> bool {
        match &self.cands[i].value {
            // Numbers are small and common: only where they stand as ids.
            Value::Number(_) => in_path || key.is_some_and(shape::is_id_key),
            Value::String(s) => s.len() >= 4 || in_path,
            _ => false,
        }
    }

    fn request_path(&mut self, path: &str) -> String {
        path.split('/')
            .map(|seg| {
                if seg.is_empty() {
                    return String::new();
                }
                let as_num = seg
                    .parse::<i64>()
                    .ok()
                    .filter(|n| n.to_string() == seg)
                    .map(Value::from);
                let hit = as_num
                    .as_ref()
                    .and_then(|v| self.find(v))
                    .or_else(|| self.find(&Value::String(seg.to_string())));
                match hit {
                    Some(i) if self.usable(i, None, true) => {
                        format!("{{{{{}}}}}", self.use_cand(i))
                    }
                    _ => seg.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn request_query(&mut self, q: &str) -> String {
        split_query(q)
            .into_iter()
            .map(|(k, v)| {
                let name = secret::percent_decode(&k);
                if Self::is_secret_value(&name, &v) {
                    return format!("{k}={}", self.secret_env(&name));
                }
                let as_num = v
                    .parse::<i64>()
                    .ok()
                    .filter(|n| n.to_string() == v)
                    .map(Value::from);
                let hit = as_num
                    .as_ref()
                    .and_then(|n| self.find(n))
                    .or_else(|| self.find(&Value::String(v.clone())));
                let v = match hit {
                    Some(i) if self.usable(i, Some(&k), false) => {
                        format!("{{{{{}}}}}", self.use_cand(i))
                    }
                    _ => v,
                };
                if v.is_empty() {
                    k
                } else {
                    format!("{k}={v}")
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    fn request_json(&mut self, key: Option<&str>, v: &Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        let x = if is_secret_name(k) {
                            self.secret_json(k, x)
                        } else {
                            self.request_json(Some(k), x)
                        };
                        (k.clone(), x)
                    })
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.request_json(key, x)).collect()),
            Value::String(s) if s == REDACTED => {
                Value::String(self.secret_env(key.unwrap_or("value")))
            }
            Value::String(s) if matcher::whole(s).is_some() || s.contains("{{") => {
                // A literal that looks like a placeholder would be read as
                // one: keep it, the note in `build` warns.
                v.clone()
            }
            other => match self.find(other) {
                Some(i) if self.usable(i, key, false) => {
                    Value::String(format!("{{{{{}}}}}", self.use_cand(i)))
                }
                _ => other.clone(),
            },
        }
    }

    /// Masks an answer's JSON body.
    fn answer_json(&mut self, key: Option<&str>, v: &Value, sent: &Sent, hint: &str) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.answer_json(Some(k), x, sent, hint)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.answer_json(key, x, sent, hint))
                    .collect(),
            ),
            Value::Null | Value::Bool(_) => v.clone(),
            Value::String(s) if key.is_some_and(is_secret_name) && matcher::whole(s).is_none() => {
                json!("{{string}}")
            }
            Value::Number(_) if key.is_some_and(is_secret_name) => json!("{{number}}"),
            _ => {
                let id_key = key.is_some_and(shape::is_id_key);
                // A value an earlier answer gave and a request has used
                // since (or this answer repeats): it must come back equal.
                if let Some(i) = self.find(v) {
                    let strong = id_key
                        || matches!(v, Value::String(s) if shape::is_uuid(s) || shape::is_hex_id(s));
                    if strong && (self.cands[i].name.is_some() || id_key) {
                        return Value::String(format!("{{{{{}}}}}", self.use_cand(i)));
                    }
                }
                if sent.has(v) {
                    return v.clone();
                }
                let new_cand = |b: &mut Builder<'_>, fallback: &'static str| {
                    let k = key.unwrap_or("value");
                    let hint = if matches!(k.to_ascii_lowercase().as_str(), "id" | "uuid" | "guid")
                    {
                        format!("{hint}_{}", k.to_ascii_lowercase())
                    } else {
                        k.to_string()
                    };
                    b.cands.push(Cand {
                        value: v.clone(),
                        hint,
                        fallback,
                        name: None,
                    });
                    Value::String(marker(b.cands.len() - 1))
                };
                match v {
                    Value::String(s) => {
                        if let Some(Ph::Var(_) | Ph::Capture(_) | Ph::Env(_)) = matcher::whole(s) {
                            v.clone()
                        } else if shape::is_time(s) {
                            json!("{{time}}")
                        } else if shape::is_uuid(s) {
                            new_cand(self, "{{uuid}}")
                        } else if id_key {
                            new_cand(self, "{{string}}")
                        } else if shape::is_hex_id(s) {
                            new_cand(self, "{{hex}}")
                        } else if key.is_some_and(shape::is_token_key) {
                            json!("{{string}}")
                        } else if key.is_some_and(shape::is_time_key) {
                            json!("{{any}}")
                        } else {
                            Value::String(self.refer(s))
                        }
                    }
                    Value::Number(_) => {
                        if id_key {
                            new_cand(self, "{{number}}")
                        } else if key.is_some_and(shape::is_time_key) {
                            json!("{{number}}")
                        } else {
                            v.clone()
                        }
                    }
                    _ => v.clone(),
                }
            }
        }
    }

    /// Masks a `Location`: ids in its path become candidates or variables.
    fn answer_location(&mut self, loc: &str, sent: &Sent) -> String {
        let (path, rest) = match loc.find(['?', '#']) {
            Some(i) => (&loc[..i], &loc[i..]),
            None => (loc, ""),
        };
        let hint = resource(path)
            .map(singular)
            .unwrap_or_else(|| "location".into());
        let segs: Vec<String> = path
            .split('/')
            .map(|seg| {
                let v = match seg.parse::<i64>() {
                    Ok(n) if n.to_string() == seg => Value::from(n),
                    _ if shape::is_uuid(seg) || shape::is_hex_id(seg) => Value::String(seg.into()),
                    _ => return seg.to_string(),
                };
                if let Some(i) = self.find(&v) {
                    return format!("{{{{{}}}}}", self.use_cand(i));
                }
                if sent.segments.iter().any(|s| s == seg) {
                    return seg.to_string();
                }
                self.cands.push(Cand {
                    value: v,
                    hint: format!("{hint}_id"),
                    fallback: "{{any}}",
                    name: None,
                });
                marker(self.cands.len() - 1)
            })
            .collect();
        // Secret query values (`?token=...` in a reset link) are not kept.
        let rest = match rest.strip_prefix('?') {
            Some(q) => {
                let (q, frag) = q.split_once('#').map_or((q, ""), |(q, f)| (q, f));
                let q = q
                    .split('&')
                    .map(|pair| match pair.split_once('=') {
                        Some((k, v))
                            if !v.is_empty() && is_secret_name(&secret::percent_decode(k)) =>
                        {
                            format!("{k}={{{{string}}}}")
                        }
                        _ => mask_text(pair),
                    })
                    .collect::<Vec<_>>()
                    .join("&");
                let frag = if frag.is_empty() {
                    String::new()
                } else {
                    format!("#{}", mask_text(frag))
                };
                format!("?{q}{frag}")
            }
            None => mask_text(rest),
        };
        format!("{}{}", segs.join("/"), rest)
    }
}

/// A placeholder written back as text.
fn render(ph: Ph<'_>) -> String {
    match ph {
        Ph::Any => "{{any}}".into(),
        Ph::Str => "{{string}}".into(),
        Ph::Number => "{{number}}".into(),
        Ph::Uuid => "{{uuid}}".into(),
        Ph::Time => "{{time}}".into(),
        Ph::Hex => "{{hex}}".into(),
        Ph::Capture(n) => format!("{{{{capture:{n}}}}}"),
        Ph::Var(n) => format!("{{{{{n}}}}}"),
        Ph::Env(v) => format!("{{{{env:{v}}}}}"),
    }
}

/// Replaces each value of `vars` (name, text) standing as a whole word in
/// `text` (not inside a longer run of letters and digits) by `{{name}}`.
fn replace_words(text: &str, vars: &[(String, String)]) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < b.len() {
        if i == 0 || !b[i - 1].is_ascii_alphanumeric() {
            for (name, value) in vars {
                if value.is_empty() {
                    continue;
                }
                let end = i + value.len();
                if text[i..].starts_with(value.as_str())
                    && (end == b.len() || !b[end].is_ascii_alphanumeric())
                {
                    out.push_str(&format!("{{{{{name}}}}}"));
                    i = end;
                    continue 'outer;
                }
            }
        }
        let c = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// Masks times, UUIDs and long hex ids inside text.
pub fn mask_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // Only at a token start, so `x2026-...` inside a word stays.
        let start_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if start_ok {
            if b.len() - i >= 36
                && s.is_char_boundary(i + 36)
                && shape::is_uuid(&s[i..i + 36])
                && (i + 36 == b.len() || !b[i + 36].is_ascii_alphanumeric())
            {
                out.push_str("{{uuid}}");
                i += 36;
                continue;
            }
            if b[i].is_ascii_digit() {
                // The longest time starting here.
                if let Some(len) = (19..=40.min(b.len() - i))
                    .rev()
                    .find(|&l| s.is_char_boundary(i + l) && shape::is_time(&s[i..i + l]))
                {
                    if i + len == b.len() || !b[i + len].is_ascii_alphanumeric() {
                        out.push_str("{{time}}");
                        i += len;
                        continue;
                    }
                }
            }
            if b[i].is_ascii_hexdigit() {
                let len = b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
                let end_ok = i + len == b.len() || !b[i + len].is_ascii_alphanumeric();
                if end_ok && shape::is_hex_id(&s[i..i + len]) {
                    out.push_str("{{hex}}");
                    i += len;
                    continue;
                }
            }
        }
        let c = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// Replaces candidate markers once every exchange has been seen.
fn finish(v: &mut Value, cands: &[Cand]) {
    match v {
        Value::String(s) if s.contains('\u{1}') => {
            for (i, c) in cands.iter().enumerate() {
                let m = marker(i);
                if s.contains(&m) {
                    let with = match &c.name {
                        Some(n) => format!("{{{{capture:{n}}}}}"),
                        None => c.fallback.to_string(),
                    };
                    *s = s.replace(&m, &with);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| finish(x, cands)),
        Value::Object(o) => o.values_mut().for_each(|x| finish(x, cands)),
        _ => {}
    }
}

/// The environment variable a redacted header's value is read from.
pub fn env_var(header: &str) -> String {
    let mut n: String = header
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    n.insert_str(0, "GIONG_RECORD_");
    n
}

/// Turns exchanges into cases, in order. `names` holds the names already
/// used (case names and variable names, when appending to a file) and gets
/// the new ones.
pub fn build(exchanges: &[&Exchange], names: &mut BTreeSet<String>) -> Built {
    let mut out = Built::default();
    let mut b = Builder {
        cands: Vec::new(),
        names,
        in_request: false,
        used_now: Vec::new(),
        env: BTreeSet::new(),
    };
    let mut case_names: BTreeSet<String> = BTreeSet::new();
    for x in exchanges {
        if let Err(why) = x.replayable() {
            out.skipped.push((x.number, why));
            continue;
        }
        // The request, with ids from earlier answers made variables.
        b.in_request = true;
        b.used_now.clear();
        let path = b.request_path(&x.path);
        let query = x.query.as_deref().map(|q| b.request_query(q));
        let mut headers = Vec::new();
        for (n, v) in &x.request.headers {
            if DROPPED_REQUEST_HEADERS.contains(&n.as_str()) {
                continue;
            }
            if Builder::is_secret_value(n, v) {
                let ph = b.secret_env(n);
                headers.push((n.clone(), ph));
            } else {
                headers.push((n.clone(), v.clone()));
            }
        }
        let req_json = is_json(&x.request.headers, &x.request.body);
        let body = if x.request.body.is_empty() {
            Body::None
        } else if let Some(j) = &req_json {
            Body::Json(b.request_json(None, j))
        } else if x.request.text {
            let t = String::from_utf8_lossy(&x.request.body).into_owned();
            if matcher::parts(&t)
                .iter()
                .any(|p| matches!(p, matcher::Part::Ph(_)))
            {
                out.notes.push((
                    x.number,
                    "the request body holds text that reads as a {{placeholder}}; it will be \
                     filled in when replayed"
                        .into(),
                ));
            }
            if is_form(&x.request.headers) {
                Body::Text(b.redact_pairs(&t))
            } else {
                Body::Text(t)
            }
        } else {
            Body::Bytes(x.request.body.clone())
        };

        b.in_request = false;
        // What was sent, to tell the client's ids from the service's.
        let mut sent = Sent {
            segments: x
                .path
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            query: x.query.as_deref().map(split_query).unwrap_or_default(),
            body: Vec::new(),
        };
        if let Some(j) = &req_json {
            collect_body(None, j, &mut sent.body);
        }

        // The expectation.
        let mut eh = BTreeMap::new();
        if let Some(ct) = x.response.header("content-type") {
            eh.insert("content-type".to_string(), ct.to_string());
        }
        if let Some(loc) = x.response.header("location") {
            let masked = b.answer_location(loc, &sent);
            eh.insert("location".to_string(), masked);
        }
        let hint = x
            .route
            .as_deref()
            .and_then(resource)
            .or_else(|| resource(&x.path))
            .map(singular)
            .unwrap_or_else(|| "value".into());
        let ebody = if let Some(enc) = x.response.header("content-encoding") {
            out.notes.push((
                x.number,
                format!(
                    "the answer was recorded {enc}-compressed, so its body is not checked; \
                     layer Record inside Compression (add it after) to record plain bodies"
                ),
            ));
            Body::Unchecked(format!("recorded {enc}-compressed"))
        } else if x.response.streamed {
            Body::Unchecked("the answer was streamed and not recorded".into())
        } else if x.response.truncated {
            out.notes.push((
                x.number,
                "the answer's body was cut short when recorded, so it is not checked".into(),
            ));
            Body::Unchecked(format!(
                "recorded cut at {} of {} bytes",
                x.response.body.len(),
                x.response.body_len.unwrap_or(0)
            ))
        } else if x.response.body.is_empty() {
            Body::Text(String::new())
        } else if let Some(j) = is_json(&x.response.headers, &x.response.body) {
            Body::Json(b.answer_json(None, &j, &sent, &hint))
        } else if x.response.text {
            Body::Text(b.refer(&mask_text(&String::from_utf8_lossy(&x.response.body))))
        } else {
            Body::Bytes(x.response.body.clone())
        };
        let name = unique(&case_name(x), &mut case_names);
        // Case names must not collide with earlier files' either.
        let name = if b.names.contains(&name) {
            unique(&name, b.names)
        } else {
            b.names.insert(name.clone());
            name
        };
        let case = Case {
            name,
            request: Request {
                method: x.method.clone(),
                path,
                query,
                headers,
                body,
            },
            expect: Expect {
                status: x.status,
                headers: eh,
                body: ebody,
            },
            line: 0,
        };
        out.cases.push(case.to_json(Some(json!({
            "exchange": x.number,
            "ts": x.ts,
            "route": x.route,
            "request_id": x.request_id,
        }))));
    }
    for c in &mut out.cases {
        finish(c, &b.cands);
    }
    out.env.append(&mut b.env);
    out
}

/// Whether a request says its body is a form (`a=1&b=2`).
pub fn is_form(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("content-type")
            && v.to_ascii_lowercase()
                .contains("application/x-www-form-urlencoded")
    })
}

/// The body as JSON when it is a JSON object or list (by content type or
/// by its first byte).
fn is_json(headers: &[(String, String)], body: &[u8]) -> Option<Value> {
    let ct = headers
        .iter()
        .find(|(n, _)| n == "content-type")
        .map(|(_, v)| v.to_ascii_lowercase());
    let says_json = ct.as_deref().is_some_and(|c| c.contains("json"));
    let first = body.iter().find(|b| !b.is_ascii_whitespace());
    if !says_json && !matches!(first, Some(b'{' | b'[')) {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// Every name a case file uses: case names and captured variables.
pub fn names_in(cases: &[Case]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) => {
                for p in matcher::parts(s) {
                    if let matcher::Part::Ph(Ph::Capture(n) | Ph::Var(n)) = p {
                        out.insert(n.to_string());
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    for c in cases {
        out.insert(c.name.clone());
        walk(&c.to_json(None), &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::exchange::{parse_line, tests::line};
    use super::*;

    fn xs(lines: &[String]) -> Vec<Exchange> {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| parse_line(l, i + 1).unwrap().unwrap())
            .collect()
    }

    fn built(lines: &[String]) -> Built {
        let all = xs(lines);
        let refs: Vec<&Exchange> = all.iter().collect();
        build(&refs, &mut BTreeSet::new())
    }

    #[test]
    fn an_id_the_service_made_is_captured_and_used_later() {
        let b = built(&[
            line(
                "POST",
                "/users",
                201,
                r#"{"name":"ada","age":7}"#,
                r#"{"id":7,"name":"ada","created_at":"2026-11-22T08:40:01Z","token":"abc"}"#,
            ),
            line("GET", "/users/7", 200, "", r#"{"id":7,"name":"ada"}"#),
            line(
                "POST",
                "/orders",
                201,
                r#"{"user_id":7,"qty":7}"#,
                r#"{"id":1,"user_id":7}"#,
            ),
        ]);
        assert!(b.skipped.is_empty());
        let c = &b.cases;
        assert_eq!(c[0]["name"], "post_users_201");
        assert_eq!(c[0]["expect"]["body_json"]["id"], "{{capture:user_id}}");
        assert_eq!(c[0]["expect"]["body_json"]["name"], "ada");
        assert_eq!(c[0]["expect"]["body_json"]["created_at"], "{{time}}");
        assert_eq!(c[0]["expect"]["body_json"]["token"], "{{string}}");
        // The request's age 7 is not the id.
        assert_eq!(c[0]["request"]["body_json"]["age"], 7);
        assert_eq!(c[1]["request"]["path"], "/users/{{user_id}}");
        assert_eq!(c[1]["expect"]["body_json"]["id"], "{{user_id}}");
        assert_eq!(c[2]["request"]["body_json"]["user_id"], "{{user_id}}");
        assert_eq!(c[2]["request"]["body_json"]["qty"], 7);
        assert_eq!(c[2]["expect"]["body_json"]["user_id"], "{{user_id}}");
        // Nobody used order 1: masked, not captured.
        assert_eq!(c[2]["expect"]["body_json"]["id"], "{{number}}");
    }

    #[test]
    fn answers_refer_to_the_ids_their_request_used() {
        let b = built(&[
            line("POST", "/users", 201, "{}", r#"{"id":1}"#),
            line(
                "GET",
                "/users/1",
                404,
                "",
                r#"{"detail":"no user 1","note":"1x 21 {{time}}","n":1}"#,
            ),
        ]);
        let body = &b.cases[1]["expect"]["body_json"];
        assert_eq!(body["detail"], "no user {{user_id}}");
        // Not inside other words or numbers, and placeholders stay as they are.
        assert_eq!(body["note"], "1x 21 {{time}}");
        assert_eq!(body["n"], 1);
        assert_eq!(
            replace_words("a1 1 1b (1)", &[("v".into(), "1".into())]),
            "a1 {{v}} 1b ({{v}})"
        );
    }

    #[test]
    fn ids_the_client_sent_stay_literal() {
        let b = built(&[line(
            "PUT",
            "/users/9",
            200,
            r#"{"name":"x"}"#,
            r#"{"id":9,"name":"x"}"#,
        )]);
        assert_eq!(b.cases[0]["expect"]["body_json"]["id"], 9);
        assert_eq!(b.cases[0]["request"]["path"], "/users/9");
    }

    #[test]
    fn locations_capture_ids() {
        let mut first: Value =
            serde_json::from_str(&line("POST", "/items", 201, "{}", "")).unwrap();
        first["response"]["headers"] =
            json!([["location", "/items/550e8400-e29b-41d4-a716-446655440000"]]);
        let b = built(&[
            first.to_string(),
            line(
                "GET",
                "/items/550e8400-e29b-41d4-a716-446655440000",
                200,
                "",
                "{}",
            ),
        ]);
        assert_eq!(
            b.cases[0]["expect"]["headers"]["location"],
            "/items/{{capture:item_id}}"
        );
        assert_eq!(b.cases[1]["request"]["path"], "/items/{{item_id}}");
        // An unused location id is any.
        let mut only: Value = serde_json::from_str(&line("POST", "/items", 201, "{}", "")).unwrap();
        only["response"]["headers"] = json!([["location", "/items/3?x=1"]]);
        let b = built(&[only.to_string()]);
        assert_eq!(
            b.cases[0]["expect"]["headers"]["location"],
            "/items/{{any}}?x=1"
        );
    }

    #[test]
    fn query_values_and_uuids_in_bodies() {
        let b = built(&[
            line(
                "POST",
                "/s",
                201,
                "{}",
                r#"{"key":"550e8400-e29b-41d4-a716-446655440000","n":3}"#,
            ),
            line(
                "GET",
                "/s?key=550e8400-e29b-41d4-a716-446655440000&limit=3",
                200,
                "",
                "[]",
            ),
        ]);
        assert_eq!(b.cases[0]["expect"]["body_json"]["key"], "{{capture:key}}");
        assert_eq!(b.cases[0]["expect"]["body_json"]["n"], 3);
        assert_eq!(b.cases[1]["request"]["query"], "key={{key}}&limit=3");
    }

    #[test]
    fn redacted_headers_read_the_environment() {
        let b = built(&[line("GET", "/me", 200, "", "{}")]);
        let h = &b.cases[0]["request"]["headers"];
        assert_eq!(
            h[1],
            json!(["authorization", "{{env:GIONG_RECORD_AUTHORIZATION}}"])
        );
        assert!(b.env.contains("GIONG_RECORD_AUTHORIZATION"));
    }

    /// The leak the dev kit audit found: a password in a JSON body, an API
    /// key in a query and a card number reached the case file.
    #[test]
    fn secret_request_fields_read_the_environment() {
        let b = built(&[line(
            "POST",
            "/login?api_key=AK-SECRET-123&page=2",
            200,
            r#"{"user":"ana","password":"hunter2-SECRET","card":{"number":"4242424242424242","cvv":123},"note":"[redacted]","empty_token":""}"#,
            r#"{"token":"abc.def","user":{"id":7,"api_key":"AK-2"},"ok":true}"#,
        )]);
        let c = &b.cases[0];
        assert_eq!(
            c["request"]["query"],
            "api_key={{env:GIONG_RECORD_API_KEY}}&page=2"
        );
        assert_eq!(
            c["request"]["body_json"],
            json!({
                "user": "ana",
                "password": "{{env:GIONG_RECORD_PASSWORD}}",
                "card": {
                    "number": "{{env:GIONG_RECORD_CARD_NUMBER}}",
                    "cvv": "{{env:GIONG_RECORD_CARD_CVV}}"
                },
                "note": "{{env:GIONG_RECORD_NOTE}}",
                "empty_token": ""
            })
        );
        assert_eq!(c["expect"]["body_json"]["token"], "{{string}}");
        assert_eq!(c["expect"]["body_json"]["user"]["api_key"], "{{string}}");
        for v in [
            "GIONG_RECORD_API_KEY",
            "GIONG_RECORD_PASSWORD",
            "GIONG_RECORD_CARD_NUMBER",
            "GIONG_RECORD_CARD_CVV",
            "GIONG_RECORD_NOTE",
            "GIONG_RECORD_AUTHORIZATION",
        ] {
            assert!(b.env.contains(v), "{v}: {:?}", b.env);
        }
        let text = c.to_string();
        for leak in ["hunter2", "AK-SECRET", "4242", "AK-2"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
        assert!(secret::scan(c).is_empty(), "{:?}", secret::scan(c));
    }

    #[test]
    fn form_bodies_headers_and_locations_keep_no_secrets() {
        let mut v: Value = serde_json::from_str(&line("POST", "/session", 303, "", "")).unwrap();
        v["request"]["headers"] = json!([
            ["content-type", "application/x-www-form-urlencoded"],
            ["x-session-token", "s3cr3t"],
            ["x-trace", "keep"]
        ]);
        v["request"]["body"] = json!("user=ana&pass%77ord=hunter2&remember=1");
        v["response"]["headers"] = json!([["location", "/reset?token=T0K3N&step=2#top"]]);
        let b = built(&[v.to_string()]);
        let c = &b.cases[0];
        assert_eq!(
            c["request"]["body"],
            "user=ana&pass%77ord={{env:GIONG_RECORD_PASSWORD}}&remember=1"
        );
        let h = c["request"]["headers"].to_string();
        assert!(h.contains("{{env:GIONG_RECORD_X_SESSION_TOKEN}}"), "{h}");
        assert!(h.contains("keep"), "{h}");
        assert_eq!(
            c["expect"]["headers"]["location"],
            "/reset?token={{string}}&step=2#top"
        );
        assert!(!c.to_string().contains("hunter2") && !c.to_string().contains("T0K3N"));
    }

    #[test]
    fn noisy_request_headers_are_dropped() {
        let mut v: Value = serde_json::from_str(&line("GET", "/", 200, "", "ok")).unwrap();
        v["request"]["headers"] = json!([
            ["host", "x"],
            ["user-agent", "curl"],
            ["accept", "*/*"],
            ["x-request-id", "1"],
            ["x-tenant", "a"]
        ]);
        let b = built(&[v.to_string()]);
        assert_eq!(
            b.cases[0]["request"]["headers"],
            json!([["accept", "*/*"], ["x-tenant", "a"]])
        );
    }

    #[test]
    fn text_bodies_are_masked_and_odd_bodies_unchecked() {
        let mut v: Value = serde_json::from_str(&line("GET", "/t", 200, "", "")).unwrap();
        v["response"]["headers"] = json!([["content-type", "text/plain"]]);
        v["response"]["body"] = json!("at 2026-11-22T08:40:01.5Z id 550e8400-e29b-41d4-a716-446655440000 req 5f3a9c2e8b41d706000100000000002a ok");
        let mut gz = v.clone();
        gz["response"]["headers"] = json!([["content-encoding", "gzip"]]);
        let mut cut = v.clone();
        cut["response"]["truncated"] = json!(true);
        cut["response"]["body_len"] = json!(999999);
        let b = built(&[v.to_string(), gz.to_string(), cut.to_string()]);
        assert_eq!(
            b.cases[0]["expect"]["body"],
            "at {{time}} id {{uuid}} req {{hex}} ok"
        );
        assert!(b.cases[1]["expect"]["body_unchecked"]
            .as_str()
            .unwrap()
            .contains("gzip"));
        assert!(b.cases[2]["expect"]["body_unchecked"].is_string());
        assert_eq!(b.notes.len(), 2);
    }

    #[test]
    fn duplicate_names_get_numbers_and_skips_are_reported() {
        let mut bad: Value = serde_json::from_str(&line("POST", "/u", 200, "abc", "")).unwrap();
        bad["request"]["truncated"] = json!(true);
        let b = built(&[
            line("GET", "/", 200, "", "a"),
            line("GET", "/", 200, "", "a"),
            bad.to_string(),
        ]);
        assert_eq!(b.cases[0]["name"], "get_root_200");
        assert_eq!(b.cases[1]["name"], "get_root_200_2");
        assert_eq!(b.skipped.len(), 1);
        assert_eq!(b.skipped[0].0, 3);
    }

    #[test]
    fn saved_files_load_back_and_round_trip() {
        let b = built(&[
            line("POST", "/users", 201, r#"{"name":"ada"}"#, r#"{"id":7}"#),
            line("GET", "/users/7?x=1", 200, "", r#"{"id":7}"#),
        ]);
        let text = serde_json::to_string_pretty(&file_json(&b.cases)).unwrap();
        let cases = load(&text).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[1].request.path, "/users/{{user_id}}");
        assert_eq!(cases[1].request.query.as_deref(), Some("x=1"));
        assert!(cases[0].line > 1 && cases[1].line > cases[0].line);
        let names = names_in(&cases);
        assert!(names.contains("user_id") && names.contains("post_users_201"));
        // Appending: new names avoid the old ones.
        let again = xs(&[
            line("POST", "/users", 201, "{}", r#"{"id":8}"#),
            line("GET", "/users/8", 200, "", "{}"),
        ]);
        let refs: Vec<&Exchange> = again.iter().collect();
        let mut used = names;
        let b2 = build(&refs, &mut used);
        assert_eq!(b2.cases[0]["name"], "post_users_201_2");
        assert_eq!(
            b2.cases[0]["expect"]["body_json"]["id"],
            "{{capture:user_id_2}}"
        );
    }

    #[test]
    fn broken_files_say_what_is_wrong() {
        let cases = [
            ("[]", "giong record file"),
            ("{\"giong_record\": 2, \"cases\": []}", "format 2"),
            ("{\"giong_record\": 1}", "`cases`"),
            ("{\"giong_record\": 1, \"cases\": [{}]}", "case 1"),
            ("{\"giong_record\": 1, \"cases\": [{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"x\"}}]}", "path"),
            ("{\"giong_record\": 1, \"cases\": [{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"/\"},\"expect\":{\"status\":42}}]}", "status"),
            ("{\"giong_record\": 1, \"cases\": [{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"/\",\"body\":\"x\",\"body_json\":1},\"expect\":{\"status\":200}}]}", "both"),
            ("{\"giong_record\": 1, \"cases\": [{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"/\",\"body_unchecked\":\"x\"},\"expect\":{\"status\":200}}]}", "expectations only"),
            ("{\"giong_record\": 1, \"cases\": [{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"/\"},\"expect\":{\"status\":200}},{\"name\":\"a\",\"request\":{\"method\":\"GET\",\"path\":\"/\"},\"expect\":{\"status\":200}}]}", "twice"),
            ("nope", "not JSON"),
        ];
        for (text, want) in cases {
            let err = load(text).unwrap_err();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn masking_text_leaves_ordinary_words() {
        assert_eq!(
            mask_text("deadbeefdeadbeef and 2026"),
            "deadbeefdeadbeef and 2026"
        );
        assert_eq!(
            mask_text("x550e8400-e29b-41d4-a716-446655440000"),
            "x550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(mask_text("é 2026-11-22 08:40:01 ü"), "é {{time}} ü");
        assert_eq!(mask_text(""), "");
    }

    #[test]
    fn names() {
        assert_eq!(singular("users"), "user");
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("address"), "address");
        assert_eq!(resource("/api/users/{id}"), Some("users"));
        assert_eq!(
            resource("/items/550e8400-e29b-41d4-a716-446655440000"),
            Some("items")
        );
        assert_eq!(resource("/"), None);
        assert_eq!(clean_name("any"), "v_any");
        assert_eq!(clean_name("9lives"), "v_9lives");
        assert_eq!(env_var("x-api-key"), "GIONG_RECORD_X_API_KEY");
        let mut used = BTreeSet::new();
        assert_eq!(unique("a", &mut used), "a");
        assert_eq!(unique("a", &mut used), "a_2");
    }
}
