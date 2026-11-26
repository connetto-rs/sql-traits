//! What a PL/pgSQL function's output parameters hold when it returns, on the
//! requests a deployment admits.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::{
    errors::{RaiseSite, ReturnedValuesError},
    prelude::*,
};
use sqlparser::{
    ast::{DataType, ExactNumberInfo, Ident, ObjectName},
    dialect::PostgreSqlDialect,
};

/// The validated installation identity pair, whose validator raises when
/// exactly one half is set.
const VALIDATOR: &str = "CREATE ROLE app;
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO app, authz_owner;
CREATE FUNCTION authz.acting_installation(OUT namespace_id UUID, OUT id UUID) RETURNS RECORD
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
  namespace_id := NULLIF(current_setting('app.installation_namespace_id', true), '')::UUID;
  id := NULLIF(current_setting('app.installation_id', true), '')::UUID;
  IF (namespace_id IS NULL) <> (id IS NULL) THEN
    RAISE EXCEPTION 'incomplete installation identity' USING ERRCODE = '22023';
  END IF;
END
$$;
ALTER FUNCTION authz.acting_installation() OWNER TO authz_owner;";

/// A setting of type `uuid` a request may leave unset.
fn optional(name: &str) -> SessionSetting {
    SessionSetting::value(name, SettingValue::Scalar(DataType::Uuid))
        .expect("a two-part name")
        .may_be_unset()
}

/// The installation identity halves and an optional caller, none grouped.
fn deployment() -> Deployment {
    Deployment::new(
        "app",
        ["app"],
        [
            optional("app.installation_namespace_id"),
            optional("app.installation_id"),
            optional("app.user_id"),
        ],
    )
    .expect("a valid deployment")
}

/// The deployment setting both installation identity halves together.
fn paired() -> Deployment {
    deployment()
        .with_settings_set_together(["app.installation_namespace_id", "app.installation_id"])
        .expect("two declared optional settings")
}

/// What `function`, in `schema`, returns under `deployment`.
fn returned(
    schema: &str,
    function: &str,
    deployment: &Deployment,
) -> Result<Vec<ReturnedValue>, ReturnedValuesError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(schema).expect("the schema parses");
    let function = db
        .function_by_target(TargetName::new(function, false), IdentifierCase::AsWritten)
        .expect("the lookup succeeds")
        .expect("the function exists");
    function.returned_values(&db, deployment)
}

/// What `body`, the body of `f(OUT a UUID, OUT b UUID)`, returns under
/// `deployment`.
fn returned_by(
    body: &str,
    deployment: &Deployment,
) -> Result<Vec<ReturnedValue>, ReturnedValuesError> {
    let schema =
        format!("CREATE FUNCTION f(OUT a UUID, OUT b UUID) LANGUAGE plpgsql AS $$ {body} $$;");
    returned(&schema, "f", deployment)
}

/// Each parameter's name with the expression it holds, rendered.
fn rendered(values: &[ReturnedValue]) -> Vec<(&str, Option<String>)> {
    values.iter().map(|value| (value.name(), value.expression().map(ToString::to_string))).collect()
}

/// What the validator returns under `deployment`.
fn validator_returns(deployment: &Deployment) -> Result<Vec<ReturnedValue>, ReturnedValuesError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(VALIDATOR).expect("the schema parses");
    let validator = db
        .function_by_target(
            TargetName::new("acting_installation", false).with_schema("authz", false),
            IdentifierCase::AsWritten,
        )
        .expect("the lookup succeeds")
        .expect("the validator exists");
    validator.returned_values(&db, deployment)
}

#[test]
fn the_validator_returns_both_setting_reads_where_the_pair_is_set_together() {
    let values = validator_returns(&paired()).expect("no raise is reachable");
    assert_eq!(
        rendered(&values),
        [
            (
                "namespace_id",
                Some(
                    "NULLIF(current_setting('app.installation_namespace_id', true), '')::UUID"
                        .to_string()
                )
            ),
            (
                "id",
                Some("NULLIF(current_setting('app.installation_id', true), '')::UUID".to_string())
            ),
        ]
    );
}

#[test]
fn the_validator_raises_where_the_pair_may_be_set_apart() {
    let raise = RaiseSite::Step { line: 6, column: 5 };
    let refusal =
        ReturnedValuesError::Raises { function: "acting_installation".into(), raise, surely: true };
    assert_eq!(validator_returns(&deployment()), Err(refusal.clone()));
    let other_group = deployment()
        .with_settings_set_together(["app.installation_id", "app.user_id"])
        .expect("two declared optional settings");
    assert_eq!(validator_returns(&other_group), Err(refusal));
}

