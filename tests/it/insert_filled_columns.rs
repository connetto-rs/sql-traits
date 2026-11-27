//! Which columns every `INSERT` takes from the database whatever it gives
//! them, generated ones and those the `BEFORE INSERT` row triggers overwrite
//! before anything reads what the statement gave.
#![expect(clippy::expect_used, reason = "a schema that does not build fails the test")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

/// The names of the columns of `relation` in `schema` that every `INSERT`
/// takes from the database, after checking that each column answers alike.
fn filled_in(schema: &str, relation: &str) -> Vec<String> {
    let database = ParserDB::parse::<PostgreSqlDialect>(schema).expect("schema builds");
    let table = table(&database, None, relation);
    let filled: Vec<String> = table
        .filled_on_insert_columns(&database)
        .expect("the table is in the database")
        .map(|column| column.column_name().to_owned())
        .collect();
    for column in table.columns(&database).expect("the table is in the database") {
        assert_eq!(
            column.is_filled_on_insert(&database).expect("the table is in the database"),
            filled.iter().any(|name| name == column.column_name()),
            "{} answers as its table does",
            column.column_name()
        );
    }
    filled
}

/// The filled columns of `t (id INT, x INT, y INT)` with the trigger `tg`,
/// declared as `declaration` (`BEFORE INSERT … FOR EACH ROW`, say), whose
/// function runs `statements` and then returns `NEW`.
fn filled_by(declaration: &str, statements: &str) -> Vec<String> {
    filled_in(
        &format!(
            "CREATE TABLE t (id INT, x INT, y INT);
             CREATE TABLE audit (id INT, x INT, y INT);
             CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN {statements} RETURN NEW; END $$;
             CREATE TRIGGER tg {declaration} EXECUTE FUNCTION fill();"
        ),
        "t",
    )
}

/// The filled columns of `t` with a `BEFORE INSERT` row trigger running
/// `statements` and then returning `NEW`.
fn filled(statements: &str) -> Vec<String> {
    filled_by("BEFORE INSERT ON t FOR EACH ROW", statements)
}

#[test]
fn copied_parent_columns_are_filled() {
    let schema = "
        CREATE TABLE resources (
          id UUID PRIMARY KEY,
          parent_id UUID,
          parent_kind TEXT,
          archived_at TIMESTAMPTZ,
          parent_archived BOOLEAN NOT NULL DEFAULT FALSE,
          inherits_archive BOOLEAN NOT NULL DEFAULT FALSE,
          archived BOOLEAN GENERATED ALWAYS AS (archived_at IS NOT NULL OR inherits_archive) STORED NOT NULL
        );
        CREATE FUNCTION copy_resource_archive() RETURNS TRIGGER LANGUAGE plpgsql
        SECURITY DEFINER SET search_path = public AS $$
        BEGIN
          SELECT parent.parent_kind, parent.archived INTO NEW.parent_kind, NEW.parent_archived
            FROM resources parent WHERE parent.id = NEW.parent_id;
          NEW.parent_archived := coalesce(NEW.parent_archived, FALSE);
          NEW.inherits_archive := NEW.parent_archived;
          RETURN NEW;
        END
        $$;
        CREATE TRIGGER resources_copy_archive BEFORE INSERT ON resources
          FOR EACH ROW EXECUTE FUNCTION copy_resource_archive();
        CREATE FUNCTION stamp_archived_at() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN
          IF NEW.archived_at IS NOT NULL THEN
            NEW.archived_at := now();
          END IF;
          RETURN NEW;
        END
        $$;
        CREATE TRIGGER resources_stamp_archived_at BEFORE INSERT ON resources
          FOR EACH ROW EXECUTE FUNCTION stamp_archived_at();";
    assert_eq!(
        filled_in(schema, "resources"),
        ["parent_kind", "parent_archived", "inherits_archive", "archived"]
    );
}

#[test]
fn an_assignment_reading_the_given_value_keeps_it() {
    assert_eq!(filled("NEW.x := NEW.x + 1;"), [] as [&str; 0]);
    assert_eq!(filled("NEW.x := 1;"), ["x"]);
    assert_eq!(filled("NEW.x := 1; NEW.x := NEW.x + 1;"), ["x"]);
}

