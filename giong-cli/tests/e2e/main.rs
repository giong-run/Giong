//! End-to-end tests of the `giong` command: the real binary, run in
//! directories of its own, the way people and agents run it.
//!
//! - `contract`: what every command promises (usage, coded errors, the JSON
//!   document, exit status, no hangs, no writes when it fails), for every
//!   command `giong help --json` lists, so new commands are covered at once.
//! - one file per command with its scenarios (`dev_ctl`: giong dev --background, status, logs, stop).
//! - `scenarios`: whole workflows across commands (new, dev, edit, reload).
//!
//! `GIONG_E2E_SLOW=1` also runs the scenarios that build generated projects;
//! `GIONG_E2E_KEEP=1` keeps the temporary directories. Owned by the dev kit's
//! test thread; builders add unit tests next to their code instead.

// Helpers for every command's scenarios; not each is used by every build.
#[allow(dead_code)]
mod harness;
mod schema;

mod bench;
mod cache;
mod check;
mod clean;
mod contract;
mod dashboard;
mod dev_ctl;
mod doctor;
mod env;
mod explain;
mod mcp;
mod new_ai;
mod record;
mod scenarios;
mod upgrade;
