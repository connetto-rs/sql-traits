//! Integration tests for the columns a function called in `FROM` exposes,
//! named and typed the way PostgreSQL builds the function's result.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking on unexpected outputs"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "test code ships in no binary, so walking the tree directly costs nothing"
)]

use core::ops::ControlFlow;

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Ident, Query, SelectItem, SetExpr, visit_expressions},
    dialect::PostgreSqlDialect,
};

use crate::support::query_parsing::query_with;

const SCHEMA: &str = "
    CREATE TABLE notes (id INT PRIMARY KEY, label VARCHAR(12) NOT NULL);
    CREATE VIEW labels AS SELECT label, id + 1 AS next_id FROM notes;
    CREATE TYPE pair AS (left_side INT, right_side TEXT COLLATE \"C\");
    CREATE DOMAIN positive AS INT CHECK (VALUE > 0);
    CREATE FUNCTION held_notes() RETURNS TABLE (note_id INT, label TEXT)
        LANGUAGE sql AS $$ SELECT id, label FROM notes $$;
    CREATE FUNCTION held_pairs(OUT note_id INT, OUT label TEXT) RETURNS SETOF record
        LANGUAGE sql AS $$ SELECT id, label FROM notes $$;
    CREATE FUNCTION bumped(INOUT total INT, step INT, OUT label TEXT)
        LANGUAGE sql AS $$ SELECT total + step, 'x' $$;
    CREATE FUNCTION unnamed_pair(OUT INT, OUT TEXT) LANGUAGE sql AS $$ SELECT 1, 'x' $$;
    CREATE FUNCTION one_out(OUT total INT) LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION one_unnamed_out(OUT INT) LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION ids() RETURNS SETOF BIGINT LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION one_id() RETURNS INT LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION all_notes() RETURNS SETOF notes LANGUAGE sql AS $$ SELECT * FROM notes $$;
    CREATE FUNCTION all_labels() RETURNS SETOF labels LANGUAGE sql AS $$ SELECT * FROM labels $$;
    CREATE FUNCTION one_pair() RETURNS pair LANGUAGE sql AS $$ SELECT 1, 'x' $$;
    CREATE FUNCTION one_positive() RETURNS positive LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION rows_of() RETURNS SETOF record LANGUAGE sql AS $$ SELECT 1, 'x' $$;
    CREATE FUNCTION same_value(value anyelement) RETURNS anyelement
        LANGUAGE sql AS $$ SELECT value $$;
    CREATE FUNCTION both_ways(value INT) RETURNS INT LANGUAGE sql AS $$ SELECT value $$;
    CREATE FUNCTION both_ways(value TEXT) RETURNS INT LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION probe(note_id INT) RETURNS VOID LANGUAGE plpgsql AS $$
    BEGIN
        PERFORM 1 FROM held_notes() held WHERE note_id = 1 AND held.note_id = 2;
    END $$;
    CREATE PROCEDURE tidy() LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE TYPE mood AS ENUM ('sad', 'glad');
    CREATE TYPE tagged_pair AS (tags TEXT[], label TEXT);
    CREATE TABLE tagged (id INT, tags TEXT[]);
    CREATE MATERIALIZED VIEW note_labels AS SELECT label FROM notes;
    CREATE VIEW loose AS SELECT * FROM same_value(1);
    CREATE FUNCTION one_mood() RETURNS mood LANGUAGE sql AS $$ SELECT 'sad'::mood $$;
    CREATE FUNCTION qualified_notes() RETURNS SETOF public.notes
        LANGUAGE sql AS $$ SELECT * FROM notes $$;
    CREATE FUNCTION all_note_labels() RETURNS SETOF note_labels
        LANGUAGE sql AS $$ SELECT * FROM note_labels $$;
    CREATE FUNCTION all_loose() RETURNS SETOF loose LANGUAGE sql AS $$ SELECT * FROM loose $$;
    CREATE FUNCTION untyped() RETURNS SETOF nowhere LANGUAGE sql AS $$ SELECT 1 $$;
    CREATE FUNCTION tag_lists() RETURNS TABLE (tags TEXT[]) LANGUAGE sql AS $$ SELECT '{}' $$;
    CREATE FUNCTION all_tagged() RETURNS SETOF tagged LANGUAGE sql AS $$ SELECT * FROM tagged $$;
    CREATE FUNCTION one_tagged_pair() RETURNS tagged_pair LANGUAGE sql AS $$ SELECT '{}', 'x' $$;
    CREATE FUNCTION pair_list() RETURNS _pair LANGUAGE sql AS $$ SELECT '{}' $$;
    CREATE TYPE shell;
    CREATE FUNCTION shell_value() RETURNS shell LANGUAGE sql AS $$ SELECT NULL $$;
";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    query_with(&PostgreSqlDialect {}, sql)
}

