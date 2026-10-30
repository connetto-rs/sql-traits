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
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE t SET b = a WHERE a IN (SELECT a FROM w);
         END;
         RENAME TABLE w TO app.w;",
    )
    .expect("the move is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
         UPDATE t SET b = a WHERE a IN (SELECT a FROM app.w); END",
        "the schema move is followed in the subquery"
    );

    let db = parse(
        "CREATE TABLE t (id INT, a INT);
         CREATE TABLE u (id INT, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             UPDATE t SET a = u.a FROM u WHERE t.id = u.id;
         END;
         ALTER TABLE u RENAME TO uu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
         UPDATE t SET a = uu.a FROM uu WHERE t.id = uu.id; END",
        "the rename is followed in the clause factor and the qualified references"
    );

    let db = parse(
        "CREATE TABLE t (id INT, a INT);
         CREATE TABLE s (id INT, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO t USING s ON t.id = s.id
             WHEN MATCHED THEN UPDATE SET a = s.a
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a);
         END;
         ALTER TABLE s RENAME TO ss;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC \
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
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
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
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
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
/// way the block's other statements bind; `SET` is refused at ingestion, and
/// `CALL` only when the named function is held.
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

    // A procedure is not modeled, so the call stays as written and only the
    // relations its arguments read are bound and followed.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             CALL p((SELECT count(*) FROM t));
             SELECT 1;
         END;
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("a procedure call is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC CALL p((SELECT count(*) FROM t2)); SELECT 1; END"
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
             DELETE FROM qct JOIN qcu ON qct.id = qcu.id WHERE qct.a > 0;
             SELECT 1;
         END;
         ALTER TABLE qcu RENAME TO qcuu;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM qct JOIN qcuu ON qct.id = qcuu.id WHERE qct.a > 0; SELECT 1; END",
        "the from join follows the rename"
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

/// A `MERGE` clause's actions name the target's columns, and a wildcard
/// update lists none to follow.
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

    let db = parse(
        "CREATE TABLE pw (id INT, a INT);
         CREATE TABLE ps (id INT, a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             MERGE INTO pw USING ps ON pw.id = ps.id
             WHEN MATCHED THEN UPDATE SET *;
             SELECT 1;
         END;
         ALTER TABLE pw RENAME COLUMN a TO aa;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC MERGE INTO pw USING ps ON pw.id = ps.id WHEN MATCHED THEN UPDATE SET *; SELECT 1; END",
        "the wildcard update lists no column to follow"
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

    let db = parse(
        "CREATE TABLE pa (a INT);
         CREATE TABLE pb (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM pa USING pb WHERE a > 0;
             SELECT 1;
         END;
         ALTER TABLE pa RENAME TO pa2;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM pa2 AS pa USING pb WHERE a > 0; SELECT 1; END",
        "the bare reference reaches two factors, so the rename shields it"
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

    // A factor the statement already reaches under an alias is left as
    // written under that alias.
    let db = parse(
        "CREATE TABLE ps2t (a INT);
         CREATE TABLE ps2s (a INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             DELETE FROM ps2t AS x USING ps2s WHERE a > 0;
             SELECT 1;
         END;
         ALTER TABLE ps2t RENAME TO ps2tt;",
    )
    .expect("the rename is accepted");
    assert_eq!(
        text(&db, "f"),
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC DELETE FROM ps2t AS x USING ps2s WHERE a > 0; SELECT 1; END",
        "the factor the statement reaches under an alias is left as written"
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
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC INSERT INTO ps3t (aa) VALUES (1) RETURNING ps3t.aa; SELECT 1; END",
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
         ALTER TABLE app.q32t RENAME TO app.q32u;",
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

    // A write under `WITH` is stored as a query, which keeps reading the
    // renamed table under its old name through an alias.
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
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC WITH x AS (SELECT q23u.a FROM q23u) DELETE FROM q23w AS q23t USING x WHERE q23t.a > 0; SELECT 1; END",
        "the write keeps reading the renamed table"
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
