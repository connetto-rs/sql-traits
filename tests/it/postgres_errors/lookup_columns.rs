#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::unreachable,
    reason = "Test assertions require valid fixtures and the measured refusal"
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

use super::refuses;

fn lookup_refuses(error: &LookupError, sqlstate: &str, message: &str) {
    let postgres = error.postgres_error().expect("a PostgreSQL refusal");
    assert_eq!(postgres.sqlstate().as_str(), sqlstate, "{error:?}");
    assert_eq!(postgres.message(), message, "{error:?}");
}

#[test]
fn missing_expression_columns() {
    for (expression, message) in [
        ("NoSuch", "column \"nosuch\" does not exist"),
        ("\"NoSuch\"", "column \"NoSuch\" does not exist"),
        ("t.NoSuch", "column t.nosuch does not exist"),
        ("public.t.NoSuch", "column t.nosuch does not exist"),
    ] {
        refuses(
            &format!("CREATE TABLE t (id INT); CREATE VIEW v AS SELECT {expression} FROM t;"),
            "42703",
            message,
        );
    }
    refuses(
        "CREATE TABLE c (id INT, CHECK (c.nosuch > 0));",
        "42703",
        "column c.nosuch does not exist",
    );
    refuses(
        "CREATE TABLE c (id INT, CHECK (nosuch > 0));",
        "42703",
        "column \"nosuch\" does not exist",
    );
    refuses(
        "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (nosuch + 1) STORED);",
        "42703",
        "column \"nosuch\" does not exist",
    );
}

#[test]
fn missing_relation_qualifiers() {
    refuses(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT x.a FROM t;",
        "42P01",
        "missing FROM-clause entry for table \"x\"",
    );
    refuses(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT t.a FROM t AS tt;",
        "42P01",
        "invalid reference to FROM-clause entry for table \"t\"",
    );
    refuses(
        "CREATE TABLE c (id INT, CHECK (other.id > 0));",
        "42P01",
        "missing FROM-clause entry for table \"other\"",
    );
    refuses(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, owner TEXT NOT NULL); ALTER TABLE docs ENABLE ROW LEVEL SECURITY; CREATE POLICY pins ON docs FOR INSERT WITH CHECK (EXISTS (SELECT 1 FROM docs WHERE lower(docs.owner) = lower(NEW.owner)));",
        "42P01",
        "missing FROM-clause entry for table \"new\"",
    );
    refuses(
        "CREATE TABLE t (id INT); CREATE TABLE c AS SELECT x.id FROM t;",
        "42P01",
        "missing FROM-clause entry for table \"x\"",
    );
    refuses(
        "CREATE TABLE t (id INT); CREATE INDEX i ON t ((x.id + 1));",
        "42P01",
        "missing FROM-clause entry for table \"x\"",
    );
    refuses(
        "CREATE TABLE t (id INT); CREATE INDEX i ON t (id) WHERE x.id > 0;",
        "42P01",
        "missing FROM-clause entry for table \"x\"",
    );
    for (expression, sqlstate, message) in [
        ("x.id > 0", "42P01", "missing FROM-clause entry for table \"x\""),
        ("NEW.\"NoSuch\" IS NULL", "42703", "column new.NoSuch does not exist"),
    ] {
        refuses(
            &format!(
                "CREATE TABLE t (id INT); CREATE FUNCTION s1_trigger() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN ({expression}) EXECUTE FUNCTION s1_trigger();"
            ),
            sqlstate,
            message,
        );
    }
}

#[test]
fn missing_declared_columns() {
    refuses(
        "CREATE TABLE t (id INT); CREATE INDEX i ON t (NoSuch);",
        "42703",
        "column \"nosuch\" does not exist",
    );
    refuses(
        "CREATE TABLE c (id INT, UNIQUE (NoSuch));",
        "42703",
        "column \"nosuch\" named in key does not exist",
    );
    refuses(
        "CREATE TABLE c (id INT, PRIMARY KEY (NoSuch));",
        "42703",
        "column \"nosuch\" named in key does not exist",
    );
    refuses(
        "CREATE TABLE t (id INT); COMMENT ON COLUMN t.NoSuch IS 'x';",
        "42703",
        "column \"nosuch\" of relation \"t\" does not exist",
    );
    refuses(
        "CREATE TABLE t (id INT); ALTER TABLE t DROP COLUMN NoSuch;",
        "42703",
        "column \"nosuch\" of relation \"t\" does not exist",
    );
    refuses(
        "CREATE TABLE t (id INT); ALTER TABLE t RENAME COLUMN NoSuch TO new;",
        "42703",
        "column \"nosuch\" does not exist",
    );
    refuses(
        "CREATE TABLE t (id INT); ALTER TABLE t ALTER COLUMN NoSuch SET DEFAULT 1;",
        "42703",
        "column \"nosuch\" of relation \"t\" does not exist",
    );
    refuses(
        "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t; ALTER VIEW v ALTER COLUMN NoSuch SET DEFAULT 1;",
        "42703",
        "column \"nosuch\" of relation \"v\" does not exist",
    );
}

