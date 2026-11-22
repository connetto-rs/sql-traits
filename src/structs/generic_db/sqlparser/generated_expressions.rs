//! PostgreSQL generation expression validation.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateFunction, CreateTable, Expr, FunctionBehavior,
    GeneratedExpressionMode, Ident, ObjectName,
};

use super::{
    AccessResolution, DefaultPrivileges, ParserDB, ParserDBBuilder, ParserIngestion,
    PostgresCatalog, SessionRoles, SessionSettings, TypeArrays, case_idents_match,
    type_facts::type_facts,
};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, generation_storage},
    structs::{
        CallResolution, CoercionStep, ExpressionContext, OperandType, OperatorResolution,
        TargetName, TypeKind,
    },
    traits::{CastLike, CastMethod, DatabaseLike, FunctionLike, OperatorLike},
    utils::{
        call_sites::{ExpressionForm, expression_form},
        columns_in_expression::visit_columns,
        object_name::target_name_of_object_name,
        walk::walk_expressions,
    },
};

/// Builds the catalog used to type generated expressions before table
/// insertion.
pub(super) fn database(
    builder: &ParserDBBuilder,
    catalog: &Arc<PostgresCatalog>,
    arrays: &Arc<TypeArrays>,
) -> ParserDB {
    builder.snapshot_with(Arc::new(ParserIngestion {
        access_resolution: AccessResolution::default(),
        active_postgres_catalog: Arc::clone(catalog),
        type_arrays: Arc::clone(arrays),
        collation_metadata: Vec::new(),
        unresolved_function_grants: Vec::new(),
        role_alterations: Vec::new(),
        session_roles: SessionRoles::default(),
        session_settings: SessionSettings::default(),
        default_privileges: DefaultPrivileges::default(),
    }))
}

/// Refuses a default alongside a generation expression before binding either,
/// naming the table and column as `dialect` stores them.
pub(super) fn defaults(
    dialect: SqlparserDialect,
    table: &ObjectName,
    column: &ColumnDef,
) -> Result<(), Error> {
    if column.options.iter().any(|option| matches!(option.option, ColumnOption::Default(_)))
        && column.options.iter().any(|option| {
            matches!(option.option, ColumnOption::Generated { generation_expr: Some(_), .. })
        })
    {
        return Err(Error::DefaultAndGeneration {
            table: dialect.stored_object_name_last(table),
            column: dialect.stored_name(&column.name),
        });
    }
    Ok(())
}

/// Validates a generated column against the completed table declaration,
/// once [`super::stored_expressions`] has refused what PostgreSQL refuses
/// while transforming the expression.
pub(super) fn column(
    database: &ParserDB,
    table: &CreateTable,
    column: &ColumnDef,
) -> Result<(), Error> {
    if !matches!(database.dialect(), SqlparserDialect::PostgreSql) {
        return Ok(());
    }
    defaults(*database.dialect(), &table.name, column)?;
    let Some(mode) = generation_storage(column, *database.dialect()) else {
        return Ok(());
    };
    for option in &column.options {
        let ColumnOption::Generated { generation_expr: Some(expression), .. } = &option.option
        else {
            continue;
        };
        references_in(
            *database.dialect(),
            database.catalog_name(),
            database.identifier_case(),
            table,
            expression,
        )?;
        let context = ExpressionContext::for_declared_columns(
            expression,
            &table.columns,
            database,
            database.identifier_case(),
        )?;
        walk(expression, |node| immutable(database, &context, node))?;
        if let OperandType::Known { type_name: source, .. } = context.type_of(expression)?
            && let OperandType::Known { type_name: target, .. } =
                crate::structs::expression_context::declared_type(database, &column.data_type)
        {
            cast(database, &source, &target, false)?;
        }
        if mode == GeneratedExpressionMode::Virtual {
            walk(expression, |node| virtual_node(database, &context, node))?;
            if let OperandType::Known { type_name: source, .. } = context.type_of(expression)?
                && let OperandType::Known { type_name: target, .. } =
                    crate::structs::expression_context::declared_type(database, &column.data_type)
            {
                cast(database, &source, &target, true)?;
            }
        }
    }
    Ok(())
}