#[test]
fn a_raise_reachable_on_a_request_setting_neither_half_refuses() {
    let body = "BEGIN
  a := NULLIF(current_setting('app.installation_namespace_id', true), '')::UUID;
  b := NULLIF(current_setting('app.installation_id', true), '')::UUID;
  IF b IS NULL THEN
    RAISE EXCEPTION 'no installation';
  END IF;
END";
    assert!(matches!(
        returned_by(body, &paired()),
        Err(ReturnedValuesError::Raises { surely: true, raise: RaiseSite::Step { .. }, .. })
    ));
}

#[test]
fn a_cast_of_an_unset_setting_read_without_missing_ok_may_raise() {
    let body = "BEGIN a := current_setting('app.installation_id')::UUID; END";
    assert!(matches!(
        returned_by(body, &paired()),
        Err(ReturnedValuesError::Raises { surely: false, .. })
    ));
}

#[test]
fn a_parameter_without_one_expression_is_told_apart() {
    let body = "DECLARE chosen UUID := NULLIF(current_setting('app.user_id', true), '')::UUID;
BEGIN
  IF chosen IS NULL THEN
    a := NULLIF(current_setting('app.installation_id', true), '')::UUID;
  END IF;
  b := chosen;
END";
    let values = returned_by(body, &paired()).expect("no raise is reachable");
    let outputs: Vec<_> = values.iter().map(ReturnedValue::output).collect();
    assert_eq!(outputs, [&ReturnedOutput::Several, &ReturnedOutput::Unread]);
    let values = returned_by("BEGIN END", &paired()).expect("no raise is reachable");
    let outputs: Vec<_> = values.iter().map(ReturnedValue::output).collect();
    assert_eq!(outputs, [&ReturnedOutput::Null, &ReturnedOutput::Null]);
}

#[test]
fn only_a_plpgsql_function_with_output_parameters_is_read() {
    let sql = "CREATE FUNCTION f(OUT a UUID) LANGUAGE sql AS $$ SELECT NULL::UUID $$;";
    assert_eq!(
        returned(sql, "f", &paired()),
        Err(ReturnedValuesError::Language { function: "f".into(), language: Some("sql".into()) })
    );
    let no_outputs =
        "CREATE FUNCTION f() RETURNS UUID LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;";
    assert_eq!(
        returned(no_outputs, "f", &paired()),
        Err(ReturnedValuesError::NoOutputParameters { function: "f".into() })
    );
    assert_eq!(
        returned("CREATE FUNCTION f(OUT result int) LANGUAGE plpgsql;", "f", &paired()),
        Err(ReturnedValuesError::Lookup(
            sql_traits::errors::LookupError::FunctionBodyUnavailable { function: "f".into() }
        ))
    );
}

#[test]
fn a_domain_parameter_still_checks_what_it_is_assigned() {
    let body = "BEGIN a := NULLIF(current_setting('app.installation_id', true), '')::UUID; END";
    let schema = format!(
        "CREATE DOMAIN installation AS UUID CHECK (VALUE <> '00000000-0000-0000-0000-000000000000');
CREATE FUNCTION f(OUT a installation) LANGUAGE plpgsql AS $$ {body} $$;"
    );
    assert!(matches!(
        returned(&schema, "f", &paired()),
        Err(ReturnedValuesError::Raises { surely: false, .. })
    ));
    let plain = format!("CREATE FUNCTION f(OUT a UUID) LANGUAGE plpgsql AS $$ {body} $$;");
    assert!(returned(&plain, "f", &paired()).is_ok(), "a uuid parameter checks nothing");
}

#[test]
fn a_declared_domain_variable_checks_the_null_it_starts_with() {
    let domains = "CREATE DOMAIN present AS int CHECK (VALUE IS NOT NULL);
CREATE DOMAIN positive AS int CHECK (VALUE > 0);
CREATE DOMAIN small_present AS present CHECK (VALUE < 10);";
    for (declared, refused) in [("present", true), ("small_present", true), ("positive", false)] {
        for path in ["", "SET search_path = app, pg_catalog"] {
            let domains = if path.is_empty() {
                domains.to_owned()
            } else {
                let qualified = domains
                    .replace("DOMAIN ", "DOMAIN app.")
                    .replace("AS present", "AS app.present");
                format!("CREATE SCHEMA app; {qualified}")
            };
            let schema = format!(
                "{domains} CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql {path} AS $$
DECLARE w {declared}; BEGIN o := 'x'; END $$;"
            );
            let returned = returned(&schema, "f", &paired());
            assert_eq!(
                matches!(
                    returned,
                    Err(ReturnedValuesError::Raises {
                        raise: RaiseSite::Declaration { .. },
                        surely: false,
                        ..
                    })
                ),
                refused,
                "{declared} {path}: {returned:?}"
            );
        }
    }
}