#[test]
fn missing_relations() {
    for (name, message) in [
        ("nosuch", "relation \"nosuch\" does not exist"),
        ("\"NoSuch\"", "relation \"NoSuch\" does not exist"),
    ] {
        let database = ParserDB::parse::<SQLiteDialect>(&format!(
            "CREATE TABLE child (id INT REFERENCES {name});"
        ))
        .unwrap();
        let table = database
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .unwrap()
            .unwrap();
        let foreign_key = table.foreign_keys(&database).unwrap().next().unwrap();
        let error = foreign_key
            .referenced_columns(&database)
            .err()
            .expect("the referenced relation is absent");
        lookup_refuses(&error, "42P01", message);
    }
}

#[test]
fn duplicate_relations() {
    refuses(
        "CREATE TABLE t(id integer); CREATE TABLE t(id integer);",
        "42P07",
        "relation \"t\" already exists",
    );
    refuses(
        "CREATE TABLE t(id integer); CREATE TABLE public.t(id integer);",
        "42P07",
        "relation \"t\" already exists",
    );
    refuses(
        "CREATE TABLE t(id integer); CREATE TABLE T(id integer);",
        "42P07",
        "relation \"t\" already exists",
    );
}

#[test]
fn foreign_key_metadata_refusals() {
    for (schema, sqlstate, message) in [
        (
            "CREATE VIEW no_key AS SELECT 1 AS id; CREATE TABLE child (id INT REFERENCES no_key);",
            "42809",
            "referenced relation \"no_key\" is not a table",
        ),
        (
            "CREATE TABLE no_key (id INT); CREATE TABLE child (id INT REFERENCES no_key);",
            "42704",
            "there is no primary key for referenced table \"no_key\"",
        ),
        (
            "CREATE TABLE no_key (id INT); CREATE TABLE child (id INT REFERENCES no_key(id));",
            "42830",
            "there is no unique constraint matching given keys for referenced table \"no_key\"",
        ),
        (
            "CREATE TABLE parent (a INT, b INT, PRIMARY KEY (a,b)); CREATE TABLE child (id INT REFERENCES parent);",
            "42830",
            "number of referencing and referenced columns for foreign key disagree",
        ),
    ] {
        let database =
            ParserDB::parse::<SQLiteDialect>(schema).expect("SQLite defers checking foreign keys");
        let table = database
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .unwrap()
            .unwrap();
        let foreign_key = table.foreign_keys(&database).unwrap().next().unwrap();
        let error =
            foreign_key.referenced_columns(&database).err().expect("the target key is invalid");
        lookup_refuses(&error, sqlstate, message);
    }
}

#[test]
fn policy_literal_input_refusals() {
    for (data_type, literal, sqlstate, message) in [
        ("DATE", "2026-02-30", "22008", "date/time field value out of range: \"2026-02-30\""),
        ("TIME", "12:99", "22008", "date/time field value out of range: \"12:99\""),
        ("TIMETZ", "12:00+16:00", "22009", "time zone displacement out of range: \"12:00+16:00\""),
        ("DATE", "5874898-01-01", "22008", "date out of range: \"5874898-01-01\""),
        ("TIMESTAMP", "294277-01-01", "22008", "timestamp out of range: \"294277-01-01\""),
        (
            "TIMESTAMPTZ",
            "2026-01-01 12:00 NoSuch",
            "22007",
            "invalid input syntax for type timestamp with time zone: \"2026-01-01 12:00 NoSuch\"",
        ),
        ("DATE", "abc", "22007", "invalid input syntax for type date: \"abc\""),
        ("TIME", "abc", "22007", "invalid input syntax for type time: \"abc\""),
        ("TIMETZ", "abc", "22007", "invalid input syntax for type time with time zone: \"abc\""),
        ("TIMESTAMP", "abc", "22007", "invalid input syntax for type timestamp: \"abc\""),
        (
            "TIMESTAMPTZ",
            "abc",
            "22007",
            "invalid input syntax for type timestamp with time zone: \"abc\"",
        ),
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t (value {data_type}); CREATE POLICY p ON t USING (value = '{literal}');"
        ))
        .unwrap();
        let policy = database.policies().next().unwrap();
        lookup_refuses(&policy.using_expression(&database).unwrap_err(), sqlstate, message);
    }
}

