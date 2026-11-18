//! Tests that a `CREATE PROCEDURE` is held as a function of procedure kind,
//! which only `CALL` runs, as PostgreSQL keeps both in `pg_proc`.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a test fails loudly on a broken premise"
)]

use sql_traits::{
    errors::{Error, Misapplied},
    prelude::*,
};
use sqlparser::{
    ast::{ArgMode, CreateFunction},
    dialect::{GenericDialect, MsSqlDialect, PostgreSqlDialect},
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The routines `db` holds under `name`, each with its argument types and
/// its kind.
fn routines(db: &ParserDB, name: &str) -> Vec<(Vec<String>, FunctionKind)> {
    db.functions()
        .filter(|function| function.name() == name)
        .map(|function| {
            (
                function
                    .normalized_argument_type_names(db)
                    .into_iter()
                    .map(std::borrow::Cow::into_owned)
                    .collect(),
                function.kind(db).expect("the routine is held"),
            )
        })
        .collect()
}

/// The one routine `db` holds under `name`.
fn routine<'db>(db: &'db ParserDB, name: &str) -> &'db CreateFunction {
    db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the routine is held")
}

const COUNT_ITEMS: &str = "CREATE TABLE items (name text);
     CREATE PROCEDURE count_items(p text, INOUT result bigint = NULL) LANGUAGE plpgsql AS $$
     BEGIN
      EXECUTE 'SELECT count(*) FROM items WHERE name = ''' || p || '''' INTO result;
     END $$;";

/// A procedure is a function of procedure kind returning nothing, whose
/// parameters keep their modes, and whose body answers the facts any
/// function body answers.
#[test]
fn a_procedure_is_a_function_of_procedure_kind() {
    let db = parse(COUNT_ITEMS).expect("schema parses");
    let procedure = routine(&db, "count_items");

    assert_eq!(procedure.kind(&db), Ok(FunctionKind::Procedure));
    assert_eq!(procedure.return_type_name(&db), None);
    assert_eq!(procedure.argument_modes().collect::<Vec<_>>(), [ArgMode::In, ArgMode::InOut]);
    assert_eq!(procedure.normalized_argument_type_names(&db), ["TEXT", "BIGINT"]);
    assert_eq!(procedure.argument_default_count(), 1);
    assert_eq!(procedure.language(), Some("plpgsql"));
    assert_eq!(procedure.is_executable_by_public(&db), Ok(true));
    assert!(procedure.search_path(&db).expect("the settings are read").is_none());

    let sites = procedure.dynamic_execution_sites(&db).expect("the body is read");
    assert_eq!(sites.sites().expect("the body is PL/pgSQL").len(), 1);
}

/// Every body spelling the parser reads becomes the function's body.
#[test]
fn a_procedure_keeps_its_string_or_atomic_body() {
    let db = parse(
        "CREATE TABLE t (x int);
         CREATE PROCEDURE quoted(x text) LANGUAGE sql AS 'SELECT lower(x)';
         CREATE PROCEDURE trailing(x text) AS 'SELECT upper(x)' LANGUAGE sql;
         CREATE PROCEDURE atomic() LANGUAGE sql BEGIN ATOMIC INSERT INTO t VALUES (1); END;",
    )
    .expect("schema parses");

    assert_eq!(routine(&db, "quoted").body(), Some("SELECT lower(x)"));
    assert_eq!(routine(&db, "trailing").body(), Some("SELECT upper(x)"));
    assert_eq!(routine(&db, "trailing").language(), Some("sql"));
    let atomic = routine(&db, "atomic");
    assert_eq!(atomic.kind(&db), Ok(FunctionKind::Procedure));
    let tables: Vec<_> = atomic
        .referenced_tables(&db)
        .expect("the body is bound")
        .into_iter()
        .map(|table| table.table_name().to_owned())
        .collect();
    assert_eq!(tables, ["t"]);
}

/// The generic dialect reads PostgreSQL's procedures, and a dialect whose
/// procedures follow other rules leaves them out.
#[test]
fn procedures_are_held_under_postgres_and_generic_only() {
    let sql = "CREATE PROCEDURE p(x text) LANGUAGE sql AS 'SELECT 1';";
    let generic = ParserDB::parse::<GenericDialect>(sql).expect("schema parses");
    assert_eq!(routines(&generic, "p"), [(vec!["TEXT".to_owned()], FunctionKind::Procedure)]);

    let mssql =
        ParserDB::parse::<MsSqlDialect>("CREATE PROCEDURE p AS SELECT 1;").expect("schema parses");
    assert_eq!(routines(&mssql, "p"), []);
}

/// A body of bare statements is no PostgreSQL procedure body.
#[test]
fn a_statement_list_body_is_refused() {
    let result = parse("CREATE PROCEDURE p() AS BEGIN SELECT 1; END;");
    assert!(
        matches!(&result, Err(Error::ProcedureBodyUnsupported { procedure_name }) if procedure_name == "p"),
        "{result:?}"
    );
}

/// A procedure and a function share `pg_proc`, so neither may repeat the
/// other's input arguments, and `OUT` parameters stay out of the identity.
#[test]
fn a_procedure_and_a_function_never_share_a_signature() {
    for sql in [
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE f(a int) LANGUAGE sql AS 'SELECT 1';",
        "CREATE PROCEDURE f(a int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'SELECT 1';",
        "CREATE PROCEDURE f(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE f(a int, OUT b text) LANGUAGE sql AS 'SELECT 1';",
    ] {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::FunctionAlreadyExists { function_name }) if function_name == "f"),
            "{sql}: {result:?}"
        );
    }

    let result = parse(
        "CREATE PROCEDURE p(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE OR REPLACE FUNCTION p(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';",
    );
    assert!(
        matches!(
            &result,
            Err(Error::FunctionKindChange { function_name, kind: FunctionKind::Procedure })
                if function_name == "p"
        ),
        "{result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("`p` is a procedure,"));

    let db = parse(
        "CREATE PROCEDURE p(a int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION p(a text) RETURNS int LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");
    assert_eq!(
        routines(&db, "p"),
        [
            (vec!["INT".to_owned()], FunctionKind::Procedure),
            (vec!["TEXT".to_owned()], FunctionKind::Normal),
        ]
    );
}

/// `DROP PROCEDURE` reaches procedures alone and `DROP FUNCTION` reaches
/// everything else, a signature naming the other kind refused as PostgreSQL
/// refuses it.
#[test]
fn drops_reach_routines_of_their_own_kind() {
    const BOTH: &str = "CREATE PROCEDURE p(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'SELECT 1';";

    for drop in ["DROP PROCEDURE p(int);", "DROP PROCEDURE p(int, OUT text);", "DROP PROCEDURE p;"]
    {
        let db = parse(&format!("{BOTH} {drop}")).expect("schema parses");
        assert_eq!(routines(&db, "p"), [], "{drop}");
        assert_eq!(routines(&db, "f").len(), 1, "{drop}");
    }

    let result = parse(&format!("{BOTH} DROP FUNCTION p(int);"));
    assert!(
        matches!(&result, Err(Error::NotAFunction { function_name }) if function_name == "p(INT)"),
        "{result:?}"
    );
    let result = parse(&format!("{BOTH} DROP FUNCTION IF EXISTS p(int);"));
    assert!(matches!(&result, Err(Error::NotAFunction { .. })), "{result:?}");
    let result = parse(&format!("{BOTH} DROP PROCEDURE f(int);"));
    assert!(
        matches!(&result, Err(Error::NotAProcedure { procedure_name }) if procedure_name == "f(INT)"),
        "{result:?}"
    );

    // A bare name looks among the routines of the statement's kind only.
    let result = parse(&format!("{BOTH} DROP FUNCTION p;"));
    assert!(
        matches!(&result, Err(Error::DropFunctionNotFound { function_name }) if function_name == "p"),
        "{result:?}"
    );
    let result = parse(&format!("{BOTH} DROP PROCEDURE f;"));
    assert!(
        matches!(&result, Err(Error::DropProcedureNotFound { procedure_name }) if procedure_name == "f"),
        "{result:?}"
    );
    let db = parse(&format!("{BOTH} DROP PROCEDURE IF EXISTS f; DROP PROCEDURE IF EXISTS q();"))
        .expect("schema parses");
    assert_eq!(routines(&db, "f").len(), 1);

    let result = parse(&format!(
        "{BOTH} CREATE PROCEDURE p(a text) LANGUAGE sql AS 'SELECT 1'; DROP PROCEDURE p;"
    ));
    assert!(
        matches!(&result, Err(Error::AmbiguousDropProcedure { procedure_name }) if procedure_name == "p"),
        "{result:?}"
    );

    let db = parse(
        "CREATE PROCEDURE p(a int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION p(a text) RETURNS int LANGUAGE sql AS 'SELECT 1';
         DROP PROCEDURE p;",
    )
    .expect("schema parses");
    assert_eq!(routines(&db, "p"), [(vec!["TEXT".to_owned()], FunctionKind::Normal)]);
}

/// A `PROCEDURE` or `ROUTINE` statement marking no mode may list every
/// argument type, `OUT` ones included, and refuses a list that names one
/// routine by its input types and another by all of them.
#[test]
fn an_unmarked_list_may_name_every_argument() {
    const ROUTINES: &str = "CREATE ROLE bob;
         CREATE PROCEDURE p(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';";

    let db = parse(&format!("{ROUTINES} DROP PROCEDURE p(int, int);")).expect("schema parses");
    assert_eq!(routines(&db, "p"), []);

    let db = parse(&format!(
        "{ROUTINES} REVOKE EXECUTE ON PROCEDURE p(int, int) FROM PUBLIC;
         REVOKE EXECUTE ON ROUTINE f(int, int) FROM PUBLIC;"
    ))
    .expect("schema parses");
    assert_eq!(routine(&db, "p").is_executable_by_public(&db), Ok(false));
    assert_eq!(routine(&db, "f").is_executable_by_public(&db), Ok(false));

    // `PROCEDURE` matches every argument among procedures alone.
    let result = parse(&format!("{ROUTINES} DROP PROCEDURE f(int, int);"));
    assert!(
        matches!(&result, Err(Error::DropProcedureNotFound { procedure_name }) if procedure_name == "f"),
        "{result:?}"
    );
    let result = parse(&format!(
        "{ROUTINES} CREATE PROCEDURE p(a int, b int, c int) LANGUAGE sql AS 'SELECT 1';
         DROP PROCEDURE p(int, OUT int);"
    ));
    assert_eq!(
        routines(&result.expect("schema parses"), "p"),
        [(vec!["INT".to_owned(), "INT".to_owned(), "INT".to_owned()], FunctionKind::Procedure)]
    );

    // PostgreSQL 18 refuses each of these as not unique, whatever the order.
    for (routines_sql, drop) in [
        (
            "CREATE PROCEDURE q(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
             CREATE PROCEDURE q(a int, b int) LANGUAGE sql AS 'SELECT 1';",
            "DROP PROCEDURE q(int, int);",
        ),
        (
            "CREATE PROCEDURE q(a int, b int) LANGUAGE sql AS 'SELECT 1';
             CREATE PROCEDURE q(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';",
            "DROP PROCEDURE q(int, int);",
        ),
        (
            "CREATE PROCEDURE q(a int, b int) LANGUAGE sql AS 'SELECT 1';
             CREATE PROCEDURE q(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';",
            "DROP PROCEDURE public.q(int, int);",
        ),
        (
            "CREATE PROCEDURE q(a int, OUT b text, c int) LANGUAGE sql AS 'SELECT 1';
             CREATE PROCEDURE q(OUT a int, b text, c int) LANGUAGE sql AS 'SELECT 1';",
            "DROP PROCEDURE q(int, text, int);",
        ),
    ] {
        let result = parse(&format!("{routines_sql} {drop}"));
        assert!(
            matches!(&result, Err(Error::AmbiguousRoutineSignature { .. })),
            "{drop}: {result:?}"
        );
    }

    // A qualified name counts only the routines of the statement's kind.
    let mixed = "CREATE FUNCTION public.g(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE public.g(a int, b int) LANGUAGE sql AS 'SELECT 1';";
    let db = parse(&format!("{mixed} DROP PROCEDURE public.g(int, int);")).expect("schema parses");
    assert_eq!(
        routines(&db, "g"),
        [(vec!["INT".to_owned(), "INT".to_owned()], FunctionKind::Normal)]
    );
    let result = parse(&format!("{mixed} DROP PROCEDURE g(int, int);"));
    assert!(matches!(&result, Err(Error::AmbiguousRoutineSignature { .. })), "{result:?}");
}

/// A bare name walks the path among routines of every kind, so a procedure
/// earlier on it hides a function of the same arguments from `DROP FUNCTION`
/// and from a trigger, as PostgreSQL 18 refuses both.
#[test]
fn a_procedure_hides_a_later_function_on_the_path() {
    const HIDING: &str = "CREATE SCHEMA a;
         CREATE TABLE t (x int);
         CREATE PROCEDURE a.tf() LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION public.tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         SET search_path = a, public;";

    let result = parse(&format!("{HIDING} DROP FUNCTION tf;"));
    assert!(
        matches!(&result, Err(Error::DropFunctionNotFound { function_name }) if function_name == "tf"),
        "{result:?}"
    );
    let result = parse(&format!(
        "{HIDING} CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf();"
    ));
    assert!(
        matches!(&result, Err(Error::FunctionNotFoundForTrigger { function_name, .. }) if function_name == "tf"),
        "{result:?}"
    );
}

/// `ALTER FUNCTION` reaches no procedure.
#[test]
fn alter_function_refuses_a_procedure() {
    for alter in [
        "ALTER FUNCTION p(int) SECURITY DEFINER;",
        "ALTER FUNCTION p(int) OWNER TO bob;",
        "ALTER FUNCTION p(int) RENAME TO q;",
    ] {
        let result = parse(&format!(
            "CREATE ROLE bob; CREATE PROCEDURE p(a int) LANGUAGE sql AS 'SELECT 1'; {alter}"
        ));
        assert!(
            matches!(&result, Err(Error::NotAFunction { function_name }) if function_name == "p(INT)"),
            "{alter}: {result:?}"
        );
    }
}

/// Each grant target reaches the routines PostgreSQL 18 lets it reach,
/// `FUNCTION` everything but procedures, `PROCEDURE` procedures alone, and
/// `ROUTINE` both.
#[test]
fn grants_reach_routines_of_their_kind() {
    const BOTH: &str = "CREATE ROLE bob;
         CREATE PROCEDURE p(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'SELECT 1';";
    let executable = |grants: &str| {
        let db = parse(&format!("{BOTH} {grants}")).expect("schema parses");
        (
            routine(&db, "p").is_executable_by_public(&db).unwrap(),
            routine(&db, "f").is_executable_by_public(&db).unwrap(),
        )
    };

    assert_eq!(executable("REVOKE EXECUTE ON PROCEDURE p(int) FROM PUBLIC;"), (false, true));
    assert_eq!(executable("REVOKE EXECUTE ON PROCEDURE p FROM PUBLIC;"), (false, true));
    assert_eq!(executable("REVOKE EXECUTE ON ROUTINE p(int), f(int) FROM PUBLIC;"), (false, false));
    assert_eq!(
        executable("REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC;"),
        (true, false)
    );
    assert_eq!(
        executable("REVOKE EXECUTE ON ALL PROCEDURES IN SCHEMA public FROM PUBLIC;"),
        (false, true)
    );
    assert_eq!(
        executable("REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA public FROM PUBLIC;"),
        (false, false)
    );

    let result = parse(&format!("{BOTH} GRANT EXECUTE ON FUNCTION p(int) TO bob;"));
    assert!(
        matches!(&result, Err(Error::NotAFunction { function_name }) if function_name == "p(INT)"),
        "{result:?}"
    );
    let result = parse(&format!("{BOTH} GRANT EXECUTE ON PROCEDURE f(int) TO bob;"));
    assert!(
        matches!(&result, Err(Error::NotAProcedure { procedure_name }) if procedure_name == "f(INT)"),
        "{result:?}"
    );
    let result = parse(&format!("{BOTH} GRANT EXECUTE ON FUNCTION p TO bob;"));
    assert!(
        matches!(&result, Err(Error::FunctionNotFoundForGrant { function_name }) if function_name == "p"),
        "{result:?}"
    );
}

/// A trigger runs a function, and a procedure of the name is none.
#[test]
fn a_trigger_reaches_no_procedure() {
    let result = parse(
        "CREATE TABLE t (x int);
         CREATE PROCEDURE q() LANGUAGE sql AS 'SELECT 1';
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION q();",
    );
    assert!(
        matches!(&result, Err(Error::FunctionNotFoundForTrigger { function_name, .. }) if function_name == "q"),
        "{result:?}"
    );
}

/// An expression calling a procedure is refused before any clause it
/// carries, as only `CALL` runs one.
#[test]
fn an_expression_calling_a_procedure_is_refused() {
    for stored in [
        "CREATE VIEW v AS SELECT q(1);",
        "CREATE VIEW v AS SELECT q(DISTINCT 1);",
        "CREATE TABLE t (x int DEFAULT q(1));",
        "CREATE TABLE t (x int CHECK (q(x) IS NULL));",
    ] {
        let result =
            parse(&format!("CREATE PROCEDURE q(a int) LANGUAGE sql AS 'SELECT 1'; {stored}"));
        assert!(
            matches!(
                &result,
                Err(Error::MisappliedCall { function_name, rule: Misapplied::ProcedureCalled })
                    if function_name == "q"
            ),
            "{stored}: {result:?}"
        );
        assert_eq!(result.unwrap_err().to_string(), "q is a procedure", "{stored}");
    }
}
