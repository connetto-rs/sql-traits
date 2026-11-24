#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use super::refuses;

#[test]
fn foreign_key_columns_must_exist() {
    for (name, stored) in [("NoSuch", "nosuch"), ("\"NoSuch\"", "NoSuch")] {
        for constraint in [
            format!("FOREIGN KEY ({name}) REFERENCES t (id)"),
            format!("FOREIGN KEY (id) REFERENCES t ({name})"),
        ] {
            refuses(
                &format!(
                    "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT); CREATE TABLE c (id INT, {constraint});"
                ),
                "42703",
                &format!("column \"{stored}\" referenced in foreign key constraint does not exist"),
            );
        }
    }
}

#[test]
fn grants_and_sequence_owners_require_columns() {
    for (name, stored) in [("NoSuch", "nosuch"), ("\"NoSuch\"", "NoSuch")] {
        for statement in [
            format!("GRANT SELECT ({name}) ON t TO PUBLIC"),
            format!("CREATE SEQUENCE seq OWNED BY t.{name}"),
        ] {
            refuses(
                &format!("CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT); {statement};"),
                "42703",
                &format!("column \"{stored}\" of relation \"t\" does not exist"),
            );
        }
    }
}

#[test]
fn foreign_keys_require_matching_unique_keys() {
    for (statement, message) in [
        (
            "CREATE TABLE c (id INT REFERENCES t(a));",
            "there is no unique constraint matching given keys for referenced table \"t\"",
        ),
        (
            "CREATE TABLE c (id INT, a INT, FOREIGN KEY(id,a) REFERENCES t(id));",
            "number of referencing and referenced columns for foreign key disagree",
        ),
    ] {
        refuses(
            &format!("CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT); {statement}"),
            "42830",
            message,
        );
    }
    refuses(
        "CREATE TABLE t (id INT PRIMARY KEY); CREATE TABLE c (id INT REFERENCES t NOT DEFERRABLE INITIALLY DEFERRED);",
        "42601",
        "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
    );
}

#[test]
fn foreign_keys_without_equality() {
    for (statement, name) in [
        (
            "CREATE TABLE \"F\" (\"R\" point, CONSTRAINT \"Fk\" FOREIGN KEY (\"R\") REFERENCES r (id));",
            "Fk",
        ),
        ("CREATE TABLE f (x point REFERENCES r);", "f_x_fkey"),
        ("CREATE TABLE f (x point); ALTER TABLE f ADD FOREIGN KEY (x) REFERENCES r;", "f_x_fkey"),
    ] {
        refuses(
            &format!("CREATE TABLE r (id INT PRIMARY KEY); {statement}"),
            "42804",
            &format!("foreign key constraint \"{name}\" cannot be implemented"),
        );
    }
}

#[test]
fn foreign_key_target_keys_and_actions() {
    for referenced in ["(a, a)", "(a, \"a\")"] {
        refuses(
            &format!(
                "CREATE TABLE t (a INT, b INT, UNIQUE (a, b)); CREATE TABLE c (x INT, y INT, FOREIGN KEY (x, y) REFERENCES t {referenced});"
            ),
            "42830",
            "foreign key referenced-columns list must not contain duplicates",
        );
    }
    for (action, clause) in [
        ("ON UPDATE CASCADE", "UPDATE"),
        ("ON UPDATE SET NULL", "UPDATE"),
        ("ON DELETE SET NULL", "DELETE"),
        ("ON DELETE SET DEFAULT", "DELETE"),
    ] {
        refuses(
            &format!(
                "CREATE TABLE t (id INT PRIMARY KEY); CREATE TABLE c (a INT, g INT GENERATED ALWAYS AS (a * 2) STORED REFERENCES t {action});"
            ),
            "42601",
            &format!(
                "invalid ON {clause} action for foreign key constraint containing generated column"
            ),
        );
    }
    for (target, reference, message) in [
        (
            "t (id INT PRIMARY KEY DEFERRABLE)",
            "t",
            "cannot use a deferrable primary key for referenced table \"t\"",
        ),
        (
            "\"T\" (id INT PRIMARY KEY DEFERRABLE)",
            "\"T\"",
            "cannot use a deferrable primary key for referenced table \"T\"",
        ),
        (
            "t (id INT UNIQUE DEFERRABLE)",
            "t (id)",
            "cannot use a deferrable unique constraint for referenced table \"t\"",
        ),
    ] {
        refuses(
            &format!("CREATE TABLE {target}; CREATE TABLE c (id INT REFERENCES {reference});"),
            "55000",
            message,
        );
    }
}

