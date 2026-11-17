//! Which trigger bodies are maintenance bodies, and the expression each column
//! they assign holds when the body returns `NEW`.
#![expect(clippy::expect_used, reason = "a schema that does not build fails the test")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

/// The `(column, expression)` pairs of a `BEFORE` row trigger on `t`, declared
/// with `columns`, whose function runs `statements` and then `RETURN NEW`,
/// none when it is no maintenance trigger.
fn assignments(columns: &str, statements: &str) -> Vec<(String, String)> {
    assignments_of_body(columns, &format!("BEGIN {statements} RETURN NEW; END"))
}

/// The database holding table `t`, declared with `columns`, and a `BEFORE`
/// row trigger on it whose function's body is `body`, written after one
/// space.
fn database(columns: &str, body: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "CREATE TABLE t ({columns});
         CREATE FUNCTION maintain() RETURNS TRIGGER LANGUAGE plpgsql AS $$ {body} $$;
         CREATE TRIGGER tg BEFORE INSERT OR UPDATE ON t
             FOR EACH ROW EXECUTE FUNCTION maintain();"
    ))
    .expect("schema builds")
}

/// The `(column, expression)` pairs of a `BEFORE` row trigger on `t`, declared
/// with `columns`, whose function's body is `body`, none when it is no
/// maintenance trigger.
fn assignments_of_body(columns: &str, body: &str) -> Vec<(String, String)> {
    let database = database(columns, body);
    let trigger = database.triggers().next().expect("the trigger is declared");
    let pairs: Vec<(String, String)> = trigger
        .maintenance_assignments(&database)
        .expect("t is in the database")
        .map(|(column, expression)| (column.column_name().to_owned(), expression.to_string()))
        .collect();
    assert_eq!(
        trigger.is_maintenance_trigger(&database).expect("t is in the database"),
        !pairs.is_empty(),
        "a maintenance trigger is one with assignments"
    );
    assert_eq!(
        trigger.maintenance_refusal(&database).expect("t is in the database").is_none(),
        !pairs.is_empty(),
        "a maintenance trigger is one without refusal"
    );
    pairs
}

/// What makes the trigger [`assignments`] describes no maintenance trigger.
fn refusal(columns: &str, statements: &str) -> MaintenanceRefusal {
    refusal_of_body(columns, &format!("BEGIN {statements} RETURN NEW; END"))
}

/// What makes the trigger [`assignments_of_body`] describes no maintenance
/// trigger.
fn refusal_of_body(columns: &str, body: &str) -> MaintenanceRefusal {
    assert_eq!(assignments_of_body(columns, body), pairs(&[]), "{body} is refused");
    let database = database(columns, body);
    let trigger = database.triggers().next().expect("the trigger is declared");
    trigger
        .maintenance_refusal(&database)
        .expect("t is in the database")
        .expect("a trigger without assignments is refused")
}

/// The pairs `expected` writes as string slices.
fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected.iter().map(|(column, value)| ((*column).to_owned(), (*value).to_owned())).collect()
}

#[test]
fn nested_if_folds_into_nested_case_keeping_tg_op() {
    assert_eq!(
        assignments(
            "id UUID PRIMARY KEY, archived_at TIMESTAMPTZ",
            "IF NEW.archived_at IS NOT NULL THEN
                 IF TG_OP = 'UPDATE' AND OLD.archived_at IS NOT NULL THEN
                     NEW.archived_at := OLD.archived_at;
                 ELSE
                     NEW.archived_at := now();
                 END IF;
             END IF;",
        ),
        pairs(&[(
            "archived_at",
            "CASE WHEN NEW.archived_at IS NOT NULL THEN CASE WHEN TG_OP = 'UPDATE' AND \
             OLD.archived_at IS NOT NULL THEN OLD.archived_at ELSE now() END ELSE \
             NEW.archived_at END",
        )])
    );
}

#[test]
fn elsif_chain_without_else_keeps_the_value_before_the_if() {
    assert_eq!(
        assignments(
            "a INT",
            "IF NEW.a < 0 THEN NEW.a := 0; ELSIF NEW.a > 10 THEN NEW.a := 10; END IF;",
        ),
        pairs(&[("a", "CASE WHEN NEW.a < 0 THEN 0 WHEN NEW.a > 10 THEN 10 ELSE NEW.a END")])
    );
}

