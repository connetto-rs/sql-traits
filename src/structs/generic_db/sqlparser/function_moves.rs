//! Moving and renaming functions, as `ALTER SCHEMA ... RENAME TO`,
//! `ALTER FUNCTION ... RENAME TO` and `ALTER FUNCTION ... SET SCHEMA` do.
//!
//! PostgreSQL binds a call to the function it reaches when the call is
//! written, so a moved function stays the one every call runs. The model
//! holds those bindings, and respells each call bound to a moved function to
//! the function's new schema-qualified name, which keeps the call reaching it
//! whatever search path later reads it. A body written as a string is not
//! bound and keeps its text, as in PostgreSQL.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateFunction, CreateFunctionBody, ObjectName, UserDefinedTypeRangeOption,
    UserDefinedTypeRepresentation, UserDefinedTypeSqlDefinitionOption, VisitMut,
};

use super::{
    CarriedCalls, ParserDB, ParserDBBuilder, StoredTable, constraint_names,
    function_argument_types, partition_keys,
};
use crate::{
    errors::Error,
    structs::{
        TableAttribute, UserDefinedType,
        metadata::{BoundCall, ExpressionCalls},
    },
    traits::FunctionLike,
    utils::call_sites::{call_reaches, for_each_call_name_mut},
};

/// The functions a statement moves, each with the name it is stored under
/// afterwards.
pub(super) struct Moves(Vec<(Arc<CreateFunction>, ObjectName)>);

impl Moves {
    /// Moves each function of `moving` to the name paired with it.
    pub(super) fn new(moving: Vec<(Arc<CreateFunction>, ObjectName)>) -> Self {
        Self(moving)
    }

    /// The name the stored function `function` moves to, if it moves.
    fn name_of(&self, function: &CreateFunction) -> Option<&ObjectName> {
        self.0
            .iter()
            .find(|(moving, _)| core::ptr::eq(moving.as_ref(), function))
            .map(|(_, name)| name)
    }

    /// Whether a call bound to `binding` reaches a moving function.
    fn reached_by(&self, binding: &ExpressionCalls<CreateFunction>) -> bool {
        binding.functions().any(|function| self.name_of(function).is_some())
    }

    /// The name a call bound to `binding` is respelled to, or [`None`] when
    /// it reaches no moving function.
    ///
    /// # Errors
    ///
    /// Returns the moving function the call reaches when it also reaches a
    /// function that stays, which the call cannot be respelled to follow.
    fn renamed<'a>(
        &'a self,
        binding: &'a [Arc<CreateFunction>],
    ) -> Result<Option<&'a ObjectName>, &'a CreateFunction> {
        let mut renamed = None;
        let mut stays = false;
        for function in binding {
            match self.name_of(function) {
                Some(name) => {
                    renamed.get_or_insert((function.as_ref(), name));
                }
                None => stays = true,
            }
        }
        match renamed {
            Some((function, _)) if stays => Err(function),
            renamed => Ok(renamed.map(|(_, name)| name)),
        }
    }
}

/// Respells every call of `node` that `calls`, the bindings resolved from
/// `node`, bind to a moving function.
///
/// # Errors
///
/// Returns [`Error::MovedFunctionCallAmbiguous`] for a call reaching both a
/// moving function and one that stays.
fn respell<N: VisitMut + ?Sized>(
    node: &mut N,
    calls: &ExpressionCalls<CreateFunction>,
    moves: &Moves,
) -> Result<(), Error> {
    let mut sites = calls.sites();
    respell_each(node, |name, _| renamed_call(moves, sites.next(), name))
}

/// The name a call written `call` and bound to `binding` is respelled to, or
/// [`None`] when it reaches no moving function.
///
/// # Errors
///
/// Returns [`Error::MovedFunctionCallAmbiguous`] for a call reaching both a
/// moving function and one that stays.
fn renamed_call(
    moves: &Moves,
    binding: Option<&[Arc<CreateFunction>]>,
    call: &ObjectName,
) -> Result<Option<ObjectName>, Error> {
    match binding.map_or(Ok(None), |binding| moves.renamed(binding)) {
        Ok(renamed) => Ok(renamed.cloned()),
        Err(function) => {
            Err(Error::MovedFunctionCallAmbiguous {
                function_name: function.name().to_string(),
                call: call.to_string(),
            })
        }
    }
}

