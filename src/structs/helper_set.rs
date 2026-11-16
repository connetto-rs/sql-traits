//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for one
//! request and one argument list, as
//! [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
//! answers it.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use sqlparser::ast::Value;

use crate::{structs::SessionSetting, traits::DatabaseLike};

/// The set a helper returns, the union of its arms.
///
/// An arm whose condition keeps no row for the arguments is left out, and
/// the walks with it where no other arm remains, so a helper returning
/// nothing for them answers no arm.
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
    /// returns a column that is neither or several columns. Arms left out
    /// for the arguments count, so the answer does not depend on them.
    #[must_use]
    pub fn covered_table(&self) -> Option<&'db DB::Table> {
        self.covered_table
    }
}

/// One `SELECT` of a helper's `UNION`, or one grantee of a grant arm whose
/// `IN` list names several.
#[derive(Debug)]
pub enum HelperArm<'db, DB: DatabaseLike> {
    /// The rows of a table its condition keeps, for the grantee it names or
    /// for anyone.
    Grant(GrantArm<'db, DB>),
    /// The rows of a table whose parent another helper returns.
    Parent(ParentArm<'db, DB>),
    /// The rows next to the ones the set itself holds, along one edge of a
    /// tree, the set growing along every walk at once.
    Walk(WalkArm<'db, DB>),
}

/// `SELECT <columns> FROM <table> [WHERE <conditions>]`, the rows the
/// conditions keep for the grantee they name, the caller gate they write,
/// the restrictions and bounds the rest of them reduce to, the caps they
/// write, and the rows of other tables whose existence takes a row away.
#[derive(Debug)]
pub struct GrantArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) columns: Vec<&'db DB::Column>,
    pub(crate) granted_to: GrantedTo<'db, DB>,
    pub(crate) gate: Option<CallerGate<'db>>,
    pub(crate) restrictions: Vec<Restriction<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
    /// Shared by the arms an `IN` list of request values splits one
    /// condition into.
    pub(crate) caps: Arc<[RowCap<'db, DB>]>,
    /// Shared as [`caps`](Self::caps) is.
    pub(crate) anti_joins: Arc<[AntiJoin<'db, DB>]>,
}

impl<'db, DB: DatabaseLike> GrantArm<'db, DB> {
    /// The table the arm reads.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The columns the arm returns, in the order the helper returns them,
    /// one for a helper returning `SETOF` a scalar type and one per column
    /// of a helper returning `TABLE (...)`.
    #[must_use]
    pub fn columns(&self) -> &[&'db DB::Column] {
        &self.columns
    }

    /// Whose rows the arm keeps.
    #[must_use]
    pub fn granted_to(&self) -> &GrantedTo<'db, DB> {
        &self.granted_to
    }

    /// The condition on a caller setting itself, or `None` where the arm
    /// keeps its rows whatever the setting reads.
    #[must_use]
    pub fn gate(&self) -> Option<&CallerGate<'db>> {
        self.gate.as_ref()
    }

    /// What columns have to hold, one restriction per column, all of them
    /// holding at once, in the order the condition first names the columns.
    /// Empty where the condition is absent or holds for every row.
    #[must_use]
    pub fn restrictions(&self) -> &[Restriction<'db, DB>] {
        &self.restrictions
    }

    /// The bounds a row has to meet against the statement time, every one
    /// of them, in the order the condition writes them.
    #[must_use]
    pub fn time_bounds(&self) -> &[TimeBound<'db, DB>] {
        &self.time_bounds
    }

    /// The caps a row has to meet, every one of them, in the order the
    /// condition writes them.
    #[must_use]
    pub fn caps(&self) -> &[RowCap<'db, DB>] {
        &self.caps
    }

    /// The anti-joins a row has to pass, every one of them, in the order the
    /// condition writes them. An anti-join whose own conditions keep no
    /// blocking row takes no row away and is left out.
    #[must_use]
    pub fn anti_joins(&self) -> &[AntiJoin<'db, DB>] {
        &self.anti_joins
    }
}

