//! The constructor functions `CREATE TYPE ... AS RANGE` creates with the
//! range, and the internal dependency that ties them to it.
//!
//! Every rule here was measured against PostgreSQL 14 and 18.6 in Docker.
//!
//! PostgreSQL creates five functions in the range's schema, even when the
//! multirange lives in another. `r(subtype, subtype)` and
//! `r(subtype, subtype, text)` build the range, and `m()`, `m(r)` and
//! `m(VARIADIC r[])` build its multirange `m`. All are `LANGUAGE internal`,
//! immutable and parallel safe, the range ones not strict and the multirange
//! ones strict, and all are owned by the bootstrap superuser under its default
//! privileges. The input never names that role, so the constructors record no
//! owner and take the `EXECUTE` defaults of the role the input leaves unnamed.
//!
//! `DROP FUNCTION` refuses a constructor, which goes only with its range,
//! and a renamed range or multirange keeps the constructors' names. `CREATE OR
//! REPLACE FUNCTION` turns a constructor into an ordinary function, dropping
//! the dependency.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use sqlparser::{
    ast::{
        ArgMode, ArrayElemTypeDef, CreateFunction, CreateFunctionBody, DataType, Expr,
        FunctionParallel, Ident, ObjectName, OperateFunctionArg, Value, ValueWithSpan,
    },
    tokenizer::Span,
};

use super::{
    Creator, DefaultPrivileges, ParserDBBuilder, function_signatures_match,
    object_name_last_identifier,
    postgres_builtins::{Volatility, server_function},
    user_defined_types::rewrite_last_ident,
};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, bound_user_type},
    structs::{UserDefinedType, metadata::RangeConstructor},
    traits::UserDefinedTypeLike,
    utils::{last_str, object_name::qualifier_of},
};

/// A type named by a stored name.
fn named_type(name: &ObjectName) -> DataType {
    DataType::Custom(name.clone(), Vec::new())
}

/// A parameter without a name.
fn parameter(mode: Option<ArgMode>, data_type: DataType) -> OperateFunctionArg {
    OperateFunctionArg { mode, name: None, data_type, default_expr: None }
}

/// The declaration of one constructor, calling the internal function
/// `symbol`, strict exactly when it builds the multirange.
fn declaration(
    name: &ObjectName,
    args: Vec<OperateFunctionArg>,
    returns: &DataType,
    multirange: bool,
    symbol: &str,
) -> CreateFunction {
    let mut function = server_function(
        name.clone(),
        args,
        returns.clone(),
        multirange,
        Volatility::Immutable,
        "internal",
    );
    function.function_body = Some(CreateFunctionBody::AsBeforeOptions {
        body: Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(symbol.to_string()),
            span: Span::empty(),
        }),
        link_symbol: None,
    });
    function.parallel = Some(FunctionParallel::Safe);
    function
}

/// The five constructors of `range`, each with whether it builds the
/// multirange, or none for a shell or a type that is no range.
fn constructors_of(range: &UserDefinedType) -> Vec<(CreateFunction, bool)> {
    let (Some(subtype), Some((multirange, multirange_ident))) = (
        range.range_subtype(),
        range
            .multirange_name()
            .and_then(|multirange| Some((multirange, object_name_last_identifier(multirange)?))),
    ) else {
        return Vec::new();
    };
    let range_type = named_type(&range.name);
    let multirange_type = named_type(multirange);
    let mut multirange_function = range.name.clone();
    rewrite_last_ident(&mut multirange_function, multirange_ident);
    let input = |data_type: &DataType| parameter(None, data_type.clone());
    let variadic = parameter(
        Some(ArgMode::Variadic),
        DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(range_type.clone()), None)),
    );
    let range_constructor =
        |args, symbol| (declaration(&range.name, args, &range_type, false, symbol), false);
    let multirange_constructor = |args, symbol| {
        (declaration(&multirange_function, args, &multirange_type, true, symbol), true)
    };
    vec![
        range_constructor(vec![input(subtype), input(subtype)], "range_constructor2"),
        range_constructor(
            vec![input(subtype), input(subtype), input(&DataType::Text)],
            "range_constructor3",
        ),
        multirange_constructor(Vec::new(), "multirange_constructor0"),
        multirange_constructor(vec![input(&range_type)], "multirange_constructor1"),
        multirange_constructor(vec![variadic], "multirange_constructor2"),
    ]
}

