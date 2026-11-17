//! Every statement and expression a PL/pgSQL body runs, the variable a
//! column reference's written name reaches beside the column it binds, which
//! PostgreSQL 18 refuses as ambiguous under `plpgsql.variable_conflict =
//! error`, and the definitions reaching each variable a site reads.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails loudly on a broken premise"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "test code ships in no binary, so walking the tree directly costs nothing"
)]

use core::ops::ControlFlow;

use sql_traits::prelude::*;
use sqlparser::{
    ast::{
        ArgMode, ConflictTarget, Ident, OnInsert, SelectItem, SelectItemQualifiedWildcardKind,
        SetExpr, Statement, TableFactor, visit_expressions,
    },
    dialect::PostgreSqlDialect,
};

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// The SQL of each site of `name`, written out.
fn sites(database: &ParserDB, name: &str) -> Vec<String> {
    let body = function(database, name).plpgsql_sites(database).unwrap();
    body.sites().unwrap().iter().map(|site| site.sql().to_string()).collect()
}

/// What the column reference `written` binds in the first site of `name`
/// writing it, `column`, `variable` or `nothing`, with the name of the
/// variable its written name reaches.
fn reached(database: &ParserDB, name: &str, written: &str) -> (&'static str, Option<String>) {
    let body = function(database, name).plpgsql_sites(database).unwrap();
    for site in body.sites().unwrap() {
        let context = site.context(IdentifierCase::Folded).unwrap();
        let mut found = None;
        let _ = visit_expressions(&site.sql(), |expression| {
            if expression.to_string() != written {
                return ControlFlow::Continue(());
            }
            let bound = match context.reference(expression).unwrap() {
                Some((ColumnDefinition::Variable(_) | ColumnDefinition::VariableField(_), _)) => {
                    "variable"
                }
                Some(_) => "column",
                None => "nothing",
            };
            let variable = context.variable(expression).unwrap();
            found = Some((bound, variable.map(|variable| variable.name().to_owned())));
            ControlFlow::Break(())
        });
        if let Some(found) = found {
            return found;
        }
    }
    panic!("{name} writes no {written}")
}

/// Every statement and expression is a site, an `EXECUTE`'s command and
/// `USING` arguments one each, in body order, the `SET` of `search_path`
/// left out.
#[test]
fn every_statement_and_expression_is_a_site_in_body_order() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION walk(p integer) RETURNS integer LANGUAGE plpgsql AS $$
         DECLARE
           total integer := p + 1;
           c CURSOR FOR SELECT id FROM notes;
           row_ record;
         BEGIN
           SET search_path = public;
           IF total > 2 THEN
             total := total * 2;
           END IF;
           FOR i IN 1 .. total LOOP
             PERFORM i;
           END LOOP;
           FOR row_ IN SELECT label FROM notes LOOP
             UPDATE notes SET label = row_.label WHERE id = p;
           END LOOP;
           OPEN c;
           EXECUTE 'SELECT $1' USING total;
           RETURN total;
         END $$;",
    );
    assert_eq!(
        sites(&database, "walk"),
        [
            "SELECT p + 1",
            "SELECT total > 2",
            "SELECT total * 2",
            "SELECT 1",
            "SELECT total",
            "SELECT i",
            "SELECT label FROM notes",
            "UPDATE notes SET label = row_.label WHERE id = p",
            "SELECT id FROM notes",
            "SELECT 'SELECT $1'",
            "SELECT total",
            "SELECT total",
        ]
    );
}

/// A body in SQL has no PL/pgSQL site.
#[test]
fn a_sql_body_has_no_site() {
    let database = postgres(
        "CREATE TABLE notes (id integer);
         CREATE FUNCTION plain(id integer) RETURNS bigint LANGUAGE sql AS
         $$ SELECT count(*) FROM notes WHERE notes.id = id $$;",
    );
    assert_eq!(sites(&database, "plain"), Vec::<String>::new());
}

