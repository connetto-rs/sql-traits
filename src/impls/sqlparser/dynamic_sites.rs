//! The places a function body runs SQL text, each with the names its
//! expressions reach and the path it runs under.

use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{iter, ops::ControlFlow, ptr};

use sqlparser::ast::{
    ArgMode, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName, Query, Statement,
    Value,
};
use sqlparser_plpgsql::{
    embedded::{Block, DeclarationKind, DeclaredType, Dynamic, Loop, Node, NodeKind, expression},
    namespace::{self, FunctionKind, Name, NameKind, Signature, Site, Sql, TriggerVariable},
};
use sqlparser_plpgsql_dataflow::{Escape, Return, Step};

use super::{
    callee_path::{call_outside_catalog, calls_path_changer},
    data_statement::{
        BodyPath, BodyWalk, RunTimeBody, resolve_on_body_path, unresolvable, walk_body,
        walk_statement,
    },
    dql::{
        call_nodes::{CallNode, call_nodes},
        scoped_roots::{ScopedRoot, is_input},
    },
    function_body::{SqlBody, run_time_path},
    outside_code::{OutsideCode, foreign_cursor, relation_read, scan_in},
    plpgsql_body::constant_command,
    site_roles::site_roles,
    value_flow::value_analysis,
};
use crate::{
    errors::{LookupError, RaiseSite, ReturnedValuesError},
    structs::{
        BodyTruth, CallResolution, Definition, DefinitionSource, Deployment, DynamicExecution,
        EmbeddedSql, ExpressionContext, ExpressionScope, IdentifierCase, OperandType, PlPgSqlLoop,
        PlPgSqlScope, PlPgSqlSite, PlPgSqlSiteRole, PlPgSqlVariable, ReturnedOutput, ReturnedValue,
        SessionSetting, SiteFlow, SiteScope, TargetName, TypeKind, VariableKind, array_of,
        column_type, declared_type, declared_type_named,
        plpgsql_flow::{FlowPoint, FlowSource},
    },
    traits::{
        ColumnLike, DataTypeLike, DatabaseLike, DomainLike, FunctionLike, Relation, TableLike,
    },
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        object_name::{PG_CATALOG, object_name_last_part, type_search_path},
        postgres_types::{PgType, names_catalog_type},
        walk::walk_expressions,
    },
};

/// The schema a domain created without one lives in.
const DEFAULT_SCHEMA: &str = "public";

/// The domain of `database` the type name `target` names in the schema
/// `schema` names, whatever schema `target` itself writes.
pub(crate) fn schema_domain<'db, DB: DatabaseLike>(
    database: &'db DB,
    target: &TargetName<'_>,
    (schema, quoted): (&str, bool),
) -> Option<&'db DB::Domain> {
    let case = database.identifier_case();
    database.domains().find(|domain| {
        case.identifiers_match(
            domain.name(),
            domain.name_is_quoted(),
            target.name(),
            target.name_is_quoted(),
        ) && case.identifiers_match(
            domain.schema().unwrap_or(DEFAULT_SCHEMA),
            domain.schema_is_quoted(),
            schema,
            quoted,
        )
    })
}

