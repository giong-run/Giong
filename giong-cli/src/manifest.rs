//! The giong dependencies written in a Cargo.toml, and the edit that moves
//! their versions, touching nothing else: comments, order, spacing, features
//! and every other dependency stay byte for byte.
//!
//! Read line by line, as TOML allows here: a dependency is one `key = value`
//! line in a dependency table (`[dependencies]`, `[dev-dependencies]`,
//! `[build-dependencies]`, their `[target.'cfg(..)'.*]` forms and
//! `[workspace.dependencies]`), with an inline table on one line (TOML 1.0
//! has no other kind), or a table of its own (`[dependencies.giong]`), or
//! dotted keys (`giong.version = "1.5"`). A form this does not read is caught
//! after the edit: `giong upgrade` asks cargo for every requirement again.
//!
//! Also version requirements as cargo reads them ([`parse_req`], [`allows`])
//! and the requirement that moves one to a new release ([`moved_req`]).

use std::ops::Range;

use crate::host::Version;

/// One comparator of a version requirement: `^1.4`, `~1.4.2`, `>=1.4`, `=1.5.0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Caret,
    Tilde,
    Exact,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    /// `*`, `1.*`: what is written before the star.
    Wildcard,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Comparator {
    pub op: Op,
    /// The parts written: `1.4` is `[1, 4]`.
    pub parts: [u32; 3],
    pub count: usize,
}

/// A requirement as cargo reads it (`"1.4"` is `^1.4`); `None` when it is
/// not one this understands (a pre-release, a stray word).
pub fn parse_req(req: &str) -> Option<Vec<Comparator>> {
    let mut out = Vec::new();
    for item in req.split(',') {
        let item = item.trim();
        let (op, rest) = if let Some(r) = item.strip_prefix(">=") {
            (Op::GreaterEq, r)
        } else if let Some(r) = item.strip_prefix("<=") {
            (Op::LessEq, r)
        } else if let Some(r) = item.strip_prefix('>') {
            (Op::Greater, r)
        } else if let Some(r) = item.strip_prefix('<') {
            (Op::Less, r)
        } else if let Some(r) = item.strip_prefix('=') {
            (Op::Exact, r)
        } else if let Some(r) = item.strip_prefix('~') {
            (Op::Tilde, r)
        } else if let Some(r) = item.strip_prefix('^') {
            (Op::Caret, r)
        } else {
            (Op::Caret, item)
        };
        let rest = rest.trim();
        let mut parts = [0u32; 3];
        let mut count = 0;
        let mut wildcard = false;
        for (i, p) in rest.split('.').enumerate() {
            if i >= 3 {
                return None;
            }
            if p == "*" || p == "x" || p == "X" {
                wildcard = true;
                break;
            }
            parts[i] = p.parse().ok()?;
            count = i + 1;
        }
        if wildcard {
            out.push(Comparator {
                op: Op::Wildcard,
                parts,
                count,
            });
        } else if count == 0 {
            return None;
        } else {
            out.push(Comparator { op, parts, count });
        }
    }
    Some(out)
}

/// Whether `v` meets every comparator, as cargo decides it (no pre-releases).
pub fn allows(req: &[Comparator], v: Version) -> bool {
    req.iter().all(|c| comparator_allows(c, v))
}

fn comparator_allows(c: &Comparator, v: Version) -> bool {
    let v = [v.0, v.1, v.2];
    let low = c.parts;
    // The first version above everything the written prefix covers.
    let prefix_end = |n: usize| -> [u32; 3] {
        let mut e = [0; 3];
        e[..n].copy_from_slice(&low[..n]);
        e[n - 1] += 1;
        e
    };
    match c.op {
        Op::Exact => v[..c.count] == low[..c.count],
        Op::Greater => {
            if c.count == 3 {
                v > low
            } else {
                v >= prefix_end(c.count)
            }
        }
        Op::GreaterEq => v >= low,
        Op::Less => v < low,
        Op::LessEq => {
            if c.count == 3 {
                v <= low
            } else {
                v < prefix_end(c.count)
            }
        }
        Op::Wildcard => c.count == 0 || v[..c.count] == low[..c.count],
        Op::Tilde => {
            let end = prefix_end(if c.count == 1 { 1 } else { 2 });
            v >= low && v < end
        }
        Op::Caret => {
            // The first non-zero part written stays.
            let keep = if low[0] > 0 || c.count == 1 {
                1
            } else if low[1] > 0 || c.count == 2 {
                2
            } else {
                3
            };
            v >= low && v < prefix_end(keep)
        }
    }
}

