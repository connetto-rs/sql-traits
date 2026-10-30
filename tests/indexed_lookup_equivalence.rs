//! Differential test: the indexed relation readers must answer exactly what a
//! linear scan answers, under every identifier comparison.
//!
//! The oracles here implement the documented rule directly using only the
//! public API, and are compared against `DatabaseLike::resolve_target_table`,
//! `DatabaseLike::table_by_target` and the `ParserDB` object-name resolvers
//! over a fixture of adversarial spellings (quote states, case, bare versus
//! explicit `public`, multiple schemas, multi-entry search paths).
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::LookupError, prelude::*, structs::TargetName, traits::TableLike,
    utils::identifier_resolution::identifiers_match,
};
use sqlparser::{
    ast::{Ident, ObjectName, ObjectNamePart},
    dialect::PostgreSqlDialect,
};

type Table = sqlparser::ast::CreateTable;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

const FIXTURE: &str = "
    CREATE SCHEMA app;
    CREATE SCHEMA app_2;
    CREATE TABLE bare_a (id INT);
    CREATE TABLE public.explicit_b (id INT);
    CREATE TABLE \"Mixed\" (id INT);
    CREATE TABLE lower_quoted_name (id INT);
    CREATE TABLE app.plain (id INT);
    CREATE TABLE app.\"Keyed\" (id INT);
    CREATE TABLE app_2.other (id INT);
    CREATE TABLE \"Odd Space\" (id INT);
    CREATE VIEW app.bare_a AS SELECT id FROM bare_a;
    CREATE MATERIALIZED VIEW app_2.plain AS SELECT id FROM app.plain;
    CREATE VIEW app_2.\"MIXED\" AS SELECT id FROM \"Mixed\";
    CREATE SEQUENCE app_2.\"Keyed\";
";

fn fixture_variants() -> Vec<(ParserDB, String)> {
    [
        "",
        "SET search_path TO app;",
        "SET search_path TO app, public;",
        "SET search_path TO \"app\", public;",
        "SET search_path TO public, app;",
        "SET search_path TO app_2, app;",
    ]
    .into_iter()
    .map(|path_sql| {
        let sql = format!("{FIXTURE}{path_sql}");
        (parse(&sql), path_sql.to_string())
    })
    .collect()
}

fn target_matrix() -> Vec<TargetName<'static>> {
    let mut matrix = Vec::new();
    for (name, quoted) in [
        ("bare_a", false),
        ("BARE_A", false),
        ("bare_a", true),
        ("explicit_b", false),
        ("Mixed", false),
        ("Mixed", true),
        ("mixed", false),
        ("plain", false),
        ("Keyed", true),
        ("Keyed", false),
        ("other", false),
        ("Odd Space", true),
        ("ghost", false),
    ] {
        matrix.push(TargetName::new(name, quoted));
        for (schema, schema_quoted) in [
            ("public", false),
            ("PUBLIC", false),
            ("public", true),
            ("app", false),
            ("APP", false),
            ("app", true),
            ("app_2", false),
        ] {
            matrix.push(TargetName::new(name, quoted).with_schema(schema, schema_quoted));
        }
    }
    matrix
}

fn table_name_matches(table: &Table, name: &str, name_quoted: bool) -> bool {
    identifiers_match(table.table_name(), table.table_name_is_quoted(), name, name_quoted)
}

fn schema_pair_matches(
    table: &Table,
    target_schema: Option<&str>,
    target_schema_quoted: bool,
) -> bool {
    match (target_schema, table.table_schema()) {
        (None, None) => true,
        (Some(target_schema), Some(table_schema)) => {
            identifiers_match(
                table_schema,
                table.table_schema_is_quoted(),
                target_schema,
                target_schema_quoted,
            )
        }
        (Some(target_schema), None) => {
            identifiers_match("public", false, target_schema, target_schema_quoted)
        }
        (None, Some(table_schema)) => {
            identifiers_match(table_schema, table.table_schema_is_quoted(), "public", false)
        }
    }
}

fn render(table: &Table) -> String {
    let name = TargetName::new(table.table_name(), table.table_name_is_quoted());
    match table.table_schema() {
        Some(schema) => name.with_schema(schema, table.table_schema_is_quoted()),
        None => name,
    }
    .to_string()
}

