//! Placeholders, and checking an answer against what a case expects.
//!
//! A case file is plain JSON a person can edit. Values that change from run
//! to run are written as placeholders:
//!
//! | Placeholder | Matches |
//! |---|---|
//! | `{{any}}` | anything (in text: any run of characters, also none) |
//! | `{{string}}` | a JSON string (in text: one or more characters) |
//! | `{{number}}` | a JSON number (in text: digits, sign, point, exponent) |
//! | `{{uuid}}` | a UUID such as `550e8400-e29b-41d4-a716-446655440000` |
//! | `{{time}}` | an RFC 3339 date and time |
//! | `{{hex}}` | 16 hex characters or more |
//! | `{{capture:NAME}}` | anything, and remembers it as `NAME` |
//! | `{{NAME}}` | exactly what `{{capture:NAME}}` remembered earlier |
//! | `{{env:VAR}}` | in requests only: the environment variable `VAR` |
//!
//! In a JSON body a placeholder is a whole string value: `"id":
//! "{{capture:user_id}}"` matches `"id": 7` and remembers the number 7; a
//! later request with `"/users/{{user_id}}"` sends `/users/7`, and a later
//! body `{"owner": "{{user_id}}"}` sends `{"owner": 7}` (the number, not a
//! string). In text (a text body, a `Location` header) placeholders may sit
//! anywhere in the string.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::shape;

/// Values remembered by `{{capture:NAME}}` during one replay of a file.
pub type Vars = BTreeMap<String, Value>;

/// A placeholder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ph<'a> {
    Any,
    Str,
    Number,
    Uuid,
    Time,
    Hex,
    Capture(&'a str),
    Var(&'a str),
    Env(&'a str),
}

impl Ph<'_> {
    /// Whether `{{x}}` may name a variable: not one of the class words.
    pub fn reserved(name: &str) -> bool {
        matches!(name, "any" | "string" | "number" | "uuid" | "time" | "hex")
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The placeholder inside `{{` and `}}`, if `inner` is one.
fn parse_inner(inner: &str) -> Option<Ph<'_>> {
    Some(match inner {
        "any" => Ph::Any,
        "string" => Ph::Str,
        "number" => Ph::Number,
        "uuid" => Ph::Uuid,
        "time" => Ph::Time,
        "hex" => Ph::Hex,
        _ => {
            if let Some(n) = inner.strip_prefix("capture:") {
                if !valid_name(n) || Ph::reserved(n) {
                    return None;
                }
                Ph::Capture(n)
            } else if let Some(v) = inner.strip_prefix("env:") {
                if !valid_name(v) {
                    return None;
                }
                Ph::Env(v)
            } else if valid_name(inner) {
                Ph::Var(inner)
            } else {
                return None;
            }
        }
    })
}

/// `s` when the whole string is one placeholder.
pub fn whole(s: &str) -> Option<Ph<'_>> {
    parse_inner(s.strip_prefix("{{")?.strip_suffix("}}")?)
}

/// A piece of a text template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part<'a> {
    Lit(&'a str),
    Ph(Ph<'a>),
}

/// Splits text into literal runs and placeholders. A `{{` that does not
/// start a placeholder is literal.
pub fn parts(s: &str) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let mut lit_start = 0;
    let mut i = 0;
    while let Some(off) = s[i..].find("{{") {
        let open = i + off;
        if let Some(close_off) = s[open + 2..].find("}}") {
            let close = open + 2 + close_off;
            if let Some(ph) = parse_inner(&s[open + 2..close]) {
                if lit_start < open {
                    out.push(Part::Lit(&s[lit_start..open]));
                }
                out.push(Part::Ph(ph));
                i = close + 2;
                lit_start = i;
                continue;
            }
        }
        i = open + 1;
    }
    if lit_start < s.len() {
        out.push(Part::Lit(&s[lit_start..]));
    }
    out
}

/// A variable's value as text (a string without quotes).
pub fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What a request needs that is not there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// `{{NAME}}` before any `{{capture:NAME}}` remembered it.
    Var(String),
    /// `{{env:VAR}}` with `VAR` not set.
    Env(String),
    /// A class placeholder such as `{{uuid}}` in a request: nothing to send.
    Class(String),
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Missing::Var(n) => write!(
                f,
                "{{{{{n}}}}} is used before an earlier answer captured it ({{{{capture:{n}}}}})"
            ),
            Missing::Env(v) => write!(f, "the environment variable {v} is not set"),
            Missing::Class(c) => write!(
                f,
                "{c} is a placeholder for answers; a request needs a value there"
            ),
        }
    }
}

