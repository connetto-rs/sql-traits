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
        ArgMode, BinaryOperator, DataType, Distinct, Expr, Function, FunctionArg, FunctionArgExpr,
        FunctionArgumentList, FunctionArguments, FunctionBehavior, FunctionSecurity, GroupByExpr,
        Ident, InTarget, Query, Select, SelectFlavor, SelectItem, SetExpr, SetOperator, Statement,
        TableFactor, Value, visit_expressions,
    },
    parser::Parser,
};

use super::function_body::{SqlBody, written_sql_body};
use crate::{
    errors::{HelperSetError, LookupError},
    structs::{
        Deployment, FunctionBody, GrantArm, HelperArm, HelperSet, IdentifierCase, LiteralSet,
        ParentArm, RowSecurity, RowStatement, SessionSetting, SettingContent, SettingValue,
        TargetName,
    },
    traits::{
        ColumnLike, DatabaseLike, DialectLike, ForeignKeyLike, FunctionLike, Relation, RoleLike,
        SchemaLike, TableLike,
    },
    utils::{
        call_sites::call_reaches,
        identifier_resolution::{SearchPath, identifiers_match},
        object_name::{
            Qualifier, object_name_last_part, qualifier_of, resolve_relation_object_name_on_path,
            target_name_of_table,
        },
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

/// A grant arm's comparison of a column to the caller.
struct Caller<'db, DB: DatabaseLike> {
    column: &'db DB::Column,
    setting: &'db SessionSetting,
    /// Whether the body reads the setting without `missing_ok`.
    raises_when_unset: bool,
}

/// A value the body passes: the literal it compares as and the value a
/// translator writes back.
#[derive(Clone)]
struct Passed {
    literal: Literal,
    value: Value,
}

/// One input parameter of the helper being read, bound to its argument.
struct Parameter<'f> {
    name: Option<TargetName<'f>>,
    passed: Passed,
}

/// The helper whose arm is being read, with what its body's names resolve to.
struct Scope<'db, DB: DatabaseLike> {
    function: &'db DB::Function,
    name: String,
    path: SearchPath<'db>,
    /// The entries of [`path`](Self::path) whose schema the owner holds
    /// `USAGE` on, which are the ones PostgreSQL searches for the owner.
    usable_path: SearchPath<'db>,
    parameters: Vec<Parameter<'db>>,
    owner: &'db DB::Role,
}

impl<DB: DatabaseLike> Scope<'_, DB> {
    fn arm_shape(&self, arm: &impl ToString) -> HelperSetError {
        HelperSetError::ArmShape { function: self.name.clone(), arm: arm.to_string() }
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
        let _: ControlFlow<()> = visit_expressions(select, |expression| {
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
    /// The rows whose column holds one of the values.
    In(&'db DB::Column, Vec<Passed>),
}

impl<'db, DB: DatabaseLike> Kept<'db, DB> {
    fn of(equality: Equality) -> Self {
        if equality == Equality::Holds { Self::All } else { Self::Nothing }
    }

    fn values(column: &'db DB::Column, values: Vec<Passed>) -> Self {
        if values.is_empty() { Self::Nothing } else { Self::In(column, values) }
    }

    fn and(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::Nothing, _) | (_, Self::Nothing) => Some(Self::Nothing),
            (Self::All, kept) | (kept, Self::All) => Some(kept),
            (Self::In(left, mut values), Self::In(right, others)) => {
                core::ptr::eq(left, right).then(|| {
                    values
                        .retain(|value| others.iter().any(|other| other.literal == value.literal));
                    Self::values(left, values)
                })
            }
        }
    }

    fn or(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Some(Self::All),
            (Self::Nothing, kept) | (kept, Self::Nothing) => Some(kept),
            (Self::In(left, mut values), Self::In(right, others)) => {
                core::ptr::eq(left, right).then(|| {
                    for other in others {
                        if !values.iter().any(|value| value.literal == other.literal) {
                            values.push(other);
                        }
                    }
                    Self::In(left, values)
                })
            }
        }
    }
}

