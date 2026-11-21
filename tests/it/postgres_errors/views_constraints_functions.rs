#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use super::refuses;

#[test]
fn view_shape_and_options() {
    for (sql, state, message) in [
        (
            "CREATE TABLE t (id int); CREATE VIEW v AS SELECT id FROM t; CREATE OR REPLACE VIEW v AS SELECT id AS other FROM t;",
            "42P16",
            "cannot change name of view column \"id\" to \"other\"",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW v AS SELECT id FROM t; CREATE OR REPLACE VIEW v AS SELECT FROM t;",
            "42P16",
            "cannot drop columns from view",
        ),
        (
            "CREATE TABLE t (id int); CREATE OR REPLACE MATERIALIZED VIEW m AS SELECT id FROM t;",
            "42601",
            "syntax error at or near \"MATERIALIZED\"",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW IF NOT EXISTS v AS SELECT id FROM t;",
            "42601",
            "syntax error at or near \"NOT\"",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW w WITH (nope=true) AS SELECT id FROM t;",
            "22023",
            "unrecognized parameter \"nope\"",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW w WITH (security_barrier='nah') AS SELECT id FROM t;",
            "22023",
            "invalid value for boolean option \"security_barrier\": nah",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW w WITH (check_option='nah') AS SELECT id FROM t;",
            "22023",
            "invalid value for enum option \"check_option\": nah",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW w WITH (security_barrier=true, security_barrier=false) AS SELECT id FROM t;",
            "22023",
            "parameter \"security_barrier\" specified more than once",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn duplicate_objects_and_missing_constraints() {
    for (sql, state, message) in [
        (
            "CREATE TABLE t (id int); CREATE VIEW v AS SELECT id FROM t; DROP TABLE t;",
            "2BP01",
            "cannot drop table t because other objects depend on it",
        ),
        (
            "CREATE TABLE t (id int); CREATE VIEW v AS SELECT id FROM t; ALTER TABLE v ADD COLUMN x int;",
            "42809",
            "ALTER action ADD COLUMN cannot be performed on relation \"v\"",
        ),
        (
            "CREATE TABLE t (id int); ALTER TABLE t RENAME CONSTRAINT nope TO yep;",
            "42704",
            "constraint \"nope\" for table \"t\" does not exist",
        ),
        (
            "CREATE TABLE t (id int); ALTER TABLE t ADD CONSTRAINT ck CHECK (id>0); ALTER TABLE t ADD CONSTRAINT ck CHECK (id>0);",
            "42710",
            "constraint \"ck\" for relation \"t\" already exists",
        ),
        (
            "CREATE TABLE t (id int); CREATE POLICY pol ON t; CREATE POLICY pol ON t;",
            "42710",
            "policy \"pol\" for table \"t\" already exists",
        ),
        (
            "CREATE EXTENSION hstore; CREATE EXTENSION hstore;",
            "42710",
            "extension \"hstore\" already exists",
        ),
        ("CREATE ROLE s7_role; CREATE ROLE s7_role;", "42710", "role \"s7_role\" already exists"),
        (
            "CREATE FUNCTION fun() RETURNS int LANGUAGE SQL RETURN 1; CREATE FUNCTION fun() RETURNS int LANGUAGE SQL RETURN 1;",
            "42723",
            "function \"fun\" already exists with same argument types",
        ),
        (
            "CREATE PROCEDURE kind_probe(a int, OUT b int) LANGUAGE sql AS 'SELECT 1'; CREATE OR REPLACE FUNCTION kind_probe(a int, OUT b int) LANGUAGE sql AS 'SELECT 1';",
            "42809",
            "cannot change routine kind",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn function_body_refusals() {
    for (sql, state, message) in [
        (
            "CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT * FROM nosuch; END;",
            "42P01",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC SET search_path TO public; END;",
            "0A000",
            "SET is not yet supported in unquoted SQL function body",
        ),
        (
            "CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC END;",
            "42P13",
            "return type mismatch in function declared to return integer",
        ),
        (
            "CREATE TABLE t (id int); CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC INSERT INTO t VALUES (1); END;",
            "42P13",
            "return type mismatch in function declared to return integer",
        ),
        (
            "CREATE TABLE t (id int); CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT id FROM t a,t b; END;",
            "42702",
            "column reference \"id\" is ambiguous",
        ),
        (
            "CREATE TABLE t (id int); CREATE FUNCTION fun() RETURNS int LANGUAGE SQL BEGIN ATOMIC INSERT INTO t VALUES (1,2); END;",
            "42601",
            "INSERT has more expressions than target columns",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn trigger_rule_and_partition_refusals() {
    let trigger =
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;";
    let parent = "CREATE TABLE parent(id int CONSTRAINT positive CHECK(id>0)) PARTITION BY RANGE(id); CREATE TABLE child PARTITION OF parent FOR VALUES FROM (0) TO (10);";
    for (sql, state, message) in [
        (format!("{trigger} CREATE TABLE t(id int); CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg(); CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();"), "42710", "trigger \"tr\" for relation \"t\" already exists"),
        ("CREATE TABLE t(id int); ALTER TABLE t ENABLE TRIGGER nope;".to_owned(), "42704", "trigger \"nope\" for table \"t\" does not exist"),
        (format!("{parent} ALTER TABLE child RENAME CONSTRAINT positive TO other;"), "42P16", "cannot rename inherited constraint \"positive\""),
        (format!("{trigger} {parent} CREATE TRIGGER tr BEFORE INSERT ON parent FOR EACH ROW EXECUTE FUNCTION trg(); DROP TRIGGER tr ON child;"), "2BP01", "cannot drop trigger tr on table child because trigger tr on table parent requires it"),
        (format!("{trigger} {parent} CREATE TRIGGER tr BEFORE INSERT ON parent FOR EACH ROW EXECUTE FUNCTION trg(); CREATE OR REPLACE TRIGGER tr BEFORE INSERT ON child FOR EACH ROW EXECUTE FUNCTION trg();"), "42710", "trigger \"tr\" for relation \"child\" is an internal or a child trigger"),
        ("CREATE TABLE t(id int); CREATE RULE r AS ON INSERT TO t DO NOTHING; CREATE RULE r AS ON INSERT TO t DO NOTHING;".to_owned(), "42710", "rule \"r\" for relation \"t\" already exists"),
    ] {
        refuses(&sql, state, message);
    }
}

#[test]
fn foreign_table_refusals() {
    let server =
        "CREATE FOREIGN DATA WRAPPER s7_fdw; CREATE SERVER s7_server FOREIGN DATA WRAPPER s7_fdw;";
    for (sql, state, message) in [
        (
            format!(
                "{server} CREATE FOREIGN TABLE ft (id int) SERVER s7_server; ALTER FOREIGN TABLE ft OPTIONS (DROP nope);"
            ),
            "42704",
            "option \"nope\" not found",
        ),
        (
            format!(
                "{server} CREATE FOREIGN TABLE ft (id int) SERVER s7_server; ALTER FOREIGN TABLE ft ALTER COLUMN id OPTIONS (SET nope 'x');"
            ),
            "42704",
            "option \"nope\" not found",
        ),
        (
            format!(
                "{server} CREATE FOREIGN TABLE ft (id int) SERVER s7_server; CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; CREATE CONSTRAINT TRIGGER ct AFTER INSERT ON ft FOR EACH ROW EXECUTE FUNCTION trg();"
            ),
            "42809",
            "\"ft\" is a foreign table",
        ),
        (
            format!(
                "{server} CREATE TABLE parent(id int) PARTITION BY RANGE(id); CREATE FOREIGN TABLE fp PARTITION OF parent FOR VALUES FROM (10) TO (20) SERVER s7_server; CREATE UNIQUE INDEX ON parent(id);"
            ),
            "42809",
            "cannot create unique index on partitioned table \"parent\"",
        ),
    ] {
        refuses(&sql, state, message);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one row per measured statement")]
fn names_follow_postgresql_storage() {
    for (
        table,
        column,
        constraint,
        policy,
        trigger,
        rule,
        missing,
        stored_table,
        stored_column,
        stored_constraint,
        stored_policy,
        stored_trigger,
        stored_rule,
        stored_missing,
    ) in [
        (
            "CaseTable",
            "CaseColumn",
            "CheckName",
            "PolicyName",
            "TriggerName",
            "RuleName",
            "Missing",
            "casetable",
            "casecolumn",
            "checkname",
            "policyname",
            "triggername",
            "rulename",
            "missing",
        ),
        (
            "\"CaseTable\"",
            "\"CaseColumn\"",
            "\"CheckName\"",
            "\"PolicyName\"",
            "\"TriggerName\"",
            "\"RuleName\"",
            "\"Missing\"",
            "CaseTable",
            "CaseColumn",
            "CheckName",
            "PolicyName",
            "TriggerName",
            "RuleName",
            "Missing",
        ),
    ] {
        let schema = format!("CREATE TABLE {table} ({column} int);");
        refuses(
            &format!("{schema} ALTER TABLE {table} RENAME CONSTRAINT {missing} TO other;"),
            "42704",
            &format!("constraint \"{stored_missing}\" for table \"{stored_table}\" does not exist"),
        );
        let check = format!("ALTER TABLE {table} ADD CONSTRAINT {constraint} CHECK({column}>0);");
        refuses(
            &format!("{schema} {check} {check}"),
            "42710",
            &format!(
                "constraint \"{stored_constraint}\" for relation \"{stored_table}\" already exists"
            ),
        );
        let create_policy = format!("CREATE POLICY {policy} ON {table};");
        refuses(
            &format!("{schema} {create_policy} {create_policy}"),
            "42710",
            &format!("policy \"{stored_policy}\" for table \"{stored_table}\" already exists"),
        );
        let create_trigger = format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION trg();"
        );
        refuses(
            &format!(
                "{schema} CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; {create_trigger} {create_trigger}"
            ),
            "42710",
            &format!("trigger \"{stored_trigger}\" for relation \"{stored_table}\" already exists"),
        );
        refuses(
            &format!("{schema} ALTER TABLE {table} ENABLE TRIGGER {missing};"),
            "42704",
            &format!("trigger \"{stored_missing}\" for table \"{stored_table}\" does not exist"),
        );
        let create_rule = format!("CREATE RULE {rule} AS ON INSERT TO {table} DO NOTHING;");
        refuses(
            &format!("{schema} {create_rule} {create_rule}"),
            "42710",
            &format!("rule \"{stored_rule}\" for relation \"{stored_table}\" already exists"),
        );
        refuses(
            &format!(
                "{schema} CREATE FUNCTION f() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT {column} FROM {table} a, {table} b; END;"
            ),
            "42702",
            &format!("column reference \"{stored_column}\" is ambiguous"),
        );
    }
    for (name, stored) in [("CaseFunction", "casefunction"), ("\"CaseFunction\"", "CaseFunction")] {
        let create = format!("CREATE FUNCTION {name}() RETURNS int LANGUAGE SQL RETURN 1;");
        refuses(
            &format!("{create} {create}"),
            "42723",
            &format!("function \"{stored}\" already exists with same argument types"),
        );
    }
    for (name, stored) in [("MissingTable", "missingtable"), ("\"MissingTable\"", "MissingTable")] {
        refuses(
            &format!(
                "CREATE FUNCTION f() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT * FROM {name}; END;"
            ),
            "42P01",
            &format!("relation \"{stored}\" does not exist"),
        );
    }
    for (name, stored) in
        [("MissingOption", "missingoption"), ("\"MissingOption\"", "MissingOption")]
    {
        refuses(
            &format!("CREATE VIEW v WITH ({name}=true) AS SELECT 1;"),
            "22023",
            &format!("unrecognized parameter \"{stored}\""),
        );
        refuses(
            &format!(
                "CREATE FOREIGN DATA WRAPPER s7_fdw; CREATE SERVER s7_server FOREIGN DATA WRAPPER s7_fdw; CREATE FOREIGN TABLE ft(id int) SERVER s7_server; ALTER FOREIGN TABLE ft OPTIONS (DROP {name});"
            ),
            "42704",
            &format!("option \"{stored}\" not found"),
        );
    }
    for (value, stored) in [("InvalidValue", "invalidvalue"), ("\"InvalidValue\"", "InvalidValue")]
    {
        refuses(
            &format!("CREATE VIEW v WITH (security_invoker={value}) AS SELECT 1;"),
            "22023",
            &format!("invalid value for boolean option \"security_invoker\": {stored}"),
        );
    }
}

#[test]
fn dependency_names_are_sql_identifiers() {
    for (sql, message) in [
        (
            "CREATE SCHEMA s7_schema; CREATE TABLE s7_schema.\"Odd.Name\"(id int); CREATE VIEW dep AS SELECT * FROM s7_schema.\"Odd.Name\"; DROP TABLE s7_schema.\"Odd.Name\";",
            "cannot drop table s7_schema.\"Odd.Name\" because other objects depend on it",
        ),
        (
            "CREATE TABLE t(id int); CREATE VIEW \"OddView\" AS SELECT id FROM t; CREATE VIEW dep2 AS SELECT * FROM \"OddView\"; DROP VIEW \"OddView\";",
            "cannot drop view \"OddView\" because other objects depend on it",
        ),
        (
            "CREATE TABLE t(id int); CREATE MATERIALIZED VIEW snap AS SELECT * FROM t; CREATE VIEW dep3 AS SELECT * FROM snap; DROP MATERIALIZED VIEW snap;",
            "cannot drop materialized view snap because other objects depend on it",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER s7_fdw; CREATE SERVER s7_server FOREIGN DATA WRAPPER s7_fdw; CREATE FOREIGN TABLE fdep(id int) SERVER s7_server; CREATE VIEW dep4 AS SELECT * FROM fdep; DROP FOREIGN TABLE fdep;",
            "cannot drop foreign table fdep because other objects depend on it",
        ),
    ] {
        refuses(sql, "2BP01", message);
    }
}

#[test]
fn routine_moves_and_result_types() {
    refuses(
        "CREATE FUNCTION other(int) RETURNS int LANGUAGE SQL RETURN 1; CREATE FUNCTION fun(int) RETURNS int LANGUAGE SQL RETURN 1; ALTER FUNCTION fun(int) RENAME TO other;",
        "42723",
        "function other(integer) already exists in schema \"public\"",
    );
    refuses(
        "CREATE SCHEMA s7_schema; CREATE FUNCTION other(int) RETURNS int LANGUAGE SQL RETURN 1; CREATE FUNCTION s7_schema.other(int) RETURNS int LANGUAGE SQL RETURN 1; ALTER FUNCTION other(int) SET SCHEMA s7_schema;",
        "42723",
        "function other(integer) already exists in schema \"s7_schema\"",
    );
    for (sql, data_type) in [
        (
            "CREATE FUNCTION result1(OUT answer varchar) LANGUAGE SQL BEGIN ATOMIC END;",
            "character varying",
        ),
        (
            "CREATE FUNCTION result2(OUT answer int, OUT other text) LANGUAGE SQL BEGIN ATOMIC END;",
            "record",
        ),
        ("CREATE FUNCTION result3() RETURNS SETOF int LANGUAGE SQL BEGIN ATOMIC END;", "integer"),
        (
            "CREATE FUNCTION result4() RETURNS TABLE(answer int) LANGUAGE SQL BEGIN ATOMIC END;",
            "integer",
        ),
        (
            "CREATE FUNCTION result5() RETURNS numeric(5,2) LANGUAGE SQL BEGIN ATOMIC END;",
            "numeric",
        ),
    ] {
        refuses(
            sql,
            "42P13",
            &format!("return type mismatch in function declared to return {data_type}"),
        );
    }
    refuses(
        "CREATE ROLE s7_owner; CREATE ROLE s7_group; GRANT CREATE ON SCHEMA public TO s7_owner; ALTER DEFAULT PRIVILEGES FOR ROLE s7_owner GRANT EXECUTE ON FUNCTIONS TO s7_group; ALTER DEFAULT PRIVILEGES FOR ROLE s7_owner REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC, s7_owner; SET ROLE s7_owner; CREATE FUNCTION \"NoExec\"() RETURNS int LANGUAGE SQL AS 'SELECT 1';",
        "42501",
        "permission denied for function NoExec",
    );
}

#[test]
fn forbidden_body_clause_shapes() {
    for (statement, token) in [
        ("UPDATE t JOIN t b ON true SET id=1", "JOIN"),
        ("UPDATE t LEFT JOIN t b ON true SET id=1", "LEFT"),
        ("UPDATE t NATURAL JOIN t b SET id=1", "NATURAL"),
        ("MERGE INTO t USING t b ON true WHEN MATCHED THEN UPDATE SET *", "*"),
        ("MERGE INTO t USING t b ON true WHEN MATCHED THEN DELETE OUTPUT t.id", "OUTPUT"),
    ] {
        refuses(
            &format!(
                "CREATE TABLE t(id int); CREATE FUNCTION bad() RETURNS void LANGUAGE SQL BEGIN ATOMIC {statement}; END;"
            ),
            "42601",
            &format!("syntax error at or near \"{token}\""),
        );
    }
}

#[test]
fn utility_body_command_tags() {
    for (statement, tag) in [
        ("DISCARD ALL", "DISCARD ALL"),
        ("DISCARD PLANS", "DISCARD PLANS"),
        ("DISCARD TEMPORARY", "DISCARD TEMP"),
        ("DISCARD SEQUENCES", "DISCARD SEQUENCES"),
        ("DEALLOCATE missing", "DEALLOCATE"),
        ("DEALLOCATE ALL", "DEALLOCATE ALL"),
        ("CLOSE cur", "CLOSE CURSOR"),
        ("CLOSE ALL", "CLOSE CURSOR ALL"),
        ("START TRANSACTION", "START TRANSACTION"),
        ("ROLLBACK TO SAVEPOINT point", "ROLLBACK"),
        ("CREATE MATERIALIZED VIEW nextview AS SELECT 1", "CREATE MATERIALIZED VIEW"),
        ("GRANT s7_util TO postgres", "GRANT ROLE"),
        ("REVOKE s7_util FROM postgres", "REVOKE ROLE"),
        ("TRUNCATE t", "TRUNCATE TABLE"),
        ("LOCK t", "LOCK TABLE"),
        ("SET TRANSACTION READ ONLY", "SET"),
        (
            "CREATE TEXT SEARCH DICTIONARY nextdict (TEMPLATE = simple)",
            "CREATE TEXT SEARCH DICTIONARY",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION nextconfig (COPY = english)",
            "CREATE TEXT SEARCH CONFIGURATION",
        ),
        (
            "ALTER TEXT SEARCH CONFIGURATION english RENAME TO other",
            "ALTER TEXT SEARCH CONFIGURATION",
        ),
    ] {
        refuses(
            &format!(
                "CREATE FUNCTION utility_probe() RETURNS void LANGUAGE SQL BEGIN ATOMIC {statement}; END;"
            ),
            "0A000",
            &format!("{tag} is not yet supported in unquoted SQL function body"),
        );
    }
}

#[test]
fn dependents_are_named_as_stored() {
    for (reader, stored) in [
        ("CREATE VIEW Reader AS SELECT a FROM t;", "reader"),
        ("CREATE MATERIALIZED VIEW \"Reader\" AS SELECT a FROM t;", "Reader"),
        (
            "CREATE FUNCTION Reader() RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT a FROM t; END;",
            "reader",
        ),
    ] {
        let sql = format!("CREATE TABLE t(a int); {reader} DROP TABLE t;");
        let error =
            sql_traits::prelude::ParserDB::parse::<sqlparser::dialect::PostgreSqlDialect>(&sql)
                .expect_err("a read table cannot be dropped");
        let sql_traits::errors::Error::RelationHasDependents { dependent_name, .. } = error else {
            panic!("{error:?} for {sql}");
        };
        assert_eq!(dependent_name, stored, "{sql}");
    }
}

#[test]
fn view_column_list_message() {
    let error = sql_traits::errors::Error::ViewColumnListTooLong { view_name: "w".into() };
    let postgres = error.postgres_error().expect("a view column list has a PostgreSQL refusal");
    assert_eq!(postgres.sqlstate().as_str(), "42601");
    assert_eq!(postgres.message(), "CREATE VIEW specifies more column names than columns");
}

#[test]
fn rename_capture_has_no_postgresql_counterpart() {
    super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>(
        "CREATE TABLE p12t (a INT, b INT);
         CREATE FUNCTION rename_probe() RETURNS INT LANGUAGE sql BEGIN ATOMIC
             WITH d AS (DELETE FROM p12t WHERE a > 0 RETURNING p12t) SELECT 1 FROM d;
         END;
         ALTER TABLE p12t RENAME COLUMN a TO p12t;",
    );
}

#[test]
fn cloned_trigger_dependency_qualifies_tables() {
    refuses(
        "CREATE SCHEMA s7_schema;
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;
         CREATE TABLE s7_schema.\"Root\"(id int) PARTITION BY RANGE(id);
         CREATE TABLE s7_schema.\"Child\" PARTITION OF s7_schema.\"Root\" FOR VALUES FROM (0) TO (10);
         CREATE TRIGGER \"Tr\" BEFORE INSERT ON s7_schema.\"Root\" FOR EACH ROW EXECUTE FUNCTION trg();
         DROP TRIGGER \"Tr\" ON s7_schema.\"Child\";",
        "2BP01",
        "cannot drop trigger Tr on table s7_schema.\"Child\" because trigger Tr on table s7_schema.\"Root\" requires it",
    );
}

#[test]
fn table_column_list_message() {
    let error = sql_traits::errors::Error::TableColumnListTooLong { table_name: "x".into() };
    let postgres = error.postgres_error().expect("a table column list has a PostgreSQL refusal");
    assert_eq!(postgres.sqlstate().as_str(), "42601");
    assert_eq!(postgres.message(), "too many column names were specified");
}
