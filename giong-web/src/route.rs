//! Route patterns, and the table that matches request paths against them.
//!
//! A pattern is a path whose segments are plain text, a parameter (`{id}`,
//! one non-empty segment) or, last, a catch-all (`{*rest}`, one or more
//! segments). Matching prefers plain text to a parameter and a parameter to a
//! catch-all at every segment, and backtracks: `/users/me` beats
//! `/users/{id}`, and `/users/{id}/posts` still matches `/users/me/posts`
//! when only the parameter route goes on to `posts`.
//!
//! The method picks among the paths that match: the most specific path
//! with a route for it wins. So the `Allow` of a `405` or of the automatic
//! answer to `OPTIONS` names the methods of every path that matches.

use crate::percent;

#[cfg(any(test, fuzzing))]
pub(crate) mod model;

/// Most parameters one pattern may have.
pub(crate) const MAX_PARAMS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Plain text; empty only for a trailing slash (`/users/`) or `/`.
    Static(String),
    Param(String),
    CatchAll(String),
}

/// A parsed route pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub(crate) segments: Vec<Segment>,
}

impl Pattern {
    /// Parses `pattern`, or says what is wrong with it.
    pub(crate) fn parse(pattern: &str) -> Result<Pattern, String> {
        let Some(body) = pattern.strip_prefix('/') else {
            return Err("a pattern starts with `/`".into());
        };
        let parts: Vec<&str> = body.split('/').collect();
        let mut segments = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            segments.push(parse_segment(part, last)?);
        }
        let pattern = Pattern { segments };
        pattern.check_names()?;
        Ok(pattern)
    }

    /// The parameter names, in order.
    pub(crate) fn names(&self) -> Vec<Box<str>> {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Param(n) | Segment::CatchAll(n) => Some(n.as_str().into()),
                Segment::Static(_) => None,
            })
            .collect()
    }

    fn check_names(&self) -> Result<(), String> {
        let names = self.names();
        if names.len() > MAX_PARAMS {
            return Err(format!(
                "{} parameters; a pattern may have {MAX_PARAMS} at most",
                names.len()
            ));
        }
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                return Err(format!("the parameter `{name}` appears twice"));
            }
        }
        Ok(())
    }

    /// `true` for a pattern that may be a prefix for [`nest`](Self::nest):
    /// no catch-all, no trailing slash.
    pub(crate) fn nestable(&self) -> Result<(), String> {
        if self.is_root() {
            return Ok(());
        }
        match self.segments.last() {
            Some(Segment::CatchAll(_)) => Err("a prefix cannot end with a catch-all".into()),
            Some(Segment::Static(s)) if s.is_empty() => {
                Err("a prefix cannot end with `/`: write `/api`, not `/api/`".into())
            }
            _ => Ok(()),
        }
    }

    /// `/` alone.
    fn is_root(&self) -> bool {
        matches!(self.segments.as_slice(), [Segment::Static(s)] if s.is_empty())
    }

    /// Whether the last segment is a catch-all.
    pub(crate) fn ends_with_catch_all(&self) -> bool {
        matches!(self.segments.last(), Some(Segment::CatchAll(_)))
    }

    /// `inner` under the prefix `self`. `/` on either side adds nothing:
    /// `/api` with `/` is `/api`, and `/` with `/users` is `/users`.
    pub(crate) fn nest(&self, inner: &Pattern) -> Result<Pattern, String> {
        let joined = self.join(inner);
        joined.check_names()?;
        Ok(joined)
    }

    /// `inner` under `self`, without checking the parameter names: for the
    /// prefixes routers are nested at, whose names are never read.
    pub(crate) fn join(&self, inner: &Pattern) -> Pattern {
        if self.is_root() {
            inner.clone()
        } else if inner.is_root() {
            self.clone()
        } else {
            let mut segments = self.segments.clone();
            segments.extend(inner.segments.iter().cloned());
            Pattern { segments }
        }
    }

    /// Every path under the prefix `self`, itself left out: `/api/{*}` for
    /// `/api`, `/{*}` for `/`.
    pub(crate) fn below(&self) -> Pattern {
        let rest = Segment::CatchAll(String::new());
        if self.is_root() {
            return Pattern {
                segments: vec![rest],
            };
        }
        let mut segments = self.segments.clone();
        segments.push(rest);
        Pattern { segments }
    }

    /// The pattern with parameter names left out: two routes with the same
    /// method and shape would match exactly the same requests.
    pub(crate) fn shape(&self) -> String {
        let mut out = String::new();
        for s in &self.segments {
            out.push('/');
            match s {
                Segment::Static(text) => out.push_str(text),
                Segment::Param(_) => out.push_str("{}"),
                Segment::CatchAll(_) => out.push_str("{*}"),
            }
        }
        out
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for s in &self.segments {
            match s {
                Segment::Static(text) => write!(f, "/{text}")?,
                Segment::Param(name) => write!(f, "/{{{name}}}")?,
                Segment::CatchAll(name) => write!(f, "/{{*{name}}}")?,
            }
        }
        Ok(())
    }
}

