//! The definitions that may reach each variable a dynamic-execution site of
//! a PL/pgSQL body reads, followed through its branches, loops, blocks,
//! handlers and control transfers.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails loudly on a broken premise"
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const SCHEMA: &str = "CREATE TABLE t (name text, id integer);";

/// The schema with `run(p text, q integer)` running `body`.
fn database(body: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE FUNCTION run(p text, q integer) RETURNS void LANGUAGE plpgsql AS $$ \
         {body} $$;"
    ))
    .expect("schema parses")
}

fn run(database: &ParserDB) -> &sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new("run", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// How a test spells a definition, the part it assigns before its source.
fn rendered(definition: &Definition<'_, '_, ParserDB>) -> String {
    let source = match definition.source() {
        DefinitionSource::Incoming => "incoming".to_owned(),
        DefinitionSource::Null => "null".to_owned(),
        DefinitionSource::False => "false".to_owned(),
        DefinitionSource::Value(value) => value.to_string(),
        DefinitionSource::QueryOutput { statement, output, rows } => {
            format!("{output:?} {rows:?} of {statement}")
        }
        DefinitionSource::DynamicOutput { command, output, rows, .. } => {
            format!("{output:?} {rows:?} of EXECUTE {command}")
        }
        DefinitionSource::CursorOutput { cursor, output } => {
            format!("{output:?} of FETCH {}", cursor.name())
        }
        DefinitionSource::ArrayElement { array, slice, output } => {
            format!("{output:?} slice {slice} of {array}")
        }
        DefinitionSource::LoopIndex { reverse, lower, upper, .. } => {
            format!("{}{lower} .. {upper}", if *reverse { "reverse " } else { "" })
        }
        DefinitionSource::CursorName => "cursor name".to_owned(),
        DefinitionSource::PortalName => "portal name".to_owned(),
        DefinitionSource::Diagnostics(item) => format!("{item:?}"),
        DefinitionSource::CallOutput { call, argument } => format!("argument {argument} of {call}"),
        DefinitionSource::Found(found) => format!("found {found:?}"),
        DefinitionSource::Unknown => "unknown".to_owned(),
    };
    let part: String = definition
        .part()
        .iter()
        .map(|step| {
            match step {
                TargetStep::Field(field) => format!(".{field}"),
                TargetStep::Index(index) => format!("[{index}]"),
                TargetStep::Slice { .. } => "[:]".to_owned(),
            }
        })
        .collect();
    if part.is_empty() { source } else { format!("{part} {source}") }
}

/// Each variable a read reaches, by name, with its definitions spelled.
fn spelled(reads: &[VariableRead<'_, '_, ParserDB>]) -> Vec<(String, Vec<String>)> {
    reads
        .iter()
        .map(|read| {
            let definitions = match read.reaching() {
                Reaching::Definitions(definitions) => definitions.iter().map(rendered).collect(),
                Reaching::Unreachable => vec!["unreachable".to_owned()],
            };
            (read.variable().name().to_owned(), definitions)
        })
        .collect()
}

/// The variables each site of the function `name` reads, with their
/// definitions spelled.
fn reaching_in(database: &ParserDB, name: &str) -> Vec<Vec<(String, Vec<String>)>> {
    let function = database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    let sites = function.dynamic_execution_sites(database).unwrap();
    sites
        .sites()
        .unwrap()
        .iter()
        .map(|site| spelled(&site.reads(IdentifierCase::Folded).unwrap()))
        .collect()
}

/// The variables each site of `run` running `body` reads, with their
/// definitions spelled.
fn reaching(body: &str) -> Vec<Vec<(String, Vec<String>)>> {
    reaching_in(&database(body), "run")
}

fn read(variable: &str, definitions: &[&str]) -> (String, Vec<String>) {
    (variable.to_owned(), definitions.iter().map(|&definition| definition.to_owned()).collect())
}

/// An assignment completing on every path replaces the default, and a
/// parameter holds the argument passed.
#[test]
fn an_assignment_replaces_what_the_variable_held() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN s := 'SELECT ' || p; EXECUTE s USING q; END"
        ),
        [vec![read("s", &["'SELECT ' || p"]), read("q", &["incoming"])]]
    );
}

/// A statement without `INTO` that is no `CALL` assigns no variable, so
/// what a variable held before it reaches the statements after it.
#[test]
fn a_statement_without_into_keeps_what_variables_hold() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN UPDATE t SET name = s WHERE id = q; EXECUTE s; END"
        ),
        [vec![read("s", &["'SELECT 1'"])]]
    );
}

/// A branch that does not assign keeps what the variable held before the
/// branch, beside what every assigning branch leaves.
#[test]
fn branches_join_what_each_path_leaves() {
    assert_eq!(
        reaching(
            "DECLARE s text;
             BEGIN
              IF q > 1 THEN s := 'SELECT 1';
              ELSIF q > 0 THEN PERFORM 1;
              ELSE s := format('SELECT %I', p);
              END IF;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["null", "'SELECT 1'", "format('SELECT %I', p)"])]]
    );
}

/// A definition written later in a loop reaches an earlier read through the
/// back edge, and a loop that runs no iteration keeps the default.
#[test]
fn a_loop_carries_its_later_writes_back_to_its_head() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              FOR iteration IN 1..2 LOOP
               EXECUTE s;
               s := 'SELECT ' || p;
              END LOOP;
              EXECUTE s;
             END"
        ),
        [
            vec![read("s", &["'SELECT 1'", "'SELECT ' || p"])],
            vec![read("s", &["'SELECT 1'", "'SELECT ' || p"])],
        ]
    );
}

