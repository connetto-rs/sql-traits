//! PostgreSQL merges equal keys declared together, as measured on PostgreSQL
//! 18.7.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Parses PostgreSQL schema statements.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Counts the unique keys of `r`.
fn unique_keys(database: &ParserDB) -> usize {
    database
        .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .unique_indices(database)
        .expect("table belongs to database")
        .count()
}

#[test]
fn primary_key_absorbs_column_unique() {
    let schema = "CREATE TABLE r (id INT PRIMARY KEY UNIQUE);";
    let database = parse(schema).expect("merged keys");
    assert_eq!(unique_keys(&database), 1);
    assert_eq!(
        database
            .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .primary_key_columns(&database)
            .expect("table belongs to database")
            .count(),
        1
    );
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_id_key;")).is_err());
    parse(&format!("{schema} CREATE TABLE c (id INT REFERENCES r);"))
        .expect("primary key reference");
}

#[test]
fn primary_key_takes_first_written_name() {
    for definition in [
        "id INT PRIMARY KEY, CONSTRAINT u UNIQUE (id)",
        "id INT UNIQUE, CONSTRAINT u PRIMARY KEY (id)",
    ] {
        let schema = format!("CREATE TABLE r ({definition});");
        assert_eq!(unique_keys(&parse(&schema).expect("merged keys")), 1);
        let database =
            parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT u;")).expect("name transferred");
        assert_eq!(unique_keys(&database), 0);
        assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_pkey;")).is_err());
    }
}

#[test]
fn unique_duplicates_keep_first_available_name() {
    for definition in [
        "CONSTRAINT a UNIQUE (id), CONSTRAINT b UNIQUE (id)",
        "UNIQUE (id), CONSTRAINT a UNIQUE (id)",
        "CONSTRAINT a UNIQUE (id), UNIQUE (id), CONSTRAINT c UNIQUE (id)",
    ] {
        let schema = format!("CREATE TABLE r (id INT, {definition});");
        assert_eq!(unique_keys(&parse(&schema).expect("merged keys")), 1);
        parse(&format!("{schema} CREATE INDEX b ON r(id); CREATE INDEX c ON r(id); ALTER TABLE r DROP CONSTRAINT a;"))
            .expect("discarded names remain free");
    }
}

#[test]
fn generated_names_count_only_surviving_keys() {
    let schema = "CREATE TABLE r (id INT UNIQUE UNIQUE);";
    assert_eq!(unique_keys(&parse(schema).expect("merged column keys")), 1);
    parse(&format!("{schema} ALTER TABLE r ADD UNIQUE (id); ALTER TABLE r DROP CONSTRAINT r_id_key1; ALTER TABLE r DROP CONSTRAINT r_id_key;"))
        .expect("generated names belong to surviving declarations");
}

#[test]
fn different_key_properties_do_not_merge() {
    for definition in [
        "a INT, b INT, PRIMARY KEY (a,b), UNIQUE (b,a)",
        "id INT PRIMARY KEY, UNIQUE (id) DEFERRABLE",
        "id INT PRIMARY KEY, UNIQUE NULLS NOT DISTINCT (id)",
        "id INT, a INT, UNIQUE (id) INCLUDE (a), UNIQUE (id)",
        "\"Id\" INT UNIQUE, id INT UNIQUE",
    ] {
        assert_eq!(
            unique_keys(&parse(&format!("CREATE TABLE r ({definition});")).expect("distinct keys")),
            2
        );
    }
}

#[test]
fn add_column_merges_its_key_options() {
    let schema = "CREATE TABLE r (a INT); ALTER TABLE r ADD COLUMN id INT PRIMARY KEY UNIQUE;";
    assert_eq!(unique_keys(&parse(schema).expect("merged added keys")), 1);
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_id_key;")).is_err());
    parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_pkey;"))
        .expect("primary key survives");
}

