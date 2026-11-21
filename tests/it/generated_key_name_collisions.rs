//! Generated constraint and index names avoid occupied namespaces.
//! Expectations were measured on PostgreSQL 18.7.

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

const KINDS: [(&str, &str); 7] = [
    ("pkey", "PRIMARY KEY (id)"),
    ("key", "UNIQUE (id)"),
    ("excl", "EXCLUDE (id WITH =)"),
    ("fkey", "FOREIGN KEY (id) REFERENCES target (id)"),
    ("check", "CHECK (id > 0)"),
    ("not_null", "NOT NULL id"),
    ("idx", ""),
];

const HOLDERS: [(&str, &str); 6] = [
    ("pkey", "PRIMARY KEY (other)"),
    ("key", "UNIQUE (other)"),
    ("excl", "EXCLUDE (other WITH =)"),
    ("fkey", "FOREIGN KEY (other) REFERENCES target (id)"),
    ("check", "CHECK (other > 0)"),
    ("not_null", "NOT NULL other"),
];

/// Builds a table with an inline or subsequently added unnamed constraint.
fn statement(prefix: &str, named: Option<&str>, kind: &str, clause: &str, alter: bool) -> String {
    let named = named.map_or_else(String::new, |named| format!(", {named}"));
    if alter || kind == "idx" {
        let addition = if kind == "idx" {
            "CREATE INDEX ON r (id);".to_owned()
        } else {
            format!("ALTER TABLE r ADD {clause};")
        };
        format!("{prefix}CREATE TABLE r (id INT, other INT{named});\n{addition}\n")
    } else {
        format!("{prefix}CREATE TABLE r (id INT, other INT{named}, {clause});\n")
    }
}

/// Checks that the generated name reaches the new constraint or index.
fn has_generated_name(sql: &str, kind: &str, expected: &str) -> bool {
    let Ok(database) = ParserDB::parse::<PostgreSqlDialect>(sql) else {
        return false;
    };
    let Ok(Some(table)) =
        database.table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
    else {
        return false;
    };
    let named = match kind {
        "pkey" | "key" => {
            table.unique_indices(&database).is_ok_and(|mut keys| {
                keys.any(|key| {
                    key.constraint_name(&database)
                        .is_ok_and(|name| name.as_deref() == Some(expected))
                })
            })
        }
        "fkey" => {
            table.foreign_keys(&database).is_ok_and(|mut keys| {
                keys.any(|key| {
                    key.constraint_name(&database)
                        .is_ok_and(|name| name.as_deref() == Some(expected))
                })
            })
        }
        "check" => {
            table.check_constraints(&database).is_ok_and(|mut checks| {
                checks.any(|check| {
                    check
                        .constraint_name(&database)
                        .is_ok_and(|name| name.as_deref() == Some(expected))
                })
            })
        }
        "not_null" => {
            table.columns(&database).is_ok_and(|mut columns| {
                columns.any(|column| {
                    column.column_name() == "id"
                        && column
                            .not_null_constraint_name(&database)
                            .is_ok_and(|name| name.as_deref() == Some(expected))
                })
            })
        }
        _ => {
            table
                .columns(&database)
                .is_ok_and(|mut columns| columns.any(|column| column.column_name() == "id"))
        }
    };
    if !named {
        return false;
    }
    let drop = if kind == "idx" {
        format!("DROP INDEX {expected};")
    } else {
        format!("ALTER TABLE r DROP CONSTRAINT {expected};")
    };
    ParserDB::parse::<PostgreSqlDialect>(&format!("{sql}{drop}")).is_ok()
}

