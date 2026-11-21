//! The answer behind [`FunctionLike::helper_set`], read from the body
//! [`FunctionLike::written_body`] hands over.

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    ArgMode, BinaryOperator, CastKind, DataType, Distinct, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentList, FunctionArguments, FunctionBehavior, FunctionSecurity,
    GroupByExpr, Ident, InTarget, Join, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart,
    Query, Select, SelectFlavor, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement,
    TableAlias, TableAliasColumnDef, TableFactor, TableFunctionArgs, UnaryOperator, Value, Visit,
    WildcardAdditionalOptions,
};

use super::{
    data_type::written_element,
    function_body::{SqlBody, written_sql_body},
};
use crate::{
    errors::{AccessorRefusal, HelperSetError, LookupError},
    structs::{
        AntiJoin, CalledArm, CallerGate, CapLift, CapOperand, Correlation, Deployment, ExcludedSet,
        FunctionBody, GrantArm, GrantedTo, GranteeSet, HelperArm, HelperSet, IdentifierCase,
        JoinedRow, LiteralSet, ParentArm, RequestNormalization, Restriction, Returned, RowCap,
        RowSecurity, RowStatement, SessionSetting, SetPacking, SettingComparison, SettingContent,
        SettingValue, TargetName, TimeBound, TimeComparison, WalkArm, WalkDirection,
    },
    traits::{
        ColumnLike, DataTypeLike, DatabaseLike, EnumerationKind, EnumerationMember, ForeignKeyLike,
        FunctionLike, IndexLike, Relation, RoleLike, SchemaLike, TableIndexLike, TableLike,
        UserDefinedTypeLike,
    },
    utils::{
        call_sites::{ExpressionForm, call_reaches, expression_form},
        identifier_resolution::{SearchPath, identifiers_match},
        object_name::{
            PG_CATALOG, Qualifier, object_name_identifiers, object_name_last_part, qualifier_of,
            resolve_relation_object_name_on_path, target_name_of_table, type_search_path,
        },
        postgres_types::{PgType, parse_data_type},
        scalar_family::ScalarFamily,
        type_modifier,
        walk::walk_expressions,
    },
};

/// See [`FunctionLike::helper_set`] and
/// [`FunctionLike::helper_set_with_exclusions`], which `reads_exclusions`
/// chooses between.
pub(crate) fn helper_set<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    arguments: &[Value],
    deployment: &'db Deployment,
    database: &'db DB,
    reads_exclusions: bool,
) -> Result<HelperSet<'db, DB>, HelperSetError> {
    Reader { deployment, database, reads_exclusions, answering: Vec::new() }
        .answer(function, arguments)
}

/// A literal the body compares, as far as comparing it goes.
#[derive(Clone, PartialEq, Eq)]
enum Literal {
    Null,
    Boolean(bool),
    Text(String),
    Number(String),
}

impl Literal {
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(Self::Null),
            Value::Boolean(boolean) => Some(Self::Boolean(*boolean)),
            Value::SingleQuotedString(text) => Some(Self::Text(text.clone())),
            Value::DollarQuotedString(text) => Some(Self::Text(text.value.clone())),
            Value::Number(number, false) => Some(Self::Number(number.clone())),
            _ => None,
        }
    }

    /// `self = other`, or `None` where the two are not compared without a
    /// type this reader does not infer.
    fn equals(&self, other: &Self) -> Option<Equality> {
        let holds = |holds: bool| if holds { Equality::Holds } else { Equality::Fails };
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => Some(Equality::Null),
            (Self::Boolean(left), Self::Boolean(right)) => Some(holds(left == right)),
            (Self::Text(left), Self::Text(right)) => Some(holds(left == right)),
            (Self::Number(left), Self::Number(right)) if left == right => Some(Equality::Holds),
            (Self::Number(left), Self::Number(right)) => {
                let (left, right) = (left.parse::<i128>().ok()?, right.parse::<i128>().ok()?);
                Some(holds(left == right))
            }
            _ => None,
        }
    }
}

/// What `=` answers for two literals.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Equality {
    Holds,
    Fails,
    /// One side is `NULL`, which keeps no row.
    Null,
}

/// `current_setting('<name>'[, <missing_ok>])`, resolved to PostgreSQL's own
/// function, or that read through `NULLIF(<read>, '')` or `lower(<read>)`,
/// either returned by an accessor the body calls.
struct SettingRead {
    name: String,
    /// Whether the body reads the setting without `missing_ok`.
    raises_when_unset: bool,
    /// Whether the body reads it through `NULLIF(<read>, '')`, which reads
    /// `''` as `NULL`.
    empty_as_null: bool,
    /// Whether an accessor returns it cast to the type the deployment
    /// declares it in, or a set cast to the type it declares its elements in.
    in_declared_type: bool,
    /// The function the body applies to the text it reads.
    normalization: Option<RequestNormalization>,
}

/// A value the body passes: the literal it compares as and the value a
/// translator writes back.
#[derive(Clone)]
struct Passed {
    literal: Literal,
    value: Value,
}

/// One conjunct gating a grant arm on a caller setting itself.
struct GateRead<'db> {
    setting: &'db SessionSetting,
    raises_when_unset: bool,
    /// Whether the conjunct casts the setting to its declared type.
    in_declared_type: bool,
    /// Whether the conjunct reads the setting through `NULLIF(<read>, '')`.
    empty_as_null: bool,
    /// The value `<>` excludes, `None` for `IS NOT NULL`.
    excluded: Option<Passed>,
}

/// The gates a grant arm's conjuncts write, merged into one.
#[derive(Default)]
struct Gates<'db> {
    gate: Option<CallerGate<'db>>,
    /// Whether the gates read the setting through `NULLIF(<read>, '')`.
    empty_as_null: bool,
    /// The literals the gate excludes, beside the values it hands over.
    excluded: Vec<Literal>,
    /// Whether one excludes `NULL`, which keeps no row.
    keeps_nothing: bool,
}

impl<'db> Gates<'db> {
    /// Merges `read`, `false` where it reads another setting than the gates
    /// merged before it, or reads it in another type or another spelling.
    fn add(&mut self, read: GateRead<'db>) -> bool {
        if self.gate.is_none() {
            self.empty_as_null = read.empty_as_null;
        }
        let gate = self.gate.get_or_insert_with(|| {
            CallerGate {
                setting: read.setting,
                raises_when_unset: false,
                in_declared_type: read.in_declared_type,
                excludes_unset: read.empty_as_null,
                excluded: Vec::new(),
            }
        });
        if !core::ptr::eq(gate.setting, read.setting)
            || gate.in_declared_type != read.in_declared_type
            || self.empty_as_null != read.empty_as_null
        {
            return false;
        }
        gate.raises_when_unset |= read.raises_when_unset;
        match read.excluded {
            Some(Passed { literal: Literal::Null, .. }) => self.keeps_nothing = true,
            Some(Passed { literal: Literal::Text(text), .. }) if text.is_empty() => {
                gate.excludes_unset = true;
            }
            Some(Passed { literal, value }) if !self.excluded.contains(&literal) => {
                self.excluded.push(literal);
                gate.excluded.push(value);
            }
            Some(_) | None => {}
        }
        true
    }
}

/// The helper an arm's `(SELECT <helper>(<arguments>))` calls, with what it
/// answers.
struct Called<'db, DB: DatabaseLike> {
    function: &'db DB::Function,
    arguments: Vec<Value>,
    set: HelperSet<'db, DB>,
}

/// A helper call an arm's subquery writes, the name it calls and the
/// arguments it passes.
struct Call<'q> {
    name: &'q ObjectName,
    arguments: &'q [FunctionArg],
}

/// One input parameter of the helper being read, bound to its argument.
struct Parameter<'f> {
    name: Option<TargetName<'f>>,
    passed: Passed,
}

/// The helper whose arm is being read, or an accessor whose body is, with
/// what its body's names resolve to.
struct Scope<'db, DB: DatabaseLike> {
    function: &'db DB::Function,
    name: String,
    path: SearchPath<'db>,
    /// The entries of [`path`](Self::path) whose schema the owner holds
    /// `USAGE` on, which are the ones PostgreSQL searches for the owner.
    usable_path: SearchPath<'db>,
    parameters: Vec<Parameter<'db>>,
    /// The role the body runs as.
    owner: &'db DB::Role,
    /// The accessors whose bodies are being read, outermost first, ending
    /// with this one where it is one.
    accessing: Vec<&'db DB::Function>,
}

impl<DB: DatabaseLike> Scope<'_, DB> {
    fn arm_shape(&self, arm: &impl ToString) -> HelperSetError {
        HelperSetError::ArmShape { function: self.name.clone(), arm: arm.to_string() }
    }

    fn irreducible(&self, condition: &Expr) -> HelperSetError {
        HelperSetError::Irreducible {
            function: self.name.clone(),
            condition: condition.to_string(),
        }
    }

    /// Splits the body into its arms and, where it writes `WITH RECURSIVE`,
    /// the term walking down a tree under them, with the final query where
    /// it reselects what the walk reached.
    fn body<'q>(&self, query: &'q Query) -> Result<Body<'q>, HelperSetError> {
        let mut arms = Vec::new();
        let Some(with) = &query.with else {
            union_arms(query, &mut arms).map_err(|arm| self.arm_shape(&arm))?;
            return Ok(Body { arms, walk: None });
        };
        let shape = || self.arm_shape(query);
        let ([cte], true) = (with.cte_tables.as_slice(), with.recursive) else {
            return Err(shape());
        };
        if !unadorned(query)
            || cte.materialized.is_some()
            || cte.from.is_some()
            || cte.alias.at.is_some()
            || !plain_query(&cte.query)
        {
            return Err(shape());
        }
        let SetExpr::SetOperation { op: SetOperator::Union, set_quantifier, left, right } =
            cte.query.body.as_ref()
        else {
            return Err(shape());
        };
        match set_quantifier {
            SetQuantifier::None | SetQuantifier::Distinct => {}
            SetQuantifier::All | SetQuantifier::AllByName => {
                return Err(HelperSetError::UnionAllWalk { function: self.name.clone() });
            }
            SetQuantifier::ByName | SetQuantifier::DistinctByName => return Err(shape()),
        }
        let SetExpr::Select(term) = right.as_ref() else { return Err(shape()) };
        set_arms(left, &mut arms).map_err(|arm| self.arm_shape(&arm))?;
        let name = &cte.alias.name;
        // PostgreSQL refuses the walk's name in the arms before its recursive
        // term, so a table of that name is not what they read.
        if arms.iter().flat_map(|arm| &arm.from).any(|from| {
            core::iter::once(&from.relation)
                .chain(from.joins.iter().map(|join| &join.relation))
                .any(|relation| walk_qualifier(relation, name).is_some())
        }) {
            return Err(shape());
        }
        let column = match cte.alias.columns.as_slice() {
            [] => arms.first().and_then(|first| output_name(first)).ok_or_else(shape)?,
            [TableAliasColumnDef { name, data_type: None }] => name,
            _ => return Err(shape()),
        };
        let SetExpr::Select(last) = query.body.as_ref() else { return Err(shape()) };
        let [from] = last.from.as_slice() else { return Err(shape()) };
        let reselection = match walk_qualifier(&from.relation, name) {
            Some(qualifier) => {
                let ([item], None) = (last.projection.as_slice(), &last.selection) else {
                    return Err(shape());
                };
                let (SelectItem::UnnamedExpr(returned)
                | SelectItem::ExprWithAlias { expr: returned, .. }) = item
                else {
                    return Err(shape());
                };
                if !plain_select(last)
                    || !from.joins.is_empty()
                    || !names_column(returned, qualifier, column, true)
                {
                    return Err(shape());
                }
                None
            }
            None => Some(last.as_ref()),
        };
        Ok(Body { arms, walk: Some(Walk { name, column, term, reselection }) })
    }

    /// The parameter `expression` names, if it names one.
    fn parameter(&self, expression: &Expr) -> Option<(&Passed, String)> {
        match expression {
            Expr::Identifier(ident) => self.parameter_named(ident),
            Expr::CompoundIdentifier(parts) => {
                let [qualifier, name] = parts.as_slice() else { return None };
                identifiers_match(
                    self.function.name(),
                    self.function.name_is_quoted(),
                    &qualifier.value,
                    qualifier.quote_style.is_some(),
                )
                .then(|| self.parameter_named(name))
                .flatten()
            }
            Expr::Value(value) => {
                let Value::Placeholder(placeholder) = &value.value else { return None };
                let position: usize = placeholder.strip_prefix('$')?.parse().ok()?;
                let parameter = self.parameters.get(position.checked_sub(1)?)?;
                Some((&parameter.passed, placeholder.clone()))
            }
            _ => None,
        }
    }

    fn parameter_named(&self, ident: &Ident) -> Option<(&Passed, String)> {
        self.parameters
            .iter()
            .find(|parameter| {
                parameter.name.as_ref().is_some_and(|name| {
                    identifiers_match(
                        name.name(),
                        name.name_is_quoted(),
                        &ident.value,
                        ident.quote_style.is_some(),
                    )
                })
            })
            .map(|parameter| (&parameter.passed, ident.value.clone()))
    }
}

/// The table an arm reads, with the name its columns are qualified by and
/// the columns it declares.
struct ArmTable<'q, 'db, DB: DatabaseLike> {
    table: &'db DB::Table,
    qualifier: Cow<'q, Ident>,
    columns: Vec<&'db DB::Column>,
    case: IdentifierCase,
}

impl<'db, DB: DatabaseLike> ArmTable<'_, 'db, DB> {
    /// The column of the table `expression` names, if it names one.
    fn column(&self, expression: &Expr) -> Option<&'db DB::Column> {
        let name = self.column_name(expression)?.to_string();
        self.columns.iter().copied().find(|column| {
            self.case.names_stored(column.column_name(), column.column_name_is_quoted(), &name)
        })
    }

    /// The name `expression` gives a column of this table, bare or qualified
    /// by the table's name or alias.
    fn column_name<'e>(&self, expression: &'e Expr) -> Option<&'e Ident> {
        match unnest(expression) {
            Expr::Identifier(ident) => Some(ident),
            Expr::CompoundIdentifier(parts) => {
                let [qualifier, ident] = parts.as_slice() else { return None };
                identifiers_match(
                    &self.qualifier.value,
                    self.qualifier.quote_style.is_some(),
                    &qualifier.value,
                    qualifier.quote_style.is_some(),
                )
                .then_some(ident)
            }
            _ => None,
        }
    }

    /// Every column of the table `node` names, each once, which the owner
    /// needs `SELECT` on wherever the name stands.
    fn columns_read<N: Visit + ?Sized>(&self, node: &N) -> Vec<&'db DB::Column> {
        let mut read: Vec<&'db DB::Column> = Vec::new();
        let _: ControlFlow<()> = walk_expressions(node, |expression| {
            if let Some(column) = self.column(expression)
                && !read.iter().any(|seen| core::ptr::eq(*seen, column))
            {
                read.push(column);
            }
            ControlFlow::Continue(())
        });
        read
    }
}

/// A relation an arm reads that is no table, with the name its columns are
/// qualified by and the names of its columns.
struct Named<'q> {
    qualifier: Cow<'q, Ident>,
    columns: Vec<Cow<'q, Ident>>,
}

impl<'q> Named<'q> {
    /// The names `alias` gives a relation named `name` whose columns are
    /// named `columns` unless the alias renames them, `None` for an alias
    /// renaming more columns than the relation has or typing one.
    fn aliased(
        alias: Option<&'q TableAlias>,
        name: Cow<'q, Ident>,
        mut columns: Vec<Cow<'q, Ident>>,
    ) -> Option<Self> {
        let Some(alias) = alias else { return Some(Self { qualifier: name, columns }) };
        if alias.at.is_some() || alias.columns.len() > columns.len() {
            return None;
        }
        for (column, renamed) in columns.iter_mut().zip(&alias.columns) {
            if renamed.data_type.is_some() {
                return None;
            }
            *column = Cow::Borrowed(&renamed.name);
        }
        Some(Self { qualifier: Cow::Borrowed(&alias.name), columns })
    }

    /// The position of the column `expression` names, bare or qualified.
    fn position(&self, expression: &Expr) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| names_column(expression, &self.qualifier, column, true))
    }
}

/// The labels a list an arm crosses its rows with writes, before the type
/// of the column returning them is known.
enum LabelSource<'q> {
    /// `unnest(enum_range(NULL::<type>))`.
    EnumRange(&'q DataType),
    /// `(VALUES (<label>), ...)`, one expression per row.
    Values(Vec<&'q Expr>),
}

/// A list of labels an arm crosses its rows with, and the names it is read
/// by.
struct LabelList<'q> {
    named: Named<'q>,
    source: LabelSource<'q>,
}

/// What a grant arm's condition keeps.
enum Kept<'db, DB: DatabaseLike> {
    /// Every row.
    All,
    /// No row, the condition being `FALSE` or `NULL`.
    Nothing,
    /// The rows each listed column of which holds what it requires, each
    /// column listed once.
    In(Vec<(&'db DB::Column, Held)>),
}

/// What a condition requires one column to hold.
#[derive(Clone)]
enum Held {
    /// One of the values, which no `NULL` is.
    Values(Vec<Passed>),
    Null,
    NotNull,
}

impl Held {
    /// Narrows `self` to what `other` also requires, `false` where no value
    /// meets both.
    fn and(&mut self, other: Self) -> bool {
        match (&mut *self, other) {
            (Self::Values(values), Self::Values(others)) => {
                values.retain(|value| others.iter().any(|other| other.literal == value.literal));
                !values.is_empty()
            }
            (Self::Values(_) | Self::NotNull, Self::NotNull) | (Self::Null, Self::Null) => true,
            (Self::NotNull, values @ Self::Values(_)) => {
                *self = values;
                true
            }
            (Self::Values(_) | Self::NotNull, Self::Null)
            | (Self::Null, Self::Values(_) | Self::NotNull) => false,
        }
    }
}

impl<DB: DatabaseLike> Clone for Kept<'_, DB> {
    fn clone(&self) -> Self {
        match self {
            Self::All => Self::All,
            Self::Nothing => Self::Nothing,
            Self::In(sets) => Self::In(sets.clone()),
        }
    }
}