#[test]
fn add_constraint_subcommands_keep_distinct_keys() {
    let schema = "CREATE TABLE r (id INT); ALTER TABLE r ADD CONSTRAINT a UNIQUE (id), ADD CONSTRAINT b UNIQUE (id);";
    assert_eq!(unique_keys(&parse(schema).expect("separate subcommands")), 2);
    parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT a, DROP CONSTRAINT b;"))
        .expect("both keys exist");
    assert_eq!(
        unique_keys(
            &parse(
                "CREATE TABLE r (id INT PRIMARY KEY); ALTER TABLE r ADD CONSTRAINT u UNIQUE (id);"
            )
            .expect("separate statements")
        ),
        2
    );
}

#[test]
fn partition_declarations_merge_duplicate_keys() {
    let schema = "CREATE TABLE p (id INT) PARTITION BY RANGE(id); CREATE TABLE r PARTITION OF p (CONSTRAINT a UNIQUE(id), CONSTRAINT b UNIQUE(id)) FOR VALUES FROM (0) TO (10);";
    assert_eq!(unique_keys(&parse(schema).expect("partition keys")), 1);
    parse(&format!("{schema} CREATE INDEX b ON r(id); ALTER TABLE r DROP CONSTRAINT a;"))
        .expect("one partition key");
}

#[test]
fn exclusion_duplicates_release_discarded_name() {
    let schema = "CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE USING btree (id WITH =), CONSTRAINT y EXCLUDE USING btree (id WITH =));";
    parse(&format!("{schema} CREATE INDEX y ON r(id); ALTER TABLE r DROP CONSTRAINT x;"))
        .expect("one exclusion key");
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT y;")).is_err());
}

#[test]
fn equivalent_explicit_defaults_merge() {
    for definition in [
        "id INT, CONSTRAINT a UNIQUE (ID), CONSTRAINT b UNIQUE (id) NOT DEFERRABLE INITIALLY IMMEDIATE",
        "id INT PRIMARY KEY, UNIQUE NULLS DISTINCT (id)",
        "id INT, a INT, CONSTRAINT first UNIQUE (ID) INCLUDE(A), CONSTRAINT second UNIQUE(id) INCLUDE(a)",
    ] {
        assert_eq!(
            unique_keys(
                &parse(&format!("CREATE TABLE r ({definition});")).expect("equivalent defaults")
            ),
            1
        );
    }
}

#[test]
fn initial_timing_distinguishes_keys() {
    let database = parse("CREATE TABLE r (id INT, CONSTRAINT first UNIQUE (id) DEFERRABLE INITIALLY IMMEDIATE, CONSTRAINT second UNIQUE(id) DEFERRABLE INITIALLY DEFERRED);").expect("different initial timing");
    assert_eq!(unique_keys(&database), 2);
}

#[test]
fn exclusion_predicates_distinguish_keys() {
    parse("CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE (id WITH =) WHERE (id > 0), CONSTRAINT y EXCLUDE (id WITH =) WHERE (id > 1)); ALTER TABLE r DROP CONSTRAINT x, DROP CONSTRAINT y;").expect("different exclusion predicates");
}

#[test]
fn renamed_survivor_keeps_its_identity() {
    parse("CREATE TABLE r (id INT CONSTRAINT first UNIQUE CONSTRAINT second UNIQUE); ALTER TABLE r RENAME COLUMN id TO value; ALTER TABLE r RENAME CONSTRAINT first TO held; ALTER TABLE r DROP CONSTRAINT held; CREATE INDEX second ON r(value);").expect("surviving key follows renames");
}

#[test]
fn exclusion_predicates_merge_equal_parse_trees() {
    for predicate in ["ID > 0", "((id > 0))", "(id) > 0"] {
        let schema = format!(
            "CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE (id WITH =) WHERE (id > 0), CONSTRAINT y EXCLUDE USING btree (id WITH =) WHERE ({predicate}));"
        );
        assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT y;")).is_err());
        parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT x;"))
            .expect("one exclusion survives");
    }
}

