//! `ALTER OPERATOR` and `DROP OPERATOR` as PostgreSQL 18 applies them, and the
//! links and estimators an operator keeps across the lifecycles of what they
//! name.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::{
    errors::{Error, OperatorAttribute},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

fn other(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("other", false)
}

/// A schema `app` with `app.<<<(int4, int8)`, `app.>>>(int8, int4)` and
/// `app.<<=(int4, int8)`, all returning `bool` and linked to nothing, and the
/// prefix `app.~~~(int4)` returning `int4`.
const OPERATORS: &str = "
CREATE SCHEMA app;
CREATE SCHEMA other;
CREATE FUNCTION app.eq(a INT4, b INT8) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
CREATE FUNCTION app.flip(a INT8, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
CREATE FUNCTION app.neg(a INT4) RETURNS INT4 LANGUAGE sql AS 'SELECT -a';
CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);
CREATE OPERATOR app.>>> (FUNCTION = app.flip, LEFTARG = int8, RIGHTARG = int4);
CREATE OPERATOR app.<<= (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);
CREATE OPERATOR app.~~~ (FUNCTION = app.neg, RIGHTARG = int4);
SET search_path = app, public;
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{OPERATORS}{sql}"))
}

/// The operator `name` over `left` and `right`, when the database holds it.
fn find<'db>(
    db: &'db ParserDB,
    name: TargetName<'_>,
    left: Option<TargetName<'_>>,
    right: TargetName<'_>,
) -> Option<&'db PostgresCatalogOperator> {
    let wanted = (name, left, Some(right));
    db.operators().unwrap().find(|operator| {
        (operator.target_name(), operator.left_type(db), operator.right_type(db)) == wanted
    })
}

fn less(db: &ParserDB) -> &PostgresCatalogOperator {
    find(db, app("<<<"), Some(pg("int4")), pg("int8")).unwrap()
}

fn greater(db: &ParserDB) -> &PostgresCatalogOperator {
    find(db, app(">>>"), Some(pg("int8")), pg("int4")).unwrap()
}

fn not_less(db: &ParserDB) -> &PostgresCatalogOperator {
    find(db, app("<<="), Some(pg("int4")), pg("int8")).unwrap()
}

fn refused_attribute(error: &Error) -> Option<OperatorAttribute> {
    match error {
        Error::OperatorAttributeAlreadySet { attribute, .. } => Some(*attribute),
        _ => None,
    }
}

#[test]
fn set_links_existing_operators_both_ways() {
    let db = parse("ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>, NEGATOR = <<=);")
        .unwrap();
    assert_eq!(less(&db).commutator(&db), Some(greater(&db)));
    assert_eq!(greater(&db).commutator(&db), Some(less(&db)));
    assert_eq!(less(&db).negator(&db), Some(not_less(&db)));
    assert_eq!(not_less(&db).negator(&db), Some(less(&db)));
}

#[test]
fn set_refuses_a_link_to_an_absent_operator_and_changes_nothing() {
    let error =
        parse("ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = eqsel, COMMUTATOR = <->);")
            .unwrap_err();
    assert!(
        matches!(&error, Error::OperatorNotFound { operator_name } if operator_name.as_ref() == "<->"),
        "{error:?}"
    );
}

#[test]
fn set_refuses_changing_a_link_or_flag_already_set() {
    let linked = "
        CREATE FUNCTION app.other(a INT8, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
        CREATE OPERATOR app.&< (FUNCTION = app.other, LEFTARG = int8, RIGHTARG = int4);
        ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>, HASHES);";
    let error =
        parse(&format!("{linked} ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = &<);"))
            .unwrap_err();
    assert_eq!(refused_attribute(&error), Some(OperatorAttribute::Commutator), "{error:?}");

    // Naming the link or flag already held changes nothing and is accepted.
    let db = parse(&format!(
        "{linked} ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>, HASHES, MERGES);"
    ))
    .unwrap();
    assert!(less(&db).can_hash());
    assert!(less(&db).can_merge());

    // `&<` is free, but `<<<` already commutes with `>>>`.
    let error =
        parse(&format!("{linked} ALTER OPERATOR app.&< (int8, int4) SET (COMMUTATOR = <<<);"))
            .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::OperatorLinkTaken { attribute: OperatorAttribute::Commutator, linked_name, linked_to, .. }
                if linked_name.as_ref() == "<<<" && linked_to.as_ref() == ">>>"
        ),
        "{error:?}"
    );
}

