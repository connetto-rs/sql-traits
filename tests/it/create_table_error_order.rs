//! PostgreSQL table creation refuses faults in transformation order.

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

type Refusal = fn(&Error) -> bool;

const RELATION_AND_INHERITANCE_CASES: &[(&str, &str, bool, Refusal)] = &[
    (
        "collation before occupied table",
        "CREATE TABLE probe(z int);
             CREATE TABLE probe(b text COLLATE absent_collation);",
        false,
        |error| matches!(error, Error::CollationNotFound { .. }),
    ),
    (
        "collation before parent lookup",
        "CREATE TABLE probe(b text COLLATE absent_collation) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::CollationNotFound { .. }),
    ),
    (
        "duplicate column before occupied table",
        "CREATE TABLE probe(z int); CREATE TABLE probe(c int,c int);",
        false,
        |error| matches!(error, Error::ColumnAlreadyExists { .. }),
    ),
    (
        "parent lookup before duplicate column",
        "CREATE TABLE probe(c int,c int) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::ParentTableNotFound { .. }),
    ),
    (
        "key column before occupied table",
        "CREATE TABLE probe(z int); CREATE TABLE probe(PRIMARY KEY(absent_column));",
        false,
        |error| matches!(error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. })),
    ),
    (
        "parent lookup before inherited key column",
        "CREATE TABLE probe(PRIMARY KEY(absent_column)) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::ParentTableNotFound { .. }),
    ),
    (
        "occupied table before foreign key",
        "CREATE TABLE probe(z int); CREATE TABLE probe(e int REFERENCES absent_parent(id));",
        false,
        |error| {
            matches!(error, Error::IdentifierLookupError(LookupError::TableLookupConflict { .. }))
        },
    ),
    (
        "parent lookup before foreign key",
        "CREATE TABLE probe(e int REFERENCES absent_parent(id)) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::ParentTableNotFound { .. }),
    ),
    (
        "identity before occupied table",
        "CREATE TABLE probe(z int); CREATE TABLE probe(g text GENERATED ALWAYS AS IDENTITY);",
        false,
        |error| matches!(error, Error::InvalidSequenceParameters { .. }),
    ),
    (
        "identity before parent lookup",
        "CREATE TABLE probe(g text GENERATED ALWAYS AS IDENTITY) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::InvalidSequenceParameters { .. }),
    ),
    (
        "second primary key before occupied table",
        "CREATE TABLE probe(z int); CREATE TABLE probe(h int PRIMARY KEY,i int PRIMARY KEY);",
        false,
        |error| matches!(error, Error::MultiplePrimaryKeys { .. }),
    ),
    (
        "second primary key before parent lookup",
        "CREATE TABLE probe(h int PRIMARY KEY,i int PRIMARY KEY) INHERITS (absent_parent);",
        false,
        |error| matches!(error, Error::MultiplePrimaryKeys { .. }),
    ),
    (
        "SQLite duplicate column before key lookup",
        "CREATE TABLE probe(a int,a int,PRIMARY KEY(absent_column));",
        true,
        |error| matches!(error, Error::ColumnAlreadyExists { .. }),
    ),
    (
        "occupied table before partition bound",
        "CREATE TABLE root(z int) PARTITION BY RANGE(z);
             CREATE TABLE probe(z int);
             CREATE TABLE probe PARTITION OF root FOR VALUES FROM(2) TO(1);",
        false,
        |error| {
            matches!(error, Error::IdentifierLookupError(LookupError::TableLookupConflict { .. }))
        },
    ),
    (
        "copy lookup before occupied table",
        "CREATE TABLE probe(z int); CREATE TABLE probe(LIKE absent_parent);",
        false,
        |error| matches!(error, Error::CopiedTableNotFound { .. }),
    ),
    (
        "occupied table before query output duplicates",
        "CREATE TABLE probe(z int); CREATE TABLE probe AS SELECT 1 AS a,2 AS a;",
        false,
        |error| {
            matches!(error, Error::IdentifierLookupError(LookupError::TableLookupConflict { .. }))
        },
    ),
];

