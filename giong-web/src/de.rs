//! With the `serde` feature: request bodies as JSON and query strings as
//! structs.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;

use giong::error::Code;
use serde::de::value::CowStrDeserializer;
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
use serde::Deserialize;

use crate::percent;
use crate::{Error, Req};

impl<S> Req<S> {
    /// The body, parsed as JSON into `T`.
    ///
    /// ```
    /// use std::borrow::Cow;
    /// use giong_web::{json, Error, Req, Response};
    ///
    /// #[derive(serde::Deserialize)]
    /// struct NewUser<'a> {
    ///     #[serde(borrow)]
    ///     name: Cow<'a, str>,
    ///     email: String,
    /// }
    ///
    /// async fn create(req: Req) -> Result<Response, Error> {
    ///     let user: NewUser = req.json()?;
    ///     json(201, &serde_json::json!({ "name": user.name, "email": user.email }))
    /// }
    /// ```
    ///
    /// # Strings: `String` or `Cow<str>`, not `&str`
    ///
    /// A field that is a `String` owns its text. A `Cow<'a, str>` field
    /// marked `#[serde(borrow)]` borrows it from the body when the JSON
    /// string has no escape and owns it when it has one (`\"`, `\n`,
    /// `\u00e9`), which saves a copy for most strings. A `&str` field can
    /// only borrow, so it fails on any escaped string: that is the
    /// program's mistake, not the client's, and is answered `500` with a
    /// logged cause that says to use one of the two, never a `4xx` that
    /// would blame valid JSON.
    ///
    /// Numbers are read by serde_json with its default settings, which may
    /// put an `f64` one unit in the last place off the nearest value. An
    /// application that needs every float exact turns on serde_json's
    /// `float_roundtrip` feature in its own `Cargo.toml` (it then applies
    /// here too), at some cost in parsing speed; amounts of money are best
    /// sent as integers or strings anyway.
    ///
    /// # Errors
    ///
    /// * `415` (`GIONG-E1208`) unless the `Content-Type` is
    ///   `application/json` or another JSON type (`application/*+json`).
    /// * `400` (`GIONG-E1207`) when the body is not valid JSON, and `422`
    ///   (`GIONG-E1207`) when it is JSON of the wrong shape: a missing field,
    ///   a string where a number belongs. The detail quotes serde_json,
    ///   with the line and column.
    /// * `500` (`GIONG-E1210`) when `T` has a `&str` field and the string
    ///   has an escape (see above), or the route reads its body as a
    ///   stream; the logged cause says what to change.
    pub fn json<'a, T: Deserialize<'a>>(&'a self) -> Result<T, Error> {
        let content_type = self.header_str("content-type");
        if !content_type.is_some_and(is_json) {
            let got = match content_type {
                Some(t) => format!("got `{}`", cut(t)),
                None => "the request has no Content-Type".into(),
            };
            return Err(Error::coded(
                415,
                Code::WebUnsupportedMediaType,
                format!("expected a JSON body with Content-Type: application/json; {got}"),
            ));
        }
        if self.body_stream().is_some() {
            return Err(Error::internal(giong::error::Error::new(
                Code::WebConfig,
                "Req::json on a route that reads its body as a stream \
                 (Router::stream_bodies): the body has not been received",
                "read the body with Req::body_stream (BodyStream::read_to_end, then \
                 serde_json::from_slice), or move the route out of the streaming router.",
            )));
        }
        serde_json::from_slice(self.body()).map_err(|e| {
            let shown = e.to_string();
            if cannot_borrow(&shown) {
                return borrow_bug("Req::json", "a JSON string with an escape", &shown);
            }
            let status = match e.classify() {
                serde_json::error::Category::Data => 422,
                _ => 400,
            };
            Error::coded(status, Code::WebBadBody, format!("request body: {shown}"))
        })
    }