fn resolve_candidates<'a>(
    target: &TargetName<'_>,
    candidates: &[&'a Table],
) -> Result<Option<&'a Table>, LookupError> {
    match candidates {
        [] => Ok(None),
        [table] => Ok(Some(*table)),
        _ => {
            let mut rendered: Vec<String> = candidates.iter().copied().map(render).collect();
            rendered.sort();
            rendered.dedup();
            Err(LookupError::AmbiguousTableLookup {
                object_name: target.to_string(),
                candidates: rendered,
            })
        }
    }
}

fn outcome(result: &Result<Option<&Table>, LookupError>) -> String {
    match result {
        Ok(table) => table.map_or(String::new(), render),
        Err(error) => format!("ERR {error:?}"),
    }
}

/// Scan oracle for the search-path resolver under any comparison.
///
/// Tables, views, materialized views and sequences share one pool of names,
/// so a schema holding the name under any kind ends the walk whether or not it
/// holds a table.
fn oracle_resolve_with<'a>(
    db: &'a ParserDB,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> Result<Option<&'a Table>, LookupError> {
    if let Some(schema) = target.schema() {
        let schema_key = case.compared_form(schema, target.schema_is_quoted());
        return resolve_candidates(target, &matching_tables(db, target, case, &schema_key));
    }

    for (entry_schema, entry_quoted) in db.search_path().collect::<Vec<_>>() {
        let schema_key = case.compared_form(entry_schema, entry_quoted);
        let candidates = matching_tables(db, target, case, &schema_key);
        if !candidates.is_empty() || name_is_claimed_by_another_kind(db, target, case, &schema_key)
        {
            return resolve_candidates(target, &candidates);
        }
    }
    Ok(None)
}

/// Whether a view, a materialized view or a sequence holds the name in the
/// schema `schema_key` names, found by scanning.
fn name_is_claimed_by_another_kind(
    db: &ParserDB,
    target: &TargetName<'_>,
    case: IdentifierCase,
    schema_key: &str,
) -> bool {
    let name_key = case.compared_form(target.name(), target.name_is_quoted());
    let claims = |schema: Option<&str>, schema_quoted: bool, name: &str, quoted: bool| {
        let stored_schema = schema.map_or_else(
            || String::from("public"),
            |schema| case.compared_form(schema, schema_quoted).into_owned(),
        );
        stored_schema == schema_key && case.compared_form(name, quoted) == name_key
    };
    db.views().any(|view| {
        claims(
            view.view_schema(),
            view.view_schema_is_quoted(),
            view.view_name(),
            view.view_name_is_quoted(),
        )
    }) || db.materialized_views().any(|view| {
        claims(
            view.view_schema(),
            view.view_schema_is_quoted(),
            view.view_name(),
            view.view_name_is_quoted(),
        )
    }) || db.sequences().any(|sequence| {
        claims(
            sequence.sequence_schema(),
            sequence.sequence_schema_is_quoted(),
            sequence.sequence_name(),
            sequence.sequence_name_is_quoted(),
        )
    })
}

fn object_name(parts: &[(&str, bool)]) -> ObjectName {
    ObjectName(
        parts
            .iter()
            .map(|(value, quoted)| {
                ObjectNamePart::Identifier(if *quoted {
                    Ident::with_quote('"', *value)
                } else {
                    Ident::new(*value)
                })
            })
            .collect(),
    )
}

fn read_part(part: &ObjectNamePart) -> (&str, bool) {
    match part {
        ObjectNamePart::Identifier(ident) => (ident.value.as_str(), ident.quote_style.is_some()),
        ObjectNamePart::Function(function_part) => {
            (function_part.name.value.as_str(), function_part.name.quote_style.is_some())
        }
    }
}

fn target_of_object_name(object_name: &ObjectName) -> TargetName<'_> {
    let parts = &object_name.0;
    let (schema_part, name_part) = match parts.len() {
        1 => (None, &parts[0]),
        _ => (Some(&parts[parts.len() - 2]), &parts[parts.len() - 1]),
    };
    let (name, quoted) = read_part(name_part);
    let target = TargetName::new(name, quoted);
    match schema_part {
        Some(part) => {
            let (schema, schema_quoted) = read_part(part);
            target.with_schema(schema, schema_quoted)
        }
        None => target,
    }
}