impl<'db, DB: DatabaseLike> Kept<'db, DB> {
    fn of(equality: Equality) -> Self {
        if equality == Equality::Holds { Self::All } else { Self::Nothing }
    }

    fn values(column: &'db DB::Column, values: Vec<Passed>) -> Self {
        if values.is_empty() {
            Self::Nothing
        } else {
            Self::In(vec![(column, Held::Values(values))])
        }
    }

    /// `<column> IN (<listed>)`, each value kept once and `NULL` matching
    /// no row.
    fn listed(column: &'db DB::Column, listed: Vec<Passed>) -> Self {
        let mut kept: Vec<Passed> = Vec::new();
        for passed in listed {
            if passed.literal != Literal::Null
                && !kept.iter().any(|seen| seen.literal == passed.literal)
            {
                kept.push(passed);
            }
        }
        Self::values(column, kept)
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Nothing, _) | (_, Self::Nothing) => Self::Nothing,
            (Self::All, kept) | (kept, Self::All) => kept,
            (Self::In(mut sets), Self::In(others)) => {
                for (column, other) in others {
                    let Some((_, held)) =
                        sets.iter_mut().find(|(seen, _)| core::ptr::eq(*seen, column))
                    else {
                        sets.push((column, other));
                        continue;
                    };
                    if !held.and(other) {
                        return Self::Nothing;
                    }
                }
                Self::In(sets)
            }
        }
    }

    /// `self OR other`, `None` where the two restrict anything but the same
    /// one column, or where one keeps its `NULL` and the other some of its
    /// values.
    fn or(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Some(Self::All),
            (Self::Nothing, kept) | (kept, Self::Nothing) => Some(kept),
            (Self::In(sets), Self::In(others)) => {
                let (Ok([(left, held)]), Ok([(right, other)])) =
                    (<[_; 1]>::try_from(sets), <[_; 1]>::try_from(others))
                else {
                    return None;
                };
                if !core::ptr::eq(left, right) {
                    return None;
                }
                let held = match (held, other) {
                    (Held::Values(mut values), Held::Values(others)) => {
                        for other in others {
                            if !values.iter().any(|value| value.literal == other.literal) {
                                values.push(other);
                            }
                        }
                        Held::Values(values)
                    }
                    (Held::Null, Held::Null) => Held::Null,
                    (Held::Values(_) | Held::NotNull, Held::NotNull)
                    | (Held::NotNull, Held::Values(_)) => Held::NotNull,
                    (Held::Null, Held::NotNull) | (Held::NotNull, Held::Null) => {
                        return Some(Self::All);
                    }
                    (Held::Values(_), Held::Null) | (Held::Null, Held::Values(_)) => return None,
                };
                Some(Self::In(vec![(left, held)]))
            }
        }
    }

    /// The restrictions the rows are kept by, `None` where no row is kept.
    fn restrictions(self) -> Option<Vec<Restriction<'db, DB>>> {
        match self {
            Self::Nothing => None,
            Self::All => Some(Vec::new()),
            Self::In(sets) => {
                Some(
                    sets.into_iter()
                        .map(|(column, held)| {
                            match held {
                                Held::Values(values) => {
                                    Restriction::In(LiteralSet {
                                        column,
                                        values: values
                                            .into_iter()
                                            .map(|passed| passed.value)
                                            .collect(),
                                    })
                                }
                                Held::Null => Restriction::Null(column),
                                Held::NotNull => Restriction::NotNull(column),
                            }
                        })
                        .collect(),
                )
            }
        }
    }
}

/// One side of a comparison in a grant arm.
enum Operand<'q, 'db, DB: DatabaseLike> {
    Column(&'db DB::Column),
    Passed(Passed),
    /// A passed value the body casts to `cast`.
    CastPassed {
        passed: Passed,
        cast: &'q DataType,
    },
    /// `current_setting(...)`, cast to `cast` where the body casts it.
    Setting {
        read: SettingRead,
        cast: Option<&'q DataType>,
    },
    Other,
}

/// A cast the body writes on a setting it reads, as written and as the type
/// each value it compares takes: the cast itself on the caller, and on a set
/// the element type of an array cast or the cast on each element.
#[derive(Clone, Copy)]
struct ReadCast<'q> {
    written: &'q DataType,
    compared: &'q DataType,
}

/// A set the body unpacks: the setting it reads, how it unpacks it and the
/// cast on its elements, where it writes one.
struct SetRead<'q> {
    read: SettingRead,
    packing: SetPacking,
    cast: Option<ReadCast<'q>>,
}

impl SetRead<'_> {
    /// Whether the body casts the elements, itself or through an accessor.
    fn is_cast(&self) -> bool {
        self.cast.is_some() || self.read.in_declared_type
    }
}

/// One arm as read, the table whose primary key it returns, where it returns
/// one, beside the arms it answers, none where its condition keeps no row.
type ReadArm<'db, DB> = (Option<&'db <DB as DatabaseLike>::Table>, Vec<HelperArm<'db, DB>>);

/// Whose rows one arm a grant arm answers keeps, with what keeping them
/// adds to the arm's other conditions.
type Grantee<'db, DB> = (GrantedTo<'db, DB>, Kept<'db, DB>);

/// An element of an `IN` list naming the request.
enum Listed<'q> {
    /// A read of a setting, with the cast the body writes on it.
    Read(SettingRead, Option<&'q DataType>),
    Literal(Passed),
}

/// A table, its primary key and the foreign key a column references them
/// through.
type ReferencedKey<'db, DB> = (
    &'db <DB as DatabaseLike>::Table,
    &'db <DB as DatabaseLike>::Column,
    &'db <DB as DatabaseLike>::ForeignKey,
);

/// A column of an arm's own table beside the column of a joined table it
/// equals.
type JoinedColumns<'db, DB> =
    (&'db <DB as DatabaseLike>::Column, &'db <DB as DatabaseLike>::Column);

/// The foreign key joining a row to an arm, with its columns.
type JoiningKey<'db, DB> = (&'db <DB as DatabaseLike>::ForeignKey, Vec<JoinedColumns<'db, DB>>);

/// The conjuncts of an arm on one row it joins, beside the columns joining
/// it.
struct Joining<'q, 'db, DB: DatabaseLike> {
    conditions: Vec<&'q Expr>,
    key: Vec<JoinedColumns<'db, DB>>,
}

/// The conjuncts of an arm on its own table, beside the rows it joins,
/// `None` where one of them keeps no row.
type JoinedConditions<'q, 'db, DB> = (Vec<&'q Expr>, Option<Vec<JoinedRow<'db, DB>>>);

/// A helper body's arms, with the term walking down a tree under them where
/// the body writes `WITH RECURSIVE`.
struct Body<'q> {
    arms: Vec<&'q Select>,
    walk: Option<Walk<'q>>,
}

/// The recursive term of a body, with the name and the one column it reads
/// the set by, and the final query where it reselects what the walk reached
/// rather than return it as `SELECT <column> FROM <walk>`.
struct Walk<'q> {
    name: &'q Ident,
    column: &'q Ident,
    term: &'q Select,
    reselection: Option<&'q Select>,
}

/// Answers helpers, following parent arms into the helpers they call.
struct Reader<'db, DB: DatabaseLike> {
    deployment: &'db Deployment,
    database: &'db DB,
    /// Whether a grant arm's exclusions are read rather than refused, in
    /// every helper answered.
    reads_exclusions: bool,
    /// The helpers being answered, outermost first.
    answering: Vec<&'db DB::Function>,
}

impl<'db, DB: DatabaseLike> Reader<'db, DB> {
    fn answer(
        &mut self,
        function: &'db DB::Function,
        arguments: &[Value],
    ) -> Result<HelperSet<'db, DB>, HelperSetError> {
        let database = self.database;
        let name = function.target_name().to_string();
        if self.answering.iter().any(|seen| core::ptr::eq(*seen, function)) {
            return Err(HelperSetError::CallCycle { function: name });
        }
        let language = function.stored_language();
        if language.as_deref() != Some("sql") {
            return Err(HelperSetError::Language {
                function: name,
                language: language.map(Cow::into_owned),
            });
        }
        let volatility = function.volatility();
        if volatility != FunctionBehavior::Stable {
            return Err(HelperSetError::Volatility { function: name, volatility });
        }
        if function.security_mode() != FunctionSecurity::Definer {
            return Err(HelperSetError::SecurityInvoker { function: name });
        }
        let Some(arity) = returned_arity(function, database) else {
            return Err(HelperSetError::ReturnType { function: name });
        };
        let Some(path) = function.search_path(database)? else {
            return Err(HelperSetError::UnpinnedSearchPath { function: name });
        };
        let parameters = parameters(function, arguments, &name, database)?;
        let owner = self.owner(function, &name)?;
        let usable_path = self.usable_path(&path, owner);
        let scope =
            Scope { function, name, path, usable_path, parameters, owner, accessing: Vec::new() };

        // A body bound at creation resolves its names with no path, so it is
        // not read as one written to run under the pinned path.
        let (written_body, body) = written_sql_body(function, database)?;
        let SqlBody::Statements(statements) = &body else {
            return Err(scope.arm_shape(&written(written_body)));
        };
        let [Statement::Query(query)] = statements.as_ref() else {
            return Err(scope.arm_shape(&written(written_body)));
        };
        let body = scope.body(query)?;
        // A walk adds one column, so a helper walking a tree returns one.
        if let Some(walk) = &body.walk
            && arity != 1
        {
            return Err(scope.arm_shape(walk.term));
        }