/// Records the constructors of `range` under the PostgreSQL dialect, each
/// bound to the range, with the `EXECUTE` the defaults of the role the input
/// leaves unnamed give.
///
/// # Errors
///
/// Returns [`Error::FunctionAlreadyExists`] when the range's schema already
/// holds a function of a constructor's signature, which PostgreSQL refuses as
/// well.
pub(super) fn create_range_constructors(
    mut builder: ParserDBBuilder,
    range: &UserDefinedType,
    defaults: &DefaultPrivileges,
) -> Result<ParserDBBuilder, Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Ok(builder);
    }
    let case = builder.identifier_case();
    let creator = Creator::new(None, defaults);
    let schema = qualifier_of(&range.name).named();
    for (function, multirange) in constructors_of(range) {
        let taken = builder.functions().iter().any(|(existing, _)| {
            function_signatures_match(
                &existing.name,
                existing.args.as_deref(),
                &function.name,
                function.args.as_deref(),
                case,
            )
        });
        if taken {
            return Err(Error::FunctionAlreadyExists {
                function_name: last_str(&function.name).to_string(),
            });
        }
        let mut metadata = creator.function_metadata(schema, case);
        metadata.set_range_constructor(Some(RangeConstructor {
            range: range.name.clone(),
            multirange,
        }));
        builder = builder.add_function(Arc::new(function), metadata);
    }
    Ok(builder)
}

/// Whether `constructor` belongs to `range`.
fn constructs(
    constructor: &RangeConstructor,
    range: &UserDefinedType,
    case: crate::structs::IdentifierCase,
) -> bool {
    bound_user_type(core::iter::once(range), &constructor.range, case)
        .is_some_and(|(_, multirange)| !multirange)
}

/// The name of the type that requires the constructor `constructor`: its
/// range, or the range's multirange for a multirange constructor.
pub(super) fn requiring_type(
    builder: &ParserDBBuilder,
    constructor: &RangeConstructor,
) -> Option<String> {
    let case = builder.identifier_case();
    let range = builder
        .user_defined_types()
        .iter()
        .map(|(type_, _)| type_.as_ref())
        .find(|range| constructs(constructor, range, case))?;
    let multirange = constructor
        .multirange
        .then(|| range.multirange_name().and_then(object_name_last_identifier))
        .flatten();
    Some(multirange.map_or_else(|| range.name().to_string(), |multirange| multirange.value.clone()))
}

/// Drops the constructors of the ranges at the positions `doomed` accepts,
/// which PostgreSQL drops with the range whatever the statement says about
/// `CASCADE`.
///
/// # Errors
///
/// Returns [`Error::FunctionReferenced`] when something still calls one, as
/// dropping the function itself does.
pub(super) fn drop_range_constructors(
    builder: &mut ParserDBBuilder,
    doomed: impl Fn(usize) -> bool,
) -> Result<(), Error> {
    let case = builder.identifier_case();
    let ranges: Vec<&UserDefinedType> = builder
        .user_defined_types()
        .iter()
        .enumerate()
        .filter(|(position, _)| doomed(*position))
        .map(|(_, (type_, _))| type_.as_ref())
        .collect();
    let dropping: Vec<Arc<CreateFunction>> = builder
        .functions()
        .iter()
        .filter(|(_, metadata)| {
            metadata.range_constructor().is_some_and(|constructor| {
                ranges.iter().any(|range| constructs(constructor, range, case))
            })
        })
        .map(|(function, _)| Arc::clone(function))
        .collect();
    if dropping.is_empty() {
        return Ok(());
    }
    if let Some(called) =
        dropping.iter().find(|function| builder.is_function_used(function, &dropping))
    {
        return Err(Error::FunctionReferenced {
            function_name: last_str(&called.name).to_string(),
        });
    }
    builder
        .functions_mut()
        .retain(|(function, _)| !dropping.iter().any(|dropped| Arc::ptr_eq(dropped, function)));
    Ok(())
}

/// Points the constructors of the range at `position` at the name `new_name`
/// a rename gives it, their own names staying as they were.
pub(super) fn follow_range_rename(
    builder: &mut ParserDBBuilder,
    position: usize,
    new_name: &Ident,
) {
    let case = builder.identifier_case();
    let range = builder.user_defined_types()[position].0.as_ref();
    let renamed: Vec<usize> = builder
        .functions()
        .iter()
        .enumerate()
        .filter(|(_, (_, metadata))| {
            metadata
                .range_constructor()
                .is_some_and(|constructor| constructs(constructor, range, case))
        })
        .map(|(index, _)| index)
        .collect();
    for index in renamed {
        if let Some(constructor) = builder.functions_mut()[index].1.range_constructor_mut() {
            rewrite_last_ident(&mut constructor.range, new_name);
        }
    }
}