fn describe(definition: &ColumnDefinition<'_, '_, '_, ParserDB>) -> String {
    match definition {
        ColumnDefinition::FunctionResult { function, data_type } => {
            let data_type = match data_type {
                FunctionResultType::Signature(name) => (*name).to_owned(),
                FunctionResultType::RowType(column) => {
                    format!("row {}", describe(&column.definition()))
                }
                FunctionResultType::Attribute { field, database } => {
                    let data_type = field.data_type().name(database);
                    match field.collation() {
                        ColumnCollation::Named(collation) => {
                            format!("attribute {data_type} COLLATE {}", collation.name().name())
                        }
                        _ => format!("attribute {data_type}"),
                    }
                }
                FunctionResultType::DefinitionList(data_type) => format!("listed {data_type}"),
            };
            format!("{} {data_type}", function.name())
        }
        ColumnDefinition::Base { table, column, .. } => {
            format!("{}.{}", table.table_name(), column.column_name())
        }
        ColumnDefinition::Expression { expression, .. } => format!("expression {expression}"),
        ColumnDefinition::Ordinality => "ordinality".to_owned(),
        ColumnDefinition::ArrayElement { .. } => "element".to_owned(),
        ColumnDefinition::Opaque => "opaque".to_owned(),
        other => panic!("unexpected definition {other:?}"),
    }
}

/// The name and definition of every column `SELECT * FROM {from}` outputs,
/// a quoted name double-quoted, or `["*"]` when the wildcard cannot expand.
fn columns(from: &str) -> Vec<(String, String)> {
    let database = database();
    let query = query(&format!("SELECT * FROM {from}"));
    let scope =
        ColumnScope::from_query(&query, &database, IdentifierCase::Folded).expect("scope builds");
    scope
        .scope_for_query(&query)
        .expect("query is recorded")
        .outputs()
        .map(|output| {
            match output {
                QueryOutput::Column { name: Some((name, quoted)), definition } => {
                    let name = if quoted { format!("\"{name}\"") } else { name.to_owned() };
                    (name, describe(&definition))
                }
                QueryOutput::Column { name: None, .. } => panic!("an unnamed column"),
                QueryOutput::Unexpanded => ("*".to_owned(), "unexpanded".to_owned()),
            }
        })
        .collect()
}

fn named(columns: &[(&str, &str)]) -> Vec<(String, String)> {
    columns
        .iter()
        .map(|(name, definition)| ((*name).to_owned(), (*definition).to_owned()))
        .collect()
}

fn unexpanded() -> Vec<(String, String)> {
    named(&[("*", "unexpanded")])
}

/// The definition `name` resolves to beside `from`, with the column name
/// its binding exposes, or `None` when it resolves nothing.
fn bare_name(from: &str, name: &str) -> Option<(String, Option<String>)> {
    let database = database();
    let query = query(&format!("SELECT 1 FROM {from}"));
    let scope =
        ColumnScope::from_query(&query, &database, IdentifierCase::Folded).expect("scope builds");
    scope.resolve_column_binding(&Expr::Identifier(Ident::new(name))).expect("lookup succeeds").map(
        |(definition, binding)| {
            (describe(&definition), binding.exposed().map(|(exposed, _)| exposed.to_owned()))
        },
    )
}

