//! Partitioned indexes create and retain dependent partition copies.
//! Expectations were measured on PostgreSQL 18.7.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const ROOT: &str = "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);\n";
const PARTITION: &str = "CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);\n";
const ATTACH: &str = "ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);\n";

/// Returns every standalone index name exposed by the model.
fn names(sql: &str) -> Result<Vec<String>, Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(sql)?;
    let mut names: Vec<_> =
        database.indexes().filter_map(|index| index.name().map(str::to_owned)).collect();
    names.sort();
    Ok(names)
}

/// Copies use generated names independently of their parent's name.
#[test]
fn generated_copy_names() -> Result<(), Error> {
    for (statement, expected) in [
        ("CREATE INDEX pix ON r (id);", vec!["pix", "r1_id_idx"]),
        ("CREATE INDEX ON r (v);", vec!["r1_v_idx", "r_v_idx"]),
        ("CREATE INDEX pix ON r ((id + 1), v);", vec!["pix", "r1_expr_v_idx"]),
        ("CREATE INDEX pix ON r (id, v);", vec!["pix", "r1_id_v_idx"]),
        (
            "CREATE INDEX r1_id_idx ON r (v);\nCREATE INDEX pix ON r (id);",
            vec!["pix", "r1_id_idx", "r1_id_idx1", "r1_v_idx"],
        ),
    ] {
        assert_eq!(names(&format!("{ROOT}{PARTITION}{statement}"))?, expected, "{statement}");
    }
    Ok(())
}

/// Partitions receive indexes created before their creation or attachment.
#[test]
fn later_partitions_receive_indexes() -> Result<(), Error> {
    for partition in [PARTITION.to_owned(), format!("CREATE TABLE r1 (id INT, v INT);\n{ATTACH}")] {
        assert_eq!(
            names(&format!("{ROOT}CREATE INDEX pix ON r (id);\n{partition}"))?,
            ["pix", "r1_id_idx"]
        );
    }
    Ok(())
}

/// An index propagates through all partition levels.
#[test]
fn multilevel_copies() -> Result<(), Error> {
    let partitions =
        "CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);";
    assert_eq!(
        names(&format!("{ROOT}{partitions}\nCREATE INDEX pix ON r (id);"))?,
        ["pix", "r11_id_idx", "r1_id_idx"]
    );
    Ok(())
}

/// Attachment adopts equal indexes despite storage and ordering differences.
#[test]
fn adopts_equal_indexes() -> Result<(), Error> {
    for definition in [
        "(id)",
        "(id ASC)",
        "(id DESC)",
        "(id NULLS FIRST)",
        "(id NULLS LAST)",
        "(id) WITH (fillfactor=70)",
        "(id int4_ops)",
    ] {
        let sql = format!(
            "{ROOT}CREATE INDEX pix ON r (id);\nCREATE TABLE r1 (id INT, v INT);\nCREATE INDEX own ON r1 {definition};\n{ATTACH}"
        );
        assert_eq!(names(&sql)?, ["own", "pix"], "{definition}");
        assert!(
            matches!(ParserDB::parse::<PostgreSqlDialect>(&format!("{sql}DROP INDEX own;")), Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "own" && parent_index_name == "pix"),
            "{definition}"
        );
    }
    Ok(())
}

/// Distinct definitions need a separate partition copy.
#[test]
fn unequal_indexes_are_not_adopted() -> Result<(), Error> {
    for definition in [
        "CREATE UNIQUE INDEX own ON r1 (id);",
        "CREATE INDEX own ON r1 USING hash (id);",
        "CREATE INDEX own ON r1 (id) INCLUDE (v);",
        "CREATE INDEX own ON r1 (id) WHERE id > 0;",
    ] {
        let sql = format!(
            "{ROOT}CREATE INDEX pix ON r (id);\nCREATE TABLE r1 (id INT, v INT);\n{definition}\n{ATTACH}"
        );
        assert_eq!(names(&sql)?, ["own", "pix", "r1_id_idx"], "{definition}");
    }
    Ok(())
}

