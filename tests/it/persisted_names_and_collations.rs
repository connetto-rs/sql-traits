//! Keeping a resolved name or collation after the database is gone.
//!
//! A consumer that stores schema facts alongside the SQL it compiled needs the
//! name and the collation in a shape it can write down and read back. These
//! tests take each shape through JSON and require the value that comes back to
//! own its text, which is what `DeserializeOwned` demands of it.

#![cfg(feature = "serde")]
#![allow(clippy::expect_used, clippy::panic)]

use core::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};
use sql_traits::{
    errors::Error,
    prelude::*,
    traits::{ColumnCollation, MySqlCollationPadding, NamedColumnCollation},
};
use sqlparser::dialect::PostgreSqlDialect;

/// Writes `value` as JSON and reads it back, with the text dropped in between.
fn round_trip<T: Serialize + DeserializeOwned + Debug>(value: &T) -> T {
    let text = serde_json::to_string(value).expect("the value writes as JSON");
    serde_json::from_str(&text).expect("what was written reads back")
}

#[test]
fn a_target_name_round_trips_in_every_shape_it_takes() {
    let quoted_schema = TargetName::new("Docs", true).with_schema("App", true);
    for original in [
        TargetName::new("docs", false),
        TargetName::new("Docs", true),
        TargetName::new("docs", false).with_schema("app", false),
        quoted_schema,
        // A doubled quote is text no borrowed deserialisation could hand back.
        TargetName::new("we\"ird", true),
    ] {
        let restored: TargetName<'static> = round_trip(&original);
        assert_eq!(restored, original);
        assert_eq!(restored.to_string(), original.to_string());
        assert_eq!(restored.name_is_quoted(), original.name_is_quoted());
        assert_eq!(restored.schema_is_quoted(), original.schema_is_quoted());
    }
}

#[test]
fn a_collation_round_trips_with_and_without_catalog_facts() {
    let known =
        NamedColumnCollation::new(TargetName::new("und-x-icu", true).with_schema("app", false))
            .with_postgres_deterministic(Some(false))
            .with_mysql_padding(Some(MySqlCollationPadding::NoPad));
    let padded = NamedColumnCollation::new(TargetName::new("ci", false))
        .with_postgres_deterministic(Some(true))
        .with_mysql_padding(Some(MySqlCollationPadding::PadSpace));
    let unknown_facts = NamedColumnCollation::new(TargetName::new("ci", false));

    for original in [known, padded, unknown_facts] {
        let restored: NamedColumnCollation<'static> = round_trip(&original);
        assert_eq!(restored, original);
        assert_eq!(restored.name(), original.name());
        assert_eq!(restored.postgres_deterministic(), original.postgres_deterministic());
        assert_eq!(restored.mysql_padding(), original.mysql_padding());
    }

    for original in [
        ColumnCollation::DatabaseDefault,
        ColumnCollation::Unknown,
        ColumnCollation::Named(NamedColumnCollation::new(TargetName::new("C", true))),
    ] {
        let restored: ColumnCollation<'static> = round_trip(&original);
        assert_eq!(restored, original);
    }
}

#[test]
fn a_resolved_collation_survives_the_database_it_came_from() -> Result<(), Error> {
    let restored = {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (name TEXT COLLATE \"und-x-icu\");",
        )?;
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
            .expect("table t was created");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)?
            .expect("column name was declared");
        let collation: ColumnCollation<'static> = column.collation(&db)?.into_owned();
        round_trip(&collation)
    };

    let ColumnCollation::Named(named) = restored else {
        panic!("a declared COLLATE resolves to a named collation");
    };
    assert_eq!(named.name().name(), "und-x-icu");
    assert!(named.name().name_is_quoted());

    Ok(())
}