/// Reads environment variables; a parameter so tests need not set real ones.
pub type Env<'e> = &'e dyn Fn(&str) -> Option<String>;

/// Percent-encodes what may not stand in a path segment or query value.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Fills a request string's placeholders. `url` percent-encodes values
/// (for the path and query).
pub fn fill_text(s: &str, vars: &Vars, env: Env<'_>, url: bool) -> Result<String, Missing> {
    let mut out = String::with_capacity(s.len());
    for p in parts(s) {
        let value = match p {
            Part::Lit(l) => {
                out.push_str(l);
                continue;
            }
            Part::Ph(Ph::Var(n)) => as_text(vars.get(n).ok_or_else(|| Missing::Var(n.into()))?),
            Part::Ph(Ph::Env(v)) => env(v).ok_or_else(|| Missing::Env(v.into()))?,
            Part::Ph(Ph::Capture(n)) => return Err(Missing::Class(format!("{{{{capture:{n}}}}}"))),
            Part::Ph(other) => return Err(Missing::Class(format!("{other:?}").to_lowercase())),
        };
        if url {
            out.push_str(&encode_component(&value));
        } else {
            out.push_str(&value);
        }
    }
    Ok(out)
}

/// Fills a request's JSON body: a string that is one `{{NAME}}` becomes the
/// remembered value with its own type.
pub fn fill_json(v: &Value, vars: &Vars, env: Env<'_>) -> Result<Value, Missing> {
    Ok(match v {
        Value::String(s) => match whole(s) {
            Some(Ph::Var(n)) => vars.get(n).cloned().ok_or_else(|| Missing::Var(n.into()))?,
            _ => Value::String(fill_text(s, vars, env, false)?),
        },
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| fill_json(x, vars, env))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| Ok((k.clone(), fill_json(x, vars, env)?)))
                .collect::<Result<_, Missing>>()?,
        ),
        other => other.clone(),
    })
}