#[test]
fn relation_and_inheritance_checks_keep_their_phase() {
    for (name, statement, sqlite, expected) in RELATION_AND_INHERITANCE_CASES {
        let result = if *sqlite {
            ParserDB::parse::<SQLiteDialect>(statement)
        } else {
            ParserDB::parse::<PostgreSqlDialect>(statement)
        };
        assert!(result.as_ref().is_err_and(expected), "{name}\n{statement}\n{result:?}");
    }
}

#[test]
fn every_transformation_phase_competes_in_both_orders() {
    let faults: &[(&str, Refusal)] = &[
        ("b text COLLATE absent_collation", |error| {
            matches!(error, Error::CollationNotFound { .. })
        }),
        ("c int,c int", |error| matches!(error, Error::ColumnAlreadyExists { .. })),
        ("PRIMARY KEY(absent_column)", |error| {
            matches!(error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. }))
        }),
        ("h int PRIMARY KEY,i int PRIMARY KEY", |error| {
            matches!(error, Error::MultiplePrimaryKeys { .. })
        }),
        ("d int REFERENCES target(id) NOT DEFERRABLE INITIALLY DEFERRED", |error| {
            matches!(error, Error::InitiallyDeferredNotDeferrable { .. })
        }),
        ("g text GENERATED ALWAYS AS IDENTITY", |error| {
            matches!(error, Error::InvalidSequenceParameters { .. })
        }),
        ("f int GENERATED ALWAYS AS(absent_expression_column+1) STORED", |error| {
            matches!(error, Error::UnknownColumn { .. })
        }),
        ("j int DEFAULT absent_expression_column", |error| {
            matches!(error, Error::ColumnReferenceInDefault { .. })
        }),
        ("payload json UNIQUE", |error| matches!(error, Error::MissingDefaultOperatorClass { .. })),
        ("e int REFERENCES absent_parent(id)", |error| {
            matches!(error, Error::ReferencedTableNotFoundForForeignKey { .. })
        }),
        ("a shell", |error| matches!(error, Error::TypeIsOnlyShell { .. })),
    ];
    let winners = [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [0, 1, 2, 3, 4, 5, 1, 1, 1, 1, 10],
        [0, 2, 2, 2, 4, 2, 2, 2, 2, 2, 10],
        [0, 3, 3, 3, 4, 3, 3, 3, 3, 3, 10],
        [4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4],
        [0, 5, 2, 3, 4, 5, 5, 5, 5, 5, 10],
        [0, 1, 2, 3, 4, 5, 6, 6, 6, 6, 10],
        [0, 1, 2, 3, 4, 5, 7, 7, 7, 7, 10],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 10],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        [10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10],
    ];
    for (first_position, (first, _)) in faults.iter().enumerate() {
        for (second_position, (second, _)) in faults.iter().enumerate() {
            if first_position == second_position {
                continue;
            }
            let statement = format!(
                "CREATE TYPE shell; CREATE TABLE target(id int PRIMARY KEY);
                 CREATE TABLE probe({first},{second});"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&statement);
            let (winner, expected) = faults[winners[first_position][second_position]];
            assert!(
                result.as_ref().is_err_and(expected),
                "{statement}\nexpected {winner}, got {result:?}"
            );
        }
    }
}

#[test]
fn defaults_and_generated_expressions_bind_in_column_order() {
    let cases: &[(&str, Refusal)] = &[
        (
            "a int DEFAULT nextval('absent_sequence'),
             b int GENERATED ALWAYS AS(absent_column) STORED",
            |error| matches!(error, Error::RelationNotFoundForDefault { .. }),
        ),
        (
            "b int GENERATED ALWAYS AS(absent_column) STORED,
             a int DEFAULT nextval('absent_sequence')",
            |error| matches!(error, Error::UnknownColumn { .. }),
        ),
        ("a int CHECK(absent_check>0),b int DEFAULT absent_default", |error| {
            matches!(error, Error::ColumnReferenceInDefault { .. })
        }),
    ];
    for (columns, expected) in cases {
        refuses(&format!("CREATE TABLE probe({columns});"), *expected);
    }
}