/// A parent's drop removes every ordinary copy.
#[test]
fn parent_drop_removes_copies() -> Result<(), Error> {
    assert_eq!(
        names(&format!("{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\nDROP INDEX pix;"))?,
        [] as [String; 0]
    );
    Ok(())
}

/// An attached copy cannot be dropped independently.
#[test]
fn copy_drop_is_refused() {
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\nDROP INDEX r1_id_idx;"
    ));
    assert!(
        matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "r1_id_idx" && parent_index_name == "pix"),
        "{result:?}"
    );
}

/// Detaching a partition makes its index independent.
#[test]
fn detached_copy_can_be_dropped() -> Result<(), Error> {
    assert_eq!(
        names(&format!(
            "{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\nALTER TABLE r DETACH PARTITION r1;\nDROP INDEX r1_id_idx;"
        ))?,
        ["pix"]
    );
    Ok(())
}

/// Renaming either endpoint retains its dependency.
#[test]
fn renames_retain_attachment() -> Result<(), Error> {
    let sql = format!(
        "{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\nALTER INDEX pix RENAME TO z;\nALTER INDEX r1_id_idx RENAME TO zz;\n"
    );
    assert_eq!(names(&sql)?, ["z", "zz"]);
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{sql}DROP INDEX zz;"));
    assert!(
        matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "zz" && parent_index_name == "z"),
        "{result:?}"
    );
    Ok(())
}

/// Table and column removal remove their index copies.
#[test]
fn table_and_column_drops() -> Result<(), Error> {
    assert_eq!(
        names(&format!("{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\nDROP TABLE r1;"))?,
        ["pix"]
    );
    assert_eq!(
        names(&format!(
            "{ROOT}{PARTITION}CREATE INDEX pix ON r (v);\nALTER TABLE r DROP COLUMN v;"
        ))?,
        [] as [String; 0]
    );
    Ok(())
}

/// A unique copy backs a foreign key to the partition.
#[test]
fn unique_copies_back_foreign_keys() -> Result<(), Error> {
    for before in [true, false] {
        let index = "CREATE UNIQUE INDEX pix ON r (id);\n";
        let sql = if before {
            format!("{ROOT}{index}{PARTITION}")
        } else {
            format!("{ROOT}{PARTITION}{index}")
        };
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{sql}CREATE TABLE c (x INT REFERENCES r1 (id));"
        ))?;
        let referencing =
            database.table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)?;
        assert!(referencing.is_some_and(|table| {
            table.foreign_keys(&database).is_ok_and(|mut keys| keys.next().is_some())
        }));
    }
    Ok(())
}

/// A constraint index adopted by a standalone parent retains both dependencies.
#[test]
fn adopted_constraint_dependencies() {
    let setup = format!(
        "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\nCREATE TABLE r1 (id INT CONSTRAINT own UNIQUE, v INT);\n{ATTACH}"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix;"));
    assert!(
        matches!(&result, Err(Error::IndexHasDependentConstraints { index_name }) if index_name == "pix"),
        "{result:?}"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{setup}ALTER TABLE r1 DROP CONSTRAINT own;"
    ));
    assert!(
        matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "own" && parent_index_name == "pix"),
        "{result:?}"
    );
}

/// Unique indexes adopt both immediate and deferred constraint indexes.
#[test]
fn constraint_index_adoption_and_cascade() -> Result<(), Error> {
    for key in ["PRIMARY KEY", "UNIQUE", "UNIQUE DEFERRABLE"] {
        let setup = format!(
            "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\nCREATE TABLE r1 (id INT CONSTRAINT own {key}, v INT);\n{ATTACH}"
        );
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{setup}ALTER TABLE r1 DROP CONSTRAINT own;"
        ));
        assert!(
            matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "own" && parent_index_name == "pix"),
            "{result:?}"
        );
        let database =
            ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix CASCADE;"))?;
        assert_eq!(database.indexes().count(), 0);
        let partition =
            database.table_by_target(TargetName::new("r1", false), IdentifierCase::AsWritten)?;
        assert!(partition.is_some_and(|table| {
            table.primary_key_columns(&database).is_ok_and(|mut columns| columns.next().is_none())
                && table.unique_indices(&database).is_ok_and(|mut indexes| indexes.next().is_none())
        }));
    }
    Ok(())
}

