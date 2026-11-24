//! `giong explain <code>`: the page of an error code, offline.

use serde_json::json;

use super::Command;
use crate::args::Args;
use crate::code::Code;
use crate::pages::PAGES;
use crate::report::{Failure, Report};

pub const COMMAND: Command = Command {
    name: "explain",
    summary: "explain an error code: what happened and how to fix it",
    usage: "giong explain <code>\n\n\
            Prints the page of a GIONG-E code, from any giong crate or this command, without \
            the network: what happened and how to fix it. The code can be written GIONG-E0102, \
            E0102 or 0102.",
    flags: &[],
    max_words: 1,
    run,
};

fn run(args: &Args) -> Result<Report, Failure> {
    let Some(word) = args.words.first() else {
        return Err(Failure::new(
            Code::USAGE,
            "giong explain needs a code",
            "write giong explain GIONG-E0102 (the code at the start of the error message)",
        ));
    };
    let number = parse_code(word).ok_or_else(|| {
        Failure::new(
            Code::UNKNOWN_CODE,
            format!("{word:?} is not an error code"),
            "a code looks like GIONG-E0102: copy it from the start of the error message",
        )
    })?;
    let Some(page) = page(number) else {
        return Err(Failure::new(
            Code::UNKNOWN_CODE,
            format!("GIONG-E{number:04} has no page in giong {}", env!("CARGO_PKG_VERSION")),
            match nearest(number) {
                Some(n) => format!(
                    "check the number (GIONG-E{n:04} exists); a code from a newer giong needs a newer \
                     giong-cli: cargo install giong-cli"
                ),
                None => "check the number".to_string(),
            },
        ));
    };
    let title = page
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .trim();
    let url = format!(
        "https://github.com/giong-run/Giong/blob/v{}/docs/errors/E{number:04}.md",
        env!("CARGO_PKG_VERSION")
    );
    let mut r = Report::new("explain");
    r.line(page.trim_end());
    r.line(format!("\n{url}"));
    r.data("code", json!(format!("GIONG-E{number:04}")));
    r.data("title", json!(title));
    r.data("page", json!(page));
    r.data("url", json!(url));
    Ok(r)
}

/// `GIONG-E0102`, `giong-e0102`, `E0102`, `e102`, `0102`, `102`.
pub fn parse_code(word: &str) -> Option<u16> {
    let w = word.trim().trim_end_matches(':');
    let w = w
        .strip_prefix("GIONG-")
        .or_else(|| w.strip_prefix("giong-"))
        .unwrap_or(w);
    let w = w.strip_prefix(['E', 'e']).unwrap_or(w);
    if w.is_empty() || w.len() > 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse().ok()
}

pub fn page(number: u16) -> Option<&'static str> {
    PAGES
        .binary_search_by_key(&number, |(n, _)| *n)
        .ok()
        .map(|i| PAGES[i].1)
}

fn nearest(number: u16) -> Option<u16> {
    PAGES
        .iter()
        .map(|(n, _)| *n)
        .min_by_key(|n| n.abs_diff(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_in_every_spelling() {
        for w in [
            "GIONG-E0102",
            "giong-e0102",
            "E0102",
            "e0102",
            "0102",
            "102",
            "GIONG-E0102:",
        ] {
            assert_eq!(parse_code(w), Some(102), "{w}");
        }
        for w in ["", "E", "E01020", "x102", "GIONG-", "E-1"] {
            assert_eq!(parse_code(w), None, "{w}");
        }
    }

    #[test]
    fn every_command_code_and_known_runtime_codes_have_pages() {
        for c in crate::code::ALL {
            let p = page(c.number()).unwrap_or_else(|| panic!("{c} has no embedded page"));
            assert!(p.starts_with(&format!("# {c}: {}", c.summary())), "{c}");
        }
        assert!(page(1).unwrap().starts_with("# GIONG-E0001"));
        assert!(page(9999).is_none());
    }

    /// src/pages.rs is docs/errors/*.md, embedded. `GIONG_CLI_REGENERATE=1`
    /// rewrites it.
    #[test]
    fn pages_are_current() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = root.join("../docs/errors");
        if !dir.exists() {
            return; // the published crate has no docs/
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.len() == 8 && n.starts_with('E') && n.ends_with(".md"))
            .collect();
        names.sort();
        let mut out = String::from(
            "//! The error pages of docs/errors/, embedded so `giong explain` works offline.\n\
             //! Generated: `GIONG_CLI_REGENERATE=1 cargo test -p giong-cli pages` rewrites it,\n\
             //! and that test fails when it is out of date.\n\n\
             /// (number, page) for every page, in order.\n\
             pub const PAGES: &[(u16, &str)] = &[\n",
        );
        for name in names {
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            let mut hashes = String::from("#");
            while text.contains(&format!("\"{hashes}")) {
                hashes.push('#');
            }
            let number: u16 = name[1..5].parse().unwrap();
            out.push_str(&format!("    ({number}, r{hashes}\"{text}\"{hashes}),\n"));
        }
        out.push_str("];\n");
        let file = root.join("src/pages.rs");
        if std::env::var_os("GIONG_CLI_REGENERATE").is_some() {
            std::fs::write(&file, &out).unwrap();
        }
        let current = std::fs::read_to_string(&file).unwrap();
        assert!(
            current == out,
            "giong-cli/src/pages.rs is out of date with docs/errors: run \
             GIONG_CLI_REGENERATE=1 cargo test -p giong-cli pages"
        );
    }
}
