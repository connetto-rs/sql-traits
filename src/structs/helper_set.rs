//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for one
//! request and one argument list, as
//! [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
//! answers it.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::Value;

use crate::{
    structs::{SessionSetting, SettingValue},
    traits::DatabaseLike,
};

/// A condition of an arm that
/// [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
/// reads only where its consumer names it, refusing an arm writing it
/// otherwise, so a consumer never keeps a row such a condition takes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArmCondition {
    /// The sets of other helpers a row has to stay outside (see
    /// [`GrantArm::exclusions`]).
    Exclusion,
    /// The settings a request has to leave unset (see
    /// [`GrantArm::unset_settings`]).
    UnsetSetting,
    /// What an edge has to hold for a walk to follow it (see
    /// [`WalkArm::restrictions`], [`WalkArm::time_bounds`] and
    /// [`WalkArm::exclusions`]).
    EdgeCondition,
}

/// The set a helper returns, the union of its arms kept to the rows its
/// [`selection`](Self::selection) keeps.
///
/// An arm whose condition keeps no row for the arguments is left out, and
/// the walks with it where no other arm remains, so a helper returning
/// nothing for them answers no arm.
#[derive(Debug)]
pub struct HelperSet<'db, DB: DatabaseLike> {
    pub(crate) arms: Vec<HelperArm<'db, DB>>,
    pub(crate) covered_table: Option<&'db DB::Table>,
    pub(crate) selection: Option<Vec<GrantArm<'db, DB>>>,
}

impl<'db, DB: DatabaseLike> HelperSet<'db, DB> {
    /// The arms, in the order the body writes them.
    #[must_use]
    pub fn arms(&self) -> &[HelperArm<'db, DB>] {
        &self.arms
    }

    /// The one table whose primary key every arm the body writes returns,
    /// as its own primary key or through a foreign key referencing it, so
    /// that every value of the set is a key of that table. An arm calling a
    /// helper returning one column and returning that column alone returns
    /// keys of the table that helper's set covers.
    ///
    /// A foreign key counts where the column stands at the position of the
    /// table's one-column primary key among the columns it references, and
    /// where it checks every value the column holds, being `MATCH FULL` or
    /// its other columns `NOT NULL`.
    ///
    /// `None` where two arms return keys of different tables, or an arm
    /// returns a column that is neither or several columns. Arms left out
    /// for the arguments count, so the answer does not depend on them.
    #[must_use]
    pub fn covered_table(&self) -> Option<&'db DB::Table> {
        self.covered_table
    }

    /// The rows of the [covered table](Self::covered_table) the final query
    /// of a walk keeps, which every value of the set is the key of.
    ///
    /// A walk's final query `SELECT <key> FROM <table> WHERE <key> IN (SELECT
    /// <column> FROM <walk>) AND <conditions>` keeps the keys it reached of
    /// the rows its conditions keep. Its conditions are read as those of a
    /// grant arm returning the key, one arm per grantee an `IN` list names,
    /// so a consumer applies them as it applies each [`GrantArm`] and keeps
    /// the values of the arms any of them also returns. A consumer leaving
    /// the selection out returns more than the helper does.
    ///
    /// `None` where the final query keeps every key the walk reached.
    #[must_use]
    pub fn selection(&self) -> Option<&[GrantArm<'db, DB>]> {
        self.selection.as_deref()
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
    /// The rows another helper returns.
    Called(CalledArm<'db, DB>),
    /// The rows next to the ones the set itself holds, along the edges one
    /// table holds, the set growing along every walk at once.
    Walk(WalkArm<'db, DB>),
}

impl<'db, DB: DatabaseLike> From<GrantArm<'db, DB>> for HelperArm<'db, DB> {
    fn from(arm: GrantArm<'db, DB>) -> Self {
        Self::Grant(arm)
    }
}

/// What an arm returns in one column of the helper's rows.
///
/// A list of labels is `CROSS JOIN unnest(enum_range(NULL::<type>))
/// [<alias>] [(<column>)]`, every label of an enum type in the order the
/// type declares them, `ALTER TYPE ... ADD VALUE` applied, or `CROSS JOIN
/// (VALUES (<label>), ...) <alias> [(<column>)]`, the labels it lists,
/// each once. Every label is of the type the helper declares the column
/// in, written as a literal of it, and the arm returns every row it keeps
/// once per label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Returned<S> {
    /// The value `S` names, read off each row the arm keeps.
    Read(S),
    /// Each of these labels in turn, beside the values read off the row.
    Labels(Vec<Value>),
}

