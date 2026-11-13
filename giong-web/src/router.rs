//! [`Router`], which collects routes, and [`App`], what it builds.

use std::fmt;
use std::future::Future;
use std::panic::Location;
use std::rc::Rc;

use giong::error::Code;
use giong_http::{Request, Response};

use crate::error::Rendered;
use crate::next::{BoxHandler, Chain, DynMiddleware, Endpoint};
use crate::req::RouteInfo;
use crate::route::{Captures, Lookup, Method, Pattern, Table};
use crate::{log, Error, Middleware, Next, Req};

struct RouteDef<S> {
    method: Method,
    pattern: Pattern,
    handler: BoxHandler<S>,
    /// From the routers it was nested in, outermost first.
    middleware: Vec<Rc<dyn DynMiddleware<S>>>,
    added_at: &'static Location<'static>,
}

/// The routes of a service and the middleware around them. Build one on
/// each worker, then [`build`](Router::build) it into an [`App`] to serve.
///
/// ```no_run
/// use std::time::Duration;
/// use giong::net::TcpListener;
/// use giong_web::middleware::{Logger, RequestId, Timeout};
/// use giong_web::{Error, Req, Response, Router, Server, Sink};
///
/// async fn health(_req: Req) -> Result<Response, Error> {
///     Ok(Response::text("ok"))
/// }
///
/// async fn user(req: Req) -> Result<Response, Error> {
///     let id: u64 = req.param("id")?;
///     Ok(Response::text(format!("user {id}")))
/// }
///
/// fn main() -> std::io::Result<()> {
///     giong::Builder::new()
///         .shutdown_on_signals(true)
///         .build()?
///         .run(|_worker| async {
///             let app = Router::new()
///                 .get("/health", health)
///                 .get("/users/{id}", user)
///                 .layer(RequestId::new())
///                 .layer(Logger::new(Sink::Stdout))
///                 .layer(Timeout::new(Duration::from_secs(10)))
///                 .build(());
///             let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
///             Server::new().serve(listener, app).await;
///         })
/// }
/// ```
///
/// ## Patterns
///
/// A pattern is a path whose segments are plain text, a parameter `{name}`
/// (one segment, at least one character) or, as the last segment, a
/// catch-all `{*name}` (the rest of the path, at least one character). At
/// each segment plain text is tried first, then a parameter, then a
/// catch-all, so `/users/me` wins over `/users/{id}`. Paths match exactly: a
/// trailing slash makes a different path (`/users/` is not `/users`), and
/// the `404` for one names the other when it exists.
///
/// A bad pattern, or a second route with the same method and pattern
/// (parameter names aside), panics where it is added: `GIONG-E1201` and
/// `GIONG-E1202`.
///
/// ## Methods
///
/// `HEAD` is answered by the `GET` route when there is no `HEAD` route.
/// `OPTIONS` is answered with `204` and an `Allow` header when there is no
/// `OPTIONS` route. A path whose routes have another method gets `405` with
/// `Allow`; a path no route matches gets `404`.
///
/// ## State
///
/// Handlers get the value given to [`build`](Router::build) through
/// [`Req::state`]: database pools, configuration, clients. A router is built
/// on every worker, so the state is per worker too, and needs neither `Send`
/// nor `Sync`.
pub struct Router<S = ()> {
    routes: Vec<RouteDef<S>>,
    middleware: Vec<Rc<dyn DynMiddleware<S>>>,
}