/// One side of a comparison in a grant arm.
enum Operand<'db, DB: DatabaseLike> {
    Column(&'db DB::Column),
    Passed(Passed),
    Setting { name: String, raises_when_unset: bool },
    Other,
}

/// One arm as read, the table whose primary key it returns, where it returns
/// one, beside the arm, `None` where its condition keeps no row.
type ReadArm<'db, DB> = (Option<&'db <DB as DatabaseLike>::Table>, Option<HelperArm<'db, DB>>);

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
        let Some(owner) = function.owner(database)? else {
            return Err(HelperSetError::UnsettledOwner { function: name });
        };
        let Some(owner) = database.role(owner) else {
            return Err(HelperSetError::UndeclaredOwner {
                function: name,
                owner: owner.to_string(),
            });
        };
        let usable_path = path
            .iter()
            .filter(|(schema, quoted)| {
                self.declared_schema(schema, *quoted)
                    .is_none_or(|schema| schema.has_usage(owner, database))
            })
            .cloned()
            .collect();
        let scope = Scope { function, name, path, usable_path, parameters, owner };

        // A body bound at creation resolves its names with no path, so it is
        // not read as one written to run under the pinned path.
        let (written_body, body) = written_sql_body(function, database)?;
        let SqlBody::Statements(statements) = &body else {
            return Err(scope.arm_shape(&written(written_body)));
        };
        let [Statement::Query(query)] = statements.as_ref() else {
            return Err(scope.arm_shape(&written(written_body)));
        };
        let mut selects = Vec::new();
        union_arms(query, &mut selects).map_err(|arm| scope.arm_shape(&arm))?;

        self.answering.push(function);
        let read = selects
            .into_iter()
            .map(|select| self.arm(&scope, select))
            .collect::<Result<Vec<_>, _>>();
        self.answering.pop();
        let read = read?;
        let mut keyed = read.iter().map(|(keyed, _)| *keyed);
        let covered_table = keyed.next().flatten().filter(|first| {
            keyed.all(|other| other.is_some_and(|other| core::ptr::eq(other, *first)))
        });
        let arms = read.into_iter().filter_map(|(_, arm)| arm).collect();
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
        let ([item], [from], Some(selection)) =
            (select.projection.as_slice(), select.from.as_slice(), &select.selection)
        else {
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
        self.require_select(scope, select, &table)?;
        let keyed = self.keyed_table(table.table, column)?;
        if let Expr::In { expr, target: InTarget::Subquery(called), negated: false, .. } =
            unnest(selection)
        {
            let arm = self.parent_arm(scope, select, &table, column, expr, called)?;
            return Ok((keyed, Some(arm)));
        }
        Ok((keyed, self.grant_arm(scope, select, &table, column, selection)?))
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
            return Err(scope.arm_shape(select));
        };
        if !unjoined
            || !with_hints.is_empty()
            || !partitions.is_empty()
            || !index_directives.is_empty()
            || alias.as_ref().is_some_and(|alias| !alias.columns.is_empty())
        {
            return Err(scope.arm_shape(select));
        }
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
            Some(alias) => Cow::Borrowed(&alias.name),
            None => Cow::Owned(Ident::new(table.table_name())),
        };
        Ok(ArmTable { table, qualifier })
    }

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
        if table
            .table
            .primary_key_column(database)?
            .is_none_or(|primary| !core::ptr::eq(primary, key))
        {
            return Err(shape());
        }
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
        let call = called_helper(subquery).ok_or_else(shape)?;
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
        let callee = self.called(scope, call, arguments.len())?;
        if !scope.owner.is_superuser() && !callee.can_execute(scope.owner, database)? {
            return Err(HelperSetError::MissingExecute {
                function: scope.name.clone(),
                owner: scope.owner.name().to_string(),
                called: callee.target_name().to_string(),
            });
        }
        let set = self.answer(callee, &arguments)?;
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

    /// The one helper a parent arm's call reaches under the path its body
    /// runs with.
    fn called(
        &self,
        scope: &Scope<'db, DB>,
        call: &Function,
        arguments: usize,
    ) -> Result<&'db DB::Function, HelperSetError> {
        let reached = |path: &SearchPath<'db>| {
            let path = || path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
            let mut reached = self.database.functions().filter(move |function| {
                call_reaches(*function, &call.name, arguments, path, IdentifierCase::AsWritten)
            });
            match (reached.next(), reached.next()) {
                (Some(callee), None) => Some(callee),
                _ => None,
            }
        };
        let unresolved = || {
            HelperSetError::UnresolvedCall {
                function: scope.name.clone(),
                call: call.name.to_string(),
            }
        };
        let Some(callee) = reached(&scope.usable_path) else {
            // A helper only a schema the owner cannot use holds is not found.
            if let Some(hidden) = reached(&scope.path) {
                self.require_usage(scope, function_schema(&hidden.target_name()))?;
            }
            return Err(unresolved());
        };
        self.require_usage(scope, function_schema(&callee.target_name()))?;
        Ok(callee)
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

    /// Refuses the answer where the owner holds `SELECT` neither on the
    /// arm's table nor on every column of it the arm reads.
    fn require_select(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
    ) -> Result<(), HelperSetError> {
        let held = table.table.privileges(scope.owner, self.database)?.select;
        if held.on_table()
            || table
                .columns_read(select, self.database)?
                .iter()
                .all(|column| held.on_column(column))
        {
            return Ok(());
        }
        Err(HelperSetError::MissingSelect {
            function: scope.name.clone(),
            owner: scope.owner.name().to_string(),
            table: target_name_of_table(table.table).to_string(),
        })
    }

    fn grant_arm(
        &self,
        scope: &Scope<'db, DB>,
        select: &Select,
        table: &ArmTable<'_, 'db, DB>,
        column: &'db DB::Column,
        selection: &Expr,
    ) -> Result<Option<HelperArm<'db, DB>>, HelperSetError> {
        let mut conjuncts = Vec::new();
        conjunction(selection, &mut conjuncts);
        let mut caller = None;
        let mut conditions = Vec::new();
        for conjunct in conjuncts {
            if let Some(found) = self.caller(scope, table, conjunct)? {
                if caller.replace(found).is_some() {
                    return Err(scope.arm_shape(select));
                }
            } else {
                conditions.push(conjunct);
            }
        }
        let Some(Caller { column: caller_column, setting, raises_when_unset }) = caller else {
            for condition in conditions {
                if let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(condition) {
                    for (column, parameter) in [(left, right), (right, left)] {
                        if table.column(column, self.database)?.is_some()
                            && let Some((_, parameter)) = scope.parameter(unnest(parameter))
                        {
                            return Err(HelperSetError::CallerFromParameter {
                                function: scope.name.clone(),
                                parameter,
                            });
                        }
                    }
                }
            }
            return Err(scope.arm_shape(select));
        };
        let mut kept = Kept::All;
        for condition in conditions {
            let irreducible = || {
                HelperSetError::Irreducible {
                    function: scope.name.clone(),
                    condition: condition.to_string(),
                }
            };
            let reduced = self.reduce(scope, table, condition)?.ok_or_else(irreducible)?;
            kept = kept.and(reduced).ok_or_else(irreducible)?;
        }
        let restriction = match kept {
            Kept::Nothing => return Ok(None),
            Kept::All => None,
            Kept::In(column, values) => {
                Some(LiteralSet {
                    column,
                    values: values.into_iter().map(|passed| passed.value).collect(),
                })
            }
        };
        Ok(Some(HelperArm::Grant(GrantArm {
            table: table.table,
            column,
            caller_column,
            setting,
            raises_when_unset,
            restriction,
        })))
    }

    /// The caller a grant arm compares a column to, where `conjunct` does.
    fn caller(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        conjunct: &Expr,
    ) -> Result<Option<Caller<'db, DB>>, HelperSetError> {
        let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnest(conjunct) else {
            return Ok(None);
        };
        let sides = (self.operand(scope, table, left)?, self.operand(scope, table, right)?);
        let ((Operand::Column(column), Operand::Setting { name, raises_when_unset })
        | (Operand::Setting { name, raises_when_unset }, Operand::Column(column))) = sides
        else {
            return Ok(None);
        };
        let Some(setting) = self.deployment.setting(&name) else {
            return Err(HelperSetError::UndeclaredSetting {
                function: scope.name.clone(),
                setting: name,
            });
        };
        if !matches!(setting.content(), SettingContent::Value(SettingValue::Caller(_))) {
            return Err(HelperSetError::SettingNotCaller {
                function: scope.name.clone(),
                setting: setting.name().to_string(),
            });
        }
        Ok(Some(Caller { column, setting, raises_when_unset }))
    }

    fn operand(
        &self,
        scope: &Scope<'db, DB>,
        table: &ArmTable<'_, 'db, DB>,
        expression: &Expr,
    ) -> Result<Operand<'db, DB>, HelperSetError> {
        let expression = unnest(expression);
        if let Some(column) = table.column(expression, self.database)? {
            return Ok(Operand::Column(column));
        }
        if let Some((passed, _)) = scope.parameter(expression) {
            return Ok(Operand::Passed(passed.clone()));
        }
        match expression {
            Expr::Value(value) => {
                Ok(Literal::of(&value.value).map_or(Operand::Other, |literal| {
                    Operand::Passed(Passed { literal, value: value.value.clone() })
                }))
            }
            Expr::Function(call) => Ok(self.setting_read(scope, call).unwrap_or(Operand::Other)),
            _ => Ok(Operand::Other),
        }
    }

    /// `current_setting('<name>'[, <missing_ok>])`, resolved to PostgreSQL's
    /// own function.
    fn setting_read(&self, scope: &Scope<'db, DB>, call: &Function) -> Option<Operand<'db, DB>> {
        // A qualifier built when the statement runs names no schema.
        let catalog = match qualifier_of(&call.name) {
            Qualifier::Named(schema, quoted) => {
                identifiers_match(schema, quoted, "pg_catalog", false)
            }
            qualifier => qualifier == Qualifier::Absent,
        };
        let (name, quoted) = object_name_last_part(&call.name)?;
        if !catalog || !identifiers_match(name, quoted, "current_setting", false) {
            return None;
        }
        let list = plain_arguments(call)?;
        // PostgreSQL's own `current_setting` is in `pg_catalog`, and an
        // overload declared in a schema the owner can use, which the call may
        // reach, is refused.
        let path = || scope.usable_path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
        if self.database.functions().any(|function| {
            call_reaches(function, &call.name, list.args.len(), path, IdentifierCase::AsWritten)
                && function.target_name().schema() != Some("pg_catalog")
        }) {
            return None;
        }
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
        Some(Operand::Setting { name, raises_when_unset })
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
                Ok(left.and(right))
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
    query.with.is_none()
        && query.order_by.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty()
        && query.for_clause.is_none()
        && query.settings.is_none()
        && query.format_clause.is_none()
        && query.pipe_operators.is_empty()
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

/// The helper call of `(SELECT <helper>(<arguments>))`.
fn called_helper(query: &Query) -> Option<&Function> {
    let SetExpr::Select(select) = query.body.as_ref() else { return None };
    let [SelectItem::UnnamedExpr(Expr::Function(call))] = select.projection.as_slice() else {
        return None;
    };
    (plain_query(query)
        && plain_select(select)
        && select.from.is_empty()
        && select.selection.is_none())
    .then_some(call)
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