/// An assignment in a nested block reaches the outer variable after the
/// block ends, and its value keeps the scope it was written in, where a
/// variable of the ended block still types.
#[test]
fn a_nested_assignment_keeps_the_scope_it_was_written_in() {
    let database = database(
        "DECLARE s text;
         BEGIN
          DECLARE local_input text := p;
          BEGIN
           s := 'SELECT ' || local_input;
          END;
          EXECUTE s;
         END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded).unwrap();
    let [command] = reads.as_slice() else { panic!("s is read") };
    let Reaching::Definitions(definitions) = command.reaching() else { panic!("s is reached") };
    let [assigned] = definitions.as_slice() else { panic!("one assignment reaches") };
    let DefinitionSource::Value(value) = assigned.source() else { panic!("a value") };
    let scope = assigned.scope().expect("an assigned value has a scope");
    let scope = ExpressionScope::PlPgSql(&scope);
    let context =
        ExpressionContext::for_expression(value, &scope, &database, IdentifierCase::Folded)
            .unwrap();
    let text = TargetName::new("text", false).with_schema("pg_catalog", false);
    assert_eq!(
        context.type_of(value).unwrap(),
        OperandType::Known { type_name: text, typmod: None }
    );
    let inputs = assigned.reads(IdentifierCase::Folded).unwrap();
    assert_eq!(spelled(&inputs), [read("local_input", &["p"])]);
    let Reaching::Definitions(defaults) = inputs[0].reaching() else { panic!("reached") };
    assert_eq!(
        spelled(&defaults[0].reads(IdentifierCase::Folded).unwrap()),
        [read("p", &["incoming"])]
    );
}

/// A declaration of the same name in a nested block is another variable,
/// whose writes never reach the outer one.
#[test]
fn a_hiding_declaration_is_another_variable() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              DECLARE s text := 'SELECT 2';
              BEGIN s := 'SELECT 3'; END;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["'SELECT 1'"])]]
    );
}

/// A handler starts from what the variables held wherever its block's
/// statements raised, so an assignment that raised leaves the earlier
/// value.
#[test]
fn a_handler_sees_the_values_at_every_raise() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              BEGIN
               s := (1 / q)::text;
               PERFORM 1 / q;
              EXCEPTION WHEN OTHERS THEN
               EXECUTE s;
              END;
             END"
        ),
        [vec![read("s", &["'SELECT 1'", "(1 / q)::TEXT"])]]
    );
}

/// A field assignment changes part of the variable, so the definitions of
/// the whole still reach.
#[test]
fn a_field_write_keeps_the_rest_of_the_variable() {
    assert_eq!(
        reaching(
            "DECLARE r record;
             BEGIN
              SELECT name INTO r FROM t;
              r.name := p;
              EXECUTE r.name;
             END"
        ),
        [vec![read("r", &["Row First { strict: false } of SELECT name FROM t", ".name p"])]]
    );
}

/// `EXIT` and `RETURN` leave, so what they skip never reaches, and a read
/// after `RETURN` is unreachable.
#[test]
fn control_transfers_skip_what_they_leave() {
    assert_eq!(
        reaching(
            "DECLARE s text;
             BEGIN
              LOOP s := 'SELECT 1'; EXIT; END LOOP;
              <<inner>> BEGIN
               s := 'SELECT 2';
               EXIT inner WHEN q > 0;
               s := 'SELECT 3';
              END;
              EXECUTE s;
              RETURN;
              EXECUTE p;
             END"
        ),
        [vec![read("s", &["'SELECT 2'", "'SELECT 3'"])], vec![read("p", &["unreachable"])]]
    );
}

/// `SELECT ... INTO` assigns a query output, with its `STRICT` and the
/// output position, and an `EXECUTE ... INTO` the command's result. The
/// definition of a query output reads what the query reads.
#[test]
fn into_assigns_query_and_command_outputs() {
    assert_eq!(
        reaching(
            "DECLARE s text; n integer;
             BEGIN
              SELECT name, id INTO STRICT s, n FROM t WHERE name = p;
              EXECUTE s USING n;
              EXECUTE 'SELECT ' || p INTO s;
              EXECUTE s;
             END"
        ),
        [
            vec![
                read(
                    "s",
                    &["Ordinal(0) First { strict: true } of SELECT name, id FROM t WHERE name = p"]
                ),
                read(
                    "n",
                    &["Ordinal(1) First { strict: true } of SELECT name, id FROM t WHERE name = p"]
                ),
            ],
            vec![read("p", &["incoming"])],
            vec![read("s", &["Ordinal(0) First { strict: false } of EXECUTE 'SELECT ' || p"])],
        ]
    );
    let database = database(
        "DECLARE s text;
         BEGIN SELECT name INTO s FROM t WHERE name = p; EXECUTE s; END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded).unwrap();
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    assert_eq!(
        spelled(&definitions[0].reads(IdentifierCase::Folded).unwrap()),
        [read("p", &["incoming"])]
    );
}

/// A `STRICT` query that raises assigns nothing, so its handler sees the
/// earlier value beside the query output.
#[test]
fn a_strict_query_that_raises_leaves_the_earlier_value() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              BEGIN
               SELECT name INTO STRICT s FROM t;
               PERFORM 1 / q;
              EXCEPTION WHEN OTHERS THEN
               EXECUTE s;
              END;
             END"
        ),
        [vec![read(
            "s",
            &["'SELECT 1'", "Ordinal(0) First { strict: true } of SELECT name FROM t"]
        )]]
    );
}

