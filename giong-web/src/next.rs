//! The [`Middleware`] trait and [`Next`], the rest of the chain.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

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
/// An error the middleware itself returns becomes a response the same way,
/// and so does a panic, in a middleware or in the handler: a `500` with
/// code `GIONG-E1210`, whose logged cause is "the handler panicked: ..." or
/// "a middleware panicked: ...". The middleware further out run as for any
/// other error, so the panic gets its log line and its request id, and the
/// connection carries on with its other requests.
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
            Endpoint::Handler(handler) => guarded(Who::Handler, || handler(req)).await,
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
                guarded(Who::Middleware, || first.call_dyn(req, next)).await
            }
            None => self.endpoint.call(req).await,
        };
        match result {
            Ok(resp) => resp,
            Err(err) => err.into_response(id),
        }
    }
}

/// Whose code panicked, for the cause of the `500`.
#[derive(Clone, Copy)]
enum Who {
    Handler,
    Middleware,
}

/// Starts a handler or a middleware with `start` and runs the future it
/// returns, turning a panic in either into a `500` (`GIONG-E1210`).
///
/// A panic would otherwise unwind through giong-http's connection task and
/// drop the connection with every request on it (over HTTP/2, those of the
/// other streams too), with no response, no log line and no request id.
/// Catching it at every step of the chain lets the middleware further out
/// (`Logger`, `RequestId`) see an ordinary error response, wherever the
/// panic happened.
///
/// `AssertUnwindSafe`: what the code that panicked shared with others, such
/// as the router's state behind a `RefCell` or a `Mutex`, may be left half
/// updated, the same risk as with any server that keeps running after a
/// panic. A `RefCell` borrow ends as the panic unwinds; a `Mutex` is
/// poisoned, as it would be on a thread.
async fn guarded<'a>(
    who: Who,
    start: impl FnOnce() -> BoxFuture<'a, Result<Response, Error>>,
) -> Result<Response, Error> {
    // A closure may run code of its own before it returns its future.
    match catch_unwind(AssertUnwindSafe(start)) {
        Ok(fut) => Caught { fut, who }.await,
        Err(payload) => Err(panicked(who, &*payload)),
    }
}

/// Polls `fut`, catching a panic in the poll.
struct Caught<'a> {
    fut: BoxFuture<'a, Result<Response, Error>>,
    who: Who,
}

impl Future for Caught<'_> {
    type Output = Result<Response, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let who = self.who;
        let fut = &mut self.fut;
        match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(poll) => poll,
            // `guarded` awaits this once, so the future that panicked is not
            // polled again; it is dropped with `Caught`.
            Err(payload) => Poll::Ready(Err(panicked(who, &*payload))),
        }
    }
}

/// The `500` for a panic, with its message as the logged cause. The panic
/// hook has already printed the message and where it happened to stderr.
fn panicked(who: Who, payload: &(dyn Any + Send)) -> Error {
    let what = match who {
        Who::Handler => "the handler panicked",
        Who::Middleware => "a middleware panicked",
    };
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
    let cause = match message {
        Some(message) => format!("{what}: {message}"),
        None => format!("{what} (with a payload that is not a string)"),
    };
    Error::internal(cause)
}

impl<S> fmt::Debug for Next<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("middleware_left", &self.chain.len())
            .finish()
    }
}
