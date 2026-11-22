//! `giong help [command]`, `giong --help`, `giong <command> --help`.

use std::process::ExitCode;

use serde_json::json;

use super::{find, unknown, Command, COMMANDS};
use crate::args::{Args, Kind, GLOBAL};
use crate::report::{Failure, Report};

pub const COMMAND: Command = Command {
    name: "help",
    summary: "print the commands, or how to use one",
    usage: "giong help [command]",
    flags: &[],
    max_words: 1,
    run,
};

fn run(args: &Args) -> Result<Report, Failure> {
    match args.words.first() {
        None => Ok(overview()),
        Some(name) => find(name).map(command_report).ok_or_else(|| unknown(name)),
    }
}

fn overview() -> Report {
    let mut r = Report::new("help");
    r.line(format!(
        "giong {}: start, run, check and ship services on the Gióng runtime\n",
        env!("CARGO_PKG_VERSION")
    ));
    r.line("Usage: giong <command> [--json] [--help]\n");
    r.line("Commands:");
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in COMMANDS {
        r.line(format!("  {:width$}  {}", c.name, c.summary));
    }
    r.line("\n--json prints one JSON document (for agents); errors carry a GIONG-E code: giong explain <code>.");
    r.line("Docs: https://giong.run/docs/cli/");
    r.data(
        "commands",
        json!(COMMANDS
            .iter()
            .map(|c| json!({"name": c.name, "summary": c.summary}))
            .collect::<Vec<_>>()),
    );
    r
}

fn command_report(c: &Command) -> Report {
    let mut r = Report::new("help");
    r.line(format!("giong {}: {}\n", c.name, c.summary));
    r.line(c.usage);
    r.line("\nFlags:");
    let flags: Vec<_> = c.flags.iter().chain(GLOBAL).collect();
    for f in &flags {
        let name = match f.kind {
            Kind::Switch => format!("--{}", f.name),
            Kind::Value => format!("--{} <value>", f.name),
        };
        r.line(format!("  {name:24}  {}", f.help));
    }
    r.data("name", json!(c.name));
    r.data("summary", json!(c.summary));
    r.data("usage", json!(c.usage));
    r.data(
        "flags",
        json!(flags
            .iter()
            .map(|f| json!({"name": f.name, "takes_value": f.kind == Kind::Value, "help": f.help}))
            .collect::<Vec<_>>()),
    );
    r
}

pub fn print_overview(json: bool) -> ExitCode {
    overview().finish(json)
}

pub fn print_command(c: &Command, json: bool) -> ExitCode {
    command_report(c).finish(json)
}
