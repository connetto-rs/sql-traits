//! Tests that an index its statement leaves unnamed takes the name PostgreSQL
//! gives it, and is then reached and contested under that name.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::{ObjectName, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The index names a schema ends up with, sorted.
fn index_names(sql: &str) -> Vec<Option<String>> {
    let db = parse(sql).expect("schema builds");
    let mut names: Vec<_> =
        db.indexes().map(|index| index.name().map(ToString::to_string)).collect();
    names.sort();
    names
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

#[test]
fn an_unnamed_index_takes_the_name_postgresql_gives_it() {
    let a61 = "a".repeat(61);
    let b63 = "b".repeat(63);
    let long = format!(
        "CREATE TABLE {a61} ({b63} INT, c INT);
         CREATE INDEX ON {a61} ({b63}, c);
         CREATE INDEX ON {a61} ({b63}, c);"
    );
    // A cut that would split a character stops before it.
    let accented = format!("a{}", "é".repeat(30));
    let multibyte = format!(
        "CREATE TABLE {accented} (bbbbbbbbbb INT); CREATE INDEX ON {accented} (bbbbbbbbbb);"
    );
    let cases: [(&str, &[&str]); 8] = [
        (
            "CREATE TABLE t (s TEXT);
             CREATE INDEX ON t (s);
             CREATE UNIQUE INDEX ON t (s);",
            &["t_s_idx", "t_s_idx1"],
        ),
        (
            "CREATE TABLE t (s TEXT);
             CREATE INDEX ON t ((s::varchar));
             CREATE INDEX ON t ((CAST(s AS text)));
             CREATE INDEX ON t ((s COLLATE \"C\"));",
            &["t_s_idx", "t_s_idx1", "t_s_idx2"],
        ),
        (
            "CREATE TABLE t (s TEXT, n INT);
             CREATE INDEX ON t (lower(s));
             CREATE INDEX ON t (coalesce(s, 'a'));
             CREATE INDEX ON t ((CASE WHEN n > 0 THEN 1 END));
             CREATE INDEX ON t ((s || 'x'));
             CREATE INDEX ON t ((-n));
             CREATE INDEX ON t ((n + 1), n);
             CREATE INDEX ON t (s, s);
             CREATE INDEX ON t (lower(s), lower(s));
             CREATE INDEX ON t (((n + 1)::int));
             CREATE INDEX ON t (n) INCLUDE (s);",
            &[
                "t_lower_idx",
                "t_coalesce_idx",
                "t_case_idx",
                "t_expr_idx",
                "t_expr_idx1",
                "t_expr_n_idx",
                "t_s_s1_idx",
                "t_lower_lower1_idx",
                "t_int4_idx",
                "t_n_s_idx",
            ],
        ),
        (
            "CREATE TABLE t (\"Mixed Col\" INT);
             CREATE INDEX ON t (\"Mixed Col\");
             CREATE TABLE \"Tab\" (a INT);
             CREATE INDEX ON \"Tab\" (a);
             CREATE TABLE UP (S TEXT);
             CREATE INDEX ON UP (S);",
            &["t_Mixed Col_idx", "Tab_a_idx", "up_s_idx"],
        ),
        (
            "CREATE TABLE t (n INT);
             CREATE TABLE t_n_idx (x INT);
             CREATE VIEW t_n_idx1 AS SELECT 1 AS one;
             CREATE INDEX ON t (n);",
            &["t_n_idx2"],
        ),
        (
            "CREATE TABLE u (s TEXT);
             CREATE INDEX ON u (s);
             CREATE INDEX ON u (s);
             DROP INDEX u_s_idx;
             CREATE INDEX ON u (s);",
            &["u_s_idx1", "u_s_idx"],
        ),
        (
            &long,
            &[
                &format!("{}_{}_idx", &a61[..29], &b63[..29]),
                &format!("{}_{}_idx1", &a61[..29], &b63[..28]),
            ],
        ),
        (&multibyte, &[&format!("a{}_bbbbbbbbbb_idx", "é".repeat(23))]),
    ];

    for (sql, expected) in cases {
        let mut expected: Vec<Option<String>> =
            expected.iter().map(|name| Some((*name).to_string())).collect();
        expected.sort();
        assert_eq!(index_names(sql), expected, "for {sql}");
    }
}

#[test]
fn an_index_counts_the_names_in_its_own_tables_schema() {
    let db = parse(
        "CREATE SCHEMA other;
         CREATE TABLE t (s TEXT);
         CREATE TABLE other.t (s TEXT);
         CREATE INDEX ON t (s);
         CREATE INDEX ON other.t (s);
         SET search_path TO other;
         CREATE INDEX ON t (s);",
    )
    .expect("schema builds");

    let mut placed: Vec<(Option<&str>, Option<&str>)> = db
        .indexes()
        .map(|index| (IndexLike::table(index, &db).table_schema(), index.name()))
        .collect();
    placed.sort_unstable();
    assert_eq!(
        placed,
        [
            (None, Some("t_s_idx")),
            (Some("other"), Some("t_s_idx")),
            (Some("other"), Some("t_s_idx1")),
        ]
    );
}

#[test]
fn a_generated_name_is_reached_and_contested_like_a_written_one() {
    const INDEXED: &str = "CREATE TABLE t (s TEXT); CREATE INDEX ON t (s);";
    const QUOTED: &str = "CREATE TABLE \"Tab\" (a INT); CREATE INDEX ON \"Tab\" (a);";

    let dropped = parse(&format!("{INDEXED} DROP INDEX t_s_idx;")).expect("the index is found");
    assert_eq!(dropped.indexes().count(), 0);

    let renamed = parse(&format!("{INDEXED} ALTER INDEX T_S_IDX RENAME TO kept;"))
        .expect("the unquoted spelling folds onto the generated name");
    assert_eq!(renamed.indexes().filter_map(IndexLike::name).collect::<Vec<_>>(), ["kept"]);

    let contested = parse(&format!("{INDEXED} CREATE TABLE t_s_idx (x INT);"));
    assert!(
        matches!(
            &contested,
            Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Table,
                conflicting_kind: ObjectKind::Index,
                ..
            })
        ),
        "got {contested:?}"
    );

    assert!(
        parse(&format!("{QUOTED} DROP INDEX Tab_a_idx;")).is_err(),
        "the folded spelling misses a name holding capitals"
    );
    parse(&format!("{QUOTED} DROP INDEX \"Tab_a_idx\";")).expect("the quoted spelling is found");
}

#[test]
fn an_index_over_an_expression_whose_name_is_not_modeled_stays_unnamed() {
    assert_eq!(
        index_names(
            "CREATE TABLE t (s TEXT);
             CREATE INDEX ON t (((s || 'x')::nvarchar));
             CREATE INDEX ON t (((s || 'x')::nvarchar));"
        ),
        [None, None]
    );
}

#[test]
fn naming_an_index_as_create_index_name_answers_stores_the_same_index() {
    let statements = Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE TABLE t (s TEXT, n INT);
         CREATE TABLE t_s_idx (x INT);
         CREATE INDEX ON t (s);
         CREATE INDEX ON t (lower(s), n);
         CREATE INDEX ON t (((s || 'x')::nvarchar));
         CREATE INDEX ON t (s);",
    )
    .expect("SQL parses");

    let mut unnamed = ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned());
    let mut named = ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned());
    let mut answers = Vec::new();
    for statement in statements {
        let mut written = statement.clone();
        if let Statement::CreateIndex(create_index) = &mut written {
            let name = named.create_index_name(create_index).expect("the table exists");
            answers.push(name.as_ref().map(|name| name.value.clone()));
            create_index.name = name.map(|name| ObjectName::from(vec![name]));
        }
        unnamed = unnamed.apply_statement(statement).expect("unnamed statement applies");
        named = named.apply_statement(written).expect("named statement applies");
    }

    assert_eq!(
        answers,
        [
            Some("t_s_idx1".to_owned()),
            Some("t_lower_n_idx".to_owned()),
            None,
            Some("t_s_idx2".to_owned())
        ]
    );
    let stored = |ingestor: ParserDBIngestor| {
        ingestor.finish().indexes().map(|index| index.attribute().clone()).collect::<Vec<_>>()
    };
    assert_eq!(stored(unnamed), stored(named));
}

