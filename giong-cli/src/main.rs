//! `giong`: start, run, check and ship services on the Gióng async runtime.
//!
//! One binary, one command per job. Every command prints text for people or,
//! with `--json`, one JSON document for agents (see [`report`]). Commands are
//! registered in [`cmd::COMMANDS`].

// While the commands arrive (three branches), some shared helpers have no
// caller yet. Removed before the pull request leaves draft.
#![allow(dead_code)]

mod args;
mod cmd;
mod code;
mod dotenv;
mod host;
#[rustfmt::skip] // generated (see src/pages.rs)
mod pages;
mod project;
mod report;
mod service;
mod sha256;

use std::process::ExitCode;

use crate::report::fail;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    run(&argv)
}

fn run(argv: &[String]) -> ExitCode {
    let json = argv
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let name = match args::command_of(argv) {
        Some(name) => name.to_string(),
        None => {
            // `giong`, `giong --help`, `giong --version`.
            if argv.iter().any(|a| a == "--version") {
                return cmd::version::print(json);
            }
            return cmd::help::print_overview(json);
        }
    };
    let Some(command) = cmd::find(&name) else {
        return fail(&name, &cmd::unknown(&name), json);
    };
    let parsed = match args::parse(argv, command.flags, command.max_words) {
        Ok(a) => a,
        Err(f) => return fail(command.name, &f, json),
    };
    if parsed.has("help") {
        return cmd::help::print_command(command, json);
    }
    match (command.run)(&parsed) {
        Ok(report) => report.finish(json),
        Err(f) => fail(command.name, &f, json),
    }
}
