//! A PostgreSQL `DO` block runs code whose effects on the schema depend on
//! the database it runs against, so a document holding one is refused rather
//! than modelled as though the block had not run.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn a_do_block_creating_a_role_refuses_the_document() {
    let sql = "CREATE TABLE t (id INT);
DO $$ BEGIN CREATE ROLE app_reader; EXCEPTION WHEN duplicate_object THEN NULL; END $$;";
    let result = ParserDB::parse::<PostgreSqlDialect>(sql);
    assert!(
        matches!(
            &result,
            Err(Error::UnsupportedDoBlock { statement })
                if statement.contains("CREATE ROLE app_reader")
        ),
        "got {result:?}"
    );
}

#[test]
fn a_do_block_leaving_the_schema_alone_is_refused_too() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "DO LANGUAGE plpgsql $$ BEGIN RAISE NOTICE 'hi'; END $$;",
    );
    assert!(matches!(result, Err(Error::UnsupportedDoBlock { .. })), "got {result:?}");
}
