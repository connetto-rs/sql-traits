//! Tests that a constraint added `NOT VALID` reads back unvalidated until
//! `ALTER TABLE ... VALIDATE CONSTRAINT` validates it.
//!
//! Every expectation here was measured against PostgreSQL 18 by reading
//! `pg_constraint.convalidated` after each statement.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, InheritedChange},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn table<'db>(database: &'db ParserDB, table_name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
}

/// Whether each check constraint of the table is validated, keyed by name.
fn checks(database: &ParserDB, table_name: &str) -> Vec<(String, bool)> {
    table(database, table_name)
        .check_constraints(database)
        .expect("table is in this database")
        .map(|check| {
            (
                check
                    .attribute()
                    .name
                    .as_ref()
                    .map_or_else(|| "<unnamed>".to_owned(), |name| name.value.clone()),
                check.is_validated(database).expect("check is in this database"),
            )
        })
        .collect()
}

/// Whether each foreign key of the table is validated, keyed by name.
fn foreign_keys(database: &ParserDB, table_name: &str) -> Vec<(String, bool)> {
    table(database, table_name)
        .foreign_keys(database)
        .expect("table is in this database")
        .map(|key| {
            (
                key.foreign_key_name().unwrap_or("<unnamed>").to_owned(),
                key.is_validated(database).expect("key is in this database"),
            )
        })
        .collect()
}

fn named(name: &str, validated: bool) -> Vec<(String, bool)> {
    vec![(name.to_owned(), validated)]
}

#[test]
fn not_valid_check_reads_back_unvalidated_until_validated() {
    let added = "CREATE TABLE orders (id INT, amount INT);
         ALTER TABLE orders ADD CONSTRAINT amount_positive CHECK (amount > 0) NOT VALID;";
    assert_eq!(checks(&database(added), "orders"), named("amount_positive", false));

    let validated = format!("{added} ALTER TABLE orders VALIDATE CONSTRAINT amount_positive;");
    assert_eq!(checks(&database(&validated), "orders"), named("amount_positive", true));
}

#[test]
fn not_valid_foreign_key_reads_back_unvalidated_until_validated() {
    let added = "CREATE TABLE customers (id INT PRIMARY KEY);
         CREATE TABLE orders (customer INT);
         ALTER TABLE orders ADD CONSTRAINT orders_customer
             FOREIGN KEY (customer) REFERENCES customers (id) NOT VALID;";
    assert_eq!(foreign_keys(&database(added), "orders"), named("orders_customer", false));

    let validated = format!("{added} ALTER TABLE orders VALIDATE CONSTRAINT orders_customer;");
    assert_eq!(foreign_keys(&database(&validated), "orders"), named("orders_customer", true));
}

#[test]
fn constraints_added_without_not_valid_are_validated() {
    let db = database(
        "CREATE TABLE customers (id INT PRIMARY KEY);
         CREATE TABLE orders (
             amount INT CONSTRAINT written_on_column CHECK (amount > 0),
             customer INT,
             CONSTRAINT declared CHECK (amount < 100),
             CONSTRAINT declared_key FOREIGN KEY (customer) REFERENCES customers (id)
         );
         ALTER TABLE orders ADD CONSTRAINT added CHECK (amount <> 7);",
    );
    assert!(checks(&db, "orders").iter().all(|(_, validated)| *validated));
    assert_eq!(foreign_keys(&db, "orders"), named("declared_key", true));
}

#[test]
fn adding_and_validating_in_one_statement_leaves_the_constraint_validated() {
    let db = database(
        "CREATE TABLE t (a INT);
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0) NOT VALID, VALIDATE CONSTRAINT ck;",
    );
    assert_eq!(checks(&db, "t"), named("ck", true));
}

#[test]
fn only_checks_and_foreign_keys_can_be_added_not_valid() {
    for constraint in ["CONSTRAINT u UNIQUE (a)", "CONSTRAINT pk PRIMARY KEY (a)"] {
        let sql = format!("CREATE TABLE t (a INT); ALTER TABLE t ADD {constraint} NOT VALID;");
        assert!(
            matches!(
                parse(&sql),
                Err(Error::NotValidRefused { ref table_name, ref constraint })
                    if table_name == "t" && constraint.starts_with("CONSTRAINT ")
            ),
            "should have been refused:\n{sql}"
        );
    }

    // Refused while the statement is read, before the table is looked up, so
    // `IF EXISTS` excuses nothing.
    assert!(matches!(
        parse("ALTER TABLE IF EXISTS absent ADD CONSTRAINT u UNIQUE (a) NOT VALID;"),
        Err(Error::NotValidRefused { .. })
    ));
}

