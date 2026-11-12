//! Rows and columns of a result.

use std::fmt;
use std::rc::Rc;

use crate::error::{Error, ErrorKind};
use crate::protocol::{ColumnDef, Reader, BINARY_COLLATION};
use crate::types::{ColumnType, FromSql};

const UNSIGNED_FLAG: u16 = 32;

/// A column of a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    name: Box<str>,
    ty: ColumnType,
    flags: u16,
    collation: u16,
}

impl Column {
    pub(crate) fn from_def(def: ColumnDef) -> Self {
        Self {
            name: def.name,
            ty: def.ty,
            flags: def.flags,
            collation: def.collation,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(name: &str, ty: ColumnType, flags: u16, collation: u16) -> Self {
        Self {
            name: name.into(),
            ty,
            flags,
            collation,
        }
    }

    /// The column's name, as the SQL names it (`AS` included).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column's type.
    pub fn ty(&self) -> ColumnType {
        self.ty
    }

    /// Whether the column is `UNSIGNED`.
    pub fn is_unsigned(&self) -> bool {
        self.flags & UNSIGNED_FLAG != 0
    }

    /// Whether the column holds bytes rather than text: `BINARY`,
    /// `VARBINARY`, `BLOB`, and the results of functions on them.
    pub fn is_binary(&self) -> bool {
        self.collation == BINARY_COLLATION
    }

    /// The type as SQL would name it, such as `BIGINT UNSIGNED`,
    /// `VARBINARY` or `TEXT`.
    pub fn type_name(&self) -> String {
        let base = match (self.ty, self.is_binary()) {
            (ColumnType::VARCHAR | ColumnType::VAR_STRING, true) => "VARBINARY".to_owned(),
            (ColumnType::STRING, true) => "BINARY".to_owned(),
            (ColumnType::TINY_BLOB, false) => "TINYTEXT".to_owned(),
            (ColumnType::BLOB, false) => "TEXT".to_owned(),
            (ColumnType::MEDIUM_BLOB, false) => "MEDIUMTEXT".to_owned(),
            (ColumnType::LONG_BLOB, false) => "LONGTEXT".to_owned(),
            (ty, _) => ty.to_string(),
        };
        if self.ty.is_integer() && self.ty != ColumnType::YEAR && self.is_unsigned() {
            format!("{base} UNSIGNED")
        } else {
            base
        }
    }
}

/// All the rows of one result, in one allocation.
pub(crate) struct ResultSet {
    columns: Rc<[Column]>,
    /// For each value, row by row: where it starts in `data` and its length,
    /// or -1 for `NULL`.
    values: Vec<(u32, i32)>,
    data: Vec<u8>,
}

/// Collects rows as they arrive, up to a size limit.
#[derive(Default)]
pub(crate) struct RowsBuilder {
    values: Vec<(u32, i32)>,
    data: Vec<u8>,
    pub(crate) overflow: bool,
}

/// Finds the values in a binary row (`ProtocolBinary::ResultsetRow`):
/// calls `each` with the range of each value's bytes in `payload`, or `None`
/// for `NULL`.
pub(crate) fn binary_row(
    payload: &[u8],
    columns: &[Column],
    mut each: impl FnMut(Option<std::ops::Range<usize>>),
) -> Result<(), Error> {
    let mut r = Reader::new(payload);
    if r.u8("a row")? != 0 {
        return Err(Error::protocol(
            "a row of a binary result did not start with 0x00",
        ));
    }
    let bitmap = r.take((columns.len() + 7 + 2) / 8, "a row")?;
    let base = payload.len();
    for (i, column) in columns.iter().enumerate() {
        let bit = i + 2;
        if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
            each(None);
            continue;
        }
        let value = match column.ty.fixed_size() {
            Some(n) => r.take(n, "a row")?,
            None if column.ty.is_temporal() => {
                let n = r.u8("a row")?;
                r.take(usize::from(n), "a row")?
            }
            None => r.lenenc_bytes("a row")?,
        };
        let start = base - r.rest().len() - value.len();
        each(Some(start..start + value.len()));
    }
    if !r.is_empty() {
        return Err(Error::protocol(format!(
            "a row of {} columns had {} bytes left over",
            columns.len(),
            r.rest().len()
        )));
    }
    Ok(())
}

impl RowsBuilder {
    /// Adds a binary row, or notes that the result outgrew `limit` bytes and
    /// drops what was collected.
    pub(crate) fn push(
        &mut self,
        payload: &[u8],
        columns: &[Column],
        limit: usize,
    ) -> Result<(), Error> {
        if self.overflow {
            return binary_row(payload, columns, |_| {});
        }
        let base = self.data.len();
        let size = base + payload.len() + (self.values.len() + columns.len()) * 8;
        if size > limit || u32::try_from(base + payload.len()).is_err() {
            binary_row(payload, columns, |_| {})?;
            self.set_overflow();
            return Ok(());
        }
        let values = &mut self.values;
        let before = values.len();
        let parsed = binary_row(payload, columns, |v| {
            values.push(match v {
                Some(r) => ((base + r.start) as u32, r.len() as i32),
                None => (0, -1),
            })
        });
        if let Err(e) = parsed {
            values.truncate(before);
            return Err(e);
        }
        self.data.extend_from_slice(payload);
        Ok(())
    }

