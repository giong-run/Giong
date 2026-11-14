//! Replies ([`Value`]), converting them to Rust types ([`FromValue`]), and
//! turning Rust values into command arguments ([`ToArg`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash};

use crate::error::{Error, ServerError};

/// A reply from the server, in RESP3's types.
///
/// Most code never matches on it: [`FromValue`] converts a reply to the Rust
/// type asked for (`Option<String>`, `i64`, `Vec<String>`,
/// `HashMap<String, String>`...). It is there for replies whose shape
/// varies, such as those of scripts.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Nothing: a key that does not exist, or a command that timed out
    /// (`_`, or RESP2's `$-1` and `*-1`).
    Nil,
    /// An integer (`:`).
    Int(i64),
    /// A string of bytes (`$`): what `GET` returns. Redis strings are
    /// binary safe, so it need not be UTF-8.
    Bulk(Vec<u8>),
    /// A short status string (`+`), such as `OK` or `PONG`.
    Status(String),
    /// An array (`*`).
    Array(Vec<Value>),
    /// A map (`%`): what `HGETALL` and `CONFIG GET` return.
    Map(Vec<(Value, Value)>),
    /// A set (`~`): what `SMEMBERS` returns.
    Set(Vec<Value>),
    /// A floating point number (`,`): what `ZSCORE` returns.
    Double(f64),
    /// A boolean (`#`).
    Bool(bool),
    /// An integer too large for `i64` (`(`), in decimal.
    BigNumber(String),
    /// Text with its format (`=`), such as `txt` or `mkd`: what `INFO`
    /// and `LATENCY DOCTOR` return.
    Verbatim {
        /// The format: three letters.
        format: String,
        /// The text.
        text: String,
    },
    /// An error (`-` or `!`) inside an array: the reply of one command of a
    /// pipeline or a transaction. An error that is the whole reply is
    /// returned as an [`Error`] instead.
    Error(ServerError),
    /// A message the server sent on its own (`>`), such as a Pub/Sub
    /// message. [`PubSub`](crate::PubSub) reads them; a
    /// [`Client`](crate::Client) never returns one.
    Push(Vec<Value>),
}

impl Value {
    /// Whether it is [`Value::Nil`].
    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// The bytes of a string: [`Bulk`](Value::Bulk),
    /// [`Status`](Value::Status) or [`Verbatim`](Value::Verbatim).
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bulk(b) => Some(b),
            Value::Status(s) => Some(s.as_bytes()),
            Value::Verbatim { text, .. } => Some(text.as_bytes()),
            _ => None,
        }
    }

    /// A string that is UTF-8: [`Bulk`](Value::Bulk),
    /// [`Status`](Value::Status) or [`Verbatim`](Value::Verbatim).
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    /// The number of an [`Int`](Value::Int).
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// What kind of value it is, for messages: `"nil"`, `"an integer"`...
    pub(crate) fn describe(&self) -> String {
        match self {
            Value::Nil => "nil".into(),
            Value::Int(n) => format!("the integer {n}"),
            Value::Bulk(b) => match std::str::from_utf8(b) {
                Ok(s) if s.len() <= 40 => format!("the string {s:?}"),
                Ok(_) => format!("a string of {} bytes", b.len()),
                Err(_) => format!("a string of {} bytes that is not UTF-8", b.len()),
            },
            Value::Status(s) => format!("the status {s:?}"),
            Value::Array(a) => format!("an array of {}", a.len()),
            Value::Map(m) => format!("a map of {} entries", m.len()),
            Value::Set(s) => format!("a set of {}", s.len()),
            Value::Double(d) => format!("the double {d}"),
            Value::Bool(b) => format!("the boolean {b}"),
            Value::BigNumber(n) => format!("the big number {n}"),
            Value::Verbatim { text, .. } => format!("a text of {} bytes", text.len()),
            Value::Error(e) => format!("the error {:?}", e.to_string()),
            Value::Push(p) => format!("a push message of {}", p.len()),
        }
    }
}