#[test]
fn structural_checks_keep_their_phase() {
    let cases: &[(&str, Refusal)] = &[
        (
            "CREATE TABLE probe(a int,PRIMARY KEY(absent_column),EXCLUDE USING gin(a WITH =));",
            |error| {
                matches!(error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. }))
            },
        ),
        ("CREATE TABLE probe(a int DEFAULT absent_column,EXCLUDE USING gin(a WITH =));", |error| {
            matches!(error, Error::ColumnReferenceInDefault { .. })
        }),
        (
            "CREATE TABLE probe(z int);CREATE TABLE probe(a int,EXCLUDE USING gin(a WITH =));",
            |error| {
                matches!(
                    error,
                    Error::IdentifierLookupError(LookupError::TableLookupConflict { .. })
                )
            },
        ),
        (
            "CREATE TABLE root(z int) PARTITION BY RANGE(z);
             CREATE TABLE probe PARTITION OF root(CHECK(absent_column>0))
             FOR VALUES FROM(2) TO(1);",
            |error| matches!(error, Error::PartitionBoundInvalid { .. }),
        ),
        (
            "CREATE TABLE root(z int) PARTITION BY RANGE(z);
             CREATE TABLE probe PARTITION OF root(CHECK((SELECT true)))
             FOR VALUES FROM(2) TO(1);",
            |error| matches!(error, Error::PartitionBoundInvalid { .. }),
        ),
        (
            "CREATE TABLE probe(a int CONSTRAINT first NOT NULL CONSTRAINT second NOT NULL,
             b text COLLATE absent_collation);",
            |error| matches!(error, Error::ConflictingNotNullConstraintNames { .. }),
        ),
        (
            "CREATE TABLE probe(a int,CONSTRAINT first NOT NULL a,CONSTRAINT second NOT NULL a,
             g text GENERATED ALWAYS AS IDENTITY);",
            |error| matches!(error, Error::InvalidSequenceParameters { .. }),
        ),
        (
            "CREATE TABLE probe(a int,CONSTRAINT first NOT NULL a,CONSTRAINT second NOT NULL a,
             c int,c int);",
            |error| matches!(error, Error::ColumnAlreadyExists { .. }),
        ),
        (
            "CREATE TABLE probe(a int,CONSTRAINT first NOT NULL a,CONSTRAINT second NOT NULL a,
             j int DEFAULT absent_column);",
            |error| matches!(error, Error::ColumnReferenceInDefault { .. }),
        ),
        (
            "CREATE TABLE probe(a int,CONSTRAINT first NOT NULL a,CONSTRAINT second NOT NULL a,
             CHECK(absent_column>0));",
            |error| matches!(error, Error::UnknownColumn { .. }),
        ),
        (
            "CREATE UNLOGGED TABLE probe(a int,PRIMARY KEY(absent_column)) PARTITION BY RANGE(a);",
            |error| {
                matches!(error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. }))
            },
        ),
        (
            "CREATE UNLOGGED TABLE probe(a int,g text GENERATED ALWAYS AS IDENTITY)
             PARTITION BY RANGE(a);",
            |error| matches!(error, Error::InvalidSequenceParameters { .. }),
        ),
        (
            "CREATE TABLE probe(a int,CHECK(absent_column>0) NO INHERIT) PARTITION BY RANGE(a);",
            |error| matches!(error, Error::UnknownColumn { .. }),
        ),
    ];
    for (statement, expected) in cases {
        refuses(statement, *expected);
    }
}