#[test]
fn only_enabled_before_insert_row_triggers_without_when_fill() {
    let now = "NEW.x := 1;";
    assert_eq!(filled_by("BEFORE INSERT OR UPDATE ON t FOR EACH ROW", now), ["x"]);
    assert_eq!(filled_by("BEFORE UPDATE ON t FOR EACH ROW", now), [] as [&str; 0]);
    assert_eq!(
        filled_by("BEFORE INSERT ON t FOR EACH ROW WHEN (NEW.id > 0)", now),
        [] as [&str; 0]
    );
    assert_eq!(filled_by("AFTER INSERT ON t FOR EACH ROW", now), [] as [&str; 0]);
    assert_eq!(filled_by("BEFORE INSERT ON t FOR EACH STATEMENT", now), [] as [&str; 0]);
    for (enablement, filled) in [
        ("DISABLE TRIGGER tg", false),
        ("ENABLE REPLICA TRIGGER tg", false),
        ("ENABLE ALWAYS TRIGGER tg", true),
    ] {
        let schema = format!(
            "CREATE TABLE t (id INT, x INT);
             CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN NEW.x := 1; RETURN NEW; END $$;
             CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();
             ALTER TABLE t {enablement};"
        );
        assert_eq!(filled_in(&schema, "t") == ["x"], filled, "{enablement}");
    }
}

#[test]
fn every_path_returning_new_assigns() {
    assert_eq!(filled("IF NEW.x IS NULL THEN NEW.x := 1; END IF;"), [] as [&str; 0]);
    assert_eq!(filled("IF NEW.id > 0 THEN NEW.x := 1; ELSE NEW.x := 2; END IF;"), ["x"]);
    assert_eq!(filled("IF NEW.id < 0 THEN RAISE EXCEPTION 'negative'; END IF; NEW.x := 1;"), ["x"]);
    assert_eq!(filled("IF NEW.id < 0 THEN RETURN NULL; END IF; NEW.x := 1;"), ["x"]);
    assert_eq!(filled("IF NEW.id < 0 THEN RETURN NEW; END IF; NEW.x := 1;"), [] as [&str; 0]);
    assert_eq!(
        filled("BEGIN NEW.x := 1 / NEW.id; EXCEPTION WHEN division_by_zero THEN NULL; END;"),
        [] as [&str; 0]
    );
}

#[test]
fn select_into_assigns_every_target() {
    assert_eq!(
        filled("SELECT a.x, a.y INTO NEW.x, NEW.y FROM audit a WHERE a.id = NEW.id;"),
        ["x", "y"]
    );
    assert_eq!(filled("SELECT a.x INTO STRICT NEW.x FROM audit a WHERE a.id = NEW.id;"), ["x"]);
    assert_eq!(filled("SELECT a.x INTO NEW.x FROM audit a WHERE a.x = NEW.x;"), [] as [&str; 0]);
}

#[test]
fn any_read_of_the_given_value_keeps_it() {
    assert_eq!(filled("NEW.y := NEW.x; NEW.x := 1;"), ["y"]);
    assert_eq!(filled("NEW.x := 1; NEW.y := NEW.x;"), ["x", "y"]);
    assert_eq!(
        filled("IF NEW.x IS NOT NULL THEN RAISE EXCEPTION 'given'; END IF; NEW.x := 1;"),
        [] as [&str; 0]
    );
    assert_eq!(filled("INSERT INTO audit SELECT NEW.*; NEW.x := 1;"), [] as [&str; 0]);
    assert_eq!(
        filled("PERFORM pg_notify('t', row_to_json(NEW)::text); NEW.x := 1;"),
        [] as [&str; 0]
    );
    assert_eq!(filled("NEW.x := 1; PERFORM pg_notify('t', row_to_json(NEW)::text);"), ["x"]);
}

#[test]
fn a_read_through_an_alias_keeps_the_given_value() {
    let schema = "
        CREATE TABLE t (id INT, x INT);
        CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        DECLARE row_alias ALIAS FOR NEW;
        BEGIN RAISE NOTICE '%', row_alias.x; NEW.x := 1; RETURN NEW; END $$;
        CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();";
    assert_eq!(filled_in(schema, "t"), [] as [&str; 0]);
}

