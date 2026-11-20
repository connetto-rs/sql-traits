//! The columns of the result a function called in `FROM` returns, built from
//! the function's declaration as PostgreSQL's `addRangeTableEntryForFunction`
//! builds them.

use alloc::{
    borrow::{Cow, ToOwned},
    format,
    string::String,
    vec,
    vec::Vec,
};
use core::marker::PhantomData;

use sqlparser::ast::{ArgMode, DataType, FunctionArg, TableAlias, TableFactor};

use super::{
    AstRef, DerivationProfile, DerivedColumn, DerivedRelationRef, DerivedShape, Deriving,
    DerivingView, FactorContribution, RelationContribution, alias_key, apply_alias_columns,
    derive_view_shape, derived_column_names, factor_address, factor_alias, factor_name,
    object_name_key,
};
use crate::{
    errors::LookupError,
    structs::{FunctionKind, IdentifierCase, TargetName, TypeKind, WrittenCall},
    traits::{
        ColumnLike, DatabaseLike, FunctionLike, Relation, TableLike, UserDefinedTypeKind,
        UserDefinedTypeLike,
    },
    utils::{object_name::type_search_path, postgres_types::PgType},
};

/// The schema a user-defined type stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

/// What declares the type of a column of the result a function called in
/// `FROM` returns, the graph's form of
/// [`FunctionResultType`](crate::structs::FunctionResultType), its arguments
/// read under a scope of cursor type `C`.
pub(super) enum ResultColumnType<'query, 'db, DB: DatabaseLike, D, C> {
    /// A type the function's signature names for a column among two or more
    /// outputs.
    Signature(Cow<'db, str>),
    /// The type the function's signature names for its only output, with
    /// the arguments the call writes and the scope they are read in.
    Scalar { name: Cow<'db, str>, arguments: AstRef<'query, 'db, [FunctionArg]>, scope: C },
    /// A column of the relation whose row type the function returns, by the
    /// column's definition.
    RowType(D),
    /// An attribute of the composite type the function returns, by its
    /// declared type, with the database holding the type.
    Attribute(&'db DB::DataType, &'db DB),
    /// A type the column definition list of the call declares.
    DefinitionList(AstRef<'query, 'db, DataType>),
}

/// One column of a function's result: its name, whether the name is quoted,
/// and what declares its type.
type ResultColumn<'query, 'db, DB, D, C> = (String, bool, ResultColumnType<'query, 'db, DB, D, C>);

/// The columns of a function's result, `None` when they are not known.
type ResultColumns<'query, 'db, DB, D, C> = Option<Vec<ResultColumn<'query, 'db, DB, D, C>>>;

/// What `factor`, a `FROM` item written as a call, contributes when the call
/// reaches one function of the database whose result PostgreSQL describes
/// without running it: the columns of that result, then the `bigint` column
/// `WITH ORDINALITY` adds, renamed by the alias's column list. The call's
/// arguments are read under `local_parent`.
///
/// `None` for any other item, and for a call that reaches no function or
/// several, one of a procedure, an aggregate or a window function, a
/// polymorphic result, a `record` result without a column definition list,
/// a column definition list PostgreSQL refuses, and an alias naming more
/// columns than the result has.
pub(super) fn collect_function_call<'query, 'db, DB, P>(
    factor: AstRef<'query, 'db, TableFactor>,
    deriving: Deriving<'_, 'db, DB>,
    local_parent: P::Location,
    profile: &mut P,
) -> Result<Option<FactorContribution<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let Some(arguments) = factor.try_map(|factor| {
        match factor {
            TableFactor::Table { args: Some(args), .. } => Some(args.args.as_slice()),
            TableFactor::Function { args, .. } => Some(args.as_slice()),
            _ => None,
        }
    }) else {
        return Ok(None);
    };
    let with_ordinality = matches!(
        factor.get(),
        TableFactor::Table { with_ordinality: true, .. }
            | TableFactor::Function { with_ordinality: true, .. }
    );
    let Some(function) = reached_function(factor.get(), deriving.database, deriving.case)? else {
        return Ok(None);
    };
    let call = Call { arguments, scope: local_parent };
    let alias = factor_alias(factor);
    let Some(columns) = result_columns(function, call, alias, with_ordinality, deriving, profile)?
    else {
        return Ok(None);
    };
    let mut derived = Vec::with_capacity(columns.len() + usize::from(with_ordinality));
    for (name, quoted, data_type) in columns {
        let definition = profile.function_result_definition(function, data_type);
        derived.push(DerivedColumn { name, quoted, source: None, definition, marker: PhantomData });
    }
    if with_ordinality {
        derived.push(DerivedColumn {
            name: "ordinality".to_owned(),
            quoted: true,
            source: None,
            definition: profile.ordinality_definition(),
            marker: PhantomData,
        });
    }
    let shape = DerivedShape { columns: derived, row_preserving: false };
    let Some(shape) = (match alias {
        Some(alias) => apply_alias_columns(Some(shape), alias.get()),
        None => Some(shape),
    }) else {
        return Ok(None);
    };
    let names = derived_column_names(&shape.columns);
    let key = alias.map(alias_key).or_else(|| factor_name(factor).and_then(object_name_key));
    Ok(Some(FactorContribution {
        relation: RelationContribution::Derived(DerivedRelationRef {
            key,
            identity: key,
            shape,
            nullable: false,
            entry_index: 0,
            factor: factor_address(factor.get()),
        }),
        names: Some(names),
    }))
}

/// The arguments a call in `FROM` writes and the scope they are read in.
#[derive(Clone, Copy)]
struct Call<'query, 'db, C> {
    arguments: AstRef<'query, 'db, [FunctionArg]>,
    scope: C,
}

/// The one function of `database` the call `factor` writes reaches under
/// `case`, or `None` when it reaches none or several, when the called name
/// has more than two parts or a part built at run time, and for a procedure,
/// an aggregate or a window function, which PostgreSQL refuses in `FROM`.
///
/// # Errors
///
/// Returns the lookup errors reading the reached function's kind returns.
fn reached_function<'db, DB: DatabaseLike>(
    factor: &TableFactor,
    database: &'db DB,
    case: IdentifierCase,
) -> Result<Option<&'db DB::Function>, LookupError> {
    let Some(call) = WrittenCall::relation(factor) else { return Ok(None) };
    let mut reached = match database.functions_reached_by(call, case) {
        Err(LookupError::InvalidObjectName { .. }) => return Ok(None),
        reached => reached?,
    };
    let (Some(function), None) = (reached.next(), reached.next()) else { return Ok(None) };
    Ok((function.kind(database)? == FunctionKind::Normal).then_some(function))
}

