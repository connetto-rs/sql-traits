#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use super::refuses;

#[test]
fn missing_schemas() {
    refuses("CREATE TABLE Missing.t (a int);", "3F000", "schema \"missing\" does not exist");
    refuses("CREATE COLLATION Missing.c FROM \"C\";", "3F000", "schema \"missing\" does not exist");
    refuses(
        "CREATE EXTENSION hstore SCHEMA Missing;",
        "3F000",
        "schema \"missing\" does not exist",
    );
    refuses(
        "SET search_path = ''; CREATE TABLE noschema(a int);",
        "3F000",
        "no schema has been selected to create in",
    );
    refuses(
        "SET search_path = ''; CREATE EXTENSION hstore;",
        "3F000",
        "no schema has been selected to create in",
    );
    refuses(
        "CREATE TABLE pg_catalog.forbidden(a int);",
        "42501",
        "permission denied to create \"pg_catalog.forbidden\"",
    );
}

#[test]
fn dependent_relations() {
    refuses(
        "CREATE TABLE t(a int PRIMARY KEY); CREATE TABLE child(a int REFERENCES t); DROP TABLE t;",
        "2BP01",
        "cannot drop table t because other objects depend on it",
    );
    refuses(
        "CREATE TABLE t(a int); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t DROP COLUMN a;",
        "2BP01",
        "cannot drop column a of table t because other objects depend on it",
    );
    refuses(
        "CREATE TABLE t(a int); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t ALTER COLUMN a TYPE int;",
        "0A000",
        "cannot alter type of a column used by a view or rule",
    );
}

#[test]
fn duplicate_columns() {
    refuses(
        "CREATE TABLE t (a int); ALTER TABLE t ADD COLUMN a int;",
        "42701",
        "column \"a\" of relation \"t\" already exists",
    );
    refuses("CREATE TABLE t (a int, a int);", "42701", "column \"a\" specified more than once");
}

#[test]
fn role_memberships_and_grants() {
    refuses(
        "CREATE TABLE t(a int); GRANT SELECT ON t TO s4_Missing;",
        "42704",
        "role \"s4_missing\" does not exist",
    );
    refuses(
        "CREATE ROLE s4_a; GRANT s4_Missing TO s4_a;",
        "42704",
        "role \"s4_missing\" does not exist",
    );
    refuses(
        "CREATE ROLE s4_a; GRANT s4_a TO s4_a;",
        "0LP01",
        "role \"s4_a\" is a member of role \"s4_a\"",
    );
    refuses(
        "CREATE ROLE s4_a; CREATE ROLE s4_b; GRANT s4_a TO s4_b WITH nonsense TRUE;",
        "42601",
        "unrecognized role option \"nonsense\"",
    );
    refuses(
        "CREATE ROLE s4_a; CREATE ROLE s4_b; CREATE ROLE s4_c; GRANT s4_a TO s4_b WITH ADMIN TRUE; GRANT s4_a TO s4_c GRANTED BY s4_b; REVOKE s4_a FROM s4_b;",
        "2BP01",
        "dependent privileges exist",
    );
    refuses(
        "GRANT SELECT ON public.Missing TO PUBLIC;",
        "42P01",
        "relation \"public.missing\" does not exist",
    );
    refuses("DROP TABLE public.Missing;", "42P01", "table \"missing\" does not exist");
}

