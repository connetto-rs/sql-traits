//! Tests that an index on a materialized view is recorded, checked and kept
//! on its view as PostgreSQL 18 does, and answers the index readers as an
//! index on a table does.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::{Expr, OrderBySort, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const VIEW: &str = "CREATE TABLE documents (identifier integer PRIMARY KEY, title text, body text);
     CREATE MATERIALIZED VIEW titles AS SELECT identifier, title, body FROM documents;";

/// The names of the indexes on materialized views, in the order created.
fn view_index_names(db: &ParserDB) -> Vec<Option<&str>> {
    db.materialized_view_indexes().map(IndexLike::name).collect()
}

/// The materialized view `name` names.
fn view<'db>(db: &'db ParserDB, name: &str) -> &'db MaterializedView {
    db.materialized_view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("the view exists")
}

/// The keys of `index` as written, a descending one marked.
fn keys<I: IndexLike<DB = ParserDB>>(index: &I, db: &ParserDB) -> Vec<String> {
    index
        .keys(db)
        .expect("held")
        .map(|key| {
            match key.order() {
                Some(OrderBySort::Desc) => format!("{} DESC", key.expression()),
                _ => key.expression().to_string(),
            }
        })
        .collect()
}

/// The name of the materialized view `index` is on.
fn view_indexed_by<'db>(index: &'db MaterializedViewIndex, db: &'db ParserDB) -> &'db str {
    match index.relation(db).expect("held") {
        Relation::MaterializedView(on) => Some(on.view_name()),
        Relation::Table(_)
        | Relation::ForeignTable(_)
        | Relation::View(_)
        | Relation::Sequence(_) => None,
    }
    .expect("the index is on a materialized view")
}

#[test]
fn two_indexes_with_one_key_on_a_materialized_view_are_both_kept() {
    let db = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_first ON titles ((lower(title)));
         CREATE INDEX titles_second ON titles ((lower(title)));"
    ))
    .expect("PostgreSQL indexes a materialized view");
    let written: Vec<Vec<String>> =
        db.materialized_view_indexes().map(|index| keys(index, &db)).collect();
    assert_eq!(written[0], written[1]);
}

#[test]
fn an_index_on_a_materialized_view_answers_every_reader() {
    let db = parse(&format!(
        "{VIEW}
         CREATE UNIQUE INDEX titles_key ON titles USING btree (identifier DESC, title)
             INCLUDE (body) NULLS NOT DISTINCT WHERE identifier > 0;"
    ))
    .expect("schema builds");
    let index = db.materialized_view_indexes().next().expect("one index");
    assert_eq!(index.name(), Some("titles_key"));
    assert_eq!(index.schema(), None, "the view was created without a qualifier");
    assert!(index.is_unique());
    assert!(!index.nulls_distinct());
    assert_eq!(index.access_method().map(ToString::to_string).as_deref(), Some("BTREE"));
    assert_eq!(index.predicate().map(ToString::to_string).as_deref(), Some("identifier > 0"));
    assert_eq!(index.include_column_names().collect::<Vec<_>>(), [("body", false)]);
    assert_eq!(keys(index, &db), ["identifier DESC", "title"]);
    assert_eq!(index.constraint_name(&db).expect("held"), None);
    assert_eq!(view_indexed_by(index, &db), "titles");
}

#[test]
fn an_index_on_a_materialized_view_reports_its_quoting() {
    let db = parse(
        "CREATE SCHEMA \"Reports\";
         CREATE TABLE documents (identifier integer PRIMARY KEY, title text);
         CREATE MATERIALIZED VIEW \"Reports\".titles AS SELECT identifier, title FROM documents;
         CREATE INDEX \"Titles_Key\" ON \"Reports\".titles (identifier, title);
         CREATE MATERIALIZED VIEW plain AS SELECT identifier FROM documents;
         CREATE INDEX plain_key ON plain (identifier);",
    )
    .expect("schema builds");
    let quoted = db.materialized_view_indexes().find(|index| index.name() == Some("Titles_Key"));
    let quoted = quoted.expect("the quoted index exists");
    assert!(quoted.name_is_quoted());
    assert_eq!(quoted.schema(), Some("Reports"));
    assert!(quoted.schema_is_quoted());
    assert_eq!(quoted.expression(&db).expect("held").to_string(), "(identifier, title)");
    let plain = db.materialized_view_indexes().find(|index| index.name() == Some("plain_key"));
    let plain = plain.expect("the plain index exists");
    assert!(!plain.name_is_quoted());
    assert!(!plain.schema_is_quoted());
}