#[test]
fn an_earlier_trigger_reading_the_column_keeps_it() {
    let schema = |reader: &str| {
        format!(
            "CREATE TABLE t (id INT, x INT, y INT);
             CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN NEW.x := 1; RETURN NEW; END $$;
             CREATE FUNCTION copy() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN NEW.y := NEW.x; RETURN NEW; END $$;
             CREATE TRIGGER b BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();
             CREATE TRIGGER {reader} BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION copy();"
        )
    };
    assert_eq!(filled_in(&schema("a"), "t"), ["y"]);
    assert_eq!(filled_in(&schema("c"), "t"), ["x", "y"]);
}

#[test]
fn an_earlier_body_in_another_language_may_read_every_column() {
    let schema = "
        CREATE TABLE t (id INT, x INT);
        CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN NEW.x := 1; RETURN NEW; END $$;
        CREATE FUNCTION inspect() RETURNS TRIGGER LANGUAGE plpython3u AS $$
        return 'OK'
        $$;
        CREATE TRIGGER a BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION inspect();
        CREATE TRIGGER b BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();";
    assert_eq!(filled_in(schema, "t"), [] as [&str; 0]);
}

#[test]
fn writing_part_of_a_column_keeps_the_rest_given() {
    let schema = |statements: &str| {
        format!(
            "CREATE TABLE t (id INT, tags TEXT[]);
             CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN {statements} RETURN NEW; END $$;
             CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();"
        )
    };
    assert_eq!(filled_in(&schema("NEW.tags[1] := 'a';"), "t"), [] as [&str; 0]);
    assert_eq!(filled_in(&schema("NEW.tags[1] := 'a'; NEW.tags := '{}';"), "t"), [] as [&str; 0]);
    assert_eq!(filled_in(&schema("NEW.tags := '{}'; NEW.tags[1] := 'a';"), "t"), ["tags"]);
}

#[test]
fn a_when_condition_reading_the_column_keeps_it() {
    let schema = "
        CREATE TABLE t (id INT, x INT);
        CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN NEW.x := 1; RETURN NEW; END $$;
        CREATE FUNCTION noop() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN RETURN NEW; END $$;
        CREATE TRIGGER a BEFORE INSERT ON t FOR EACH ROW WHEN (NEW.x > 0) EXECUTE FUNCTION noop();
        CREATE TRIGGER b BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();";
    assert_eq!(filled_in(schema, "t"), [] as [&str; 0]);
}

#[test]
fn generated_columns_are_filled_and_defaults_are_not() {
    let schema = "
        CREATE TABLE t (
          always INT GENERATED ALWAYS AS IDENTITY,
          by_default INT GENERATED BY DEFAULT AS IDENTITY,
          sequenced SERIAL,
          defaulted INT DEFAULT 1,
          stored INT GENERATED ALWAYS AS (by_default * 2) STORED,
          computed INT GENERATED ALWAYS AS (by_default * 3) VIRTUAL
        );";
    assert_eq!(filled_in(schema, "t"), ["always", "stored", "computed"]);
}

#[test]
fn an_insert_rule_keeps_every_given_value() {
    let schema = "
        CREATE TABLE t (id INT, x INT);
        CREATE TABLE audit (id INT, x INT);
        CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN NEW.x := 1; RETURN NEW; END $$;
        CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();
        CREATE RULE log AS ON INSERT TO t DO ALSO INSERT INTO audit VALUES (new.id, new.x);";
    assert_eq!(filled_in(schema, "t"), [] as [&str; 0]);
}

#[test]
fn a_partitioned_table_needs_every_partition_filled() {
    let schema = "
        CREATE TABLE t (id INT, x INT) PARTITION BY LIST (id);
        CREATE TABLE t_one PARTITION OF t FOR VALUES IN (1);
        CREATE TABLE t_two PARTITION OF t FOR VALUES IN (2);
        CREATE FUNCTION fill() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN NEW.x := 1; RETURN NEW; END $$;
        CREATE FUNCTION show() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN RAISE NOTICE '%', NEW.x; RETURN NEW; END $$;
        CREATE TRIGGER b BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fill();
        CREATE TRIGGER a BEFORE INSERT ON t_two FOR EACH ROW EXECUTE FUNCTION show();";
    assert_eq!(filled_in(schema, "t_one"), ["x"]);
    assert_eq!(filled_in(schema, "t_two"), [] as [&str; 0]);
    assert_eq!(filled_in(schema, "t"), [] as [&str; 0]);
}
