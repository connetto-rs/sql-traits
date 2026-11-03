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
}

impl<'db, DB: DatabaseLike> HelperSet<'db, DB> {
    /// The arms, in the order the body writes them.
    #[must_use]
    pub fn arms(&self) -> &[HelperArm<'db, DB>] {
        &self.arms
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

/// `SELECT <key> FROM <table> WHERE <foreign key column> IN (SELECT
/// <function>(<arguments>))`.
#[derive(Debug)]
pub struct ParentArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) foreign_key: &'db DB::ForeignKey,
    pub(crate) foreign_key_column: &'db DB::Column,
    pub(crate) referenced_table: &'db DB::Table,
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

    /// The single-column foreign key from the table to its parent.
    #[must_use]
    pub fn foreign_key(&self) -> &'db DB::ForeignKey {
        self.foreign_key
    }

    /// The column of [`foreign_key`](Self::foreign_key).
    #[must_use]
    pub fn foreign_key_column(&self) -> &'db DB::Column {
        self.foreign_key_column
    }

    /// The table [`foreign_key`](Self::foreign_key) references.
    #[must_use]
    pub fn referenced_table(&self) -> &'db DB::Table {
        self.referenced_table
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