#[test]
fn an_index_another_database_holds_is_reported_absent() {
    let db = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title);
         CREATE INDEX ON titles (((title || 'x')::nvarchar));"
    ))
    .expect("schema builds");
    let other =
        parse("CREATE TABLE documents (identifier integer PRIMARY KEY);").expect("schema builds");
    let named = db.materialized_view_indexes().find(|index| index.name().is_some());
    assert_eq!(
        named.expect("the named index exists").relation(&other).err(),
        Some(ObjectKind::Index.not_in_database("titles_title"))
    );
    for index in db.materialized_view_indexes() {
        for absent in
            [index.relation(&other).err(), index.expression(&other).err(), index.keys(&other).err()]
        {
            assert!(
                matches!(
                    absent,
                    Some(LookupError::ObjectNotInDatabase { object_kind: ObjectKind::Index, .. })
                ),
                "got {absent:?}"
            );
        }
    }
    assert_eq!(
        other.indexes_on_materialized_view(view(&db, "titles")).err(),
        Some(ObjectKind::MaterializedView.not_in_database("titles"))
    );
}

#[test]
fn table_indexes_and_materialized_view_indexes_are_listed_apart() {
    let db = parse(&format!(
        "{VIEW}
         CREATE MATERIALIZED VIEW bodies AS SELECT identifier, body FROM documents;
         CREATE INDEX documents_title ON documents (title);
         CREATE INDEX titles_title ON titles (title);
         CREATE INDEX bodies_body ON bodies (body);"
    ))
    .expect("schema builds");
    let tables: Vec<_> = db.indexes().map(IndexLike::name).collect();
    assert_eq!(tables, [Some("documents_title")]);
    let on_titles: Vec<_> = db
        .indexes_on_materialized_view(view(&db, "titles"))
        .expect("held")
        .map(IndexLike::name)
        .collect();
    assert_eq!(on_titles, [Some("titles_title")]);
}

#[test]
fn an_unnamed_index_on_a_materialized_view_takes_the_generated_name() {
    let db = parse(&format!(
        "{VIEW}
         CREATE INDEX ON titles (title);
         CREATE INDEX ON titles (title);"
    ))
    .expect("schema builds");
    assert_eq!(view_index_names(&db), [Some("titles_title_idx"), Some("titles_title_idx1")]);
}

#[test]
fn create_index_name_answers_the_name_an_index_on_a_materialized_view_takes() {
    let mut statements = Parser::parse_sql(
        &PostgreSqlDialect {},
        &format!("{VIEW} CREATE INDEX ON titles (title);"),
    )
    .expect("SQL parses");
    let create_index = match statements.pop() {
        Some(Statement::CreateIndex(create_index)) => Some(create_index),
        _ => None,
    }
    .expect("the last statement is the index");
    let mut ingestor = ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned());
    for statement in statements {
        ingestor = ingestor.apply_statement(statement).expect("statement applies");
    }
    let name = ingestor.create_index_name(&create_index).expect("the view exists");
    assert_eq!(name.map(|name| name.value).as_deref(), Some("titles_title_idx"));
}

#[test]
fn an_index_on_a_materialized_view_without_a_key_is_refused() {
    let mut statements = Parser::parse_sql(
        &PostgreSqlDialect {},
        &format!("{VIEW} CREATE INDEX titles_title ON titles (title);"),
    )
    .expect("SQL parses");
    let mut create_index = match statements.pop() {
        Some(Statement::CreateIndex(create_index)) => Some(create_index),
        _ => None,
    }
    .expect("the last statement is the index");
    create_index.columns.clear();
    let mut ingestor = ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned());
    for statement in statements {
        ingestor = ingestor.apply_statement(statement).expect("statement applies");
    }
    let refused = ingestor.apply_statement(Statement::CreateIndex(create_index)).err();
    assert!(
        matches!(
            &refused,
            Some(Error::InvalidIndex { index_name, .. }) if index_name == "titles_title"
        ),
        "got {refused:?}"
    );
}

#[test]
fn an_index_naming_a_column_the_view_does_not_expose_is_refused() {
    for statement in [
        "CREATE INDEX titles_missing ON titles (missing);",
        "CREATE INDEX titles_missing ON titles (title) INCLUDE (missing);",
    ] {
        let result = parse(&format!("{VIEW} {statement}"));
        assert!(
            matches!(
                &result,
                Err(Error::IdentifierLookupError(LookupError::ColumnNotFound { column_name, .. }))
                    if column_name == "missing"
            ),
            "{statement} got {result:?}"
        );
    }
}

