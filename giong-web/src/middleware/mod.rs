//! The middleware giong-web comes with.
//!
//! Add them with [`Router::layer`](crate::Router::layer), in this order when
//! you use several: [`RequestId`] first, so that everything after it knows
//! the id; [`Logger`] next, so that it sees every response, including those
//! the others make; then [`Cors`], [`Compression`] and [`Timeout`].
//!
//! ```no_run
//! use std::time::Duration;
//! use giong_web::middleware::{Compression, Cors, Logger, RequestId, Timeout};
//! use giong_web::{Router, Sink};
//!
//! # async fn handler(_: giong_web::Req) -> Result<giong_web::Response, giong_web::Error> { todo!() }
//! let app = Router::new()
//!     .get("/", handler)
//!     .layer(RequestId::new())
//!     .layer(Logger::new(Sink::Stdout))
//!     .layer(Cors::new().allow_origin("https://app.example.com"))
//!     .layer(Compression::new())
//!     .layer(Timeout::new(Duration::from_secs(10)))
//!     .build(());
//! ```

mod compression;
mod cors;
mod logger;
mod request_id;
mod timeout;

#[cfg(any(test, fuzzing))]
pub(crate) use compression::gunzip;
pub use compression::Compression;
pub use cors::Cors;
pub use logger::Logger;
pub use request_id::RequestId;
pub use timeout::Timeout;

/// Panics with `GIONG-E1213` for a middleware setting that cannot work.
#[track_caller]
pub(crate) fn invalid_config(message: String, fix: &'static str) -> ! {
    let err = giong::error::Error::new(giong::error::Code::WebConfig, message, fix);
    panic!("{err}");
}