        self.answering.push(function);
        let read = body
            .arms
            .into_iter()
            .map(|select| self.arm(&scope, select, arity))
            .collect::<Result<Vec<_>, _>>();
        self.answering.pop();
        let mut read = read?;
        if let Some(walk) = &body.walk {
            for walk in self.walk_arms(&scope, walk)? {
                read.push((Some(walk.tree), vec![HelperArm::Walk(walk)]));
            }
        }
        let mut keyed = read.iter().map(|(keyed, _)| *keyed);
        let covered_table = keyed.next().flatten().filter(|first| {
            keyed.all(|other| other.is_some_and(|other| core::ptr::eq(other, *first)))
        });
        let mut arms: Vec<_> = read.into_iter().flat_map(|(_, arms)| arms).collect();
        let mut selection = None;
        if let Some(walk @ Walk { reselection: Some(last), .. }) = &body.walk {
            let kept = self.reselection(&scope, walk, last, covered_table)?;
            if kept.is_empty() {
                arms.clear();
            } else if !kept.iter().any(keeps_every_row) {
                selection = Some(kept);
            }
        }
        // A walk under arms keeping no row starts from nothing.
        if arms.iter().all(|arm| matches!(arm, HelperArm::Walk(_))) {
            arms.clear();
            selection = None;
        }
        Ok(HelperSet { arms, covered_table, selection })
    }

    /// Reads one arm returning `arity` columns, answering the table whose
    /// primary key it returns, where it returns one column holding one, and
    /// the arms it answers, none where its condition keeps no row.
    fn arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        arity: usize,
    ) -> Result<ReadArm<'db, DB>, HelperSetError> {
        if !plain_select(select) || select.projection.len() != arity {
            return Err(scope.arm_shape(select));
        }
        let [from] = select.from.as_slice() else {
            return Err(scope.arm_shape(select));
        };
        let shape = || scope.arm_shape(select);
        let mut lists = Vec::new();
        let mut joins = Vec::new();
        for join in &from.joins {
            match (join.global, is_cross(join), join_condition(join)) {
                (false, true, _) => {
                    lists.push(self.label_list(scope, &join.relation).ok_or_else(shape)?);
                }
                (false, false, Some(_)) => joins.push(join),
                _ => return Err(shape()),
            }
        }
        if let Some(call) = called_relation(&from.relation) {
            if !joins.is_empty() || select.selection.is_some() {
                return Err(shape());
            }
            return self.called_arm(scope, select, call, &lists);
        }
        let mut rows = vec![self.arm_table(scope, &from.relation, true, shape)?];
        for join in joins {
            rows.push(self.arm_table(scope, &join.relation, true, shape)?);
        }
        // PostgreSQL refuses two relations of one name, and a bare name two
        // rows hold.
        let qualifiers = rows.iter().map(|row| row.qualifier.as_ref());
        if named_twice(qualifiers, &lists)
            || rows_named(&rows, select).is_none()
            || names_list(select, &lists)
        {
            return Err(shape());
        }
        let table = &rows[0];
        let returned = self.returned(scope, select, &lists, |returned| {
            let column = table.column(returned);
            // A bare name a row and a list both hold is ambiguous.
            let ambiguous = matches!(unnest(returned), Expr::Identifier(_))
                && rows.iter().any(|row| row.column(returned).is_some());
            (column, ambiguous)
        })?;
        for row in &rows {
            self.require_select(scope, row.table, || row.columns_read(select))?;
        }
        let [Returned::Read(column)] = returned[..] else {
            return Ok((None, self.grant_arm(scope, select, &rows, &returned, None, None)?));
        };
        let keyed = self.keyed_table(table.table, column)?;
        let child = self.child_table(table.table, column)?;
        if let Some(child) = child
            && from.joins.is_empty()
            && let Some(Expr::In {
                expr, target: InTarget::Subquery(called), negated: false, ..
            }) = select.selection.as_ref().map(unnest)
            && !matches!(unnest(expr), Expr::Tuple(_))
            && self.set_elements(scope, called)?.is_none()
            && self.parent_comparison(table, column, child, expr)?
            && self.capping_key(table, &returned, expr)?.is_none()
        {
            let arm = self.parent_arm(scope, select, table, (column, child), expr, called)?;
            return Ok((keyed, vec![arm]));
        }
        Ok((keyed, self.grant_arm(scope, select, &rows, &returned, child, None)?))
    }

    /// What each item of `select` returns, a column `read` answers or a
    /// list of `lists` it names, each list returned once. `read` answers the
    /// column an item names, beside whether the item is a bare name a list
    /// could hold too.
    fn returned<S>(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        lists: &[LabelList<'_>],
        read: impl Fn(&Expr) -> (Option<S>, bool),
    ) -> Result<Vec<Returned<S>>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let mut returned = Vec::with_capacity(select.projection.len());
        let mut listed = vec![false; lists.len()];
        for (index, item) in select.projection.iter().enumerate() {
            let (SelectItem::UnnamedExpr(item) | SelectItem::ExprWithAlias { expr: item, .. }) =
                item
            else {
                return Err(shape());
            };
            let mut naming =
                lists.iter().enumerate().filter(|(_, list)| list.named.position(item).is_some());
            let (column, ambiguous) = read(item);
            match (column, naming.next(), naming.next()) {
                (Some(column), None, None) => returned.push(Returned::Read(column)),
                (None, Some((position, list)), None) if !ambiguous && !listed[position] => {
                    listed[position] = true;
                    let declared =
                        returned_type(scope.function, self.database, index).ok_or_else(shape)?;
                    let labels = self.labels(scope, &list.source, &declared).ok_or_else(shape)?;
                    returned.push(Returned::Labels(labels));
                }
                _ => return Err(shape()),
            }
        }
        // A list returned nowhere only repeats the rows.
        if listed.contains(&false) {
            return Err(shape());
        }
        Ok(returned)
    }

    /// Reads an arm whose `FROM` calls another helper, crossed with `lists`,
    /// `call` being the name it calls, its alias and its arguments.
    fn called_arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        (name, alias, arguments): (&ObjectName, Option<&TableAlias>, &[FunctionArg]),
        lists: &[LabelList<'_>],
    ) -> Result<ReadArm<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let Some(callee) = self.called(scope, name, arguments.len())? else {
            return Err(HelperSetError::UnresolvedCall {
                function: scope.name.clone(),
                call: name.to_string(),
            });
        };
        // A function in `FROM` is named by its own name, without its schema.
        let (function_name, quoted) = object_name_last_part(name).ok_or_else(shape)?;
        let function_name =
            Ident { quote_style: quoted.then_some('"'), ..Ident::new(function_name) };
        let declared: Vec<Cow<'_, Ident>> = callee
            .returned_columns(database)
            .map(|(column, _)| {
                let ident = Ident::new(column.name());
                Cow::Owned(Ident { quote_style: column.name_is_quoted().then_some('"'), ..ident })
            })
            .collect();
        let columns = if declared.is_empty() {
            vec![alias.map_or_else(
                || Cow::Owned(function_name.clone()),
                |alias| Cow::Borrowed(&alias.name),
            )]
        } else {
            declared
        };
        let callee_arity = columns.len();
        let named = Named::aliased(alias, Cow::Owned(function_name), columns).ok_or_else(shape)?;
        if named_twice([named.qualifier.as_ref()], lists) || names_list(select, lists) {
            return Err(shape());
        }
        let returned =
            self.returned(scope, select, lists, |returned| (named.position(returned), false))?;
        let call = Call { name, arguments };
        let Called { function, arguments, set } =
            self.called_set(scope, select, &[], &call, callee_arity)?;
        let keyed = match returned[..] {
            [Returned::Read(0)] if callee_arity == 1 => set.covered_table(),
            _ => None,
        };
        let arm = CalledArm { returned, function, arguments, set: Box::new(set) };
        Ok((keyed, vec![HelperArm::Called(arm)]))
    }

    /// The list of labels `relation` writes, `unnest(enum_range(NULL::<type>))`
    /// with PostgreSQL's own functions or `(VALUES (<label>), ...)` under an
    /// alias, one column per row, `None` where it writes no such list.
    fn label_list<'q>(
        &self,
        scope: &Scope<'db, DB>,
        relation: &'q TableFactor,
    ) -> Option<LabelList<'q>> {
        match relation {
            TableFactor::UNNEST {
                alias,
                array_exprs,
                with_offset: false,
                with_offset_alias: None,
                with_ordinality: false,
            } => {
                let [Expr::Function(range)] = array_exprs.as_slice() else { return None };
                let unnest_name = ObjectName::from(vec![Ident::new("unnest")]);
                if !self.reaches_catalog(scope, &unnest_name, "unnest", 1) {
                    return None;
                }
                let [FunctionArg::Unnamed(FunctionArgExpr::Expr(null))] =
                    self.catalog_arguments(scope, range, "enum_range")?.args.as_slice()
                else {
                    return None;
                };
                let Expr::Cast {
                    kind: CastKind::Cast | CastKind::DoubleColon,
                    expr,
                    data_type,
                    format: None,
                } = unnest(null)
                else {
                    return None;
                };
                let Expr::Value(null) = unnest(expr) else { return None };
                if !matches!(Literal::of(&null.value), Some(Literal::Null)) {
                    return None;
                }
                // A function returning one column names it after its alias.
                let column = alias.as_ref().map_or_else(
                    || Cow::Owned(Ident::new("unnest")),
                    |alias| Cow::Borrowed(&alias.name),
                );
                let named =
                    Named::aliased(alias.as_ref(), Cow::Owned(Ident::new("unnest")), vec![column])?;
                Some(LabelList { named, source: LabelSource::EnumRange(data_type) })
            }
            TableFactor::Derived { lateral: false, subquery, alias: Some(alias), sample: None } => {
                let SetExpr::Values(values) = subquery.body.as_ref() else { return None };
                if !plain_query(subquery) || values.explicit_row || values.value_keyword {
                    return None;
                }
                let mut labels = Vec::with_capacity(values.rows.len());
                for row in &values.rows {
                    let [label] = row.content.as_slice() else { return None };
                    labels.push(label);
                }
                let column = Cow::Owned(Ident::new("column1"));
                let named = Named::aliased(Some(alias), Cow::Borrowed(&alias.name), vec![column])?;
                Some(LabelList { named, source: LabelSource::Values(labels) })
            }
            _ => None,
        }
    }

    /// The labels `source` lists as values of `declared`, in the order it
    /// lists them, each once, `None` where `declared` is no enum type or the
    /// list writes anything but labels of it. `VALUES` reads as text unless
    /// one row casts its label, so one has to, and every cast has to name
    /// the declared type.
    fn labels(
        &self,
        scope: &Scope<'db, DB>,
        source: &LabelSource<'_>,
        declared: &DataType,
    ) -> Option<Vec<Value>> {
        let declared_labels = self.enum_labels(declared)?;
        let listed = match source {
            LabelSource::EnumRange(cast) => {
                if !self.is_declared_type(scope, cast, declared) {
                    return None;
                }
                declared_labels.clone()
            }
            LabelSource::Values(rows) => {
                let mut cast_once = false;
                let mut listed = Vec::with_capacity(rows.len());
                for row in rows {
                    let literal = match unnest(row) {
                        Expr::Cast {
                            kind: CastKind::Cast | CastKind::DoubleColon,
                            expr,
                            data_type,
                            format: None,
                        } => {
                            if !self.is_declared_type(scope, data_type, declared) {
                                return None;
                            }
                            cast_once = true;
                            unnest(expr)
                        }
                        literal => literal,
                    };
                    let Expr::Value(literal) = literal else { return None };
                    let Value::SingleQuotedString(label) = &literal.value else { return None };
                    listed.push(label.as_str());
                }
                if !cast_once {
                    return None;
                }
                listed
            }
        };
        let mut labels: Vec<&str> = Vec::with_capacity(listed.len());
        for label in listed {
            // PostgreSQL refuses a label the type does not declare.
            if !declared_labels.contains(&label) {
                return None;
            }
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        Some(labels.into_iter().map(|label| Value::SingleQuotedString(label.to_string())).collect())
    }

    /// The labels of the enum type `declared` names, in the order it declares
    /// them, an unqualified name naming one in `public`.
    fn enum_labels(&self, declared: &DataType) -> Option<Vec<&'db str>> {
        // PostgreSQL refuses a modifier on an enum type.
        let DataType::Custom(name, _) = declared else { return None };
        let (schema, name) = object_name_identifiers(name).ok()?;
        let schema = schema.map_or(("public", false), |schema| {
            (schema.value.as_str(), schema.quote_style.is_some())
        });
        let user_type = self.database.user_defined_types().find(|user_type| {
            identifiers_match(
                user_type.name(),
                user_type.name_is_quoted(),
                &name.value,
                name.quote_style.is_some(),
            ) && identifiers_match(
                user_type.schema().unwrap_or("public"),
                user_type.schema_is_quoted(),
                schema.0,
                schema.1,
            )
        })?;
        let enumeration = user_type.enumeration()?;
        (enumeration.kind() == EnumerationKind::Enum)
            .then(|| enumeration.into_members().map(EnumerationMember::label).collect())
    }

    /// The column `compared` names where its comparison with a helper's set
    /// caps an arm returning `returned`, `None` where it names the arm's
    /// grantee or parent. It caps where the column is the arm's table's
    /// primary key, which the arm returns and which references no other
    /// table through a single-column foreign key.
    fn capping_key(
        &self,
        table: &ArmTable<'_, 'db, DB>,
        returned: &[Returned<&'db DB::Column>],
        compared: &Expr,
    ) -> Result<Option<&'db DB::Column>, LookupError> {
        let database = self.database;
        let Some(column) = table.column(compared) else { return Ok(None) };
        let is_returned = returned.iter().any(
            |returned| matches!(returned, Returned::Read(read) if core::ptr::eq(*read, column)),
        );
        let is_primary = table
            .table
            .primary_key_column(database)?
            .is_some_and(|primary| core::ptr::eq(primary, column));
        let caps = is_returned
            && is_primary
            && single_column_foreign_keys(table.table, column, database)?.is_empty();
        Ok(caps.then_some(column))
    }

    /// The cap `<key> IN (SELECT <helper>(<arguments>))` or `<key> IN
    /// (SELECT * FROM <helper>(<arguments>))` writes on the arm's own key.
    fn key_cap(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        rows: &[ArmTable<'_, 'db, DB>],
        key: &'db DB::Column,
        subquery: &Query,
    ) -> Result<RowCap<'db, DB>, HelperSetError> {
        let call = selected_call(subquery)
            .or_else(|| projected_call(subquery))
            .ok_or_else(|| scope.arm_shape(select))?;
        let Called { function, arguments, set } = self.called_set(scope, select, rows, &call, 1)?;
        Ok(RowCap {
            operands: vec![CapOperand::Column(key)],
            function,
            arguments,
            set: Box::new(set),
            lift: None,
        })
    }

    /// Reads the final query of a walk reselecting the keys it reached,
    /// `SELECT <key> FROM <table> WHERE <key> IN (SELECT <column> FROM
    /// <walk>) [AND <conditions>]`, its conditions read as those of a grant
    /// arm returning the key, one arm per grantee they name and none where
    /// they keep no row. The walk has to reach keys of the table alone,
    /// which it does where every arm and the walk return them, so the
    /// reselection keeps every one of them the conditions keep.
    fn reselection(
        &mut self,
        scope: &Scope<'db, DB>,
        walk: &Walk<'_>,
        select: &Select,
        covered_table: Option<&'db DB::Table>,
    ) -> Result<Vec<GrantArm<'db, DB>>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let ([item], [from], Some(selection)) =
            (select.projection.as_slice(), select.from.as_slice(), &select.selection)
        else {
            return Err(shape());
        };
        if !plain_select(select) {
            return Err(shape());
        }
        let table = self.arm_table(scope, &from.relation, from.joins.is_empty(), shape)?;
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(shape());
        };
        let Some(key) = table.table.primary_key_column(database)? else { return Err(shape()) };
        let is_key = |expression: &Expr| {
            table.column(expression).is_some_and(|column| core::ptr::eq(column, key))
        };
        if !is_key(returned) {
            return Err(shape());
        }
        let mut conjuncts = Vec::new();
        conjunction(selection, &mut conjuncts);
        let Some(reached) = conjuncts.iter().copied().find(|conjunct| {
            matches!(conjunct, Expr::In { expr, target: InTarget::Subquery(subquery), negated: false, .. }
                if reads_walk(subquery, walk) && is_key(expr))
        }) else {
            return Err(shape());
        };
        if !covered_table.is_some_and(|covered| core::ptr::eq(covered, table.table)) {
            return Err(HelperSetError::UncoveredReselection {
                function: scope.name.clone(),
                table: target_name_of_table(table.table).to_string(),
            });
        }
        // The walk's column the subquery returns is no column of the table.
        self.require_select(scope, table.table, || {
            let mut read = vec![key];
            for conjunct in conjuncts.iter().filter(|conjunct| !core::ptr::eq(**conjunct, reached))
            {
                read.extend(table.columns_read(*conjunct));
            }
            read
        })?;
        let rows = [table];
        self.grant_arm(scope, select, &rows, &[Returned::Read(key)], None, Some(reached))
    }

    /// Reads the recursive term of a walk, one arm per edge it follows.
    ///
    /// The term joins the walk to one edge, walking down as
    /// `SELECT <child> FROM <table> JOIN <walk> ON <parent column> =
    /// <walk>.<column>` and up as
    /// `SELECT <parent column> FROM <table> JOIN <walk> ON <child> =
    /// <walk>.<column>`, the join either way round, or reads a `UNION` of edges
    /// from a `LATERAL` subquery after the walk, each
    /// `SELECT <added> FROM <table> WHERE <compared> = <walk>.<column>`.
    /// PostgreSQL refuses the walk's name twice in the term and in a subquery
    /// of it, so a step over several edges compares each with the walk's
    /// column from inside a `LATERAL` subquery.
    fn walk_arms(
        &self,
        scope: &Scope<'db, DB>,
        walk: &Walk<'_>,
    ) -> Result<Vec<WalkArm<'db, DB>>, HelperSetError> {
        let term = walk.term;
        let shape = || scope.arm_shape(term);
        let ([item], None) = (term.projection.as_slice(), &term.selection) else {
            return Err(shape());
        };
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(shape());
        };
        if !plain_select(term) {
            return Err(shape());
        }
        match term.from.as_slice() {
            [from] => {
                let [join] = from.joins.as_slice() else { return Err(shape()) };
                if join.global {
                    return Err(shape());
                }
                match &join.join_operator {
                    JoinOperator::Join(JoinConstraint::On(on))
                    | JoinOperator::Inner(JoinConstraint::On(on)) => {
                        let edge = self.joined_edge(
                            scope,
                            walk,
                            (&from.relation, &join.relation),
                            returned,
                            on,
                        )?;
                        Ok(vec![edge])
                    }
                    JoinOperator::CrossJoin(JoinConstraint::None) => {
                        self.lateral_edges(scope, walk, &from.relation, &join.relation, returned)
                    }
                    _ => Err(shape()),
                }
            }
            [from, step] if from.joins.is_empty() && step.joins.is_empty() => {
                self.lateral_edges(scope, walk, &from.relation, &step.relation, returned)
            }
            _ => Err(shape()),
        }
    }

    /// Reads the edge a term joining the walk to one table follows, `ON`
    /// comparing a column of the table with the walk's column.
    fn joined_edge(
        &self,
        scope: &Scope<'db, DB>,
        walk: &Walk<'_>,
        (from, joined): (&TableFactor, &TableFactor),
        returned: &Expr,
        on: &Expr,
    ) -> Result<WalkArm<'db, DB>, HelperSetError> {
        let term = walk.term;
        let shape = || scope.arm_shape(term);
        let (relation, qualifier) =
            match (walk_qualifier(from, walk.name), walk_qualifier(joined, walk.name)) {
                (None, Some(qualifier)) => (from, qualifier),
                (Some(qualifier), None) => (joined, qualifier),
                _ => return Err(shape()),
            };
        let Some(compared) = compared_with_walk(on, qualifier, walk.column) else {
            return Err(shape());
        };
        // A bare name the walk's column shares is ambiguous in the join.
        let ambiguous = |expression: &Expr| matches!(unnest(expression), Expr::Identifier(ident) if same_ident(ident, walk.column));
        if ambiguous(compared) || ambiguous(returned) {
            return Err(shape());
        }
        let table = self.arm_table(scope, relation, true, shape)?;
        self.edge_arm(scope, term, &table, returned, compared)
    }

    /// Reads the edges `FROM <walk> CROSS JOIN LATERAL (<edges>) <step>`
    /// follows, the term returning the step's one column.
    fn lateral_edges(
        &self,
        scope: &Scope<'db, DB>,
        walk: &Walk<'_>,
        walked: &TableFactor,
        step: &TableFactor,
        returned: &Expr,
    ) -> Result<Vec<WalkArm<'db, DB>>, HelperSetError> {
        let shape = || scope.arm_shape(walk.term);
        let (
            Some(qualifier),
            TableFactor::Derived { lateral: true, subquery, alias: Some(alias), sample: None },
        ) = (walk_qualifier(walked, walk.name), step)
        else {
            return Err(shape());
        };
        if alias.at.is_some() || same_ident(&alias.name, qualifier) {
            return Err(shape());
        }
        let mut edges = Vec::new();
        union_arms(subquery, &mut edges).map_err(|edge| scope.arm_shape(&edge))?;
        let column = match alias.columns.as_slice() {
            [] => edges.first().and_then(|first| output_name(first)).ok_or_else(shape)?,
            [TableAliasColumnDef { name, data_type: None }] => name,
            _ => return Err(shape()),
        };
        // A bare name the walk's column shares is ambiguous beside the walk.
        if !names_column(returned, &alias.name, column, !same_ident(column, walk.column)) {
            return Err(shape());
        }
        edges
            .into_iter()
            .map(|edge| self.lateral_edge(scope, edge, qualifier, walk.column))
            .collect()
    }

    /// Reads one edge of a `LATERAL` step, `SELECT <added> FROM <table> WHERE
    /// <compared> = <walk>.<column>` either way round, where `qualifier` names
    /// the walk.
    fn lateral_edge(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        qualifier: &Ident,
        column: &Ident,
    ) -> Result<WalkArm<'db, DB>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let ([item], [from], Some(selection)) =
            (select.projection.as_slice(), select.from.as_slice(), &select.selection)
        else {
            return Err(shape());
        };
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(shape());
        };
        if !plain_select(select) {
            return Err(shape());
        }
        let table = self.arm_table(scope, &from.relation, from.joins.is_empty(), shape)?;
        // A table named like the walk hides it from the subquery.
        if same_ident(&table.qualifier, qualifier) {
            return Err(shape());
        }
        let Some(compared) = compared_with_walk(selection, qualifier, column) else {
            return Err(shape());
        };
        self.edge_arm(scope, select, &table, returned, compared)
    }

    /// Reads the edge of `table` adding `returned` of each row whose
    /// `compared` the set holds, and checks the owner reads both columns.
    fn edge_arm(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        returned: &Expr,
        compared: &Expr,
    ) -> Result<WalkArm<'db, DB>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let (Some(returned), Some(compared)) = (table.column(returned), table.column(compared))
        else {
            return Err(shape());
        };
        let Some(walk) = self.edge(table.table, returned, compared)? else {
            return Err(shape());
        };
        self.require_select(scope, table.table, || vec![walk.child, walk.column])?;
        Ok(walk)
    }

    /// The walk adding `returned` of each row of `table` whose `compared` the
    /// set holds, `None` where the two columns draw no edge of a tree.
    ///
    /// The child is the table's primary key where one of the two is, else the
    /// one a unique key covers alone. The parent column names a row of the
    /// tree as [`tree_reference`](Self::tree_reference) reads it. The child is
    /// the tree's primary key where the table is the tree, and otherwise names
    /// a row of the same tree from an edge table.
    fn edge(
        &self,
        table: &'db DB::Table,
        returned: &'db DB::Column,
        compared: &'db DB::Column,
    ) -> Result<Option<WalkArm<'db, DB>>, LookupError> {
        let database = self.database;
        let primary = table.primary_key_column(database)?;
        let is_primary = |column| primary.is_some_and(|primary| core::ptr::eq(primary, column));
        let direction = match (is_primary(returned), is_primary(compared)) {
            (true, false) => WalkDirection::Down,
            (false, true) => WalkDirection::Up,
            (true, true) => return Ok(None),
            (false, false) => {
                match (
                    holds_key(table, &[returned], database)?,
                    holds_key(table, &[compared], database)?,
                ) {
                    (true, false) => WalkDirection::Down,
                    (false, true) => WalkDirection::Up,
                    _ => return Ok(None),
                }
            }
        };
        let (child, column) = match direction {
            WalkDirection::Down => (returned, compared),
            WalkDirection::Up => (compared, returned),
        };
        let Some((tree, key, foreign_key)) = self.tree_reference(table, column)? else {
            return Ok(None);
        };
        let drawn = if core::ptr::eq(tree, table) {
            core::ptr::eq(child, key)
        } else {
            self.tree_reference(table, child)?
                .is_some_and(|(referenced, _, _)| core::ptr::eq(referenced, tree))
        };
        Ok(drawn.then_some(WalkArm { table, tree, key, child, column, foreign_key, direction }))
    }

    /// The table whose row `column` of `table` names, its one-column primary
    /// key and the first foreign key the table declares naming it, where
    /// every foreign key holding the column at the position of a table's
    /// one-column primary key references the same table. The key answered
    /// checks every row whose column is not `NULL`, holding that column alone,
    /// matching `FULL` or holding its other columns `NOT NULL`, so the column
    /// names a row of the table whenever it holds a value.
    fn tree_reference(
        &self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Result<Option<ReferencedKey<'db, DB>>, LookupError> {
        let database = self.database;
        let mut tree: Option<&'db DB::Table> = None;
        let mut checked = None;
        for foreign_key in table.foreign_keys(database)? {
            let Some(referenced_column) =
                foreign_key.referenced_column_for_host_column(database, column)?
            else {
                continue;
            };
            let referenced = foreign_key.referenced_table(database)?;
            if !referenced
                .primary_key_column(database)?
                .is_some_and(|primary| core::ptr::eq(primary, referenced_column))
            {
                continue;
            }
            if tree.is_some_and(|tree| !core::ptr::eq(tree, referenced)) {
                return Ok(None);
            }
            tree = Some(referenced);
            if checked.is_none() && checks_every_row(foreign_key, column, database)? {
                checked = Some((referenced, referenced_column, foreign_key));
            }
        }
        Ok(checked)
    }

    /// The table whose primary key `column` of `table` holds, `table` itself
    /// where `column` is its primary key, else the table whose row it names
    /// as [`tree_reference`](Self::tree_reference) reads it, through a
    /// foreign key of any width checking every value it holds.
    fn keyed_table(
        &self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Result<Option<&'db DB::Table>, LookupError> {
        if table
            .primary_key_column(self.database)?
            .is_some_and(|primary| core::ptr::eq(primary, column))
        {
            return Ok(Some(table));
        }
        Ok(self.tree_reference(table, column)?.map(|(referenced, _, _)| referenced))
    }

    /// The table whose row `column` of `table` names in an arm a parent arm
    /// may return it from, `table` itself where `column` is its primary key,
    /// else the other table whose primary key the column references through
    /// its one single-column foreign key, where a unique key of `table`
    /// covers the column alone.
    fn child_table(
        &self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Result<Option<&'db DB::Table>, LookupError> {
        let database = self.database;
        if table.primary_key_column(database)?.is_some_and(|primary| core::ptr::eq(primary, column))
        {
            return Ok(Some(table));
        }
        let Some((referenced, _, _)) = self.referenced_key(table, column)? else {
            return Ok(None);
        };
        let names_child =
            !core::ptr::eq(referenced, table) && holds_key(table, &[column], database)?;
        Ok(names_child.then_some(referenced))
    }

    /// Whether comparing `compared` with a helper's set, in an arm returning
    /// `key`, a row of `child`, reads as a parent arm. It always does for the
    /// arm's own primary key, and for a row of another table where `compared`
    /// is another column with one single-column foreign key.
    fn parent_comparison(
        &self,
        table: &ArmTable<'_, 'db, DB>,
        key: &'db DB::Column,
        child: &'db DB::Table,
        compared: &Expr,
    ) -> Result<bool, LookupError> {
        let database = self.database;
        if core::ptr::eq(child, table.table) {
            return Ok(true);
        }
        let Some(column) = table.column(compared) else {
            return Ok(false);
        };
        Ok(!core::ptr::eq(column, key)
            && single_column_foreign_keys(table.table, column, database)?.len() == 1)
    }

    /// The table, its primary key and the foreign key `column` of `table`
    /// references it through, where the column's one single-column foreign
    /// key references that table's one-column primary key.
    fn referenced_key(
        &self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Result<Option<ReferencedKey<'db, DB>>, LookupError> {
        let database = self.database;
        let [foreign_key] = single_column_foreign_keys(table, column, database)?[..] else {
            return Ok(None);
        };
        let referenced = foreign_key.referenced_table(database)?;
        let (Some(primary), Some(referenced_column)) = (
            referenced.primary_key_column(database)?,
            foreign_key.referenced_columns(database)?.next(),
        ) else {
            return Ok(None);
        };
        Ok(core::ptr::eq(primary, referenced_column).then_some((referenced, primary, foreign_key)))
    }

    /// Resolves the one table an arm reads, and checks the owner reads all
    /// of it, `shape` naming the refusal of anything else.
    fn arm_table<'q>(
        &self,
        scope: &Scope<'db, DB>,
        relation: &'q TableFactor,
        unjoined: bool,
        shape: impl Fn() -> HelperSetError,
    ) -> Result<ArmTable<'q, 'db, DB>, HelperSetError> {
        let database = self.database;
        let Some((name, alias)) = plain_table(relation).filter(|_| unjoined) else {
            return Err(shape());
        };
        let resolve = |path: &SearchPath<'db>| {
            resolve_relation_object_name_on_path(name, database, path, database.identifier_case())
        };
        let table = match resolve(&scope.usable_path)? {
            Some(Relation::Table(table)) => table,
            Some(_) => return Err(shape()),
            None => {
                // A table only a schema the owner cannot use holds is not
                // found.
                if let Some(Relation::Table(hidden)) = resolve(&scope.path)? {
                    self.require_usage(scope, table_schema(hidden))?;
                }
                return Err(shape());
            }
        };
        self.require_usage(scope, table_schema(table))?;
        let refuse = |reason: fn(String, String, String) -> HelperSetError| {
            reason(
                scope.name.clone(),
                scope.owner.name().to_string(),
                target_name_of_table(table).to_string(),
            )
        };
        // `Lookup` is not met for a table the database resolved itself, and
        // every other answer leaves the owner bound by some policy.
        if !matches!(
            table.row_security(RowStatement::Select, scope.owner, database),
            Ok(RowSecurity::Exempt(_))
        ) {
            return Err(refuse(|function, owner, table| {
                HelperSetError::NotExempt { function, owner, table }
            }));
        }
        let qualifier = match alias {
            Some(alias) => Cow::Borrowed(alias),
            None => Cow::Owned(Ident::new(table.table_name())),
        };
        let columns = table.columns(database)?.collect();
        Ok(ArmTable { table, qualifier, columns, case: database.identifier_case() })
    }

    /// Reads `<column> IN (SELECT <helper>(<arguments>))` in an arm returning
    /// `key`, which names a row of `child_table`.
    fn parent_arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        (key, child_table): (&'db DB::Column, &'db DB::Table),
        compared: &Expr,
        subquery: &Query,
    ) -> Result<HelperArm<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let Some(column) = table.column(compared) else {
            return Err(shape());
        };
        let foreign_key = match single_column_foreign_keys(table.table, column, database)?[..] {
            [foreign_key] => Some(foreign_key),
            // Without a foreign key the set names the parent, and the key the
            // arm returns names no row but its own.
            [] if !core::ptr::eq(column, key) => None,
            _ => return Err(shape()),
        };
        let call = projected_call(subquery).ok_or_else(shape)?;
        let Called { function: callee, arguments, set } =
            self.called_set(scope, select, core::slice::from_ref(table), &call, 1)?;
        let parent_table = match foreign_key {
            Some(foreign_key) => foreign_key.referenced_table(database)?,
            None => {
                set.covered_table().ok_or_else(|| {
                    HelperSetError::UncoveredParent {
                        function: scope.name.clone(),
                        column: column.column_name().to_string(),
                        called: callee.target_name().to_string(),
                    }
                })?
            }
        };
        Ok(HelperArm::Parent(ParentArm {
            table: table.table,
            key,
            child_table,
            column,
            foreign_key,
            parent_table,
            function: callee,
            arguments,
            set: Box::new(set),
        }))
    }

    /// Reads `<column> IN (SELECT <helper>(<arguments>))` in a grant arm,
    /// the column holding a grantee of the helper's set.
    fn grantee_set(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        compared: &Expr,
        subquery: &Query,
    ) -> Result<GranteeSet<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let Some(column) = table.column(compared) else {
            return Err(shape());
        };
        let foreign_key = match single_column_foreign_keys(table.table, column, database)?[..] {
            [] => None,
            [foreign_key] => Some(foreign_key),
            _ => return Err(shape()),
        };
        let call = projected_call(subquery).ok_or_else(shape)?;
        let Called { function, arguments, set } =
            self.called_set(scope, select, core::slice::from_ref(table), &call, 1)?;
        Ok(GranteeSet { column, foreign_key, function, arguments, set: Box::new(set) })
    }

    /// The helper `call` reaches, the arguments it passes, each a parameter
    /// substituted or a literal, and what the helper answers for them, the
    /// helper returning `arity` columns. `rows` holds the tables whose
    /// columns the call may name.
    fn called_set(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        rows: &[ArmTable<'_, 'db, DB>],
        call: &Call<'_>,
        arity: usize,
    ) -> Result<Called<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let mut arguments = Vec::new();
        for argument in call.arguments {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(argument)) = argument else {
                return Err(shape());
            };
            // A column of a table takes the name before a parameter does,
            // and passes a value of each row.
            if rows.iter().any(|row| row.column(argument).is_some()) {
                return Err(shape());
            }
            let passed = scope.parameter(argument).map(|(passed, _)| passed.value.clone());
            let literal = || {
                let Expr::Value(value) = argument else { return None };
                Literal::of(&value.value).map(|_| value.value.clone())
            };
            arguments.push(passed.or_else(literal).ok_or_else(shape)?);
        }
        let Some(callee) = self.called(scope, call.name, arguments.len())? else {
            return Err(HelperSetError::UnresolvedCall {
                function: scope.name.clone(),
                call: call.name.to_string(),
            });
        };
        // A helper returning neither `SETOF` nor `TABLE` is refused as it is
        // answered below.
        if returned_arity(callee, database).is_some_and(|returned| returned != arity) {
            return Err(shape());
        }
        let set = self.answer(callee, &arguments)?;
        Ok(Called { function: callee, arguments, set })
    }

    /// The one function a call written as `written` reaches under the path
    /// the body runs with, which the owner may execute, `None` where it
    /// reaches none or several.
    fn called(
        &self,
        scope: &Scope<'db, DB>,
        written: &ObjectName,
        arguments: usize,
    ) -> Result<Option<&'db DB::Function>, HelperSetError> {
        let database = self.database;
        let name = object_name_last_part(written).map_or("", |(name, _)| name);
        let reached = |path: &SearchPath<'db>| {
            let path = || path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
            let mut reached = database.functions_named(name).filter(move |function| {
                call_reaches(*function, written, arguments, path, database.identifier_case())
            });
            match (reached.next(), reached.next()) {
                (Some(callee), None) => Some(callee),
                _ => None,
            }
        };

        let Some(callee) = reached(&scope.usable_path) else {
            // A function only a schema the owner cannot use holds is not found.
            if let Some(hidden) = reached(&scope.path) {
                self.require_usage(scope, function_schema(&hidden.target_name()))?;
            }
            return Ok(None);
        };
        self.require_usage(scope, function_schema(&callee.target_name()))?;
        if !scope.owner.is_superuser() && !callee.can_execute(scope.owner, database)? {
            return Err(HelperSetError::MissingExecute {
                function: scope.name.clone(),
                owner: scope.owner.name().to_string(),
                called: callee.target_name().to_string(),
            });
        }
        Ok(Some(callee))
    }

    /// The role that owns `function`, which `name` names.
    fn owner(
        &self,
        function: &'db DB::Function,
        name: &str,
    ) -> Result<&'db DB::Role, HelperSetError> {
        let database = self.database;
        let Some(owner) = function.owner(database)? else {
            return Err(HelperSetError::UnsettledOwner { function: name.to_string() });
        };
        database.role(owner).ok_or_else(|| {
            HelperSetError::UndeclaredOwner { function: name.to_string(), owner: owner.to_string() }
        })
    }

    /// The entries of `path` whose schema `owner` holds `USAGE` on, or which
    /// the database does not declare.
    fn usable_path(&self, path: &SearchPath<'db>, owner: &'db DB::Role) -> SearchPath<'db> {
        path.iter()
            .filter(|(schema, quoted)| {
                self.declared_schema(schema, *quoted)
                    .is_none_or(|schema| schema.has_usage(owner, self.database))
            })
            .cloned()
            .collect()
    }

    /// The schema `(name, quoted)` names, or `None` where the database
    /// declares none by that name, as it declares no `pg_catalog`.
    fn declared_schema(&self, name: &str, quoted: bool) -> Option<&'db DB::Schema> {
        self.database
            .schemas()
            .find(|schema| identifiers_match(schema.name(), schema.name_is_quoted(), name, quoted))
    }

    /// Refuses the answer where the owner lacks `USAGE` on `schema`.
    fn require_usage(
        &self,
        scope: &Scope<'db, DB>,
        (name, quoted): (&str, bool),
    ) -> Result<(), HelperSetError> {
        match self.declared_schema(name, quoted) {
            Some(schema) if !schema.has_usage(scope.owner, self.database) => {
                Err(HelperSetError::MissingUsage {
                    function: scope.name.clone(),
                    owner: scope.owner.name().to_string(),
                    schema: schema.name().to_string(),
                })
            }
            _ => Ok(()),
        }
    }

    /// Refuses the answer where the owner holds `SELECT` neither on `table`
    /// nor on every column of it `read` answers the arm reads.
    fn require_select(
        &self,
        scope: &Scope<'db, DB>,
        table: &'db DB::Table,
        read: impl FnOnce() -> Vec<&'db DB::Column>,
    ) -> Result<(), HelperSetError> {
        let held = table.privileges(scope.owner, self.database)?.select;
        if held.on_table() || read().iter().all(|column| held.on_column(column)) {
            return Ok(());
        }
        Err(HelperSetError::MissingSelect {
            function: scope.name.clone(),
            owner: scope.owner.name().to_string(),
            table: target_name_of_table(table).to_string(),
        })
    }

    /// Reads a grant arm returning `returned`, answering one arm per grantee
    /// an `IN` list naming the request lists and none where its conditions
    /// keep no row, each as an `A`. An arm returning a row of `child` from a
    /// helper's set is a parent arm where
    /// [`parent_comparison`](Self::parent_comparison) reads it as one, which
    /// takes no other condition, so such a set beside one is refused unless
    /// [`capping_key`](Self::capping_key) reads it as a cap. `rows` holds
    /// the arm's table and then each table it joins. `reached` is the
    /// conjunct of a walk's final query keeping the keys the walk reached,
    /// which is no condition of the arm.
    fn grant_arm<A: From<GrantArm<'db, DB>>>(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        rows: &[ArmTable<'_, 'db, DB>],
        returned: &[Returned<&'db DB::Column>],
        child: Option<&'db DB::Table>,
        reached: Option<&Expr>,
    ) -> Result<Vec<A>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let table = rows.first().ok_or_else(shape)?;
        let (own, joined_rows) = self.joined_rows(scope, select, rows, reached)?;
        let mut grantees = None;
        let mut gates = Gates::default();
        let mut caps = Vec::new();
        let mut exclusions = Vec::new();
        let mut conditions = Vec::new();
        for conjunct in own {
            if let Some(cap) = self.cap(scope, select, rows, conjunct)? {
                caps.push(cap);
                continue;
            }
            if let Some(exclusion) = self.exclusion(scope, select, table, conjunct)? {
                // `NOT IN` an empty set holds for every row, `NULL` too.
                if !exclusion.set.arms.is_empty() {
                    exclusions.push(exclusion);
                }
                continue;
            }
            let named = if let Some(request) = self.setting_comparison(scope, table, conjunct)? {
                Some(vec![(GrantedTo::Request(request), Kept::All)])
            } else if let Some(listed) = self.request_list(scope, table, conjunct)? {
                Some(listed)
            } else if let Expr::In {
                expr,
                target: InTarget::Subquery(called),
                negated: false,
                ..
            } = unnest(conjunct)
            {
                if let Some(key) = self.capping_key(table, returned, expr)? {
                    caps.push(self.key_cap(scope, select, rows, key, called)?);
                    continue;
                }
                if let (Some(child), [Returned::Read(column)]) = (child, returned)
                    && self.parent_comparison(table, column, child, expr)?
                {
                    return Err(shape());
                }
                let grantees = self.grantee_set(scope, select, table, expr, called)?;
                Some(vec![(GrantedTo::Set(grantees), Kept::All)])
            } else {
                None
            };
            if let Some(named) = named {
                if grantees.replace(named).is_some() {
                    return Err(shape());
                }
            } else if let Some(read) = self.gate(scope, table, conjunct)? {
                if !gates.add(read) {
                    return Err(shape());
                }
            } else {
                conditions.push(conjunct);
            }
        }
        let mut anti_joins = Vec::new();
        let mut filters = Vec::new();
        for condition in conditions {
            match not_exists(condition) {
                Some(blocking) => {
                    anti_joins.extend(self.anti_join(scope, table, condition, blocking)?);
                }
                None => filters.push(condition),
            }
        }
        let (kept, time_bounds) = self.filter(scope, table, filters)?;
        let Some(joined_rows) = joined_rows.filter(|_| !gates.keeps_nothing) else {
            return Ok(Vec::new());
        };
        let mut grantees = grantees.unwrap_or_else(|| vec![(GrantedTo::Anyone, Kept::All)]);
        let caps: Arc<[RowCap<'db, DB>]> = caps.into();
        let anti_joins: Arc<[AntiJoin<'db, DB>]> = anti_joins.into();
        let exclusions: Arc<[ExcludedSet<'db, DB>]> = exclusions.into();
        let joined_rows: Arc<[JoinedRow<'db, DB>]> = joined_rows.into();
        let grant = |(granted_to, own): Grantee<'db, DB>,
                     kept: Kept<'db, DB>,
                     gate: Option<CallerGate<'db>>,
                     time_bounds: Vec<TimeBound<'db, DB>>| {
            let restrictions = kept.and(own).restrictions()?;
            Some(A::from(GrantArm {
                table: table.table,
                returned: returned.to_vec(),
                granted_to,
                gate,
                restrictions,
                time_bounds,
                caps: Arc::clone(&caps),
                anti_joins: Arc::clone(&anti_joins),
                exclusions: Arc::clone(&exclusions),
                joined_rows: Arc::clone(&joined_rows),
            }))
        };
        // Every grantee but the last takes its own copy of the conditions.
        let last = grantees.pop();
        let mut arms = Vec::with_capacity(grantees.len() + 1);
        for grantee in grantees {
            arms.extend(grant(grantee, kept.clone(), gates.gate.clone(), time_bounds.clone()));
        }
        arms.extend(last.and_then(|last| grant(last, kept, gates.gate, time_bounds)));
        Ok(arms)
    }

    /// The exclusion `conjunct` writes on a column of the arm's `table`, if
    /// it writes one, `<column> NOT IN (SELECT <helper>(<arguments>))` or
    /// another spelling [`excluded_operands`] reads, refused where the
    /// reading leaves exclusions unread. Excluding the elements of a setting
    /// is no exclusion.
    fn exclusion(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<ExcludedSet<'db, DB>>, HelperSetError> {
        let Some((compared, subquery)) = excluded_operands(conjunct) else { return Ok(None) };
        let (Some(column), Some(call)) = (table.column(compared), projected_call(subquery)) else {
            return Ok(None);
        };
        if self.set_elements(scope, subquery)?.is_some() {
            return Ok(None);
        }
        if !self.reads_exclusions {
            return Err(HelperSetError::UnreadExclusion {
                function: scope.name.clone(),
                condition: conjunct.to_string(),
            });
        }
        let Called { function, arguments, set } =
            self.called_set(scope, select, core::slice::from_ref(table), &call, 1)?;
        Ok(Some(ExcludedSet { column, function, arguments, set: Box::new(set) }))
    }

    /// Splits the conditions of an arm reading `rows`, its table and then
    /// each table it joins, into the conjuncts on the arm's own table and the
    /// rows it joins, `None` in place of the rows where one of them keeps no
    /// row. The conjunct `reached` is left out.
    fn joined_rows<'q>(
        &self,
        scope: &Scope<'db, DB>,
        select: &'q Select,
        rows: &[ArmTable<'_, 'db, DB>],
        reached: Option<&Expr>,
    ) -> Result<JoinedConditions<'q, 'db, DB>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let (table, joined) = rows.split_first().ok_or_else(shape)?;
        // An inner join keeps the rows its condition and `WHERE` keep alike,
        // and a list of labels crossed with them keeps every one.
        let mut conjuncts = Vec::new();
        for join in select.from.iter().flat_map(|from| &from.joins).filter(|join| !is_cross(join)) {
            conjunction(join_condition(join).ok_or_else(shape)?, &mut conjuncts);
        }
        if let Some(selection) = &select.selection {
            conjunction(selection, &mut conjuncts);
        }
        conjuncts
            .retain(|conjunct| reached.is_none_or(|reached| !core::ptr::eq(*conjunct, reached)));
        let mut own = Vec::new();
        let mut joining: Vec<Joining<'_, 'db, DB>> =
            joined.iter().map(|_| Joining { conditions: Vec::new(), key: Vec::new() }).collect();
        for conjunct in conjuncts {
            // The subquery of an anti-join resolves names on its own table
            // first, so it is read against the arm's table alone.
            if not_exists(conjunct).is_some() {
                own.push(conjunct);
                continue;
            }
            match rows_named(rows, conjunct).ok_or_else(shape)?.as_slice() {
                [] | [0] => own.push(conjunct),
                // A cap reads the columns of every row as its operands.
                _ if cap_shape(conjunct).is_some() => own.push(conjunct),
                [row] => joining[row - 1].conditions.push(conjunct),
                _ => {
                    let (row, (host, referenced)) = Self::key_equality(table, joined, conjunct)
                        .ok_or_else(|| scope.irreducible(conjunct))?;
                    let key = &mut joining[row].key;
                    if !key.iter().any(|(seen_host, seen_referenced)| {
                        core::ptr::eq(*seen_host, host)
                            && core::ptr::eq(*seen_referenced, referenced)
                    }) {
                        key.push((host, referenced));
                    }
                }
            }
        }
        let mut keys = Vec::with_capacity(joined.len());
        for (row, joining) in joined.iter().zip(&joining) {
            keys.push(self.joining_key(table.table, row.table, &joining.key)?.ok_or_else(shape)?);
        }
        let mut joined_rows = Vec::with_capacity(joined.len());
        for ((row, joining), (foreign_key, columns)) in joined.iter().zip(&joining).zip(keys) {
            let (kept, time_bounds) =
                self.filter(scope, row, joining.conditions.iter().copied())?;
            joined_rows.push(kept.restrictions().map(|restrictions| {
                JoinedRow { table: row.table, foreign_key, columns, restrictions, time_bounds }
            }));
        }
        Ok((own, joined_rows.into_iter().collect()))
    }

    /// What `conditions` keep of `table`, beside the bounds they set against
    /// the statement time.
    fn filter<'c>(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conditions: impl IntoIterator<Item = &'c Expr>,
    ) -> Result<(Kept<'db, DB>, Vec<TimeBound<'db, DB>>), HelperSetError> {
        let mut kept = Kept::All;
        let mut time_bounds = Vec::new();
        for condition in conditions {
            if let Some(bound) = self.time_bound(scope, table, condition) {
                time_bounds.push(bound);
                continue;
            }
            let reduced = self
                .reduce(scope, table, condition)?
                .ok_or_else(|| scope.irreducible(condition))?;
            kept = kept.and(reduced);
        }
        Ok((kept, time_bounds))
    }

    /// `<joined column> = <column>`, either way round, a column of the arm's
    /// own `table` equated with one of a row it joins, answered as the index
    /// of that row in `joined` beside the two columns.
    fn key_equality(
        table: &ArmTable<'_, 'db, DB>,
        joined: &[ArmTable<'_, 'db, DB>],
        conjunct: &Expr,
    ) -> Option<(usize, JoinedColumns<'db, DB>)> {
        let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(conjunct) else {
            return None;
        };
        joined.iter().enumerate().find_map(|(index, row)| {
            [(left, right), (right, left)].into_iter().find_map(|(host, referenced)| {
                Some((index, (table.column(host)?, row.column(referenced)?)))
            })
        })
    }

    /// The one foreign key of `host` referencing `joined` whose every column
    /// `key` equates with the column it references, and nothing else, with
    /// its columns in the order it declares them.
    fn joining_key(
        &self,
        host: &'db DB::Table,
        joined: &'db DB::Table,
        key: &[JoinedColumns<'db, DB>],
    ) -> Result<Option<JoiningKey<'db, DB>>, LookupError> {
        let database = self.database;
        let mut found = None;
        for foreign_key in host.foreign_keys(database)? {
            if !core::ptr::eq(foreign_key.referenced_table(database)?, joined) {
                continue;
            }
            let columns: Vec<_> = foreign_key
                .host_columns(database)?
                .zip(foreign_key.referenced_columns(database)?)
                .collect();
            let equated = columns.len() == key.len()
                && columns.iter().all(|(host, referenced)| {
                    key.iter().any(|(equated_host, equated_referenced)| {
                        core::ptr::eq(*equated_host, *host)
                            && core::ptr::eq(*equated_referenced, *referenced)
                    })
                });
            if equated && found.replace((foreign_key, columns)).is_some() {
                return Ok(None);
            }
        }
        Ok(found)
    }

    /// Reads `condition`, `NOT EXISTS (<blocking>)` in a grant arm reading
    /// `table`, `None` where its conditions keep no blocking row.
    fn anti_join(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        condition: &Expr,
        blocking: &Query,
    ) -> Result<Option<AntiJoin<'db, DB>>, HelperSetError> {
        let database = self.database;
        let shape = || scope.irreducible(condition);
        let SetExpr::Select(select) = blocking.body.as_ref() else { return Err(shape()) };
        let ([item], [from], Some(selection)) =
            (select.projection.as_slice(), select.from.as_slice(), &select.selection)
        else {
            return Err(shape());
        };
        // `EXISTS` reads no value of a row, but `*` still needs every column.
        let reads_every_column = match item {
            SelectItem::UnnamedExpr(Expr::Value(value)) if Literal::of(&value.value).is_some() => {
                false
            }
            SelectItem::Wildcard(options) if *options == WildcardAdditionalOptions::default() => {
                true
            }
            _ => return Err(shape()),
        };
        if !plain_query(blocking) || !plain_select(select) {
            return Err(shape());
        }
        let blocker = self.arm_table(scope, &from.relation, from.joins.is_empty(), shape)?;
        // The blocking table's name hides the arm's, so no column correlates.
        if same_ident(&blocker.qualifier, &table.qualifier) {
            return Err(shape());
        }
        let mut conjuncts = Vec::new();
        conjunction(selection, &mut conjuncts);
        let mut correlations = Vec::new();
        let mut filters = Vec::new();
        for conjunct in conjuncts {
            if let Some(correlation) = correlation(table, &blocker, conjunct) {
                correlations.push(correlation);
            } else if reads_arm(table, &blocker, conjunct) {
                return Err(scope.irreducible(conjunct));
            } else {
                filters.push(conjunct);
            }
        }
        if correlations.is_empty() {
            return Err(shape());
        }
        self.require_select(scope, blocker.table, || {
            if reads_every_column { blocker.columns.clone() } else { blocker.columns_read(select) }
        })?;
        let (kept, time_bounds) = self.filter(scope, &blocker, filters)?;
        let Some(restrictions) = kept.restrictions() else { return Ok(None) };
        let correlated: Vec<_> =
            correlations.iter().map(|correlation| correlation.blocking_column).collect();
        Ok(Some(AntiJoin {
            table: blocker.table,
            keyed: holds_key(blocker.table, &correlated, database)?,
            correlations,
            restrictions,
            time_bounds,
        }))
    }

    /// `<column> IN (<values>)` listing at least one read of a setting
    /// beside literals and parameters: one grantee per read, each compared
    /// as `<column> = <read>` is, and the literals together as one grantee
    /// of anyone restricted to them, in the place of the first. `None`
    /// where the list names no read or anything else.
    fn request_list(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<Vec<Grantee<'db, DB>>>, HelperSetError> {
        let Expr::In { expr, target: InTarget::List(list), negated: false, .. } = unnest(conjunct)
        else {
            return Ok(None);
        };
        let Some(column) = table.column(expr) else { return Ok(None) };
        let mut elements = Vec::with_capacity(list.len());
        for element in list {
            elements.push(match self.operand(scope, table, element)? {
                Operand::Setting { read, cast } => Listed::Read(read, cast),
                Operand::Passed(passed) => Listed::Literal(passed),
                _ => return Ok(None),
            });
        }
        if !elements.iter().any(|element| matches!(element, Listed::Read(..))) {
            return Ok(None);
        }
        let mut grantees = Vec::with_capacity(elements.len());
        let mut literals = None;
        let mut values = Vec::new();
        for element in elements {
            match element {
                Listed::Read(read, cast) => {
                    let cast = cast.map(|cast| ReadCast { written: cast, compared: cast });
                    let request = self.compared(scope, column, &read, None, cast)?;
                    grantees.push((GrantedTo::Request(request), Kept::All));
                }
                Listed::Literal(passed) => {
                    if literals.is_none() {
                        literals = Some(grantees.len());
                        grantees.push((GrantedTo::Anyone, Kept::Nothing));
                    }
                    values.push(passed);
                }
            }
        }
        if let Some(index) = literals {
            grantees[index].1 = Kept::listed(column, values);
        }
        Ok(Some(grantees))
    }

    /// The cap `conjunct` writes, if it writes one: `<operands> IN (SELECT *
    /// FROM <helper>(<arguments>))` with two operands or more, or `<lift> OR
    /// <operands> IN (...)` either way round, with one operand or more, one
    /// operand also compared with `(SELECT <helper>(<arguments>))`. `rows`
    /// holds the arm's table and then each table it joins, whose columns
    /// the operands may name.
    fn cap(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        rows: &[ArmTable<'_, 'db, DB>],
        conjunct: &Expr,
    ) -> Result<Option<RowCap<'db, DB>>, HelperSetError> {
        let Some((lifted, expr, subquery)) = cap_shape(conjunct) else { return Ok(None) };
        let shape = || scope.arm_shape(select);
        let (table, joined) = rows.split_first().ok_or_else(shape)?;
        let lift = match lifted {
            Some(lifted) => {
                let Some(lift) = self.lift(scope, table, lifted)? else { return Ok(None) };
                Some(lift)
            }
            None => None,
        };
        let items = match unnest(expr) {
            Expr::Tuple(items) => items.as_slice(),
            item => core::slice::from_ref(item),
        };
        let call = selected_call(subquery)
            .or_else(|| projected_call(subquery).filter(|_| items.len() == 1))
            .ok_or_else(shape)?;
        let Called { function, arguments, set } =
            self.called_set(scope, select, rows, &call, items.len())?;
        let database = self.database;
        let mut operands = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let joined_column = joined
                .iter()
                .enumerate()
                .find_map(|(row, joined)| Some((row, joined.column(item)?)));
            if let Some((row, column)) = joined_column {
                operands.push(CapOperand::Joined { row, column });
                continue;
            }
            operands.push(match self.operand(scope, table, item)? {
                Operand::Column(column) => CapOperand::Column(column),
                Operand::Passed(passed) => CapOperand::Value(passed.value),
                Operand::CastPassed { passed, cast }
                    if returned_type(function, database, index)
                        .is_some_and(|declared| self.is_declared_type(scope, cast, &declared)) =>
                {
                    CapOperand::Value(passed.value)
                }
                _ => return Err(shape()),
            });
        }
        Ok(Some(RowCap { operands, function, arguments, set: Box::new(set), lift }))
    }

    /// The lift `expression` writes on a cap, if it writes one,
    /// `current_setting(<setting>) IS NULL`, the read optionally cast to the
    /// type the setting is declared in.
    fn lift(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        expression: &Expr,
    ) -> Result<Option<CapLift<'db>>, HelperSetError> {
        let Expr::IsNull(read) = unnest(expression) else { return Ok(None) };
        let Operand::Setting { read, cast } = self.operand(scope, table, read)? else {
            return Ok(None);
        };
        if read.normalization.is_some() {
            return Ok(None);
        }
        let (setting, declared) = self.declared_setting(scope, &read.name)?;
        if let Some(cast) = cast
            && !declared.is_some_and(|declared| self.is_declared_type(scope, cast, declared))
        {
            return Err(HelperSetError::SettingCast {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
                cast: cast.to_string(),
            });
        }
        Ok(Some(CapLift {
            setting,
            raises_when_unset: read.raises_when_unset,
            lifts_on_empty: read.empty_as_null,
        }))
    }

    /// The gate `conjunct` writes on a caller setting,
    /// `current_setting(<caller>) <> <text>` or `current_setting(<caller>)
    /// IS NOT NULL`, the operands either way round. The setting may be cast
    /// to its declared type, and then the text too.
    fn gate(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<GateRead<'db>>, HelperSetError> {
        let (read, cast, excluded, excluded_cast) = match unnest(conjunct) {
            Expr::IsNotNull(read) => {
                let Operand::Setting { read, cast } = self.operand(scope, table, read)? else {
                    return Ok(None);
                };
                (read, cast, None, None)
            }
            Expr::BinaryOp { left, op: BinaryOperator::NotEq, right } => {
                let ((Operand::Setting { read, cast }, excluded)
                | (excluded, Operand::Setting { read, cast })) =
                    (self.operand(scope, table, left)?, self.operand(scope, table, right)?)
                else {
                    return Ok(None);
                };
                let (excluded, excluded_cast) =
                    match (excluded, cast.is_some() || read.in_declared_type) {
                        (Operand::Passed(passed), _) => (passed, None),
                        // `text` compares with no value cast to another type.
                        (Operand::CastPassed { passed, cast: excluded_cast }, true) => {
                            (passed, Some(excluded_cast))
                        }
                        _ => return Ok(None),
                    };
                (read, cast, Some(excluded), excluded_cast)
            }
            _ => return Ok(None),
        };
        // A setting reads as `text`, which PostgreSQL compares with no
        // number or boolean. A read cast to its declared type takes the same
        // literals, which PostgreSQL then reads in that type.
        if excluded
            .as_ref()
            .is_some_and(|passed| !matches!(passed.literal, Literal::Text(_) | Literal::Null))
        {
            return Ok(None);
        }
        if read.normalization.is_some() {
            return Ok(None);
        }
        let (setting, declared) = self.caller_setting(scope, &read.name)?;
        if let Some(cast) = cast
            && !self.is_declared_type(scope, cast, declared)
        {
            return Err(HelperSetError::SettingCast {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
                cast: cast.to_string(),
            });
        }
        if excluded_cast.is_some_and(|cast| !self.is_declared_type(scope, cast, declared)) {
            return Ok(None);
        }
        Ok(Some(GateRead {
            setting,
            raises_when_unset: read.raises_when_unset,
            in_declared_type: cast.is_some() || read.in_declared_type,
            empty_as_null: read.empty_as_null,
            excluded,
        }))
    }

    /// The bound `condition` sets on a `timestamptz` column of the arm's
    /// table against the statement time, where it sets one.
    fn time_bound(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        condition: &Expr,
    ) -> Option<TimeBound<'db, DB>> {
        let database = self.database;
        let (comparison, null) = match unnest(condition) {
            Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
                match (unnest(left), unnest(right)) {
                    (Expr::IsNull(null), comparison) | (comparison, Expr::IsNull(null)) => {
                        (comparison, Some(null))
                    }
                    _ => return None,
                }
            }
            comparison => (comparison, None),
        };
        let Expr::BinaryOp { left, op, right } = comparison else { return None };
        let (compared, comparison) = if self.statement_time(scope, right) {
            let comparison = match op {
                BinaryOperator::Gt => TimeComparison::Later,
                BinaryOperator::GtEq => TimeComparison::LaterOrEqual,
                BinaryOperator::Lt => TimeComparison::Earlier,
                BinaryOperator::LtEq => TimeComparison::EarlierOrEqual,
                _ => return None,
            };
            (left, comparison)
        } else if self.statement_time(scope, left) {
            let comparison = match op {
                BinaryOperator::Lt => TimeComparison::Later,
                BinaryOperator::LtEq => TimeComparison::LaterOrEqual,
                BinaryOperator::Gt => TimeComparison::Earlier,
                BinaryOperator::GtEq => TimeComparison::EarlierOrEqual,
                _ => return None,
            };
            (right, comparison)
        } else {
            return None;
        };
        let column = table.column(compared)?;
        if column.data_type(database).scalar_family(database) != Some(ScalarFamily::TimestampTz) {
            return None;
        }
        if let Some(null) = null
            && table.column(null).is_none_or(|nulled| !core::ptr::eq(nulled, column))
        {
            return None;
        }
        Some(TimeBound { column, comparison, keeps_null: null.is_some() })
    }

    /// Whether `expression` reads the statement time, the start of the
    /// transaction, as `now()`, `CURRENT_TIMESTAMP` and
    /// `transaction_timestamp()` read it.
    fn statement_time(&self, scope: &Scope<'db, DB>, expression: &Expr) -> bool {
        let Expr::Function(call) = unnest(expression) else { return false };
        if let Some(form) = expression_form(&call.name) {
            // `CURRENT_TIMESTAMP(<precision>)` rounds the time it reads.
            return form == ExpressionForm::Value("TIMESTAMP WITH TIME ZONE")
                && matches!(call.args, FunctionArguments::None)
                && call.filter.is_none()
                && call.over.is_none()
                && call.null_treatment.is_none()
                && call.within_group.is_empty();
        }
        ["now", "transaction_timestamp"].into_iter().any(|name| {
            self.catalog_arguments(scope, call, name).is_some_and(|list| list.args.is_empty())
        })
    }

    /// The comparison of a column with a value of the request `conjunct`
    /// writes, if it writes one: `<column> = <principal>`, `<column> = ANY
    /// (<array>)` or `<column> IN (SELECT <elements>)`.
    fn setting_comparison(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<SettingComparison<'db, DB>>, HelperSetError> {
        let (column, read, packing, cast) = match unnest(conjunct) {
            Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
                let sides = (self.operand(scope, table, left)?, self.operand(scope, table, right)?);
                let ((Operand::Column(column), Operand::Setting { read, cast })
                | (Operand::Setting { read, cast }, Operand::Column(column))) = sides
                else {
                    return Ok(None);
                };
                let cast = cast.map(|cast| ReadCast { written: cast, compared: cast });
                (column, read, None, cast)
            }
            Expr::AnyOp { left, compare_op: BinaryOperator::Eq, right, .. } => {
                let (Some(column), Some(set)) = (table.column(left), self.set_array(scope, right)?)
                else {
                    return Ok(None);
                };
                (column, set.read, Some(set.packing), set.cast)
            }
            Expr::In { expr, target: InTarget::Subquery(query), negated: false, .. } => {
                let (Some(column), Some(set)) =
                    (table.column(expr), self.set_elements(scope, query)?)
                else {
                    return Ok(None);
                };
                (column, set.read, Some(set.packing), set.cast)
            }
            _ => return Ok(None),
        };
        self.compared(scope, column, &read, packing.as_ref(), cast).map(Some)
    }

    /// `column` compared with `read`, the identity of a principal where
    /// `packing` is `None`, else each element of a set unpacked as
    /// `packing`, and either cast as `cast`.
    fn compared(
        &self,
        scope: &Scope<'db, DB>,
        column: &'db DB::Column,
        read: &SettingRead,
        packing: Option<&SetPacking>,
        cast: Option<ReadCast<'_>>,
    ) -> Result<SettingComparison<'db, DB>, HelperSetError> {
        let (setting, _) = self.declared_setting(scope, &read.name)?;
        // `string_to_array('', <delimiter>)` is an empty array.
        let excludes_unset =
            read.empty_as_null || matches!(packing, Some(SetPacking::Delimited(_)));
        let refuse = |reason: fn(String, String) -> HelperSetError| {
            Err(reason(scope.name.clone(), setting.name().to_string()))
        };
        let (value, declared) = match (packing, setting.content()) {
            (
                None,
                SettingContent::Value(
                    value @ (SettingValue::Caller(declared)
                    | SettingValue::Principal { identity: declared, .. }
                    | SettingValue::Scalar(declared)),
                ),
            ) => (value, declared),
            (None, _) => {
                return refuse(|function, setting| {
                    HelperSetError::SettingNotSingle { function, setting }
                });
            }
            (
                Some(packing),
                SettingContent::Value(value @ SettingValue::Set { element, packing: declared }),
            ) => {
                if packing != declared {
                    return refuse(|function, setting| {
                        HelperSetError::MispackedSet { function, setting }
                    });
                }
                (value, element)
            }
            (Some(_), _) => {
                return refuse(|function, setting| {
                    HelperSetError::SettingNotSet { function, setting }
                });
            }
        };
        if let Some(normalization) = read.normalization
            && !matches!(value, SettingValue::Scalar(_))
        {
            return Err(HelperSetError::SettingNormalized {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
                normalization,
            });
        }
        if let Some(cast) = cast
            && !self.is_declared_type(scope, cast.compared, declared)
        {
            return Err(HelperSetError::SettingCast {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
                cast: cast.written.to_string(),
            });
        }
        Ok(SettingComparison {
            column,
            setting,
            value,
            raises_when_unset: read.raises_when_unset,
            excludes_unset,
            normalization: read.normalization,
        })
    }

    /// Whether `cast`, as the helper's path resolves it, is `declared`, the
    /// type a setting is declared in, whose canonical text the cast keeps.
    fn is_declared_type(
        &self,
        scope: &Scope<'db, DB>,
        cast: &DataType,
        declared: &DataType,
    ) -> bool {
        let modifier = type_modifier::declared(cast);
        let written = PgType::of(cast);
        if modifier.is_none()
            || modifier != type_modifier::declared(declared)
            || written != PgType::of(declared)
        {
            return false;
        }
        // An unqualified name is looked up on the path, where a type of that
        // name in a schema before `pg_catalog` is the one the cast reaches.
        let qualified = matches!(cast, DataType::Custom(name, _) if name.0.len() > 1);
        let (PgType::Catalog(name), false) = (written, qualified) else {
            return true;
        };
        let path = || scope.usable_path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
        !type_search_path(path)
            .take_while(|(schema, quoted)| !identifiers_match(schema, *quoted, PG_CATALOG, false))
            .any(|(schema, quoted)| {
                self.database
                    .type_facts(TargetName::new(name, false).with_schema(schema, quoted))
                    .is_some()
            })
    }

    /// The set `expression` builds an array of: `string_to_array(<setting>,
    /// <delimiter>)`, optionally cast to an array, `ARRAY(SELECT
    /// <elements>)`, or an accessor returning either.
    fn set_array<'q>(
        &self,
        scope: &Scope<'db, DB>,
        expression: &'q Expr,
    ) -> Result<Option<SetRead<'q>>, HelperSetError> {
        let (expression, cast) = match unnest(expression) {
            Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr,
                data_type: written @ DataType::Array(definition),
                format: None,
            } => {
                let Some(compared) = written_element(definition) else { return Ok(None) };
                (unnest(expr), Some(ReadCast { written, compared }))
            }
            expression => (expression, None),
        };
        let Expr::Function(call) = expression else { return Ok(None) };
        if let FunctionArguments::Subquery(_) = &call.args {
            let Some(query) = array_constructor(call).filter(|_| cast.is_none()) else {
                return Ok(None);
            };
            return self.set_elements(scope, query);
        }
        // A third argument names a string read as `NULL`, which changes the
        // elements.
        let Some(
            [
                FunctionArg::Unnamed(FunctionArgExpr::Expr(read)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(delimiter)),
            ],
        ) = self.catalog_arguments(scope, call, "string_to_array").map(|list| list.args.as_slice())
        else {
            // A read casts its elements once.
            return Ok(self.accessor_set(scope, call)?.and_then(|(read, packing)| {
                (cast.is_none() || !read.in_declared_type).then_some(SetRead {
                    read,
                    packing,
                    cast,
                })
            }));
        };
        let Expr::Value(delimiter) = unnest(delimiter) else { return Ok(None) };
        let Some(Literal::Text(delimiter)) = Literal::of(&delimiter.value) else { return Ok(None) };
        Ok(self
            .text_setting(scope, read)?
            .map(|read| SetRead { read, packing: SetPacking::Delimited(delimiter), cast }))
    }

    /// The set `query` returns the elements of, each optionally cast: `SELECT
    /// unnest(<array>)` or `SELECT
    /// jsonb_array_elements_text(<setting>::jsonb)`.
    fn set_elements<'q>(
        &self,
        scope: &Scope<'db, DB>,
        query: &'q Query,
    ) -> Result<Option<SetRead<'q>>, HelperSetError> {
        let (call, cast) = match projected(query) {
            Some(Expr::Function(call)) => (call, None),
            Some(Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr,
                data_type,
                format: None,
            }) => {
                let Expr::Function(call) = unnest(expr) else { return Ok(None) };
                (call, Some(ReadCast { written: data_type, compared: data_type }))
            }
            _ => return Ok(None),
        };
        if let Some([FunctionArg::Unnamed(FunctionArgExpr::Expr(array))]) =
            self.catalog_arguments(scope, call, "unnest").map(|list| list.args.as_slice())
        {
            let Some(set) = self.set_array(scope, array)? else { return Ok(None) };
            // A read casts its elements once.
            return Ok(match (cast, set.is_cast()) {
                (Some(_), true) => None,
                (cast, _) => Some(SetRead { cast: cast.or(set.cast), ..set }),
            });
        }
        let Some([FunctionArg::Unnamed(FunctionArgExpr::Expr(document))]) = self
            .catalog_arguments(scope, call, "jsonb_array_elements_text")
            .map(|list| list.args.as_slice())
        else {
            return Ok(None);
        };
        let Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr: read,
            data_type: DataType::JSONB,
            format: None,
        } = unnest(document)
        else {
            return Ok(None);
        };
        Ok(self
            .text_setting(scope, read)?
            .map(|read| SetRead { read, packing: SetPacking::JsonArray, cast }))
    }

    /// The setting `expression` reads as the text it is set to.
    fn text_setting(
        &self,
        scope: &Scope<'db, DB>,
        expression: &Expr,
    ) -> Result<Option<SettingRead>, HelperSetError> {
        Ok(self.setting_expression(scope, expression)?.filter(|read| !read.in_declared_type))
    }

    /// The setting `expression` reads, `current_setting(...)`,
    /// `NULLIF(<read>, '')`, `lower(<read>)` or an accessor returning a read.
    fn setting_expression(
        &self,
        scope: &Scope<'db, DB>,
        expression: &Expr,
    ) -> Result<Option<SettingRead>, HelperSetError> {
        let Expr::Function(call) = unnest(expression) else { return Ok(None) };
        if let Some([FunctionArg::Unnamed(FunctionArgExpr::Expr(read))]) =
            self.catalog_arguments(scope, call, "lower").map(|list| list.args.as_slice())
        {
            return Ok(self
                .text_setting(scope, read)?
                .filter(|read| read.normalization.is_none())
                .map(|read| {
                    SettingRead { normalization: Some(RequestNormalization::Lower), ..read }
                }));
        }
        if expression_form(&call.name) != Some(ExpressionForm::NullIf) {
            if let Some(read) = self.setting_read(scope, call) {
                return Ok(Some(read));
            }
            return self.accessor_read(scope, call);
        }
        let Some(
            [
                FunctionArg::Unnamed(FunctionArgExpr::Expr(read)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(empty))),
            ],
        ) = plain_arguments(call).map(|list| list.args.as_slice())
        else {
            return Ok(None);
        };
        if Literal::of(&empty.value) != Some(Literal::Text(String::new())) {
            return Ok(None);
        }
        Ok(self
            .text_setting(scope, read)?
            .filter(|read| !read.empty_as_null)
            .map(|read| SettingRead { empty_as_null: true, ..read }))
    }

    /// The setting `expression` reads, with the cast it writes on the read
    /// where it writes one.
    fn typed_setting<'q>(
        &self,
        scope: &Scope<'db, DB>,
        expression: &'q Expr,
    ) -> Result<Option<(SettingRead, Option<&'q DataType>)>, HelperSetError> {
        let Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            format: None,
        } = unnest(expression)
        else {
            return Ok(self.setting_expression(scope, expression)?.map(|read| (read, None)));
        };
        Ok(self.text_setting(scope, expr)?.map(|read| (read, Some(data_type))))
    }

    /// The setting the accessor `call` reaches returns, a zero-argument
    /// `sql` function whose body selects a setting read, optionally cast, as
    /// it reads under the path and the role the accessor runs with.
    fn accessor_read(
        &self,
        scope: &Scope<'db, DB>,
        call: &Function,
    ) -> Result<Option<SettingRead>, HelperSetError> {
        let read = self.accessed(scope, call, |body, returned| {
            let Some((read, cast)) = self.typed_setting(body, returned)? else { return Ok(None) };
            let in_declared_type = cast.is_some() || read.in_declared_type;
            let declared = if in_declared_type {
                let (setting, declared) = self.declared_setting(body, &read.name)?;
                if let Some(cast) = cast
                    && declared.is_none_or(|declared| !self.is_declared_type(body, cast, declared))
                {
                    return Err(HelperSetError::SettingCast {
                        function: body.name.clone(),
                        setting: setting.name().to_string(),
                        cast: cast.to_string(),
                    });
                }
                declared
            } else {
                None
            };
            let returns_read = self
                .accessor_returns(body)
                .is_some_and(|returned| self.is_read_type(body, &returned, declared));
            Ok(Some((SettingRead { in_declared_type, ..read }, (), returns_read)))
        })?;
        Ok(read.map(|(read, ())| read))
    }

    /// The set the accessor `call` reaches returns, a zero-argument `sql`
    /// function whose body selects an array of its elements, optionally cast,
    /// as it reads under the path and the role the accessor runs with, with
    /// how the body unpacks it.
    fn accessor_set(
        &self,
        scope: &Scope<'db, DB>,
        call: &Function,
    ) -> Result<Option<(SettingRead, SetPacking)>, HelperSetError> {
        self.accessed(scope, call, |body, returned| {
            let Some(set) = self.set_array(body, returned)? else { return Ok(None) };
            let in_declared_type = set.is_cast();
            let element = if in_declared_type {
                let (setting, _) = self.declared_setting(body, &set.read.name)?;
                let element = match setting.content() {
                    SettingContent::Value(SettingValue::Set { element, .. }) => Some(element),
                    _ => None,
                };
                if let Some(cast) = set.cast
                    && element
                        .is_none_or(|element| !self.is_declared_type(body, cast.compared, element))
                {
                    return Err(HelperSetError::SettingCast {
                        function: body.name.clone(),
                        setting: setting.name().to_string(),
                        cast: cast.written.to_string(),
                    });
                }
                element
            } else {
                None
            };
            let returns_read = self.accessor_returns(body).is_some_and(|returned| {
                matches!(&returned, DataType::Array(definition)
                    if written_element(definition)
                        .is_some_and(|returned| self.is_read_type(body, returned, element)))
            });
            Ok(Some((SettingRead { in_declared_type, ..set.read }, set.packing, returns_read)))
        })
    }

    /// What `read` reads from the expression the body of the accessor `call`
    /// reaches selects, read under the path and the role the accessor runs
    /// with, the setting read beside what else it reads and whether the
    /// accessor returns the read's own type. An accessor is a zero-argument
    /// `sql` function whose body is one query selecting one expression, and
    /// it reads as that read only where PostgreSQL runs it on each call and
    /// returns the read unchanged.
    fn accessed<T>(
        &self,
        scope: &Scope<'db, DB>,
        call: &Function,
        read: impl FnOnce(
            &Scope<'db, DB>,
            &Expr,
        ) -> Result<Option<(SettingRead, T, bool)>, HelperSetError>,
    ) -> Result<Option<(SettingRead, T)>, HelperSetError> {
        let database = self.database;
        if !plain_arguments(call).is_some_and(|list| list.args.is_empty()) {
            return Ok(None);
        }
        let Some(accessor) = self.called(scope, &call.name, 0)? else { return Ok(None) };
        let name = accessor.target_name().to_string();
        if scope.accessing.iter().any(|seen| core::ptr::eq(*seen, accessor)) {
            return Err(HelperSetError::CallCycle { function: name });
        }
        if accessor.stored_language().as_deref() != Some("sql") {
            return Ok(None);
        }
        // A body bound at creation resolves its names with no path, so it is
        // not read as one written to run under the path.
        let Ok((_, SqlBody::Statements(statements))) = written_sql_body(accessor, database) else {
            return Ok(None);
        };
        let [Statement::Query(query)] = statements.as_ref() else { return Ok(None) };
        let Some(returned) = projected(query) else { return Ok(None) };
        let path = match accessor.search_path(database)? {
            Some(path) => path,
            None => scope.path.clone(),
        };
        let owner = if accessor.security_mode() == FunctionSecurity::Definer {
            self.owner(accessor, &name)?
        } else {
            scope.owner
        };
        let usable_path = self.usable_path(&path, owner);
        let mut accessing = scope.accessing.clone();
        accessing.push(accessor);
        let body = Scope {
            function: accessor,
            name,
            path,
            usable_path,
            parameters: Vec::new(),
            owner,
            accessing,
        };
        let Some((setting_read, accessed, returns_read)) = read(&body, returned)? else {
            return Ok(None);
        };
        let reason = if accessor.volatility() == FunctionBehavior::Immutable {
            AccessorRefusal::Immutable
        } else if sets_setting(accessor, &setting_read.name) {
            AccessorRefusal::SetsSetting
        } else if !returns_read {
            AccessorRefusal::ReturnType
        } else {
            return Ok(Some((setting_read, accessed)));
        };
        Err(HelperSetError::Accessor {
            function: scope.name.clone(),
            accessor: body.name,
            setting: setting_read.name,
            reason,
        })
    }

    /// The caller setting `name` names, with the type the deployment
    /// declares it in.
    fn caller_setting(
        &self,
        scope: &Scope<'db, DB>,
        name: &str,
    ) -> Result<(&'db SessionSetting, &'db DataType), HelperSetError> {
        let (setting, _) = self.declared_setting(scope, name)?;
        let SettingContent::Value(SettingValue::Caller(declared)) = setting.content() else {
            return Err(HelperSetError::SettingNotCaller {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
            });
        };
        Ok((setting, declared))
    }

    /// The setting `name` names, with the type of the one value it holds,
    /// `None` for a set or a JSON document.
    fn declared_setting(
        &self,
        scope: &Scope<'db, DB>,
        name: &str,
    ) -> Result<(&'db SessionSetting, Option<&'db DataType>), HelperSetError> {
        let Some(setting) = self.deployment.setting(name) else {
            return Err(HelperSetError::UndeclaredSetting {
                function: scope.name.clone(),
                setting: name.to_string(),
            });
        };
        let declared = match setting.content() {
            SettingContent::Value(value) => value.single_type(),
            SettingContent::Json(_) => None,
        };
        Ok((setting, declared))
    }

    /// The type of the one value the accessor `scope` reads returns, `None`
    /// where it returns a set of rows or declares no type the reader parses.
    fn accessor_returns(&self, scope: &Scope<'db, DB>) -> Option<DataType> {
        let accessor = scope.function;
        if accessor.returns_set() {
            return None;
        }
        accessor.result_type_name(self.database).and_then(|name| parse_data_type(&name))
    }

    /// Whether `returned` is the type a read takes, `declared` where the read
    /// is cast to it and `text` where it is not.
    fn is_read_type(
        &self,
        scope: &Scope<'db, DB>,
        returned: &DataType,
        declared: Option<&DataType>,
    ) -> bool {
        match declared {
            Some(declared) => self.is_declared_type(scope, returned, declared),
            None => PgType::of(returned) == PgType::of(&DataType::Text),
        }
    }

    fn operand<'q>(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        expression: &'q Expr,
    ) -> Result<Operand<'q, 'db, DB>, HelperSetError> {
        let expression = unnest(expression);
        if let Some(column) = table.column(expression) {
            return Ok(Operand::Column(column));
        }
        if let Some((passed, _)) = scope.parameter(expression) {
            return Ok(Operand::Passed(passed.clone()));
        }
        if let Some((read, cast)) = self.typed_setting(scope, expression)? {
            return Ok(Operand::Setting { read, cast });
        }
        Ok(match expression {
            Expr::Value(value) => {
                Literal::of(&value.value).map_or(Operand::Other, |literal| {
                    Operand::Passed(Passed { literal, value: value.value.clone() })
                })
            }
            Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr,
                data_type,
                format: None,
            } => {
                match self.operand(scope, table, expr)? {
                    Operand::Passed(passed) => Operand::CastPassed { passed, cast: data_type },
                    _ => Operand::Other,
                }
            }
            _ => Operand::Other,
        })
    }

    /// `current_setting('<name>'[, <missing_ok>])`, resolved to PostgreSQL's
    /// own function.
    fn setting_read(&self, scope: &Scope<'db, DB>, call: &Function) -> Option<SettingRead> {
        let list = self.catalog_arguments(scope, call, "current_setting")?;
        let literal = |argument: &FunctionArg| {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) = argument else {
                return None;
            };
            Literal::of(&value.value)
        };
        let (name, raises_when_unset) = match list.args.as_slice() {
            [name] => (literal(name)?, true),
            [name, missing_ok] => {
                let Literal::Boolean(missing_ok) = literal(missing_ok)? else { return None };
                (literal(name)?, !missing_ok)
            }
            _ => return None,
        };
        let Literal::Text(name) = name else { return None };
        Some(SettingRead {
            name,
            raises_when_unset,
            empty_as_null: false,
            in_declared_type: false,
            normalization: None,
        })
    }

    /// The arguments of `call` where it calls PostgreSQL's own `name`, in
    /// `pg_catalog`, and nothing else it could reach.
    fn catalog_arguments<'c>(
        &self,
        scope: &Scope<'db, DB>,
        call: &'c Function,
        name: &str,
    ) -> Option<&'c FunctionArgumentList> {
        let list = plain_arguments(call)?;
        self.reaches_catalog(scope, &call.name, name, list.args.len()).then_some(list)
    }

    /// Whether a call written as `written` with `arguments` arguments calls
    /// PostgreSQL's own `name`, in `pg_catalog`, and nothing else it could
    /// reach.
    fn reaches_catalog(
        &self,
        scope: &Scope<'db, DB>,
        written: &ObjectName,
        name: &str,
        arguments: usize,
    ) -> bool {
        // A qualifier built when the statement runs names no schema.
        let catalog = match qualifier_of(written) {
            Qualifier::Named(schema, quoted) => {
                identifiers_match(schema, quoted, "pg_catalog", false)
            }
            qualifier => qualifier == Qualifier::Absent,
        };
        let Some((called, quoted)) = object_name_last_part(written) else { return false };
        if !catalog || !identifiers_match(called, quoted, name, false) {
            return false;
        }
        // An overload declared in a schema the owner can use, which the call
        // may reach, is refused.
        let path = || scope.usable_path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
        !self.database.functions_named(called).any(|function| {
            call_reaches(function, written, arguments, path, self.database.identifier_case())
                && function.target_name().schema() != Some("pg_catalog")
        })
    }

    /// What `condition` keeps, `None` where it reduces to no restriction.
    fn reduce(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        condition: &Expr,
    ) -> Result<Option<Kept<'db, DB>>, HelperSetError> {
        match unnest(condition) {
            Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
                let (Some(left), Some(right)) =
                    (self.reduce(scope, table, left)?, self.reduce(scope, table, right)?)
                else {
                    return Ok(None);
                };
                Ok(Some(left.and(right)))
            }
            Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
                let (Some(left), Some(right)) =
                    (self.reduce(scope, table, left)?, self.reduce(scope, table, right)?)
                else {
                    return Ok(None);
                };
                Ok(left.or(right))
            }
            Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
                let sides = (self.operand(scope, table, left)?, self.operand(scope, table, right)?);
                Ok(match sides {
                    (Operand::Passed(left), Operand::Passed(right)) => {
                        left.literal.equals(&right.literal).map(Kept::of)
                    }
                    (Operand::Column(column), Operand::Passed(passed))
                    | (Operand::Passed(passed), Operand::Column(column)) => {
                        let values =
                            if passed.literal == Literal::Null { Vec::new() } else { vec![passed] };
                        Some(Kept::values(column, values))
                    }
                    _ => None,
                })
            }
            Expr::In { expr, target: InTarget::List(list), negated: false, .. } => {
                let mut values = Vec::new();
                for item in list {
                    let Operand::Passed(passed) = self.operand(scope, table, item)? else {
                        return Ok(None);
                    };
                    values.push(passed);
                }
                match self.operand(scope, table, expr)? {
                    Operand::Column(column) => Ok(Some(Kept::listed(column, values))),
                    Operand::Passed(passed) => {
                        let mut found = Equality::Fails;
                        for value in &values {
                            match passed.literal.equals(&value.literal) {
                                None => return Ok(None),
                                Some(Equality::Holds) => return Ok(Some(Kept::All)),
                                Some(Equality::Null) => found = Equality::Null,
                                Some(Equality::Fails) => {}
                            }
                        }
                        Ok(Some(Kept::of(found)))
                    }
                    _ => Ok(None),
                }
            }
            Expr::BinaryOp {
                left,
                op:
                    op @ (BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq),
                right,
            } => self.threshold(scope, table, left, op, right),
            tested @ (Expr::IsNull(operand) | Expr::IsNotNull(operand)) => {
                let null = matches!(tested, Expr::IsNull(_));
                Ok(match self.operand(scope, table, operand)? {
                    Operand::Passed(passed) => {
                        let holds = (passed.literal == Literal::Null) == null;
                        Some(Kept::of(if holds { Equality::Holds } else { Equality::Fails }))
                    }
                    Operand::Column(column) => {
                        Some(Kept::In(vec![(
                            column,
                            if null { Held::Null } else { Held::NotNull },
                        )]))
                    }
                    _ => None,
                })
            }
            expression => {
                Ok(match self.operand(scope, table, expression)? {
                    Operand::Passed(Passed { literal: Literal::Boolean(boolean), .. }) => {
                        Some(Kept::of(if boolean { Equality::Holds } else { Equality::Fails }))
                    }
                    Operand::Passed(Passed { literal: Literal::Null, .. }) => Some(Kept::Nothing),
                    // A boolean column alone keeps the rows holding `TRUE`.
                    Operand::Column(column)
                        if column.data_type(self.database).scalar_family(self.database)
                            == Some(ScalarFamily::Bool) =>
                    {
                        let passed =
                            Passed { literal: Literal::Boolean(true), value: Value::Boolean(true) };
                        Some(Kept::values(column, vec![passed]))
                    }
                    _ => None,
                })
            }
        }
    }

    /// What `<left> <op> <right>` keeps where it compares a column of an
    /// enum type with a passed label, the labels on the kept side of it in
    /// the order the type declares them, which is the order PostgreSQL
    /// compares them in.
    fn threshold(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        left: &Expr,
        op: &BinaryOperator,
        right: &Expr,
    ) -> Result<Option<Kept<'db, DB>>, HelperSetError> {
        let database = self.database;
        let sides = (self.operand(scope, table, left)?, self.operand(scope, table, right)?);
        // `<label> < <column>` keeps what `<column> > <label>` keeps.
        let (column, passed, column_greater) = match sides {
            (Operand::Column(column), Operand::Passed(passed)) => {
                (column, passed, matches!(op, BinaryOperator::Gt | BinaryOperator::GtEq))
            }
            (Operand::Passed(passed), Operand::Column(column)) => {
                (column, passed, matches!(op, BinaryOperator::Lt | BinaryOperator::LtEq))
            }
            _ => return Ok(None),
        };
        let inclusive = matches!(op, BinaryOperator::GtEq | BinaryOperator::LtEq);
        let Some(enumeration) = column.data_type(database).enumeration(database) else {
            return Ok(None);
        };
        if enumeration.kind() != EnumerationKind::Enum {
            return Ok(None);
        }
        let label = match passed.literal {
            Literal::Null => return Ok(Some(Kept::Nothing)),
            Literal::Text(label) => label,
            Literal::Boolean(_) | Literal::Number(_) => return Ok(None),
        };
        let labels: Vec<&str> = enumeration.into_members().map(EnumerationMember::label).collect();
        // PostgreSQL refuses a label the type does not declare.
        let Some(position) = labels.iter().position(|declared| *declared == label) else {
            return Ok(None);
        };
        let kept = match (column_greater, inclusive) {
            (true, true) => &labels[position..],
            (true, false) => &labels[position + 1..],
            (false, true) => &labels[..=position],
            (false, false) => &labels[..position],
        };
        let values = kept
            .iter()
            .map(|label| {
                Passed {
                    literal: Literal::Text((*label).to_string()),
                    value: Value::SingleQuotedString((*label).to_string()),
                }
            })
            .collect();
        Ok(Some(Kept::values(column, values)))
    }
}

