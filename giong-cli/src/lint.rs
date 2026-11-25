//! giong's own checks of a project's source, the ones the compiler cannot
//! make because they are about running on a giong worker: a blocking call in
//! an `async` function (`GIONG-E5006`), a listener bound to a host name
//! (`GIONG-E5007`), a `RefCell` borrow kept across an `.await`
//! (`GIONG-E5008`).
//!
//! They read the source, not the types, so they are heuristics and say so in
//! their fix. Comments and the insides of strings are masked first, so a
//! commented-out call or a word in a message is never reported.

use std::fs;
use std::path::{Path, PathBuf};

use crate::code::Code;
use crate::report::{Edit, Finding, Severity};
use crate::watch::SKIP;

/// The source files of the packages under `dirs` (each package's directory),
/// not descending into build output or into another package.
pub fn source_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            // Tests and benchmarks block on purpose (a std client against
            // the server under test): not reported.
            if SKIP.contains(&name.as_ref())
                || name.starts_with('.')
                || name == "tests"
                || name == "benches"
                || name == "fuzz"
                || path.join("CACHEDIR.TAG").exists()
                || (path != root && path.join("Cargo.toml").is_file())
            {
                continue;
            }
            walk(root, &path, out);
        } else if kind.is_file()
            && name.ends_with(".rs")
            && !name.ends_with("_tests.rs")
            && name != "tests.rs"
        {
            out.push(path);
        }
    }
}

/// Every finding in one file. `shown` is the path to report (relative to the
/// project).
pub fn check_file(text: &str, shown: &str) -> Vec<Finding> {
    let masked = mask(text);
    let m = masked.as_bytes();
    let regions = async_regions(m);
    let mut excluded = blocking_ok_regions(m);
    excluded.extend(test_regions(m));
    let in_async = |at: usize| {
        regions.iter().any(|r| r.0 <= at && at < r.1)
            && !excluded.iter().any(|r| r.0 <= at && at < r.1)
    };
    let mut out = Vec::new();
    let uses_std_fs = masked.contains("use std::fs;") || masked.contains("use std::fs::{");

    // GIONG-E5006: blocking calls.
    let mut blocking: Vec<(&str, &str)> = vec![
        ("std::thread::sleep(", "std::thread::sleep"),
        ("thread::sleep(", "std::thread::sleep"),
        ("std::fs::", "std::fs"),
        (
            "std::net::TcpStream::connect(",
            "std::net::TcpStream::connect",
        ),
        // (Binding an IP address does not block: std's bind is not listed.)
        ("std::process::Command::new(", "std::process::Command"),
        ("reqwest::blocking::", "reqwest::blocking"),
        (".read_line(", "reading a line from standard input"),
    ];
    if uses_std_fs {
        blocking.push(("fs::", "std::fs"));
    }
    let mut seen_at = Vec::new();
    for (pattern, what) in &blocking {
        for at in find_all(&masked, pattern) {
            // `thread::sleep(` also matches inside `std::thread::sleep(`;
            // `fs::` inside `std::fs::` or `giong::fs::`.
            if at > 0 && is_path_char(m[at - 1]) {
                continue;
            }
            if *pattern == "fs::" && preceded_by(m, at, "::") {
                continue;
            }
            if *pattern == ".read_line(" && !masked[line_start(m, at)..at].contains("stdin") {
                continue;
            }
            if !in_async(at) || seen_at.contains(&at) {
                continue;
            }
            seen_at.push(at);
            let (line, col) = line_col(text, at);
            out.push(
                Finding::new(
                    Code::BLOCKING_CALL,
                    Severity::Warning,
                    format!("{what} blocks the worker inside an async function"),
                )
                .detail("every other task on this worker waits until it returns")
                .fix(
                    "use giong's version (giong::fs, giong::net, giong::time::sleep), or run it \
                     on a thread of its own; this check reads the source, so ignore it if this \
                     code never runs on a worker",
                )
                .at(shown, line, col),
            );
        }
    }

    // GIONG-E5007: binding a host name.
    for at in find_all(&masked, "bind(") {
        if excluded.iter().any(|r| r.0 <= at && at < r.1) {
            continue;
        }
        if at > 0 && is_path_char(m[at - 1]) && m[at - 1] != b':' && m[at - 1] != b'.' {
            continue;
        }
        let open = at + "bind(".len();
        let rest = &text[open..];
        let lead = rest.len() - rest.trim_start().len();
        let start = open + lead + 1; // after the quote
        if !rest.trim_start().starts_with("\"localhost:") {
            continue;
        }
        let (line, col) = line_col(text, at);
        out.push(
            Finding::new(
                Code::HOST_NAME_BIND,
                Severity::Error,
                "a listener binds the host name localhost",
            )
            .detail("giong binds IP addresses only: localhost can mean 127.0.0.1 or ::1")
            .fix("bind \"127.0.0.1:<port>\" (giong fix makes the change)")
            .at(shown, line, col)
            .edits(vec![Edit {
                file: shown.to_string(),
                start,
                end: start + "localhost".len(),
                replacement: "127.0.0.1".into(),
            }]),
        );
    }

    // GIONG-E5008: a RefCell borrow kept across an await.
    for (start, end) in &regions {
        for at in find_all(&masked[*start..*end], "let ") {
            let at = start + at;
            if at > 0 && is_path_char(m[at - 1]) {
                continue;
            }
            let Some(semi) = statement_end(m, at, *end) else {
                continue;
            };
            let stmt = masked[at..semi].trim_end();
            if !(stmt.ends_with(".borrow_mut()") || stmt.ends_with(".borrow()")) {
                continue;
            }
            let Some(name) = binding_name(&stmt[4..]) else {
                continue;
            };
            // From the `;`: in half-typed code a `}` inside unclosed
            // parentheses can close the block between `at` and the `;`.
            let block_end = enclosing_block_end(m, semi, *end);
            let after = &masked[semi..block_end];
            let await_at = after.find(".await");
            let drop_at = after.find(&format!("drop({name})"));
            if let Some(w) = await_at {
                if drop_at.map_or(true, |d| d > w) {
                    let (line, col) = line_col(text, at);
                    out.push(
                        Finding::new(
                            Code::BORROW_ACROSS_AWAIT,
                            Severity::Warning,
                            format!("the RefCell borrow `{name}` is held across an .await"),
                        )
                        .detail(
                            "another task on this worker that borrows the same cell panics with \
                             \"already borrowed\"",
                        )
                        .fix(format!(
                            "take what you need, drop({name}) (or end its block), then await; \
                             this check reads the source and may be wrong if it is dropped otherwise"
                        ))
                        .at(shown, line, col),
                    );
                }
            }
        }
    }
    out.sort_by_key(|f| f.place.as_ref().map(|p| (p.line, p.column)));
    out
}

fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':'
}

fn preceded_by(m: &[u8], at: usize, s: &str) -> bool {
    at >= s.len() && &m[at - s.len()..at] == s.as_bytes()
}

fn find_all(hay: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        out.push(from + i);
        from += i + needle.len().max(1);
    }
    out
}

fn line_start(m: &[u8], at: usize) -> usize {
    m[..at]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1)
}

/// 1-based line and column (in characters) of byte `at`.
fn line_col(text: &str, at: usize) -> (u32, u32) {
    let before = &text[..at];
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() as u32 + 1;
    (line, col)
}

/// The `;` that ends the statement starting at `at`, at the same nesting.
fn statement_end(m: &[u8], at: usize, limit: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, b) in m.iter().enumerate().take(limit).skip(at) {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            b';' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// The `}` that closes the block the byte `at` is in.
fn enclosing_block_end(m: &[u8], at: usize, limit: usize) -> usize {
    let mut depth = 0i32;
    for (i, b) in m.iter().enumerate().take(limit).skip(at) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    limit
}

/// `mut guard = x.borrow_mut()` -> `guard` (a plain name only).
fn binding_name(stmt: &str) -> Option<&str> {
    let (pat, _) = stmt.split_once('=')?;
    let pat = pat.trim();
    let pat = pat.strip_prefix("mut ").unwrap_or(pat).trim();
    let pat = pat.split(':').next().unwrap_or(pat).trim();
    (!pat.is_empty() && pat.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && pat != "_")
        .then_some(pat)
}

/// The byte ranges of `async fn` bodies and `async` blocks and closures.
fn async_regions(m: &[u8]) -> Vec<(usize, usize)> {
    let text = std::str::from_utf8(m).unwrap_or("");
    let mut out = Vec::new();
    for at in find_all(text, "async") {
        if at > 0 && (m[at - 1].is_ascii_alphanumeric() || m[at - 1] == b'_') {
            continue;
        }
        let after = at + 5;
        if m.get(after)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            continue;
        }
        // The body: the first `{` after the keyword, outside parentheses
        // (the signature's arguments) and angle-free enough for real code.
        let mut depth = 0i32;
        let mut i = after;
        let mut open = None;
        while i < m.len() {
            match m[i] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b'{' if depth == 0 => {
                    open = Some(i);
                    break;
                }
                // A declaration without a body (`async fn f();` in a trait).
                b';' if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        if let Some(open) = open {
            let close = matching_brace(m, open);
            out.push((open, close));
        }
    }
    out
}

/// Arguments of `thread::spawn(` and `spawn_blocking(`: code there runs on a
/// thread of its own, where blocking is fine.
fn blocking_ok_regions(m: &[u8]) -> Vec<(usize, usize)> {
    let text = std::str::from_utf8(m).unwrap_or("");
    let mut out = Vec::new();
    for pattern in [
        "thread::spawn(",
        "spawn_blocking(",
        ".spawn(move ||",
        "scope(|",
    ] {
        for at in find_all(text, pattern) {
            let open = at + text[at..].find('(').unwrap_or(0);
            let mut depth = 0i32;
            let mut close = m.len();
            for (i, b) in m.iter().enumerate().skip(open) {
                match b {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push((open, close));
        }
    }
    out
}

/// `#[cfg(test)]` items (the test module) and `#[test]` functions.
fn test_regions(m: &[u8]) -> Vec<(usize, usize)> {
    let text = std::str::from_utf8(m).unwrap_or("");
    let mut out = Vec::new();
    for pattern in ["#[cfg(test)]", "#[test]", "#[giong::test]"] {
        for at in find_all(text, pattern) {
            if let Some(open) = text[at..].find('{').map(|o| at + o) {
                out.push((at, matching_brace(m, open)));
            }
        }
    }
    out
}

fn matching_brace(m: &[u8], open: usize) -> usize {
    let mut depth = 0i32;
    for (i, b) in m.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    m.len()
}

/// The source with comments and the insides of string and character
/// literals replaced by spaces (same length, line breaks kept).
pub fn mask(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to.min(b.len())] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..]
                    .iter()
                    .position(|c| *c == b'\n')
                    .map_or(b.len(), |p| i + p);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                let mut j = i;
                while j < b.len() {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'r' if (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#'))
                && (i == 0
                    || !is_ident(b[i - 1])
                    || (b[i - 1] == b'b' && (i < 2 || !is_ident(b[i - 2])))) =>
            {
                // r"..." or r#"..."#
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let body = j + 1;
                let closing: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat(b'#').take(hashes))
                    .collect();
                let end = b[body..]
                    .windows(closing.len())
                    .position(|w| w == closing.as_slice())
                    .map_or(b.len(), |p| body + p);
                blank(&mut out, body, end);
                i = end + closing.len();
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                blank(&mut out, i + 1, j);
                i = j + 1;
            }
            b'\'' => {
                // A character literal ('a', '\n', '\u{..}'), not a lifetime.
                if b.get(i + 1) == Some(&b'\\') {
                    // The escaped character is never the end, even a quote.
                    let from = (i + 3).min(b.len());
                    let end = b[from..]
                        .iter()
                        .position(|c| *c == b'\'')
                        .map_or(b.len(), |p| from + p);
                    blank(&mut out, i + 1, end);
                    i = end + 1;
                } else {
                    let ch_len = text[i + 1..].chars().next().map_or(1, char::len_utf8);
                    if b.get(i + 1 + ch_len) == Some(&b'\'') {
                        blank(&mut out, i + 1, i + 1 + ch_len);
                        i += 2 + ch_len;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    // Blanking only touches ASCII bytes with spaces inside literals and
    // comments, but a multi-byte character there becomes several spaces:
    // still valid UTF-8, same length.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(src: &str) -> Vec<(u16, u32)> {
        check_file(src, "src/main.rs")
            .iter()
            .map(|f| (f.code.number(), f.place.as_ref().unwrap().line))
            .collect()
    }

    #[test]
    fn half_typed_code_does_not_panic() {
        // Found by the lint fuzz target: a `}` inside unclosed parentheses.
        let src =
            "u:D;\n\nasync frig {\n g{\n let _n = vln((Durat8`)dy\n}\n\nac.borrow_mut();OG\n/";
        check_file(src, "src/main.rs");
        // Every prefix and every one-byte deletion of a file with all three
        // checks in it, as an editor shows them while someone types.
        let full = "use std::cell::RefCell;\nasync fn f(c: &RefCell<u8>) {\n    let g = c.borrow_mut();\n    \
                    std::thread::sleep(d);\n    giong::net::TcpListener::bind(\"localhost:80\");\n    \
                    x.await;\n    drop(g);\n    let s = \"é\"; /* ü */ let h = async { y.await };\n}\n";
        let mut cut = full.char_indices().map(|(i, _)| i).collect::<Vec<_>>();
        cut.push(full.len());
        for &end in &cut {
            check_file(&full[..end], "src/main.rs");
        }
        for w in cut.windows(2) {
            let mut s = full[..w[0]].to_string();
            s.push_str(&full[w[1]..]);
            for f in check_file(&s, "src/main.rs") {
                for e in &f.edits {
                    assert!(
                        s.is_char_boundary(e.start)
                            && s.is_char_boundary(e.end)
                            && e.start <= e.end
                    );
                }
            }
        }
    }

    #[test]
    fn masking_keeps_offsets() {
        let src = "let a = \"x // y\"; // c\nlet b = 'q'; /* a /* b */ c */ r#\"s\"#; 'a: loop {}";
        let m = mask(src);
        assert_eq!(m.len(), src.len());
        assert!(!m.contains("// y") && !m.contains("/* a"));
        assert!(m.contains("'a: loop"));
        assert!(m.contains("let b = ' ';"));
    }

    /// `'\''` holds a quote: the literal ends at the quote after it, so the
    /// next quote does not open a character or a string that hides code.
    #[test]
    fn an_escaped_quote_character_ends_where_it_ends() {
        let src = "async fn h() {\n    let q = ['\\'','\"'];\n    std::thread::sleep(d);\n}\n";
        assert_eq!(codes(src), [(5006, 3)]);
        let m = mask("let q = b'\\''; let s = \"x\";");
        assert_eq!(m, "let q = b'  '; let s = \" \";");
    }

    #[test]
    fn blocking_calls_in_async_code_only() {
        let src = r#"
fn setup() { std::thread::sleep(d); }
async fn handler() {
    std::thread::sleep(d);
    let s = std::fs::read_to_string("x");
    // std::thread::sleep(d);
    let msg = "std::fs::write is fine in a string";
    std::thread::spawn(move || { std::thread::sleep(d) });
    giong::fs::read("x").await;
}
"#;
        assert_eq!(codes(src), [(5006, 4), (5006, 5)]);
    }

    #[test]
    fn blocks_and_closures_count() {
        let src = "use std::fs;\nfn main() {\n    rt.run(|_| async move {\n        fs::read(\"a\");\n    });\n}\n";
        assert_eq!(codes(src), [(5006, 4)]);
    }

    #[test]
    fn tests_are_left_alone() {
        let src = "#[cfg(test)]\nmod tests {\n    async fn t() { std::thread::sleep(d); }\n}\nasync fn f() { std::thread::sleep(d); }\n";
        assert_eq!(codes(src), [(5006, 5)]);
    }

    #[test]
    fn localhost_binds_get_an_edit() {
        let src =
            "async fn f() {\n    let l = TcpListener::bind( \"localhost:8080\").unwrap();\n}\n";
        let f = &check_file(src, "src/main.rs")[0];
        assert_eq!(f.code, Code::HOST_NAME_BIND);
        let e = &f.edits[0];
        let mut fixed = src.to_string();
        fixed.replace_range(e.start..e.end, &e.replacement);
        assert!(fixed.contains("bind( \"127.0.0.1:8080\")"), "{fixed}");
        assert!(codes("fn f() { x.bind(\"127.0.0.1:1\"); }").is_empty());
    }

    #[test]
    fn borrows_across_await() {
        let src = r#"
async fn bad(c: &RefCell<u32>) {
    let mut g = c.borrow_mut();
    *g += 1;
    sleep(d).await;
}
async fn dropped(c: &RefCell<u32>) {
    let g = c.borrow();
    let v = *g;
    drop(g);
    sleep(d).await;
}
async fn scoped(c: &RefCell<u32>) {
    {
        let g = c.borrow();
    }
    sleep(d).await;
}
"#;
        assert_eq!(codes(src), [(5008, 3)]);
    }
}
