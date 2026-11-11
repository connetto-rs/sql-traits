//! Builder-assembled tables preserve their identities and references through
//! schema moves.
#![expect(clippy::expect_used, clippy::panic, reason = "Tests require concrete parsed identities.")]

extern crate alloc;

use alloc::sync::Arc;

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};

/// Applies every statement `sql` parses, in order.
fn apply(mut input: ParserDBIngestor, sql: &str) -> ParserDBIngestor {
    for statement in Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("SQL parses") {
        input = input.apply_statement(statement).expect("statement applies");
    }
    input
}

/// The table `name` holds, qualified when the name writes a schema.
fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(
        TargetName::parse(name).expect("one or two identifiers"),
        IdentifierCase::AsWritten,
    )
    .expect("lookup runs")
    .expect("table exists")
}

/// The column `column` of the table `table` holds.
fn column<'db>(
    db: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    table(db, table_name)
        .column(column_name, db, IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("column exists")
}

/// The names of every column of the table `name` holds.
fn column_names(db: &ParserDB, name: &str) -> Vec<String> {
    table(db, name)
        .columns(db)
        .expect("columns resolve")
        .map(|column| column.column_name().to_owned())
        .collect()
}

/// The definition of the view `name` holds, as the parser renders it.
fn view_definition(db: &ParserDB, name: &str) -> String {
    db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("the view exists")
        .definition()
        .to_string()
}

/// Renders a query independently of whitespace and capitalization.
fn rendered(query: &str) -> String {
    let Some(Statement::Query(query)) =
        Parser::parse_sql(&PostgreSqlDialect {}, query).expect("the query parses").pop()
    else {
        panic!("expected a query")
    };
    query.to_string()
}

/// The statement `DROP TYPE name;` parses to.
fn drop_type(name: &str) -> Statement {
    Parser::parse_sql(&PostgreSqlDialect {}, &format!("DROP TYPE {name};"))
        .expect("the statement parses")
        .remove(0)
}

#[test]
fn a_builder_assembled_table_moves_schemas_with_no_recorded_array_ownership() {
    let Some(Statement::CreateTable(node)) =
        Parser::parse_sql(&PostgreSqlDialect {}, "CREATE TABLE t (a INT, b INT);")
            .expect("SQL parses")
            .pop()
    else {
        panic!("expected a table")
    };
    let node = Arc::new(node);

    // Build without ingesting the table declaration.
    let empty = ParserDB::parse::<PostgreSqlDialect>("").expect("empty schema");
    let mut builder = ParserDBBuilder::new("built".to_owned(), *empty.dialect());
    let mut metadata = TableMetadata::default();
    for definition in &node.columns {
        let column = Arc::new(TableAttribute::new(Arc::clone(&node), definition.clone()));
        metadata.add_column(Arc::clone(&column));
        builder = builder.add_column(column, ColumnMetadata::default());
    }
    let db: ParserDB = builder.add_table(node, metadata).expect("the table is added").into();

    let table_id = db.table_identity(table(&db, "t")).expect("table has an identity");
    let column_id = db.column_identity(column(&db, "t", "a")).expect("column has an identity");

    let moved = apply(
        db.into_ingestor(),
        "CREATE SCHEMA app;
         CREATE VIEW v AS SELECT a FROM t;
         ALTER TABLE t RENAME TO app.t;",
    )
    .finish();

    let destination = table(&moved, "app.t");
    assert_eq!(moved.table_identity(destination), Some(table_id));
    assert_eq!(column_names(&moved, "app.t"), ["a", "b"]);
    assert_eq!(moved.column_identity(column(&moved, "app.t", "a")), Some(column_id));
    assert_eq!(view_definition(&moved, "v"), rendered("SELECT a FROM app.t"));
    assert!(
        moved
            .table_by_target(
                TargetName::new("t", false).with_schema("public", false),
                IdentifierCase::AsWritten,
            )
            .expect("lookup runs")
            .is_none(),
        "the source schema no longer holds the table"
    );

    let refused = moved.into_ingestor().apply_statement(drop_type("app._t"));
    assert!(matches!(refused, Err(Error::TypeNotFound { .. })));
}