/// The requirement that moves `old` to `target` the way the person wrote it:
/// `"1.4"` becomes `"1.5"`, `"^1.4.2"` becomes `"^1.5.0"`, `"~1.4"`
/// becomes `"~1.5"`. `None` when `old` is not one plain caret or tilde
/// requirement (`=1.4.2`, `>=1.4, <1.6`): those are left as written, and
/// refused when they do not allow `target`.
pub fn moved_req(old: &str, target: Version) -> Option<String> {
    let req = parse_req(old)?;
    let [c] = req.as_slice() else { return None };
    let prefix = match c.op {
        Op::Caret if old.trim_start().starts_with('^') => "^",
        Op::Caret => "",
        Op::Tilde => "~",
        _ => return None,
    };
    let t = [target.0, target.1, target.2];
    let parts: Vec<String> = t[..c.count].iter().map(u32::to_string).collect();
    Some(format!("{prefix}{}", parts.join(".")))
}

/// What a dependency says about where it comes from.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Dep {
    /// The crate: `package = "..."` when renamed, else the key.
    pub crate_name: String,
    /// The key it is written under.
    pub key: String,
    /// The table it is in, as written in the header (`dependencies`,
    /// `target.'cfg(unix)'.dev-dependencies`, `workspace.dependencies`).
    pub table: String,
    /// The line of its version (or of its key, without one), 1-based.
    pub line: u32,
    /// The version requirement, as written.
    pub req: Option<String>,
    /// The bytes of the requirement inside its quotes.
    pub req_span: Option<Range<usize>>,
    pub path: bool,
    pub git: bool,
    /// `workspace = true`: the version is in `[workspace.dependencies]`.
    pub workspace: bool,
}

impl Dep {
    /// Whether the version comes from a registry, so a release moves it.
    pub fn is_registry(&self) -> bool {
        !self.path && !self.git && !self.workspace
    }
}

/// Whether a table holds dependencies; for `[dependencies.giong]`, the key
/// of the dependency the table is.
fn dep_table(header: &str) -> Option<(String, Option<String>)> {
    let parts = split_dotted(header);
    let is_deps = |p: &str| {
        matches!(
            p,
            "dependencies"
                | "dev-dependencies"
                | "dev_dependencies"
                | "build-dependencies"
                | "build_dependencies"
        )
    };
    let (table_len, ok) = match parts.as_slice() {
        [d, ..] if is_deps(d) => (1, true),
        [w, d, ..] if w == "workspace" && d == "dependencies" => (2, true),
        [t, _, d, ..] if t == "target" && is_deps(d) => (3, true),
        _ => (0, false),
    };
    if !ok {
        return None;
    }
    let table = parts[..table_len].join(".");
    match parts.len() - table_len {
        0 => Some((table, None)),
        1 => Some((table, Some(unquote(&parts[table_len]).to_string()))),
        _ => None,
    }
}

/// `target.'cfg(a.b)'.dependencies` split at the dots outside quotes.
fn split_dotted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    for c in s.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, '.') => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
}

/// The end of the line's content: before a `#` outside a string.
fn content_end(line: &str) -> usize {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') => return i,
            _ => {}
        }
    }
    line.len()
}

/// `key = value` at top level of `s` (the first `=` outside quotes), with
/// the value's byte range in `s`.
fn key_value(s: &str) -> Option<(String, Range<usize>)> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '=') => {
                let key = s[..i].trim();
                if key.is_empty() {
                    return None;
                }
                let start = i + 1 + (s[i + 1..].len() - s[i + 1..].trim_start().len());
                let end = start + s[start..].trim_end().len();
                return Some((key.to_string(), start..end));
            }
            _ => {}
        }
    }
    None
}

/// The range of a string's content (inside its quotes), if `value` is one
/// basic or literal string.
///
/// One string: a quote of its kind inside ends it, so `"1.4" } x = ["a"`
/// (an inline table cut at a stray `}`) is no requirement. Taken as one,
/// its edit replaced the bytes between two strings, and the file read back
/// with other values in them. A requirement never holds a quote, so the
/// escaped `\"` that a basic string may hold is refused too.
fn string_content(value: &str) -> Option<Range<usize>> {
    let q = value.chars().next()?;
    if (q == '"' || q == '\'') && value.len() >= 2 && value.ends_with(q) {
        let inner = 1..value.len() - 1;
        (!value[inner.clone()].contains(q)).then_some(inner)
    } else {
        None
    }
}

