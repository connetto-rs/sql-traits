//! Tests that a function body, written `RETURN <expression>` or as a
//! `BEGIN ATOMIC` block, is bound to the relations it reads, as PostgreSQL
//! does at `CREATE FUNCTION` time.
//!
//! Every rule asserted here was measured against PostgreSQL 18.6 in Docker
//! rather than read from documentation.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::{ast::CreateFunction, dialect::PostgreSqlDialect, parser::Parser};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn function<'db>(db: &'db ParserDB, name: &str) -> &'db CreateFunction {
    db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the function is recorded")
}

fn body(db: &ParserDB, name: &str) -> String {
    function(db, name).body_expression().expect("a RETURN body").to_string()
}

/// The stored function as the parser would spell it, body included.
fn text(db: &ParserDB, name: &str) -> String {
    function(db, name).to_string()
}

/// The refusal a block body gets when its final statement produces no result.
fn refuses_missing_result(sql: &str) {
    let refused = parse(sql).expect_err("the final statement produces no result");
    assert!(
        matches!(&refused, Error::FunctionBodyResultMissing { function_name } if function_name == "f"),
        "got {refused:?}"
    );
}

/// A `BEGIN ATOMIC` body resolves under the session's `search_path` in
/// effect at `CREATE FUNCTION`, one statement at a time, and a `WITH` item
/// answers its own name rather than a relation.
#[test]
fn a_block_body_resolves_under_the_creation_search_path() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE t (a INT, b INT);
         CREATE TABLE u (a INT);
         CREATE TABLE app.w (a INT);
         SET search_path = app, public;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t (a) SELECT a FROM u;
             UPDATE t SET b = a WHERE a IN (SELECT a FROM w);
             DELETE FROM t WHERE a = 0;
             WITH x AS (SELECT a FROM u) SELECT count(*)::int FROM x, t;
         END;",
    )
    .expect("every statement binds under the session path");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
         INSERT INTO t (a) SELECT a FROM u; \
         UPDATE t SET b = a WHERE a IN (SELECT a FROM app.w); \
         DELETE FROM t WHERE a = 0; \
         WITH x AS (SELECT a FROM u) SELECT count(*)::INT FROM x, t; END",
        "t and u answer in public and w in app, while x reads the WITH item"
    );

    // A `WITH` item is not a relation, so a table that answers its name
    // holds no dependency the block body reads.
    let db = parse(
        "CREATE TABLE x (a INT);
         CREATE TABLE u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT a FROM u) SELECT count(*)::int FROM x;
         END;
         DROP TABLE x;",
    )
    .expect("the WITH item is not a relation");
    assert!(
        db.function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some(),
        "the function outlives the table the item shadows"
    );
}

/// A relation any statement of the block names that the database does not
/// hold refuses the statement, before it records or replaces anything.
#[test]
fn a_block_body_naming_an_absent_relation_is_refused() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t (a) SELECT a FROM u;
             DELETE FROM t WHERE a IN (SELECT a FROM nope);
         END;",
    )
    .expect_err("nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, function_name }
            if relation_name == "nope" && function_name == "f"),
        "got {refused:?}"
    );

    // A relation a statement's own target names is refused too.
    let refused = parse(
        "CREATE TABLE u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO nope (a) VALUES (1);
             SELECT a FROM u;
         END;",
    )
    .expect_err("nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, .. }
            if relation_name == "nope"),
        "got {refused:?}"
    );
}

/// The stored block follows renames and schema moves of the relations its
/// statements write, tables and columns alike. The pinned parser does not
/// read `ALTER TABLE ... SET SCHEMA`, so the cross-schema case uses the
/// `RENAME TABLE` spelling the model accepts for a schema move.
#[test]
fn a_block_body_follows_renames_of_what_it_writes() {
    let db = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE TABLE u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t (a) SELECT a FROM u;
             UPDATE t SET b = a WHERE a IN (SELECT a FROM u);
             DELETE FROM t WHERE a = 0;
             RETURN (SELECT a FROM u);
         END;
         ALTER TABLE t RENAME COLUMN a TO aa;",
    )
    .expect("the column rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
         INSERT INTO t (aa) SELECT a FROM u; \
         UPDATE t SET b = t.aa WHERE t.aa IN (SELECT a FROM u); \
         DELETE FROM t WHERE t.aa = 0; \
         RETURN (SELECT a FROM u); END",
        "the column rename is followed in the target, the SET targets and the references"
    );

    let db = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE TABLE u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t (a) SELECT a FROM u;
             DELETE FROM t WHERE a = 0;
             RETURN (SELECT a FROM u);
         END;
         ALTER TABLE u RENAME TO uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
         INSERT INTO t (a) SELECT a FROM uu; \
         DELETE FROM t WHERE a = 0; \
         RETURN (SELECT a FROM uu); END",
        "the rename is followed in every statement that reads the table"
    );

    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE t (a INT, b INT);
         CREATE TABLE w (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE t SET b = a WHERE a IN (SELECT a FROM w);
         END;
         RENAME TABLE w TO app.w;",
    )
    .expect("the move is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         UPDATE t SET b = a WHERE a IN (SELECT a FROM app.w); END",
        "the schema move is followed in the subquery"
    );

    let db = parse(
        "CREATE TABLE t (id INT, a INT);
         CREATE TABLE u (id INT, a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE t SET a = u.a FROM u WHERE t.id = u.id;
         END;
         ALTER TABLE u RENAME TO uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         UPDATE t SET a = uu.a FROM uu WHERE t.id = uu.id; END",
        "the rename is followed in the clause factor and the qualified references"
    );

    let db = parse(
        "CREATE TABLE t (id INT, a INT);
         CREATE TABLE s (id INT, a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO t USING s ON t.id = s.id
             WHEN MATCHED THEN UPDATE SET a = s.a
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a);
         END;
         ALTER TABLE s RENAME TO ss;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         MERGE INTO t USING ss ON t.id = ss.id WHEN MATCHED THEN UPDATE SET a = ss.a \
         WHEN NOT MATCHED THEN INSERT (id, a) VALUES (ss.id, ss.a); END",
        "the rename is followed in the merge source and its references"
    );
}

/// A relation a block statement writes cannot be dropped without `CASCADE`,
/// and `CASCADE` takes the function with it.
#[test]
fn a_relation_a_block_body_writes_cannot_be_dropped_from_under_it() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM t WHERE a = 0;
         END;
         DROP TABLE t;",
    )
    .expect_err("f writes t");
    assert!(
        matches!(&refused, Error::RelationHasDependents { object_kind, object_name, dependent_kind, dependent_name }
            if *object_kind == ObjectKind::Table
                && object_name == "t"
                && *dependent_kind == ObjectKind::Function
                && dependent_name == "f"),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t (a) VALUES (1);
         END;
         DROP TABLE t CASCADE;",
    )
    .expect("cascade is accepted");
    assert!(
        db.function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the function went with the table"
    );
}

/// `RETURN (SELECT ...)` may stand as a statement of the block and binds the
/// way the block's other statements bind; `SET` and `CALL` are refused at
/// ingestion.
#[test]
fn a_block_body_return_set_and_call_statements() {
    let refused = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             RETURN (SELECT a FROM nope);
         END;",
    )
    .expect_err("nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, .. }
            if relation_name == "nope"),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             RETURN (SELECT a FROM t);
         END;
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC RETURN (SELECT a FROM t2); END",
        "the return statement follows the rename"
    );

    let refused = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             SET search_path = 'app';
         END;",
    )
    .expect_err("SET is not supported in an unquoted SQL function body");
    assert!(
        matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
            if function_name == "f"),
        "got {refused:?}"
    );

    let refused = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             CALL p();
             SELECT 1;
         END;",
    )
    .expect_err("CALL is not supported in an unquoted SQL function body");
    assert!(
        matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
            if function_name == "f"),
        "got {refused:?}"
    );
}

/// `CREATE OR REPLACE` binds the new block afresh, so the old body's
/// relations stop being read, and a body written as a string stays unbound.
#[test]
fn a_replacement_binds_a_block_body_afresh_and_a_string_block_stays_unbound() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             RETURN (SELECT a FROM t);
         END;
         CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             RETURN (SELECT b FROM u);
         END;
         DROP TABLE t;",
    )
    .expect("the old body no longer reads t");
    assert_eq!(
        text(&db, "f"),
        "CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql \
         BEGIN ATOMIC RETURN (SELECT b FROM u); END"
    );

    // A block written as a string is never bound, so a name it cannot
    // resolve refuses nothing.
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION q() RETURNS INT LANGUAGE sql
             AS $$ BEGIN ATOMIC INSERT INTO nope (a) VALUES (1); END; $$;
         DROP TABLE app.t;",
    )
    .expect("string bodies are never bound");
    assert_eq!(
        function(&db, "q").body(),
        Some(" BEGIN ATOMIC INSERT INTO nope (a) VALUES (1); END; ")
    );
}
/// A body resolves under the session's `search_path` in effect at
/// `CREATE FUNCTION`; the function's own `SET search_path` clause plays no
/// part.
#[test]
fn a_return_body_resolves_under_the_creation_search_path() {
    let db = parse(
        "SET search_path = public;
         CREATE SCHEMA app;
         CREATE TABLE t (a INT);
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql SET search_path = app
             RETURN (SELECT a FROM t);",
    )
    .expect("the body binds under the session path");
    assert_eq!(
        body(&db, "r"),
        "(SELECT a FROM t)",
        "the public table answers the name, not the app one"
    );
}

/// A relation the body names that the database does not hold refuses the
/// statement, before it records or replaces anything.
#[test]
fn a_return_body_naming_an_absent_relation_is_refused() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM nope);",
    )
    .expect_err("nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, function_name }
            if relation_name == "nope" && function_name == "r"),
        "got {refused:?}"
    );

    // A schema-qualified miss is refused under the name the body wrote.
    let refused = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM app.nope);",
    )
    .expect_err("app.nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, .. }
            if relation_name == "app.nope"),
        "got {refused:?}"
    );

    // A name a visible `WITH` item answers is left alone, as for a view.
    parse(
        "CREATE FUNCTION r() RETURNS INT LANGUAGE sql
            RETURN ((WITH nope AS (SELECT 1 AS a) SELECT a FROM nope));",
    )
    .expect("the WITH item answers the name");

    // A failed replacement refuses before it touches the old function.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);",
    )
    .expect("the function is recorded");
    let statement = Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE OR REPLACE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM nope);",
    )
    .expect("SQL parses")
    .pop()
    .expect("one statement");
    let Err(refused) = db.into_ingestor().apply_statement(statement) else {
        panic!("a body naming an absent relation is refused");
    };
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { function_name, .. }
            if function_name == "r"),
        "got {refused:?}"
    );
}

