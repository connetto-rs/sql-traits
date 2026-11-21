use super::refuses;

#[test]
fn missing_objects() {
    for (statement, message) in [
        ("DROP INDEX NoSuch;", "index \"nosuch\" does not exist"),
        ("DROP TYPE NoSuch;", "type \"nosuch\" does not exist"),
        ("DROP DOMAIN NoSuch;", "type \"nosuch\" does not exist"),
        ("DROP ROLE s6_NoSuch;", "role \"s6_nosuch\" does not exist"),
        (
            "CREATE TABLE t(a int); DROP TRIGGER NoSuch ON t;",
            "trigger \"nosuch\" for table \"t\" does not exist",
        ),
        (
            "CREATE TABLE t(a int); DROP POLICY NoSuch ON t;",
            "policy \"nosuch\" for table \"t\" does not exist",
        ),
    ] {
        refuses(statement, "42704", message);
    }
}

#[test]
fn default_privileges() {
    refuses(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA NoSuch GRANT SELECT ON TABLES TO PUBLIC;",
        "3F000",
        "schema \"nosuch\" does not exist",
    );
    refuses(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public.nested GRANT SELECT ON TABLES TO PUBLIC;",
        "42601",
        "syntax error at or near \".\"",
    );
    for object in ["SCHEMAS", "LARGE OBJECTS"] {
        refuses(
            &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON {object} TO PUBLIC;"),
            "0LP01",
            &format!("cannot use IN SCHEMA clause when using GRANT/REVOKE ON {object}"),
        );
    }
    refuses(
        "ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON TABLES TO PUBLIC;",
        "0LP01",
        "invalid privilege type EXECUTE for relation",
    );
    refuses(
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON FUNCTIONS TO PUBLIC;",
        "0LP01",
        "invalid privilege type SELECT for function",
    );
}

#[test]
fn enum_and_composite_labels() {
    refuses(
        "CREATE TYPE mood AS ENUM('a','a');",
        "23505",
        "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
    );
    refuses("CREATE TYPE pair AS (A int,a int);", "42701", "column \"a\" specified more than once");
    refuses(
        "CREATE TYPE r AS RANGE (multirange_type_name=m);",
        "42601",
        "type attribute \"subtype\" is required",
    );
    for statement in ["ALTER TYPE mood ADD VALUE 'a';", "ALTER TYPE mood RENAME VALUE 'a' TO 'a';"]
    {
        refuses(
            &format!("CREATE TYPE mood AS ENUM('a'); {statement}"),
            "42710",
            "enum label \"a\" already exists",
        );
    }
    for statement in
        ["ALTER TYPE mood ADD VALUE 'b' BEFORE 'z';", "ALTER TYPE mood RENAME VALUE 'z' TO 'b';"]
    {
        refuses(
            &format!("CREATE TYPE mood AS ENUM('a'); {statement}"),
            "22023",
            "\"z\" is not an existing enum label",
        );
    }
}

#[test]
fn dependent_types() {
    refuses(
        "CREATE TYPE mood AS ENUM ('a'); CREATE TABLE t(a mood); DROP TYPE mood;",
        "2BP01",
        "cannot drop type mood because other objects depend on it",
    );
    refuses(
        "CREATE DOMAIN dom AS int; CREATE TABLE td(a dom); DROP DOMAIN dom;",
        "2BP01",
        "cannot drop type dom because other objects depend on it",
    );
    refuses(
        "CREATE TYPE mood AS ENUM ('a'); DROP TYPE _mood;",
        "2BP01",
        "cannot drop type mood[] because type mood requires it",
    );
    refuses(
        "CREATE TYPE mood AS ENUM ('a'); ALTER TYPE _mood RENAME TO nope;",
        "42809",
        "cannot alter array type mood[]",
    );
    refuses(
        "CREATE TABLE t(a int); DROP TYPE t;",
        "2BP01",
        "cannot drop type t because table t requires it",
    );
    refuses(
        "CREATE TABLE t(a int); ALTER TYPE t RENAME TO nope;",
        "42809",
        "t is a table's row type",
    );
    refuses(
        "CREATE TYPE r AS RANGE (subtype=int4); DROP TYPE r_multirange;",
        "2BP01",
        "cannot drop type r_multirange because type r requires it",
    );
    refuses(
        "DROP TYPE int4;",
        "2BP01",
        "cannot drop type integer because it is required by the database system",
    );
    for statement in ["CREATE TABLE ts(a shell);", "ALTER TYPE shell RENAME TO shell2;"] {
        refuses(
            &format!("CREATE TYPE shell; {statement}"),
            "42704",
            "type \"shell\" is only a shell",
        );
    }
}

