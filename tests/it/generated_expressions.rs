//! Generation expression restrictions.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn generated_references_are_refused_in_every_storage_mode() {
    for source_mode in ["", "VIRTUAL", "STORED"] {
        for target_mode in ["", "VIRTUAL", "STORED"] {
            for suffix in [
                format!(", h int GENERATED ALWAYS AS (g + 1) {target_mode});"),
                format!("); ALTER TABLE t ADD h int GENERATED ALWAYS AS (g + 1) {target_mode};"),
            ] {
                let sql = format!(
                    "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x + 1) {source_mode}{suffix}"
                );
                assert!(
                    matches!(
                        ParserDB::parse::<PostgreSqlDialect>(&sql),
                        Err(Error::GeneratedColumnReference { column }) if column == "g"
                    ),
                    "{sql}"
                );
            }
        }
    }
}

#[test]
fn self_reference_is_refused() {
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (g int GENERATED ALWAYS AS (g + 1));"
        ),
        Err(Error::GeneratedColumnReference { column }) if column == "g"
    ));
}

#[test]
fn volatility_includes_calls_operators_and_casts() {
    for mode in ["", "VIRTUAL", "STORED"] {
        for (source, target, expression) in [
            ("int", "float8", "random()"),
            ("int", "timestamptz", "now()"),
            ("int", "date", "CURRENT_DATE"),
            ("timestamptz", "text", "x::text"),
            ("timestamp", "text", "CAST(x AS text)"),
            ("date", "text", "x::text"),
            ("text", "date", "x::date"),
            ("timestamptz", "date", "x::date"),
            ("timestamptz", "timestamptz", "x + interval '1 day'"),
        ] {
            let sql = format!(
                "CREATE TABLE t (x {source}, g {target} GENERATED ALWAYS AS ({expression}) {mode});"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
            assert!(
                matches!(&result, Err(Error::GenerationExpressionNotImmutable)),
                "{sql}\n{result:?}"
            );
        }
    }
}

#[test]
fn immutable_builtin_expressions_and_tableoid_are_allowed() -> Result<(), Error> {
    for mode in ["", "VIRTUAL", "STORED"] {
        for (target, expression) in [
            ("int", "abs(x) + 1"),
            ("text", "x::text"),
            ("oid", "tableoid"),
            ("oid", "t.tableoid"),
            ("int[]", "ARRAY[x, 1]"),
            ("date", "'2026-01-01'::date"),
        ] {
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "CREATE TABLE t (x int, g {target} GENERATED ALWAYS AS ({expression}) {mode});"
            ))?;
        }
    }
    Ok(())
}

#[test]
fn other_system_columns_are_refused() {
    for mode in ["", "VIRTUAL", "STORED"] {
        for column in ["ctid", "xmin", "xmax", "cmin", "cmax"] {
            let sql =
                format!("CREATE TABLE t (g text GENERATED ALWAYS AS ({column}::text) {mode});");
            assert!(
                matches!(
                    ParserDB::parse::<PostgreSqlDialect>(&sql),
                    Err(Error::GeneratedSystemColumnReference { column: found }) if found == column
                ),
                "{sql}"
            );
        }
    }
}

#[test]
fn default_conflict_precedes_expression_resolution() {
    for mode in ["", "VIRTUAL", "STORED"] {
        for options in [
            format!("DEFAULT 1 GENERATED ALWAYS AS (missing) {mode}"),
            format!("GENERATED ALWAYS AS (random()) {mode} DEFAULT 1"),
        ] {
            assert!(matches!(
                ParserDB::parse::<PostgreSqlDialect>(&format!("CREATE TABLE t (g int {options});")),
                Err(Error::DefaultAndGeneration { table, column }) if table == "t" && column == "g"
            ));
        }
    }
}

#[test]
fn virtual_checks_every_expression_node_type() -> Result<(), Error> {
    for expression in [
        "x IS NULL",
        "ARRAY[x] IS NULL",
        "COALESCE(x, x) IS NULL",
        "CASE WHEN true THEN x ELSE x END IS NULL",
    ] {
        for mode in ["", "VIRTUAL"] {
            let sql = format!(
                "CREATE TYPE mood AS ENUM ('ok'); CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS ({expression}) {mode});"
            );
            assert!(
                matches!(
                    ParserDB::parse::<PostgreSqlDialect>(&sql),
                    Err(Error::GenerationExpressionUserDefinedType)
                ),
                "{sql}"
            );
        }
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TYPE mood AS ENUM ('ok'); CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS ({expression}) STORED);"
        ))?;
    }
    Ok(())
}

#[test]
fn virtual_user_functions_are_refused_after_volatility() -> Result<(), Error> {
    let declaration = "CREATE FUNCTION helper(int) RETURNS int LANGUAGE sql IMMUTABLE RETURN $1;";
    for mode in ["", "VIRTUAL"] {
        assert!(matches!(
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "{declaration} CREATE TABLE t (x int, g int GENERATED ALWAYS AS (helper(x)) {mode});"
            )),
            Err(Error::GenerationExpressionUserDefinedFunction)
        ));
        assert!(matches!(
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "{declaration} CREATE TABLE t (x int, g float8 GENERATED ALWAYS AS (helper(x) + random()) {mode});"
            )),
            Err(Error::GenerationExpressionNotImmutable)
        ));
    }
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{declaration} CREATE TABLE t (x int, g int GENERATED ALWAYS AS (helper(x)) STORED);"
    ))?;
    Ok(())
}

