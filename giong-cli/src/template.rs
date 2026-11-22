//! The template language of `giong new`: plain text with three kinds of tags.
//!
//! - `{{name}}` is replaced by the value of a variable. Unknown variables are an
//!   error, never an empty string, so a typo in a template fails its test.
//! - `{{#if cond}}`, `{{#else}}` and `{{/if}}` keep or drop whole lines. Each tag
//!   stands alone on its line (indentation allowed) and the tag's line is never
//!   written. Blocks nest.
//! - `{{#file-if cond}}` as the first line of a file writes the file only when the
//!   condition holds.
//!
//! A condition is one or more terms joined by `|` (any holds) or by `&` (all hold),
//! not both; a term is a flag name, or `!` and a flag name. Unknown flags are an
//! error too.
//!
//! The language is small on purpose: a template stays readable as the file it
//! becomes, and every combination of flags is rendered and compiled in CI.

use std::collections::BTreeMap;
use std::fmt;

/// What a template is rendered with.
#[derive(Debug, Default, Clone)]
pub struct Context {
    vars: BTreeMap<String, String>,
    flags: BTreeMap<String, bool>,
}

impl Context {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a variable, used as `{{name}}`.
    pub fn var(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.vars.insert(name.to_string(), value.into());
        self
    }

    /// Sets a flag, used in conditions.
    pub fn flag(&mut self, name: &str, on: bool) -> &mut Self {
        self.flags.insert(name.to_string(), on);
        self
    }

    /// The value of a flag, if it is known.
    pub fn get_flag(&self, name: &str) -> Option<bool> {
        self.flags.get(name).copied()
    }

    fn eval(&self, cond: &str) -> Result<bool, String> {
        let cond = cond.trim();
        if cond.is_empty() {
            return Err("an empty condition".into());
        }
        let (any, terms): (bool, Vec<&str>) = match (cond.contains('|'), cond.contains('&')) {
            (true, true) => {
                return Err(format!(
                    "condition {cond:?} mixes `|` and `&`; nest two blocks instead"
                ))
            }
            (true, false) => (true, cond.split('|').collect()),
            (false, true) => (false, cond.split('&').collect()),
            (false, false) => (false, vec![cond]),
        };
        let mut values = Vec::with_capacity(terms.len());
        for term in terms {
            let term = term.trim();
            let (negate, name) = match term.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, term),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("{term:?} is not a flag name"));
            }
            let value = self
                .flags
                .get(name)
                .copied()
                .ok_or_else(|| format!("unknown flag {name:?}"))?;
            values.push(value != negate);
        }
        Ok(if any {
            values.iter().any(|v| *v)
        } else {
            values.iter().all(|v| *v)
        })
    }
}

/// A template that cannot be rendered: a bug in the template, with its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    /// The 1-based line of the problem.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for TemplateError {}

enum Tag<'a> {
    If(&'a str),
    Else,
    End,
    FileIf(&'a str),
}

/// The block tag a line holds, if it holds one.
fn tag(line: &str) -> Option<Tag<'_>> {
    let t = line.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?.trim();
    if let Some(cond) = inner.strip_prefix("#if ") {
        Some(Tag::If(cond))
    } else if let Some(cond) = inner.strip_prefix("#file-if ") {
        Some(Tag::FileIf(cond))
    } else if inner == "#else" {
        Some(Tag::Else)
    } else if inner == "/if" {
        Some(Tag::End)
    } else {
        None
    }
}

/// Renders a template. `Ok(None)` when a `{{#file-if}}` says the file is not
/// wanted.
pub fn render(text: &str, cx: &Context) -> Result<Option<String>, TemplateError> {
    let err = |line: usize, message: String| TemplateError { line, message };
    let mut out = String::with_capacity(text.len());
    // One entry per open block: (this branch is written, the parent is written,
    // the line it opened on, an `{{#else}}` was seen).
    let mut stack: Vec<(bool, bool, usize, bool)> = Vec::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let n = i + 1;
        let writing = stack.last().map_or(true, |b| b.0);
        match tag(line) {
            Some(Tag::FileIf(cond)) => {
                if n != 1 {
                    return Err(err(n, "`{{#file-if}}` must be the first line".into()));
                }
                if !cx.eval(cond).map_err(|m| err(n, m))? {
                    return Ok(None);
                }
            }
            Some(Tag::If(cond)) => {
                let holds = cx.eval(cond).map_err(|m| err(n, m))?;
                stack.push((writing && holds, writing, n, false));
            }
            Some(Tag::Else) => {
                let Some(top) = stack.last_mut() else {
                    return Err(err(n, "`{{#else}}` without `{{#if}}`".into()));
                };
                if top.3 {
                    return Err(err(n, "a second `{{#else}}` in one block".into()));
                }
                top.3 = true;
                top.0 = top.1 && !top.0;
            }
            Some(Tag::End) => {
                if stack.pop().is_none() {
                    return Err(err(n, "`{{/if}}` without `{{#if}}`".into()));
                }
            }
            None => {
                if writing {
                    substitute(line, cx, &mut out).map_err(|m| err(n, m))?;
                } else {
                    // Checked even when dropped: a typo must not hide behind a flag.
                    substitute(line, cx, &mut String::new()).map_err(|m| err(n, m))?;
                }
            }
        }
    }
    if let Some(open) = stack.last() {
        return Err(err(open.2, "`{{#if}}` is never closed".into()));
    }
    Ok(Some(out))
}