fn mismatch(value: &Value, wanted: &str, hint: &str) -> Error {
    if let Value::Error(e) = value {
        return Error::with_server(crate::error::ErrorKind::Server, "", e.clone());
    }
    let hint = if hint.is_empty() {
        String::new()
    } else {
        format!("; {hint}")
    };
    Error::conversion(format!(
        "the reply is {}, which cannot be read as {wanted}{hint}",
        value.describe()
    ))
}

fn nil(wanted: &str) -> Error {
    Error::conversion(format!(
        "the reply is nil (the key does not exist, or nothing came), which cannot be read as \
         {wanted}; ask for an Option<{wanted}>"
    ))
}

/// A Rust type a reply converts to.
///
/// Implemented for [`Value`] itself, `()`, `bool`, the integer types, `f64`
/// and `f32`, `String`, `Vec<u8>` (a string's bytes), `Option<T>` (nil is
/// `None`), `Vec<T>`, `HashMap`, `BTreeMap`, `HashSet`, `BTreeSet`, and
/// tuples of up to six (an array of exactly that many).
///
/// A missing key is nil: read it as an `Option`, or it is an error. An
/// error inside an array (one command of a pipeline) is returned as that
/// error when it is converted, so `Vec<i64>` fails on it while `Vec<Value>`
/// keeps it.
pub trait FromValue: Sized {
    /// Converts the reply.
    fn from_value(value: Value) -> Result<Self, Error>;

    /// A whole string read as `Vec<Self>`: only `u8` does this, so that
    /// `Vec<u8>` is a string's bytes rather than an array of numbers.
    #[doc(hidden)]
    fn from_byte_vec(_bytes: Vec<u8>) -> Option<Vec<Self>> {
        None
    }
}

impl FromValue for Value {
    fn from_value(value: Value) -> Result<Self, Error> {
        Ok(value)
    }
}

impl FromValue for () {
    /// Any reply but an error.
    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Error(_) => Err(mismatch(&value, "()", "")),
            _ => Ok(()),
        }
    }
}

impl FromValue for bool {
    /// [`Value::Bool`], or the integers 0 and 1 (what `EXISTS`, `SISMEMBER`
    /// and `EXPIRE` return).
    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Bool(b) => Ok(b),
            Value::Int(0) => Ok(false),
            Value::Int(1) => Ok(true),
            Value::Nil => Err(nil("bool")),
            _ => Err(mismatch(&value, "a bool", "only 0, 1 and booleans are")),
        }
    }
}

/// Reads an integer from an [`Int`](Value::Int), a decimal string or a
/// [`BigNumber`](Value::BigNumber) that fits.
fn integer<T: TryFrom<i64> + std::str::FromStr>(value: Value, name: &str) -> Result<T, Error> {
    let parsed = match &value {
        Value::Int(n) => T::try_from(*n).ok(),
        Value::Bulk(_) | Value::Status(_) | Value::BigNumber(_) => match &value {
            Value::Bulk(b) => std::str::from_utf8(b).ok(),
            Value::Status(s) | Value::BigNumber(s) => Some(s.as_str()),
            _ => None,
        }
        .and_then(|s| s.parse::<T>().ok()),
        Value::Nil => return Err(nil(name)),
        _ => return Err(mismatch(&value, name, "")),
    };
    parsed.ok_or_else(|| mismatch(&value, name, "it is not an integer in that type's range"))
}

macro_rules! integers {
    ($($t:ty),*) => {$(
        impl FromValue for $t {
            /// [`Value::Int`], or a string or big number holding a decimal
            /// integer in range.
            fn from_value(value: Value) -> Result<Self, Error> {
                integer(value, stringify!($t))
            }
        }
    )*};
}
integers!(i8, i16, i32, i64, isize, u16, u32, u64, usize);

impl FromValue for u8 {
    fn from_value(value: Value) -> Result<Self, Error> {
        integer(value, "u8")
    }

