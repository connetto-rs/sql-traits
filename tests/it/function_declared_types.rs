//! Tests that a function answers the types its arguments and results are
//! declared with, type modifiers included, although PostgreSQL discards those
//! modifiers and the normalized type names leave them out.
#![expect(clippy::expect_used, reason = "Fixtures must declare the functions the tests read.")]

use sql_traits::prelude::*;
use sqlparser::{ast::CreateFunction, dialect::PostgreSqlDialect};

const SCHEMA: &str = "
    CREATE FUNCTION args(OUT id VARCHAR(3), a NUMERIC(5,2), b VARCHAR) RETURNS RECORD
        LANGUAGE sql AS 'SELECT 1';
    CREATE FUNCTION outs(OUT id VARCHAR(3)) LANGUAGE sql AS 'SELECT ''a''';
    CREATE FUNCTION stamp() RETURNS TIMESTAMP(0) LANGUAGE sql AS 'SELECT now()';
    CREATE FUNCTION codes() RETURNS SETOF VARCHAR(2) LANGUAGE sql AS 'SELECT ''ab''';
    CREATE FUNCTION rows() RETURNS TABLE (x VARCHAR(2), y TEXT) LANGUAGE sql
        AS 'SELECT ''ab'', ''c''';";

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the function exists")
}

/// Each argument answers its declared modifier, in declaration order, while
/// its type name stays the normalized one overload resolution compares.
#[test]
fn an_argument_type_keeps_its_declared_modifier() {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema applies");
    let args = function(&database, "args");
    let modifiers: Vec<_> =
        args.argument_types(&database).map(|declared| declared.typmod(&database)).collect();
    assert_eq!(
        modifiers,
        [
            Some(TypeModifier::Modified(7)),
            Some(TypeModifier::Modified(327_686)),
            Some(TypeModifier::Unmodified),
        ]
    );
    assert_eq!(
        args.argument_type_names(&database).collect::<Vec<_>>(),
        ["VARCHAR", "NUMERIC", "VARCHAR"]
    );
}

/// The return type answers its declared modifier, the element type for a set,
/// and nothing for a function whose output arguments declare its result.
#[test]
fn a_return_type_keeps_its_declared_modifier() {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema applies");
    let modifier = |name: &str| {
        function(&database, name).return_type(&database).map(|declared| declared.typmod(&database))
    };
    assert_eq!(modifier("stamp"), Some(Some(TypeModifier::Modified(0))));
    assert_eq!(modifier("codes"), Some(Some(TypeModifier::Modified(6))));
    assert_eq!(modifier("outs"), None);
    let codes = function(&database, "codes");
    assert_eq!(codes.return_type_name(&database).as_deref(), Some("SETOF VARCHAR"));
}

/// Each `RETURNS TABLE` column answers its declared modifier.
#[test]
fn a_returned_column_type_keeps_its_declared_modifier() {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema applies");
    let columns: Vec<_> = function(&database, "rows")
        .returned_columns(&database)
        .map(|(name, declared)| {
            (name.name().to_owned(), declared.name(&database), declared.typmod(&database))
        })
        .collect();
    assert_eq!(
        columns,
        [
            ("x".to_owned(), "VARCHAR".into(), Some(TypeModifier::Modified(6))),
            ("y".to_owned(), "TEXT".into(), Some(TypeModifier::Unmodified)),
        ]
    );
}