/// Respells every function name a user-defined type's definition writes, in
/// the order its calls bind them, to the name its bound function moves to.
fn respell_user_type(
    node: &mut UserDefinedType,
    calls: &ExpressionCalls<CreateFunction>,
    moves: &Moves,
) -> Result<(), Error> {
    let mut sites = calls.sites();
    let mut respell = |name: &mut ObjectName| -> Result<(), Error> {
        renamed_call(moves, sites.next(), name).map(|renamed| {
            if let Some(renamed) = renamed {
                *name = renamed;
            }
        })
    };
    // A base type and a range declare functions, which a moved one respells.
    if let Some(UserDefinedTypeRepresentation::SqlDefinition { options }) =
        node.representation.as_mut()
    {
        for option in options {
            if let Some(name) = base_function_name(option) {
                respell(name)?;
            }
        }
    }
    if let Some(UserDefinedTypeRepresentation::Range { options }) = node.representation.as_mut() {
        for option in options {
            if let Some(name) = range_function_name(option) {
                respell(name)?;
            }
        }
    }
    Ok(())
}

/// The function name a base type option declares, or none for an option that
/// declares no function.
fn base_function_name(option: &mut UserDefinedTypeSqlDefinitionOption) -> Option<&mut ObjectName> {
    match option {
        UserDefinedTypeSqlDefinitionOption::Input(name)
        | UserDefinedTypeSqlDefinitionOption::Output(name)
        | UserDefinedTypeSqlDefinitionOption::Receive(name)
        | UserDefinedTypeSqlDefinitionOption::Send(name)
        | UserDefinedTypeSqlDefinitionOption::TypmodOut(name)
        | UserDefinedTypeSqlDefinitionOption::Analyze(name)
        | UserDefinedTypeSqlDefinitionOption::Subscript(name)
        | UserDefinedTypeSqlDefinitionOption::TypmodIn(name) => Some(name),
        _ => None,
    }
}

/// The function name a range option declares, or none for the subtype and
/// multirange options, which name types.
fn range_function_name(option: &mut UserDefinedTypeRangeOption) -> Option<&mut ObjectName> {
    match option {
        UserDefinedTypeRangeOption::Canonical(name)
        | UserDefinedTypeRangeOption::SubtypeDiff(name) => Some(name),
        _ => None,
    }
}

