//! The definitions a PL/pgSQL body may give its variables, carried to every
//! statement and expression it evaluates by the walk of
//! `sqlparser-plpgsql-dataflow`, and bound to the database.

use alloc::{collections::BTreeMap, vec::Vec};
use core::convert::Infallible;

use sqlparser::ast::{
    ArgMode, Expr, Function, FunctionArg, FunctionArguments, FunctionSecurity, Ident, Statement,
};
use sqlparser_plpgsql::{
    embedded::{Block, DiagnosticsKind, expression},
    namespace::{Name, Namespace, Site, Sql},
};
use sqlparser_plpgsql_dataflow::{
    Catalog, Ending, Error, Part, Source, Transactions,
    reaching::{Definition, ReachingDefinitions},
    values::{Analysis, analyze},
    walk,
};

use super::{
    data_statement::{BodyPath, unresolvable},
    dynamic_sites::{BodyNames, SiteKey, body_sql, namespace_error},
};
use crate::{
    errors::LookupError,
    structs::{
        BodyTruth, DefinitionSource, DiagnosticsItem, FunctionKind, IdentifierCase, PlPgSqlScope,
        TargetStep,
        plpgsql_flow::{CursorQuery, ValueFlow, Written},
    },
    traits::{DatabaseLike, FunctionLike},
    utils::{
        call_typing::called_object_name,
        object_name::{RoutinePath, reaches_unheld_schema},
        postgres_types::PgType,
    },
};

/// Every definition of the PL/pgSQL body `block` of the function `names`
/// belongs to, and the definitions reaching each statement, expression and
/// dynamic SQL of it.
///
/// # Errors
///
/// Returns the errors [`body_sql`] and [`FunctionLike::kind`] return, and
/// [`LookupError::FunctionBodyUnresolvable`] for a write whose target names
/// no variable.
pub(crate) fn value_flow<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
) -> Result<ValueFlow<'b, 'db, DB>, LookupError> {
    let function = names.function().name();
    let kind = names.function().kind(names.database())?;
    let transactions = transactions(names, kind);
    let (sites, paths) = body_sql(names, block, hex_strings_may_be_integers)?;
    let mut catalog = BodyCatalog { names, paths: &paths };
    let mut domain = ReachingDefinitions::new();
    let flow = walk(
        block,
        &names.signature(),
        transactions,
        ending(names, kind),
        &mut catalog,
        &mut domain,
    )
    .map_err(|error| walk_error(function, error, |never: Infallible| match never {}))?;
    let namespaces: BTreeMap<SiteKey, Namespace> =
        sites.into_iter().map(|Site { sql, namespace }| (SiteKey::of(sql), namespace)).collect();
    // Each definition owns the scope of the SQL it reads, which several
    // definitions of one step share.
    let scope = |sql: Sql<'b>| {
        let key = SiteKey::of(sql);
        let namespace = namespaces.get(&key)?.clone();
        Some(PlPgSqlScope::new(names, namespace, paths.get(&key)?.clone()))
    };
    let definitions =
        domain.definitions().iter().map(|definition| written(names, definition, scope)).collect();
    let cursors = domain
        .cursors()
        .iter()
        .filter_map(|(&id, cursor)| {
            let statement = &*cursor.query;
            Some((id, CursorQuery { statement, scope: scope(Sql::Statement(statement))? }))
        })
        .collect();
    Ok(ValueFlow { definitions, flow, cursors, rows: Default::default() })
}

/// The walk of the PL/pgSQL body `block` of the function `names` belongs to
/// with the value guarantees domain over `context`.
///
/// # Errors
///
/// Returns the errors [`value_flow`] returns, and those of `context`.
pub(crate) fn value_analysis<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    context: BodyTruth<'b, 'db, DB>,
) -> Result<Analysis<'b, BodyTruth<'b, 'db, DB>>, LookupError> {
    let function = names.function().name();
    let kind = names.function().kind(names.database())?;
    let transactions = transactions(names, kind);
    let (_, paths) = body_sql(names, block, hex_strings_may_be_integers)?;
    let mut catalog = BodyCatalog { names, paths: &paths };
    analyze(block, &names.signature(), transactions, ending(names, kind), &mut catalog, context)
        .map_err(|error| walk_error(function, error, |error| error))
}

