//! `giong dashboard`: the local dashboard in the browser, for the service
//! `giong dev` runs in this project (`giong dev --ui` starts the same).

use std::path::PathBuf;

use serde_json::json;

use super::routes::{BASE_FLAG, MCP_FLAG};
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dashboard::{Dashboard, Target, PORT};
use crate::report::{Failure, Report};
use crate::service;

pub const COMMAND: Command = Command {
    name: "dashboard",
    summary: "open the local dashboard of the running service in the browser",
    usage: "giong dashboard [--port <n>] [--mcp <address>] [--base-url <url>] [--log <file>]\n\n\
            Serves a page on 127.0.0.1:7700 and prints its address, \
            http://127.0.0.1:7700/#t=<token>: the token is new on each start and the page's \
            API refuses requests without it (GIONG-E5051), so only the person who started it can \
            use it. The page shows the \
            service giong dev runs in this project: build state and the last error, the live \
            request log with status and time taken (bodies too when the router has giong-web's \
            Record middleware), routes with curl lines and an OpenAPI download, workers and \
            tasks by spawn line with a one-second profile, connection pools, the service's \
            warnings with their error pages, and a box to send requests. It reads what the \
            service already gives (its output, which giong dev keeps in .giong/dev.log, and its \
            giong-mcp): the service runs no extra code for it. `giong dev --ui` starts it \
            together with the service. Stop it with Ctrl-C.",
    flags: &[
        Flag {
            name: "port",
            kind: Kind::Value,
            help: "the port on 127.0.0.1 (default 7700)",
        },
        MCP_FLAG,
        BASE_FLAG,
        Flag {
            name: "log",
            kind: Kind::Value,
            help: "the service's output to read, when giong dev does not run it",
        },
    ],
    max_words: 0,
    run,
};

fn run(args: &Args) -> Result<Report, Failure> {
    let port: u16 = args.number("port")?.unwrap_or(PORT);
    let target = Target {
        root: service::project_root(),
        mcp: args.value("mcp").map(str::to_string),
        base_url: args.value("base-url").map(str::to_string),
        log: args.value("log").map(PathBuf::from),
    };
    if let Some(log) = &target.log {
        if !log.is_file() {
            return Err(Failure::new(
                Code::USAGE,
                format!("--log {} is not a file", log.display()),
                "give --log the file the service's output goes to",
            ));
        }
    }
    let d = Dashboard::bind(port, target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            Failure::new(
                Code::DASHBOARD_PORT_IN_USE,
                format!("127.0.0.1:{port} is in use"),
                "another dashboard may be open already (giong dev --ui starts one); open it, \
                 or pick another port with --port",
            )
        } else {
            Failure::new(
                Code::DASHBOARD_FAILED,
                format!("the dashboard cannot listen on 127.0.0.1:{port}: {e}"),
                "pick another port with --port",
            )
        }
    })?;
    let url = d.url();
    let mut r = Report::new("dashboard");
    r.data("url", json!(url));
    if args.json() {
        // The one document, printed before the server blocks, so an agent
        // reads the address while the page is up.
        println!("{}", r.to_json());
    } else {
        eprintln!("giong dashboard: {url} (Ctrl-C stops it)");
    }
    d.serve();
    // Already printed: a second document would break `--json`.
    Ok(r.quiet())
}