/// A loop over a query assigns its target each row, and the empty row, a
/// null, when the query returns none. An empty `FOREACH` keeps what the
/// target held.
#[test]
fn loops_over_rows_and_arrays_assign_their_targets() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1'; e text := 'SELECT 2';
             BEGIN
              FOR s IN SELECT name FROM t LOOP END LOOP;
              FOREACH e IN ARRAY ARRAY[p] LOOP END LOOP;
              EXECUTE s USING e;
             END"
        ),
        [vec![
            read(
                "s",
                &[
                    "Ordinal(0) Empty of SELECT name FROM t",
                    "Ordinal(0) Each of SELECT name FROM t"
                ]
            ),
            read("e", &["'SELECT 2'", "Row slice 0 of ARRAY[p]"]),
        ]]
    );
}

/// A loop over a command's rows assigns each row, and the empty row when
/// the command returns none.
#[test]
fn a_loop_over_a_command_assigns_each_row_or_the_empty_row() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              FOR s IN EXECUTE 'SELECT ' || p LOOP EXECUTE s; END LOOP;
              EXECUTE s;
             END"
        ),
        [
            vec![read("p", &["incoming"])],
            vec![read("s", &["Ordinal(0) Each of EXECUTE 'SELECT ' || p"])],
            vec![read(
                "s",
                &[
                    "Ordinal(0) Empty of EXECUTE 'SELECT ' || p",
                    "Ordinal(0) Each of EXECUTE 'SELECT ' || p",
                ],
            )],
        ]
    );
}

/// A loop over a query whose body always returns leaves the loop only
/// through the query returning no row, so only the empty row follows it.
#[test]
fn a_query_loop_that_always_returns_leaves_only_the_empty_row() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              FOR s IN SELECT name FROM t LOOP RETURN; END LOOP;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["Ordinal(0) Empty of SELECT name FROM t"])]]
    );
}

/// `FETCH`, `OPEN`, `GET DIAGNOSTICS` and `CALL` assign what they read,
/// and a `CALL` adds what the procedure may return beside the earlier
/// value.
#[test]
fn every_other_write_assigns_its_targets() {
    assert_eq!(
        reaching(
            "DECLARE c CURSOR FOR SELECT name FROM t; k refcursor; s text; n integer;
              o text := 'SELECT 1';
             BEGIN
              OPEN c;
              FETCH c INTO s;
              GET DIAGNOSTICS n = ROW_COUNT;
              CALL fill(o, 'x', run.p, missing);
              OPEN k FOR SELECT name FROM t;
              OPEN k FOR EXECUTE 'SELECT 2';
              FOR i IN 1..q LOOP EXECUTE s USING n, c, o, i, k, p; END LOOP;
             END"
        ),
        [
            vec![],
            vec![
                read("s", &["Ordinal(0) of FETCH c"]),
                read("n", &["RowCount"]),
                read("c", &["cursor name", "portal name"]),
                read("o", &["'SELECT 1'", "argument 0 of CALL fill(o, 'x', run.p, missing)"]),
                read("i", &["1 .. q"]),
                read("k", &["null", "portal name", "portal name"]),
                read("p", &["incoming", "argument 2 of CALL fill(o, 'x', run.p, missing)"]),
            ],
        ]
    );
}

/// An output parameter starts as a null and an input parameter as the
/// argument, which a branch assigning it joins.
#[test]
fn parameters_start_as_their_arguments_or_nulls() {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE FUNCTION outputs(p text, OUT r text) LANGUAGE plpgsql AS $$
         BEGIN IF p IS NULL THEN p := 'SELECT 1'; END IF; EXECUTE r USING p; END $$;"
    ))
    .expect("schema parses");
    let outputs = database
        .function_by_target(TargetName::new("outputs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    let body = outputs.dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    assert_eq!(
        spelled(&sites[0].reads(IdentifierCase::Folded).unwrap()),
        [read("r", &["null"]), read("p", &["incoming", "'SELECT 1'"])]
    );
}

