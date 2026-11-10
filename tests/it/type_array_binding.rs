//! Generated array aliases bind to their element identities at declaration.
#![expect(clippy::expect_used, clippy::panic, reason = "Tests require concrete parsed identities.")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{
        ArrayElemTypeDef, CreateDomain, CreateFunction, DataType, FunctionReturnType, Ident,
        ObjectName,
    },
    dialect::PostgreSqlDialect,
};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the input builds")
}

/// The element type a stored array declaration names.
fn array_element(data_type: &DataType) -> &DataType {
    match data_type {
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => element,
        other => panic!("the stored type is {other:?}, which declares no element"),
    }
}

/// Checks the qualified array element identity.
fn assert_element(data_type: &DataType, schema: &str, name: &str, quoted: bool) {
    let element = array_element(data_type);
    let ident = if quoted { Ident::with_quote('"', name) } else { Ident::new(name) };
    let expected = DataType::Custom(ObjectName::from(vec![Ident::new(schema), ident]), Vec::new());
    assert_eq!(element, &expected);
}

/// The declared type of column `column` of the table `table` of schema `o`.
fn column_type<'db>(database: &'db ParserDB, table: &str, column: &str) -> &'db DataType {
    database
        .table_by_target(
            TargetName::new(table, false).with_schema("o", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table exists")
        .column(column, database, IdentifierCase::AsWritten)
        .expect("the table is in this database")
        .expect("the column exists")
        .data_type(database)
}

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the function exists")
}

fn domain<'db>(database: &'db ParserDB, name: &str) -> &'db CreateDomain {
    database.domains().find(|domain| domain.name() == name).expect("the domain exists")
}

#[test]
fn an_array_alias_binds_to_the_first_schema_the_path_reaches() {
    let db = parse(
        "CREATE SCHEMA a;
         CREATE SCHEMA b;
         CREATE SCHEMA o;
         CREATE TYPE a.token AS ENUM ('one');
         CREATE TYPE b.token AS ENUM ('two');
         SET search_path TO b, a;
         CREATE TABLE o.t (c _token);",
    );
    assert_element(column_type(&db, "t", "c"), "b", "token", false);
}

#[test]
fn a_qualified_array_alias_binds_to_the_schema_it_names() {
    let db = parse(
        "CREATE SCHEMA s;
         CREATE SCHEMA o;
         CREATE TYPE s.token AS ENUM ('one');
         CREATE TYPE token AS ENUM ('two');
         CREATE TABLE o.t (c s._token);",
    );
    assert_element(column_type(&db, "t", "c"), "s", "token", false);
}

#[test]
fn a_quoted_array_alias_keeps_the_quoted_owner_name() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TYPE \"Token\" AS ENUM ('one');
         CREATE TABLE o.t (c \"_Token\", d _token);",
    );
    assert_element(column_type(&db, "t", "c"), "public", "Token", true);
    let written = DataType::Custom(ObjectName::from(Ident::new("_token")), Vec::new());
    assert_eq!(column_type(&db, "t", "d"), &written);
}

#[test]
fn a_signature_array_alias_follows_the_owner_rename() {
    let db = parse(
        "CREATE TYPE token AS ENUM ('one');
         CREATE FUNCTION f(x _token) RETURNS _token LANGUAGE sql AS 'SELECT NULL';
         ALTER TYPE token RENAME TO coin;",
    );
    let function = function(&db, "f");
    assert_element(
        &function.args.as_ref().expect("the argument is declared")[0].data_type,
        "public",
        "coin",
        false,
    );
    match function.return_type.as_ref().expect("the result is declared") {
        FunctionReturnType::DataType(result) => assert_element(result, "public", "coin", false),
        other @ FunctionReturnType::SetOf(_) => {
            panic!("the result is {other:?}, which names no scalar type")
        }
    }
}

#[test]
fn a_domain_array_alias_binds_to_the_domain_it_reaches() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE DOMAIN token AS INT;
         CREATE TABLE o.t (c _token);",
    );
    assert_element(column_type(&db, "t", "c"), "public", "token", false);
    let domain = domain(&db, "token");
    assert!(matches!(domain.data_type, DataType::Int(_)));
}

