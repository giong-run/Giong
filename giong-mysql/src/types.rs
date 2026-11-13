//! Converting values between Rust and MySQL.
//!
//! Statements run as prepared statements, so values travel in MySQL's binary
//! format both ways: no number is printed and parsed again.
//!
//! **Parameters** ([`ToSql`]): each Rust type is sent as the MySQL type that
//! holds it exactly, and the server converts it to the column's type as it
//! would a literal.
//!
//! | Rust | Sent as |
//! |---|---|
//! | `i8`, `i16`, `i32`, `i64` (and `u8` to `u64`) | `TINYINT` to `BIGINT` (`UNSIGNED`) |
//! | `f32`, `f64` | `FLOAT`, `DOUBLE` |
//! | `bool` | `TINYINT`, 1 or 0 |
//! | `&str`, `String` | a string in the connection's character set (`utf8mb4`) |
//! | `&[u8]`, `Vec<u8>`, `[u8; N]` | a binary string |
//! | `SystemTime` | `DATETIME(6)`, in UTC |
//! | `Option<T>` | `T`, or `NULL` for `None` |
//!
//! Pass a `DECIMAL` as a `&str` (`"19.99"`): it stays exact.
//!
//! **Columns** ([`FromSql`], through [`Row::get`](crate::Row::get)): a value
//! is converted only when nothing is lost, and the error says what to read
//! it as otherwise.
//!
//! | MySQL | Rust |
//! |---|---|
//! | `TINYINT` ... `BIGINT`, `YEAR`, signed or `UNSIGNED` | any integer type the value fits in; `bool` for 0 and 1 |
//! | `DECIMAL` | `String` or `&str`; an integer type when the value is a whole number that fits |
//! | `FLOAT`, `DOUBLE` | `f32` (`FLOAT` only), `f64` |
//! | `CHAR`, `VARCHAR`, `TEXT`, `ENUM`, `SET`, `JSON` | `String`, `&str`, `Vec<u8>`, `&[u8]` |
//! | `BINARY`, `VARBINARY`, `BLOB`, `BIT`, `GEOMETRY` | `Vec<u8>`, `&[u8]`; [`Bits`] for `BIT` |
//! | `DATETIME`, `TIMESTAMP` | `SystemTime` (in UTC), or `String` as `2026-01-31 12:00:00.000001` |
//! | `DATE`, `TIME` | `String`, as `2026-01-31` and `-838:59:59` |
//! | `NULL` | `Option<T>` |
//!
//! The session's time zone is set to UTC when connecting, so `TIMESTAMP`
//! columns and `NOW()` agree with `SystemTime`. Do not change `time_zone` on
//! a connection.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;
use crate::row::Column;

/// A MySQL column or parameter type, as the protocol numbers it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColumnType(u8);

macro_rules! types {
    ($($name:ident = $code:literal, $sql:literal;)*) => {
        impl ColumnType {
            $(
                #[doc = concat!("`", $sql, "`")]
                pub const $name: ColumnType = ColumnType($code);
            )*

            /// The SQL name of the type, such as `BIGINT` or `VARCHAR`, if this
            /// crate knows it.
            fn sql_name(self) -> Option<&'static str> {
                match self.0 {
                    $($code => Some($sql),)*
                    _ => None,
                }
            }
        }
    };
}

types! {
    DECIMAL = 0, "DECIMAL";
    TINY = 1, "TINYINT";
    SHORT = 2, "SMALLINT";
    LONG = 3, "INT";
    FLOAT = 4, "FLOAT";
    DOUBLE = 5, "DOUBLE";
    NULL = 6, "NULL";
    TIMESTAMP = 7, "TIMESTAMP";
    LONGLONG = 8, "BIGINT";
    INT24 = 9, "MEDIUMINT";
    DATE = 10, "DATE";
    TIME = 11, "TIME";
    DATETIME = 12, "DATETIME";
    YEAR = 13, "YEAR";
    VARCHAR = 15, "VARCHAR";
    BIT = 16, "BIT";
    VECTOR = 242, "VECTOR";
    JSON = 245, "JSON";
    NEWDECIMAL = 246, "DECIMAL";
    ENUM = 247, "ENUM";
    SET = 248, "SET";
    TINY_BLOB = 249, "TINYBLOB";
    MEDIUM_BLOB = 250, "MEDIUMBLOB";
    LONG_BLOB = 251, "LONGBLOB";
    BLOB = 252, "BLOB";
    VAR_STRING = 253, "VARCHAR";
    STRING = 254, "CHAR";
    GEOMETRY = 255, "GEOMETRY";
}