/// A definition is the same handle at every read it reaches, through a
/// copy of the site too, and a definition the body did not write has no
/// scope and reads nothing.
#[test]
fn a_definition_is_one_handle_for_every_read() {
    let database = database(
        "DECLARE s text := 'SELECT 1'; n text;
         BEGIN EXECUTE s; EXECUTE s USING p, n; END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let copy = sites[1].clone();
    let first = sites[0].reads(IdentifierCase::Folded).unwrap();
    let second = copy.reads(IdentifierCase::Folded).unwrap();
    let Reaching::Definitions(at_first) = first[0].reaching() else { panic!("s is reached") };
    let Reaching::Definitions(at_second) = second[0].reaching() else { panic!("s is reached") };
    assert_eq!(at_first, at_second);
    assert_eq!(at_first[0].variable().name(), "s");
    let Reaching::Definitions(incoming) = second[1].reaching() else { panic!("p is reached") };
    assert_ne!(incoming[0].id(), at_first[0].id());
    assert_eq!(incoming[0].variable().name(), "p");
    assert!(incoming[0].scope().is_none());
    assert!(incoming[0].reads(IdentifierCase::Folded).unwrap().is_empty());
    let Reaching::Definitions(null) = second[2].reaching() else { panic!("n is reached") };
    assert!(null[0].scope().is_none());
    assert!(null[0].reads(IdentifierCase::Folded).unwrap().is_empty());
}

/// A catalog call running SQL text reads the variables its text argument
/// names, and a column of the query holding the call hides a variable of
/// its name.
#[test]
fn a_catalog_call_reads_what_its_text_argument_names() {
    assert_eq!(
        reaching(
            "DECLARE s text; name text := 'SELECT 1';
             BEGIN
              s := 'SELECT ' || p;
              PERFORM query_to_xml(s, true, false, '');
              PERFORM query_to_xml(name, true, false, '') FROM t;
             END"
        ),
        [vec![read("s", &["'SELECT ' || p"])], vec![]]
    );
}

/// A read through an alias reads the variable the alias names.
#[test]
fn an_alias_reads_the_variable_it_names() {
    assert_eq!(
        reaching(
            "DECLARE s text; a ALIAS FOR p;
             BEGIN s := 'SELECT ' || a; EXECUTE s USING a; END"
        ),
        [vec![read("s", &["'SELECT ' || a"]), read("p", &["incoming"])]]
    );
}

/// `CONTINUE` carries what it leaves back to the loop head, a `WHILE`
/// loop may run no iteration, and `CASE` joins its branches.
#[test]
fn continue_while_and_case_join_their_paths() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              WHILE q > 0 LOOP
               EXECUTE s;
               IF q > 1 THEN s := 'SELECT 2'; CONTINUE; END IF;
               CASE q WHEN 0 THEN s := 'SELECT 3'; ELSE s := 'SELECT 4'; END CASE;
              END LOOP;
              EXECUTE s;
             END"
        ),
        [
            vec![read("s", &["'SELECT 1'", "'SELECT 2'", "'SELECT 3'", "'SELECT 4'"])],
            vec![read("s", &["'SELECT 1'", "'SELECT 2'", "'SELECT 3'", "'SELECT 4'"])],
        ]
    );
}

/// A `CASE` without an `ELSE` raises `case_not_found` where no `WHEN`
/// holds, so a definition no `WHEN` matched does not fall through it.
#[test]
fn a_case_without_an_else_raises_where_no_when_holds() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              CASE q WHEN 0 THEN s := 'SELECT 2'; END CASE;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["'SELECT 2'"])]]
    );
}

/// The `case_not_found` a `CASE` without an `ELSE` raises is still a raise,
/// so a handler of it sees what reached the `CASE` where no `WHEN` matched,
/// and not what a `WHEN` that completed wrote.
#[test]
fn a_handler_of_the_missing_else_sees_every_state_that_raised() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              BEGIN
               CASE q WHEN 0 THEN s := 'SELECT 2'; END CASE;
              EXCEPTION WHEN OTHERS THEN
               EXECUTE s;
              END;
             END"
        ),
        [vec![read("s", &["'SELECT 1'"])]]
    );
}

/// `EXIT` naming an outer loop leaves both loops, so the statements after
/// the inner loop never run.
#[test]
fn a_labelled_exit_leaves_every_loop_up_to_its_label() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              <<outer>> LOOP
               LOOP s := 'SELECT 2'; EXIT outer; END LOOP;
               s := 'SELECT 3';
              END LOOP;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["'SELECT 2'"])]]
    );
}

/// A loop over a cursor holding null gives it the name of the portal it
/// opens, and leaving the loop, by `EXIT` too, gives the null back.
#[test]
fn a_cursor_loop_names_its_portal_while_it_runs() {
    assert_eq!(
        reaching(
            "DECLARE c CURSOR FOR SELECT name FROM t;
             BEGIN
              c := NULL;
              FOR r IN c LOOP
               EXECUTE 'MOVE NEXT IN ' || quote_ident(c);
               EXIT WHEN q > 0;
              END LOOP;
              EXECUTE 'CLOSE ' || quote_ident(c);
             END"
        ),
        [vec![read("c", &["NULL", "portal name"])], vec![read("c", &["NULL"])]]
    );
}

/// `EXIT` leaving a cursor loop for an enclosing one gives the cursor its
/// null back, as leaving the cursor loop itself does.
#[test]
fn a_labelled_exit_through_a_cursor_loop_gives_its_null_back() {
    assert_eq!(
        reaching(
            "DECLARE c CURSOR FOR SELECT name FROM t;
             BEGIN
              c := NULL;
              <<outer>> LOOP
               FOR r IN c LOOP EXIT outer; END LOOP;
              END LOOP;
              EXECUTE 'CLOSE ' || quote_ident(c);
             END"
        ),
        [vec![read("c", &["NULL"])]]
    );
}

/// A declaration in a loop runs its default on every iteration, so the
/// default reads what the previous iteration left.
#[test]
fn a_declaration_in_a_loop_reads_the_previous_iteration() {
    let database = database(
        "DECLARE s text := 'SELECT 1';
         BEGIN
          FOR i IN 1..2 LOOP
           DECLARE d text := s || ' UNION SELECT 2';
           BEGIN s := d; END;
          END LOOP;
          EXECUTE s;
         END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded).unwrap();
    assert_eq!(spelled(&reads), [read("s", &["'SELECT 1'", "d"])]);
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    let inputs = definitions[1].reads(IdentifierCase::Folded).unwrap();
    assert_eq!(spelled(&inputs), [read("d", &["s || ' UNION SELECT 2'"])]);
    let Reaching::Definitions(defaults) = inputs[0].reaching() else { panic!("d is reached") };
    assert_eq!(
        spelled(&defaults[0].reads(IdentifierCase::Folded).unwrap()),
        [read("s", &["'SELECT 1'", "d"])]
    );
}