/// A site stands at its point of the body's value flow, so a field of a
/// record takes the type of the rows reaching it there.
#[test]
fn a_record_field_types_from_the_rows_reaching_its_site() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label varchar(10));
         CREATE FUNCTION first_label() RETURNS text LANGUAGE plpgsql AS $$
         DECLARE r record; result text;
         BEGIN
           SELECT label INTO r FROM notes LIMIT 1;
           result := r.label;
           RETURN result;
         END $$;",
    );
    let body = function(&database, "first_label").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let site = &sites[1];
    assert_eq!(site.sql().to_string(), "SELECT r.label");
    let context = site.context(IdentifierCase::Folded).unwrap();
    let mut typed = None;
    let _ = visit_expressions(&site.sql(), |expression| {
        if expression.to_string() == "r.label" {
            typed = Some(context.type_of(expression).unwrap());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    let varchar = TargetName::new("varchar", false).with_schema("pg_catalog", false);
    assert_eq!(typed, Some(OperandType::Known { type_name: varchar, typmod: Some(14) }));
}

const FIND_NOTE: &str = "CREATE TABLE notes (id integer PRIMARY KEY, label text);
    CREATE FUNCTION find_note(id integer) RETURNS integer LANGUAGE plpgsql AS $$
    DECLARE result integer;
    BEGIN
      SELECT id INTO result FROM notes WHERE notes.id = find_note.id;
      RETURN result;
    END $$;";

/// The bare name binds the column and also reaches the argument, the
/// reference PostgreSQL 18 refuses, while each qualified name reaches one.
#[test]
fn a_bare_name_reaches_both_the_column_and_the_argument() {
    let database = postgres(FIND_NOTE);
    assert_eq!(reached(&database, "find_note", "id"), ("column", Some("id".to_owned())));
    assert_eq!(reached(&database, "find_note", "notes.id"), ("column", None));
    assert_eq!(
        reached(&database, "find_note", "find_note.id"),
        ("variable", Some("id".to_owned()))
    );
}

/// The collision inside a scalar subquery of an `EXECUTE` command is the
/// same one.
#[test]
fn a_dynamic_command_reaches_the_argument_beside_the_column() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION find_note(id integer) RETURNS text LANGUAGE plpgsql AS $$
         DECLARE result text;
         BEGIN
           EXECUTE (SELECT 'SELECT ' || id FROM notes LIMIT 1) INTO result;
           RETURN result;
         END $$;",
    );
    assert_eq!(reached(&database, "find_note", "id"), ("column", Some("id".to_owned())));
    let function = function(&database, "find_note");
    let body = function.dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let context = sites[0].context(IdentifierCase::Folded).unwrap();
    let mut variable = None;
    let _ = visit_expressions(sites[0].command(), |expression| {
        if expression.to_string() == "id" {
            variable = Some(context.variable(expression).unwrap());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    let parameter = variable.expect("the command writes id").expect("id reaches the argument");
    assert!(matches!(parameter.kind(), VariableKind::Parameter { position: 1, .. }));
}

/// A column named `new` reaches a variable only in a trigger function.
#[test]
fn new_reaches_a_variable_only_in_a_trigger_function() {
    let database = postgres(
        "CREATE TABLE flags (new integer);
         CREATE FUNCTION plain() RETURNS integer LANGUAGE plpgsql AS $$
         DECLARE result integer;
         BEGIN SELECT new INTO result FROM flags; RETURN result; END $$;
         CREATE FUNCTION fired() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM new FROM flags; RETURN NEW; END $$;",
    );
    assert_eq!(reached(&database, "plain", "new"), ("column", None));
    let (bound, variable) = reached(&database, "fired", "new");
    assert_eq!((bound, variable.as_deref()), ("column", Some("new")));
}

/// PostgreSQL 18 analyzes a utility statement such as `CREATE INDEX` with no
/// PL/pgSQL names in reach, so its column reaches no argument of its name.
#[test]
fn a_utility_statement_reaches_no_variable() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY);
         CREATE FUNCTION indexed(id integer) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN CREATE INDEX ON notes (id); END $$;",
    );
    assert_eq!(reached(&database, "indexed", "id"), ("nothing", None));
}