/// The stored body follows renames and schema moves of the relations it
/// reads, tables, views and materialized views alike. The pinned parser does
/// not read `ALTER TABLE ... SET SCHEMA`, so the cross-schema case uses the
/// `RENAME TABLE` spelling the model accepts for a schema move.
#[test]
fn a_return_body_follows_renames_of_what_it_reads() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM t2)");

    // A qualified column reference follows too.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT t.a FROM t);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT t2.a FROM t2)");

    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE u (a INT);
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM u);
         RENAME TABLE u TO app.u;",
    )
    .expect("the move is accepted");
    assert_eq!(body(&db, "g"), "(SELECT a FROM app.u)");
    let moved = db
        .table_by_target(
            TargetName::new("u", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table is recorded");
    assert_eq!(moved.table_schema().map(ToString::to_string).as_deref(), Some("app"));

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         ALTER TABLE v RENAME TO w;",
    )
    .expect("the view rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM w)");

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM m);
         ALTER TABLE m RENAME TO n;",
    )
    .expect("the materialized view rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM n)");

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         ALTER TABLE t RENAME COLUMN a TO b;",
    )
    .expect("the column rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT b AS a FROM t)");
}

/// A body that reads only a relation the rename does not touch stays as
/// written.
#[test]
fn a_return_body_reading_an_unrenamed_relation_stays_as_written() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM u);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM u)");
}

/// A relation the body reads cannot be dropped without `CASCADE`, and
/// `CASCADE` takes the function with it.
#[test]
fn a_relation_a_function_reads_cannot_be_dropped_from_under_it() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         DROP TABLE t;",
    )
    .expect_err("r reads t");
    assert!(
        matches!(&refused, Error::RelationHasDependents { object_kind, object_name, dependent_kind, dependent_name }
            if *object_kind == ObjectKind::Table
                && object_name == "t"
                && *dependent_kind == ObjectKind::Function
                && dependent_name == "r"),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         DROP TABLE t CASCADE;",
    )
    .expect("cascade is accepted");
    assert!(
        db.function_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the function went with the table"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         DROP VIEW v;",
    )
    .expect_err("r reads v");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         DROP VIEW v CASCADE;",
    )
    .expect("cascade is accepted");
    assert!(
        db.function_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the function went with the view"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM m);
         DROP MATERIALIZED VIEW m;",
    )
    .expect_err("r reads m");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );
}

/// A body may read a temporary relation, and the function stays where it was
/// created, not temporary.
#[test]
fn a_return_body_may_read_a_temporary_relation() {
    let db = parse(
        "CREATE TEMP TABLE scratch (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM scratch);",
    )
    .expect("a body may read a temporary table");
    assert_eq!(body(&db, "r"), "(SELECT a FROM pg_temp.scratch)");
    assert!(
        function(&db, "r").target_name().schema().is_none(),
        "the function stays in the default schema"
    );

    // The temporary read still blocks a drop of the table.
    let refused = parse(
        "CREATE TEMP TABLE scratch (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM scratch);
         DROP TABLE scratch;",
    )
    .expect_err("r reads scratch");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );
}

/// A body written as a string is never bound and holds no dependency.
#[test]
fn a_string_body_is_never_bound() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION q() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';
         CREATE FUNCTION w() RETURNS INT LANGUAGE sql AS $$SELECT a FROM nope$$;",
    )
    .expect("string bodies are never bound");
    assert_eq!(function(&db, "q").body(), Some("SELECT a FROM t"));
    assert!(function(&db, "q").body_expression().is_none());
    assert_eq!(function(&db, "w").body(), Some("SELECT a FROM nope"));

    // Nothing a string body names blocks a drop.
    parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION q() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';
         DROP TABLE app.t;",
    )
    .expect("a string body holds no dependency");
}

/// `CREATE OR REPLACE` binds the replacement afresh, so the old body's
/// relations stop being read.
#[test]
fn a_replacement_binds_the_new_body_afresh() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (b INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         CREATE OR REPLACE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT b FROM u);
         DROP TABLE t;",
    )
    .expect("the old body no longer reads t");
    assert_eq!(body(&db, "r"), "(SELECT b FROM u)");
    assert!(
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "t went with the old body's last reader"
    );
}