#[test]
fn a_rowtype_array_alias_binds_to_the_rowtype_it_reaches() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TABLE token (value INT);
         CREATE TABLE o.t (c _token);",
    );
    assert_element(column_type(&db, "t", "c"), "public", "token", false);
    let rowtype = db
        .table_by_target(TargetName::new("token", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the relation exists");
    assert_eq!(rowtype.table_name(), "token");
}

#[test]
fn a_domain_declared_over_a_shell_is_refused() {
    let result = ParserDB::parse::<PostgreSqlDialect>("CREATE TYPE s; CREATE DOMAIN d AS s;");
    assert!(matches!(result, Err(Error::TypeIsOnlyShell { .. })));
}

#[test]
fn a_rowtype_array_follows_relation_renames() {
    for declaration in [
        "CREATE TABLE token (value INT)",
        "CREATE VIEW token AS SELECT 1 AS value",
        "CREATE MATERIALIZED VIEW token AS SELECT 1 AS value",
    ] {
        let db = parse(&format!(
            "CREATE SCHEMA o;
             {declaration};
             CREATE TABLE o.t (c _token);
             ALTER TABLE token RENAME TO coin;"
        ));
        assert_element(column_type(&db, "t", "c"), "public", "coin", false);
    }
}

#[test]
fn an_array_alias_follows_its_owner_schema() {
    let db = parse(
        "CREATE SCHEMA s;
         CREATE SCHEMA o;
         CREATE TYPE s.token AS ENUM ('one');
         CREATE TABLE o.t (c s._token);
         ALTER SCHEMA s RENAME TO renamed;
         CREATE TABLE o.after (c renamed._token);",
    );
    assert_element(column_type(&db, "t", "c"), "renamed", "token", false);
    assert_element(column_type(&db, "after", "c"), "renamed", "token", false);
}

#[test]
fn snapshots_preserve_array_names_across_resumed_ingestion() {
    let ingestor = parse("CREATE SCHEMA o; CREATE TYPE token AS ENUM ('one');").into_ingestor();
    let snapshot = ingestor.snapshot();
    let resume = |ingestor, sql| {
        sqlparser::parser::Parser::parse_sql(&PostgreSqlDialect {}, sql)
            .expect("statements parse")
            .into_iter()
            .try_fold(ingestor, ParserDBIngestor::apply_statement)
            .expect("statements apply")
            .finish()
    };
    let displaced =
        resume(ingestor, "CREATE TYPE _token AS ENUM ('two'); CREATE TABLE o.t (c __token);");
    let original = resume(snapshot.into_ingestor(), "CREATE TABLE o.t (c _token);");
    assert_element(column_type(&displaced, "t", "c"), "public", "token", false);
    assert_element(column_type(&original, "t", "c"), "public", "token", false);
}

#[test]
fn builtin_array_precedence_respects_explicit_catalog_placement() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TYPE public.text AS ENUM ('one');
         CREATE TABLE o.implicit_catalog (c _text);
         SET search_path TO public, pg_catalog;
         CREATE TABLE o.explicit_catalog (c _text);",
    );
    assert_element(column_type(&db, "implicit_catalog", "c"), "pg_catalog", "text", false);
    assert_element(column_type(&db, "explicit_catalog", "c"), "public", "text", false);
}

#[test]
fn internal_char_arrays_remain_distinct_from_sql_character() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TABLE o.t (internal pg_catalog._char, character CHAR[]);",
    );
    assert_element(column_type(&db, "t", "internal"), "pg_catalog", "char", false);
    assert_ne!(
        array_element(column_type(&db, "t", "internal")),
        array_element(column_type(&db, "t", "character"))
    );
}

#[test]
fn quoted_catalog_array_names_require_exact_case() {
    let db = parse(
        r#"CREATE SCHEMA o;
           CREATE TABLE o.t (canonical pg_catalog."_text", lookalike pg_catalog."_TEXT");"#,
    );
    assert_element(column_type(&db, "t", "canonical"), "pg_catalog", "text", false);
    assert!(matches!(
        column_type(&db, "t", "lookalike"),
        DataType::Custom(name, modifiers)
            if name.to_string() == "pg_catalog.\"_TEXT\"" && modifiers.is_empty()
    ));
}

#[test]
fn arrays_of_generated_arrays_bind_to_the_same_element() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TYPE token AS ENUM ('one');
         CREATE TABLE o.t (c _token[]);",
    );
    assert_element(column_type(&db, "t", "c"), "public", "token", false);
}

#[test]
fn temporary_rowtype_arrays_precede_the_search_path() {
    let db = parse(
        "CREATE SCHEMA o;
         CREATE TYPE token AS ENUM ('one');
         CREATE TEMP TABLE token (value INT);
         CREATE TABLE o.t (c _token);",
    );
    assert_element(column_type(&db, "t", "c"), "pg_temp", "token", false);
}

#[test]
fn generic_dialect_does_not_infer_postgresql_arrays() {
    let db = ParserDB::parse::<sqlparser::dialect::GenericDialect>(
        "CREATE SCHEMA o;
         CREATE TYPE token AS ENUM ('one');
         CREATE TABLE o.t (c _token);",
    )
    .expect("generic declarations parse");
    assert_eq!(
        column_type(&db, "t", "c"),
        &DataType::Custom(ObjectName::from(Ident::new("_token")), Vec::new())
    );
}
