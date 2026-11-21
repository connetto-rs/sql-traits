//! Virtual generated columns cannot participate in PostgreSQL index
//! constraints.
#![expect(clippy::expect_used, reason = "These cases require PostgreSQL constraint refusals.")]

use sql_traits::{errors::Error, prelude::ParserDB};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn virtual_index_reads_are_refused() {
    for storage in ["VIRTUAL", ""] {
        for index in [
            "CREATE INDEX indexed ON probe(v)",
            "CREATE INDEX indexed ON probe((v+1))",
            "CREATE INDEX indexed ON probe(base) WHERE v>0",
            "CREATE INDEX indexed ON probe(base) INCLUDE(v)",
            "CREATE UNIQUE INDEX indexed ON probe(v)",
            "CREATE INDEX indexed ON probe((probe.v+1))",
        ] {
            let sql = format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) {storage}); {index};"
            );
            assert!(matches!(parse(&sql), Err(Error::VirtualGeneratedIndex)), "{sql}");
        }
    }
}

#[test]
fn virtual_constraints_are_refused_in_every_declaration_form() {
    for storage in ["VIRTUAL", ""] {
        for (inline, table, refusal) in [
            ("UNIQUE", "UNIQUE(v)", Error::VirtualGeneratedUnique),
            ("PRIMARY KEY", "PRIMARY KEY(v)", Error::VirtualGeneratedPrimaryKey),
            (
                "REFERENCES target(id)",
                "FOREIGN KEY(v) REFERENCES target(id)",
                Error::VirtualGeneratedForeignKey,
            ),
        ] {
            for declaration in [
                format!(
                    "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) {storage} {inline})"
                ),
                format!(
                    "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) {storage}, {table})"
                ),
                format!(
                    "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) {storage}); ALTER TABLE probe ADD {table}"
                ),
                format!(
                    "CREATE TABLE probe(base int); ALTER TABLE probe ADD COLUMN v int GENERATED ALWAYS AS(base+1) {storage} {inline}"
                ),
                format!(
                    "CREATE TABLE probe(base int); ALTER TABLE probe ADD COLUMN v int GENERATED ALWAYS AS(base+1) {storage}, ADD {table}"
                ),
            ] {
                let sql = format!("CREATE TABLE target(id int PRIMARY KEY); {declaration};");
                let error = parse(&sql).expect_err(&sql);
                assert_eq!(
                    core::mem::discriminant(&error),
                    core::mem::discriminant(&refusal),
                    "{sql}: {error}"
                );
            }
        }
    }
}

#[test]
fn virtual_exclusion_and_included_columns_are_refused() {
    for constraint in [
        "EXCLUDE USING btree (v WITH =)",
        "EXCLUDE USING btree ((v+1) WITH =)",
        "EXCLUDE USING btree (base WITH =) WHERE (v>0)",
        "UNIQUE(base) INCLUDE(v)",
        "PRIMARY KEY(base) INCLUDE(v)",
    ] {
        for declaration in [
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1), {constraint})"
            ),
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1)); ALTER TABLE probe ADD {constraint}"
            ),
        ] {
            let error = parse(&declaration).expect_err(&declaration);
            if constraint.starts_with("PRIMARY KEY") {
                assert!(
                    matches!(error, Error::VirtualGeneratedPrimaryKey),
                    "{declaration}: {error}"
                );
            } else {
                assert!(matches!(error, Error::VirtualGeneratedUnique), "{declaration}: {error}");
            }
        }
    }
}

#[test]
fn stored_constraints_and_virtual_checks_are_accepted() {
    for constraint in [
        "UNIQUE(v)",
        "PRIMARY KEY(v)",
        "FOREIGN KEY(v) REFERENCES target(id)",
        "EXCLUDE USING btree(v WITH =)",
    ] {
        let sql = format!(
            "CREATE TABLE target(id int PRIMARY KEY); CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) STORED, {constraint});"
        );
        assert!(parse(&sql).is_ok(), "{sql}");
    }
    assert!(parse("CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) NOT NULL CHECK(v>0)); ALTER TABLE probe ADD COLUMN added int GENERATED ALWAYS AS(base+2) NOT NULL CHECK(added>0);").is_ok());
    assert!(parse("CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) STORED); CREATE INDEX indexed ON probe((v+1)) INCLUDE(v) WHERE v>0;").is_ok());
    assert!(ParserDB::parse::<SQLiteDialect>("CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) VIRTUAL UNIQUE); CREATE INDEX indexed ON probe(v);").is_ok());
}