impl ColumnType {
    pub(crate) fn from_code(code: u8) -> Self {
        Self(code)
    }

    /// The number the protocol uses for the type.
    pub fn code(self) -> u8 {
        self.0
    }

    /// Whether values of the type are integers.
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Self::TINY | Self::SHORT | Self::LONG | Self::INT24 | Self::LONGLONG | Self::YEAR
        )
    }

    /// Whether values of the type are dates or times.
    pub fn is_temporal(self) -> bool {
        matches!(
            self,
            Self::DATE | Self::TIME | Self::DATETIME | Self::TIMESTAMP
        )
    }

    /// How many bytes a value takes in a binary row, for the fixed-size
    /// types; `None` for the others.
    pub(crate) fn fixed_size(self) -> Option<usize> {
        Some(match self {
            Self::TINY => 1,
            Self::SHORT | Self::YEAR => 2,
            Self::LONG | Self::INT24 | Self::FLOAT => 4,
            Self::LONGLONG | Self::DOUBLE => 8,
            Self::NULL => 0,
            _ => return None,
        })
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.sql_name() {
            Some(name) => f.write_str(name),
            None => write!(f, "type {}", self.0),
        }
    }
}

impl fmt::Debug for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ColumnType({self})")
    }
}

// ----------------------------------------------------------------- ToSql

/// The type a parameter was sent as, from [`ToSql::to_sql`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Encoded {
    pub(crate) ty: ColumnType,
    pub(crate) unsigned: bool,
}

impl Encoded {
    const fn signed(ty: ColumnType) -> Self {
        Self {
            ty,
            unsigned: false,
        }
    }

    const fn unsigned(ty: ColumnType) -> Self {
        Self { ty, unsigned: true }
    }

    pub(crate) const NULL: Self = Self::signed(ColumnType::NULL);
}

/// A Rust value that can be a statement parameter.
///
/// `to_sql` appends the value in MySQL's binary format and says which type
/// that is. Implement it for your own types by delegating to one of the
/// implementations here:
///
/// ```
/// use giong_mysql::{Encoded, Error, ToSql};
///
/// struct Email(String);
///
/// impl ToSql for Email {
///     fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
///         self.0.to_sql(out)
///     }
/// }
/// ```
pub trait ToSql {
    /// Appends the value to `out` and returns its type. Return an error made
    /// with [`Error::conversion`] when the value cannot be sent; it is
    /// reported with the parameter's number.
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error>;
}

macro_rules! int_to_sql {
    ($($t:ty => $ty:ident, $how:ident;)*) => {$(
        impl ToSql for $t {
            fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
                out.extend_from_slice(&self.to_le_bytes());
                Ok(Encoded::$how(ColumnType::$ty))
            }
        }
    )*};
}

int_to_sql! {
    i8 => TINY, signed;
    i16 => SHORT, signed;
    i32 => LONG, signed;
    i64 => LONGLONG, signed;
    u8 => TINY, unsigned;
    u16 => SHORT, unsigned;
    u32 => LONG, unsigned;
    u64 => LONGLONG, unsigned;
    f32 => FLOAT, signed;
    f64 => DOUBLE, signed;
}

impl ToSql for bool {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        out.push(u8::from(*self));
        Ok(Encoded::signed(ColumnType::TINY))
    }
}

impl ToSql for str {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        crate::protocol::put_lenenc_bytes(out, self.as_bytes());
        Ok(Encoded::signed(ColumnType::VAR_STRING))
    }
}

