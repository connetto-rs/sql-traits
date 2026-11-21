//! PostgreSQL adopts standalone unique indexes as constraints.
//! Expectations were measured on PostgreSQL 18.7.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Parses PostgreSQL schema statements.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Creates a nullable column and its standalone unique index.
const SETUP: &str = "CREATE TABLE r (id INT, v INT);
    CREATE UNIQUE INDEX ix ON r (id);";

/// Adoption supplies primary key columns for implicit references.
#[test]
fn primary_key_adopts_columns_and_binds_implicit_references() {
    let database = parse(&format!(
        "{SETUP}
         ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY USING INDEX ix;
         CREATE TABLE c (x INT REFERENCES r);
         CREATE INDEX ix ON r(v);"
    ))
    .expect("index is adopted");
    let table = database
        .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table");
    assert_eq!(table.primary_key_columns(&database).expect("key columns").count(), 1);
    assert!(
        !table
            .primary_key_columns(&database)
            .expect("key columns")
            .next()
            .expect("key column")
            .is_nullable(&database)
            .expect("nullability")
    );
}

/// An unnamed constraint inherits its index's name.
#[test]
fn unnamed_constraint_uses_index_name() {
    parse(&format!(
        "{SETUP}
         ALTER TABLE r ADD PRIMARY KEY USING INDEX ix;
         ALTER TABLE r DROP CONSTRAINT ix;
         CREATE INDEX ix ON r(id);"
    ))
    .expect("constraint owns index name");
}

/// Dropping the constraint releases its backing index.
#[test]
fn unique_constraint_drop_frees_both_names() {
    parse(&format!(
        "{SETUP}
         ALTER TABLE r ADD CONSTRAINT u UNIQUE USING INDEX ix;
         ALTER TABLE r DROP CONSTRAINT u;
         CREATE INDEX ix ON r(id);
         CREATE INDEX u ON r(v);"
    ))
    .expect("both names are free");
}

/// The adopted index belongs to the constraint even under `CASCADE`.
#[test]
fn adopted_index_cannot_be_dropped_separately() {
    assert!(matches!(
        parse(&format!(
            "{SETUP}
             ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY USING INDEX ix;
             DROP INDEX ix;"
        )),
        Err(Error::DropIndexNotFound { index_name }) if index_name == "ix"
    ));
    for suffix in ["", " CASCADE"] {
        let error = parse(&format!(
            "{SETUP}
             ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY USING INDEX ix;
             DROP INDEX pk{suffix};"
        ))
        .expect_err("constraint holds index");
        assert!(matches!(error, Error::IndexRequiredByConstraint { index_name, table_name }
            if index_name == "pk" && table_name == "r"));
    }
}

/// Adoption rejects indexes that cannot represent the requested key.
#[test]
fn unsuitable_indexes_are_refused() {
    assert!(matches!(
        parse("CREATE TABLE r(id INT);
               CREATE INDEX ix ON r(id);
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexNotUnique { index_name }) if index_name == "ix"
    ));
    assert!(matches!(
        parse("CREATE TABLE r(id INT);
               CREATE UNIQUE INDEX ix ON r(id) WHERE id > 0;
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexPartial { index_name }) if index_name == "ix"
    ));
    assert!(matches!(
        parse("CREATE TABLE r(id INT);
               CREATE UNIQUE INDEX ix ON r((id + 1));
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexExpressions { index_name }) if index_name == "ix"
    ));
    assert!(matches!(
        parse("CREATE TABLE r(id INT);
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexNotFound { index_name }) if index_name == "ix"
    ));
    assert!(matches!(
        parse("CREATE TABLE r(id INT);
               CREATE TABLE q(id INT);
               CREATE UNIQUE INDEX ix ON q(id);
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexWrongTable { index_name, table_name })
            if index_name == "ix" && table_name == "r"
    ));
    assert!(matches!(
        parse("CREATE TABLE r(id INT CONSTRAINT ix UNIQUE);
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"),
        Err(Error::AdoptedIndexAlreadyConstrained { index_name }) if index_name == "ix"
    ));
    assert!(matches!(
        parse(
            "CREATE TABLE r(id INT) PARTITION BY RANGE(id);
               CREATE UNIQUE INDEX ix ON r(id);
               ALTER TABLE r ADD UNIQUE USING INDEX ix;"
        ),
        Err(Error::AdoptedIndexOnPartitionedTable)
    ));
}

/// Constraints require ascending indexes with nulls last.
#[test]
fn nondefault_sorting_is_refused() {
    for ordering in ["id DESC", "id ASC NULLS FIRST", "id DESC NULLS LAST", "id DESC NULLS FIRST"] {
        let error = parse(&format!(
            "CREATE TABLE r(id INT);
             CREATE UNIQUE INDEX ix ON r({ordering});
             ALTER TABLE r ADD UNIQUE USING INDEX ix;"
        ))
        .expect_err("sorting refused");
        assert!(matches!(error, Error::AdoptedIndexSorting { index_name, column_number: 1 }
            if index_name == "ix"));
    }
}

/// Constraint renaming preserves quoted key and included columns.
#[test]
fn quoted_index_with_include_survives_constraint_rename() {
    let database = parse(
        "CREATE TABLE r(id INT, v INT);
         CREATE UNIQUE INDEX \"Ix\" ON r(id ASC NULLS LAST) INCLUDE(v);
         ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY USING INDEX \"Ix\";
         ALTER TABLE r RENAME CONSTRAINT pk TO renamed;",
    )
    .expect("quoted index adopted");
    let table = database
        .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table");
    assert_eq!(table.primary_key_columns(&database).expect("key columns").count(), 1);
    let key = table.unique_indices(&database).expect("unique keys").next().expect("adopted key");
    assert_eq!(
        key.include_columns(&database)
            .expect("included columns")
            .map(ColumnLike::column_name)
            .collect::<Vec<_>>(),
        ["v"]
    );
    assert!(matches!(parse(
        "CREATE TABLE r(id INT);
         CREATE UNIQUE INDEX ix ON r(id);
         ALTER TABLE r ADD PRIMARY KEY USING INDEX ix;
         ALTER TABLE r RENAME CONSTRAINT ix TO renamed;
         DROP INDEX renamed;"
    ), Err(Error::IndexRequiredByConstraint { index_name, table_name })
        if index_name == "renamed" && table_name == "r"));
}

/// Adoption resolves an unqualified index in the target schema.
#[test]
fn index_is_resolved_in_the_table_schema() {
    parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.r(id INT);
         CREATE UNIQUE INDEX ix ON s.r(id);
         ALTER TABLE s.r ADD PRIMARY KEY USING INDEX ix;
         CREATE TABLE c(x INT REFERENCES s.r);",
    )
    .expect("target schema index found");
}