/// The request's reproduction: a name the function's result holds resolves
/// to that column, and a name it does not hold resolves nothing.
#[test]
fn a_bare_name_beside_a_returns_table_call_resolves_to_its_column() {
    assert_eq!(
        bare_name("held_notes() held", "note_id"),
        Some(("held_notes INT".to_owned(), Some("note_id".to_owned())))
    );
    assert_eq!(bare_name("held_notes() held", "anything_at_all"), None);
    assert_eq!(bare_name("notes", "anything_at_all"), None);
}

#[test]
fn returns_table_and_output_arguments_expose_their_columns_in_order() {
    assert_eq!(
        columns("held_notes()"),
        named(&[("note_id", "held_notes INT"), ("label", "held_notes TEXT")])
    );
    assert_eq!(
        columns("held_pairs()"),
        named(&[("note_id", "held_pairs INT"), ("label", "held_pairs TEXT")])
    );
    // `INOUT` is an output too, and the input `step` is none.
    assert_eq!(
        columns("bumped(1, 2)"),
        named(&[("total", "bumped INT"), ("label", "bumped TEXT")])
    );
    // PostgreSQL names an unnamed output by its position among the outputs.
    assert_eq!(
        columns("unnamed_pair()"),
        named(&[("\"column1\"", "unnamed_pair INT"), ("\"column2\"", "unnamed_pair TEXT")])
    );
}

/// A function of one output names its column after the output, then the
/// alias, then itself, as PostgreSQL's `chooseScalarFunctionAlias` does.
#[test]
fn a_single_column_takes_the_name_postgresql_chooses() {
    assert_eq!(columns("one_out() AS t"), named(&[("total", "one_out INT")]));
    assert_eq!(columns("one_unnamed_out() AS t"), named(&[("t", "one_unnamed_out INT")]));
    assert_eq!(
        columns("one_unnamed_out()"),
        named(&[("\"one_unnamed_out\"", "one_unnamed_out INT")])
    );
    assert_eq!(columns("ids()"), named(&[("\"ids\"", "ids BIGINT")]));
    assert_eq!(columns("ids() AS i"), named(&[("i", "ids BIGINT")]));
    assert_eq!(columns("ids() AS t(i)"), named(&[("i", "ids BIGINT")]));
    assert_eq!(columns("one_id()"), named(&[("\"one_id\"", "one_id INT")]));
    // A domain over a scalar type and an enum are scalar results.
    assert_eq!(columns("one_positive() AS p"), named(&[("p", "one_positive positive")]));
    assert_eq!(columns("one_mood()"), named(&[("\"one_mood\"", "one_mood mood")]));
    // An array type spelled as its stored `_` name is a scalar result too.
    assert_eq!(columns("pair_list()"), named(&[("\"pair_list\"", "pair_list public.pair[]")]));
}

#[test]
fn a_row_type_result_exposes_the_columns_of_its_type() {
    assert_eq!(
        columns("all_notes()"),
        named(&[("id", "all_notes row notes.id"), ("label", "all_notes row notes.label")])
    );
    assert_eq!(
        columns("qualified_notes()"),
        named(&[
            ("id", "qualified_notes row notes.id"),
            ("label", "qualified_notes row notes.label")
        ])
    );
    assert_eq!(
        columns("all_labels()"),
        named(&[
            ("label", "all_labels row notes.label"),
            ("next_id", "all_labels row expression id + 1")
        ])
    );
    assert_eq!(
        columns("all_note_labels()"),
        named(&[("label", "all_note_labels row notes.label")])
    );
    assert_eq!(
        columns("one_pair()"),
        named(&[
            ("left_side", "one_pair attribute INT"),
            ("right_side", "one_pair attribute TEXT COLLATE C")
        ])
    );
}

