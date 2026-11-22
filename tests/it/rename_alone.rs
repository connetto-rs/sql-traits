//! PostgreSQL relation renames occupy an entire alteration statement.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    dialect::{GenericDialect, PostgreSqlDialect},
    parser::Parser,
};

const SETUP: &str =
    "CREATE TABLE probe (value int, spare int, CONSTRAINT positive CHECK (value > 0));";

#[test]
fn rename_cannot_follow_another_action() {
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SETUP} ALTER TABLE probe ALTER COLUMN value SET DEFAULT 4, RENAME COLUMN value TO renamed;"
    ));
    assert!(
        matches!(result, Err(Error::RenameCommandNotAlone { relation_name, .. }) if relation_name == "probe")
    );
}

#[test]
fn rename_cannot_precede_another_action() {
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SETUP} ALTER TABLE probe RENAME COLUMN value TO renamed, ALTER COLUMN renamed SET DEFAULT 4;"
    ));
    assert!(
        matches!(result, Err(Error::RenameCommandNotAlone { relation_name, .. }) if relation_name == "probe")
    );
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "Unexpected variants and missing diagnostics fail this test."
)]
#[test]
fn combined_rename_forms_are_refused() {
    for form in [
        "RENAME COLUMN value TO renamed",
        "RENAME value TO renamed",
        "RENAME CONSTRAINT positive TO renamed_constraint",
        "RENAME TO renamed_probe",
        "SET SCHEMA destination",
    ] {
        for qualifier in ["", "IF EXISTS ", "ONLY ", "IF EXISTS ONLY "] {
            for companion in [
                "RENAME COLUMN spare TO renamed_spare",
                "ADD COLUMN extra int",
                "ALTER COLUMN spare SET DEFAULT 4",
                "DROP COLUMN spare",
                "ATTACH PARTITION child FOR VALUES FROM (0) TO (10)",
            ] {
                for (first, second) in [(form, companion), (companion, form)] {
                    for (relation, setup) in [
                        ("TABLE", SETUP),
                        (
                            "FOREIGN TABLE",
                            "CREATE FOREIGN TABLE probe (value int, spare int, CONSTRAINT positive CHECK (value > 0)) SERVER warehouse;",
                        ),
                    ] {
                        let statement =
                            format!("ALTER {relation} {qualifier}probe {first}, {second};");
                        if Parser::parse_sql(&PostgreSqlDialect {}, &statement).is_ok() {
                            let error = ParserDB::parse::<PostgreSqlDialect>(&format!(
                                "{setup} {statement}"
                            ))
                            .expect_err(&statement);
                            let first_is_standalone = first.starts_with("RENAME")
                                || first.starts_with("SET SCHEMA")
                                || first.starts_with("ATTACH");
                            let standalone = if first_is_standalone { first } else { second };
                            let foreign_grammar = relation == "FOREIGN TABLE"
                                && (first.starts_with("RENAME CONSTRAINT")
                                    || first.starts_with("ATTACH"));
                            let expected_token =
                                if foreign_grammar && first.starts_with("RENAME CONSTRAINT") {
                                    "CONSTRAINT"
                                } else if foreign_grammar {
                                    "ATTACH"
                                } else if first_is_standalone {
                                    ","
                                } else if standalone.starts_with("SET SCHEMA") {
                                    "SCHEMA"
                                } else {
                                    "RENAME"
                                };
                            let written_token = match &error {
                                Error::ForeignTableCommandUnavailable { written_token, .. }
                                    if foreign_grammar =>
                                {
                                    written_token
                                }
                                Error::SetSchemaCommandNotAlone { written_token }
                                    if standalone.starts_with("SET SCHEMA") =>
                                {
                                    written_token
                                }
                                Error::RenameCommandNotAlone { relation_name, written_token }
                                    if standalone.starts_with("RENAME")
                                        && relation_name == "probe" =>
                                {
                                    written_token
                                }
                                Error::PartitionCommandNotAlone { table_name, written_token }
                                    if standalone.starts_with("ATTACH")
                                        && table_name == "probe" =>
                                {
                                    written_token
                                }
                                other => panic!("{statement} got {other:?}"),
                            };
                            assert_eq!(written_token, expected_token, "{statement}");
                            let postgres = error.postgres_error().expect("syntax diagnostic");
                            assert_eq!(postgres.sqlstate().to_string(), "42601", "{statement}");
                            assert_eq!(
                                postgres.message(),
                                format!("syntax error at or near \"{expected_token}\""),
                                "{statement}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn standalone_renames_remain_accepted() {
    for form in [
        "RENAME COLUMN value TO renamed",
        "RENAME value TO renamed",
        "RENAME CONSTRAINT positive TO renamed_constraint",
        "RENAME TO renamed_probe",
    ] {
        for qualifier in ["", "IF EXISTS ", "ONLY ", "IF EXISTS ONLY "] {
            let statement = format!("{SETUP} ALTER TABLE {qualifier}probe {form};");
            assert!(ParserDB::parse::<PostgreSqlDialect>(&statement).is_ok(), "{statement}");
        }
    }
}

#[test]
fn ordinary_actions_remain_combinable() {
    assert!(ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SETUP} ALTER TABLE probe ADD COLUMN extra int, ALTER COLUMN value SET DEFAULT 4, DROP COLUMN spare;"
    )).is_ok());
}

#[test]
fn relation_renames_cannot_be_combined() {
    for setup in [
        "CREATE VIEW probe AS SELECT 1 AS value, 2 AS spare;",
        "CREATE MATERIALIZED VIEW probe AS SELECT 1 AS value, 2 AS spare;",
        "CREATE FOREIGN TABLE probe (value int, spare int) SERVER warehouse;",
        "CREATE TABLE backing (value int, spare int); CREATE INDEX probe ON backing (value);",
    ] {
        for form in
            ["RENAME COLUMN value TO renamed", "RENAME value TO renamed", "RENAME TO renamed_probe"]
        {
            for operations in [
                format!("{form}, ALTER COLUMN spare SET DEFAULT 4"),
                format!("ALTER COLUMN spare SET DEFAULT 4, {form}"),
            ] {
                let statement = format!("{setup} ALTER TABLE probe {operations};");
                assert!(
                    matches!(ParserDB::parse::<PostgreSqlDialect>(&statement), Err(Error::RenameCommandNotAlone { relation_name, .. }) if relation_name == "probe"),
                    "{statement}"
                );
            }
        }
    }
}

#[test]
fn missing_relation_does_not_bypass_rename_syntax() {
    for qualifier in ["IF EXISTS ", "IF EXISTS ONLY "] {
        let statement = format!(
            "ALTER TABLE {qualifier}absent RENAME COLUMN value TO renamed, ADD COLUMN extra int;"
        );
        assert!(
            matches!(ParserDB::parse::<PostgreSqlDialect>(&statement), Err(Error::RenameCommandNotAlone { relation_name, .. }) if relation_name == "absent"),
            "{statement}"
        );
    }
}

#[test]
fn generic_dialect_keeps_combined_renames() {
    assert!(ParserDB::parse::<GenericDialect>(&format!(
        "{SETUP} ALTER TABLE probe RENAME COLUMN value TO renamed, ALTER COLUMN renamed SET DEFAULT 4;"
    )).is_ok());
}