impl<S: 'static> Default for Router<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: 'static> Router<S> {
    /// A router without routes.
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            middleware: Vec::new(),
        }
    }

    /// Adds a `GET` route (which also answers `HEAD`).
    ///
    /// # Panics
    ///
    /// Panics if the pattern is not valid (`GIONG-E1201`) or a `GET` route
    /// with the same pattern exists (`GIONG-E1202`).
    #[track_caller]
    pub fn get<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        self.add(Method::Get, pattern, handler)
    }

    /// Adds a `POST` route. Panics as [`get`](Self::get) does.
    #[track_caller]
    pub fn post<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        self.add(Method::Post, pattern, handler)
    }

    /// Adds a `PUT` route. Panics as [`get`](Self::get) does.
    #[track_caller]
    pub fn put<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        self.add(Method::Put, pattern, handler)
    }

    /// Adds a `PATCH` route. Panics as [`get`](Self::get) does.
    #[track_caller]
    pub fn patch<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        self.add(Method::Patch, pattern, handler)
    }

    /// Adds a `DELETE` route. Panics as [`get`](Self::get) does.
    #[track_caller]
    pub fn delete<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        self.add(Method::Delete, pattern, handler)
    }

    /// Adds a route for any other method, such as `OPTIONS`, `HEAD` or
    /// `PROPFIND`. Methods are case-sensitive.
    ///
    /// # Panics
    ///
    /// Panics as [`get`](Self::get) does, and if `method` is not a valid
    /// method name (`GIONG-E1201`).
    #[track_caller]
    pub fn route<F, Fut>(self, method: &str, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        if !crate::header::is_token(method) {
            invalid_route(pattern, &format!("{method:?} is not a method name"));
        }
        self.add(Method::parse(method), pattern, handler)
    }

    #[track_caller]
    fn add<F, Fut>(mut self, method: Method, pattern: &str, handler: F) -> Self
    where
        F: Fn(Req<S>) -> Fut + 'static,
        Fut: Future<Output = Result<Response, Error>> + 'static,
    {
        let parsed = match Pattern::parse(pattern) {
            Ok(p) => p,
            Err(why) => invalid_route(pattern, &why),
        };
        let handler: BoxHandler<S> = Rc::new(move |req| Box::pin(handler(req)));
        self.push(RouteDef {
            method,
            pattern: parsed,
            handler,
            middleware: Vec::new(),
            added_at: Location::caller(),
        });
        self
    }

    /// Adds a route, refusing a second one with the same method and shape.
    #[track_caller]
    fn push(&mut self, def: RouteDef<S>) {
        let shape = def.pattern.shape();
        if let Some(old) = self
            .routes
            .iter()
            .find(|r| r.method == def.method && r.pattern.shape() == shape)
        {
            let err = giong::error::Error::new(
                Code::WebDuplicateRoute,
                format!(
                    "{} {} would answer the same requests as {} {}, added at {}",
                    def.method.as_str(),
                    def.pattern,
                    old.method.as_str(),
                    old.pattern,
                    old.added_at
                ),
                "remove one of the two routes, or give them different paths.",
            );
            panic!("{err}");
        }
        self.routes.push(def);
    }

    /// Adds `middleware` around every route of this router, wherever the
    /// router ends up nested. On the outermost router it also runs for the
    /// requests answered with `404`, `405` or the automatic `OPTIONS`.
    ///
    /// Middleware run in the order they were added: the first one added
    /// sees the request first.
    pub fn layer<M: Middleware<S>>(mut self, middleware: M) -> Self {
        self.middleware.push(Rc::new(middleware));
        self
    }

    /// Adds every route of `router` under `prefix`, each with `router`'s
    /// middleware around it.
    ///
    /// `nest("/api", r)` turns `r`'s `/users` into `/api/users` and its `/`
    /// into `/api`. The prefix may have parameters (`/teams/{team}`), which
    /// the nested handlers read like their own.
    ///
    /// # Panics
    ///
    /// Panics if `prefix` is not a valid pattern, ends with `/` or a
    /// catch-all, or shares a parameter name with a nested route
    /// (`GIONG-E1201`), or if a nested route duplicates an existing one
    /// (`GIONG-E1202`).
    #[track_caller]
    pub fn nest(mut self, prefix: &str, router: Router<S>) -> Self {
        let parsed = Pattern::parse(prefix).and_then(|p| p.nestable().map(|()| p));
        let prefix_pattern = match parsed {
            Ok(p) => p,
            Err(why) => invalid_route(prefix, &why),
        };
        for def in router.routes {
            let pattern = match prefix_pattern.nest(&def.pattern) {
                Ok(p) => p,
                Err(why) => invalid_route(&format!("{prefix_pattern} + {}", def.pattern), &why),
            };
            let mut middleware = router.middleware.clone();
            middleware.extend(def.middleware);
            self.push(RouteDef {
                pattern,
                middleware,
                ..def
            });
        }
        self
    }

    /// Finishes the router: the [`App`] that answers requests, with `state`
    /// for the handlers. Pass `()` when they need none.
    pub fn build(self, state: S) -> App<S> {
        let mut table = Table::default();
        let top: Box<Chain<S>> = self.middleware.into_boxed_slice();
        let mut routes = Vec::with_capacity(self.routes.len());
        for (i, def) in self.routes.into_iter().enumerate() {
            // Duplicates were refused when the routes were added.
            let _ = table.insert(&def.pattern, def.method, i);
            let chain: Box<Chain<S>> = top.iter().cloned().chain(def.middleware).collect();
            routes.push(Route {
                info: Rc::new(RouteInfo {
                    pattern: def.pattern.to_string().into(),
                    names: def.pattern.names().into(),
                }),
                chain,
                handler: def.handler,
            });
        }
        App {
            state: Rc::new(state),
            table,
            routes: routes.into(),
            top,
        }
    }
}

