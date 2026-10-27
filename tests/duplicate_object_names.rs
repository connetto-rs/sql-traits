//! Tests that a name a schema already uses cannot be taken twice.
//!
//! Six kinds of object are covered: a column within a table, an index, a policy
//! on a table, a trigger on a table, a role, and a function signature. Every
//! expectation here was run against a real PostgreSQL 16 first, including the
//! ones that look like duplicates and are not, since a check that refuses valid
//! input is the worse of the two failures.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const TRIGGER_FUNCTION: &str =
    "CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;";

// Columns.

#[test]
fn a_column_declared_twice_is_refused() {
    let refused = parse("CREATE TABLE docs (id INT, id INT);");
    assert!(
        matches!(&refused, Err(Error::ColumnAlreadyExists { table_name, column_name })
            if table_name == "docs" && column_name == "id"),
        "got {refused:?}"
    );
}

/// An unquoted identifier folds, so `id` and `ID` are one column, while a
/// quoted one keeps its case and `"ID"` is another.
#[test]
fn column_folding_decides_what_counts_as_the_same_column() {
    assert!(parse("CREATE TABLE docs (id INT, ID INT);").is_err(), "the unquoted pair folds");
    parse("CREATE TABLE docs (id INT, \"ID\" INT);").expect("the quoted one is a second column");
}

/// The database reports the repeated column rather than the repeated table, so
/// this crate has to check the node before it offers it to the store.
#[test]
fn a_repeated_column_is_reported_before_a_repeated_table() {
    let refused = parse("CREATE TABLE docs (id INT); CREATE TABLE docs (id INT, id INT);");
    assert!(
        matches!(&refused, Err(Error::ColumnAlreadyExists { column_name, .. }) if column_name == "id"),
        "got {refused:?}"
    );
}

#[test]
fn a_column_name_freed_by_a_rename_can_be_taken_again() {
    parse(
        "CREATE TABLE docs (id INT);
         ALTER TABLE docs RENAME COLUMN id TO old_id;
         ALTER TABLE docs ADD COLUMN id INT;",
    )
    .expect("the rename frees the name");
}

// Indexes.

#[test]
fn an_index_name_used_twice_is_refused() {
    let refused = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         CREATE INDEX i ON docs (id);",
    );
    assert!(
        matches!(&refused, Err(Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, object_name })
            if *object_kind == ObjectKind::Index
                && *conflicting_kind == ObjectKind::Index
                && object_name == "i"),
        "got {refused:?}"
    );
}

/// An index name is scoped to the schema of its table, not to that table, so
/// two tables in one schema cannot both carry an index called `i` while two
/// schemas can.
#[test]
fn an_index_name_is_scoped_to_a_schema_and_not_to_a_table() {
    assert!(
        parse(
            "CREATE TABLE a (id INT);
             CREATE TABLE b (id INT);
             CREATE INDEX i ON a (id);
             CREATE INDEX i ON b (id);"
        )
        .is_err(),
        "one schema holds one `i`"
    );

    parse(
        "CREATE SCHEMA one;
         CREATE SCHEMA two;
         CREATE TABLE one.a (id INT);
         CREATE TABLE two.b (id INT);
         CREATE INDEX i ON one.a (id);
         CREATE INDEX i ON two.b (id);",
    )
    .expect("two schemas hold one `i` each");
}

/// The spelling that has cost this work two wrong turns: the index behind a
/// named `UNIQUE` or `PRIMARY KEY` constraint takes that name, and a
/// `CREATE INDEX` cannot then take it, in either order.
#[test]
fn a_constraint_backed_index_holds_its_name_against_a_create_index() {
    let after_constraint = parse(
        "CREATE TABLE docs (id INT CONSTRAINT c UNIQUE);
         CREATE INDEX c ON docs (id);",
    );
    assert!(
        matches!(&after_constraint, Err(Error::RelationNameAlreadyTaken { conflicting_kind, object_name, .. })
            if *conflicting_kind == ObjectKind::UniqueIndex && object_name == "c"),
        "got {after_constraint:?}"
    );

    let before_constraint = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX c ON docs (id);
         CREATE TABLE other (id INT CONSTRAINT c UNIQUE);",
    );
    assert!(
        matches!(&before_constraint, Err(Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. })
            if *object_kind == ObjectKind::UniqueIndex && *conflicting_kind == ObjectKind::Index),
        "got {before_constraint:?}"
    );
}