/// The `key = value` pairs of a one-line inline table `{ ... }`, with each
/// value's range in `value`.
fn inline_pairs(value: &str) -> Option<Vec<(String, Range<usize>)>> {
    let inner_start = value.find('{')? + 1;
    let inner_end = value.rfind('}')?;
    if inner_end < inner_start {
        return None;
    }
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote = None;
    let mut start = inner_start;
    let bytes: Vec<(usize, char)> = value[inner_start..inner_end]
        .char_indices()
        .map(|(i, c)| (i + inner_start, c))
        .collect();
    let push = |from: usize, to: usize, out: &mut Vec<(String, Range<usize>)>| {
        let item = &value[from..to];
        if let Some((k, r)) = key_value(item) {
            out.push((k, from + r.start..from + r.end));
        }
    };
    for (i, c) in bytes {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                push(start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(start, inner_end, &mut out);
    Some(out)
}

/// What one field says about the dependency.
fn absorb(dep: &mut Dep, field: &str, value: &str, value_at: usize, line: u32) {
    match field {
        "version" => {
            if let Some(r) = string_content(value) {
                dep.req = Some(value[r.clone()].to_string());
                dep.req_span = Some(value_at + r.start..value_at + r.end);
                dep.line = line;
            }
        }
        "package" => dep.crate_name = unquote(value).to_string(),
        "path" => dep.path = true,
        "git" => dep.git = true,
        "workspace" => dep.workspace = value.trim() == "true",
        _ => {}
    }
}

/// Every dependency of `text`, in the order written.
pub fn scan(text: &str) -> Vec<Dep> {
    let mut deps: Vec<Dep> = Vec::new();
    // The dependency table we are in, and the key when it is a table of one.
    let mut table: Option<(String, Option<String>)> = None;
    let mut offset = 0;
    for (n, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = n as u32 + 1;
        let start = offset;
        offset += raw.len();
        let line = &raw[..content_end(raw)];
        let lead = line.len() - line.trim_start().len();
        let body = line.trim();
        if body.is_empty() {
            continue;
        }
        if body.starts_with('[') {
            let inner = body
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            table = if body.starts_with("[[") {
                None
            } else {
                dep_table(&inner)
            };
            if let Some((t, Some(key))) = &table {
                deps.push(Dep {
                    crate_name: key.clone(),
                    key: key.clone(),
                    table: t.clone(),
                    line: line_no,
                    ..Dep::default()
                });
            }
            continue;
        }
        let Some((t, single)) = &table else { continue };
        let Some((key, vr)) = key_value(body) else {
            continue;
        };
        let value = &body[vr.clone()];
        let value_at = start + lead + vr.start;
        match single {
            Some(dep_key) => {
                // `[dependencies.giong]` then `version = "1.5"`.
                let dep = deps
                    .iter_mut()
                    .rev()
                    .find(|d| &d.key == dep_key && &d.table == t)
                    .expect("pushed with its header");
                absorb(dep, unquote(&key), value, value_at, line_no);
            }
            None => {
                let parts = split_dotted(&key);
                let dep_key = unquote(&parts[0]).to_string();
                let existing = deps.iter().rposition(|d| d.key == dep_key && &d.table == t);
                let idx = match existing {
                    Some(i) if parts.len() > 1 => i,
                    _ => {
                        deps.push(Dep {
                            crate_name: dep_key.clone(),
                            key: dep_key.clone(),
                            table: t.clone(),
                            line: line_no,
                            ..Dep::default()
                        });
                        deps.len() - 1
                    }
                };
                let dep = &mut deps[idx];
                if parts.len() > 1 {
                    // `giong.version = "1.5"`
                    absorb(dep, unquote(&parts[1]), value, value_at, line_no);
                } else if value.starts_with('{') {
                    for (field, r) in inline_pairs(value).unwrap_or_default() {
                        absorb(dep, &field, &value[r.clone()], value_at + r.start, line_no);
                    }
                } else {
                    absorb(dep, "version", value, value_at, line_no);
                }
            }
        }
    }
    deps
}

/// One version to change in a file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Change {
    pub dep: Dep,
    pub new_req: String,
}

/// `text` with each change's requirement replaced.
pub fn apply(text: &str, changes: &[Change]) -> String {
    let mut spans: Vec<(&Range<usize>, &str)> = changes
        .iter()
        .filter_map(|c| Some((c.dep.req_span.as_ref()?, c.new_req.as_str())))
        .collect();
    spans.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (r, new) in spans {
        out.replace_range(r.clone(), new);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        crate::host::parse_version(s).unwrap()
    }

    #[test]
    fn requirements_are_read_as_cargo_reads_them() {
        let ok = |r: &str, ver: &str| allows(&parse_req(r).unwrap(), v(ver));
        assert!(ok("1.4", "1.5.0"));
        assert!(ok("1.4.2", "1.9.3"));
        assert!(!ok("1.4.2", "1.4.1"));
        assert!(!ok("1.4", "2.0.0"));
        assert!(ok("1", "1.99.0"));
        assert!(ok("^0.3.1", "0.3.9"));
        assert!(!ok("^0.3.1", "0.4.0"));
        assert!(ok("~1.4", "1.4.9"));
        assert!(!ok("~1.4", "1.5.0"));
        assert!(ok("~1", "1.9.0"));
        assert!(ok("=1.4", "1.4.7"));
        assert!(!ok("=1.4.2", "1.5.0"));
        assert!(ok(">=1.4, <1.6", "1.5.0"));
        assert!(!ok(">=1.4, <1.6", "1.6.0"));
        assert!(ok(">1.4", "1.5.0"));
        assert!(!ok(">1.4", "1.4.9"));
        assert!(ok("<=1.5", "1.5.9"));
        assert!(ok("*", "1.5.0"));
        assert!(ok("1.*", "1.5.0"));
        assert!(!ok("1.4.*", "1.5.0"));
        assert!(parse_req("1.5.0-rc.1").is_none());
        assert!(parse_req("latest").is_none());
    }

    #[test]
    fn a_requirement_moves_in_the_shape_it_was_written() {
        let t = v("1.5.0");
        assert_eq!(moved_req("1.4", t).as_deref(), Some("1.5"));
        assert_eq!(moved_req("1.4.2", t).as_deref(), Some("1.5.0"));
        assert_eq!(moved_req("^1.4.2", t).as_deref(), Some("^1.5.0"));
        assert_eq!(moved_req("~1.4", t).as_deref(), Some("~1.5"));
        assert_eq!(moved_req("1", t).as_deref(), Some("1"));
        assert_eq!(moved_req(" 1.4 ", t).as_deref(), Some("1.5"));
        assert_eq!(moved_req("=1.4.2", t), None);
        assert_eq!(moved_req(">=1.4, <1.6", t), None);
    }

    const SHAPES: &str = "\
[package]
name = \"app\"
version = \"0.1.0\" # not a dependency

[dependencies]
giong = \"1.4.2\" # the runtime
giong-web = { version = \"1.4\", features = [\"tracing\", \"dev\"] }
serde = \"1\"
rt = { package = 'giong-http', version = '~1.4', default-features = false }
giong-mcp.version = \"1.4.0\"
giong-mcp.features = [\"web\"]
local = { path = \"../local\" }
giong-tls = { path = \"../giong/giong-tls\", version = \"1.4\" }

[dev-dependencies.giong-postgres]
version = \"^1.4.2\"
features = [\"tls\"]

[target.'cfg(target_os = \"linux\")'.dependencies]
giong-redis = { workspace = true }

[workspace.dependencies]
giong-redis = \"=1.4.2\"

[[bin]]
name = \"giong\"
";

    #[test]
    fn every_shape_of_dependency_is_read() {
        let deps = scan(SHAPES);
        let find = |key: &str, table: &str| {
            deps.iter()
                .find(|d| d.key == key && d.table == table)
                .unwrap_or_else(|| panic!("{key} in {table}: {deps:#?}"))
        };
        let g = find("giong", "dependencies");
        assert_eq!(g.req.as_deref(), Some("1.4.2"));
        assert_eq!(&SHAPES[g.req_span.clone().unwrap()], "1.4.2");
        assert_eq!(g.line, 6);
        let w = find("giong-web", "dependencies");
        assert_eq!(w.req.as_deref(), Some("1.4"));
        let rt = find("rt", "dependencies");
        assert_eq!(rt.crate_name, "giong-http");
        assert_eq!(rt.req.as_deref(), Some("~1.4"));
        assert_eq!(&SHAPES[rt.req_span.clone().unwrap()], "~1.4");
        let mcp = find("giong-mcp", "dependencies");
        assert_eq!(mcp.req.as_deref(), Some("1.4.0"));
        assert_eq!(deps.iter().filter(|d| d.key == "giong-mcp").count(), 1);
        assert!(find("giong-tls", "dependencies").path);
        assert!(!find("giong-tls", "dependencies").is_registry());
        let pg = find("giong-postgres", "dev-dependencies");
        assert_eq!(pg.req.as_deref(), Some("^1.4.2"));
        assert_eq!(pg.line, 16);
        let r = find(
            "giong-redis",
            "target.'cfg(target_os = \"linux\")'.dependencies",
        );
        assert!(r.workspace && r.req.is_none());
        let wr = find("giong-redis", "workspace.dependencies");
        assert_eq!(wr.req.as_deref(), Some("=1.4.2"));
        assert!(!deps.iter().any(|d| d.key == "name" || d.key == "version"));
    }

    #[test]
    fn an_edit_changes_only_the_versions() {
        let deps = scan(SHAPES);
        let changes: Vec<Change> = deps
            .iter()
            .filter(|d| d.crate_name.starts_with("giong") && d.is_registry() && d.req.is_some())
            .filter_map(|d| {
                Some(Change {
                    dep: d.clone(),
                    new_req: moved_req(d.req.as_deref()?, Version(1, 5, 0))?,
                })
            })
            .collect();
        let out = apply(SHAPES, &changes);
        let want = SHAPES
            .replace("giong = \"1.4.2\"", "giong = \"1.5.0\"")
            .replace("version = \"1.4\", features", "version = \"1.5\", features")
            .replace("version = '~1.4'", "version = '~1.5'")
            .replace(
                "giong-mcp.version = \"1.4.0\"",
                "giong-mcp.version = \"1.5.0\"",
            )
            .replace("version = \"^1.4.2\"", "version = \"^1.5.0\"");
        assert_eq!(out, want);
        // Read again, the edit is where it was.
        let again = scan(&out);
        assert_eq!(again.len(), deps.len());
    }

    #[test]
    fn crlf_and_comments_and_odd_spacing() {
        let text = "[dependencies]\r\n  giong   =   \"1.4\"   # pinned for now\r\n\"giong-web\" = {version=\"1.4.1\"}\r\n";
        let deps = scan(text);
        assert_eq!(deps.len(), 2, "{deps:?}");
        assert_eq!(&text[deps[0].req_span.clone().unwrap()], "1.4");
        assert_eq!(deps[1].key, "giong-web");
        assert_eq!(&text[deps[1].req_span.clone().unwrap()], "1.4.1");
        let out = apply(
            text,
            &[
                Change {
                    dep: deps[0].clone(),
                    new_req: "1.5".into(),
                },
                Change {
                    dep: deps[1].clone(),
                    new_req: "1.5.0".into(),
                },
            ],
        );
        assert_eq!(
            out,
            "[dependencies]\r\n  giong   =   \"1.5\"   # pinned for now\r\n\"giong-web\" = {version=\"1.5.0\"}\r\n"
        );
    }

    #[test]
    fn a_requirement_is_one_string() {
        // A stray `}` ends the inline table early for the reader, and the
        // comma inside the list after it splits the rest: `version` was
        // then read as `1.4" }tures = ["a`, and editing it took out the
        // bytes between the strings. Found by the manifest fuzz target.
        let text = "[dependencies]\nrt = { version = \"1.4\" }tures = [\"a\", \"b\"] }\n";
        let deps = scan(text);
        assert_eq!(deps.len(), 1, "{deps:?}");
        assert_eq!(deps[0].req, None, "{deps:?}");
        assert_eq!(deps[0].req_span, None);
        for value in ["\"1.4\" \"1.5\"", "'1.4' x '2'", "\"1\\\"2\""] {
            assert_eq!(string_content(value), None, "{value}");
        }
        assert_eq!(string_content("\"1.4\""), Some(1..4));
        assert_eq!(string_content("'~1.4'"), Some(1..5));
        assert_eq!(string_content("\"a'b\""), Some(1..4));
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let text = "[dependencies]\ngiong = { git = \"https://x/y#main\", version = \"1.4\" }\n";
        let deps = scan(text);
        assert!(deps[0].git);
        assert_eq!(deps[0].req.as_deref(), Some("1.4"));
    }
}