/// Binds each input parameter of `function` to its argument.
fn parameters<'db, F: FunctionLike>(
    function: &'db F,
    arguments: &[Value],
    name: &str,
    database: &'db F::DB,
) -> Result<Vec<Parameter<'db>>, HelperSetError> {
    let mut names = function.argument_names(database);
    let inputs: Vec<_> = function
        .argument_modes()
        .map(|mode| (mode, names.next().flatten()))
        .filter(|(mode, _)| *mode != ArgMode::Out)
        .map(|(_, name)| name)
        .collect();
    if inputs.len() != arguments.len() {
        return Err(HelperSetError::ArgumentCount {
            function: name.to_string(),
            expected: inputs.len(),
            given: arguments.len(),
        });
    }
    inputs
        .into_iter()
        .zip(arguments)
        .map(|(input, value)| {
            let literal = Literal::of(value).ok_or_else(|| {
                HelperSetError::UnsupportedArgument {
                    function: name.to_string(),
                    argument: value.to_string(),
                }
            })?;
            Ok(Parameter { name: input, passed: Passed { literal, value: value.clone() } })
        })
        .collect()
}

/// How many columns `function` returns, one where it is declared `RETURNS
/// SETOF` one scalar type and one per column of `RETURNS TABLE (...)`, or
/// `None` for any other result.
fn returned_arity<F: FunctionLike>(function: &F, database: &F::DB) -> Option<usize> {
    match function.returned_columns(database).count() {
        0 => returns_scalar_set(function, database).then_some(1),
        columns => Some(columns),
    }
}