#[test]
fn missing_and_misnamed_routines() {
    refuses("DROP FUNCTION missing;", "42883", "could not find a function named \"missing\"");
    refuses("DROP FUNCTION missing(int);", "42883", "function missing(integer) does not exist");
    refuses("DROP PROCEDURE missing;", "42883", "could not find a procedure named \"missing\"");
    refuses("DROP PROCEDURE missing(int);", "42883", "procedure missing(integer) does not exist");
    refuses(
        "GRANT EXECUTE ON FUNCTION missing TO PUBLIC;",
        "42883",
        "could not find a function named \"missing\"",
    );
    refuses(
        "GRANT EXECUTE ON FUNCTION missing(int) TO PUBLIC;",
        "42883",
        "function missing(integer) does not exist",
    );
    refuses(
        "GRANT EXECUTE ON PROCEDURE public.missing TO PUBLIC;",
        "42883",
        "could not find a procedure named \"public.missing\"",
    );
    refuses(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE SQL RETURN $1; CREATE FUNCTION f(text) RETURNS text LANGUAGE SQL RETURN $1; GRANT EXECUTE ON FUNCTION f TO PUBLIC;",
        "42725",
        "function name \"f\" is not unique",
    );
    refuses(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE SQL RETURN $1; GRANT SELECT ON FUNCTION f(int) TO PUBLIC;",
        "0LP01",
        "invalid privilege type SELECT for function",
    );
    refuses(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE SQL RETURN $1; DROP PROCEDURE f(int);",
        "42809",
        "f(integer) is not a procedure",
    );
    refuses(
        "CREATE PROCEDURE p(a int) LANGUAGE SQL AS 'SELECT 1'; DROP FUNCTION p(int);",
        "42809",
        "p(integer) is not a function",
    );
}

#[test]
fn unavailable_parser_message() {
    super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>("SELECT FROM;");
}

#[test]
fn function_dependents() {
    for dependent in [
        "CREATE TABLE t(a int DEFAULT f(1));",
        "CREATE TABLE t(a int, b int GENERATED ALWAYS AS (f(a)) STORED);",
        "CREATE TABLE t(a int CHECK(f(a)>0));",
        "CREATE TABLE t(a int); CREATE INDEX i ON t(f(a));",
        "CREATE VIEW v AS SELECT f(1);",
        "CREATE MATERIALIZED VIEW v AS SELECT f(1);",
        "CREATE TABLE t(a int); CREATE POLICY p ON t USING(f(a)>0);",
        "CREATE TABLE t(a int); CREATE RULE r AS ON INSERT TO t DO ALSO SELECT f(1);",
        "CREATE FUNCTION g() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT f(1); END;",
        "CREATE DOMAIN d AS int DEFAULT f(1);",
        "CREATE TABLE t(a int) PARTITION BY RANGE(f(a));",
    ] {
        refuses(
            &format!(
                "CREATE FUNCTION f(int) RETURNS int LANGUAGE SQL IMMUTABLE RETURN $1; {dependent} DROP FUNCTION f(int);"
            ),
            "2BP01",
            "cannot drop function f(integer) because other objects depend on it",
        );
    }
    refuses(
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'; CREATE TABLE t(a int); CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f(); DROP FUNCTION f();",
        "2BP01",
        "cannot drop function f() because other objects depend on it",
    );
    refuses(
        "CREATE FUNCTION f(int,int) RETURNS int LANGUAGE SQL IMMUTABLE RETURN $1; CREATE OPERATOR <<< (FUNCTION=f, LEFTARG=int, RIGHTARG=int); DROP FUNCTION f(int,int);",
        "2BP01",
        "cannot drop function f(integer,integer) because other objects depend on it",
    );
    refuses(
        "CREATE EXTENSION \"uuid-ossp\"; CREATE TABLE t(a uuid DEFAULT uuid_generate_v4()); DROP EXTENSION \"uuid-ossp\";",
        "2BP01",
        "cannot drop extension uuid-ossp because other objects depend on it",
    );
    refuses(
        "CREATE EXTENSION \"uuid-ossp\"; DROP FUNCTION uuid_generate_v4();",
        "2BP01",
        "cannot drop function uuid_generate_v4() because extension uuid-ossp requires it",
    );
    refuses(
        "CREATE TYPE intrange AS RANGE(subtype=int); DROP FUNCTION intrange(int,int);",
        "2BP01",
        "cannot drop function intrange(integer,integer) because type intrange requires it",
    );
    refuses(
        "CREATE TYPE \"R\" AS RANGE(subtype=int); DROP FUNCTION \"R\"(int,int);",
        "2BP01",
        "cannot drop function \"R\"(integer,integer) because type \"R\" requires it",
    );
}