/// A qualified adoption index name produces `Error::SqlParserError`.
#[test]
fn qualified_index_name_is_invalid_syntax() {
    assert!(matches!(
        parse(&format!(
            "{SETUP}
         ALTER TABLE r ADD UNIQUE USING INDEX public.ix;"
        )),
        Err(Error::SqlParserError { .. })
    ));
}

/// The AST exposes deferrability because no public trait reads it and foreign
/// key validation omits it.
#[test]
fn adopted_constraints_accept_deferrability() {
    for key in ["PRIMARY KEY", "UNIQUE"] {
        let database = parse(&format!(
            "{SETUP}
             ALTER TABLE r ADD {key} USING INDEX ix DEFERRABLE INITIALLY DEFERRED;"
        ))
        .expect("deferrable key accepted");
        let table = database
            .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("lookup")
            .expect("table");
        assert!(table.constraints.iter().any(|constraint| {
            let characteristics = match constraint {
                sqlparser::ast::TableConstraint::PrimaryKey(key) => key.characteristics,
                sqlparser::ast::TableConstraint::Unique(key) => key.characteristics,
                _ => None,
            };
            characteristics.is_some_and(|characteristics| {
                characteristics.deferrable == Some(true)
                    && characteristics.initially
                        == Some(sqlparser::ast::DeferrableInitial::Deferred)
            })
        }));
    }
}

/// Removing a primary key preserves its implied `NOT NULL`.
#[test]
fn primary_key_nullability_survives_constraint_drop() {
    let database = parse(&format!(
        "{SETUP}
         ALTER TABLE r ADD PRIMARY KEY USING INDEX ix;
         ALTER TABLE r DROP CONSTRAINT ix;"
    ))
    .expect("primary key removed");
    let table = database
        .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table");
    let column = table
        .columns(&database)
        .expect("columns")
        .find(|column| column.column_name() == "id")
        .expect("column");
    assert!(!column.is_nullable(&database).expect("nullability"));
}

/// An index in another schema cannot supply the target table's key.
#[test]
fn index_in_another_schema_is_not_visible_for_adoption() {
    let error = parse(
        "CREATE TABLE r(id INT);
         CREATE SCHEMA isolated;
         CREATE TABLE isolated.q(id INT);
         CREATE UNIQUE INDEX ix ON isolated.q(id);
         ALTER TABLE r ADD UNIQUE USING INDEX ix;",
    )
    .expect_err("index absent in table schema");
    assert!(matches!(error, Error::AdoptedIndexNotFound { index_name } if index_name == "ix"));
}

/// Sorting errors identify the offending key position.
#[test]
fn sorting_error_identifies_the_key_column() {
    let error = parse(
        "CREATE TABLE r(id INT, v INT);
         CREATE UNIQUE INDEX ix ON r(id, v DESC);
         ALTER TABLE r ADD PRIMARY KEY USING INDEX ix;",
    )
    .expect_err("second column sorting refused");
    assert!(matches!(error, Error::AdoptedIndexSorting { index_name, column_number: 2 }
        if index_name == "ix"));
}

/// The constraint keeps whether the adopted index treats nulls as distinct.
#[test]
fn unique_constraint_keeps_the_index_null_treatment() {
    for (clause, distinct) in [("NULLS NOT DISTINCT", false), ("NULLS DISTINCT", true), ("", true)]
    {
        let database = parse(&format!(
            "CREATE TABLE r (id INT);
             CREATE UNIQUE INDEX ix ON r (id) {clause};
             ALTER TABLE r ADD CONSTRAINT u UNIQUE USING INDEX ix;"
        ))
        .expect("index is adopted");
        let table = database
            .table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("lookup")
            .expect("table");
        let held: Vec<bool> = table
            .unique_indices(&database)
            .expect("table is in this database")
            .map(IndexLike::nulls_distinct)
            .collect();
        assert_eq!(held, [distinct], "{clause}");
    }
}