/// `SELECT <columns> FROM <table> [JOIN <joined table> ON <key>]... [WHERE
/// <conditions>]`, the rows the conditions keep for the grantee they name,
/// the caller gate they write, the settings they require unset, the
/// restrictions and bounds the rest of them reduce to, the caps they write,
/// the rows of other tables each row has to name, the rows of other tables
/// whose existence takes a row away, and the sets of other helpers a row
/// has to stay outside.
#[derive(Debug)]
pub struct GrantArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) returned: Vec<Returned<&'db DB::Column>>,
    pub(crate) granted_to: GrantedTo<'db, DB>,
    pub(crate) gate: Option<CallerGate<'db>>,
    /// Shared as [`caps`](Self::caps) is.
    pub(crate) unset_settings: Arc<[UnsetSetting<'db>]>,
    pub(crate) restrictions: Vec<Restriction<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
    /// Shared by the arms an `IN` list of request values splits one
    /// condition into.
    pub(crate) caps: Arc<[RowCap<'db, DB>]>,
    /// Shared as [`caps`](Self::caps) is.
    pub(crate) anti_joins: Arc<[AntiJoin<'db, DB>]>,
    /// Shared as [`caps`](Self::caps) is.
    pub(crate) exclusions: Arc<[ExcludedSet<'db, DB>]>,
    /// Shared as [`caps`](Self::caps) is.
    pub(crate) joined_rows: Arc<[JoinedRow<'db, DB>]>,
}

impl<'db, DB: DatabaseLike> GrantArm<'db, DB> {
    /// The table the arm reads.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// What the arm returns, in the order the helper returns it, one column
    /// for a helper returning `SETOF` a scalar type and one per column of a
    /// helper returning `TABLE (...)`, each a column of the arm's
    /// [`table`](Self::table) or a list of labels.
    #[must_use]
    pub fn returned(&self) -> &[Returned<&'db DB::Column>] {
        &self.returned
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

    /// The settings the request has to leave unset for the arm to keep any
    /// row, every one of them, in the order the condition writes them. Empty
    /// in every answer whose reading leaves [`ArmCondition::UnsetSetting`]
    /// unread, which refuses an arm writing one.
    #[must_use]
    pub fn unset_settings(&self) -> &[UnsetSetting<'db>] {
        &self.unset_settings
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

    /// The sets of other helpers a row's column has to stay outside, every
    /// one of them, in the order the condition writes them. An exclusion of
    /// a set holding no arm takes no row away and is left out. Empty in every
    /// answer whose reading leaves [`ArmCondition::Exclusion`] unread, which
    /// refuses an arm writing one.
    #[must_use]
    pub fn exclusions(&self) -> &[ExcludedSet<'db, DB>] {
        &self.exclusions
    }

    /// The rows the arm joins, in the order the body joins them, each of
    /// which a row of the arm's table has to name for the arm to keep it.
    #[must_use]
    pub fn joined_rows(&self) -> &[JoinedRow<'db, DB>] {
        &self.joined_rows
    }
}

/// `JOIN <table> ON <referenced column> = <host column> [AND ...]`, the row
/// a grant arm's row names through one foreign key of the arm's table, every
/// column of the key equated with the column it references, either way round
/// and in any order, with the conditions the arm sets on that row.
///
/// A foreign key references a unique key, so a row of the arm names at most
/// one joined row, and the arm keeps its row only where that joined row
/// exists and meets the conditions. A row holding `NULL` in a column of the
/// key names none.
#[derive(Debug)]
pub struct JoinedRow<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) foreign_key: &'db DB::ForeignKey,
    pub(crate) columns: Vec<(&'db DB::Column, &'db DB::Column)>,
    pub(crate) restrictions: Vec<Restriction<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
}

impl<'db, DB: DatabaseLike> JoinedRow<'db, DB> {
    /// The table joined, the one [`foreign_key`](Self::foreign_key)
    /// references.
    #[must_use]
    pub fn table(&self) -> &'db DB::Table {
        self.table
    }

    /// The foreign key of the arm's table naming the joined row.
    #[must_use]
    pub fn foreign_key(&self) -> &'db DB::ForeignKey {
        self.foreign_key
    }

    /// Each column of [`foreign_key`](Self::foreign_key) beside the column of
    /// the joined table it references, in the order the key declares them.
    #[must_use]
    pub fn columns(&self) -> &[(&'db DB::Column, &'db DB::Column)] {
        &self.columns
    }

    /// What columns of the joined row have to hold, as
    /// [`GrantArm::restrictions`] sets it on the arm's own row.
    #[must_use]
    pub fn restrictions(&self) -> &[Restriction<'db, DB>] {
        &self.restrictions
    }

    /// The bounds the joined row has to meet against the statement time, as
    /// [`GrantArm::time_bounds`] sets them on the arm's own row.
    #[must_use]
    pub fn time_bounds(&self) -> &[TimeBound<'db, DB>] {
        &self.time_bounds
    }
}

/// Whose rows a [`GrantArm`] keeps.
#[derive(Debug)]
pub enum GrantedTo<'db, DB: DatabaseLike> {
    /// The rows whose columns hold values the request carries, the caller,
    /// another principal, an element of a declared set, or a scalar value,
    /// one comparison or more, in the order the condition writes them. The
    /// arm keeps a row only where every comparison holds on it, as for
    /// `<namespace column> = <setting> AND <identity column> = <other
    /// setting>` naming one principal by two values.
    Request(Vec<SettingComparison<'db, DB>>),
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
    pub(crate) set: Arc<HelperSet<'db, DB>>,
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
    /// [`arguments`](Self::arguments), the one answer every arm of the same
    /// [`helper_set`](crate::traits::FunctionLike::helper_set) call reading
    /// that helper with those arguments holds.
    #[must_use]
    pub fn set(&self) -> &Arc<HelperSet<'db, DB>> {
        &self.set
    }
}

/// `<column> NOT IN (SELECT <function>(<arguments>))`, a conjunct of a grant
/// arm's condition keeping a row only where its column holds no value the
/// helper returns. PostgreSQL evaluates the other spellings alike, `NOT
/// (<column> IN (...))`, `<column> <> ALL (<set>)` and `NOT (<column> = ANY
/// (<set>))`, `<set>` being `(SELECT <function>(<arguments>))` or
/// `ARRAY(SELECT <function>(<arguments>))`.
///
/// The column is one of the arm's table, and each argument is a literal or
/// an argument of the helper, as for a [`GranteeSet`]. The helper's owner
/// executes the function excluded, so the caller needs no `EXECUTE` on it.
///
/// The conjunct is `NULL`, which drops the row, where the column holds
/// `NULL` and the helper returns any row, and where the helper returns
/// `NULL` but not the column's value. A row holding `NULL` is kept where
/// the helper returns no row. A consumer subtracting the set as a set of
/// keys keeps those rows unless it proves the column `NOT NULL` and every
/// column the set returns `NOT NULL`.
#[derive(Debug)]
pub struct ExcludedSet<'db, DB: DatabaseLike> {
    pub(crate) column: &'db DB::Column,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Arc<HelperSet<'db, DB>>,
}

