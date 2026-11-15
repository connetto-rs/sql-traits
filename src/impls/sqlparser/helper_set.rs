//! The answer behind [`FunctionLike::helper_set`], read from the body
//! [`FunctionLike::written_body`] hands over.

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::{
    ast::{
        ArgMode, BinaryOperator, CastKind, DataType, Distinct, Expr, Function, FunctionArg,
        FunctionArgExpr, FunctionArgumentList, FunctionArguments, FunctionBehavior,
        FunctionSecurity, GroupByExpr, Ident, InTarget, JoinConstraint, JoinOperator, ObjectName,
        ObjectNamePart, Query, Select, SelectFlavor, SelectItem, SetExpr, SetOperator,
        SetQuantifier, Statement, TableAliasColumnDef, TableFactor, Value,
    },
    parser::Parser,
};

use super::{
    data_type::written_element,
    function_body::{SqlBody, written_sql_body},
};
use crate::{
    errors::{AccessorRefusal, HelperSetError, LookupError},
    structs::{
        CallerGate, Deployment, FunctionBody, GrantArm, GrantedTo, GranteeSet, HelperArm,
        HelperSet, IdentifierCase, LiteralSet, ParentArm, RowSecurity, RowStatement,
        SessionSetting, SetPacking, SettingComparison, SettingContent, SettingValue, TargetName,
        TimeBound, TimeComparison, WalkArm, WalkDirection,
    },
    traits::{
        ColumnLike, DataTypeLike, DatabaseLike, DialectLike, ForeignKeyLike, FunctionLike,
        Relation, RoleLike, SchemaLike, TableLike,
    },
    utils::{
        call_sites::{ExpressionForm, call_reaches, expression_form},
        identifier_resolution::{SearchPath, identifiers_match},
        object_name::{
            PG_CATALOG, Qualifier, object_name_last_part, qualifier_of,
            resolve_relation_object_name_on_path, target_name_of_table, type_search_path,
        },
        postgres_types::{PgType, parse_data_type},
        scalar_family::ScalarFamily,
        type_modifier,
        walk::walk_expressions,
    },
};

/// See [`FunctionLike::helper_set`].
pub(crate) fn helper_set<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    arguments: &[Value],
    deployment: &'db Deployment,
    database: &'db DB,
) -> Result<HelperSet<'db, DB>, HelperSetError> {
    Reader { deployment, database, answering: Vec::new() }.answer(function, arguments)
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
/// function, or that read through `NULLIF(<read>, '')`, either returned by
/// an accessor the body calls.
struct SettingRead {
    name: String,
    /// Whether the body reads the setting without `missing_ok`.
    raises_when_unset: bool,
    /// Whether the body reads it through `NULLIF(<read>, '')`, which reads
    /// `''` as `NULL`.
    empty_as_null: bool,
    /// Whether an accessor returns it cast to the type the deployment
    /// declares it in.
    in_declared_type: bool,
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

    /// Splits the body into its arms and, where it writes `WITH RECURSIVE`,
    /// the term walking down a tree under them.
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
        if arms
            .iter()
            .any(|arm| arm.from.iter().any(|from| walk_qualifier(&from.relation, name).is_some()))
        {
            return Err(shape());
        }
        let column = match cte.alias.columns.as_slice() {
            [] => arms.first().and_then(|first| output_name(first)).ok_or_else(shape)?,
            [TableAliasColumnDef { name, data_type: None }] => name,
            _ => return Err(shape()),
        };
        let SetExpr::Select(last) = query.body.as_ref() else { return Err(shape()) };
        let ([from], [item], None) =
            (last.from.as_slice(), last.projection.as_slice(), &last.selection)
        else {
            return Err(shape());
        };
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(shape());
        };
        let selects_column = from.joins.is_empty()
            && walk_qualifier(&from.relation, name)
                .is_some_and(|qualifier| names_column(returned, qualifier, column, true));
        if !plain_select(last) || !selects_column {
            return Err(shape());
        }
        Ok(Body { arms, walk: Some(Walk { name, column, term }) })
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

/// The table an arm reads, with the name its columns are qualified by.
struct ArmTable<'q, 'db, DB: DatabaseLike> {
    table: &'db DB::Table,
    qualifier: Cow<'q, Ident>,
}

