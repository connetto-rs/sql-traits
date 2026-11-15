//! Tests that renaming a schema moves what it holds.
//!
//! PostgreSQL keeps every object by identity, so `ALTER SCHEMA s RENAME TO s2`
//! moves every table, index, sequence, view and materialized view of `s` to
//! `s2`, and every reference to one of them follows: a column default drawing
//! from a sequence, a view or a `BEGIN ATOMIC` body reading a relation, a
//! foreign key, a grant, a rule. The session's `search_path` keeps its text and
//! stops reaching the schema. The expectations below were read from PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn target(name: &str) -> TargetName<'_> {
    match name.split_once('.') {
        Some((schema, name)) => TargetName::new(name, false).with_schema(schema, false),
        None => TargetName::new(name, false),
    }
}

fn table<'db>(db: &'db ParserDB, name: &str) -> Option<&'db <ParserDB as DatabaseLike>::Table> {
    db.table_by_target(target(name), IdentifierCase::AsWritten).expect("unambiguous lookup")
}

/// The schema `s` with relations reaching each other and from `public`.
const SCHEMA: &str = "
CREATE ROLE reader;
CREATE SCHEMA s;
CREATE DOMAIN s.posint AS INT CHECK (VALUE > 0);
CREATE TABLE s.parent (id INT PRIMARY KEY, n s.posint);
CREATE TABLE s.child (id SERIAL PRIMARY KEY, parent_id INT REFERENCES s.parent (id));
CREATE INDEX child_parent ON s.child (parent_id);
CREATE SEQUENCE s.q;
CREATE TABLE public.outside (id INT DEFAULT nextval('s.q'), p INT REFERENCES s.parent (id));
CREATE VIEW s.v AS SELECT id FROM s.child;
CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.parent;
CREATE VIEW public.pv AS SELECT id FROM s.v;
CREATE VIEW public.qv AS SELECT last_value FROM s.q;
CREATE FUNCTION public.counted() RETURNS BIGINT LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM s.parent; END;
CREATE RULE keep AS ON DELETE TO s.v DO INSTEAD NOTHING;
GRANT SELECT ON s.parent, s.v TO reader;
GRANT USAGE ON SEQUENCE s.q TO reader;
GRANT SELECT ON s.q TO reader;
";

#[test]
fn every_relation_of_the_schema_moves() {
    let db = parse(&format!("{SCHEMA} ALTER SCHEMA s RENAME TO s2;")).expect("schema parses");

    for name in ["parent", "child"] {
        assert!(table(&db, &format!("s2.{name}")).is_some(), "{name} moved");
        assert!(table(&db, &format!("s.{name}")).is_none(), "{name} left s");
    }
    let mut sequences: Vec<String> = db
        .sequences()
        .map(|sequence| {
            format!(
                "{}.{}",
                sequence.sequence_schema().unwrap_or("public"),
                sequence.sequence_name()
            )
        })
        .collect();
    sequences.sort();
    assert_eq!(sequences, ["s2.child_id_seq", "s2.q"]);
    let mut views: Vec<String> =
        db.views()
            .map(|view| format!("{}.{}", view.view_schema().unwrap_or("public"), view.view_name()))
            .chain(db.materialized_views().map(|view| {
                format!("{}.{}", view.view_schema().unwrap_or("public"), view.view_name())
            }))
            .collect();
    views.sort();
    assert_eq!(views, ["public.pv", "public.qv", "s2.mv", "s2.v"]);

    let child = table(&db, "s2.child").expect("child moved");
    assert_eq!(child.indices(&db).expect("child is held").count(), 1);
}