/// The inline spelling used to drop the name it was declared with, so the two
/// spellings of one constraint disagreed about what the index is called.
#[test]
fn both_spellings_of_a_named_unique_constraint_read_back_the_name() {
    let db = parse(
        "CREATE TABLE inline (id INT CONSTRAINT inline_uq UNIQUE);
         CREATE TABLE declared (id INT, CONSTRAINT declared_uq UNIQUE (id));",
    )
    .expect("both build");

    let name_of = |table: &str| {
        db.table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the table exists")
            .unique_indices(&db)
            .expect("its unique indices")
            .next()
            .and_then(IndexLike::name)
            .map(str::to_string)
    };

    assert_eq!(name_of("inline").as_deref(), Some("inline_uq"));
    assert_eq!(name_of("declared").as_deref(), Some("declared_uq"));
}

/// Two constraints of one table cannot share a name either, since each puts an
/// index of that name into the schema.
#[test]
fn two_named_constraints_on_one_table_cannot_share_a_name() {
    assert!(
        parse("CREATE TABLE docs (a INT CONSTRAINT c UNIQUE, b INT CONSTRAINT c UNIQUE);").is_err()
    );
    assert!(
        parse(
            "CREATE TABLE docs (a INT, b INT, CONSTRAINT c UNIQUE (a), CONSTRAINT c UNIQUE (b));"
        )
        .is_err()
    );
}

/// An index shares its pool of names with the tables of the schema, so neither
/// can take a name the other holds.
#[test]
fn an_index_and_a_table_cannot_share_a_name() {
    let index_onto_table = parse("CREATE TABLE docs (id INT); CREATE INDEX docs ON docs (id);");
    assert!(
        matches!(&index_onto_table, Err(Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. })
            if *object_kind == ObjectKind::Index && *conflicting_kind == ObjectKind::Table),
        "got {index_onto_table:?}"
    );

    let table_onto_index = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         CREATE TABLE i (id INT);",
    );
    assert!(
        matches!(&table_onto_index, Err(Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. })
            if *object_kind == ObjectKind::Table && *conflicting_kind == ObjectKind::Index),
        "got {table_onto_index:?}"
    );
}

/// One table name against another stays with the store, which names both
/// spellings, rather than being reported a second way by the read.
#[test]
fn a_repeated_table_name_is_still_a_lookup_conflict() {
    let refused = parse("CREATE TABLE docs (id INT); CREATE TABLE docs (id INT);");
    assert!(
        matches!(
            &refused,
            Err(Error::IdentifierLookupError(LookupError::TableLookupConflict { .. }))
        ),
        "got {refused:?}"
    );
}

/// An index the statement does not name is named by the server, so it contests
/// nothing and any number of them may sit on one table.
#[test]
fn unnamed_indexes_never_collide() {
    parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX ON docs (id);
         CREATE INDEX ON docs (id);",
    )
    .expect("neither has a name to contest");
}

/// `IF NOT EXISTS` skips the statement whole, whatever kind of object holds the
/// name, which is what the notice a real server emits says.
#[test]
fn if_not_exists_skips_a_taken_name() {
    let db = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         CREATE INDEX IF NOT EXISTS i ON docs (id);
         CREATE INDEX IF NOT EXISTS docs ON docs (id);
         CREATE TABLE IF NOT EXISTS docs (other INT);
         CREATE TABLE IF NOT EXISTS i (id INT);",
    )
    .expect("every repeat is skipped");

    assert_eq!(db.tables().count(), 1, "no second table was created");
    assert_eq!(db.indexes().count(), 1, "no second index was created");
    assert!(
        db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("it exists")
            .column("other", &db, IdentifierCase::AsWritten)
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn a_dropped_or_renamed_index_frees_its_name() {
    parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         DROP INDEX i;
         CREATE INDEX i ON docs (id);",
    )
    .expect("the drop frees the name");

    let db = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         ALTER INDEX i RENAME TO j;
         CREATE INDEX i ON docs (id);",
    )
    .expect("the rename frees the name");

    let mut names: Vec<_> = db.indexes().filter_map(IndexLike::name).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["i", "j"]);
}

