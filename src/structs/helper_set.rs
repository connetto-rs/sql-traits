//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for one
//! request and one argument list, as
//! [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
//! answers it.

use alloc::{boxed::Box, vec::Vec};

use sqlparser::ast::Value;

use crate::{structs::SessionSetting, traits::DatabaseLike};

/// The set a helper returns, the union of its arms.
///
/// An arm whose condition keeps no row for the arguments is left out, and a
/// walk with it where no other arm remains, so a helper returning nothing
/// for them answers no arm.
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
    /// The rows of a table its condition keeps, compared with a value of the
    /// request or not.
    Grant(GrantArm<'db, DB>),
    /// The rows of a table whose parent another helper returns.
    Parent(ParentArm<'db, DB>),
    /// The rows of a table whose parent the set itself holds.
    Walk(WalkArm<'db, DB>),
}

/// `SELECT <column> FROM <table> [WHERE [<setting comparison>] [AND
/// <condition>]]`.
///
/// An arm comparing no column with a setting returns the same rows for
/// every request.
#[derive(Debug)]
pub struct GrantArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) column: &'db DB::Column,
    pub(crate) request: Option<SettingComparison<'db, DB>>,
    pub(crate) restrictions: Vec<LiteralSet<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
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

    /// The comparison of a column with a value of the request, or `None`
    /// where the arm keeps the same rows for every request.
    #[must_use]
    pub fn request(&self) -> Option<&SettingComparison<'db, DB>> {
        self.request.as_ref()
    }

    /// The values columns have to hold, each set over its own column, all of
    /// them holding at once. Empty where the condition is absent or holds
    /// for every row.
    #[must_use]
    pub fn restrictions(&self) -> &[LiteralSet<'db, DB>] {
        &self.restrictions
    }

    /// The bounds a row has to meet against the statement time, every one
    /// of them, in the order the condition writes them.
    #[must_use]
    pub fn time_bounds(&self) -> &[TimeBound<'db, DB>] {
        &self.time_bounds
    }
}

/// `<column> = current_setting(<caller>)`, or `<column>` compared with each
/// element of a declared set, `<column> = ANY (<array>)` or `<column> IN
/// (SELECT <elements>)`.
///
/// `<array>` is `string_to_array(current_setting(<set>), <delimiter>)`, the
/// set packed as [`SetPacking::Delimited`](crate::structs::SetPacking) by
/// that delimiter, or `ARRAY(SELECT <elements>)`. `<elements>` is
/// `unnest(<array>)`, or
/// `jsonb_array_elements_text(current_setting(<set>)::jsonb)`, the set packed
/// as [`SetPacking::JsonArray`](crate::structs::SetPacking).
#[derive(Debug)]
pub struct SettingComparison<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
}

impl<'db, DB: DatabaseLike> SettingComparison<'db, DB> {
    /// The column compared.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The setting compared with, as the deployment declares it, either
    /// [`SettingValue::Caller`](crate::structs::SettingValue::Caller) or
    /// [`SettingValue::Set`](crate::structs::SettingValue::Set).
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
}

/// `<column> <comparison> <statement time> [OR <column> IS NULL]`, a
/// conjunct of a grant arm's condition comparing a `timestamptz` column of
/// the arm's table with the start of the transaction, which `now()`,
/// `CURRENT_TIMESTAMP` and `transaction_timestamp()` read.
///
/// The rows the arm keeps depend on when the helper runs, so a consumer
/// answers the bound against the time of each check.
#[derive(Debug)]
pub struct TimeBound<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) comparison: TimeComparison,
    pub(crate) keeps_null: bool,
}

impl<'db, DB: DatabaseLike> TimeBound<'db, DB> {
    /// The column compared to the statement time.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// How the column compares to the statement time, with the column on
    /// the left whichever side the condition writes it on.
    #[must_use]
    pub fn comparison(&self) -> TimeComparison {
        self.comparison
    }

    /// Whether a row holding `NULL` in the column is kept, which the
    /// condition writes as `OR <column> IS NULL`.
    #[must_use]
    pub fn keeps_null(&self) -> bool {
        self.keeps_null
    }
}

/// How the column of a [`TimeBound`] compares to the statement time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeComparison {
    /// `<column> > <statement time>`.
    Later,
    /// `<column> >= <statement time>`.
    LaterOrEqual,
    /// `<column> < <statement time>`.
    Earlier,
    /// `<column> <= <statement time>`.
    EarlierOrEqual,
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

/// The recursive term of `WITH RECURSIVE <walk> (<column>) AS (<arms> UNION
/// SELECT <key> FROM <table> JOIN <walk> ON <column> = <walk>.<column>)
/// SELECT <column> FROM <walk>`, which adds every row of the table whose
/// parent the set holds until the set holds every row below its other arms.
///
/// The walk ends on a tree holding a cycle, as `UNION` keeps no row twice.
#[derive(Debug)]
pub struct WalkArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) column: &'db DB::Column,
    pub(crate) foreign_key: &'db DB::ForeignKey,
}

impl<'db, DB: DatabaseLike> WalkArm<'db, DB> {
    /// The table the walk descends.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The table's one-column primary key, which the arm returns.
    #[must_use]
    pub fn key(&self) -> &'db DB::Column {
        self.key
    }

    /// The column naming each row's parent.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The single-column foreign key of [`column`](Self::column), which
    /// references [`key`](Self::key).
    #[must_use]
    pub fn foreign_key(&self) -> &'db DB::ForeignKey {
        self.foreign_key
    }
}