#[test]
fn relation_kinds_and_names() {
    for (statement, expected) in [
        ("DROP DOMAIN t;", "domain"),
        ("DROP VIEW t;", "view"),
        ("DROP MATERIALIZED VIEW t;", "materialized view"),
        ("DROP SEQUENCE t;", "sequence"),
    ] {
        refuses(
            &format!("CREATE TABLE t(a int); {statement}"),
            "42809",
            &format!("\"t\" is not a {expected}"),
        );
    }
    for statement in [
        "CREATE TABLE mood(a int);",
        "CREATE VIEW mood AS SELECT 1;",
        "CREATE SEQUENCE mood;",
        "CREATE DOMAIN mood AS int;",
        "CREATE TYPE mood;",
    ] {
        refuses(
            &format!("CREATE TYPE mood AS ENUM ('a'); {statement}"),
            "42710",
            "type \"mood\" already exists",
        );
    }
}

#[test]
fn foreign_options_and_repeated_keys() {
    refuses(
        "CREATE TABLE rep(a int, UNIQUE(a,a));",
        "42701",
        "column \"a\" appears twice in unique constraint",
    );
    refuses(
        "CREATE TABLE rep(a int, PRIMARY KEY(a,a));",
        "42701",
        "column \"a\" appears twice in primary key constraint",
    );
    refuses("CREATE TABLE opt(a int OPTIONS(x 'a'));", "42809", "\"opt\" is not a foreign table");
    for (constraint, kind) in
        [("PRIMARY KEY", "primary key"), ("UNIQUE", "unique"), ("REFERENCES t", "foreign key")]
    {
        refuses(
            &format!(
                "CREATE TABLE t(a int PRIMARY KEY); CREATE FOREIGN TABLE ft(a int {constraint}) SERVER s6_srv;"
            ),
            "0A000",
            &format!("{kind} constraints are not supported on foreign tables"),
        );
    }
    for statement in [
        "CREATE FOREIGN TABLE ft(a int) SERVER s6_srv OPTIONS(x 'a',x 'b');",
        "CREATE FOREIGN TABLE ft(a int OPTIONS(x 'a',x 'b')) SERVER s6_srv;",
        "CREATE FOREIGN TABLE ft(a int) SERVER s6_srv OPTIONS(x 'a'); ALTER FOREIGN TABLE ft OPTIONS(ADD x 'b');",
    ] {
        refuses(statement, "42710", "option \"x\" provided more than once");
    }
}

#[test]
fn timezone() {
    refuses(
        "SET timezone='NoSuch';",
        "22023",
        "invalid value for parameter \"TimeZone\": \"NoSuch\"",
    );
}