/// Whose rows a [`GrantArm`] keeps.
#[derive(Debug)]
pub enum GrantedTo<'db, DB: DatabaseLike> {
    /// The rows whose column holds the caller, another principal, or an
    /// element of a declared set, the request carries.
    Request(SettingComparison<'db, DB>),
    /// `<column> IN (SELECT <function>(<arguments>))`, the rows naming one of
    /// the grantees another helper returns, such as every grantee the caller
    /// is.
    Set(GranteeSet<'db, DB>),
    /// Every row the other conditions keep, whoever the request names, the
    /// conditions naming no grantee.
    Anyone,
}

/// The column a grant arm compares with the set another helper returns.
#[derive(Debug)]
pub struct GranteeSet<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) foreign_key: Option<&'db DB::ForeignKey>,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Box<HelperSet<'db, DB>>,
}

impl<'db, DB: DatabaseLike> GranteeSet<'db, DB> {
    /// The column holding the grantee.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The single-column foreign key of [`column`](Self::column), or `None`
    /// where the column has none.
    #[must_use]
    pub fn foreign_key(&self) -> Option<&'db DB::ForeignKey> {
        self.foreign_key
    }

    /// The helper returning the grantees.
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

/// `<operands> IN (SELECT * FROM <function>(<arguments>))`, a conjunct of a
/// grant arm's condition keeping a row only where some row the helper
/// returns holds, column by column, what the operands hold, optionally
/// behind `<setting> IS NULL OR`, which lifts the cap for a request leaving
/// the setting unset.
///
/// The operands are written as a row, `(<operand>, ...)`, one per column
/// the helper returns. A single operand is a cap only behind `IS NULL OR`,
/// and may then compare with `(SELECT <function>(<arguments>))` as well,
/// since without it the conjunct names the arm's grantee (see
/// [`GrantedTo::Set`]). Each operand is a column of the arm's table, or a
/// literal or an argument, plain or cast to the type the helper declares
/// the column it pairs with in. An operand holding `NULL` matches no row,
/// as PostgreSQL's row comparison answers `NULL` for it.
#[derive(Debug)]
pub struct RowCap<'db, DB: DatabaseLike> {
    pub(crate) operands: Vec<CapOperand<'db, DB>>,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Box<HelperSet<'db, DB>>,
    pub(crate) lift: Option<CapLift<'db>>,
}

impl<'db, DB: DatabaseLike> RowCap<'db, DB> {
    /// The operands, one per column [`set`](Self::set) returns, in its
    /// order.
    #[must_use]
    pub fn operands(&self) -> &[CapOperand<'db, DB>] {
        &self.operands
    }

    /// The helper returning the rows the operands are compared with.
    #[must_use]
    pub fn function(&self) -> &'db DB::Function {
        self.function
    }

    /// The arguments the cap passes to [`function`](Self::function), the
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

    /// The `<setting> IS NULL` lifting the cap, or `None` where the cap
    /// holds for every request.
    #[must_use]
    pub fn lift(&self) -> Option<&CapLift<'db>> {
        self.lift.as_ref()
    }
}

/// One operand of a [`RowCap`].
#[derive(Debug)]
pub enum CapOperand<'db, DB: DatabaseLike> {
    /// A column of the arm's table, compared row by row.
    Column(&'db DB::Column),
    /// A literal or an argument, the same for every row.
    Value(Value),
}

/// `current_setting(<setting>) IS NULL`, lifting a [`RowCap`] for a request
/// leaving the setting unset. The body may cast the read to the type the
/// deployment declares the setting in, and may read it through
/// `NULLIF(<read>, '')`, which reads `''` as `NULL`.
///
/// A custom setting set for one transaction reads `''` once the
/// transaction ends, so a read without `NULLIF` keeps the cap on a pooled
/// connection that once carried the setting (see
/// [`lifts_on_empty`](Self::lifts_on_empty)).
#[derive(Debug)]
pub struct CapLift<'db> {
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
    pub(crate) lifts_on_empty: bool,
}

impl<'db> CapLift<'db> {
    /// The setting read, as the deployment declares it.
    #[must_use]
    pub fn setting(&self) -> &'db SessionSetting {
        self.setting
    }

    /// Whether the body reads the setting with `current_setting(name)`, which
    /// raises `42704` where the setting was never set.
    #[must_use]
    pub fn raises_when_unset(&self) -> bool {
        self.raises_when_unset
    }

    /// Whether the setting reading `''` lifts the cap, the body reading it
    /// through `NULLIF(<read>, '')`. A read cast to a type `''` is not a
    /// value of, such as `uuid`, raises `22P02` on `''` otherwise.
    #[must_use]
    pub fn lifts_on_empty(&self) -> bool {
        self.lifts_on_empty
    }
}