#[test]
fn exclusion_predicates_preserve_distinct_parse_trees() {
    for predicate in ["r.id > 0", "id > 0::int", "id > '0'", "0 < id"] {
        parse(&format!("CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE (id WITH =) WHERE (id > 0), CONSTRAINT y EXCLUDE (id WITH =) WHERE ({predicate})); ALTER TABLE r DROP CONSTRAINT x, DROP CONSTRAINT y;")).expect("both exclusion predicates survive");
    }
}

#[test]
fn exclusion_column_and_expression_remain_distinct() {
    parse("CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE (id WITH =), CONSTRAINT y EXCLUDE ((ID) WITH =)); ALTER TABLE r DROP CONSTRAINT x, DROP CONSTRAINT y;").expect("column and expression are distinct index elements");
}

#[test]
fn added_primary_key_takes_merged_option_name() {
    let schema =
        "CREATE TABLE r (a INT); ALTER TABLE r ADD COLUMN id INT PRIMARY KEY CONSTRAINT u UNIQUE;";
    assert_eq!(unique_keys(&parse(schema).expect("merged named column keys")), 1);
    parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT u;")).expect("transferred name");
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_pkey;")).is_err());
}

#[test]
fn merged_written_name_does_not_reserve_generated_name() {
    parse("CREATE TABLE r (id INT UNIQUE, CONSTRAINT a UNIQUE(id)); ALTER TABLE r ADD UNIQUE(id); ALTER TABLE r DROP CONSTRAINT a, DROP CONSTRAINT r_id_key;").expect("unnamed survivor gets the first free generated name");
}

#[test]
fn table_key_before_column_keeps_its_name() {
    for definition in [
        "CONSTRAINT a UNIQUE (id), id INT CONSTRAINT b UNIQUE",
        "x INT, CONSTRAINT a UNIQUE (id), id INT CONSTRAINT b UNIQUE",
    ] {
        let schema = format!("CREATE TABLE r ({definition});");
        assert_eq!(unique_keys(&parse(&schema).expect("merged keys")), 1);
        parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT a;"))
            .expect("first written key survives");
        assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT b;")).is_err());
    }
}

#[test]
fn exclusion_with_and_without_predicate_remain_distinct() {
    parse("CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE (id WITH =) WHERE (id > 0), CONSTRAINT y EXCLUDE (id WITH =)); ALTER TABLE r DROP CONSTRAINT x, DROP CONSTRAINT y;")
        .expect("both exclusions survive");
}

#[test]
fn equal_exclusion_expressions_merge() {
    let schema = "CREATE TABLE r (id INT, CONSTRAINT x EXCLUDE ((id + 1) WITH =), CONSTRAINT y EXCLUDE ((id + 1) WITH =));";
    parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT x;")).expect("first exclusion survives");
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT y;")).is_err());
}

#[test]
fn unnamed_table_primary_key_takes_merged_unique_name() {
    let schema = "CREATE TABLE r (id INT, PRIMARY KEY (id), CONSTRAINT u UNIQUE (id));";
    let database = parse(schema).expect("merged table keys");
    assert_eq!(unique_keys(&database), 1);
    let table = database
        .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(table.primary_key_columns(&database).expect("table belongs to database").count(), 1);
    let database = parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT u;"))
        .expect("primary key takes unique name");
    assert_eq!(unique_keys(&database), 0);
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_pkey;")).is_err());
}

#[test]
fn unnamed_exclusion_takes_merged_exclusion_name() {
    let schema = "CREATE TABLE r (id INT, EXCLUDE (id WITH =), CONSTRAINT x EXCLUDE (id WITH =));";
    parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT x;"))
        .expect("exclusion takes written name");
    assert!(parse(&format!("{schema} ALTER TABLE r DROP CONSTRAINT r_id_excl;")).is_err());
}