    fn from_byte_vec(bytes: Vec<u8>) -> Option<Vec<u8>> {
        Some(bytes)
    }
}

impl FromValue for i128 {
    fn from_value(value: Value) -> Result<Self, Error> {
        match &value {
            Value::Int(n) => Ok(i128::from(*n)),
            Value::BigNumber(s) | Value::Status(s) => s
                .parse()
                .map_err(|_| mismatch(&value, "i128", "it is not an integer in that range")),
            Value::Bulk(b) => std::str::from_utf8(b)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| mismatch(&value, "i128", "it is not an integer in that range")),
            Value::Nil => Err(nil("i128")),
            _ => Err(mismatch(&value, "i128", "")),
        }
    }
}

impl FromValue for f64 {
    /// [`Value::Double`], [`Value::Int`], or a string holding a number
    /// (`inf` and `-inf` included).
    fn from_value(value: Value) -> Result<Self, Error> {
        let parsed = match &value {
            Value::Double(d) => Some(*d),
            Value::Int(n) => Some(*n as f64),
            Value::Bulk(b) => std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()),
            Value::Status(s) | Value::BigNumber(s) => s.parse().ok(),
            Value::Nil => return Err(nil("f64")),
            _ => None,
        };
        parsed.ok_or_else(|| mismatch(&value, "f64", ""))
    }
}

impl FromValue for f32 {
    /// As for `f64`, rounded.
    fn from_value(value: Value) -> Result<Self, Error> {
        f64::from_value(value).map(|d| d as f32)
    }
}