/// The columns of the result `function` returns when called in `FROM` as
/// `call`, named as PostgreSQL's `chooseScalarFunctionAlias` and
/// `build_function_result_tupdesc_d` name them, the column definition list
/// of `alias` declaring those of a `record` result.
///
/// A function's output arguments, its `RETURNS TABLE` columns among them,
/// name its columns, an unnamed one taking `column` and its position among
/// the outputs. A single unnamed output and a scalar result take the alias,
/// or else the function's name. The column of a function of one output
/// carries `call`, from which PostgreSQL derives the column's collation.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving the row type the
/// function returns, or deriving that of a view, runs into.
fn result_columns<'query, 'db, DB, P>(
    function: &'db DB::Function,
    call: Call<'query, 'db, P::Location>,
    alias: Option<AstRef<'query, 'db, TableAlias>>,
    with_ordinality: bool,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Result<ResultColumns<'query, 'db, DB, P::Definition, P::Location>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let database = deriving.database;
    let listed =
        alias.filter(|alias| alias.get().columns.iter().any(|column| column.data_type.is_some()));
    let scalar_name = || {
        alias.map_or_else(
            || (function.stored_name().into_owned(), true),
            |alias| {
                let name = &alias.get().name;
                (name.value.clone(), name.quote_style.is_some())
            },
        )
    };
    let scalar =
        |name| ResultColumnType::Scalar { name, arguments: call.arguments, scope: call.scope };
    let outputs: Vec<(Option<TargetName<'db>>, Cow<'db, str>)> = function
        .argument_modes()
        .zip(function.argument_names(database))
        .zip(function.argument_type_names(database))
        .filter(|((mode, _), _)| matches!(mode, ArgMode::Out | ArgMode::InOut))
        .map(|((_, name), data_type)| (name, data_type))
        .chain(function.returned_columns(database).map(|(name, data_type)| (Some(name), data_type)))
        .collect();
    if !outputs.is_empty() {
        if listed.is_some() || outputs.iter().any(|(_, data_type)| is_polymorphic(data_type)) {
            return Ok(None);
        }
        let single = outputs.len() == 1;
        let columns = outputs.into_iter().enumerate().map(|(position, (name, data_type))| {
            let (name, quoted) = match name {
                Some(name) => (name.name().to_owned(), name.name_is_quoted()),
                None if single => scalar_name(),
                None => (format!("column{}", position + 1), true),
            };
            let data_type =
                if single { scalar(data_type) } else { ResultColumnType::Signature(data_type) };
            (name, quoted, data_type)
        });
        return Ok(Some(columns.collect()));
    }
    let Some(result) = function.result_type_name(database) else { return Ok(None) };
    let Some(declared) = PgType::parse(&result) else { return Ok(None) };
    if declared.polymorphic().is_some() {
        return Ok(None);
    }
    if declared == PgType::RECORD {
        // PostgreSQL refuses a column definition list beside `WITH
        // ORDINALITY` outside `ROWS FROM`.
        return Ok(listed.filter(|_| !with_ordinality).and_then(definition_list));
    }
    if listed.is_some() {
        return Ok(None);
    }
    match result_shape(declared, database) {
        ResultShape::Row(row_type) => row_type_columns(&row_type, deriving, profile),
        ResultShape::Scalar => {
            let (name, quoted) = scalar_name();
            Ok(Some(vec![(name, quoted, scalar(result))]))
        }
        ResultShape::Unknown => Ok(None),
    }
}