impl ToSql for String {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_str().to_sql(out)
    }
}

impl ToSql for [u8] {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        crate::protocol::put_lenenc_bytes(out, self);
        Ok(Encoded::signed(ColumnType::BLOB))
    }
}

impl ToSql for Vec<u8> {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_slice().to_sql(out)
    }
}

impl<const N: usize> ToSql for [u8; N] {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        self.as_slice().to_sql(out)
    }
}

impl ToSql for SystemTime {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        let (secs, micros) = match self.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_micros()),
            Err(e) => {
                let d = e.duration();
                // Before 1970: whole seconds round down, the rest counts up.
                let (s, us) = (d.as_secs() as i64, d.subsec_micros());
                if us == 0 {
                    (-s, 0)
                } else {
                    (-s - 1, 1_000_000 - us)
                }
            }
        };
        let days = secs.div_euclid(86_400);
        let in_day = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        if !(1..=9999).contains(&year) {
            return Err(Error::conversion(format!(
                "the time is in the year {year}; MySQL keeps years 1 to 9999"
            )));
        }
        out.push(11);
        out.extend_from_slice(&(year as u16).to_le_bytes());
        out.push(month);
        out.push(day);
        out.push((in_day / 3600) as u8);
        out.push((in_day / 60 % 60) as u8);
        out.push((in_day % 60) as u8);
        out.extend_from_slice(&micros.to_le_bytes());
        Ok(Encoded::signed(ColumnType::DATETIME))
    }
}

impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        match self {
            Some(v) => v.to_sql(out),
            None => Ok(Encoded::NULL),
        }
    }
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        (**self).to_sql(out)
    }
}

impl<T: ToSql + ?Sized> ToSql for Box<T> {
    fn to_sql(&self, out: &mut Vec<u8>) -> Result<Encoded, Error> {
        (**self).to_sql(out)
    }
}

// --------------------------------------------------------------- FromSql

/// A Rust value that a column can be read as, with
/// [`Row::get`](crate::Row::get).
///
/// `'a` is the lifetime of the row, so that `&str` and `&[u8]` can borrow
/// from it without copying.
pub trait FromSql<'a>: Sized {
    /// Converts a value that is not `NULL`. `raw` is the value in MySQL's
    /// binary format: the bytes of a string, the little-endian bytes of a
    /// number, or the fields of a date or time.
    ///
    /// Return an error made with [`Error::conversion`] saying why the value
    /// cannot be converted; it is reported with the column's name.
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error>;

    /// Converts `NULL`. The default refuses it.
    fn from_null(column: &Column) -> Result<Self, Error> {
        let _ = column;
        Err(Error::conversion(format!(
            "the value is NULL; read it as Option<{}>",
            std::any::type_name::<Self>()
        )))
    }
}

/// What to read a column as.
fn read_as(column: &Column) -> &'static str {
    let ty = column.ty();
    match ty {
        _ if ty.is_integer() => "; read it as an integer type it fits in (i64 or u64 always do)",
        ColumnType::DECIMAL | ColumnType::NEWDECIMAL => "; read it as String or &str",
        ColumnType::FLOAT => "; read it as f32 or f64",
        ColumnType::DOUBLE => "; read it as f64",
        ColumnType::DATETIME | ColumnType::TIMESTAMP => "; read it as SystemTime or String",
        ColumnType::DATE | ColumnType::TIME => "; read it as String",
        ColumnType::BIT => "; read it as Bits or Vec<u8>",
        _ if column.is_binary() => "; it is binary data: read it as Vec<u8> or &[u8]",
        _ => "; read it as String or &str",
    }
}

fn wrong_column<T>(column: &Column) -> Error {
    Error::conversion(format!(
        "a {} cannot be read as {}{}",
        column.type_name(),
        std::any::type_name::<T>(),
        read_as(column)
    ))
}

