//! `CREATE OPERATOR` as PostgreSQL 18 applies it, read back from a live
//! server.
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

fn public(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("public", false)
}

/// A schema `app` with `app.eq(int4, int8)`, `app.flip(int8, int4)`, both
/// returning `bool`, and `app.add(int4, int4)` returning `int4`.
const FUNCTIONS: &str = "
CREATE SCHEMA app;
CREATE FUNCTION app.eq(a INT4, b INT8) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
CREATE FUNCTION app.flip(a INT8, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
CREATE FUNCTION app.add(a INT4, b INT4) RETURNS INT4 LANGUAGE sql AS 'SELECT a + b';
CREATE FUNCTION app.yes(a INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT TRUE';
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{FUNCTIONS}{sql}"))
}

/// The operator `name` over `left` and `right` outside `pg_catalog`.
fn declared<'db>(
    db: &'db ParserDB,
    name: TargetName<'_>,
    left: Option<TargetName<'_>>,
    right: TargetName<'_>,
) -> &'db PostgresCatalogOperator {
    let wanted = (name, left, Some(right));
    db.operators()
        .unwrap()
        .find(|operator| {
            (operator.target_name(), operator.left_type(db), operator.right_type(db)) == wanted
        })
        .unwrap()
}

/// The operators outside `pg_catalog`.
fn declared_count(db: &ParserDB) -> usize {
    db.operators().unwrap().filter(|operator| operator.schema() != Some("pg_catalog")).count()
}

#[test]
fn an_operator_binds_its_function_and_takes_its_result() {
    let db = parse("CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);")
        .unwrap();
    let operator = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    assert_eq!(operator.kind(), OperatorKind::Binary);
    assert_eq!(operator.result_type(&db), Some(pg("bool")));
    assert_eq!(operator.implementation(&db).unwrap().target_name(), app("eq"));
    assert!(!operator.can_hash());
    assert!(!operator.can_merge());
    assert_eq!(operator.commutator(&db), None);
    assert_eq!(declared_count(&db), 1);
}

#[test]
fn an_unqualified_operator_lands_in_the_creation_schema() {
    let db = parse(
        "SET search_path = app, public;
         CREATE OPERATOR ~-~ (PROCEDURE = yes, RIGHTARG = int4);",
    )
    .unwrap();
    let operator = declared(&db, app("~-~"), None, pg("int4"));
    assert_eq!(operator.kind(), OperatorKind::Prefix);
    assert_eq!(operator.implementation(&db).unwrap().target_name(), app("yes"));
}

#[test]
fn a_builtin_function_binds_through_the_implicit_catalog_schema() {
    let db =
        parse("CREATE OPERATOR <<< (FUNCTION = int4eq, LEFTARG = int4, RIGHTARG = int4);").unwrap();
    let operator = declared(&db, public("<<<"), Some(pg("int4")), pg("int4"));
    assert_eq!(operator.implementation(&db).unwrap().target_name(), pg("int4eq"));
}

#[test]
fn an_operator_over_a_declared_type_and_its_array_binds_their_function() {
    let db = parse(
        "CREATE TYPE app.token AS ENUM ('a');
         CREATE FUNCTION app.has(a app.token[], b app.token) RETURNS BOOL LANGUAGE sql AS 'SELECT TRUE';
         CREATE OPERATOR app.@> (FUNCTION = app.has, LEFTARG = app.token[], RIGHTARG = app.token);",
    )
    .unwrap();
    let operator = declared(&db, app("@>"), Some(app("_token")), app("token"));
    assert_eq!(operator.implementation(&db).unwrap().target_name(), app("has"));
}

#[test]
fn a_function_binds_only_by_its_exact_argument_types() {
    let error =
        parse("CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int2, RIGHTARG = int8);")
            .unwrap_err();
    assert!(
        matches!(&error, Error::OperatorFunctionNotFound { function_name, .. } if function_name.as_ref() == "app.eq"),
        "{error:?}"
    );
}

#[test]
fn an_operator_without_a_right_operand_is_refused() {
    let error = parse("CREATE OPERATOR app.### (FUNCTION = app.yes, LEFTARG = int4);").unwrap_err();
    assert!(matches!(error, Error::OperatorRightArgumentRequired { .. }), "{error:?}");
}

#[test]
fn an_operator_in_an_absent_schema_is_refused() {
    let error =
        parse("CREATE OPERATOR nowhere.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);")
            .unwrap_err();
    assert!(matches!(error, Error::SchemaNotFoundForRelation { .. }), "{error:?}");
}

#[test]
fn a_second_definition_is_refused() {
    let error = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);
         CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);",
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::OperatorAlreadyExists { operator_name } if operator_name.as_ref() == "<<<"),
        "{error:?}"
    );
}