#[test]
fn binding_errors_precede_generated_reference_errors() {
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x+1), h int GENERATED ALWAYS AS (g+missing));"
        ),
        Err(Error::UnknownColumn { .. })
    ));
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x+1), h float8 GENERATED ALWAYS AS (g+random()));"
        ),
        Err(Error::GeneratedColumnReference { column }) if column == "g"
    ));
}

#[test]
fn alter_add_tableoid_is_allowed() -> Result<(), Error> {
    for mode in ["", "VIRTUAL", "STORED"] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t (x int); ALTER TABLE t ADD g oid GENERATED ALWAYS AS (tableoid) {mode};"
        ))?;
    }
    Ok(())
}

#[test]
fn forward_generated_references_are_refused() {
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (h int GENERATED ALWAYS AS (g+1), g int GENERATED ALWAYS AS (1));"
        ),
        Err(Error::GeneratedColumnReference { column }) if column == "g"
    ));
}

#[test]
fn virtual_domain_expression_is_refused() -> Result<(), Error> {
    let declaration = "CREATE DOMAIN nonnegative AS int CHECK (VALUE >= 0);";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{declaration} CREATE TABLE t (x nonnegative, g bool GENERATED ALWAYS AS (x IS NULL));"
        )),
        Err(Error::GenerationExpressionUserDefinedType)
    ));
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{declaration} CREATE TABLE t (x nonnegative, g bool GENERATED ALWAYS AS (x IS NULL) STORED);"
    ))?;
    Ok(())
}

#[test]
fn container_text_conversions_are_not_immutable() {
    for (source, target) in
        [("int[]", "text"), ("text", "int[]"), ("int4range", "text"), ("daterange", "text")]
    {
        assert!(matches!(
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "CREATE TABLE t (x {source}, g {target} GENERATED ALWAYS AS (x::{target}) STORED);"
            )),
            Err(Error::GenerationExpressionNotImmutable)
        ));
    }
}

#[test]
fn virtual_refusal_follows_expression_node_order() {
    let declaration = "CREATE TYPE mood AS ENUM ('ok'); CREATE FUNCTION helper(int) RETURNS int LANGUAGE sql IMMUTABLE RETURN $1;";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{declaration} CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS (x IS NULL OR helper(1) IS NULL));"
        )),
        Err(Error::GenerationExpressionUserDefinedType)
    ));
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{declaration} CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS (helper(1) IS NULL OR x IS NULL));"
        )),
        Err(Error::GenerationExpressionUserDefinedFunction)
    ));
}

#[test]
fn alter_add_cannot_reference_a_later_added_column() {
    for mode in ["", "VIRTUAL", "STORED"] {
        let sql = format!(
            "CREATE TABLE t (x int); ALTER TABLE t ADD h int GENERATED ALWAYS AS (g + 1) {mode}, ADD g int GENERATED ALWAYS AS (x + 1) {mode};"
        );
        assert!(
            matches!(
                ParserDB::parse::<PostgreSqlDialect>(&sql),
                Err(Error::UnknownColumn { reference }) if reference.to_string() == "g"
            ),
            "{sql}"
        );
    }
}

#[test]
fn generation_expression_errors_match_postgres_messages() {
    for (sql, expected) in [
        (
            "CREATE TABLE t (g int GENERATED ALWAYS AS (g + 1));",
            "cannot use generated column \"g\" in column generation expression",
        ),
        (
            "CREATE TABLE t (g text GENERATED ALWAYS AS (ctid::text));",
            "cannot use system column \"ctid\" in column generation expression",
        ),
        (
            "CREATE TABLE t (g float8 GENERATED ALWAYS AS (random()));",
            "generation expression is not immutable",
        ),
        (
            "CREATE TYPE mood AS ENUM ('ok'); CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS (x IS NULL));",
            "generation expression uses user-defined type",
        ),
        (
            "CREATE FUNCTION helper(int) RETURNS int LANGUAGE sql IMMUTABLE RETURN $1; CREATE TABLE t (x int, g int GENERATED ALWAYS AS (helper(x)));",
            "generation expression uses user-defined function",
        ),
        (
            "CREATE TABLE t (g int DEFAULT 1 GENERATED ALWAYS AS (2));",
            "both default and generation expression specified for column \"g\" of table \"t\"",
        ),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(sql);
        assert_eq!(result.err().map(|error| error.to_string()).as_deref(), Some(expected), "{sql}");
    }
}

#[test]
fn interval_literals_have_a_type_for_operator_selection() -> Result<(), Error> {
    use sqlparser::{ast::Expr, parser::Parser};

    let database = ParserDB::parse::<PostgreSqlDialect>("")?;
    let expression: Expr =
        Parser::new(&PostgreSqlDialect {}).try_with_sql("interval '1 day'")?.parse_expr()?;
    let context = ExpressionContext::for_expression(
        &expression,
        &ExpressionScope::Empty,
        &database,
        IdentifierCase::Folded,
    )?;
    assert_eq!(
        context.type_of(&expression)?,
        OperandType::Known {
            type_name: TargetName::new("interval", false).with_schema("pg_catalog", false),
            typmod: None,
        },
    );
    for mode in ["", "VIRTUAL", "STORED"] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t (x timestamp, g timestamp GENERATED ALWAYS AS (x + interval '1 day') {mode});"
        ))?;
    }
    Ok(())
}
