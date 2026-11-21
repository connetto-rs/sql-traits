//! The places a function body runs SQL text, each with the names its
//! expressions reach and the path it runs under.

use alloc::{
    borrow::Cow,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::iter;

use sqlparser::ast::{ArgMode, Expr, ObjectName, Query, Statement};
use sqlparser_plpgsql::{
    embedded::{Block, DeclarationKind, DeclaredType, Dynamic, expression},
    namespace::{self, FunctionKind, Name, NameKind, Signature, Site, Sql, TriggerVariable},
};

use super::{
    callee_path::calls_path_changer,
    data_statement::{
        BodyPath, BodyWalk, RunTimeBody, resolve_on_body_path, unresolvable, walk_body,
        walk_statement,
    },
    dql::{
        call_nodes::{CallNode, call_nodes},
        scoped_roots::{ScopedRoot, is_input},
    },
    function_body::{SqlBody, run_time_path},
    plpgsql_body::constant_command,
};
use crate::{
    errors::LookupError,
    structs::{
        CallResolution, DynamicExecution, EmbeddedSql, ExpressionContext, ExpressionScope,
        IdentifierCase, OperandType, PlPgSqlScope, PlPgSqlSite, PlPgSqlVariable, SiteFlow,
        SiteScope, TargetName, TypeKind, VariableKind, array_of, column_type, declared_type,
        declared_type_named, plpgsql_flow::FlowPoint,
    },
    traits::{ColumnLike, DatabaseLike, FunctionLike, Relation, TableLike},
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        object_name::{PG_CATALOG, object_name_last_part},
        postgres_types::PgType,
    },
};

/// A `pg_catalog` function running SQL text one of its arguments gives.
struct TextSink {
    /// The function's name.
    name: &'static str,
    /// How many input parameters the overload takes.
    inputs: usize,
    /// The input parameter, counted from 0, the text passes.
    text: usize,
}

/// Every overload PostgreSQL 18 declares in `pg_catalog` with an argument it
/// runs as SQL text. `query_to_xmlschema` only plans the text, which runs
/// the immutable calls of constant arguments it folds.
const TEXT_SINKS: [TextSink; 6] = [
    TextSink { name: "query_to_xml", inputs: 4, text: 0 },
    TextSink { name: "query_to_xml_and_xmlschema", inputs: 4, text: 0 },
    TextSink { name: "query_to_xmlschema", inputs: 4, text: 0 },
    TextSink { name: "ts_stat", inputs: 1, text: 0 },
    TextSink { name: "ts_stat", inputs: 2, text: 0 },
    TextSink { name: "ts_rewrite", inputs: 2, text: 1 },
];

/// An argument of a function, as its PL/pgSQL body reaches it.
#[derive(Debug)]
struct Parameter<'db> {
    /// The name, as PostgreSQL stores it, `None` for an unnamed argument.
    name: Option<String>,
    mode: ArgMode,
    data_type: OperandType<'db>,
}

/// What every PL/pgSQL scope of a function's body reads about the function,
/// its arguments, its result and the path its body compiles under.
#[derive(Debug)]
pub(crate) struct BodyNames<'db, DB: DatabaseLike> {
    function: &'db DB::Function,
    database: &'db DB,
    /// The arguments, then the `RETURNS TABLE` columns as output ones.
    parameters: Vec<Parameter<'db>>,
    /// The type `$0` takes.
    returned: OperandType<'db>,
    kind: FunctionKind,
    /// The path PostgreSQL compiles the body under, which `%TYPE` and
    /// `%ROWTYPE` resolve on.
    declaration_path: BodyPath<'db>,
}