#[test]
fn columns_assigned_in_different_branches_each_get_a_case() {
    assert_eq!(
        assignments(
            "created_at TIMESTAMP, updated_at TIMESTAMP",
            "IF TG_OP = 'INSERT' THEN NEW.created_at := now(); ELSE NEW.updated_at := now(); END IF;",
        ),
        pairs(&[
            ("created_at", "CASE WHEN TG_OP = 'INSERT' THEN now() ELSE NEW.created_at END"),
            ("updated_at", "CASE WHEN TG_OP = 'INSERT' THEN NEW.updated_at ELSE now() END"),
        ])
    );
}

/// PostgreSQL runs the statements in order, so a later read of `NEW.a` sees
/// the value assigned before it, and each pair must read `NEW` as it was
/// before the body ran.
#[test]
fn a_read_after_an_assignment_reads_the_assigned_value() {
    assert_eq!(
        assignments("a INT, b INT", "NEW.a := NEW.a + 1; NEW.b := NEW.a * 2;"),
        pairs(&[("a", "NEW.a + 1"), ("b", "(NEW.a + 1) * 2")])
    );
    assert_eq!(
        assignments("a INT, b INT", "NEW.a := 1; IF NEW.a > 0 THEN NEW.b := NEW.a; END IF;"),
        pairs(&[("a", "1"), ("b", "CASE WHEN 1 > 0 THEN 1 ELSE NEW.b END")])
    );
    assert_eq!(
        assignments("a INT", "IF TG_OP = 'INSERT' THEN NEW.a := 1; END IF; NEW.a := NEW.a + 1;"),
        pairs(&[("a", "CASE WHEN TG_OP = 'INSERT' THEN 1 ELSE NEW.a END + 1")])
    );
}

#[test]
fn quoted_columns_resolve_and_substitute_as_written() {
    assert_eq!(
        assignments("\"ColA\" INT, b INT", "NEW.\"ColA\" := 1; NEW.b := NEW.\"ColA\";"),
        pairs(&[("ColA", "1"), ("b", "1")])
    );
    assert_eq!(assignments("\"ColA\" INT", "NEW.cola := 1;"), pairs(&[]));
    assert_eq!(
        assignments("\"ColA\" INT", "IF TG_OP = 'INSERT' THEN NEW.\"ColA\" := 1; END IF;"),
        pairs(&[("ColA", "CASE WHEN TG_OP = 'INSERT' THEN 1 ELSE NEW.\"ColA\" END")])
    );
}

/// PL/pgSQL labels the level holding `NEW` with the function's name, so
/// `maintain.NEW` is `NEW` in every place a body writes it.
#[test]
fn new_qualified_by_the_function_name_is_new() {
    assert_eq!(
        assignments_of_body(
            "a INT, b INT",
            "BEGIN maintain.NEW.a := 1; NEW.b := maintain.new.a + 1; RETURN maintain.NEW; END",
        ),
        pairs(&[("a", "1"), ("b", "1 + 1")])
    );
    assert_eq!(
        assignments("a INT, h TEXT", "NEW.a := 1; NEW.h := md5(maintain.NEW::text);"),
        pairs(&[])
    );
}

/// The whole row read after an assignment holds the assigned value, which no
/// expression over the row before the body ran can write.
#[test]
fn the_whole_row_read_after_an_assignment_refuses_the_body() {
    assert_eq!(
        assignments("a INT, h TEXT", "NEW.h := md5(NEW::text); NEW.a := 1;"),
        pairs(&[("h", "md5(NEW::TEXT)"), ("a", "1")])
    );
    assert_eq!(
        refusal("a INT, h TEXT", "NEW.a := 1; NEW.h := md5(NEW::text);").to_string(),
        "`NEW` in `NEW.h := md5(NEW::text);` at line 1, column 20 reads NEW after a column of it is assigned"
    );
}

/// `NEW.*` expands to the whole row, so it reads the assigned values too, in
/// a row constructor, as a function argument and in a select list.
#[test]
fn a_wildcard_over_new_after_an_assignment_refuses_the_body() {
    assert_eq!(
        assignments("a INT, h TEXT", "NEW.h := row_to_json(NEW.*); NEW.a := 1;"),
        pairs(&[("h", "row_to_json(NEW.*)"), ("a", "1")])
    );
    for (read, refused) in [
        ("md5(ROW(NEW.*)::text)", "ROW(NEW.*)"),
        ("row_to_json(NEW.*)", "row_to_json(NEW.*)"),
        ("(SELECT NEW.*)", "NEW.*"),
        ("row_to_json(maintain.NEW.*)", "row_to_json(maintain.NEW.*)"),
        ("row_to_json(NEW.a.*)", "row_to_json(NEW.a.*)"),
    ] {
        assert_eq!(
            refusal("a INT, h TEXT", &format!("NEW.a := 1; NEW.h := {read};")).to_string(),
            format!(
                "`{refused}` in `NEW.h := {read};` at line 1, column 20 reads NEW after a column of it is assigned"
            )
        );
    }
}