fn fixed<const N: usize>(raw: &[u8], column: &Column) -> Result<[u8; N], Error> {
    raw.try_into().map_err(|_| {
        Error::conversion(format!(
            "the server sent {} bytes for a {}, which has {N}",
            raw.len(),
            column.type_name()
        ))
    })
}

/// An integer column's value, widened to i128 so that every signed and
/// unsigned value fits.
fn get_int(column: &Column, raw: &[u8]) -> Option<Result<i128, Error>> {
    let unsigned = column.is_unsigned();
    let ty = column.ty();
    let v = match ty {
        ColumnType::TINY => fixed::<1>(raw, column).map(|[b]| {
            if unsigned {
                i128::from(b)
            } else {
                i128::from(b as i8)
            }
        }),
        ColumnType::SHORT | ColumnType::YEAR => fixed::<2>(raw, column).map(|b| {
            if unsigned || ty == ColumnType::YEAR {
                i128::from(u16::from_le_bytes(b))
            } else {
                i128::from(i16::from_le_bytes(b))
            }
        }),
        ColumnType::LONG | ColumnType::INT24 => fixed::<4>(raw, column).map(|b| {
            if unsigned {
                i128::from(u32::from_le_bytes(b))
            } else {
                i128::from(i32::from_le_bytes(b))
            }
        }),
        ColumnType::LONGLONG => fixed::<8>(raw, column).map(|b| {
            if unsigned {
                i128::from(u64::from_le_bytes(b))
            } else {
                i128::from(i64::from_le_bytes(b))
            }
        }),
        ColumnType::DECIMAL | ColumnType::NEWDECIMAL => return Some(decimal_int(raw)),
        _ => return None,
    };
    Some(v)
}

/// A `DECIMAL` that is a whole number, such as `SUM` of an integer column
/// returns.
fn decimal_int(raw: &[u8]) -> Result<i128, Error> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| Error::conversion("the server sent a DECIMAL that is not text"))?;
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if !frac.bytes().all(|b| b == b'0') {
        return Err(Error::conversion(format!(
            "the DECIMAL {text} is not a whole number; read it as String"
        )));
    }
    whole.parse().map_err(|_| {
        Error::conversion(format!(
            "the DECIMAL {text} is too large for any integer type; read it as String"
        ))
    })
}

macro_rules! int_from_sql {
    ($($t:ty),*) => {$(
        impl<'a> FromSql<'a> for $t {
            fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
                let v = get_int(column, raw).ok_or_else(|| wrong_column::<$t>(column))??;
                <$t>::try_from(v).map_err(|_| {
                    Error::conversion(format!(
                        "{v} does not fit in {}; read it as {}",
                        stringify!($t),
                        if v > i128::from(i64::MAX) { "u64" } else { "i64" },
                    ))
                })
            }
        }
    )*};
}

int_from_sql!(i8, i16, i32, i64, u8, u16, u32, u64);

impl<'a> FromSql<'a> for bool {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        match get_int(column, raw).ok_or_else(|| wrong_column::<bool>(column))?? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(Error::conversion(format!(
                "{v} is neither 0 nor 1, so it is not a bool; read it as an integer"
            ))),
        }
    }
}

impl<'a> FromSql<'a> for f64 {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        match column.ty() {
            ColumnType::DOUBLE => Ok(f64::from_le_bytes(fixed(raw, column)?)),
            ColumnType::FLOAT => Ok(f64::from(f32::from_le_bytes(fixed(raw, column)?))),
            _ => Err(wrong_column::<f64>(column)),
        }
    }
}

impl<'a> FromSql<'a> for f32 {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        match column.ty() {
            ColumnType::FLOAT => Ok(f32::from_le_bytes(fixed(raw, column)?)),
            _ => Err(wrong_column::<f32>(column)),
        }
    }
}

/// Whether the column's value is text this crate can hand out as `&str`.
fn is_text(column: &Column) -> bool {
    let ty = column.ty();
    match ty {
        ColumnType::DECIMAL
        | ColumnType::NEWDECIMAL
        | ColumnType::JSON
        | ColumnType::ENUM
        | ColumnType::SET => true,
        ColumnType::VARCHAR
        | ColumnType::VAR_STRING
        | ColumnType::STRING
        | ColumnType::TINY_BLOB
        | ColumnType::MEDIUM_BLOB
        | ColumnType::LONG_BLOB
        | ColumnType::BLOB => !column.is_binary(),
        _ => false,
    }
}