#[test]
fn quoted_names() {
    for (statement, message) in [
        ("DROP INDEX \"NoSuch\";", "index \"NoSuch\" does not exist"),
        ("DROP TYPE \"NoSuch\";", "type \"NoSuch\" does not exist"),
        ("DROP DOMAIN \"NoSuch\";", "type \"NoSuch\" does not exist"),
        ("DROP ROLE \"s6_NoSuch\";", "role \"s6_NoSuch\" does not exist"),
        (
            "CREATE TABLE \"T\"(\"A\" int); DROP TRIGGER \"NoSuch\" ON \"T\";",
            "trigger \"NoSuch\" for table \"T\" does not exist",
        ),
        (
            "CREATE TABLE \"T\"(\"A\" int); DROP POLICY \"NoSuch\" ON \"T\";",
            "policy \"NoSuch\" for table \"T\" does not exist",
        ),
        (
            "CREATE TABLE \"T\"(\"A\" int); CREATE POLICY \"P\" ON \"T\" TO \"s6_NoSuch\" USING(true);",
            "role \"s6_NoSuch\" does not exist",
        ),
    ] {
        refuses(statement, "42704", message);
    }
    refuses("CREATE SCHEMA \"S\"; CREATE SCHEMA \"S\";", "42P06", "schema \"S\" already exists");
    refuses("SET ROLE \"s6_NoSuch\";", "22023", "role \"s6_NoSuch\" does not exist");
}

#[test]
fn qualified_targets() {
    for (statement, message) in [
        ("DROP TYPE s.NoSuch;", "type \"s.nosuch\" does not exist"),
        ("DROP INDEX s.NoSuch;", "index \"nosuch\" does not exist"),
        (
            "CREATE TABLE s.t(a int); DROP TRIGGER \"NoSuch\" ON s.t;",
            "trigger \"NoSuch\" for table \"t\" does not exist",
        ),
        (
            "CREATE TABLE s.t(a int); DROP POLICY \"NoSuch\" ON s.t;",
            "policy \"NoSuch\" for table \"t\" does not exist",
        ),
    ] {
        refuses(&format!("CREATE SCHEMA s; {statement}"), "42704", message);
    }
    for statement in ["DROP TRIGGER x ON s.nosuch;", "DROP POLICY x ON s.nosuch;"] {
        refuses(
            &format!("CREATE SCHEMA s; {statement}"),
            "42P01",
            "relation \"s.nosuch\" does not exist",
        );
    }
    for (statement, kind) in [
        ("DROP VIEW s.NoSuch;", "view"),
        ("DROP MATERIALIZED VIEW s.NoSuch;", "materialized view"),
        ("DROP SEQUENCE s.NoSuch;", "sequence"),
    ] {
        refuses(
            &format!("CREATE SCHEMA s; {statement}"),
            "42P01",
            &format!("{kind} \"nosuch\" does not exist"),
        );
    }
}

#[test]
fn type_diagnostic_spelling() {
    refuses(
        "CREATE TYPE \"Mood\" AS ENUM('a'); CREATE TABLE t(a \"Mood\"); DROP TYPE \"Mood\";",
        "2BP01",
        "cannot drop type \"Mood\" because other objects depend on it",
    );
    refuses(
        "CREATE TYPE \"Mood\" AS ENUM('a'); DROP TYPE \"_Mood\";",
        "2BP01",
        "cannot drop type \"Mood\"[] because type \"Mood\" requires it",
    );
    refuses(
        "CREATE TYPE \"Mood\" AS ENUM('a'); ALTER TYPE \"_Mood\" RENAME TO x;",
        "42809",
        "cannot alter array type \"Mood\"[]",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TYPE s.m AS ENUM('a'); CREATE TABLE tm(a s.m); DROP TYPE s.m;",
        "2BP01",
        "cannot drop type s.m because other objects depend on it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TYPE s.m AS ENUM('a'); DROP TYPE s._m;",
        "2BP01",
        "cannot drop type s.m[] because type s.m requires it",
    );
    refuses(
        "CREATE SCHEMA s; CREATE TYPE s.m AS ENUM('a'); ALTER TYPE s._m RENAME TO x;",
        "42809",
        "cannot alter array type s.m[]",
    );
    refuses(
        "CREATE DOMAIN \"D\" AS int; ALTER TYPE \"D\" ADD VALUE 'x';",
        "42809",
        "\"D\" is not an enum",
    );
    refuses(
        "CREATE DOMAIN d AS int; ALTER TYPE d RENAME VALUE 'a' TO 'b';",
        "42809",
        "d is not an enum",
    );
}