#[test]
fn validating_a_name_the_table_does_not_hold_is_refused() {
    assert!(matches!(
        parse("CREATE TABLE t (a INT); ALTER TABLE t VALIDATE CONSTRAINT absent;"),
        Err(Error::ValidateConstraintNotFound { ref table_name, ref constraint_name })
            if table_name == "t" && constraint_name == "absent"
    ));

    // PostgreSQL names an unnamed constraint `t_a_check`, a name the model
    // never generates, so the validation cannot find it.
    assert!(matches!(
        parse(
            "CREATE TABLE t (a INT);
             ALTER TABLE t ADD CHECK (a > 0) NOT VALID;
             ALTER TABLE t VALIDATE CONSTRAINT t_a_check;"
        ),
        Err(Error::ValidateConstraintNotFound { .. })
    ));

    assert!(
        parse("ALTER TABLE IF EXISTS absent VALIDATE CONSTRAINT ck;").is_ok(),
        "IF EXISTS excuses an absent table"
    );
    assert!(matches!(
        parse("ALTER TABLE absent VALIDATE CONSTRAINT ck;"),
        Err(Error::AlterTableNotFound { .. })
    ));
}

#[test]
fn only_checks_foreign_keys_and_not_null_can_be_validated() {
    for sql in [
        "CREATE TABLE t (a INT, CONSTRAINT u UNIQUE (a)); ALTER TABLE t VALIDATE CONSTRAINT u;",
        "CREATE TABLE t (a INT CONSTRAINT u UNIQUE); ALTER TABLE t VALIDATE CONSTRAINT u;",
        "CREATE TABLE t (a INT CONSTRAINT u PRIMARY KEY); ALTER TABLE t VALIDATE CONSTRAINT u;",
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::ConstraintNotValidatable { ref table_name, ref constraint_name })
                    if table_name == "t" && constraint_name == "u"
            ),
            "should have been refused:\n{sql}"
        );
    }

    for sql in [
        "CREATE TABLE t (a INT CONSTRAINT c CHECK (a > 0)); ALTER TABLE t VALIDATE CONSTRAINT c;",
        "CREATE TABLE t (a INT CONSTRAINT c NOT NULL); ALTER TABLE t VALIDATE CONSTRAINT c;",
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE t (a INT CONSTRAINT c REFERENCES r (id));
         ALTER TABLE t VALIDATE CONSTRAINT c;",
    ] {
        assert!(parse(sql).is_ok(), "should have been accepted:\n{sql}");
    }
}

#[test]
fn a_not_valid_check_reaches_every_inheriting_table_unvalidated() {
    let added = "CREATE TABLE a1 (x INT);
         CREATE TABLE a2 () INHERITS (a1);
         CREATE TABLE a3 () INHERITS (a2);
         ALTER TABLE a1 ADD CONSTRAINT ck CHECK (x > 0) NOT VALID;";
    let db = database(added);
    for table_name in ["a1", "a2", "a3"] {
        assert_eq!(checks(&db, table_name), named("ck", false), "{table_name}");
    }

    let validated = database(&format!("{added} ALTER TABLE a1 VALIDATE CONSTRAINT ck;"));
    for table_name in ["a1", "a2", "a3"] {
        assert_eq!(checks(&validated, table_name), named("ck", true), "{table_name}");
    }

    // Validating a child's copy reaches below it and leaves its parent alone.
    let below = database(&format!("{added} ALTER TABLE a2 VALIDATE CONSTRAINT ck;"));
    assert_eq!(checks(&below, "a1"), named("ck", false));
    assert_eq!(checks(&below, "a2"), named("ck", true));
    assert_eq!(checks(&below, "a3"), named("ck", true));
}

#[test]
fn only_cannot_validate_a_check_the_tables_below_also_hold() {
    assert!(matches!(
        parse(
            "CREATE TABLE parent (a INT);
             CREATE TABLE child () INHERITS (parent);
             ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
             ALTER TABLE ONLY parent VALIDATE CONSTRAINT ck;"
        ),
        Err(Error::OnlyRefusedWithChildren {
            ref table_name,
            change: InheritedChange::ValidateConstraint,
        }) if table_name == "parent"
    ));

    // A table without children, a check already validated, and a check that
    // never travels all take `ONLY`.
    for sql in [
        "CREATE TABLE parent (a INT);
         ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         ALTER TABLE ONLY parent VALIDATE CONSTRAINT ck;",
        "CREATE TABLE parent (a INT, CONSTRAINT ck CHECK (a > 0));
         CREATE TABLE child () INHERITS (parent);
         ALTER TABLE ONLY parent VALIDATE CONSTRAINT ck;",
        "CREATE TABLE parent (a INT);
         CREATE TABLE child () INHERITS (parent);
         ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NO INHERIT NOT VALID;
         ALTER TABLE ONLY parent VALIDATE CONSTRAINT ck;",
    ] {
        let db = database(sql);
        assert_eq!(checks(&db, "parent"), named("ck", true), "{sql}");
    }
}