#[test]
fn an_index_on_a_materialized_view_shares_the_relation_pool() {
    let taken = parse(&format!("{VIEW} CREATE INDEX documents ON titles (title);"));
    assert!(
        matches!(
            &taken,
            Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Index,
                conflicting_kind: ObjectKind::Table,
                ..
            })
        ),
        "got {taken:?}"
    );
    let holding = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title);
         CREATE TABLE titles_title (a int);"
    ));
    assert!(
        matches!(
            &holding,
            Err(Error::RelationNameAlreadyTaken { conflicting_kind: ObjectKind::Index, .. })
        ),
        "got {holding:?}"
    );
    let skipped = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title);
         CREATE INDEX IF NOT EXISTS titles_title ON titles (body);"
    ))
    .expect("IF NOT EXISTS skips a taken name");
    let index = skipped.materialized_view_indexes().next().expect("one index");
    assert_eq!(keys(index, &skipped), ["title"]);
    assert_eq!(skipped.materialized_view_indexes().count(), 1);
}

#[test]
fn an_index_qualified_by_another_schema_is_refused() {
    let result = parse(&format!(
        "CREATE SCHEMA other; {VIEW} CREATE INDEX other.titles_title ON titles (title);"
    ));
    assert!(matches!(result, Err(Error::IndexInAnotherSchema { .. })), "got {result:?}");
}

#[test]
fn only_tables_and_materialized_views_are_indexed() {
    for (statement, kind) in [
        ("CREATE VIEW plain AS SELECT title FROM documents;", ObjectKind::View),
        ("CREATE SEQUENCE plain;", ObjectKind::Sequence),
    ] {
        let named =
            parse(&format!("{VIEW} {statement} CREATE INDEX plain_title ON plain (title);"));
        let unnamed = parse(&format!("{VIEW} {statement} CREATE INDEX ON plain (title);"));
        for result in [named, unnamed] {
            assert!(
                matches!(
                    &result,
                    Err(Error::RelationKindMismatch { actual_kind, .. }) if *actual_kind == kind
                ),
                "{statement} got {result:?}"
            );
        }
    }
    let missing = parse(&format!("{VIEW} CREATE INDEX absent_title ON absent (title);"));
    assert!(matches!(missing, Err(Error::TableNotFoundForIndex { .. })), "got {missing:?}");
}

#[test]
fn drop_index_and_alter_index_rename_reach_an_index_on_a_materialized_view() {
    let renamed = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title);
         CREATE INDEX titles_body ON titles (body);
         ALTER INDEX titles_title RENAME TO titles_heading;
         DROP INDEX titles_body;
         CREATE INDEX titles_title ON titles (title);"
    ))
    .expect("a renamed index frees its old name");
    assert_eq!(view_index_names(&renamed), [Some("titles_heading"), Some("titles_title")]);

    let taken = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title);
         ALTER INDEX titles_title RENAME TO documents;"
    ));
    assert!(matches!(&taken, Err(Error::RelationNameAlreadyTaken { .. })), "got {taken:?}");
}

#[test]
fn the_indexes_of_a_materialized_view_go_with_it() {
    for drop in [
        "DROP MATERIALIZED VIEW titles;",
        "DROP TABLE documents CASCADE;",
        "ALTER TABLE documents DROP COLUMN body CASCADE;",
    ] {
        let db = parse(&format!(
            "{VIEW}
             CREATE INDEX titles_title ON titles (title);
             {drop}
             CREATE TABLE titles_title (a int);"
        ))
        .expect(drop);
        assert_eq!(db.materialized_view_indexes().count(), 0, "{drop}");
    }
}

#[test]
fn an_index_follows_its_materialized_view_through_renames() {
    let db = parse(&format!(
        "{VIEW}
         CREATE INDEX titles_title ON titles (title) INCLUDE (body) WHERE title <> '';
         ALTER MATERIALIZED VIEW titles RENAME TO headings;
         ALTER MATERIALIZED VIEW headings RENAME COLUMN title TO heading;
         ALTER MATERIALIZED VIEW headings RENAME COLUMN body TO text;"
    ))
    .expect("schema builds");
    let index = db.materialized_view_indexes().next().expect("one index");
    assert_eq!(view_indexed_by(index, &db), "headings");
    assert_eq!(keys(index, &db), ["heading"]);
    assert_eq!(index.include_column_names().collect::<Vec<_>>(), [("text", false)]);
    assert_eq!(index.predicate().map(ToString::to_string).as_deref(), Some("heading <> ''"));
    assert_eq!(db.indexes_on_materialized_view(view(&db, "headings")).expect("held").count(), 1);
}

#[test]
fn renaming_one_materialized_view_leaves_the_indexes_of_another() {
    let db = parse(&format!(
        "{VIEW}
         CREATE MATERIALIZED VIEW bodies AS SELECT identifier, title FROM documents;
         CREATE INDEX titles_title ON titles (title);
         CREATE INDEX bodies_title ON bodies (title);
         ALTER MATERIALIZED VIEW titles RENAME TO headings;
         ALTER MATERIALIZED VIEW headings RENAME COLUMN title TO heading;"
    ))
    .expect("schema builds");
    let untouched =
        db.materialized_view_indexes().find(|index| index.name() == Some("bodies_title"));
    let untouched = untouched.expect("the index on bodies exists");
    assert_eq!(view_indexed_by(untouched, &db), "bodies");
    assert_eq!(keys(untouched, &db), ["title"]);
}

