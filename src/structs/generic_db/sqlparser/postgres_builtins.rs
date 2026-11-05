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
        FunctionKind,
        generic_db::{Stored, builtin_functions::BuiltinTable},
        metadata::FunctionMetadata,
    },
    utils::{null_returning_positions, object_name::PG_CATALOG},
};

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
pub(super) struct PostgresBuiltin {
    name: &'static str,
    arguments: &'static str,
    result: &'static str,
    null_input: NullInput,
    volatility: Volatility,
    kind: FunctionKind,
    language: Language,
}

/// A row of [`POSTGRES_18_FUNCTIONS`].
pub(super) const fn builtin(
    name: &'static str,
    arguments: &'static str,
    result: &'static str,
    null_input: NullInput,
    volatility: Volatility,
    kind: FunctionKind,
    language: Language,
) -> PostgresBuiltin {
    PostgresBuiltin { name, arguments, result, null_input, volatility, kind, language }
}

impl PostgresBuiltin {
    /// The declaration in `pg_catalog`, so a user function may shadow it the
    /// way the server allows.
    fn function(&self) -> Option<CreateFunction> {
        let name = ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new(PG_CATALOG)),
            ObjectNamePart::Identifier(stored_ident(self.name)),
        ]);
        declaration(
            name,
            self.arguments,
            self.result,
            matches!(self.null_input, NullInput::Strict),
            self.volatility,
            self.language,
        )
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut metadata = FunctionMetadata::default();
        if let Some(positions) = null_returning_positions(self.name, |types| {
            self.arguments.split(", ").eq(types.iter().copied())
        }) {
            metadata.set_null_returning_arguments(positions);
        }
        metadata.set_kind(self.kind);
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
        let start =
            POSTGRES_18_FUNCTIONS.partition_point(|row| folded_bytes(row.name).lt(folded.bytes()));
        let end =
            POSTGRES_18_FUNCTIONS.partition_point(|row| folded_bytes(row.name).le(folded.bytes()));
        start..end
    }

    fn name(&self, row: usize) -> &'static str {
        POSTGRES_18_FUNCTIONS.get(row).map_or("", |row| row.name)
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
    use alloc::vec::Vec;

    use super::{POSTGRES_18_BUILTINS, POSTGRES_18_FUNCTIONS, folded_bytes};
    use crate::structs::generic_db::builtin_functions::BuiltinTable;

    #[test]
    fn every_row_declares_its_function() {
        let undeclared: Vec<_> = (0..POSTGRES_18_FUNCTIONS.len())
            .filter(|row| POSTGRES_18_BUILTINS.function(*row).is_none())
            .map(|row| {
                let builtin = &POSTGRES_18_FUNCTIONS[row];
                (builtin.name, builtin.arguments, builtin.result)
            })
            .collect();
        assert!(undeclared.is_empty(), "rows sqlparser cannot declare: {undeclared:?}");
    }

    /// A lookup reads the rows of one folded name, and within them those of
    /// one stored name, as adjacent runs.
    #[test]
    fn rows_are_sorted_by_folded_then_stored_name() {
        assert!(POSTGRES_18_FUNCTIONS.windows(2).all(|pair| {
            folded_bytes(pair[0].name)
                .cmp(folded_bytes(pair[1].name))
                .then_with(|| pair[0].name.cmp(pair[1].name))
                .is_le()
        }));
    }
}