/// A `BEGIN ATOMIC` statement respells its own clauses the way PostgreSQL
/// respells them after the renames: the columns a write target lists, the
/// clause factors, and the references its clauses own, a reference nothing
/// in reach answers kept as written.
#[test]
fn a_block_body_statement_follows_renames_of_its_own_clauses() {
    let db = parse(
        "CREATE TABLE q1t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q1t SET a = 0;
             RETURN 1;
         END;
         ALTER TABLE q1t RENAME COLUMN a TO b;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q1t SET b = 0; RETURN 1; END",
        "the column rename is followed in the SET target"
    );

    let db = parse(
        "CREATE TABLE q14t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q14t SET a = 0 WHERE q14t.a > 0;
             SELECT 1;
         END;
         ALTER TABLE q14t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q14t SET aa = 0 WHERE q14t.aa > 0; SELECT 1; END",
        "the qualified reference follows the rename"
    );

    let db = parse(
        "CREATE TABLE q15t (a INT, c INT);
         CREATE TABLE q15u (b INT);
         CREATE FUNCTION f(y INT) RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q15t SET a = b FROM q15u WHERE q15t.c > y;
             SELECT 1;
         END;
         ALTER TABLE q15t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f(y INT) RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q15t SET aa = b FROM q15u WHERE q15t.c > y; SELECT 1; END",
        "the rename touches only the target's own column"
    );

    let db = parse(
        "CREATE TABLE qt (a INT);
         CREATE TABLE qu (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE qt SET a = 0 FROM qu AS s WHERE qt.a > s.b;
             SELECT 1;
         END;
         ALTER TABLE qu RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE qt SET a = 0 FROM qu AS s WHERE qt.a > s.bb; SELECT 1; END",
        "the aliased factor's reference follows the rename"
    );
}

/// Aliases and tuple targets in a block statement follow renames the way
/// PostgreSQL stores them.
#[test]
fn a_block_body_statement_follows_renames_through_aliases() {
    // An alias column list pins the factor's output names, so the rename of
    // one of them stays as written under the pinned name.
    let db = parse(
        "CREATE TABLE q16t (a INT);
         CREATE TABLE q16u (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q16t SET a = q16u.a FROM q16u AS q16u (a, b) WHERE q16u.b > 0;
             SELECT 1;
         END;
         ALTER TABLE q16u RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q16t SET a = q16u.a FROM q16u AS q16u (a, b) WHERE q16u.b > 0; SELECT 1; END",
        "the alias column list pins the old name"
    );

    // A tuple target lists the target's columns by name.
    let db = parse(
        "CREATE TABLE pt (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE pt SET (a, b) = (1, 2) WHERE a > 0;
             SELECT 1;
         END;
         ALTER TABLE pt RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE pt SET (aa, b) = (1, 2) WHERE pt.aa > 0; SELECT 1; END",
        "the tuple target and the reference follow the rename"
    );

    let db = parse(
        "CREATE TABLE q6t (a INT);
         CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q6t WHERE a > x;
             SELECT x;
         END;
         ALTER TABLE q6t RENAME TO q6tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q6tt WHERE a > x; SELECT x; END",
        "the factor follows the rename and the parameter reference stays as written"
    );

    let db = parse(
        "CREATE TABLE q12t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO q12t (a) VALUES (1);
             SELECT 1;
         END;
         ALTER TABLE q12t RENAME TO q12tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO q12tt (a) VALUES (1); SELECT 1; END",
        "the insert target follows the rename"
    );

    let db = parse(
        "CREATE TABLE s19ft (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO s19ft AS s19fa (a, b) VALUES (1, 2);
             SELECT 1;
         END;
         ALTER TABLE s19ft RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO s19ft AS s19fa (aa, b) VALUES (1, 2); SELECT 1; END",
        "the target's own column list follows the rename under the alias"
    );

    let db = parse(
        "CREATE TABLE s19gt (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO s19gt AS s19ga (a, b) VALUES (1, 2);
             SELECT 1;
         END;
         ALTER TABLE s19gt RENAME TO s19g2;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO s19g2 AS s19ga (a, b) VALUES (1, 2); SELECT 1; END",
        "the aliased target follows the rename"
    );
}

/// A temporary relation follows its rename bare, the way PostgreSQL drops its
/// `pg_temp` qualifier from a stored body, and a string body holds no
/// relation to follow.
#[test]
fn a_block_body_statement_follows_temporary_renames_bare() {
    let db = parse(
        "CREATE TEMP TABLE q20t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q20t WHERE a = 0;
             SELECT 1;
         END;
         ALTER TABLE q20t RENAME TO q20t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q20t2 WHERE a = 0; SELECT 1; END",
        "the temporary target follows the rename without its qualifier"
    );

    // A string body holds no relation to follow while a block body does.
    let db = parse(
        "CREATE TABLE ps2t (a INT);
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE ps2t SET a = 0;
             SELECT 1;
         END;
         ALTER TABLE ps2t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE ps2t SET aa = 0; SELECT 1; END",
        "the block body follows the rename"
    );
}

/// A join in a statement's own clauses is a clause factor of its own, as is
/// a derived relation, which keeps reading what it reads.
#[test]
fn a_block_body_statement_follows_renames_of_its_clause_factors() {
    let db = parse(
        "CREATE TABLE q8t (id INT, a INT);
         CREATE TABLE q8u (id INT);
         CREATE TABLE q8v (id INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q8t SET a = 1 FROM q8u JOIN q8v ON q8u.id = q8v.id WHERE q8t.id = q8u.id;
             SELECT 1;
         END;
         ALTER TABLE q8u RENAME TO q8uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q8t SET a = 1 FROM q8uu JOIN q8v ON q8uu.id = q8v.id WHERE q8t.id = q8uu.id; SELECT 1; END",
        "the joined factor and its references follow the rename"
    );

    let db = parse(
        "CREATE TABLE q9t (id INT, a INT);
         CREATE TABLE q9u (id INT);
         CREATE TABLE q9v (id INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q9t USING q9u JOIN q9v ON q9u.id = q9v.id WHERE q9t.id = q9u.id;
             SELECT 1;
         END;
         ALTER TABLE q9u RENAME TO q9uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q9t USING q9uu JOIN q9v ON q9uu.id = q9v.id WHERE q9t.id = q9uu.id; SELECT 1; END",
        "the using join follows the rename"
    );

    let db = parse(
        "CREATE TABLE qct (id INT, a INT);
         CREATE TABLE qcu (id INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM qct USING qcu WHERE qct.id = qcu.id AND qct.a > 0;
             SELECT 1;
         END;
         ALTER TABLE qcu RENAME TO qcuu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM qct USING qcuu WHERE qct.id = qcuu.id AND qct.a > 0; SELECT 1; END",
        "the using factor follows the rename"
    );

    let db = parse(
        "CREATE TABLE q24t (a INT);
         CREATE TABLE q24u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q24t USING (SELECT a FROM q24u) x WHERE q24t.a = x.a;
             SELECT 1;
         END;
         ALTER TABLE q24t RENAME TO q24tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q24tt USING (SELECT a FROM q24u) x WHERE q24tt.a = x.a; SELECT 1; END",
        "the target follows the rename and the derived relation keeps its own reads"
    );
}

/// A `MERGE` clause's actions name the target's columns.
#[test]
fn a_block_body_merge_follows_renames_of_its_clauses() {
    let db = parse(
        "CREATE TABLE q11t (id INT, a INT);
         CREATE TABLE q11s (id INT, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO q11t USING q11s ON q11t.id = q11s.id
             WHEN MATCHED THEN DELETE
             WHEN NOT MATCHED THEN DO NOTHING;
             SELECT 1;
         END;
         ALTER TABLE q11s RENAME TO q11ss;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC MERGE INTO q11t USING q11ss ON q11t.id = q11ss.id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN DO NOTHING; SELECT 1; END",
        "the source follows the rename and the listing-free actions stay as written"
    );

    let db = parse(
        "CREATE TABLE q11ct (id INT, a INT);
         CREATE TABLE q11cs (id INT, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO q11ct USING q11cs ON q11ct.id = q11cs.id
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (q11cs.id, q11cs.a);
             SELECT 1;
         END;
         ALTER TABLE q11ct RENAME TO q11ctt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC MERGE INTO q11ctt USING q11cs ON q11ctt.id = q11cs.id WHEN NOT MATCHED THEN INSERT (id, a) VALUES (q11cs.id, q11cs.a); SELECT 1; END",
        "the target follows the rename and the insert lists its own columns"
    );

    // A wildcard `MERGE UPDATE SET` PostgreSQL's parser does not accept is
    // refused at ingestion.
    let refused = parse(
        "CREATE TABLE pw (id INT, a INT);
         CREATE TABLE ps (id INT, a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO pw USING ps ON pw.id = ps.id
             WHEN MATCHED THEN UPDATE SET *;
             SELECT 1;
         END;",
    )
    .expect_err("the wildcard update is not a clause shape the body holds");
    assert!(
        matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
            if function_name == "f"),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE pmgt (a INT, b INT);
         CREATE TABLE pmgx (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO pmgt USING pmgx ON pmgt.a = pmgx.a
             WHEN MATCHED THEN UPDATE SET (a, b) = (1, 2);
             SELECT 1;
         END;
         ALTER TABLE pmgt RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC MERGE INTO pmgt USING pmgx ON pmgt.aa = pmgx.a WHEN MATCHED THEN UPDATE SET (aa, b) = (1, 2); SELECT 1; END",
        "the tuple update lists the target's columns and the reference follows the rename"
    );
}

/// A clause reference the renames make uncertain is shielded, each renamed
/// relation the statement reads taking the new name under an alias that
/// brings back the old name and the old column names.
#[test]
fn a_block_body_statement_shields_its_uncertain_references() {
    let db = parse(
        "CREATE TABLE q25t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q25t WHERE q25t IS NOT NULL;
             SELECT 1;
         END;
         ALTER TABLE q25t RENAME TO q25tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q25tt AS q25t WHERE q25t IS NOT NULL; SELECT 1; END",
        "the whole-row reference names the relation, so the rename shields it"
    );

    let db = parse(
        "CREATE TABLE q30t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q30t SET a = 0 WHERE public.q30t.a > 0;
             SELECT 1;
         END;
         ALTER TABLE q30t RENAME COLUMN a TO ab;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q30t SET ab = 0 WHERE q30t.ab > 0; SELECT 1; END",
        "the three-part reference binds under the clause and follows the rename"
    );

    // A bare reference two of the statement's own factors expose is refused
    // at ingestion, the way PostgreSQL's analysis refuses it.
    let refused = parse(
        "CREATE TABLE pa (a INT);
         CREATE TABLE pb (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM pa USING pb WHERE a > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the bare reference reaches two factors");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // A write target the statement respells is renamed in place, and a
    // qualified reference to it follows the rename under the new name.
    let db = parse(
        "CREATE TABLE ps1t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO ps1t (a) VALUES (1) RETURNING ps1t.a;
             SELECT 1;
         END;
         ALTER TABLE ps1t RENAME TO ps1tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO ps1tt (a) VALUES (1) RETURNING ps1tt.a; SELECT 1; END",
        "the target renames in place and the reference follows it"
    );

    // A factor the statement reaches under an alias takes the new name and
    // keeps that alias.
    let db = parse(
        "CREATE TABLE ps2t (a INT);
         CREATE VIEW ps2v AS SELECT * FROM ps2nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM ps2t AS x USING ps2v WHERE ps2v.nope > 0;
             SELECT 1;
         END;
         ALTER TABLE ps2t RENAME TO ps2tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM ps2tt AS x USING ps2v WHERE ps2v.nope > 0; SELECT 1; END",
        "the factor the statement reaches under an alias keeps the alias"
    );

    // A write target a column rename reaches is renamed in its own column
    // list and its reference, and gets no alias of its own.
    let db = parse(
        "CREATE TABLE ps3t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO ps3t (a) VALUES (1) RETURNING ps3t.a;
             SELECT 1;
         END;
         ALTER TABLE ps3t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO ps3t (aa) VALUES (1) RETURNING ps3t.aa AS a; SELECT 1; END",
        "the target's own column list and its reference follow the rename"
    );
}

/// A clause reference the statement's own clauses do not scope is refused at
/// `CREATE FUNCTION`, the way PostgreSQL refuses it, along with a factor
/// name or a reference naming another database and a reference with more
/// parts than a database, a schema and a column.
#[test]
fn a_block_body_refuses_its_unanalyzable_references() {
    // A three-part reference naming a relation the statement does not read.
    let refused = parse(
        "CREATE TABLE ps1t (a INT);
         CREATE TABLE ps1s (b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO ps1t (a) VALUES (1) RETURNING public.ps1s.b;
             SELECT 1;
         END;",
    )
    .expect_err("the reference names a relation the statement does not scope");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.ps1s.b");

    // A three-part reference in a `WHERE` clause naming an unscoped relation.
    let refused = parse(
        "CREATE TABLE ps3t (a INT);
         CREATE TABLE ps3s (b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE ps3t SET a = 0 WHERE public.ps3s.b > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the reference names a relation the statement does not scope");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.ps3s.b");

    // A factor name naming another database is refused, and so is a
    // reference naming one.
    let refused = parse(
        "CREATE TABLE ps3t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE other_db.public.ps3t SET a = 0;
             SELECT 1;
         END;",
    )
    .expect_err("the factor name names another database");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "other_db.public.ps3t");

    let refused = parse(
        "CREATE TABLE ps3t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE ps3t SET a = 0 WHERE other_db.public.ps3t.a > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the reference names another database");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "other_db.public.ps3t.a");

    // A reference with more parts than a database, a schema and a column.
    let refused = parse(
        "CREATE TABLE ps3t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE ps3t SET a = 0 WHERE postgres.public.ps3t.a.b > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the reference carries too many parts");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "postgres.public.ps3t.a.b");

    // A schema part that does not match the factor's own schema.
    let refused = parse(
        "CREATE TEMP TABLE q20t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q20t WHERE public.q20t.a > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the schema part does not match the factor's schema");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.q20t.a");

    // A column part the factor's columns do not hold.
    let refused = parse(
        "CREATE TABLE q25t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q25t SET a = 0 WHERE public.q25t.nope > 0;
             SELECT 1;
         END;",
    )
    .expect_err("the factor's columns do not hold the column part");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.q25t.nope");
}

/// A clause reference naming a relation the statement's own clauses scope
/// under a matching schema is dropped to the parts it keeps at `CREATE
/// FUNCTION`, the way PostgreSQL stores it, a four-part one losing its
/// database part and a temporary relation its temporary schema.
#[test]
fn a_block_body_stores_its_analyzed_references_bare() {
    // A database-qualified reference and a database-qualified factor name
    // naming this database drop to the parts they keep.
    let db = parse(
        "CREATE TABLE q31t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE unknown_catalog.public.q31t SET a = 0 WHERE unknown_catalog.public.q31t.a > 0;
             SELECT 1;
         END;",
    )
    .expect("the database qualifier is dropped");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q31t SET a = 0 WHERE q31t.a > 0; SELECT 1; END",
        "the factor name and the reference are stored without their extra parts"
    );

    // A factor in a schema the path reaches under a qualifier keeps it, and
    // a rename moves the qualifier with the name.
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.q32t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE app.q32t SET a = 0;
             SELECT 1;
         END;
         ALTER TABLE app.q32t RENAME TO q32u;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE app.q32u SET a = 0; SELECT 1; END",
        "the qualified factor follows the rename under its schema"
    );

    // A temporary relation's temporary schema is dropped from a reference to
    // it, the way PostgreSQL stores it.
    let db = parse(
        "CREATE TEMP TABLE q21t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q21t WHERE pg_temp.q21t.a > 0;
             SELECT 1;
         END;",
    )
    .expect("the temporary schema is dropped");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q21t WHERE q21t.a > 0; SELECT 1; END",
        "the reference to the temporary relation is stored bare"
    );

    // An opaque view answers no column, so a reference to one of its
    // presumed outputs is stored as written and shields when a rename
    // reaches the statement.
    let db = parse(
        "CREATE TABLE q22t (a INT);
         CREATE VIEW q22v AS SELECT * FROM q22nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q22t USING q22v WHERE q22v.nope > 0;
             SELECT 1;
         END;
         ALTER TABLE q22t RENAME TO q22u;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q22u AS q22t USING q22v WHERE q22v.nope > 0; SELECT 1; END",
        "the unnameable view's reference shields the rename"
    );

    // A write under `WITH` follows the rename in its own clauses, the
    // `WITH` item's outputs answering their own name.
    let db = parse(
        "CREATE TABLE q23t (a INT);
         CREATE TABLE q23u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT q23u.a FROM q23u) DELETE FROM q23t USING x WHERE q23t.a > 0;
             SELECT 1;
         END;
         ALTER TABLE q23t RENAME TO q23w;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT q23u.a FROM q23u) DELETE FROM q23w USING x WHERE q23w.a > 0; SELECT 1; END",
        "the write under the WITH clause follows the rename in its own clauses"
    );

    // A table function is not a relation, so it answers no column and no
    // rename while the statement's table still follows its own.
    let db = parse(
        "CREATE TABLE q24t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q24t USING generate_series(1, 3) g WHERE q24t.a > 0;
             SELECT 1;
         END;
         ALTER TABLE q24t RENAME TO q24w;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q24w USING generate_series(1, 3) g WHERE q24w.a > 0; SELECT 1; END",
        "the table follows its own clause while the function answers nothing"
    );
}

/// A query part of a block statement is respelled on its own, and a part not
/// certain enough to respell by is shielded the way a view's definition is.
#[test]
fn a_block_body_query_part_follows_renames_on_its_own() {
    let db = parse(
        "CREATE TABLE q2t (a INT, c INT);
         CREATE TABLE q2s (d INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             SELECT a FROM (q2t JOIN q2s ON q2t.c = q2s.d);
         END;
         ALTER TABLE q2t RENAME TO q2u;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT a FROM (q2u AS q2t JOIN q2s ON q2t.c = q2s.d); END",
        "the parenthesized join is not certain enough to respell, so it is shielded"
    );

    let db = parse(
        "CREATE TABLE q3t (a INT);
        CREATE TABLE q3x (b INT);
        CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
            RETURN (SELECT a FROM q3t WHERE EXISTS (SELECT 1 FROM q3x WHERE q3x.b = a));
        END;
        ALTER TABLE q3t RENAME COLUMN a TO b;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC RETURN (SELECT b AS a FROM q3t WHERE EXISTS (SELECT 1 FROM q3x WHERE q3x.b = q3t.b)); END",
        "the outer reference the rename makes ambiguous is qualified"
    );
}

/// A view or a materialized view a statement reads is a clause factor of its
/// own, reached by its outputs.
#[test]
fn a_block_body_statement_reads_views_as_clause_factors() {
    let db = parse(
        "CREATE TABLE q17t (id INT, a INT);
         CREATE TABLE q17t2 (id INT, a INT);
         CREATE VIEW q17v AS SELECT id, a FROM q17t2;
         CREATE VIEW q17w AS SELECT id FROM q17t2;
         CREATE MATERIALIZED VIEW q17m AS SELECT id FROM q17t2;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q17t USING q17v, q17w, q17m WHERE q17t.id = q17v.id;
             SELECT 1;
         END;
         ALTER TABLE q17t RENAME TO q17tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q17tt USING q17v, q17w, q17m WHERE q17tt.id = q17v.id; SELECT 1; END",
        "the target follows the rename and the view factors keep reading their definitions"
    );

    let db = parse(
        "CREATE TABLE q18t (id INT, a INT);
         CREATE TABLE q18t2 (id INT, x INT);
         CREATE VIEW q18v (v_id, v_x) AS SELECT id, x FROM q18t2;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q18t USING q18v WHERE q18t.id = q18v.v_id;
             SELECT 1;
         END;
         ALTER TABLE q18t RENAME TO q18tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q18tt USING q18v WHERE q18tt.id = q18v.v_id; SELECT 1; END",
        "the declared output names are what the reference reaches"
    );
}

/// A rename that a reference in a `RETURN` body's subquery would capture
/// qualifies that reference, as it does in a view.
#[test]
fn a_return_body_qualifies_a_reference_a_rename_would_capture() {
    let db = parse(
        "CREATE TABLE q30t (a INT);
         CREATE TABLE q30x (b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql
             RETURN (SELECT a FROM q30t WHERE EXISTS (SELECT 1 FROM q30x WHERE q30x.b = a));
         ALTER TABLE q30t RENAME COLUMN a TO b;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql RETURN (SELECT b AS a FROM q30t WHERE EXISTS (SELECT 1 FROM q30x WHERE q30x.b = q30t.b))"
    );
}

/// A quoted relation keeps its quoting when a block statement follows its
/// rename.
#[test]
fn a_block_body_follows_the_rename_of_a_quoted_relation() {
    let db = parse(
        "CREATE TABLE \"Q31t\" (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE \"Q31t\" SET a = 0 WHERE a > 0;
             SELECT 1;
         END;
         ALTER TABLE \"Q31t\" RENAME TO \"Q31u\";",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE \"Q31u\" SET a = 0 WHERE a > 0; SELECT 1; END"
    );
}

/// When an opaque view leaves a statement's references uncertain, a column
/// rename of another factor restores the old name through an alias column
/// list.
#[test]
fn a_shielded_statement_restores_a_renamed_column_through_its_alias() {
    let db = parse(
        "CREATE TABLE q32t (a INT);
         CREATE TABLE q32w (a INT);
         CREATE VIEW q32v AS SELECT * FROM q32nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q32t USING q32w, q32v WHERE q32v.nope > q32w.a;
             SELECT 1;
         END;
         ALTER TABLE q32w RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q32t USING q32w AS q32w (a), q32v WHERE q32v.nope > q32w.a; SELECT 1; END"
    );
}

/// A write target's column list names a composite column's field after the
/// column, and the column part follows a rename of the column.
#[test]
fn a_block_body_follows_a_renamed_composite_column_in_a_write_list() {
    let db = parse(
        "CREATE TYPE q33p AS (x INT, y INT);
         CREATE TABLE q33t (c q33p, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE q33t SET c.x = 1, a = 2;
             INSERT INTO q33t (c.y, a) VALUES (1, 2);
             SELECT 1;
         END;
         ALTER TABLE q33t RENAME COLUMN c TO d;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE q33t SET d.x = 1, a = 2; INSERT INTO q33t (d.y, a) VALUES (1, 2); SELECT 1; END"
    );
}

/// A shielded statement whose factor already names the renamed column
/// through its alias column list keeps that alias as written.
#[test]
fn a_shielded_statement_keeps_an_alias_that_already_names_the_column() {
    let db = parse(
        "CREATE TABLE q34t (a INT);
         CREATE TABLE q34w (a INT);
         CREATE VIEW q34v AS SELECT * FROM q34nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q34t USING q34w AS w (z), q34v WHERE q34v.nope > w.z;
             SELECT 1;
         END;
         ALTER TABLE q34w RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q34t USING q34w AS w (z), q34v WHERE q34v.nope > w.z; SELECT 1; END"
    );
}

/// A column rename reaching the write target of a statement an opaque view
/// leaves unbound is refused, since PostgreSQL accepts no alias column list
/// that would bring the old name back on an `UPDATE`, `DELETE` or `MERGE`
/// target.
#[test]
fn a_column_rename_of_a_shielded_write_target_is_refused() {
    for (statement, relation) in [
        ("UPDATE q35t SET a = 1 FROM q35v WHERE q35v.nope > 0", "q35t"),
        ("DELETE FROM q35t USING q35v WHERE q35v.nope > a", "q35t"),
        ("MERGE INTO q35t USING q35v ON q35v.nope = a WHEN MATCHED THEN DELETE", "q35t"),
        ("UPDATE q35w SET a = 1 FROM q35v WHERE q35v.nope > 0", "q35w"),
    ] {
        let sql = format!(
            "CREATE TABLE q35t (a INT);
             CREATE VIEW q35w AS SELECT a FROM q35t;
             CREATE VIEW q35v AS SELECT * FROM q35nope;
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
                 {statement};
                 SELECT 1;
             END;
             ALTER TABLE {relation} RENAME COLUMN a TO b;"
        );
        assert!(
            matches!(
                parse(&sql),
                Err(Error::FunctionBodyBlocksColumnRename {
                    ref column_name,
                    ref relation_name,
                    ref function_name,
                }) if column_name == "a" && relation_name == relation && function_name == "f"
            ),
            "{statement} refuses the rename"
        );
    }
}

/// A block holding any statement other than a query, `RETURN` or one of the
/// data-modifying statements is refused at ingestion, the way PostgreSQL
/// refuses DDL and `SET` in an unquoted SQL function body, and a
/// data-modifying statement under its `WITH` clause is held as written.
#[test]
fn a_block_body_refuses_its_unsupported_statement_kinds() {
    // A data definition PostgreSQL does not support in an unquoted body.
    let refused = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             CREATE TABLE q (a INT);
             SELECT 1;
         END;",
    )
    .expect_err("a data definition is not supported in an unquoted SQL function body");
    assert!(
        matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
            if function_name == "f"),
        "got {refused:?}"
    );

    // A `SET` PostgreSQL does not support in an unquoted body.
    let refused = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             SET x TO y;
             SELECT 1;
         END;",
    )
    .expect_err("a SET is not supported in an unquoted SQL function body");
    assert!(
        matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
            if function_name == "f"),
        "got {refused:?}"
    );

    // A data-modifying statement under a `WITH` clause is held.
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) UPDATE m1 SET b = 1;
         END;",
    )
    .expect("a data-modifying statement under a WITH clause binds");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) UPDATE m1 SET b = 1; END",
        "the statement under the WITH clause is stored as written"
    );
}