/// The type `function` declares its returned column at `index` in, where
/// it reads back as one, the element type for a helper returning `SETOF`
/// one.
fn returned_type<F: FunctionLike>(
    function: &F,
    database: &F::DB,
    index: usize,
) -> Option<DataType> {
    let declared = match function.returned_columns(database).nth(index) {
        Some((_, declared)) => declared,
        None => function.result_type_name(database)?,
    };
    parse_data_type(&declared)
}

/// Whether `function` is declared `RETURNS SETOF` one scalar type, which a
/// `record` is not.
fn returns_scalar_set<F: FunctionLike>(function: &F, database: &F::DB) -> bool {
    if !function.return_type_name(database).is_some_and(|declared| declared.starts_with("SETOF ")) {
        return false;
    }
    let Some(element) = function.result_type_name(database) else { return false };
    // The element is read back as written, so a quoted `"RECORD"` names a
    // type of its own rather than `record`.
    !parse_data_type(&element).is_some_and(|element| is_row(&element))
}

/// Whether a `SETOF` type is a row rather than one scalar.
fn is_row(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Custom(name, modifiers) if modifiers.is_empty()
        && object_name_last_part(name)
            .is_some_and(|(name, quoted)| identifiers_match(name, quoted, "record", false)))
}

/// Whether `function` sets the setting `name` for its own run, PostgreSQL
/// comparing setting names regardless of case.
fn sets_setting<F: FunctionLike>(function: &F, name: &str) -> bool {
    function.configuration_parameters().iter().any(|parameter| {
        let parts = &parameter.name.0;
        parts.len() == name.split('.').count()
            && parts.iter().zip(name.split('.')).all(|(written, part)| {
                matches!(written, ObjectNamePart::Identifier(ident) if ident.value.eq_ignore_ascii_case(part))
            })
    })
}

