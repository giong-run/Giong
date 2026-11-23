//! `giong`: start, run, check and ship services on the Gióng async runtime.
//!
//! One binary, one command per job. Every command prints text for people or,
//! with `--json`, one JSON document for agents (see [`report`]). Commands are
//! registered in [`cmd::COMMANDS`].

// While the commands arrive (three branches), some shared helpers have no
// caller yet. Removed before the pull request leaves draft.
#![allow(dead_code)]

mod args;
mod build;
mod cmd;
mod code;
mod dotenv;
mod hist;
mod host;
mod lint;
#[rustfmt::skip] // generated (see src/pages.rs)
mod pages;
mod loadgen;
mod procs;
mod project;
mod report;
mod service;
mod sha256;
mod watch;

use std::process::ExitCode;

use crate::report::fail;

fn main() -> ExitCode {
    let mut argv = Vec::new();
    for (i, a) in std::env::args_os().skip(1).enumerate() {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => {
                let failure = report::Failure::new(
                    code::Code::USAGE,
                    format!("argument {} ({a:?}) is not valid UTF-8", i + 1),
                    "giong takes names and paths in UTF-8; rename the file or directory",
                );
                let json = std::env::args_os().any(|a| a == "--json");
                return fail("giong", &failure, json);
            }
        }
    }
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
            if let Some(bad) = argv
                .iter()
                .take_while(|a| *a != "--")
                .find(|a| !matches!(a.as_str(), "--help" | "--version" | "--json"))
            {
                let failure = report::Failure::new(
                    code::Code::USAGE,
                    format!("giong has no option {bad:?}"),
                    "giong --help lists the commands; options go after the command",
                );
                return fail("giong", &failure, json);
            }
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