impl<'db, DB: DatabaseLike> ArmTable<'_, 'db, DB> {
    /// The column `expression` names, if it names one of the table's.
    fn column(
        &self,
        expression: &Expr,
        database: &'db DB,
    ) -> Result<Option<&'db DB::Column>, LookupError> {
        let Some(name) = self.column_name(expression) else { return Ok(None) };
        self.table.column(&name.to_string(), database, IdentifierCase::AsWritten)
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

    /// Every column of the table `select` names, each once, which the owner
    /// needs `SELECT` on wherever the name stands.
    fn columns_read(
        &self,
        select: &Select,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Column>, LookupError> {
        let columns: Vec<&'db DB::Column> = self.table.columns(database)?.collect();
        let mut read: Vec<&'db DB::Column> = Vec::new();
        let _: ControlFlow<()> = walk_expressions(select, |expression| {
            let named = self.column_name(expression).and_then(|name| {
                let name = name.to_string();
                columns.iter().copied().find(|column| {
                    IdentifierCase::AsWritten.names_stored(
                        column.column_name(),
                        column.column_name_is_quoted(),
                        &name,
                    )
                })
            });
            if let Some(column) = named
                && !read.iter().any(|seen| core::ptr::eq(*seen, column))
            {
                read.push(column);
            }
            ControlFlow::Continue(())
        });
        Ok(read)
    }
}

/// What a grant arm's condition keeps.
enum Kept<'db, DB: DatabaseLike> {
    /// Every row.
    All,
    /// No row, the condition being `FALSE` or `NULL`.
    Nothing,
    /// The rows each listed column of which holds one of its values, each
    /// column listed once.
    In(Vec<(&'db DB::Column, Vec<Passed>)>),
}

impl<'db, DB: DatabaseLike> Kept<'db, DB> {
    fn of(equality: Equality) -> Self {
        if equality == Equality::Holds { Self::All } else { Self::Nothing }
    }

    fn values(column: &'db DB::Column, values: Vec<Passed>) -> Self {
        if values.is_empty() { Self::Nothing } else { Self::In(vec![(column, values)]) }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Nothing, _) | (_, Self::Nothing) => Self::Nothing,
            (Self::All, kept) | (kept, Self::All) => kept,
            (Self::In(mut sets), Self::In(others)) => {
                for (column, others) in others {
                    let Some((_, values)) =
                        sets.iter_mut().find(|(seen, _)| core::ptr::eq(*seen, column))
                    else {
                        sets.push((column, others));
                        continue;
                    };
                    values
                        .retain(|value| others.iter().any(|other| other.literal == value.literal));
                    if values.is_empty() {
                        return Self::Nothing;
                    }
                }
                Self::In(sets)
            }
        }
    }

    /// `self OR other`, `None` where the two restrict anything but the same
    /// one column.
    fn or(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Some(Self::All),
            (Self::Nothing, kept) | (kept, Self::Nothing) => Some(kept),
            (Self::In(mut sets), Self::In(mut others)) => {
                let (Some((right, others)), None) = (others.pop(), others.pop()) else {
                    return None;
                };
                let [(left, values)] = sets.as_mut_slice() else { return None };
                if !core::ptr::eq(*left, right) {
                    return None;
                }
                for other in others {
                    if !values.iter().any(|value| value.literal == other.literal) {
                        values.push(other);
                    }
                }
                Some(Self::In(sets))
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

/// One arm as read, the table whose primary key it returns, where it returns
/// one, beside the arm, `None` where its condition keeps no row.
type ReadArm<'db, DB> = (Option<&'db <DB as DatabaseLike>::Table>, Option<HelperArm<'db, DB>>);

/// A helper body's arms, with the term walking down a tree under them where
/// the body writes `WITH RECURSIVE`.
struct Body<'q> {
    arms: Vec<&'q Select>,
    walk: Option<Walk<'q>>,
}

/// The recursive term of a body, with the name and the one column it reads
/// the set by.
struct Walk<'q> {
    name: &'q Ident,
    column: &'q Ident,
    term: &'q Select,
}