fn parse_segment(part: &str, last: bool) -> Result<Segment, String> {
    if part.is_empty() {
        return if last {
            Ok(Segment::Static(String::new()))
        } else {
            Err("empty segment (`//`)".into())
        };
    }
    if let Some(inner) = part.strip_prefix('{') {
        let Some(inner) = inner.strip_suffix('}') else {
            return Err(format!(
                "segment `{part}`: a segment is either plain text or one parameter, such as `{{id}}`"
            ));
        };
        if let Some(name) = inner.strip_prefix('*') {
            check_name(name, part)?;
            if !last {
                return Err(format!("the catch-all `{part}` must be the last segment"));
            }
            return Ok(Segment::CatchAll(name.into()));
        }
        check_name(inner, part)?;
        return Ok(Segment::Param(inner.into()));
    }
    if part.contains(['{', '}']) {
        return Err(format!(
            "segment `{part}`: a segment is either plain text or one parameter, such as `{{id}}`"
        ));
    }
    if part == "." || part == ".." {
        return Err(format!("segment `{part}`: dot segments are not allowed"));
    }
    if let Some(bad) = part.chars().find(|&c| !is_pchar(c)) {
        return Err(format!(
            "segment `{part}`: `{bad}` is not allowed; use letters, digits and `-._~!$&'()*+,;=:@` \
             (write text as it is, not percent-encoded)"
        ));
    }
    Ok(Segment::Static(part.into()))
}

/// A character a static segment may have: RFC 3986 `pchar` without
/// percent-encoding.
fn is_pchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@".contains(c)
}

fn check_name(name: &str, part: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "segment `{part}`: a parameter name is letters, digits and `_`, not starting with a digit"
        ))
    }
}

/// An HTTP method, as routes and requests name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Other(Box<str>),
}

impl Method {
    pub(crate) fn parse(s: &str) -> Method {
        match s {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            other => Method::Other(other.into()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Other(s) => s,
        }
    }
}

/// The routes registered at one path, by method.
#[derive(Debug, Default)]
pub(crate) struct Methods(Vec<(Method, usize)>);

impl Methods {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The route for `method`. `HEAD` falls back to `GET`.
    fn find(&self, method: &Method) -> Option<usize> {
        let exact = self.0.iter().find(|(m, _)| m == method).map(|&(_, r)| r);
        match (exact, method) {
            (None, Method::Head) => self.find(&Method::Get),
            _ => exact,
        }
    }

    fn insert(&mut self, method: Method, route: usize) -> Result<(), usize> {
        if let Some(&(_, existing)) = self.0.iter().find(|(m, _)| *m == method) {
            return Err(existing);
        }
        self.0.push((method, route));
        Ok(())
    }
}

/// Every path that matched a request no route took, most specific first,
/// each with its routes: what `405`, the automatic answer to `OPTIONS` and
/// their `Allow` header are made of.
#[derive(Debug)]
pub(crate) struct Allowed<'t>(Vec<&'t Methods>);

