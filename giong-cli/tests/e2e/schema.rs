//! The `--json` contract of every command, written out as a checker.
//!
//! `src/report.rs` documents one document shape for every command. This file
//! is that documentation as code, strict on purpose: a field that is added to
//! the output must be added here too, and a field may never change its type
//! or meaning ("add, never change meaning"). The `data` object is the
//! command's own and is only checked to be an object; commands check their
//! own `data` in their test files.

use serde_json::{Map, Value};

/// Top-level keys of a report, and whether each must be present.
const REPORT_KEYS: &[(&str, bool)] = &[
    ("command", true),
    ("version", true),
    ("ok", true),
    ("findings", true),
    ("data", true),
    // Only when the command could not do its job (exit status 2).
    ("error", false),
];

const ERROR_KEYS: &[&str] = &["code", "message", "fix", "url"];

/// Keys of a finding, and whether each must be present.
const FINDING_KEYS: &[(&str, bool)] = &[
    ("code", true),
    ("severity", true),
    ("title", true),
    ("detail", true),
    ("fix", true),
    ("url", true),
    ("fixable", true),
    // A place in the code: all three or none.
    ("file", false),
    ("line", false),
    ("column", false),
    // When `fixable`.
    ("edits", false),
];

const EDIT_KEYS: &[&str] = &["file", "start_byte", "end_byte", "replacement"];

/// stdout must be exactly one JSON document followed by one newline: agents
/// read it with a single `json.loads`.
pub fn parse_single_document(stdout: &str) -> Result<Value, String> {
    if stdout.is_empty() {
        return Err("stdout is empty; --json must print one JSON document".into());
    }
    if !stdout.ends_with('\n') {
        return Err("the JSON document does not end with a newline".into());
    }
    let body = stdout.strip_suffix('\n').unwrap();
    let body = body.strip_suffix('\r').unwrap_or(body);
    if body.contains('\n') {
        return Err(
            "stdout holds more than one line; --json prints one document on one line".into(),
        );
    }
    serde_json::from_str(body).map_err(|e| format!("stdout is not JSON: {e}"))
}

/// Checks a report against the contract.
pub fn check_report(v: &Value, command: &str) -> Result<(), String> {
    let obj = v.as_object().ok_or("the document is not an object")?;
    keys(obj, REPORT_KEYS, "report")?;
    let cmd = str_field(obj, "command")?;
    if cmd != command {
        return Err(format!("\"command\" is {cmd:?}, want {command:?}"));
    }
    let version = str_field(obj, "version")?;
    if version != crate::harness::VERSION {
        return Err(format!(
            "\"version\" is {version:?}, want {:?}",
            crate::harness::VERSION
        ));
    }
    let ok = obj["ok"].as_bool().ok_or("\"ok\" is not a boolean")?;
    let findings = obj["findings"]
        .as_array()
        .ok_or("\"findings\" is not an array")?;
    if !obj["data"].is_object() {
        return Err("\"data\" is not an object".into());
    }
    let mut any_error = false;
    for (i, f) in findings.iter().enumerate() {
        check_finding(f).map_err(|e| format!("findings[{i}]: {e}"))?;
        any_error |= f["severity"] == "error";
    }
    if let Some(err) = obj.get("error") {
        let e = err.as_object().ok_or("\"error\" is not an object")?;
        for k in ERROR_KEYS {
            let s = str_field(e, k).map_err(|m| format!("error: {m}"))?;
            if s.is_empty() {
                return Err(format!("error.{k} is empty"));
            }
        }
        for k in e.keys() {
            if !ERROR_KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "error has an undocumented key {k:?}; add it to tests/e2e/schema.rs"
                ));
            }
        }
        check_code(e["code"].as_str().unwrap())?;
        check_url(e["url"].as_str().unwrap(), e["code"].as_str().unwrap())?;
        if ok {
            return Err("\"ok\" is true but the document has an \"error\"".into());
        }
    } else if ok == any_error {
        return Err(format!(
            "\"ok\" is {ok} but {} finding is an error",
            if any_error { "a" } else { "no" }
        ));
    }
    Ok(())
}

/// The exit status a document promises: 2 with an `error`, 1 with an error
/// finding, else 0.
pub fn exit_code_for(v: &Value) -> i32 {
    if v.get("error").is_some() {
        2
    } else if v["ok"] == false {
        1
    } else {
        0
    }
}