#[test]
fn dependent_column_type_changes() {
    for (dependent, description) in [
        (
            "CREATE FUNCTION dependent() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT a FROM t; END;",
            "by a function or procedure",
        ),
        ("CREATE POLICY pol ON t USING(a > 0);", "in a policy definition"),
        (
            "CREATE FUNCTION trig() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'; CREATE TRIGGER trg BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION trig();",
            "in a trigger definition",
        ),
    ] {
        refuses(
            &format!("CREATE TABLE t(a int); {dependent} ALTER TABLE t ALTER COLUMN a TYPE int;"),
            "0A000",
            &format!("cannot alter type of a column used {description}"),
        );
    }
}

#[test]
fn bare_procedure_bodies() {
    refuses("CREATE PROCEDURE p() AS SELECT 1;", "42601", "syntax error at or near \"SELECT\"");
    refuses(
        "CREATE PROCEDURE p() AS BEGIN SELECT 1; END;",
        "42601",
        "syntax error at or near \"BEGIN\"",
    );
}

#[test]
fn quoted_and_qualified_dependencies() {
    refuses(
        "CREATE TABLE \"T\"(\"A\" int PRIMARY KEY); CREATE TABLE child(a int REFERENCES \"T\"); DROP TABLE \"T\";",
        "2BP01",
        "cannot drop table \"T\" because other objects depend on it",
    );
    refuses(
        "CREATE TABLE \"T\"(\"A\" int); CREATE VIEW v AS SELECT \"A\" FROM \"T\"; ALTER TABLE \"T\" DROP COLUMN \"A\";",
        "2BP01",
        "cannot drop column A of table \"T\" because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TABLE s.t(a int PRIMARY KEY); CREATE TABLE child(a int REFERENCES s.t); DROP TABLE s.t;",
        "2BP01",
        "cannot drop table s.t because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TABLE s.p(id int); CREATE TABLE c() INHERITS (s.p); DROP TABLE s.p;",
        "2BP01",
        "cannot drop table s.p because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int GENERATED ALWAYS AS IDENTITY); DROP SEQUENCE s.t_id_seq;",
        "2BP01",
        "cannot drop sequence s.t_id_seq because column id of table s.t requires it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE SEQUENCE s.q; CREATE TABLE s.t (id int DEFAULT nextval('s.q')); DROP SEQUENCE s.q;",
        "2BP01",
        "cannot drop sequence s.q because other objects depend on it",
    );
    refuses(
        "CREATE FUNCTION \"F\"(int) RETURNS int LANGUAGE SQL IMMUTABLE RETURN $1; CREATE VIEW v AS SELECT \"F\"(1); DROP FUNCTION \"F\"(int);",
        "2BP01",
        "cannot drop function \"F\"(integer) because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE FUNCTION s.f(int) RETURNS int LANGUAGE SQL IMMUTABLE RETURN $1; CREATE VIEW v AS SELECT s.f(1); DROP FUNCTION s.f(int);",
        "2BP01",
        "cannot drop function s.f(integer) because other objects depend on it",
    );
}

#[test]
fn procedure_and_routine_privileges() {
    for (keyword, kind) in [("PROCEDURE", "procedure"), ("ROUTINE", "routine")] {
        refuses(
            &format!(
                "CREATE PROCEDURE p(a int) LANGUAGE SQL AS 'SELECT 1'; CREATE PROCEDURE p(a text) LANGUAGE SQL AS 'SELECT 1'; GRANT EXECUTE ON {keyword} p TO PUBLIC;"
            ),
            "42725",
            &format!("{kind} name \"p\" is not unique"),
        );
    }
    refuses(
        "CREATE PROCEDURE p(a int) LANGUAGE SQL AS 'SELECT 1'; GRANT SELECT ON PROCEDURE p(int) TO PUBLIC;",
        "0LP01",
        "invalid privilege type SELECT for procedure",
    );
    refuses(
        "CREATE FUNCTION f() RETURNS int LANGUAGE SQL RETURN 1; GRANT SELECT(a) ON FUNCTION f() TO PUBLIC;",
        "0LP01",
        "column privileges are only valid for relations",
    );
    refuses(
        "CREATE FUNCTION f() RETURNS int LANGUAGE SQL RETURN 1; GRANT DROP ON FUNCTION f() TO PUBLIC;",
        "42601",
        "unrecognized privilege type \"drop\"",
    );
    refuses(
        "CREATE FUNCTION f() RETURNS int LANGUAGE SQL RETURN 1; GRANT USAGE ON ROUTINE f() TO PUBLIC;",
        "0LP01",
        "invalid privilege type USAGE for routine",
    );
}

