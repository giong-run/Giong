//! `giong version`.

use std::process::ExitCode;

use serde_json::json;

use super::Command;
use crate::args::Args;
use crate::report::{Failure, Report};

pub const COMMAND: Command = Command {
    name: "version",
    summary: "print the version of this command",
    usage:
        "giong version\n\nThe version of giong-cli, which matches the giong release it comes with.",
    flags: &[],
    max_words: 0,
    run,
};

fn run(_args: &Args) -> Result<Report, Failure> {
    let mut r = Report::new("version");
    r.line(format!("giong {}", env!("CARGO_PKG_VERSION")));
    r.data("version", json!(env!("CARGO_PKG_VERSION")));
    Ok(r)
}

/// `giong --version`.
pub fn print(json: bool) -> ExitCode {
    run(&Args::default()).expect("infallible").finish(json)
}