#[test]
fn absent_commutator_and_negator_become_linked_shells() {
    let db = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             COMMUTATOR = OPERATOR(app.<<<), NEGATOR = <<=);",
    )
    .unwrap();
    let operator = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    let commutator = declared(&db, app("<<<"), Some(pg("int8")), pg("int4"));
    let negator = declared(&db, public("<<="), Some(pg("int4")), pg("int8"));
    assert_eq!(operator.commutator(&db), Some(commutator));
    assert_eq!(operator.negator(&db), Some(negator));
    assert_eq!(commutator.commutator(&db), Some(operator));
    assert_eq!(negator.negator(&db), Some(operator));
    for shell in [commutator, negator] {
        assert_eq!(shell.result_type(&db), None);
        assert!(shell.implementation(&db).is_none());
    }
    assert_eq!(declared_count(&db), 3);
}

#[test]
fn a_definition_fills_a_shell_with_its_own_links() {
    let db = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             COMMUTATOR = OPERATOR(app.<<<));
         CREATE OPERATOR app.<<< (FUNCTION = app.flip, LEFTARG = int8, RIGHTARG = int4);",
    )
    .unwrap();
    let operator = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    let filled = declared(&db, app("<<<"), Some(pg("int8")), pg("int4"));
    assert_eq!(filled.implementation(&db).unwrap().target_name(), app("flip"));
    assert_eq!(filled.result_type(&db), Some(pg("bool")));
    assert_eq!(filled.commutator(&db), None);
    assert_eq!(operator.commutator(&db), Some(filled));
    assert_eq!(declared_count(&db), 2);
}

#[test]
fn an_existing_commutator_and_negator_link_back() {
    let db = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.flip, LEFTARG = int8, RIGHTARG = int4);
         CREATE OPERATOR app.<<= (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);
         CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             COMMUTATOR = OPERATOR(app.<<<), NEGATOR = OPERATOR(app.<<=));",
    )
    .unwrap();
    let operator = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    let commutator = declared(&db, app("<<<"), Some(pg("int8")), pg("int4"));
    let negator = declared(&db, app("<<="), Some(pg("int4")), pg("int8"));
    assert_eq!(commutator.commutator(&db), Some(operator));
    assert_eq!(negator.negator(&db), Some(operator));
}

#[test]
fn a_commutator_linked_elsewhere_is_refused() {
    let error = parse(
        "CREATE FUNCTION app.lt(a INT4, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a < b';
         CREATE OPERATOR app.<~ (FUNCTION = app.lt, LEFTARG = int4, RIGHTARG = int4,
             COMMUTATOR = OPERATOR(app.~>));
         CREATE OPERATOR app.&< (FUNCTION = app.lt, LEFTARG = int4, RIGHTARG = int4,
             COMMUTATOR = OPERATOR(app.~>));",
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::OperatorLinkTaken { attribute: OperatorAttribute::Commutator, linked_name, .. }
                if linked_name.as_ref() == "~>"
        ),
        "{error:?}"
    );
}

#[test]
fn an_operator_may_commute_with_itself_and_never_negate_itself() {
    let db = parse(
        "CREATE FUNCTION app.same(a INT4, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
         CREATE OPERATOR app.#= (FUNCTION = app.same, LEFTARG = int4, RIGHTARG = int4,
             COMMUTATOR = OPERATOR(app.#=));",
    )
    .unwrap();
    let operator = declared(&db, app("#="), Some(pg("int4")), pg("int4"));
    assert_eq!(operator.commutator(&db), Some(operator));
    assert_eq!(declared_count(&db), 1);

    let error = parse(
        "CREATE FUNCTION app.same(a INT4, b INT4) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
         CREATE OPERATOR app.<^ (FUNCTION = app.same, LEFTARG = int4, RIGHTARG = int4,
             NEGATOR = OPERATOR(app.<^));",
    )
    .unwrap_err();
    assert!(matches!(error, Error::OperatorOwnNegator { .. }), "{error:?}");
}

#[test]
fn only_a_boolean_operator_takes_a_negator_flags_or_estimators() {
    for (option, attribute) in [
        ("NEGATOR = <->", OperatorAttribute::Negator),
        ("HASHES", OperatorAttribute::Hashes),
        ("MERGES", OperatorAttribute::Merges),
        ("RESTRICT = eqsel", OperatorAttribute::Restrict),
        ("JOIN = eqjoinsel", OperatorAttribute::Join),
    ] {
        let error = parse(&format!(
            "CREATE OPERATOR app.&&& (FUNCTION = app.add, LEFTARG = int4, RIGHTARG = int4, {option});"
        ))
        .unwrap_err();
        assert!(
            matches!(&error, Error::OperatorAttributeNeedsBoolean { attribute: refused, .. } if *refused == attribute),
            "{option}: {error:?}"
        );
    }
}

#[test]
fn only_a_binary_operator_takes_a_commutator_flags_or_a_join_estimator() {
    for (option, attribute) in [
        ("COMMUTATOR = ~~~~", OperatorAttribute::Commutator),
        ("HASHES", OperatorAttribute::Hashes),
        ("MERGES", OperatorAttribute::Merges),
        ("JOIN = eqjoinsel", OperatorAttribute::Join),
    ] {
        let error = parse(&format!(
            "CREATE OPERATOR app.~~~~ (FUNCTION = app.yes, RIGHTARG = int4, {option});"
        ))
        .unwrap_err();
        assert!(
            matches!(&error, Error::OperatorAttributeNeedsBinary { attribute: refused, .. } if *refused == attribute),
            "{option}: {error:?}"
        );
    }
    let db = parse(
        "CREATE OPERATOR app.~~~~ (FUNCTION = app.yes, RIGHTARG = int4, RESTRICT = eqsel,
             NEGATOR = OPERATOR(app.!~~));",
    )
    .unwrap();
    let operator = declared(&db, app("~~~~"), None, pg("int4"));
    assert_eq!(operator.restriction_estimator(&db).unwrap().target_name(), pg("eqsel"));
    assert_eq!(operator.negator(&db).map(|negator| negator.target_name()), Some(app("!~~")));
}

