//! One entry of the record SQLite stores for each row of an index.

use sqlparser::ast::Expr;

/// What one entry of a SQLite index record holds, the `cid` and `name` of a
/// `pragma_index_xinfo` row.
#[derive(Debug, PartialEq, Eq)]
pub enum SqliteIndexed<'db, C> {
    /// A column of the table, `cid` its position and `name` its name.
    Column(&'db C),
    /// An expression key, `cid` `-2` and no name. The expression is the
    /// key as written, its `COLLATE` included.
    Expression(&'db Expr),
    /// The rowid of a rowid table, `cid` `-1` and no name.
    Rowid,
}

impl<C> Clone for SqliteIndexed<'_, C> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for SqliteIndexed<'_, C> {}

/// One entry of the record SQLite stores for each row of an index, a row of
/// `pragma_index_xinfo`, with what it holds, its direction, the collation it
/// compares under, and whether it is a key or an auxiliary entry SQLite
/// appends to find the table row.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::SQLiteDialect;
///
/// let db = ParserDB::parse::<SQLiteDialect>(
///     "CREATE TABLE r (label TEXT COLLATE NOCASE);
///      CREATE INDEX plain ON r (label DESC);",
/// )?;
/// let index = db.indexes().next().unwrap();
/// let entries: Vec<_> = index.sqlite_index_columns(&db)?.unwrap().collect();
/// let SqliteIndexed::Column(column) = entries[0].indexed() else { panic!() };
/// assert_eq!(column.column_name(), "label");
/// assert!(entries[0].is_descending());
/// assert_eq!(entries[0].collation(), "NOCASE");
/// assert!(entries[0].is_key());
/// assert_eq!(entries[1].indexed(), SqliteIndexed::Rowid);
/// assert!(!entries[1].is_descending());
/// assert_eq!(entries[1].collation(), "BINARY");
/// assert!(!entries[1].is_key());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct SqliteIndexColumn<'db, C> {
    indexed: SqliteIndexed<'db, C>,
    descending: bool,
    collation: &'db str,
    key: bool,
}

impl<C> Clone for SqliteIndexColumn<'_, C> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for SqliteIndexColumn<'_, C> {}

impl<'db, C> SqliteIndexColumn<'db, C> {
    /// Returns a key entry over `indexed`, sorting in the direction
    /// `descending` gives under the collation `collation`.
    #[must_use]
    #[inline]
    pub const fn key(
        indexed: SqliteIndexed<'db, C>,
        descending: bool,
        collation: &'db str,
    ) -> Self {
        Self { indexed, descending, collation, key: true }
    }

    /// Returns this entry as an auxiliary one, which SQLite appends after the
    /// keys to find the table row and which takes no part in a uniqueness
    /// check.
    #[must_use]
    #[inline]
    pub const fn auxiliary(mut self) -> Self {
        self.key = false;
        self
    }

    /// Returns what the entry holds.
    #[must_use]
    #[inline]
    pub const fn indexed(&self) -> SqliteIndexed<'db, C> {
        self.indexed
    }

    /// Returns whether the entry sorts descending, `desc` of its
    /// `pragma_index_xinfo` row.
    #[must_use]
    #[inline]
    pub const fn is_descending(&self) -> bool {
        self.descending
    }

    /// Returns the name of the collation the entry compares under, spelled
    /// as written, `coll` of its `pragma_index_xinfo` row.
    #[must_use]
    #[inline]
    pub const fn collation(&self) -> &'db str {
        self.collation
    }

    /// Returns whether the entry is a key, `key` of its `pragma_index_xinfo`
    /// row, rather than an auxiliary entry.
    #[must_use]
    #[inline]
    pub const fn is_key(&self) -> bool {
        self.key
    }
}