/// An element write keeps the rest of the array, `FOUND` holds the `false`
/// each call starts with until a statement sets it, and nothing after
/// `RAISE EXCEPTION` runs.
#[test]
fn elements_unknowns_and_raises() {
    assert_eq!(
        reaching(
            "DECLARE a text[] := ARRAY['SELECT 1'];
             BEGIN
              a[q] := p;
              a[1:q] := ARRAY[p];
              a[1].f := p;
              EXECUTE a[1] USING FOUND;
              RAISE EXCEPTION 'stop';
              EXECUTE p;
             END"
        ),
        [
            vec![
                read("a", &["ARRAY['SELECT 1']", "[q] p", "[:] ARRAY[p]", "[1].f p"]),
                read("found", &["false"]),
            ],
            vec![read("p", &["unreachable"])],
        ]
    );
}

/// A statement processing rows and a loop ending both set `FOUND`, and a
/// path setting neither keeps the `false` the call started with.
#[test]
fn found_holds_what_the_last_statement_setting_it_found() {
    assert_eq!(
        reaching(
            "BEGIN
              IF q > 0 THEN PERFORM 1; END IF;
              EXECUTE 'SELECT 1' USING FOUND;
              FOR i IN 1..q LOOP END LOOP;
              EXECUTE 'SELECT 2' USING FOUND;
             END"
        ),
        [vec![read("found", &["false", "found Rows"])], vec![read("found", &["found Iterated"])],]
    );
}

/// The count of a `FETCH` or `MOVE` and the value of a `RAISE` option run
/// where the statement stands, after the writes before it.
#[test]
fn cursor_counts_and_raise_options_read_the_definitions_before_them() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1'; c refcursor; n integer;
             BEGIN
              OPEN c SCROLL FOR SELECT id FROM t;
              s := p;
              FETCH ABSOLUTE length(query_to_xml(s, false, false, '')::text) FROM c INTO n;
              s := s || ' UNION SELECT 2';
              MOVE RELATIVE length(query_to_xml(s, false, false, '')::text) FROM c;
              RAISE NOTICE 'moved' USING DETAIL = query_to_xml(s || q, false, false, '')::text;
             END"
        ),
        [
            vec![read("s", &["p"])],
            vec![read("s", &["s || ' UNION SELECT 2'"])],
            vec![read("s", &["s || ' UNION SELECT 2'"]), read("q", &["incoming"])],
        ]
    );
}

/// A function runs in its caller's transaction, so `COMMIT` and `ROLLBACK`
/// in its body raise `invalid_transaction_termination` and nothing after
/// them runs, while a handler of their block takes what reached them.
#[test]
fn commit_and_rollback_raise_in_a_function() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              BEGIN
               IF q > 0 THEN COMMIT; ELSE ROLLBACK AND CHAIN; END IF;
               s := p;
              EXCEPTION WHEN invalid_transaction_termination THEN
               EXECUTE s;
              END;
              EXECUTE s;
             END"
        ),
        [vec![read("s", &["'SELECT 1'"])], vec![read("s", &["'SELECT 1'"])]]
    );
}

/// The variables each site of the procedure `run(p text, q integer)`
/// running `body` reads.
fn reaching_in_procedure(body: &str) -> Vec<Vec<(String, Vec<String>)>> {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE PROCEDURE run(p text, q integer) LANGUAGE plpgsql AS $$ {body} $$;"
    ))
    .expect("schema parses");
    reaching_in(&database, "run")
}

/// A procedure a `CALL` outside a transaction block runs goes on after
/// `COMMIT` and `ROLLBACK`, in a query loop and in a handler too.
#[test]
fn commit_and_rollback_run_on_in_a_procedure() {
    assert_eq!(
        reaching_in_procedure(
            "DECLARE s text := 'SELECT 1'; r record;
             BEGIN
              COMMIT;
              s := 'SELECT ' || p;
              EXECUTE s;
              FOR r IN SELECT id FROM t LOOP ROLLBACK AND CHAIN; s := s || r.id; END LOOP;
              EXECUTE s;
              BEGIN
               PERFORM 1 / q;
              EXCEPTION WHEN division_by_zero THEN
               COMMIT;
               s := p;
              END;
              EXECUTE s;
             END"
        ),
        [
            vec![read("s", &["'SELECT ' || p"])],
            vec![read("s", &["'SELECT ' || p", "s || r.id"])],
            vec![read("s", &["'SELECT ' || p", "s || r.id", "p"])],
        ]
    );
}

/// A handler in force makes `COMMIT` and `ROLLBACK` in a procedure raise
/// `invalid_transaction_termination`, whichever enclosing block holds it.
#[test]
fn commit_and_rollback_raise_under_a_handler_in_a_procedure() {
    assert_eq!(
        reaching_in_procedure(
            "DECLARE s text := 'SELECT 1';
             BEGIN
              BEGIN
               COMMIT;
               s := p;
              EXCEPTION WHEN invalid_transaction_termination THEN
               EXECUTE s;
              END;
              BEGIN
               BEGIN ROLLBACK; s := p; END;
              EXCEPTION WHEN invalid_transaction_termination THEN
               EXECUTE s;
              END;
              EXECUTE s;
             END"
        ),
        [
            vec![read("s", &["'SELECT 1'"])],
            vec![read("s", &["'SELECT 1'"])],
            vec![read("s", &["'SELECT 1'"])],
        ]
    );
}

