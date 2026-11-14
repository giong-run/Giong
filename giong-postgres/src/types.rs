//! Converting between Rust values and PostgreSQL values.
//!
//! | Rust | PostgreSQL |
//! |---|---|
//! | `bool` | `boolean` |
//! | `i16`, `i32`, `i64` | `smallint`, `integer`, `bigint` (any of them, checked for range); `oid` |
//! | `f32`, `f64` | `real`, `double precision` |
//! | `&str`, `String` | `text`, `varchar`, `char(n)`, `name`; as parameters, any type the server can parse from text (`uuid`, `numeric`, `date`, `json`...); as results, any column this table does not list |
//! | `&[u8]`, `Vec<u8>` | `bytea` |
//! | `SystemTime` | `timestamptz` |
//! | `Option<T>` | `NULL` or a value |
//!
//! Integers convert between the three integer types when the value fits, and
//! fail with an error naming the parameter or column when it does not, so a
//! conversion never loses anything silently.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;
pub use crate::protocol::Encoded;

/// A PostgreSQL type, identified by its OID.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Type(u32);

macro_rules! types {
    ($($(#[$doc:meta])* $name:ident = $oid:expr, $sql:expr;)*) => {
        impl Type {
            $($(#[$doc])* pub const $name: Type = Type($oid);)*

            /// The type's name in SQL, for the built-in types this crate knows.
            pub fn name(self) -> Option<&'static str> {
                match self.0 {
                    $($oid => Some($sql),)*
                    _ => None,
                }
            }
        }
    };
}

types! {
    /// `boolean`
    BOOL = 16, "boolean";
    /// `bytea`
    BYTEA = 17, "bytea";
    /// `"char"`, the one-byte internal type
    CHAR = 18, "\"char\"";
    /// `name`
    NAME = 19, "name";
    /// `bigint`
    INT8 = 20, "bigint";
    /// `smallint`
    INT2 = 21, "smallint";
    /// `integer`
    INT4 = 23, "integer";
    /// `text`
    TEXT = 25, "text";
    /// `oid`
    OID = 26, "oid";
    /// `json`
    JSON = 114, "json";
    /// `real`
    FLOAT4 = 700, "real";
    /// `double precision`
    FLOAT8 = 701, "double precision";
    /// `unknown`, the type of an untyped literal
    UNKNOWN = 705, "unknown";
    /// `character(n)`
    BPCHAR = 1042, "character";
    /// `character varying`
    VARCHAR = 1043, "character varying";
    /// `date`
    DATE = 1082, "date";
    /// `time`
    TIME = 1083, "time";
    /// `timestamp` (without time zone)
    TIMESTAMP = 1114, "timestamp";
    /// `timestamptz`
    TIMESTAMPTZ = 1184, "timestamptz";
    /// `interval`
    INTERVAL = 1186, "interval";
    /// `numeric`
    NUMERIC = 1700, "numeric";
    /// `uuid`
    UUID = 2950, "uuid";
    /// `jsonb`
    JSONB = 3802, "jsonb";
}

impl Type {
    /// The type with this OID.
    pub fn from_oid(oid: u32) -> Self {
        Type(oid)
    }

    /// The OID.
    pub fn oid(self) -> u32 {
        self.0
    }

    /// Whether the text and binary forms of this type are the same UTF-8
    /// bytes.
    fn is_text(self) -> bool {
        matches!(
            self,
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN
        )
    }

    /// Whether results of this type are asked for in binary. The rest come as
    /// text, so that any column can be read as a string.
    pub(crate) fn binary_result(self) -> bool {
        matches!(
            self,
            Type::BOOL
                | Type::BYTEA
                | Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::OID
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::TIMESTAMPTZ
        )
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "type with OID {}", self.0),
        }
    }
}

impl std::fmt::Debug for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Type({self})")
    }
}

/// The format a value arrived in.
///
/// These are the only two formats of PostgreSQL's protocol, so the enum is
/// complete: a `match` on it needs no wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Text, as `psql` shows it.
    Text,
    /// PostgreSQL's binary format for the type.
    Binary,
}