#[test]
fn key_access_methods_and_primary_keys() {
    for (method, stored) in [("hash", "hash"), ("gist", "gist"), ("HASH", "hash")] {
        refuses(
            &format!("CREATE TABLE t (a INT); CREATE UNIQUE INDEX i ON t USING {method} (a);"),
            "0A000",
            &format!("access method \"{stored}\" does not support unique indexes"),
        );
    }
    for (sql, method) in [
        ("CREATE TABLE t (a INT[], EXCLUDE USING gin (a WITH &&));", "gin"),
        ("CREATE TABLE t (a INT, EXCLUDE USING brin (a WITH =));", "brin"),
    ] {
        refuses(
            sql,
            "0A000",
            &format!("access method \"{method}\" does not support exclusion constraints"),
        );
    }
    for (sql, table) in [
        ("CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY);", "t"),
        ("CREATE TABLE \"T\" (a INT PRIMARY KEY, b INT, PRIMARY KEY (b));", "T"),
        ("CREATE TABLE t (a INT PRIMARY KEY, b INT); ALTER TABLE t ADD PRIMARY KEY (b);", "t"),
    ] {
        refuses(
            sql,
            "42P16",
            &format!("multiple primary keys for table \"{table}\" are not allowed"),
        );
    }
}

#[test]
fn function_row_estimates() {
    for rows in ["0", "-5"] {
        refuses(
            &format!(
                "CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql ROWS {rows} AS 'SELECT 1';"
            ),
            "22023",
            "ROWS must be positive",
        );
    }
    for sql in [
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql ROWS 10 AS 'SELECT 1';",
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1'; ALTER FUNCTION f() ROWS 10;",
    ] {
        refuses(sql, "22023", "ROWS is not applicable when function does not return a set");
    }
}

#[test]
fn relations_must_exist() {
    for statement in [
        "CREATE TRIGGER tr BEFORE INSERT ON NoSuch EXECUTE FUNCTION f();",
        "CREATE POLICY p ON NoSuch;",
        "CREATE RULE r AS ON INSERT TO NoSuch DO NOTHING;",
        "CREATE INDEX ix ON NoSuch(id);",
        "CREATE SEQUENCE seq OWNED BY NoSuch.id;",
        "CREATE TABLE d (id int DEFAULT nextval('NoSuch'));",
    ] {
        refuses(statement, "42P01", "relation \"nosuch\" does not exist");
    }
}

