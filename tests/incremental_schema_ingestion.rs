//! Integration tests for incremental schema ingestion.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sql_traits::{
    structs::{
        ColumnDefinition, ColumnScope, IdentifierCase, ParserDB, ParserDBIngestor, TargetName,
    },
    traits::{ColumnLike, DatabaseLike, TableLike},
};
use sqlparser::{
    ast::{SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn statements(sql: &str) -> Vec<Statement> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("SQL parses")
}

fn inspect_definition<R>(
    database: &ParserDB,
    inspect: impl FnOnce(ColumnDefinition<'_, '_, '_, ParserDB>) -> R,
) -> R {
    let Statement::Query(query) = statements("SELECT v.id FROM v").pop().expect("one statement")
    else {
        panic!("expected a query")
    };
    let scope = ColumnScope::from_query(&query, database, IdentifierCase::AsWritten)
        .expect("query scope builds");
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select") };
    let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
        panic!("expected an expression")
    };
    let definition = scope
        .resolve_column_definition(reference)
        .expect("definition resolves")
        .expect("column exists");
    inspect(definition)
}

#[test]
fn snapshots_preserve_each_incremental_schema_state() {
    let mut input = ParserDBIngestor::new::<PostgreSqlDialect>("test".to_owned());

    for statement in statements("CREATE TABLE a (id INT PRIMARY KEY);") {
        input = input.apply_statement(statement).expect("table applies");
    }
    for statement in statements("CREATE VIEW v AS SELECT id FROM a;") {
        input = input.apply_statement(statement).expect("first view applies");
    }
    let first = input.snapshot();
    assert!(inspect_definition(&first, |definition| {
        matches!(
            definition,
            ColumnDefinition::Base { table, column }
                if table.table_name() == "a" && column.column_name() == "id"
        )
    }));
    for statement in statements("DROP VIEW v;") {
        input = input.apply_statement(statement).expect("drop applies");
    }
    let dropped = input.snapshot();
    assert!(
        dropped
            .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );

    for statement in statements("CREATE VIEW v AS SELECT id + 1 AS id FROM a;") {
        input = input.apply_statement(statement).expect("second view applies");
    }
    let second = input.finish();
    let definition = inspect_definition(&second, |definition| {
        match definition {
            ColumnDefinition::Expression { expression, .. } => expression.to_string(),
            ColumnDefinition::Base { .. } => "base".to_owned(),
            ColumnDefinition::SetOperation { .. } => "set operation".to_owned(),
            ColumnDefinition::RecursiveUnion { .. } => "recursive union".to_owned(),
            ColumnDefinition::Opaque => "opaque".to_owned(),
        }
    });
    assert_eq!(definition, "id + 1");

    assert!(inspect_definition(&first, |definition| {
        matches!(
            definition,
            ColumnDefinition::Base { table, column }
                if table.table_name() == "a" && column.column_name() == "id"
        )
    }));
}

#[test]
fn parse_options_apply_to_incremental_statements() {
    use sql_traits::structs::{AccessResolution, ParseOptions};

    let input = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .ingestor::<PostgreSqlDialect>("test".to_owned());
    let input = statements("CREATE TABLE t (id INT);")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("table applies");
    let input = statements("GRANT SELECT ON t TO missing_role;")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("open world grant applies");

    assert_eq!(input.finish().table_grants().count(), 1);
}

#[test]
fn postgres_catalog_and_created_collations_survive_incremental_statements() {
    use sql_traits::{
        structs::{ParseOptions, PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType},
        traits::ColumnCollation,
    };

    let catalog = PostgresCatalog::empty()
        .with_collation(PostgresCatalogCollation::new("base", false).with_deterministic(false))
        .with_collatable_type(PostgresCatalogType::new("text", false));
    let options = ParseOptions::default().with_postgres_catalog(catalog);
    assert_eq!(options.postgres_catalog().collations().count(), 1);
    let input = options.ingestor::<PostgreSqlDialect>("test".to_owned());
    let input = statements("CREATE COLLATION ci FROM base;")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("collation applies");
    let input = statements("CREATE TABLE t (name TEXT COLLATE ci);")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("table applies");

    let database = input.finish();
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let column = table
        .column("name", &database, IdentifierCase::AsWritten)
        .expect("column lookup runs")
        .expect("column exists");
    let ColumnCollation::Named(collation) =
        column.collation(&database).expect("collation metadata resolves")
    else {
        panic!("expected a named collation")
    };
    assert_eq!(collation.name().name(), "ci");
    assert_eq!(collation.postgres_deterministic(), Some(false));
}

#[test]
fn finished_schema_resumes_ingestion() {
    let input = ParserDBIngestor::new::<PostgreSqlDialect>("test".to_owned());
    let input = statements("CREATE TABLE t (id INT PRIMARY KEY);")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("table applies");

    let input = input.finish().into_ingestor();
    let input = statements("ALTER TABLE t ADD COLUMN label TEXT;")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("alter applies after resumption");

    let database = input.finish();
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(
        table
            .column("id", &database, IdentifierCase::AsWritten)
            .expect("column lookup runs")
            .is_some()
    );
    assert!(
        table
            .column("label", &database, IdentifierCase::AsWritten)
            .expect("column lookup runs")
            .is_some()
    );
}

#[test]
fn resumed_ingestion_preserves_options_and_created_collations() {
    use sql_traits::{
        structs::{
            AccessResolution, ParseOptions, PostgresCatalog, PostgresCatalogCollation,
            PostgresCatalogType,
        },
        traits::ColumnCollation,
    };

    let catalog = PostgresCatalog::empty()
        .with_collation(PostgresCatalogCollation::new("base", false).with_deterministic(false))
        .with_collatable_type(PostgresCatalogType::new("text", false));
    let input = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .with_postgres_catalog(catalog)
        .ingestor::<PostgreSqlDialect>("test".to_owned());
    let input = statements("CREATE COLLATION ci FROM base;")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("collation applies");

    let input = input.finish().into_ingestor();
    let input = statements("GRANT SELECT ON missing_table TO missing_role;")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("open world grant applies after resumption");
    let input = statements("CREATE TABLE u (name TEXT COLLATE ci);")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("table applies after resumption");

    let database = input.finish();
    assert_eq!(database.table_grants().count(), 1);
    let table = database
        .table_by_target(TargetName::new("u", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let column = table
        .column("name", &database, IdentifierCase::AsWritten)
        .expect("column lookup runs")
        .expect("column exists");
    let ColumnCollation::Named(collation) =
        column.collation(&database).expect("collation metadata resolves")
    else {
        panic!("expected a named collation")
    };
    assert_eq!(collation.name().name(), "ci");
    assert_eq!(collation.postgres_deterministic(), Some(false));
}

#[test]
fn batch_parsed_database_resumes_ingestion() {
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT PRIMARY KEY);")
        .expect("schema builds");

    let input = statements("ALTER TABLE t ADD COLUMN label TEXT;")
        .into_iter()
        .try_fold(database.into_ingestor(), ParserDBIngestor::apply_statement)
        .expect("alter applies after resumption");

    let database = input.finish();
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(
        table
            .column("label", &database, IdentifierCase::AsWritten)
            .expect("column lookup runs")
            .is_some()
    );
}

#[test]
fn resumed_ingestion_preserves_closed_world_resolution() {
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT PRIMARY KEY);")
        .expect("schema builds");

    let result = statements("GRANT SELECT ON t TO missing_role;")
        .into_iter()
        .try_fold(database.into_ingestor(), ParserDBIngestor::apply_statement);

    assert!(result.is_err(), "closed world must still refuse a grant to an unknown role");
}

#[test]
fn one_shot_parsed_database_resumes_with_options_and_collations() {
    use sql_traits::{
        structs::{
            AccessResolution, ParseOptions, PostgresCatalog, PostgresCatalogCollation,
            PostgresCatalogType,
        },
        traits::ColumnCollation,
    };

    let catalog = PostgresCatalog::empty()
        .with_collation(PostgresCatalogCollation::new("base", false).with_deterministic(false))
        .with_collatable_type(PostgresCatalogType::new("text", false));
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("CREATE COLLATION ci FROM base;")
        .expect("schema builds");

    let input = statements("GRANT SELECT ON missing_table TO missing_role;")
        .into_iter()
        .try_fold(database.into_ingestor(), ParserDBIngestor::apply_statement)
        .expect("open world grant applies after resumption");
    let input = statements("CREATE TABLE u (name TEXT COLLATE ci);")
        .into_iter()
        .try_fold(input, ParserDBIngestor::apply_statement)
        .expect("table applies after resumption");

    let database = input.finish();
    assert_eq!(database.table_grants().count(), 1);
    let table = database
        .table_by_target(TargetName::new("u", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let column = table
        .column("name", &database, IdentifierCase::AsWritten)
        .expect("column lookup runs")
        .expect("column exists");
    let ColumnCollation::Named(collation) =
        column.collation(&database).expect("collation metadata resolves")
    else {
        panic!("expected a named collation")
    };
    assert_eq!(collation.name().name(), "ci");
    assert_eq!(collation.postgres_deterministic(), Some(false));
}

#[test]
fn snapshot_resumes_ingestion_independently() {
    let mut input = ParserDBIngestor::new::<PostgreSqlDialect>("test".to_owned());
    for statement in statements("CREATE TABLE t (id INT PRIMARY KEY);") {
        input = input.apply_statement(statement).expect("table applies");
    }

    let resumed = statements("ALTER TABLE t ADD COLUMN label TEXT;")
        .into_iter()
        .try_fold(input.snapshot().into_ingestor(), ParserDBIngestor::apply_statement)
        .expect("alter applies on the snapshot");
    let database = resumed.finish();
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(
        table
            .column("label", &database, IdentifierCase::AsWritten)
            .expect("column lookup runs")
            .is_some()
    );

    let original = input.finish();
    let table = original
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(
        table
            .column("label", &original, IdentifierCase::AsWritten)
            .expect("column lookup runs")
            .is_none()
    );
}