    /// The query string, parsed into `T`: a struct whose fields are the
    /// parameters.
    ///
    /// ```
    /// use giong_web::{Error, Req, Response};
    ///
    /// #[derive(serde::Deserialize)]
    /// struct Page {
    ///     page: Option<u32>,
    ///     per_page: Option<u32>,
    ///     sort: Option<String>,
    /// }
    ///
    /// // GET /users?page=2&sort=name
    /// async fn list(req: Req) -> Result<Response, Error> {
    ///     let p: Page = req.query()?;
    ///     let page = p.page.unwrap_or(1);
    ///     # let _ = (page, p.per_page, p.sort);
    ///     # Ok(Response::new(204))
    /// }
    /// ```
    ///
    /// Values are percent-decoded (`+` is a space) and read with the
    /// field's type: numbers, `bool` (`true`/`false`), `char`, strings,
    /// enums of unit variants. An `Option` field is `None` when the key is
    /// absent or its value empty. Unknown keys are ignored unless `T` says
    /// `#[serde(deny_unknown_fields)]`. String fields are `String`, or
    /// `Cow<'a, str>` with `#[serde(borrow)]`, as for [`json`](Self::json):
    /// a `&str` field cannot hold a value with `+` or `%XX`, and fails with
    /// a `500` whose logged cause says so.
    ///
    /// This and [`query_param`](Self::query_param) follow one strict rule:
    /// the whole query string must be valid percent-encoded UTF-8, and a
    /// key that is read (a field of `T`, every key of a map) must appear
    /// once. Sequences are refused too: one value per key.
    ///
    /// # Errors
    ///
    /// `400` (`GIONG-E1205`) when a value does not parse, a required field
    /// is missing, a key appears twice, or the query string is not valid
    /// percent-encoding. The detail names the parameter.
    pub fn query<'a, T: Deserialize<'a>>(&'a self) -> Result<T, Error> {
        from_query(self.query_string().unwrap_or("")).map_err(|e| {
            if cannot_borrow(&e.0) {
                return borrow_bug("Req::query", "a value with `+` or `%XX`", &e.0);
            }
            Error::coded(400, Code::WebBadParameter, format!("query string: {}", e.0))
        })
    }
}

/// serde's words when a `&str` (or `&[u8]`) field is given a string it
/// cannot lend, because it had to be unescaped.
fn cannot_borrow(message: &str) -> bool {
    message.contains("expected a borrowed string") || message.contains("expected a borrowed byte")
}

/// The `500` for a `&str` field given a value it cannot borrow: a mistake
/// in the program, which valid input from the client brings out.
fn borrow_bug(what: &str, value: &str, serde_says: &str) -> Error {
    Error::internal(giong::error::Error::new(
        Code::WebConfig,
        format!(
            "{what}: a `&str` field cannot hold {value}, which has to be decoded into a new \
             string ({serde_says})"
        ),
        "make the field a String, or a Cow<'a, str> marked #[serde(borrow)], which borrows \
         when it can and owns the string otherwise.",
    ))
}

fn is_json(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    let Some((kind, sub)) = mime.split_once('/') else {
        return false;
    };
    kind.eq_ignore_ascii_case("application")
        && (sub.eq_ignore_ascii_case("json") || sub.to_ascii_lowercase().ends_with("+json"))
}

fn cut(s: &str) -> &str {
    let mut end = s.len().min(100);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Why a query string does not fit the type.
#[derive(Debug)]
pub(crate) struct DeError(String);

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeError(msg.to_string())
    }
}

pub(crate) fn from_query<'de, T: Deserialize<'de>>(query: &'de str) -> Result<T, DeError> {
    T::deserialize(Query {
        pairs: Box::new(percent::query_pairs(query)),
        key: Cow::Borrowed(""),
        value: None,
        seen: None,
    })
}

/// The whole query string: a map from keys to values.
struct Query<'de> {
    pairs: Box<dyn Iterator<Item = (&'de str, &'de str)> + 'de>,
    key: Cow<'de, str>,
    value: Option<Cow<'de, str>>,
    /// The keys so far, when every key is read (a map): a key given twice
    /// is refused, as serde refuses a struct's field given twice. A struct
    /// ignores the keys it does not know, repeated or not.
    seen: Option<HashSet<Cow<'de, str>>>,
}

fn decoded(s: &str) -> Result<Cow<'_, str>, DeError> {
    percent::decode_query(s).map_err(|e| DeError(format!("{:?}: {}", cut(s), e.describe())))
}

impl<'de> de::Deserializer<'de> for Query<'de> {
    type Error = DeError;

    /// A map, `serde_json::Value` and the like: every key is read.
    fn deserialize_any<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value, DeError> {
        self.seen = Some(HashSet::new());
        visitor.visit_map(self)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_map(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier ignored_any
    }
}

impl<'de> MapAccess<'de> for Query<'de> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some((key, value)) = self.pairs.next() else {
            return Ok(None);
        };
        self.key = decoded(key)?;
        self.value = Some(decoded(value)?);
        if let Some(seen) = &mut self.seen {
            if !seen.insert(self.key.clone()) {
                return Err(DeError(format!(
                    "parameter `{}` appears more than once; send one value per key",
                    cut(&self.key)
                )));
            }
        }
        seed.deserialize(Part(self.key.clone())).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let value = self.value.take().unwrap_or(Cow::Borrowed(""));
        seed.deserialize(Part(value))
            .map_err(|e| DeError(format!("parameter `{}`: {}", cut(&self.key), e.0)))
    }
}