impl<'db, DB: DatabaseLike> BodyNames<'db, DB> {
    /// The names of the body of `function`.
    ///
    /// # Errors
    ///
    /// Returns the errors reading the function's own `search_path` setting.
    pub(crate) fn new(function: &'db DB::Function, database: &'db DB) -> Result<Self, LookupError> {
        // `RETURNS TABLE` columns follow the arguments as output parameters.
        let returned_columns = function
            .returned_columns(database)
            .map(|(name, type_name)| ((ArgMode::Out, Some(name)), type_name));
        let parameters: Vec<_> = function
            .argument_modes()
            .zip(function.argument_names(database))
            .zip(function.argument_type_names(database))
            .chain(returned_columns)
            .map(|((mode, name), type_name)| {
                Parameter {
                    name: name.map(|name| {
                        stored_identifier(name.name(), name.name_is_quoted()).into_owned()
                    }),
                    mode,
                    data_type: declared_type_named(database, &type_name),
                }
            })
            .collect();
        let returned = function.return_type_name(database);
        let returned_type = returned.as_deref().and_then(PgType::parse);
        let outputs = parameters
            .iter()
            .any(|parameter| matches!(parameter.mode, ArgMode::Out | ArgMode::InOut));
        let kind = match &returned_type {
            Some(PgType::Catalog("trigger")) => FunctionKind::Trigger,
            Some(PgType::Catalog("event_trigger")) => FunctionKind::EventTrigger,
            Some(pg_type) if pg_type.polymorphic().is_some() && !outputs => {
                FunctionKind::PolymorphicResult
            }
            _ => FunctionKind::Plain,
        };
        let returned = returned
            .as_deref()
            .map_or(OperandType::Unknown, |returned| declared_type_named(database, returned));
        Ok(Self {
            function,
            database,
            parameters,
            returned,
            kind,
            declaration_path: run_time_path(function, database)?,
        })
    }

