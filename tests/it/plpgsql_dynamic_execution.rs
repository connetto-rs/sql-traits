//! The places a PL/pgSQL body runs SQL it builds at run time, with the names
//! their expressions reach typed as PostgreSQL 18 declares them.
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

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{ArgMode, Expr, SetExpr, Value, visit_expressions},
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

/// The command and `USING` arguments of each site of `name`, written out.
fn sites(database: &ParserDB, name: &str) -> Result<Vec<(String, Vec<String>)>, LookupError> {
    let body = function(database, name).dynamic_execution_sites(database)?;
    Ok(body
        .sites()?
        .iter()
        .map(|site| {
            (site.command().to_string(), site.arguments().map(ToString::to_string).collect())
        })
        .collect())
}

/// How a test spells the type an expression takes.
fn rendered(operand: &OperandType<'_>) -> String {
    match operand {
        OperandType::Known { type_name, typmod: Some(typmod) } => format!("{type_name}({typmod})"),
        OperandType::Known { type_name, typmod: None } => type_name.to_string(),
        OperandType::UntypedLiteral => "unknown literal".to_owned(),
        OperandType::Unknown => "?".to_owned(),
    }
}

/// Every name and `$n` the command of each site of `name` writes, with the
/// type it takes there.
fn names(database: &ParserDB, name: &str) -> Vec<Vec<(String, String)>> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    body.sites()
        .unwrap()
        .iter()
        .map(|site| {
            let context = site.context(IdentifierCase::Folded).unwrap();
            let mut typed = Vec::new();
            let _ = visit_expressions(site.command(), |expression| {
                if matches!(
                    expression,
                    Expr::Identifier(_)
                        | Expr::CompoundIdentifier(_)
                        | Expr::Value(sqlparser::ast::ValueWithSpan {
                            value: Value::Placeholder(_),
                            ..
                        })
                ) {
                    typed.push((
                        expression.to_string(),
                        rendered(&context.type_of(expression).unwrap()),
                    ));
                }
                ControlFlow::<()>::Continue(())
            });
            typed
        })
        .collect()
}

