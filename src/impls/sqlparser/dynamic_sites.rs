//! The places a PL/pgSQL body runs SQL it builds at run time, each with the
//! names it reaches and the path it runs under.

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{ArgMode, ObjectName, Statement};
use sqlparser_plpgsql::{
    embedded::{Block, DeclarationKind, DeclaredType, Dynamic, expression},
    namespace::{self, FunctionKind, Name, NameKind, Signature, Sql, TriggerVariable},
};

use super::{
    data_statement::{
        BodyPath, BodyWalk, Embedded, RunTimeBody, calls_user_code, resolve_on_body_path,
        unresolvable, walk_body, walk_statement,
    },
    function_body::run_time_path,
    plpgsql_body::constant_command,
};
use crate::{
    errors::LookupError,
    structs::{
        DynamicExecution, IdentifierCase, OperandType, PlPgSqlScope, PlPgSqlVariable, TargetName,
        TypeKind, VariableKind, array_of, column_type, declared_type, declared_type_named,
    },
    traits::{ColumnLike, DatabaseLike, FunctionLike, Relation, TableLike},
    utils::{
        identifier_resolution::stored_identifier, object_name::PG_CATALOG, postgres_types::PgType,
    },
};

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

    /// What the body learns about the function from its definition.
    fn signature(&self) -> Signature {
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
        resolve_on_body_path(of, self.database, &self.declaration_path)?
            .ok_or_else(|| LookupError::RelationNotFound { object_name: of.to_string() })
    }

    /// The type of the column `relation.column` names.
    fn column_type(&self, of: &ObjectName) -> Result<OperandType<'db>, LookupError> {
        let Some((column, relation)) =
            of.0.split_last().filter(|(_, relation)| !relation.is_empty())
        else {
            return Err(unresolvable(self.function.name(), &format_args!("{of}%TYPE")));
        };
        let relation = ObjectName(relation.to_vec());
        let (Relation::Table(table) | Relation::ForeignTable(table)) = self.relation(&relation)?
        else {
            // A view's columns take the types of its query's outputs.
            return Ok(OperandType::Unknown);
        };
        let column = table.column(&column.to_string(), self.database, IdentifierCase::AsWritten)?;
        let column = column.ok_or_else(|| {
            LookupError::ColumnNotFound {
                table_name: relation.to_string(),
                column_name: of.0.last().map(ToString::to_string).unwrap_or_default(),
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

/// Every place the PL/pgSQL body `block` of the function `names` belongs to
/// runs SQL it builds at run time, in body order.
///
/// # Errors
///
/// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
/// naming no variable and for an `OPEN` naming no cursor declared with a
/// query, and the errors reading the path a statement of the body sets.
pub(crate) fn dynamic_sites<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
) -> Result<Vec<DynamicExecution<'b, 'db, DB>>, LookupError> {
    let function = names.function.name();
    let sites = block.sites(&names.signature(), |name| names.holds_row(name)).map_err(|error| {
        match error {
            namespace::Error::UnknownAliasTarget { alias, target } => {
                unresolvable(function, &format_args!("{alias} ALIAS FOR {target}"))
            }
            namespace::Error::UnboundCursor { name } => {
                unresolvable(function, &format_args!("OPEN {name}"))
            }
        }
    })?;
    let mut collector = PathCollector {
        function,
        database: names.database,
        path: owned_path(names.declaration_path.clone()),
        hex_strings_may_be_integers,
        paths: Vec::new(),
    };
    walk_body(function, RunTimeBody::PlPgSql(block), &mut collector, hex_strings_may_be_integers)?;
    let dynamic_sites = sites.into_iter().filter_map(|site| {
        let Sql::Dynamic(dynamic) = site.sql else { return None };
        Some((dynamic, site.namespace))
    });
    Ok(dynamic_sites
        .zip(collector.paths)
        .map(|((dynamic, namespace), (walked, path))| {
            debug_assert!(core::ptr::eq(dynamic, walked), "sites pair in body order");
            let arguments = dynamic.arguments.iter().map(expression).collect();
            let scope = PlPgSqlScope::new(names, namespace, path);
            DynamicExecution::new(expression(&dynamic.command), arguments, scope)
        })
        .collect())
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

/// Gathers the path each dynamic-execution site of one PL/pgSQL body runs
/// under.
struct PathCollector<'f, DB> {
    function: &'f str,
    database: &'f DB,
    /// The path in force where the walk stands.
    path: BodyPath<'static>,
    hex_strings_may_be_integers: bool,
    /// The address of each site walked, in body order, with the path it
    /// runs under.
    paths: Vec<(*const Dynamic, BodyPath<'static>)>,
}

impl<DB: DatabaseLike> PathCollector<'_, DB> {
    /// Reads the path the constant command `statements` leaves behind, as
    /// the body statements they are would.
    fn constant_command(&mut self, statements: &[Statement]) {
        let function = self.function;
        for statement in statements {
            if let Err(error) =
                walk_statement(function, statement, self, self.hex_strings_may_be_integers)
            {
                self.path = BodyPath::Unknown(error);
            }
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

    fn walk(&mut self, _sql: Embedded<'_>) -> Result<(), LookupError> {
        Ok(())
    }

    fn runs_user_code(&self, sql: Embedded<'_>) -> bool {
        calls_user_code(&sql, self.database)
    }

    /// Code outside the body may change the path, which later names then
    /// cannot rely on.
    fn opaque(&mut self, _function: &str, statement: &'s Statement) -> Result<(), LookupError> {
        self.forget(statement);
        Ok(())
    }

    fn dynamic(&mut self, _function: &str, dynamic: &'s Dynamic) -> Result<(), LookupError> {
        self.paths.push((core::ptr::from_ref(dynamic), self.path.clone()));
        match constant_command(dynamic) {
            Some(statements) => self.constant_command(&statements),
            None => self.forget(&Embedded::Expression(&dynamic.command)),
        }
        let calling = core::iter::once(&*dynamic.command)
            .chain(&dynamic.arguments)
            .find(|query| self.runs_user_code(Embedded::Expression(query)));
        if let Some(query) = calling {
            self.forget(query);
        }
        Ok(())
    }
}