/// Whether the body of the function `names` belongs to, of kind `kind`, may
/// end and start transactions, which a procedure with neither a `SET` clause
/// nor `SECURITY DEFINER` may.
fn transactions<DB: DatabaseLike>(names: &BodyNames<'_, DB>, kind: FunctionKind) -> Transactions {
    let routine = names.function();
    if kind == FunctionKind::Procedure
        && routine.configuration_parameters().is_empty()
        && routine.security_mode() != FunctionSecurity::Definer
    {
        Transactions::Controlled
    } else {
        Transactions::Refused
    }
}

/// What control reaching the end of the body of the function `names` belongs
/// to, of kind `kind`, does. PostgreSQL compiles an implicit `RETURN` after
/// the body of a procedure and of a function with `OUT` or `INOUT`
/// parameters, returning a set, `void` or `event_trigger`, and any other
/// function raises `2F005` there.
fn ending<DB: DatabaseLike>(names: &BodyNames<'_, DB>, kind: FunctionKind) -> Ending {
    let routine = names.function();
    let returns = kind == FunctionKind::Procedure
        || routine.returns_set()
        || routine.argument_modes().any(|mode| matches!(mode, ArgMode::Out | ArgMode::InOut))
        || matches!(
            routine.return_type_name(names.database()).as_deref().and_then(PgType::parse),
            Some(PgType::Catalog("void" | "event_trigger"))
        );
    if returns { Ending::Returns } else { Ending::Raises }
}

/// The error the body of `function` answers for what stopped the walk, the
/// domain's error answering as `domain` gives it.
fn walk_error<E>(
    function: &str,
    error: Error<E>,
    domain: impl FnOnce(E) -> LookupError,
) -> LookupError {
    match error {
        Error::Namespace(error) => namespace_error(function, error),
        Error::UnresolvedTarget { name } => unresolvable(function, &name),
        Error::Domain(error) => domain(error),
    }
}

/// What the database tells the walk about the body of the function `names`
/// belongs to.
struct BodyCatalog<'a, 'db, DB: DatabaseLike> {
    names: &'a BodyNames<'db, DB>,
    /// The path each statement and expression of the body runs under.
    paths: &'a BTreeMap<SiteKey, BodyPath<'static>>,
}

impl<DB: DatabaseLike> Catalog for BodyCatalog<'_, '_, DB> {
    fn holds_row(&self, name: &Name) -> bool {
        self.names.holds_row(name)
    }

    fn written_back(&mut self, call: &Statement) -> Option<Vec<bool>> {
        let path = self.paths.get(&SiteKey::of(Sql::Statement(call)))?;
        written_back(call, self.names.database(), path)
    }
}

/// The definition the walk's `definition` makes, bound to the variables of
/// `names` and to the scope `scope` gives the SQL its source reads.
fn written<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    definition: &Definition<'b>,
    scope: impl Fn(Sql<'b>) -> Option<PlPgSqlScope<'b, 'db, DB>>,
) -> Written<'b, 'db, DB> {
    let write = &definition.write;
    let part = write
        .part
        .iter()
        .map(|step| {
            match step {
                Part::Field(field) => TargetStep::Field(field.clone()),
                Part::Index(index) => TargetStep::Index(expression(index)),
                Part::Slice { lower, upper } => {
                    TargetStep::Slice { lower: lower.map(expression), upper: upper.map(expression) }
                }
            }
        })
        .collect();
    let source = match &write.source {
        // The walk's reaching definitions keep no definition of the null a
        // cursor loop gives back, which removes the portal name instead.
        Source::Null | Source::PortalReleased => DefinitionSource::Null,
        Source::Value(value) => DefinitionSource::Value(expression(value)),
        Source::CursorName => DefinitionSource::CursorName,
        Source::PortalName => DefinitionSource::PortalName,
        Source::Query { statement, output, rows } => {
            DefinitionSource::QueryOutput { statement, output: *output, rows: *rows }
        }
        Source::Dynamic { dynamic, output, rows } => {
            DefinitionSource::DynamicOutput {
                command: expression(&dynamic.command),
                arguments: dynamic.arguments.iter().map(expression).collect(),
                output: *output,
                rows: *rows,
            }
        }
        Source::Cursor { cursor, output } => {
            DefinitionSource::CursorOutput { cursor: names.variable(cursor), output: *output }
        }
        Source::Call { call, argument } => {
            DefinitionSource::CallOutput { call, argument: *argument }
        }
        Source::Diagnostics(kind) => DefinitionSource::Diagnostics(diagnostics_item(*kind)),
        Source::LoopIndex { reverse, lower, upper, step } => {
            DefinitionSource::LoopIndex {
                reverse: *reverse,
                lower: expression(lower),
                upper: expression(upper),
                step: step.map(expression),
            }
        }
        Source::ArrayElement { array, slice, output } => {
            DefinitionSource::ArrayElement {
                array: expression(array),
                slice: *slice,
                output: *output,
            }
        }
        Source::Found(found) => DefinitionSource::Found(*found),
    };
    Written {
        variable: names.variable(&write.variable),
        part,
        source,
        scope: write.site.and_then(scope),
        inputs: definition.inputs.clone(),
    }
}