impl FromValue for String {
    /// A string that is UTF-8: [`Value::Bulk`], [`Value::Status`],
    /// [`Value::Verbatim`] or [`Value::BigNumber`].
    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Bulk(b) => String::from_utf8(b).map_err(|e| {
                Error::conversion(format!(
                    "the reply is a string of {} bytes that is not UTF-8, which cannot be read \
                     as a String; read it as a Vec<u8>",
                    e.as_bytes().len()
                ))
            }),
            Value::Status(s) | Value::BigNumber(s) => Ok(s),
            Value::Verbatim { text, .. } => Ok(text),
            Value::Nil => Err(nil("String")),
            _ => Err(mismatch(&value, "a String", "")),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    /// Nil is `None`; anything else is converted to `T`.
    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Nil => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

/// The elements of an array, a set or a push; a map's entries as arrays of
/// two.
fn elements(value: Value, wanted: &str) -> Result<Vec<Value>, Error> {
    match value {
        Value::Array(a) | Value::Set(a) | Value::Push(a) => Ok(a),
        Value::Map(m) => Ok(m
            .into_iter()
            .map(|(k, v)| Value::Array(vec![k, v]))
            .collect()),
        Value::Nil => Err(nil(wanted)),
        _ => Err(mismatch(&value, wanted, "")),
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    /// An array or a set, each element converted; a map's entries, each an
    /// array of two (so `Vec<(String, String)>` works). `Vec<u8>` is a
    /// string's bytes instead.
    fn from_value(value: Value) -> Result<Self, Error> {
        let value = match value {
            Value::Bulk(b) => match T::from_byte_vec(b) {
                Some(v) => return Ok(v),
                None => Value::Bulk(Vec::new()),
            },
            Value::Status(s) => match T::from_byte_vec(s.into_bytes()) {
                Some(v) => return Ok(v),
                None => Value::Status(String::new()),
            },
            v => v,
        };
        if matches!(value, Value::Bulk(_) | Value::Status(_)) {
            return Err(Error::conversion(
                "the reply is a string, which cannot be read as a Vec of anything but u8"
                    .to_string(),
            ));
        }
        elements(value, "Vec")?
            .into_iter()
            .map(T::from_value)
            .collect()
    }
}

/// A map's entries, or an array of keys and values one after the other.
fn pairs(value: Value, wanted: &str) -> Result<Vec<(Value, Value)>, Error> {
    match value {
        Value::Map(m) => Ok(m),
        Value::Array(a) if a.len() % 2 == 0 => {
            let mut out = Vec::with_capacity(a.len() / 2);
            let mut it = a.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                out.push((k, v));
            }
            Ok(out)
        }
        Value::Nil => Err(nil(wanted)),
        _ => Err(mismatch(
            &value,
            wanted,
            "a map or an array of keys and values is",
        )),
    }
}

impl<K, V, S> FromValue for HashMap<K, V, S>
where
    K: FromValue + Eq + Hash,
    V: FromValue,
    S: BuildHasher + Default,
{
    /// A map, or an array of keys and values one after the other.
    fn from_value(value: Value) -> Result<Self, Error> {
        pairs(value, "HashMap")?
            .into_iter()
            .map(|(k, v)| Ok((K::from_value(k)?, V::from_value(v)?)))
            .collect()
    }
}

impl<K: FromValue + Ord, V: FromValue> FromValue for BTreeMap<K, V> {
    /// A map, or an array of keys and values one after the other.
    fn from_value(value: Value) -> Result<Self, Error> {
        pairs(value, "BTreeMap")?
            .into_iter()
            .map(|(k, v)| Ok((K::from_value(k)?, V::from_value(v)?)))
            .collect()
    }
}

impl<T, S> FromValue for HashSet<T, S>
where
    T: FromValue + Eq + Hash,
    S: BuildHasher + Default,
{
    /// A set or an array.
    fn from_value(value: Value) -> Result<Self, Error> {
        elements(value, "HashSet")?
            .into_iter()
            .map(T::from_value)
            .collect()
    }
}

impl<T: FromValue + Ord> FromValue for BTreeSet<T> {
    /// A set or an array.
    fn from_value(value: Value) -> Result<Self, Error> {
        elements(value, "BTreeSet")?
            .into_iter()
            .map(T::from_value)
            .collect()
    }
}

macro_rules! tuples {
    ($($n:literal => ($($t:ident),+);)*) => {$(
        impl<$($t: FromValue),+> FromValue for ($($t,)+) {
            /// An array of exactly this many elements, such as the replies
            /// of a pipeline.
            fn from_value(value: Value) -> Result<Self, Error> {
                let items = elements(value, concat!("a tuple of ", $n))?;
                if items.len() != $n {
                    return Err(Error::conversion(format!(
                        "the reply has {} elements, which cannot be read as a tuple of {}",
                        items.len(),
                        $n
                    )));
                }
                let mut it = items.into_iter();
                Ok(($($t::from_value(it.next().expect("counted"))?,)+))
            }
        }
    )*};
}
tuples! {
    1 => (A);
    2 => (A, B);
    3 => (A, B, C);
    4 => (A, B, C, D);
    5 => (A, B, C, D, E);
    6 => (A, B, C, D, E, F);
}

/// A Rust value that can be a command's argument.
///
/// Implemented for strings (`&str`, `String`), bytes (`&[u8]`, `Vec<u8>`,
/// `[u8; N]`), the integer types, `f64` and `f32`, and references to any
/// of these. Numbers are sent in decimal, as Redis expects.
///
/// Implement it for your own types, such as typed keys. Whatever
/// [`write_arg`](Self::write_arg) writes, in as many pieces as it likes,
/// is sent as one argument:
///
/// ```
/// use giong_redis::{cmd, ArgWriter, ToArg};
///
/// struct SessionKey(u64);
///
/// impl ToArg for SessionKey {
///     fn write_arg(&self, out: &mut ArgWriter<'_>) {
///         out.write(b"session:");
///         self.0.write_arg(out);
///     }
/// }
///
/// let get = cmd("GET").arg(SessionKey(42)); // GET session:42
/// ```
pub trait ToArg {
    /// Writes the argument's bytes to `out`. Everything written, nothing
    /// included, is one argument.
    fn write_arg(&self, out: &mut ArgWriter<'_>);
}

/// Where [`ToArg::write_arg`] writes one argument of a command.
///
/// The pieces written are joined into that one argument: an argument
/// cannot end early, or spill into the next one, whatever an
/// implementation does. It also implements [`std::fmt::Write`], so
/// `write!(out, "user:{id}")` works.
pub struct ArgWriter<'a> {
    out: &'a mut Vec<u8>,
    /// Where the argument's `$` is.
    start: usize,
    /// Where its bytes begin, once something was written.
    body: Option<usize>,
    /// More than one piece was written: the length in the header written
    /// with the first is too short.
    grown: bool,
}

impl ArgWriter<'_> {
    /// Adds bytes to the argument.
    pub fn write(&mut self, bytes: &[u8]) {
        match self.body {
            // The header is written with the first piece's length, which is
            // the argument's whole length almost always: one piece.
            None => {
                self.out.push(b'$');
                let mut digits = [0u8; 20];
                self.out
                    .extend_from_slice(decimal_u64(&mut digits, bytes.len() as u64));
                self.out.extend_from_slice(b"\r\n");
                self.body = Some(self.out.len());
                self.out.extend_from_slice(bytes);
            }
            Some(_) => {
                self.grown |= !bytes.is_empty();
                self.out.extend_from_slice(bytes);
            }
        }
    }

