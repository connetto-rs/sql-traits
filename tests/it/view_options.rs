//! Tests that a view carries the options PostgreSQL lets it run under, and the
//! role it is handed to.
//!
//! Every rule asserted here was measured against PostgreSQL 18.6 in Docker. A
//! plain view takes `check_option`, `security_barrier` and `security_invoker`
//! and refuses any other name. A materialized view takes storage parameters
//! and refuses those three. An option named alone is set to `true`. `CREATE OR
//! REPLACE VIEW` replaces the whole option set, while `ALTER TABLE ... SET`
//! changes only the options it names and `ALTER TABLE ... RESET` turns them
//! off, refusing no name at all.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The `(security_invoker, security_barrier)` pair of plain view `v`.
fn flags(db: &ParserDB) -> (bool, bool) {
    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is recorded");
    (view.is_security_invoker(), view.is_security_barrier())
}

/// The flags of a view declared with `options` over a table `t`.
fn declared(options: &str) -> Result<(bool, bool), Error> {
    parse(&format!("CREATE TABLE t (id INT); CREATE VIEW v WITH ({options}) AS SELECT id FROM t;"))
        .map(|db| flags(&db))
}

#[test]
fn a_view_runs_with_its_owners_rights_unless_it_says_otherwise() {
    let db = parse("CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;").expect("schema");
    assert_eq!(flags(&db), (false, false));
}

#[test]
fn a_boolean_option_takes_every_spelling_postgres_parse_bool_takes() {
    // Measured one by one: each prefix of true, false, yes and no, two or
    // three letters of on and off, a lone 1 or 0 in any integer spelling, and
    // the same words quoted in any of the string forms.
    let truthy = [
        "true",
        "TRUE",
        "t",
        "tr",
        "tru",
        "'TrUe'",
        "\"TRUE\"",
        "y",
        "ye",
        "yes",
        "on",
        "On",
        "1",
        "'1'",
        "+1",
        "01",
        "00000000000000000001",
        "0_1",
        "0x1",
        "0X1",
        "0x_01",
        "0o1",
        "0b1",
        "+0b1",
        "$$yes$$",
        "E'yes'",
        "U&'yes'",
    ];
    for spelling in truthy {
        assert_eq!(
            declared(&format!("security_invoker = {spelling}")).expect(spelling),
            (true, false),
            "{spelling}"
        );
    }
    let falsy = [
        "false", "f", "fals", "n", "no", "nO", "of", "off", "OFF", "0", "'0'", "-0", "00", "0x0",
        "0B0", "0o_0", "-0o0",
    ];
    for spelling in falsy {
        assert_eq!(
            declared(&format!("security_invoker = {spelling}")).expect(spelling),
            (false, false),
            "{spelling}"
        );
    }
}

#[test]
fn a_boolean_option_refuses_what_postgres_parse_bool_refuses() {
    // `o` could begin either on or off, a sign only goes before an integer,
    // and every other spelling here is no prefix of a boolean word, or an
    // integer other than 1 and 0.
    let refused = [
        "o",
        "truex",
        "yess",
        "maybe",
        "on_x",
        "2",
        "-1",
        "1_0",
        "0b10",
        "0x2",
        "1.0",
        "1e0",
        "' true'",
        "'yes '",
        "''",
        "'01'",
        "NULL",
        "none",
        "default",
        "-yes",
        "+on",
        "a.b",
        "pg_catalog.on",
    ];
    for spelling in refused {
        let error = declared(&format!("security_barrier = {spelling}")).expect_err(spelling);
        assert!(
            matches!(&error, Error::InvalidViewOptionValue { option_name, .. }
                if option_name == "security_barrier"),
            "{spelling} got {error:?}"
        );
    }
}

#[test]
fn each_boolean_option_sets_its_own_flag() {
    assert_eq!(declared("security_barrier = true").expect("schema"), (false, true));
    assert_eq!(
        declared("security_invoker = on, security_barrier = yes").expect("schema"),
        (true, true)
    );
}

#[test]
fn an_option_name_folds_like_an_identifier() {
    assert_eq!(declared("Security_Invoker = true").expect("unquoted folds"), (true, false));
    assert_eq!(declared("\"security_invoker\" = true").expect("quoted lower"), (true, false));
    let error = declared("\"Security_Invoker\" = true").expect_err("quoted keeps its case");
    assert!(
        matches!(&error, Error::UnrecognizedViewOption { object_kind: ObjectKind::View, .. }),
        "got {error:?}"
    );
}

