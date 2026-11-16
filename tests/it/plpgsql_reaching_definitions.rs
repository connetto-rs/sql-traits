//! The definitions that may reach each variable a dynamic-execution site of
//! a PL/pgSQL body reads, followed through its branches, loops, blocks,
//! handlers and control transfers.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails loudly on a broken premise"
)]

use sql_traits::prelude::*;
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
        DefinitionSource::Value(value) => value.to_string(),
        DefinitionSource::CursorName => "cursor name".to_owned(),
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

/// The variables each site of `run` running `body` reads, with their
/// definitions spelled.
fn reaching(body: &str) -> Vec<Vec<(String, Vec<String>)>> {
    let database = database(body);
    let sites = run(&database).dynamic_execution_sites(&database).unwrap();
    sites
        .sites()
        .unwrap()
        .iter()
        .map(|site| spelled(&site.reads(IdentifierCase::Folded).unwrap()))
        .collect()
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
        [vec![read("r", &["unknown", ".name p"])]]
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

/// The writes the walk does not trace assign an unknown value, which
/// replaces what a whole variable held unless the write may leave it.
#[test]
fn untraced_writes_assign_an_unknown_value() {
    assert_eq!(
        reaching(
            "DECLARE c CURSOR FOR SELECT name FROM t; k refcursor; s text := 'SELECT 1'; f text;
              n integer; o text := 'SELECT 2'; e text := 'SELECT 3'; l text := 'SELECT 4';
              g text; h text;
             BEGIN
              SELECT name INTO s FROM t;
              OPEN c;
              FETCH c INTO f;
              GET DIAGNOSTICS n = ROW_COUNT;
              CALL fill(o, 'x', run.p, missing);
              FOREACH e IN ARRAY ARRAY[p] LOOP END LOOP;
              FOR l IN SELECT name FROM t LOOP END LOOP;
              EXECUTE 'SELECT 5' INTO g;
              FOR h IN EXECUTE 'SELECT 6' LOOP END LOOP;
              OPEN k FOR SELECT name FROM t;
              OPEN k FOR EXECUTE 'SELECT 7';
              ASSERT p IS NOT NULL;
              EXECUTE s USING f, n, c, o, e, l, g, h, k, p;
             END"
        ),
        [
            vec![],
            vec![],
            vec![],
            vec![
                read("s", &["unknown"]),
                read("f", &["unknown"]),
                read("n", &["unknown"]),
                read("c", &["cursor name", "unknown"]),
                read("o", &["'SELECT 2'", "unknown"]),
                read("e", &["'SELECT 3'", "unknown"]),
                read("l", &["unknown"]),
                read("g", &["unknown"]),
                read("h", &["unknown"]),
                read("k", &["null", "unknown", "unknown"]),
                read("p", &["incoming", "unknown"]),
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
        [vec![read("c", &["NULL", "unknown"])], vec![read("c", &["NULL"])]]
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

/// An element write keeps the rest of the array, a value nothing in the
/// body tells is unknown, and nothing after `RAISE EXCEPTION` runs.
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
                read("found", &["unknown"]),
            ],
            vec![read("p", &["unreachable"])],
        ]
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
            read("s", &["'SELECT 1'", "unknown"]),
            read("o", &["'SELECT 2'", "unknown"]),
            read("p", &["incoming", "unknown"]),
        ]]
    );
}