impl Allowed<'_> {
    /// The paths that matched, most specific first, as the routes at each.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn paths(&self) -> impl Iterator<Item = &Methods> + '_ {
        self.0.iter().copied()
    }

    /// The first route added at the most specific path that matched: its
    /// middleware answer the request.
    pub(crate) fn first_route(&self) -> usize {
        // `lookup` only makes an `Allowed` from at least one path, and a
        // path is only kept with at least one route.
        self.0[0].0[0].1
    }

    /// The `Allow` header: every method that would route to the request's
    /// path, from the most specific path to the least (the method picks
    /// among the paths that match, so `DELETE /users/me` may go to
    /// `/users/{id}`); `HEAD` where `GET` has a route, and `OPTIONS`, which
    /// the router answers itself.
    pub(crate) fn allow(&self) -> String {
        let mut out: Vec<&str> = Vec::new();
        for methods in &self.0 {
            for (m, _) in &methods.0 {
                if !out.contains(&m.as_str()) {
                    out.push(m.as_str());
                }
            }
        }
        if out.contains(&"GET") && !out.contains(&"HEAD") {
            out.push("HEAD");
        }
        if !out.contains(&"OPTIONS") {
            out.push("OPTIONS");
        }
        out.join(", ")
    }
}

#[derive(Debug, Default)]
struct Node {
    /// Sorted by text, for binary search.
    statics: Vec<(Box<str>, Node)>,
    param: Option<Box<Node>>,
    catch_all: Methods,
    here: Methods,
}

impl Node {
    fn static_child(&self, text: &str) -> Option<&Node> {
        self.statics
            .binary_search_by(|(t, _)| (**t).cmp(text))
            .ok()
            .map(|i| &self.statics[i].1)
    }

    fn static_child_mut(&mut self, text: &str) -> &mut Node {
        let i = match self.statics.binary_search_by(|(t, _)| (**t).cmp(text)) {
            Ok(i) => i,
            Err(i) => {
                self.statics.insert(i, (text.into(), Node::default()));
                i
            }
        };
        &mut self.statics[i].1
    }
}

/// Where the parameters of a match are in the path: byte ranges.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Captures {
    len: usize,
    spans: [(u32, u32); MAX_PARAMS],
}

impl Captures {
    pub(crate) const fn new() -> Self {
        Self {
            len: 0,
            spans: [(0, 0); MAX_PARAMS],
        }
    }

    fn push(&mut self, start: usize, end: usize) {
        // Patterns have at most MAX_PARAMS parameters, so a match never
        // pushes more.
        self.spans[self.len] = (start as u32, end as u32);
        self.len += 1;
    }

    fn pop(&mut self) {
        self.len -= 1;
    }

    pub(crate) fn get(&self, i: usize) -> Option<(usize, usize)> {
        (i < self.len).then(|| {
            let (s, e) = self.spans[i];
            (s as usize, e as usize)
        })
    }
}

/// What the table found for a request.
#[derive(Debug)]
pub(crate) enum Lookup<'t> {
    /// This route, with the parameters in the captures.
    Found(usize),
    /// `OPTIONS` on a path with routes but no `OPTIONS` route of its own.
    Options(Allowed<'t>),
    /// Routes match the path, none for the method.
    NotAllowed(Allowed<'t>),
    NotFound,
}

/// Every route of a router, as a tree of path segments.
#[derive(Debug, Default)]
pub(crate) struct Table {
    root: Node,
}

impl Table {
    /// Adds `route` for `method` at `pattern`; `Err` with the route already
    /// there.
    pub(crate) fn insert(
        &mut self,
        pattern: &Pattern,
        method: Method,
        route: usize,
    ) -> Result<(), usize> {
        let mut node = &mut self.root;
        for seg in &pattern.segments {
            node = match seg {
                Segment::Static(text) => node.static_child_mut(text),
                Segment::Param(_) => node.param.get_or_insert_with(Default::default),
                Segment::CatchAll(_) => return node.catch_all.insert(method, route),
            };
        }
        node.here.insert(method, route)
    }

    /// Finds the route for `method` and `path`, filling in `caps`.
    ///
    /// The most specific path with a route for the method wins; if no path
    /// has one, the request gets `405` or the automatic answer to
    /// `OPTIONS`, with every path that matched.
    pub(crate) fn lookup<'t>(
        &'t self,
        method: &Method,
        path: &str,
        caps: &mut Captures,
    ) -> Lookup<'t> {
        if !path.starts_with('/') {
            return Lookup::NotFound;
        }
        // Filled only when a more specific path lacks the method, so the
        // usual hit does not allocate.
        let mut matched = Vec::new();
        if let Some(route) = search(&self.root, path, 0, method, caps, &mut matched) {
            return Lookup::Found(route);
        }
        // Without a hit the search tried every path, so `matched` has all
        // those that match.
        match (matched.is_empty(), method) {
            (true, _) => Lookup::NotFound,
            (false, Method::Options) => Lookup::Options(Allowed(matched)),
            (false, _) => Lookup::NotAllowed(Allowed(matched)),
        }
    }
}