// ----------------------------------------------------------------- ToSql

/// A Rust value that can be a statement parameter.
///
/// The server says which type each parameter has (from the SQL, as if the
/// parameter were an untyped literal); `to_sql` appends the value in that
/// type's binary format or as text, and says which. Implement it for your own
/// types by delegating to one of the implementations here.
pub trait ToSql {
    /// Appends the value for a parameter of type `ty` to `out`.
    ///
    /// Return an error made with [`Error::conversion`] when the value cannot
    /// be a `ty`; it is reported with the parameter's number.
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error>;
}

fn wrong_param(rust: &str, ty: Type, instead: &str) -> Error {
    Error::conversion(format!("a {rust} cannot be a {ty}{instead}"))
}

/// What to pass instead for a parameter of type `ty`.
fn suggestion(ty: Type) -> &'static str {
    match ty {
        Type::BOOL => "; pass a bool",
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => "; pass an i16, i32 or i64",
        Type::FLOAT4 => "; pass an f32",
        Type::FLOAT8 => "; pass an f64 or f32",
        Type::BYTEA => "; pass a &[u8] or Vec<u8>",
        Type::TIMESTAMPTZ => "; pass a SystemTime or a string such as \"2026-01-31 12:00:00+00\"",
        t if t.is_text() => "; pass a &str or String",
        _ => "; pass the value as a &str in the type's text form, or cast the parameter in SQL",
    }
}

fn put_int(v: i64, rust: &str, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
    let range = |name: &str| {
        Error::conversion(format!(
            "{v} does not fit in a {name}; change the column type or pass a smaller value"
        ))
    };
    match ty {
        Type::INT2 => {
            let v = i16::try_from(v).map_err(|_| range("smallint"))?;
            out.extend_from_slice(&v.to_be_bytes());
        }
        Type::INT4 => {
            let v = i32::try_from(v).map_err(|_| range("integer"))?;
            out.extend_from_slice(&v.to_be_bytes());
        }
        Type::INT8 => out.extend_from_slice(&v.to_be_bytes()),
        Type::OID => {
            let v = u32::try_from(v).map_err(|_| range("oid"))?;
            out.extend_from_slice(&v.to_be_bytes());
        }
        // Exact in text; the server rounds for the float types as it would a
        // literal.
        Type::NUMERIC | Type::FLOAT4 | Type::FLOAT8 => {
            out.extend_from_slice(v.to_string().as_bytes());
            return Ok(Encoded::Text);
        }
        _ => return Err(wrong_param(rust, ty, suggestion(ty))),
    }
    Ok(Encoded::Binary)
}

macro_rules! int_to_sql {
    ($($t:ty),*) => {$(
        impl ToSql for $t {
            fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
                put_int(i64::from(*self), stringify!($t), ty, out)
            }
        }
    )*};
}

int_to_sql!(i16, i32, i64);

impl ToSql for f64 {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        match ty {
            Type::FLOAT8 => out.extend_from_slice(&self.to_bits().to_be_bytes()),
            Type::FLOAT4 => {
                let narrow = *self as f32;
                if f64::from(narrow) != *self && !self.is_nan() {
                    return Err(Error::conversion(format!(
                        "{self} cannot be stored in a real without losing precision; \
                         pass an f32, or make the column double precision"
                    )));
                }
                out.extend_from_slice(&narrow.to_bits().to_be_bytes());
            }
            Type::NUMERIC => return put_float_text(*self, out),
            _ => return Err(wrong_param("f64", ty, suggestion(ty))),
        }
        Ok(Encoded::Binary)
    }
}

impl ToSql for f32 {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        match ty {
            Type::FLOAT4 => out.extend_from_slice(&self.to_bits().to_be_bytes()),
            Type::FLOAT8 => out.extend_from_slice(&f64::from(*self).to_bits().to_be_bytes()),
            Type::NUMERIC => return put_float_text(f64::from(*self), out),
            _ => return Err(wrong_param("f32", ty, suggestion(ty))),
        }
        Ok(Encoded::Binary)
    }
}

