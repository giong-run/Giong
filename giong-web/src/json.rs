//! JSON: a small writer for the bodies and log lines giong-web makes itself,
//! and, with the `serde` feature, [`json`] for handlers.

use std::fmt::Write as _;

#[cfg(feature = "serde")]
use crate::{Error, Response};

/// Builds one JSON object, key by key. Keys are written as given: they are
/// always literals in this crate.
pub(crate) struct JsonWriter {
    out: String,
    first: bool,
}

impl JsonWriter {
    pub(crate) fn object() -> Self {
        Self::with_capacity(128)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut out = String::with_capacity(capacity);
        out.push('{');
        Self { out, first: true }
    }

    fn key(&mut self, key: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        self.out.push('"');
        self.out.push_str(key);
        self.out.push_str("\":");
    }

    pub(crate) fn str(&mut self, key: &str, value: &str) {
        self.key(key);
        escape_into(&mut self.out, value);
    }

    pub(crate) fn u64(&mut self, key: &str, value: u64) {
        self.key(key);
        let _ = write!(self.out, "{value}");
    }

    pub(crate) fn null(&mut self, key: &str) {
        self.key(key);
        self.out.push_str("null");
    }

    /// A value that is JSON already, such as an object from another writer.
    pub(crate) fn raw(&mut self, key: &str, json: &str) {
        self.key(key);
        self.out.push_str(json);
    }

    pub(crate) fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }

    /// The object followed by a newline: one line of a JSON-lines log.
    pub(crate) fn finish_line(mut self) -> String {
        self.out.push_str("}\n");
        self.out
    }
}

/// Appends `s` as a JSON string, quotes included.
pub(crate) fn escape_into(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0..=0x1f | 0x7f => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{:04x}", b);
        } else {
            out.push_str(escape);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// A response with `value` as its JSON body and `Content-Type:
/// application/json`.
///
/// The status is always given: there is no implicit `200`.
///
/// ```
/// use giong_web::{json, Error, Req, Response};
///
/// #[derive(serde::Serialize)]
/// struct User {
///     id: u64,
///     name: String,
/// }
///
/// async fn show(req: Req) -> Result<Response, Error> {
///     let id = req.param("id")?;
///     json(200, &User { id, name: "Ada".into() })
/// }
/// ```
///
/// A value that cannot be serialised (a map with keys that are not strings,
/// say) is an [`Error::internal`].
///
/// # `json` and `Response::json`
///
/// This function serializes a value. [`Response::json`], which comes from
/// giong-http, does not: it takes a body that is JSON already (bytes or a
/// string, say from a cache or another service) and only sets the
/// `Content-Type`, with status `200`. Use this one for values, so that
/// strings are escaped by serde_json rather than by hand, and
/// `Response::json` only to pass on JSON made elsewhere.
///
/// # Panics
///
/// Panics if `status` is not in `200..=599`, as [`Response::new`] does. The
/// status is meant to be a literal; for an error status that comes from
/// elsewhere, see [`Error::from_status`].
#[cfg(feature = "serde")]
pub fn json<T: serde::Serialize + ?Sized>(status: u16, value: &T) -> Result<Response, Error> {
    let body = serde_json::to_vec(value).map_err(Error::internal)?;
    Ok(Response::new(status)
        .header("Content-Type", "application/json")
        .body(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped() {
        let mut out = String::new();
        escape_into(&mut out, "a\"b\\c\nd\u{1}e\u{7f}é");
        assert_eq!(out, r#""a\"b\\c\nd\u0001e\u007fé""#);
    }

    #[test]
    fn objects_are_built_key_by_key() {
        let mut w = JsonWriter::object();
        w.str("a", "x");
        w.u64("n", 7);
        w.null("z");
        w.raw("o", "{}");
        assert_eq!(
            w.finish_line(),
            "{\"a\":\"x\",\"n\":7,\"z\":null,\"o\":{}}\n"
        );
    }
}