    /// Returns the function whose body the names belong to.
    pub(crate) const fn function(&self) -> &'db DB::Function {
        self.function
    }

    /// Returns the database the function belongs to.
    pub(crate) const fn database(&self) -> &'db DB {
        self.database
    }

    /// What the body learns about the function from its definition.
    pub(crate) fn signature(&self) -> Signature {
        Signature {
            name: stored_identifier(self.function.name(), self.function.name_is_quoted())
                .into_owned(),
            parameters: self.parameters.iter().map(|parameter| parameter.name.clone()).collect(),
            kind: self.kind,
        }
    }

    /// The argument `$position` names.
    fn parameter(&self, position: usize) -> Option<&Parameter<'db>> {
        self.parameters.get(position.checked_sub(1)?)
    }

    /// The variable `name` reaches, with its declared name and type.
    pub(crate) fn variable(&self, name: &Name) -> PlPgSqlVariable<'db> {
        let (declared, kind) = match name.kind() {
            NameKind::Parameter { position } => {
                let parameter = self.parameter(*position);
                let declared = parameter
                    .and_then(|parameter| parameter.name.clone())
                    .unwrap_or_else(|| format!("${position}"));
                let mode = parameter.map_or(ArgMode::In, |parameter| parameter.mode.clone());
                (declared, VariableKind::Parameter { position: *position, mode })
            }
            NameKind::ReturnValue => ("$0".into(), VariableKind::ReturnValue),
            NameKind::Trigger(variable) => (variable.name().into(), VariableKind::Trigger),
            NameKind::Found => ("found".into(), VariableKind::Found),
            NameKind::Declared { declaration, .. } => {
                let kind = match &declaration.kind {
                    DeclarationKind::Variable(variable) => {
                        VariableKind::Declared {
                            constant: variable.constant,
                            not_null: variable.not_null,
                        }
                    }
                    DeclarationKind::Cursor(_) | DeclarationKind::Alias(_) => VariableKind::Cursor,
                };
                (declaration.name.clone(), kind)
            }
            NameKind::CursorParameter { parameter, .. } => {
                (parameter.name.clone(), VariableKind::CursorParameter)
            }
            NameKind::LoopIndex { variable } => (variable.clone(), VariableKind::LoopIndex),
            NameKind::LoopRecord { variable, .. } => (variable.clone(), VariableKind::LoopRecord),
            NameKind::SqlState => ("sqlstate".into(), VariableKind::Handler),
            NameKind::SqlErrm => ("sqlerrm".into(), VariableKind::Handler),
        };
        PlPgSqlVariable::new(name.id(), declared, kind, self.data_type(name))
    }

    /// Whether the variable `name` reaches holds a row, a `record` or a
    /// composite type, which PL/pgSQL reads a qualified name naming it by as
    /// a field. A type that did not resolve counts as one, so the qualified
    /// name stays on the variable with its type unknown.
    pub(crate) fn holds_row(&self, name: &Name) -> bool {
        let Ok(OperandType::Known { type_name, .. }) = self.data_type(name) else { return true };
        let record = type_name.name() == "record"
            && type_name.schema().is_some_and(|schema| schema == PG_CATALOG);
        record
            || self
                .database
                .type_facts(type_name)
                .is_some_and(|facts| facts.kind() == Some(TypeKind::Composite))
    }

    /// The type the variable `name` reaches is declared with.
    fn data_type(&self, name: &Name) -> Result<OperandType<'db>, LookupError> {
        let named = |type_name: &str| declared_type_named(self.database, type_name);
        Ok(match name.kind() {
            NameKind::Parameter { position } => {
                self.parameter(*position)
                    .map_or(OperandType::Unknown, |parameter| parameter.data_type.clone())
            }
            NameKind::ReturnValue => self.returned.clone(),
            NameKind::Trigger(variable) => named(trigger_type(*variable)),
            NameKind::Found => named("boolean"),
            NameKind::Declared { declaration, type_of } => {
                match &declaration.kind {
                    DeclarationKind::Variable(variable) => {
                        self.declared_type(&variable.data_type, type_of.as_deref())?
                    }
                    DeclarationKind::Cursor(_) | DeclarationKind::Alias(_) => named("refcursor"),
                }
            }
            NameKind::CursorParameter { parameter, type_of } => {
                self.declared_type(&parameter.data_type, type_of.as_deref())?
            }
            NameKind::LoopIndex { .. } => named("integer"),
            NameKind::LoopRecord { .. } => named("record"),
            NameKind::SqlState | NameKind::SqlErrm => named("text"),
        })
    }

    /// The type `data_type` declares. A `%TYPE` takes the type of the
    /// variable `type_of` it names, or of the column it names under the path
    /// the body compiles with, and a `%ROWTYPE` the row type of the relation
    /// it names.
    fn declared_type(
        &self,
        data_type: &DeclaredType,
        type_of: Option<&Name>,
    ) -> Result<OperandType<'db>, LookupError> {
        match data_type {
            DeclaredType::Data(data_type) => Ok(declared_type(self.database, data_type)),
            DeclaredType::ColumnType { of, array } => {
                let element = match type_of {
                    Some(variable) => self.data_type(variable)?,
                    None => self.column_type(of)?,
                };
                Ok(if *array { array_of(self.database, &element) } else { element })
            }
            DeclaredType::RowType { of, array } => {
                let element = self.row_type(of)?;
                Ok(if *array { array_of(self.database, &element) } else { element })
            }
        }
    }

    /// The relation `of` names under the path the body compiles with.
    fn relation(&self, of: &ObjectName) -> Result<Relation<'db, DB>, LookupError> {
        resolve_on_body_path(of, self.database, &self.declaration_path)?.ok_or_else(|| {
            LookupError::RelationNotFound {
                object_name: crate::impls::SqlparserDialect::for_error_names(
                    self.database.dialect(),
                )
                .stored_object_name(of),
            }
        })
    }

    /// The type of the column `relation.column` names.
    fn column_type(&self, of: &ObjectName) -> Result<OperandType<'db>, LookupError> {
        let Some((column, relation)) =
            of.0.split_last().filter(|(_, relation)| !relation.is_empty())
        else {
            return Err(unresolvable(self.function.name(), &format_args!("{of}%TYPE")));
        };
        let dialect = if crate::traits::DialectLike::sqlparser_dialect(self.database.dialect())
            .is::<sqlparser::dialect::PostgreSqlDialect>()
        {
            crate::impls::SqlparserDialect::PostgreSql
        } else {
            crate::impls::SqlparserDialect::Generic
        };
        let relation = ObjectName(relation.to_vec());
        let (Relation::Table(table) | Relation::ForeignTable(table)) = self.relation(&relation)?
        else {
            // A view's columns take the types of its query's outputs.
            return Ok(OperandType::Unknown);
        };
        let column =
            table.column(&column.to_string(), self.database, self.database.identifier_case())?;
        let column = column.ok_or_else(|| {
            LookupError::ColumnNotFound {
                table_name: dialect.stored_object_name(&relation),
                column_name: dialect.stored_object_name_last(of),
            }
        })?;
        Ok(column_type(self.database, column.data_type(self.database)))
    }

    /// The row type of the relation `of` names, named as the relation.
    fn row_type(&self, of: &ObjectName) -> Result<OperandType<'db>, LookupError> {
        let (name, quoted, schema) = match self.relation(of)? {
            Relation::Table(table) | Relation::ForeignTable(table) => {
                (
                    table.table_name(),
                    table.table_name_is_quoted(),
                    table.table_schema().map(|schema| (schema, table.table_schema_is_quoted())),
                )
            }
            // Only a table's row type is read here.
            Relation::View(_) | Relation::MaterializedView(_) | Relation::Sequence(_) => {
                return Ok(OperandType::Unknown);
            }
        };
        let mut type_name = TargetName::new(name, quoted);
        if let Some((schema, quoted)) = schema {
            type_name = type_name.with_schema(schema, quoted);
        }
        Ok(OperandType::Known { type_name, typmod: None })
    }
}

