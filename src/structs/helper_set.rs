//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for one caller
//! and one argument list, as
//! [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
//! answers it.

use alloc::{boxed::Box, vec::Vec};

use sqlparser::ast::Value;

use crate::{structs::SessionSetting, traits::DatabaseLike};

/// The set a helper returns, the union of its arms.
///
/// An arm whose condition keeps no row for the arguments is left out, so a
/// helper returning nothing for them answers no arm.
#[derive(Debug)]
pub struct HelperSet<'db, DB: DatabaseLike> {
    pub(crate) arms: Vec<HelperArm<'db, DB>>,
    pub(crate) covered_table: Option<&'db DB::Table>,
}

impl<'db, DB: DatabaseLike> HelperSet<'db, DB> {
    /// The arms, in the order the body writes them.
    #[must_use]
    pub fn arms(&self) -> &[HelperArm<'db, DB>] {
        &self.arms
    }

    /// The one table whose primary key every arm the body writes returns,
    /// as its own primary key or through a single-column foreign key
    /// referencing it, so that every value of the set is a key of that
    /// table.
    ///
    /// `None` where two arms return keys of different tables, or an arm
    /// returns a column that is neither. Arms left out for the arguments
    /// count, so the answer does not depend on them.
    #[must_use]
    pub fn covered_table(&self) -> Option<&'db DB::Table> {
        self.covered_table
    }
}

/// One `SELECT` of a helper's `UNION`.
#[derive(Debug)]
pub enum HelperArm<'db, DB: DatabaseLike> {
    /// The rows of a grant table naming the caller.
    Grant(GrantArm<'db, DB>),
    /// The rows of a table whose parent another helper returns.
    Parent(ParentArm<'db, DB>),
}

/// `SELECT <column> FROM <table> WHERE <caller column> =
/// current_setting(<caller setting>) [AND <condition>]`.
#[derive(Debug)]
pub struct GrantArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) column: &'db DB::Column,
    pub(crate) caller_column: &'db DB::Column,
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
    pub(crate) restriction: Option<LiteralSet<'db, DB>>,
}

impl<'db, DB: DatabaseLike> GrantArm<'db, DB> {
    /// The table the arm reads.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The column the arm returns.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The column holding the caller.
    #[must_use]
    pub fn caller_column(&self) -> &'db DB::Column {
        self.caller_column
    }

    /// The setting holding the caller, as the deployment declares it.
    #[must_use]
    pub fn setting(&self) -> &'db SessionSetting {
        self.setting
    }

    /// Whether the body reads the setting with `current_setting(name)`, which
    /// raises `42704` where the setting was never set, rather than with
    /// `current_setting(name, true)`, which reads `NULL` and keeps no row.
    #[must_use]
    pub fn raises_when_unset(&self) -> bool {
        self.raises_when_unset
    }

    /// The values one column has to hold, or `None` where the condition is
    /// absent or holds for every row.
    #[must_use]
    pub fn restriction(&self) -> Option<&LiteralSet<'db, DB>> {
        self.restriction.as_ref()
    }
}

/// `<column> IN (<values>)`.
#[derive(Debug)]
pub struct LiteralSet<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) values: Vec<Value>,
}

impl<'db, DB: DatabaseLike> LiteralSet<'db, DB> {
    /// The column.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The values, each once, in the order the condition first names them.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }
}

/// `SELECT <key> FROM <table> WHERE <column> IN (SELECT
/// <function>(<arguments>))`, the column naming a row of the parent table.
///
/// The column names the parent through its single-column foreign key, or,
/// where it has none, through the set it is compared to, which then covers
/// the parent table (see [`HelperSet::covered_table`]). A column whose
/// parent may be one of several tables is read by one arm per table.
#[derive(Debug)]
pub struct ParentArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) column: &'db DB::Column,
    pub(crate) foreign_key: Option<&'db DB::ForeignKey>,
    pub(crate) parent_table: &'db DB::Table,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Box<HelperSet<'db, DB>>,
}

impl<'db, DB: DatabaseLike> ParentArm<'db, DB> {
    /// The table the arm reads.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The table's primary key, which the arm returns.
    #[must_use]
    pub fn key(&self) -> &'db DB::Column {
        self.key
    }

    /// The column naming the parent.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The single-column foreign key of [`column`](Self::column), or `None`
    /// where the column has none and [`set`](Self::set) names the parent
    /// table.
    #[must_use]
    pub fn foreign_key(&self) -> Option<&'db DB::ForeignKey> {
        self.foreign_key
    }

    /// The parent table, the one [`foreign_key`](Self::foreign_key)
    /// references or, without one, the one [`set`](Self::set) covers.
    #[must_use]
    pub fn parent_table(&self) -> &'db DB::Table {
        self.parent_table
    }

    /// The helper returning the parents.
    #[must_use]
    pub fn function(&self) -> &'db DB::Function {
        self.function
    }

    /// The arguments the arm passes to [`function`](Self::function), the
    /// helper's own arguments substituted.
    #[must_use]
    pub fn arguments(&self) -> &[Value] {
        &self.arguments
    }

    /// What [`function`](Self::function) returns for
    /// [`arguments`](Self::arguments).
    #[must_use]
    pub fn set(&self) -> &HelperSet<'db, DB> {
        &self.set
    }
}