/// A table alias shadows a record variable and a block label of its name,
/// which PostgreSQL 18 refuses as ambiguous, but never a scalar variable,
/// which has no field, nor a record followed by more than one field.
#[test]
fn qualified_names_reach_records_and_labels_but_not_scalars() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION by_record() RETURNS integer LANGUAGE plpgsql AS $$
         DECLARE r record; result integer;
         BEGIN SELECT r.id INTO result FROM notes r; RETURN result; END $$;
         CREATE FUNCTION by_scalar() RETURNS integer LANGUAGE plpgsql AS $$
         DECLARE n integer := 5; result integer;
         BEGIN SELECT n.id INTO result FROM notes n; RETURN result; END $$;
         CREATE FUNCTION by_label() RETURNS integer LANGUAGE plpgsql AS $$
         <<blk>> DECLARE id integer := 7; result integer;
         BEGIN SELECT blk.id INTO result FROM notes blk; RETURN result; END $$;
         CREATE FUNCTION by_schema() RETURNS integer LANGUAGE plpgsql AS $$
         DECLARE public record; result integer;
         BEGIN SELECT public.notes.id INTO result FROM public.notes; RETURN result; END $$;",
    );
    assert_eq!(reached(&database, "by_record", "r.id"), ("column", Some("r".to_owned())));
    assert_eq!(reached(&database, "by_scalar", "n.id"), ("column", None));
    assert_eq!(reached(&database, "by_label", "blk.id"), ("column", Some("id".to_owned())));
    assert_eq!(reached(&database, "by_schema", "public.notes.id"), ("column", None));
}

/// The scope reaches the variable a qualified wildcard and a conflict target
/// name, which no column reference node writes.
#[test]
fn wildcards_and_conflict_targets_reach_through_the_scope() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION spread() RETURNS bigint LANGUAGE plpgsql AS $$
         DECLARE r notes; s integer; c bigint;
         BEGIN
           SELECT count(*) INTO c FROM (SELECT r.* FROM notes r) x;
           SELECT count(*) INTO c FROM (SELECT s.* FROM notes s) x;
           RETURN c;
         END $$;
         CREATE FUNCTION upsert() RETURNS TABLE(id integer) LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO notes (id) VALUES (2) ON CONFLICT (id) DO NOTHING;
           RETURN;
         END $$;",
    );
    let body = function(&database, "spread").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let wildcards: Vec<Option<String>> =
        sites[..2]
            .iter()
            .map(|site| {
                let EmbeddedSql::Statement(Statement::Query(query)) = site.sql() else {
                    panic!("a query")
                };
                let SetExpr::Select(outer) = query.body.as_ref() else { panic!("a select") };
                let TableFactor::Derived { subquery, .. } = &outer.from[0].relation else {
                    panic!("a derived table")
                };
                let SetExpr::Select(inner) = subquery.body.as_ref() else { panic!("a select") };
                let SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(name),
                    _,
                ) = &inner.projection[0]
                else {
                    panic!("a qualified wildcard")
                };
                let qualifier: Vec<Ident> =
                    name.0.iter().map(|part| part.as_ident().unwrap().clone()).collect();
                site.plpgsql_scope()
                    .wildcard_variable(&qualifier)
                    .map(|variable| variable.name().to_owned())
            })
            .collect();
    assert_eq!(wildcards, [Some("r".to_owned()), None]);

    let body = function(&database, "upsert").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let EmbeddedSql::Statement(Statement::Insert(insert)) = sites[0].sql() else {
        panic!("an insert")
    };
    let Some(OnInsert::OnConflict(conflict)) = &insert.on else { panic!("on conflict") };
    let Some(ConflictTarget::Columns { columns, .. }) = &conflict.conflict_target else {
        panic!("conflict columns")
    };
    let output = sites[0].plpgsql_scope().variable(&columns[..1]).expect("id is the output column");
    assert_eq!(output.kind(), &VariableKind::Parameter { position: 1, mode: ArgMode::Out });
}

