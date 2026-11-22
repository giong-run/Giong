//! The commands. Each module exports a `COMMAND`; add it to [`COMMANDS`].
//!
//! A command takes the parsed [`Args`] and returns a [`Report`] (findings and
//! data, printed by `main` as text or JSON) or a [`Failure`] (it could not do
//! its job: exit status 2). A long-running command (`dev`) prints as it goes
//! and returns its report when it stops.

use crate::args::{distance, Args, Flag};
use crate::code::Code;
use crate::report::{Failure, Report};

pub mod ai;
pub mod explain;
pub mod fix;
pub mod help;
pub mod new;
pub mod version;

/// A command of `giong`.
pub struct Command {
    /// What is typed: `giong <name>`.
    pub name: &'static str,
    /// One line for `giong --help`.
    pub summary: &'static str,
    /// The usage line(s) and a paragraph, for `giong <name> --help`.
    pub usage: &'static str,
    /// Flags besides `--json` and `--help`.
    pub flags: &'static [Flag],
    /// How many words may follow the command.
    pub max_words: usize,
    pub run: fn(&Args) -> Result<Report, Failure>,
}

/// Every command, in the order `giong --help` lists them.
pub const COMMANDS: &[&Command] = &[
    // Thread B adds `new`, `ai`, `fix` here; thread C `db`, `top`, `bench`,
    // `image`, `openapi`, `record`.
    &new::COMMAND,
    &ai::COMMAND,
    &fix::COMMAND,
    &explain::COMMAND,
    &version::COMMAND,
    &help::COMMAND,
];

pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().copied().find(|c| c.name == name)
}

pub fn unknown(name: &str) -> Failure {
    let close = COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|c| distance(c, name) <= 2)
        .min_by_key(|c| distance(c, name));
    let fix = match close {
        Some(c) => format!("did you mean giong {c}?"),
        None => "giong --help lists the commands".to_string(),
    };
    Failure::new(Code::USAGE, format!("giong has no command {name:?}"), fix)
}