/// Resolves all names before refusing generated or forbidden system columns.
pub(super) fn references(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    expression: &Expr,
) -> Result<(), Error> {
    references_in(
        *builder.dialect(),
        builder.catalog_name(),
        builder.identifier_case(),
        table,
        expression,
    )
}

/// Resolves references using the declaration's catalog identity rules.
fn references_in(
    dialect: SqlparserDialect,
    catalog_name: &str,
    case: crate::structs::IdentifierCase,
    table: &CreateTable,
    expression: &Expr,
) -> Result<(), Error> {
    let mut refused = None;
    visit_columns(
        expression,
        catalog_name,
        &target_name_of_object_name(&table.name),
        case,
        dialect,
        |name| {
            if let Some(column) =
                table.columns.iter().find(|column| case_idents_match(&column.name, name, case))
            {
                if refused.is_none() && generation_storage(column, dialect).is_some() {
                    refused = Some(Error::GeneratedColumnReference {
                        column: dialect.stored_name(&column.name),
                    });
                }
                return true;
            }
            if system_name(case, name, "tableoid") {
                return true;
            }
            if let Some(system) = forbidden_system_column(case, name) {
                if refused.is_none() {
                    refused =
                        Some(Error::GeneratedSystemColumnReference { column: system.to_string() });
                }
                return true;
            }
            false
        },
    )?;
    refused.map_or(Ok(()), Err)
}

/// Compares a system column's unquoted catalog name.
fn system_name(case: crate::structs::IdentifierCase, name: &Ident, system: &str) -> bool {
    case.identifiers_match(system, false, &name.value, name.quote_style.is_some())
}

/// The system column `name` reads that a generation expression may not.
fn forbidden_system_column(
    case: crate::structs::IdentifierCase,
    name: &Ident,
) -> Option<&'static str> {
    ["ctid", "xmin", "xmax", "cmin", "cmax"]
        .into_iter()
        .find(|system| system_name(case, name, system))
}

/// The system columns PostgreSQL refuses that `expression`, the generation
/// expression of a column of `table`, reads outside its subqueries, each
/// with the address of the name reading it.
///
/// A reference to another table is left for [`references`] to refuse, and
/// the names past it are not read.
pub(super) fn system_columns(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    expression: &Expr,
) -> Vec<(usize, &'static str)> {
    let case = builder.identifier_case();
    let mut found = Vec::new();
    let foreign = visit_columns(
        expression,
        builder.catalog_name(),
        &target_name_of_object_name(&table.name),
        case,
        *builder.dialect(),
        |name| {
            let held =
                table.columns.iter().any(|column| case_idents_match(&column.name, name, case));
            if let Some(system) = forbidden_system_column(case, name).filter(|_| !held) {
                found.push((core::ptr::from_ref(name).addr(), system));
            }
            true
        },
    );
    debug_assert!(
        foreign.is_ok() || matches!(foreign, Err(Error::ForeignColumnReference { .. })),
        "every name resolves, so only a reference to another table stops the walk"
    );
    found
}