#[test]
fn a_cast_to_a_checking_domain_raises_where_its_checks_refuse() {
    let domains = "CREATE DOMAIN present AS int CHECK (VALUE IS NOT NULL);
CREATE DOMAIN positive AS int CHECK (VALUE > 0);
CREATE DOMAIN small_present AS present CHECK (VALUE < 10);";
    // Only `RAISE`, `ASSERT` and `CASE` surely raise, so an assignment
    // whose every evaluation raises still only may.
    for (cast, raises) in
        [("NULL::present", true), ("NULL::small_present", true), ("NULL::positive", false)]
    {
        for path in ["", "SET search_path = app, pg_catalog"] {
            let domains = if path.is_empty() {
                domains.to_owned()
            } else {
                let qualified = domains
                    .replace("DOMAIN ", "DOMAIN app.")
                    .replace("AS present", "AS app.present");
                format!("CREATE SCHEMA app; {qualified}")
            };
            let schema = format!(
                "{domains} CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql {path} AS $$
BEGIN o := ({cast})::text; END $$;"
            );
            let returned = returned(&schema, "f", &paired());
            assert_eq!(
                matches!(returned, Err(ReturnedValuesError::Raises { surely: false, .. })),
                raises,
                "{cast} {path}: {returned:?}"
            );
            assert!(raises || returned.is_ok(), "{cast} {path}: {returned:?}");
        }
    }
}

#[test]
fn a_setting_declared_as_a_domain_checks_the_null_an_unset_read_gives() {
    let present = DataType::Custom(ObjectName::from(vec![Ident::new("present")]), Vec::new());
    let setting = SessionSetting::value("app.n", SettingValue::Scalar(present))
        .expect("a two-part name")
        .may_be_unset();
    let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
    let schema = "CREATE DOMAIN present AS int CHECK (VALUE IS NOT NULL);
CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$
BEGIN o := (NULLIF(current_setting('app.n', true), '')::present IS NULL)::text; END $$;";
    let returned = returned(schema, "f", &deployment);
    assert!(
        matches!(returned, Err(ReturnedValuesError::Raises { surely: false, .. })),
        "{returned:?}"
    );
}

/// The pair validator's body, over `a` and `b`, with `nullif` as the
/// function reading each half through.
fn pair_validator(nullif: &str) -> String {
    format!(
        "BEGIN
  a := {nullif}(current_setting('app.installation_namespace_id', true), '')::UUID;
  b := {nullif}(current_setting('app.installation_id', true), '')::UUID;
  IF (a IS NULL) <> (b IS NULL) THEN
    RAISE EXCEPTION 'incomplete installation identity';
  END IF;
END"
    )
}

#[test]
fn a_setting_the_function_sets_itself_holds_what_no_request_tells() {
    let body = pair_validator("NULLIF");
    let clause = format!(
        "CREATE FUNCTION f(OUT a UUID, OUT b UUID) LANGUAGE plpgsql SET app.installation_id = '' AS $$ {body} $$;"
    );
    assert!(matches!(
        returned(&clause, "f", &paired()),
        Err(ReturnedValuesError::Raises { surely: true, .. })
    ));
    let configured =
        body.replacen("BEGIN", "BEGIN\n  PERFORM set_config('app.installation_id', '', true);", 1);
    assert!(matches!(
        returned_by(&configured, &paired()),
        Err(ReturnedValuesError::Raises { surely: true, .. })
    ));
    let reset = body.replacen("BEGIN", "BEGIN\n  RESET ALL;", 1);
    assert!(matches!(
        returned_by(&reset, &paired()),
        Err(ReturnedValuesError::Raises { surely: true, .. })
    ));
}

#[test]
fn only_the_bare_nullif_keyword_turns_an_empty_setting_into_null() {
    assert!(returned_by(&pair_validator("NULLIF"), &paired()).is_ok());
    // A quoted `"nullif"` calls a function, which `pg_catalog` does not hold.
    assert_eq!(
        returned_by(&pair_validator("\"nullif\""), &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "\"nullif\"".into() })
    );
}

#[test]
fn a_setting_every_request_sets_may_read_the_empty_string() {
    let label = SessionSetting::value("app.label", SettingValue::Scalar(DataType::Text))
        .expect("a two-part name");
    let deployment = Deployment::new("app", ["app"], [label]).expect("a valid deployment");
    let schema = "CREATE FUNCTION f(OUT a TEXT) LANGUAGE plpgsql AS $$ BEGIN
  a := NULLIF(current_setting('app.label'), '');
  IF a IS NULL THEN
    RAISE EXCEPTION 'empty label';
  END IF;
END $$;";
    assert!(matches!(
        returned(schema, "f", &deployment),
        Err(ReturnedValuesError::Raises { surely: true, .. })
    ));
}