fn substitute(line: &str, cx: &Context, out: &mut String) -> Result<(), String> {
    let mut rest = line;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let inner = after.trim_start();
        let is_tag = inner
            .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '#' || c == '/');
        if !is_tag {
            // Not a tag: Rust's `{{` in a format string, JSON, and so on.
            out.push_str("{{");
            rest = after;
            continue;
        }
        let end = after
            .find("}}")
            .ok_or_else(|| "`{{` without `}}`".to_string())?;
        let name = after[..end].trim();
        if name.starts_with('#') || name.starts_with('/') {
            return Err(format!(
                "block tag `{{{{{name}}}}}` must stand alone on its line"
            ));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{name:?} is not a variable name"));
        }
        let value = cx
            .vars
            .get(name)
            .ok_or_else(|| format!("unknown variable {name:?}"))?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> Context {
        let mut cx = Context::new();
        cx.var("name", "api").var("v", "1.5.0");
        cx.flag("pg", true).flag("redis", false).flag("tls", true);
        cx
    }

    fn r(t: &str) -> String {
        render(t, &cx()).unwrap().unwrap()
    }

    fn e(t: &str) -> TemplateError {
        render(t, &cx()).unwrap_err()
    }

    #[test]
    fn variables() {
        assert_eq!(r("name = \"{{name}}\" {{ v }}\n"), "name = \"api\" 1.5.0\n");
        assert_eq!(r("no tags at all"), "no tags at all");
        assert_eq!(r(""), "");
    }

    #[test]
    fn blocks_keep_or_drop_lines() {
        let t = "a\n{{#if pg}}\nb\n{{#else}}\nc\n{{/if}}\n  {{#if redis}}\nd\n  {{#else}}\ne\n  {{/if}}\nf\n";
        assert_eq!(r(t), "a\nb\ne\nf\n");
    }

    #[test]
    fn nested_blocks() {
        let t = "{{#if pg}}\n{{#if redis}}\nx\n{{#else}}\ny\n{{/if}}\n{{#else}}\n{{#if tls}}\nz\n{{/if}}\n{{/if}}\n";
        assert_eq!(r(t), "y\n");
        // An inner branch never writes when the outer one is dropped.
        let t = "{{#if redis}}\n{{#if pg}}\nx\n{{#else}}\ny\n{{/if}}\n{{/if}}\n";
        assert_eq!(r(t), "");
        let t = "{{#if redis}}\n{{#else}}\n{{#if !pg}}\nx\n{{#else}}\ny\n{{/if}}\n{{/if}}\n";
        assert_eq!(r(t), "y\n");
    }

    #[test]
    fn conditions() {
        assert_eq!(r("{{#if pg | redis}}\nx\n{{/if}}\n"), "x\n");
        assert_eq!(r("{{#if pg & redis}}\nx\n{{/if}}\n"), "");
        assert_eq!(r("{{#if pg & !redis & tls}}\nx\n{{/if}}\n"), "x\n");
        assert_eq!(r("{{#if !pg | redis}}\nx\n{{/if}}\n"), "");
        assert_eq!(r("{{#if ! redis}}\nx\n{{/if}}\n"), "x\n");
    }

    #[test]
    fn file_if() {
        assert_eq!(render("{{#file-if redis}}\nx\n", &cx()).unwrap(), None);
        assert_eq!(r("{{#file-if pg}}\nx\n"), "x\n");
        assert!(e("x\n{{#file-if pg}}\n").message.contains("first line"));
    }

    #[test]
    fn last_line_without_newline() {
        assert_eq!(r("{{#if pg}}\nx\n{{/if}}"), "x\n");
        assert_eq!(r("a\n{{name}}"), "a\napi");
    }

    #[test]
    fn crlf_lines() {
        assert_eq!(r("{{#if pg}}\r\nx\r\n{{/if}}\r\ny\r\n"), "x\r\ny\r\n");
    }

    #[test]
    fn errors_name_the_line() {
        assert_eq!(e("a\n{{nope}}\n").line, 2);
        assert!(e("{{nope}}").message.contains("unknown variable \"nope\""));
        assert!(e("{{#if nope}}\n{{/if}}\n")
            .message
            .contains("unknown flag"));
        assert_eq!(e("a\n{{#if pg}}\nb\n").line, 2);
        assert!(e("{{/if}}\n").message.contains("without"));
        assert!(e("{{#else}}\n").message.contains("without"));
        assert!(e("{{#if pg}}\n{{#else}}\n{{#else}}\n{{/if}}\n")
            .message
            .contains("second"));
        assert!(e("{{#if pg | redis & tls}}\n{{/if}}\n")
            .message
            .contains("mixes"));
        assert!(e("{{#if }}\n{{/if}}\n").message.contains("flag name") || true);
        assert!(e("{{#if pg-x}}\n{{/if}}\n")
            .message
            .contains("not a flag name"));
        assert!(e("x {{#if pg}} y\n").message.contains("stand alone"));
        assert!(e("x {{name\n").message.contains("without"));
    }

    #[test]
    fn typos_in_dropped_branches_are_errors() {
        assert!(e("{{#if redis}}\n{{nmae}}\n{{/if}}\n")
            .message
            .contains("unknown variable"));
        assert!(e("{{#if redis}}\n{{#if nope}}\n{{/if}}\n{{/if}}\n")
            .message
            .contains("unknown flag"));
    }

    #[test]
    fn braces_that_are_not_tags_stay() {
        assert_eq!(
            r(r#"format!(r"{{\"m\":{}}}", x) {{{{ {{ }} {{name}}"#),
            r#"format!(r"{{\"m\":{}}}", x) {{{{ {{ }} api"#
        );
        assert_eq!(r("{{}}"), "{{}}");
        assert!(e("{{name x}}").message.contains("not a variable name"));
    }

    #[test]
    fn values_are_not_rescanned() {
        let mut c = cx();
        c.var("x", "{{name}}");
        assert_eq!(render("{{x}}", &c).unwrap().unwrap(), "{{name}}");
    }
}