/// A function that declares a result is refused when its block is empty or
/// ends in a data-modifying statement without `RETURNING`, the way
/// PostgreSQL's return type mismatch refuses it, under a `WITH` clause as
/// well, and a `RETURNING` list produces a result.
#[test]
fn a_block_body_refuses_a_result_its_final_statement_produces_none() {
    // An empty block of a function that declares a result.
    refuses_missing_result("CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC END;");

    // A final data-modifying statement without a `RETURNING` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;",
    );

    // The same statement under a `WITH` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) UPDATE m1 SET b = 1;
         END;",
    );

    // An `INSERT` under a `WITH` clause without a `RETURNING` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) INSERT INTO m1 (a) VALUES (1);
         END;",
    );

    // A `DELETE` under a `WITH` clause without a `RETURNING` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM m1 WHERE a > 0;
         END;",
    );

    // A `MERGE` under a `WITH` clause without a `RETURNING` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1;
         END;",
    );

    // A `MERGE` without a `RETURNING` clause.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1;
         END;",
    );

    // An `OUT` argument declares a result, and so does an `INOUT` one.
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f(OUT o INT) LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;",
    );
    refuses_missing_result(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f(INOUT x INT) LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;",
    );
}

/// `RETURNS SETOF` and `RETURNS TABLE` declare a result the final
/// statement must produce, and a `RETURNING` list produces it.
#[test]
fn a_set_or_table_declaration_declares_a_result_a_returning_list_produces() {
    // `RETURNS SETOF` and `RETURNS TABLE` declare a result.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS SETOF INT LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;",
    )
    .expect_err("the SETOF declaration produces no result from the final statement");
    assert!(
        matches!(&refused, Error::FunctionBodyResultMissing { function_name }
            if function_name == "f"),
        "got {refused:?}"
    );
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS TABLE (a INT) LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;",
    )
    .expect_err("the TABLE declaration produces no result from the final statement");
    assert!(
        matches!(&refused, Error::FunctionBodyResultMissing { function_name }
            if function_name == "f"),
        "got {refused:?}"
    );

    // A `RETURNING` list produces a result.
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1 RETURNING a;
         END;",
    )
    .expect("the RETURNING list produces a result");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE m1 SET b = 1 RETURNING a; END",
        "the RETURNING clause is stored as written"
    );
}

/// `RETURNS VOID` and a function without a result declaration accept a block
/// whose final statement is a data-modifying statement without `RETURNING`,
/// and an empty block.
#[test]
fn a_void_block_body_accepts_its_resultless_statements() {
    // An empty block of a `void` function.
    let db = parse("CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC END;")
        .expect("void declares no result");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC END",
        "the empty block is stored"
    );

    // A final data-modifying statement without a `RETURNING` clause, written
    // directly and under a `WITH` clause.
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) UPDATE m1 SET b = 1;
         END;",
    )
    .expect("the final statement writes rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) UPDATE m1 SET b = 1; END",
        "the statement is stored as written"
    );
}

/// A join on the target of an `UPDATE` or a `DELETE` is refused at
/// ingestion, the way PostgreSQL's parser refuses it, under a `WITH` clause
/// as well.
#[test]
fn a_block_body_refuses_a_join_on_its_write_target() {
    for statement in [
        "DELETE FROM m1 JOIN m2 ON m1.a = m2.a WHERE m1.a > 0",
        "UPDATE m1 JOIN m2 ON m1.a = m2.a SET b = 1",
        "WITH x AS (SELECT 1) DELETE FROM m1 JOIN m2 ON m1.a = m2.a WHERE m1.a > 0",
        "WITH x AS (SELECT 1) UPDATE m1 JOIN m2 ON m1.a = m2.a SET b = 1",
    ] {
        let sql = format!(
            "CREATE TABLE m1 (a INT, b INT);
             CREATE TABLE m2 (a INT);
             CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
                 {statement};
             END;"
        );
        let refused = parse(&sql).expect_err("a join on the write target is a syntax error");
        assert!(
            matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
                if function_name == "f"),
            "got {refused:?}"
        );
    }
}