#[test]
fn a_cast_to_the_declared_type_with_other_modifiers_may_raise() {
    let amount = SessionSetting::value(
        "app.amount",
        SettingValue::Scalar(DataType::Numeric(ExactNumberInfo::None)),
    )
    .expect("a two-part name")
    .may_be_unset();
    let deployment = Deployment::new("app", ["app"], [amount, optional("app.installation_id")])
        .expect("a valid deployment")
        .with_settings_set_together(["app.amount", "app.installation_id"])
        .expect("two declared optional settings");
    let cast = |target: &str| {
        format!(
            "CREATE FUNCTION f(OUT a NUMERIC) LANGUAGE plpgsql AS $$ BEGIN
  a := NULLIF(current_setting('app.amount', true), '')::{target};
END $$;"
        )
    };
    assert!(returned(&cast("NUMERIC"), "f", &deployment).is_ok());
    assert!(matches!(
        returned(&cast("NUMERIC(3, 0)"), "f", &deployment),
        Err(ReturnedValuesError::Raises { surely: false, .. })
    ));
}

#[test]
fn a_body_running_code_outside_pg_catalog_is_refused() {
    let body = pair_validator("NULLIF");
    let clearing = format!(
        "CREATE FUNCTION public.clear_installation() RETURNS text LANGUAGE sql
  AS $$ SELECT set_config('app.installation_id', '', true) $$;
CREATE FUNCTION f(OUT a UUID, OUT b UUID) LANGUAGE plpgsql AS $$
DECLARE cleared text := public.clear_installation();
{body} $$;"
    );
    assert_eq!(
        returned(&clearing, "f", &paired()),
        Err(ReturnedValuesError::RunsCode {
            function: "f".into(),
            code: "public.clear_installation".into()
        })
    );
    let raising = "CREATE FUNCTION boom() RETURNS int LANGUAGE plpgsql
  AS $$ BEGIN RAISE EXCEPTION 'boom'; END $$;
CREATE FUNCTION f(OUT a int) LANGUAGE plpgsql AS $$ BEGIN a := boom(); END $$;";
    assert!(matches!(returned(raising, "f", &paired()), Err(ReturnedValuesError::RunsCode { .. })));
    let called = body.replacen(
        "BEGIN",
        "BEGIN\n  BEGIN DO $do$ BEGIN END $do$; EXCEPTION WHEN others THEN NULL; END;",
        1,
    );
    assert!(matches!(returned_by(&called, &paired()), Err(ReturnedValuesError::RunsCode { .. })));
}

#[test]
fn a_setting_the_body_sets_is_not_read_as_the_request_sets_it() {
    let schema = "CREATE FUNCTION f(OUT a text) LANGUAGE plpgsql AS $$ BEGIN
  BEGIN
    PERFORM set_config('app.installation_id', 'forged', true);
  EXCEPTION WHEN others OR query_canceled OR assert_failure THEN NULL;
  END;
  a := current_setting('app.installation_id', true);
END $$;";
    let values = returned(schema, "f", &deployment()).expect("no raise is reachable");
    let outputs: Vec<_> = values.iter().map(ReturnedValue::output).collect();
    assert_eq!(outputs, [&ReturnedOutput::Unread]);
}

#[test]
fn a_statement_whose_trigger_may_set_a_setting_is_refused() {
    let body = pair_validator("NULLIF").replacen(
        "BEGIN",
        "BEGIN\n  BEGIN INSERT INTO audit VALUES (1); EXCEPTION WHEN others THEN NULL; END;",
        1,
    );
    let schema = format!(
        "CREATE TABLE audit (n INT);
CREATE FUNCTION clear() RETURNS trigger LANGUAGE plpgsql AS $t$ BEGIN
  PERFORM set_config('app.installation_id', '', true);
  RETURN NEW;
END $t$;
CREATE TRIGGER clearing BEFORE INSERT ON audit FOR EACH ROW EXECUTE FUNCTION clear();
CREATE FUNCTION f(OUT a UUID, OUT b UUID) LANGUAGE plpgsql AS $$ {body} $$;"
    );
    assert_eq!(
        returned(&schema, "f", &paired()),
        Err(ReturnedValuesError::RunsCode {
            function: "f".into(),
            code: "INSERT INTO audit VALUES (1)".into()
        })
    );
    let read = "CREATE VIEW cleared AS SELECT 1 AS n;
CREATE FUNCTION f(OUT a int) LANGUAGE plpgsql AS $$ BEGIN a := (SELECT n FROM cleared); END $$;";
    assert_eq!(
        returned(read, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "cleared".into() })
    );
    let table = "CREATE VIEW cleared AS SELECT 1 AS n;