    /// How many bytes the argument has so far.
    pub fn len(&self) -> usize {
        self.body.map_or(0, |body| self.out.len() - body)
    }

    /// Whether nothing was written to the argument yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ends the argument: fixes the length in its header if it grew, or
    /// writes an empty argument if nothing was written.
    fn finish(self) {
        let Some(body) = self.body else {
            self.out.extend_from_slice(b"$0\r\n\r\n");
            return;
        };
        if self.grown {
            let len = self.out.len() - body;
            let mut digits = [0u8; 20];
            let digits = decimal_u64(&mut digits, len as u64);
            let header = 1 + digits.len() + 2;
            // The length only grew, so the header can only have become
            // longer: move the bytes along to make room.
            let extra = header - (body - self.start);
            if extra > 0 {
                let end = self.out.len();
                self.out.resize(end + extra, 0);
                self.out.copy_within(body..end, body + extra);
            }
            let at = self.start + 1;
            self.out[at..at + digits.len()].copy_from_slice(digits);
            self.out[at + digits.len()..at + digits.len() + 2].copy_from_slice(b"\r\n");
        }
        self.out.extend_from_slice(b"\r\n");
    }
}

impl std::fmt::Write for ArgWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

/// Writes `arg` as one RESP bulk string at the end of `out`.
pub(crate) fn write_bulk(out: &mut Vec<u8>, arg: &(impl ToArg + ?Sized)) {
    let mut w = ArgWriter {
        start: out.len(),
        out,
        body: None,
        grown: false,
    };
    arg.write_arg(&mut w);
    w.finish();
}

impl<T: ToArg + ?Sized> ToArg for &T {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        (**self).write_arg(out)
    }
}

impl ToArg for str {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        out.write(self.as_bytes())
    }
}

impl ToArg for String {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        out.write(self.as_bytes())
    }
}

impl ToArg for [u8] {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        out.write(self)
    }
}

impl ToArg for Vec<u8> {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        out.write(self)
    }
}

impl<const N: usize> ToArg for [u8; N] {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        out.write(self)
    }
}

macro_rules! int_args {
    ($($t:ty),*) => {$(
        impl ToArg for $t {
            fn write_arg(&self, out: &mut ArgWriter<'_>) {
                let mut buf = [0u8; 40];
                out.write(decimal(&mut buf, i128::from(*self)))
            }
        }
    )*};
}
int_args!(i8, i16, i32, i64, u8, u16, u32, u64);

impl ToArg for isize {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        let mut buf = [0u8; 40];
        out.write(decimal(&mut buf, *self as i128))
    }
}

impl ToArg for usize {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        let mut buf = [0u8; 40];
        out.write(decimal(&mut buf, *self as i128))
    }
}