/// The wildcard a `MERGE UPDATE SET` names is refused at ingestion, the way
/// PostgreSQL's parser refuses it, under a `WITH` clause as well.
#[test]
fn a_block_body_refuses_a_wildcard_merge_update_set() {
    for statement in [
        "MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET *",
        "WITH x AS (SELECT 1) MERGE INTO m1 USING m2 ON m1.a = m2.a \
         WHEN MATCHED THEN UPDATE SET *",
    ] {
        let sql = format!(
            "CREATE TABLE m1 (a INT, b INT);
             CREATE TABLE m2 (a INT);
             CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
                 {statement};
             END;"
        );
        let refused = parse(&sql).expect_err("a wildcard merge update is a syntax error");
        assert!(
            matches!(&refused, Error::FunctionBodyStatementUnsupported { function_name, .. }
                if function_name == "f"),
            "got {refused:?}"
        );
    }
}

/// An `INSERT` without a column list writes the first columns of the target
/// it writes, the way PostgreSQL stores it, under a `WITH` clause as well.
#[test]
fn a_block_body_insert_without_a_column_list_writes_the_target_columns() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO m1 VALUES (1, 2);
         END;",
    )
    .expect("the insert writes its values");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO m1 (a, b) VALUES (1, 2); END",
        "the values row names the target's first two columns"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO m1 VALUES (1);
         END;",
    )
    .expect("the insert writes its value");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO m1 (a) VALUES (1); END",
        "the values row names the target's first column"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m3 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO m1 SELECT a, b FROM m3;
         END;",
    )
    .expect("the select row writes its columns");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO m1 (a, b) SELECT a, b FROM m3; END",
        "the select row names the target's first two columns"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) INSERT INTO m1 VALUES (1);
         END;",
    )
    .expect("the insert under a WITH clause writes its value");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) INSERT INTO m1 (a) VALUES (1); END",
        "the insert under the WITH clause names the target's first column"
    );
}

/// A three-part reference a factor's alias would have to reach is refused at
/// ingestion, the way PostgreSQL refuses the invalid `FROM`-clause entry, in
/// a `DELETE` and in an `UPDATE`.
#[test]
fn a_block_body_refuses_a_three_part_reference_through_its_factor_alias() {
    // A reference a `DELETE` target's alias would have to reach.
    let refused = parse(
        "CREATE TABLE q26t (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q26t AS x WHERE public.x.a > 0;
         END;",
    )
    .expect_err("the reference reaches the factor through its alias");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.x.a");

    // A reference an `UPDATE` target's alias would have to reach.
    let refused = parse(
        "CREATE TABLE q27t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE q27t AS x SET b = 1 WHERE public.x.a > 0;
         END;",
    )
    .expect_err("the reference reaches the factor through its alias");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.x.a");
}

/// A bare reference a subquery's own factors do not answer is resolved
/// against the statement's factors, which refuse it when two expose it.
#[test]
fn a_block_body_refuses_a_bare_reference_its_outer_factors_expose_twice() {
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE TABLE m3 (c INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1 FROM m2 WHERE EXISTS (SELECT 1 FROM m3 WHERE c = a);
         END;",
    )
    .expect_err("the outer factors expose the reference twice");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );
}

/// A bare column reference more than one of a statement's own factors
/// exposes is refused at ingestion, the way PostgreSQL's analysis refuses
/// it, in the statement's clauses, in its `RETURNING` list and in a `SELECT`
/// scope that names two such factors, while a `WITH` item the scope does not
/// name exposes nothing to it.
#[test]
fn a_block_body_refuses_a_bare_reference_its_own_factors_expose_twice() {
    // The factors of an `UPDATE`'s own clauses.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1 FROM m2 WHERE a > 0;
         END;",
    )
    .expect_err("two factors expose the reference");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // The factors an `INSERT` source `SELECT` names.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE TABLE m3 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO m1 (a) SELECT a FROM m2 JOIN m3 ON m2.a = m3.a WHERE a > 0;
         END;",
    )
    .expect_err("two factors expose the reference");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // A `MERGE` clause value.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = a;
         END;",
    )
    .expect_err("two factors expose the reference");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // A `RETURNING` list.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM m1 USING m2 WHERE m1.a = m2.a RETURNING a;
         END;",
    )
    .expect_err("two factors expose the reference");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // A `SELECT` scope that names a `WITH` item and a table that both
    // expose the reference.
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (SELECT a FROM m2) SELECT a FROM m1, d;
         END;",
    )
    .expect_err("two factors expose the reference");
    assert!(
        matches!(&refused, Error::FunctionBodyReferenceAmbiguous { column_name, function_name }
            if column_name == "a" && function_name == "f"),
        "got {refused:?}"
    );

    // A `SELECT` scope that names only one factor exposing it.
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (SELECT a FROM m2) SELECT a FROM m1;
         END;",
    )
    .expect("one factor exposes the reference");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (SELECT a FROM m2) SELECT a FROM m1; END",
        "the reference reaches the one factor that exposes it"
    );

    // A `WITH` item a data-modifying statement's own clauses do not name
    // exposes nothing to them.
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH d AS (SELECT a FROM m2) DELETE FROM m1 WHERE a > 0;
         END;",
    )
    .expect("the item's columns do not reach the statement's clauses");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH d AS (SELECT a FROM m2) DELETE FROM m1 WHERE a > 0; END",
        "the reference reaches the statement's own factor"
    );
}

/// A `RETURNING` wildcard writes the qualified columns it reaches, the way
/// PostgreSQL stores it, the target's for an `INSERT`, the target's and its
/// clause factors' for an `UPDATE` or a `DELETE`, and the source's and the
/// target's for a `MERGE`, under a `WITH` clause as well.
#[test]
fn a_block_body_statement_spells_its_returning_wildcards() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM m1 USING m2 WHERE m1.a = m2.a RETURNING *;
             DELETE FROM m1 AS x USING m2 WHERE x.a = m2.a RETURNING *;
             DELETE FROM m1 USING m2 WHERE m1.a = m2.a RETURNING m2.*;
         END;",
    )
    .expect("the deletes write their rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         DELETE FROM m1 USING m2 WHERE m1.a = m2.a RETURNING m1.a, m1.b, m2.a; \
         DELETE FROM m1 AS x USING m2 WHERE x.a = m2.a RETURNING x.a, x.b, m2.a; \
         DELETE FROM m1 USING m2 WHERE m1.a = m2.a RETURNING m2.a; END",
        "each wildcard names the qualified columns it reaches"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1 FROM m2 WHERE m1.a = m2.a RETURNING *;
         END;",
    )
    .expect("the update writes its rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE m1 SET b = 1 FROM m2 WHERE m1.a = m2.a RETURNING m1.a, m1.b, m2.a; END",
        "the wildcard names the target's columns and the from factor's"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m3 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO m1 (a) VALUES (1) RETURNING *;
             INSERT INTO m1 SELECT a, b FROM m3 RETURNING *;
         END;",
    )
    .expect("the inserts write their rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         INSERT INTO m1 (a) VALUES (1) RETURNING m1.a, m1.b; \
         INSERT INTO m1 (a, b) SELECT a, b FROM m3 RETURNING m1.a, m1.b; END",
        "each wildcard names the target's columns"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING *;
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.*;
         END;",
    )
    .expect("the merges write their rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC \
         MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m2.a, m1.a, m1.b; \
         MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.a, m1.b; END",
        "each wildcard names the qualified columns it reaches, the source first"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM m1 RETURNING *;
         END;",
    )
    .expect("the delete under a WITH clause writes its rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) DELETE FROM m1 RETURNING m1.a, m1.b; END",
        "the wildcard under the WITH clause names the target's columns"
    );
}

/// A `SELECT` wildcard in a block query writes the columns it reaches, the
/// way `spell_out` does for a view, in the query and in the subqueries and
/// `WITH` items it holds.
#[test]
fn a_block_body_statement_spells_its_select_wildcards() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             SELECT * FROM m1;
         END;",
    )
    .expect("the select writes its rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC SELECT a, b FROM m1; END",
        "the wildcard names the relation's columns"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             SELECT count(*) FROM (SELECT * FROM m1) s;
         END;",
    )
    .expect("the select writes its rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM (SELECT a, b FROM m1) s; END",
        "the wildcard in the subquery names the relation's columns"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT * FROM m1) DELETE FROM m2 WHERE (SELECT max(a) FROM x) > 0;
         END;",
    )
    .expect("the select under a WITH item writes its rows");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT a, b FROM m1) DELETE FROM m2 WHERE (SELECT max(a) FROM x) > 0; END",
        "the wildcard in the WITH item names the relation's columns"
    );
}

/// A temporary relation a block statement names is stored by its last part,
/// the way PostgreSQL stores it, and the relation the body writes cannot be
/// dropped from under the function.
#[test]
fn a_block_body_stores_a_temporary_relation_bare() {
    let db = parse(
        "CREATE TEMP TABLE f19t (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM pg_temp.f19t WHERE a > 0;
         END;",
    )
    .expect("the temporary relation binds");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC DELETE FROM f19t WHERE a > 0; END",
        "the temporary schema is dropped from the stored name"
    );

    let refused = parse(
        "CREATE TEMP TABLE f19u (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM pg_temp.f19u WHERE a > 0;
         END;
         DROP TABLE f19u;",
    )
    .expect_err("f writes f19u");
    assert!(
        matches!(&refused, Error::RelationHasDependents {
            object_kind,
            object_name,
            dependent_kind,
            dependent_name
        } if *object_kind == ObjectKind::Table
            && object_name == "f19u"
            && *dependent_kind == ObjectKind::Function
            && dependent_name == "f"),
        "got {refused:?}"
    );
}

/// An `INSERT` whose row is wider than its target is refused at ingestion,
/// the way PostgreSQL refuses it, with or without a column list, under a
/// `WITH` clause as well.
#[test]
fn a_block_body_refuses_an_insert_wider_than_its_target() {
    for statement in [
        "INSERT INTO p1t VALUES (1, 2, 3)",
        "INSERT INTO p1t (a, b) VALUES (1, 2, 3)",
        "WITH x AS (SELECT 1) INSERT INTO p1t VALUES (1, 2, 3)",
    ] {
        let sql = format!(
            "CREATE TABLE p1t (a INT, b INT);
             CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
                 {statement};
             END;"
        );
        let refused = parse(&sql).expect_err("the row is wider than the target");
        assert!(
            matches!(&refused, Error::FunctionBodyInsertTooWide { function_name }
                if function_name == "f"),
            "got {refused:?}"
        );
    }
}

/// A conflict clause's column list, its `DO UPDATE` targets and its
/// `EXCLUDED` reference follow a column rename, and a relation rename moves
/// the target while the alias and the `EXCLUDED` reference stay as written.
#[test]
fn a_block_body_insert_conflict_follows_renames() {
    let db = parse(
        "CREATE TABLE p2t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO p2t AS t (a, b) VALUES (1, 2)
             ON CONFLICT (a) DO UPDATE SET b = t.b + excluded.b;
         END;
         ALTER TABLE p2t RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO p2t AS t (a, bb) VALUES (1, 2) ON CONFLICT(a) DO UPDATE SET bb = t.bb + excluded.bb; END",
        "the conflict list, the update targets and the EXCLUDED reference follow the rename"
    );

    let db = parse(
        "CREATE TABLE p3t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO p3t (a, b) VALUES (1, 2)
             ON CONFLICT (a) WHERE b > 0 DO UPDATE SET b = 1 WHERE p3t.b > 0;
         END;
         ALTER TABLE p3t RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO p3t (a, bb) VALUES (1, 2) ON CONFLICT(a) WHERE bb > 0 DO UPDATE SET bb = 1 WHERE p3t.bb > 0; END",
        "the conflict predicate and the update's clauses follow the rename"
    );

    let db = parse(
        "CREATE TABLE p4t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO p4t AS t (a, b) VALUES (1, 2)
             ON CONFLICT (a) DO UPDATE SET b = t.b + excluded.b;
         END;
         ALTER TABLE p4t RENAME TO p4tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO p4tt AS t (a, b) VALUES (1, 2) ON CONFLICT(a) DO UPDATE SET b = t.b + excluded.b; END",
        "the target follows the rename and the alias and the EXCLUDED reference stay"
    );
}