/// The body as it is written, for an error naming it.
fn written(body: FunctionBody<'_>) -> String {
    match body {
        FunctionBody::Text(text) | FunctionBody::BoundDefinition(text) => text.to_string(),
        FunctionBody::Expression(expression) | FunctionBody::Return(expression) => {
            expression.to_string()
        }
        FunctionBody::Select(select) => select.to_string(),
        FunctionBody::Statements(statements) | FunctionBody::Atomic(statements) => {
            statements.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
        }
    }
}

/// Collects the `SELECT`s of a `UNION`, rendering the query that is not one
/// where it meets it.
fn union_arms<'q>(query: &'q Query, arms: &mut Vec<&'q Select>) -> Result<(), String> {
    if !plain_query(query) {
        return Err(query.to_string());
    }
    set_arms(&query.body, arms)
}

fn set_arms<'q>(body: &'q SetExpr, arms: &mut Vec<&'q Select>) -> Result<(), String> {
    match body {
        SetExpr::Select(select) => {
            arms.push(select);
            Ok(())
        }
        SetExpr::Query(query) => union_arms(query, arms),
        SetExpr::SetOperation { op: SetOperator::Union, left, right, .. } => {
            set_arms(left, arms)?;
            set_arms(right, arms)
        }
        other => Err(other.to_string()),
    }
}