impl<'db, DB: DatabaseLike> ExcludedSet<'db, DB> {
    /// The column of the arm's table compared with the set.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The helper returning the values excluded.
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
    /// [`arguments`](Self::arguments), shared as [`GranteeSet::set`] is.
    #[must_use]
    pub fn set(&self) -> &Arc<HelperSet<'db, DB>> {
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
/// the helper returns. A single operand is a cap behind `IS NULL OR`, and
/// may then compare with `(SELECT <function>(<arguments>))` as well. Without
/// it, a single operand is a cap only where it is the primary key of the
/// arm's table, which the arm returns and which references no table through
/// a single-column foreign key, or where it is a column of the arm's table
/// in an arm naming its grantee by a value of the request (see
/// [`GrantedTo::Request`]), since otherwise the conjunct names the arm's
/// grantee (see [`GrantedTo::Set`]) or its parent (see [`ParentArm`]).
/// Each operand is a column of the arm's table or of a row it joins (see
/// [`GrantArm::joined_rows`]), or a literal or an argument, plain or cast to
/// the type the helper declares the column it pairs with in. An operand
/// holding `NULL` matches no row, as PostgreSQL's row comparison answers
/// `NULL` for it.
#[derive(Debug)]
pub struct RowCap<'db, DB: DatabaseLike> {
    pub(crate) operands: Vec<CapOperand<'db, DB>>,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Arc<HelperSet<'db, DB>>,
    pub(crate) lift: Option<UnsetSetting<'db>>,
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
    /// [`arguments`](Self::arguments), shared as [`GranteeSet::set`] is.
    #[must_use]
    pub fn set(&self) -> &Arc<HelperSet<'db, DB>> {
        &self.set
    }

    /// The `<setting> IS NULL` lifting the cap, or `None` where the cap
    /// holds for every request.
    #[must_use]
    pub fn lift(&self) -> Option<&UnsetSetting<'db>> {
        self.lift.as_ref()
    }
}

