//! The parsers of the `giong` command, built here from its own source files:
//! giong-cli is a binary crate, so the fuzz targets include the modules that
//! need nothing else of it. (A module that needs more of the command, such as
//! doctor's Cargo.toml edit, joins once it is self-contained.)

#![allow(dead_code)]

#[path = "../../src/code.rs"]
pub mod code;
#[path = "../../src/report.rs"]
pub mod report;
#[path = "../../src/args.rs"]
pub mod args;
#[path = "../../src/dotenv.rs"]
pub mod dotenv;
#[path = "../../src/host.rs"]
pub mod host;
#[path = "../../src/watch.rs"]
pub mod watch;
#[path = "../../src/lint.rs"]
pub mod lint;
