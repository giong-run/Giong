//! Rows and columns of a result.

use std::fmt;
use std::rc::Rc;

use crate::error::{Error, ErrorKind};
use crate::protocol;
use crate::types::{Format, FromSql, Type};

/// A column of a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    name: Box<str>,
    ty: Type,
}

impl Column {
    pub(crate) fn new(name: &str, ty: Type) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }

    /// The column's name, as the SQL names it (`AS` included).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column's type.
    pub fn ty(&self) -> Type {
        self.ty
    }

    /// The format this crate asks the server to send the column in.
    pub(crate) fn format(&self) -> Format {
        if self.ty.binary_result() {
            Format::Binary
        } else {
            Format::Text
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
    rows: usize,
    pub(crate) overflow: bool,
}

impl RowsBuilder {
    /// Adds the row in a `DataRow` body, or notes that the result outgrew
    /// `limit` bytes and drops what was collected.
    pub(crate) fn push(&mut self, body: &[u8], columns: usize, limit: usize) -> Result<(), Error> {
        if self.overflow {
            return protocol::data_row(body, columns, |_| {});
        }
        let base = self.data.len();
        let size = base + body.len() + (self.values.len() + columns) * 8;
        if size > limit || u32::try_from(base + body.len()).is_err() {
            protocol::data_row(body, columns, |_| {})?;
            self.set_overflow();
            return Ok(());
        }
        let values = &mut self.values;
        protocol::data_row(body, columns, |v| {
            values.push(match v {
                Some(r) => ((base + r.start) as u32, r.len() as i32),
                None => (0, -1),
            })
        })?;
        self.data.extend_from_slice(body);
        self.rows += 1;
        Ok(())
    }

    pub(crate) fn set_overflow(&mut self) {
        self.overflow = true;
        self.values = Vec::new();
        self.data = Vec::new();
        self.rows = 0;
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
    /// # async fn f(client: &giong_postgres::Client) -> Result<(), giong_postgres::Error> {
    /// let row = client.query_one("SELECT id, email FROM users WHERE id = $1", &[&7i64]).await?;
    /// let id: i64 = row.get("id")?;
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
            T::from_null(col.ty)
        } else {
            let start = start as usize;
            T::from_sql(
                col.ty,
                col.format(),
                &self.set.data[start..start + len as usize],
            )
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
    fn from_sql(_: Type, _: Format, _: &[u8]) -> Result<Self, Error> {
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
            } else if col.format() == Format::Text {
                let start = start as usize;
                let raw = &self.set.data[start..start + len as usize];
                map.entry(&col.name(), &String::from_utf8_lossy(raw));
            } else {
                map.entry(
                    &col.name(),
                    &format_args!("<{} bytes of {}>", len, col.ty()),
                );
            }
        }
        map.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_body(values: &[Option<&[u8]>]) -> Vec<u8> {
        let mut b = (values.len() as i16).to_be_bytes().to_vec();
        for v in values {
            match v {
                Some(v) => {
                    b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    b.extend_from_slice(v);
                }
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        b
    }

    fn columns() -> Rc<[Column]> {
        vec![
            Column::new("id", Type::INT4),
            Column::new("name", Type::TEXT),
        ]
        .into()
    }

    #[test]
    fn rows_share_one_buffer_and_read_by_name_or_position() {
        let mut b = RowsBuilder::default();
        b.push(
            &row_body(&[Some(&7i32.to_be_bytes()), Some(b"ana")]),
            2,
            1 << 20,
        )
        .unwrap();
        b.push(&row_body(&[Some(&8i32.to_be_bytes()), None]), 2, 1 << 20)
            .unwrap();
        let rows = b.finish(columns()).into_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<i32>("id").unwrap(), 7);
        assert_eq!(rows[0].get::<&str>(1).unwrap(), "ana");
        assert_eq!(rows[1].get::<i64>(0).unwrap(), 8);
        assert_eq!(rows[1].get::<Option<String>>("name").unwrap(), None);
        assert!(rows[1].is_null("name").unwrap());
        assert!(!rows[1].is_null("id").unwrap());

        let e = rows[1].get::<String>("name").unwrap_err();
        assert!(
            e.to_string()
                .starts_with("GIONG-E3005: column \"name\": the value is NULL"),
            "{e}"
        );
        let e = rows[0].get::<String>("id").unwrap_err();
        assert!(e.to_string().contains("read it as i16, i32 or i64"), "{e}");
        let e = rows[0].get::<i32>("nope").unwrap_err();
        assert!(e.to_string().contains("[\"id\", \"name\"]"), "{e}");
        assert!(rows[0].get::<i32>(5).is_err());
        assert_eq!(
            format!("{:?}", rows[0]),
            "{\"id\": <4 bytes of integer>, \"name\": \"ana\"}"
        );
    }

    #[test]
    fn a_result_over_the_limit_is_dropped() {
        let mut b = RowsBuilder::default();
        let body = row_body(&[Some(&7i32.to_be_bytes()), Some(&[b'x'; 100])]);
        b.push(&body, 2, 250).unwrap();
        assert!(!b.overflow);
        b.push(&body, 2, 250).unwrap();
        assert!(b.overflow);
        b.push(&body, 2, 250).unwrap();
        assert!(b.finish(columns()).into_rows().is_empty());
        // Malformed rows are still caught after the overflow.
        let mut b = RowsBuilder::default();
        b.set_overflow();
        assert!(b.push(&[0, 3], 2, 300).is_err());
    }
}
