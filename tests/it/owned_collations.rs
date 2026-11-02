//! Collation metadata that outlives the database it was resolved against.
//!
//! A consumer resolving a column's comparison rule once and answering rows
//! later cannot hold a borrowed value, so these tests read every fact a
//! collation carries after the catalog and the SQL text behind it are gone.

#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::Error,
    prelude::*,
    traits::{ColumnCollation, MySqlCollationPadding, NamedColumnCollation},
};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn a_collation_outlives_the_catalog_it_was_read_from() -> Result<(), Error> {
    let collation = {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        )?;
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
            .expect("table t was created");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)?
            .expect("column name was declared");
        let ColumnCollation::Named(named) = column.collation(&db)? else {
            panic!("a declared COLLATE resolves to a named collation");
        };
        named.into_owned()
    };

    assert_eq!(collation.name().name(), "ci");
    assert_eq!(collation.postgres_deterministic(), Some(false));
    assert_eq!(collation.mysql_padding(), None);

    Ok(())
}

#[test]
fn an_owned_collation_keeps_every_fact_the_borrowed_one_carried() {
    let padded = {
        let name = String::from("utf8mb4_general_ci");
        let borrowed = NamedColumnCollation::new(TargetName::new(&name, false))
            .with_postgres_deterministic(Some(true))
            .with_mysql_padding(Some(MySqlCollationPadding::PadSpace));
        ColumnCollation::Named(borrowed).into_owned()
    };

    let ColumnCollation::Named(named) = padded else {
        panic!("the named case stays the named case");
    };
    assert_eq!(named.name().name(), "utf8mb4_general_ci");
    assert_eq!(named.postgres_deterministic(), Some(true));
    assert_eq!(named.mysql_padding(), Some(MySqlCollationPadding::PadSpace));
}

#[test]
fn the_borrowless_collation_cases_survive_the_conversion() {
    assert_eq!(
        ColumnCollation::<'static>::DatabaseDefault,
        ColumnCollation::DatabaseDefault.into_owned()
    );
    assert_eq!(ColumnCollation::<'static>::Unknown, ColumnCollation::Unknown.into_owned());
}