/// The renamed index lands in the same pool the old name came out of, so the
/// new name has to be free there.
#[test]
fn an_index_cannot_be_renamed_onto_a_taken_name() {
    let refused = parse(
        "CREATE TABLE docs (id INT);
         CREATE INDEX i ON docs (id);
         ALTER INDEX i RENAME TO docs;",
    );
    assert!(
        matches!(&refused, Err(Error::RelationNameAlreadyTaken { conflicting_kind, object_name, .. })
            if *conflicting_kind == ObjectKind::Table && object_name == "docs"),
        "got {refused:?}"
    );

    let absent = parse("CREATE TABLE docs (id INT); ALTER INDEX i RENAME TO j;");
    assert!(
        matches!(&absent, Err(Error::AlterIndexNotFound { index_name }) if index_name == "i"),
        "got {absent:?}"
    );
}

// Policies.

#[test]
fn a_policy_name_used_twice_on_one_table_is_refused() {
    let refused = parse(
        "CREATE TABLE docs (id INT);
         CREATE POLICY p ON docs USING (true);
         CREATE POLICY p ON docs FOR INSERT WITH CHECK (true);",
    );
    assert!(
        matches!(&refused, Err(Error::PolicyAlreadyExists { policy_name, table_name })
            if policy_name == "p" && table_name == "docs"),
        "the command the policy is FOR does not make it a second policy, got {refused:?}"
    );
}

#[test]
fn a_policy_name_is_free_on_another_table() {
    parse(
        "CREATE TABLE a (id INT);
         CREATE TABLE b (id INT);
         CREATE POLICY p ON a USING (true);
         CREATE POLICY p ON b USING (true);",
    )
    .expect("a policy belongs to its table");
}

#[test]
fn a_dropped_or_renamed_policy_frees_its_name() {
    parse(
        "CREATE TABLE docs (id INT);
         CREATE POLICY p ON docs USING (true);
         DROP POLICY p ON docs;
         CREATE POLICY p ON docs USING (true);",
    )
    .expect("the drop frees the name");

    parse(
        "CREATE TABLE docs (id INT);
         CREATE POLICY p ON docs USING (true);
         ALTER POLICY p ON docs RENAME TO q;
         CREATE POLICY p ON docs USING (true);",
    )
    .expect("the rename frees the name");
}

// Roles.

#[test]
fn a_role_created_twice_is_refused() {
    let refused = parse("CREATE ROLE r; CREATE ROLE r;");
    assert!(
        matches!(&refused, Err(Error::RoleAlreadyExists { role_name }) if role_name == "r"),
        "got {refused:?}"
    );
}

/// The setting that excuses a dump naming a role it never creates has nothing
/// to say here, because this statement is the creation.
#[test]
fn the_role_check_ignores_the_access_setting() {
    let permissive = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>("CREATE ROLE r; CREATE ROLE r;");
    assert!(matches!(&permissive, Err(Error::RoleAlreadyExists { .. })), "got {permissive:?}");
}

#[test]
fn a_dropped_or_renamed_role_frees_its_name() {
    parse("CREATE ROLE r; DROP ROLE r; CREATE ROLE r;").expect("the drop frees the name");

    let db = parse("CREATE ROLE r; ALTER ROLE r RENAME TO q; CREATE ROLE r;")
        .expect("the rename frees the name");
    let mut names: Vec<_> = db.roles().map(RoleLike::name).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["q", "r"]);
}

/// A grant holds the role itself rather than its spelling, so it follows the
/// role across a rename. Grants are the references a `DROP ROLE` refuses to
/// strand, so they are the ones the rename carries.
#[test]
fn a_grant_follows_a_renamed_role() {
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT);
         GRANT SELECT ON docs TO r;
         ALTER ROLE r RENAME TO q;",
    )
    .expect("the rename applies");

    assert_eq!(
        db.unresolved_access_references().expect("targets are well formed").count(),
        0,
        "the grant found the renamed role"
    );
}

#[test]
fn renaming_a_role_onto_a_taken_name_is_refused() {
    let refused = parse("CREATE ROLE r; CREATE ROLE q; ALTER ROLE r RENAME TO q;");
    assert!(
        matches!(&refused, Err(Error::RoleAlreadyExists { role_name }) if role_name == "q"),
        "got {refused:?}"
    );

    let absent = parse("ALTER ROLE r RENAME TO q;");
    assert!(
        matches!(&absent, Err(Error::AlterRoleNotFound { role_name }) if role_name == "r"),
        "got {absent:?}"
    );
}