/// A float as `numeric` text; Rust prints the shortest form that reads back
/// as the same float.
fn put_float_text(v: f64, out: &mut Vec<u8>) -> Result<Encoded, Error> {
    let text = if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        v.to_string()
    };
    out.extend_from_slice(text.as_bytes());
    Ok(Encoded::Text)
}

impl ToSql for bool {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        if ty != Type::BOOL {
            return Err(wrong_param("bool", ty, suggestion(ty)));
        }
        out.push(*self as u8);
        Ok(Encoded::Binary)
    }
}

impl ToSql for str {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        if ty == Type::BYTEA {
            return Err(wrong_param("string", ty, "; pass a &[u8] or Vec<u8>"));
        }
        crate::protocol::check_text("a string parameter", self)?;
        out.extend_from_slice(self.as_bytes());
        // For text types both forms are the same bytes; for the others the
        // server parses the text as it would a literal.
        Ok(if ty.is_text() {
            Encoded::Binary
        } else {
            Encoded::Text
        })
    }
}

impl ToSql for String {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_str().to_sql(ty, out)
    }
}

impl ToSql for [u8] {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        if ty != Type::BYTEA {
            return Err(wrong_param("byte slice", ty, suggestion(ty)));
        }
        out.extend_from_slice(self);
        Ok(Encoded::Binary)
    }
}

impl ToSql for Vec<u8> {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_slice().to_sql(ty, out)
    }
}

impl<const N: usize> ToSql for [u8; N] {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_slice().to_sql(ty, out)
    }
}

/// Microseconds between the Unix epoch and PostgreSQL's, 2000-01-01 UTC.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

impl ToSql for SystemTime {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        if ty != Type::TIMESTAMPTZ {
            let instead = if ty == Type::TIMESTAMP {
                "; a timestamp without time zone has no fixed moment: make the column \
                 timestamptz, or pass the wall-clock time as a string"
            } else {
                suggestion(ty)
            };
            return Err(wrong_param("SystemTime", ty, instead));
        }
        let unix_micros = match self.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_micros()).ok(),
            Err(before) => i64::try_from(before.duration().as_micros())
                .ok()
                .map(|m| -m),
        };
        let micros = unix_micros
            .and_then(|m| m.checked_sub(PG_EPOCH_MICROS))
            .ok_or_else(|| Error::conversion(format!("{self:?} is out of timestamptz's range")))?;
        out.extend_from_slice(&micros.to_be_bytes());
        Ok(Encoded::Binary)
    }
}

impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        match self {
            Some(v) => v.to_sql(ty, out),
            None => Ok(Encoded::Null),
        }
    }
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        (**self).to_sql(ty, out)
    }
}

impl<T: ToSql + ?Sized> ToSql for Box<T> {
    fn to_sql(&self, ty: Type, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        (**self).to_sql(ty, out)
    }
}

// --------------------------------------------------------------- FromSql

