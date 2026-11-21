use super::refuses;

const TRIGGER_FUNCTION: &str =
    "CREATE FUNCTION trigger_fn() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

#[test]
fn generation_expressions() {
    for (sql, sqlstate, message) in [
        (
            "CREATE TABLE MyT (MyG int DEFAULT 1 GENERATED ALWAYS AS (1));",
            "42601",
            "both default and generation expression specified for column \"myg\" of table \"myt\"",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE s.\"T\" (\"G\" int GENERATED ALWAYS AS (1) STORED DEFAULT 1);",
            "42601",
            "both default and generation expression specified for column \"G\" of table \"T\"",
        ),
        (
            "CREATE TABLE \"Tt\" (a int); ALTER TABLE \"Tt\" ADD COLUMN \"Gg\" int DEFAULT 1 GENERATED ALWAYS AS (1);",
            "42601",
            "both default and generation expression specified for column \"Gg\" of table \"Tt\"",
        ),
        (
            "CREATE TABLE t (x int, \"G\" int GENERATED ALWAYS AS (x), h int GENERATED ALWAYS AS (\"G\"));",
            "42P17",
            "cannot use generated column \"G\" in column generation expression",
        ),
        (
            "CREATE TABLE t (x int, g text GENERATED ALWAYS AS (CTID::text));",
            "42P10",
            "cannot use system column \"ctid\" in column generation expression",
        ),
        (
            "CREATE TABLE t (x int, g float8 GENERATED ALWAYS AS (random()) STORED);",
            "42P17",
            "generation expression is not immutable",
        ),
        (
            "CREATE TYPE mood AS ENUM ('ok'); CREATE TABLE t (x mood, g bool GENERATED ALWAYS AS (x IS NULL));",
            "0A000",
            "generation expression uses user-defined type",
        ),
        (
            "CREATE FUNCTION helper(int) RETURNS int LANGUAGE sql IMMUTABLE RETURN $1; CREATE TABLE t (x int, g int GENERATED ALWAYS AS (helper(x)));",
            "0A000",
            "generation expression uses user-defined function",
        ),
    ] {
        refuses(sql, sqlstate, message);
    }
}

#[test]
fn virtual_columns_in_indexes_and_keys() {
    for (sql, message) in [
        (
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x)); CREATE INDEX ON t (g);",
            "indexes on virtual generated columns are not supported",
        ),
        (
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x)); ALTER TABLE t ADD UNIQUE (g);",
            "unique constraints on virtual generated columns are not supported",
        ),
        (
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x), EXCLUDE (g WITH =));",
            "unique constraints on virtual generated columns are not supported",
        ),
        (
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x) PRIMARY KEY);",
            "primary keys on virtual generated columns are not supported",
        ),
        (
            "CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x) REFERENCES p);",
            "foreign key constraints on virtual generated columns are not supported",
        ),
        (
            "CREATE DOMAIN d AS int; CREATE TABLE t (x int, \"G\" d GENERATED ALWAYS AS (x));",
            "virtual generated column \"G\" cannot have a domain type",
        ),
        (
            "CREATE TYPE mood AS ENUM ('ok'); CREATE TABLE t (x int, MyG mood GENERATED ALWAYS AS ('ok'::mood) VIRTUAL);",
            "virtual generated column \"myg\" cannot have a user-defined type",
        ),
    ] {
        refuses(sql, "0A000", message);
    }
}

#[test]
fn altering_generated_columns() {
    for (sql, sqlstate, message) in [
        (
            "CREATE TABLE \"Tt\" (x int, \"Gg\" int GENERATED ALWAYS AS (x) STORED); ALTER TABLE \"Tt\" ALTER COLUMN \"Gg\" SET DEFAULT 1;",
            "42601",
            "column \"Gg\" of relation \"Tt\" is a generated column",
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE s.Tt (x int, Gg int GENERATED ALWAYS AS (x) STORED); ALTER TABLE s.Tt ALTER COLUMN Gg SET DEFAULT 1;",
            "42601",
            "column \"gg\" of relation \"tt\" is a generated column",
        ),
        (
            "CREATE TABLE t (x int, g int GENERATED ALWAYS AS (x) STORED); ALTER TABLE t ALTER COLUMN g TYPE bigint USING g::bigint;",
            "42611",
            "cannot specify USING when altering type of generated column",
        ),
    ] {
        refuses(sql, sqlstate, message);
    }
}

#[test]
fn inherited_generation_kinds() {
    for (sql, sqlstate, message) in [
        (
            "CREATE TABLE parent (a int, v int GENERATED ALWAYS AS (a + 1) STORED); CREATE TABLE child (v int GENERATED ALWAYS AS (a + 2) VIRTUAL) INHERITS (parent);",
            "42611",
            "column \"v\" inherits from generated column of different kind",
        ),
        (
            "CREATE TABLE parent (a int, \"V\" int GENERATED ALWAYS AS (a + 1) STORED) PARTITION BY RANGE (a); CREATE TABLE child PARTITION OF parent (\"V\" GENERATED ALWAYS AS (a + 2) VIRTUAL) FOR VALUES FROM (0) TO (10);",
            "42611",
            "column \"V\" inherits from generated column of different kind",
        ),
        (
            "CREATE TABLE parent (a int, \"V\" int GENERATED ALWAYS AS (a + 1) STORED); CREATE TABLE \"Child\" (a int, \"V\" int GENERATED ALWAYS AS (a + 2)); ALTER TABLE \"Child\" INHERIT parent;",
            "42804",
            "column \"V\" inherits from generated column of different kind",
        ),
        (
            "CREATE TABLE parent (a int, v int GENERATED ALWAYS AS (a + 1)) PARTITION BY RANGE (a); CREATE TABLE child (a int, v int GENERATED ALWAYS AS (a + 2) STORED); ALTER TABLE parent ATTACH PARTITION child FOR VALUES FROM (0) TO (10);",
            "42804",
            "column \"v\" inherits from generated column of different kind",
        ),
    ] {
        refuses(sql, sqlstate, message);
    }
}

#[test]
fn before_trigger_conditions() {
    for (table, condition, sqlstate, message) in [
        (
            "CREATE TABLE parent (a int, v int GENERATED ALWAYS AS (a + 1));",
            "NEW.v > 0",
            "42P17",
            "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
        ),
        (
            "CREATE TABLE parent (a int, v int GENERATED ALWAYS AS (a + 1));",
            "NEW IS DISTINCT FROM OLD",
            "42P17",
            "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
        ),
        (
            "CREATE TABLE parent (a int);",
            "NEW.tableoid > 0",
            "0A000",
            "BEFORE trigger's WHEN condition cannot reference NEW system columns",
        ),
    ] {
        refuses(
            &format!(
                "{TRIGGER_FUNCTION} {table} CREATE TRIGGER g BEFORE UPDATE ON parent FOR EACH ROW WHEN ({condition}) EXECUTE FUNCTION trigger_fn();"
            ),
            sqlstate,
            message,
        );
    }
}
