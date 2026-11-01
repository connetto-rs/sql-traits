//! A deployment description names the roles a request runs as and the session
//! settings its policies read, and refuses what PostgreSQL could never hold.
#![allow(clippy::expect_used)]

use sql_traits::{errors::DeploymentError, prelude::*};
use sqlparser::{ast::DataType, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

const KINDS: &str = "CREATE ROLE reader LOGIN NOINHERIT;
     CREATE ROLE anonymous NOLOGIN;
     CREATE ROLE member NOLOGIN;";

fn caller() -> SessionSetting {
    SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Uuid))
        .expect("a two-part name")
}

fn subjects() -> SessionSetting {
    SessionSetting::value(
        "app.subjects",
        SettingValue::Set { element: DataType::Text, packing: SetPacking::Delimited(",".into()) },
    )
    .expect("a two-part name")
}

#[test]
fn a_description_resolves_its_roles_in_the_schema() {
    let database = parse(KINDS);
    let deployment = Deployment::new("reader", ["anonymous", "member"], [caller(), subjects()])
        .expect("a valid description");

    let resolved = deployment.resolve(&database).expect("every role is declared");
    assert_eq!(resolved.login_role().name(), "reader");
    let acting: Vec<&str> = resolved.acting_roles().map(RoleLike::name).collect();
    assert_eq!(acting, ["anonymous", "member"]);
}

#[test]
fn a_role_the_schema_does_not_declare_is_refused_by_name() {
    let database = parse(KINDS);
    let unknown_acting =
        Deployment::new("reader", ["anonymous", "bot"], [caller()]).expect("a valid description");
    assert_eq!(
        unknown_acting.resolve(&database).map(|_| ()),
        Err(DeploymentError::UndeclaredRole { role: "bot".into() })
    );

    let unknown_login =
        Deployment::new("authenticator", ["member"], [caller()]).expect("a valid description");
    assert_eq!(
        unknown_login.resolve(&database).map(|_| ()),
        Err(DeploymentError::UndeclaredRole { role: "authenticator".into() })
    );
}

#[test]
fn a_pooled_deployment_acts_as_its_login_role() {
    let database = parse(KINDS);
    let pooled = Deployment::new("reader", ["reader"], [caller()]).expect("a pooled description");
    let resolved = pooled.resolve(&database).expect("the role is declared");
    assert_eq!(resolved.acting_roles().map(RoleLike::name).collect::<Vec<_>>(), ["reader"]);
}

#[test]
fn a_request_has_to_act_as_some_role_and_each_only_once() {
    assert_eq!(
        Deployment::new("reader", Vec::<String>::new(), [caller()]),
        Err(DeploymentError::NoActingRole)
    );
    assert_eq!(
        Deployment::new("reader", ["member", "member"], [caller()]),
        Err(DeploymentError::DuplicateActingRole { role: "member".into() })
    );
}

#[test]
fn setting_names_differing_only_in_case_are_one_setting() {
    let shouted = SessionSetting::value("APP.User_Id", SettingValue::Scalar(DataType::Text))
        .expect("a two-part name");
    assert_eq!(
        Deployment::new("reader", ["member"], [caller(), shouted]),
        Err(DeploymentError::DuplicateSetting { name: "APP.User_Id".into() })
    );

    let deployment =
        Deployment::new("reader", ["member"], [caller()]).expect("a valid description");
    let found = deployment.setting("App.User_ID").expect("names compare without case");
    assert_eq!(found.name(), "app.user_id");
}

#[test]
fn only_a_custom_setting_name_can_carry_a_request_value() {
    for refused in ["user_id", "", ".user_id", "app.", "app..user_id"] {
        assert_eq!(
            SessionSetting::value(refused, SettingValue::Scalar(DataType::Text)).map(|_| ()),
            Err(DeploymentError::InvalidSettingName { name: refused.into() }),
            "{refused:?} is not a custom setting name"
        );
    }
    assert!(
        SessionSetting::value("request.jwt.claims", SettingValue::Scalar(DataType::Text)).is_ok(),
        "PostgREST's three-part name is a custom setting"
    );
}

#[test]
fn an_empty_separator_cannot_split_a_set() {
    let empty = SettingValue::Set {
        element: DataType::Text,
        packing: SetPacking::Delimited(String::new()),
    };
    assert_eq!(
        SessionSetting::value("app.subjects", empty).map(|_| ()),
        Err(DeploymentError::EmptySeparator { setting: "app.subjects".into() })
    );
}

#[test]
fn a_json_setting_names_distinct_non_empty_fields() {
    let sub = || JsonField::new(["sub"], SettingValue::Caller(DataType::Uuid)).expect("a path");
    let roles = JsonField::new(
        ["app_metadata", "roles"],
        SettingValue::Set { element: DataType::Text, packing: SetPacking::JsonArray },
    )
    .expect("a path");
    assert!(
        SessionSetting::json("request.jwt.claims", [sub(), roles]).is_ok(),
        "a field may be declared the caller, as Supabase reads `sub` from the claims"
    );

    assert_eq!(
        JsonField::new(Vec::<String>::new(), SettingValue::Scalar(DataType::Text)).map(|_| ()),
        Err(DeploymentError::EmptyFieldPath)
    );
    assert_eq!(
        SessionSetting::json("request.jwt.claims", Vec::new()).map(|_| ()),
        Err(DeploymentError::NoJsonFields { setting: "request.jwt.claims".into() })
    );
    assert_eq!(
        SessionSetting::json("request.jwt.claims", [sub(), sub()]).map(|_| ()),
        Err(DeploymentError::DuplicateJsonField {
            setting: "request.jwt.claims".into(),
            path: vec!["sub".into()],
        })
    );
}