/// The type PostgreSQL 18 declares `variable` with.
const fn trigger_type(variable: TriggerVariable) -> &'static str {
    match variable {
        TriggerVariable::New | TriggerVariable::Old => "record",
        TriggerVariable::TgName
        | TriggerVariable::TgRelname
        | TriggerVariable::TgTableName
        | TriggerVariable::TgTableSchema => "name",
        TriggerVariable::TgWhen
        | TriggerVariable::TgLevel
        | TriggerVariable::TgOp
        | TriggerVariable::TgEvent
        | TriggerVariable::TgTag => "text",
        TriggerVariable::TgRelid => "oid",
        TriggerVariable::TgNargs => "integer",
        TriggerVariable::TgArgv => "text[]",
    }
}

/// Every statement and expression the PL/pgSQL body `block` of the function
/// `names` belongs to runs, in body order, each with the names it reaches
/// and the path it runs under. A `SET` or `RESET` of `search_path`, which
/// reads no name, is left out.
///
/// # Errors
///
/// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
/// naming no variable and for an `OPEN` naming no cursor declared with a
/// query, and the errors reading the path a statement of the body sets.
fn scoped_sites<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    flow: &Arc<SiteFlow<'b, 'db, DB>>,
) -> Result<Vec<(Sql<'b>, PlPgSqlScope<'b, 'db, DB>)>, LookupError> {
    let (sites, mut paths) = body_sql(names, block, hex_strings_may_be_integers)?;
    Ok(sites
        .into_iter()
        .filter_map(|site| {
            let key = SiteKey::of(site.sql);
            // Only a statement setting the path goes unrecorded.
            let path = paths.remove(&key)?;
            let point = FlowPoint::Site { flow: Arc::<SiteFlow<'b, 'db, DB>>::clone(flow), key };
            Some((site.sql, PlPgSqlScope::new(names, site.namespace, path).at(point)))
        })
        .collect())
}

/// Every statement and expression the PL/pgSQL body `block` of the function
/// `names` belongs to runs, as [`FunctionLike::plpgsql_sites`] describes.
///
/// # Errors
///
/// Returns the errors of [`scoped_sites`].
fn plpgsql_sql_sites<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    flow: &Arc<SiteFlow<'b, 'db, DB>>,
) -> Result<Vec<PlPgSqlSite<'b, 'db, DB>>, LookupError> {
    let database = names.database;
    let mut found = Vec::new();
    for (sql, scope) in scoped_sites(names, block, hex_strings_may_be_integers, flow)? {
        match sql {
            Sql::Statement(statement) => {
                let sql = EmbeddedSql::Statement(statement);
                found.push(PlPgSqlSite::new(sql, scope, Arc::clone(flow), database));
            }
            Sql::Expression(query) => {
                let sql = EmbeddedSql::Expression(query);
                found.push(PlPgSqlSite::new(sql, scope, Arc::clone(flow), database));
            }
            Sql::Dynamic(dynamic) => {
                for query in iter::once(&*dynamic.command).chain(&dynamic.arguments) {
                    let sql = EmbeddedSql::Expression(query);
                    found.push(PlPgSqlSite::new(sql, scope.clone(), Arc::clone(flow), database));
                }
            }
        }
    }
    Ok(found)
}

