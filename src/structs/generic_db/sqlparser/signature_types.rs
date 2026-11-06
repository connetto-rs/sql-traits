//! The types a function's signature names, each argument's, the result's and
//! each column of a `RETURNS TABLE`.
//!
//! Every rule here was measured against PostgreSQL 18.6 in Docker.
//!
//! PostgreSQL records a dependency of the function on each of them. A bare
//! name binds to the type the path reaches when the function is created, a
//! rename of the type, of its schema or of the relation whose row type it is
//! carries every signature along, and dropping the type is refused while a
//! signature names it, `CASCADE` dropping the function.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::{
    ColumnDef, CreateFunction, DataType, FunctionReturnType, ObjectName, OperateFunctionArg,
};

use super::{
    ParserDBBuilder,
    domains::{Reached, bind_signature_type, custom_name, custom_name_mut, reach_in_schema},
    object_name_last_identifier, schema_qualifiers_match,
    view_rewrites::edit_function,
    views::bound_read_key,
};
use crate::utils::object_name::{RelationKey, qualifier_of};

/// The columns of a `RETURNS TABLE`, or the one type another result names.
fn result_types(function: &CreateFunction) -> (Option<&[ColumnDef]>, Option<&DataType>) {
    match &function.return_type {
        Some(FunctionReturnType::DataType(DataType::Table(Some(columns)))) => {
            (Some(columns.as_slice()), None)
        }
        Some(FunctionReturnType::DataType(data_type) | FunctionReturnType::SetOf(data_type)) => {
            (None, Some(data_type))
        }
        None => (None, None),
    }
}

/// Every type the signature of `function` declares.
pub(super) fn signature_types(function: &CreateFunction) -> impl Iterator<Item = &DataType> {
    let (columns, result) = result_types(function);
    function
        .args
        .iter()
        .flatten()
        .map(|argument| &argument.data_type)
        .chain(columns.into_iter().flatten().map(|column| &column.data_type))
        .chain(result)
}

/// [`signature_types`], for rewriting.
fn signature_types_mut(function: &mut CreateFunction) -> impl Iterator<Item = &mut DataType> {
    let (columns, result) = match &mut function.return_type {
        Some(FunctionReturnType::DataType(DataType::Table(Some(columns)))) => {
            (Some(columns.iter_mut()), None)
        }
        Some(FunctionReturnType::DataType(data_type) | FunctionReturnType::SetOf(data_type)) => {
            (None, Some(data_type))
        }
        None => (None, None),
    };
    function
        .args
        .iter_mut()
        .flatten()
        .map(|argument| &mut argument.data_type)
        .chain(columns.into_iter().flatten().map(|column| &mut column.data_type))
        .chain(result)
}

/// Binds every bare type name the signature of `function` declares to the
/// type the session's path reaches now.
pub(super) fn bind_signature(builder: &ParserDBBuilder, function: &mut CreateFunction) {
    for data_type in signature_types_mut(function) {
        bind_signature_type(builder, data_type);
    }
}

/// Binds the argument types a statement naming a function lists, the way
/// the function's own signature was bound, so the two compare alike.
pub(super) fn bind_arguments(builder: &ParserDBBuilder, arguments: &mut [OperateFunctionArg]) {
    for argument in arguments {
        bind_signature_type(builder, &mut argument.data_type);
    }
}

/// Rewrites with `rewrite` every type name of every stored signature that
/// `names` accepts, re-pointing what holds each rewritten function.
pub(super) fn retarget_signatures(
    builder: &mut ParserDBBuilder,
    names: impl Fn(&ObjectName) -> bool,
    rewrite: impl Fn(&mut ObjectName),
) {
    let positions: Vec<usize> = builder
        .functions()
        .iter()
        .enumerate()
        .filter(|(_, (function, _))| signature_types(function).filter_map(custom_name).any(&names))
        .map(|(position, _)| position)
        .collect();
    for position in positions {
        edit_function(builder, position, |function| {
            for name in signature_types_mut(function).filter_map(custom_name_mut) {
                if names(name) {
                    rewrite(name);
                }
            }
        });
    }
}

/// Points every signature naming the row type of the relation `key` names at
/// `to`, the relation's name once renamed or moved.
pub(super) fn follow_relation_rename(
    builder: &mut ParserDBBuilder,
    key: &RelationKey,
    to: &ObjectName,
) {
    retarget_signatures(
        builder,
        |name| bound_read_key(None, name).as_ref() == Some(key),
        |name| name.clone_from(to),
    );
}

/// The relations whose row types the signature of `function` names, keyed
/// as a body's reads are.
pub(super) fn row_type_keys(function: &CreateFunction) -> impl Iterator<Item = RelationKey> {
    signature_types(function).filter_map(custom_name).filter_map(|name| bound_read_key(None, name))
}

/// The stored functions whose signature names a domain, a user-defined type
/// or the row type of a relation that `schema` holds, which `DROP SCHEMA ...
/// CASCADE` takes wherever they live.
pub(super) fn functions_naming_schema_types(
    builder: &ParserDBBuilder,
    schema: (&str, bool),
) -> Vec<Arc<CreateFunction>> {
    let case = builder.identifier_case();
    let held = |name: &ObjectName| {
        schema_qualifiers_match(qualifier_of(name).named(), Some(schema), case)
            && object_name_last_identifier(name).is_some_and(|ident| {
                reach_in_schema(builder, ident, schema)
                    .is_some_and(|reached| !matches!(reached, Reached::Catalog))
            })
    };
    builder
        .functions()
        .iter()
        .filter(|(function, _)| signature_types(function).filter_map(custom_name).any(&held))
        .map(|(function, _)| Arc::clone(function))
        .collect()
}