/// PostgreSQL refuses a body reading a field `NEW` does not have, and a field
/// of an assigned column reads a value no pair can write.
#[test]
fn reading_a_field_the_row_lacks_or_inside_an_assigned_column_refuses_the_body() {
    assert_eq!(
        refusal("a INT", "NEW.a := NEW.missing;").to_string(),
        "the table declares no column `missing`, which the body writes at line 1, column 21"
    );
    assert_eq!(
        refusal("a INT, b INT", "NEW.a := 1; NEW.b := NEW.a.x;").to_string(),
        "`NEW.a.x` in `NEW.b := NEW.a.x;` at line 1, column 20 reads NEW after a column of it is assigned"
    );
}

/// Each statement but an assignment to a column of `NEW` or an `IF` over
/// such assignments is named, with where the body writes it.
#[test]
fn any_other_statement_refuses_the_body_naming_it() {
    for statement in [
        "RAISE NOTICE 'stamping';",
        "PERFORM pg_notify('t', 'stamped');",
        "INSERT INTO t (a) VALUES (1);",
        "LOOP EXIT; END LOOP;",
        "BEGIN NEW.a := 2; END;",
    ] {
        assert_eq!(
            refusal("a INT", &format!("NEW.a := 1; {statement}")).to_string(),
            format!(
                "`{statement}` at line 1, column 20 neither assigns a column of NEW nor is an IF over such assignments"
            )
        );
    }
    assert_eq!(
        refusal("a INT", "NEW.a := 1; IF NEW.a IS NULL THEN RETURN NEW; END IF;").to_string(),
        "`RETURN NEW;` at line 1, column 42 returns before the end of the body"
    );
}

/// The statement refusing a body on a later line is named on that line.
#[test]
fn a_refusing_statement_is_placed_on_its_line() {
    assert_eq!(
        refusal_of_body(
            "txt TEXT",
            "BEGIN\n  NEW.txt := upper(NEW.txt);\n  INSERT INTO audit (txt) VALUES (NEW.txt);\n  RETURN NEW;\nEND"
        )
        .to_string(),
        "`INSERT INTO audit (txt) VALUES (NEW.txt);` at line 3, column 3 neither assigns a column of NEW nor is an IF over such assignments"
    );
}

#[test]
fn a_body_that_does_not_return_new_or_assigns_elsewhere_is_refused() {
    for (body, refused) in [
        (
            "BEGIN NEW.a := 1; RETURN NULL; END",
            "`RETURN NULL;` at line 1, column 20 ends the body without returning NEW",
        ),
        (
            "BEGIN NEW.a := 1; END",
            "`NEW.a := 1;` at line 1, column 8 ends the body without returning NEW",
        ),
        ("BEGIN END", "the body has no statement"),
        ("BEGIN RETURN NEW; END", "the body assigns no column of NEW"),
        ("DECLARE x INT; BEGIN NEW.a := 1; RETURN NEW; END", "the body declares `x`"),
        (
            "BEGIN NEW.a := 1; RETURN NEW; EXCEPTION WHEN OTHERS THEN RETURN NEW; END",
            "the body handles errors",
        ),
        (
            "BEGIN OLD.a := 1; RETURN NEW; END",
            "`OLD.a := 1;` at line 1, column 8 neither assigns a column of NEW nor is an IF over such assignments",
        ),
        (
            "BEGIN NEW := OLD; RETURN NEW; END",
            "`NEW := OLD;` at line 1, column 8 neither assigns a column of NEW nor is an IF over such assignments",
        ),
        (
            "BEGIN NEW.a.x := 1; RETURN NEW; END",
            "`NEW.a.x := 1;` at line 1, column 8 neither assigns a column of NEW nor is an IF over such assignments",
        ),
        (
            "BEGIN NEW.a[1] := 1; RETURN NEW; END",
            "`NEW.a[1] := 1;` at line 1, column 8 neither assigns a column of NEW nor is an IF over such assignments",
        ),
        (
            "BEGIN NEW.b := 1; RETURN NEW; END",
            "the table declares no column `b`, which the body writes at line 1, column 12",
        ),
    ] {
        assert_eq!(refusal_of_body("a INT", body).to_string(), refused, "{body}");
    }
    assert!(matches!(
        refusal_of_body("a INT", "BEGIN NEW.a := (1; RETURN NEW; END"),
        MaintenanceRefusal::UnparsableBody { .. }
    ));
}
