//! The privileges one role holds on one table, per command and per column, as
//! PostgreSQL's `has_table_privilege`, `has_column_privilege` and
//! `has_any_column_privilege` answer them.

use alloc::vec::Vec;

/// Where a role holds one of the privileges a column can carry, `SELECT`,
/// `INSERT`, `UPDATE` or `REFERENCES`.
///
/// PostgreSQL keeps the table's entry apart from each column's. A table-level
/// privilege holds on every column, including one added later, while a column
/// grant holds on the columns it names and never on the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnPrivilege<'db, C> {
    /// The table-level privilege holds, and with it every column.
    Table,
    /// Column grants alone give the privilege, on these columns in table
    /// order. None means the role does not hold it at all.
    Columns(Vec<&'db C>),
}

impl<C: PartialEq> ColumnPrivilege<'_, C> {
    /// Returns whether the privilege holds on the table, as
    /// `has_table_privilege` answers.
    #[must_use]
    pub fn on_table(&self) -> bool {
        matches!(self, Self::Table)
    }

    /// Returns whether the privilege holds on `column`, as
    /// `has_column_privilege` answers.
    #[must_use]
    pub fn on_column(&self, column: &C) -> bool {
        match self {
            Self::Table => true,
            Self::Columns(columns) => columns.contains(&column),
        }
    }

    /// Returns whether the privilege holds on at least one column, as
    /// `has_any_column_privilege` answers.
    #[must_use]
    pub fn on_any_column(&self) -> bool {
        match self {
            Self::Table => true,
            Self::Columns(columns) => !columns.is_empty(),
        }
    }
}

/// The privileges a role holds on a table, as
/// [`TableLike::privileges`](crate::traits::TableLike::privileges) answers.
///
/// `SELECT`, `INSERT`, `UPDATE` and `REFERENCES` can be granted per column, so
/// each says where it holds. `DELETE`, `TRUNCATE`, `TRIGGER` and `MAINTAIN`
/// exist only on the whole table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePrivileges<'db, C> {
    /// `SELECT`.
    pub select: ColumnPrivilege<'db, C>,
    /// `INSERT`.
    pub insert: ColumnPrivilege<'db, C>,
    /// `UPDATE`.
    pub update: ColumnPrivilege<'db, C>,
    /// `REFERENCES`.
    pub references: ColumnPrivilege<'db, C>,
    /// `DELETE`.
    pub delete: bool,
    /// `TRUNCATE`.
    pub truncate: bool,
    /// `TRIGGER`.
    pub trigger: bool,
    /// `MAINTAIN`.
    pub maintain: bool,
}

impl<C> TablePrivileges<'_, C> {
    /// Every table privilege, held on the table and so on every column, as a
    /// superuser holds them.
    pub(crate) fn all() -> Self {
        Self {
            select: ColumnPrivilege::Table,
            insert: ColumnPrivilege::Table,
            update: ColumnPrivilege::Table,
            references: ColumnPrivilege::Table,
            delete: true,
            truncate: true,
            trigger: true,
            maintain: true,
        }
    }
}
