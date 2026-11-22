//! Recorded requests: the `exchange` lines giong-web's `Record` middleware
//! writes, turned into replayable cases (`giong record save`) and replayed
//! against the running service (`giong record replay`).
//!
//! - [`exchange`]: reading `exchange` lines from a log.
//! - [`case`]: making cases from exchanges, with volatile values masked and
//!   ids the service made captured and reused by later requests.
//! - [`matcher`]: checking an answer against a case's expectation.
//! - [`replay`]: sending a case file's requests and checking the answers.
//! - [`shape`] and [`base64`]: what a value looks like, and bodies `Record`
//!   wrote as base64. These, [`exchange`] and [`matcher`] use nothing else of
//!   the crate, so a fuzz target can include them by path.

pub mod base64;
pub mod case;
pub mod exchange;
pub mod matcher;
pub mod replay;
pub mod shape;
