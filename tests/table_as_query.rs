//! Integration tests for the columns `CREATE TABLE … AS` records.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{ColumnDef, DataType, Expr, Ident, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/query_parsing.rs"));

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
    let db = with_column_list(&["a"], "SELECT * FROM generate_series(1, 3) AS g")
        .expect("schema builds");
    assert_eq!(columns(&db, "cn"), pairs(&[("a", "")]));
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

fn expression(sql: &str) -> Expr {
    let query = query_with(&PostgreSqlDialect {}, &format!("SELECT {sql}"));
    let SetExpr::Select(select) = *query.body else { panic!("expected a plain SELECT body") };
    let Some(SelectItem::UnnamedExpr(expression)) = select.projection.into_iter().next() else {
        panic!("expected an expression projection")
    };
    expression
}

/// What a definition is, as `table.column TYPE` for a stored column read
/// through the database the definition names, and `= expression` for an
/// expression.
fn describe(definition: Option<ColumnDefinition<'_, '_, '_, ParserDB>>) -> String {
    match definition {
        Some(ColumnDefinition::Base { table, column, database }) => {
            column.collation(database).expect("the database holds the column");
            format!(
                "{}.{} {}",
                table.table_name(),
                column.column_name(),
                column.data_type(database)
            )
        }
        Some(ColumnDefinition::Expression { expression, .. }) => format!("= {expression}"),
        Some(other) => format!("{other:?}"),
        None => "unresolved".to_owned(),
    }
}

/// The definition `reference` resolves to in `SELECT * FROM table`.
fn definition_in(db: &ParserDB, table: &str, reference: &str) -> String {
    let query = query_with(&PostgreSqlDialect {}, &format!("SELECT * FROM {table}"));
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::AsWritten).expect("scope builds");
    describe(scope.resolve_column_definition(&expression(reference)).expect("definition resolves"))
}

/// The definition `inner` resolves to inside the expression defining
/// `reference` in `SELECT * FROM table`.
fn definition_inside(db: &ParserDB, table: &str, reference: &str, inner: &str) -> String {
    let query = query_with(&PostgreSqlDialect {}, &format!("SELECT * FROM {table}"));
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining, .. }) =
        scope.resolve_column_definition(&expression(reference)).expect("definition resolves")
    else {
        panic!("`{reference}` is defined by an expression")
    };
    describe(defining.resolve_column_definition(&expression(inner)).expect("definition resolves"))
}

const COMPUTED: &str = "CREATE TABLE ce AS SELECT id + 1 AS n, iv * 2, name || 'x' AS s FROM t2;";

fn computed(sql: &str) -> ParserDB {
    parse(&format!("{COMPUTED} {sql}"))
}

#[test]
fn computed_outputs_resolve_to_their_defining_expression() {
    let db = computed("");
    assert_eq!(definition_in(&db, "ce", "n"), "= id + 1");
    assert_eq!(definition_in(&db, "ce", "\"?column?\""), "= iv * 2");
    assert_eq!(definition_inside(&db, "ce", "n", "id"), "t2.id INT");
    assert_eq!(definition_inside(&db, "ce", "\"?column?\"", "iv"), "t2.iv INTERVAL");
}

#[test]
fn a_computed_column_names_the_output_it_was_created_from() {
    let db = computed("CREATE TABLE ct AS SELECT id FROM t2;");
    let ordinal = |table, name| {
        column(&db, table, name).created_from_query(&db).map(|created| created.ordinal())
    };
    assert_eq!(ordinal("ce", "n"), Some(0));
    assert_eq!(ordinal("ce", "s"), Some(2));
    assert_eq!(ordinal("ct", "id"), None);
}

#[test]
fn the_definition_reads_the_catalog_as_the_statement_saw_it() {
    let db = computed("ALTER TABLE t2 ALTER COLUMN iv TYPE text;");
    assert_eq!(definition_inside(&db, "ce", "\"?column?\"", "iv"), "t2.iv INTERVAL");

    let db = computed("DROP TABLE t2;");
    assert_eq!(definition_inside(&db, "ce", "\"?column?\"", "iv"), "t2.iv INTERVAL");
    assert_eq!(definition_inside(&db, "ce", "s", "name"), "t2.name TEXT");
}

#[test]
fn a_declared_type_replaces_the_defining_expression() {
    let db = computed("ALTER TABLE ce ALTER COLUMN n TYPE bigint;");
    assert_eq!(definition_in(&db, "ce", "n"), "ce.n BIGINT");
    assert!(column(&db, "ce", "n").created_from_query(&db).is_none());
}

#[test]
fn renaming_or_dropping_columns_keeps_the_definitions() {
    let db = computed(
        "ALTER TABLE ce RENAME COLUMN n TO m;
         ALTER TABLE ce DROP COLUMN \"?column?\";
         ALTER TABLE ce RENAME TO renamed;",
    );
    assert_eq!(definition_in(&db, "renamed", "m"), "= id + 1");
    assert_eq!(definition_in(&db, "renamed", "s"), "= name || 'x'");
}

#[test]
fn copied_columns_keep_the_defining_expression() {
    let db = computed(
        "CREATE TABLE child () INHERITS (ce);
         CREATE TABLE copy (LIKE ce);
         CREATE TABLE again AS SELECT n FROM ce;
         CREATE VIEW v AS SELECT n FROM ce;",
    );
    for relation in ["child", "copy", "again", "v"] {
        assert_eq!(definition_in(&db, relation, "n"), "= id + 1", "{relation}");
    }
}

#[test]
fn a_table_scope_resolves_computed_columns() {
    let db = computed("");
    let table = db
        .table_by_target(TargetName::new("ce", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("table exists");
    let scope = ColumnScope::for_table(table, &db, IdentifierCase::AsWritten);
    assert_eq!(
        describe(scope.resolve_column_definition(&expression("n")).expect("definition resolves")),
        "= id + 1"
    );
}
