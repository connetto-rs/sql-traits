//! The functions PostgreSQL 18 provides in `pg_catalog`, which a schema calls
//! without declaring them.

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::ops::Range;

use once_cell::race::OnceBox;
use sqlparser::{
    ast::{
        CreateFunction, CreateFunctionBody, DataType, Expr, FunctionBehavior, FunctionCalledOnNull,
        FunctionReturnType, Ident, ObjectName, ObjectNamePart, OperateFunctionArg, Statement,
        Value, ValueWithSpan,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
    tokenizer::Span,
};

use super::postgres_18_functions::POSTGRES_18_FUNCTIONS;
use crate::{
    structs::{
        AggregateKind, FunctionKind,
        generic_db::{Stored, builtin_functions::BuiltinTable},
        metadata::FunctionMetadata,
    },
    utils::{null_returning_positions, object_name::PG_CATALOG},
};

/// `pg_proc.prokind`, with `pg_aggregate.aggkind` and
/// `pg_aggregate.aggnumdirectargs` for an aggregate whose arguments split
/// between direct ones and those `WITHIN GROUP` orders by.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Normal,
    Window,
    Aggregate,
    OrderedSet(usize),
    HypotheticalSet(usize),
}

impl Kind {
    const fn function(self) -> FunctionKind {
        match self {
            Self::Normal => FunctionKind::Normal,
            Self::Window => FunctionKind::Window,
            Self::Aggregate | Self::OrderedSet(_) | Self::HypotheticalSet(_) => {
                FunctionKind::Aggregate
            }
        }
    }

    const fn aggregate(self) -> Option<AggregateKind> {
        match self {
            Self::Normal | Self::Window => None,
            Self::Aggregate => Some(AggregateKind::Normal),
            Self::OrderedSet(direct_arguments) => {
                Some(AggregateKind::OrderedSet { direct_arguments })
            }
            Self::HypotheticalSet(direct_arguments) => {
                Some(AggregateKind::HypotheticalSet { direct_arguments })
            }
        }
    }
}

/// `pg_proc.proisstrict`.
#[derive(Clone, Copy)]
pub(super) enum NullInput {
    /// Any `NULL` argument makes the call `NULL`.
    Strict,
    /// The function runs on `NULL` arguments.
    CalledOnNull,
}

/// `pg_proc.provolatile`.
#[derive(Clone, Copy)]
pub(super) enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

impl Volatility {
    fn behavior(self) -> FunctionBehavior {
        match self {
            Self::Immutable => FunctionBehavior::Immutable,
            Self::Stable => FunctionBehavior::Stable,
            Self::Volatile => FunctionBehavior::Volatile,
        }
    }
}

/// `pg_proc.prolang`, among the languages `pg_catalog` functions are written
/// in.
#[derive(Clone, Copy)]
pub(super) enum Language {
    Internal,
    C,
    Sql,
}

impl Language {
    fn name(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::C => "c",
            Self::Sql => "sql",
        }
    }
}

/// One `pg_proc` row: the function's name, its parameters as
/// `pg_get_function_arguments` spells them, its result as
/// `pg_get_function_result` does, its strictness, volatility, kind and
/// language.
///
/// A tuple struct, so a row is built by its constructor rather than by a
/// function only const evaluation ever runs.
#[derive(Clone, Copy)]
pub(super) struct PostgresBuiltin(
    pub(super) &'static str,
    pub(super) &'static str,
    pub(super) &'static str,
    pub(super) NullInput,
    pub(super) Volatility,
    pub(super) Kind,
    pub(super) Language,
);

impl PostgresBuiltin {
    /// The name `pg_proc` stores.
    fn name(&self) -> &'static str {
        self.0
    }

    /// The declaration in `pg_catalog`, so a user function may shadow it the
    /// way the server allows.
    fn function(&self) -> Option<CreateFunction> {
        let Self(name, arguments, result, null_input, volatility, _, language) = *self;
        let name = ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new(PG_CATALOG)),
            ObjectNamePart::Identifier(stored_ident(name)),
        ]);
        declaration(
            name,
            arguments,
            result,
            matches!(null_input, NullInput::Strict),
            volatility,
            language,
        )
    }

    fn metadata(&self) -> FunctionMetadata {
        let Self(name, arguments, _, _, _, kind, _) = *self;
        let mut metadata = FunctionMetadata::default();
        if let Some(positions) =
            null_returning_positions(name, |types| arguments.split(", ").eq(types.iter().copied()))
        {
            metadata.set_null_returning_arguments(positions);
        }
        metadata.set_kind(kind.function());
        metadata.set_aggregate_kind(kind.aggregate());
        metadata
    }
}

/// An identifier holding `name` as `pg_proc` stores it, quoted when folding
/// would change it.
fn stored_ident(name: &str) -> Ident {
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Ident::with_quote('"', name)
    } else {
        Ident::new(name)
    }
}