/// Depth-first search from `node`, where `path[pos..]` is the rest of the
/// path: empty, or starting with `/`.
fn search<'t>(
    node: &'t Node,
    path: &str,
    pos: usize,
    method: &Method,
    caps: &mut Captures,
    matched: &mut Vec<&'t Methods>,
) -> Option<usize> {
    if pos == path.len() {
        return check(&node.here, method, matched);
    }
    let start = pos + 1;
    let end = path[start..].find('/').map_or(path.len(), |i| start + i);
    let seg = &path[start..end];

    let child = if seg.contains('%') {
        // `%7E` and `~` are the same character; a segment that does not
        // decode matches no text.
        percent::decode(seg)
            .ok()
            .and_then(|text| node.static_child(&text))
    } else {
        node.static_child(seg)
    };
    if let Some(child) = child {
        if let Some(route) = search(child, path, end, method, caps, matched) {
            return Some(route);
        }
    }
    if let Some(child) = &node.param {
        if !seg.is_empty() {
            caps.push(start, end);
            if let Some(route) = search(child, path, end, method, caps, matched) {
                return Some(route);
            }
            caps.pop();
        }
    }
    if start < path.len() && !node.catch_all.is_empty() {
        caps.push(start, path.len());
        if let Some(route) = check(&node.catch_all, method, matched) {
            return Some(route);
        }
        caps.pop();
    }
    None
}