#[test]
fn a_plain_view_takes_only_the_three_view_options() {
    assert_eq!(declared("check_option = LOCAL").expect("local"), (false, false));
    assert_eq!(declared("check_option = 'Cascaded'").expect("cascaded"), (false, false));
    let error = declared("check_option = loc").expect_err("no prefix for an enum");
    assert!(
        matches!(&error, Error::InvalidViewOptionValue { option_name, .. }
            if option_name == "check_option"),
        "got {error:?}"
    );
    for option in ["fillfactor = 50", "foo = 1"] {
        let error = declared(option).expect_err(option);
        assert!(
            matches!(&error, Error::UnrecognizedViewOption { object_kind: ObjectKind::View, .. }),
            "{option} got {error:?}"
        );
    }
}

#[test]
fn the_generic_dialect_reads_the_same_view_options() {
    let generic = |options: &str| {
        ParserDB::parse::<GenericDialect>(&format!(
            "CREATE TABLE t (id INT); CREATE VIEW v WITH ({options}) AS SELECT id FROM t;"
        ))
        .map(|db| flags(&db))
    };
    // It reads `0x` as a hexadecimal literal, still one integer to PostgreSQL.
    assert_eq!(generic("security_invoker = 0x1").expect("0x1"), (true, false));
    assert_eq!(generic("security_invoker = 0x0").expect("0x0"), (false, false));
    let error = generic("security_invoker = 0x2").expect_err("2 is no boolean");
    assert!(matches!(&error, Error::InvalidViewOptionValue { .. }), "got {error:?}");
    // MSSQL's table options parse here too, and no view takes one.
    let error = generic("CLUSTERED COLUMNSTORE INDEX").expect_err("an MSSQL table option");
    assert!(
        matches!(&error, Error::UnrecognizedViewOption { object_kind: ObjectKind::View, option_name, .. }
            if option_name == "CLUSTERED COLUMNSTORE INDEX"),
        "got {error:?}"
    );
}

#[test]
fn an_option_written_twice_is_refused() {
    for options in [
        "security_invoker = true, security_invoker = false",
        "security_invoker = true, Security_Invoker = true",
    ] {
        let error = declared(options).expect_err(options);
        assert!(
            matches!(&error, Error::RepeatedViewOption { option_name, .. }
                if option_name == "security_invoker"),
            "{options} got {error:?}"
        );
    }
}

#[test]
fn a_replacement_replaces_the_whole_option_set() {
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v WITH (security_invoker = true) AS SELECT id FROM t;
         CREATE OR REPLACE VIEW v WITH (security_barrier = true) AS SELECT id FROM t;",
    )
    .expect("schema");
    assert_eq!(flags(&db), (false, true));

    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v WITH (security_invoker = true) AS SELECT id FROM t;
         CREATE OR REPLACE VIEW v AS SELECT id FROM t;",
    )
    .expect("schema");
    assert_eq!(flags(&db), (false, false));
}

#[test]
fn alter_table_set_changes_only_the_options_it_names() {
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v WITH (security_invoker = true) AS SELECT id FROM t;
         ALTER TABLE v SET (security_barrier = true);",
    )
    .expect("schema");
    assert_eq!(flags(&db), (true, true));

    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v WITH (security_invoker = true, security_barrier = true) AS SELECT id FROM t;
         ALTER TABLE v SET (security_invoker = off);",
    )
    .expect("schema");
    assert_eq!(flags(&db), (false, true));
}

#[test]
fn alter_table_set_refuses_what_create_view_refuses() {
    let schema = "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;";
    let error = parse(&format!("{schema} ALTER TABLE v SET (security_invoker = maybe);"))
        .expect_err("not a boolean");
    assert!(matches!(&error, Error::InvalidViewOptionValue { .. }), "got {error:?}");
    let error = parse(&format!("{schema} ALTER TABLE v SET (fillfactor = 50);"))
        .expect_err("not a view option");
    assert!(matches!(&error, Error::UnrecognizedViewOption { .. }), "got {error:?}");
    let error = parse(&format!(
        "{schema} ALTER TABLE v SET (security_invoker = true, security_invoker = false);"
    ))
    .expect_err("written twice");
    assert!(matches!(&error, Error::RepeatedViewOption { .. }), "got {error:?}");
}