/// Expression equality, key ordering and null distinction govern adoption.
#[test]
fn expression_and_key_shape_adoption() -> Result<(), Error> {
    for (parent, child, expected) in [
        (
            "CREATE INDEX pix ON r ((id + 1));",
            "CREATE INDEX own ON r1 ((id + 1));",
            vec!["own", "pix"],
        ),
        (
            "CREATE INDEX pix ON r (id, v);",
            "CREATE INDEX own ON r1 (v, id);",
            vec!["own", "pix", "r1_id_v_idx"],
        ),
        (
            "CREATE UNIQUE INDEX pix ON r (id) NULLS NOT DISTINCT;",
            "CREATE UNIQUE INDEX own ON r1 (id);",
            vec!["own", "pix", "r1_id_idx"],
        ),
        (
            "CREATE UNIQUE INDEX pix ON r (id);",
            "CREATE UNIQUE INDEX own ON r1 (id);",
            vec!["own", "pix"],
        ),
    ] {
        assert_eq!(
            names(&format!("{ROOT}{parent}\nCREATE TABLE r1 (id INT, v INT);\n{child}\n{ATTACH}"))?,
            expected
        );
    }
    Ok(())
}

/// Creating the parent index adopts an equal index on an existing partition.
#[test]
fn creation_adopts_existing_copy() -> Result<(), Error> {
    let setup =
        format!("{ROOT}{PARTITION}CREATE INDEX own ON r1 (id);\nCREATE INDEX pix ON r (id);\n");
    assert_eq!(names(&setup)?, ["own", "pix"]);
    assert_eq!(names(&format!("{setup}DROP INDEX pix;"))?, [] as [String; 0]);
    Ok(())
}

/// Intermediate indexes retain their own dependent copies after detachment.
#[test]
fn multilevel_drop_and_detach() -> Result<(), Error> {
    let setup = format!(
        "{ROOT}CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);\nCREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);\nCREATE INDEX pix ON r (id);\n"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX r11_id_idx;"));
    assert!(
        matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "r11_id_idx" && parent_index_name == "r1_id_idx"),
        "{result:?}"
    );
    assert_eq!(names(&format!("{setup}DROP INDEX pix;"))?, [] as [String; 0]);
    assert_eq!(
        names(&format!("{setup}ALTER TABLE r DETACH PARTITION r1;\nDROP INDEX r1_id_idx;"))?,
        ["pix"]
    );
    Ok(())
}

/// Index attachments survive table and column renames.
#[test]
fn table_and_column_renames_preserve_attachments() {
    for statement in ["ALTER TABLE r RENAME TO root;", "ALTER TABLE r RENAME COLUMN id TO key;"] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{ROOT}{PARTITION}CREATE INDEX pix ON r (id);\n{statement}\nDROP INDEX r1_id_idx;"
        ));
        assert!(
            matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "r1_id_idx" && parent_index_name == "pix"),
            "{result:?}"
        );
    }
}

/// Foreign partitions receive no copies of nonunique indexes.
#[test]
fn foreign_partitions_have_no_indexes() -> Result<(), Error> {
    for statements in [
        "CREATE FOREIGN TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) SERVER k5_server;\nCREATE INDEX pix ON r (id);",
        "CREATE INDEX pix ON r (id);\nCREATE FOREIGN TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) SERVER k5_server;",
        "CREATE INDEX pix ON r (id);\nCREATE FOREIGN TABLE r1 (id INT, v INT) SERVER k5_server;\nALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);",
    ] {
        assert_eq!(names(&format!("{ROOT}{statements}"))?, ["pix"]);
    }
    Ok(())
}