impl<'a> FromSql<'a> for &'a str {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        if !is_text(column) {
            return Err(wrong_column::<&str>(column));
        }
        std::str::from_utf8(raw).map_err(|_| {
            Error::conversion("the value is not valid UTF-8; read it as Vec<u8> or &[u8]")
        })
    }
}

impl<'a> FromSql<'a> for String {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        if column.ty().is_temporal() {
            return format_temporal(column.ty(), raw);
        }
        <&str>::from_sql(column, raw).map(str::to_owned)
    }
}

/// Whether the column's value is a string of bytes (text or binary).
fn is_bytes(column: &Column) -> bool {
    !column.ty().is_integer() && !column.ty().is_temporal() && column.ty().fixed_size().is_none()
}

impl<'a> FromSql<'a> for &'a [u8] {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        if !is_bytes(column) {
            return Err(wrong_column::<&[u8]>(column));
        }
        Ok(raw)
    }
}

impl<'a> FromSql<'a> for Vec<u8> {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        <&[u8]>::from_sql(column, raw).map(<[u8]>::to_vec)
    }
}

/// `BIT(n)` as a number.
fn bit_value(raw: &[u8]) -> Result<u64, Error> {
    if raw.len() > 8 {
        return Err(Error::conversion("a BIT value longer than 64 bits"));
    }
    Ok(raw.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b)))
}

/// A `BIT(n)` column read as a number: `row.get::<Bits>("flags")?.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bits(pub u64);

impl<'a> FromSql<'a> for Bits {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        if column.ty() != ColumnType::BIT {
            return Err(wrong_column::<Bits>(column));
        }
        bit_value(raw).map(Bits)
    }
}

/// The fields of a binary `DATE`, `DATETIME` or `TIMESTAMP`.
struct DateTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
}

fn date_time(raw: &[u8]) -> Result<DateTime, Error> {
    let get = |i: usize| raw.get(i).copied().unwrap_or(0);
    if !matches!(raw.len(), 0 | 4 | 7 | 11) {
        return Err(Error::conversion(format!(
            "the server sent a date of {} bytes; dates have 0, 4, 7 or 11",
            raw.len()
        )));
    }
    Ok(DateTime {
        year: u16::from_le_bytes([get(0), get(1)]),
        month: get(2),
        day: get(3),
        hour: get(4),
        minute: get(5),
        second: get(6),
        micros: if raw.len() == 11 {
            u32::from_le_bytes([raw[7], raw[8], raw[9], raw[10]])
        } else {
            0
        },
    })
}

fn format_temporal(ty: ColumnType, raw: &[u8]) -> Result<String, Error> {
    use std::fmt::Write;
    let mut s = String::new();
    if ty == ColumnType::TIME {
        if !matches!(raw.len(), 0 | 8 | 12) {
            return Err(Error::conversion(format!(
                "the server sent a TIME of {} bytes; times have 0, 8 or 12",
                raw.len()
            )));
        }
        let get = |i: usize| raw.get(i).copied().unwrap_or(0);
        let negative = get(0) == 1;
        let days = u32::from_le_bytes([get(1), get(2), get(3), get(4)]);
        // In u64: a TIME is at most 34 days, but a server could send more.
        let hours = u64::from(days) * 24 + u64::from(get(5));
        let _ = write!(
            s,
            "{}{:02}:{:02}:{:02}",
            if negative { "-" } else { "" },
            hours,
            get(6),
            get(7)
        );
        if raw.len() == 12 {
            let micros = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
            let _ = write!(s, ".{micros:06}");
        }
        return Ok(s);
    }
    let d = date_time(raw)?;
    let _ = write!(s, "{:04}-{:02}-{:02}", d.year, d.month, d.day);
    if ty != ColumnType::DATE {
        let _ = write!(s, " {:02}:{:02}:{:02}", d.hour, d.minute, d.second);
        if raw.len() == 11 {
            let _ = write!(s, ".{:06}", d.micros);
        }
    }
    Ok(s)
}