/// Whether `call` writes back to each argument it passes, when it reaches
/// one procedure of `database` under `path`, matching every argument, `OUT`
/// ones included, by position or by name, and `None` when it reaches none or
/// several or may land in a schema the model does not hold.
fn written_back<DB: DatabaseLike>(
    call: &Statement,
    database: &DB,
    path: &BodyPath<'_>,
) -> Option<Vec<bool>> {
    let Statement::Call(Function { name, args: FunctionArguments::List(list), .. }) = call else {
        return None;
    };
    let written = list
        .args
        .iter()
        .map(|argument| {
            match argument {
                FunctionArg::Unnamed(_) => Some(None),
                FunctionArg::Named { name, .. }
                | FunctionArg::ExprNamed { name: Expr::Identifier(name), .. } => Some(Some(name)),
                FunctionArg::ExprNamed { .. } => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let target = called_object_name(name).ok()?;
    let case = IdentifierCase::Folded;
    let routine_path = match (target.schema(), path) {
        (None, BodyPath::Unknown(_)) => return None,
        (Some(_), _) | (None, BodyPath::Database) => RoutinePath::Database,
        (None, BodyPath::Set(path)) => RoutinePath::Set(path),
    };
    if reaches_unheld_schema(database, &target, routine_path, case) {
        return None;
    }
    let path = match target.schema() {
        Some(schema) => Vec::from([(schema, target.schema_is_quoted())]),
        None => routine_path.schemas(database),
    };
    let mut reached = database.functions_named(target.name()).filter_map(|function| {
        let declared = function.target_name();
        let (schema, quoted) = declared
            .schema()
            .map_or(("public", false), |schema| (schema, declared.schema_is_quoted()));
        let on_path = path.iter().any(|&(entry, entry_quoted)| {
            case.identifiers_match(schema, quoted, entry, entry_quoted)
        });
        let named = case.identifiers_match(
            function.name(),
            function.name_is_quoted(),
            target.name(),
            target.name_is_quoted(),
        );
        (on_path && named && function.kind(database) == Ok(FunctionKind::Procedure))
            .then(|| parameter_writes(function, database, &written, case))
            .flatten()
    });
    let writes = reached.next()?;
    reached.next().is_none().then_some(writes)
}

/// Whether a call of `function` passing arguments under the names `written`
/// writes back to each, `None` when the arguments do not match its
/// parameters.
fn parameter_writes<F: FunctionLike>(
    function: &F,
    database: &F::DB,
    written: &[Option<&Ident>],
    case: IdentifierCase,
) -> Option<Vec<bool>> {
    let modes: Vec<ArgMode> = function.argument_modes().collect();
    if modes.contains(&ArgMode::Variadic) {
        return None;
    }
    let names: Vec<_> = function.argument_names(database).collect();
    let mut supplied = Vec::from_iter(core::iter::repeat_n(false, modes.len()));
    let mut writes = Vec::with_capacity(written.len());
    let mut named_seen = false;
    for (position, written) in written.iter().enumerate() {
        let parameter = match written {
            // PostgreSQL refuses a positional argument after a named one.
            None if named_seen => return None,
            None => position,
            Some(written) => {
                named_seen = true;
                names.iter().position(|declared| {
                    declared.as_ref().is_some_and(|declared| {
                        case.identifiers_match(
                            declared.name(),
                            declared.name_is_quoted(),
                            &written.value,
                            written.quote_style.is_some(),
                        )
                    })
                })?
            }
        };
        let already = core::mem::replace(supplied.get_mut(parameter)?, true);
        if already {
            return None;
        }
        writes.push(matches!(modes.get(parameter), Some(ArgMode::Out | ArgMode::InOut)));
    }
    let missing = supplied.iter().filter(|supplied| !**supplied).count();
    (missing <= function.argument_default_count()).then_some(writes)
}

/// The [`DiagnosticsItem`] standing for `kind`.
const fn diagnostics_item(kind: DiagnosticsKind) -> DiagnosticsItem {
    match kind {
        DiagnosticsKind::RowCount => DiagnosticsItem::RowCount,
        DiagnosticsKind::RoutineOid => DiagnosticsItem::RoutineOid,
        DiagnosticsKind::Context => DiagnosticsItem::Context,
        DiagnosticsKind::ReturnedSqlstate => DiagnosticsItem::ReturnedSqlstate,
        DiagnosticsKind::ColumnName => DiagnosticsItem::ColumnName,
        DiagnosticsKind::ConstraintName => DiagnosticsItem::ConstraintName,
        DiagnosticsKind::DatatypeName => DiagnosticsItem::DatatypeName,
        DiagnosticsKind::MessageText => DiagnosticsItem::MessageText,
        DiagnosticsKind::TableName => DiagnosticsItem::TableName,
        DiagnosticsKind::SchemaName => DiagnosticsItem::SchemaName,
        DiagnosticsKind::ExceptionDetail => DiagnosticsItem::ExceptionDetail,
        DiagnosticsKind::ExceptionHint => DiagnosticsItem::ExceptionHint,
        DiagnosticsKind::ExceptionContext => DiagnosticsItem::ExceptionContext,
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser_plpgsql_dataflow::Ending;

    use super::{BodyNames, ending};
    use crate::{
        prelude::ParserDB,
        traits::{DatabaseLike, FunctionLike},
    };

    const ROUTINES: &str = "
        CREATE FUNCTION scalar() RETURNS int LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION fires() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION nothing() RETURNS void LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION qualified_nothing() RETURNS pg_catalog.void LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION outputs(OUT result int) LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION passes(INOUT result int) LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION many() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE FUNCTION rows() RETURNS TABLE(id int) LANGUAGE plpgsql AS $$ BEGIN END $$;
        CREATE PROCEDURE run() LANGUAGE plpgsql AS $$ BEGIN END $$;
    ";

    /// What control reaching the end of the body of `name` does.
    fn ending_of(database: &ParserDB, name: &str) -> Ending {
        let function = database
            .functions()
            .find(|function| function.name() == name)
            .expect("the routine is declared");
        let names = BodyNames::new(function, database).expect("the body path resolves");
        ending(&names, function.kind(database).expect("the kind is known"))
    }

    #[test]
    fn routines_given_an_implicit_return_return_at_the_end_of_their_body() {
        let database = ParserDB::parse::<PostgreSqlDialect>(ROUTINES).expect("routines parse");
        for name in
            ["nothing", "qualified_nothing", "ddl", "outputs", "passes", "many", "rows", "run"]
        {
            assert_eq!(ending_of(&database, name), Ending::Returns, "{name}");
        }
    }

    #[test]
    fn other_functions_raise_at_the_end_of_their_body() {
        let database = ParserDB::parse::<PostgreSqlDialect>(ROUTINES).expect("routines parse");
        for name in ["scalar", "fires"] {
            assert_eq!(ending_of(&database, name), Ending::Raises, "{name}");
        }
    }
}