/// A `RETURNING` list's column references keep their output names when a
/// column rename reaches them, `OLD` and `NEW` references resolving to the
/// target's columns, and an alias the list already carries is kept.
#[test]
fn a_block_body_returning_keeps_its_output_names() {
    let db = parse(
        "CREATE TABLE p5t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE p5t SET a = 1 RETURNING old.a;
         END;
         ALTER TABLE p5t RENAME COLUMN a TO c;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE p5t SET c = 1 RETURNING old.c AS a; END",
        "the OLD reference follows the rename and keeps its output name"
    );

    let db = parse(
        "CREATE TABLE p6t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE p6t SET b = 1 RETURNING a;
         END;
        ALTER TABLE p6t RENAME COLUMN a TO c;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE p6t SET b = 1 RETURNING p6t.c AS a; END",
        "the bare reference keeps its output name under the rename"
    );

    let db = parse(
        "CREATE TABLE p7t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE p7t SET b = 1 RETURNING new.a;
         END;
         ALTER TABLE p7t RENAME COLUMN a TO c;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE p7t SET b = 1 RETURNING new.c AS a; END",
        "the NEW reference follows the rename and keeps its output name"
    );

    let db = parse(
        "CREATE TABLE p8t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE p8t SET b = 1 RETURNING p8t.a;
         END;
         ALTER TABLE p8t RENAME COLUMN a TO c;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE p8t SET b = 1 RETURNING p8t.c AS a; END",
        "the qualified reference follows the rename and keeps its output name"
    );

    let db = parse(
        "CREATE TABLE p9t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE p9t SET b = 1 RETURNING a AS x;
         END;
         ALTER TABLE p9t RENAME COLUMN a TO c;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE p9t SET b = 1 RETURNING p9t.c AS x; END",
        "the alias the list carries is kept"
    );
}

/// A whole-row reference a column rename captures becomes a qualified
/// wildcard when it names the write target outside a `RETURNING` list, is
/// refused when the list holds it, and shields the factor it names when it
/// names another factor.
#[test]
fn a_block_body_whole_row_reference_a_column_rename_captures() {
    let db = parse(
        "CREATE TABLE p10t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM p10t WHERE p10t IS NOT NULL RETURNING p10t.a;
         END;
         ALTER TABLE p10t RENAME COLUMN a TO p10t;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM p10t WHERE p10t.* IS NOT NULL RETURNING p10t.p10t AS a; END",
        "the target's whole-row reference the rename captures becomes a wildcard"
    );

    let refused = parse(
        "CREATE TABLE p11t (a INT, b INT);
         CREATE FUNCTION f() RETURNS p11t LANGUAGE sql BEGIN ATOMIC
             UPDATE p11t SET b = 1 RETURNING p11t;
         END;
         ALTER TABLE p11t RENAME COLUMN a TO p11t;",
    )
    .expect_err("the rename captures the RETURNING list's whole-row reference");
    assert!(
        matches!(
            &refused,
            Error::FunctionBodyBlocksColumnRename {
                column_name,
                relation_name,
                function_name,
            } if column_name == "a" && relation_name == "p11t" && function_name == "f"
        ),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE p12t (a INT, b INT);
         CREATE FUNCTION f() RETURNS p12t LANGUAGE sql BEGIN ATOMIC
             UPDATE p12t SET a = 1 WHERE p12t IS NOT NULL RETURNING p12t;
         END;
         ALTER TABLE p12t RENAME TO p12tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS p12t LANGUAGE sql BEGIN ATOMIC UPDATE p12tt AS p12t SET a = 1 WHERE p12t IS NOT NULL RETURNING p12t; END",
        "the whole-row references shield the relation rename"
    );

    let db = parse(
        "CREATE TABLE p13t (a INT);
         CREATE FUNCTION f() RETURNS p13t LANGUAGE sql BEGIN ATOMIC
             INSERT INTO p13t (a) VALUES (1) RETURNING p13t;
         END;
         ALTER TABLE p13t RENAME TO p13tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS p13t LANGUAGE sql BEGIN ATOMIC INSERT INTO p13tt AS p13t (a) VALUES (1) RETURNING p13t; END",
        "the insert target the whole-row reference names shields the rename"
    );

    let db = parse(
        "CREATE TABLE p14t (a INT);
         CREATE TABLE p14u (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM p14t AS x USING p14u WHERE x IS NOT NULL;
         END;
         ALTER TABLE p14t RENAME COLUMN a TO x;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC DELETE FROM p14t AS x USING p14u WHERE x.* IS NOT NULL; END",
        "the alias-named whole-row reference the rename captures becomes a wildcard"
    );

    let db = parse(
        "CREATE TABLE p15t (a INT);
         CREATE TABLE p15u (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM p15t USING p15u AS x WHERE x IS NOT NULL;
         END;
         ALTER TABLE p15u RENAME COLUMN a TO x;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC DELETE FROM p15t USING p15u AS x (a) WHERE x IS NOT NULL; END",
        "the other factor the rename would capture is shielded"
    );

    let db = parse(
        "CREATE TABLE p16t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM p16t WHERE p16t IS NOT NULL;
         END;
         ALTER TABLE p16t RENAME COLUMN a TO b2;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC DELETE FROM p16t WHERE p16t IS NOT NULL; END",
        "a rename that captures no whole-row reference leaves it as written"
    );
}

/// A bare reference naming a function parameter takes the function's name
/// when a column rename makes a column of that name reachable, and stays
/// bare while no rename does.
#[test]
fn a_block_body_qualifies_a_parameter_a_rename_captures() {
    let db = parse(
        "CREATE TABLE p17t (a INT, c INT);
         CREATE FUNCTION f2(x INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE p17t SET a = x;
         END;
         ALTER TABLE p17t RENAME COLUMN c TO x;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f2"),
        "CREATE FUNCTION f2(x INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE p17t SET a = f2.x; END",
        "the parameter the rename captures is qualified by the function's name"
    );

    let db = parse(
        "CREATE TABLE p18t (a INT, c INT);
         CREATE FUNCTION f2(y INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE p18t SET a = y;
         END;
         ALTER TABLE p18t RENAME COLUMN c TO z;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f2"),
        "CREATE FUNCTION f2(y INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE p18t SET a = y; END",
        "the parameter no rename reaches stays bare"
    );
}

/// A bare reference the renames make ambiguous is qualified by the name its
/// expected factor is reached by, in a write's own clauses.
#[test]
fn a_block_body_qualifies_a_reference_its_renames_make_ambiguous() {
    let db = parse(
        "CREATE TABLE p19t (c INT, a INT);
         CREATE TABLE p19u (b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE p19t SET a = b FROM p19u WHERE c > 0;
         END;
         ALTER TABLE p19t RENAME COLUMN c TO b;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE p19t SET a = p19u.b FROM p19u WHERE p19t.b > 0; END",
        "the references the rename makes ambiguous take the name their factor is reached by"
    );
}

/// A subquery a write's clause holds follows its renames on its own,
/// shielding the whole-row reference the renames capture.
#[test]
fn a_block_body_subquery_a_write_clause_holds_shields_its_captures() {
    let db = parse(
        "CREATE TABLE p20t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE p20t SET a = (SELECT max(a) FROM p20t WHERE p20t IS NOT NULL);
         END;
         ALTER TABLE p20t RENAME TO p20tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE p20tt SET a = (SELECT max(a) FROM p20tt AS p20t WHERE p20t IS NOT NULL); END",
        "the subquery shields the rename and the target follows it"
    );
}

/// A data-modifying statement under a `WITH` clause follows its target's
/// renames and its clause factors' the way a top-level one does, a `WITH`
/// item's outputs answer their own name, which the item's own reads move in
/// the item, and a factor's alias is kept.
#[test]
fn a_block_body_write_under_a_with_follows_its_renames() {
    let db = parse(
        "CREATE TABLE w1t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) INSERT INTO w1t (a) VALUES (1);
             SELECT 1;
         END;
         ALTER TABLE w1t RENAME TO w1tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) INSERT INTO w1tt (a) VALUES (1); SELECT 1; END",
        "the insert target under the WITH clause follows the rename"
    );

    let db = parse(
        "CREATE TABLE w2t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) UPDATE w2t AS y SET b = 1;
             SELECT 1;
         END;
         ALTER TABLE w2t RENAME TO w2tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) UPDATE w2tt AS y SET b = 1; SELECT 1; END",
        "the aliased write target follows the rename under its alias"
    );

    let db = parse(
        "CREATE TABLE w8t (id INT, a INT);
         CREATE TABLE w8u (id INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM w8t USING w8u WHERE w8t.id = w8u.id;
             SELECT 1;
         END;
         ALTER TABLE w8u RENAME TO w8uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) DELETE FROM w8t USING w8uu WHERE w8t.id = w8uu.id; SELECT 1; END",
        "the write's clause factor follows the rename under the WITH clause"
    );

    let db = parse(
        "CREATE TABLE w3t (a INT);
         CREATE TABLE w3u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT a FROM w3u) DELETE FROM w3t USING x WHERE w3t.a = x.a RETURNING w3t.a;
             SELECT 1;
         END;
         ALTER TABLE w3t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT a FROM w3u) DELETE FROM w3t USING x WHERE w3t.aa = x.a RETURNING w3t.aa AS a; SELECT 1; END",
        "the write's references follow the rename and the item's outputs answer their own name"
    );

    let db = parse(
        "CREATE TABLE w4t (a INT);
         CREATE TABLE w4u (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT a FROM w4u) DELETE FROM w4t USING x WHERE w4t.a = x.a;
             SELECT 1;
         END;
         ALTER TABLE w4u RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT aa AS a FROM w4u) DELETE FROM w4t USING x WHERE w4t.a = x.a; SELECT 1; END",
        "the item follows its own read and its outputs keep their name"
    );
}

