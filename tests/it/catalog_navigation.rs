//! Moving between a column's name and its ordinal.
//!
//! Every consumer asking a table for a column's ordinal, or for the name at an
//! ordinal, or for its declared key as ordinals, writes the same composition
//! over `columns`. These tests pin the answers that composition owes: the
//! identifier rule, the round trip between the two directions, declaration
//! order for a key, and a key column that resolves to nothing being an error
//! rather than a shorter key.

#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

/// Writes a stored column name back as the text a lookup takes, quoting it
/// again when the declaration quoted it.
fn as_lookup_text(column: &impl ColumnLike) -> String {
    if column.column_name_is_quoted() {
        format!("\"{}\"", column.column_name().replace('"', "\"\""))
    } else {
        column.column_name().to_owned()
    }
}

#[test]
fn a_column_ordinal_follows_the_identifier_rule() -> Result<(), Error> {
    // "ID" and id are two columns in PostgreSQL: the quoted one keeps its
    // case, the bare one folds to lowercase.
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (\"ID\" INT, id INT, \"Name\" TEXT);",
    )?;
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table t was created");

    assert_eq!(table.column_id_by_name("\"ID\"", &db, IdentifierCase::AsWritten)?, Some(0));
    assert_eq!(table.column_id_by_name("id", &db, IdentifierCase::AsWritten)?, Some(1));
    // A bare lookup folds before comparing, so it reaches the bare column and
    // never the quoted one.
    assert_eq!(table.column_id_by_name("ID", &db, IdentifierCase::AsWritten)?, Some(1));
    // A quoted lookup compares exactly, and the bare column is stored folded,
    // so the folded spelling quoted still reaches it.
    assert_eq!(table.column_id_by_name("\"id\"", &db, IdentifierCase::AsWritten)?, Some(1));
    assert_eq!(table.column_id_by_name("\"Name\"", &db, IdentifierCase::AsWritten)?, Some(2));
    assert_eq!(table.column_id_by_name("name", &db, IdentifierCase::AsWritten)?, None);
    assert_eq!(table.column_id_by_name("absent", &db, IdentifierCase::AsWritten)?, None);

    Ok(())
}

#[test]
fn a_column_name_and_its_ordinal_round_trip() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (\"ID\" INT, id INT, \"Name\" TEXT, payload JSONB);",
    )?;
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table t was created");

    let number_of_columns = table.number_of_columns(&db)?;
    assert_eq!(number_of_columns, 4);

    for column_id in 0..number_of_columns {
        let column =
            table.column_by_id(column_id, &db)?.expect("the ordinal is inside the column list");
        assert_eq!(table.column_name_by_id(column_id, &db)?, Some(column.column_name()));
        assert_eq!(
            table.column_id_by_name(&as_lookup_text(column), &db, IdentifierCase::AsWritten)?,
            Some(column_id),
            "the name at ordinal {column_id} answers that same ordinal"
        );
    }

    // One past the last column names nothing, which is what an ordinal read
    // off a row of the wrong arity looks like.
    assert_eq!(table.column_name_by_id(number_of_columns, &db)?, None);

    Ok(())
}

#[test]
fn a_declared_key_answers_ordinals_in_declaration_order() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "
        CREATE TABLE out_of_order (a INT, b INT, c INT, d INT, PRIMARY KEY (d, b));
        CREATE TABLE single (id INT PRIMARY KEY, payload TEXT);
        CREATE TABLE keyless (a INT, b INT);
        ",
    )?;

    let out_of_order = db
        .table_by_target(TargetName::new("out_of_order", false), IdentifierCase::AsWritten)?
        .expect("table out_of_order was created");
    // The key is written (d, b), so the ordinals arrive as the key declares
    // them and not as the columns are declared.
    assert_eq!(out_of_order.primary_key_column_ids(&db)?, vec![3, 1]);

    let single = db
        .table_by_target(TargetName::new("single", false), IdentifierCase::AsWritten)?
        .expect("table single was created");
    assert_eq!(single.primary_key_column_ids(&db)?, vec![0]);

    let keyless = db
        .table_by_target(TargetName::new("keyless", false), IdentifierCase::AsWritten)?
        .expect("table keyless was created");
    assert_eq!(keyless.primary_key_column_ids(&db)?, Vec::<usize>::new());

    Ok(())
}

#[test]
fn a_quoted_key_column_keeps_its_own_ordinal() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (\"ID\" INT, id INT, PRIMARY KEY (id));",
    )?;
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table t was created");

    assert_eq!(table.primary_key_column_ids(&db)?, vec![1]);

    Ok(())
}

#[test]
fn a_column_ordinal_is_asked_of_the_table_that_declares_it() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "
        CREATE TABLE first (id INT, payload TEXT);
        CREATE TABLE second (payload TEXT, id INT);
        ",
    )?;
    let first = db
        .table_by_target(TargetName::new("first", false), IdentifierCase::AsWritten)?
        .expect("table first was created");
    let second = db
        .table_by_target(TargetName::new("second", false), IdentifierCase::AsWritten)?
        .expect("table second was created");

    assert_eq!(first.column_id_by_name("id", &db, IdentifierCase::AsWritten)?, Some(0));
    assert_eq!(second.column_id_by_name("id", &db, IdentifierCase::AsWritten)?, Some(1));

    // A table the database does not hold answers with the object error rather
    // than a missing column.
    let other = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE first (id INT);")?;
    assert!(matches!(
        second.column_id_by_name("id", &other, IdentifierCase::AsWritten),
        Err(LookupError::ObjectNotInDatabase { .. })
    ));
    assert!(matches!(
        second.column_name_by_id(0, &other),
        Err(LookupError::ObjectNotInDatabase { .. })
    ));
    assert!(matches!(
        second.primary_key_column_ids(&other),
        Err(LookupError::ObjectNotInDatabase { .. })
    ));

    Ok(())
}

#[test]
fn the_navigation_accessors_answer_the_same_through_a_reference() -> Result<(), Error> {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT, b INT, PRIMARY KEY (b, a));")?;
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table t was created");
    let by_reference = &table;

    assert_eq!(
        TableLike::column_id_by_name(by_reference, "b", &db, IdentifierCase::AsWritten)?,
        Some(1)
    );
    assert_eq!(TableLike::column_name_by_id(by_reference, 0, &db)?, Some("a"));
    assert_eq!(TableLike::primary_key_column_ids(by_reference, &db)?, vec![1, 0]);

    Ok(())
}