/// Short text of a value for a difference message.
fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 80 {
        let cut: String = s.chars().take(77).collect();
        format!("{cut}...")
    } else {
        s
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Checks `actual` against `expected`, appending differences to `diffs`
/// (at most 20) and remembering captures in `vars`. `at` is where the
/// value is, such as `body.users[0].id`.
pub fn match_json(
    expected: &Value,
    actual: &Value,
    at: &str,
    vars: &mut Vars,
    diffs: &mut Vec<String>,
) {
    if diffs.len() >= 20 {
        return;
    }
    if let Value::String(s) = expected {
        if let Some(ph) = whole(s) {
            let ok = match ph {
                Ph::Any => true,
                Ph::Str => actual.is_string(),
                Ph::Number => actual.is_number(),
                Ph::Uuid => actual.as_str().is_some_and(shape::is_uuid),
                Ph::Time => actual.as_str().is_some_and(shape::is_time),
                Ph::Hex => actual.as_str().is_some_and(shape::is_hex_id),
                Ph::Capture(n) => {
                    if actual.is_null() {
                        diffs.push(format!(
                            "{at}: expected a value to capture as {n}, got null"
                        ));
                        return;
                    }
                    vars.insert(n.to_string(), actual.clone());
                    true
                }
                Ph::Var(n) => match vars.get(n) {
                    Some(v) => {
                        if v != actual {
                            diffs.push(format!(
                                "{at}: expected {} (captured as {n}), got {}",
                                show(v),
                                show(actual)
                            ));
                        }
                        return;
                    }
                    None => {
                        diffs.push(format!("{at}: {}", Missing::Var(n.into())));
                        return;
                    }
                },
                Ph::Env(v) => {
                    diffs.push(format!(
                        "{at}: {{{{env:{v}}}}} is for requests, not answers"
                    ));
                    return;
                }
            };
            if !ok {
                diffs.push(format!("{at}: expected {s}, got {}", show(actual)));
            }
            return;
        }
        // A string with placeholders inside: match as text.
        if s.contains("{{") && !parts(s).iter().all(|p| matches!(p, Part::Lit(_))) {
            match actual.as_str() {
                Some(a) => {
                    if let Err(d) = match_text(s, a, vars) {
                        diffs.push(format!("{at}: {d}"));
                    }
                }
                None => diffs.push(format!(
                    "{at}: expected a string like {}, got {}",
                    show(expected),
                    kind(actual)
                )),
            }
            return;
        }
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (k, ev) in e {
                match a.get(k) {
                    Some(av) => match_json(ev, av, &format!("{at}.{k}"), vars, diffs),
                    None if diffs.len() < 20 => {
                        diffs.push(format!("{at}: the field {k:?} is missing"))
                    }
                    None => {}
                }
            }
            for k in a.keys().filter(|k| !e.contains_key(*k)) {
                if diffs.len() < 20 {
                    diffs.push(format!("{at}: an unexpected field {k:?}"));
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                diffs.push(format!(
                    "{at}: expected a list of {}, got {}",
                    e.len(),
                    a.len()
                ));
            }
            for (i, (ev, av)) in e.iter().zip(a).enumerate() {
                match_json(ev, av, &format!("{at}[{i}]"), vars, diffs);
            }
        }
        (Value::Number(e), Value::Number(a)) => {
            // 1 and 1.0 are the same number.
            if e != a && e.as_f64() != a.as_f64() {
                diffs.push(format!("{at}: expected {e}, got {a}"));
            }
        }
        _ => {
            if expected != actual {
                if std::mem::discriminant(expected) == std::mem::discriminant(actual) {
                    diffs.push(format!(
                        "{at}: expected {}, got {}",
                        show(expected),
                        show(actual)
                    ));
                } else {
                    diffs.push(format!(
                        "{at}: expected {} {}, got {} {}",
                        kind(expected),
                        show(expected),
                        kind(actual),
                        show(actual)
                    ));
                }
            }
        }
    }
}

/// How far a class placeholder may reach from `from`: the ends to try.
fn class_char(ph: Ph<'_>, c: char) -> bool {
    match ph {
        Ph::Number => c.is_ascii_digit() || "+-.eE".contains(c),
        Ph::Uuid => c.is_ascii_hexdigit() || c == '-',
        Ph::Time => c.is_ascii_digit() || "-:.+TtZz ".contains(c),
        Ph::Hex => c.is_ascii_hexdigit(),
        _ => true,
    }
}

fn class_ok(ph: Ph<'_>, s: &str) -> bool {
    match ph {
        Ph::Any => true,
        Ph::Str | Ph::Capture(_) => !s.is_empty(),
        Ph::Number => s.bytes().any(|b| b.is_ascii_digit()) && s.parse::<f64>().is_ok(),
        Ph::Uuid => shape::is_uuid(s),
        Ph::Time => shape::is_time(s),
        Ph::Hex => shape::is_hex_id(s),
        Ph::Var(_) | Ph::Env(_) => false,
    }
}

struct Text<'p, 's> {
    parts: Vec<Part<'p>>,
    s: &'s str,
    failed: HashSet<(usize, usize)>,
    open: HashSet<(usize, usize)>,
    caps: Vec<(String, String)>,
}

impl Text<'_, '_> {
    fn go(&mut self, i: usize, pos: usize, vars: &Vars) -> bool {
        if i == self.parts.len() {
            return pos == self.s.len();
        }
        if self.failed.contains(&(i, pos)) {
            return false;
        }
        let ok = match self.parts[i].clone() {
            Part::Lit(l) => self.s[pos..].starts_with(l) && self.go(i + 1, pos + l.len(), vars),
            Part::Ph(Ph::Var(n)) => match vars.get(n) {
                Some(v) => {
                    let t = as_text(v);
                    self.s[pos..].starts_with(t.as_str()) && self.go(i + 1, pos + t.len(), vars)
                }
                None => false,
            },
            Part::Ph(Ph::Env(_)) => false,
            Part::Ph(ph) if matches!(ph, Ph::Any | Ph::Str | Ph::Capture(_)) => {
                // Unconstrained: try every end from the shortest. `open`
                // remembers (part, end) pairs already tried for this part,
                // whatever the start: trying them again cannot succeed, so
                // the work stays linear in the text per placeholder.
                let min = usize::from(!matches!(ph, Ph::Any));
                let mut ok = false;
                let mut end = pos;
                loop {
                    if end - pos >= min {
                        if self.open.contains(&(i, end)) {
                            break;
                        }
                        self.open.insert((i, end));
                        let mark = self.caps.len();
                        if let Ph::Capture(n) = ph {
                            self.caps
                                .push((n.to_string(), self.s[pos..end].to_string()));
                        }
                        if self.go(i + 1, end, vars) {
                            ok = true;
                            break;
                        }
                        self.caps.truncate(mark);
                    }
                    match self.s[end..].chars().next() {
                        Some(c) => end += c.len_utf8(),
                        None => break,
                    }
                }
                ok
            }
            Part::Ph(ph) => {
                // A class: only runs of its characters, never long.
                let cap = match ph {
                    Ph::Uuid => 36,
                    Ph::Time => 40,
                    Ph::Number => 64,
                    _ => 512,
                };
                let mut ends = Vec::new();
                for (off, c) in self.s[pos..].char_indices() {
                    if off >= cap || !class_char(ph, c) {
                        break;
                    }
                    ends.push(pos + off + c.len_utf8());
                }
                // Longest first: `{{number}}` before `.` should take `1.5`.
                let mut ok = false;
                for &end in ends.iter().rev() {
                    if class_ok(ph, &self.s[pos..end]) && self.go(i + 1, end, vars) {
                        ok = true;
                        break;
                    }
                }
                ok
            }
        };
        if !ok {
            self.failed.insert((i, pos));
        }
        ok
    }
}

/// Matches text against a template with placeholders, remembering captures.
pub fn match_text(template: &str, actual: &str, vars: &mut Vars) -> Result<(), String> {
    let ps = parts(template);
    if let Some(n) = ps.iter().find_map(|p| match p {
        Part::Ph(Ph::Var(n)) if !vars.contains_key(*n) => Some(*n),
        _ => None,
    }) {
        return Err(Missing::Var(n.into()).to_string());
    }
    let mut t = Text {
        parts: ps,
        s: actual,
        failed: HashSet::new(),
        open: HashSet::new(),
        caps: Vec::new(),
    };
    if t.go(0, 0, vars) {
        for (n, v) in t.caps {
            // A captured number stays a number, so `{{n}}` in a JSON body
            // sends a number.
            let value = match v.parse::<i64>() {
                Ok(i) if i.to_string() == v => Value::from(i),
                _ => Value::String(v),
            };
            vars.insert(n, value);
        }
        Ok(())
    } else {
        Err(format!(
            "expected text like {}, got {}",
            show(&Value::String(template.into())),
            show(&Value::String(actual.into()))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn placeholders_parse() {
        assert_eq!(whole("{{any}}"), Some(Ph::Any));
        assert_eq!(whole("{{capture:user_id}}"), Some(Ph::Capture("user_id")));
        assert_eq!(whole("{{env:TOKEN}}"), Some(Ph::Env("TOKEN")));
        assert_eq!(whole("{{user_id}}"), Some(Ph::Var("user_id")));
        assert_eq!(whole("{{capture:any}}"), None);
        assert_eq!(whole("{{a b}}"), None);
        assert_eq!(whole("{{}}"), None);
        assert_eq!(whole("{any}"), None);
        assert_eq!(whole("x{{any}}"), None);
        assert_eq!(
            parts("/u/{{id}}/x{{nope }}{{"),
            vec![
                Part::Lit("/u/"),
                Part::Ph(Ph::Var("id")),
                Part::Lit("/x{{nope }}{{")
            ]
        );
        assert_eq!(parts(""), vec![]);
    }

    #[test]
    fn json_matches_with_placeholders_and_captures() {
        let mut vars = Vars::new();
        let mut d = Vec::new();
        let exp =
            json!({"id": "{{capture:uid}}", "at": "{{time}}", "tags": ["a", "{{string}}"], "n": 1});
        let act = json!({"id": 7, "at": "2026-11-22T08:40:01Z", "tags": ["a", "b"], "n": 1.0});
        match_json(&exp, &act, "body", &mut vars, &mut d);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(vars["uid"], json!(7));
        let exp = json!({"owner": "{{uid}}"});
        match_json(&exp, &json!({"owner": 7}), "body", &mut vars, &mut d);
        assert!(d.is_empty(), "{d:?}");
        match_json(&exp, &json!({"owner": "7"}), "body", &mut vars, &mut d);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("captured as uid"), "{d:?}");
    }

    #[test]
    fn json_differences_name_the_place() {
        let mut vars = Vars::new();
        let mut d = Vec::new();
        let exp = json!({"a": {"b": [1, 2]}, "c": "x", "gone": true, "t": "{{time}}", "z": null});
        let act = json!({"a": {"b": [1, 3, 4]}, "c": 5, "new": 1, "t": "yesterday", "z": null});
        match_json(&exp, &act, "body", &mut vars, &mut d);
        let all = d.join("\n");
        assert!(
            all.contains("body.a.b: expected a list of 2, got 3"),
            "{all}"
        );
        assert!(all.contains("body.a.b[1]: expected 2, got 3"), "{all}");
        assert!(
            all.contains("body.c: expected a string \"x\", got a number 5"),
            "{all}"
        );
        assert!(all.contains("body: the field \"gone\" is missing"), "{all}");
        assert!(all.contains("body: an unexpected field \"new\""), "{all}");
        assert!(
            all.contains("body.t: expected {{time}}, got \"yesterday\""),
            "{all}"
        );
        assert_eq!(d.len(), 6, "{all}");
    }

    #[test]
    fn differences_are_capped() {
        let exp: Value = (0..100)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let mut d = Vec::new();
        match_json(&exp, &json!({}), "body", &mut Vars::new(), &mut d);
        assert_eq!(d.len(), 20);
    }

    #[test]
    fn unbound_variables_and_null_captures_are_differences() {
        let mut d = Vec::new();
        match_json(
            &json!("{{nope}}"),
            &json!(1),
            "body",
            &mut Vars::new(),
            &mut d,
        );
        assert!(d[0].contains("used before"), "{d:?}");
        let mut d = Vec::new();
        match_json(
            &json!("{{capture:x}}"),
            &Value::Null,
            "body",
            &mut Vars::new(),
            &mut d,
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn text_templates() {
        let mut vars = Vars::new();
        assert!(match_text("/users/{{capture:id}}", "/users/42", &mut vars).is_ok());
        assert_eq!(vars["id"], json!(42));
        assert!(match_text("/users/{{id}}/x", "/users/42/x", &mut vars).is_ok());
        assert!(match_text("/users/{{id}}/x", "/users/43/x", &mut vars).is_err());
        assert!(match_text("hello {{string}}!", "hello world!", &mut vars).is_ok());
        assert!(match_text("hello {{string}}!", "hello !", &mut vars).is_err());
        assert!(match_text("a{{any}}b", "ab", &mut vars).is_ok());
        assert!(match_text("v={{number}}.", "v=1.5.", &mut vars).is_ok());
        assert!(match_text("v={{number}}", "v=x", &mut vars).is_err());
        assert!(match_text("at {{time}} ok", "at 2026-11-22T08:40:01Z ok", &mut vars).is_ok());
        assert!(match_text(
            "{{uuid}}",
            "550e8400-e29b-41d4-a716-446655440000",
            &mut vars
        )
        .is_ok());
        assert!(match_text("{{uuid}}", "550e8400", &mut vars).is_err());
        assert!(match_text(
            "id {{hex}}",
            "id 5f3a9c2e8b41d706000100000000002a",
            &mut vars
        )
        .is_ok());
        assert!(match_text("plain", "plain", &mut vars).is_ok());
        assert!(match_text("plain", "plainer", &mut vars).is_err());
        assert!(match_text("{{nope}}", "x", &mut vars)
            .unwrap_err()
            .contains("used before"));
        // Repeated literals and backtracking.
        assert!(match_text("{{any}}-{{capture:last}}", "a-b-c", &mut vars).is_ok());
        // `{{any}}` takes as little as it can.
        assert_eq!(vars["last"], json!("b-c"));
        assert!(match_text("é{{string}}ü", "éxyzü", &mut vars).is_ok());
    }

    #[test]
    fn text_matching_stays_fast_on_bad_input() {
        // Many `{{any}}` against a long text that fails at the end: the
        // memo keeps this polynomial (milliseconds here). Without it the
        // search is about 4000^12 steps and never ends, so the bound can be
        // generous enough for ThreadSanitizer and slow machines.
        let template = "{{any}}a".repeat(12) + "!";
        let text = "a".repeat(4_000);
        let start = std::time::Instant::now();
        assert!(match_text(&template, &text, &mut Vars::new()).is_err());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn requests_are_filled() {
        let mut vars = Vars::new();
        vars.insert("id".into(), json!(7));
        vars.insert("name".into(), json!("a b/c"));
        let env = |k: &str| (k == "TOKEN").then(|| "Bearer xyz".to_string());
        assert_eq!(
            fill_text("/u/{{id}}/{{name}}", &vars, &env, true).unwrap(),
            "/u/7/a%20b%2Fc"
        );
        assert_eq!(
            fill_text("{{env:TOKEN}}", &vars, &env, false).unwrap(),
            "Bearer xyz"
        );
        assert_eq!(
            fill_text("{{env:NOPE}}", &vars, &no_env, false),
            Err(Missing::Env("NOPE".into()))
        );
        assert_eq!(
            fill_text("/u/{{x}}", &vars, &no_env, true),
            Err(Missing::Var("x".into()))
        );
        assert!(matches!(
            fill_text("{{uuid}}", &vars, &no_env, true),
            Err(Missing::Class(_))
        ));
        let body = json!({"owner": "{{id}}", "note": "for {{name}}", "list": ["{{id}}"], "n": 1});
        assert_eq!(
            fill_json(&body, &vars, &no_env).unwrap(),
            json!({"owner": 7, "note": "for a b/c", "list": [7], "n": 1})
        );
    }
}