#[test]
fn every_reference_follows_the_move() {
    let db = parse(&format!("{SCHEMA} ALTER SCHEMA s RENAME TO s2;")).expect("schema parses");

    let outside = table(&db, "outside").expect("outside stays in public");
    let default = outside
        .columns(&db)
        .expect("outside is held")
        .find(|column| column.column_name() == "id")
        .and_then(|column| column.default_expression().map(ToString::to_string))
        .expect("the default survives");
    assert!(default.contains("'s2.q'"), "{default}");
    for key in outside.foreign_keys(&db).expect("outside is held") {
        let target = key.referenced_table_name();
        assert_eq!((target.schema(), target.name()), (Some("s2"), "parent"));
    }
    let parent = table(&db, "s2.parent").expect("parent moved");
    let n = parent
        .column("n", &db, IdentifierCase::AsWritten)
        .expect("parent is held")
        .expect("n survives");
    assert_eq!(n.data_type(&db).name(&db), "s2.posint");

    let serial = db
        .sequences()
        .find(|sequence| sequence.sequence_name() == "child_id_seq")
        .expect("the serial sequence survives");
    let owner = match serial.owner(&db).expect("owner resolves") {
        Some(SequenceOwner::Column(column)) => Some(column),
        _ => None,
    }
    .expect("the serial sequence keeps its column");
    let owning = ColumnLike::table(owner, &db);
    assert_eq!(
        (owning.table_schema(), owning.table_name(), owner.column_name()),
        (Some("s2"), "child", "id")
    );
    let id = table(&db, "s2.child")
        .expect("child moved")
        .column("id", &db, IdentifierCase::AsWritten)
        .expect("child is held")
        .expect("id survives");
    assert_eq!(
        id.default_expression().map(ToString::to_string).as_deref(),
        Some("nextval('s2.child_id_seq'::REGCLASS)")
    );
    assert!(id.is_serial(&db));

    let pv = db.views().find(|view| view.view_name() == "pv").expect("pv stays");
    assert!(pv.definition().to_string().contains("s2.v"), "{}", pv.definition());
    let qv = db.views().find(|view| view.view_name() == "qv").expect("qv stays");
    assert!(qv.definition().to_string().contains("s2.q"), "{}", qv.definition());
    let counted = db
        .function_by_target(TargetName::new("counted", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("counted stays");
    let read: Vec<String> = counted
        .referenced_tables(&db)
        .expect("counted is held")
        .into_iter()
        .map(|table| format!("{}.{}", table.table_schema().unwrap_or("public"), table.table_name()))
        .collect();
    assert_eq!(read, ["s2.parent"]);

    let rule = db.rules().next().expect("the rule survives");
    assert_eq!(rule.table_name.to_string(), "s2.v");
}

/// Every table of the schema moves, so one referencing a sibling still
/// stands in the old schema whenever the sibling moves first.
#[test]
fn a_table_referencing_a_sibling_moves_whichever_moves_first() {
    for child in [
        "CREATE TABLE s.child (id INT DEFAULT 1, p INT REFERENCES s.parent (id));",
        "CREATE TABLE s.child (id INT PRIMARY KEY, p INT REFERENCES s.parent (id));",
    ] {
        let db = parse(&format!(
            "CREATE SCHEMA s;
             CREATE TABLE s.parent (id INT PRIMARY KEY);
             {child}
             ALTER SCHEMA s RENAME TO s2;"
        ))
        .expect("schema parses");
        let child = table(&db, "s2.child").expect("child moved");
        for key in child.foreign_keys(&db).expect("child is held") {
            let target = key.referenced_table_name();
            assert_eq!((target.schema(), target.name()), (Some("s2"), "parent"));
        }
    }
}

#[test]
fn grants_follow_the_move() {
    let db = parse(&format!("{SCHEMA} ALTER SCHEMA s RENAME TO s2;")).expect("schema parses");
    let reader = db.role("reader").expect("the role is declared");

    let parent = table(&db, "s2.parent").expect("parent moved");
    assert_eq!(parent.privileges(reader, &db).map(|held| held.select.on_table()), Ok(true));
    let mut reached: Vec<String> = db
        .table_grants()
        .filter(|grant| grant.applies_to_role(reader))
        .flat_map(|grant| grant.relations(&db).collect::<Vec<_>>())
        .map(|relation| {
            match relation {
                Relation::Table(table) => format!("table {}", table.table_name()),
                Relation::ForeignTable(table) => format!("foreign table {}", table.table_name()),
                Relation::View(view) => {
                    format!("view {}.{}", view.view_schema().unwrap_or("public"), view.view_name())
                }
                Relation::MaterializedView(view) => {
                    format!("materialized view {}", view.view_name())
                }
                Relation::Sequence(sequence) => {
                    format!(
                        "sequence {}.{}",
                        sequence.sequence_schema().unwrap_or("public"),
                        sequence.sequence_name()
                    )
                }
            }
        })
        .collect();
    reached.sort();
    assert_eq!(reached, ["sequence s2.q", "sequence s2.q", "table parent", "view s2.v"]);
}

/// The session's path keeps naming the old schema, which no longer exists,
/// so an unqualified name lands in the next schema on it.
#[test]
fn the_search_path_keeps_its_text() {
    let db = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.parent (id INT);
         SET search_path = s, public;
         ALTER SCHEMA s RENAME TO s2;
         CREATE TABLE parent (id INT);",
    )
    .expect("schema parses");

    assert!(table(&db, "s2.parent").is_some());
    assert!(table(&db, "public.parent").is_some());
}
