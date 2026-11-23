//! [`Record`] in a release build without the `dev` feature: the type and
//! its methods, so code that adds it when asked still compiles, but no
//! recording code. Making one panics with `GIONG-E1213`.

use crate::log::Sink;
use crate::{Error, Middleware, Next, Req, Response};

/// Recording for development (see the documentation of a debug build).
/// This release build was made without giong-web's `dev` feature, so it
/// holds none of the recording code: [`Record::new`] panics with
/// `GIONG-E1213`, naming the cause and the fix.
#[derive(Clone, Debug)]
pub struct Record {
    _off: (),
}

impl Record {
    /// Panics with `GIONG-E1213`: recording is not compiled into a release
    /// build without giong-web's `dev` feature.
    #[track_caller]
    pub fn new(sink: Sink) -> Self {
        let _ = sink;
        super::invalid_config(
            "Record is for development and this release build leaves it out (built without giong-web's `dev` feature)".to_string(),
            "record with a debug build (giong dev without --release), or build with --features giong-web/dev; in production, do not add Record (it is added only when GIONG_RECORD is set in giong's templates).",
        )
    }

    /// As in a debug build; unreachable here, since [`Record::new`] panics.
    pub fn max_body(self, bytes: usize) -> Self {
        let _ = bytes;
        self
    }

    /// As in a debug build; unreachable here, since [`Record::new`] panics.
    pub fn redact_header(self, name: &str) -> Self {
        let _ = name;
        self
    }

    /// As in a debug build; unreachable here, since [`Record::new`] panics.
    pub fn redact_field(self, name: &str) -> Self {
        let _ = name;
        self
    }

    /// As in a debug build; unreachable here, since [`Record::new`] panics.
    pub fn keep_field(self, name: &str) -> Self {
        let _ = name;
        self
    }
}

impl<S: 'static> Middleware<S> for Record {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        Ok(next.run(req).await)
    }
}