/// An index already attached to a constraint index cannot serve a second
/// parent.
#[test]
fn constraint_copies_cannot_be_adopted_twice() -> Result<(), Error> {
    let sql = "CREATE TABLE r (id INT UNIQUE) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
CREATE UNIQUE INDEX pix ON r (id);";
    assert_eq!(names(sql)?, ["pix", "r1_id_idx"]);
    Ok(())
}

/// Attaching an indexed subtree preserves its existing descendant attachments.
#[test]
fn indexed_subtree_attachment() -> Result<(), Error> {
    let sql = "CREATE TABLE r (id INT) PARTITION BY RANGE (id);
CREATE UNIQUE INDEX pix ON r (id);
CREATE TABLE r1 (id INT) PARTITION BY RANGE (id);
CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);
CREATE UNIQUE INDEX own ON r1 (id);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);";
    assert_eq!(names(sql)?, ["own", "pix", "r11_id_idx"]);
    Ok(())
}

/// Cascading an adopted constraint index removes its inherited constraint
/// copies.
#[test]
fn adopted_constraint_subtree_drop() -> Result<(), Error> {
    let sql = "CREATE TABLE r (id INT) PARTITION BY RANGE (id);
CREATE UNIQUE INDEX pix ON r (id);
CREATE TABLE r1 (id INT CONSTRAINT own UNIQUE) PARTITION BY RANGE (id);
CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
DROP INDEX pix CASCADE;";
    let database = ParserDB::parse::<PostgreSqlDialect>(sql)?;
    assert_eq!(database.indexes().count(), 0);
    for name in ["r1", "r11"] {
        let partition =
            database.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)?;
        assert!(partition.is_some_and(|table| {
            table.unique_indices(&database).is_ok_and(|mut indexes| indexes.next().is_none())
        }));
    }
    Ok(())
}

/// Operator families, predicates and collations participate in index equality.
#[test]
fn operator_predicate_and_collation_adoption() -> Result<(), Error> {
    for (parent, child, copies) in [
        ("(v)", "(v text_pattern_ops)", vec!["r1_v_idx"]),
        ("(v varchar_pattern_ops)", "(v text_pattern_ops)", vec![]),
        ("(v pg_catalog.text_ops)", "(v text_ops)", vec![]),
        ("(id) WHERE id > 0", "(id) WHERE ((id > 0))", vec![]),
        ("(id) WHERE id > 0", "(id) WHERE id > 1", vec!["r1_id_idx"]),
        ("((id + 1))", "((id + 2))", vec!["r1_expr_idx"]),
        ("(v COLLATE \"C\")", "(v COLLATE \"C\")", vec![]),
        ("(v COLLATE \"C\")", "(v COLLATE \"POSIX\")", vec!["r1_v_idx"]),
        ("(v)", "(v COLLATE \"C\")", vec![]),
    ] {
        let sql = format!(
            "CREATE TABLE r (id INT, v TEXT COLLATE \"C\") PARTITION BY RANGE (id);\nCREATE INDEX pix ON r {parent};\nCREATE TABLE r1 (id INT, v TEXT COLLATE \"C\");\nCREATE INDEX own ON r1 {child};\n{ATTACH}"
        );
        let expected: Vec<_> = ["own", "pix"].into_iter().chain(copies).collect();
        assert_eq!(names(&sql)?, expected, "{parent} and {child}");
    }
    Ok(())
}

