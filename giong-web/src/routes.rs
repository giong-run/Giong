//! [`RouteList`]: what a built [`App`](crate::App) serves, for tools.

use std::cell::RefCell;
use std::fmt;
use std::fmt::Write as _;
use std::panic::Location;
use std::rc::{Rc, Weak};

use crate::json::escape_into;

/// The routes of a built [`App`](crate::App), in the order they were added:
/// what tools read to list a service's endpoints, write an OpenAPI
/// document or show them on a dashboard.
///
/// Get one with [`App::routes`](crate::App::routes), or every one alive on
/// the current worker with [`RouteList::live`]. Nothing here runs while
/// requests are served: the list is made once, when the router is built.
///
/// ```
/// use giong_web::{Error, Req, Response, Router};
///
/// async fn user(_req: Req) -> Result<Response, Error> {
///     Ok(Response::text("ok"))
/// }
///
/// let app = Router::new()
///     .get("/users/{id}", user)
///     .delete("/users/{id}", user)
///     .build(());
/// let routes = app.routes();
/// assert_eq!(routes.len(), 2);
/// let first = routes.iter().next().unwrap();
/// assert_eq!(first.method(), "GET");
/// assert_eq!(first.pattern(), "/users/{id}");
/// assert_eq!(&*first.params()[0], "id");
/// ```
///
/// Besides the routes listed, the router answers `HEAD` with a path's
/// `GET` route and `OPTIONS` by itself, unless routes for them were added
/// (see [`Router`](crate::Router)).
pub struct RouteList {
    routes: Box<[RouteEntry]>,
}

/// One route of a [`RouteList`].
pub struct RouteEntry {
    method: Box<str>,
    pattern: Box<str>,
    params: Box<[Box<str>]>,
    catch_all: bool,
    stream_limit: Option<u64>,
    middleware: usize,
    added_at: &'static Location<'static>,
}

thread_local! {
    /// The route lists of the apps built on this thread, each with a weak
    /// reference to its app's [`Alive`] token: an entry goes once its app
    /// is dropped, whoever still holds the list.
    static LIVE: RefCell<Vec<(Weak<Alive>, Rc<RouteList>)>> = const { RefCell::new(Vec::new()) };
}

/// Held by an [`App`](crate::App) for as long as it exists, so that
/// [`RouteList::live`] stops reporting it once it is dropped.
#[derive(Debug)]
pub(crate) struct Alive;

impl RouteList {
    /// Makes the list of a new app and registers it on this thread; the app
    /// keeps both.
    pub(crate) fn new(routes: Vec<RouteEntry>) -> (Rc<RouteList>, Rc<Alive>) {
        let list = Rc::new(RouteList {
            routes: routes.into_boxed_slice(),
        });
        let alive = Rc::new(Alive);
        // Fails only while the thread is being torn down: the list is then
        // not reported, which is all that can go wrong.
        let _ = LIVE.try_with(|live| {
            let mut live = live.borrow_mut();
            live.retain(|(a, _)| a.strong_count() > 0);
            live.push((Rc::downgrade(&alive), list.clone()));
        });
        (list, alive)
    }

    /// The route lists of every [`App`](crate::App) built on the current
    /// thread and not dropped yet, oldest first.
    ///
    /// On a giong worker this is the app (or apps) that worker serves, so
    /// a tool such as giong-mcp can list a running service's routes without
    /// the service handing them over. An app dropped since is not
    /// included, even if a list returned earlier is still held.
    pub fn live() -> Vec<Rc<RouteList>> {
        LIVE.try_with(|live| {
            let mut live = live.borrow_mut();
            live.retain(|(a, _)| a.strong_count() > 0);
            live.iter().map(|(_, l)| l.clone()).collect()
        })
        .unwrap_or_default()
    }