pub fn check_finding(f: &Value) -> Result<(), String> {
    let obj = f.as_object().ok_or("not an object")?;
    keys(obj, FINDING_KEYS, "finding")?;
    let code = str_field(obj, "code")?;
    check_code(code)?;
    let sev = str_field(obj, "severity")?;
    if !matches!(sev, "info" | "warning" | "error") {
        return Err(format!("severity {sev:?} is not info, warning or error"));
    }
    if str_field(obj, "title")?.trim().is_empty() {
        return Err("the title is empty".into());
    }
    str_field(obj, "detail")?;
    let fix = str_field(obj, "fix")?;
    if sev != "info" && fix.trim().is_empty() {
        return Err(format!(
            "{code}: a {sev} has no fix; every problem says what to do"
        ));
    }
    check_url(str_field(obj, "url")?, code)?;
    let place: Vec<bool> = ["file", "line", "column"]
        .iter()
        .map(|k| obj.contains_key(*k))
        .collect();
    if place.iter().any(|p| *p) {
        if !place.iter().all(|p| *p) {
            return Err("file, line and column come together".into());
        }
        if str_field(obj, "file")?.is_empty() {
            return Err("file is empty".into());
        }
        let line = obj["line"].as_u64().ok_or("line is not a number")?;
        let col = obj["column"].as_u64().ok_or("column is not a number")?;
        if line == 0 || col == 0 {
            return Err(format!("line {line} column {col}: both count from 1"));
        }
    }
    let fixable = obj["fixable"].as_bool().ok_or("fixable is not a boolean")?;
    match obj.get("edits") {
        None if fixable => return Err("fixable is true but there are no edits".into()),
        None => {}
        Some(edits) => {
            let edits = edits.as_array().ok_or("edits is not an array")?;
            if !fixable {
                return Err("edits are given but fixable is false".into());
            }
            if edits.is_empty() {
                return Err("edits is empty; leave it out instead".into());
            }
            for (i, e) in edits.iter().enumerate() {
                let o = e
                    .as_object()
                    .ok_or_else(|| format!("edits[{i}] is not an object"))?;
                for k in o.keys() {
                    if !EDIT_KEYS.contains(&k.as_str()) {
                        return Err(format!("edits[{i}] has an undocumented key {k:?}"));
                    }
                }
                let file = str_field(o, "file").map_err(|m| format!("edits[{i}]: {m}"))?;
                if file.is_empty() || std::path::Path::new(file).is_absolute() {
                    return Err(format!(
                        "edits[{i}].file {file:?} must be relative to the project"
                    ));
                }
                let s = o["start_byte"]
                    .as_u64()
                    .ok_or_else(|| format!("edits[{i}].start_byte is not a number"))?;
                let en = o["end_byte"]
                    .as_u64()
                    .ok_or_else(|| format!("edits[{i}].end_byte is not a number"))?;
                if s > en {
                    return Err(format!("edits[{i}]: start_byte {s} > end_byte {en}"));
                }
                str_field(o, "replacement").map_err(|m| format!("edits[{i}]: {m}"))?;
            }
        }
    }
    Ok(())
}

/// `GIONG-E` and four digits.
pub fn check_code(code: &str) -> Result<(), String> {
    let digits = code
        .strip_prefix("GIONG-E")
        .ok_or_else(|| format!("code {code:?} does not start with GIONG-E"))?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("code {code:?} is not GIONG-E and four digits"));
    }
    Ok(())
}

/// The page of the code, at the tag of this version.
fn check_url(url: &str, code: &str) -> Result<(), String> {
    let want = format!(
        "https://github.com/giong-run/Giong/blob/v{}/docs/errors/{}.md",
        crate::harness::VERSION,
        code.trim_start_matches("GIONG-")
    );
    if url != want {
        return Err(format!("url {url:?}, want {want:?}"));
    }
    Ok(())
}

fn keys(obj: &Map<String, Value>, spec: &[(&str, bool)], what: &str) -> Result<(), String> {
    for (k, required) in spec {
        if *required && !obj.contains_key(*k) {
            return Err(format!("the {what} has no {k:?}"));
        }
    }
    for k in obj.keys() {
        if !spec.iter().any(|(s, _)| s == k) {
            return Err(format!(
                "the {what} has an undocumented key {k:?}: document it in src/report.rs and \
                 add it to tests/e2e/schema.rs"
            ));
        }
    }
    Ok(())
}

fn str_field<'a>(obj: &'a Map<String, Value>, k: &str) -> Result<&'a str, String> {
    obj.get(k)
        .ok_or_else(|| format!("no {k:?}"))?
        .as_str()
        .ok_or_else(|| format!("{k:?} is not a string"))
}

#[test]
fn the_checker_rejects_what_it_should() {
    use serde_json::json;
    let good = json!({"command": "x", "version": crate::harness::VERSION, "ok": true,
                      "findings": [], "data": {}});
    check_report(&good, "x").unwrap();
    assert!(check_report(&good, "y").is_err());
    let mut extra = good.clone();
    extra["surprise"] = json!(1);
    assert!(check_report(&extra, "x").is_err());
    let mut lie = good.clone();
    lie["findings"] = json!([{"code": "GIONG-E5000", "severity": "error", "title": "t",
        "detail": "", "fix": "f", "url": format!(
            "https://github.com/giong-run/Giong/blob/v{}/docs/errors/E5000.md",
            crate::harness::VERSION), "fixable": false}]);
    assert!(check_report(&lie, "x").unwrap_err().contains("ok"));
    lie["ok"] = json!(false);
    check_report(&lie, "x").unwrap();
    assert_eq!(exit_code_for(&lie), 1);
    lie["findings"][0]["line"] = json!(3);
    assert!(check_report(&lie, "x").unwrap_err().contains("together"));
    assert!(parse_single_document("{}\n{}\n").is_err());
    assert!(parse_single_document("{}").is_err());
    assert!(parse_single_document("{}\n").is_ok());
}