/// `current_setting(<caller setting>) <> <value>` for each excluded value,
/// or `current_setting(<caller setting>) IS NOT NULL`, which keep the arm's
/// rows only for a setting reading neither `NULL` nor an excluded value. The
/// body may cast every read of the setting to its declared type, and then
/// each value plain or cast to that type. It may read the setting through
/// `NULLIF(<read>, '')` in every conjunct, which reads `''` as `NULL`.
///
/// A custom setting set for one transaction reads `''` once the transaction
/// ends, so `IS NOT NULL` holds on a pooled connection that once carried a
/// caller. `<> ''` and a read through `NULLIF` hold only where a caller was
/// sent (see [`excludes_unset`](Self::excludes_unset)).
#[derive(Debug, Clone)]
pub struct CallerGate<'db> {
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
    pub(crate) in_declared_type: bool,
    pub(crate) excludes_unset: bool,
    pub(crate) excluded: Vec<Value>,
}

impl<'db> CallerGate<'db> {
    /// The caller setting the gate reads, as the deployment declares it.
    #[must_use]
    pub fn setting(&self) -> &'db SessionSetting {
        self.setting
    }

    /// Whether the body reads the setting with `current_setting(name)`, which
    /// raises `42704` where the setting was never set.
    #[must_use]
    pub fn raises_when_unset(&self) -> bool {
        self.raises_when_unset
    }

    /// Whether the body casts the setting to its declared type, so each
    /// excluded value compares with the caller as that type rather than as
    /// text. As a `uuid`, `'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'` and its
    /// lower-case spelling exclude the same caller.
    #[must_use]
    pub fn compares_in_declared_type(&self) -> bool {
        self.in_declared_type
    }

    /// Whether the gate keeps no row for a request that sent no caller, the
    /// setting reading `''` or `NULL`. The body writes `<> ''` or reads the
    /// setting through `NULLIF(<read>, '')`. A read cast to a type `''` is
    /// not a value of, such as `uuid`, raises `22P02` on `''` unless it goes
    /// through `NULLIF`, so a gate of such reads answers `false`.
    #[must_use]
    pub fn excludes_unset(&self) -> bool {
        self.excludes_unset
    }

    /// The values the setting must differ from, each once, in the order the
    /// body first names them, `''` left to
    /// [`excludes_unset`](Self::excludes_unset). Empty for `IS NOT NULL` or
    /// `<> ''` alone.
    #[must_use]
    pub fn excluded(&self) -> &[Value] {
        &self.excluded
    }
}

/// `<column> = current_setting(<principal>)[::<declared type>]`, the
/// principal the caller or another the deployment declares, or `<column>`
/// compared with each element of a declared set, `<column> = ANY (<array>)`
/// or `<column> IN (SELECT <elements>)`.
///
/// `<column> IN (<values>)` listing principals beside literals and
/// parameters is read as one grant arm per principal, compared as `=`
/// compares it, and one arm granted to anyone for the literals together, as
/// PostgreSQL reads the list as `<column> = <value>` for each value joined
/// by `OR`. A principal reading `NULL` keeps no row through its arm.
///
/// `<array>` is `string_to_array(current_setting(<set>),
/// <delimiter>)[::<element type>[]]`, the set packed as
/// [`SetPacking::Delimited`](crate::structs::SetPacking) by that delimiter,
/// or `ARRAY(SELECT <elements>)`. `<elements>` is
/// `unnest(<array>)[::<element type>]`, or
/// `jsonb_array_elements_text(current_setting(<set>)::jsonb)[::<element
/// type>]`, the set packed as
/// [`SetPacking::JsonArray`](crate::structs::SetPacking). The element type is
/// the one the deployment declares, written at most once in a read. Every
/// `current_setting(...)` may be read through `NULLIF(current_setting(...),
/// '')`, which reads `''` as `NULL`.
#[derive(Debug)]
pub struct SettingComparison<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
    pub(crate) excludes_unset: bool,
}

impl<'db, DB: DatabaseLike> SettingComparison<'db, DB> {
    /// The column compared.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The setting compared with, as the deployment declares it, either
    /// [`SettingValue::Caller`](crate::structs::SettingValue::Caller),
    /// [`SettingValue::Principal`](crate::structs::SettingValue::Principal)
    /// or [`SettingValue::Set`](crate::structs::SettingValue::Set).
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