/// Tests every generated kind against each constraint kind on both tables.
#[test]
fn constraint_collision_matrix() {
    let mut failures = Vec::new();
    for alter in [false, true] {
        for (kind, clause) in KINDS {
            let candidate =
                if kind == "pkey" { "r_pkey".to_owned() } else { format!("r_id_{kind}") };
            for same_table in [true, false] {
                for (holder, definition) in HOLDERS {
                    if same_table && kind == "pkey" && holder == "pkey" {
                        continue;
                    }
                    let named = format!("CONSTRAINT {candidate} {definition}");
                    let prefix = if same_table {
                        "CREATE TABLE target (id INT PRIMARY KEY);\n".to_owned()
                    } else {
                        format!(
                            "CREATE TABLE target (id INT PRIMARY KEY);\nCREATE TABLE existing (other INT, {named});\n"
                        )
                    };
                    let sql = statement(
                        &prefix,
                        same_table.then_some(named.as_str()),
                        kind,
                        clause,
                        alter,
                    );
                    let relation_collision =
                        !alter && same_table && kind == "pkey" && matches!(holder, "key" | "excl");
                    let constraint_collision = !alter
                        && same_table
                        && (matches!(kind, "pkey" | "key" | "excl") && holder == "fkey"
                            || matches!(kind, "check" | "not_null")
                                && matches!(holder, "pkey" | "key" | "excl" | "fkey")
                            || kind == "check" && holder == "not_null");
                    let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
                    let numbered = kind != "idx" || matches!(holder, "pkey" | "key" | "excl");
                    let expected = format!("{candidate}{}", if numbered { "1" } else { "" });
                    let matches = if relation_collision {
                        matches!(&result, Err(Error::RelationNameAlreadyTaken { object_kind: ObjectKind::UniqueIndex, conflicting_kind: ObjectKind::UniqueIndex, object_name }) if object_name == &candidate)
                    } else if constraint_collision {
                        matches!(&result, Err(Error::ConstraintAlreadyExists { table_name, constraint_name }) if table_name == "r" && constraint_name == &candidate)
                    } else {
                        has_generated_name(&sql, kind, &expected)
                    };
                    if !matches {
                        failures.push(format!("{alter}/{kind}/{same_table}/{holder}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Tests every generated kind against each schema relation kind.
#[test]
fn relation_collision_matrix() {
    let mut failures = Vec::new();
    for alter in [false, true] {
        for (kind, clause) in KINDS {
            let candidate =
                if kind == "pkey" { "r_pkey".to_owned() } else { format!("r_id_{kind}") };
            for (holder, declaration) in [
                ("index", format!("CREATE INDEX {candidate} ON target (id);")),
                ("table", format!("CREATE TABLE {candidate} (value INT);")),
                ("view", format!("CREATE VIEW {candidate} AS SELECT id FROM target;")),
                ("sequence", format!("CREATE SEQUENCE {candidate};")),
                (
                    "materialized_view",
                    format!("CREATE MATERIALIZED VIEW {candidate} AS SELECT id FROM target;"),
                ),
                ("composite_type", format!("CREATE TYPE {candidate} AS (value INT);")),
            ] {
                let sql = statement(
                    &format!("CREATE TABLE target (id INT PRIMARY KEY);\n{declaration}\n"),
                    None,
                    kind,
                    clause,
                    alter,
                );
                let expected = format!(
                    "{candidate}{}",
                    if matches!(kind, "pkey" | "key" | "excl" | "idx") { "1" } else { "" }
                );
                let matches = has_generated_name(&sql, kind, &expected);
                if !matches {
                    failures.push(format!("{alter}/{kind}/{holder}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A later explicit name cannot displace a name already generated in its stage.
#[test]
fn later_named_constraints_collide_in_creation_order() {
    for (kind, clause) in KINDS.into_iter().filter(|(kind, _)| *kind != "idx") {
        let candidate = if kind == "pkey" { "r_pkey".to_owned() } else { format!("r_id_{kind}") };
        for (holder, definition) in HOLDERS {
            if kind == "pkey" && holder == "pkey" {
                continue;
            }
            let sql = format!(
                "CREATE TABLE target (id INT PRIMARY KEY);\nCREATE TABLE r (id INT, other INT, {clause}, CONSTRAINT {candidate} {definition});\n"
            );
            let relation_collision =
                matches!(kind, "pkey" | "key" | "excl") && matches!(holder, "key" | "excl");
            let constraint_collision = holder == "fkey"
                || matches!(kind, "check" | "not_null")
                    && matches!(holder, "pkey" | "key" | "excl")
                || kind == "check" && matches!(holder, "check" | "not_null")
                || kind == "not_null" && holder == "not_null";
            if relation_collision {
                assert!(
                    matches!(
                        ParserDB::parse::<PostgreSqlDialect>(&sql),
                        Err(Error::RelationNameAlreadyTaken {
                            object_kind: ObjectKind::UniqueIndex,
                            conflicting_kind: ObjectKind::UniqueIndex,
                            object_name,
                        }) if object_name == candidate
                    ),
                    "{sql}"
                );
            } else if constraint_collision {
                assert!(
                    matches!(
                        ParserDB::parse::<PostgreSqlDialect>(&sql),
                        Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
                            if table_name == "r" && constraint_name == candidate
                    ),
                    "{sql}"
                );
            } else {
                assert!(has_generated_name(&sql, kind, &format!("{candidate}1")), "{sql}");
            }
        }
    }
}

/// Existing names follow their constraints through table and column changes.
#[test]
fn name_collisions_follow_schema_changes() {
    for (sql, expected) in [
        (
            "CREATE TABLE r (other INT, CONSTRAINT r_id_key CHECK (other > 0));\nALTER TABLE r ADD COLUMN id INT UNIQUE;",
            "r_id_key1",
        ),
        (
            "CREATE TABLE r (id INT, CONSTRAINT occupied CHECK (id > 0));\nALTER TABLE r RENAME CONSTRAINT occupied TO r_id_key;\nALTER TABLE r ADD UNIQUE (id);",
            "r_id_key1",
        ),
        (
            "CREATE TABLE r (id INT, CONSTRAINT r_id_key CHECK (id > 0));\nALTER TABLE r DROP CONSTRAINT r_id_key;\nALTER TABLE r ADD UNIQUE (id);",
            "r_id_key",
        ),
        (
            "CREATE TABLE old (id INT, CONSTRAINT r_id_key CHECK (id > 0));\nALTER TABLE old RENAME TO r;\nALTER TABLE r ADD UNIQUE (id);",
            "r_id_key1",
        ),
        (
            "CREATE TABLE r (old INT, CONSTRAINT r_id_key CHECK (old > 0));\nALTER TABLE r RENAME COLUMN old TO id;\nALTER TABLE r ADD UNIQUE (id);",
            "r_id_key1",
        ),
        (
            "CREATE TABLE parent (id INT, CONSTRAINT r_id_key CHECK (id > 0));\nCREATE TABLE r (UNIQUE (id)) INHERITS (parent);",
            "r_id_key1",
        ),
    ] {
        assert!(has_generated_name(sql, "key", expected), "{sql}");
    }
}

/// Attached partitions choose index names past their existing constraint names.
#[test]
fn partition_key_names_avoid_existing_constraints() {
    let sql = "CREATE TABLE parent (id INT PRIMARY KEY) PARTITION BY RANGE (id);\nCREATE TABLE r (id INT NOT NULL, CONSTRAINT r_pkey CHECK (id > 0));\nALTER TABLE parent ATTACH PARTITION r FOR VALUES FROM (0) TO (10);\nALTER TABLE parent DETACH PARTITION r;\n";
    assert!(has_generated_name(sql, "pkey", "r_pkey1"));
}

/// A partition's copied primary key precedes its locally declared check.
#[test]
fn partition_creation_refuses_a_later_named_check() {
    let sql = "CREATE TABLE parent (id INT PRIMARY KEY) PARTITION BY RANGE (id);\nCREATE TABLE r PARTITION OF parent (CONSTRAINT r_pkey CHECK (id > 0)) FOR VALUES FROM (0) TO (10);";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(sql),
        Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
            if table_name == "r" && constraint_name == "r_pkey"
    ));
}

/// Check and not-null names reserve the generated key name on inline
/// declarations.
#[test]
fn original_key_name_collisions() {
    for (sql, kind, expected) in [
        (
            "CREATE TABLE r (id INT, CONSTRAINT r_pkey CHECK (id > 0), PRIMARY KEY (id));",
            "pkey",
            "r_pkey1",
        ),
        (
            "CREATE TABLE r (id INT, CONSTRAINT r_id_key CHECK (id > 0));\nALTER TABLE r ADD UNIQUE (id);",
            "key",
            "r_id_key1",
        ),
        ("CREATE TABLE r (id INT CONSTRAINT r_id_key NOT NULL UNIQUE);", "key", "r_id_key1"),
        (
            "CREATE TABLE r (id INT, CONSTRAINT r_id_excl CHECK (id > 0), EXCLUDE (id WITH =));",
            "excl",
            "r_id_excl1",
        ),
    ] {
        assert!(has_generated_name(sql, kind, expected), "{sql}");
    }
}

/// Grouped alterations reserve not-null names before indexes and other
/// constraints.
#[test]
fn grouped_alterations_follow_constraint_passes() {
    for (kind, clause) in KINDS.into_iter().filter(|(kind, _)| *kind != "idx") {
        let candidate = if kind == "pkey" { "r_pkey".to_owned() } else { format!("r_id_{kind}") };
        for (holder, definition) in HOLDERS {
            if kind == "pkey" && holder == "pkey" {
                continue;
            }
            for reverse in [false, true] {
                let named = format!("CONSTRAINT {candidate} {definition}");
                let (first, second) =
                    if reverse { (clause, named.as_str()) } else { (named.as_str(), clause) };
                let sql = format!(
                    "CREATE TABLE target (id INT PRIMARY KEY);\nCREATE TABLE r (id INT, other INT);\nALTER TABLE r ADD {first}, ADD {second};\n"
                );
                let index_kind = matches!(kind, "pkey" | "key" | "excl");
                let index_holder = matches!(holder, "pkey" | "key" | "excl");
                let relation_collision = reverse && index_kind && index_holder;
                let constraint_collision = index_kind && matches!(holder, "check" | "fkey")
                    || kind == "not_null" && holder != "not_null"
                    || reverse
                        && matches!(kind, "check" | "fkey")
                        && matches!(holder, "check" | "fkey")
                    || reverse && kind == "not_null" && holder == "not_null";
                if relation_collision {
                    assert!(
                        matches!(
                            ParserDB::parse::<PostgreSqlDialect>(&sql),
                            Err(Error::RelationNameAlreadyTaken {
                                object_kind: ObjectKind::UniqueIndex,
                                conflicting_kind: ObjectKind::UniqueIndex,
                                object_name,
                            }) if object_name == candidate
                        ),
                        "{sql}"
                    );
                } else if constraint_collision {
                    assert!(
                        matches!(
                            ParserDB::parse::<PostgreSqlDialect>(&sql),
                            Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
                                if table_name == "r" && constraint_name == candidate
                        ),
                        "{sql}"
                    );
                } else {
                    assert!(has_generated_name(&sql, kind, &format!("{candidate}1")), "{sql}");
                }
            }
        }
    }
}