/// `REVERSE` counts down from the first bound written, so the index of a
/// reversed loop is told apart from one counting up between the same bounds.
#[test]
fn a_reversed_range_starts_at_its_first_bound() {
    assert_eq!(
        reaching(
            "BEGIN
              FOR i IN REVERSE q .. 1 LOOP EXECUTE 'SELECT ' || i; END LOOP;
              FOR i IN q .. 1 LOOP EXECUTE 'SELECT ' || i; END LOOP;
             END"
        ),
        [vec![read("i", &["reverse q .. 1"])], vec![read("i", &["q .. 1"])]]
    );
}

/// A notice raises nothing, a definition reads each variable once however
/// often its value names it, and a write after `RETURN` reaches no read.
#[test]
fn notices_continue_and_returns_end_the_path() {
    let database = database(
        "DECLARE s text := 'SELECT 1';
         BEGIN
          RAISE NOTICE 'running %', p;
          CALL fill(t.*);
          s := p || p;
          EXECUTE s;
          RETURN;
          s := q;
          EXECUTE s;
         END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads: Vec<_> =
        sites.iter().map(|site| spelled(&site.reads(IdentifierCase::Folded).unwrap())).collect();
    assert_eq!(reads, [vec![read("s", &["p || p"])], vec![read("s", &["unreachable"])]]);
    let reached = sites[0].reads(IdentifierCase::Folded).unwrap();
    let Reaching::Definitions(definitions) = reached[0].reaching() else { panic!("s is reached") };
    assert_eq!(
        spelled(&definitions[0].reads(IdentifierCase::Folded).unwrap()),
        [read("p", &["incoming"])]
    );
}

/// A positional name reads the parameter it stands for, at the site and in
/// a definition's value.
#[test]
fn a_positional_name_reads_its_parameter() {
    let database = database("DECLARE s text; BEGIN s := 'SELECT ' || $1; EXECUTE s USING $2; END");
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded).unwrap();
    assert_eq!(spelled(&reads), [read("s", &["'SELECT ' || $1"]), read("q", &["incoming"])]);
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    assert_eq!(
        spelled(&definitions[0].reads(IdentifierCase::Folded).unwrap()),
        [read("p", &["incoming"])]
    );
}

/// A procedure may return a value to a variable passed in parentheses, under
/// a cast to its own type or by position, as PostgreSQL 18 writes an `INOUT`
/// result back through each.
#[test]
fn a_call_may_write_a_parenthesised_cast_or_positional_argument() {
    assert_eq!(
        reaching(
            "DECLARE s text := 'SELECT 1'; o text := 'SELECT 2';
             BEGIN CALL fill((s), o::text, $1); EXECUTE s USING o, p; END"
        ),
        [vec![
            read("s", &["'SELECT 1'", "argument 0 of CALL fill((s), o::TEXT, $1)"]),
            read("o", &["'SELECT 2'", "argument 1 of CALL fill((s), o::TEXT, $1)"]),
            read("p", &["incoming", "argument 2 of CALL fill((s), o::TEXT, $1)"]),
        ]]
    );
}

/// A procedure returns no value to an argument computed from an expression,
/// nor to a call written without its argument list.
#[test]
fn a_call_writes_no_computed_argument() {
    assert_eq!(
        reaching("BEGIN CALL fill(p || p, q); CALL fill; EXECUTE p USING q; END"),
        [vec![
            read("p", &["incoming"]),
            read("q", &["incoming", "argument 1 of CALL fill(p || p, q)"]),
        ]]
    );
}

/// The schema with the routines `routines` declares and `run(p text, q
/// integer)` running `body`.
fn database_with(routines: &str, body: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} {routines} CREATE FUNCTION run(p text, q integer) RETURNS void LANGUAGE \
         plpgsql AS $$ {body} $$;"
    ))
    .expect("schema parses")
}

/// A `CALL` reaching one procedure surely writes the variables it passes at
/// the procedure's `INOUT` and `OUT` parameters, matched by position or by
/// name, and leaves the variables it passes elsewhere as they were. The
/// second call names its schema since the first may change the path.
#[test]
fn a_call_of_one_procedure_writes_its_output_parameters() {
    let database = database_with(
        "CREATE PROCEDURE fill(a text, INOUT b text, OUT c text) LANGUAGE plpgsql AS $$ BEGIN END $$;",
        "DECLARE s text := 'SELECT 1'; o text := 'SELECT 2'; r text := 'SELECT 3';
         BEGIN
          CALL fill(s, o, r); EXECUTE s USING o, r;
          CALL public.fill(c => s, a => r, b => o); EXECUTE s USING o, r;
         END",
    );
    assert_eq!(
        reaching_in(&database, "run"),
        [
            vec![
                read("s", &["'SELECT 1'"]),
                read("o", &["argument 1 of CALL fill(s, o, r)"]),
                read("r", &["argument 2 of CALL fill(s, o, r)"]),
            ],
            vec![
                read("s", &["argument 0 of CALL public.fill(c => s, a => r, b => o)"]),
                read("o", &["argument 2 of CALL public.fill(c => s, a => r, b => o)"]),
                read("r", &["argument 2 of CALL fill(s, o, r)"]),
            ],
        ]
    );
}

