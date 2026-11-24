//! The command line: `giong [global flags] <command> [words] [--flag [value]]`.
//!
//! Hand-written instead of a parser crate: the command builds in a second,
//! and every mistake is answered the same way, with `GIONG-E5000` and the
//! usage of the command.

use std::collections::BTreeMap;

use crate::code::Code;
use crate::report::Failure;

/// What a flag takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `--json`: present or not.
    Switch,
    /// `--template web`: one value, the next word or after `=`.
    Value,
}

/// A flag a command accepts.
#[derive(Clone, Copy, Debug)]
pub struct Flag {
    pub name: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

/// Flags every command accepts.
pub const GLOBAL: &[Flag] = &[
    Flag {
        name: "json",
        kind: Kind::Switch,
        help: "print one JSON document instead of text (for agents and scripts)",
    },
    Flag {
        name: "help",
        kind: Kind::Switch,
        help: "print the usage of the command",
    },
];

/// A parsed command line.
#[derive(Debug, Default, Clone)]
pub struct Args {
    /// The command, such as `doctor`; empty for plain `giong`.
    pub command: String,
    /// Words after the command that are not flags.
    pub words: Vec<String>,
    switches: Vec<&'static str>,
    values: BTreeMap<&'static str, String>,
    /// Everything after a lone `--`, passed on untouched (to cargo or the service).
    pub rest: Vec<String>,
}

impl Args {
    /// Whether a switch was given.
    pub fn has(&self, name: &str) -> bool {
        self.switches.contains(&name)
    }

    /// The value of a flag, if it was given.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The value of a flag parsed as a number, if it was given.
    pub fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, Failure> {
        match self.value(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| {
                Failure::new(
                    Code::USAGE,
                    format!("--{name} {v:?} is not a number"),
                    format!("give --{name} a whole number"),
                )
            }),
        }
    }

    /// `--json`.
    pub fn json(&self) -> bool {
        self.has("json")
    }
}

/// Finds the command: the first word that is not a flag.
pub fn command_of(argv: &[String]) -> Option<&str> {
    argv.iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .find(|a| !a.starts_with('-') || *a == "-")
}

/// Parses `argv` (without the program name) for a command that accepts
/// `flags` (besides the global ones) and at most `max_words` words.
pub fn parse(argv: &[String], flags: &[Flag], max_words: usize) -> Result<Args, Failure> {
    let mut args = Args::default();
    let mut iter = argv.iter();
    let mut command_seen = false;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            args.rest = iter.cloned().collect();
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let flag = GLOBAL
                .iter()
                .chain(flags)
                .find(|f| f.name == name)
                .ok_or_else(|| unknown_flag(name, flags))?;
            match flag.kind {
                Kind::Switch => {
                    if inline.is_some() {
                        return Err(Failure::new(
                            Code::USAGE,
                            format!("--{name} takes no value"),
                            format!("write --{name} on its own"),
                        ));
                    }
                    if !args.switches.contains(&flag.name) {
                        args.switches.push(flag.name);
                    }
                }
                Kind::Value => {
                    let value = match inline {
                        Some(v) => v,
                        None => match iter.next() {
                            Some(v) if !v.starts_with("--") => v.clone(),
                            _ => {
                                return Err(Failure::new(
                                    Code::USAGE,
                                    format!("--{name} needs a value"),
                                    format!("write --{name} <value> or --{name}=<value>"),
                                ))
                            }
                        },
                    };
                    if args.values.insert(flag.name, value).is_some() {
                        return Err(Failure::new(
                            Code::USAGE,
                            format!("--{name} was given twice"),
                            format!("give --{name} once"),
                        ));
                    }
                }
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            let fix = match arg.as_str() {
                "-h" => "write --help".to_string(),
                "-V" | "-v" => "write --version, or giong version".to_string(),
                _ => "flags are written in full with two dashes, such as --json".to_string(),
            };
            return Err(Failure::new(
                Code::USAGE,
                format!("unknown flag {arg}"),
                fix,
            ));
        } else if !command_seen {
            args.command = arg.clone();
            command_seen = true;
        } else {
            args.words.push(arg.clone());
        }
    }
    if args.words.len() > max_words && !args.has("help") {
        let extra = &args.words[max_words];
        return Err(Failure::new(
            Code::USAGE,
            format!("giong {} does not take {extra:?}", args.command),
            format!(
                "see giong {} --help; to pass words on to another program, put them after --",
                args.command
            ),
        ));
    }
    Ok(args)
}

fn unknown_flag(name: &str, flags: &[Flag]) -> Failure {
    let known: Vec<&str> = GLOBAL.iter().chain(flags).map(|f| f.name).collect();
    let close = known
        .iter()
        .copied()
        .filter(|k| distance(k, name) <= 2)
        .min_by_key(|k| distance(k, name));
    let fix = match close {
        Some(k) => format!("did you mean --{k}?"),
        None => format!(
            "the flags are: {}",
            known
                .iter()
                .map(|k| format!("--{k}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Failure::new(Code::USAGE, format!("unknown flag --{name}"), fix)
}

/// Edit distance, for "did you mean".
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb {
                prev
            } else {
                1 + prev.min(row[j]).min(row[j + 1])
            };
            prev = cur;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAGS: &[Flag] = &[
        Flag {
            name: "template",
            kind: Kind::Value,
            help: "",
        },
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "",
        },
    ];

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn words_flags_and_rest() {
        let a = parse(&argv("new api --template web --json -- --x y"), FLAGS, 1).unwrap();
        assert_eq!(a.command, "new");
        assert_eq!(a.words, ["api"]);
        assert_eq!(a.value("template"), Some("web"));
        assert!(a.json());
        assert_eq!(a.rest, ["--x", "y"]);
        let a = parse(&argv("new --template=grpc"), FLAGS, 1).unwrap();
        assert_eq!(a.value("template"), Some("grpc"));
    }

    #[test]
    fn mistakes_are_coded_and_say_what_to_do() {
        let e = parse(&argv("new --templat web"), FLAGS, 1).unwrap_err();
        assert_eq!(e.code, Code::USAGE);
        assert!(e.fix.contains("--template"), "{}", e.fix);
        let e = parse(&argv("new --template"), FLAGS, 1).unwrap_err();
        assert!(e.what.contains("needs a value"));
        let e = parse(&argv("new --template a --template b"), FLAGS, 1).unwrap_err();
        assert!(e.what.contains("twice"));
        let e = parse(&argv("new a b"), FLAGS, 1).unwrap_err();
        assert!(e.what.contains("\"b\""));
        let e = parse(&argv("new --dry-run=yes"), FLAGS, 1).unwrap_err();
        assert!(e.what.contains("takes no value"));
        let e = parse(&argv("new -h"), FLAGS, 1).unwrap_err();
        assert!(e.fix.contains("--help"));
    }

    #[test]
    fn command_is_the_first_word() {
        assert_eq!(command_of(&argv("--json doctor")), Some("doctor"));
        assert_eq!(command_of(&argv("--json")), None);
        assert_eq!(command_of(&argv("-- doctor")), None);
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("json", "jsno"), 2);
        assert_eq!(distance("", "ab"), 2);
        assert_eq!(distance("dev", "dev"), 0);
    }
}