/// Each read of `name`'s sites, in body order, as the variable read and
/// what reaches it, a value as written and any other source by kind.
fn reads(database: &ParserDB, name: &str) -> Vec<Vec<String>> {
    let body = function(database, name).plpgsql_sites(database).unwrap();
    let sites = body.sites().unwrap();
    sites
        .iter()
        .map(|site| {
            site.reads(IdentifierCase::Folded)
                .unwrap()
                .iter()
                .map(|read| {
                    let reaching = match read.reaching() {
                        Reaching::Definitions(definitions) => {
                            definitions
                                .iter()
                                .map(|definition| {
                                    match definition.source() {
                                        DefinitionSource::Value(value) => value.to_string(),
                                        DefinitionSource::Incoming => "incoming".to_owned(),
                                        DefinitionSource::QueryOutput { .. } => "query".to_owned(),
                                        other => format!("{other:?}"),
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(" | ")
                        }
                        Reaching::Unreachable => "unreachable".to_owned(),
                    };
                    format!("{}: {reaching}", read.variable().name())
                })
                .collect()
        })
        .collect()
}

/// Every site reads the variables its own SQL names, with the definitions
/// each path to it may leave: a column binds ahead of a variable, an `INTO`
/// target is written and not read, an `EXECUTE`'s command and `USING`
/// argument each read their own, and a site after `RETURN` is reached by no
/// path.
#[test]
fn each_site_reads_its_variables_with_the_definitions_reaching_it() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION relabel(p integer, q text) RETURNS text LANGUAGE plpgsql AS $$
         DECLARE
           target integer := 0;
           found text;
         BEGIN
           IF p > 0 THEN
             target := p;
           END IF;
           SELECT label INTO found FROM notes WHERE id = target;
           EXECUTE 'SELECT ' || found USING q;
           RETURN found;
           RETURN q;
         END $$;",
    );
    assert_eq!(
        reads(&database, "relabel"),
        [
            vec![],
            vec!["p: incoming".to_owned()],
            vec!["p: incoming".to_owned()],
            vec!["target: 0 | p".to_owned()],
            vec!["found: query".to_owned()],
            vec!["q: incoming".to_owned()],
            vec!["found: query".to_owned()],
            vec!["q: unreachable".to_owned()],
        ]
    );
}

/// A `LIMIT` or `OFFSET` count reads the variables and outer columns it
/// names, and binds no column of its own query's `FROM`, which PostgreSQL
/// refuses there.
#[test]
fn row_counts_read_variables_and_outer_columns_but_not_their_own_from() {
    let database = postgres(
        "CREATE TABLE notes (id integer PRIMARY KEY, label text);
         CREATE FUNCTION page(p integer) RETURNS SETOF text LANGUAGE plpgsql AS $$
         DECLARE
           size integer := 10;
           skip integer := 0;
         BEGIN
           RETURN QUERY SELECT label FROM notes LIMIT size OFFSET skip;
           RETURN QUERY SELECT label FROM notes OFFSET p ROWS FETCH FIRST 5 ROWS ONLY;
         END $$;
         CREATE FUNCTION nested() RETURNS SETOF text LANGUAGE plpgsql AS $$
         BEGIN
           RETURN QUERY SELECT n.label FROM notes AS n
             WHERE n.label IN (SELECT label FROM notes LIMIT n.id);
         END $$;
         CREATE FUNCTION own() RETURNS SETOF text LANGUAGE plpgsql AS $$
         BEGIN RETURN QUERY SELECT label FROM notes LIMIT id; END $$;",
    );
    assert_eq!(
        reads(&database, "page"),
        [
            vec![],
            vec![],
            vec!["size: 10".to_owned(), "skip: 0".to_owned()],
            vec!["p: incoming".to_owned()],
        ]
    );
    assert_eq!(reached(&database, "page", "size"), ("variable", Some("size".to_owned())));
    assert_eq!(reached(&database, "nested", "n.id"), ("column", None));
    assert_eq!(reached(&database, "own", "id"), ("nothing", None));
}