/// Stops the expression walk at its first refusal.
fn walk(expression: &Expr, mut check: impl FnMut(&Expr) -> Result<(), Error>) -> Result<(), Error> {
    match walk_expressions(expression, |node| {
        match check(node) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

/// Refuses mutable calls, operator implementations and conversions.
fn immutable(
    database: &ParserDB,
    context: &ExpressionContext<'_, '_, ParserDB>,
    expression: &Expr,
) -> Result<(), Error> {
    if matches!(expression, Expr::Nested(_)) {
        return Ok(());
    }
    if let Expr::Function(function) = expression
        && matches!(expression_form(&function.name), Some(ExpressionForm::Value(_)))
    {
        return Err(Error::GenerationExpressionNotImmutable);
    }
    if let Some(call) = context.call(expression)? {
        match call.resolution() {
            CallResolution::Selected(selected) => {
                immutable_function(selected.function())?;
                for argument in selected.arguments() {
                    conversions(database, argument.evidence().conversion(), false)?;
                }
            }
            CallResolution::Cast(target) => {
                if let Some(argument) = call.arguments().first()
                    && let OperandType::Known { type_name: source, .. } =
                        context.type_of(argument)?
                {
                    cast(database, &source, target, false)?;
                }
            }
            _ => {}
        }
    }
    if let Some(operators) = context.operators(expression)? {
        for operator in operators {
            if let OperatorResolution::Selected(selected) = operator.resolution() {
                if let Some(function) = selected.operator().implementation(database) {
                    immutable_function(function)?;
                }
                if let Some(left) = selected.left() {
                    conversions(database, left.conversion(), false)?;
                }
                conversions(database, selected.right().conversion(), false)?;
            }
        }
    }
    if let Expr::Cast { expr, .. } = expression
        && let OperandType::Known { type_name: source, .. } = context.type_of(expr)?
        && let OperandType::Known { type_name: target, .. } = context.type_of(expression)?
    {
        cast(database, &source, &target, false)?;
    }
    Ok(())
}

/// Requires the effective volatility of a selected function to be immutable.
fn immutable_function(function: &CreateFunction) -> Result<(), Error> {
    if function.volatility() == FunctionBehavior::Immutable {
        Ok(())
    } else {
        Err(Error::GenerationExpressionNotImmutable)
    }
}

/// Refuses a user-defined type or function at each virtual expression node.
fn virtual_node(
    database: &ParserDB,
    context: &ExpressionContext<'_, '_, ParserDB>,
    expression: &Expr,
) -> Result<(), Error> {
    if let OperandType::Known { type_name, .. } = context.type_of(expression)?
        && user_type(database, &type_name)
    {
        return Err(Error::GenerationExpressionUserDefinedType);
    }
    if let Some(call) = context.call(expression)?
        && let CallResolution::Selected(selected) = call.resolution()
    {
        user_function(database, selected.function())?;
        for argument in selected.arguments() {
            conversions(database, argument.evidence().conversion(), true)?;
        }
    }
    if let Some(operators) = context.operators(expression)? {
        for operator in operators {
            if let OperatorResolution::Selected(selected) = operator.resolution() {
                if let Some(function) = selected.operator().implementation(database) {
                    user_function(database, function)?;
                }
                if let Some(left) = selected.left() {
                    conversions(database, left.conversion(), true)?;
                }
                conversions(database, selected.right().conversion(), true)?;
            }
        }
    }
    if let Expr::Cast { expr, .. } = expression
        && let OperandType::Known { type_name: source, .. } = context.type_of(expr)?
        && let OperandType::Known { type_name: target, .. } = context.type_of(expression)?
    {
        cast(database, &source, &target, true)?;
    }
    Ok(())
}

/// Recognizes declared types, including domains and arrays of declared types.
fn user_type(database: &ParserDB, name: &TargetName<'_>) -> bool {
    if name.schema().is_some_and(|schema| schema != "pg_catalog") {
        return true;
    }
    let Some(facts) = type_facts(database, name) else { return false };
    if matches!(facts.kind(), Some(TypeKind::Domain | TypeKind::Enum | TypeKind::Composite)) {
        return true;
    }
    facts.array_element().is_some_and(|element| user_type(database, element))
}

/// Refuses functions held by declarations rather than the builtin catalog.
fn user_function(database: &ParserDB, function: &CreateFunction) -> Result<(), Error> {
    if database.functions.iter().any(|(declared, _)| core::ptr::eq(declared.as_ref(), function)) {
        Err(Error::GenerationExpressionUserDefinedFunction)
    } else {
        Ok(())
    }
}

/// Checks conversions introduced by overload resolution.
fn conversions(
    database: &ParserDB,
    steps: &[CoercionStep<'_, ParserDB>],
    virtual_only: bool,
) -> Result<(), Error> {
    for step in steps {
        match step {
            CoercionStep::Function(conversion)
            | CoercionStep::LengthCoercion { cast: conversion, .. } => {
                if let Some(function) = conversion.function(database) {
                    if virtual_only {
                        user_function(database, function)?;
                    } else {
                        immutable_function(function)?;
                    }
                }
            }
            CoercionStep::InOut(conversion) if !virtual_only => {
                in_out(
                    database,
                    &conversion.source_type(database),
                    &conversion.target_type(database),
                )?;
            }
            CoercionStep::Array { element } => conversions(database, element, virtual_only)?,
            _ => {}
        }
    }
    Ok(())
}

/// Checks the catalog cast, array conversion or text I/O conversion between
/// types.
fn cast(
    database: &ParserDB,
    source: &TargetName<'_>,
    target: &TargetName<'_>,
    virtual_only: bool,
) -> Result<(), Error> {
    if source == target {
        return Ok(());
    }
    if let Some(facts) = type_facts(database, source)
        && let Some(domain) = facts.domain()
    {
        return cast(database, domain.base(), target, virtual_only);
    }
    if let Some(facts) = type_facts(database, target)
        && let Some(domain) = facts.domain()
    {
        return cast(database, source, domain.base(), virtual_only);
    }
    if let Some(mut casts) = database.casts()
        && let Some(conversion) = casts.find(|conversion| {
            conversion.source_type(database) == *source
                && conversion.target_type(database) == *target
        })
    {
        match conversion.method() {
            CastMethod::Function => {
                if let Some(function) = conversion.function(database) {
                    return if virtual_only {
                        user_function(database, function)
                    } else {
                        immutable_function(function)
                    };
                }
            }
            CastMethod::InOut if !virtual_only => return in_out(database, source, target),
            CastMethod::Binary | CastMethod::InOut => {}
        }
        return Ok(());
    }
    if let (Some(source_facts), Some(target_facts)) =
        (type_facts(database, source), type_facts(database, target))
        && let (Some(source_element), Some(target_element)) =
            (source_facts.array_element(), target_facts.array_element())
    {
        return cast(database, source_element, target_element, virtual_only);
    }
    if !virtual_only && (string_type(source) || string_type(target)) {
        return in_out(database, source, target);
    }
    Ok(())
}

/// Identifies builtin string targets that admit I/O conversions.
fn string_type(name: &TargetName<'_>) -> bool {
    name.schema() == Some("pg_catalog") && matches!(name.name(), "text" | "varchar" | "bpchar")
}

/// Refuses mutable type input or output functions used by a text conversion.
fn in_out(
    database: &ParserDB,
    source: &TargetName<'_>,
    target: &TargetName<'_>,
) -> Result<(), Error> {
    if mutable_io(database, source, false) || mutable_io(database, target, true) {
        Err(Error::GenerationExpressionNotImmutable)
    } else {
        Ok(())
    }
}

/// Classifies PostgreSQL type I/O volatility, including container I/O
/// functions.
fn mutable_io(database: &ParserDB, name: &TargetName<'_>, input: bool) -> bool {
    if let Some(facts) = type_facts(database, name) {
        if let Some(domain) = facts.domain() {
            return mutable_io(database, domain.base(), input);
        }
        if facts.array_element().is_some()
            || matches!(
                facts.kind(),
                Some(TypeKind::Enum | TypeKind::Composite | TypeKind::Range | TypeKind::Multirange)
            )
        {
            return true;
        }
    }
    matches!(
        name.name(),
        "aclitem"
            | "date"
            | "interval"
            | "money"
            | "timestamp"
            | "timestamptz"
            | "regclass"
            | "regcollation"
            | "regconfig"
            | "regdictionary"
            | "regnamespace"
            | "regoper"
            | "regoperator"
            | "regproc"
            | "regprocedure"
            | "regrole"
            | "regtype"
    ) || (input && matches!(name.name(), "time" | "timetz" | "xml"))
}