#[test]
fn a_table_created_below_a_not_valid_check_holds_a_validated_copy() {
    let db = database(
        "CREATE TABLE parent (a INT);
         ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE pt (a INT) PARTITION BY RANGE (a);
         ALTER TABLE pt ADD CONSTRAINT pck CHECK (a >= 0) NOT VALID;
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);",
    );
    assert_eq!(checks(&db, "parent"), named("ck", false));
    assert_eq!(checks(&db, "child"), named("ck", true));
    assert_eq!(checks(&db, "pt"), named("pck", false));
    assert_eq!(checks(&db, "pt1"), named("pck", true));
}

#[test]
fn a_child_holding_the_check_keeps_its_own_validation() {
    let db = database(
        "CREATE TABLE parent (a INT);
         CREATE TABLE child (a INT, CONSTRAINT ck CHECK (a > 0)) INHERITS (parent);
         ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;",
    );
    assert_eq!(checks(&db, "parent"), named("ck", false));
    assert_eq!(checks(&db, "child"), named("ck", true));

    let db = database(
        "CREATE TABLE parent (a INT);
         CREATE TABLE child () INHERITS (parent);
         ALTER TABLE child ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;",
    );
    assert_eq!(checks(&db, "child"), named("ck", false));

    assert!(matches!(
        parse(
            "CREATE TABLE parent (a INT);
             CREATE TABLE child () INHERITS (parent);
             ALTER TABLE ONLY child ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
             ALTER TABLE parent ADD CONSTRAINT ck CHECK (a > 0);"
        ),
        Err(Error::NotValidConstraintConflict { ref table_name, .. }) if table_name == "child"
    ));
}

#[test]
fn a_not_valid_foreign_key_reaches_every_partition_unvalidated() {
    let added = "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE pt (a INT) PARTITION BY RANGE (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
         ALTER TABLE pt ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES r (id) NOT VALID;
         CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (a);
         CREATE TABLE pt21 PARTITION OF pt2 FOR VALUES FROM (10) TO (15);";
    let db = database(added);
    for table_name in ["pt", "pt1", "pt2", "pt21"] {
        assert_eq!(foreign_keys(&db, table_name), named("fk", false), "{table_name}");
    }

    // A foreign key on a partitioned table is validated on every partition,
    // whether or not the statement says `ONLY`.
    let validated = database(&format!("{added} ALTER TABLE ONLY pt VALIDATE CONSTRAINT fk;"));
    for table_name in ["pt", "pt1", "pt2", "pt21"] {
        assert_eq!(foreign_keys(&validated, table_name), named("fk", true), "{table_name}");
    }

    let partition = database(&format!("{added} ALTER TABLE pt1 VALIDATE CONSTRAINT fk;"));
    assert_eq!(foreign_keys(&partition, "pt"), named("fk", false));
    assert_eq!(foreign_keys(&partition, "pt1"), named("fk", true));
}

#[test]
fn a_partition_holding_the_foreign_key_is_validated_by_either_side() {
    let schema = "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE pt (a INT) PARTITION BY RANGE (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);";

    let parent_valid = database(&format!(
        "{schema}
         ALTER TABLE pt1 ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES r (id) NOT VALID;
         ALTER TABLE pt ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES r (id);"
    ));
    assert_eq!(foreign_keys(&parent_valid, "pt1"), named("fk", true));

    let partition_valid = database(&format!(
        "{schema}
         ALTER TABLE pt1 ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES r (id);
         ALTER TABLE pt ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES r (id) NOT VALID;"
    ));
    assert_eq!(foreign_keys(&partition_valid, "pt"), named("fk", false));
    assert_eq!(foreign_keys(&partition_valid, "pt1"), named("fk", true));
}

#[test]
fn validation_follows_the_constraint_through_later_changes() {
    let renamed = database(
        "CREATE TABLE t (a INT, b INT);
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         ALTER TABLE t ADD CONSTRAINT other CHECK (b > 0);
         ALTER TABLE t RENAME COLUMN a TO c;
         ALTER TABLE t RENAME TO u;",
    );
    assert_eq!(checks(&renamed, "u"), vec![("ck".to_owned(), false), ("other".to_owned(), true)]);

    let readded = database(
        "CREATE TABLE t (a INT);
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         ALTER TABLE t DROP CONSTRAINT ck;
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0);",
    );
    assert_eq!(checks(&readded, "t"), named("ck", true));
}

#[test]
fn a_rolled_back_validation_leaves_the_constraint_unvalidated() {
    let db = database(
        "CREATE TABLE t (a INT);
         ALTER TABLE t ADD CONSTRAINT ck CHECK (a > 0) NOT VALID;
         BEGIN;
         ALTER TABLE t VALIDATE CONSTRAINT ck;
         ROLLBACK;",
    );
    assert_eq!(checks(&db, "t"), named("ck", false));
}