#[test]
fn session_role_refusals() {
    refuses("SET ROLE s6_NoSuch;", "22023", "role \"s6_nosuch\" does not exist");
    refuses("SET session_authorization = NoSuch;", "22023", "role \"nosuch\" does not exist");
    refuses(
        "CREATE ROLE s6_role SUPERUSER; SET ROLE s6_role; DROP ROLE s6_role;",
        "55006",
        "current user cannot be dropped",
    );
    refuses(
        "CREATE ROLE s6_role SUPERUSER; SET ROLE s6_role; ALTER ROLE s6_role RENAME TO s6_new;",
        "0A000",
        "current user cannot be renamed",
    );
    refuses(
        "CREATE ROLE s6_role SUPERUSER; SET SESSION AUTHORIZATION s6_role; ALTER ROLE s6_role RENAME TO s6_new;",
        "0A000",
        "session user cannot be renamed",
    );
    refuses(
        "CREATE ROLE s6_session SUPERUSER; CREATE ROLE s6_current SUPERUSER; SET SESSION AUTHORIZATION s6_session; SET ROLE s6_current; DROP ROLE s6_session;",
        "55006",
        "session user cannot be dropped",
    );
    refuses(
        "CREATE ROLE s6_role; CREATE TABLE t(a int); GRANT SELECT ON t TO s6_role; DROP ROLE s6_role;",
        "2BP01",
        "role \"s6_role\" cannot be dropped because some objects depend on it",
    );
}

#[test]
fn invalid_setting_syntax() {
    for (statement, token) in [
        ("SET role = foo();", "("),
        ("SET session_authorization = 1+1;", "+"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = interval '1 hour';", "'1 hour'"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET search_path = foo();", "("),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = NULL;", "NULL"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = ARRAY[1];", "ARRAY"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = 1+1;", "+"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = 'a'::text;", "::"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = -1 + 1;", "+"),
        ("CREATE ROLE s6_role; ALTER ROLE s6_role SET timezone = (1+1);", "("),
    ] {
        refuses(statement, "42601", &format!("syntax error at or near \"{token}\""));
    }
}

#[test]
fn multiple_session_roles() {
    refuses("SET role = a,b;", "22023", "SET role takes only one argument");
}

#[test]
fn no_creation_schema() {
    refuses(
        "SET search_path=''; CREATE COLLATION NoSuch FROM pg_catalog.\"C\";",
        "3F000",
        "no schema has been selected to create in",
    );
}

#[test]
fn model_only_settings() {
    use sqlparser::dialect::PostgreSqlDialect;
    super::refuses_without_counterpart::<PostgreSqlDialect>(
        "CREATE ROLE s6_role; ALTER ROLE s6_role SET work_mem FROM CURRENT;",
    );
    super::refuses_without_counterpart::<PostgreSqlDialect>(
        "ALTER ROLE CURRENT_USER SET work_mem = '1MB';",
    );
}

#[test]
fn absent_relation_operations() {
    for statement in [
        "ALTER VIEW NoSuch RENAME TO b;",
        "CREATE TABLE par(a int) PARTITION BY LIST(a); ALTER TABLE par ATTACH PARTITION NoSuch FOR VALUES IN(1);",
    ] {
        refuses(statement, "42P01", "relation \"nosuch\" does not exist");
    }
}