    /// The routes, in the order they were added.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &RouteEntry> + '_ {
        self.routes.iter()
    }

    /// How many routes there are.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// The list as one JSON object, for tools:
    ///
    /// ```json
    /// {"routes":[{"method":"GET","pattern":"/users/{id}","params":["id"],"catch_all":false,"stream_limit":null,"middleware":2,"added_at":"src/main.rs:31:14"}]}
    /// ```
    ///
    /// Each field is the [`RouteEntry`] method of the same name. Fields may
    /// be added in later versions; none will change meaning.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(32 + 160 * self.routes.len());
        out.push_str("{\"routes\":[");
        for (i, r) in self.routes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            r.write_json(&mut out);
        }
        out.push_str("]}");
        out
    }
}

impl RouteEntry {
    pub(crate) fn new(
        method: &str,
        pattern: Box<str>,
        params: Box<[Box<str>]>,
        catch_all: bool,
        stream_limit: Option<u64>,
        middleware: usize,
        added_at: &'static Location<'static>,
    ) -> Self {
        Self {
            method: method.into(),
            pattern,
            params,
            catch_all,
            stream_limit,
            middleware,
            added_at,
        }
    }

    /// The method, such as `"GET"`, as given (methods are case-sensitive).
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The full pattern, with the prefixes of the routers it was nested
    /// in, such as `"/api/users/{id}"`.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The names of the pattern's parameters, in order; for a catch-all
    /// (`{*path}`) the last one.
    pub fn params(&self) -> &[Box<str>] {
        &self.params
    }

    /// Whether the last parameter is a catch-all, whose value may span
    /// several segments.
    pub fn catch_all(&self) -> bool {
        self.catch_all
    }

    /// The body limit in bytes when the route reads its request body as a
    /// stream ([`Router::stream_bodies`](crate::Router::stream_bodies)),
    /// `None` when the body is received in full first.
    pub fn stream_limit(&self) -> Option<u64> {
        self.stream_limit
    }

    /// How many middleware run around the handler: those of the outermost
    /// router and of every router it was nested in.
    pub fn middleware(&self) -> usize {
        self.middleware
    }

    /// Where the route was added (`file:line:column`).
    pub fn added_at(&self) -> &'static Location<'static> {
        self.added_at
    }

    fn write_json(&self, out: &mut String) {
        out.push_str("{\"method\":");
        escape_into(out, &self.method);
        out.push_str(",\"pattern\":");
        escape_into(out, &self.pattern);
        out.push_str(",\"params\":[");
        for (i, p) in self.params.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            escape_into(out, p);
        }
        out.push_str("],\"catch_all\":");
        out.push_str(if self.catch_all { "true" } else { "false" });
        out.push_str(",\"stream_limit\":");
        match self.stream_limit {
            Some(n) => {
                let _ = write!(out, "{n}");
            }
            None => out.push_str("null"),
        }
        let _ = write!(out, ",\"middleware\":{},\"added_at\":", self.middleware);
        escape_into(out, &self.added_at.to_string());
        out.push('}');
    }
}

impl fmt::Debug for RouteList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.routes.iter()).finish()
    }
}

impl fmt::Debug for RouteEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteEntry")
            .field("method", &self.method)
            .field("pattern", &self.pattern)
            .field("params", &self.params)
            .field("catch_all", &self.catch_all)
            .field("stream_limit", &self.stream_limit)
            .field("middleware", &self.middleware)
            .field("added_at", &self.added_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_every_string() {
        let list = RouteList {
            routes: vec![RouteEntry::new(
                "PROP\"FIND",
                "/a/\"b\"/{x}".into(),
                vec!["x".into()].into(),
                false,
                Some(7),
                0,
                Location::caller(),
            )]
            .into(),
        };
        let json = list.to_json();
        assert!(json.contains(r#""method":"PROP\"FIND""#), "{json}");
        assert!(json.contains(r#""pattern":"/a/\"b\"/{x}""#), "{json}");
        assert!(json.contains(r#""stream_limit":7"#), "{json}");
    }

    #[test]
    fn an_empty_list_is_valid_json() {
        let list = RouteList {
            routes: Vec::new().into(),
        };
        assert_eq!(list.to_json(), r#"{"routes":[]}"#);
        assert!(list.is_empty());
    }
}
