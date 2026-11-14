//! A slow reference for [`Table`]: routes and a request path made from
//! fuzz input, matched both ways. Used by `crate::fuzzing` (the fuzz
//! targets) and the randomised tests.
//!
//! The reference tries every route on its own, segment by segment, then
//! picks the most specific of those that match: at the first segment where
//! two differ, plain text beats a parameter and a parameter beats a
//! catch-all. The table must find the same route, with the same parameter
//! spans, or the same `405`/`OPTIONS`/`404` answer: for `405` and `OPTIONS`,
//! every shape that matched, in the same order, and the same `Allow`.

use std::fmt::Write;

use super::{Captures, Lookup, Method, Methods, Pattern, Table};
use crate::percent;

/// Plain-text segments the routes are made of: few, so that routes overlap.
const TEXTS: [&str; 4] = ["a", "b", "ab", "~"];
const METHODS: [&str; 5] = ["GET", "HEAD", "POST", "OPTIONS", "DELETE"];

/// The routes of one shape, by method, as the table keeps them.
type Routes = Vec<(Method, usize)>;

/// A shape that matched: its rank at each segment, its routes, the
/// parameter spans.
type Match<'a> = (Vec<u8>, &'a [(Method, usize)], Vec<(usize, usize)>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seg {
    Text(&'static str),
    Param,
    CatchAll,
}

impl Seg {
    /// The order the table tries them in.
    fn rank(self) -> u8 {
        match self {
            Seg::Text(_) => 0,
            Seg::Param => 1,
            Seg::CatchAll => 2,
        }
    }
}

/// Routes from the start of `data`, a method and a path from the rest;
/// panics if the table and the reference disagree.
pub(crate) fn check(data: &[u8]) {
    let mut bytes = data.iter().copied();
    let count = usize::from(bytes.next().unwrap_or(0) % 12);
    let mut table = Table::default();
    // One entry per shape.
    let mut model: Vec<(Vec<Seg>, Routes)> = Vec::new();
    for route in 0..count {
        let Some(b) = bytes.next() else { break };
        let method = Method::parse(METHODS[usize::from(b % 5)]);
        let len = 1 + usize::from(b / 5 % 4);
        let mut segs = Vec::with_capacity(len);
        let mut text = String::new();
        for i in 0..len {
            let last = i + 1 == len;
            let pick = bytes.next().unwrap_or(0) % if last { 7 } else { 5 };
            let seg = match pick {
                0..=3 => Seg::Text(TEXTS[usize::from(pick)]),
                4 => Seg::Param,
                5 => Seg::CatchAll,
                _ => Seg::Text(""),
            };
            text.push('/');
            match seg {
                Seg::Text(t) => text.push_str(t),
                Seg::Param => write!(text, "{{p{i}}}").unwrap(),
                Seg::CatchAll => text.push_str("{*rest}"),
            }
            segs.push(seg);
        }
        let pattern = Pattern::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(pattern.to_string(), text);

        let existing = model.iter_mut().find(|(s, _)| *s == segs);
        let expected = existing
            .as_ref()
            .and_then(|(_, ms)| ms.iter().find(|(m, _)| *m == method))
            .map(|&(_, r)| r);
        let got = table.insert(&pattern, method.clone(), route);
        assert_eq!(got, expected.map_or(Ok(()), Err), "inserting {text}");
        if got.is_ok() {
            match existing {
                Some((_, ms)) => ms.push((method, route)),
                None => model.push((segs, vec![(method, route)])),
            }
        }
    }

    let rest: Vec<u8> = bytes.collect();
    let (method, path) = match rest.split_first() {
        Some((&m, p)) => (Method::parse(METHODS[usize::from(m % 5)]), path_from(p)),
        None => (Method::Get, "/".to_string()),
    };

    let mut matches: Vec<Match> = model
        .iter()
        .filter_map(|(segs, ms)| {
            let caps = reference_match(segs, &path)?;
            Some((segs.iter().map(|s| s.rank()).collect(), &ms[..], caps))
        })
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    for pair in matches.windows(2) {
        assert_ne!(pair[0].0, pair[1].0, "two shapes match {path} equally");
    }
    let want = matches
        .iter()
        .find_map(|(_, ms, caps)| find(ms, &method).map(|r| (r, caps)));

    let mut caps = Captures::new();
    let got = table.lookup(&method, &path, &mut caps);
    let routes = |ms: &Methods| ms.0.iter().map(|(_, r)| *r).collect::<Vec<_>>();
    match (&got, want, matches.first()) {
        (Lookup::Found(r), Some((want_r, want_caps)), _) => {
            assert_eq!(*r, want_r, "{} {path}", method.as_str());
            let spans: Vec<_> = (0..).map_while(|i| caps.get(i)).collect();
            assert_eq!(&spans, want_caps, "{} {path}", method.as_str());
        }
        (Lookup::Options(allowed) | Lookup::NotAllowed(allowed), None, Some((_, first, _))) => {
            assert_eq!(
                matches!(got, Lookup::Options(_)),
                method == Method::Options,
                "{path}"
            );
            // Every shape that matched, most specific first: the `Allow`
            // header names the methods of all of them.
            let got_paths: Vec<Vec<usize>> = allowed.paths().map(routes).collect();
            let want_paths: Vec<Vec<usize>> = matches
                .iter()
                .map(|(_, ms, _)| ms.iter().map(|(_, r)| *r).collect())
                .collect();
            assert_eq!(got_paths, want_paths, "{} {path}", method.as_str());
            assert_eq!(allowed.first_route(), first[0].1, "{path}");
            assert_eq!(
                allowed.allow(),
                reference_allow(&matches),
                "{} {path}",
                method.as_str()
            );
        }
        (Lookup::NotFound, None, None) => {}
        (got, want, first) => panic!(
            "{} {path}: the table found {got:?}, the reference {want:?} (first match {:?}); routes {model:?}",
            method.as_str(),
            first.map(|f| f.1)
        ),
    }
}

/// A request path from fuzz bytes, in pieces that the routes' texts and
/// percent-encoding can make interesting.
fn path_from(bytes: &[u8]) -> String {
    let mut path = String::new();
    if bytes.first() != Some(&0xff) {
        path.push('/');
    }
    for &b in bytes {
        path.push_str(match b % 13 {
            0..=2 => "/",
            3 => "a",
            4 => "b",
            5 => "%61",
            6 => "%62",
            7 => "%",
            8 => "%ff",
            9 => "~",
            10 => "%7E",
            11 => "%2F",
            _ => "c",
        });
    }
    path
}

/// The `Allow` header for a request no route took: every method of every
/// shape that matched, most specific shape first, since any of them would
/// route there; `HEAD` when `GET` is there, and `OPTIONS`.
fn reference_allow(matches: &[Match]) -> String {
    let mut methods: Vec<&str> = Vec::new();
    for (_, routes, _) in matches {
        for (m, _) in routes.iter() {
            if !methods.contains(&m.as_str()) {
                methods.push(m.as_str());
            }
        }
    }
    if methods.contains(&"GET") && !methods.contains(&"HEAD") {
        methods.push("HEAD");
    }
    if !methods.contains(&"OPTIONS") {
        methods.push("OPTIONS");
    }
    methods.join(", ")
}

/// The route for `method` among one shape's routes: `HEAD` falls back to
/// `GET`.
fn find(routes: &[(Method, usize)], method: &Method) -> Option<usize> {
    let exact = routes.iter().find(|(m, _)| m == method).map(|&(_, r)| r);
    match (exact, method) {
        (None, Method::Head) => find(routes, &Method::Get),
        _ => exact,
    }
}

/// Whether `segs` matches `path` on its own, and the parameter spans.
fn reference_match(segs: &[Seg], path: &str) -> Option<Vec<(usize, usize)>> {
    if !path.starts_with('/') {
        return None;
    }
    let mut pos = 0;
    let mut caps = Vec::new();
    for seg in segs {
        if pos == path.len() {
            return None;
        }
        let start = pos + 1;
        if *seg == Seg::CatchAll {
            // The rest of the path, at least one character.
            if start == path.len() {
                return None;
            }
            caps.push((start, path.len()));
            return Some(caps);
        }
        let end = path[start..].find('/').map_or(path.len(), |i| start + i);
        let text = &path[start..end];
        match seg {
            Seg::Text(want) => {
                if !percent::decode(text).is_ok_and(|t| t == *want) {
                    return None;
                }
            }
            Seg::Param => {
                if text.is_empty() {
                    return None;
                }
                caps.push((start, end));
            }
            Seg::CatchAll => unreachable!(),
        }
        pos = end;
    }
    (pos == path.len()).then_some(caps)
}