#[track_caller]
fn invalid_route(pattern: &str, why: &str) -> ! {
    let err = giong::error::Error::new(
        Code::WebInvalidRoute,
        format!("invalid route pattern {pattern:?}: {why}"),
        "write patterns such as \"/users/{id}\" or \"/files/{*path}\".",
    );
    panic!("{err}");
}

impl<S> fmt::Debug for Router<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let routes: Vec<String> = self
            .routes
            .iter()
            .map(|r| format!("{} {}", r.method.as_str(), r.pattern))
            .collect();
        f.debug_struct("Router")
            .field("routes", &routes)
            .field("middleware", &self.middleware.len())
            .finish()
    }
}

struct Route<S> {
    info: Rc<RouteInfo>,
    /// The outermost router's middleware, then those of the routers this
    /// route was nested in.
    chain: Box<Chain<S>>,
    handler: BoxHandler<S>,
}

/// A finished [`Router`]: give it to [`Server::serve`](crate::Server::serve),
/// or call [`handle`](App::handle) directly in tests.
pub struct App<S = ()> {
    state: Rc<S>,
    table: Table,
    routes: Box<[Route<S>]>,
    /// The outermost router's middleware, for `404`, `405` and `OPTIONS`.
    top: Box<Chain<S>>,
}

impl<S: 'static> App<S> {
    /// Answers one request: routing, middleware, handler.
    ///
    /// The server calls this for every request. Tests can too, with a
    /// request made by [`Request::from_bytes`]:
    ///
    /// ```
    /// use giong_web::{Error, Req, Request, Response, Router};
    ///
    /// async fn hello(req: Req) -> Result<Response, Error> {
    ///     let name: String = req.param("name")?;
    ///     Ok(Response::text(format!("hello {name}")))
    /// }
    ///
    /// let app = Router::new().get("/hello/{name}", hello).build(());
    /// let req = Request::from_bytes(b"GET /hello/ada HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    /// let resp = giong::block_on(app.handle(req));
    /// assert_eq!(resp.status(), 200);
    /// assert_eq!(resp.body_bytes(), b"hello ada");
    /// ```
    pub async fn handle(&self, http: Request) -> Response {
        let method = Method::parse(http.method());
        let mut caps = Captures::new();
        let (route, endpoint) = match self.table.lookup(&method, http.path(), &mut caps) {
            Lookup::Found(i) => {
                let route = &self.routes[i];
                (Some(route), Endpoint::Handler(&route.handler))
            }
            Lookup::NotAllowed(methods) => (None, Endpoint::NotAllowed(methods.allow())),
            Lookup::Options(methods) => (None, Endpoint::Options(methods.allow())),
            Lookup::NotFound => (None, Endpoint::NotFound(self.hint(http.path()))),
        };
        let chain = route.map_or(&*self.top, |r| &*r.chain);
        let info = route.map(|r| r.info.clone());
        let req = Req::new(http, self.state.clone(), info.clone(), caps);
        let resp = Next { chain, endpoint }.run(req).await;
        if let Some(rendered) = resp.extension::<Rendered>() {
            if rendered.error.status() >= 500 && !rendered.logged.get() {
                rendered.logged.set(true);
                log::unlogged_error(rendered, method.as_str(), info.as_deref());
            }
        }
        resp
    }

    /// For a `404`: the same path with or without a trailing slash, when a
    /// route has it.
    fn hint(&self, path: &str) -> Option<String> {
        let other = match path.strip_suffix('/') {
            Some("") => return None,
            Some(without) => without.to_string(),
            None => format!("{path}/"),
        };
        let mut caps = Captures::new();
        match self.table.lookup(&Method::Get, &other, &mut caps) {
            Lookup::NotFound => None,
            _ => Some(other),
        }
    }
}

impl<S: 'static> giong_http::Handler for App<S> {
    fn call(&self, req: Request) -> impl Future<Output = Response> {
        self.handle(req)
    }
}

impl<S> Drop for App<S> {
    fn drop(&mut self) {
        // Workers drop their app as they stop: write out what this one
        // logged before the process can exit.
        log::flush();
    }
}

impl<S> fmt::Debug for App<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let routes: Vec<&str> = self.routes.iter().map(|r| &*r.info.pattern).collect();
        f.debug_struct("App")
            .field("routes", &routes)
            .field("middleware", &self.top.len())
            .finish()
    }
}