CREATE FUNCTION f(OUT a int) LANGUAGE plpgsql AS $$ BEGIN
  a := (SELECT s.n FROM (TABLE cleared) AS s);
END $$;";
    assert_eq!(
        returned(table, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "TABLE cleared".into() })
    );
    let scanned = "CREATE VIEW cleared AS SELECT 1 AS n;
CREATE FUNCTION f(OUT a xml) LANGUAGE plpgsql AS $$ BEGIN
  a := table_to_xml('cleared', true, false, '');
END $$;";
    assert_eq!(
        returned(scanned, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "table_to_xml".into() })
    );
}

#[test]
fn a_fetch_from_a_portal_the_caller_opened_is_refused() {
    let fetched = "CREATE FUNCTION f(c refcursor, OUT o text) LANGUAGE plpgsql AS $$ BEGIN
  FETCH c INTO o;
END $$;";
    assert_eq!(
        returned(fetched, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "c".into() })
    );
    let bound = "CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$
DECLARE k CURSOR FOR SELECT 'x';
BEGIN
  OPEN k;
  FETCH k INTO o;
  CLOSE k;
END $$;";
    assert!(
        !matches!(returned(bound, "f", &paired()), Err(ReturnedValuesError::RunsCode { .. })),
        "a cursor the body declares runs the query the body holds"
    );
    let shadowed = "CREATE FUNCTION f(C refcursor, OUT o text) LANGUAGE plpgsql AS $$ BEGIN
  DECLARE c CURSOR FOR SELECT 'x'; BEGIN NULL; END;
  FETCH c INTO o;
END $$;";
    assert_eq!(
        returned(shadowed, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "c".into() }),
        "a parameter written in capitals is the cursor the outer block fetches from"
    );
    for assignment in ["k := 'foreign';", "SELECT 'foreign' INTO k;"] {
        let schema = format!(
            "CREATE FUNCTION f(OUT result text) LANGUAGE plpgsql AS $$
DECLARE k CURSOR FOR SELECT 'x';
BEGIN {assignment} FETCH k INTO result; END $$;"
        );
        assert_eq!(
            returned(&schema, "f", &paired()),
            Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "k".into() }),
            "{assignment}"
        );
    }
}

#[test]
fn a_domain_or_operator_running_a_function_outside_pg_catalog_is_refused() {
    let evil = "CREATE FUNCTION public.evil(int) RETURNS boolean LANGUAGE sql
  AS $f$ SELECT set_config('app.installation_id', '', false) IS NOT NULL $f$;
CREATE DOMAIN d AS int CHECK (public.evil(VALUE));";
    for (body, code) in
        [("DECLARE w d; BEGIN o := 'x'; END", "d"), ("BEGIN o := (1::d)::text; END", "d")]
    {
        let schema =
            format!("{evil} CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$ {body} $$;");
        assert_eq!(
            returned(&schema, "f", &paired()),
            Err(ReturnedValuesError::RunsCode { function: "f".into(), code: code.into() }),
            "{body}"
        );
    }
    let held = format!(
        "{evil} CREATE TABLE holder (w d);
CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$ DECLARE r holder%ROWTYPE; BEGIN o := 'x'; END $$;"
    );
    assert_eq!(
        returned(&held, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "holder%ROWTYPE".into() })
    );
    let viewed = format!(
        "{evil} CREATE TABLE holder (w d); CREATE VIEW seen AS SELECT w FROM holder;
CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$ DECLARE r seen; BEGIN o := 'x'; END $$;"
    );
    assert_eq!(
        returned(&viewed, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "seen".into() })
    );
    let operator = "CREATE FUNCTION public.evil2(int, int) RETURNS boolean LANGUAGE sql
  AS $f$ SELECT true $f$;
CREATE OPERATOR public.## (LEFTARG = int, RIGHTARG = int, FUNCTION = public.evil2);
CREATE FUNCTION f(OUT o int) LANGUAGE plpgsql AS $$ BEGIN
  o := 1;
  IF 1 ## 2 THEN o := 2; END IF;
END $$;";
    assert_eq!(
        returned(operator, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "##".into() })
    );
}

#[test]
fn a_built_in_setting_the_body_sets_is_read_without_current_setting() {
    let schema = "CREATE FUNCTION f(OUT a text) LANGUAGE plpgsql AS $$ BEGIN
  BEGIN
    PERFORM set_config('role', 'other', true);
  EXCEPTION WHEN others OR query_canceled OR assert_failure THEN NULL;
  END;
  a := current_user;
END $$;";
    let values = returned(schema, "f", &deployment()).expect("no raise is reachable");
    let outputs: Vec<_> = values.iter().map(ReturnedValue::output).collect();
    assert_eq!(outputs, [&ReturnedOutput::Unread]);
}

