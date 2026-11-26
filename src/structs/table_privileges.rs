//! The privileges one role holds on one table or view, per command and per
//! column, as PostgreSQL's `has_table_privilege`, `has_column_privilege` and
//! `has_any_column_privilege` answer them.

use alloc::{vec, vec::Vec};

use sqlparser::ast::Action;

use crate::{
    traits::{DatabaseLike, GrantLike},
    utils::identifier_resolution::identifiers_match,
};

/// Where a role holds one of the privileges a column can carry, `SELECT`,
/// `INSERT`, `UPDATE` or `REFERENCES`.
///
/// PostgreSQL keeps the relation's entry apart from each column's. A
/// relation-level privilege holds on every column, including one added later,
/// while a column grant holds on the columns it names and never on the
/// relation.
///
/// `C` names a column. A table answers with its column objects, and a view,
/// whose output columns are no objects of their own, with their positions in
/// output order counted from zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnPrivilege<C> {
    /// The relation-level privilege holds, and with it every column.
    Table,
    /// Column grants alone give the privilege, on these columns in column
    /// order. None means the role does not hold it at all.
    Columns(Vec<C>),
}

impl<C: PartialEq> ColumnPrivilege<C> {
    /// Returns whether the privilege holds on the relation, as
    /// `has_table_privilege` answers.
    #[must_use]
    pub fn on_table(&self) -> bool {
        matches!(self, Self::Table)
    }

    /// Returns whether the privilege holds on `column`, as
    /// `has_column_privilege` answers.
    #[must_use]
    pub fn on_column(&self, column: C) -> bool
    where
        C: Copy,
    {
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
/// [`TableLike::privileges`](crate::traits::TableLike::privileges) answers,
/// or on a view, as
/// [`ViewLike::privileges`](crate::traits::ViewLike::privileges) answers.
///
/// `SELECT`, `INSERT`, `UPDATE` and `REFERENCES` can be granted per column, so
/// each says where it holds. `DELETE`, `TRUNCATE`, `TRIGGER` and `MAINTAIN`
/// exist only on the whole relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePrivileges<C> {
    /// `SELECT`.
    pub select: ColumnPrivilege<C>,
    /// `INSERT`.
    pub insert: ColumnPrivilege<C>,
    /// `UPDATE`.
    pub update: ColumnPrivilege<C>,
    /// `REFERENCES`.
    pub references: ColumnPrivilege<C>,
    /// `DELETE`.
    pub delete: bool,
    /// `TRUNCATE`.
    pub truncate: bool,
    /// `TRIGGER`.
    pub trigger: bool,
    /// `MAINTAIN`.
    pub maintain: bool,
}

impl<C> TablePrivileges<C> {
    /// Every table privilege, held on the relation and so on every column, as
    /// a superuser holds them.
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

const TABLE_PRIVILEGES: usize = 8;

const COLUMN_PRIVILEGES: usize = 4;

/// The slot of a table privilege, `SELECT`, `INSERT`, `UPDATE` and
/// `REFERENCES` first, then `DELETE`, `TRUNCATE`, `TRIGGER` and `MAINTAIN`,
/// or `None` for an action that is no table privilege.
fn privilege_slot(action: &Action) -> Option<usize> {
    match action {
        Action::Select { .. } => Some(0),
        Action::Insert { .. } => Some(1),
        Action::Update { .. } => Some(2),
        Action::References { .. } => Some(3),
        Action::Delete => Some(4),
        Action::Truncate => Some(5),
        Action::Trigger => Some(6),
        Action::Maintain => Some(7),
        _ => None,
    }
}

/// The table privileges one role has been found to hold on a table or view,
/// at the relation level and on each column by position.
pub(crate) struct HeldPrivileges<'names> {
    /// The name of each column, with its quote state, in column order.
    names: &'names [(&'names str, bool)],
    table: [bool; TABLE_PRIVILEGES],
    columns: [Vec<bool>; COLUMN_PRIVILEGES],
}

impl<'names> HeldPrivileges<'names> {
    /// Nothing held yet on a relation whose columns are named `names`.
    pub(crate) fn new(names: &'names [(&'names str, bool)]) -> Self {
        Self {
            names,
            table: [false; TABLE_PRIVILEGES],
            columns: core::array::from_fn(|_| vec![false; names.len()]),
        }
    }

    /// Records what `grant` gives, when it reaches one of `holders`, or
    /// `PUBLIC`. `covered` marks the columns a column grant covers, as
    /// [`take`](Self::take) reads it.
    pub(crate) fn take_grant<G: GrantLike>(
        &mut self,
        grant: &G,
        holders: &[&<G::DB as DatabaseLike>::Role],
        covered: Option<&[bool]>,
        database: &G::DB,
    ) {
        if !(grant.applies_to_public()
            || holders.iter().any(|holder| grant.applies_to_role(holder)))
        {
            return;
        }
        if grant.is_all_privileges() {
            match covered {
                None => self.table = [true; TABLE_PRIVILEGES],
                Some(covered) => self.columns.iter_mut().for_each(|held| mark(held, covered)),
            }
            return;
        }
        for privilege in grant.privileges(database) {
            self.take(privilege, covered);
        }
    }

    /// Records `privilege` on the columns it lists. A privilege listing no
    /// columns holds on the relation for a relation-level entry, and on the
    /// columns `covered` marks for a column grant, which never holds on the
    /// relation and carries no privilege a column cannot.
    pub(crate) fn take(&mut self, privilege: &Action, covered: Option<&[bool]>) {
        let Some(slot) = privilege_slot(privilege) else {
            return;
        };
        let listed = match privilege {
            Action::Select { columns: listed }
            | Action::Insert { columns: listed }
            | Action::Update { columns: listed }
            | Action::References { columns: listed } => listed.as_deref(),
            _ => None,
        };
        match (listed, covered) {
            (None, None) => self.table[slot] = true,
            (None, Some(covered)) => {
                if let Some(held) = self.columns.get_mut(slot) {
                    mark(held, covered);
                }
            }
            (Some(listed), _) => {
                for (held, (name, quoted)) in self.columns[slot].iter_mut().zip(self.names) {
                    *held |= listed.iter().any(|ident| {
                        identifiers_match(&ident.value, ident.quote_style.is_some(), name, *quoted)
                    });
                }
            }
        }
    }

    /// The answer, naming the column at each position by `columns`, which
    /// lists them in the order of the names this was built over.
    pub(crate) fn answer<C: Copy>(self, columns: &[C]) -> TablePrivileges<C> {
        let [select, insert, update, references] = self.columns;
        let column_privilege = |slot: usize, held: Vec<bool>| {
            if self.table[slot] {
                ColumnPrivilege::Table
            } else {
                ColumnPrivilege::Columns(
                    columns
                        .iter()
                        .zip(held)
                        .filter(|(_, held)| *held)
                        .map(|(column, _)| *column)
                        .collect(),
                )
            }
        };
        TablePrivileges {
            select: column_privilege(0, select),
            insert: column_privilege(1, insert),
            update: column_privilege(2, update),
            references: column_privilege(3, references),
            delete: self.table[4],
            truncate: self.table[5],
            trigger: self.table[6],
            maintain: self.table[7],
        }
    }
}

/// Marks as held each column `covered` marks.
fn mark(held: &mut [bool], covered: &[bool]) {
    for (held, covered) in held.iter_mut().zip(covered) {
        *held |= covered;
    }
}
