//! Generated columns retain their storage kind through inheritance and trigger
//! validation.

use sql_traits::{
    errors::{ChildMismatch, Error, UnattachablePartition},
    prelude::*,
};
use sqlparser::{ast::GeneratedExpressionMode, dialect::PostgreSqlDialect};

const FUNCTION: &str =
    "CREATE FUNCTION trigger_fn() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

#[test]
fn child_generation_kind_must_match_parent() {
    for (parent_mode, child_mode) in
        [("STORED", "VIRTUAL"), ("STORED", ""), ("VIRTUAL", "STORED"), ("", "STORED")]
    {
        for partition in [false, true] {
            let parent = format!(
                "CREATE TABLE parent(a int, v int GENERATED ALWAYS AS(a+1) {parent_mode}){};",
                if partition { " PARTITION BY RANGE(a)" } else { "" }
            );
            let child = if partition {
                format!(
                    "CREATE TABLE child PARTITION OF parent(v GENERATED ALWAYS AS(a+2) {child_mode}) FOR VALUES FROM (0) TO (10);"
                )
            } else {
                format!(
                    "CREATE TABLE child(v int GENERATED ALWAYS AS(a+2) {child_mode}) INHERITS(parent);"
                )
            };
            let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{parent}{child}"));
            assert_eq!(
                result.as_ref().err().map(ToString::to_string),
                Some("column \"v\" inherits from generated column of different kind".to_owned()),
            );
            assert!(
                matches!(result, Err(Error::InheritedGeneratedKindConflict { column_name, .. }) if column_name == "v")
            );
        }
    }
}

#[expect(clippy::unwrap_used, reason = "the parsed tables and inherited column must exist")]
#[test]
fn children_receive_virtual_storage() -> Result<(), Error> {
    for mode in ["VIRTUAL", ""] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE parent(a int, v int GENERATED ALWAYS AS(a+1) {mode}); CREATE TABLE child() INHERITS(parent); CREATE TABLE plain(v int) INHERITS(parent); CREATE TABLE root(a int, v int GENERATED ALWAYS AS(a+1) {mode}) PARTITION BY RANGE(a); CREATE TABLE part PARTITION OF root FOR VALUES FROM (0) TO (10);"
        ))?;
        for name in ["child", "plain", "part"] {
            let table = database
                .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)?
                .unwrap();
            let column =
                table.columns(&database)?.find(|column| column.column_name() == "v").unwrap();
            assert_eq!(
                column.generation_storage(&database),
                Some(GeneratedExpressionMode::Virtual)
            );
        }
    }
    Ok(())
}

#[test]
fn existing_child_generation_kind_must_match_parent() {
    for (parent_mode, child_mode) in [("STORED", ""), ("", "STORED"), ("VIRTUAL", "STORED")] {
        let statements = format!(
            "CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1) {parent_mode}); CREATE TABLE child(a int,v int GENERATED ALWAYS AS(a+2) {child_mode}); ALTER TABLE child INHERIT parent;"
        );
        assert!(
            matches!(ParserDB::parse::<PostgreSqlDialect>(&statements), Err(Error::InheritanceChildMismatch { reason: ChildMismatch::GeneratedKind { column_name }, .. }) if column_name == "v")
        );
    }
}

#[test]
fn before_trigger_refuses_new_generated_columns_before_resolving_function() {
    for mode in ["STORED", "VIRTUAL", ""] {
        for condition in ["NEW.v > 0", "NEW IS DISTINCT FROM OLD", "NEW.* IS DISTINCT FROM OLD.*"] {
            for function in ["trigger_fn", "missing"] {
                let statements = format!(
                    "{FUNCTION} CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1) {mode}); CREATE TRIGGER generated BEFORE UPDATE ON parent FOR EACH ROW WHEN({condition}) EXECUTE FUNCTION {function}();"
                );
                let result = ParserDB::parse::<PostgreSqlDialect>(&statements);
                assert_eq!(
                    result.as_ref().err().map(ToString::to_string),
                    Some(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns"
                            .to_owned()
                    ),
                );
                assert!(matches!(result, Err(Error::BeforeTriggerGeneratedColumn { .. })));
            }
        }
    }
}

#[test]
fn trigger_generated_references_follow_timing_and_row() -> Result<(), Error> {
    for mode in ["STORED", "VIRTUAL", ""] {
        for (timing, condition) in [
            ("BEFORE", "OLD.v > 0"),
            ("AFTER", "NEW.v > 0"),
            ("AFTER", "NEW.tableoid > 0"),
            ("BEFORE", "OLD.tableoid > 0"),
        ] {
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "{FUNCTION} CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1) {mode}); CREATE TRIGGER generated {timing} UPDATE ON parent FOR EACH ROW WHEN({condition}) EXECUTE FUNCTION trigger_fn();"
            ))?;
        }
    }
    Ok(())
}

#[test]
fn before_trigger_refuses_new_system_columns() {
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{FUNCTION} CREATE TABLE parent(a int); CREATE TRIGGER generated BEFORE UPDATE ON parent FOR EACH ROW WHEN(NEW.tableoid > 0) EXECUTE FUNCTION trigger_fn();"
    ));
    assert_eq!(
        result.as_ref().err().map(ToString::to_string),
        Some("BEFORE trigger's WHEN condition cannot reference NEW system columns".to_owned()),
    );
    assert!(
        matches!(result, Err(Error::BeforeTriggerSystemColumn { column_name, .. }) if column_name == "tableoid")
    );
}