    /// Whether the comparison keeps no row for a request that left the
    /// setting unset, the setting reading `''` or `NULL`.
    ///
    /// A read through `NULLIF(<read>, '')` reads both as `NULL`, and a
    /// delimited set unpacks `''` to no element. A caller read otherwise
    /// matches a column holding `''`, or raises `22P02` where it is cast to
    /// a type `''` is not a value of, and a JSON set raises `22P02` on `''`.
    #[must_use]
    pub fn excludes_unset(&self) -> bool {
        self.excludes_unset
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

impl<DB: DatabaseLike> Clone for TimeBound<'_, DB> {
    fn clone(&self) -> Self {
        Self { column: self.column, comparison: self.comparison, keeps_null: self.keeps_null }
    }
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

/// What a grant arm's condition requires of one column.
///
/// `<column> IS NULL` keeps the rows whose column holds no value, as a
/// revocation stamp left `NULL` keeps a link active. It stays apart from a
/// [`TimeBound`] on the same column, which compares a value with the
/// statement time.
#[derive(Debug)]
pub enum Restriction<'db, DB: DatabaseLike> {
    /// The column holds one of the set's values.
    In(LiteralSet<'db, DB>),
    /// `<column> IS NULL`.
    Null(&'db DB::Column),
    /// `<column> IS NOT NULL`.
    NotNull(&'db DB::Column),
}

impl<'db, DB: DatabaseLike> Restriction<'db, DB> {
    /// The column restricted.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        match self {
            Self::In(set) => set.column,
            Self::Null(column) | Self::NotNull(column) => column,
        }
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

/// `NOT EXISTS (SELECT 1 FROM <table> WHERE <correlations> [AND
/// <conditions>])`, a conjunct of a grant arm's condition keeping a row of
/// the arm only where no row of `<table>` matches it. The subquery may
/// return any literal or `*` in place of `1`, as `EXISTS` reads none of it.
///
/// Each correlation is `<table>.<column> = <arm table>.<column>`, either way
/// round. A row of the arm holding `NULL` in a correlated column matches no
/// blocking row, so it is kept. The other conditions read only the blocking
/// table, the helper's arguments and the statement time, and reduce to
/// [`restrictions`](Self::restrictions) and
/// [`time_bounds`](Self::time_bounds) the way a grant arm's do.
#[derive(Debug)]
pub struct AntiJoin<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) correlations: Vec<Correlation<'db, DB>>,
    pub(crate) keyed: bool,
    pub(crate) restrictions: Vec<Restriction<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
}

impl<'db, DB: DatabaseLike> AntiJoin<'db, DB> {
    /// The table whose rows block a row of the arm.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The equalities a blocking row has to meet with the row of the arm,
    /// all of them, in the order the condition writes them.
    #[must_use]
    pub fn correlations(&self) -> &[Correlation<'db, DB>] {
        &self.correlations
    }

    /// Whether the correlated columns of [`table`](Self::table) hold every
    /// column of its primary key or of one of its unique keys over plain
    /// columns with no predicate, so at most one blocking row matches each
    /// row of the arm.
    #[must_use]
    pub fn is_keyed(&self) -> bool {
        self.keyed
    }

    /// What columns of a blocking row have to hold, as
    /// [`GrantArm::restrictions`] reads them. Empty where the conditions
    /// beside the correlations are absent or hold for every row.
    #[must_use]
    pub fn restrictions(&self) -> &[Restriction<'db, DB>] {
        &self.restrictions
    }

    /// The bounds a blocking row has to meet against the statement time,
    /// every one of them, in the order the condition writes them.
    #[must_use]
    pub fn time_bounds(&self) -> &[TimeBound<'db, DB>] {
        &self.time_bounds
    }
}

/// `<blocking column> = <arm column>`, one equality of an [`AntiJoin`].
#[derive(Debug)]
pub struct Correlation<'db, DB: DatabaseLike> {
    pub(crate) blocking_column: &'db DB::Column,
    pub(crate) arm_column: &'db DB::Column,
}

impl<'db, DB: DatabaseLike> Correlation<'db, DB> {
    /// The column of the blocking table.
    #[must_use]
    pub fn blocking_column(&self) -> &'db DB::Column {
        self.blocking_column
    }