/// A `CALL` reaching no procedure, or several the argument count cannot
/// tell apart, may write every variable it passes.
#[test]
fn a_call_of_no_single_procedure_may_write_every_variable() {
    for routines in [
        "CREATE FUNCTION fill(a text, b text) RETURNS void LANGUAGE sql AS 'SELECT';",
        "CREATE PROCEDURE fill(a text, INOUT b text) LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE fill(a integer, INOUT b text) LANGUAGE sql AS 'SELECT 1';",
        "CREATE PROCEDURE fill(a text) LANGUAGE sql AS 'SELECT 1';",
    ] {
        let database = database_with(
            routines,
            "DECLARE s text := 'SELECT 1'; BEGIN CALL fill(p, s); EXECUTE s USING p; END",
        );
        assert_eq!(
            reaching_in(&database, "run"),
            [vec![
                read("s", &["'SELECT 1'", "argument 1 of CALL fill(p, s)"]),
                read("p", &["incoming", "argument 0 of CALL fill(p, s)"]),
            ]],
            "{routines}"
        );
    }
}

/// A `CALL` whose path walks a schema the model does not hold may reach a
/// procedure there taking only `IN` arguments, so it may write every variable
/// it passes and leaves the earlier definitions in place.
#[test]
fn a_call_through_an_unheld_schema_may_write_every_variable() {
    let database = database_with(
        "CREATE PROCEDURE fill(a text, INOUT b text) LANGUAGE sql AS 'SELECT 1';",
        "DECLARE s text := 'SELECT 1';
         BEGIN SET search_path = ext, public; CALL fill(p, s); EXECUTE s USING p; END",
    );
    assert_eq!(
        reaching_in(&database, "run"),
        [vec![
            read("s", &["'SELECT 1'", "argument 1 of CALL fill(p, s)"]),
            read("p", &["incoming", "argument 0 of CALL fill(p, s)"]),
        ]]
    );
}

/// A `CALL` whose arguments the procedure's parameters cannot settle, through
/// a parameter given twice or a name that is no identifier, may write every
/// variable it passes.
#[test]
fn a_call_its_parameters_cannot_settle_may_write_every_variable() {
    for (routine, call, s_argument, p_argument) in [
        (
            "CREATE PROCEDURE fill(a text, INOUT b text) LANGUAGE sql AS 'SELECT 1';",
            "CALL fill(s, a => p)",
            0,
            1,
        ),
        (
            "CREATE PROCEDURE fill(a text, INOUT b text) LANGUAGE sql AS 'SELECT 1';",
            "CALL fill(p, 'b' => s)",
            1,
            0,
        ),
    ] {
        let database = database_with(
            routine,
            &format!("DECLARE s text := 'SELECT 1'; BEGIN {call}; EXECUTE s USING p; END"),
        );
        let s_written = format!("argument {s_argument} of {call}");
        let p_written = format!("argument {p_argument} of {call}");
        assert_eq!(
            reaching_in(&database, "run"),
            [vec![
                read("s", &["'SELECT 1'", s_written.as_str()]),
                read("p", &["incoming", p_written.as_str()]),
            ]],
            "{call}"
        );
    }
}

/// An unqualified `CALL` after another `CALL`, which may have changed the
/// path, may write every variable it passes.
#[test]
fn a_call_after_a_call_may_write_every_variable() {
    let database = database_with(
        "CREATE PROCEDURE fill(a text, INOUT b text) LANGUAGE sql AS 'SELECT 1';",
        "DECLARE s text := 'SELECT 1';
         BEGIN CALL public.fill(p, s); CALL fill(p, s); EXECUTE s USING p; END",
    );
    assert_eq!(
        reaching_in(&database, "run"),
        [vec![
            read("s", &["argument 1 of CALL public.fill(p, s)", "argument 1 of CALL fill(p, s)"]),
            read("p", &["incoming", "argument 0 of CALL fill(p, s)"]),
        ]]
    );
}