    pub(crate) fn set_overflow(&mut self) {
        self.overflow = true;
        self.values = Vec::new();
        self.data = Vec::new();
    }

    pub(crate) fn finish(self, columns: Rc<[Column]>) -> ResultSet {
        ResultSet {
            columns,
            values: self.values,
            data: self.data,
        }
    }
}

impl ResultSet {
    pub(crate) fn into_rows(self) -> Vec<Row> {
        let count = if self.columns.is_empty() {
            0
        } else {
            self.values.len() / self.columns.len()
        };
        let set = Rc::new(self);
        (0..count)
            .map(|index| Row {
                set: set.clone(),
                index,
            })
            .collect()
    }
}

/// A column position or name, for [`Row::get`].
pub trait RowIndex: sealed::Sealed {
    #[doc(hidden)]
    fn position(&self, columns: &[Column]) -> Option<usize>;
    #[doc(hidden)]
    fn describe(&self) -> String;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for usize {}
    impl Sealed for &str {}
}

impl RowIndex for usize {
    fn position(&self, columns: &[Column]) -> Option<usize> {
        (*self < columns.len()).then_some(*self)
    }

    fn describe(&self) -> String {
        format!("number {self}")
    }
}

impl RowIndex for &str {
    fn position(&self, columns: &[Column]) -> Option<usize> {
        columns.iter().position(|c| &*c.name == *self)
    }

    fn describe(&self) -> String {
        format!("{self:?}")
    }
}

/// A row of a result.
///
/// The rows of one result share one buffer, so a `Row` is cheap to keep and
/// to clone.
#[derive(Clone)]
pub struct Row {
    set: Rc<ResultSet>,
    index: usize,
}

impl Row {
    /// Reads a column, by name or by position (from 0), as `T`.
    ///
    /// ```no_run
    /// # async fn f(client: &giong_mysql::Client) -> Result<(), giong_mysql::Error> {
    /// let row = client.query_one("SELECT id, email FROM users WHERE id = ?", &[&7i64]).await?;
    /// let id: u64 = row.get("id")?;
    /// let email: &str = row.get(1)?;
    /// # Ok(()) }
    /// ```
    ///
    /// The error names the column and says what it can be read as when the
    /// type does not fit, or that the value is `NULL` and needs an `Option`.
    pub fn get<'a, T: FromSql<'a>>(&'a self, column: impl RowIndex) -> Result<T, Error> {
        let columns = &self.set.columns;
        let i = column.position(columns).ok_or_else(|| {
            let names: Vec<&str> = columns.iter().map(|c| c.name()).collect();
            Error::new(
                ErrorKind::Type,
                format!(
                    "there is no column {} in this row; its columns are {names:?}",
                    column.describe()
                ),
            )
        })?;
        let col = &columns[i];
        let (start, len) = self.set.values[self.index * columns.len() + i];
        let value = if len < 0 {
            T::from_null(col)
        } else {
            let start = start as usize;
            T::from_sql(col, &self.set.data[start..start + len as usize])
        };
        value.map_err(|e| e.context(&format!("column {:?}", col.name())))
    }

    /// Whether the column holds `NULL`.
    pub fn is_null(&self, column: impl RowIndex) -> Result<bool, Error> {
        self.get::<Option<Ignored>>(column).map(|v| v.is_none())
    }

    /// The columns of the result.
    pub fn columns(&self) -> &[Column] {
        &self.set.columns
    }

    /// The number of columns.
    pub fn len(&self) -> usize {
        self.set.columns.len()
    }

    /// Whether the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.set.columns.is_empty()
    }
}

/// Accepts any value, for [`Row::is_null`].
struct Ignored;

impl FromSql<'_> for Ignored {
    fn from_sql(_: &Column, _: &[u8]) -> Result<Self, Error> {
        Ok(Ignored)
    }
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (i, col) in self.set.columns.iter().enumerate() {
            let (start, len) = self.set.values[self.index * self.set.columns.len() + i];
            if len < 0 {
                map.entry(&col.name(), &"NULL");
                continue;
            }
            let start = start as usize;
            let raw = &self.set.data[start..start + len as usize];
            if let Ok(v) = i128_of(col, raw) {
                map.entry(&col.name(), &v);
            } else if let Ok(s) = String::from_sql(col, raw) {
                map.entry(&col.name(), &s);
            } else {
                map.entry(
                    &col.name(),
                    &format_args!("<{} bytes of {}>", len, col.type_name()),
                );
            }
        }
        map.finish()
    }
}