/// Whether `query` is its body alone.
fn plain_query(query: &Query) -> bool {
    query.with.is_none() && unadorned(query)
}

/// Whether `query` writes nothing beside its `WITH` and its body.
fn unadorned(query: &Query) -> bool {
    query.order_by.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty()
        && query.for_clause.is_none()
        && query.settings.is_none()
        && query.format_clause.is_none()
        && query.pipe_operators.is_empty()
}

/// The name and the alias of the table `relation` names with nothing else
/// written.
fn plain_table(relation: &TableFactor) -> Option<(&ObjectName, Option<&Ident>)> {
    let (name, alias, None) = plain_relation(relation)? else { return None };
    Some((name, alias))
}

/// The name, the alias and the arguments, where it calls a function, of the
/// relation `relation` names with nothing else written.
fn plain_relation(
    relation: &TableFactor,
) -> Option<(&ObjectName, Option<&Ident>, Option<&TableFunctionArgs>)> {
    let (name, alias, args) = aliased_relation(relation)?;
    let plain = alias.is_none_or(|alias| alias.columns.is_empty() && alias.at.is_none());
    plain.then(|| (name, alias.map(|alias| &alias.name), args))
}

/// The name, the alias and the arguments of the function `relation` calls
/// with nothing else written, the alias naming its columns or not.
fn called_relation(
    relation: &TableFactor,
) -> Option<(&ObjectName, Option<&TableAlias>, &[FunctionArg])> {
    let (name, alias, Some(args)) = aliased_relation(relation)? else { return None };
    Some((name, alias, &args.args))
}