/// A data-modifying `WITH` item follows its own renames through its own
/// clauses, and a query or a data-modifying statement reading the item keeps
/// reading its outputs.
#[test]
fn a_block_body_write_in_a_with_item_follows_its_renames() {
    let db = parse(
        "CREATE TABLE w5t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM w5t WHERE a > 0 RETURNING a) SELECT a FROM d;
         END;
         ALTER TABLE w5t RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (DELETE FROM w5t WHERE w5t.aa > 0 RETURNING w5t.aa AS a) SELECT a FROM d; END",
        "the item's write follows its rename and the query keeps reading the item's outputs"
    );

    let db = parse(
        "CREATE TABLE w6t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) UPDATE w6t SET b = 1;
             SELECT 1;
         END;
         ALTER TABLE w6t RENAME TO w6tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) UPDATE w6tt SET b = 1; SELECT 1; END",
        "the write under the WITH clause follows its relation rename bare"
    );

    let db = parse(
        "CREATE TABLE w7t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM w7t WHERE a > 0 RETURNING a) SELECT a FROM d;
         END;
         ALTER TABLE w7t RENAME TO w7tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (DELETE FROM w7tt WHERE a > 0 RETURNING a) SELECT a FROM d; END",
        "the item's write follows its relation rename and the query keeps reading its outputs"
    );

    // A data-modifying statement that reads the item keeps reading the
    // item's outputs.
    let db = parse(
        "CREATE TABLE w9b (a INT);
         CREATE TABLE w9 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM w9b RETURNING a) INSERT INTO w9 (a) SELECT a FROM d;
         END;
         ALTER TABLE w9b RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH d AS (DELETE FROM w9b RETURNING w9b.aa AS a) INSERT INTO w9 (a) SELECT a FROM d; END",
        "the item's write follows its rename and the insert keeps reading the item's outputs"
    );
}

/// A three-part reference a factor's alias or a `WITH` item's name would
/// have to reach is refused at ingestion, in a write under a `WITH` clause
/// as well.
#[test]
fn a_block_body_refuses_a_three_part_reference_in_a_write_under_a_with() {
    let refused = parse(
        "CREATE TABLE p21t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM p21t AS y WHERE public.y.a > 0;
         END;",
    )
    .expect_err("the reference reaches the factor through its alias");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.y.a");

    let refused = parse(
        "CREATE TABLE p22t (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1 AS a) DELETE FROM p22t USING x WHERE public.x.a > 0;
         END;",
    )
    .expect_err("the reference reaches the WITH item through a schema");
    let Error::IdentifierLookupError(LookupError::InvalidObjectName { object_name, .. }) = &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(object_name, "public.x.a");
}

/// A statement under a `WITH` clause reading an opaque relation shields its
/// renames the way a top-level one does, keeping the alias an aliased factor
/// carries and restoring a renamed column through one.
#[test]
fn a_block_body_write_under_a_with_shields_its_opaque_factors() {
    let db = parse(
        "CREATE TABLE p23t (a INT);
         CREATE VIEW p23v AS SELECT * FROM p23nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM p23t AS y USING p23v WHERE p23v.nope > 0;
             SELECT 1;
         END;
         ALTER TABLE p23t RENAME TO p23tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) DELETE FROM p23tt AS y USING p23v WHERE p23v.nope > 0; SELECT 1; END",
        "the aliased write target follows the rename under its alias"
    );

    let db = parse(
        "CREATE TABLE p24t (a INT);
         CREATE TABLE p24w (a INT);
         CREATE VIEW p24v AS SELECT * FROM p24nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) DELETE FROM p24t USING p24w, p24v WHERE p24v.nope > p24w.a;
             SELECT 1;
         END;
         ALTER TABLE p24w RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) DELETE FROM p24t USING p24w AS p24w (a), p24v WHERE p24v.nope > p24w.a; SELECT 1; END",
        "the renamed column is restored through the factor's alias column list"
    );
}

/// A `MERGE` `OUTPUT` clause is refused at ingestion, the way PostgreSQL's
/// function-body grammar refuses it.
#[test]
fn a_block_body_refuses_a_merge_output_and_a_dml_return() {
    let refused = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 OUTPUT m1.a;
         END;",
    )
    .expect_err("the merge writes an OUTPUT clause");
    assert!(matches!(&refused, Error::FunctionBodyStatementUnsupported { .. }), "got {refused:?}");
}

/// A function without a `RETURNS` clause declares no result, so its final
/// statement may produce none, and its body follows its renames.
#[test]
fn a_function_without_a_return_type_declares_no_result() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE FUNCTION f() LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1;
         END;
         ALTER TABLE m1 RENAME COLUMN b TO bb;",
    )
    .expect("the body is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() LANGUAGE sql BEGIN ATOMIC UPDATE m1 SET bb = 1; END",
        "the body follows the rename"
    );
}

/// An `INSERT` whose target a `WITH` item answers is refused at ingestion,
/// the way PostgreSQL refuses the relation.
#[test]
fn a_block_body_refuses_an_insert_target_a_with_item_answers() {
    let refused = parse(
        "CREATE TABLE m1 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) INSERT INTO x (a) VALUES (1);
         END;",
    )
    .expect_err("the insert target is a WITH item");
    let Error::RelationNotFoundForFunctionBody { relation_name, function_name } = &refused else {
        panic!("got {refused:?}")
    };
    assert_eq!(relation_name, "x");
    assert_eq!(function_name, "f");
}

/// A `MERGE` follows its clause renames, keeps its `RETURNING` output names,
/// and its `DELETE` clause writes no column list to follow.
#[test]
fn a_block_body_merge_follows_its_renames() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.a;
         END;
         ALTER TABLE m1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC MERGE INTO m1 USING m2 ON m1.aa = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.aa AS a; END",
        "the merge's clauses follow the rename and the returning item keeps its output name"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT 1) MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.a;
         END;
         ALTER TABLE m1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT 1) MERGE INTO m1 USING m2 ON m1.aa = m2.a WHEN MATCHED THEN UPDATE SET b = 1 RETURNING m1.aa AS a; END",
        "the merge under the WITH clause follows the rename"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN UPDATE SET b = 1;
         END;
         ALTER TABLE m1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC MERGE INTO m1 USING m2 ON m1.aa = m2.a WHEN MATCHED THEN UPDATE SET b = 1; END",
        "the merge without a returning clause follows the rename"
    );

    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO m1 USING m2 ON m1.a = m2.a WHEN MATCHED THEN DELETE RETURNING m1.a;
         END;
         ALTER TABLE m1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC MERGE INTO m1 USING m2 ON m1.aa = m2.a WHEN MATCHED THEN DELETE RETURNING m1.aa AS a; END",
        "the merge's delete clause follows the rename"
    );
}

/// A join in an `UPDATE` `FROM` clause is a clause factor that follows its
/// renames, as do the references it scopes.
#[test]
fn a_block_body_write_follows_a_join_in_its_from_clause() {
    let db = parse(
        "CREATE TABLE j1 (a INT, b INT);
         CREATE TABLE j2 (a INT, b INT);
         CREATE TABLE j3 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE j1 SET b = 1 FROM j2 JOIN j3 ON j2.a = j3.a WHERE j1.a = j2.a;
         END;
         ALTER TABLE j2 RENAME TO j2t;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE j1 SET b = 1 FROM j2t JOIN j3 ON j2t.a = j3.a WHERE j1.a = j2t.a; END",
        "the join factor and its references follow the rename"
    );
}

/// A whole-row reference keeps the name it wrote when the rename it names
/// moves the relation the reference reaches under its alias, and a whole-row
/// reference a `WITH` item answers is left as written.
#[test]
fn a_block_body_write_keeps_a_whole_row_reference_the_renames_move() {
    let db = parse(
        "CREATE TABLE k1 (a INT, b INT);
         CREATE TABLE k2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             DELETE FROM k1 USING k2 AS x WHERE x IS NOT NULL;
         END;
         ALTER TABLE k2 RENAME TO k2t;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC DELETE FROM k1 USING k2t AS x WHERE x IS NOT NULL; END",
        "the aliased factor follows the rename and the whole-row reference keeps its name"
    );

    let db = parse(
        "CREATE TABLE t1 (a INT, b INT);
         CREATE TABLE t2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT a FROM t2) UPDATE t1 SET b = 1 FROM x WHERE x IS NOT NULL;
         END;
         ALTER TABLE t1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT a FROM t2) UPDATE t1 SET b = 1 FROM x WHERE x IS NOT NULL; END",
        "the whole-row reference the WITH item answers is left as written"
    );
}

/// A bare reference a column rename makes exposed by two factors takes the
/// name that keeps it reaching what it reached.
#[test]
fn a_block_body_qualifies_a_reference_a_column_rename_exposes_twice() {
    let db = parse(
        "CREATE TABLE c1 (b INT, c INT);
         CREATE TABLE c2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH d AS (SELECT a FROM c2) UPDATE c1 SET b = 1 FROM d WHERE a > 0;
         END;
         ALTER TABLE c1 RENAME COLUMN c TO a;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH d AS (SELECT a FROM c2) UPDATE c1 SET b = 1 FROM d WHERE d.a > 0; END",
        "the reference takes the name that keeps it reaching the WITH item"
    );
}