#[test]
fn flags_and_estimators_bind_as_declared() {
    let db = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             RESTRICT = eqsel, JOIN = eqjoinsel, HASHES, MERGES);",
    )
    .unwrap();
    let operator = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    assert!(operator.can_hash());
    assert!(operator.can_merge());
    assert_eq!(operator.restriction_estimator(&db).unwrap().target_name(), pg("eqsel"));
    assert_eq!(operator.join_estimator(&db).unwrap().target_name(), pg("eqjoinsel"));

    let error = parse(
        "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             RESTRICT = nosuch);",
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::OperatorFunctionNotFound { function_name, .. } if function_name.as_ref() == "nosuch"),
        "{error:?}"
    );
}

#[test]
fn a_creator_without_execute_on_the_function_is_refused() {
    let error = parse(
        "CREATE ROLE r;
         REVOKE EXECUTE ON FUNCTION app.eq(INT4, INT8) FROM PUBLIC;
         SET ROLE r;
         CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8);",
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::OperatorFunctionNotExecutable { function_name, role_name, .. }
                if function_name.as_ref() == "eq" && role_name.as_ref() == "r"
        ),
        "{error:?}"
    );
}

#[test]
fn a_rolled_back_creation_leaves_no_operator() {
    let db = parse(
        "BEGIN;
         CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8,
             COMMUTATOR = OPERATOR(app.<<<));
         ROLLBACK;",
    )
    .unwrap();
    assert_eq!(declared_count(&db), 0);
}

#[test]
fn a_link_into_an_absent_schema_is_refused() {
    for link in ["COMMUTATOR = OPERATOR(nowhere.>>>)", "NEGATOR = OPERATOR(nowhere.<<=)"] {
        let error = parse(&format!(
            "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = int4, RIGHTARG = int8, {link});"
        ))
        .unwrap_err();
        assert!(matches!(error, Error::SchemaNotFoundForRelation { .. }), "{link}: {error:?}");
    }
}

#[test]
fn an_operand_type_no_catalog_holds_is_refused() {
    for left in ["datetime", "datetime[]"] {
        let error = parse(&format!(
            "CREATE OPERATOR app.<<< (FUNCTION = app.eq, LEFTARG = {left}, RIGHTARG = int8);"
        ))
        .unwrap_err();
        assert!(matches!(error, Error::TypeNotFound { .. }), "{left}: {error:?}");
    }
}

#[test]
fn an_unqualified_function_binds_through_the_user_schema_and_a_placed_catalog_schema() {
    let db = parse(
        "CREATE ROLE ur;
         CREATE SCHEMA ur;
         CREATE FUNCTION ur.ueq(a INT4, b INT8) RETURNS BOOL LANGUAGE sql AS 'SELECT a = b';
         SET ROLE ur;
         SET search_path = \"$user\";
         CREATE OPERATOR app.<<< (FUNCTION = ueq, LEFTARG = int4, RIGHTARG = int8);
         RESET ROLE;
         SET search_path = \"$user\", app, pg_catalog;
         CREATE OPERATOR app.>>> (FUNCTION = int4eq, LEFTARG = int4, RIGHTARG = int4);",
    )
    .unwrap();
    let user = declared(&db, app("<<<"), Some(pg("int4")), pg("int8"));
    assert_eq!(
        user.implementation(&db).unwrap().target_name(),
        TargetName::new("ueq", false).with_schema("ur", false)
    );
    let catalog = declared(&db, app(">>>"), Some(pg("int4")), pg("int4"));
    assert_eq!(catalog.implementation(&db).unwrap().target_name(), pg("int4eq"));
}

#[test]
fn a_refused_attribute_is_named_as_postgresql_spells_it() {
    for (option, spelled) in [
        ("NEGATOR = <->", "negator"),
        ("HASHES", "hashes"),
        ("MERGES", "merges"),
        ("RESTRICT = eqsel", "restrict"),
        ("JOIN = eqjoinsel", "join"),
    ] {
        let error = parse(&format!(
            "CREATE OPERATOR app.&&& (FUNCTION = app.add, LEFTARG = int4, RIGHTARG = int4, {option});"
        ))
        .unwrap_err();
        assert!(error.to_string().contains(&format!(" {spelled} attribute ")), "{error}");
    }
    let error =
        parse("CREATE OPERATOR app.~~~~ (FUNCTION = app.yes, RIGHTARG = int4, COMMUTATOR = ~~~~);")
            .unwrap_err();
    assert!(error.to_string().contains(" commutator attribute "), "{error}");
}