#[test]
fn attaching_partition_requires_matching_generated_storage() {
    let statements = "CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1)) PARTITION BY RANGE(a); CREATE TABLE child(a int,v int GENERATED ALWAYS AS(a+2) STORED); ALTER TABLE parent ATTACH PARTITION child FOR VALUES FROM(0) TO(10);";
    assert!(
        matches!(ParserDB::parse::<PostgreSqlDialect>(statements), Err(Error::PartitionNotAttachable { reason: UnattachablePartition::Shape(ChildMismatch::GeneratedKind { column_name }), .. }) if column_name == "v")
    );
}

#[test]
fn matching_generation_kind_allows_a_local_expression() -> Result<(), Error> {
    for mode in ["STORED", "VIRTUAL", ""] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1) {mode}); CREATE TABLE child(v int GENERATED ALWAYS AS(a+2) {mode}) INHERITS(parent); CREATE TABLE root(a int,v int GENERATED ALWAYS AS(a+1) {mode}) PARTITION BY RANGE(a); CREATE TABLE part PARTITION OF root(v GENERATED ALWAYS AS(a+2) {mode}) FOR VALUES FROM(0) TO(10);"
        ))?;
    }
    Ok(())
}

#[test]
fn before_trigger_checks_generated_columns_received_from_parent() {
    for child in [
        "CREATE TABLE child() INHERITS(parent)",
        "CREATE TABLE child PARTITION OF parent FOR VALUES FROM(0) TO(10)",
    ] {
        let partition = if child.contains("PARTITION") { " PARTITION BY RANGE(a)" } else { "" };
        let statements = format!(
            "{FUNCTION} CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1)){partition}; {child}; CREATE TRIGGER generated BEFORE UPDATE ON child FOR EACH ROW WHEN(NEW.v>0) EXECUTE FUNCTION trigger_fn();"
        );
        let result = ParserDB::parse::<PostgreSqlDialect>(&statements);
        assert_eq!(
            result.as_ref().err().map(ToString::to_string),
            Some(
                "BEFORE trigger's WHEN condition cannot reference NEW generated columns".to_owned()
            ),
        );
        assert!(matches!(result, Err(Error::BeforeTriggerGeneratedColumn { .. })));
    }
}

#[test]
fn before_trigger_accepts_whole_plain_row() -> Result<(), Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{FUNCTION} CREATE TABLE parent(a int); CREATE TRIGGER generated BEFORE UPDATE ON parent FOR EACH ROW WHEN(NEW IS DISTINCT FROM OLD) EXECUTE FUNCTION trigger_fn();"
    ))?;
    Ok(())
}

#[test]
fn missing_column_precedes_before_generated_refusal() {
    for condition in ["NEW.missing>0 AND NEW.v>0", "NEW.v>0 AND NEW.missing>0"] {
        let statements = format!(
            "{FUNCTION} CREATE TABLE parent(a int,v int GENERATED ALWAYS AS(a+1)); CREATE TRIGGER generated BEFORE UPDATE ON parent FOR EACH ROW WHEN({condition}) EXECUTE FUNCTION trigger_fn();"
        );
        assert!(matches!(
            ParserDB::parse::<PostgreSqlDialect>(&statements),
            Err(Error::UnknownColumn { .. })
        ));
    }
}

#[expect(clippy::unwrap_used, reason = "the inherited column must exist")]
#[test]
fn parent_added_column_reaches_children() -> Result<(), Error> {
    for mode in ["VIRTUAL", ""] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE parent(a int); CREATE TABLE child() INHERITS(parent); ALTER TABLE parent ADD COLUMN v int GENERATED ALWAYS AS(a+1) {mode}; CREATE TABLE root(a int) PARTITION BY RANGE(a); CREATE TABLE part PARTITION OF root FOR VALUES FROM(0) TO(10); ALTER TABLE root ADD COLUMN v int GENERATED ALWAYS AS(a+1) {mode};"
        ))?;
        for name in ["child", "part"] {
            let table = database
                .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)?
                .unwrap();
            let column =
                table.columns(&database)?.find(|column| column.column_name() == "v").unwrap();
            assert_eq!(
                column.generation_storage(&database),
                Some(GeneratedExpressionMode::Virtual)
            );
        }
    }
    Ok(())
}

#[expect(clippy::unwrap_used, reason = "the inherited table and column must exist")]
#[test]
fn parent_added_generation_keeps_existing_child_storage() -> Result<(), Error> {
    for (declaration, expected) in [
        ("int", None),
        ("int GENERATED ALWAYS AS(a+2) STORED", Some(GeneratedExpressionMode::Stored)),
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE parent(a int); CREATE TABLE child(v {declaration}) INHERITS(parent); ALTER TABLE parent ADD COLUMN v int GENERATED ALWAYS AS(a+1);"
        ))?;
        let table = database
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?
            .unwrap();
        let column = table.columns(&database)?.find(|column| column.column_name() == "v").unwrap();
        assert_eq!(column.generation_storage(&database), expected);
    }
    Ok(())
}