/// A Rust value that a column can be read as, with
/// [`Row::get`](crate::Row::get).
///
/// `'a` is the lifetime of the row, so that `&str` and `&[u8]` can borrow
/// from it without copying.
///
/// **Which format a column arrives in** depends on its type alone, not on
/// the Rust type it is read as: `boolean`, `bytea`, `smallint`, `integer`,
/// `bigint`, `oid`, `real`, `double precision` and `timestamptz` come in
/// binary, and every other type (`uuid`, `numeric`, `date`, `jsonb`, enums,
/// arrays...) as text, as `psql` would show it. An implementation for one of
/// those types reads the text, and should still check `format` and refuse
/// the format it does not handle with [`Error::conversion`], rather than
/// assume it, so that a value in another format is an error and never a
/// wrong value.
///
/// ```
/// use giong_postgres::types::Format;
/// use giong_postgres::{Error, FromSql, Type};
///
/// /// A `uuid` column, kept as its text.
/// struct Uuid(String);
///
/// impl<'a> FromSql<'a> for Uuid {
///     fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
///         if ty != Type::UUID || format != Format::Text {
///             return Err(Error::conversion(format!("expected uuid as text, got {ty} as {format:?}")));
///         }
///         let text = std::str::from_utf8(raw).map_err(|_| Error::conversion("uuid is not UTF-8"))?;
///         Ok(Uuid(text.to_owned()))
///     }
/// }
/// ```
pub trait FromSql<'a>: Sized {
    /// Converts a value that is not `NULL`, of type `ty`, in `format`.
    ///
    /// Return an error made with [`Error::conversion`] saying why the value
    /// cannot be converted; it is reported with the column's name.
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error>;

    /// Converts `NULL`. The default refuses it.
    fn from_null(ty: Type) -> Result<Self, Error> {
        let _ = ty;
        Err(Error::conversion(format!(
            "the value is NULL; read it as Option<{}>",
            std::any::type_name::<Self>()
        )))
    }
}

/// What to read a column of type `ty` as.
fn read_as(ty: Type) -> &'static str {
    match ty {
        Type::BOOL => "; read it as bool",
        Type::INT2 | Type::INT4 | Type::INT8 => "; read it as i16, i32 or i64",
        Type::OID => "; read it as i64",
        Type::FLOAT4 => "; read it as f32 or f64",
        Type::FLOAT8 => "; read it as f64",
        Type::BYTEA => "; read it as Vec<u8> or &[u8]",
        Type::TIMESTAMPTZ => "; read it as SystemTime, or cast it to text in SQL",
        _ => "; read it as String or &str",
    }
}

fn wrong_column<T>(ty: Type) -> Error {
    Error::conversion(format!(
        "a {ty} cannot be read as {}{}",
        std::any::type_name::<T>(),
        read_as(ty)
    ))
}

fn fixed<const N: usize>(raw: &[u8], ty: Type) -> Result<[u8; N], Error> {
    raw.try_into().map_err(|_| {
        Error::conversion(format!(
            "the server sent {} bytes for a {ty}, which has {N}",
            raw.len()
        ))
    })
}

fn get_int(ty: Type, format: Format, raw: &[u8]) -> Option<Result<i64, Error>> {
    if format != Format::Binary {
        return None;
    }
    Some(match ty {
        Type::INT2 => fixed::<2>(raw, ty).map(|b| i64::from(i16::from_be_bytes(b))),
        Type::INT4 => fixed::<4>(raw, ty).map(|b| i64::from(i32::from_be_bytes(b))),
        Type::INT8 => fixed::<8>(raw, ty).map(i64::from_be_bytes),
        Type::OID => fixed::<4>(raw, ty).map(|b| i64::from(u32::from_be_bytes(b))),
        _ => return None,
    })
}

macro_rules! int_from_sql {
    ($($t:ty),*) => {$(
        impl<'a> FromSql<'a> for $t {
            fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
                let v = get_int(ty, format, raw).ok_or_else(|| wrong_column::<$t>(ty))??;
                <$t>::try_from(v).map_err(|_| {
                    Error::conversion(format!(
                        "{v} does not fit in an {}; read it as i64", stringify!($t)
                    ))
                })
            }
        }
    )*};
}

int_from_sql!(i16, i32, i64);

impl<'a> FromSql<'a> for f64 {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        match (ty, format) {
            (Type::FLOAT8, Format::Binary) => {
                Ok(f64::from_bits(u64::from_be_bytes(fixed(raw, ty)?)))
            }
            (Type::FLOAT4, Format::Binary) => Ok(f64::from(f32::from_bits(u32::from_be_bytes(
                fixed(raw, ty)?,
            )))),
            _ => Err(wrong_column::<f64>(ty)),
        }
    }
}