#[test]
fn ambiguous_column_and_output_names() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (id INT, a INT); CREATE TABLE t2 (id INT);",
    )
    .unwrap();
    for statement in [
        "SELECT id FROM t JOIN t2 ON true",
        "SELECT repeated.id FROM (SELECT id,a AS id FROM t) repeated",
    ] {
        let sqlparser::ast::Statement::Query(query) =
            sqlparser::parser::Parser::parse_sql(&PostgreSqlDialect {}, statement)
                .unwrap()
                .remove(0)
        else {
            unreachable!()
        };
        lookup_refuses(
            &query.projection_source_table(&database).unwrap_err(),
            "42702",
            "column reference \"id\" is ambiguous",
        );
    }
    for (clause, message) in [
        ("ORDER BY", "ORDER BY \"x\" is ambiguous"),
        ("GROUP BY", "GROUP BY \"x\" is ambiguous"),
        ("DISTINCT ON", "DISTINCT ON \"x\" is ambiguous"),
    ] {
        let statement = if clause == "DISTINCT ON" {
            "SELECT DISTINCT ON(x) id AS x, a AS x FROM t".to_owned()
        } else {
            format!("SELECT id AS x, a AS x FROM t {clause} x")
        };
        let sqlparser::ast::Statement::Query(query) =
            sqlparser::parser::Parser::parse_sql(&PostgreSqlDialect {}, &statement)
                .unwrap()
                .remove(0)
        else {
            unreachable!()
        };
        let scope = ColumnScope::from_query(&query, &database, IdentifierCase::Folded).unwrap();
        let key = sqlparser::ast::Expr::Identifier(sqlparser::ast::Ident::new("x"));
        let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
        let select_scope = scope.scope_for_select(select).unwrap();
        let result = match clause {
            "ORDER BY" => scope.scope_for_query(&query).unwrap().resolve_order_by_definition(&key),
            "GROUP BY" => select_scope.resolve_group_by_definition(&key),
            _ => select_scope.resolve_distinct_on_definition(&key),
        };
        let error = result.unwrap_err();
        lookup_refuses(&error, "42702", message);
    }
}

#[test]
fn ambiguous_function_names() {
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE FUNCTION s1_overloaded(integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1'; CREATE FUNCTION s1_overloaded(text) RETURNS text LANGUAGE SQL AS 'SELECT $1';").unwrap();
    for (target, message) in [
        (TargetName::new("s1_overloaded", false), "function name \"s1_overloaded\" is not unique"),
        (
            TargetName::new("S1_OVERLOADED", false).with_schema("public", false),
            "function name \"public.s1_overloaded\" is not unique",
        ),
    ] {
        lookup_refuses(
            &database.resolve_target_function(target, IdentifierCase::Folded).unwrap_err(),
            "42725",
            message,
        );
    }
}

#[test]
fn trigger_relation_kinds() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SEQUENCE s1_seq; CREATE MATERIALIZED VIEW s1_mv AS SELECT 1 AS id;",
    )
    .unwrap();
    for name in ["s1_seq", "s1_mv"] {
        let statements = sqlparser::parser::Parser::parse_sql(&PostgreSqlDialect {}, &format!("CREATE TRIGGER test BEFORE INSERT ON {name} FOR EACH ROW EXECUTE FUNCTION s1_trigger();")).unwrap();
        let sqlparser::ast::Statement::CreateTrigger(trigger) = &statements[0] else {
            panic!("trigger statement")
        };
        let error = trigger.relation(&database).expect_err("this kind cannot have triggers");
        lookup_refuses(&error, "42809", &format!("relation \"{name}\" cannot have triggers"));
    }
}

#[test]
fn qualified_comment_columns() {
    refuses(
        "CREATE TABLE t(id INT); CREATE VIEW v AS SELECT id FROM t; COMMENT ON COLUMN public.v.NoSuch IS 'x';",
        "42703",
        "column \"nosuch\" of relation \"public.v\" does not exist",
    );
    refuses(
        "CREATE TYPE pair AS (a INT); COMMENT ON COLUMN public.pair.NoSuch IS 'x';",
        "42703",
        "column \"nosuch\" of relation \"public.pair\" does not exist",
    );
}

#[test]
fn primary_key_expression_syntax() {
    for (expression, token) in
        [("id + 1", "+"), ("(id + 1)", "("), ("id::integer", "::"), ("true", "true")]
    {
        refuses(
            &format!("CREATE TABLE s1_pk (id integer, PRIMARY KEY ({expression}));"),
            "42601",
            &format!("syntax error at or near \"{token}\""),
        );
    }
}

#[test]
fn column_reference_qualification() {
    refuses(
        "CREATE TABLE t(id INT); CREATE VIEW v AS SELECT x.public.t.NoSuch FROM t;",
        "0A000",
        "cross-database references are not implemented: x.public.t.nosuch",
    );
    refuses(
        "CREATE TABLE t(id INT); CREATE VIEW v AS SELECT a.b.c.d.NoSuch FROM t;",
        "42601",
        "improper qualified name (too many dotted names): a.b.c.d.nosuch",
    );
}

#[test]
fn search_path_scalar_syntax() {
    for (value, token) in [("a.b", "."), ("a + b", "+"), ("-'app'", "'app'"), ("NULL", "NULL")] {
        refuses(
            &format!("SET search_path TO {value};"),
            "42601",
            &format!("syntax error at or near \"{token}\""),
        );
    }
    refuses(
        "CREATE FUNCTION s1_setting() RETURNS integer LANGUAGE sql SET search_path TO a.b AS 'SELECT 1';",
        "42601",
        "syntax error at or near \".\"",
    );
}