/// The variables a definition reads come with the lookup error a query of
/// its source runs into.
#[test]
fn a_definition_reading_an_unresolvable_query_returns_its_error() {
    let database = database(
        "DECLARE s text;
         BEGIN s := (SELECT name FROM t AS a, t AS b); EXECUTE s; END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded).unwrap();
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    assert!(matches!(
        definitions[0].reads(IdentifierCase::Folded),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
}

/// A write to a name the body declares nowhere leaves the reads of its
/// sites unresolvable, naming the target.
#[test]
fn a_write_to_an_undeclared_name_is_unresolvable() {
    for body in [
        "BEGIN missing := 1; EXECUTE p; END",
        "BEGIN SELECT 1 INTO missing; EXECUTE p; END",
        "BEGIN GET DIAGNOSTICS missing = ROW_COUNT; EXECUTE p; END",
    ] {
        let database = database(body);
        let sites = run(&database).dynamic_execution_sites(&database).unwrap();
        let sites = sites.sites().unwrap();
        assert!(
            matches!(
                sites[0].reads(IdentifierCase::Folded),
                Err(LookupError::FunctionBodyUnresolvable { function, statement })
                    if function == "run" && statement == "missing"
            ),
            "{body}"
        );
    }
}

/// `GET DIAGNOSTICS` and `GET STACKED DIAGNOSTICS` assign each target the
/// item it names.
#[test]
fn diagnostics_assign_the_items_they_name() {
    assert_eq!(
        reaching(
            "DECLARE a text; b text; c text; d text; e text; f text; g text; h text; i text;
              j text; k text; l text;
             BEGIN
              GET DIAGNOSTICS a = PG_ROUTINE_OID, b = PG_CONTEXT;
              BEGIN
               PERFORM 1 / q;
              EXCEPTION WHEN OTHERS THEN
               GET STACKED DIAGNOSTICS c = RETURNED_SQLSTATE, d = COLUMN_NAME,
                e = CONSTRAINT_NAME, f = PG_DATATYPE_NAME, g = MESSAGE_TEXT, h = TABLE_NAME,
                i = SCHEMA_NAME, j = PG_EXCEPTION_DETAIL, k = PG_EXCEPTION_HINT,
                l = PG_EXCEPTION_CONTEXT;
               EXECUTE a USING b, c, d, e, f, g, h, i, j, k, l;
              END;
             END"
        ),
        [vec![
            read("a", &["RoutineOid"]),
            read("b", &["Context"]),
            read("c", &["ReturnedSqlstate"]),
            read("d", &["ColumnName"]),
            read("e", &["ConstraintName"]),
            read("f", &["DatatypeName"]),
            read("g", &["MessageText"]),
            read("h", &["TableName"]),
            read("i", &["SchemaName"]),
            read("j", &["ExceptionDetail"]),
            read("k", &["ExceptionHint"]),
            read("l", &["ExceptionContext"]),
        ]]
    );
}

/// A definition whose value comes from no SQL of the body, a diagnostics
/// item, a declaration without a default or a `FETCH`, reads nothing.
#[test]
fn a_definition_without_sql_reads_nothing() {
    let database = database(
        "DECLARE a text; b text; c CURSOR FOR SELECT name FROM t;
         BEGIN
          GET DIAGNOSTICS a = PG_CONTEXT;
          IF q > 0 THEN OPEN c; FETCH c INTO b; END IF;
          EXECUTE a USING b;
         END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    for read in sites[0].reads(IdentifierCase::Folded).unwrap() {
        let Reaching::Definitions(definitions) = read.reaching() else { panic!("reached") };
        for definition in definitions {
            let reads = definition.reads(IdentifierCase::Folded).unwrap();
            assert!(reads.is_empty(), "{}", rendered(definition));
        }
    }
}

/// A lone row target takes the whole row, a row-typed parameter and a
/// trigger's `NEW` included, while a lone scalar target takes the first
/// output.
#[test]
fn a_lone_row_target_takes_the_whole_row() {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE FUNCTION rows(r t, p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          FOR i IN 1..2 LOOP
           SELECT * INTO r FROM t;
           SELECT id INTO i FROM t;
           EXECUTE p USING r, i;
          END LOOP;
         END $$;
         CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN SELECT * INTO NEW FROM t; EXECUTE 'SELECT 1' USING NEW; RETURN NEW; END $$;"
    ))
    .expect("schema parses");
    let reads = |name: &str| reaching_in(&database, name);
    assert_eq!(
        reads("rows"),
        [vec![
            read("p", &["incoming"]),
            read("r", &["Row First { strict: false } of SELECT * FROM t"]),
            read("i", &["Ordinal(0) First { strict: false } of SELECT id FROM t"]),
        ]]
    );
    assert_eq!(
        reads("stamp"),
        [vec![read("new", &["Row First { strict: false } of SELECT * FROM t"])]]
    );
}

/// What a command writes reads what the command and its arguments read,
/// and a range loop index what its bounds read.
#[test]
fn command_and_range_definitions_read_their_inputs() {
    let database = database(
        "DECLARE s text;
         BEGIN
          EXECUTE 'SELECT ' || p INTO s USING q;
          FOR i IN q..q + 1 BY q LOOP EXECUTE s USING i; END LOOP;
         END",
    );
    let body = run(&database).dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reads = sites[1].reads(IdentifierCase::Folded).unwrap();
    let inputs = |index: usize| {
        let Reaching::Definitions(definitions) = reads[index].reaching() else {
            panic!("the variable is reached")
        };
        spelled(&definitions[0].reads(IdentifierCase::Folded).unwrap())
    };
    assert_eq!(inputs(0), [read("p", &["incoming"]), read("q", &["incoming"])]);
    assert_eq!(inputs(1), [read("q", &["incoming"])]);
}

/// `RETURN NEXT` and `RETURN QUERY` read what reaches them and carry on, a
/// bare `RETURN NEXT` included, and `RETURN QUERY EXECUTE` is a site of its
/// own.
#[test]
fn set_returning_statements_read_what_reaches_them_and_carry_on() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION listed(p text, q integer) RETURNS SETOF xml LANGUAGE plpgsql AS $$
         DECLARE s text := 'SELECT ' || p;
         BEGIN
          RETURN NEXT query_to_xml(s, true, false, '');
          s := 'SELECT ' || q;
          RETURN QUERY SELECT query_to_xml(s, true, false, '');
          s := 'SELECT 1';
          RETURN QUERY EXECUTE s USING q;
         END $$;
         CREATE FUNCTION filled(p text, OUT x text) RETURNS SETOF text LANGUAGE plpgsql AS $$
         BEGIN x := p; RETURN NEXT; EXECUTE x; END $$;",
    )
    .expect("schema parses");
    assert_eq!(
        reaching_in(&database, "listed"),
        [
            vec![read("s", &["'SELECT ' || p"])],
            vec![read("s", &["'SELECT ' || q"])],
            vec![read("s", &["'SELECT 1'"]), read("q", &["incoming"])],
        ]
    );
    assert_eq!(reaching_in(&database, "filled"), [vec![read("x", &["p"])]]);
}