#[test]
fn create_index_name_answers_a_written_name_and_refuses_a_missing_table() {
    let ingestor = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (s TEXT);")
        .expect("schema builds")
        .into_ingestor();
    let create_index = |sql| {
        match Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("SQL parses").remove(0) {
            Statement::CreateIndex(create_index) => create_index,
            other => panic!("expected CREATE INDEX, got {other}"),
        }
    };

    let written = ingestor
        .create_index_name(&create_index("CREATE INDEX \"Mine\" ON t (s);"))
        .expect("a written name is answered");
    assert_eq!(
        written.map(|name| (name.value, name.quote_style)),
        Some(("Mine".to_owned(), Some('"')))
    );

    let missing = ingestor.create_index_name(&create_index("CREATE INDEX ON absent (s);"));
    assert!(matches!(missing, Err(Error::TableNotFoundForIndex { .. })), "got {missing:?}");
    let applied = parse("CREATE INDEX ON absent (s);");
    assert!(matches!(applied, Err(Error::TableNotFoundForIndex { .. })), "got {applied:?}");
}

#[test]
fn a_partition_key_name_is_cut_to_fit_and_counted_in_the_schema() {
    let a61 = "a".repeat(61);
    let b63 = "b".repeat(63);
    let db = parse(&format!(
        "CREATE TABLE r ({b63} INT PRIMARY KEY, c INT, UNIQUE ({b63}, c)) PARTITION BY RANGE ({b63});
         CREATE TABLE {a61} PARTITION OF r FOR VALUES FROM (1) TO (2);
         CREATE TABLE {a61}2 PARTITION OF r FOR VALUES FROM (2) TO (3);"
    ))
    .expect("schema builds");

    let key_names = |partition: &str| {
        let mut names: Vec<String> = table(&db, partition)
            .unique_indices(&db)
            .expect("in database")
            .filter_map(|index| IndexLike::name(index).map(ToString::to_string))
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        key_names(&a61),
        [format!("{}_{}_key", &a61[..29], &b63[..29]), format!("{}_pkey", &a61[..58])]
    );
    assert_eq!(
        key_names(&format!("{a61}2")),
        [format!("{}_{}_key1", &a61[..29], &b63[..28]), format!("{}_pkey1", &a61[..57])]
    );
}

#[test]
fn a_partition_names_its_copy_of_an_inline_unique_key_after_the_column() {
    let db = parse(
        "CREATE TABLE r (A INT UNIQUE) PARTITION BY RANGE (A);
         CREATE TABLE p PARTITION OF r FOR VALUES FROM (1) TO (2);",
    )
    .expect("schema builds");

    let names: Vec<_> = table(&db, "p")
        .unique_indices(&db)
        .expect("in database")
        .filter_map(|index| IndexLike::name(index).map(ToString::to_string))
        .collect();
    assert_eq!(names, ["p_a_key"]);
}