/// One operand of a [`RowCap`].
#[derive(Debug)]
pub enum CapOperand<'db, DB: DatabaseLike> {
    /// A column of the arm's table, compared row by row.
    Column(&'db DB::Column),
    /// A column of the row the arm joins at `row` in
    /// [`GrantArm::joined_rows`], read off the row each row of the arm names.
    Joined {
        /// The index of the joined row in [`GrantArm::joined_rows`].
        row: usize,
        /// The column of that row's table.
        column: &'db DB::Column,
    },
    /// A literal or an argument, the same for every row.
    Value(Value),
}

/// `current_setting(<setting>) IS NULL`, holding for a request leaving the
/// setting unset, which lifts a [`RowCap`] or, as a conjunct of its own,
/// keeps a grant arm's rows only for such a request (see
/// [`GrantArm::unset_settings`]). The body may cast the read to the type the
/// deployment declares the setting in, and may read it through
/// `NULLIF(<read>, '')`, which reads `''` as `NULL`.
///
/// A custom setting set for one transaction reads `''` once the
/// transaction ends, so a read without `NULLIF` fails on a pooled
/// connection that once carried the setting (see
/// [`reads_empty_as_unset`](Self::reads_empty_as_unset)).
#[derive(Debug)]
pub struct UnsetSetting<'db> {
    pub(crate) setting: &'db SessionSetting,
    pub(crate) raises_when_unset: bool,
    pub(crate) reads_empty_as_unset: bool,
}

impl<'db> UnsetSetting<'db> {
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