#[test]
fn set_binds_and_clears_estimators() {
    let db = parse("ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = eqsel, JOIN = eqjoinsel);")
        .unwrap();
    assert_eq!(less(&db).restriction_estimator(&db).unwrap().target_name(), pg("eqsel"));
    assert_eq!(less(&db).join_estimator(&db).unwrap().target_name(), pg("eqjoinsel"));

    let db = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = eqsel, JOIN = eqjoinsel);
         ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = NONE);",
    )
    .unwrap();
    assert!(less(&db).restriction_estimator(&db).is_none());
    assert_eq!(less(&db).join_estimator(&db).unwrap().target_name(), pg("eqjoinsel"));
}

#[test]
fn set_checks_the_operator_kind_and_result() {
    let error = parse("ALTER OPERATOR app.~~~ (NONE, int4) SET (RESTRICT = eqsel);").unwrap_err();
    assert!(
        matches!(
            error,
            Error::OperatorAttributeNeedsBoolean { attribute: OperatorAttribute::Restrict, .. }
        ),
        "{error:?}"
    );
    // A shell has no result yet.
    let error = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>);
         CREATE OPERATOR app.#= (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8, NEGATOR = OPERATOR(app.#-#));
         ALTER OPERATOR app.#-# (int4, int8) SET (JOIN = eqjoinsel);",
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            Error::OperatorAttributeNeedsBoolean { attribute: OperatorAttribute::Join, .. }
        ),
        "{error:?}"
    );
}

#[test]
fn an_absent_operator_is_refused_by_alter_and_skipped_by_drop_if_exists() {
    let error = parse("ALTER OPERATOR app.<<< (int8, int8) SET (RESTRICT = eqsel);").unwrap_err();
    assert!(matches!(error, Error::OperatorNotFound { .. }), "{error:?}");
    let error = parse("DROP OPERATOR app.<<< (int8, int8);").unwrap_err();
    assert!(matches!(error, Error::OperatorNotFound { .. }), "{error:?}");
    let db = parse("DROP OPERATOR IF EXISTS app.<<< (int8, int8), app.~~~ (NONE, int4);").unwrap();
    assert!(find(&db, app("~~~"), None, pg("int4")).is_none());
    assert!(find(&db, app("<<<"), Some(pg("int4")), pg("int8")).is_some());
}

#[test]
fn drop_clears_the_links_others_hold() {
    let db = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>, NEGATOR = <<=);
         DROP OPERATOR app.>>> (int8, int4), app.<<= (int4, int8);
         CREATE OPERATOR app.>>> (FUNCTION = app.flip, LEFTARG = int8, RIGHTARG = int4);",
    )
    .unwrap();
    assert_eq!(less(&db).commutator(&db), None);
    assert_eq!(less(&db).negator(&db), None);
}

#[test]
fn a_dropped_function_takes_the_operator_its_estimators_run() {
    let estimator = "
        CREATE FUNCTION app.sel(internal, oid, internal, INT4) RETURNS FLOAT8 LANGUAGE internal STABLE STRICT AS 'eqsel';
        ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = app.sel);";
    let error =
        parse(&format!("{estimator} DROP FUNCTION app.sel(internal, oid, internal, INT4);"))
            .unwrap_err();
    assert!(matches!(error, Error::FunctionReferenced { .. }), "{error:?}");
    let db = parse(&format!(
        "{estimator} DROP FUNCTION app.sel(internal, oid, internal, INT4) CASCADE;"
    ))
    .unwrap();
    assert!(find(&db, app("<<<"), Some(pg("int4")), pg("int8")).is_none());
}

#[test]
fn a_renamed_estimator_and_schema_carry_every_identity() {
    let db = parse(
        "CREATE FUNCTION app.sel(internal, oid, internal, INT4) RETURNS FLOAT8 LANGUAGE internal STABLE STRICT AS 'eqsel';
         ALTER OPERATOR app.<<< (int4, int8) SET (RESTRICT = app.sel, COMMUTATOR = >>>);
         ALTER FUNCTION app.sel(internal, oid, internal, INT4) RENAME TO picked;
         ALTER SCHEMA app RENAME TO moved;",
    )
    .unwrap();
    let moved = |name| TargetName::new(name, false).with_schema("moved", false);
    let less = find(&db, moved("<<<"), Some(pg("int4")), pg("int8")).unwrap();
    assert_eq!(less.restriction_estimator(&db).unwrap().target_name(), moved("picked"));
    assert_eq!(less.commutator(&db).map(|commutator| commutator.target_name()), Some(moved(">>>")));
}