#[test]
fn an_option_named_alone_is_on() {
    assert_eq!(declared("security_invoker").expect("bare"), (true, false));
    assert_eq!(declared("security_barrier, security_invoker = off").expect("mixed"), (false, true));
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER TABLE v SET (security_barrier);",
    )
    .expect("schema");
    assert_eq!(flags(&db), (false, true));

    // Measured: a bare `check_option` reads `true`, which is no setting of it.
    let error = declared("check_option").expect_err("check_option takes no boolean");
    assert!(
        matches!(&error, Error::InvalidViewOptionValue { option_name, option_value, .. }
            if option_name == "check_option" && option_value == "true"),
        "got {error:?}"
    );
    let error = declared("foo").expect_err("not a view option");
    assert!(matches!(&error, Error::UnrecognizedViewOption { .. }), "got {error:?}");
    let error = declared("security_invoker, security_invoker = false").expect_err("twice");
    assert!(matches!(&error, Error::RepeatedViewOption { .. }), "got {error:?}");
}

#[test]
fn alter_table_reset_turns_off_only_the_options_it_names() {
    let schema = "CREATE TABLE t (id INT);
                  CREATE VIEW v WITH (security_invoker, security_barrier) AS SELECT id FROM t;";
    let after = |statement: &str| flags(&parse(&format!("{schema} {statement}")).expect(statement));
    assert_eq!(after("ALTER TABLE v RESET (security_invoker);"), (false, true));
    assert_eq!(after("ALTER TABLE v RESET (Security_Invoker);"), (false, true));
    assert_eq!(after("ALTER TABLE v RESET (security_barrier, security_invoker);"), (false, false));
    // Measured: PostgreSQL refuses no name in `RESET`, and none of these
    // resets either flag.
    for untouched in [
        "ALTER TABLE v RESET (\"Security_Invoker\");",
        "ALTER TABLE v RESET (toast.security_invoker);",
        "ALTER TABLE v RESET (check_option);",
        "ALTER TABLE v RESET (fillfactor, no_such_option);",
    ] {
        assert_eq!(after(untouched), (true, true), "{untouched}");
    }
    assert_eq!(after("ALTER TABLE v RESET (security_invoker, security_invoker);"), (false, true));

    // One statement applies its actions in order.
    assert_eq!(
        after("ALTER TABLE v RESET (security_invoker), SET (security_invoker);"),
        (true, true)
    );
    assert_eq!(
        after("ALTER TABLE v SET (security_invoker = false), RESET (security_barrier);"),
        (false, false)
    );
}

#[test]
fn a_materialized_view_takes_storage_parameters_and_no_view_option() {
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE MATERIALIZED VIEW m WITH (fillfactor = 50) AS SELECT id FROM t;
         ALTER TABLE m SET (fillfactor = 70);",
    )
    .expect("storage parameters are accepted");
    let view = db
        .materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is recorded");
    assert!(!view.is_security_invoker());
    assert!(!view.is_security_barrier());

    for statement in [
        "CREATE MATERIALIZED VIEW m WITH (security_invoker = true) AS SELECT id FROM t;",
        "CREATE MATERIALIZED VIEW m WITH (security_invoker) AS SELECT id FROM t;",
        "CREATE MATERIALIZED VIEW m WITH (check_option = local) AS SELECT id FROM t;",
        "CREATE MATERIALIZED VIEW m AS SELECT id FROM t;
         ALTER TABLE m SET (security_barrier = true);",
        "CREATE MATERIALIZED VIEW m AS SELECT id FROM t;
         ALTER TABLE m SET (security_barrier);",
    ] {
        let error = parse(&format!("CREATE TABLE t (id INT); {statement}")).expect_err(statement);
        assert!(
            matches!(
                &error,
                Error::UnrecognizedViewOption { object_kind: ObjectKind::MaterializedView, .. }
            ),
            "{statement} got {error:?}"
        );
    }
    // Measured: `RESET` refuses nothing here either, and a bare storage
    // parameter is accepted.
    parse(
        "CREATE TABLE t (id INT);
         CREATE MATERIALIZED VIEW m WITH (autovacuum_enabled) AS SELECT id FROM t;
         ALTER TABLE m RESET (security_invoker, fillfactor, toast.autovacuum_enabled);",
    )
    .expect("RESET on a materialized view refuses no name");
}