/// Answers helpers, following parent arms into the helpers they call.
struct Reader<'db, DB: DatabaseLike> {
    deployment: &'db Deployment,
    database: &'db DB,
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
        if !returns_scalar_set(function, database) {
            return Err(HelperSetError::ReturnType { function: name });
        }
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

        self.answering.push(function);
        let read = body
            .arms
            .into_iter()
            .map(|select| self.arm(&scope, select))
            .collect::<Result<Vec<_>, _>>();
        self.answering.pop();
        let mut read = read?;
        if let Some(walk) = &body.walk {
            let walk = self.walk_arm(&scope, walk)?;
            read.push((Some(walk.table), Some(HelperArm::Walk(walk))));
        }
        let mut keyed = read.iter().map(|(keyed, _)| *keyed);
        let covered_table = keyed.next().flatten().filter(|first| {
            keyed.all(|other| other.is_some_and(|other| core::ptr::eq(other, *first)))
        });
        let mut arms: Vec<_> = read.into_iter().filter_map(|(_, arm)| arm).collect();
        // A walk under arms keeping no row starts from nothing.
        if arms.iter().all(|arm| matches!(arm, HelperArm::Walk(_))) {
            arms.clear();
        }
        Ok(HelperSet { arms, covered_table })
    }

    /// Reads one arm, answering the table whose primary key it returns,
    /// where it returns one, and the arm, `None` where its condition keeps
    /// no row.
    fn arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
    ) -> Result<ReadArm<'db, DB>, HelperSetError> {
        if !plain_select(select) {
            return Err(scope.arm_shape(select));
        }
        let ([item], [from]) = (select.projection.as_slice(), select.from.as_slice()) else {
            return Err(scope.arm_shape(select));
        };
        let table = self.arm_table(scope, select, &from.relation, from.joins.is_empty())?;
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(scope.arm_shape(select));
        };
        let Some(column) = table.column(returned, self.database)? else {
            return Err(scope.arm_shape(select));
        };
        let database = self.database;
        self.require_select(scope, table.table, || table.columns_read(select, database))?;
        let keyed = self.keyed_table(table.table, column)?;
        let returns_key = table
            .table
            .primary_key_column(database)?
            .is_some_and(|primary| core::ptr::eq(primary, column));
        if returns_key
            && let Some(Expr::In {
                expr, target: InTarget::Subquery(called), negated: false, ..
            }) = select.selection.as_ref().map(unnest)
            && self.set_elements(scope, called)?.is_none()
        {
            let arm = self.parent_arm(scope, select, &table, column, expr, called)?;
            return Ok((keyed, Some(arm)));
        }
        Ok((keyed, self.grant_arm(scope, select, &table, column, returns_key)?))
    }

    /// Reads the recursive term of a walk, `SELECT <key> FROM <table> JOIN
    /// <walk> ON <parent column> = <walk>.<column>` walking down and `SELECT
    /// <parent column> FROM <table> JOIN <walk> ON <key> = <walk>.<column>`
    /// walking up, the join either way round. PostgreSQL refuses the walk's
    /// name in a subquery of the term, so the join is its one spelling.
    fn walk_arm(
        &self,
        scope: &Scope<'db, DB>,
        walk: &Walk<'_>,
    ) -> Result<WalkArm<'db, DB>, HelperSetError> {
        let database = self.database;
        let term = walk.term;
        let shape = || scope.arm_shape(term);
        let ([item], [from]) = (term.projection.as_slice(), term.from.as_slice()) else {
            return Err(shape());
        };
        let (SelectItem::UnnamedExpr(returned) | SelectItem::ExprWithAlias { expr: returned, .. }) =
            item
        else {
            return Err(shape());
        };
        let ([join], None) = (from.joins.as_slice(), &term.selection) else {
            return Err(shape());
        };
        let (JoinOperator::Join(JoinConstraint::On(on))
        | JoinOperator::Inner(JoinConstraint::On(on))) = &join.join_operator
        else {
            return Err(shape());
        };
        let (relation, qualifier) = match (
            walk_qualifier(&from.relation, walk.name),
            walk_qualifier(&join.relation, walk.name),
        ) {
            (None, Some(qualifier)) => (&from.relation, qualifier),
            (Some(qualifier), None) => (&join.relation, qualifier),
            _ => return Err(shape()),
        };
        let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(on) else {
            return Err(shape());
        };
        let compared = if names_column(right, qualifier, walk.column, false) {
            left
        } else if names_column(left, qualifier, walk.column, false) {
            right
        } else {
            return Err(shape());
        };
        // A bare name the walk's column shares is ambiguous in the join.
        let ambiguous = |expression: &Expr| matches!(unnest(expression), Expr::Identifier(ident) if same_ident(ident, walk.column));
        if !plain_select(term) || join.global || ambiguous(compared) || ambiguous(returned) {
            return Err(shape());
        }
        let table = self.arm_table(scope, term, relation, true)?;
        let (Some(key), Some(returned), Some(compared)) = (
            table.table.primary_key_column(database)?,
            table.column(returned, database)?,
            table.column(compared, database)?,
        ) else {
            return Err(shape());
        };
        let (column, direction) = match (core::ptr::eq(returned, key), core::ptr::eq(compared, key))
        {
            (true, false) => (compared, WalkDirection::Down),
            (false, true) => (returned, WalkDirection::Up),
            _ => return Err(shape()),
        };
        let [foreign_key] = single_column_foreign_keys(table.table, column, database)?[..] else {
            return Err(shape());
        };
        let references_key = core::ptr::eq(foreign_key.referenced_table(database)?, table.table)
            && foreign_key
                .referenced_columns(database)?
                .next()
                .is_some_and(|referenced| core::ptr::eq(referenced, key));
        if !references_key {
            return Err(shape());
        }
        self.require_select(scope, table.table, || Ok(vec![key, column]))?;
        Ok(WalkArm { table: table.table, key, column, foreign_key, direction })
    }

    /// The table whose primary key `column` of `table` holds, `table` itself
    /// where `column` is its primary key, else the table its one
    /// single-column foreign key references, where that key references the
    /// primary key.
    fn keyed_table(
        &self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Result<Option<&'db DB::Table>, LookupError> {
        let database = self.database;
        if table.primary_key_column(database)?.is_some_and(|primary| core::ptr::eq(primary, column))
        {
            return Ok(Some(table));
        }
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
        Ok(core::ptr::eq(primary, referenced_column).then_some(referenced))
    }

    /// Resolves the one table an arm reads, and checks the owner reads all
    /// of it.
    fn arm_table<'q>(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        relation: &'q TableFactor,
        unjoined: bool,
    ) -> Result<ArmTable<'q, 'db, DB>, HelperSetError> {
        let database = self.database;
        let Some((name, alias)) = plain_table(relation).filter(|_| unjoined) else {
            return Err(scope.arm_shape(select));
        };
        let resolve = |path: &SearchPath<'db>| {
            resolve_relation_object_name_on_path(name, database, path, IdentifierCase::AsWritten)
        };
        let table = match resolve(&scope.usable_path)? {
            Some(Relation::Table(table)) => table,
            Some(_) => return Err(scope.arm_shape(select)),
            None => {
                // A table only a schema the owner cannot use holds is not
                // found.
                if let Some(Relation::Table(hidden)) = resolve(&scope.path)? {
                    self.require_usage(scope, table_schema(hidden))?;
                }
                return Err(scope.arm_shape(select));
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
        Ok(ArmTable { table, qualifier })
    }

    /// Reads `<column> IN (SELECT <helper>(<arguments>))` in an arm returning
    /// `key`, its table's primary key.
    fn parent_arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        key: &'db DB::Column,
        compared: &Expr,
        subquery: &Query,
    ) -> Result<HelperArm<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let Some(column) = table.column(compared, database)? else {
            return Err(shape());
        };
        let foreign_key = match single_column_foreign_keys(table.table, column, database)?[..] {
            [foreign_key] => Some(foreign_key),
            // Without a foreign key the set names the parent, and the key the
            // arm returns names no row but its own.
            [] if !core::ptr::eq(column, key) => None,
            _ => return Err(shape()),
        };
        let Called { function: callee, arguments, set } =
            self.called_set(scope, select, table, subquery)?;
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
        let Some(column) = table.column(compared, database)? else {
            return Err(shape());
        };
        let foreign_key = match single_column_foreign_keys(table.table, column, database)?[..] {
            [] => None,
            [foreign_key] => Some(foreign_key),
            _ => return Err(shape()),
        };
        let Called { function, arguments, set } =
            self.called_set(scope, select, table, subquery)?;
        Ok(GranteeSet { column, foreign_key, function, arguments, set: Box::new(set) })
    }

    /// The helper `(SELECT <helper>(<arguments>))` calls, the arguments it
    /// passes, each a parameter substituted or a literal, and what the helper
    /// answers for them.
    fn called_set(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        subquery: &Query,
    ) -> Result<Called<'db, DB>, HelperSetError> {
        let database = self.database;
        let shape = || scope.arm_shape(select);
        let call = projected_call(subquery).ok_or_else(shape)?;
        let list = plain_arguments(call).ok_or_else(shape)?;
        let mut arguments = Vec::new();
        for argument in &list.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(argument)) = argument else {
                return Err(shape());
            };
            // A column of the table takes the name before a parameter does,
            // and passes a value of each row.
            if table.column(argument, database)?.is_some() {
                return Err(shape());
            }
            let passed = scope.parameter(argument).map(|(passed, _)| passed.value.clone());
            let literal = || {
                let Expr::Value(value) = argument else { return None };
                Literal::of(&value.value).map(|_| value.value.clone())
            };
            arguments.push(passed.or_else(literal).ok_or_else(shape)?);
        }
        let Some(callee) = self.called(scope, call, arguments.len())? else {
            return Err(HelperSetError::UnresolvedCall {
                function: scope.name.clone(),
                call: call.name.to_string(),
            });
        };
        let set = self.answer(callee, &arguments)?;
        Ok(Called { function: callee, arguments, set })
    }

    /// The one function `call` reaches under the path its body runs with,
    /// which the owner may execute, `None` where it reaches none or several.
    fn called(
        &self,
        scope: &Scope<'db, DB>,
        call: &Function,
        arguments: usize,
    ) -> Result<Option<&'db DB::Function>, HelperSetError> {
        let database = self.database;
        let name = object_name_last_part(&call.name).map_or("", |(name, _)| name);
        let reached = |path: &SearchPath<'db>| {
            let path = || path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
            let mut reached = database.functions_named(name).filter(move |function| {
                call_reaches(*function, &call.name, arguments, path, IdentifierCase::AsWritten)
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
        read: impl FnOnce() -> Result<Vec<&'db DB::Column>, LookupError>,
    ) -> Result<(), HelperSetError> {
        let held = table.privileges(scope.owner, self.database)?.select;
        if held.on_table() || read()?.iter().all(|column| held.on_column(column)) {
            return Ok(());
        }
        Err(HelperSetError::MissingSelect {
            function: scope.name.clone(),
            owner: scope.owner.name().to_string(),
            table: target_name_of_table(table).to_string(),
        })
    }

    /// Reads a grant arm, `None` where its conditions keep no row. An arm
    /// returning its table's key from a helper's set is a parent arm, which
    /// takes no other condition, so `returns_key` refuses a set beside one.
    fn grant_arm(
        &mut self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        column: &'db DB::Column,
        returns_key: bool,
    ) -> Result<Option<HelperArm<'db, DB>>, HelperSetError> {
        let shape = || scope.arm_shape(select);
        let mut conjuncts = Vec::new();
        if let Some(selection) = &select.selection {
            conjunction(selection, &mut conjuncts);
        }
        let mut granted_to = None;
        let mut gates = Gates::default();
        let mut conditions = Vec::new();
        for conjunct in conjuncts {
            let grantee = if let Some(request) = self.setting_comparison(scope, table, conjunct)? {
                Some(GrantedTo::Request(request))
            } else if let Expr::In {
                expr,
                target: InTarget::Subquery(called),
                negated: false,
                ..
            } = unnest(conjunct)
            {
                if returns_key {
                    return Err(shape());
                }
                Some(GrantedTo::Set(self.grantee_set(scope, select, table, expr, called)?))
            } else {
                None
            };
            if let Some(grantee) = grantee {
                if granted_to.replace(grantee).is_some() {
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
        let mut kept = Kept::All;
        let mut time_bounds = Vec::new();
        for condition in conditions {
            if let Some(bound) = self.time_bound(scope, table, condition)? {
                time_bounds.push(bound);
                continue;
            }
            let reduced = self.reduce(scope, table, condition)?.ok_or_else(|| {
                HelperSetError::Irreducible {
                    function: scope.name.clone(),
                    condition: condition.to_string(),
                }
            })?;
            kept = kept.and(reduced);
        }
        let restrictions = match kept {
            Kept::Nothing => return Ok(None),
            Kept::All => Vec::new(),
            Kept::In(sets) => {
                sets.into_iter()
                    .map(|(column, values)| {
                        LiteralSet {
                            column,
                            values: values.into_iter().map(|passed| passed.value).collect(),
                        }
                    })
                    .collect()
            }
        };
        if gates.keeps_nothing {
            return Ok(None);
        }
        Ok(Some(HelperArm::Grant(GrantArm {
            table: table.table,
            column,
            granted_to: granted_to.unwrap_or(GrantedTo::Anyone),
            gate: gates.gate,
            restrictions,
            time_bounds,
        })))
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
    ) -> Result<Option<TimeBound<'db, DB>>, HelperSetError> {
        let database = self.database;
        let (comparison, null) = match unnest(condition) {
            Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
                match (unnest(left), unnest(right)) {
                    (Expr::IsNull(null), comparison) | (comparison, Expr::IsNull(null)) => {
                        (comparison, Some(null))
                    }
                    _ => return Ok(None),
                }
            }
            comparison => (comparison, None),
        };
        let Expr::BinaryOp { left, op, right } = comparison else { return Ok(None) };
        let (compared, comparison) = if self.statement_time(scope, right) {
            let comparison = match op {
                BinaryOperator::Gt => TimeComparison::Later,
                BinaryOperator::GtEq => TimeComparison::LaterOrEqual,
                BinaryOperator::Lt => TimeComparison::Earlier,
                BinaryOperator::LtEq => TimeComparison::EarlierOrEqual,
                _ => return Ok(None),
            };
            (left, comparison)
        } else if self.statement_time(scope, left) {
            let comparison = match op {
                BinaryOperator::Lt => TimeComparison::Later,
                BinaryOperator::LtEq => TimeComparison::LaterOrEqual,
                BinaryOperator::Gt => TimeComparison::Earlier,
                BinaryOperator::GtEq => TimeComparison::EarlierOrEqual,
                _ => return Ok(None),
            };
            (right, comparison)
        } else {
            return Ok(None);
        };
        let Some(column) = table.column(compared, database)? else { return Ok(None) };
        if column.data_type(database).scalar_family(database) != Some(ScalarFamily::TimestampTz) {
            return Ok(None);
        }
        if let Some(null) = null
            && table.column(null, database)?.is_none_or(|nulled| !core::ptr::eq(nulled, column))
        {
            return Ok(None);
        }
        Ok(Some(TimeBound { column, comparison, keeps_null: null.is_some() }))
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
    /// writes, if it writes one: `<column> = <caller>`, `<column> = ANY
    /// (<array>)` or `<column> IN (SELECT <elements>)`.
    fn setting_comparison(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<SettingComparison<'db, DB>>, HelperSetError> {
        let database = self.database;
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
                let (Some(column), Some(set)) =
                    (table.column(left, database)?, self.set_array(scope, right)?)
                else {
                    return Ok(None);
                };
                (column, set.read, Some(set.packing), set.cast)
            }
            Expr::In { expr, target: InTarget::Subquery(query), negated: false, .. } => {
                let (Some(column), Some(set)) =
                    (table.column(expr, database)?, self.set_elements(scope, query)?)
                else {
                    return Ok(None);
                };
                (column, set.read, Some(set.packing), set.cast)
            }
            _ => return Ok(None),
        };
        let Some(setting) = self.deployment.setting(&read.name) else {
            return Err(HelperSetError::UndeclaredSetting {
                function: scope.name.clone(),
                setting: read.name,
            });
        };
        // `string_to_array('', <delimiter>)` is an empty array.
        let excludes_unset =
            read.empty_as_null || matches!(packing, Some(SetPacking::Delimited(_)));
        let refuse = |reason: fn(String, String) -> HelperSetError| {
            Err(reason(scope.name.clone(), setting.name().to_string()))
        };
        let declared = match (packing, setting.content()) {
            (None, SettingContent::Value(SettingValue::Caller(declared))) => declared,
            (None, _) => {
                return refuse(|function, setting| {
                    HelperSetError::SettingNotCaller { function, setting }
                });
            }
            (
                Some(packing),
                SettingContent::Value(SettingValue::Set { element, packing: declared }),
            ) => {
                if packing != *declared {
                    return refuse(|function, setting| {
                        HelperSetError::MispackedSet { function, setting }
                    });
                }
                element
            }
            (Some(_), _) => {
                return refuse(|function, setting| {
                    HelperSetError::SettingNotSet { function, setting }
                });
            }
        };
        if let Some(cast) = cast
            && !self.is_declared_type(scope, cast.compared, declared)
        {
            return Err(HelperSetError::SettingCast {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
                cast: cast.written.to_string(),
            });
        }
        Ok(Some(SettingComparison {
            column,
            setting,
            raises_when_unset: read.raises_when_unset,
            excludes_unset,
        }))
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
    /// <delimiter>)`, optionally cast to an array, or `ARRAY(SELECT
    /// <elements>)`.
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
        if let FunctionArguments::Subquery(query) = &call.args {
            // `ARRAY(SELECT ...)` is a constructor, which no function shadows.
            let [ObjectNamePart::Identifier(name)] = call.name.0.as_slice() else {
                return Ok(None);
            };
            if cast.is_some()
                || name.quote_style.is_some()
                || !name.value.eq_ignore_ascii_case("array")
            {
                return Ok(None);
            }
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
            return Ok(None);
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
            return Ok(match (cast, set.cast) {
                (Some(_), Some(_)) => None,
                (cast, array_cast) => Some(SetRead { cast: cast.or(array_cast), ..set }),
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
    /// `NULLIF(<read>, '')` or an accessor returning a read.
    fn setting_expression(
        &self,
        scope: &Scope<'db, DB>,
        expression: &Expr,
    ) -> Result<Option<SettingRead>, HelperSetError> {
        let Expr::Function(call) = unnest(expression) else { return Ok(None) };
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
        let database = self.database;
        if !plain_arguments(call).is_some_and(|list| list.args.is_empty()) {
            return Ok(None);
        }
        let Some(accessor) = self.called(scope, call, 0)? else { return Ok(None) };
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
        let Some((read, cast)) = self.typed_setting(&body, returned)? else { return Ok(None) };
        let in_declared_type = cast.is_some() || read.in_declared_type;
        let declared = if in_declared_type {
            let (setting, declared) = self.caller_setting(&body, &read.name)?;
            if let Some(cast) = cast
                && !self.is_declared_type(&body, cast, declared)
            {
                return Err(HelperSetError::SettingCast {
                    function: body.name,
                    setting: setting.name().to_string(),
                    cast: cast.to_string(),
                });
            }
            Some(declared)
        } else {
            None
        };
        let reason = if accessor.volatility() == FunctionBehavior::Immutable {
            AccessorRefusal::Immutable
        } else if sets_setting(accessor, &read.name) {
            AccessorRefusal::SetsSetting
        } else if !self.returns_read(&body, declared) {
            AccessorRefusal::ReturnType
        } else {
            return Ok(Some(SettingRead { in_declared_type, ..read }));
        };
        Err(HelperSetError::Accessor {
            function: scope.name.clone(),
            accessor: body.name,
            setting: read.name,
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
        let Some(setting) = self.deployment.setting(name) else {
            return Err(HelperSetError::UndeclaredSetting {
                function: scope.name.clone(),
                setting: name.to_string(),
            });
        };
        let SettingContent::Value(SettingValue::Caller(declared)) = setting.content() else {
            return Err(HelperSetError::SettingNotCaller {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
            });
        };
        Ok((setting, declared))
    }

    /// Whether the accessor `scope` reads returns one value of the type its
    /// body's read takes, `declared` where the read is cast to it and `text`
    /// where it is not.
    fn returns_read(&self, scope: &Scope<'db, DB>, declared: Option<&DataType>) -> bool {
        let accessor = scope.function;
        if accessor.returns_set() {
            return false;
        }
        let Some(returned) =
            accessor.result_type_name(self.database).and_then(|name| parse_data_type(&name))
        else {
            return false;
        };
        match declared {
            Some(declared) => self.is_declared_type(scope, &returned, declared),
            None => PgType::of(&returned) == PgType::of(&DataType::Text),
        }
    }

    fn operand<'q>(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        expression: &'q Expr,
    ) -> Result<Operand<'q, 'db, DB>, HelperSetError> {
        let expression = unnest(expression);
        if let Some(column) = table.column(expression, self.database)? {
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
        Some(SettingRead { name, raises_when_unset, empty_as_null: false, in_declared_type: false })
    }

    /// The arguments of `call` where it calls PostgreSQL's own `name`, in
    /// `pg_catalog`, and nothing else it could reach.
    fn catalog_arguments<'c>(
        &self,
        scope: &Scope<'db, DB>,
        call: &'c Function,
        name: &str,
    ) -> Option<&'c FunctionArgumentList> {
        // A qualifier built when the statement runs names no schema.
        let catalog = match qualifier_of(&call.name) {
            Qualifier::Named(schema, quoted) => {
                identifiers_match(schema, quoted, "pg_catalog", false)
            }
            qualifier => qualifier == Qualifier::Absent,
        };
        let (called, quoted) = object_name_last_part(&call.name)?;
        if !catalog || !identifiers_match(called, quoted, name, false) {
            return None;
        }
        let list = plain_arguments(call)?;
        // An overload declared in a schema the owner can use, which the call
        // may reach, is refused.
        let path = || scope.usable_path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
        if self.database.functions_named(called).any(|function| {
            call_reaches(function, &call.name, list.args.len(), path, IdentifierCase::AsWritten)
                && function.target_name().schema() != Some("pg_catalog")
        }) {
            return None;
        }
        Some(list)
    }

    /// What `condition` keeps, `None` where it reduces to no literal set.
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
                    Operand::Column(column) => {
                        let mut kept: Vec<Passed> = Vec::new();
                        for passed in values {
                            if passed.literal != Literal::Null
                                && !kept.iter().any(|seen| seen.literal == passed.literal)
                            {
                                kept.push(passed);
                            }
                        }
                        Ok(Some(Kept::values(column, kept)))
                    }
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
            expression => {
                Ok(match self.operand(scope, table, expression)? {
                    Operand::Passed(Passed { literal: Literal::Boolean(boolean), .. }) => {
                        Some(Kept::of(if boolean { Equality::Holds } else { Equality::Fails }))
                    }
                    Operand::Passed(Passed { literal: Literal::Null, .. }) => Some(Kept::Nothing),
                    _ => None,
                })
            }
        }
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

/// Whether `function` is declared `RETURNS SETOF` one scalar type, which a
/// `record` is not.
fn returns_scalar_set<F: FunctionLike>(function: &F, database: &F::DB) -> bool {
    if !function.return_type_name(database).is_some_and(|declared| declared.starts_with("SETOF ")) {
        return false;
    }
    let Some(element) = function.result_type_name(database) else { return false };
    // The element is read back as written, so a quoted `"RECORD"` names a
    // type of its own rather than `record`.
    let element = Parser::new(database.dialect().sqlparser_dialect())
        .try_with_sql(&element)
        .and_then(|mut parser| parser.parse_data_type());
    !element.is_ok_and(|element| is_row(&element))
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
    let TableFactor::Table {
        name,
        alias,
        args: None,
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
        && alias.as_ref().is_none_or(|alias| alias.columns.is_empty() && alias.at.is_none());
    plain.then(|| (name, alias.as_ref().map(|alias| &alias.name)))
}

/// The name `relation`'s columns are qualified by where it reads the walk
/// named `walk`, its alias or the walk's own name.
fn walk_qualifier<'q>(relation: &'q TableFactor, walk: &Ident) -> Option<&'q Ident> {
    let (name, alias) = plain_table(relation)?;
    let [ObjectNamePart::Identifier(part)] = name.0.as_slice() else { return None };
    same_ident(part, walk).then_some(alias.unwrap_or(part))
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

/// The call of `(SELECT <function>(<arguments>))`.
fn projected_call(query: &Query) -> Option<&Function> {
    let Expr::Function(call) = projected(query)? else { return None };
    Some(call)
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