/// A column of a composite attribute declared with `COLLATE` keeps the
/// collation and the facts the type resolved when it was declared, as
/// PostgreSQL hands the attribute's `attcollation` to the column.
#[test]
fn a_composite_attribute_column_keeps_its_collation() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE labelled AS (label TEXT COLLATE \"C\");
         CREATE FUNCTION labels() RETURNS SETOF labelled LANGUAGE sql AS $$ SELECT 'a'::text $$;",
    )
    .expect("schema parses");
    let query = query("SELECT l.label FROM labels() AS l");
    let scope =
        ColumnScope::from_query(&query, &database, IdentifierCase::Folded).expect("scope builds");
    let reference = Expr::CompoundIdentifier(vec![Ident::new("l"), Ident::new("label")]);
    let Some((
        ColumnDefinition::FunctionResult {
            data_type: FunctionResultType::Attribute { field, database: holding },
            ..
        },
        _,
    )) = scope.resolve_column_binding(&reference).expect("lookup succeeds")
    else {
        panic!("`l.label` resolves to an attribute of the composite result");
    };
    assert_eq!((field.name(), field.data_type().name(holding)), (Some("label"), "TEXT".into()));
    let ColumnCollation::Named(collation) = field.collation() else {
        panic!("a declared COLLATE names the collation");
    };
    assert_eq!(collation.name().name(), "C");
    assert_eq!(collation.postgres_deterministic(), Some(true));
}

#[test]
fn aliases_rename_and_ordinality_appends() {
    assert_eq!(
        columns("held_notes() held(renamed_id)"),
        named(&[("renamed_id", "held_notes INT"), ("label", "held_notes TEXT")])
    );
    assert_eq!(
        columns("held_notes() WITH ORDINALITY"),
        named(&[
            ("note_id", "held_notes INT"),
            ("label", "held_notes TEXT"),
            ("\"ordinality\"", "ordinality")
        ])
    );
    assert_eq!(
        columns("ids() WITH ORDINALITY AS t(i, n)"),
        named(&[("i", "ids BIGINT"), ("n", "ordinality")])
    );
    // A `LATERAL` call and a schema-qualified call reach the same function.
    assert_eq!(
        columns("notes, LATERAL held_notes() AS h(a, b)")[2..],
        named(&[("a", "held_notes INT"), ("b", "held_notes TEXT")])
    );
    assert_eq!(columns("public.one_id() AS o"), named(&[("o", "one_id INT")]));
}

#[test]
fn a_column_definition_list_declares_the_columns_of_a_record_result() {
    assert_eq!(
        columns("rows_of() AS t(a INT, b VARCHAR(3))"),
        named(&[("a", "rows_of listed INT"), ("b", "rows_of listed VARCHAR(3)")])
    );
}

/// PostgreSQL refuses a column definition list beside `WITH ORDINALITY` and
/// for any result but `record`, and requires one for a bare `record`.
#[test]
fn calls_whose_columns_are_not_known_stay_opaque() {
    for from in [
        "rows_of()",
        "rows_of() AS t",
        "rows_of() WITH ORDINALITY AS t(a INT, b TEXT)",
        "held_notes() AS t(a INT, b TEXT)",
        "held_pairs() AS t(a INT, b TEXT)",
        "ids() AS t(a BIGINT)",
        "held_notes() AS t(a, b, c)",
        "same_value(1)",
        "both_ways(1)",
        "missing_function()",
        "held_notes(1)",
        "tidy()",
        "elsewhere.held_notes()",
        "catalog.public.held_notes()",
        "untyped()",
        "all_loose()",
        "shell_value()",
    ] {
        assert_eq!(columns(from), unexpanded(), "`{from}` stays opaque");
    }
}