// Triggers.

#[test]
fn a_trigger_name_used_twice_on_one_table_is_refused() {
    let refused = parse(&format!(
        "CREATE TABLE docs (id INT);
         {TRIGGER_FUNCTION}
         CREATE TRIGGER t AFTER INSERT ON docs FOR EACH ROW EXECUTE FUNCTION touch();
         CREATE TRIGGER t AFTER UPDATE ON docs FOR EACH ROW EXECUTE FUNCTION touch();"
    ));
    assert!(
        matches!(&refused, Err(Error::TriggerAlreadyExists { trigger_name, table_name })
            if trigger_name == "t" && table_name == "docs"),
        "a different event does not make it a second trigger, got {refused:?}"
    );
}

#[test]
fn a_trigger_name_is_free_on_another_table() {
    parse(&format!(
        "CREATE TABLE a (id INT);
         CREATE TABLE b (id INT);
         {TRIGGER_FUNCTION}
         CREATE TRIGGER t AFTER INSERT ON a FOR EACH ROW EXECUTE FUNCTION touch();
         CREATE TRIGGER t AFTER INSERT ON b FOR EACH ROW EXECUTE FUNCTION touch();"
    ))
    .expect("a trigger belongs to its table");
}

/// Dropping by name alone reached a trigger of the same name on another table,
/// which left the duplicate check one statement away from being undone.
#[test]
fn dropping_a_trigger_leaves_the_same_name_on_another_table_alone() {
    let refused = parse(&format!(
        "CREATE TABLE a (id INT);
         CREATE TABLE b (id INT);
         {TRIGGER_FUNCTION}
         CREATE TRIGGER t AFTER INSERT ON a FOR EACH ROW EXECUTE FUNCTION touch();
         CREATE TRIGGER t AFTER INSERT ON b FOR EACH ROW EXECUTE FUNCTION touch();
         DROP TRIGGER t ON a;
         CREATE TRIGGER t AFTER INSERT ON b FOR EACH ROW EXECUTE FUNCTION touch();"
    ));
    assert!(
        matches!(&refused, Err(Error::TriggerAlreadyExists { table_name, .. }) if table_name == "b"),
        "got {refused:?}"
    );

    let missing = parse(&format!(
        "CREATE TABLE a (id INT);
         CREATE TABLE b (id INT);
         {TRIGGER_FUNCTION}
         CREATE TRIGGER t AFTER INSERT ON a FOR EACH ROW EXECUTE FUNCTION touch();
         DROP TRIGGER t ON b;"
    ));
    assert!(
        matches!(&missing, Err(Error::DropTriggerNotFound { trigger_name }) if trigger_name == "t"),
        "got {missing:?}"
    );
}

#[test]
fn create_or_replace_replaces_the_stored_trigger() {
    let db = parse(&format!(
        "CREATE TABLE docs (id INT);
         {TRIGGER_FUNCTION}
         CREATE TRIGGER t AFTER INSERT ON docs FOR EACH ROW EXECUTE FUNCTION touch();
         CREATE OR REPLACE TRIGGER t AFTER UPDATE ON docs FOR EACH ROW EXECUTE FUNCTION touch();"
    ))
    .expect("the replacement is accepted");

    let triggers: Vec<_> = db.triggers().collect();
    assert_eq!(triggers.len(), 1, "the stale node is gone");
}

// Functions.

#[test]
fn a_function_signature_used_twice_is_refused() {
    let refused = parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION f(y INT) RETURNS INT AS 'SELECT 2;';",
    );
    assert!(
        matches!(&refused, Err(Error::FunctionAlreadyExists { function_name }) if function_name == "f"),
        "argument names are not part of the signature, got {refused:?}"
    );
}

/// The return type is not part of what identifies a function, so changing it
/// does not make a second one.
#[test]
fn a_different_return_type_is_still_the_same_function() {
    assert!(
        parse(
            "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1;';
             CREATE FUNCTION f() RETURNS TEXT AS 'SELECT ''x'';';"
        )
        .is_err()
    );
}

#[test]
fn functions_taking_different_arguments_may_share_a_name() {
    let db = parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2;';",
    )
    .expect("an overload is a different function");
    assert_eq!(db.functions().filter(|f| f.name() == "f").count(), 2);
}