    /// Whether the setting reading `''` holds as unset, the body reading it
    /// through `NULLIF(<read>, '')`. A read cast to a type `''` is not a
    /// value of, such as `uuid`, raises `22P02` on `''` otherwise.
    #[must_use]
    pub fn reads_empty_as_unset(&self) -> bool {
        self.reads_empty_as_unset
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

/// `<column> = <read>`, `<read>` reading the principal the caller or another
/// the deployment declares, or a scalar value, or `<column>` compared with
/// each element of a declared set, `<column> = ANY (<array>)` or `<column>
/// IN (SELECT <elements>)`. A grant arm may write several, on the same
/// column or on different ones, and keeps a row only where all of them hold
/// on it (see [`GrantedTo::Request`]).
///
/// `<read>` is `current_setting(<setting>)[::<declared type>]`. A read of a
/// scalar may be normalized by `lower(...)`, once, which a consumer applies
/// to the value it supplies as PostgreSQL does (see
/// [`normalization`](Self::normalization)).
///
/// `<column> IN (<values>)` listing principals or scalars beside literals
/// and parameters is read as one grant arm per read, compared as `=`
/// compares it, and one arm granted to anyone for the literals together, as
/// PostgreSQL reads the list as `<column> = <value>` for each value joined
/// by `OR`. A read holding `NULL` keeps no row through its arm.
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
    pub(crate) value: &'db SettingValue,
    pub(crate) raises_when_unset: bool,
    pub(crate) excludes_unset: bool,
    pub(crate) normalization: Option<RequestNormalization>,
}

impl<DB: DatabaseLike> Clone for SettingComparison<'_, DB> {
    fn clone(&self) -> Self {
        Self {
            column: self.column,
            setting: self.setting,
            value: self.value,
            raises_when_unset: self.raises_when_unset,
            excludes_unset: self.excludes_unset,
            normalization: self.normalization,
        }
    }
}

impl<'db, DB: DatabaseLike> SettingComparison<'db, DB> {
    /// The column compared.
    #[must_use]
    pub fn column(&self) -> &'db DB::Column {
        self.column
    }

    /// The setting compared with, as the deployment declares it.
    #[must_use]
    pub fn setting(&self) -> &'db SessionSetting {
        self.setting
    }

    /// What the column is compared with, the [`SettingValue::Caller`],
    /// [`SettingValue::Principal`], [`SettingValue::Scalar`] or
    /// [`SettingValue::Set`] the [`setting`](Self::setting) holds.
    #[must_use]
    pub fn value(&self) -> &'db SettingValue {
        self.value
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

    /// The normalization the body writes on its read of a scalar, `None`
    /// where the column is compared with the value as the request sends it.
    /// A row is kept where the column holds the normalized value, so a
    /// consumer normalizes the value it supplies before comparing it.
    #[must_use]
    pub fn normalization(&self) -> Option<RequestNormalization> {
        self.normalization
    }
}

/// A function a [`SettingComparison`] applies to its read of a scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestNormalization {
    /// PostgreSQL's own `lower(text)`, which converts the text to lower case
    /// by the rules of the database's default collation, and keeps `NULL`
    /// and `''` as they are.
    Lower,
}

impl core::fmt::Display for RequestNormalization {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Lower => "lower",
        })
    }
}

/// `<column> <comparison> <statement time> [OR <column> IS NULL]`, a
/// conjunct of a grant arm's condition comparing a `timestamptz` column of
/// the arm's table, or of a row it joins, with the start of the transaction,
/// which `now()`, `CURRENT_TIMESTAMP` and `transaction_timestamp()` read.
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
    pub(crate) set: Arc<HelperSet<'db, DB>>,
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
    /// [`arguments`](Self::arguments), shared as [`GranteeSet::set`] is.
    #[must_use]
    pub fn set(&self) -> &Arc<HelperSet<'db, DB>> {
        &self.set
    }
}

/// `SELECT <returned> FROM <function>(<arguments>) [<alias>] [(<columns>)]
/// [CROSS JOIN <labels>]...`, every row another helper returns.
///
/// Each column the arm returns is a column of the called helper's rows or a
/// list of labels (see [`Returned`]). A column is named by the alias's
/// column list, else by the name the helper declares it under in `TABLE
/// (...)`, else, for a helper returning `SETOF` a scalar type, by the alias
/// or the function's name.
#[derive(Debug)]
pub struct CalledArm<'db, DB: DatabaseLike> {
    pub(crate) returned: Vec<Returned<usize>>,
    pub(crate) function: &'db DB::Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) set: Arc<HelperSet<'db, DB>>,
}