/// Respells every call of `node` to the name `renamed` gives it from its
/// written name and argument count, leaving a call it gives none as written.
fn respell_each<N: VisitMut + ?Sized>(
    node: &mut N,
    mut renamed: impl FnMut(&ObjectName, usize) -> Result<Option<ObjectName>, Error>,
) -> Result<(), Error> {
    let flow = for_each_call_name_mut(node, |name, arguments| {
        match renamed(name, arguments) {
            Ok(respelled) => {
                if let Some(respelled) = respelled {
                    *name = respelled;
                }
                ControlFlow::Continue(())
            }
            Err(error) => ControlFlow::Break(error),
        }
    });
    match flow {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

/// Moves every function of `moves` to its new name, respelling every call
/// bound to one and re-pointing every binding at the moved node.
///
/// # Errors
///
/// Returns [`Error::MovedFunctionCallAmbiguous`] for a call reaching both a
/// moving function and one that stays, and the refusals rebuilding a table
/// meets.
pub(super) fn move_functions(
    mut builder: ParserDBBuilder,
    moves: &Moves,
) -> Result<ParserDBBuilder, Error> {
    let mut replaced = Vec::new();
    for (function, metadata) in builder.functions_mut() {
        let renamed = moves.name_of(function).cloned();
        let calls_moved = moves.reached_by(metadata.body_calls());
        if renamed.is_none() && !calls_moved {
            continue;
        }
        let stale = Arc::clone(function);
        let node = Arc::make_mut(function);
        if let Some(renamed) = renamed {
            node.name = renamed;
        }
        match &mut node.function_body {
            Some(CreateFunctionBody::Return(expression)) => {
                respell(expression, metadata.body_calls(), moves)?;
            }
            Some(CreateFunctionBody::BeginAtomic(block)) => {
                respell(&mut block.statements, metadata.body_calls(), moves)?;
            }
            _ => {}
        }
        replaced.push((stale, Arc::clone(function)));
    }

    for (policy, metadata) in builder.policies_mut() {
        if moves.reached_by(metadata.using_calls()) || moves.reached_by(metadata.check_calls()) {
            let policy = Arc::make_mut(policy);
            respell(&mut policy.using, metadata.using_calls(), moves)?;
            respell(&mut policy.with_check, metadata.check_calls(), moves)?;
            for (written, calls) in metadata.written_clauses_mut() {
                if let Some(written) = written {
                    respell(written, calls, moves)?;
                }
            }
        }
    }
    for (view, metadata, _) in builder.views_mut() {
        if moves.reached_by(metadata.calls()) {
            respell(Arc::make_mut(view).declaration_mut().query_mut(), metadata.calls(), moves)?;
        }
    }
    for (view, metadata, _) in builder.materialized_views_mut() {
        if moves.reached_by(metadata.calls()) {
            respell(Arc::make_mut(view).declaration_mut().query_mut(), metadata.calls(), moves)?;
        }
    }
    for (rule, calls) in builder.rules_mut() {
        if moves.reached_by(calls) {
            respell(Arc::make_mut(rule), calls, moves)?;
        }
    }
    for (domain, metadata) in builder.domains_mut() {
        if moves.reached_by(metadata.calls()) {
            respell(Arc::make_mut(domain), metadata.calls(), moves)?;
        }
    }
    for (type_, calls) in builder.user_defined_types_mut() {
        if moves.reached_by(calls) {
            respell_user_type(Arc::make_mut(type_), calls, moves)?;
        }
    }
    let case = builder.identifier_case();
    for (trigger, calls) in builder.triggers_mut() {
        if moves.reached_by(calls) {
            respell(&mut Arc::make_mut(trigger).condition, calls, moves)?;
        }
        // A trigger holds the name of the function it runs, bound at creation
        // and left bare for one in the default schema.
        let runs = trigger.exec_body.as_ref().and_then(|body| {
            moves.0.iter().find(|(function, _)| {
                call_reaches(
                    function.as_ref(),
                    &body.func_desc.name,
                    0,
                    || [("public", false)].into_iter(),
                    case,
                ) && function_argument_types(function.args.as_deref()).is_empty()
            })
        });
        if let Some((_, renamed)) = runs
            && let Some(body) = &mut Arc::make_mut(trigger).exec_body
        {
            body.func_desc.name = renamed.clone();
        }
    }

    builder = respell_tables(builder, moves)?;

    for (stale, fresh) in &replaced {
        builder.repoint_function(stale, fresh);
    }
    Ok(builder)
}

/// Respells the calls bound to a moving function in every table, through its
/// columns, its checks and its indexes, and rebuilds each such table.
fn respell_tables(mut builder: ParserDBBuilder, moves: &Moves) -> Result<ParserDBBuilder, Error> {
    let mut tables: Vec<StoredTable> = Vec::new();
    let mut held = |table: &sqlparser::ast::CreateTable| {
        let stored = StoredTable::of(table);
        if !tables.contains(&stored) {
            tables.push(stored);
        }
    };
    for (column, metadata, _) in builder.columns() {
        if moves.reached_by(metadata.calls()) {
            held(TableAttribute::table(column.as_ref()));
        }
    }
    for (check, metadata) in builder.check_constraints() {
        if moves.reached_by(metadata.calls()) {
            held(TableAttribute::table(check.as_ref()));
        }
    }
    for (index, metadata) in builder.indices() {
        if moves.reached_by(metadata.calls()) {
            held(TableAttribute::table(index.as_ref()));
        }
    }
    for (table, metadata, _) in builder.tables() {
        if moves.reached_by(metadata.partition_key_calls())
            || metadata.exclusion_calls().iter().any(|calls| moves.reached_by(calls))
        {
            held(table);
        }
    }

    let case = builder.identifier_case();
    for stored in tables {
        for (index, metadata) in builder.indices_mut() {
            if stored.matches(TableAttribute::table(index.as_ref()), case)
                && moves.reached_by(metadata.calls())
            {
                respell(Arc::make_mut(index).attribute_mut(), metadata.calls(), moves)?;
            }
        }
        // The table node spells the calls its columns, checks, partition key
        // and exclusion constraints were bound from, each found by its
        // written name and argument count, the way the rebuild finds it.
        let mut bound = CarriedCalls::default();
        for (column, metadata, _) in builder.columns() {
            if stored.matches(TableAttribute::table(column.as_ref()), case) {
                bound.record(column.attribute(), metadata.calls());
            }
        }
        for (check, metadata) in builder.check_constraints() {
            if stored.matches(TableAttribute::table(check.as_ref()), case) {
                bound.record(metadata.expression(), metadata.calls());
            }
        }
        for (table, metadata, _) in builder.tables() {
            if !stored.matches(table, case) {
                continue;
            }
            for (exclusion, calls) in
                constraint_names::exclusions(table).zip(metadata.exclusion_calls())
            {
                bound.record(exclusion, calls);
            }
            if let Some(entries) = partition_keys::key_entries(table) {
                bound.record(entries, metadata.partition_key_calls());
            }
        }
        builder = ParserDB::replace_table_node_carrying(
            builder,
            &stored,
            |carried| {
                carried.rename(|binding| moves.renamed(binding).ok().flatten().cloned());
            },
            |_, node| {
                respell_each(node, |name, arguments| {
                    let binding = bound.take_bound(name, arguments);
                    renamed_call(moves, binding.as_ref().map(BoundCall::reached), name)
                })
            },
        )?;
    }
    Ok(builder)
}