/// Tables whose stored parts equal `schema_key` and the target's name under
/// `case`, found by scanning.
fn matching_tables<'a>(
    db: &'a ParserDB,
    target: &TargetName<'_>,
    case: IdentifierCase,
    schema_key: &str,
) -> Vec<&'a Table> {
    let name_key = case.compared_form(target.name(), target.name_is_quoted());
    db.tables()
        .filter(|table| {
            let stored_schema = table.table_schema().map_or_else(
                || String::from("public"),
                |schema| case.compared_form(schema, table.table_schema_is_quoted()).into_owned(),
            );
            case.compared_form(table.table_name(), table.table_name_is_quoted()) == name_key
                && stored_schema == schema_key
        })
        .collect()
}

/// Scan oracle for the parts lookup, which consults no search path.
fn oracle_by_target_with<'a>(
    db: &'a ParserDB,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> Result<Option<&'a Table>, LookupError> {
    let schema_key = target.schema().map_or_else(
        || String::from("public"),
        |schema| case.compared_form(schema, target.schema_is_quoted()).into_owned(),
    );
    resolve_candidates(target, &matching_tables(db, target, case, &schema_key))
}

#[test]
fn every_comparison_matches_scan() {
    for (db, path_sql) in fixture_variants() {
        let path: Vec<(&str, bool)> = db.search_path().collect();
        for target in target_matrix() {
            for case in [IdentifierCase::AsWritten, IdentifierCase::Folded, IdentifierCase::Exact] {
                let shown = target.to_string();
                assert_eq!(
                    outcome(&db.resolve_target_table(target.clone(), case)),
                    outcome(&oracle_resolve_with(&db, &target, case)),
                    "resolve_target_table({shown}, {case:?}) on {path_sql:?}"
                );
                assert_eq!(
                    outcome(&db.resolve_target_relation_on_path(target.clone(), &path, case).map(
                        |relation| {
                            relation.and_then(|relation| {
                                match relation {
                                    Relation::Table(table) => Some(table),
                                    Relation::View(_)
                                    | Relation::MaterializedView(_)
                                    | Relation::Sequence(_) => None,
                                }
                            })
                        }
                    )),
                    outcome(&oracle_resolve_with(&db, &target, case)),
                    "resolve_target_relation_on_path({shown}, {case:?}) on {path_sql:?}"
                );
                assert_eq!(
                    outcome(&db.table_by_target(target.clone(), case)),
                    outcome(&oracle_by_target_with(&db, &target, case)),
                    "table_by_target({shown}, {case:?}) on {path_sql:?}"
                );
            }
        }
    }
}

#[test]
fn object_name_resolvers_match_scan() {
    let names: Vec<ObjectName> = vec![
        object_name(&[("bare_a", false)]),
        object_name(&[("BARE_A", false)]),
        object_name(&[("bare_a", true)]),
        object_name(&[("public", false), ("explicit_b", false)]),
        object_name(&[("PUBLIC", false), ("explicit_b", false)]),
        object_name(&[("app", false), ("plain", false)]),
        object_name(&[("APP", false), ("plain", false)]),
        object_name(&[("app", false), ("Keyed", false)]),
        object_name(&[("app", false), ("Keyed", true)]),
        object_name(&[("Mixed", true)]),
        object_name(&[("mixed", false)]),
        object_name(&[("Odd Space", true)]),
        object_name(&[("app_2", false), ("other", false)]),
        object_name(&[("ghost", false)]),
    ];
    for (db, path_sql) in fixture_variants() {
        for name in &names {
            let target = target_of_object_name(name);
            assert_eq!(
                outcome(&db.resolve_table_object_name(name)),
                outcome(&{
                    let candidates: Vec<&Table> = db
                        .tables()
                        .filter(|table| {
                            table_name_matches(table, target.name(), target.name_is_quoted())
                                && schema_pair_matches(
                                    table,
                                    target.schema(),
                                    target.schema_is_quoted(),
                                )
                        })
                        .collect();
                    resolve_candidates(&target, &candidates)
                }),
                "resolve_table_object_name({name}) on {path_sql:?}"
            );
            assert_eq!(
                outcome(&db.resolve_table_object_name_on_search_path(name)),
                outcome(&oracle_resolve_with(&db, &target, IdentifierCase::AsWritten)),
                "resolve_table_object_name_on_search_path({name}) on {path_sql:?}"
            );
        }
    }
}