/// Writes `n` in decimal at the end of `buf` and returns that part.
pub(crate) fn decimal(buf: &mut [u8; 40], n: i128) -> &[u8] {
    let negative = n < 0;
    let m = n.unsigned_abs();
    let mut i = buf.len();
    if let Ok(small) = u64::try_from(m) {
        // 64-bit division is much cheaper than 128-bit.
        let mut digits = [0u8; 20];
        let d = decimal_u64(&mut digits, small);
        i -= d.len();
        buf[i..].copy_from_slice(d);
    } else {
        let mut m = m;
        while m > 0 {
            i -= 1;
            buf[i] = b'0' + (m % 10) as u8;
            m /= 10;
        }
    }
    if negative {
        i -= 1;
        buf[i] = b'-';
    }
    &buf[i..]
}

/// Writes `n` in decimal at the end of `buf` and returns that part.
pub(crate) fn decimal_u64(buf: &mut [u8; 20], mut n: u64) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}

impl ToArg for f64 {
    /// The shortest decimal that reads back as the same number; `inf` and
    /// `-inf` for the infinities, which Redis accepts as scores.
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        if self.is_infinite() {
            out.write(if *self > 0.0 { b"inf" } else { b"-inf" })
        } else {
            use std::fmt::Write;
            let _ = write!(out, "{self}");
        }
    }
}

