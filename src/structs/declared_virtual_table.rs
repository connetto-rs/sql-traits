//! What a live SQLite database reports for a virtual table: the columns its
//! module declares through `sqlite3_declare_vtab` and the shadow tables the
//! module keeps.

use alloc::{string::String, vec::Vec};

/// A column a virtual table's module declares, as `pragma_table_xinfo`
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclaredColumn {
    name: String,
    declared_type: String,
    hidden: bool,
}

impl DeclaredColumn {
    /// Records a column named `name` of the declared type `declared_type`,
    /// empty for an untyped column, hidden from `SELECT *` when `hidden`.
    #[must_use]
    pub fn new(name: impl Into<String>, declared_type: impl Into<String>, hidden: bool) -> Self {
        Self { name: name.into(), declared_type: declared_type.into(), hidden }
    }

    /// Moves out the name, the declared type and the hidden flag.
    pub(crate) fn into_parts(self) -> (String, String, bool) {
        (self.name, self.declared_type, self.hidden)
    }
}

/// The columns of a virtual table, in declaration order, and the suffixes of
/// the shadow tables its module keeps, as a live database reports them.
///
/// A shadow table of the virtual table `t` is the stored table
/// `t_<suffix>` in the same schema, read the way SQLite reads it: split at
/// its last underscore. `pragma_table_list` reports it with `type =
/// 'shadow'`.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
///
/// let create = Parser::parse_sql(&SQLiteDialect {}, "CREATE VIRTUAL TABLE g USING geopoly(a);")
///     .expect("SQL parses")
///     .remove(0);
/// let declared = DeclaredVirtualTable::new(
///     [DeclaredColumn::new("_shape", "", false), DeclaredColumn::new("a", "", false)],
///     ["node", "parent", "rowid"],
/// );
/// let db = ParserDBIngestor::new::<SQLiteDialect>("main".to_owned())
///     .apply_virtual_table(create, declared)?
///     .finish();
/// let g = db.virtual_tables().next().expect("g is virtual");
/// let names: Vec<&str> = g.columns(&db)?.map(|column| column.column_name()).collect();
/// assert_eq!(names, ["_shape", "a"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclaredVirtualTable {
    columns: Vec<DeclaredColumn>,
    shadow_suffixes: Vec<String>,
}

impl DeclaredVirtualTable {
    /// Records the columns of a virtual table, in declaration order, and the
    /// suffixes of the shadow tables its module keeps.
    #[must_use]
    pub fn new(
        columns: impl IntoIterator<Item = DeclaredColumn>,
        shadow_suffixes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            columns: columns.into_iter().collect(),
            shadow_suffixes: shadow_suffixes.into_iter().map(Into::into).collect(),
        }
    }

    /// Moves out the columns and the shadow suffixes.
    pub(crate) fn into_parts(self) -> (Vec<DeclaredColumn>, Vec<String>) {
        (self.columns, self.shadow_suffixes)
    }
}