#[test]
fn indexing_and_referencing_other_kinds() {
    refuses(
        "CREATE VIEW v AS SELECT 1 a; CREATE INDEX ix ON v(a);",
        "42809",
        "cannot create index on relation \"v\"",
    );
    refuses(
        "CREATE VIEW v AS SELECT 1 a; CREATE TABLE fk(a int REFERENCES v(a));",
        "42809",
        "referenced relation \"v\" is not a table",
    );
    refuses(
        "CREATE FOREIGN TABLE ft(a int) SERVER s6_srv; CREATE TABLE fk(a int REFERENCES ft(a));",
        "42809",
        "referenced relation \"ft\" is not a table",
    );
    refuses("CREATE VIEW v AS SELECT 1 a; DROP INDEX v;", "42809", "\"v\" is not an index");
    refuses(
        "CREATE TABLE t (a INT NOT NULL); CREATE SEQUENCE s; ALTER TABLE t REPLICA IDENTITY USING INDEX s;",
        "42809",
        "\"s\" is not an index",
    );
}

#[test]
fn index_keys_without_default_operator_class() {
    for (sql, type_name, method) in [
        ("CREATE TABLE r (id json UNIQUE);", "json", "btree"),
        ("CREATE TABLE r (id point); CREATE INDEX ix ON r (id);", "point", "btree"),
        ("CREATE TABLE r (id xml); ALTER TABLE r ADD PRIMARY KEY (id);", "xml", "btree"),
        (
            "CREATE DOMAIN \"Spot\" AS point; CREATE TABLE r (id \"Spot\" PRIMARY KEY);",
            "\"Spot\"",
            "btree",
        ),
        (
            "CREATE SCHEMA s; CREATE DOMAIN s.d AS point; CREATE TABLE r (id s.d UNIQUE);",
            "s.d",
            "btree",
        ),
        ("CREATE TABLE t (a point); CREATE INDEX i ON t USING hash (a);", "point", "hash"),
        ("CREATE TABLE t (a json); CREATE INDEX i ON t USING gin (a);", "json", "gin"),
        ("CREATE TABLE t (a int[]); CREATE INDEX i ON t USING gist (a);", "integer[]", "gist"),
    ] {
        refuses(
            sql,
            "42704",
            &format!(
                "data type {type_name} has no default operator class for access method \"{method}\""
            ),
        );
    }
}

#[test]
fn catalog_type_dependencies() {
    refuses(
        "CREATE TYPE token AS ENUM('a'); CREATE FUNCTION eq(token,token) RETURNS bool LANGUAGE sql AS 'SELECT true'; CREATE OPERATOR ~= (leftarg=token,rightarg=token,function=eq); DROP TYPE token;",
        "2BP01",
        "cannot drop type token because other objects depend on it",
    );
}

#[test]
fn foreign_and_trigger_relation_kinds() {
    refuses("DROP FOREIGN TABLE NoSuch;", "42704", "foreign table \"nosuch\" does not exist");
    refuses(
        "CREATE MATERIALIZED VIEW mv AS SELECT 1 a; CREATE TRIGGER tr AFTER INSERT ON mv EXECUTE FUNCTION f();",
        "42809",
        "relation \"mv\" cannot have triggers",
    );
    refuses(
        "CREATE FOREIGN TABLE f(a int, EXCLUDE USING gist(a WITH =)) SERVER s6_srv;",
        "0A000",
        "exclusion constraints are not supported on foreign tables",
    );
}

#[test]
fn built_in_type_drop_spellings() {
    refuses(
        "DROP TYPE int;",
        "2BP01",
        "cannot drop type integer because it is required by the database system",
    );
    refuses("DROP DOMAIN int;", "42809", "\"pg_catalog.int4\" is not a domain");
    refuses("DROP DOMAIN int4;", "42809", "\"int4\" is not a domain");
    refuses("DROP DOMAIN \"int\";", "42704", "type \"int\" does not exist");
    refuses("DROP TYPE \"int\";", "42704", "type \"int\" does not exist");
}