/// A parent index cannot disappear while a foreign key depends on its copy.
#[test]
fn foreign_key_copy_drop_dependencies() -> Result<(), Error> {
    let setup = format!(
        "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\n{PARTITION}CREATE TABLE f (id INT REFERENCES r1(id));\n"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix;"));
    assert!(
        matches!(&result, Err(Error::KeyReferenced { key_name, table_name, referencing_table, dropping_index: true }) if key_name == "pix" && table_name == "r" && referencing_table == "f"),
        "{result:?}"
    );
    for cascade in ["", " CASCADE"] {
        let result =
            ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX r1_id_idx{cascade};"));
        assert!(
            matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "r1_id_idx" && parent_index_name == "pix"),
            "{result:?}"
        );
    }
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix CASCADE;"))?;
    assert_eq!(database.indexes().count(), 0);
    let table = database
        .table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("f"))?;
    assert_eq!(table.foreign_keys(&database)?.count(), 0);
    Ok(())
}

/// An adopted constraint copy retains its parent dependency before foreign
/// keys.
#[test]
fn adopted_constraint_foreign_key_drop_dependencies() -> Result<(), Error> {
    let setup = format!(
        "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\nCREATE TABLE r1 (id INT CONSTRAINT own UNIQUE, v INT);\n{ATTACH}CREATE TABLE f (id INT REFERENCES r1(id));\n"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix;"));
    assert!(
        matches!(&result, Err(Error::IndexHasDependentConstraints { index_name }) if index_name == "pix"),
        "{result:?}"
    );
    for operation in ["DROP INDEX own", "ALTER TABLE r1 DROP CONSTRAINT own"] {
        for cascade in ["", " CASCADE"] {
            let result =
                ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}{operation}{cascade};"));
            assert!(
                matches!(&result, Err(Error::IndexRequiredByParent { index_name, parent_index_name }) if index_name == "own" && parent_index_name == "pix"),
                "{result:?}"
            );
        }
    }
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix CASCADE;"))?;
    assert_eq!(database.indexes().count(), 0);
    let partition = database
        .table_by_target(TargetName::new("r1", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("r1"))?;
    assert_eq!(partition.unique_indices(&database)?.count(), 0);
    let table = database
        .table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("f"))?;
    assert_eq!(table.foreign_keys(&database)?.count(), 0);
    Ok(())
}

/// Foreign keys follow descendant copies and remain bound after detachment.
#[test]
fn foreign_key_descendant_and_detached_dependencies() -> Result<(), Error> {
    let setup = format!(
        "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\nCREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);\nCREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);\nCREATE TABLE f (id INT REFERENCES r11(id));\n"
    );
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix;"));
    assert!(
        matches!(&result, Err(Error::KeyReferenced { key_name, table_name, referencing_table, dropping_index: true }) if key_name == "pix" && table_name == "r" && referencing_table == "f"),
        "{result:?}"
    );
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix CASCADE;"))?;
    assert_eq!(database.indexes().count(), 0);
    let table = database
        .table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("f"))?;
    assert_eq!(table.foreign_keys(&database)?.count(), 0);

    let setup = format!(
        "{ROOT}CREATE UNIQUE INDEX pix ON r (id);\n{PARTITION}CREATE TABLE f (id INT REFERENCES r1(id));\nALTER TABLE r DETACH PARTITION r1;\n"
    );
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX pix;"))?;
    assert_eq!(database.indexes().count(), 1);
    assert_eq!(database.indexes().next().and_then(|index| index.name()), Some("r1_id_idx"));
    let table = database
        .table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("f"))?;
    assert_eq!(table.foreign_keys(&database)?.count(), 1);
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX r1_id_idx;"));
    assert!(
        matches!(&result, Err(Error::KeyReferenced { key_name, table_name, referencing_table, dropping_index: true }) if key_name == "r1_id_idx" && table_name == "r1" && referencing_table == "f"),
        "{result:?}"
    );
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("{setup}DROP INDEX r1_id_idx CASCADE;"))?;
    assert_eq!(database.indexes().count(), 1);
    assert_eq!(database.indexes().next().and_then(|index| index.name()), Some("pix"));
    let table = database
        .table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
        .ok_or_else(|| sql_traits::errors::ObjectKind::Table.not_in_database("f"))?;
    assert_eq!(table.foreign_keys(&database)?.count(), 0);
    Ok(())
}
