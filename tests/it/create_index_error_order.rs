//! Index refusals follow dialect validation phases.

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect, SQLiteDialect};

const TABLE: &str =
    "CREATE TABLE probe(z text, payload json, number int); CREATE INDEX occupied ON probe(z);";

fn postgres_error(setup: &str, statement: &str) -> String {
    format!("{:?}", ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}{statement};")).err())
}

#[test]
fn postgres_keys_are_checked_one_at_a_time() {
    for statement in [
        "CREATE INDEX idx ON probe(payload, absent_column)",
        "CREATE INDEX idx ON probe(payload) INCLUDE(absent_column)",
        "CREATE INDEX occupied ON probe(payload)",
    ] {
        assert!(
            postgres_error(TABLE, statement).contains("MissingDefaultOperatorClass"),
            "{statement}"
        );
    }
    for statement in [
        "CREATE INDEX idx ON probe(absent_column, payload)",
        "CREATE INDEX occupied ON probe(absent_column)",
    ] {
        assert!(postgres_error(TABLE, statement).contains("ColumnNotFound"), "{statement}");
    }
}

#[test]
fn sqlite_name_precedes_columns() {
    let setup = "CREATE TABLE probe(z int); CREATE INDEX idx ON probe(z);";
    let error = ParserDB::parse::<SQLiteDialect>(&format!(
        "{setup}CREATE INDEX idx ON probe(absent_column);"
    ))
    .err();
    assert!(matches!(error, Some(Error::RelationNameAlreadyTaken { .. })));
    assert!(
        ParserDB::parse::<SQLiteDialect>(&format!(
            "{setup}CREATE INDEX IF NOT EXISTS idx ON probe(absent_column);"
        ))
        .is_ok()
    );
    let error =
        ParserDB::parse::<SQLiteDialect>(&format!("{setup}CREATE INDEX idx ON absent_table(z);"))
            .err();
    assert!(matches!(error, Some(Error::TableNotFoundForIndex { .. })));
}

#[test]
fn postgres_predicate_precedes_expression_and_plain_keys() {
    for statement in [
        "CREATE INDEX idx ON probe(payload) WHERE absent_predicate > 0",
        "CREATE INDEX idx ON probe(absent_key) WHERE absent_predicate > 0",
        "CREATE INDEX idx ON probe((absent_key + 1)) WHERE absent_predicate > 0",
        "CREATE INDEX idx ON probe(z) INCLUDE(absent_include) WHERE absent_predicate > 0",
        "CREATE UNIQUE INDEX idx ON probe USING hash(z) WHERE absent_predicate > 0",
    ] {
        let error = postgres_error(TABLE, statement);
        assert!(
            error.contains("UnknownColumn") && error.contains("absent_predicate"),
            "{statement}: {error}"
        );
    }
}

#[test]
fn postgres_expression_keys_precede_plain_keys() {
    let error =
        postgres_error(TABLE, "CREATE INDEX idx ON probe(payload, (absent_expression + 1))");
    assert!(error.contains("UnknownColumn") && error.contains("absent_expression"), "{error}");
}

#[test]
fn postgres_table_precedes_access_method() {
    let error = postgres_error(TABLE, "CREATE UNIQUE INDEX idx ON absent_table USING hash(z)");
    assert!(error.contains("TableNotFoundForIndex"), "{error}");
    let error = postgres_error(TABLE, "CREATE UNIQUE INDEX idx ON probe USING hash(absent_key)");
    assert!(error.contains("AccessMethodWithoutUniqueSupport"), "{error}");
}

#[test]
fn postgres_partition_checks_follow_keys_and_include() {
    let setup = "CREATE TABLE probe(z text,payload json,number int) PARTITION BY RANGE(number); CREATE INDEX occupied ON probe(z);";
    for only in ["", "ONLY "] {
        let error =
            postgres_error(setup, &format!("CREATE UNIQUE INDEX occupied ON {only}probe(payload)"));
        assert!(error.contains("MissingDefaultOperatorClass"), "{error}");
        let error = postgres_error(
            setup,
            &format!("CREATE UNIQUE INDEX occupied ON {only}probe(z) INCLUDE(absent_include)"),
        );
        assert!(error.contains("ColumnNotFound"), "{error}");
        let error =
            postgres_error(setup, &format!("CREATE UNIQUE INDEX occupied ON {only}probe(z)"));
        assert!(error.contains("UniqueConstraintMissesPartitionKey"), "{error}");
    }
}

#[test]
fn materialized_view_keys_follow_expression_transform() {
    let setup = "CREATE MATERIALIZED VIEW probe AS SELECT '1'::text AS z;";
    let error =
        postgres_error(setup, "CREATE INDEX idx ON probe((CAST(z AS json)), absent_column)");
    assert!(error.contains("MissingDefaultOperatorClass"), "{error}");
    let error = postgres_error(
        setup,
        "CREATE INDEX idx ON probe(absent_column) WHERE absent_predicate > 0",
    );
    assert!(error.contains("UnknownColumn") && error.contains("absent_predicate"), "{error}");
}

#[test]
fn materialized_view_index_columns_outside_postgres() {
    let setup = "CREATE MATERIALIZED VIEW probe AS SELECT 1 AS z;";
    for statement in [
        "CREATE INDEX idx ON probe(absent_column)",
        "CREATE INDEX idx ON probe(z) INCLUDE(absent_column)",
    ] {
        let error = ParserDB::parse::<GenericDialect>(&format!("{setup}{statement};")).err();
        assert!(
            matches!(
                &error,
                Some(Error::IdentifierLookupError(LookupError::ColumnNotFound { column_name, .. }))
                    if column_name == "absent_column"
            ),
            "{statement}: {error:?}"
        );
    }
}

#[test]
fn postgres_expression_transform_follows_written_order() {
    for statement in [
        "CREATE INDEX idx ON probe(((SELECT 1))) WHERE absent_predicate > 0",
        "CREATE INDEX idx ON probe((absent_first + 1), ((SELECT 1)))",
    ] {
        let error = postgres_error(TABLE, statement);
        assert!(error.contains("UnknownColumn"), "{error}");
    }
}

#[test]
fn postgres_foreign_partition_precedes_predicate_unless_only() {
    let setup = "CREATE TABLE probe(z int) PARTITION BY RANGE(z); CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w; CREATE FOREIGN TABLE child PARTITION OF probe FOR VALUES FROM (0) TO (10) SERVER s;";
    let error =
        postgres_error(setup, "CREATE UNIQUE INDEX idx ON probe(z) WHERE absent_predicate > 0");
    assert!(error.contains("UniqueIndexOverForeignPartition"), "{error}");
    let error = postgres_error(
        setup,
        "CREATE UNIQUE INDEX idx ON ONLY probe(z) WHERE absent_predicate > 0",
    );
    assert!(error.contains("UnknownColumn"), "{error}");
}