/// Every place the PL/pgSQL body `block` of the function `names` belongs to
/// runs SQL text, in the order the body evaluates it. The calls a statement
/// or expression writes come in the order [`call_nodes`] meets them, and an
/// `EXECUTE`-family site follows the calls its command and its `USING`
/// arguments write, which run before it.
///
/// # Errors
///
/// Returns the errors of [`scoped_sites`] and of [`text_sink_calls`].
fn plpgsql_dynamic_sites<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    flow: &Arc<SiteFlow<'b, 'db, DB>>,
) -> Result<Vec<DynamicExecution<'b, 'db, DB>>, LookupError> {
    let database = names.database;
    let mut found = Vec::new();
    for (sql, scope) in scoped_sites(names, block, hex_strings_may_be_integers, flow)? {
        let (first, arguments): (_, &[Query]) = match sql {
            Sql::Statement(statement) => (ScopedRoot::Statement(statement), &[]),
            Sql::Expression(query) => (ScopedRoot::Query(query), &[]),
            Sql::Dynamic(dynamic) => (ScopedRoot::Query(&dynamic.command), &dynamic.arguments),
        };
        for root in iter::once(first).chain(arguments.iter().map(ScopedRoot::Query)) {
            for (command, called) in
                text_sink_calls(root, &ExpressionScope::PlPgSql(&scope), database)?
            {
                let holder = SiteScope::PlPgSql { scope: scope.clone(), flow: Arc::clone(flow) };
                found.push(DynamicExecution::call(root, command, holder, called, database));
            }
        }
        if let Sql::Dynamic(dynamic) = sql {
            let arguments = dynamic.arguments.iter().map(expression).collect();
            found.push(DynamicExecution::execute(
                expression(&dynamic.command),
                arguments,
                SiteScope::PlPgSql { scope, flow: Arc::clone(flow) },
                database,
            ));
        }
    }
    Ok(found)
}

/// Every statement and expression the PL/pgSQL body `block` of the function
/// `names` belongs to runs, in body order with the names each reaches, and
/// the path each runs under, a statement setting the path left out.
///
/// # Errors
///
/// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
/// naming no variable and for an `OPEN` naming no cursor declared with a
/// query, and the errors reading the path a statement of the body sets.
pub(crate) fn body_sql<'b, DB: DatabaseLike>(
    names: &BodyNames<'_, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
) -> Result<(Vec<Site<'b>>, BTreeMap<SiteKey, BodyPath<'static>>), LookupError> {
    let function = names.function.name();
    let sites = block
        .sites(&names.signature(), |name| names.holds_row(name))
        .map_err(|error| namespace_error(function, error))?;
    let mut collector = PathCollector {
        function,
        database: names.database,
        path: owned_path(names.declaration_path.clone()),
        hex_strings_may_be_integers,
        constant: false,
        paths: BTreeMap::new(),
    };
    walk_body(function, RunTimeBody::PlPgSql(block), &mut collector, hex_strings_may_be_integers)?;
    Ok((sites, collector.paths))
}

/// The error the body of `function` answers when its names do not resolve.
pub(crate) fn namespace_error(function: &str, error: namespace::Error) -> LookupError {
    match error {
        namespace::Error::UnknownAliasTarget { alias, target } => {
            unresolvable(function, &format_args!("{alias} ALIAS FOR {target}"))
        }
        namespace::Error::UnboundCursor { name } => {
            unresolvable(function, &format_args!("OPEN {name}"))
        }
    }
}

/// The body of a function, parsed, with what reading its sites needs.
#[derive(Debug)]
pub(crate) struct SiteSource<'db, DB: DatabaseLike> {
    names: BodyNames<'db, DB>,
    body: SqlBody<'db>,
    hex_strings_may_be_integers: bool,
}

impl<'db, DB: DatabaseLike> SiteSource<'db, DB> {
    pub(crate) const fn new(
        names: BodyNames<'db, DB>,
        body: SqlBody<'db>,
        hex_strings_may_be_integers: bool,
    ) -> Self {
        Self { names, body, hex_strings_may_be_integers }
    }

    /// Every dynamic-execution site of the body, in body order.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`plpgsql_dynamic_sites`] for a PL/pgSQL body,
    /// and the lookup errors resolving a call [`text_sink_calls`] reads.
    pub(crate) fn dynamic_sites(&self) -> Result<Vec<DynamicExecution<'_, 'db, DB>>, LookupError> {
        match &self.body {
            SqlBody::PlPgSql(block) => {
                let flow =
                    Arc::new(SiteFlow::new(&self.names, block, self.hex_strings_may_be_integers));
                plpgsql_dynamic_sites(&self.names, block, self.hex_strings_may_be_integers, &flow)
            }
            SqlBody::Statements(statements) | SqlBody::Atomic(statements) => {
                self.sql_sites(statements.iter().map(ScopedRoot::Statement))
            }
            SqlBody::Return(expression) => {
                self.sql_sites(iter::once(ScopedRoot::Expression(expression)))
            }
            // Another dialect's `AS (expression)` or `AS RETURN SELECT`,
            // where no `pg_catalog` function resolves.
            SqlBody::Expression(_) | SqlBody::Select(_) => Ok(Vec::new()),
        }
    }