#[test]
fn every_target_a_domain_types_checks_what_it_is_assigned() {
    let schema = "CREATE DOMAIN present AS int CHECK (VALUE IS NOT NULL);
CREATE TABLE t (c present);";
    for (outputs, body, refused) in [
        ("OUT a present", "BEGIN a := NULL; END", true),
        ("OUT o text", "DECLARE w t.c%TYPE; BEGIN o := 'x'; END", true),
        ("OUT o text", "DECLARE r t%ROWTYPE; BEGIN r.c := NULL; o := 'x'; END", true),
        ("OUT o text", "BEGIN o := NULL; END", false),
    ] {
        let function =
            format!("{schema} CREATE FUNCTION f({outputs}) LANGUAGE plpgsql AS $$ {body} $$;");
        let returned = returned(&function, "f", &paired());
        assert_eq!(
            matches!(returned, Err(ReturnedValuesError::Raises { .. })),
            refused,
            "{body}: {returned:?}"
        );
    }
}

#[test]
fn a_keyword_form_call_reaching_a_function_outside_pg_catalog_is_refused() {
    let schema = "CREATE FUNCTION public.ceil(integer) RETURNS integer LANGUAGE sql
  AS $f$ SELECT CASE WHEN set_config('app.installation_id', '', false) IS NOT NULL THEN $1 END $f$;
CREATE FUNCTION f(OUT o text) LANGUAGE plpgsql AS $$ BEGIN o := ceil(5)::text; END $$;";
    assert_eq!(
        returned(schema, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "ceil".into() })
    );
}

#[test]
fn a_domain_whose_check_sets_a_setting_is_refused() {
    let schema = "CREATE DOMAIN sneaky AS int
  CHECK (set_config('app.installation_id', '', false) IS NOT NULL);
CREATE FUNCTION f(OUT a uuid, OUT b int) LANGUAGE plpgsql AS $$ BEGIN
  b := NULL::sneaky;
  a := current_setting('app.installation_id')::uuid;
END $$;";
    assert_eq!(
        returned(schema, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "sneaky".into() })
    );
}

/// Return paths distinguish nontermination and sets.
#[test]
fn return_shapes_are_told_apart() {
    assert_eq!(
        returned_by("BEGIN LOOP END LOOP; END", &paired()),
        Err(ReturnedValuesError::NeverReturns { function: "f".into() })
    );
    let schema = "CREATE FUNCTION f(OUT a int) RETURNS SETOF int LANGUAGE plpgsql
AS $$ BEGIN a := 1; RETURN NEXT; END $$;";
    assert_eq!(
        returned(schema, "f", &paired()),
        Err(ReturnedValuesError::ReturnsSet { function: "f".into() })
    );
    for body in [
        "BEGIN IF current_setting('app.installation_id', true) IS NULL THEN RETURN; END IF; RETURN; END",
        "BEGIN IF current_setting('app.installation_id', true) IS NULL THEN RETURN; END IF; END",
    ] {
        let values = returned_by(body, &paired()).expect("both paths return null outputs");
        assert_eq!(rendered(&values), [("a", None), ("b", None)]);
        assert_eq!(values[0].output(), &ReturnedOutput::Null);
    }
}

/// Dynamic statements are refused even with constant text.
#[test]
fn dynamic_statements_are_refused() {
    for (body, code) in [
        ("BEGIN EXECUTE 'SELECT 1'; END", "EXECUTE 'SELECT 1'"),
        ("BEGIN PERFORM query_to_xml('SELECT 1', true, false, ''); END", "query_to_xml"),
    ] {
        assert_eq!(
            returned_by(body, &paired()),
            Err(ReturnedValuesError::RunsCode { function: "f".into(), code: code.into() }),
            "{body}"
        );
    }
}

/// Setting changes invalidate request reads.
#[test]
fn computed_settings_invalidate_outputs() {
    for (change, read) in [
        ("PERFORM set_config('app.' || 'installation_id', '', true);", "'app.installation_id'"),
        ("PERFORM set_config('app.installation_id', '', true);", "'app.' || 'installation_id'"),
        ("SET app.installation_id = '';", "'app.installation_id'"),
        ("RESET app.installation_id;", "'app.installation_id'"),
        ("PERFORM set_config(NULL, '', true);", "'app.installation_id'"),
    ] {
        let schema = format!(
            "CREATE FUNCTION f(OUT result text) LANGUAGE plpgsql AS $$ BEGIN
BEGIN {change} EXCEPTION WHEN others OR query_canceled OR assert_failure THEN NULL; END;
BEGIN result := current_setting({read}, true);
EXCEPTION WHEN others OR query_canceled OR assert_failure THEN LOOP END LOOP; END;
END $$;"
        );
        let answer = returned(&schema, "f", &paired());
        assert!(answer.is_ok(), "{change} {answer:?}");
        let values = answer.expect("only successful reads return");
        assert_eq!(values[0].output(), &ReturnedOutput::Unread, "{change}");
    }
}

