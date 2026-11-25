//! Whether control leaves each loop of a PL/pgSQL body once a path enters
//! it.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

/// The label and the answer of [`PlPgSqlLoop::ends`] of every loop of the
/// function `name` that `schema` creates, in body order.
fn loop_ends(schema: &str, name: &str) -> Vec<(Option<String>, Option<bool>)> {
    let database = ParserDB::parse::<PostgreSqlDialect>(schema).expect("schema parses");
    let function = database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    let body = function.plpgsql_sites(&database).expect("body parses");
    body.loops(IdentifierCase::Folded)
        .expect("body analyses")
        .iter()
        .map(|repeated| (repeated.repeated().label.clone(), repeated.ends()))
        .collect()
}

/// The answers for the function `name` running `body`, its loops unlabelled.
fn ends_of(name: &str, body: &str) -> Vec<Option<bool>> {
    let schema = format!("CREATE FUNCTION {name}() RETURNS void LANGUAGE plpgsql AS $$ {body} $$;");
    loop_ends(&schema, name).into_iter().map(|(_, ends)| ends).collect()
}

#[test]
fn a_loop_nothing_leaves_never_ends() {
    assert_eq!(ends_of("spin", "BEGIN LOOP PERFORM pg_sleep(1); END LOOP; END"), [Some(false)]);
    assert_eq!(
        ends_of(
            "poll",
            "DECLARE n integer := 0; BEGIN WHILE n < 10 LOOP EXIT WHEN n > 3; END LOOP; END",
        ),
        [Some(false)],
    );
}

#[test]
fn an_exit_a_false_condition_or_an_uncaught_raise_ends_a_loop() {
    assert_eq!(
        ends_of(
            "drain",
            "DECLARE r record; BEGIN WHILE true LOOP SELECT 1 INTO r; EXIT WHEN FOUND; END LOOP; END",
        ),
        [Some(true)],
    );
    assert_eq!(
        ends_of(
            "counted",
            "DECLARE i integer := 0; BEGIN WHILE i < 10 LOOP i := i + 1; END LOOP; END",
        ),
        [Some(true)],
    );
    assert_eq!(ends_of("fail", "BEGIN LOOP RAISE EXCEPTION 'stop'; END LOOP; END"), [Some(true)]);
}

#[test]
fn a_raise_a_handler_inside_the_loop_catches_does_not_end_it() {
    assert_eq!(
        ends_of(
            "retry",
            "BEGIN LOOP BEGIN RAISE EXCEPTION 'again'; EXCEPTION WHEN others THEN PERFORM 1; END; \
             END LOOP; END",
        ),
        [Some(false)],
    );
}

#[test]
fn a_loop_no_path_reaches_has_no_answer() {
    assert_eq!(ends_of("after", "BEGIN RETURN; LOOP PERFORM 1; END LOOP; END"), [None]);
}

#[test]
fn an_exit_naming_an_outer_loop_ends_both() {
    assert_eq!(
        loop_ends(
            "CREATE FUNCTION nested() RETURNS void LANGUAGE plpgsql AS $$
             BEGIN
                 <<outer>> LOOP
                     <<inner>> LOOP
                         EXIT outer;
                     END LOOP;
                 END LOOP;
             END $$;",
            "nested",
        ),
        [(Some("outer".into()), Some(true)), (Some("inner".into()), Some(true))],
    );
}

#[test]
fn every_loop_comes_in_body_order_wherever_it_nests() {
    let labels: Vec<Option<String>> = loop_ends(
        "CREATE FUNCTION spread(n integer) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
             IF n > 0 THEN
                 <<in_if>> LOOP EXIT; END LOOP;
             ELSIF n < 0 THEN
                 <<in_elsif>> LOOP EXIT; END LOOP;
             ELSE
                 <<in_else>> LOOP EXIT; END LOOP;
             END IF;
             CASE n
                 WHEN 1 THEN <<in_when>> LOOP EXIT; END LOOP;
                 ELSE <<in_case_else>> LOOP EXIT; END LOOP;
             END CASE;
             BEGIN
                 <<in_block>> FOR i IN 1 .. n LOOP
                     <<in_for>> WHILE i > 0 LOOP EXIT; END LOOP;
                 END LOOP;
             EXCEPTION WHEN others THEN
                 <<in_handler>> LOOP EXIT; END LOOP;
             END;
         END $$;",
        "spread",
    )
    .into_iter()
    .map(|(label, _)| label)
    .collect();
    assert_eq!(
        labels,
        [
            "in_if",
            "in_elsif",
            "in_else",
            "in_when",
            "in_case_else",
            "in_block",
            "in_for",
            "in_handler"
        ]
        .map(|label| Some(label.into())),
    );
}

#[test]
fn a_body_in_another_language_has_no_loops() {
    let schema = "CREATE FUNCTION one() RETURNS integer LANGUAGE sql AS 'SELECT 1';";
    assert_eq!(loop_ends(schema, "one"), []);
}
