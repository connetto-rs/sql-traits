//! Integration tests for the columns `CREATE TABLE … AS` records.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{ColumnDef, DataType, Ident, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

const SOURCE: &str =
    "CREATE TABLE t2 (id int NOT NULL DEFAULT 1, iv interval, name text COLLATE \"C\", n serial);";

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SOURCE} {sql}")).expect("schema parses")
}

fn columns(db: &ParserDB, table: &str) -> Vec<(String, String)> {
    db.table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table exists")
        .columns(db)
        .expect("columns")
        .map(|column| (column.column_name().to_owned(), column.data_type(db).to_string()))
        .collect()
}

fn column<'db>(
    db: &'db ParserDB,
    table: &str,
    name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    db.table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table exists")
        .column(name, db, IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("column exists")
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected.iter().map(|(name, ty)| ((*name).to_owned(), (*ty).to_owned())).collect()
}

#[test]
fn column_reference_outputs_take_the_source_type() {
    let db = parse("CREATE TABLE ct AS SELECT id, iv FROM t2;");
    assert_eq!(columns(&db, "ct"), pairs(&[("id", "INT"), ("iv", "INTERVAL")]));
}

#[test]
fn with_no_data_records_the_same_columns() {
    let db = parse("CREATE TABLE ct AS SELECT id, iv FROM t2 WITH NO DATA;");
    assert_eq!(columns(&db, "ct"), pairs(&[("id", "INT"), ("iv", "INTERVAL")]));
}

#[test]
fn a_wildcard_records_every_source_column() {
    let db = parse("CREATE TABLE ct AS SELECT * FROM t2;");
    assert_eq!(
        columns(&db, "ct"),
        pairs(&[("id", "INT"), ("iv", "INTERVAL"), ("name", "TEXT"), ("n", "INTEGER")])
    );
}

#[test]
fn copied_columns_carry_no_constraint_or_default() {
    let db = parse("CREATE TABLE ct AS SELECT id, n FROM t2;");
    for name in ["id", "n"] {
        let column = column(&db, "ct", name);
        assert!(column.is_nullable(&db).expect("nullability"), "{name} is nullable");
        assert!(!column.has_default(), "{name} has no default");
        assert!(!column.is_serial(), "{name} is not serial");
    }
}

#[test]
fn copied_columns_keep_the_source_collation() {
    let db = parse("CREATE TABLE ct AS SELECT name FROM t2;");
    let ColumnCollation::Named(collation) =
        column(&db, "ct", "name").collation(&db).expect("collation")
    else {
        panic!("expected a named collation");
    };
    assert_eq!(collation.name().name(), "C");
}

#[test]
fn outputs_read_through_a_view_take_the_base_type() {
    let db =
        parse("CREATE VIEW v AS SELECT iv AS span FROM t2; CREATE TABLE ct AS SELECT span FROM v;");
    assert_eq!(columns(&db, "ct"), pairs(&[("span", "INTERVAL")]));
}

#[test]
fn computed_outputs_are_named_as_postgres_names_them() {
    let db = parse("CREATE TABLE ce AS SELECT id + 1 AS n, iv * 2, 'x' AS s FROM t2;");
    assert_eq!(
        columns(&db, "ce").into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
        ["n", "?column?", "s"]
    );
    assert_eq!(*column(&db, "ce", "n").data_type(&db), DataType::Unspecified);
}

#[test]
fn repeated_output_names_are_refused() {
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SOURCE} CREATE TABLE ct AS SELECT id, id FROM t2;"
    ));
    assert!(
        matches!(result, Err(Error::ColumnAlreadyExists { column_name, .. }) if column_name == "id"),
        "PostgreSQL refuses a column specified more than once"
    );
}

/// A `CREATE TABLE … AS query` statement with `names` written as its column
/// list, built from the AST since the parser does not yet read the list under
/// PostgreSQL.
fn with_column_list(names: &[&str], query: &str) -> Result<ParserDB, Error> {
    let mut statements =
        Parser::parse_sql(&PostgreSqlDialect {}, &format!("{SOURCE} CREATE TABLE cn AS {query};"))
            .expect("statements parse");
    let Some(Statement::CreateTable(create_table)) = statements.last_mut() else {
        panic!("expected CREATE TABLE")
    };
    create_table.columns = names
        .iter()
        .map(|name| {
            ColumnDef {
                name: Ident::new(*name),
                data_type: DataType::Unspecified,
                options: Vec::new(),
            }
        })
        .collect();
    ParserDB::from_statements(statements, "test".to_owned())
}

#[test]
fn a_column_list_renames_outputs_in_order() {
    let db = with_column_list(&["a", "b"], "SELECT id, iv FROM t2").expect("schema builds");
    assert_eq!(columns(&db, "cn"), pairs(&[("a", "INT"), ("b", "INTERVAL")]));
}

#[test]
fn a_short_column_list_leaves_the_rest_named_by_the_query() {
    let db = with_column_list(&["a"], "SELECT id, iv FROM t2").expect("schema builds");
    assert_eq!(columns(&db, "cn"), pairs(&[("a", "INT"), ("iv", "INTERVAL")]));
}

#[test]
fn outputs_that_cannot_be_counted_leave_the_declared_names() {
    for query in ["SELECT * FROM generate_series(1, 3) AS g", "TABLE t2"] {
        let db = with_column_list(&["a"], query).expect("schema builds");
        assert_eq!(columns(&db, "cn"), pairs(&[("a", "")]), "{query}");
    }
}

#[test]
fn typed_columns_beside_the_query_are_kept_as_declared() {
    let db = ParserDB::parse::<sqlparser::dialect::MySqlDialect>(
        "CREATE TABLE t2 (id INT); CREATE TABLE m (extra TEXT) AS SELECT id FROM t2;",
    )
    .expect("schema parses");
    assert_eq!(columns(&db, "m"), pairs(&[("extra", "TEXT")]));
}

#[test]
fn a_column_list_longer_than_the_outputs_is_refused() {
    assert!(matches!(
        with_column_list(&["a", "b", "c"], "SELECT id, iv FROM t2"),
        Err(Error::TableColumnListTooLong { table_name }) if table_name == "cn"
    ));
}