impl<'a> FromSql<'a> for f32 {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        match (ty, format) {
            (Type::FLOAT4, Format::Binary) => {
                Ok(f32::from_bits(u32::from_be_bytes(fixed(raw, ty)?)))
            }
            (Type::FLOAT8, _) => Err(Error::conversion(
                "a double precision cannot be read as f32 without losing precision; read it as f64",
            )),
            _ => Err(wrong_column::<f32>(ty)),
        }
    }
}

impl<'a> FromSql<'a> for bool {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        if (ty, format) != (Type::BOOL, Format::Binary) {
            return Err(wrong_column::<bool>(ty));
        }
        Ok(fixed::<1>(raw, ty)?[0] != 0)
    }
}

impl<'a> FromSql<'a> for &'a str {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        if format != Format::Text && !ty.is_text() {
            return Err(wrong_column::<&str>(ty));
        }
        std::str::from_utf8(raw)
            .map_err(|_| Error::conversion(format!("the {ty} value is not valid UTF-8")))
    }
}

impl<'a> FromSql<'a> for String {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        if format != Format::Text && !ty.is_text() {
            return Err(wrong_column::<String>(ty));
        }
        <&str>::from_sql(ty, format, raw).map(str::to_owned)
    }
}

impl<'a> FromSql<'a> for &'a [u8] {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        if (ty, format) != (Type::BYTEA, Format::Binary) {
            return Err(wrong_column::<&[u8]>(ty));
        }
        Ok(raw)
    }
}

impl<'a> FromSql<'a> for Vec<u8> {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        <&[u8]>::from_sql(ty, format, raw).map(<[u8]>::to_vec)
    }
}

impl<'a> FromSql<'a> for SystemTime {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        if (ty, format) != (Type::TIMESTAMPTZ, Format::Binary) {
            return Err(wrong_column::<SystemTime>(ty));
        }
        let micros = i64::from_be_bytes(fixed(raw, ty)?);
        if micros == i64::MAX || micros == i64::MIN {
            return Err(Error::conversion(
                "the timestamptz is infinity or -infinity, which SystemTime cannot hold; \
                 read it as Option and use NULL instead, or filter it out in SQL",
            ));
        }
        let unix = micros
            .checked_add(PG_EPOCH_MICROS)
            .ok_or_else(|| Error::conversion("the timestamptz is out of SystemTime's range"))?;
        let t = if unix >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_micros(unix as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_micros(unix.unsigned_abs()))
        };
        t.ok_or_else(|| Error::conversion("the timestamptz is out of SystemTime's range"))
    }
}