/// A bare reference to a function parameter takes the function's name when a
/// column rename brings a column of the parameter's name into scope.
#[test]
fn a_block_body_qualifies_a_parameter_under_a_with_clause() {
    let db = parse(
        "CREATE TABLE t1 (a INT, b INT);
         CREATE FUNCTION f41(x INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH d AS (SELECT 1) UPDATE t1 SET b = 1 FROM d WHERE x > 0;
         END;
         ALTER TABLE t1 RENAME COLUMN a TO x;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f41"),
        "CREATE FUNCTION f41(x INT) RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH d AS (SELECT 1) UPDATE t1 SET b = 1 FROM d WHERE f41.x > 0; END",
        "the parameter reference is qualified with the function's name"
    );
}

/// A data-modifying `WITH` item's `RETURNING` list keeps the names its items
/// write, whether an item aliases the reference it names or writes no name
/// at all.
#[test]
fn a_block_body_write_in_a_with_item_keeps_its_returning_item_names() {
    let db = parse(
        "CREATE TABLE t1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM t1 WHERE a > 0 RETURNING a AS x) SELECT x FROM d;
         END;
         ALTER TABLE t1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (DELETE FROM t1 WHERE t1.aa > 0 RETURNING t1.aa AS x) SELECT x FROM d; END",
        "the aliased returning item keeps the alias it writes"
    );

    let db = parse(
        "CREATE TABLE t1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM t1 WHERE a > 0 RETURNING 1) SELECT 1 FROM d;
         END;
         ALTER TABLE t1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (DELETE FROM t1 WHERE t1.aa > 0 RETURNING 1) SELECT 1 FROM d; END",
        "the unnamable returning item is left as written"
    );

    let db = parse(
        "CREATE TABLE t1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH x AS (WITH y AS (SELECT 1) DELETE FROM t1 WHERE a > 0 RETURNING a) SELECT a FROM x;
         END;
         ALTER TABLE t1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (WITH y AS (SELECT 1) DELETE FROM t1 WHERE t1.aa > 0 RETURNING t1.aa AS a) SELECT a FROM x; END",
        "the write in the nested WITH item follows the rename"
    );
}

/// An `INSERT` without a column list writes the target's columns, the row
/// width its source's first row, projection, or set operation's first
/// operand writes, and `DEFAULT VALUES` writes none.
#[test]
fn a_block_body_insert_source_writes_the_target_columns() {
    let db = parse(
        "CREATE TABLE w1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO w1 DEFAULT VALUES;
         END;
         ALTER TABLE w1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO w1 DEFAULT VALUES; END",
        "the default values insert writes no column list"
    );

    let db = parse(
        "CREATE TABLE w1 (a INT, b INT);
         CREATE TABLE c2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO w1 SELECT a FROM c2;
         END;
         ALTER TABLE w1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO w1 (aa) SELECT a FROM c2; END",
        "the select source's projection names the column list"
    );

    let db = parse(
        "CREATE TABLE w1 (a INT, b INT);
         CREATE TABLE c2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO w1 SELECT a FROM c2 UNION SELECT a FROM c2;
         END;
         ALTER TABLE w1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO w1 (aa) SELECT a FROM c2 UNION SELECT a FROM c2; END",
        "the set operation's first operand names the column list"
    );
}

/// An `INSERT` target that reads no columns keeps the `RETURNING` wildcard
/// it writes, and the statement's own sources follow their renames.
#[test]
fn a_block_body_opaque_insert_target_keeps_its_returning_wildcard() {
    let db = parse(
        "CREATE TABLE w1b (a INT, b INT);
         CREATE VIEW v1 AS SELECT * FROM v1nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (INSERT INTO v1 (a) SELECT a FROM w1b RETURNING *) SELECT a FROM d;
         END;
         ALTER TABLE w1b RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (INSERT INTO v1 (a) SELECT aa AS a FROM w1b RETURNING *) SELECT a FROM d; END",
        "the source follows the rename and the wildcard is kept as written"
    );
}

/// A `WITH` item that answers a clause factor's name is skipped by the
/// shield, which shields the renamed factors the statement reads.
#[test]
fn a_block_body_write_under_a_with_shields_past_a_with_item() {
    let db = parse(
        "CREATE TABLE p14t (a INT);
         CREATE VIEW v2 AS SELECT * FROM v2nope;
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH v1 AS (SELECT 1) DELETE FROM p14t USING v1, v2 WHERE p14t.a = v2.a;
         END;
         ALTER TABLE v2 RENAME TO v2t;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH v1 AS (SELECT 1) DELETE FROM p14t USING v1, v2t AS v2 WHERE p14t.a = v2.a; END",
        "the renamed factor is shielded and the WITH item is left alone"
    );
}

/// A data-modifying `WITH` item whose write target a whole-row reference in
/// its own `RETURNING` list names refuses a column rename that captures it.
#[test]
fn a_block_body_refuses_a_target_whole_row_its_returning_names() {
    let refused = parse(
        "CREATE TABLE p12t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM p12t WHERE a > 0 RETURNING p12t) SELECT 1 FROM d;
         END;
         ALTER TABLE p12t RENAME COLUMN a TO p12t;",
    )
    .expect_err("the rename captures the whole-row reference");
    let Error::FunctionBodyBlocksColumnRename { column_name, relation_name, function_name } =
        &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(column_name, "a");
    assert_eq!(relation_name, "p12t");
    assert_eq!(function_name, "f");

    let refused = parse(
        "CREATE TABLE p13t (a INT, b INT);
         CREATE VIEW v1 AS SELECT * FROM v1nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM p13t USING v1 WHERE p13t.a = v1.a RETURNING p13t.a) SELECT a FROM d;
         END;
         ALTER TABLE p13t RENAME COLUMN a TO aa;",
    )
    .expect_err("the rename reaches the shielded write target");
    let Error::FunctionBodyBlocksColumnRename { column_name, relation_name, function_name } =
        &refused
    else {
        panic!("got {refused:?}")
    };
    assert_eq!(column_name, "a");
    assert_eq!(relation_name, "p13t");
    assert_eq!(function_name, "f");
}

/// A write through a `WITH` item keeps its own write lists as written, the
/// item's columns the writes reach, and the item follows its own read.
#[test]
fn a_block_body_write_through_a_with_item_keeps_its_lists() {
    let db = parse(
        "CREATE TABLE u3 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             WITH x AS (SELECT a, b FROM u3) UPDATE x SET b = 1;
         END;
         ALTER TABLE u3 RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT a, bb AS b FROM u3) UPDATE x SET b = 1; END",
        "the item follows its read and the write keeps its lists as written"
    );
}

/// An `ON CONFLICT` target follows a renamed conflict column, as does the
/// column list the target writes.
#[test]
fn a_block_body_insert_conflict_follows_a_renamed_conflict_column() {
    let db = parse(
        "CREATE TABLE w1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO w1 AS t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = t.b + excluded.b;
         END;
         ALTER TABLE w1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO w1 AS t (aa, b) VALUES (1, 2) ON CONFLICT(aa) DO UPDATE SET b = t.b + excluded.b; END",
        "the conflict target and the column list follow the rename"
    );
}

/// A reference an alias column list pins keeps the name the list writes,
/// which a column rename of the pinned column leaves alone.
#[test]
fn a_block_body_keeps_a_reference_a_pinned_alias_column_list_shields() {
    let db = parse(
        "CREATE TABLE q35t (a INT);
         CREATE TABLE q35w (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM q35t USING q35w AS w (z) WHERE z > 0;
             SELECT 1;
         END;
         ALTER TABLE q35w RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM q35t USING q35w AS w (z) WHERE z > 0; SELECT 1; END",
        "the pinned column list keeps the reference as written"
    );
}

/// A pseudo reference a write target does not expose is kept as written, and
/// its set list follows the renames.
#[test]
fn a_block_body_keeps_its_unanalyzable_inserts_and_pseudo_references() {
    let db = parse(
        "CREATE TABLE p1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO p1 (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = excluded.c;
         END;
         ALTER TABLE p1 RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO p1 (a) VALUES (1) ON CONFLICT(a) DO UPDATE SET bb = excluded.c; END",
        "the set list follows the rename and the pseudo reference is kept as written"
    );
}

/// A qualified `RETURNING` wildcard an opaque write target cannot name is
/// kept as written, the wildcard the target answers by its own name.
#[test]
fn a_block_body_opaque_insert_target_keeps_its_named_returning_wildcard() {
    let db = parse(
        "CREATE TABLE w1 (a INT);
         CREATE VIEW v1 AS SELECT * FROM v1nope;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (INSERT INTO v1 (a) SELECT a FROM w1 RETURNING v1.*) SELECT a FROM d;
         END;",
    )
    .expect("the opaque target keeps its wildcard as written");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH d AS (INSERT INTO v1 (a) SELECT a FROM w1 RETURNING v1.*) SELECT a FROM d; END",
        "the wildcard the opaque target cannot name is kept as written"
    );
}

/// An `INSERT` source in parentheses is kept as written at creation.
#[test]
fn a_block_body_insert_keeps_its_parenthesized_source() {
    let db = parse(
        "CREATE TABLE u1 (a INT, b INT);
         CREATE TABLE t1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO t1 (SELECT a FROM u1) RETURNING a;
         END;",
    )
    .expect("the parenthesized source is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO t1 (SELECT a FROM u1) RETURNING a; END",
        "the parenthesized source is kept as written"
    );
}

/// A rename of an opaque write target shields the statement, a `DO UPDATE
/// SET` reference through `EXCLUDED` the target cannot answer keeping its
/// written form while the target follows the rename under its alias.
#[test]
fn a_block_body_shields_an_opaque_target_its_excluded_reference_reaches() {
    let db = parse(
        "CREATE VIEW v1 AS SELECT * FROM v1nope;
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO v1 (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a;
         END;
         ALTER TABLE v1 RENAME TO v1t;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO v1t AS v1 (a) VALUES (1) ON CONFLICT(a) DO UPDATE SET a = excluded.a; END",
        "the target follows the rename under its alias and the reference stays as written"
    );
}

/// A column rename a `RETURNING` list holds aliases only the item it
/// reaches, the rest of the list keeping the names it writes.
#[test]
fn a_block_body_returning_keeps_the_output_names_a_rename_leaves() {
    let db = parse(
        "CREATE TABLE r1 (a INT, b INT);
         CREATE FUNCTION f() RETURNS TABLE (a INT, b INT) LANGUAGE sql BEGIN ATOMIC
             UPDATE r1 SET a = 1 RETURNING a, b;
         END;
         ALTER TABLE r1 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS TABLE(a INT, b INT) LANGUAGE sql BEGIN ATOMIC UPDATE r1 SET aa = 1 RETURNING r1.aa AS a, b; END",
        "the reached item takes the alias and the other keeps its written name"
    );
}

/// A bare reference a column rename makes ambiguous takes the name its
/// factor is reached by, the factor's new name when the renames moved it.
#[test]
fn a_block_body_qualifies_a_capture_its_factor_moved_by_the_renames() {
    let db = parse(
        "CREATE TABLE m1 (b INT, c INT);
         CREATE TABLE m2 (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET c = 1 FROM m2 WHERE b > 0;
             SELECT 1;
         END;
         ALTER TABLE m1 RENAME TO m1t;
         ALTER TABLE m1t RENAME COLUMN b TO a;",
    )
    .expect("the renames are accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE m1t SET c = 1 FROM m2 WHERE m1t.a > 0; SELECT 1; END",
        "the capture takes the name its factor is reached by after the move"
    );
}

/// A tuple target a `DO UPDATE SET` lists is renamed through the parts that
/// name the target's own columns.
#[test]
fn a_block_body_insert_conflict_follows_a_renamed_tuple_target() {
    let db = parse(
        "CREATE TABLE w1 (a INT UNIQUE, b INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             INSERT INTO w1 (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET (a, b) = (1, 2);
         END;
         ALTER TABLE w1 RENAME COLUMN b TO bb;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO w1 (a, bb) VALUES (1, 2) ON CONFLICT(a) DO UPDATE SET (a, bb) = (1, 2); END",
        "the tuple target follows the rename"
    );
}

/// A factor an alias names twice shields the rename through the alias's
/// column list, the references keeping the names they wrote.
#[test]
fn a_block_body_write_shields_a_factor_its_alias_names_twice() {
    let db = parse(
        "CREATE TABLE m1 (a INT, b INT);
         CREATE TABLE m2 (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE m1 SET b = 1 FROM m2 AS m1 WHERE m1.a > 0;
             SELECT 1;
         END;
         ALTER TABLE m2 RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC UPDATE m1 SET b = 1 FROM m2 AS m1 (a, b) WHERE m1.a > 0; SELECT 1; END",
        "the rename is restored through the alias column list"
    );
}

/// A write target or clause factor written with `ONLY` keeps the marker when
/// a rename reaches it.
#[test]
fn a_block_body_keeps_only_markers_through_renames() {
    let db = parse(
        "CREATE TABLE o1t (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             UPDATE ONLY o1t SET a = 1 WHERE a > 0;
         END;
         ALTER TABLE o1t RENAME TO o1u;
         ALTER TABLE o1u RENAME COLUMN a TO b;",
    )
    .expect("the renames are accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE ONLY o1u SET b = 1 WHERE o1u.b > 0; END"
    );

    let db = parse(
        "CREATE TABLE o2t (a INT);
         CREATE TABLE o2s (a INT);
         CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC
             MERGE INTO ONLY o2t USING ONLY o2s ON o2t.a = o2s.a WHEN MATCHED THEN DELETE;
         END;
         ALTER TABLE o2t RENAME TO o2u;
         ALTER TABLE o2s RENAME TO o2v;",
    )
    .expect("the renames are accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC MERGE INTO ONLY o2u USING ONLY o2v ON o2u.a = o2v.a WHEN MATCHED THEN DELETE; END"
    );
}