/// What a type name reaches on the path a body compiles under.
pub(crate) enum TypeReach<'db, DB: DatabaseLike> {
    /// A domain of the database.
    Domain(&'db DB::Domain),
    /// A type that is no domain of the database.
    Other,
    /// No type, which PostgreSQL refuses as it plans the statement naming it.
    Missing,
}

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
            .map(|(name, data_type)| ((ArgMode::Out, Some(name)), data_type.name(database)));
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

    /// The function's `OUT` and `INOUT` parameters and `RETURNS TABLE`
    /// columns, in declaration order, each as the variable the body reaches
    /// it by.
    pub(crate) fn outputs(&self) -> Vec<PlPgSqlVariable<'db>> {
        // The parameters come first among the variables of the body, so
        // parameter `$n` has id `n - 1`.
        self.parameters
            .iter()
            .enumerate()
            .filter(|(_, parameter)| matches!(parameter.mode, ArgMode::Out | ArgMode::InOut))
            .map(|(index, parameter)| {
                let position = index + 1;
                let name = parameter.name.clone().unwrap_or_else(|| format!("${position}"));
                let kind = VariableKind::Parameter { position, mode: parameter.mode.clone() };
                PlPgSqlVariable::new(index, name, kind, Ok(parameter.data_type.clone()))
            })
            .collect()
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

    /// The domain the type `target` names, as [`Self::type_reached`] finds
    /// it.
    ///
    /// # Errors
    ///
    /// Returns the error an earlier statement changing the path leaves for
    /// an unqualified name.
    pub(crate) fn domain_named(
        &self,
        target: &TargetName<'_>,
    ) -> Result<Option<&'db DB::Domain>, LookupError> {
        Ok(match self.type_reached(target)? {
            TypeReach::Domain(domain) => Some(domain),
            TypeReach::Other | TypeReach::Missing => None,
        })
    }

    /// What the type `target` names, an unqualified name found on the path
    /// PostgreSQL compiles the body under, where the first schema holding a
    /// type of that name decides.
    ///
    /// # Errors
    ///
    /// Returns the error an earlier statement changing the path leaves for
    /// an unqualified name.
    pub(crate) fn type_reached(
        &self,
        target: &TargetName<'_>,
    ) -> Result<TypeReach<'db, DB>, LookupError> {
        let database = self.database;
        let in_schema = |schema: &str, quoted: bool| {
            if let Some(domain) = schema_domain(database, target, (schema, quoted)) {
                return Some(TypeReach::Domain(domain));
            }
            let catalog = identifiers_match(schema, quoted, PG_CATALOG, false)
                && names_catalog_type(&stored_identifier(target.name(), target.name_is_quoted()));
            let qualified =
                TargetName::new(target.name(), target.name_is_quoted()).with_schema(schema, quoted);
            (catalog || database.type_facts(qualified).is_some()).then_some(TypeReach::Other)
        };
        if let Some(schema) = target.schema() {
            return Ok(in_schema(schema, target.schema_is_quoted()).unwrap_or(TypeReach::Missing));
        }
        let first = |path: &mut dyn Iterator<Item = (&str, bool)>| {
            for (schema, quoted) in path {
                if let Some(reach) = in_schema(schema, quoted) {
                    return reach;
                }
            }
            TypeReach::Missing
        };
        Ok(match &self.declaration_path {
            BodyPath::Set(path) => {
                first(&mut type_search_path(|| {
                    path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted))
                }))
            }
            BodyPath::Database => first(&mut type_search_path(|| database.search_path())),
            BodyPath::Unknown(error) => return Err(error.clone()),
        })
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
                site: crate::errors::ColumnLookupSite::Relation,
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
            let point =
                FlowPoint::Site { flow: Arc::<SiteFlow<'b, 'db, DB>>::clone(flow), sql: site.sql };
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
pub(crate) fn plpgsql_sql_sites<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    flow: &Arc<SiteFlow<'b, 'db, DB>>,
) -> Result<Vec<PlPgSqlSite<'b, 'db, DB>>, LookupError> {
    let database = names.database;
    let roles = site_roles(block);
    let mut found = Vec::new();
    for (at, scope) in scoped_sites(names, block, hex_strings_may_be_integers, flow)? {
        let site =
            |sql, role, scope| PlPgSqlSite::new(sql, at, role, scope, Arc::clone(flow), database);
        match at {
            Sql::Statement(statement) => {
                let role = (PlPgSqlSiteRole::Statement, None);
                found.push(site(EmbeddedSql::Statement(statement), role, scope));
            }
            Sql::Expression(query) => {
                let role = roles
                    .get(&core::ptr::from_ref(query).addr())
                    .copied()
                    .unwrap_or((PlPgSqlSiteRole::Expression, None));
                found.push(site(EmbeddedSql::Expression(query), role, scope));
            }
            Sql::Dynamic(dynamic) => {
                for query in iter::once(&*dynamic.command).chain(&dynamic.arguments) {
                    let role = (PlPgSqlSiteRole::Expression, None);
                    found.push(site(EmbeddedSql::Expression(query), role, scope.clone()));
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

    /// The parsed PL/pgSQL block, `None` for a body in another language.
    pub(crate) const fn block(&self) -> Option<&Block> {
        match &self.body {
            SqlBody::PlPgSql(block) => Some(block),
            _ => None,
        }
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

    /// Every loop of a PL/pgSQL body, in body order, with whether control
    /// leaves it under `case`, and none for a body in another language.
    ///
    /// # Errors
    ///
    /// Returns, for a body holding a loop, the errors of
    /// [`SiteFlow::analysis`].
    pub(crate) fn plpgsql_loops(
        &self,
        case: IdentifierCase,
    ) -> Result<Vec<PlPgSqlLoop<'_>>, LookupError> {
        let Some(block) = self.block() else { return Ok(Vec::new()) };
        let loops = block_loops(block);
        if loops.is_empty() {
            return Ok(Vec::new());
        }
        let flow = Arc::new(SiteFlow::new(&self.names, block, self.hex_strings_may_be_integers));
        let analysis = flow.analysis(case)?.borrow();
        Ok(loops
            .into_iter()
            .map(|(_, repeated)| PlPgSqlLoop::new(repeated, analysis.ends(repeated)))
            .collect())
    }

    /// What each output parameter of the PL/pgSQL body holds when it
    /// returns, on every request `deployment` admits, as
    /// [`FunctionLike::returned_values`] describes.
    ///
    /// The body is read once for each kind of request, each group of
    /// settings set together that the body reads either set or unset as a
    /// whole.
    ///
    /// # Errors
    ///
    /// Returns the errors [`FunctionLike::returned_values`] documents.
    pub(crate) fn returned_values(
        &self,
        deployment: &Deployment,
        case: IdentifierCase,
    ) -> Result<Vec<ReturnedValue>, ReturnedValuesError> {
        let routine = self.names.function();
        let function = || routine.stored_name().into_owned();
        let Some(block) = self.block() else {
            let language = routine.stored_language().map(Cow::into_owned);
            return Err(ReturnedValuesError::Language { function: function(), language });
        };
        let outputs = self.names.outputs();
        if outputs.is_empty() {
            return Err(ReturnedValuesError::NoOutputParameters { function: function() });
        }
        let (names, hex) = (&self.names, self.hex_strings_may_be_integers);
        let flow = Arc::new(SiteFlow::new(names, block, hex));
        let sites = plpgsql_sql_sites(names, block, hex, &flow)?;
        let mut nodes = Vec::new();
        block_nodes(block, &mut nodes);
        let dynamic = plpgsql_dynamic_sites(names, block, hex, &flow)?;
        let mut blocks = Vec::from([block]);
        blocks.extend(nodes.iter().filter_map(|node| {
            match &node.kind {
                NodeKind::Block(nested) => Some(nested),
                _ => None,
            }
        }));
        let body = Body { function: routine, blocks: &blocks, nodes: &nodes };
        if let Some(code) = code_outside(&body, &sites, &dynamic, names.database()) {
            return Err(ReturnedValuesError::RunsCode { function: function(), code });
        }
        let settings = BodySettings::of(routine, &nodes, &sites);
        let mut returned: Vec<Return<'_>> = Vec::new();
        // The escape refusing the function, one that surely raises named
        // before one that only may, on whichever request reaches it.
        let mut refusal: Option<(Step<'_>, bool)> = None;
        for presence in deployment.request_presences(
            |setting| settings.read.iter().any(|name| setting.is_named(name)),
            |setting| settings.changes(setting),
        ) {
            let context = BodyTruth::new(names, &sites, case)?.with_presence(presence);
            let analysis = value_analysis(names, block, hex, context)?;
            for escape in analysis.escapes() {
                let escaped = match escape {
                    Escape::Raises(step) => (step, true),
                    Escape::MayRaise(step) => (step, false),
                    // Only a body whose end raises `2F005` meets it, and the
                    // end of a function with output parameters returns.
                    Escape::End => continue,
                };
                if refusal.is_none_or(|(_, surely)| !surely && escaped.1) {
                    refusal = Some(escaped);
                }
            }
            for point in analysis.returns() {
                if !returned.iter().any(|seen| same_return(*seen, point)) {
                    returned.push(point);
                }
            }
        }
        if let Some((step, surely)) = refusal {
            let raise = raise_site(block, step);
            return Err(ReturnedValuesError::Raises { function: function(), raise, surely });
        }
        if returned.is_empty() {
            return Err(ReturnedValuesError::NeverReturns { function: function() });
        }
        let values = flow.flow()?;
        let states: Vec<_> = values
            .flow
            .returns()
            .filter(|(point, _)| returned.iter().any(|reached| same_return(*reached, *point)))
            .map(|(_, state)| state)
            .collect();
        let mut returned_values = Vec::new();
        for variable in outputs {
            let mut definitions: Vec<Definition<'_, '_, DB>> = Vec::new();
            for state in &states {
                for definition in values.definitions_of(state, &variable) {
                    if !definitions.iter().any(|seen| seen.id() == definition.id()) {
                        definitions.push(definition);
                    }
                }
            }
            let output = match definitions.as_slice() {
                [definition] => returned_output(definition, &settings, case)?,
                _ => ReturnedOutput::Several,
            };
            returned_values.push(ReturnedValue::new(variable.name().into(), output));
        }
        Ok(returned_values)
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

/// What the one definition reaching every return gives its output
/// parameter, the body doing with settings what `settings` tells.
///
/// # Errors
///
/// Returns the errors [`Definition::reads`] returns.
fn returned_output<DB: DatabaseLike>(
    definition: &Definition<'_, '_, DB>,
    settings: &BodySettings,
    case: IdentifierCase,
) -> Result<ReturnedOutput, LookupError> {
    Ok(match definition.source() {
        DefinitionSource::Null => ReturnedOutput::Null,
        DefinitionSource::Value(value)
            if definition.part().is_empty()
                && definition.reads(case)?.is_empty()
                && !settings.reads_own(value) =>
        {
            ReturnedOutput::Expression(Box::new(value.clone().into_owned()))
        }
        _ => ReturnedOutput::Unread,
    })
}

/// The steps of a PL/pgSQL body and the function it belongs to.
struct Body<'b, DB: DatabaseLike> {
    function: &'b DB::Function,
    /// The outer block and every block nested in it.
    blocks: &'b [&'b Block],
    /// Every step, as [`block_nodes`] lists them.
    nodes: &'b [&'b Node],
}

/// The first code `body` may run that it does not hold, as written. That
/// is a call that may run a function outside `pg_catalog` or scan what the
/// caller reaches, SQL built at run time, a statement other than a query,
/// `SET` and `RESET`, which may run triggers, rules and column defaults or
/// call a procedure, a relation a query reads, whose view or policies may
/// call functions, an operator whose function lies outside `pg_catalog`, a
/// type whose domain checks call one, and a portal the caller opened. Such
/// code may raise or set a setting where the body cannot tell, even inside
/// a block whose handler catches the raise.
fn code_outside<DB: DatabaseLike>(
    body: &Body<'_, DB>,
    sites: &[PlPgSqlSite<'_, '_, DB>],
    dynamic: &[DynamicExecution<'_, '_, DB>],
    database: &DB,
) -> Option<String> {
    let statement = body.nodes.iter().find_map(|node| {
        let NodeKind::Statement { statement, .. } = &node.kind else { return None };
        match &**statement {
            Statement::Set(_) | Statement::Reset(_) => None,
            Statement::Query(query) => relation_read(&**query),
            other => Some(other.to_string()),
        }
    });
    let run_time = || {
        dynamic.first().map(|site| {
            site.function().map_or_else(
                || format!("EXECUTE {}", site.command()),
                |function| function.stored_name().into_owned(),
            )
        })
    };
    let outside = OutsideCode::of(database);
    let site_code = || {
        sites.iter().find_map(|site| {
            let sql = site.sql();
            call_outside_catalog(&sql, database)
                .or_else(|| scan_in(&sql))
                .or_else(|| relation_read(&sql))
                .or_else(|| outside.operator_in(&sql))
                .or_else(|| outside.cast_in(&sql))
        })
    };
    let declared = || outside.declared_in(body.function, body.blocks, database);
    let cursor = || foreign_cursor(body.function, body.blocks, body.nodes, database);
    statement.or_else(run_time).or_else(site_code).or_else(declared).or_else(cursor)
}

/// What a PL/pgSQL body does with settings, as its steps and its function's
/// `SET` clause write it.
struct BodySettings {
    /// The settings the sites read through `current_setting` with a literal
    /// name, each once.
    read: Vec<String>,
    /// The settings the `SET` clause names, each once.
    configured: Vec<String>,
    /// The settings a `set_config` of the body with a literal name sets,
    /// each once.
    changed: Vec<String>,
    /// Whether the body may set a setting it does not name, through `SET`,
    /// `RESET` or `set_config` with a computed name, or the `SET` clause
    /// names one this model does not spell.
    changes_any: bool,
}

impl BodySettings {
    /// What the body of `function`, whose steps are `nodes` and sites are
    /// `sites`, does with settings.
    fn of<DB: DatabaseLike>(
        function: &DB::Function,
        nodes: &[&Node],
        sites: &[PlPgSqlSite<'_, '_, DB>],
    ) -> Self {
        let mut settings = Self {
            read: Vec::new(),
            configured: Vec::new(),
            changed: Vec::new(),
            changes_any: false,
        };
        for parameter in function.configuration_parameters() {
            let parts: Option<Vec<&str>> = parameter
                .name
                .0
                .iter()
                .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
                .collect();
            match parts {
                Some(parts) => once(&mut settings.configured, &parts.join(".")),
                None => settings.changes_any = true,
            }
        }
        settings.changes_any |= nodes.iter().any(|node| {
            matches!(&node.kind, NodeKind::Statement { statement, .. }
                if matches!(**statement, Statement::Set(_) | Statement::Reset(_)))
        });
        for site in sites {
            let _ = walk_expressions(&site.sql(), |expression| {
                match setting_call(expression) {
                    Some((SettingCall::Read, Some(name))) => once(&mut settings.read, name),
                    Some((SettingCall::Set, Some(name))) => once(&mut settings.changed, name),
                    Some((SettingCall::Set, None)) => settings.changes_any = true,
                    Some((SettingCall::Read, None)) | None => {}
                }
                ControlFlow::<()>::Continue(())
            });
        }
        settings
    }

    /// Whether the body or its `SET` clause may set `setting`.
    fn changes(&self, setting: &SessionSetting) -> bool {
        self.changes_any
            || self.changed.iter().chain(&self.configured).any(|name| setting.is_named(name))
    }

    /// Whether `expression` reads a setting the body itself may set before
    /// the read, so that it reads what the body set rather than the request.
    fn reads_own(&self, expression: &Expr) -> bool {
        // A built-in parameter such as `role` or `search_path` is read by
        // `current_user`, `current_schema` and name resolution without a
        // `current_setting` call, so any expression may read it.
        if self.changes_any || self.changed.iter().any(|name| !name.contains('.')) {
            return true;
        }
        walk_expressions(expression, |expression| {
            let own = match setting_call(expression) {
                Some((SettingCall::Read, Some(name))) => {
                    self.changed.iter().any(|changed| changed.eq_ignore_ascii_case(name))
                }
                Some((SettingCall::Read, None)) => !self.changed.is_empty(),
                Some((SettingCall::Set, _)) | None => false,
            };
            if own { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        })
        .is_break()
    }
}

/// Whether a call reads or sets a setting.
#[derive(Clone, Copy)]
pub(super) enum SettingCall {
    /// `current_setting`.
    Read,
    /// `set_config`.
    Set,
}

/// Whether `expression` calls `current_setting` or `set_config`, with the
/// setting it names when the name is a literal.
pub(super) fn setting_call(expression: &Expr) -> Option<(SettingCall, Option<&str>)> {
    let Expr::Function(call) = expression else { return None };
    let (called, _) = object_name_last_part(&call.name)?;
    let kind = if called.eq_ignore_ascii_case("current_setting") {
        SettingCall::Read
    } else if called.eq_ignore_ascii_case("set_config") {
        SettingCall::Set
    } else {
        return None;
    };
    let name = match &call.args {
        FunctionArguments::List(list) => {
            match list.args.first() {
                Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value)))) => {
                    match &value.value {
                        Value::SingleQuotedString(name) => Some(name.as_str()),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };
    Some((kind, name))
}

/// Pushes `name` to `names` unless it holds it, compared without case.
fn once(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|held| held.eq_ignore_ascii_case(name)) {
        names.push(name.into());
    }
}

/// Whether `first` and `second` are the same point of one body.
fn same_return(first: Return<'_>, second: Return<'_>) -> bool {
    match (first, second) {
        (Return::Statement(first), Return::Statement(second)) => ptr::eq(first, second),
        (Return::End, Return::End) => true,
        _ => false,
    }
}

/// Where in `block` the error `step` raises is raised.
#[expect(clippy::expect_used, reason = "every loop a step iterates is one of the body's")]
fn raise_site(block: &Block, step: Step<'_>) -> RaiseSite {
    let node = match step {
        Step::Declaration(declaration) => {
            return RaiseSite::Declaration { variable: declaration.name.clone() };
        }
        Step::Node(node) => node,
        Step::Iteration(repeated) => {
            block_loops(block)
                .into_iter()
                .find_map(|(node, each)| ptr::eq(each, repeated).then_some(node))
                .expect("the body holds the loop")
        }
    };
    RaiseSite::Step { line: node.span.start.line, column: node.span.start.column }
}

/// Every loop of the statements and handlers of `block`, with the step
/// holding it, in body order, each before the loops it holds.
fn block_loops(block: &Block) -> Vec<(&Node, &Loop)> {
    let mut nodes = Vec::new();
    block_nodes(block, &mut nodes);
    nodes
        .into_iter()
        .filter_map(|node| {
            match &node.kind {
                NodeKind::Loop(repeated) => Some((node, repeated)),
                _ => None,
            }
        })
        .collect()
}

/// Pushes every step of the statements and handlers of `block` to `nodes`,
/// in body order, each before the steps it holds.
fn block_nodes<'b>(block: &'b Block, nodes: &mut Vec<&'b Node>) {
    each_node(&block.statements, nodes);
    for handler in &block.handlers {
        each_node(&handler.statements, nodes);
    }
}

/// Pushes every step of `steps` and of the statements nested in them to
/// `nodes`, in body order, each before the steps it holds.
fn each_node<'b>(steps: &'b [Node], nodes: &mut Vec<&'b Node>) {
    for node in steps {
        nodes.push(node);
        match &node.kind {
            NodeKind::Block(block) => block_nodes(block, nodes),
            NodeKind::If(conditional) => {
                for branch in &conditional.branches {
                    each_node(&branch.statements, nodes);
                }
                each_node(&conditional.otherwise, nodes);
            }
            NodeKind::Case(case) => {
                for branch in &case.branches {
                    each_node(&branch.statements, nodes);
                }
                if let Some(otherwise) = &case.otherwise {
                    each_node(otherwise, nodes);
                }
            }
            NodeKind::Loop(repeated) => each_node(&repeated.statements, nodes),
            NodeKind::Statement { .. }
            | NodeKind::Perform { .. }
            | NodeKind::Assignment { .. }
            | NodeKind::Dynamic { .. }
            | NodeKind::Fetch { .. }
            | NodeKind::Move { .. }
            | NodeKind::Diagnostics { .. }
            | NodeKind::Open { .. }
            | NodeKind::Close { .. }
            | NodeKind::Commit { .. }
            | NodeKind::Rollback { .. }
            | NodeKind::Exit { .. }
            | NodeKind::Return { .. }
            | NodeKind::ReturnNext { .. }
            | NodeKind::ReturnQuery { .. }
            | NodeKind::Raise { .. }
            | NodeKind::Assert { .. } => {}
        }
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