#[test]
fn an_index_follows_its_materialized_view_into_a_renamed_schema() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.documents (identifier integer, title text);
         CREATE MATERIALIZED VIEW app.titles AS SELECT identifier, title FROM app.documents;
         CREATE INDEX titles_title ON app.titles (title);
         ALTER SCHEMA app RENAME TO archive;
         DROP INDEX archive.titles_title;",
    )
    .expect("the index moved with its view");
    assert_eq!(db.materialized_view_indexes().count(), 0);

    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.documents (identifier integer, title text);
         CREATE MATERIALIZED VIEW app.titles AS SELECT identifier, title FROM app.documents;
         CREATE INDEX titles_title ON app.titles (title);
         ALTER SCHEMA app RENAME TO archive;",
    )
    .expect("schema builds");
    assert_eq!(db.materialized_view_indexes().next().expect("one index").schema(), Some("archive"));
}

#[test]
fn a_function_an_index_on_a_materialized_view_calls_is_held_by_it() {
    const CALLED: &str = "CREATE FUNCTION normalized(t text) RETURNS text LANGUAGE sql IMMUTABLE
         RETURN lower(t);";
    let db = parse(&format!(
        "{CALLED} {VIEW} CREATE INDEX titles_normalized ON titles ((normalized(title)));"
    ))
    .expect("schema builds");
    let index = db.materialized_view_indexes().next().expect("one index");
    let key = index.keys(&db).expect("held").next().expect("one key").expression();
    let call = match key {
        Expr::Nested(inner) => {
            match inner.as_ref() {
                Expr::Function(call) => Some(call),
                _ => None,
            }
        }
        _ => None,
    }
    .expect("the key is a parenthesized call");
    let reached: Vec<_> = index
        .functions_reached_by(call, &db)
        .expect("a call of the key")
        .map(FunctionLike::name)
        .collect();
    assert_eq!(reached, ["normalized"]);

    let refused = parse(&format!(
        "{CALLED} {VIEW}
         CREATE INDEX titles_normalized ON titles ((normalized(title)));
         DROP FUNCTION normalized(text);"
    ));
    assert!(
        matches!(
            &refused,
            Err(Error::FunctionReferenced { dependent_kind: ObjectKind::Index, dependent_name, .. })
                if dependent_name == "titles_normalized"
        ),
        "got {refused:?}"
    );

    let cascaded = parse(&format!(
        "{CALLED} {VIEW}
         CREATE INDEX titles_normalized ON titles ((normalized(title)));
         DROP FUNCTION normalized(text) CASCADE;"
    ))
    .expect("CASCADE takes the index");
    assert_eq!(cascaded.materialized_view_indexes().count(), 0);
    assert_eq!(cascaded.materialized_views().count(), 1);
}

#[test]
fn an_index_on_a_materialized_view_calls_a_renamed_function_by_its_new_name() {
    const CALLED: &str = "CREATE FUNCTION normalized(t text) RETURNS text LANGUAGE sql IMMUTABLE
         RETURN lower(t);";
    let renamed = format!(
        "{CALLED} {VIEW}
         CREATE INDEX titles_normalized ON titles ((normalized(title)));
         ALTER FUNCTION normalized(text) RENAME TO folded;"
    );
    let db = parse(&renamed).expect("schema builds");
    let index = db.materialized_view_indexes().next().expect("one index");
    assert_eq!(keys(index, &db), ["(folded(title))"]);

    let refused = parse(&format!("{renamed} DROP FUNCTION folded(text);"));
    assert!(
        matches!(
            &refused,
            Err(Error::FunctionReferenced { dependent_kind: ObjectKind::Index, dependent_name, .. })
                if dependent_name == "titles_normalized"
        ),
        "got {refused:?}"
    );
}

#[test]
fn an_extension_may_supply_the_operator_an_index_on_a_materialized_view_applies() {
    let db = parse(&format!(
        "CREATE EXTENSION pg_trgm; {VIEW}
         CREATE INDEX titles_similar ON titles ((title % 'x'));"
    ))
    .expect("`pg_trgm` declares `%` over `text`");
    let index = db.materialized_view_indexes().next().expect("one index");
    assert_eq!(keys(index, &db), ["(title % 'x')"]);
}

#[test]
fn a_rolled_back_index_on_a_materialized_view_is_gone() {
    let db = parse(&format!(
        "{VIEW}
         BEGIN;
         CREATE INDEX titles_title ON titles (title);
         ROLLBACK;
         CREATE TABLE titles_title (a int);"
    ))
    .expect("the rollback freed the name");
    assert_eq!(db.materialized_view_indexes().count(), 0);
}