#[test]
fn primary_key_precedes_unique_and_foreign_key_validation() {
    for constraints in ["UNIQUE PRIMARY KEY", "PRIMARY KEY UNIQUE"] {
        let sql = format!(
            "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) {constraints});"
        );
        assert!(matches!(parse(&sql), Err(Error::VirtualGeneratedPrimaryKey)), "{sql}");
    }
    assert!(matches!(
        parse(
            "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) REFERENCES missing(id) UNIQUE);"
        ),
        Err(Error::VirtualGeneratedUnique)
    ));
}

#[test]
fn quoted_names_and_nonvirtual_keys_remain_distinct() {
    assert!(parse("CREATE TABLE probe(base int, \"V\" int GENERATED ALWAYS AS(base+1), v int UNIQUE); CREATE INDEX indexed ON probe(v);").is_ok());
    assert!(matches!(
        parse(
            "CREATE TABLE probe(base int, \"V\" int GENERATED ALWAYS AS(base+1)); CREATE INDEX indexed ON probe(\"V\");"
        ),
        Err(Error::VirtualGeneratedIndex)
    ));
}

#[test]
fn foreign_key_target_errors_precede_virtual_source_refusal() {
    for target in ["missing", "target_nonunique"] {
        for declaration in [
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) REFERENCES {target}(id))"
            ),
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1)); ALTER TABLE probe ADD FOREIGN KEY(v) REFERENCES {target}(id)"
            ),
            format!(
                "CREATE TABLE probe(base int); ALTER TABLE probe ADD COLUMN v int GENERATED ALWAYS AS(base+1) REFERENCES {target}(id)"
            ),
        ] {
            let sql = format!("CREATE TABLE target_nonunique(id int); {declaration};");
            let error = parse(&sql).expect_err(&sql);
            assert!(!matches!(error, Error::VirtualGeneratedForeignKey), "{sql}");
        }
    }
}

#[test]
fn skipped_add_column_does_not_install_virtual_constraints() {
    assert!(parse("CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1)); ALTER TABLE probe ADD COLUMN IF NOT EXISTS v int GENERATED ALWAYS AS(base+1) UNIQUE;").is_ok());
}

#[test]
fn added_column_primary_key_precedes_unique() {
    for constraints in ["UNIQUE PRIMARY KEY", "PRIMARY KEY UNIQUE"] {
        let sql = format!(
            "CREATE TABLE probe(base int); ALTER TABLE probe ADD COLUMN v int GENERATED ALWAYS AS(base+1) {constraints};"
        );
        assert!(matches!(parse(&sql), Err(Error::VirtualGeneratedPrimaryKey)), "{sql}");
    }
}

#[test]
fn foreign_key_target_then_generated_action_then_virtual_storage_are_checked() {
    for target in ["missing", "target_nonunique", "target"] {
        for declaration in [
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1) REFERENCES {target}(id) ON UPDATE CASCADE)"
            ),
            format!(
                "CREATE TABLE probe(base int, v int GENERATED ALWAYS AS(base+1)); ALTER TABLE probe ADD FOREIGN KEY(v) REFERENCES {target}(id) ON UPDATE CASCADE"
            ),
        ] {
            let sql = format!(
                "CREATE TABLE target(id int PRIMARY KEY); CREATE TABLE target_nonunique(id int); {declaration};"
            );
            let error = parse(&sql).expect_err(&sql);
            assert_eq!(
                matches!(error, Error::InvalidForeignKeyUpdateActionForGeneratedColumn),
                target == "target",
                "{sql}: {error}"
            );
            assert!(!matches!(error, Error::VirtualGeneratedForeignKey), "{sql}");
        }
    }
}