impl<'a, T: FromSql<'a>> FromSql<'a> for Option<T> {
    fn from_sql(ty: Type, format: Format, raw: &'a [u8]) -> Result<Self, Error> {
        T::from_sql(ty, format, raw).map(Some)
    }

    fn from_null(_: Type) -> Result<Self, Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: &dyn ToSql, ty: Type) -> Result<(Encoded, Vec<u8>), Error> {
        let mut out = Vec::new();
        v.to_sql(ty, &mut out).map(|e| (e, out))
    }

    #[test]
    fn integers_convert_when_they_fit() {
        assert_eq!(
            enc(&7i64, Type::INT4).unwrap(),
            (Encoded::Binary, vec![0, 0, 0, 7])
        );
        assert_eq!(enc(&7i16, Type::INT8).unwrap().1, 7i64.to_be_bytes());
        assert_eq!(
            enc(&7i32, Type::NUMERIC).unwrap(),
            (Encoded::Text, b"7".to_vec())
        );
        let e = enc(&3_000_000_000i64, Type::INT4).unwrap_err();
        assert!(
            e.to_string().contains("does not fit in a integer")
                || e.to_string().contains("does not fit in a"),
            "{e}"
        );
        let e = enc(&1i32, Type::TEXT).unwrap_err();
        assert!(e.to_string().contains("pass a &str"), "{e}");

        assert_eq!(
            i16::from_sql(Type::INT8, Format::Binary, &7i64.to_be_bytes()).unwrap(),
            7
        );
        let e = i16::from_sql(Type::INT8, Format::Binary, &70_000i64.to_be_bytes()).unwrap_err();
        assert!(e.to_string().contains("read it as i64"), "{e}");
        assert!(i32::from_sql(Type::INT4, Format::Binary, &[0, 0, 7]).is_err());
        assert!(i32::from_sql(Type::TEXT, Format::Text, b"7").is_err());
    }

    #[test]
    fn floats_never_lose_precision_silently() {
        assert!(enc(&0.5f64, Type::FLOAT4).is_ok());
        assert!(enc(&0.1f64, Type::FLOAT4).is_err());
        assert_eq!(enc(&0.1f64, Type::NUMERIC).unwrap().1, b"0.1");
        assert_eq!(
            enc(&f64::NEG_INFINITY, Type::NUMERIC).unwrap().1,
            b"-Infinity"
        );
        let bits = 1.5f32.to_bits().to_be_bytes();
        assert_eq!(
            f64::from_sql(Type::FLOAT4, Format::Binary, &bits).unwrap(),
            1.5
        );
        assert!(f32::from_sql(
            Type::FLOAT8,
            Format::Binary,
            &1.5f64.to_bits().to_be_bytes()
        )
        .is_err());
    }

    #[test]
    fn strings_go_as_text_to_types_the_server_parses() {
        assert_eq!(enc(&"abc", Type::TEXT).unwrap().0, Encoded::Binary);
        assert_eq!(
            enc(&"6c1b0a3e-0000-4000-8000-000000000000", Type::UUID)
                .unwrap()
                .0,
            Encoded::Text
        );
        assert!(enc(&"abc", Type::BYTEA).is_err());
        assert!(enc(&"a\0c", Type::TEXT).is_err());
        assert_eq!(
            <&str>::from_sql(Type::UUID, Format::Text, b"x").unwrap(),
            "x"
        );
        assert!(String::from_sql(Type::INT4, Format::Binary, &[0, 0, 0, 1]).is_err());
        assert!(String::from_sql(Type::TEXT, Format::Text, &[0xff]).is_err());
    }

    #[test]
    fn timestamps_round_trip() {
        let t = UNIX_EPOCH + Duration::from_micros(1_767_225_600_123_456);
        let (how, bytes) = enc(&t, Type::TIMESTAMPTZ).unwrap();
        assert_eq!(how, Encoded::Binary);
        assert_eq!(
            SystemTime::from_sql(Type::TIMESTAMPTZ, Format::Binary, &bytes).unwrap(),
            t
        );
        let before = UNIX_EPOCH - Duration::from_secs(86_400 * 365 * 40);
        let (_, bytes) = enc(&before, Type::TIMESTAMPTZ).unwrap();
        assert_eq!(
            SystemTime::from_sql(Type::TIMESTAMPTZ, Format::Binary, &bytes).unwrap(),
            before
        );
        // 2000-01-01 is zero.
        let (_, bytes) = enc(
            &(UNIX_EPOCH + Duration::from_secs(946_684_800)),
            Type::TIMESTAMPTZ,
        )
        .unwrap();
        assert_eq!(bytes, [0; 8]);
        assert!(
            SystemTime::from_sql(Type::TIMESTAMPTZ, Format::Binary, &i64::MAX.to_be_bytes())
                .is_err()
        );
        assert!(enc(&t, Type::TIMESTAMP)
            .unwrap_err()
            .to_string()
            .contains("timestamptz"));
    }

    #[test]
    fn null_needs_an_option() {
        assert_eq!(enc(&None::<i32>, Type::INT4).unwrap().0, Encoded::Null);
        assert_eq!(Option::<i32>::from_null(Type::INT4).unwrap(), None);
        let e = i32::from_null(Type::INT4).unwrap_err();
        assert!(e.to_string().contains("Option<i32>"), "{e}");
    }

    #[test]
    fn types_have_names() {
        assert_eq!(Type::INT4.to_string(), "integer");
        assert_eq!(Type::from_oid(99999).to_string(), "type with OID 99999");
    }
}