/// The route for `method` among the routes of a path that matched, or
/// `None`, keeping the path in `matched` for the `Allow` header.
fn check<'t>(
    methods: &'t Methods,
    method: &Method,
    matched: &mut Vec<&'t Methods>,
) -> Option<usize> {
    if methods.is_empty() {
        return None;
    }
    let found = methods.find(method);
    if found.is_none() {
        matched.push(methods);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(routes: &[(&str, &str)]) -> Table {
        let mut t = Table::default();
        for (i, (method, pattern)) in routes.iter().enumerate() {
            let p = Pattern::parse(pattern).unwrap();
            t.insert(&p, Method::parse(method), i).unwrap();
        }
        t
    }

    /// The route found and its parameters, as strings.
    fn find(t: &Table, method: &str, path: &str) -> Option<(usize, Vec<String>)> {
        let mut caps = Captures::new();
        match t.lookup(&Method::parse(method), path, &mut caps) {
            Lookup::Found(r) => {
                let params = (0..caps.len)
                    .map(|i| {
                        let (s, e) = caps.get(i).unwrap();
                        path[s..e].to_string()
                    })
                    .collect();
                Some((r, params))
            }
            _ => None,
        }
    }

    #[test]
    fn patterns_parse_or_say_why_not() {
        let p = Pattern::parse("/users/{id}/files/{*path}").unwrap();
        assert_eq!(p.to_string(), "/users/{id}/files/{*path}");
        assert_eq!(p.shape(), "/users/{}/files/{*}");
        assert_eq!(p.names(), vec!["id".into(), "path".into()] as Vec<Box<str>>);
        assert_eq!(Pattern::parse("/").unwrap().to_string(), "/");
        assert_eq!(Pattern::parse("/a/").unwrap().to_string(), "/a/");
        for (bad, why) in [
            ("users", "starts with `/`"),
            ("/a//b", "empty segment"),
            ("/a/{id", "either plain text or one parameter"),
            ("/a/x{id}", "either plain text or one parameter"),
            ("/{*rest}/more", "must be the last segment"),
            ("/{1d}", "parameter name"),
            ("/{}", "parameter name"),
            ("/{a}/{a}", "appears twice"),
            ("/caf%C3%A9", "`%` is not allowed"),
            ("/a b", "` ` is not allowed"),
            ("/..", "dot segments"),
        ] {
            let err = Pattern::parse(bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
        let many: String = (0..17).map(|i| format!("/{{p{i}}}")).collect();
        assert!(Pattern::parse(&many).unwrap_err().contains("16 at most"));
    }

    #[test]
    fn nesting_joins_prefixes() {
        let p = |s| Pattern::parse(s).unwrap();
        assert_eq!(
            p("/api").nest(&p("/users")).unwrap().to_string(),
            "/api/users"
        );
        assert_eq!(p("/api").nest(&p("/")).unwrap().to_string(), "/api");
        assert_eq!(p("/").nest(&p("/users")).unwrap().to_string(), "/users");
        assert_eq!(
            p("/t/{id}").nest(&p("/u/{id}")).unwrap_err(),
            "the parameter `id` appears twice"
        );
        assert!(p("/api/").nestable().is_err());
        assert!(p("/{*all}").nestable().is_err());
        assert!(p("/").nestable().is_ok());
    }

    #[test]
    fn text_beats_parameter_beats_catch_all() {
        let t = table(&[
            ("GET", "/users/{id}"),
            ("GET", "/users/me"),
            ("GET", "/users/{id}/posts/{post}"),
            ("GET", "/files/{*path}"),
            ("GET", "/"),
            ("GET", "/users/"),
        ]);
        assert_eq!(find(&t, "GET", "/users/me"), Some((1, vec![])));
        assert_eq!(find(&t, "GET", "/users/7"), Some((0, vec!["7".into()])));
        assert_eq!(
            find(&t, "GET", "/users/me/posts/3"),
            Some((2, vec!["me".into(), "3".into()]))
        );
        assert_eq!(
            find(&t, "GET", "/files/a/b.txt"),
            Some((3, vec!["a/b.txt".into()]))
        );
        assert_eq!(find(&t, "GET", "/"), Some((4, vec![])));
        assert_eq!(find(&t, "GET", "/users/"), Some((5, vec![])));
        assert_eq!(find(&t, "GET", "/files/"), None);
        assert_eq!(find(&t, "GET", "/files"), None);
        assert_eq!(find(&t, "GET", "/users"), None);
        assert_eq!(find(&t, "GET", "/users//posts/1"), None);
        assert_eq!(find(&t, "GET", "*"), None);
    }

    #[test]
    fn percent_encoded_text_matches_its_decoded_form() {
        let t = table(&[("GET", "/~me"), ("GET", "/{x}")]);
        assert_eq!(find(&t, "GET", "/%7Eme"), Some((0, vec![])));
        // A slash encoded in a segment stays inside the parameter.
        assert_eq!(find(&t, "GET", "/a%2Fb"), Some((1, vec!["a%2Fb".into()])));
    }

    #[test]
    fn the_method_picks_among_paths_and_head_falls_back_to_get() {
        let t = table(&[
            ("GET", "/users/me"),
            ("POST", "/users/{id}"),
            ("GET", "/items"),
            ("DELETE", "/items"),
        ]);
        assert_eq!(find(&t, "POST", "/users/me"), Some((1, vec!["me".into()])));
        assert_eq!(find(&t, "HEAD", "/items"), Some((2, vec![])));
        let mut caps = Captures::new();
        match t.lookup(&Method::parse("PUT"), "/items", &mut caps) {
            Lookup::NotAllowed(m) => assert_eq!(m.allow(), "GET, DELETE, HEAD, OPTIONS"),
            other => panic!("{other:?}"),
        }
        match t.lookup(&Method::Options, "/items", &mut caps) {
            Lookup::Options(m) => assert_eq!(m.allow(), "GET, DELETE, HEAD, OPTIONS"),
            other => panic!("{other:?}"),
        }
        // Every path that matches counts: PUT /users/me has nowhere to go,
        // but POST /users/me goes to /users/{id}.
        match t.lookup(&Method::Put, "/users/me", &mut caps) {
            Lookup::NotAllowed(m) => {
                assert_eq!(m.allow(), "GET, POST, HEAD, OPTIONS");
                assert_eq!(m.first_route(), 0);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            t.lookup(&Method::Get, "/nothing", &mut caps),
            Lookup::NotFound
        ));
    }

    #[test]
    fn the_same_method_and_shape_cannot_be_added_twice() {
        let mut t = table(&[("GET", "/a/{x}"), ("GET", "/b/{*rest}")]);
        let dup = Pattern::parse("/a/{y}").unwrap();
        assert_eq!(t.insert(&dup, Method::Get, 9), Err(0));
        assert_eq!(t.insert(&dup, Method::Post, 9), Ok(()));
        let dup = Pattern::parse("/b/{*other}").unwrap();
        assert_eq!(t.insert(&dup, Method::Get, 9), Err(1));
    }
}
