use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

use super::{refuses, refuses_without_counterpart};

#[test]
fn absent_schemas_extensions_relations_and_roles() {
    for (statement, state, message) in [
        ("DROP SCHEMA NoSuch;", "3F000", "schema \"nosuch\" does not exist"),
        ("DROP SCHEMA \"NoSuch\";", "3F000", "schema \"NoSuch\" does not exist"),
        ("DROP EXTENSION NoSuch;", "42704", "extension \"nosuch\" does not exist"),
        ("DROP EXTENSION \"NoSuch\";", "42704", "extension \"NoSuch\" does not exist"),
        ("ALTER SCHEMA NoSuch RENAME TO something;", "3F000", "schema \"nosuch\" does not exist"),
        ("ALTER TABLE NoSuch RENAME TO something;", "42P01", "relation \"nosuch\" does not exist"),
        (
            "ALTER TABLE public.NoSuch ADD COLUMN a int;",
            "42P01",
            "relation \"public.nosuch\" does not exist",
        ),
        (
            "ALTER INDEX public.NoSuch RENAME TO something;",
            "42P01",
            "relation \"public.nosuch\" does not exist",
        ),
        ("ALTER ROLE s8_NoSuch RENAME TO s8_new;", "42704", "role \"s8_nosuch\" does not exist"),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn absent_or_nonvalidatable_constraints() {
    for (statement, state, message) in [
        (
            "ALTER TABLE t DROP CONSTRAINT NoSuch;",
            "42704",
            "constraint \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t VALIDATE CONSTRAINT NoSuch;",
            "42704",
            "constraint \"nosuch\" of relation \"t\" does not exist",
        ),
        (
            "ALTER TABLE t VALIDATE CONSTRAINT pk;",
            "42809",
            "cannot validate constraint \"pk\" of relation \"t\"",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT ne CHECK(a>0) NOT ENFORCED; ALTER TABLE t VALIDATE CONSTRAINT ne;",
            "55000",
            "cannot validate NOT ENFORCED constraint",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT uk UNIQUE(a) NOT VALID;",
            "0A000",
            "UNIQUE constraints cannot be marked NOT VALID",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT pk2 PRIMARY KEY(a) NOT VALID;",
            "0A000",
            "PRIMARY KEY constraints cannot be marked NOT VALID",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT ex EXCLUDE USING btree(a WITH =) NOT VALID;",
            "0A000",
            "EXCLUDE constraints cannot be marked NOT VALID",
        ),
    ] {
        refuses(
            &format!(
                "CREATE TABLE t (a int CONSTRAINT pk PRIMARY KEY, CONSTRAINT ck CHECK(a>0)); {statement}"
            ),
            state,
            message,
        );
    }
    refuses(
        "CREATE TABLE parent (a int); CREATE TABLE child () INHERITS (parent); ALTER TABLE child ADD CONSTRAINT ck CHECK(a>0) NOT VALID; ALTER TABLE parent ADD CONSTRAINT ck CHECK(a>0);",
        "42P17",
        "constraint \"ck\" conflicts with NOT VALID constraint on relation \"child\"",
    );
}

#[test]
fn absent_policies_and_function_targets() {
    for statement in [
        "ALTER POLICY NoSuch ON t RENAME TO other;",
        "ALTER POLICY NoSuch ON public.t RENAME TO other;",
    ] {
        refuses(
            &format!("CREATE TABLE t(a int); {statement}"),
            "42704",
            "policy \"nosuch\" for table \"t\" does not exist",
        );
    }
    for (statement, message) in [
        ("ALTER FUNCTION NoSuch SECURITY DEFINER;", "could not find a function named \"nosuch\""),
        ("ALTER FUNCTION NoSuch() SECURITY DEFINER;", "function nosuch() does not exist"),
        (
            "ALTER FUNCTION public.NoSuch(integer, varchar) SECURITY DEFINER;",
            "function public.nosuch(integer, character varying) does not exist",
        ),
        (
            "ALTER FUNCTION \"NoSuch\"(\"pg_catalog\".\"int4\"[]) SECURITY DEFINER;",
            "function NoSuch(integer[]) does not exist",
        ),
    ] {
        refuses(statement, "42883", message);
    }
    for (statement, state, message) in [
        (
            "ALTER FUNCTION f(integer) SET SCHEMA NoSuch;",
            "3F000",
            "schema \"nosuch\" does not exist",
        ),
        (
            "ALTER FUNCTION f(integer) DEPENDS ON EXTENSION NoSuch;",
            "42704",
            "extension \"nosuch\" does not exist",
        ),
    ] {
        refuses(
            &format!(
                "CREATE FUNCTION f(integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; {statement}"
            ),
            state,
            message,
        );
    }
}

#[test]
fn ambiguous_routine_names_and_signatures() {
    for (statement, kind) in
        [("DROP FUNCTION f;", "function"), ("ALTER FUNCTION f SECURITY DEFINER;", "function")]
    {
        refuses(
            &format!(
                "CREATE FUNCTION f(integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; CREATE FUNCTION f(text) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; {statement}"
            ),
            "42725",
            &format!("{kind} name \"f\" is not unique"),
        );
    }
    refuses(
        "CREATE PROCEDURE p(a integer) LANGUAGE SQL AS 'SELECT 1'; CREATE PROCEDURE p(a text) LANGUAGE SQL AS 'SELECT 1'; DROP PROCEDURE p;",
        "42725",
        "procedure name \"p\" is not unique",
    );
    for (statement, kind, name) in [
        ("DROP PROCEDURE q(int,int);", "procedure", "q"),
        ("GRANT EXECUTE ON ROUTINE public.q(int,int) TO PUBLIC;", "routine", "public.q"),
    ] {
        refuses(
            &format!(
                "CREATE PROCEDURE q(a int, OUT b int) LANGUAGE sql AS 'SELECT 1'; CREATE PROCEDURE q(a int, b int) LANGUAGE sql AS 'SELECT 1'; {statement}"
            ),
            "42725",
            &format!("{kind} name \"{name}\" is not unique"),
        );
    }
}

#[test]
fn occupied_schemas() {
    refuses(
        "CREATE SCHEMA occupied; CREATE TABLE occupied.t(a int); DROP SCHEMA occupied;",
        "2BP01",
        "cannot drop schema occupied because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA \"Mixed Schema\"; CREATE TABLE \"Mixed Schema\".t(a int); DROP SCHEMA \"Mixed Schema\";",
        "2BP01",
        "cannot drop schema Mixed Schema because other objects depend on it",
    );
}

#[test]
fn absent_transactions_and_savepoints() {
    for (statement, command) in [
        ("SAVEPOINT NoSuch;", "SAVEPOINT"),
        ("RELEASE SAVEPOINT NoSuch;", "RELEASE SAVEPOINT"),
        ("ROLLBACK TO SAVEPOINT NoSuch;", "ROLLBACK TO SAVEPOINT"),
        ("COMMIT AND CHAIN;", "COMMIT AND CHAIN"),
        ("ROLLBACK AND CHAIN;", "ROLLBACK AND CHAIN"),
    ] {
        refuses(statement, "25P01", &format!("{command} can only be used in transaction blocks"));
    }
    refuses("BEGIN; RELEASE SAVEPOINT NoSuch;", "3B001", "savepoint \"nosuch\" does not exist");
    refuses(
        "BEGIN; ROLLBACK TO SAVEPOINT \"NoSuch\";",
        "3B001",
        "savepoint \"NoSuch\" does not exist",
    );
}

#[test]
fn model_only_refusals() {
    refuses_without_counterpart::<PostgreSqlDialect>("DO $$ BEGIN NULL; END $$;");
    refuses_without_counterpart::<SQLiteDialect>("BEGIN; BEGIN;");
}

#[test]
fn quoted_and_qualified_missing_objects() {
    for (statement, state, message) in [
        (
            "ALTER SCHEMA \"NoSuch\" RENAME TO something;",
            "3F000",
            "schema \"NoSuch\" does not exist",
        ),
        ("ALTER SCHEMA NoSuch OWNER TO postgres;", "3F000", "schema \"nosuch\" does not exist"),
        (
            "ALTER TABLE \"NoSuch\" RENAME TO something;",
            "42P01",
            "relation \"NoSuch\" does not exist",
        ),
        (
            "ALTER TABLE public.\"NoSuch\" RENAME TO something;",
            "42P01",
            "relation \"public.NoSuch\" does not exist",
        ),
        ("ALTER TABLE \"NoSuch\" ADD COLUMN a int;", "42P01", "relation \"NoSuch\" does not exist"),
        (
            "ALTER INDEX \"NoSuch\" RENAME TO something;",
            "42P01",
            "relation \"NoSuch\" does not exist",
        ),
        (
            "ALTER ROLE \"s8_NoSuch\" RENAME TO s8_new;",
            "42704",
            "role \"s8_NoSuch\" does not exist",
        ),
        ("ALTER ROLE s8_NoSuch LOGIN;", "42704", "role \"s8_nosuch\" does not exist"),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn quoted_constraint_and_policy_names() {
    for (statement, state, message) in [
        (
            "ALTER TABLE public.\"MixedTable\" DROP CONSTRAINT \"NoSuch\";",
            "42704",
            "constraint \"NoSuch\" of relation \"MixedTable\" does not exist",
        ),
        (
            "ALTER TABLE public.\"MixedTable\" VALIDATE CONSTRAINT \"NoSuch\";",
            "42704",
            "constraint \"NoSuch\" of relation \"MixedTable\" does not exist",
        ),
        (
            "ALTER TABLE public.\"MixedTable\" VALIDATE CONSTRAINT \"MixedKey\";",
            "42809",
            "cannot validate constraint \"MixedKey\" of relation \"MixedTable\"",
        ),
        (
            "ALTER POLICY \"NoSuch\" ON public.\"MixedTable\" RENAME TO something;",
            "42704",
            "policy \"NoSuch\" for table \"MixedTable\" does not exist",
        ),
        (
            "ALTER POLICY NoSuch ON \"MixedTable\" USING (true);",
            "42704",
            "policy \"nosuch\" for table \"MixedTable\" does not exist",
        ),
    ] {
        refuses(
            &format!(
                "CREATE TABLE \"MixedTable\" (a int CONSTRAINT \"MixedKey\" UNIQUE, CONSTRAINT \"MixedCheck\" CHECK(a>0)); {statement}"
            ),
            state,
            message,
        );
    }
    refuses(
        "CREATE TABLE \"MixedParent\" (a int); CREATE TABLE \"MixedChild\" () INHERITS (\"MixedParent\"); ALTER TABLE \"MixedChild\" ADD CONSTRAINT \"MixedCheck\" CHECK(a>0) NOT VALID; ALTER TABLE \"MixedParent\" ADD CONSTRAINT \"MixedCheck\" CHECK(a>0);",
        "42P17",
        "constraint \"MixedCheck\" conflicts with NOT VALID constraint on relation \"MixedChild\"",
    );
}

#[test]
fn quoted_function_and_procedure_names() {
    for (statement, state, message) in [
        (
            "DROP FUNCTION public.\"MixedFunc\";",
            "42725",
            "function name \"public.MixedFunc\" is not unique",
        ),
        (
            "ALTER FUNCTION public.\"MixedFunc\" SECURITY DEFINER;",
            "42725",
            "function name \"public.MixedFunc\" is not unique",
        ),
        (
            "ALTER FUNCTION \"MixedFunc\"(integer) SET SCHEMA \"NoSuch\";",
            "3F000",
            "schema \"NoSuch\" does not exist",
        ),
        (
            "ALTER FUNCTION \"MixedFunc\"(integer) NO DEPENDS ON EXTENSION \"NoSuch\";",
            "42704",
            "extension \"NoSuch\" does not exist",
        ),
    ] {
        refuses(
            &format!(
                "CREATE FUNCTION \"MixedFunc\"(integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; CREATE FUNCTION \"MixedFunc\"(text) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; {statement}"
            ),
            state,
            message,
        );
    }
    refuses(
        "CREATE PROCEDURE \"MixedProc\"(a integer) LANGUAGE SQL AS 'SELECT 1'; CREATE PROCEDURE \"MixedProc\"(a text) LANGUAGE SQL AS 'SELECT 1'; DROP PROCEDURE public.\"MixedProc\";",
        "42725",
        "procedure name \"public.MixedProc\" is not unique",
    );
    for (statement, kind) in [
        ("DROP PROCEDURE public.\"MixedAmb\"(int,int);", "procedure"),
        ("GRANT EXECUTE ON ROUTINE public.\"MixedAmb\"(int,int) TO PUBLIC;", "routine"),
    ] {
        refuses(
            &format!(
                "CREATE PROCEDURE \"MixedAmb\"(a int, OUT b int) LANGUAGE sql AS 'SELECT 1'; CREATE PROCEDURE \"MixedAmb\"(a int, b int) LANGUAGE sql AS 'SELECT 1'; {statement}"
            ),
            "42725",
            &format!("{kind} name \"public.MixedAmb\" is not unique"),
        );
    }
}

#[test]
fn quoted_missing_savepoints() {
    refuses("BEGIN; RELEASE SAVEPOINT \"NoSuch\";", "3B001", "savepoint \"NoSuch\" does not exist");
    refuses("BEGIN; ROLLBACK TO SAVEPOINT NoSuch;", "3B001", "savepoint \"nosuch\" does not exist");
    refuses("END AND CHAIN;", "25P01", "COMMIT AND CHAIN can only be used in transaction blocks");
}

#[test]
fn missing_namespaces_and_policy_tables() {
    for statement in [
        "ALTER FUNCTION \"NoSuchSchema\".NoSuch SECURITY DEFINER;",
        "ALTER FUNCTION \"NoSuchSchema\".NoSuch(integer) SECURITY DEFINER;",
        "ALTER TABLE \"NoSuchSchema\".t RENAME TO something;",
        "ALTER TABLE \"NoSuchSchema\".t ADD COLUMN a int;",
        "ALTER INDEX \"NoSuchSchema\".t RENAME TO something;",
        "ALTER POLICY NoSuch ON \"NoSuchSchema\".t RENAME TO other;",
    ] {
        refuses(statement, "3F000", "schema \"NoSuchSchema\" does not exist");
    }
    refuses(
        "ALTER POLICY NoSuch ON missing RENAME TO other;",
        "42P01",
        "relation \"missing\" does not exist",
    );
    refuses(
        "ALTER POLICY NoSuch ON public.missing USING (true);",
        "42P01",
        "relation \"public.missing\" does not exist",
    );
}

#[test]
fn function_signatures_use_input_types_and_visibility() {
    refuses(
        "ALTER FUNCTION NoSuch(IN a integer, OUT b text) SECURITY DEFINER;",
        "42883",
        "function nosuch(integer) does not exist",
    );
    refuses(
        "ALTER FUNCTION public.NoSuch(VARIADIC integer[]) SECURITY DEFINER;",
        "42883",
        "function public.nosuch(integer[]) does not exist",
    );
    refuses(
        "CREATE SCHEMA occupied; CREATE DOMAIN d AS int; CREATE DOMAIN occupied.d AS int; ALTER FUNCTION NoSuch(public.d, occupied.d[]) SECURITY DEFINER;",
        "42883",
        "function nosuch(d, occupied.d[]) does not exist",
    );
    refuses(
        "CREATE FUNCTION NoSuch(integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'; ALTER FUNCTION NoSuch(text) SECURITY DEFINER;",
        "42883",
        "function nosuch(text) does not exist",
    );
}

#[test]
fn schema_model_limitations_have_no_counterpart() {
    use sql_traits::errors::{Error, ObjectKind};
    use sqlparser::dialect::{DatabricksDialect, DuckDbDialect, TeradataDialect};

    refuses_without_counterpart::<PostgreSqlDialect>(
        "ALTER AGGREGATE sum(integer) OWNER TO postgres;",
    );
    refuses_without_counterpart::<PostgreSqlDialect>(
        "ALTER AGGREGATE sum(integer) RENAME TO s8_sum;",
    );
    refuses_without_counterpart::<PostgreSqlDialect>(
        "CREATE SCHEMA s; CREATE FUNCTION s.f(x INT) RETURNS INT LANGUAGE SQL AS 'SELECT x'; CREATE FUNCTION s.f(x TEXT) RETURNS INT LANGUAGE SQL AS 'SELECT 1'; CREATE VIEW tv AS SELECT 1 AS a; CREATE RULE rv AS ON INSERT TO tv DO INSTEAD SELECT s.f(new.a); ALTER FUNCTION s.f(INT) RENAME TO g;",
    );
    refuses_without_counterpart::<DuckDbDialect>("BEGIN; SAVEPOINT s;");
    refuses_without_counterpart::<DatabricksDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT);");
    refuses_without_counterpart::<TeradataDialect>(
        "BEGIN TRANSACTION; CREATE TABLE t (id INT); CREATE TABLE u (id INT);",
    );
    refuses_without_counterpart::<SQLiteDialect>("COMMIT;");
    for error in [
        Error::TableDependencyGraph { catalog_name: "s8".into(), reason: "invalid graph".into() },
        Error::CyclicTableDependencies { catalog_name: "s8".into() },
        Error::UnnamedObject { object_kind: ObjectKind::Table },
    ] {
        assert_eq!(error.postgres_error(), None);
    }
}

#[test]
fn policies_require_tables() {
    for (definition, target) in [
        ("CREATE VIEW view_target AS SELECT 1 AS a;", "view_target"),
        ("CREATE MATERIALIZED VIEW materialized_target AS SELECT 1 AS a;", "materialized_target"),
        ("CREATE SEQUENCE sequence_target;", "sequence_target"),
        (
            "CREATE TABLE policy_host(a int); CREATE INDEX index_target ON policy_host(a);",
            "index_target",
        ),
    ] {
        refuses(
            &format!("{definition} ALTER POLICY NoSuch ON {target} USING (true);"),
            "42809",
            &format!("\"{target}\" is not a table"),
        );
    }
    refuses(
        "CREATE VIEW \"MixedViewTarget\" AS SELECT 1 AS a; ALTER POLICY NoSuch ON public.\"MixedViewTarget\" USING (true);",
        "42809",
        "\"MixedViewTarget\" is not a table",
    );
}

#[test]
fn built_in_namespaces_exist_without_declarations() {
    for schema in ["information_schema", "pg_toast"] {
        refuses(
            &format!("ALTER TABLE {schema}.NoSuch ADD COLUMN a int;"),
            "42P01",
            &format!("relation \"{schema}.nosuch\" does not exist"),
        );
    }
}

#[test]
fn metadata_alterations_resolve_their_targets() {
    for (statement, state, message) in [
        (
            "ALTER TABLE NoSuch ENABLE ROW LEVEL SECURITY;",
            "42P01",
            "relation \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE public.NoSuch OWNER TO postgres;",
            "42P01",
            "relation \"public.nosuch\" does not exist",
        ),
        ("ALTER TABLE \"NoSuch\" SET UNLOGGED;", "42P01", "relation \"NoSuch\" does not exist"),
        (
            "ALTER FUNCTION NoSuch(integer) OWNER TO postgres;",
            "42883",
            "function nosuch(integer) does not exist",
        ),
        (
            "ALTER FUNCTION NoSuch(integer) IMMUTABLE;",
            "42883",
            "function nosuch(integer) does not exist",
        ),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn replica_identity_indexes() {
    let over = |table: &str, index: &str, name: &str| {
        format!(
            "CREATE TABLE t ({table}); {index} ALTER TABLE t REPLICA IDENTITY USING INDEX {name};"
        )
    };
    for (statement, state, message) in [
        (
            over("a INT NOT NULL", "", "nosuch"),
            "42704",
            "index \"nosuch\" for table \"t\" does not exist",
        ),
        (
            over("a INT NOT NULL", "", "\"NoSuch\""),
            "42704",
            "index \"NoSuch\" for table \"t\" does not exist",
        ),
        (
            over(
                "a INT NOT NULL",
                "CREATE TABLE u (a INT NOT NULL); CREATE UNIQUE INDEX \"Ui\" ON u (a);",
                "\"Ui\"",
            ),
            "42809",
            "\"Ui\" is not an index for table \"t\"",
        ),
        (
            over("a INT NOT NULL", "CREATE INDEX \"I\" ON t (a);", "\"I\""),
            "42809",
            "cannot use non-unique index \"I\" as replica identity",
        ),
        (
            over("a INT NOT NULL", "CREATE UNIQUE INDEX i ON t (a) WHERE a > 0;", "i"),
            "0A000",
            "cannot use partial index \"i\" as replica identity",
        ),
        (
            over("a INT NOT NULL", "CREATE UNIQUE INDEX i ON t ((a + 1));", "i"),
            "0A000",
            "cannot use expression index \"i\" as replica identity",
        ),
        (
            over("a INT NOT NULL", "ALTER TABLE t ADD CONSTRAINT i UNIQUE (a) DEFERRABLE;", "i"),
            "0A000",
            "cannot use non-immediate index \"i\" as replica identity",
        ),
        (
            over("a INT NOT NULL, \"B\" INT", "CREATE UNIQUE INDEX i ON t (a, \"B\");", "i"),
            "42809",
            "index \"i\" cannot be used as replica identity because column \"B\" is nullable",
        ),
        (
            over(
                "a INT NOT NULL, \"B\" INT",
                "ALTER TABLE t ADD CONSTRAINT nn NOT NULL \"B\" NOT VALID; CREATE UNIQUE INDEX \"I\" ON t (a, \"B\");",
                "\"I\"",
            ),
            "55000",
            "cannot use index \"I\" as replica identity",
        ),
    ] {
        refuses(&statement, state, message);
    }
    refuses(
        &format!(
            "{} ALTER TABLE t ALTER COLUMN \"A\" DROP NOT NULL;",
            over("\"A\" INT NOT NULL", "CREATE UNIQUE INDEX i ON t (\"A\");", "i")
        ),
        "42P16",
        "column \"A\" is in index used as replica identity",
    );
}

#[test]
fn dropped_indexes_and_keys_others_depend_on() {
    for (statement, state, message) in [
        (
            "CREATE TABLE \"R\" (id INT, CONSTRAINT \"K\" UNIQUE (id)); CREATE TABLE f (r INT REFERENCES \"R\" (id)); ALTER TABLE \"R\" DROP CONSTRAINT \"K\";",
            "2BP01",
            "cannot drop constraint K on table \"R\" because other objects depend on it",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY); CREATE TABLE f (r INT REFERENCES r); ALTER TABLE r DROP CONSTRAINT r_pkey;",
            "2BP01",
            "cannot drop constraint r_pkey on table r because other objects depend on it",
        ),
        (
            "CREATE TABLE r (id INT); CREATE UNIQUE INDEX \"Ix\" ON r (id); CREATE TABLE f (r INT REFERENCES r (id)); DROP INDEX \"Ix\";",
            "2BP01",
            "cannot drop index \"Ix\" because other objects depend on it",
        ),
        (
            "CREATE TABLE \"T\" (a INT, CONSTRAINT \"K\" UNIQUE (a)); DROP INDEX \"K\";",
            "2BP01",
            "cannot drop index \"K\" because constraint K on table \"T\" requires it",
        ),
        (
            "CREATE TABLE t (a INT, CONSTRAINT k UNIQUE (a)); DROP INDEX k CASCADE;",
            "2BP01",
            "cannot drop index k because constraint k on table t requires it",
        ),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn adopted_indexes() {
    for (statement, state, message) in [
        (
            "CREATE TABLE t (a INT); CREATE INDEX \"I\" ON t (a); ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "42809",
            "\"I\" is not a unique index",
        ),
        (
            "CREATE TABLE t (a INT); CREATE UNIQUE INDEX \"I\" ON t (a) WHERE a > 0; ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "42809",
            "\"I\" is a partial index",
        ),
        (
            "CREATE TABLE t (a INT); CREATE UNIQUE INDEX \"I\" ON t ((a + 1)); ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "42809",
            "index \"I\" contains expressions",
        ),
        (
            "CREATE TABLE t (a INT); ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX \"NoSuch\";",
            "42704",
            "index \"NoSuch\" does not exist",
        ),
        (
            "CREATE TABLE \"T\" (a INT); CREATE TABLE u (a INT); CREATE UNIQUE INDEX \"I\" ON u (a); ALTER TABLE \"T\" ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "55000",
            "index \"I\" does not belong to table \"T\"",
        ),
        (
            "CREATE TABLE t (a INT, CONSTRAINT \"K\" UNIQUE (a)); ALTER TABLE t ADD CONSTRAINT k2 UNIQUE USING INDEX \"K\";",
            "55000",
            "index \"K\" is already associated with a constraint",
        ),
        (
            "CREATE TABLE p (a INT) PARTITION BY RANGE (a); CREATE UNIQUE INDEX i ON p (a); ALTER TABLE p ADD CONSTRAINT k UNIQUE USING INDEX i;",
            "0A000",
            "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
        ),
        (
            "CREATE TABLE t (a INT, b INT); CREATE UNIQUE INDEX \"I\" ON t (a, b DESC); ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "42809",
            "index \"I\" column number 2 does not have default sorting behavior",
        ),
        (
            "CREATE TABLE p (a INT) PARTITION BY RANGE (a); CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10); CREATE UNIQUE INDEX \"I\" ON ONLY p (a); ALTER TABLE p ADD CONSTRAINT k UNIQUE USING INDEX \"I\";",
            "55000",
            "index \"I\" is not valid",
        ),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn generation_expression_drops() {
    for (statement, state, message) in [
        (
            "CREATE TABLE \"T\" (\"A\" INT); ALTER TABLE \"T\" ALTER COLUMN \"A\" DROP EXPRESSION;",
            "55000",
            "column \"A\" of relation \"T\" is not a generated column",
        ),
        (
            "CREATE TABLE p (a INT, \"B\" INT GENERATED ALWAYS AS (a * 2) STORED); CREATE TABLE \"C\" () INHERITS (p); ALTER TABLE \"C\" ALTER COLUMN \"B\" DROP EXPRESSION;",
            "42P16",
            "cannot drop generation expression from inherited column",
        ),
        (
            "CREATE TABLE p (a INT, \"B\" INT GENERATED ALWAYS AS (a * 2) STORED); CREATE TABLE c () INHERITS (p); ALTER TABLE ONLY p ALTER COLUMN \"B\" DROP EXPRESSION;",
            "0A000",
            "ALTER TABLE / DROP EXPRESSION must be applied to child tables too",
        ),
    ] {
        refuses(statement, state, message);
    }
}

/// The PostgreSQL parser takes no virtual generated column, so the statements
/// reach the PostgreSQL model through the generic one.
#[test]
fn virtual_generation_drops() {
    use sql_traits::prelude::*;
    use sqlparser::{dialect::GenericDialect, parser::Parser};

    let sql = "CREATE TABLE \"T\" (a INT, \"B\" INT GENERATED ALWAYS AS (a * 2) VIRTUAL); ALTER TABLE \"T\" ALTER COLUMN \"B\" DROP EXPRESSION;";
    let Ok(statements) = Parser::parse_sql(&GenericDialect {}, sql) else {
        panic!("the generic parser refuses {sql}");
    };
    let Err(error) = ParserDB::from_statements_with_dialect(
        statements,
        "postgres".to_owned(),
        SqlparserDialect::PostgreSql,
    ) else {
        panic!("ParserDB accepts {sql}");
    };
    let Some(refusal) = error.postgres_error() else {
        panic!("{error:?} answers no PostgreSQL error for {sql}");
    };
    assert_eq!(
        (refusal.sqlstate().as_str(), refusal.message()),
        ("0A000", "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns"),
    );
}

#[test]
fn renames_beside_other_actions() {
    for (statement, state, message) in [
        (
            "CREATE TABLE t (a INT); ALTER TABLE t RENAME COLUMN a TO x, ADD COLUMN c INT;",
            "42601",
            "syntax error at or near \",\"",
        ),
        (
            "CREATE TABLE t (a INT); ALTER TABLE t ADD COLUMN c INT, rename COLUMN a TO x;",
            "42601",
            "syntax error at or near \"rename\"",
        ),
        (
            "CREATE TABLE t (a INT, CONSTRAINT k CHECK (a > 0)); ALTER TABLE t ADD COLUMN c INT, RENAME CONSTRAINT k TO j;",
            "42601",
            "syntax error at or near \"RENAME\"",
        ),
    ] {
        refuses(statement, state, message);
    }
}