impl ToArg for f32 {
    fn write_arg(&self, out: &mut ArgWriter<'_>) {
        f64::from(*self).write_arg(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::Bulk(s.as_bytes().to_vec())
    }

    /// The bytes of the argument, without the bulk string around them.
    fn arg(a: impl ToArg) -> Vec<u8> {
        let mut out = Vec::new();
        write_bulk(&mut out, &a);
        let body = out.iter().position(|&b| b == b'\n').unwrap() + 1;
        let len: usize = std::str::from_utf8(&out[1..body - 2])
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(out.len(), body + len + 2, "{out:?}");
        assert!(out.ends_with(b"\r\n"));
        out[body..body + len].to_vec()
    }

    #[test]
    fn arguments_are_written_as_redis_reads_them() {
        assert_eq!(arg("key"), b"key");
        assert_eq!(arg(0i64), b"0");
        assert_eq!(arg(-42i32), b"-42");
        assert_eq!(arg(i64::MIN), b"-9223372036854775808");
        assert_eq!(arg(u64::MAX), b"18446744073709551615");
        assert_eq!(arg(usize::MAX), usize::MAX.to_string().as_bytes());
        assert_eq!(arg(1.5f64), b"1.5");
        assert_eq!(arg(f64::INFINITY), b"inf");
        assert_eq!(arg(f64::NEG_INFINITY), b"-inf");
        assert_eq!(arg(vec![0u8, 255]), [0, 255]);
        let nested: &&&str = &&"nested";
        assert_eq!(arg(nested), b"nested");
    }

    #[test]
    fn strings_and_numbers_convert_both_ways_they_are_sent() {
        assert_eq!(String::from_value(bulk("hello")).unwrap(), "hello");
        assert_eq!(i64::from_value(bulk("-17")).unwrap(), -17);
        assert_eq!(i64::from_value(Value::Int(9)).unwrap(), 9);
        assert_eq!(u8::from_value(Value::Int(200)).unwrap(), 200);
        assert!(u8::from_value(Value::Int(300)).is_err());
        assert!(u32::from_value(Value::Int(-1)).is_err());
        assert_eq!(f64::from_value(bulk("2.5")).unwrap(), 2.5);
        assert_eq!(
            f64::from_value(Value::Double(f64::INFINITY)).unwrap(),
            f64::INFINITY
        );
        assert_eq!(f64::from_value(bulk("-inf")).unwrap(), f64::NEG_INFINITY);
        assert!(i128::from_value(Value::BigNumber(
            "3492890328409238509324850943850943825024385".into()
        ))
        .is_err());
        assert_eq!(
            i128::from_value(Value::BigNumber(
                "-170141183460469231731687303715884105728".into()
            ))
            .unwrap(),
            i128::MIN
        );
        assert!(bool::from_value(Value::Int(1)).unwrap());
        assert!(!bool::from_value(Value::Bool(false)).unwrap());
        assert!(bool::from_value(Value::Int(2)).is_err());
    }

    #[test]
    fn nil_needs_an_option_and_says_so() {
        let e = String::from_value(Value::Nil).unwrap_err();
        assert_eq!(e.kind(), crate::ErrorKind::Type);
        assert!(e.to_string().contains("Option<String>"), "{e}");
        assert_eq!(Option::<String>::from_value(Value::Nil).unwrap(), None);
        assert_eq!(
            Option::<String>::from_value(bulk("x")).unwrap(),
            Some("x".to_string())
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_point_to_vec_u8() {
        let e = String::from_value(Value::Bulk(vec![0xff, 0xfe])).unwrap_err();
        assert!(e.to_string().contains("Vec<u8>"), "{e}");
        assert_eq!(
            Vec::<u8>::from_value(Value::Bulk(vec![0xff, 0xfe])).unwrap(),
            [0xff, 0xfe]
        );
        assert_eq!(
            Vec::<u8>::from_value(Value::Array(vec![Value::Int(1), Value::Int(2)])).unwrap(),
            [1, 2]
        );
        assert!(Vec::<String>::from_value(bulk("abc")).is_err());
    }

    #[test]
    fn collections_read_maps_sets_and_flat_arrays() {
        let map = Value::Map(vec![(bulk("a"), bulk("1")), (bulk("b"), bulk("2"))]);
        let m: HashMap<String, i64> = FromValue::from_value(map.clone()).unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
        let b: BTreeMap<String, String> = FromValue::from_value(Value::Array(vec![
            bulk("k1"),
            bulk("v1"),
            bulk("k2"),
            bulk("v2"),
        ]))
        .unwrap();
        assert_eq!(b.len(), 2);
        assert!(HashMap::<String, String>::from_value(Value::Array(vec![bulk("odd")])).is_err());
        let pairs: Vec<(String, i64)> = FromValue::from_value(map).unwrap();
        assert_eq!(pairs, [("a".to_string(), 1), ("b".to_string(), 2)]);
        let s: HashSet<String> =
            FromValue::from_value(Value::Set(vec![bulk("x"), bulk("y")])).unwrap();
        assert!(s.contains("x") && s.contains("y"));
        let scores: Vec<(String, f64)> = FromValue::from_value(Value::Array(vec![
            Value::Array(vec![bulk("m1"), Value::Double(1.0)]),
            Value::Array(vec![bulk("m2"), Value::Double(2.5)]),
        ]))
        .unwrap();
        assert_eq!(scores[1], ("m2".to_string(), 2.5));
    }

    #[test]
    fn tuples_need_the_exact_length() {
        let v = Value::Array(vec![Value::Status("OK".into()), Value::Int(3)]);
        let (ok, n): (String, i64) = FromValue::from_value(v.clone()).unwrap();
        assert_eq!((ok.as_str(), n), ("OK", 3));
        assert!(<(String, i64, i64)>::from_value(v).is_err());
    }

    #[test]
    fn an_error_inside_an_array_is_returned_when_converted() {
        let wrong = ServerError::new("WRONGTYPE Operation against a key".into());
        let v = Value::Array(vec![Value::Int(1), Value::Error(wrong)]);
        let e = Vec::<i64>::from_value(v.clone()).unwrap_err();
        assert_eq!(e.kind(), crate::ErrorKind::Server);
        assert_eq!(e.server_code(), Some("WRONGTYPE"));
        assert_eq!(Vec::<Value>::from_value(v).unwrap().len(), 2);
        assert!(<()>::from_value(Value::Error(ServerError::new("ERR x".into()))).is_err());
    }
}
