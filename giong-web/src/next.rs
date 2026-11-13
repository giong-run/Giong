//! The [`Middleware`] trait and [`Next`], the rest of the chain.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use giong::error::Code;
use giong_http::Response;

use crate::{Error, Req};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A route's handler with its type erased.
pub(crate) type BoxHandler<S> = Rc<dyn Fn(Req<S>) -> BoxFuture<'static, Result<Response, Error>>>;

/// Code that runs around handlers: it sees each request before the handler
/// and each response after it, and may answer by itself.
///
/// A middleware gets the request and [`Next`], the rest of the chain (more
/// middleware, then the handler). It calls [`Next::run`] to go on, or returns
/// without calling it to answer itself:
///
/// ```
/// use giong_web::{Error, Middleware, Next, Req, Response};
///
/// /// Refuses requests without the right API key.
/// struct ApiKey(&'static str);
///
/// impl<S: 'static> Middleware<S> for ApiKey {
///     async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
///         if req.header_str("x-api-key") != Some(self.0) {
///             return Err(Error::unauthorized("bad_api_key", "send a valid X-Api-Key header"));
///         }
///         let resp = next.run(req).await;
///         Ok(resp.header("X-Checked", "1"))
///     }
/// }
/// ```
///
/// [`Router::layer`](crate::Router::layer) adds a middleware to a router.
/// Middleware run in the order they were added: the first one added sees the
/// request first and the response last.
///
/// What [`Next::run`] returns is always a [`Response`]: an error returned
/// further in has already become one (see [`Error::of`] to read it back).
/// An error the middleware itself returns becomes a response the same way.
pub trait Middleware<S = ()>: 'static {
    /// Handles one request.
    fn call(&self, req: Req<S>, next: Next<'_, S>)
        -> impl Future<Output = Result<Response, Error>>;
}

/// [`Middleware`] with its type erased, for a chain of different ones.
pub(crate) trait DynMiddleware<S> {
    fn call_dyn<'a>(
        &'a self,
        req: Req<S>,
        next: Next<'a, S>,
    ) -> BoxFuture<'a, Result<Response, Error>>;
}

impl<S: 'static, M: Middleware<S>> DynMiddleware<S> for M {
    fn call_dyn<'a>(
        &'a self,
        req: Req<S>,
        next: Next<'a, S>,
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(self.call(req, next))
    }
}

pub(crate) type Chain<S> = [Rc<dyn DynMiddleware<S>>];

/// What answers once the middleware have run.
pub(crate) enum Endpoint<'a, S> {
    Handler(&'a BoxHandler<S>),
    /// No route: `404`, with a hint about a trailing slash.
    NotFound(Option<String>),
    /// `405`, with the `Allow` header's value.
    NotAllowed(String),
    /// The automatic answer to `OPTIONS`, with the `Allow` header's value.
    Options(String),
}

impl<S> Endpoint<'_, S> {
    async fn call(self, req: Req<S>) -> Result<Response, Error> {
        match self {
            Endpoint::Handler(handler) => handler(req).await,
            Endpoint::NotFound(hint) => {
                let mut detail = format!("no route for {} {}", req.method(), cut(req.path()));
                if let Some(hint) = hint {
                    detail.push_str(&format!("; did you mean {}?", cut(&hint)));
                }
                Err(Error::coded(404, Code::WebNoRoute, detail))
            }
            Endpoint::NotAllowed(allow) => {
                let detail = format!(
                    "{} is not allowed on {}; allowed: {allow}",
                    req.method(),
                    cut(req.path())
                );
                Err(Error::coded(405, Code::WebMethodNotAllowed, detail)
                    .with_header("Allow", allow))
            }
            Endpoint::Options(allow) => Ok(Response::new(204).header("Allow", &allow)),
        }
    }
}

/// A path from the client, cut short for an error detail.
fn cut(path: &str) -> &str {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path;
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    &path[..end]
}

/// The rest of the chain: the middleware after this one, then the handler.
pub struct Next<'a, S = ()> {
    pub(crate) chain: &'a Chain<S>,
    pub(crate) endpoint: Endpoint<'a, S>,
}

impl<S: 'static> Next<'_, S> {
    /// Runs the rest of the chain and returns its response. An error from
    /// further in comes back as the response it was turned into.
    pub async fn run(self, req: Req<S>) -> Response {
        let id = req.id_buf();
        let result = match self.chain.split_first() {
            Some((first, rest)) => {
                let next = Next {
                    chain: rest,
                    endpoint: self.endpoint,
                };
                first.call_dyn(req, next).await
            }
            None => self.endpoint.call(req).await,
        };
        match result {
            Ok(resp) => resp,
            Err(err) => err.into_response(id),
        }
    }
}

impl<S> fmt::Debug for Next<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("middleware_left", &self.chain.len())
            .finish()
    }
}