/// The variable the name `written` in the command of the only site of `name`
/// binds, or whose field it names, as its kind and the name it was declared
/// under.
fn variable(database: &ParserDB, name: &str, written: &str) -> Option<(VariableKind, String)> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    let sites = body.sites().unwrap();
    let [site] = sites.as_slice() else { panic!("{name} has one site") };
    let context = site.context(IdentifierCase::Folded).unwrap();
    let mut found = None;
    let _ = visit_expressions(site.command(), |expression| {
        if expression.to_string() == written {
            found = Some(context.reference(expression).unwrap());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    match found.expect("the command writes the name")? {
        (ColumnDefinition::Variable(variable) | ColumnDefinition::VariableField(variable), _) => {
            Some((variable.kind().clone(), variable.name().to_owned()))
        }
        (other, _) => panic!("{written} binds {other:?}"),
    }
}

const REPRODUCTION: &str = "CREATE TABLE t (name text);
    CREATE FUNCTION vulnerable(p text) RETURNS bigint LANGUAGE plpgsql AS $$
    DECLARE result bigint;
    BEGIN
     EXECUTE 'SELECT count(*) FROM t WHERE name = ''' || p || '''' INTO result;
     RETURN result;
    END $$;
    CREATE FUNCTION quoted(p text) RETURNS bigint LANGUAGE plpgsql AS $$
    DECLARE result bigint;
    BEGIN
     EXECUTE format('SELECT count(*) FROM t WHERE name = %L', p) INTO result;
     RETURN result;
    END $$;
    CREATE FUNCTION bound(p text) RETURNS bigint LANGUAGE plpgsql AS $$
    DECLARE result bigint;
    BEGIN
     EXECUTE 'SELECT count(*) FROM t WHERE name = $1' INTO result USING p;
     RETURN result;
    END $$;";

/// The concatenation PostgreSQL 18 lets an argument rewrite, and the two
/// spellings that keep it a value, each answer its command and arguments
/// with the parameter typed `text`.
#[test]
fn concatenated_quoted_and_bound_commands_are_sites_with_typed_parameters() {
    let database = postgres(REPRODUCTION);
    assert_eq!(
        sites(&database, "vulnerable").unwrap(),
        [("'SELECT count(*) FROM t WHERE name = ''' || p || ''''".to_owned(), vec![])]
    );
    assert_eq!(
        sites(&database, "quoted").unwrap(),
        [("format('SELECT count(*) FROM t WHERE name = %L', p)".to_owned(), vec![])]
    );
    assert_eq!(
        sites(&database, "bound").unwrap(),
        [("'SELECT count(*) FROM t WHERE name = $1'".to_owned(), vec!["p".to_owned()])]
    );
    for name in ["vulnerable", "quoted"] {
        assert_eq!(names(&database, name), [[("p".to_owned(), "pg_catalog.text".to_owned())]]);
    }
    let kind = VariableKind::Parameter { position: 1, mode: ArgMode::In };
    assert_eq!(variable(&database, "vulnerable", "p"), Some((kind, "p".to_owned())));
}

/// The command of a site types as a whole, and an escaping call resolves to
/// the function the path the body runs under reaches.
#[test]
fn a_command_types_and_its_calls_resolve_under_the_function_path() {
    let database = postgres(
        "CREATE SCHEMA evil;
         CREATE FUNCTION evil.quote_ident(text) RETURNS text LANGUAGE sql AS 'SELECT $1';
         CREATE FUNCTION trusting(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'DROP TABLE ' || quote_ident(p); END $$;
         CREATE FUNCTION hijacked(p text) RETURNS void LANGUAGE plpgsql
             SET search_path = evil, pg_catalog AS $$
         BEGIN EXECUTE 'DROP TABLE ' || quote_ident(p); END $$;",
    );
    for (name, schema) in [("trusting", "pg_catalog"), ("hijacked", "evil")] {
        let body = function(&database, name).dynamic_execution_sites(&database).unwrap();
        let sites = body.sites().unwrap();
        let context = sites[0].context(IdentifierCase::Folded).unwrap();
        assert_eq!(
            rendered(&context.type_of(sites[0].command()).unwrap()),
            "pg_catalog.text",
            "{name}"
        );
        let Expr::BinaryOp { right, .. } = sites[0].command() else { panic!("a concatenation") };
        let call = context.call(right).unwrap().expect("a call");
        let CallResolution::Selected(selected) = call.resolution() else {
            panic!("{name} selects one function: {:?}", call.resolution())
        };
        assert_eq!(selected.function().target_name().schema(), Some(schema), "{name}");
    }
}

/// The schema of the function each call written in the command of every site
/// of `name` selects, or `?` where nothing tells which one runs.
fn called_schemas(database: &ParserDB, name: &str) -> Vec<Vec<String>> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    body.sites()
        .unwrap()
        .iter()
        .map(|site| {
            let context = site.context(IdentifierCase::Folded).unwrap();
            let mut schemas = Vec::new();
            let _ = visit_expressions(site.command(), |expression| {
                if let Some(call) = context.call(expression).unwrap() {
                    schemas.push(match call.resolution() {
                        CallResolution::Selected(selected) => {
                            selected
                                .function()
                                .target_name()
                                .schema()
                                .unwrap_or("public")
                                .to_owned()
                        }
                        _ => "?".to_owned(),
                    });
                }
                ControlFlow::<()>::Continue(())
            });
            schemas
        })
        .collect()
}

/// A constant command setting `search_path` sets it for the sites after it,
/// while a command built at run time or a call to a user function, in a
/// statement or a `USING` argument, leaves a later call reaching a name only
/// `pg_catalog` defines, and nothing for a name another schema defines too.
/// In a loop, later also means an earlier statement on the next iteration.
#[test]
fn later_sites_follow_the_path_earlier_statements_leave() {
    let database = postgres(
        "CREATE SCHEMA evil;
         CREATE FUNCTION evil.quote_ident(text) RETURNS text LANGUAGE sql AS 'SELECT $1';
         CREATE FUNCTION helper() RETURNS void LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION constant_set(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE 'SET search_path = evil, pg_catalog';
          EXECUTE 'SELECT ' || quote_ident(p);
         END $$;
         CREATE FUNCTION built(p text, command text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE command;
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;
         CREATE FUNCTION after_call(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          PERFORM helper();
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;
         CREATE FUNCTION using_call(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE 'SELECT $1' USING helper();
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;
         CREATE FUNCTION in_loop(p text, command text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          LOOP
           EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
           EXECUTE command;
          END LOOP;
         END $$;",
    );
    assert_eq!(called_schemas(&database, "constant_set"), [vec![], vec!["evil".to_owned()]]);
    for name in ["built", "after_call", "using_call"] {
        let last = called_schemas(&database, name).pop().unwrap();
        assert_eq!(last, ["?", "pg_catalog"], "{name}");
    }
    let first = called_schemas(&database, "in_loop").swap_remove(0);
    assert_eq!(first, ["?", "pg_catalog"]);
}

/// `EXECUTE`, `RETURN QUERY EXECUTE`, `OPEN ... FOR EXECUTE` and
/// `FOR ... IN EXECUTE` are all sites, listed in body order through nested
/// blocks, branches, loops and handlers.
#[test]
fn every_dynamic_form_is_a_site_in_body_order() {
    let database = postgres(
        "CREATE FUNCTION every_form(p text) RETURNS SETOF text LANGUAGE plpgsql AS $$
         DECLARE cursor_value refcursor; row_value record;
         BEGIN
          RETURN QUERY EXECUTE 'SELECT $1::text' USING p;
          OPEN cursor_value FOR EXECUTE 'SELECT ' || 2 USING p, 3;
          FOR row_value IN EXECUTE 'SELECT ' || 3 LOOP
           IF p = 'a' THEN EXECUTE 'SELECT ' || 4; ELSE EXECUTE 'SELECT ' || 5; END IF;
          END LOOP;
          BEGIN
           EXECUTE 'SELECT ' || 6;
          EXCEPTION WHEN others THEN EXECUTE 'SELECT ' || 7;
          END;
         END $$;",
    );
    assert_eq!(
        sites(&database, "every_form").unwrap(),
        [
            ("'SELECT $1::text'".to_owned(), vec!["p".to_owned()]),
            ("'SELECT ' || 2".to_owned(), vec!["p".to_owned(), "3".to_owned()]),
            ("'SELECT ' || 3".to_owned(), vec![]),
            ("'SELECT ' || 4".to_owned(), vec![]),
            ("'SELECT ' || 5".to_owned(), vec![]),
            ("'SELECT ' || 6".to_owned(), vec![]),
            ("'SELECT ' || 7".to_owned(), vec![]),
        ]
    );
}

/// A body that builds no SQL answers no site, while a body nothing can read
/// answers why.
#[test]
fn a_body_without_dynamic_sql_has_no_sites_and_an_unreadable_one_errs() {
    let database = postgres(
        "CREATE TABLE t (name text);
         CREATE FUNCTION static_body() RETURNS bigint LANGUAGE plpgsql AS $$
         BEGIN RETURN (SELECT count(*) FROM t); END $$;
         CREATE FUNCTION sql_body() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM t';
         CREATE FUNCTION linked() RETURNS bigint LANGUAGE c AS 'library', 'symbol';
         CREATE FUNCTION broken() RETURNS bigint LANGUAGE plpgsql AS 'BEGIN RETURN';",
    );
    assert_eq!(sites(&database, "static_body").unwrap(), []);
    assert_eq!(sites(&database, "sql_body").unwrap(), []);
    assert!(matches!(
        sites(&database, "linked"),
        Err(LookupError::FunctionBodyNotSql { language, .. }) if language == "c"
    ));
    assert!(matches!(
        sites(&database, "broken"),
        Err(LookupError::FunctionBodyUnparsable { function, .. }) if function == "broken"
    ));
}

/// A local hides the parameter of its name, which the function's name and
/// `$1` still reach. `$n` counts the output parameters too.
#[test]
fn a_local_hides_a_parameter_that_the_function_label_and_position_reach() {
    let database = postgres(
        "CREATE FUNCTION shadowed(p text, OUT q bigint, r text) RETURNS record LANGUAGE plpgsql AS $$
         DECLARE p integer := 1;
         BEGIN EXECUTE 'SELECT ' || p || shadowed.p || $1 || $2 || $3; END $$;",
    );
    assert_eq!(
        names(&database, "shadowed"),
        [[
            ("p".to_owned(), "pg_catalog.int4".to_owned()),
            ("shadowed.p".to_owned(), "pg_catalog.text".to_owned()),
            ("$1".to_owned(), "pg_catalog.text".to_owned()),
            ("$2".to_owned(), "pg_catalog.int8".to_owned()),
            ("$3".to_owned(), "pg_catalog.text".to_owned()),
        ]]
    );
    assert_eq!(
        variable(&database, "shadowed", "shadowed.p"),
        Some((VariableKind::Parameter { position: 1, mode: ArgMode::In }, "p".to_owned()))
    );
    assert_eq!(
        variable(&database, "shadowed", "p"),
        Some((VariableKind::Declared { constant: false, not_null: false }, "p".to_owned()))
    );
}

/// A `RETURNS TABLE` column is an output parameter after the arguments, which
/// its name, the function's label and its position reach, as PostgreSQL 18
/// declares it.
#[test]
fn a_returned_table_column_is_an_output_parameter() {
    let database = postgres(
        "CREATE FUNCTION tabled(p text) RETURNS TABLE (total bigint) LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || total || tabled.total || $2; END $$;",
    );
    assert_eq!(
        names(&database, "tabled"),
        [[
            ("total".to_owned(), "pg_catalog.int8".to_owned()),
            ("tabled.total".to_owned(), "pg_catalog.int8".to_owned()),
            ("$2".to_owned(), "pg_catalog.int8".to_owned()),
        ]]
    );
    assert_eq!(
        variable(&database, "tabled", "total"),
        Some((VariableKind::Parameter { position: 2, mode: ArgMode::Out }, "total".to_owned()))
    );
}

/// The integer `FOR` loop declares its index for its body alone, a cursor
/// `FOR` loop its record, a handler `SQLSTATE` and `SQLERRM`, and every
/// function `FOUND`.
#[test]
fn loops_and_handlers_declare_their_own_names() {
    let database = postgres(
        "CREATE TABLE t (name text);
         CREATE FUNCTION implicit() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE i text; c CURSOR FOR SELECT name FROM t;
         BEGIN
          FOR i IN 1..2 LOOP EXECUTE 'SELECT ' || i; END LOOP;
          EXECUTE 'SELECT ' || i || found;
          FOR row_value IN c LOOP EXECUTE 'SELECT ' || row_value; END LOOP;
         EXCEPTION WHEN others THEN EXECUTE 'SELECT ' || sqlstate || sqlerrm;
         END $$;",
    );
    assert_eq!(
        names(&database, "implicit"),
        [
            vec![("i".to_owned(), "pg_catalog.int4".to_owned())],
            vec![
                ("i".to_owned(), "pg_catalog.text".to_owned()),
                ("found".to_owned(), "pg_catalog.bool".to_owned()),
            ],
            vec![("row_value".to_owned(), "pg_catalog.record".to_owned())],
            vec![
                ("sqlstate".to_owned(), "pg_catalog.text".to_owned()),
                ("sqlerrm".to_owned(), "pg_catalog.text".to_owned()),
            ],
        ]
    );
}

/// A name reaching a loop's variable through `ALIAS FOR` answers the name
/// the loop declares.
#[test]
fn an_alias_of_a_loop_variable_answers_the_loop_name() {
    let database = postgres(
        "CREATE TABLE t (name text);
         CREATE FUNCTION indexed() RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          FOR i IN 1..2 LOOP DECLARE a ALIAS FOR i; BEGIN EXECUTE 'SELECT ' || a; END; END LOOP;
         END $$;
         CREATE FUNCTION recorded() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE c CURSOR FOR SELECT name FROM t;
         BEGIN
          FOR r IN c LOOP DECLARE b ALIAS FOR r; BEGIN EXECUTE 'SELECT ' || b; END; END LOOP;
         END $$;",
    );
    assert_eq!(
        variable(&database, "indexed", "a"),
        Some((VariableKind::LoopIndex, "i".to_owned()))
    );
    assert_eq!(
        variable(&database, "recorded", "b"),
        Some((VariableKind::LoopRecord, "r".to_owned()))
    );
}

/// `%TYPE` and `%ROWTYPE` resolve through the schema under the function's
/// own path, a row's field takes its column's type, and a record's field
/// stays unknown.
#[test]
fn copied_types_resolve_through_the_schema_and_record_fields_stay_unknown() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (name varchar(5), n integer);
         CREATE FUNCTION copied(p text) RETURNS void LANGUAGE plpgsql SET search_path = app AS $$
         DECLARE a t.name%TYPE; b p%TYPE; c t%ROWTYPE; d b%TYPE; r record;
         BEGIN EXECUTE 'SELECT ' || a || b || c || c.n || d || r.anything; END $$;",
    );
    assert_eq!(
        names(&database, "copied"),
        [[
            ("a".to_owned(), "pg_catalog.varchar(9)".to_owned()),
            ("b".to_owned(), "pg_catalog.text".to_owned()),
            ("c".to_owned(), "app.t".to_owned()),
            ("c.n".to_owned(), "pg_catalog.int4".to_owned()),
            ("d".to_owned(), "pg_catalog.text".to_owned()),
            ("r.anything".to_owned(), "?".to_owned()),
        ]]
    );
    assert_eq!(
        variable(&database, "copied", "r.anything"),
        Some((VariableKind::Declared { constant: false, not_null: false }, "r".to_owned()))
    );
}

/// A `%TYPE` naming nothing the schema holds keeps the lookup error, and
/// the name stays a local of unknown type.
#[test]
fn an_unresolvable_copied_type_keeps_its_error() {
    let database = postgres(
        "CREATE FUNCTION missing() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE a absent.name%TYPE;
         BEGIN EXECUTE 'SELECT ' || a; END $$;",
    );
    let body = function(&database, "missing").dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let context = sites[0].context(IdentifierCase::Folded).unwrap();
    let Expr::BinaryOp { right, .. } = sites[0].command() else { panic!("a concatenation") };
    let Some((ColumnDefinition::Variable(local), _)) = context.reference(right).unwrap() else {
        panic!("a binds a local")
    };
    assert!(local.data_type().is_err(), "{:?}", local.data_type());
    assert_eq!(context.type_of(right).unwrap(), OperandType::Unknown);
}

/// The sites follow the body through `CREATE OR REPLACE`, and moving or
/// renaming the function keeps them, while a path set on it later changes
/// what a copied type resolves to.
#[test]
fn sites_follow_replacement_renaming_and_the_function_path() {
    let database = postgres(
        "CREATE SCHEMA a; CREATE SCHEMA b;
         CREATE TABLE a.t (v integer); CREATE TABLE b.t (v text);
         CREATE FUNCTION f(p text) RETURNS void LANGUAGE plpgsql SET search_path = a AS $$
         BEGIN EXECUTE 'SELECT ' || p; END $$;
         CREATE OR REPLACE FUNCTION f(p text) RETURNS void LANGUAGE plpgsql SET search_path = a AS $$
         DECLARE x t.v%TYPE; BEGIN EXECUTE 'SELECT ' || x; END $$;
         ALTER FUNCTION f(text) RENAME TO g;
         ALTER FUNCTION g(text) SET search_path = b;",
    );
    assert_eq!(names(&database, "g"), [[("x".to_owned(), "pg_catalog.text".to_owned())]]);
}

/// PostgreSQL 18 resolves `%TYPE` and `%ROWTYPE` when it compiles the body,
/// under the function's own path, whatever path the statements before the
/// declaration set.
#[test]
fn copied_types_resolve_under_the_path_the_body_compiles_with() {
    let database = postgres(
        "CREATE SCHEMA a; CREATE SCHEMA b;
         CREATE TABLE a.t (v integer); CREATE TABLE b.t (v text);
         CREATE FUNCTION helper() RETURNS integer LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION after_set() RETURNS void LANGUAGE plpgsql SET search_path = a AS $$
         BEGIN
          SET search_path = b;
          DECLARE x t.v%TYPE; r t%ROWTYPE;
          BEGIN EXECUTE 'SELECT ' || x || r; END;
         END $$;
         CREATE FUNCTION after_call() RETURNS void LANGUAGE plpgsql SET search_path = a AS $$
         BEGIN
          PERFORM helper();
          DECLARE x t.v%TYPE;
          BEGIN EXECUTE 'SELECT ' || x; END;
         END $$;",
    );
    assert_eq!(
        names(&database, "after_set"),
        [[("x".to_owned(), "pg_catalog.int4".to_owned()), ("r".to_owned(), "a.t".to_owned()),]]
    );
    assert_eq!(names(&database, "after_call"), [[("x".to_owned(), "pg_catalog.int4".to_owned())]]);
}

/// A qualified name passes over a scalar variable its first part names and
/// reaches the level that first part labels, as `plpgsql_ns_lookup` reads
/// it, while a row or record variable takes the qualified name as a field.
#[test]
fn a_qualified_name_passes_over_a_scalar_named_like_its_qualifier() {
    let database = postgres(
        "CREATE FUNCTION scalar_first() RETURNS void LANGUAGE plpgsql AS $$
         <<blk>> DECLARE x text;
         BEGIN DECLARE blk integer; BEGIN EXECUTE 'SELECT ' || blk.x; END; END $$;
         CREATE FUNCTION total(total integer) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || total.total; END $$;",
    );
    assert_eq!(
        names(&database, "scalar_first"),
        [[("blk.x".to_owned(), "pg_catalog.text".to_owned())]]
    );
    assert_eq!(
        variable(&database, "scalar_first", "blk.x"),
        Some((VariableKind::Declared { constant: false, not_null: false }, "x".to_owned()))
    );
    assert_eq!(
        variable(&database, "total", "total.total"),
        Some((VariableKind::Parameter { position: 1, mode: ArgMode::In }, "total".to_owned()))
    );
}

/// A constant command running a `DO` block may set the path for the sites
/// after it, as code outside the body does.
#[test]
fn a_constant_do_block_hides_the_path_from_later_sites() {
    let database = postgres(
        "CREATE SCHEMA evil;
         CREATE FUNCTION evil.quote_ident(text) RETURNS text LANGUAGE sql AS 'SELECT $1';
         CREATE FUNCTION via_do(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE 'DO $x$ BEGIN PERFORM 1; END $x$';
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;",
    );
    let last = called_schemas(&database, "via_do").pop().unwrap();
    assert_eq!(last, ["?", "pg_catalog"]);
}

/// A constant command PostgreSQL 18 only refuses when it runs leaves the
/// path unknown after the branch holding it, and the function keeps its
/// sites.
#[test]
fn a_constant_command_failing_at_run_time_in_a_branch_keeps_the_sites() {
    let database = postgres(
        "CREATE FUNCTION bad_set(flag boolean) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          IF flag THEN EXECUTE 'SET search_path = a.b'; END IF;
          EXECUTE 'SELECT ' || quote_literal('x');
         END $$;",
    );
    assert_eq!(called_schemas(&database, "bad_set"), [vec![], vec!["pg_catalog".to_owned()]]);
}

/// An unnamed argument binds only its `$n`, and an `ALIAS` reaches the
/// variable it names by position or by name, while an alias naming nothing
/// refuses the body, as PostgreSQL 18 refuses to compile it.
#[test]
fn aliases_and_unnamed_arguments_reach_their_parameters() {
    let database = postgres(
        "CREATE FUNCTION aliased(p text, integer) RETURNS void LANGUAGE plpgsql AS $$
         DECLARE a ALIAS FOR $2; b ALIAS FOR p;
         BEGIN EXECUTE 'SELECT ' || a || b || $2; END $$;
         CREATE FUNCTION dangling() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE c ALIAS FOR nothing;
         BEGIN EXECUTE 'SELECT ' || c; END $$;",
    );
    assert_eq!(
        names(&database, "aliased"),
        [[
            ("a".to_owned(), "pg_catalog.int4".to_owned()),
            ("b".to_owned(), "pg_catalog.text".to_owned()),
            ("$2".to_owned(), "pg_catalog.int4".to_owned()),
        ]]
    );
    let unnamed =
        Some((VariableKind::Parameter { position: 2, mode: ArgMode::In }, "$2".to_owned()));
    assert_eq!(variable(&database, "aliased", "a"), unnamed);
    let body = function(&database, "aliased").dynamic_execution_sites(&database).unwrap();
    let found = body.sites().unwrap();
    let scope = found[0].plpgsql_scope().expect("a PL/pgSQL site");
    assert_eq!(scope.function().name(), "aliased");
    assert_eq!(scope.parameter(1).as_ref().map(PlPgSqlVariable::name), Some("p"));
    assert_eq!(
        sites(&database, "dangling").unwrap_err(),
        LookupError::FunctionBodyUnresolvable {
            function: "dangling".to_owned(),
            statement: "c ALIAS FOR nothing".to_owned(),
        }
    );
}

/// A trigger function holds `NEW`, `OLD` and the `TG_` variables, and an
/// event trigger function, however qualified its return type, `TG_EVENT`
/// and `TG_TAG`.
#[test]
fn trigger_functions_hold_their_trigger_variables() {
    let database = postgres(
        "CREATE FUNCTION audited() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || tg_table_name || tg_argv || tg_nargs || new.id; RETURN new; END $$;
         CREATE FUNCTION ddl() RETURNS pg_catalog.event_trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || tg_event || tg_tag; END $$;",
    );
    assert_eq!(
        names(&database, "audited"),
        [[
            ("tg_table_name".to_owned(), "pg_catalog.name".to_owned()),
            ("tg_argv".to_owned(), "pg_catalog._text".to_owned()),
            ("tg_nargs".to_owned(), "pg_catalog.int4".to_owned()),
            ("new.id".to_owned(), "?".to_owned()),
        ]]
    );
    assert_eq!(
        variable(&database, "audited", "new.id"),
        Some((VariableKind::Trigger, "new".to_owned()))
    );
    assert_eq!(
        names(&database, "ddl"),
        [[
            ("tg_event".to_owned(), "pg_catalog.text".to_owned()),
            ("tg_tag".to_owned(), "pg_catalog.text".to_owned()),
        ]]
    );
}

/// Each table a trigger executing `name` is defined on, with the type the
/// field `written` in the command of the only site of `name` takes on it.
fn row_field_types(database: &ParserDB, name: &str, written: &str) -> Vec<(String, String)> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    let sites = body.sites().unwrap();
    let [site] = sites.as_slice() else { panic!("{name} has one site") };
    let scope = site.scope();
    let context =
        ExpressionContext::for_expression(site.command(), &scope, database, IdentifierCase::Folded)
            .unwrap();
    let mut found = None;
    let _ = visit_expressions(site.command(), |expression| {
        if expression.to_string() == written {
            found = Some(context.row_field_types(expression).unwrap());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found
        .expect("the command writes the name")
        .iter()
        .map(|(table, data_type)| (table.table_name().to_owned(), rendered(data_type)))
        .collect()
}

/// A field of `NEW` or `OLD`, written bare, through an alias or qualified by
/// the function's name, takes its column's type from the table whose trigger
/// executes the function, as PostgreSQL 18 types it per firing.
#[test]
fn trigger_row_fields_take_the_firing_tables_column_types() {
    let database = postgres(
        "CREATE TABLE audit (name text, size varchar(5));
         CREATE FUNCTION audit_row() RETURNS trigger LANGUAGE plpgsql AS $$
         DECLARE fresh ALIAS FOR new;
         BEGIN
          EXECUTE 'SELECT ' || NEW.name || old.size || fresh.name || audit_row.new.name
           || new.missing || new.name.part || new;
          RETURN NEW;
         END $$;
         CREATE TRIGGER audit_insert BEFORE INSERT ON audit FOR EACH ROW EXECUTE FUNCTION audit_row();
         CREATE TRIGGER audit_update BEFORE UPDATE ON audit FOR EACH ROW EXECUTE FUNCTION audit_row();",
    );
    assert_eq!(
        names(&database, "audit_row"),
        [[
            ("NEW.name".to_owned(), "pg_catalog.text".to_owned()),
            ("old.size".to_owned(), "pg_catalog.varchar(9)".to_owned()),
            ("fresh.name".to_owned(), "pg_catalog.text".to_owned()),
            ("audit_row.new.name".to_owned(), "pg_catalog.text".to_owned()),
            ("new.missing".to_owned(), "?".to_owned()),
            ("new.name.part".to_owned(), "?".to_owned()),
            ("new".to_owned(), "pg_catalog.record".to_owned()),
        ]]
    );
    assert_eq!(
        row_field_types(&database, "audit_row", "NEW.name"),
        [("audit".to_owned(), "pg_catalog.text".to_owned())]
    );
    assert_eq!(
        row_field_types(&database, "audit_row", "new.missing"),
        [("audit".to_owned(), "?".to_owned())]
    );
    assert_eq!(row_field_types(&database, "audit_row", "new.name.part"), []);
    assert_eq!(row_field_types(&database, "audit_row", "new"), []);
}

/// One function behind triggers on tables that disagree on a field types it
/// as unknown, each table's own answer kept, a statement-level trigger's
/// table included.
#[test]
fn trigger_row_fields_disagreeing_across_tables_stay_unknown() {
    let database = postgres(
        "CREATE TABLE counted (id integer, name integer);
         CREATE TABLE nameless (id integer);
         CREATE TABLE items (id integer, name text);
         CREATE FUNCTION show_name() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || new.name || new.id; RETURN NEW; END $$;
         CREATE TRIGGER show_counted BEFORE INSERT ON counted FOR EACH ROW EXECUTE FUNCTION show_name();
         CREATE TRIGGER show_nameless BEFORE INSERT ON nameless FOR EACH ROW EXECUTE FUNCTION show_name();
         CREATE TRIGGER show_statement BEFORE INSERT ON items FOR EACH STATEMENT EXECUTE FUNCTION show_name();",
    );
    assert_eq!(
        names(&database, "show_name"),
        [[
            ("new.name".to_owned(), "?".to_owned()),
            ("new.id".to_owned(), "pg_catalog.int4".to_owned()),
        ]]
    );
    assert_eq!(
        row_field_types(&database, "show_name", "new.name"),
        [
            ("counted".to_owned(), "pg_catalog.int4".to_owned()),
            ("nameless".to_owned(), "?".to_owned()),
            ("items".to_owned(), "pg_catalog.text".to_owned()),
        ]
    );
}

/// The triggers and columns the schema ends with type a trigger function's
/// row fields, whatever order created them.
#[test]
fn trigger_row_fields_follow_later_triggers_and_column_changes() {
    let database = postgres(
        "CREATE FUNCTION first() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || new.name; RETURN NEW; END $$;
         CREATE FUNCTION second() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || new.name; RETURN NEW; END $$;
         CREATE TABLE a (name integer);
         CREATE TABLE b (name integer);
         CREATE TRIGGER on_a BEFORE INSERT ON a FOR EACH ROW EXECUTE FUNCTION first();
         CREATE TRIGGER on_b BEFORE INSERT ON b FOR EACH ROW EXECUTE FUNCTION second();
         ALTER TABLE a ALTER COLUMN name TYPE text;
         CREATE OR REPLACE TRIGGER on_a BEFORE INSERT ON a FOR EACH ROW EXECUTE FUNCTION second();",
    );
    assert_eq!(names(&database, "first"), [[("new.name".to_owned(), "?".to_owned())]]);
    assert_eq!(row_field_types(&database, "first", "new.name"), []);
    assert_eq!(
        row_field_types(&database, "second", "new.name"),
        [
            ("a".to_owned(), "pg_catalog.text".to_owned()),
            ("b".to_owned(), "pg_catalog.int4".to_owned()),
        ]
    );
    let dropped = postgres(
        "CREATE TABLE a (name text);
         CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || new.name; RETURN NEW; END $$;
         CREATE TRIGGER on_a BEFORE INSERT ON a FOR EACH ROW EXECUTE FUNCTION f();
         DROP TRIGGER on_a ON a;",
    );
    assert_eq!(names(&dropped, "f"), [[("new.name".to_owned(), "?".to_owned())]]);
}

/// `%TYPE[]` and `%ROWTYPE[]` declare the array over the copied type with
/// its modifier, an array's array being the array itself, as PostgreSQL 18
/// declares them, and a type copied from a view stays unknown.
#[test]
fn copied_array_types_keep_the_modifier_and_views_stay_unknown() {
    let database = postgres(
        "CREATE TABLE t (name varchar(5), tags text[]);
         CREATE VIEW w AS SELECT name FROM t;
         CREATE FUNCTION arrays() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE a t.name%TYPE[]; b t.tags%TYPE[]; r t%ROWTYPE; v w.name%TYPE[]; vr w%ROWTYPE;
         BEGIN EXECUTE 'SELECT ' || a || b || r || v || vr; END $$;",
    );
    assert_eq!(
        names(&database, "arrays"),
        [[
            ("a".to_owned(), "pg_catalog._varchar(9)".to_owned()),
            ("b".to_owned(), "pg_catalog._text".to_owned()),
            ("r".to_owned(), "t".to_owned()),
            ("v".to_owned(), "?".to_owned()),
            ("vr".to_owned(), "?".to_owned()),
        ]]
    );
}

/// The error resolving the declared type of each variable the command of the
/// only site of `name` names, by the name written, each name typing as
/// unknown.
fn declared_type_errors(database: &ParserDB, name: &str) -> Vec<(String, Option<LookupError>)> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    let sites = body.sites().unwrap();
    let [site] = sites.as_slice() else { panic!("{name} has one site") };
    let context = site.context(IdentifierCase::Folded).unwrap();
    let mut found = Vec::new();
    let _ = visit_expressions(site.command(), |expression| {
        if let Some((
            ColumnDefinition::Variable(variable) | ColumnDefinition::VariableField(variable),
            _,
        )) = context.reference(expression).unwrap()
        {
            found.push((expression.to_string(), variable.data_type().err().cloned()));
            assert_eq!(context.type_of(expression).unwrap(), OperandType::Unknown);
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// A `%TYPE` naming a missing column or a missing variable, and a
/// `%ROWTYPE` naming a missing relation, keep the error on the variable,
/// whose fields then type as unknown.
#[test]
fn missing_copied_types_keep_their_errors() {
    let database = postgres(
        "CREATE TABLE t (name text);
         CREATE FUNCTION missing_parts() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE m t.absent%TYPE; n nothing%TYPE; x absent%ROWTYPE;
         BEGIN EXECUTE 'SELECT ' || m || n || x.f; END $$;",
    );
    assert_eq!(
        declared_type_errors(&database, "missing_parts"),
        [
            (
                "m".to_owned(),
                Some(LookupError::ColumnNotFound {
                    table_name: "t".to_owned(),
                    column_name: "absent".to_owned(),
                })
            ),
            (
                "n".to_owned(),
                Some(LookupError::FunctionBodyUnresolvable {
                    function: "missing_parts".to_owned(),
                    statement: "nothing%TYPE".to_owned(),
                })
            ),
            (
                "x.f".to_owned(),
                Some(LookupError::RelationNotFound { object_name: "absent".to_owned() })
            ),
        ]
    );
}

/// A constant command in dollar quotes sets the path for the sites after it,
/// a constant command that does not parse or is no string hides it, and a
/// `USING` argument calling user code inside a loop hides it from the
/// loop's earlier sites.
#[test]
fn constant_commands_and_looped_arguments_shape_the_path() {
    let database = postgres(
        "CREATE SCHEMA evil;
         CREATE FUNCTION evil.quote_ident(text) RETURNS text LANGUAGE sql AS 'SELECT $1';
         CREATE FUNCTION dollar_set(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE $q$SET search_path = evil, pg_catalog$q$;
          EXECUTE 'SELECT ' || quote_ident(p);
         END $$;
         CREATE FUNCTION unparsable(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE 'not sql at all';
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;
         CREATE FUNCTION null_command(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE NULL;
          EXECUTE 'SELECT ' || quote_ident(p) || quote_literal(p);
         END $$;
         CREATE FUNCTION looped_argument(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          LOOP EXECUTE 'SELECT $1' USING quote_ident(p); END LOOP;
         END $$;",
    );
    assert_eq!(called_schemas(&database, "dollar_set"), [vec![], vec!["evil".to_owned()]]);
    for name in ["unparsable", "null_command"] {
        let last = called_schemas(&database, name).pop().unwrap();
        assert_eq!(last, ["?", "pg_catalog"], "{name}");
    }
    let body = function(&database, "looped_argument").dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let argument = sites[0].arguments().next().expect("one argument");
    let scope = sites[0].scope();
    let context =
        ExpressionContext::for_expression(argument, &scope, &database, IdentifierCase::Folded)
            .unwrap();
    let call = context.call(argument).unwrap().expect("a call");
    assert!(matches!(call.resolution(), CallResolution::Undetermined), "{:?}", call.resolution());
}

/// Under a path nothing tells, an unqualified operator or `FROM` call that
/// a schema besides `pg_catalog` defines stays undetermined, while one only
/// `pg_catalog` defines resolves there. A command selecting from a relation
/// is the query as a scalar subquery.
#[test]
fn operators_and_from_calls_another_schema_defines_stay_undetermined_under_an_unknown_path() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.cat(text, text) RETURNS text LANGUAGE sql AS 'SELECT $1';
         CREATE OPERATOR app.|| (LEFTARG = text, RIGHTARG = text, FUNCTION = app.cat);
         CREATE FUNCTION helper() RETURNS void LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION commands() RETURNS SETOF text LANGUAGE sql AS 'SELECT ''SELECT 1''';
         CREATE FUNCTION hidden(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          EXECUTE c FROM commands() AS c;
          PERFORM helper();
          EXECUTE 'SELECT ' || p || (1 + 2);
          EXECUTE c FROM commands() AS c;
         END $$;",
    );
    let body = function(&database, "hidden").dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let [before, concatenated, after] = sites.as_slice() else { panic!("three sites") };
    assert_eq!(before.command().to_string(), "(SELECT c FROM commands() AS c)");
    let from_call = |site: &DynamicExecution<'_, '_, ParserDB>| {
        let Expr::Subquery(query) = site.command() else { panic!("a scalar subquery") };
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT") };
        let context = site.context(IdentifierCase::Folded).unwrap();
        let call = context.relation_call(&select.from[0].relation).unwrap().expect("a call");
        matches!(call.resolution(), CallResolution::Selected(_))
    };
    assert!(from_call(before));
    assert!(!from_call(after));
    let context = concatenated.context(IdentifierCase::Folded).unwrap();
    let mut resolved = Vec::new();
    let _ = visit_expressions(concatenated.command(), |expression| {
        if let Expr::BinaryOp { .. } = expression {
            for application in context.operators(expression).unwrap().unwrap_or_default() {
                let selected = matches!(application.resolution(), OperatorResolution::Selected(_));
                resolved.push((application.name().name().to_owned(), selected));
            }
        }
        ControlFlow::<()>::Continue(())
    });
    assert_eq!(
        resolved,
        [("||".to_owned(), false), ("||".to_owned(), false), ("+".to_owned(), true)]
    );
}

/// A function returning a polymorphic type through no output argument
/// holds `$0`, typed as its declared result, and one with an output
/// argument does not.
#[test]
fn a_polymorphic_result_declares_dollar_zero() {
    let database = postgres(
        "CREATE FUNCTION first_of(x anyelement) RETURNS anyelement LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || $0; RETURN x; END $$;
         CREATE FUNCTION with_out(x anyelement, OUT y anyelement) LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || $0; END $$;",
    );
    assert_eq!(
        names(&database, "first_of"),
        [[("$0".to_owned(), "pg_catalog.anyelement".to_owned())]]
    );
    assert_eq!(names(&database, "with_out"), [[("$0".to_owned(), "?".to_owned())]]);
}