/// Refusals display the failing statement or variable default.
#[test]
fn raise_locations_are_displayed() {
    for (body, message) in [
        ("BEGIN RAISE EXCEPTION 'failed'; END", "an error does leave `f` at line 1, column 8"),
        (
            "DECLARE chosen integer := 1 / 0; BEGIN END",
            "an error may leave `f` at the default of `chosen`",
        ),
    ] {
        assert_eq!(returned_by(body, &paired()).expect_err("the body raises").to_string(), message);
    }
}

/// Types containing an external domain check refuse output analysis.
#[test]
fn composite_types_and_parameters_run_domain_checks() {
    let schema = "CREATE FUNCTION evil(integer) RETURNS boolean LANGUAGE sql AS $$ SELECT true $$;
CREATE DOMAIN checked AS int CHECK (evil(VALUE));
CREATE TYPE wrapped AS (value checked);
CREATE TYPE plain AS (value int);";
    for (arguments, body, code) in [
        ("OUT result checked", "BEGIN END", "checked"),
        ("OUT result int", "DECLARE value wrapped; BEGIN END", "wrapped"),
        ("OUT result int", "DECLARE value absent%ROWTYPE; BEGIN END", "absent%ROWTYPE"),
    ] {
        let function =
            format!("{schema} CREATE FUNCTION f({arguments}) LANGUAGE plpgsql AS $$ {body} $$;");
        assert_eq!(
            returned(&function, "f", &paired()),
            Err(ReturnedValuesError::RunsCode { function: "f".into(), code: code.into() }),
            "{body}"
        );
    }
}

/// Relation scans through quoted calls and set operands remain external code.
#[test]
fn quoted_scans_and_set_operands_are_refused() {
    for (expression, code) in [
        ("\"table_to_xml\"('seen', true, false, '')", "\"table_to_xml\""),
        ("(SELECT value FROM (TABLE seen UNION ALL TABLE seen) AS source)", "TABLE seen"),
    ] {
        let schema = format!(
            "CREATE TABLE seen (value int);
CREATE FUNCTION f(OUT result text) LANGUAGE plpgsql AS $$ BEGIN result := ({expression})::text; END $$;"
        );
        assert_eq!(
            returned(&schema, "f", &paired()),
            Err(ReturnedValuesError::RunsCode { function: "f".into(), code: code.into() }),
            "{expression}"
        );
    }
}

/// Setting reads and nullification preserve their returned expressions.
#[test]
fn setting_expression_shapes_are_preserved() {
    for expression in [
        "current_setting('app.installation_id', true)",
        "NULLIF(current_setting('app.installation_id', true), 'unset')",
        "NULLIF(current_setting('app.installation_id', true), NULL)",
        "NULLIF(current_setting('app.installation_id', true), current_setting('app.installation_id', true))",
        "NULLIF('literal', '')",
        "current_setting('app.user_id', true)",
        "current_setting('search_path', true)",
        "pg_catalog.current_setting('app.installation_id', true)",
        "concat('a', 'b', 'c')",
        "concat('app.installation_id', true)",
    ] {
        let schema = format!(
            "CREATE FUNCTION f(OUT result text) LANGUAGE plpgsql AS $$ BEGIN result := {expression}; END $$;"
        );
        let answer = returned(&schema, "f", &paired());
        if expression == "current_setting('search_path', true)" {
            assert!(matches!(answer, Err(ReturnedValuesError::Raises { surely: false, .. })));
            continue;
        }
        assert!(answer.is_ok(), "{expression} {answer:?}");
        let values = answer.expect("text reads cannot raise");
        assert_eq!(values[0].expression().expect("one expression").to_string(), expression);
    }
}

/// A loop bound that divides by zero refuses output analysis.
#[test]
fn raising_loop_iterations_are_located() {
    for body in [
        "BEGIN FOR counter IN 1..(1 / 0) LOOP NULL; END LOOP; END",
        "BEGIN WHILE 1 / 0 = 1 LOOP NULL; END LOOP; END",
    ] {
        assert!(matches!(
            returned_by(body, &paired()),
            Err(ReturnedValuesError::Raises {
                raise: RaiseSite::Step { line: 1, column: 8 },
                surely: false,
                ..
            })
        ));
    }
}