#[test]
fn index_names_are_checked_after_expressions_and_opclasses() {
    let cases: &[(&str, Refusal)] = &[
        (
            "a int CONSTRAINT occupied UNIQUE,b int GENERATED ALWAYS AS(absent_column) STORED",
            |error| matches!(error, Error::UnknownColumn { .. }),
        ),
        ("a json CONSTRAINT occupied UNIQUE", |error| {
            matches!(error, Error::MissingDefaultOperatorClass { .. })
        }),
        ("a int CONSTRAINT occupied UNIQUE,b int REFERENCES absent_parent(id)", |error| {
            matches!(error, Error::RelationNameAlreadyTaken { .. })
        }),
    ];
    for (columns, expected) in cases {
        refuses(&format!("CREATE TABLE occupied(z int);CREATE TABLE probe({columns});"), *expected);
    }
}

#[test]
fn inherited_primary_keys_follow_bounds_and_expressions() {
    let cases: &[(&str, &str, Refusal)] = &[
        ("PRIMARY KEY(b)", "FROM(1) TO(2)", |error| {
            matches!(error, Error::MultiplePrimaryKeys { .. })
        }),
        ("PRIMARY KEY(b)", "FROM(2) TO(1)", |error| {
            matches!(error, Error::PartitionBoundInvalid { .. })
        }),
        ("b DEFAULT absent_column,PRIMARY KEY(b)", "FROM(1) TO(2)", |error| {
            matches!(error, Error::ColumnReferenceInDefault { .. })
        }),
    ];
    for (columns, bound, expected) in cases {
        let statement = format!(
            "CREATE TABLE root(a int PRIMARY KEY,b int) PARTITION BY RANGE(a);
             CREATE TABLE probe PARTITION OF root({columns}) FOR VALUES {bound};"
        );
        refuses(&statement, *expected);
    }
}

#[test]
fn inherited_defaults_keep_the_parent_sequence() -> Result<(), sql_traits::errors::Error> {
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE parent(id serial);
         CREATE SCHEMA other;
         SET search_path = other;
         CREATE TABLE child() INHERITS (public.parent);",
    )?;
    Ok(())
}

#[test]
fn copied_defaults_keep_the_source_sequence() -> Result<(), sql_traits::errors::Error> {
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE parent(id serial);
         CREATE SCHEMA other;
         SET search_path = other;
         CREATE TABLE child(LIKE public.parent INCLUDING DEFAULTS);",
    )?;
    Ok(())
}

#[test]
#[expect(clippy::expect_used, reason = "The fixture defines both columns.")]
fn redeclared_inherited_columns_keep_creation() -> Result<(), sql_traits::errors::Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE parent AS SELECT 1 AS a;
         CREATE TABLE child(a int) INHERITS (parent);",
    )?;
    let metadata = |name| {
        let table = database
            .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous")
            .expect("table");
        let column = table
            .column("a", &database, IdentifierCase::AsWritten)
            .expect("unambiguous")
            .expect("column");
        database.column_metadata(column).expect("metadata")
    };
    assert_eq!(metadata("child"), metadata("parent"));
    Ok(())
}

#[test]
fn deferral_precedes_column_constraint_validation() {
    let cases: &[(&str, Refusal)] = &[
        (
            "CREATE TABLE probe(a text COLLATE absent_collation,
             FOREIGN KEY(a) REFERENCES target NOT DEFERRABLE INITIALLY DEFERRED);",
            |error| matches!(error, Error::InitiallyDeferredNotDeferrable { .. }),
        ),
        (
            "CREATE TABLE probe(a int CONSTRAINT first NOT NULL CONSTRAINT second NOT NULL
             REFERENCES target NOT DEFERRABLE INITIALLY DEFERRED);",
            |error| matches!(error, Error::InitiallyDeferredNotDeferrable { .. }),
        ),
    ];
    for (statement, expected) in cases {
        let result = ParserDB::parse::<PostgreSqlDialect>(statement);
        assert!(result.as_ref().is_err_and(expected), "{statement}\n{result:?}");
    }
}

fn refuses(statement: &str, expected: Refusal) {
    let result = ParserDB::parse::<PostgreSqlDialect>(statement);
    assert!(result.as_ref().is_err_and(expected), "{statement}\n{result:?}");
}