#[test]
fn type_rename_collisions() {
    refuses(
        "CREATE TABLE rowt(a int); CREATE TYPE ren AS ENUM('a'); ALTER TYPE ren RENAME TO rowt;",
        "42710",
        "type \"rowt\" already exists",
    );
    refuses(
        "CREATE TABLE rowt(a int); CREATE TYPE comp AS(a int); ALTER TYPE comp RENAME TO rowt;",
        "42P07",
        "relation \"rowt\" already exists",
    );
    refuses(
        "CREATE SEQUENCE seq; CREATE TYPE seq AS(a int);",
        "42P07",
        "relation \"seq\" already exists",
    );
}

#[test]
fn non_postgres_refusals() {
    use sql_traits::errors::Error;
    use sqlparser::dialect::{MySqlDialect, SQLiteDialect};
    for statement in [
        "CREATE VIRTUAL TABLE c USING csv(filename='data.csv');",
        "CREATE VIRTUAL TABLE f USING fts5();",
        "CREATE VIRTUAL TABLE f5 USING fts5(a); CREATE VIEW v AS SELECT a FROM f5 WHERE f5 MATCH 'x'; ALTER TABLE f5 RENAME TO docs;",
        "CREATE VIEW v AS SELECT * FROM json_each('[1]'); CREATE VIRTUAL TABLE t USING fts4(content=v);",
    ] {
        super::refuses_without_counterpart::<SQLiteDialect>(statement);
    }
    super::refuses_without_counterpart::<MySqlDialect>(
        "CREATE TABLE t(a int); CREATE INDEX i ON t(a,a);",
    );
    for error in [
        Error::VirtualTableColumnsMismatch {
            table_name: "t".into(),
            module_name: "fts5".into(),
            column_name: "a".into(),
        },
        Error::VirtualTableShadowSuffixRefused {
            table_name: "t".into(),
            module_name: "fts5".into(),
            suffix: "node".into(),
        },
        Error::NotAVirtualTable { statement: "CREATE TABLE t(a)".into() },
    ] {
        assert_eq!(error.postgres_error(), None);
    }
}

#[test]
fn timezone_refusal_kinds() {
    refuses(
        "SET timezone = 'interval ''abc''';",
        "22007",
        "invalid input syntax for type interval: \"abc\"",
    );
    refuses(
        "SET timezone = 'interval ''9999999999999999999999 hours''';",
        "22015",
        "interval field value out of range: \"9999999999999999999999 hours\"",
    );
    refuses(
        "SET timezone = 'interval ''1 month''';",
        "22023",
        "invalid value for parameter \"TimeZone\": \"interval '1 month'\"",
    );
    refuses(
        "SET timezone = 'interval ''1 day''';",
        "22023",
        "invalid value for parameter \"TimeZone\": \"interval '1 day'\"",
    );
    refuses(
        "SET timezone = 'interval ''168 hours''';",
        "22023",
        "invalid value for parameter \"TimeZone\": \"interval '168 hours'\"",
    );
    refuses(
        "SET timezone = '+168';",
        "22023",
        "invalid value for parameter \"TimeZone\": \"+168\"",
    );
    refuses(
        "SET timezone = 'UTC+00:00:01';",
        "22023",
        "time zone \"UTC+00:00:01\" appears to use leap seconds",
    );
}

#[test]
fn qualified_absent_schema() {
    for statement in [
        "DROP TYPE absent.NoSuch;",
        "DROP DOMAIN absent.NoSuch;",
        "ALTER TYPE absent.NoSuch RENAME TO other;",
        "DROP INDEX absent.NoSuch;",
        "DROP VIEW absent.NoSuch;",
        "DROP SEQUENCE absent.NoSuch;",
        "DROP TRIGGER tr ON absent.NoSuch;",
        "DROP POLICY p ON absent.NoSuch;",
        "COMMENT ON TABLE absent.NoSuch IS 'x';",
    ] {
        refuses(statement, "3F000", "schema \"absent\" does not exist");
    }
}

#[test]
fn range_without_attributes() {
    refuses("CREATE TYPE r AS RANGE ();", "42601", "syntax error at or near \")\"");
}