    /// Every statement and expression of a PL/pgSQL body, in body order, and
    /// none for a body in another language.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`scoped_sites`].
    pub(crate) fn plpgsql_sites(&self) -> Result<Vec<PlPgSqlSite<'_, 'db, DB>>, LookupError> {
        match &self.body {
            SqlBody::PlPgSql(block) => {
                let flow =
                    Arc::new(SiteFlow::new(&self.names, block, self.hex_strings_may_be_integers));
                plpgsql_sql_sites(&self.names, block, self.hex_strings_may_be_integers, &flow)
            }
            SqlBody::Statements(_)
            | SqlBody::Atomic(_)
            | SqlBody::Return(_)
            | SqlBody::Expression(_)
            | SqlBody::Select(_) => Ok(Vec::new()),
        }
    }

    /// The sites of the SQL body whose statements or expression are
    /// `roots`, each call typed with the function's input parameters in
    /// reach.
    fn sql_sites<'b>(
        &'b self,
        roots: impl Iterator<Item = ScopedRoot<'b>>,
    ) -> Result<Vec<DynamicExecution<'b, 'db, DB>>, LookupError> {
        let function = self.names.function;
        let database = self.names.database;
        let mut sites = Vec::new();
        for root in roots {
            let scope = ExpressionScope::FunctionBody(function);
            for (command, called) in text_sink_calls(root, &scope, database)? {
                let scope = SiteScope::FunctionBody(function);
                sites.push(DynamicExecution::call(root, command, scope, called, database));
            }
        }
        Ok(sites)
    }
}

/// Every call `root` writes to a `pg_catalog` function running SQL text, in
/// the order [`call_nodes`] meets them, as the argument giving the text and
/// the function called. A call resolves under `scope` as PostgreSQL 18
/// selects its function, so a user function of the same name that takes
/// its arguments better or that the path places first is no such call.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving `root` runs into and
/// the lookup errors typing the arguments of a call written with the name
/// of one of [`TEXT_SINKS`].
fn text_sink_calls<'b, 'db, DB: DatabaseLike>(
    root: ScopedRoot<'b>,
    scope: &ExpressionScope<'_, 'db, DB>,
    database: &'db DB,
) -> Result<Vec<(&'b Expr, &'db DB::Function)>, LookupError> {
    let candidates: Vec<_> =
        call_nodes(root).into_iter().filter(|node| names_text_sink(node.name())).collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let context = ExpressionContext::for_root(root, scope, database, IdentifierCase::Folded)?;
    let mut calls = Vec::new();
    for node in candidates {
        let call = match node {
            CallNode::Expression { call, .. } => context.call(call)?,
            CallNode::Relation { factor, .. } => context.relation_call(factor)?,
        };
        let Some(call) = call else { continue };
        let CallResolution::Selected(selected) = call.resolution() else { continue };
        let called = selected.function();
        let Some(text) = text_parameter(called) else { continue };
        let command = selected
            .arguments()
            .iter()
            .zip(call.arguments())
            .find(|(argument, _)| argument.parameter() == text)
            .map(|(_, &command)| (command, called));
        calls.extend(command);
    }
    Ok(calls)
}

/// Whether `name` is written as the name of one of [`TEXT_SINKS`].
fn names_text_sink(name: &ObjectName) -> bool {
    object_name_last_part(name).is_some_and(|(called, quoted)| {
        TEXT_SINKS.iter().any(|sink| identifiers_match(sink.name, false, called, quoted))
    })
}

/// The input parameter, counted from 0, through which `function` takes SQL
/// text it runs, or `None` for any function but one of [`TEXT_SINKS`].
fn text_parameter<F: FunctionLike>(function: &F) -> Option<usize> {
    let declared = function.target_name();
    let in_catalog = declared.schema().is_some_and(|schema| {
        identifiers_match(schema, declared.schema_is_quoted(), PG_CATALOG, false)
    });
    if !in_catalog {
        return None;
    }
    let inputs = function.argument_modes().filter(is_input).count();
    TEXT_SINKS
        .iter()
        .find(|sink| sink.inputs == inputs && function.name() == sink.name)
        .map(|sink| sink.text)
}

