//! Tests that an index counts as simple only when every one of its entries is
//! a plain column, however many entries it has.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

/// Whether the one index `index` declares on `t (a INT, b INT)` is simple.
fn is_simple(index: &str) -> bool {
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("CREATE TABLE t (a INT, b INT);\n{index}"))
            .expect("schema builds");
    let mut indexes = database.indexes();
    let only = indexes.next().expect("one index is declared");
    assert!(indexes.next().is_none(), "{index} declares one index");
    only.is_simple(&database).expect("index belongs to the database")
}

/// PostgreSQL stores a parenthesised plain column as a plain key column, so a
/// column keeps the index simple at any depth of parentheses.
#[test]
fn test_plain_column_entries_are_simple() {
    for index in [
        "CREATE INDEX i ON t (a);",
        "CREATE INDEX i ON t (a, b);",
        "CREATE INDEX i ON t ((a));",
        "CREATE INDEX i ON t (((a)));",
        "CREATE INDEX i ON t ((a), b);",
        "CREATE INDEX i ON t (b, ((a)));",
        "CREATE UNIQUE INDEX i ON t (t.a, b);",
    ] {
        assert!(is_simple(index), "{index} is simple");
    }
}

/// One expression entry makes the index an expression index wherever it
/// stands, since a lookup uses it only when the query writes that expression.
#[test]
fn test_any_expression_entry_is_not_simple() {
    for index in [
        "CREATE INDEX i ON t (abs(a));",
        "CREATE INDEX i ON t ((a + b));",
        "CREATE INDEX i ON t ((abs(a)), b);",
        "CREATE INDEX i ON t (b, (a + b));",
        "CREATE UNIQUE INDEX i ON t (a, (abs(b)));",
        "CREATE INDEX i ON t ((a, b));",
    ] {
        assert!(!is_simple(index), "{index} is not simple");
    }
}