impl<'db, DB: DatabaseLike> CalledArm<'db, DB> {
    /// What the arm returns, in the order the helper returns it, each the
    /// position of a column of the rows [`set`](Self::set) holds or a list
    /// of labels.
    #[must_use]
    pub fn returned(&self) -> &[Returned<usize>] {
        &self.returned
    }

    /// The helper the arm calls.
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
    /// [`arguments`](Self::arguments), shared as [`GranteeSet::set`] is.
    #[must_use]
    pub fn set(&self) -> &Arc<HelperSet<'db, DB>> {
        &self.set
    }
}

/// One edge the recursive term of `WITH RECURSIVE <walk> (<column>) AS
/// (<arms> UNION <term>) SELECT <column> FROM <walk>` follows, walking the
/// rows of the [tree](Self::tree) along the edges the table holds until the
/// set stops growing.
///
/// Each row of the table meeting the [restrictions](Self::restrictions), the
/// [time bounds](Self::time_bounds) and the [exclusions](Self::exclusions)
/// is an edge from the tree's row its child column names to the one its
/// parent column names. The table is the tree itself, its child column the
/// primary key, where each row names its own parent, or an edge table
/// holding any number of edges per child and per parent, so the edges may
/// draw any graph. The child column of an edge table is the one of the two
/// its primary key is, else the one a unique key covers alone, and the
/// column the term adds where its keys tell neither apart, the walk then
/// going [down](WalkDirection::Down). A term reading a `LATERAL` union of
/// edges answers one walk per edge, each adding to the one set every other
/// one reads. The walk ends on edges closing a cycle, as `UNION` keeps no
/// row twice.
#[derive(Debug)]
pub struct WalkArm<'db, DB: DatabaseLike> {
    pub(crate) table: &'db DB::Table,
    pub(crate) tree: &'db DB::Table,
    pub(crate) key: &'db DB::Column,
    pub(crate) child: &'db DB::Column,
    pub(crate) column: &'db DB::Column,
    pub(crate) foreign_key: &'db DB::ForeignKey,
    pub(crate) direction: WalkDirection,
    pub(crate) restrictions: Vec<Restriction<'db, DB>>,
    pub(crate) time_bounds: Vec<TimeBound<'db, DB>>,
    pub(crate) exclusions: Vec<ExcludedSet<'db, DB>>,
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

    /// What columns of an edge have to hold for the walk to follow it, as
    /// [`GrantArm::restrictions`] reads them. Empty where the term sets no
    /// condition on the edge or one holding for every row, and in every
    /// answer whose reading leaves [`ArmCondition::EdgeCondition`] unread,
    /// which refuses a term setting one.
    #[must_use]
    pub fn restrictions(&self) -> &[Restriction<'db, DB>] {
        &self.restrictions
    }

    /// The bounds an edge has to meet against the statement time for the
    /// walk to follow it, every one of them, in the order the term writes
    /// them. Empty in every answer whose reading leaves
    /// [`ArmCondition::EdgeCondition`] unread.
    #[must_use]
    pub fn time_bounds(&self) -> &[TimeBound<'db, DB>] {
        &self.time_bounds
    }

    /// The sets of other helpers a column of an edge has to stay outside for
    /// the walk to follow it, as [`GrantArm::exclusions`] reads them, every
    /// one of them in the order the term writes them. Empty in every answer
    /// whose reading leaves [`ArmCondition::EdgeCondition`] or
    /// [`ArmCondition::Exclusion`] unread, which refuses a term writing one.
    #[must_use]
    pub fn exclusions(&self) -> &[ExcludedSet<'db, DB>] {
        &self.exclusions
    }
}

/// Which way a [`WalkArm`] follows its edges.
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
    /// no edge. A walk reads as one up only where a key of its table orients
    /// its edges (see [`WalkArm`]).
    Up,
}