/// `unnest` expands a function's result column only when the column's
/// declared type is an array, whatever declares it.
#[test]
fn unnest_expands_a_function_result_column_of_an_array_type() {
    for from in [
        "tag_lists() l",
        "all_tagged() l",
        "one_tagged_pair() l",
        "rows_of() AS l(tags TEXT[], label TEXT)",
    ] {
        let columns = columns(&format!("{from}, unnest(l.tags) AS u(tag)"));
        assert_eq!(columns.last(), Some(&("tag".to_owned(), "element".to_owned())), "{from}");
    }
    for from in ["held_notes() l", "all_notes() l", "one_pair() l", "rows_of() AS l(label TEXT)"] {
        let columns = columns(&format!("{from}, unnest(l.label) AS u(tag)"));
        assert_eq!(columns.last(), unexpanded().last(), "{from}");
    }
}

/// A function's rows are no rows of a base table, even when the function
/// returns that table's row type.
#[test]
fn a_function_result_has_no_source_table() {
    let database = database();
    for sql in ["SELECT * FROM all_notes()", "SELECT id FROM all_notes() WITH ORDINALITY"] {
        let source = query(sql).projection_source_table(&database).expect("source resolves");
        assert!(source.is_none(), "{sql}");
    }
}

/// The column types the expression context gives each kind of result.
#[test]
fn function_result_columns_take_their_declared_types() {
    let database = database();
    let query = query(
        "SELECT h.note_id, n.label, p.right_side, r.b, i.i, l.label
         FROM held_notes() h, all_notes() n, one_pair() p, rows_of() AS r(a INT, b VARCHAR(3)),
              ids() AS i, all_labels() l",
    );
    let context =
        ExpressionContext::new(&query, &database, IdentifierCase::Folded).expect("context builds");
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT body") };
    let types: Vec<_> = select
        .projection
        .iter()
        .map(|item| {
            let SelectItem::UnnamedExpr(expression) = item else { panic!("an expression") };
            match context.type_of(expression).expect("typing succeeds") {
                OperandType::Known { type_name, typmod } => (type_name.name().to_owned(), typmod),
                other => panic!("`{expression}` types as {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        types,
        [
            ("int4".to_owned(), None),
            ("varchar".to_owned(), Some(16)),
            ("text".to_owned(), None),
            ("varchar".to_owned(), Some(7)),
            ("int8".to_owned(), None),
            ("varchar".to_owned(), Some(16)),
        ]
    );
}

/// In a PL/pgSQL body a bare name a function in `FROM` outputs reaches both
/// the function's column and the parameter of that name, which PostgreSQL
/// refuses as ambiguous, and a qualified one reaches the column alone.
#[test]
fn a_function_column_and_a_parameter_of_its_name_both_reach_a_bare_name() {
    let database = database();
    let function = database
        .functions()
        .find(|function| function.name() == "probe")
        .expect("probe is declared");
    let body = function.plpgsql_sites(&database).expect("body parses");
    let sites = body.sites().expect("sites resolve");
    let context = sites[0].context(IdentifierCase::Folded).expect("context builds");
    let mut reached = Vec::new();
    let _ = visit_expressions(&sites[0].sql(), |expression| {
        if matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            let column = context.reference(expression).expect("a node of the site");
            let variable = context.variable(expression).expect("a node of the site");
            reached.push((
                expression.to_string(),
                column.map(|(definition, binding)| {
                    (describe(&definition), binding.exposed().map(|(name, _)| name.to_owned()))
                }),
                variable.map(|variable| variable.name().to_owned()),
            ));
        }
        ControlFlow::<()>::Continue(())
    });
    let column = || Some(("held_notes INT".to_owned(), Some("note_id".to_owned())));
    assert_eq!(
        reached,
        [
            ("note_id".to_owned(), column(), Some("note_id".to_owned())),
            ("held.note_id".to_owned(), column(), None),
        ]
    );
}