/// Null assignments to record targets preserve null outputs.
#[test]
fn record_target_shapes_preserve_outputs() {
    for (declaration, assignment) in [
        ("value record;", "value := NULL;"),
        ("value pair;", "value := NULL;"),
        ("value pair;", "value.first := NULL;"),
    ] {
        let schema = format!(
            "CREATE TYPE pair AS (first integer, second integer);
CREATE FUNCTION f(OUT result text) LANGUAGE plpgsql AS $$
DECLARE {declaration} BEGIN {assignment} END $$;"
        );
        let values = returned(&schema, "f", &paired()).expect("null assignments do not raise");
        assert_eq!(values[0].output(), &ReturnedOutput::Null);
    }
}

/// Required setting casts retain the expression returning their value.
#[test]
fn required_setting_casts_preserve_outputs() {
    let setting = SessionSetting::value("app.identity", SettingValue::Scalar(DataType::Uuid))
        .expect("a two-part name");
    let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
    let schema = "CREATE FUNCTION f(OUT result uuid) LANGUAGE plpgsql AS $$ BEGIN
result := NULLIF(current_setting('app.identity', true), '')::uuid; END $$;";
    let values = returned(schema, "f", &deployment).expect("a required UUID has valid text");
    assert_eq!(
        rendered(&values),
        [("result", Some("NULLIF(current_setting('app.identity', true), '')::UUID".into()))]
    );
    assert!(matches!(
        returned_by(
            "BEGIN a := current_setting('app.installation_id', true)::uuid; END",
            &paired()
        ),
        Err(ReturnedValuesError::Raises { surely: false, .. })
    ));
}

/// Implicit comparisons may invoke an overloaded equality operator.
#[test]
fn implied_operators_are_refused() {
    let schema = "CREATE FUNCTION evil(integer, integer) RETURNS boolean LANGUAGE sql
AS $$ SELECT true $$;
CREATE OPERATOR public.= (LEFTARG = integer, RIGHTARG = integer, FUNCTION = evil);
CREATE FUNCTION f(OUT result int) LANGUAGE plpgsql AS $$ BEGIN result := 1; END $$;";
    assert_eq!(
        returned(schema, "f", &paired()),
        Err(ReturnedValuesError::RunsCode { function: "f".into(), code: "=".into() })
    );
}

/// Aliases preserve the output parameter they name.
#[test]
fn output_aliases_preserve_null() {
    let schema = "CREATE FUNCTION f(OUT result int) LANGUAGE plpgsql AS $$
DECLARE renamed ALIAS FOR result; BEGIN renamed := NULL; END $$;";
    let values = returned(schema, "f", &paired()).expect("the alias names the output");
    assert_eq!(rendered(&values), [("result", Some("NULL".into()))]);
}

/// Null output assignments keep composite and record targets nullable.
#[test]
fn composite_output_targets_preserve_null() {
    for target in ["record", "pair"] {
        let schema = format!(
            "CREATE TYPE pair AS (first integer, second integer);
CREATE FUNCTION f(OUT result {target}) LANGUAGE plpgsql AS $$ BEGIN result := NULL; END $$;"
        );
        let values = returned(&schema, "f", &paired()).expect("a null output is permitted");
        assert_eq!(rendered(&values), [("result", Some("NULL".into()))]);
    }
}

/// Literal assignments preserve expressions of the output's type.
#[test]
fn typed_literal_outputs_preserve_expressions() {
    for (target, expression) in [("date", "DATE '2024-01-01'"), ("integer", "-1")] {
        let schema = format!(
            "CREATE FUNCTION f(OUT result {target}) LANGUAGE plpgsql AS $$ BEGIN result := {expression}; END $$;"
        );
        let values = returned(&schema, "f", &paired()).expect("the literal fits the output type");
        assert_eq!(rendered(&values), [("result", Some(expression.into()))]);
    }
}

/// A cast or a declaration naming a type off the body's search path raises.
#[test]
fn a_type_outside_the_body_type_path_is_refused() {
    for body in ["BEGIN result := NULL::hidden; END", "DECLARE w hidden; BEGIN result := 1; END"] {
        let schema = format!(
            "CREATE DOMAIN public.hidden AS integer;
CREATE FUNCTION f(OUT result integer) LANGUAGE plpgsql SET search_path = pg_catalog AS $$
{body} $$;"
        );
        let answer = returned(&schema, "f", &paired());
        assert!(matches!(answer, Err(ReturnedValuesError::Raises { .. })), "{body}: {answer:?}");
    }
    let reached = "CREATE DOMAIN public.hidden AS integer;
CREATE FUNCTION f(OUT result integer) LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN result := NULL::hidden; END $$;";
    assert!(returned(reached, "f", &paired()).is_ok());
}