/// `path` owning its schema names, so it outlives the statements it was
/// read from.
fn owned_path(path: BodyPath<'_>) -> BodyPath<'static> {
    match path {
        BodyPath::Database => BodyPath::Database,
        BodyPath::Set(path) => {
            BodyPath::Set(
                path.into_iter()
                    .map(|(schema, quoted)| (Cow::Owned(schema.into_owned()), quoted))
                    .collect(),
            )
        }
        BodyPath::Unknown(error) => BodyPath::Unknown(error),
    }
}

/// The SQL of a site of a PL/pgSQL body, by address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SiteKey {
    Statement(usize),
    Expression(usize),
    Dynamic(usize),
}

impl SiteKey {
    pub(crate) fn of(sql: Sql<'_>) -> Self {
        match sql {
            Sql::Statement(statement) => Self::Statement(core::ptr::from_ref(statement).addr()),
            Sql::Expression(query) => Self::Expression(core::ptr::from_ref(query).addr()),
            Sql::Dynamic(dynamic) => Self::Dynamic(core::ptr::from_ref(dynamic).addr()),
        }
    }
}

/// Gathers the path each statement and expression of one PL/pgSQL body runs
/// under.
struct PathCollector<'f, DB> {
    function: &'f str,
    database: &'f DB,
    /// The path in force where the walk stands.
    path: BodyPath<'static>,
    hex_strings_may_be_integers: bool,
    /// Whether the walk reads the statements a constant command runs, which
    /// are no site of the body.
    constant: bool,
    /// The path each statement and expression walked runs under.
    paths: BTreeMap<SiteKey, BodyPath<'static>>,
}

impl<DB: DatabaseLike> PathCollector<'_, DB> {
    /// Reads the path the constant command `statements` leaves behind, as
    /// the body statements they are would.
    fn constant_command(&mut self, statements: &[Statement]) {
        let function = self.function;
        self.constant = true;
        for statement in statements {
            if let Err(error) =
                walk_statement(function, statement, self, self.hex_strings_may_be_integers)
            {
                self.path = BodyPath::Unknown(error);
            }
        }
        self.constant = false;
    }

    /// Records the path in force for the site `key`.
    fn record(&mut self, key: SiteKey) {
        if !self.constant {
            self.paths.insert(key, self.path.clone());
        }
    }

    /// Makes the path unknown after `sql`, which may change it.
    fn forget(&mut self, sql: &dyn core::fmt::Display) {
        self.path = BodyPath::Unknown(unresolvable(self.function, &format_args!("{sql}")));
    }
}

impl<'s, DB: DatabaseLike> BodyWalk<'s> for PathCollector<'_, DB> {
    type SavedPath = BodyPath<'static>;

    fn save_path(&self) -> Self::SavedPath {
        self.path.clone()
    }

    fn restore_path(&mut self, saved: Self::SavedPath) {
        self.path = saved;
    }

    fn set_path(&mut self, path: BodyPath<'s>) {
        self.path = owned_path(path);
    }

    fn walk(&mut self, sql: EmbeddedSql<'_>) -> Result<(), LookupError> {
        self.record(SiteKey::of(match sql {
            EmbeddedSql::Statement(statement) => Sql::Statement(statement),
            EmbeddedSql::Expression(query) => Sql::Expression(query),
        }));
        Ok(())
    }

    fn calls_path_changer(&self, sql: EmbeddedSql<'_>) -> bool {
        calls_path_changer(&sql, self.database)
    }

    /// Code outside the body may change the path, which later names then
    /// cannot rely on.
    fn opaque(&mut self, _function: &str, statement: &'s Statement) -> Result<(), LookupError> {
        self.record(SiteKey::of(Sql::Statement(statement)));
        self.forget(statement);
        Ok(())
    }

    fn dynamic(&mut self, _function: &str, dynamic: &'s Dynamic) -> Result<(), LookupError> {
        self.record(SiteKey::of(Sql::Dynamic(dynamic)));
        match constant_command(dynamic) {
            Some(statements) => self.constant_command(&statements),
            None => self.forget(&EmbeddedSql::Expression(&dynamic.command)),
        }
        let calling = core::iter::once(&*dynamic.command)
            .chain(&dynamic.arguments)
            .find(|query| self.calls_path_changer(EmbeddedSql::Expression(query)));
        if let Some(query) = calling {
            self.forget(query);
        }
        Ok(())
    }
}