/// The declaration of a function the server provides, whose body the input
/// never sees, from its parameter list and result as PostgreSQL prints them.
///
/// [`None`] when sqlparser does not read that list or result.
pub(super) fn declaration(
    name: ObjectName,
    arguments: &str,
    result: &str,
    strict: bool,
    volatility: Volatility,
    language: Language,
) -> Option<CreateFunction> {
    let sql = format!(
        "CREATE FUNCTION f({arguments}) RETURNS {result} LANGUAGE {} AS ''",
        language.name()
    );
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, &sql).ok()?;
    let Some(Statement::CreateFunction(mut function)) = statements.pop() else { return None };
    function.name = name;
    function.behavior = Some(volatility.behavior());
    function.called_on_null = strict.then_some(FunctionCalledOnNull::Strict);
    Some(function)
}

/// The declaration of a function the server provides returning `returns`,
/// with an empty body.
pub(super) fn server_function(
    name: ObjectName,
    args: Vec<OperateFunctionArg>,
    returns: DataType,
    strict: bool,
    volatility: Volatility,
    language: &str,
) -> CreateFunction {
    CreateFunction {
        or_alter: false,
        or_replace: false,
        temporary: false,
        if_not_exists: false,
        name,
        args: Some(args),
        return_type: Some(FunctionReturnType::DataType(returns)),
        function_body: Some(CreateFunctionBody::AsBeforeOptions {
            body: Expr::Value(ValueWithSpan {
                value: Value::SingleQuotedString(String::new()),
                span: Span::empty(),
            }),
            link_symbol: None,
        }),
        behavior: Some(volatility.behavior()),
        called_on_null: strict.then_some(FunctionCalledOnNull::Strict),
        parallel: None,
        using: None,
        language: Some(Ident::new(language)),
        determinism_specifier: None,
        options: None,
        remote_connection: None,
        security: None,
        leakproof: None,
        window: false,
        cost: None,
        rows: None,
        support: None,
        set_params: vec![],
    }
}

/// [`POSTGRES_18_FUNCTIONS`], each row declared the first time a database
/// reaches it.
pub(super) struct Postgres18Functions;

/// The one table of PostgreSQL 18's builtins every database shares.
pub(super) static POSTGRES_18_BUILTINS: Postgres18Functions = Postgres18Functions;

/// The declaration of each row of [`POSTGRES_18_FUNCTIONS`], set once.
static DECLARED: [OnceBox<Option<Stored<CreateFunction>>>; POSTGRES_18_FUNCTIONS.len()] =
    [const { OnceBox::new() }; POSTGRES_18_FUNCTIONS.len()];

/// `name` folded as PostgreSQL folds an unquoted identifier, byte by byte.
fn folded_bytes(name: &str) -> impl Iterator<Item = u8> + '_ {
    name.bytes().map(|byte| byte.to_ascii_lowercase())
}

impl BuiltinTable<CreateFunction> for Postgres18Functions {
    fn schema(&self) -> &'static str {
        PG_CATALOG
    }

    fn len(&self) -> usize {
        POSTGRES_18_FUNCTIONS.len()
    }

    fn rows_folding_to(&self, folded: &str) -> Range<usize> {
        let start = POSTGRES_18_FUNCTIONS
            .partition_point(|row| folded_bytes(row.name()).lt(folded.bytes()));
        let end = POSTGRES_18_FUNCTIONS
            .partition_point(|row| folded_bytes(row.name()).le(folded.bytes()));
        start..end
    }

    fn name(&self, row: usize) -> &'static str {
        POSTGRES_18_FUNCTIONS.get(row).map_or("", PostgresBuiltin::name)
    }

    fn function(&self, row: usize) -> Option<&'static Stored<CreateFunction>> {
        let builtin = POSTGRES_18_FUNCTIONS.get(row)?;
        DECLARED
            .get(row)?
            .get_or_init(|| {
                Box::new(
                    builtin.function().map(|function| (Arc::new(function), builtin.metadata())),
                )
            })
            .as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::{POSTGRES_18_BUILTINS, POSTGRES_18_FUNCTIONS, PostgresBuiltin, folded_bytes};
    use crate::structs::generic_db::builtin_functions::BuiltinTable;

    #[test]
    fn every_row_declares_its_function() {
        for (row, PostgresBuiltin(name, arguments, result, ..)) in
            POSTGRES_18_FUNCTIONS.iter().enumerate()
        {
            let declared = POSTGRES_18_BUILTINS.function(row).is_some();
            assert!(declared, "sqlparser cannot declare {name}({arguments}) {result}");
        }
    }

    /// A lookup reads the rows of one folded name, and within them those of
    /// one stored name, as adjacent runs.
    #[test]
    fn rows_are_sorted_by_folded_then_stored_name() {
        assert!(POSTGRES_18_FUNCTIONS.windows(2).all(|pair| {
            folded_bytes(pair[0].name())
                .cmp(folded_bytes(pair[1].name()))
                .then_with(|| pair[0].name().cmp(pair[1].name()))
                .is_le()
        }));
    }
}