#[test]
fn type_function_dependents() {
    refuses(
        "CREATE TYPE span AS RANGE (subtype=int4); CREATE TABLE t(x int4 CHECK(span(x,x+1) IS NOT NULL)); DROP TYPE span;",
        "2BP01",
        "cannot drop type span because other objects depend on it",
    );
    refuses(
        "CREATE FUNCTION f(int,int) RETURNS float8 LANGUAGE SQL IMMUTABLE RETURN ($1-$2)::float8; CREATE TYPE r AS RANGE (subtype=int, subtype_diff=f); DROP FUNCTION f(int,int);",
        "2BP01",
        "cannot drop function f(integer,integer) because other objects depend on it",
    );
    refuses(
        "CREATE TYPE span AS RANGE (subtype=int4); DROP FUNCTION span_multirange();",
        "2BP01",
        "cannot drop function span_multirange() because type span_multirange requires it",
    );
    refuses(
        "CREATE TYPE span AS RANGE (subtype=int4); DROP FUNCTION span_multirange(VARIADIC span[]);",
        "2BP01",
        "cannot drop function span_multirange(span[]) because type span_multirange requires it",
    );
}

#[test]
fn qualified_missing_objects() {
    for statement in [
        "DROP TABLE missing.t;",
        "GRANT SELECT ON missing.t TO PUBLIC;",
        "DROP FUNCTION missing.f();",
        "GRANT EXECUTE ON FUNCTION missing.f() TO PUBLIC;",
    ] {
        refuses(statement, "3F000", "schema \"missing\" does not exist");
    }
    refuses(
        "DROP FUNCTION public.Missing(int,text);",
        "42883",
        "function public.missing(integer, text) does not exist",
    );
    refuses(
        "DROP FUNCTION public.\"Missing\";",
        "42883",
        "could not find a function named \"public.Missing\"",
    );
    refuses(
        "DROP PROCEDURE public.\"Missing\"(int,text);",
        "42883",
        "procedure public.Missing(integer, text) does not exist",
    );
}

#[test]
fn relation_placement_families() {
    for statement in [
        "CREATE VIEW v AS SELECT 1;",
        "CREATE SEQUENCE s;",
        "CREATE FUNCTION f() RETURNS int LANGUAGE SQL RETURN 1;",
    ] {
        refuses(
            &format!("SET search_path=''; {statement}"),
            "3F000",
            "no schema has been selected to create in",
        );
    }
    for statement in [
        "CREATE VIEW pg_catalog.v AS SELECT 1;",
        "CREATE MATERIALIZED VIEW pg_catalog.v AS SELECT 1;",
    ] {
        refuses(statement, "42501", "permission denied to create \"pg_catalog.v\"");
    }
    refuses(
        "CREATE SEQUENCE pg_catalog.s;",
        "42501",
        "permission denied to create \"pg_catalog.s\"",
    );
    for dependent in [
        "CREATE MATERIALIZED VIEW v AS SELECT a FROM t;",
        "CREATE TABLE d(a int); CREATE RULE r AS ON INSERT TO d DO ALSO SELECT a FROM t;",
    ] {
        refuses(
            &format!("CREATE TABLE t(a int); {dependent} ALTER TABLE t ALTER COLUMN a TYPE int;"),
            "0A000",
            "cannot alter type of a column used by a view or rule",
        );
    }
}