    /// The column of the arm's table it equals.
    #[must_use]
    pub fn arm_column(&self) -> &'db DB::Column {
        self.arm_column
    }
}

/// `SELECT <key> FROM <table> WHERE <column> IN (SELECT
/// <function>(<arguments>))`, the key naming a row of the child table and
/// the column naming a row of the parent table.
///
/// The key is the table's primary key, the table then being the child table,
/// or a column a unique key covers alone whose one single-column foreign key
/// references another table's primary key, that table then being the child
/// table, as in an edge table holding the parent of each child that has one.
/// The column names the parent
/// through its single-column foreign key, or, where it has none and the key is
/// the table's primary key, through the set it is compared to, which then
/// covers the parent table (see [`HelperSet::covered_table`]). A column whose
/// parent may be one of several tables is read by one arm per table.
#[derive(Debug)]
pub struct ParentArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) child_table: &'db DB::Table,
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

    /// The column the arm returns, the table's primary key or a column
    /// naming a row of [`child_table`](Self::child_table).
    #[must_use]
    pub fn key(&self) -> &'db DB::Column {
        self.key
    }

    /// The table whose primary key [`key`](Self::key) holds, the arm's own
    /// [`table`](Self::table) where the key is its primary key, else the table
    /// the key's one single-column foreign key references.
    #[must_use]
    pub fn child_table(&self) -> &'db DB::Table {
        self.child_table
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

/// One edge the recursive term of `WITH RECURSIVE <walk> (<column>) AS
/// (<arms> UNION <term>) SELECT <column> FROM <walk>` follows, walking the
/// tree whose edges the table holds until the set stops growing.
///
/// Each row of the table is an edge from the tree's row its child column
/// names to the one its parent column names. The table is the tree itself,
/// its child column the primary key, where each row names its own parent,
/// or an edge table, its child column its primary key or a column a unique
/// key covers alone, holding one row per child that has a parent. A term
/// reading a `LATERAL` union of edges answers one walk per edge, each
/// adding to the one set every other one reads. The walk ends on a tree
/// holding a cycle, as `UNION` keeps no row twice.
#[derive(Debug)]
pub struct WalkArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) tree: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) child: &'db DB::Column,
    pub(crate) column: &'db DB::Column,
    pub(crate) foreign_key: &'db DB::ForeignKey,
    pub(crate) direction: WalkDirection,
}

impl<'db, DB: DatabaseLike> WalkArm<'db, DB> {
    /// The table holding the edges, the one the term reads.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The table whose rows the walk adds, [`table`](Self::table) itself
    /// where each row names its own parent.
    #[must_use]
    pub fn tree(&self) -> &'db DB::Table {
        self.tree
    }

    /// The tree's one-column primary key.
    #[must_use]
    pub fn key(&self) -> &'db DB::Column {
        self.key
    }

    /// The column naming the child of each edge, [`key`](Self::key) where
    /// the tree holds its own edges.
    #[must_use]
    pub fn child(&self) -> &'db DB::Column {
        self.child
    }

    /// The column naming the parent of each edge.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The first foreign key the table declares through which
    /// [`column`](Self::column) names a row of the tree, holding the column
    /// alone or at the position of [`key`](Self::key) among others.
    #[must_use]
    pub fn foreign_key(&self) -> &'db DB::ForeignKey {
        self.foreign_key
    }

    /// Which way the walk follows the tree.
    #[must_use]
    pub fn direction(&self) -> WalkDirection {
        self.direction
    }
}

/// Which way a [`WalkArm`] follows its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkDirection {
    /// `SELECT <child> FROM <table> JOIN <walk> ON <column> =
    /// <walk>.<column>`, or a `LATERAL` edge `SELECT <child> FROM <table>
    /// WHERE <column> = <walk>.<column>`, which adds the child of every edge
    /// whose parent the set holds, so the set ends holding every row below its
    /// other arms.
    Down,
    /// `SELECT <column> FROM <table> JOIN <walk> ON <child> =
    /// <walk>.<column>`, or a `LATERAL` edge `SELECT <column> FROM <table>
    /// WHERE <child> = <walk>.<column>`, which adds the parent of every edge
    /// whose child the set holds, so the set ends holding every row above its
    /// other arms. A root of a tree holding its own edges adds its parent
    /// column's `NULL`, which no `IN` matches, and a root of an edge table has
    /// no edge.
    Up,
}