/// One key or value.
struct Part<'de>(Cow<'de, str>);

impl Part<'_> {
    fn parse<T>(&self, expected: &str) -> Result<T, DeError>
    where
        T: std::str::FromStr,
        T::Err: fmt::Display,
    {
        self.0.parse().map_err(|e| {
            DeError(format!(
                "cannot read {:?} as {expected} ({e})",
                cut(&self.0)
            ))
        })
    }
}

macro_rules! parse_as {
    ($($method:ident => $visit:ident, $ty:ty, $expected:literal;)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
                visitor.$visit(self.parse::<$ty>($expected)?)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for Part<'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Cow::Borrowed(s) => visitor.visit_borrowed_str(s),
            Cow::Owned(s) => visitor.visit_string(s),
        }
    }

    parse_as! {
        deserialize_bool => visit_bool, bool, "`true` or `false`";
        deserialize_i8 => visit_i8, i8, "an integer";
        deserialize_i16 => visit_i16, i16, "an integer";
        deserialize_i32 => visit_i32, i32, "an integer";
        deserialize_i64 => visit_i64, i64, "an integer";
        deserialize_i128 => visit_i128, i128, "an integer";
        deserialize_u8 => visit_u8, u8, "an unsigned integer";
        deserialize_u16 => visit_u16, u16, "an unsigned integer";
        deserialize_u32 => visit_u32, u32, "an unsigned integer";
        deserialize_u64 => visit_u64, u64, "an unsigned integer";
        deserialize_u128 => visit_u128, u128, "an unsigned integer";
        deserialize_f32 => visit_f32, f32, "a number";
        deserialize_f64 => visit_f64, f64, "a number";
        deserialize_char => visit_char, char, "one character";
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.0.is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let de: CowStrDeserializer<'de, DeError> = self.0.into_deserializer();
        visitor.visit_enum(de)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DeError> {
        Err(DeError(
            "lists are not supported in a query string: one value per key".into(),
        ))
    }

    serde::forward_to_deserialize_any! {
        str string bytes byte_buf unit_struct tuple tuple_struct map struct
        identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum Sort {
        Name,
        Age,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filter<'a> {
        page: u32,
        per_page: Option<u32>,
        q: Option<String>,
        #[serde(borrow)]
        tag: Option<Cow<'a, str>>,
        sort: Option<Sort>,
        active: Option<bool>,
        min: Option<f64>,
    }

    #[test]
    fn fields_are_read_with_their_types() {
        let f: Filter = from_query(
            "page=2&q=caf%C3%A9+au+lait&tag=rust&sort=age&active=true&min=1.5&per_page=",
        )
        .unwrap();
        assert_eq!(
            f,
            Filter {
                page: 2,
                per_page: None,
                q: Some("café au lait".into()),
                tag: Some("rust".into()),
                sort: Some(Sort::Age),
                active: Some(true),
                min: Some(1.5),
            }
        );
        // An encoded value is decoded into a new string, which a `&str`
        // field could not hold.
        let f: Filter = from_query("page=1&tag=rust+lang").unwrap();
        assert_eq!(f.tag.as_deref(), Some("rust lang"));
    }

    #[test]
    fn mistakes_name_the_parameter() {
        let err = |q| from_query::<Filter>(q).unwrap_err().0;
        assert_eq!(err(""), "missing field `page`");
        assert!(
            err("page=x").starts_with("parameter `page`: cannot read \"x\" as an unsigned integer")
        );
        assert!(err("page=1&sort=size").contains("unknown variant `size`"));
        assert!(err("page=1&page=2").contains("duplicate field `page`"));
        assert!(err("page=1&active=yes").contains("`true` or `false`"));
        assert!(err("page=%zz").contains("not followed by two hexadecimal digits"));
    }

    #[test]
    fn lists_are_refused() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Tags {
            tag: Vec<String>,
        }
        let err = from_query::<Tags>("tag=a&tag=b").unwrap_err().0;
        assert!(err.contains("one value per key"), "{err}");
    }

    #[test]
    fn json_content_types() {
        assert!(is_json("application/json"));
        assert!(is_json("Application/JSON; charset=utf-8"));
        assert!(is_json("application/merge-patch+json"));
        assert!(!is_json("text/json"));
        assert!(!is_json("application/x-www-form-urlencoded"));
        assert!(!is_json("json"));
    }
}