#[test]
fn a_view_reports_the_role_it_is_handed_to() {
    let db = parse(
        "CREATE ROLE app_owner;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         CREATE VIEW unowned AS SELECT id FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT id FROM t;
         ALTER TABLE v OWNER TO app_owner;
         ALTER TABLE m OWNER TO app_owner;",
    )
    .expect("schema");
    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is recorded");
    assert_eq!(view.owner(&db).expect("held"), Some("app_owner"));
    let unowned = db
        .view_by_target(TargetName::new("unowned", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is recorded");
    assert_eq!(unowned.owner(&db).expect("held"), None);
    let materialized = db
        .materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is recorded");
    assert_eq!(materialized.owner(&db).expect("held"), Some("app_owner"));

    // A view read off one schema is not held by another.
    let other = parse("CREATE TABLE t (id INT);").expect("schema");
    assert!(matches!(
        view.owner(&other),
        Err(LookupError::ObjectNotInDatabase { object_kind: ObjectKind::View, .. })
    ));
    assert!(matches!(
        materialized.owner(&other),
        Err(LookupError::ObjectNotInDatabase { object_kind: ObjectKind::MaterializedView, .. })
    ));
}

#[test]
fn a_toast_option_on_a_plain_view_is_dropped_unread() {
    // Measured: a view has no TOAST table, and PostgreSQL stores nothing for a
    // `toast.` option on one, whatever its name or value.
    for options in [
        "toast.foo = 1",
        "toast.security_invoker = true",
        "toast.autovacuum_enabled",
        "toast.autovacuum_enabled = maybe",
        "TOAST.security_barrier = on, security_barrier = off",
    ] {
        assert_eq!(declared(options).expect(options), (false, false), "{options}");
    }
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER TABLE v SET (toast.security_invoker = true);",
    )
    .expect("ALTER TABLE ... SET takes a toast option too");
    assert_eq!(flags(&db), (false, false));

    // Measured: any other namespace is refused.
    let error = declared("other.x = 1").expect_err("unknown namespace");
    assert!(
        matches!(&error, Error::UnrecognizedViewOption { object_kind: ObjectKind::View, option_name, .. }
            if option_name == "other.x"),
        "got {error:?}"
    );
}

#[test]
fn a_materialized_view_refuses_a_view_option_on_its_toast_table() {
    for options in ["toast.security_invoker = true", "toast.check_option = local"] {
        let error = parse(&format!(
            "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m WITH ({options}) AS SELECT id FROM t;"
        ))
        .expect_err(options);
        assert!(
            matches!(
                &error,
                Error::UnrecognizedViewOption { object_kind: ObjectKind::MaterializedView, .. }
            ),
            "{options} got {error:?}"
        );
    }
}

#[test]
fn alter_view_sets_and_resets_options() {
    let schema = "CREATE TABLE t (id INT);
                  CREATE VIEW v WITH (security_barrier) AS SELECT id FROM t;";
    let after = |statement: &str| flags(&parse(&format!("{schema} {statement}")).expect(statement));
    assert_eq!(after("ALTER VIEW v SET (security_invoker);"), (true, true));
    assert_eq!(
        after("ALTER VIEW v SET (security_invoker = yes, security_barrier = off);"),
        (true, false)
    );
    assert_eq!(after("ALTER VIEW v RESET (security_barrier);"), (false, false));
    assert_eq!(after("ALTER VIEW v RESET (Security_Barrier, no_such_option);"), (false, false));
    assert_eq!(after("ALTER VIEW v RESET (\"Security_Barrier\");"), (false, true));
    assert_eq!(after("ALTER VIEW IF EXISTS v SET (security_invoker);"), (true, true));

    for (statement, expected) in [
        ("ALTER VIEW v SET (security_invoker = maybe);", "invalid value"),
        ("ALTER VIEW v SET (fillfactor = 50);", "unrecognized"),
        ("ALTER VIEW v SET (security_invoker, security_invoker = false);", "repeated"),
    ] {
        let error = parse(&format!("{schema} {statement}")).expect_err(statement);
        let matched = match expected {
            "invalid value" => matches!(error, Error::InvalidViewOptionValue { .. }),
            "unrecognized" => matches!(error, Error::UnrecognizedViewOption { .. }),
            _ => matches!(error, Error::RepeatedViewOption { .. }),
        };
        assert!(matched, "{statement} got {error:?}");
    }
}

#[test]
fn alter_view_hands_the_view_to_a_role() {
    let db = parse(
        "CREATE ROLE r1;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         CREATE VIEW w AS SELECT id FROM t;
         ALTER VIEW v OWNER TO r1;
         ALTER VIEW w OWNER TO r1;
         ALTER VIEW w OWNER TO CURRENT_USER;",
    )
    .expect("schema");
    let owner = |name: &str| {
        db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the view is recorded")
            .owner(&db)
            .expect("held")
            .map(str::to_string)
    };
    assert_eq!(owner("v").as_deref(), Some("r1"));
    // `CURRENT_USER` names whoever runs the statement, which no role spells.
    assert_eq!(owner("w"), None);
}

#[test]
fn alter_view_names_a_plain_view() {
    // Measured: PostgreSQL refuses `ALTER VIEW` against any other relation as
    // "is not a view", and an absent one unless `IF EXISTS` excuses it.
    let schema = "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;";
    for (statement, actual) in [
        ("ALTER VIEW m OWNER TO CURRENT_USER;", ObjectKind::MaterializedView),
        ("ALTER VIEW m SET (fillfactor = 50);", ObjectKind::MaterializedView),
        ("ALTER VIEW t RENAME TO w;", ObjectKind::Table),
        ("ALTER VIEW t RESET (fillfactor);", ObjectKind::Table),
    ] {
        let error = parse(&format!("{schema} {statement}")).expect_err(statement);
        assert!(
            matches!(&error, Error::RelationKindMismatch { expected_kind: ObjectKind::View, actual_kind, .. }
                if *actual_kind == actual),
            "{statement} got {error:?}"
        );
    }
    let error = parse("ALTER VIEW nope OWNER TO CURRENT_USER;").expect_err("absent");
    assert!(
        matches!(&error, Error::RelationNotFound { object_kind: ObjectKind::View, .. }),
        "got {error:?}"
    );
    parse("ALTER VIEW IF EXISTS nope OWNER TO CURRENT_USER;").expect("IF EXISTS skips");
}

#[test]
fn alter_view_renames_the_view_and_its_columns() {
    let error = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER VIEW v RENAME COLUMN id TO x;
         ALTER VIEW v RENAME id TO y;",
    )
    .expect_err("v exposes no id once it is renamed");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound { column_name, .. })
            if column_name == "id"),
        "got {error:?}"
    );

    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER VIEW v RENAME TO w;
         ALTER VIEW w RENAME COLUMN id TO x;",
    )
    .expect("schema");
    assert!(
        db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    let renamed = db
        .view_by_target(TargetName::new("w", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("renamed");
    assert_eq!(renamed.definition().to_string(), "SELECT id AS x FROM t");
}

#[test]
fn a_view_column_default_is_accepted_for_a_column_the_view_exposes() {
    // Measured: PostgreSQL takes `SET DEFAULT` and `DROP DEFAULT` on a view
    // column through either spelling, which the model does not record.
    let schema = "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;";
    for statement in [
        "ALTER VIEW v ALTER COLUMN id SET DEFAULT 1;",
        "ALTER VIEW v ALTER id DROP DEFAULT;",
        "ALTER TABLE v ALTER COLUMN id SET DEFAULT 1;",
        "ALTER TABLE v ALTER COLUMN id DROP DEFAULT;",
    ] {
        parse(&format!("{schema} {statement}")).expect(statement);
    }
    let error = parse(&format!("{schema} ALTER VIEW v ALTER COLUMN nope SET DEFAULT 1;"))
        .expect_err("v exposes no such column");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound { column_name, .. }) if column_name == "nope"),
        "got {error:?}"
    );
    // Measured: anything else on a view column, and a default on a
    // materialized view's column, are refused.
    for (statement, schema) in [
        ("ALTER TABLE v ALTER COLUMN id SET NOT NULL;", schema),
        (
            "ALTER TABLE m ALTER COLUMN id SET DEFAULT 1;",
            "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
        ),
    ] {
        let error = parse(&format!("{schema} {statement}")).expect_err(statement);
        assert!(
            matches!(&error, Error::AlterActionUnsupportedOnRelation { .. }),
            "{statement} got {error:?}"
        );
    }
}

#[test]
fn alter_view_refuses_what_the_model_cannot_apply() {
    let error = parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER VIEW v SET SCHEMA s;",
    )
    .expect_err("the model does not move a view between schemas");
    assert!(
        matches!(&error, Error::UnsupportedAlterTableOperation { operation, .. }
            if operation == "SET SCHEMA s"),
        "got {error:?}"
    );
}