#[test]
fn set_schema_moves_the_operator_and_the_links_to_it() {
    let db = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>);
         ALTER OPERATOR app.>>> (int8, int4) SET SCHEMA other;",
    )
    .unwrap();
    let moved = find(&db, other(">>>"), Some(pg("int8")), pg("int4")).unwrap();
    assert_eq!(less(&db).commutator(&db), Some(moved));
    assert_eq!(moved.commutator(&db), Some(less(&db)));

    let error = parse(
        "CREATE OPERATOR other.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);
         ALTER OPERATOR app.<<< (int4, int8) SET SCHEMA other;",
    )
    .unwrap_err();
    assert!(matches!(error, Error::OperatorAlreadyExists { .. }), "{error:?}");
}

#[test]
fn owner_to_names_a_declared_role() {
    parse("CREATE ROLE r; ALTER OPERATOR app.<<< (int4, int8) OWNER TO r;").unwrap();
    let error = parse("ALTER OPERATOR app.<<< (int8, int8) OWNER TO r;").unwrap_err();
    assert!(matches!(error, Error::OperatorNotFound { .. }), "{error:?}");
}

#[test]
fn a_rolled_back_drop_restores_the_operator_and_its_links() {
    let db = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (COMMUTATOR = >>>);
         BEGIN;
         DROP OPERATOR app.>>> (int8, int4);
         ROLLBACK;",
    )
    .unwrap();
    assert_eq!(less(&db).commutator(&db), Some(greater(&db)));
}

#[test]
fn set_refuses_an_absent_negator_and_the_operator_itself() {
    let error = parse("ALTER OPERATOR app.<<< (int4, int8) SET (NEGATOR = &<);").unwrap_err();
    assert!(
        matches!(&error, Error::OperatorNotFound { operator_name } if operator_name.as_ref() == "&<"),
        "{error:?}"
    );
    let error = parse("ALTER OPERATOR app.<<< (int4, int8) SET (NEGATOR = <<<);").unwrap_err();
    assert!(matches!(error, Error::OperatorOwnNegator { .. }), "{error:?}");
}

#[test]
fn owner_to_an_undeclared_role_is_refused_only_in_a_closed_world() {
    let error = parse("ALTER OPERATOR app.<<< (int4, int8) OWNER TO ghost;").unwrap_err();
    assert!(matches!(error, Error::RoleNotFoundForOwner { .. }), "{error:?}");
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(&format!(
            "{OPERATORS} ALTER OPERATOR app.<<< (int4, int8) OWNER TO ghost;"
        ))
        .unwrap();
}

#[test]
fn a_drop_if_exists_of_absent_operators_changes_nothing() {
    let db = parse("DROP OPERATOR IF EXISTS app.<<< (int8, int8), app.>>> (int4, int4);").unwrap();
    assert!(find(&db, app("<<<"), Some(pg("int4")), pg("int8")).is_some());
    assert!(find(&db, app(">>>"), Some(pg("int8")), pg("int4")).is_some());
}

#[test]
fn set_schema_moves_the_negator_links_to_it() {
    let db = parse(
        "ALTER OPERATOR app.<<< (int4, int8) SET (NEGATOR = <<=);
         ALTER OPERATOR app.<<= (int4, int8) SET SCHEMA other;",
    )
    .unwrap();
    let moved = find(&db, other("<<="), Some(pg("int4")), pg("int8")).unwrap();
    assert_eq!(less(&db).negator(&db), Some(moved));
    assert_eq!(moved.negator(&db), Some(less(&db)));
}

#[test]
fn a_dropped_join_estimator_is_refused_while_an_operator_runs_it() {
    let error = parse(
        "CREATE FUNCTION app.joinsel(internal, oid, internal, INT2, internal) RETURNS FLOAT8 LANGUAGE internal STABLE STRICT AS 'eqjoinsel';
         ALTER OPERATOR app.<<< (int4, int8) SET (JOIN = app.joinsel);
         DROP FUNCTION app.joinsel(internal, oid, internal, INT2, internal);",
    )
    .unwrap_err();
    assert!(matches!(error, Error::FunctionReferenced { .. }), "{error:?}");
}