/// The name, the alias and the arguments, where it calls a function, of the
/// relation `relation` names with nothing but its alias written.
fn aliased_relation(
    relation: &TableFactor,
) -> Option<(&ObjectName, Option<&TableAlias>, Option<&TableFunctionArgs>)> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_directives,
        inheritance: None,
    } = relation
    else {
        return None;
    };
    let plain = with_hints.is_empty()
        && partitions.is_empty()
        && index_directives.is_empty()
        && args.as_ref().is_none_or(|args| args.settings.is_none());
    plain.then_some((name, alias.as_ref(), args.as_ref()))
}

/// Whether `join` is `CROSS JOIN <relation>`, which pairs every row with
/// every row of the relation.
fn is_cross(join: &Join) -> bool {
    matches!(join.join_operator, JoinOperator::CrossJoin(JoinConstraint::None))
}

/// Whether two of the relations an arm reads, the ones `qualifiers` name and
/// `lists`, share a name, which PostgreSQL refuses.
fn named_twice<'n>(
    qualifiers: impl IntoIterator<Item = &'n Ident>,
    lists: &'n [LabelList<'_>],
) -> bool {
    let names: Vec<&Ident> = qualifiers
        .into_iter()
        .chain(lists.iter().map(|list| list.named.qualifier.as_ref()))
        .collect();
    names
        .iter()
        .enumerate()
        .any(|(index, name)| names[..index].iter().any(|other| same_ident(name, other)))
}

/// Whether a condition of `select`, its `WHERE` or the `ON` of a join, names
/// a column of one of `lists`, whose labels an arm only crosses its rows
/// with.
fn names_list(select: &Select, lists: &[LabelList<'_>]) -> bool {
    let joined = select.from.iter().flat_map(|from| &from.joins).filter_map(join_condition);
    select.selection.iter().chain(joined).any(|condition| {
        walk_expressions(condition, |expression| {
            if lists.iter().any(|list| list.named.position(expression).is_some()) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .is_break()
    })
}

/// The name `relation`'s columns are qualified by where it reads the walk
/// named `walk`, its alias or the walk's own name.
fn walk_qualifier<'q>(relation: &'q TableFactor, walk: &Ident) -> Option<&'q Ident> {
    let (name, alias) = plain_table(relation)?;
    let [ObjectNamePart::Identifier(part)] = name.0.as_slice() else { return None };
    same_ident(part, walk).then_some(alias.unwrap_or(part))
}

/// Whether `query` is `SELECT <column> FROM <walk>`, returning every value
/// the walk reached.
fn reads_walk(query: &Query, walk: &Walk<'_>) -> bool {
    let SetExpr::Select(select) = query.body.as_ref() else { return false };
    let ([SelectItem::UnnamedExpr(returned)], [from], None) =
        (select.projection.as_slice(), select.from.as_slice(), &select.selection)
    else {
        return false;
    };
    plain_query(query)
        && plain_select(select)
        && from.joins.is_empty()
        && walk_qualifier(&from.relation, walk.name)
            .is_some_and(|qualifier| names_column(returned, qualifier, walk.column, true))
}

/// Whether `arm` keeps every row of its table, for every request.
fn keeps_every_row<DB: DatabaseLike>(arm: &GrantArm<'_, DB>) -> bool {
    matches!(arm.granted_to, GrantedTo::Anyone)
        && arm.gate.is_none()
        && arm.restrictions.is_empty()
        && arm.time_bounds.is_empty()
        && arm.caps.is_empty()
        && arm.anti_joins.is_empty()
        && arm.exclusions.is_empty()
        && arm.joined_rows.is_empty()
}

/// Whether `expression` names `column` qualified by `qualifier`, or bare
/// where `bare` allows it.
fn names_column(expression: &Expr, qualifier: &Ident, column: &Ident, bare: bool) -> bool {
    match unnest(expression) {
        Expr::Identifier(ident) => bare && same_ident(ident, column),
        Expr::CompoundIdentifier(parts) => {
            matches!(parts.as_slice(), [named, ident] if same_ident(named, qualifier) && same_ident(ident, column))
        }
        _ => false,
    }
}

/// The side of `<left> = <right>` compared with `column` of the walk
/// `qualifier` names, `None` where `condition` compares nothing with it.
fn compared_with_walk<'q>(
    condition: &'q Expr,
    qualifier: &Ident,
    column: &Ident,
) -> Option<&'q Expr> {
    let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(condition) else {
        return None;
    };
    if names_column(right, qualifier, column, false) {
        Some(left)
    } else if names_column(left, qualifier, column, false) {
        Some(right)
    } else {
        None
    }
}

/// Whether two identifiers name the same object, as PostgreSQL folds them.
fn same_ident(left: &Ident, right: &Ident) -> bool {
    identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// The name of the column `select` returns, which names the column of a
/// `WITH` item declaring none.
fn output_name(select: &Select) -> Option<&Ident> {
    match select.projection.as_slice() {
        [SelectItem::ExprWithAlias { alias, .. }] => Some(alias),
        [SelectItem::UnnamedExpr(expression)] => {
            match unnest(expression) {
                Expr::Identifier(ident) => Some(ident),
                Expr::CompoundIdentifier(parts) => parts.last(),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `select` filters rows with nothing but `FROM` and `WHERE`.
fn plain_select(select: &Select) -> bool {
    matches!(select.distinct, None | Some(Distinct::Distinct))
        && select.optimizer_hints.is_empty()
        && select.select_modifiers.is_none()
        && select.top.is_none()
        && select.exclude.is_none()
        && select.into.is_none()
        && select.lateral_views.is_empty()
        && select.prewhere.is_none()
        && select.connect_by.is_empty()
        && matches!(&select.group_by, GroupByExpr::Expressions(expressions, modifiers) if expressions.is_empty() && modifiers.is_empty())
        && select.cluster_by.is_empty()
        && select.distribute_by.is_empty()
        && select.sort_by.is_empty()
        && select.having.is_none()
        && select.named_window.is_empty()
        && select.qualify.is_none()
        && select.value_table_mode.is_none()
        && select.flavor == SelectFlavor::Standard
}

/// The parts of `conjunct` where it is shaped as a cap: `<operands> IN
/// (<subquery>)` with a row of operands, or `<test> IS NULL OR <operands> IN
/// (<subquery>)` either way round, the `IS NULL` test where written beside
/// the operands and the subquery.
fn cap_shape(conjunct: &Expr) -> Option<(Option<&Expr>, &Expr, &Query)> {
    fn membership(expression: &Expr) -> Option<(&Expr, &Query)> {
        let Expr::In { expr, target: InTarget::Subquery(subquery), negated: false, .. } =
            unnest(expression)
        else {
            return None;
        };
        Some((expr.as_ref(), subquery.as_ref()))
    }
    match unnest(conjunct) {
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            [(left, right), (right, left)].into_iter().find_map(|(lift, tested)| {
                let lift = unnest(lift);
                let (operands, subquery) = membership(tested)?;
                matches!(lift, Expr::IsNull(_)).then_some((Some(lift), operands, subquery))
            })
        }
        written => {
            let (operands, subquery) = membership(written)?;
            matches!(unnest(operands), Expr::Tuple(_)).then_some((None, operands, subquery))
        }
    }
}

/// The call of `(SELECT <function>(<arguments>))`.
fn projected_call(query: &Query) -> Option<Call<'_>> {
    let Expr::Function(call) = projected(query)? else { return None };
    Some(Call { name: &call.name, arguments: &plain_arguments(call)?.args })
}

/// The call of `(SELECT * FROM <function>(<arguments>) [<alias>])`.
fn selected_call(query: &Query) -> Option<Call<'_>> {
    let SetExpr::Select(select) = query.body.as_ref() else { return None };
    let ([SelectItem::Wildcard(options)], [from]) =
        (select.projection.as_slice(), select.from.as_slice())
    else {
        return None;
    };
    let (name, _, Some(TableFunctionArgs { args, .. })) = plain_relation(&from.relation)? else {
        return None;
    };
    (plain_query(query)
        && plain_select(select)
        && select.selection.is_none()
        && from.joins.is_empty()
        && *options == WildcardAdditionalOptions::default())
    .then_some(Call { name, arguments: args })
}

/// The expression `(SELECT <expression>)` returns.
fn projected(query: &Query) -> Option<&Expr> {
    let SetExpr::Select(select) = query.body.as_ref() else { return None };
    let [SelectItem::UnnamedExpr(expression)] = select.projection.as_slice() else {
        return None;
    };
    (plain_query(query)
        && plain_select(select)
        && select.from.is_empty()
        && select.selection.is_none())
    .then_some(expression)
}

/// The subquery of `ARRAY(<query>)`, a constructor no function shadows.
fn array_constructor(call: &Function) -> Option<&Query> {
    let FunctionArguments::Subquery(query) = &call.args else { return None };
    let [ObjectNamePart::Identifier(name)] = call.name.0.as_slice() else { return None };
    (name.quote_style.is_none() && name.value.eq_ignore_ascii_case("array")).then_some(query)
}

/// The compared value and the subquery of `<value> NOT IN (<query>)`, also
/// written `NOT (<value> IN (<query>))`, `<value> <> ALL (<set>)` or `NOT
/// (<value> = ANY (<set>))`, `<set>` being `(<query>)` or `ARRAY(<query>)`,
/// which PostgreSQL evaluates alike, `NULL` included.
fn excluded_operands(conjunct: &Expr) -> Option<(&Expr, &Query)> {
    fn set_query(set: &Expr) -> Option<&Query> {
        match unnest(set) {
            Expr::Subquery(query) => Some(query),
            Expr::Function(call) => array_constructor(call),
            _ => None,
        }
    }
    match unnest(conjunct) {
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => {
            match unnest(expr) {
                Expr::In { expr, target: InTarget::Subquery(query), negated: false, .. } => {
                    Some((expr, query))
                }
                Expr::AnyOp { left, compare_op: BinaryOperator::Eq, right, .. } => {
                    Some((left, set_query(right)?))
                }
                _ => None,
            }
        }
        Expr::In { expr, target: InTarget::Subquery(query), negated: true, .. } => {
            Some((expr, query))
        }
        Expr::AllOp { left, compare_op: BinaryOperator::NotEq, right } => {
            Some((left, set_query(right)?))
        }
        _ => None,
    }
}

/// The arguments of `call`, where it is a plain call writing nothing but its
/// argument list.
fn plain_arguments(call: &Function) -> Option<&FunctionArgumentList> {
    let FunctionArguments::List(list) = &call.args else { return None };
    let plain = call.filter.is_none()
        && call.over.is_none()
        && call.null_treatment.is_none()
        && call.within_group.is_empty()
        && matches!(call.parameters, FunctionArguments::None)
        && list.duplicate_treatment.is_none()
        && list.clauses.is_empty();
    plain.then_some(list)
}

/// The schema `table` lives in, `public` for one stored without a qualifier.
fn table_schema<T: TableLike>(table: &T) -> (&str, bool) {
    table
        .table_schema()
        .map_or(("public", false), |schema| (schema, table.table_schema_is_quoted()))
}

/// The schema a function's `target` name lives in, `public` for one stored
/// without a qualifier.
fn function_schema<'t>(target: &'t TargetName<'_>) -> (&'t str, bool) {
    target.schema().map_or(("public", false), |schema| (schema, target.schema_is_quoted()))
}

/// `expression` without the parentheses around it.
fn unnest(mut expression: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    expression
}

/// The terms `expression` joins with `AND`.
fn conjunction<'q>(expression: &'q Expr, terms: &mut Vec<&'q Expr>) {
    match unnest(expression) {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            conjunction(left, terms);
            conjunction(right, terms);
        }
        term => terms.push(term),
    }
}

/// The foreign keys of `table` whose one host column is `column`.
fn single_column_foreign_keys<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    column: &'db DB::Column,
    database: &'db DB,
) -> Result<Vec<&'db DB::ForeignKey>, LookupError> {
    let mut foreign_keys = Vec::new();
    for foreign_key in table.foreign_keys(database)? {
        let mut host = foreign_key.host_columns(database)?;
        if host.next().is_some_and(|host| core::ptr::eq(host, column)) && host.next().is_none() {
            foreign_keys.push(foreign_key);
        }
    }
    Ok(foreign_keys)
}

/// Whether `foreign_key` checks every row whose `column` is not `NULL`.
fn checks_every_row<'db, DB: DatabaseLike>(
    foreign_key: &'db DB::ForeignKey,
    column: &'db DB::Column,
    database: &'db DB,
) -> Result<bool, LookupError> {
    if foreign_key.match_full(database) {
        return Ok(true);
    }
    for host in foreign_key.host_columns(database)? {
        if !core::ptr::eq(host, column) && host.is_nullable(database)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The subquery of `NOT EXISTS (<query>)`, also written `NOT (EXISTS
/// (<query>))`.
fn not_exists(condition: &Expr) -> Option<&Query> {
    match unnest(condition) {
        Expr::Exists { subquery, negated: true } => Some(subquery),
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => {
            let Expr::Exists { subquery, negated: false } = unnest(expr) else { return None };
            Some(subquery)
        }
        _ => None,
    }
}

/// The equality `conjunct` writes between a column of the blocking table
/// and a column of the arm's, if it writes one. A name both tables hold
/// names the blocking table's, as PostgreSQL resolves it in the subquery.
fn correlation<'db, DB: DatabaseLike>(
    table: &ArmTable<'_, 'db, DB>,
    blocker: &ArmTable<'_, 'db, DB>,
    conjunct: &Expr,
) -> Option<Correlation<'db, DB>> {
    let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(conjunct) else {
        return None;
    };
    [(left, right), (right, left)].into_iter().find_map(|(blocking, arm)| {
        let blocking_column = blocker.column(blocking)?;
        let arm_column = table.column(arm).filter(|_| blocker.column(arm).is_none())?;
        Some(Correlation { blocking_column, arm_column })
    })
}

/// Whether `condition` names a column of the arm's table anywhere, a name
/// the blocking table holds naming the blocking table's.
fn reads_arm<DB: DatabaseLike>(
    table: &ArmTable<'_, '_, DB>,
    blocker: &ArmTable<'_, '_, DB>,
    condition: &Expr,
) -> bool {
    walk_expressions(condition, |expression| {
        if blocker.column(expression).is_none() && table.column(expression).is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

/// The `ON` condition of an inner `join`, the one spelling of a join an arm
/// reads.
fn join_condition(join: &Join) -> Option<&Expr> {
    let (JoinOperator::Join(JoinConstraint::On(on)) | JoinOperator::Inner(JoinConstraint::On(on))) =
        &join.join_operator
    else {
        return None;
    };
    Some(on)
}

/// The indexes in `rows` of the rows whose columns `node` names, each once,
/// `None` where it names bare a column two of them hold, which PostgreSQL
/// refuses as ambiguous.
fn rows_named<DB: DatabaseLike, N: Visit + ?Sized>(
    rows: &[ArmTable<'_, '_, DB>],
    node: &N,
) -> Option<Vec<usize>> {
    let mut named = Vec::new();
    let walked = walk_expressions(node, |expression| {
        if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return ControlFlow::Continue(());
        }
        let mut holding = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.column(expression).is_some())
            .map(|(index, _)| index);
        match (holding.next(), holding.next()) {
            (_, Some(_)) => return ControlFlow::Break(()),
            (Some(index), None) if !named.contains(&index) => named.push(index),
            _ => {}
        }
        ControlFlow::Continue(())
    });
    walked.is_continue().then_some(named)
}

/// Whether `columns` hold every column of the primary key of `table`, or of
/// a valid unique key of it over plain columns with no predicate.
fn holds_key<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    columns: &[&'db DB::Column],
    database: &'db DB,
) -> Result<bool, LookupError> {
    let held = |key: &mut dyn Iterator<Item = &'db DB::Column>| {
        let mut key = key.peekable();
        key.peek().is_some()
            && key.all(|column| columns.iter().any(|held| core::ptr::eq(*held, column)))
    };
    if held(&mut table.primary_key_columns(database)?) {
        return Ok(true);
    }
    for index in table.unique_indices(database)? {
        if plain_key(index, database)? && held(&mut index.columns(database)?) {
            return Ok(true);
        }
    }
    for index in table.indices(database)?.filter(|index| index.is_unique()) {
        if index.is_valid(database)?
            && plain_key(index, database)?
            && held(&mut index.columns(database)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `index` is over plain columns with no predicate.
fn plain_key<I: IndexLike>(index: &I, database: &I::DB) -> Result<bool, LookupError> {
    Ok(index.predicate().is_none() && index.is_simple(database)?)
}