/// An integer column's value, for `Debug`.
fn i128_of(col: &Column, raw: &[u8]) -> Result<i128, Error> {
    if !col.ty().is_integer() {
        return Err(Error::conversion("not an integer"));
    }
    if col.is_unsigned() {
        u64::from_sql(col, raw).map(i128::from)
    } else {
        i64::from_sql(col, raw).map(i128::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Rc<[Column]> {
        vec![
            Column::for_tests("id", ColumnType::LONGLONG, UNSIGNED_FLAG, 63),
            Column::for_tests("name", ColumnType::VAR_STRING, 0, 255),
            Column::for_tests("at", ColumnType::DATETIME, 0, 63),
        ]
        .into()
    }

    /// A binary row with the id, the name (or NULL) and a date.
    fn row(id: u64, name: Option<&str>) -> Vec<u8> {
        let mut p = vec![0, if name.is_none() { 1 << 3 } else { 0 }];
        p.extend_from_slice(&id.to_le_bytes());
        if let Some(n) = name {
            crate::protocol::put_lenenc_bytes(&mut p, n.as_bytes());
        }
        p.extend_from_slice(&[4, 0xEA, 0x07, 1, 31]);
        p
    }

    #[test]
    fn rows_share_one_buffer_and_read_by_name_or_position() {
        let cols = columns();
        let mut b = RowsBuilder::default();
        b.push(&row(7, Some("ana")), &cols, 1 << 20).unwrap();
        b.push(&row(8, None), &cols, 1 << 20).unwrap();
        let rows = b.finish(cols).into_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<u64>("id").unwrap(), 7);
        assert_eq!(rows[0].get::<&str>(1).unwrap(), "ana");
        // A DATETIME at midnight comes without its time.
        assert_eq!(rows[0].get::<String>("at").unwrap(), "2026-01-31 00:00:00");
        assert_eq!(rows[1].get::<i32>(0).unwrap(), 8);
        assert_eq!(rows[1].get::<Option<String>>("name").unwrap(), None);
        assert!(rows[1].is_null("name").unwrap());
        assert!(!rows[1].is_null("id").unwrap());

        let e = rows[1].get::<String>("name").unwrap_err();
        assert!(
            e.to_string()
                .starts_with("GIONG-E3105: column \"name\": the value is NULL"),
            "{e}"
        );
        let e = rows[0].get::<String>("id").unwrap_err();
        assert!(
            e.to_string().contains("BIGINT UNSIGNED cannot be read"),
            "{e}"
        );
        let e = rows[0].get::<i32>("nope").unwrap_err();
        assert!(e.to_string().contains("[\"id\", \"name\", \"at\"]"), "{e}");
        assert!(rows[0].get::<i32>(5).is_err());
        assert_eq!(
            format!("{:?}", rows[0]),
            "{\"id\": 7, \"name\": \"ana\", \"at\": \"2026-01-31 00:00:00\"}"
        );
    }

    #[test]
    fn malformed_rows_are_refused() {
        let cols = columns();
        let mut b = RowsBuilder::default();
        let mut long = row(7, Some("ana"));
        long.push(0);
        assert!(b.push(&long, &cols, 1 << 20).is_err());
        let good = row(7, Some("ana"));
        assert!(b.push(&good[..good.len() - 1], &cols, 1 << 20).is_err());
        assert!(b.push(&[1, 0, 0], &cols, 1 << 20).is_err());
        // A failed row leaves nothing behind.
        b.push(&good, &cols, 1 << 20).unwrap();
        assert_eq!(b.finish(cols).into_rows().len(), 1);
    }

    #[test]
    fn a_result_over_the_limit_is_dropped() {
        let cols = columns();
        let mut b = RowsBuilder::default();
        // 116 bytes, and 24 for where its three values are.
        let body = row(7, Some(&"x".repeat(100)));
        assert_eq!(body.len(), 116);
        b.push(&body, &cols, 250).unwrap();
        assert!(!b.overflow);
        b.push(&body, &cols, 250).unwrap();
        assert!(b.overflow);
        b.push(&body, &cols, 250).unwrap();
        assert!(b.finish(cols.clone()).into_rows().is_empty());
        let mut b = RowsBuilder::default();
        b.set_overflow();
        assert!(b.push(&[0, 0], &cols, 300).is_err());
    }
}