/// Whether the type `name` names is a polymorphic pseudo-type, which takes
/// the type of the arguments of each call.
fn is_polymorphic(name: &str) -> bool {
    PgType::parse(name).is_some_and(|declared| declared.polymorphic().is_some())
}

/// The columns the column definition list of `alias` declares, or `None`
/// when one of its columns declares no type.
fn definition_list<'query, 'db, DB: DatabaseLike, D, C>(
    alias: AstRef<'query, 'db, TableAlias>,
) -> ResultColumns<'query, 'db, DB, D, C> {
    (0..alias.get().columns.len())
        .map(|position| {
            let column = alias.map(|alias| &alias.columns[position]);
            let data_type = column.try_map(|column| column.data_type.as_ref())?;
            let name = &column.get().name;
            Some((
                name.value.clone(),
                name.quote_style.is_some(),
                ResultColumnType::DefinitionList(data_type),
            ))
        })
        .collect()
}

/// What a function's declared result type is, as far as the database
/// describes it.
enum ResultShape {
    /// A type whose values are not rows, which a call in `FROM` outputs as
    /// one column.
    Scalar,
    /// The row type of a relation or a composite type, or a domain over one,
    /// by the composite type's name.
    Row(TargetName<'static>),
    /// A type the database does not describe, which may be either.
    Unknown,
}

/// What `declared`, a function's declared result type, is in `database`.
fn result_shape<DB: DatabaseLike>(declared: PgType, database: &DB) -> ResultShape {
    let PgType::Named(name) = declared else { return ResultShape::Scalar };
    let Some(facts) = database.type_facts(name.clone()) else { return ResultShape::Unknown };
    match facts.kind() {
        Some(TypeKind::Composite) => ResultShape::Row(name),
        Some(TypeKind::Domain) => {
            facts.into_domain().map_or(ResultShape::Unknown, |domain| {
                result_shape(PgType::from_target(&domain.into_base()), database)
            })
        }
        Some(TypeKind::Base | TypeKind::Enum | TypeKind::Range | TypeKind::Multirange) => {
            ResultShape::Scalar
        }
        Some(TypeKind::Pseudo) | None => ResultShape::Unknown,
    }
}

/// The columns of the row type `name` names, the first schema of the type
/// search path holding it answering an unqualified name: those of a table,
/// a view or a materialized view, or the attributes of a composite type.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving the relation, or
/// deriving the shape of a view, runs into.
fn row_type_columns<'query, 'db, DB, P>(
    name: &TargetName<'_>,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Result<ResultColumns<'query, 'db, DB, P::Definition, P::Location>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let database = deriving.database;
    let schemas: Vec<(&str, bool)> = match name.schema() {
        Some(schema) => vec![(schema, name.schema_is_quoted())],
        None => type_search_path(|| database.search_path()).collect(),
    };
    for (schema, schema_quoted) in schemas {
        let target =
            TargetName::new(name.name(), name.name_is_quoted()).with_schema(schema, schema_quoted);
        if let Some(relation) = database.resolve_target_relation(target.clone(), deriving.case)? {
            return relation_columns(relation, deriving, profile);
        }
        if let Some(attributes) = database
            .user_defined_types()
            .find(|type_| {
                type_.kind() == UserDefinedTypeKind::Composite
                    && holds_type(*type_, &target, deriving.case)
            })
            .and_then(UserDefinedTypeLike::attributes)
        {
            let columns = attributes.map(|(name, data_type)| {
                let data_type = ResultColumnType::Attribute(data_type, database);
                (name.name().to_owned(), name.name_is_quoted(), data_type)
            });
            return Ok(Some(columns.collect()));
        }
    }
    Ok(None)
}

/// Whether `type_` is the type the schema-qualified `target` names, under
/// `case`.
fn holds_type<T: UserDefinedTypeLike>(
    type_: &T,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> bool {
    let (schema, schema_quoted) =
        type_.schema().map_or((DEFAULT_SCHEMA, false), |schema| (schema, type_.schema_is_quoted()));
    case.identifiers_match(
        type_.name(),
        type_.name_is_quoted(),
        target.name(),
        target.name_is_quoted(),
    ) && target.schema().is_some_and(|wanted| {
        case.identifiers_match(schema, schema_quoted, wanted, target.schema_is_quoted())
    })
}

/// The columns of the row type of `relation`, `None` for a sequence and for
/// a view whose columns cannot be enumerated.
///
/// # Errors
///
/// Returns the lookup errors listing a table's columns or deriving the shape
/// of a view runs into.
fn relation_columns<'query, 'db, DB, P>(
    relation: Relation<'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Result<ResultColumns<'query, 'db, DB, P::Definition, P::Location>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let database = deriving.database;
    let (view, row_preserving) = match relation {
        Relation::Table(table) | Relation::ForeignTable(table) => {
            let columns = table.columns(database)?.map(|column| {
                let definition = profile.column_definition(table, column, database);
                let data_type = ResultColumnType::RowType(definition);
                (column.column_name().to_owned(), column.column_name_is_quoted(), data_type)
            });
            return Ok(Some(columns.collect()));
        }
        Relation::View(view) => (DerivingView::Plain(view), true),
        Relation::MaterializedView(view) => (DerivingView::Materialized(view), false),
        Relation::Sequence(_) => return Ok(None),
    };
    let Some(shape) = derive_view_shape(view, row_preserving, deriving, profile)? else {
        return Ok(None);
    };
    Ok(Some(
        shape
            .columns
            .into_iter()
            .map(|column| {
                (column.name, column.quoted, ResultColumnType::RowType(column.definition))
            })
            .collect(),
    ))
}