#[test]
fn model_only_errors() {
    use sql_traits::prelude::*;
    let error = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<sqlparser::dialect::PostgreSqlDialect>(
            "GRANT ALL PRIVILEGES ON elsewhere TO r; REVOKE DELETE ON elsewhere FROM r;",
        )
        .expect_err("the open world refuses the revoke");
    assert!(matches!(error, sql_traits::errors::Error::UnsupportedRevoke { .. }));
    assert_eq!(error.postgres_error(), None);
    super::refuses_without_counterpart::<sqlparser::dialect::GenericDialect>(
        "CREATE TABLE t(a int PRIMARY KEY); ALTER TABLE t DROP PRIMARY KEY;",
    );
    #[cfg(feature = "std")]
    assert_eq!(
        sql_traits::errors::Error::IoError(std::io::Error::other("read failed")).postgres_error(),
        None
    );
    #[cfg(feature = "git")]
    assert_eq!(
        sql_traits::errors::Error::GitError(git2::Error::from_str("read failed")).postgres_error(),
        None
    );
}

#[test]
fn unavailable_cast_parser_message() {
    super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>(
        "CREATE TYPE mood AS ENUM ('ok'); CREATE FUNCTION f(mood) RETURNS text LANGUAGE SQL IMMUTABLE RETURN $1::text; CREATE CAST (mood AS text) WITH FUNCTION f(mood); DROP FUNCTION f(mood);",
    );
}

#[test]
fn quoted_and_folded_names() {
    for statement in [
        "CREATE TABLE \"Missing\".t(a int);",
        "CREATE COLLATION \"Missing\".c FROM \"C\";",
        "CREATE EXTENSION hstore SCHEMA \"Missing\";",
    ] {
        refuses(statement, "3F000", "schema \"Missing\" does not exist");
    }
    refuses(
        "CREATE TABLE pg_catalog.\"Upper\"(a int);",
        "42501",
        "permission denied to create \"pg_catalog.Upper\"",
    );
    refuses(
        "GRANT SELECT ON \"Missing\" TO PUBLIC;",
        "42P01",
        "relation \"Missing\" does not exist",
    );
    refuses("DROP TABLE \"Missing\";", "42P01", "table \"Missing\" does not exist");
    refuses(
        "CREATE TABLE t(a int); GRANT SELECT ON t TO \"s4_Missing\";",
        "42704",
        "role \"s4_Missing\" does not exist",
    );
    refuses(
        "CREATE ROLE s4_a; GRANT \"s4_Missing\" TO s4_a;",
        "42704",
        "role \"s4_Missing\" does not exist",
    );
    refuses(
        "CREATE ROLE s4_a; CREATE ROLE s4_b; GRANT s4_a TO s4_b WITH \"Nonsense\" TRUE;",
        "42601",
        "unrecognized role option \"Nonsense\"",
    );
    refuses(
        "CREATE TABLE \"Upper\"(\"Column\" int); ALTER TABLE \"Upper\" ADD COLUMN \"Column\" int;",
        "42701",
        "column \"Column\" of relation \"Upper\" already exists",
    );
    refuses(
        "CREATE TABLE Mixed(Mixed int, mixed int);",
        "42701",
        "column \"mixed\" specified more than once",
    );
    refuses(
        "CREATE VIEW \"Upper\" AS SELECT 1 AS \"Column\", 2 AS \"Column\";",
        "42701",
        "column \"Column\" specified more than once",
    );
    refuses(
        "CREATE TABLE t(a int PRIMARY KEY); CREATE TABLE child(a int REFERENCES t); ALTER TABLE t DROP COLUMN a;",
        "2BP01",
        "cannot drop column a of table t because other objects depend on it",
    );
}

#[test]
fn grants_on_non_grantable_relations() {
    refuses(
        "CREATE TABLE t(a int); CREATE INDEX i ON t(a); GRANT SELECT ON i TO PUBLIC;",
        "42809",
        "\"i\" is an index",
    );
    refuses(
        "CREATE TABLE t(a int PRIMARY KEY); GRANT SELECT ON t_pkey TO PUBLIC;",
        "42809",
        "\"t_pkey\" is an index",
    );
    refuses(
        "CREATE TYPE c AS (a int); GRANT SELECT ON c TO PUBLIC;",
        "42809",
        "\"c\" is a composite type",
    );
    refuses(
        "CREATE TABLE t(a int); CREATE INDEX \"I\" ON t(a); REVOKE SELECT ON public.\"I\" FROM PUBLIC;",
        "42809",
        "\"I\" is an index",
    );
}
