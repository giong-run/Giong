//! `.env` files: `KEY=value` lines, for `giong dev`, `giong test` and
//! `giong doctor`. Never read in production: a service reads only its
//! environment.
//!
//! The format is the common subset every tool agrees on: `KEY=value`, an
//! optional `export ` in front, `#` comments on their own line, values in
//! single quotes (taken as they are) or double quotes (with `\n`, `\"`, `\\`,
//! and over several lines), and unquoted values cut at ` #`. A leading byte
//! order mark is skipped. No variable expansion: a value is what is
//! written.

use std::path::Path;

/// One `KEY=value` with the line it came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Var {
    pub key: String,
    pub value: String,
    pub line: u32,
}

/// A line that is not `KEY=value`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BadLine {
    pub line: u32,
    pub why: &'static str,
}

pub fn parse(text: &str) -> (Vec<Var>, Vec<BadLine>) {
    let mut vars = Vec::new();
    let mut bad = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = i as u32 + 1;
        let l = lines[i].trim();
        i += 1;
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = match l.strip_prefix("export") {
            Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
            _ => l,
        };
        let Some((key, value)) = l.split_once('=') else {
            bad.push(BadLine {
                line,
                why: "no = in the line",
            });
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || key.starts_with(|c: char| c.is_ascii_digit())
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            bad.push(BadLine {
                line,
                why: "a name is letters, digits and _, not starting with a digit",
            });
            continue;
        }
        let value = value.trim();
        let value = if let Some(rest) = value.strip_prefix('\'') {
            match rest.find('\'') {
                Some(end) => rest[..end].to_string(),
                None => {
                    bad.push(BadLine {
                        line,
                        why: "a ' quote is not closed",
                    });
                    continue;
                }
            }
        } else if let Some(rest) = value.strip_prefix('"') {
            // A value may go on over the next lines until its quote closes.
            let mut joined = rest.to_string();
            let mut v = unquote_double(&joined);
            let mut j = i;
            while v.is_none() && j < lines.len() {
                joined.push('\n');
                joined.push_str(lines[j]);
                j += 1;
                v = unquote_double(&joined);
            }
            if v.is_some() {
                i = j;
            }
            match v {
                Some(v) => v,
                None => {
                    bad.push(BadLine {
                        line,
                        why: "a \" quote is not closed",
                    });
                    continue;
                }
            }
        } else {
            match value.find(" #") {
                Some(at) => value[..at].trim_end().to_string(),
                None => value.to_string(),
            }
        };
        vars.push(Var {
            key: key.to_string(),
            value,
            line,
        });
    }
    (vars, bad)
}

fn unquote_double(rest: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Reads and parses a file; `None` when it does not exist.
pub fn read(path: &Path) -> Option<(Vec<Var>, Vec<BadLine>)> {
    std::fs::read_to_string(path).ok().map(|t| parse(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_tabs_and_values_over_several_lines() {
        let (vars, bad) =
            parse("\u{feff}A=1\nexport\tB=2\nKEY=\"-----BEGIN\nabc\n-----END\"\nC=3\n");
        assert!(bad.is_empty(), "{bad:?}");
        let got: Vec<(&str, &str, u32)> = vars
            .iter()
            .map(|v| (v.key.as_str(), v.value.as_str(), v.line))
            .collect();
        assert_eq!(
            got,
            [
                ("A", "1", 1),
                ("B", "2", 2),
                ("KEY", "-----BEGIN\nabc\n-----END", 3),
                ("C", "3", 6)
            ]
        );
        // A quote never closed is one bad line, and the lines after it count.
        let (vars, bad) = parse("A=\"open\nB=2\n");
        assert_eq!(bad.len(), 1);
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn the_common_subset() {
        let text = "\
# comment
DATABASE_URL=postgres://u:p@localhost/db
export WORKERS = 2
EMPTY=
QUOTED='a # b $HOME'
DOUBLE=\"line\\nnext \\\"q\\\"\"
TRAILING=value # comment
HASH=a#b
";
        let (vars, bad) = parse(text);
        assert!(bad.is_empty(), "{bad:?}");
        let get = |k: &str| vars.iter().find(|v| v.key == k).unwrap().value.clone();
        assert_eq!(get("DATABASE_URL"), "postgres://u:p@localhost/db");
        assert_eq!(get("WORKERS"), "2");
        assert_eq!(get("EMPTY"), "");
        assert_eq!(get("QUOTED"), "a # b $HOME");
        assert_eq!(get("DOUBLE"), "line\nnext \"q\"");
        assert_eq!(get("TRAILING"), "value");
        assert_eq!(get("HASH"), "a#b");
        assert_eq!(vars[0].line, 2);
    }

    #[test]
    fn bad_lines_are_reported_with_their_number() {
        let (vars, bad) = parse("OK=1\nnot a var\n1X=2\nA-B=3\nQ='open\nD=\"open\n");
        assert_eq!(vars.len(), 1);
        let lines: Vec<u32> = bad.iter().map(|b| b.line).collect();
        assert_eq!(lines, [2, 3, 4, 5, 6]);
    }
}