#[test]
fn sequence_ownership_and_dependencies() {
    for (sql, state, message) in [
        (
            "CREATE TABLE t(id int); CREATE SEQUENCE seq OWNED BY t;",
            "42601",
            "invalid OWNED BY option",
        ),
        ("COMMENT ON COLUMN id IS 'x';", "42601", "column name must be qualified"),
        (
            "CREATE TABLE t(id int); CREATE SCHEMA other; CREATE SEQUENCE other.seq OWNED BY t.id;",
            "55000",
            "sequence must be in same schema as table it is linked to",
        ),
        (
            "CREATE SEQUENCE seq; CREATE SEQUENCE seq2 OWNED BY seq.id;",
            "42809",
            "sequence cannot be owned by relation \"seq\"",
        ),
        (
            "CREATE TABLE ident (id int GENERATED ALWAYS AS IDENTITY); DROP SEQUENCE ident_id_seq;",
            "2BP01",
            "cannot drop sequence ident_id_seq because column id of table ident requires it",
        ),
        (
            "CREATE TABLE serialt (id SERIAL); DROP SEQUENCE serialt_id_seq;",
            "2BP01",
            "cannot drop sequence serialt_id_seq because other objects depend on it",
        ),
        ("CREATE TABLE d (id int DEFAULT nextval(''));", "42602", "invalid name syntax"),
        (
            "CREATE TABLE d (id int DEFAULT nextval('a.b.c.d'));",
            "42601",
            "improper relation name (too many dotted names): a.b.c.d",
        ),
        ("CREATE TABLE d (id int DEFAULT nextval('\"foo'));", "42602", "invalid name syntax"),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn expressions_and_dependencies() {
    for (sql, state, message) in [
        (
            "CREATE TABLE d (id int DEFAULT (SELECT 1));",
            "0A000",
            "cannot use subquery in DEFAULT expression",
        ),
        (
            "CREATE TABLE d (id int, a int DEFAULT id);",
            "0A000",
            "cannot use column reference in DEFAULT expression",
        ),
        (
            "CREATE TABLE t(id int); CREATE TRIGGER tr BEFORE INSERT ON t EXECUTE FUNCTION NoSuch();",
            "42883",
            "function nosuch() does not exist",
        ),
        (
            "CREATE TABLE parent(id int); CREATE TABLE child() INHERITS(parent); DROP TABLE parent;",
            "2BP01",
            "cannot drop table parent because other objects depend on it",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn collation_definitions() {
    for (sql, state, message) in [
        (
            "CREATE TABLE coll (id int COLLATE \"C\");",
            "42804",
            "collations are not supported by type integer",
        ),
        (
            "CREATE TABLE coll (id text COLLATE \"C\" COLLATE \"POSIX\");",
            "42601",
            "multiple COLLATE clauses not allowed",
        ),
        (
            "CREATE COLLATION col (provider = libc, provider = libc, locale = 'C');",
            "42601",
            "conflicting or redundant options",
        ),
        (
            "CREATE COLLATION col (provider = nonsense, locale = 'C');",
            "42P17",
            "unrecognized collation provider: nonsense",
        ),
        (
            "CREATE COLLATION col (provider = 'nonsense', locale = 'C');",
            "42P17",
            "unrecognized collation provider: nonsense",
        ),
        (
            "CREATE COLLATION col (provider = 123, locale = 'C');",
            "42P17",
            "unrecognized collation provider: 123",
        ),
        (
            "CREATE COLLATION col (deterministic = nonsense, locale = 'C');",
            "42601",
            "deterministic requires a Boolean value",
        ),
        (
            "CREATE COLLATION col (deterministic = 'yes', locale = 'C');",
            "42601",
            "deterministic requires a Boolean value",
        ),
        (
            "CREATE COLLATION col (deterministic = 5, locale = 'C');",
            "42601",
            "deterministic requires a Boolean value",
        ),
        (
            "CREATE COLLATION col (NoSuch = true, locale = 'C');",
            "42601",
            "collation attribute \"nosuch\" not recognized",
        ),
        (
            "CREATE COLLATION col (\"NoSuch\" = true, locale = 'C');",
            "42601",
            "collation attribute \"NoSuch\" not recognized",
        ),
        (
            "CREATE COLLATION col (x.provider = libc, locale = 'C');",
            "42601",
            "syntax error at or near \".\"",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn trigger_firing_shapes() {
    let setup = "CREATE TABLE t(id int); CREATE VIEW v AS SELECT * FROM t;";
    for (statement, state, message) in [
        (
            "CREATE TRIGGER tr INSTEAD OF INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
            "42809",
            "\"t\" is a table",
        ),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();",
            "42809",
            "\"v\" is a view",
        ),
        ("CREATE TRIGGER tr AFTER TRUNCATE ON v EXECUTE FUNCTION f();", "42809", "\"v\" is a view"),
        (
            "CREATE TRIGGER tr INSTEAD OF INSERT ON v EXECUTE FUNCTION f();",
            "0A000",
            "INSTEAD OF triggers must be FOR EACH ROW",
        ),
        (
            "CREATE TRIGGER tr INSTEAD OF UPDATE OF id ON v FOR EACH ROW EXECUTE FUNCTION f();",
            "0A000",
            "INSTEAD OF triggers cannot have column lists",
        ),
        (
            "CREATE TRIGGER tr INSTEAD OF INSERT ON v FOR EACH ROW WHEN (true) EXECUTE FUNCTION f();",
            "0A000",
            "INSTEAD OF triggers cannot have WHEN conditions",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON v REFERENCING NEW TABLE AS nt EXECUTE FUNCTION f();",
            "42809",
            "\"v\" is a view",
        ),
    ] {
        refuses(&format!("{setup} {statement}"), state, message);
    }
}

const TRANSITION_SETUP: &str = "CREATE TABLE t(id int);
    CREATE TABLE p(id int) PARTITION BY RANGE (id);
    CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
    CREATE TABLE parent(id int); CREATE TABLE child() INHERITS (parent);
    CREATE FOREIGN TABLE ft(id int) SERVER s;
    CREATE TABLE lone(id int);
    CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;";
const PER_STATEMENT: &str = "FOR EACH STATEMENT EXECUTE FUNCTION f();";
const PER_ROW: &str = "FOR EACH ROW EXECUTE FUNCTION f();";

#[test]
fn transition_tables_refused_on_relations_postgres_refuses_them_on() {
    for (trigger, state, message) in [
        (
            format!(
                "CREATE TRIGGER tr AFTER INSERT ON t REFERENCING NEW TABLE AS s.x {PER_STATEMENT}"
            ),
            "42601",
            "syntax error at or near \".\"",
        ),
        (
            format!(
                "CREATE TRIGGER tr AFTER INSERT ON ft REFERENCING NEW TABLE AS n {PER_STATEMENT}"
            ),
            "42809",
            "\"ft\" is a foreign table",
        ),
        (
            format!("CREATE TRIGGER tr AFTER INSERT ON p REFERENCING NEW TABLE AS n {PER_ROW}"),
            "0A000",
            "\"p\" is a partitioned table",
        ),
        (
            format!("CREATE TRIGGER tr BEFORE INSERT ON p REFERENCING NEW TABLE AS n {PER_ROW}"),
            "0A000",
            "\"p\" is a partitioned table",
        ),
        (
            format!(
                "CREATE TRIGGER tr INSTEAD OF INSERT ON p REFERENCING NEW TABLE AS n {PER_ROW}"
            ),
            "42809",
            "\"p\" is a table",
        ),
        (
            format!("CREATE TRIGGER tr AFTER INSERT ON p1 REFERENCING OLD TABLE AS n {PER_ROW}"),
            "0A000",
            "ROW triggers with transition tables are not supported on partitions",
        ),
        (
            format!("CREATE TRIGGER tr AFTER INSERT ON child REFERENCING NEW TABLE AS n {PER_ROW}"),
            "0A000",
            "ROW triggers with transition tables are not supported on inheritance children",
        ),
    ] {
        refuses(&format!("{TRANSITION_SETUP} {trigger}"), state, message);
    }
}

#[test]
fn transition_tables_refused_in_triggers_postgres_refuses_them_in() {
    for (referencing, state, message) in [
        (
            "BEFORE TRUNCATE ON t REFERENCING OLD TABLE AS a OLD TABLE AS b",
            "42P17",
            "transition table name can only be specified for an AFTER trigger",
        ),
        (
            "AFTER TRUNCATE ON t REFERENCING NEW TABLE AS n",
            "0A000",
            "TRUNCATE triggers with transition tables are not supported",
        ),
        (
            "AFTER INSERT OR UPDATE ON t REFERENCING NEW TABLE AS n",
            "0A000",
            "transition tables cannot be specified for triggers with more than one event",
        ),
        (
            "AFTER UPDATE OF id ON t REFERENCING NEW TABLE AS n",
            "0A000",
            "transition tables cannot be specified for triggers with column lists",
        ),
        (
            "AFTER DELETE ON t REFERENCING NEW TABLE AS n",
            "42P17",
            "NEW TABLE can only be specified for an INSERT or UPDATE trigger",
        ),
        (
            "AFTER UPDATE ON t REFERENCING NEW TABLE AS a NEW TABLE AS b",
            "42P17",
            "NEW TABLE cannot be specified multiple times",
        ),
        (
            "AFTER INSERT ON t REFERENCING OLD TABLE AS a NEW TABLE AS a",
            "42P17",
            "OLD TABLE can only be specified for a DELETE or UPDATE trigger",
        ),
        (
            "AFTER UPDATE ON t REFERENCING OLD TABLE AS a OLD TABLE AS b",
            "42P17",
            "OLD TABLE cannot be specified multiple times",
        ),
        (
            "AFTER UPDATE ON t REFERENCING NEW TABLE AS x OLD TABLE AS \"x\"",
            "42P17",
            "OLD TABLE name and NEW TABLE name cannot be the same",
        ),
    ] {
        refuses(
            &format!("{TRANSITION_SETUP} CREATE TRIGGER tr {referencing} {PER_STATEMENT}"),
            state,
            message,
        );
    }
}

#[test]
fn a_row_trigger_with_transition_tables_keeps_its_table_out_of_hierarchies() {
    let triggers = format!(
        "CREATE TRIGGER zz AFTER INSERT ON lone REFERENCING NEW TABLE AS n {PER_ROW}
         CREATE TRIGGER \"B\" AFTER INSERT ON lone REFERENCING NEW TABLE AS n {PER_ROW}
         CREATE TRIGGER \"A\" AFTER INSERT ON lone REFERENCING NEW TABLE AS n {PER_STATEMENT}"
    );
    for (alteration, message) in [
        (
            "ALTER TABLE p ATTACH PARTITION lone FOR VALUES FROM (10) TO (20);",
            "trigger \"B\" prevents table \"lone\" from becoming a partition",
        ),
        (
            "ALTER TABLE lone INHERIT parent;",
            "trigger \"B\" prevents table \"lone\" from becoming an inheritance child",
        ),
    ] {
        refuses(&format!("{TRANSITION_SETUP} {triggers} {alteration}"), "0A000", message);
    }
}

#[test]
fn rule_actions_require_valid_relations_and_columns() {
    let setup = "CREATE TABLE t(id int PRIMARY KEY, a int, b text); CREATE TABLE parent(id int); CREATE SEQUENCE seq;";
    for (statement, state, message) in [
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO NoSuch VALUES (1);",
            "42P01",
            "relation \"nosuch\" does not exist",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO t VALUES (1,2,'x',4);",
            "42601",
            "INSERT has more expressions than target columns",
        ),
        (
            "CREATE RULE r AS ON INSERT TO t DO ALSO SELECT id FROM t, parent;",
            "42702",
            "column reference \"id\" is ambiguous",
        ),
        (
            "CREATE RULE r AS ON INSERT TO seq DO NOTHING;",
            "42809",
            "relation \"seq\" cannot have rules",
        ),
        (
            "CREATE RULE r AS ON SELECT TO t DO INSTEAD SELECT id,a,b FROM t;",
            "42809",
            "relation \"t\" cannot have ON SELECT rules",
        ),
    ] {
        refuses(&format!("{setup} {statement}"), state, message);
    }
}

#[test]
fn sequence_parameters() {
    for (statement, state, message) in [
        ("CREATE SEQUENCE z INCREMENT 0;", "22023", "INCREMENT must not be zero"),
        (
            "CREATE SEQUENCE z AS boolean;",
            "22023",
            "sequence type must be smallint, integer, or bigint",
        ),
        (
            "CREATE TABLE z(id boolean GENERATED ALWAYS AS IDENTITY);",
            "22023",
            "identity column type must be smallint, integer, or bigint",
        ),
        (
            "CREATE SEQUENCE z INCREMENT 9223372036854775808;",
            "22003",
            "value \"9223372036854775808\" is out of range for type bigint",
        ),
        (
            "CREATE SEQUENCE z AS smallint MAXVALUE 32768;",
            "22023",
            "MAXVALUE (32768) is out of range for sequence data type smallint",
        ),
        (
            "CREATE SEQUENCE z AS smallint MINVALUE -32769;",
            "22023",
            "MINVALUE (-32769) is out of range for sequence data type smallint",
        ),
        (
            "CREATE SEQUENCE z MINVALUE 3 MAXVALUE 2;",
            "22023",
            "MINVALUE (3) must be less than MAXVALUE (2)",
        ),
        (
            "CREATE SEQUENCE z MINVALUE 3 START 2;",
            "22023",
            "START value (2) cannot be less than MINVALUE (3)",
        ),
        (
            "CREATE SEQUENCE z MAXVALUE 3 START 4;",
            "22023",
            "START value (4) cannot be greater than MAXVALUE (3)",
        ),
        ("CREATE SEQUENCE z CACHE 0;", "22023", "CACHE (0) must be greater than zero"),
    ] {
        refuses(statement, state, message);
    }
}

#[test]
fn function_clause_rules() {
    for (expression, state, message) in [
        ("now(*)", "42809", "now(*) specified, but now is not an aggregate function"),
        (
            "lower(DISTINCT b)",
            "42809",
            "DISTINCT specified, but lower is not an aggregate function",
        ),
        (
            "left(b) WITHIN GROUP (ORDER BY 1)",
            "42809",
            "WITHIN GROUP specified, but left is not an aggregate function",
        ),
        (
            "lower(b ORDER BY b)",
            "42809",
            "ORDER BY specified, but lower is not an aggregate function",
        ),
        (
            "lower(b) FILTER (WHERE id>0)",
            "42809",
            "FILTER specified, but lower is not an aggregate function",
        ),
        (
            "lower(b) OVER ()",
            "42809",
            "OVER specified, but lower is not a window function nor an aggregate function",
        ),
        (
            "percentile_cont(0.5, 1.0)",
            "42809",
            "WITHIN GROUP is required for ordered-set aggregate percentile_cont",
        ),
        (
            "percentile_cont(0.5) WITHIN GROUP (ORDER BY id) OVER ()",
            "0A000",
            "OVER is not supported for ordered-set aggregate percentile_cont",
        ),
        (
            "string_agg(b) WITHIN GROUP (ORDER BY b)",
            "42809",
            "string_agg is not an ordered-set aggregate, so it cannot have WITHIN GROUP",
        ),
        ("row_number()", "42809", "window function row_number requires an OVER clause"),
        (
            "lag(id) WITHIN GROUP (ORDER BY 1) OVER ()",
            "42809",
            "window function lag cannot have WITHIN GROUP",
        ),
        ("count()", "42809", "count(*) must be used to call a parameterless aggregate function"),
        (
            "json_object_agg(key => b, value => id)",
            "0A000",
            "aggregates cannot use named arguments",
        ),
        ("count(DISTINCT id) OVER ()", "0A000", "DISTINCT is not implemented for window functions"),
        (
            "sum(id ORDER BY id) OVER ()",
            "0A000",
            "aggregate ORDER BY is not implemented for window functions",
        ),
        (
            "row_number() FILTER (WHERE id>0) OVER ()",
            "0A000",
            "FILTER is not implemented for non-aggregate window functions",
        ),
        (
            "string_agg(b, ',' ORDER BY b) WITHIN GROUP (ORDER BY b)",
            "42601",
            "cannot use multiple ORDER BY clauses with WITHIN GROUP",
        ),
        (
            "sum(DISTINCT id) WITHIN GROUP (ORDER BY id)",
            "42601",
            "cannot use DISTINCT with WITHIN GROUP",
        ),
        ("NoSuch(1)", "42883", "function nosuch(integer) does not exist"),
        ("1 + true", "42883", "operator does not exist: integer + boolean"),
        (
            "array_shuffle('x')",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
    ] {
        refuses(
            &format!(
                "CREATE TABLE t(id int, b text); CREATE VIEW v AS SELECT {expression} AS result FROM t;"
            ),
            state,
            message,
        );
    }
    refuses(
        "CREATE PROCEDURE s3_proc() LANGUAGE SQL AS 'SELECT 1'; CREATE VIEW v AS SELECT s3_proc();",
        "42809",
        "s3_proc() is a procedure",
    );
}

#[test]
fn index_grammar_refusals() {
    for (sql, token) in [
        ("CREATE TABLE t(id int); CREATE INDEX other.ix ON t(id);", "."),
        ("CREATE TABLE uq (a text, UNIQUE (lower(a)));", "("),
        ("CREATE TABLE uq (a int, UNIQUE (a + 1));", "+"),
        ("CREATE TABLE uq (a int, UNIQUE (uq.a));", "."),
        ("CREATE TABLE uq (a int, UNIQUE ((a + 1)));", "("),
    ] {
        refuses(sql, "42601", &format!("syntax error at or near \"{token}\""));
    }
}

#[test]
fn quoted_names_in_dependency_messages() {
    refuses(
        "CREATE TABLE \"Parent\" (id int); CREATE TABLE \"Child\"() INHERITS(\"Parent\"); DROP TABLE \"Parent\";",
        "2BP01",
        "cannot drop table \"Parent\" because other objects depend on it",
    );
    refuses(
        "CREATE TABLE \"Identity\" (\"Id\" int GENERATED ALWAYS AS IDENTITY); DROP SEQUENCE \"Identity_Id_seq\";",
        "2BP01",
        "cannot drop sequence \"Identity_Id_seq\" because column Id of table \"Identity\" requires it",
    );
}

#[test]
fn omitted_referenced_columns_require_primary_key() {
    refuses(
        "CREATE TABLE nokey(id int); CREATE TABLE fk(id int REFERENCES nokey);",
        "42704",
        "there is no primary key for referenced table \"nokey\"",
    );
}

#[test]
fn empty_index_ast() {
    use sql_traits::prelude::ParserDB;
    use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};
    let mut statements = Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE TABLE t(id int); CREATE INDEX ix ON t(id);",
    )
    .expect("the statements parse");
    let Statement::CreateIndex(index) = &mut statements[1] else {
        panic!("the second statement creates an index")
    };
    index.columns.clear();
    let error =
        ParserDB::from_statements(statements, "test".into()).expect_err("an index needs a column");
    let refusal = error.postgres_error().expect("an empty index has a PostgreSQL refusal");
    assert_eq!(
        (refusal.sqlstate().as_str(), refusal.message()),
        ("42601", "syntax error at or near \")\"")
    );
}

#[test]
fn select_rules_on_views() {
    let setup = "CREATE TABLE t(id int); CREATE VIEW v AS SELECT id FROM t;";
    refuses(
        &format!("{setup} CREATE RULE r AS ON SELECT TO v DO INSTEAD SELECT id FROM t;"),
        "55000",
        "\"v\" is already a view",
    );
    for action in ["NOTHING", "ALSO SELECT id FROM t"] {
        refuses(
            &format!("{setup} CREATE RULE r AS ON SELECT TO v DO {action};"),
            "0A000",
            "rules on SELECT must have action INSTEAD SELECT",
        );
    }
    super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>(&format!(
        "{setup} CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT id FROM t;"
    ));
}

#[test]
fn subquery_expression_locations() {
    for (sql, place) in [
        ("CREATE TABLE subq(id int CHECK ((SELECT true)));", "check constraint"),
        (
            "CREATE TABLE subq(id int, gen int GENERATED ALWAYS AS ((SELECT 1)) STORED);",
            "column generation expression",
        ),
        ("CREATE TABLE t(id int); CREATE INDEX subix ON t(((SELECT 1)));", "index expression"),
        (
            "CREATE TABLE t(id int); CREATE INDEX subix ON t(id) WHERE (SELECT true);",
            "index predicate",
        ),
        (
            "CREATE TABLE subpart(id int) PARTITION BY RANGE (((SELECT 1)));",
            "partition key expression",
        ),
        (
            "CREATE TABLE rangepart(id int) PARTITION BY RANGE(id); CREATE TABLE subpart PARTITION OF rangepart FOR VALUES FROM ((SELECT 1)) TO (2);",
            "partition bound",
        ),
        (
            "CREATE TABLE t(a int); ALTER TABLE t ALTER COLUMN a TYPE bigint USING (SELECT 1);",
            "transform expression",
        ),
        (
            "CREATE TABLE t(a int); CREATE TRIGGER subtr BEFORE INSERT ON t FOR EACH ROW WHEN ((SELECT true)) EXECUTE FUNCTION f();",
            "trigger WHEN condition",
        ),
    ] {
        refuses(sql, "0A000", &format!("cannot use subquery in {place}"));
    }
}

#[test]
fn ambiguous_and_polymorphic_overloads() {
    let functions = "CREATE FUNCTION h(int8,int4) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT true'; CREATE FUNCTION h(int4,int8) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT true';";
    refuses(
        &format!("{functions} CREATE TABLE amb(a int2 CHECK(h(a,a)));"),
        "42725",
        "function h(smallint, smallint) is not unique",
    );
    refuses(
        &format!(
            "{functions} CREATE OPERATOR <<< (LEFTARG=int8,RIGHTARG=int4,FUNCTION=h); CREATE OPERATOR <<< (LEFTARG=int4,RIGHTARG=int8,FUNCTION=h); CREATE TABLE amb(a int2 CHECK(a <<< a));"
        ),
        "42725",
        "operator is not unique: smallint <<< smallint",
    );
    refuses(
        "CREATE FUNCTION poly(anyelement,anyelement) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT true'; CREATE OPERATOR <<<< (LEFTARG=anyelement,RIGHTARG=anyelement,FUNCTION=poly); CREATE TABLE amb(a int CHECK('a' <<<< 'a'));",
        "42804",
        "could not determine polymorphic type because input has type unknown",
    );
}

#[test]
fn sequence_integer_literals() {
    for (sql, state, message) in [
        (
            "CREATE SEQUENCE bad INCREMENT 1.5;",
            "22P02",
            "invalid input syntax for type bigint: \"1.5\"",
        ),
        (
            "CREATE SEQUENCE bad START 1e3;",
            "22P02",
            "invalid input syntax for type bigint: \"1e3\"",
        ),
        ("CREATE SEQUENCE bad START $1;", "42601", "syntax error at or near \"$1\""),
        (
            "CREATE SEQUENCE bad INCREMENT +9223372036854775808;",
            "22003",
            "value \"9223372036854775808\" is out of range for type bigint",
        ),
        (
            "CREATE SEQUENCE bad START -1.5;",
            "22P02",
            "invalid input syntax for type bigint: \"-1.5\"",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn explicit_schema_must_exist() {
    for sql in [
        "CREATE TRIGGER tr BEFORE INSERT ON absent.t EXECUTE FUNCTION f();",
        "CREATE INDEX ix ON absent.t(id);",
        "CREATE POLICY p ON absent.t;",
        "CREATE SEQUENCE seq3 OWNED BY absent.t.id;",
        "CREATE RULE r AS ON INSERT TO absent.t DO NOTHING;",
        "CREATE TABLE t(id int); CREATE TRIGGER tr BEFORE INSERT ON t EXECUTE FUNCTION absent.f();",
    ] {
        refuses(sql, "3F000", "schema \"absent\" does not exist");
    }
    refuses(
        "CREATE TABLE d (id int DEFAULT nextval('absent.t'));",
        "42P01",
        "relation \"absent.t\" does not exist",
    );
    refuses(
        "CREATE TABLE t(id int); CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO absent.t VALUES (1);",
        "42P01",
        "relation \"absent.t\" does not exist",
    );
}

#[test]
fn owners_require_roles() {
    for (role, stored) in [("s3_NoSuch", "s3_nosuch"), ("\"s3_NoSuch\"", "s3_NoSuch")] {
        for sql in [
            format!("CREATE TABLE t(id int); ALTER TABLE t OWNER TO {role};"),
            format!(
                "CREATE FUNCTION h(int8,int4) RETURNS bool LANGUAGE sql AS 'SELECT true'; ALTER FUNCTION h(int8,int4) OWNER TO {role};"
            ),
            format!("ALTER SCHEMA public OWNER TO {role};"),
            format!("CREATE SCHEMA owner_schema AUTHORIZATION {role};"),
        ] {
            refuses(&sql, "42704", &format!("role \"{stored}\" does not exist"));
        }
    }
}

#[test]
fn model_limitations_have_no_postgres_counterpart() {
    use sql_traits::prelude::*;
    use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};
    super::refuses_without_counterpart::<PostgreSqlDialect>(
        "CREATE VIEW series_view AS SELECT * FROM generate_series(1,3); CREATE SEQUENCE series_sequence OWNED BY series_view.generate_series;",
    );
    super::refuses_without_counterpart::<SQLiteDialect>(
        "CREATE TABLE t(id int PRIMARY KEY); CREATE TABLE fk_qual(id int REFERENCES public.t(id));",
    );
    let catalog =
        PostgresCatalog::empty().with_collation(PostgresCatalogCollation::new("ci", false));
    let error = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("CREATE TABLE coll_custom(name citext COLLATE ci);")
        .expect_err("the catalog lacks the collatability of citext");
    assert!(matches!(error, sql_traits::errors::Error::ColumnTypeCollatabilityNotInCatalog { .. }));
    assert_eq!(error.postgres_error(), None);
}

#[test]
fn provider_must_support_nondeterministic_collations() {
    refuses(
        "CREATE COLLATION det5 (provider=libc,locale='C',deterministic='false');",
        "0A000",
        "nondeterministic collations not supported with this provider",
    );
}