impl<'a> FromSql<'a> for SystemTime {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        if !matches!(column.ty(), ColumnType::DATETIME | ColumnType::TIMESTAMP) {
            return Err(wrong_column::<SystemTime>(column));
        }
        let d = date_time(raw)?;
        if d.month == 0 || d.day == 0 {
            return Err(Error::conversion(format!(
                "{} is a zero date, which is no point in time; read it as String, or store NULL \
                 instead",
                format_temporal(column.ty(), raw)?
            )));
        }
        let days = days_from_civil(i64::from(d.year), d.month, d.day);
        let secs = days * 86_400
            + i64::from(d.hour) * 3600
            + i64::from(d.minute) * 60
            + i64::from(d.second);
        let micros = Duration::from_micros(u64::from(d.micros));
        Ok(if secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(secs as u64) + micros
        } else {
            UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + micros
        })
    }
}

impl<'a, T: FromSql<'a>> FromSql<'a> for Option<T> {
    fn from_sql(column: &Column, raw: &'a [u8]) -> Result<Self, Error> {
        T::from_sql(column, raw).map(Some)
    }

    fn from_null(_: &Column) -> Result<Self, Error> {
        Ok(None)
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar
/// (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u8, d: u8) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number from [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ty: ColumnType, flags: u16, collation: u16) -> Column {
        Column::for_tests("c", ty, flags, collation)
    }

    const UNSIGNED: u16 = 32;

    #[test]
    fn integers_convert_when_they_fit() {
        let tiny = col(ColumnType::TINY, 0, 63);
        assert_eq!(i8::from_sql(&tiny, &[0xFF]).unwrap(), -1);
        assert!(u8::from_sql(&tiny, &[0xFF]).is_err());
        let utiny = col(ColumnType::TINY, UNSIGNED, 63);
        assert_eq!(u8::from_sql(&utiny, &[0xFF]).unwrap(), 255);
        assert_eq!(i64::from_sql(&utiny, &[0xFF]).unwrap(), 255);
        let ubig = col(ColumnType::LONGLONG, UNSIGNED, 63);
        let max = u64::MAX.to_le_bytes();
        assert_eq!(u64::from_sql(&ubig, &max).unwrap(), u64::MAX);
        let e = i64::from_sql(&ubig, &max).unwrap_err().to_string();
        assert!(e.contains("read it as u64"), "{e}");
        let year = col(ColumnType::YEAR, UNSIGNED, 63);
        assert_eq!(i32::from_sql(&year, &2026u16.to_le_bytes()).unwrap(), 2026);
        assert!(bool::from_sql(&tiny, &[1]).unwrap());
        assert!(bool::from_sql(&tiny, &[2]).is_err());
        assert!(i64::from_sql(&tiny, &[1, 2]).is_err());

        let dec = col(ColumnType::NEWDECIMAL, 0, 63);
        assert_eq!(i64::from_sql(&dec, b"-42.000").unwrap(), -42);
        assert!(i64::from_sql(&dec, b"1.5").is_err());
        assert_eq!(<&str>::from_sql(&dec, b"1.5").unwrap(), "1.5");
        let text = col(ColumnType::VAR_STRING, 0, 255);
        let e = i64::from_sql(&text, b"12").unwrap_err().to_string();
        assert!(e.contains("VARCHAR cannot be read as i64"), "{e}");
    }

    #[test]
    fn strings_and_bytes_are_told_apart() {
        let text = col(ColumnType::BLOB, 16, 255);
        let binary = col(ColumnType::BLOB, 16 | 128, 63);
        assert_eq!(<&str>::from_sql(&text, b"hi").unwrap(), "hi");
        assert_eq!(<&[u8]>::from_sql(&text, b"hi").unwrap(), b"hi");
        let e = <&str>::from_sql(&binary, b"hi").unwrap_err().to_string();
        assert!(e.contains("BLOB cannot be read as &str"), "{e}");
        assert!(e.contains("binary data"), "{e}");
        assert_eq!(Vec::<u8>::from_sql(&binary, b"\0\xFF").unwrap(), b"\0\xFF");
        assert!(<&str>::from_sql(&text, b"\xFF").is_err());
        let bit = col(ColumnType::BIT, UNSIGNED, 63);
        assert_eq!(Bits::from_sql(&bit, &[1, 2]).unwrap(), Bits(258));
        assert!(i64::from_sql(&bit, &[1]).is_err());
    }

    #[test]
    fn times_round_trip_through_the_calendar() {
        for days in [-719_162, -1, 0, 1, 10_957, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_404), (2025, 11, 12));

        let datetime = col(ColumnType::DATETIME, 0, 63);
        for t in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::from_micros(1_762_955_000_123_456),
            UNIX_EPOCH - Duration::from_micros(86_400_000_001),
        ] {
            let mut out = Vec::new();
            assert_eq!(t.to_sql(&mut out).unwrap().ty, ColumnType::DATETIME);
            assert_eq!(out[0], 11);
            assert_eq!(SystemTime::from_sql(&datetime, &out[1..]).unwrap(), t);
        }
        let mut out = Vec::new();
        (UNIX_EPOCH + Duration::from_micros(1_762_955_000_123_456))
            .to_sql(&mut out)
            .unwrap();
        assert_eq!(
            String::from_sql(&datetime, &out[1..]).unwrap(),
            "2025-11-12 13:43:20.123456"
        );
        let date = col(ColumnType::DATE, 0, 63);
        assert_eq!(
            String::from_sql(&date, &[0xEA, 0x07, 1, 31]).unwrap(),
            "2026-01-31"
        );
        assert_eq!(String::from_sql(&date, &[]).unwrap(), "0000-00-00");
        assert!(SystemTime::from_sql(&datetime, &[]).is_err());
        let time = col(ColumnType::TIME, 0, 63);
        let raw = [1, 34, 0, 0, 0, 22, 59, 59];
        assert_eq!(String::from_sql(&time, &raw).unwrap(), "-838:59:59");
        // More days than a TIME holds does not overflow.
        let huge = [0, 0xFF, 0xFF, 0xFF, 0xFF, 23, 0, 0];
        assert_eq!(
            String::from_sql(&time, &huge).unwrap(),
            "103079215103:00:00"
        );
        let far = UNIX_EPOCH + Duration::from_secs(400_000_000_000);
        assert!(far.to_sql(&mut Vec::new()).is_err());
    }

    #[test]
    fn parameters_say_their_type() {
        let mut out = Vec::new();
        assert_eq!(
            (-2i16).to_sql(&mut out).unwrap(),
            Encoded::signed(ColumnType::SHORT)
        );
        assert_eq!(
            7u32.to_sql(&mut out).unwrap(),
            Encoded::unsigned(ColumnType::LONG)
        );
        assert_eq!("ab".to_sql(&mut out).unwrap().ty, ColumnType::VAR_STRING);
        assert_eq!(None::<i32>.to_sql(&mut out).unwrap(), Encoded::NULL);
        assert_eq!(out, [0xFE, 0xFF, 7, 0, 0, 0, 2, b'a', b'b']);
    }

    #[test]
    fn a_time_with_any_day_count_is_shown_not_overflowed() {
        let time = col(ColumnType::TIME, 0, 63);
        // 0xffff_ffff days: more than u32 hours can hold (found by fuzzing).
        let raw = [0, 0xff, 0xff, 0xff, 0xff, 23, 59, 58];
        assert_eq!(String::from_sql(&time, &raw).unwrap(), "103079215103:59:58");
        let raw = [1, 34, 0, 0, 0, 22, 59, 59];
        assert_eq!(String::from_sql(&time, &raw).unwrap(), "-838:59:59");
    }
}
