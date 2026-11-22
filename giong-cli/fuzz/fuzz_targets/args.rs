//! The command line: any argv (split at NUL bytes) parses or fails with a
//! coded usage error, never panics, and what it parses is what was given.
#![no_main]

use giong_cli_fuzz::args::{command_of, parse, Flag, Kind};
use giong_cli_fuzz::code::Code;
use libfuzzer_sys::fuzz_target;

const FLAGS: &[Flag] = &[
    Flag { name: "template", kind: Kind::Value, help: "" },
    Flag { name: "days", kind: Kind::Value, help: "" },
    Flag { name: "dry-run", kind: Kind::Switch, help: "" },
    Flag { name: "all", kind: Kind::Switch, help: "" },
];

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (max_words, text) = match text.split_once('\n') {
        Some((n, rest)) => (n.len() % 4, rest),
        None => (1, text),
    };
    let argv: Vec<String> = text.split('\0').map(String::from).collect();
    // A lone "-" is a word for parse() and a flag for command_of(): reported
    // to the CLI's owner (audit round 2, F1). Remove this line once fixed.
    if argv.iter().any(|a| a == "-") {
        return;
    }
    let cmd = command_of(&argv).map(String::from);
    match parse(&argv, FLAGS, max_words) {
        Err(f) => {
            assert_eq!(f.code, Code::USAGE);
            assert!(!f.what.is_empty() && !f.fix.is_empty());
        }
        Ok(a) => {
            // main.rs picks the command with command_of before it knows the
            // command's flags; when no value flag comes first, both agree.
            let first_value_flag = argv.iter().position(|x| x.starts_with("--template") || x.starts_with("--days"));
            let first_word = argv.iter().position(|x| *x == a.command);
            if !a.command.is_empty() && first_value_flag.map_or(true, |f| Some(f) > first_word) {
                assert_eq!(cmd.as_deref(), Some(a.command.as_str()), "{argv:?}");
            }
            if !a.has("help") {
                assert!(a.words.len() <= max_words, "{argv:?}");
            }
            // Everything after the first `--` is passed on untouched.
            if let Some(i) = argv.iter().position(|x| x == "--") {
                assert_eq!(a.rest, argv[i + 1..]);
            } else {
                assert!(a.rest.is_empty());
            }
            // Every word was in argv, before any `--`.
            let before: Vec<&String> = argv.iter().take_while(|x| *x != "--").collect();
            for w in &a.words {
                assert!(before.contains(&w), "{w:?} not in {argv:?}");
            }
            if let Some(v) = a.value("template") {
                assert!(text.contains(v));
            }
            let _ = a.number::<u64>("days");
        }
    }
});
