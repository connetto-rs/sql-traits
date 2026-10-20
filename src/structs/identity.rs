//! Identities a [`GenericDB`](crate::structs::GenericDB) gives the relations
//! and columns it holds.

/// The identity of a table, view or materialized view, opaque, equal only
/// when both name the same relation, and ordered without meaning.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db =
///     ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT); ALTER TABLE t RENAME TO u;")?;
/// let table = db.tables().next().expect("one table");
/// let identity = db.table_identity(table).expect("the table is held here");
/// assert_eq!(db.table_by_identity(identity).map(TableLike::table_name), Some("u"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationIdentity(u64);

/// The identity of a table column, opaque on the terms of
/// [`RelationIdentity`], with one of its own for a column inherited from a
/// parent table.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE t (a INT, c INT); ALTER TABLE t DROP COLUMN a;",
/// )?;
/// let table = db.tables().next().expect("one table");
/// let column = table.columns(&db)?.next().expect("one column");
/// let identity = db.column_identity(column).expect("the column is held here");
/// assert_eq!(db.column_by_identity(identity).map(ColumnLike::column_name), Some("c"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnIdentity(u64);

/// The counter every identity of one ingestion is drawn from, one serving
/// both kinds so no relation and no column ever share a number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IdentityCounter {
    next: u64,
}

impl IdentityCounter {
    fn draw(&mut self) -> u64 {
        let drawn = self.next;
        self.next += 1;
        drawn
    }

    /// Hands out an identity no relation or column has held before.
    pub(crate) fn relation(&mut self) -> RelationIdentity {
        RelationIdentity(self.draw())
    }

    /// Hands out an identity no relation or column has held before.
    pub(crate) fn column(&mut self) -> ColumnIdentity {
        ColumnIdentity(self.draw())
    }
}