/// Type aliases fold the way the database folds them, so `integer` and `int4`
/// name one type while `varchar` and `text` name two.
#[test]
fn argument_types_are_compared_after_folding_their_aliases() {
    assert!(
        parse(
            "CREATE FUNCTION f(x INTEGER) RETURNS INT AS 'SELECT 1;';
             CREATE FUNCTION f(x INT4) RETURNS INT AS 'SELECT 2;';"
        )
        .is_err(),
        "one type under two names"
    );

    parse(
        "CREATE FUNCTION f(x VARCHAR) RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2;';",
    )
    .expect("two types");
}

/// An `OUT` parameter describes the result rather than the call, so it is left
/// out of the signature and cannot be what tells two functions apart.
#[test]
fn an_out_parameter_does_not_make_a_second_function() {
    assert!(
        parse(
            "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
             CREATE FUNCTION f(x INT, OUT o INT) RETURNS INT AS 'SELECT 1;';"
        )
        .is_err()
    );
}

#[test]
fn create_or_replace_replaces_the_stored_function() {
    let db = parse(
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1;';
         CREATE OR REPLACE FUNCTION f() RETURNS INT AS 'SELECT 2;';",
    )
    .expect("the replacement is accepted");

    let stored: Vec<_> = db.functions().filter(|f| f.name() == "f").collect();
    assert_eq!(stored.len(), 1, "the stale node is gone");
    assert_eq!(stored[0].body(), Some("SELECT 2;"), "and the surviving one is the new body");
}

/// The builtins this crate registers stand in for `pg_catalog`, which is a
/// different schema from the one a `CREATE FUNCTION` lands in, so shadowing one
/// is accepted exactly as the database accepts it.
#[test]
fn a_function_may_shadow_a_builtin() {
    parse("CREATE FUNCTION length(t TEXT) RETURNS INT AS 'SELECT 1;';")
        .expect("the builtin lives in another schema");
    parse("CREATE FUNCTION now() RETURNS INT AS 'SELECT 1;';").expect("likewise");
}

#[test]
fn a_function_name_is_free_in_another_schema() {
    parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 2;';",
    )
    .expect("two schemas hold one `f` each");
}

/// Dropping by name alone took every overload with it, which left the duplicate
/// check one statement away from being undone.
#[test]
fn dropping_a_function_takes_only_the_signature_it_names() {
    let refused = parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2;';
         DROP FUNCTION f(INT);
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 3;';",
    );
    assert!(
        matches!(&refused, Err(Error::FunctionAlreadyExists { function_name }) if function_name == "f"),
        "the text overload was never dropped, got {refused:?}"
    );

    let missing = parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         DROP FUNCTION f(TEXT);",
    );
    assert!(
        matches!(&missing, Err(Error::DropFunctionNotFound { function_name }) if function_name == "f"),
        "got {missing:?}"
    );
}

/// Without an argument list the statement names whichever function carries the
/// name, and says nothing at all when more than one does.
#[test]
fn dropping_a_function_without_an_argument_list_needs_the_name_to_be_unique() {
    parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         DROP FUNCTION f;",
    )
    .expect("only one function carries the name");

    let ambiguous = parse(
        "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2;';
         DROP FUNCTION f;",
    );
    assert!(
        matches!(&ambiguous, Err(Error::AmbiguousDropFunction { function_name }) if function_name == "f"),
        "got {ambiguous:?}"
    );
}

/// A name without an argument list still names a schema, `public` when it
/// writes none, so a function of that name in another schema neither answers
/// nor makes the name ambiguous.
#[test]
fn dropping_a_function_without_an_argument_list_stays_in_its_schema() {
    let dropped = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1;';
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 2;';
         DROP FUNCTION f;",
    )
    .expect("only public.f carries the unqualified name");
    let left: Vec<_> = dropped
        .functions()
        .filter(|function| function.name() == "f")
        .map(|function| function.target_name().schema().map(str::to_string))
        .collect();
    assert_eq!(left, [Some("app".to_string())]);

    let missing = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1;';
         DROP FUNCTION f;",
    );
    assert!(
        matches!(&missing, Err(Error::DropFunctionNotFound { function_name }) if function_name == "f"),
        "got {missing:?}"
    );
}

#[test]
fn a_dropped_function_frees_its_signature() {
    parse(
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1;';
         DROP FUNCTION f();
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2;';",
    )
    .expect("the drop frees the signature");
}
