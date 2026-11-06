//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for a caller,
//! and why it refuses to answer.
//!
//! The fixture is connetto's R100 ownership schema. Every caller's expected
//! set was measured on PostgreSQL 17.11 by calling the helpers over the rows
//! [`ROWS`] holds.
#![allow(clippy::expect_used, clippy::panic)]

extern crate alloc;

use alloc::collections::BTreeSet;

use sql_traits::{errors::HelperSetError, prelude::*};
use sqlparser::{
    ast::{DataType, Value},
    dialect::PostgreSqlDialect,
};

const R100: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE TABLE organizations (id TEXT PRIMARY KEY, created_by TEXT NOT NULL);
CREATE TABLE teams (
  id TEXT PRIMARY KEY,
  organization_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE CASCADE
);
CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL REFERENCES teams (id) ON DELETE CASCADE
);
CREATE TABLE organization_grants (
  organization_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
  principal_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('read', 'write', 'admin')),
  PRIMARY KEY (organization_id, principal_id)
);
CREATE TABLE team_grants (
  team_id TEXT NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  principal_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('read', 'write', 'admin')),
  PRIMARY KEY (team_id, principal_id)
);
CREATE TABLE project_grants (
  project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
  principal_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('read', 'write', 'admin')),
  PRIMARY KEY (project_id, principal_id)
);
CREATE TABLE connetto_installations (
  id TEXT PRIMARY KEY, bot TEXT NOT NULL, owner TEXT NOT NULL, accepted_version INT NOT NULL);
CREATE SCHEMA authz;
CREATE FUNCTION authz.organizations_with(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.organization_id FROM public.organization_grants g
  WHERE g.principal_id = current_setting('app.user_id', true)
    AND (g.role = 'admin' OR min_role = 'read' OR (min_role = 'write' AND g.role = 'write'))
$$;
CREATE FUNCTION authz.teams_with(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.team_id FROM public.team_grants g
  WHERE g.principal_id = current_setting('app.user_id', true)
    AND (g.role = 'admin' OR min_role = 'read' OR (min_role = 'write' AND g.role = 'write'))
  UNION
  SELECT t.id FROM public.teams t
  WHERE t.organization_id IN (SELECT authz.organizations_with(min_role))
$$;
CREATE FUNCTION authz.projects_with(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.project_id FROM public.project_grants g
  WHERE g.principal_id = current_setting('app.user_id', true)
    AND (g.role = 'admin' OR min_role = 'read' OR (min_role = 'write' AND g.role = 'write'))
  UNION
  SELECT p.id FROM public.projects p
  WHERE p.team_id IN (SELECT authz.teams_with(min_role))
$$;
CREATE FUNCTION authz.owners_with(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT authz.organizations_with(min_role)
  UNION SELECT authz.teams_with(min_role)
  UNION SELECT authz.projects_with(min_role)
$$;
CREATE FUNCTION authz.installations_administered() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT i.id FROM public.connetto_installations i
  WHERE i.owner IN (SELECT authz.organizations_with('admin'))
  UNION
  SELECT i.id FROM public.connetto_installations i
  WHERE i.owner IN (SELECT authz.teams_with('admin'))
  UNION
  SELECT i.id FROM public.connetto_installations i
  WHERE i.owner IN (SELECT authz.projects_with('admin'))
$$;
ALTER FUNCTION authz.organizations_with(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.teams_with(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.projects_with(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.owners_with(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.installations_administered() OWNER TO authz_owner;
REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA authz FROM PUBLIC;
GRANT SELECT ON organizations, teams, projects, organization_grants, team_grants, project_grants, connetto_installations TO authz_owner;
GRANT USAGE ON SCHEMA authz TO authz_owner;
ALTER TABLE organizations ENABLE ROW LEVEL SECURITY;
ALTER TABLE teams ENABLE ROW LEVEL SECURITY;
ALTER TABLE projects ENABLE ROW LEVEL SECURITY;
ALTER TABLE organization_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE team_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE project_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE connetto_installations ENABLE ROW LEVEL SECURITY;
ALTER TABLE organizations FORCE ROW LEVEL SECURITY;
ALTER TABLE teams FORCE ROW LEVEL SECURITY;
ALTER TABLE projects FORCE ROW LEVEL SECURITY;
ALTER TABLE organization_grants FORCE ROW LEVEL SECURITY;
ALTER TABLE team_grants FORCE ROW LEVEL SECURITY;
ALTER TABLE project_grants FORCE ROW LEVEL SECURITY;
ALTER TABLE connetto_installations FORCE ROW LEVEL SECURITY;
";

/// The rows of R100's behaviour matrix, as `(table, [(column, value)])`.
const ROWS: &[(&str, &[(&str, &str)])] = &[
    ("organizations", &[("id", "org"), ("created_by", "founder")]),
    ("teams", &[("id", "T"), ("organization_id", "org")]),
    ("teams", &[("id", "U"), ("organization_id", "org")]),
    ("projects", &[("id", "P"), ("team_id", "T")]),
    ("projects", &[("id", "Q"), ("team_id", "U")]),
    (
        "organization_grants",
        &[("organization_id", "org"), ("principal_id", "alice"), ("role", "admin")],
    ),
    (
        "organization_grants",
        &[("organization_id", "org"), ("principal_id", "olga"), ("role", "read")],
    ),
    (
        "organization_grants",
        &[("organization_id", "org"), ("principal_id", "owen"), ("role", "write")],
    ),
    ("team_grants", &[("team_id", "T"), ("principal_id", "tom"), ("role", "admin")]),
    ("team_grants", &[("team_id", "T"), ("principal_id", "bob"), ("role", "write")]),
    ("team_grants", &[("team_id", "U"), ("principal_id", "uma"), ("role", "read")]),
    ("team_grants", &[("team_id", "T"), ("principal_id", "inst-T"), ("role", "write")]),
    ("project_grants", &[("project_id", "Q"), ("principal_id", "quinn"), ("role", "read")]),
    ("connetto_installations", &[("id", "inst-org"), ("owner", "org")]),
    ("connetto_installations", &[("id", "inst-T"), ("owner", "T")]),
    ("connetto_installations", &[("id", "inst-U"), ("owner", "U")]),
    ("connetto_installations", &[("id", "inst-P"), ("owner", "P")]),
    ("connetto_installations", &[("id", "inst-Q"), ("owner", "Q")]),
    ("connetto_installations", &[("id", "inst-x"), ("owner", "x")]),
];

/// What PostgreSQL 17.11 returns for `(caller, helper, minimum role)`, the
/// helpers of [`R100`] called over [`ROWS`].
const MEASURED: &[(&str, &str, &str, &[&str])] = &[
    ("alice", "projects_with", "read", &["P", "Q"]),
    ("alice", "teams_with", "read", &["T", "U"]),
    ("alice", "organizations_with", "read", &["org"]),
    ("alice", "projects_with", "write", &["P", "Q"]),
    ("alice", "teams_with", "write", &["T", "U"]),
    ("alice", "organizations_with", "write", &["org"]),
    ("alice", "projects_with", "admin", &["P", "Q"]),
    ("alice", "teams_with", "admin", &["T", "U"]),
    ("alice", "organizations_with", "admin", &["org"]),
    ("olga", "projects_with", "read", &["P", "Q"]),
    ("olga", "teams_with", "read", &["T", "U"]),
    ("olga", "organizations_with", "read", &["org"]),
    ("olga", "projects_with", "write", &[]),
    ("olga", "teams_with", "write", &[]),
    ("olga", "organizations_with", "write", &[]),
    ("olga", "projects_with", "admin", &[]),
    ("olga", "teams_with", "admin", &[]),
    ("olga", "organizations_with", "admin", &[]),
    ("owen", "projects_with", "read", &["P", "Q"]),
    ("owen", "teams_with", "read", &["T", "U"]),
    ("owen", "organizations_with", "read", &["org"]),
    ("owen", "projects_with", "write", &["P", "Q"]),
    ("owen", "teams_with", "write", &["T", "U"]),
    ("owen", "organizations_with", "write", &["org"]),
    ("owen", "projects_with", "admin", &[]),
    ("owen", "teams_with", "admin", &[]),
    ("owen", "organizations_with", "admin", &[]),
    ("tom", "projects_with", "read", &["P"]),
    ("tom", "teams_with", "read", &["T"]),
    ("tom", "organizations_with", "read", &[]),
    ("tom", "projects_with", "write", &["P"]),
    ("tom", "teams_with", "write", &["T"]),
    ("tom", "organizations_with", "write", &[]),
    ("tom", "projects_with", "admin", &["P"]),
    ("tom", "teams_with", "admin", &["T"]),
    ("tom", "organizations_with", "admin", &[]),
    ("bob", "projects_with", "read", &["P"]),
    ("bob", "teams_with", "read", &["T"]),
    ("bob", "organizations_with", "read", &[]),
    ("bob", "projects_with", "write", &["P"]),
    ("bob", "teams_with", "write", &["T"]),
    ("bob", "organizations_with", "write", &[]),
    ("bob", "projects_with", "admin", &[]),
    ("bob", "teams_with", "admin", &[]),
    ("bob", "organizations_with", "admin", &[]),
    ("uma", "projects_with", "read", &["Q"]),
    ("uma", "teams_with", "read", &["U"]),
    ("uma", "organizations_with", "read", &[]),
    ("uma", "projects_with", "write", &[]),
    ("uma", "teams_with", "write", &[]),
    ("uma", "organizations_with", "write", &[]),
    ("uma", "projects_with", "admin", &[]),
    ("uma", "teams_with", "admin", &[]),
    ("uma", "organizations_with", "admin", &[]),
    ("quinn", "projects_with", "read", &["Q"]),
    ("quinn", "teams_with", "read", &[]),
    ("quinn", "organizations_with", "read", &[]),
    ("quinn", "projects_with", "write", &[]),
    ("quinn", "teams_with", "write", &[]),
    ("quinn", "organizations_with", "write", &[]),
    ("quinn", "projects_with", "admin", &[]),
    ("quinn", "teams_with", "admin", &[]),
    ("quinn", "organizations_with", "admin", &[]),
    ("inst-T", "projects_with", "read", &["P"]),
    ("inst-T", "teams_with", "read", &["T"]),
    ("inst-T", "organizations_with", "read", &[]),
    ("inst-T", "projects_with", "write", &["P"]),
    ("inst-T", "teams_with", "write", &["T"]),
    ("inst-T", "organizations_with", "write", &[]),
    ("inst-T", "projects_with", "admin", &[]),
    ("inst-T", "teams_with", "admin", &[]),
    ("inst-T", "organizations_with", "admin", &[]),
    ("zed", "projects_with", "read", &[]),
    ("zed", "teams_with", "read", &[]),
    ("zed", "organizations_with", "read", &[]),
    ("zed", "projects_with", "write", &[]),
    ("zed", "teams_with", "write", &[]),
    ("zed", "organizations_with", "write", &[]),
    ("zed", "projects_with", "admin", &[]),
    ("zed", "teams_with", "admin", &[]),
    ("zed", "organizations_with", "admin", &[]),
];

/// What PostgreSQL 17.11 returns for `authz.installations_administered()`,
/// called by each caller over [`ROWS`].
const INSTALLATIONS_MEASURED: &[(&str, &[&str])] = &[
    ("alice", &["inst-P", "inst-Q", "inst-T", "inst-U", "inst-org"]),
    ("olga", &[]),
    ("owen", &[]),
    ("tom", &["inst-P", "inst-T"]),
    ("bob", &[]),
    ("uma", &[]),
    ("quinn", &[]),
    ("inst-T", &[]),
    ("zed", &[]),
];

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

/// A deployment whose caller is `app.user_id`.
fn deployment() -> Deployment {
    let caller = SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))
        .expect("a custom setting name")
        .may_be_unset();
    Deployment::new("app", ["app"], [caller]).expect("a valid deployment")
}

fn function<'db>(
    database: &'db ParserDB,
    schema: &str,
    name: &str,
) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(
            TargetName::new(name, false).with_schema(schema, false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("function exists")
}

fn text(value: &str) -> Value {
    Value::SingleQuotedString(value.to_string())
}

fn answer<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
    schema: &str,
    name: &str,
    arguments: &[Value],
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    function(database, schema, name).helper_set(arguments, deployment, database)
}

/// An answer as one line, each arm as its table and column followed by what
/// filters it.
fn render(set: &HelperSet<'_, ParserDB>) -> String {
    let arms: Vec<String> = set
        .arms()
        .iter()
        .map(|arm| {
            match arm {
                HelperArm::Grant(grant) => {
                    let restriction = grant.restriction().map_or(String::new(), |set| {
                        let values: Vec<String> =
                            set.values().iter().map(ToString::to_string).collect();
                        format!(" AND {} IN ({})", set.column().column_name(), values.join(", "))
                    });
                    format!(
                        "{}.{} WHERE {} = {}{restriction}",
                        grant.table().table_name(),
                        grant.column().column_name(),
                        grant.caller_column().column_name(),
                        grant.setting().name(),
                    )
                }
                HelperArm::Parent(parent) => {
                    let arguments: Vec<String> =
                        parent.arguments().iter().map(ToString::to_string).collect();
                    let typed = if parent.foreign_key().is_some() { "->" } else { "as" };
                    format!(
                        "{}.{} WHERE {} {typed} {} IN {}({}) {{{}}}",
                        parent.table().table_name(),
                        parent.key().column_name(),
                        parent.column().column_name(),
                        parent.parent_table().table_name(),
                        parent.function().target_name(),
                        arguments.join(", "),
                        render(parent.set()),
                    )
                }
            }
        })
        .collect();
    arms.join(" UNION ")
}

/// The fixture rows of `table`.
fn rows(table: &str) -> impl Iterator<Item = &'static [(&'static str, &'static str)]> + '_ {
    ROWS.iter().filter(move |(name, _)| *name == table).map(|(_, row)| *row)
}

fn cell(row: &[(&str, &'static str)], column: &str) -> &'static str {
    row.iter()
        .find(|(name, _)| *name == column)
        .map(|(_, value)| *value)
        .expect("the column has a value")
}

/// Evaluates an answer over [`ROWS`] for `caller`, the way a translator
/// lowering it to SQL over the backing tables would.
fn evaluate(set: &HelperSet<'_, ParserDB>, caller: &str) -> BTreeSet<&'static str> {
    let mut returned = BTreeSet::new();
    for arm in set.arms() {
        match arm {
            HelperArm::Grant(grant) => {
                let allowed: Option<Vec<String>> = grant.restriction().map(|set| {
                    set.values()
                        .iter()
                        .map(|value| {
                            match value {
                                Value::SingleQuotedString(text) => text.clone(),
                                other => panic!("R100 restricts by text, not {other}"),
                            }
                        })
                        .collect()
                });
                for row in rows(grant.table().table_name()) {
                    let restricted = grant.restriction().is_none_or(|set| {
                        let value = cell(row, set.column().column_name());
                        allowed.as_ref().is_some_and(|allowed| allowed.iter().any(|v| v == value))
                    });
                    if cell(row, grant.caller_column().column_name()) == caller && restricted {
                        returned.insert(cell(row, grant.column().column_name()));
                    }
                }
            }
            HelperArm::Parent(parent) => {
                let parents = evaluate(parent.set(), caller);
                for row in rows(parent.table().table_name()) {
                    if parents.contains(cell(row, parent.column().column_name())) {
                        returned.insert(cell(row, parent.key().column_name()));
                    }
                }
            }
        }
    }
    returned
}

#[test]
fn a_grant_arm_answers_its_caller_column_and_the_roles_its_condition_keeps() {
    let database = postgres(R100);
    let deployment = deployment();
    let cases = [
        ("read", "organization_grants.organization_id WHERE principal_id = app.user_id"),
        (
            "write",
            "organization_grants.organization_id WHERE principal_id = app.user_id AND role IN ('admin', 'write')",
        ),
        (
            "admin",
            "organization_grants.organization_id WHERE principal_id = app.user_id AND role IN ('admin')",
        ),
    ];
    for (role, expected) in cases {
        let set = answer(&database, &deployment, "authz", "organizations_with", &[text(role)])
            .expect("answered");
        assert_eq!(render(&set), expected, "organizations_with('{role}')");
    }
}

#[test]
fn a_parent_arm_answers_the_helper_it_calls_for_the_same_role() {
    let database = postgres(R100);
    let deployment = deployment();
    let set = answer(&database, &deployment, "authz", "projects_with", &[text("write")])
        .expect("answered");
    assert_eq!(
        render(&set),
        "project_grants.project_id WHERE principal_id = app.user_id AND role IN ('admin', 'write') \
         UNION projects.id WHERE team_id -> teams IN authz.teams_with('write') {\
         team_grants.team_id WHERE principal_id = app.user_id AND role IN ('admin', 'write') \
         UNION teams.id WHERE organization_id -> organizations IN authz.organizations_with('write') {\
         organization_grants.organization_id WHERE principal_id = app.user_id AND role IN ('admin', 'write')}}"
    );
    let HelperArm::Parent(parent) = &set.arms()[1] else {
        panic!("the second arm is a parent arm")
    };
    assert_eq!(
        parent
            .foreign_key()
            .expect("typed by a foreign key")
            .referenced_table(&database)
            .expect("resolves")
            .table_name(),
        "teams"
    );
    assert_eq!(parent.parent_table().table_name(), "teams");
    assert_eq!(parent.function().name(), "teams_with");
}

#[test]
fn every_helper_at_every_role_returns_what_postgres_returns_for_every_caller() {
    let database = postgres(R100);
    let deployment = deployment();
    for &(caller, helper, role, expected) in MEASURED {
        let set = answer(&database, &deployment, "authz", helper, &[text(role)]).expect("answered");
        let returned: Vec<_> = evaluate(&set, caller).into_iter().collect();
        assert_eq!(returned, expected, "{caller} calling {helper}('{role}')");
    }
}

/// The name of the table `set` covers.
fn covered<'db>(set: &HelperSet<'db, ParserDB>) -> Option<&'db str> {
    set.covered_table().map(TableLike::table_name)
}

#[test]
fn a_set_covers_the_one_table_whose_primary_key_every_arm_returns() {
    let database = postgres(R100);
    let deployment = deployment();
    for (helper, table) in [
        ("organizations_with", "organizations"),
        ("teams_with", "teams"),
        ("projects_with", "projects"),
    ] {
        for role in ["read", "write", "admin"] {
            let set =
                answer(&database, &deployment, "authz", helper, &[text(role)]).expect("answered");
            assert_eq!(covered(&set), Some(table), "{helper}('{role}')");
        }
    }
    let set = answer(&database, &deployment, "authz", "installations_administered", &[])
        .expect("answered");
    assert_eq!(covered(&set), Some("connetto_installations"));

    let none = format!("{GRANT_ARM} AND min_role = 'any'");
    let database = postgres(&helper(DEFINER, &none, ""));
    let set =
        answer(&database, &deployment, "public", "docs_with", &[text("none")]).expect("answered");
    assert!(set.arms().is_empty());
    assert_eq!(covered(&set), Some("docs"), "an arm keeping no row still returns keys of docs");

    let database = postgres(MEMBERSHIP);
    let set = answer(&database, &deployment, "public", "docs_for", &[]).expect("answered");
    assert_eq!(covered(&set), None, "members.doc_id references nothing");

    for (body, after) in [
        (
            format!(
                "{GRANT_ARM} UNION SELECT folder_id FROM folder_members \
                 WHERE user_id = current_setting('app.user_id', true) AND role = min_role"
            ),
            "CREATE TABLE folders (id TEXT PRIMARY KEY);
             CREATE TABLE folder_members (folder_id TEXT REFERENCES folders (id), user_id TEXT, role TEXT);
             GRANT SELECT ON folders, folder_members TO owner;",
        ),
        (
            "SELECT slug FROM slug_members \
             WHERE user_id = current_setting('app.user_id', true) AND role = min_role"
                .to_string(),
            "CREATE TABLE slugs (id TEXT PRIMARY KEY, slug TEXT UNIQUE);
             CREATE TABLE slug_members (slug TEXT REFERENCES slugs (slug), user_id TEXT, role TEXT);
             GRANT SELECT ON slugs, slug_members TO owner;",
        ),
        (
            "SELECT code FROM code_members \
             WHERE user_id = current_setting('app.user_id', true) AND role = min_role"
                .to_string(),
            "CREATE TABLE codes (code TEXT UNIQUE);
             CREATE TABLE code_members (code TEXT REFERENCES codes (code), user_id TEXT, role TEXT);
             GRANT SELECT ON codes, code_members TO owner;",
        ),
    ] {
        let database = postgres(&helper(DEFINER, &body, after));
        let set = answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .expect("answered");
        assert_eq!(covered(&set), None, "{body}");
    }
}

#[test]
fn a_column_without_a_foreign_key_is_typed_by_the_table_the_called_set_covers() {
    let database = postgres(R100);
    let deployment = deployment();
    let set = answer(&database, &deployment, "authz", "installations_administered", &[])
        .expect("answered");
    let parents: Vec<_> = set
        .arms()
        .iter()
        .map(|arm| {
            let HelperArm::Parent(parent) = arm else { panic!("a parent arm") };
            assert!(parent.foreign_key().is_none(), "owner references nothing");
            (
                parent.table().table_name(),
                parent.key().column_name(),
                parent.column().column_name(),
                parent.parent_table().table_name(),
                parent.function().name(),
            )
        })
        .collect();
    assert_eq!(
        parents,
        [
            ("connetto_installations", "id", "owner", "organizations", "organizations_with"),
            ("connetto_installations", "id", "owner", "teams", "teams_with"),
            ("connetto_installations", "id", "owner", "projects", "projects_with"),
        ]
    );
    for &(caller, expected) in INSTALLATIONS_MEASURED {
        let returned: Vec<_> = evaluate(&set, caller).into_iter().collect();
        assert_eq!(returned, expected, "{caller} calling installations_administered()");
    }
}

/// `notes_with`, whose one arm compares `notes.target`, a column with no
/// foreign key, to `docs_with`, its body `body`.
fn notes(body: &str, arm: &str) -> Result<String, HelperSetError> {
    let after = format!(
        "CREATE TABLE folders (id TEXT PRIMARY KEY);
         CREATE TABLE folder_members (folder_id TEXT REFERENCES folders (id), user_id TEXT, role TEXT);
         CREATE TABLE notes (id TEXT PRIMARY KEY, target TEXT);
         CREATE FUNCTION notes_with(min_role TEXT) RETURNS SETOF TEXT {DEFINER} AS $$ {arm} $$;
         ALTER FUNCTION notes_with(TEXT) OWNER TO owner;
         GRANT SELECT ON folders, folder_members, notes TO owner;"
    );
    let database = postgres(&helper(DEFINER, body, &after));
    let deployment = deployment();
    answer(&database, &deployment, "public", "notes_with", &[text("read")]).map(|set| render(&set))
}

#[test]
fn a_column_without_a_foreign_key_compared_to_a_set_covering_no_table_refuses_the_answer() {
    let typed = "SELECT id FROM notes WHERE target IN (SELECT docs_with(min_role))";
    assert_eq!(
        notes(GRANT_ARM, typed).expect("answered"),
        "notes.id WHERE target as docs IN docs_with('read') \
         {members.doc_id WHERE user_id = app.user_id AND role IN ('read')}"
    );
    let mixed = format!(
        "{GRANT_ARM} UNION SELECT folder_id FROM folder_members \
         WHERE user_id = current_setting('app.user_id', true) AND role = min_role"
    );
    assert!(matches!(
        notes(&mixed, typed),
        Err(HelperSetError::UncoveredParent { function, column, called })
            if function == "notes_with" && column == "target" && called == "docs_with"
    ));
    assert!(
        matches!(
            notes(GRANT_ARM, "SELECT id FROM notes WHERE id IN (SELECT docs_with(min_role))"),
            Err(HelperSetError::ArmShape { .. })
        ),
        "the key the arm returns is no parent"
    );
}

#[test]
fn a_condition_reducing_to_false_contributes_no_arm() {
    let database = postgres(R100);
    let deployment = deployment();
    let set = answer(&database, &deployment, "authz", "organizations_with", &[text("owner")])
        .expect("answered");
    assert_eq!(
        render(&set),
        "organization_grants.organization_id WHERE principal_id = app.user_id AND role IN ('admin')"
    );

    let database = postgres(
        "CREATE ROLE owner NOLOGIN BYPASSRLS;
         CREATE TABLE members (doc_id TEXT, user_id TEXT);
         CREATE FUNCTION docs_for(wanted TEXT) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
           SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id') AND wanted = 'any'
         $$;
         ALTER FUNCTION docs_for(TEXT) OWNER TO owner;
         GRANT SELECT ON members TO owner;",
    );
    let set =
        answer(&database, &deployment, "public", "docs_for", &[text("none")]).expect("answered");
    assert!(set.arms().is_empty());
    let set =
        answer(&database, &deployment, "public", "docs_for", &[Value::Null]).expect("answered");
    assert!(set.arms().is_empty(), "a NULL comparison keeps no row");
}

/// A membership table with no role, its helper reading positional parameters
/// and a bare caller setting.
const MEMBERSHIP: &str = "
CREATE ROLE owner NOLOGIN BYPASSRLS;
CREATE TABLE members (doc_id TEXT, user_id TEXT, kind TEXT);
CREATE FUNCTION docs_for() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
  SELECT doc_id FROM members WHERE current_setting('app.user_id') = user_id
$$;
CREATE FUNCTION docs_of_kind(TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
  SELECT m.doc_id FROM members AS m WHERE m.user_id = current_setting('app.user_id', true) AND m.kind = $1
$$;
ALTER FUNCTION docs_for() OWNER TO owner;
ALTER FUNCTION docs_of_kind(TEXT) OWNER TO owner;
GRANT SELECT ON members TO owner;
";

#[test]
fn a_role_less_membership_table_answers_every_grant_row_of_the_caller() {
    let database = postgres(MEMBERSHIP);
    let deployment = deployment();
    let set = answer(&database, &deployment, "public", "docs_for", &[]).expect("answered");
    assert_eq!(render(&set), "members.doc_id WHERE user_id = app.user_id");
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    assert!(
        grant.raises_when_unset(),
        "current_setting without missing_ok raises on an unset setting"
    );

    let set = answer(&database, &deployment, "public", "docs_of_kind", &[text("note")])
        .expect("answered");
    assert_eq!(render(&set), "members.doc_id WHERE user_id = app.user_id AND kind IN ('note')");
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    assert!(!grant.raises_when_unset());
}

#[test]
fn a_column_named_like_a_parameter_is_the_column() {
    let database = postgres(
        "CREATE ROLE owner NOLOGIN BYPASSRLS;
         CREATE TABLE members (doc_id TEXT, user_id TEXT, kind TEXT);
         CREATE FUNCTION docs_for(kind TEXT) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
           SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id') AND kind = 'note'
         $$;
         ALTER FUNCTION docs_for(TEXT) OWNER TO owner;
         GRANT SELECT ON members TO owner;",
    );
    let deployment = deployment();
    let set =
        answer(&database, &deployment, "public", "docs_for", &[text("other")]).expect("answered");
    assert_eq!(render(&set), "members.doc_id WHERE user_id = app.user_id AND kind IN ('note')");
}

/// One R100-shaped helper over `members`, its owner `owner`, with `header`
/// declaring the function up to its body and `body` its query.
fn helper(header: &str, body: &str, after: &str) -> String {
    format!(
        "CREATE ROLE owner NOLOGIN BYPASSRLS;
         CREATE TABLE docs (id TEXT PRIMARY KEY, parent_id TEXT REFERENCES docs (id));
         CREATE TABLE members (doc_id TEXT REFERENCES docs (id), user_id TEXT, role TEXT);
         CREATE FUNCTION docs_with(min_role TEXT) RETURNS SETOF TEXT
         {header} AS $$ {body} $$;
         ALTER FUNCTION docs_with(TEXT) OWNER TO owner;
         GRANT SELECT ON docs, members TO owner;
         {after}"
    )
}

const DEFINER: &str = "LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public";
const GRANT_ARM: &str = "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) AND role = min_role";

fn refusal(sql: &str) -> HelperSetError {
    refusal_with(sql, &deployment())
}

fn refusal_with(sql: &str, deployment: &Deployment) -> HelperSetError {
    let database = postgres(sql);
    answer(&database, deployment, "public", "docs_with", &[text("read")]).expect_err("refused")
}

#[test]
fn the_shape_of_the_declaration_is_checked_before_the_body() {
    let header = |header: &str| refusal(&helper(header, GRANT_ARM, ""));
    assert!(matches!(
        header("LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = public"),
        HelperSetError::Language { language: Some(language), .. } if language == "plpgsql"
    ));
    assert!(matches!(
        header("LANGUAGE sql SECURITY DEFINER SET search_path = public"),
        HelperSetError::Volatility { volatility: sqlparser::ast::FunctionBehavior::Volatile, .. }
    ));
    assert!(matches!(
        header("LANGUAGE sql IMMUTABLE SECURITY DEFINER SET search_path = public"),
        HelperSetError::Volatility { volatility: sqlparser::ast::FunctionBehavior::Immutable, .. }
    ));
    assert!(matches!(
        header("LANGUAGE sql STABLE SET search_path = public"),
        HelperSetError::SecurityInvoker { function } if function == "docs_with"
    ));
    assert!(matches!(
        header("LANGUAGE sql STABLE SECURITY DEFINER"),
        HelperSetError::UnpinnedSearchPath { .. }
    ));

    let scalar = helper(DEFINER, GRANT_ARM, "").replace("RETURNS SETOF TEXT", "RETURNS TEXT");
    assert!(matches!(refusal(&scalar), HelperSetError::ReturnType { .. }));

    let database = postgres(&helper(DEFINER, GRANT_ARM, ""));
    let deployment = deployment();
    assert!(matches!(
        answer(&database, &deployment, "public", "docs_with", &[]),
        Err(HelperSetError::ArgumentCount { expected: 1, given: 0, .. })
    ));
}

#[test]
fn an_owner_the_schema_cannot_settle_refuses_the_answer() {
    let migrated = helper(DEFINER, GRANT_ARM, "")
        .replace("CREATE ROLE owner NOLOGIN BYPASSRLS;", "")
        .replace("ALTER FUNCTION docs_with(TEXT) OWNER TO owner;", "")
        .replace("GRANT SELECT ON docs, members TO owner;", "");
    let database = ParseOptions::default()
        .with_session_role("migrator")
        .parse::<PostgreSqlDialect>(&migrated)
        .expect("schema parses");
    let deployment = deployment();
    assert!(matches!(
        answer(&database, &deployment, "public", "docs_with", &[text("read")]),
        Err(HelperSetError::UndeclaredOwner { owner, .. }) if owner == "migrator"
    ));
    let unowned = helper(DEFINER, GRANT_ARM, "")
        .replace("ALTER FUNCTION docs_with(TEXT) OWNER TO owner;", "");
    assert!(matches!(refusal(&unowned), HelperSetError::UnsettledOwner { .. }));
}

#[test]
fn a_superuser_owner_reads_every_table_whatever_it_was_granted() {
    let sql = helper(
        DEFINER,
        GRANT_ARM,
        "ALTER TABLE members ENABLE ROW LEVEL SECURITY;
         ALTER TABLE members FORCE ROW LEVEL SECURITY;",
    )
    .replace("CREATE ROLE owner NOLOGIN BYPASSRLS;", "CREATE ROLE owner SUPERUSER;")
    .replace("GRANT SELECT ON docs, members TO owner;", "");
    let database = postgres(&sql);
    let deployment = deployment();
    let set = answer(&database, &deployment, "public", "docs_with", &[text("read")])
        .expect("a superuser passes every privilege check and every policy");
    assert_eq!(render(&set), "members.doc_id WHERE user_id = app.user_id AND role IN ('read')");
}

#[test]
fn a_table_whose_policies_bind_the_owner_refuses_the_answer() {
    let sql = helper(DEFINER, GRANT_ARM, "ALTER TABLE members ENABLE ROW LEVEL SECURITY;")
        .replace("CREATE ROLE owner NOLOGIN BYPASSRLS;", "CREATE ROLE owner NOLOGIN;");
    let owner_owns_it = format!("{sql} ALTER TABLE members OWNER TO owner;");
    let database = postgres(&owner_owns_it);
    let deployment = deployment();
    answer(&database, &deployment, "public", "docs_with", &[text("read")])
        .expect("an owner not forced past its own table reads it");

    let forced = format!("{owner_owns_it} ALTER TABLE members FORCE ROW LEVEL SECURITY;");
    assert!(matches!(
        refusal(&forced),
        HelperSetError::NotExempt { table, owner, .. } if table == "members" && owner == "owner"
    ));
    assert!(matches!(
        refusal(&sql),
        HelperSetError::NotExempt { table, .. } if table == "members"
    ));
}

#[test]
fn an_owner_lacking_a_privilege_the_body_needs_refuses_the_answer() {
    let sql = helper(DEFINER, GRANT_ARM, "")
        .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON docs TO owner;");
    assert!(matches!(
        refusal(&sql),
        HelperSetError::MissingSelect { table, owner, .. } if table == "members" && owner == "owner"
    ));

    let calls = helper(
        DEFINER,
        "SELECT id FROM docs WHERE parent_id IN (SELECT other.docs_with(min_role))",
        "CREATE ROLE stranger NOLOGIN BYPASSRLS;
         CREATE SCHEMA other;
         CREATE FUNCTION other.docs_with(min_role TEXT) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
           SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true)
         $$;
         ALTER FUNCTION other.docs_with(TEXT) OWNER TO stranger;
         GRANT SELECT ON members TO stranger;
         REVOKE EXECUTE ON FUNCTION other.docs_with(TEXT) FROM PUBLIC;
         GRANT USAGE ON SCHEMA other TO owner;",
    );
    assert!(matches!(
        refusal(&calls),
        HelperSetError::MissingExecute { called, owner, .. } if called == "other.docs_with" && owner == "owner"
    ));
}

#[test]
fn a_caller_must_come_from_a_declared_caller_setting() {
    let parameter = helper(DEFINER, "SELECT doc_id FROM members WHERE user_id = min_role", "");
    assert!(matches!(
        refusal(&parameter),
        HelperSetError::CallerFromParameter { parameter, .. } if parameter == "min_role"
    ));
    let undeclared = helper(DEFINER, &GRANT_ARM.replace("app.user_id", "app.tenant"), "");
    assert!(matches!(
        refusal(&undeclared),
        HelperSetError::UndeclaredSetting { setting, .. } if setting == "app.tenant"
    ));
    let scalar = Deployment::new(
        "app",
        ["app"],
        [SessionSetting::value("app.user_id", SettingValue::Scalar(DataType::Text))
            .expect("valid")],
    )
    .expect("valid");
    assert!(matches!(
        refusal_with(&helper(DEFINER, GRANT_ARM, ""), &scalar),
        HelperSetError::SettingNotCaller { setting, .. } if setting == "app.user_id"
    ));
}

#[test]
fn a_condition_over_two_columns_or_a_negation_is_irreducible() {
    for condition in [
        "(role = 'admin' OR doc_id = 'd')",
        "NOT role = min_role",
        "role <> min_role",
        "role LIKE min_role",
    ] {
        let body = format!(
            "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) AND {condition}"
        );
        assert!(
            matches!(refusal(&helper(DEFINER, &body, "")), HelperSetError::Irreducible { .. }),
            "{condition}"
        );
    }
}

#[test]
fn an_arm_of_any_other_shape_refuses_the_answer() {
    let database = postgres(R100);
    let deployment = deployment();
    assert!(matches!(
        answer(&database, &deployment, "authz", "owners_with", &[text("read")]),
        Err(HelperSetError::ArmShape { function, .. }) if function == "authz.owners_with"
    ));
    for body in [
        "SELECT doc_id FROM members JOIN docs ON docs.id = members.doc_id WHERE user_id = current_setting('app.user_id', true)",
        "SELECT doc_id FROM members WHERE role = 'admin'",
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) GROUP BY doc_id",
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) INTERSECT SELECT id FROM docs",
        "SELECT parent_id FROM docs WHERE id IN (SELECT docs_with(min_role))",
    ] {
        assert!(
            matches!(refusal(&helper(DEFINER, body, "")), HelperSetError::ArmShape { .. }),
            "{body}"
        );
    }
}

#[test]
fn a_helper_reaching_itself_refuses_the_answer() {
    let sql = helper(
        DEFINER,
        &format!(
            "{GRANT_ARM} UNION SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(min_role))"
        ),
        "",
    );
    assert!(matches!(
        refusal(&sql),
        HelperSetError::CallCycle { function } if function == "docs_with"
    ));
}

/// The helper `docs_with(<parameters>)` over the fixture of [`helper`], its
/// body `body`.
fn helper_taking(parameters: &str, types: &str, body: &str, after: &str) -> String {
    helper(DEFINER, body, after)
        .replace("docs_with(min_role TEXT)", &format!("docs_with({parameters})"))
        .replace("docs_with(TEXT)", &format!("docs_with({types})"))
}

/// What `docs_with` answers for `arguments`, rendered, with `condition`
/// filtering the grant arm after the caller.
fn kept(
    parameters: &str,
    types: &str,
    condition: &str,
    arguments: &[Value],
) -> Result<String, HelperSetError> {
    let body = format!(
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) AND ({condition})"
    );
    let database = postgres(&helper_taking(parameters, types, &body, ""));
    let deployment = deployment();
    answer(&database, &deployment, "public", "docs_with", arguments).map(|set| render(&set))
}

const ROLES: &str = "members.doc_id WHERE user_id = app.user_id";

fn number(value: &str) -> Value {
    Value::Number(value.to_string(), false)
}

#[test]
fn an_in_list_keeps_the_literals_it_lists_and_decides_one_over_parameters() {
    let text_kept =
        |condition: &str, argument: Value| kept("min_role TEXT", "TEXT", condition, &[argument]);
    assert_eq!(
        text_kept("role IN ('admin', min_role)", text("write")).expect("answered"),
        format!("{ROLES} AND role IN ('admin', 'write')")
    );
    assert_eq!(
        text_kept("role IN ('admin', NULL, min_role)", Value::Null).expect("answered"),
        format!("{ROLES} AND role IN ('admin')"),
        "a NULL in the list matches no row"
    );
    assert_eq!(
        text_kept("(role = 'admin' OR role IN ('admin', min_role))", text("admin"))
            .expect("answered"),
        format!("{ROLES} AND role IN ('admin')"),
        "a value the union already holds is kept once"
    );
    let parenthesised = helper(DEFINER, &GRANT_ARM.replace("SELECT doc_id", "SELECT (doc_id)"), "");
    let database = postgres(&parenthesised);
    let deployment = deployment();
    let set =
        answer(&database, &deployment, "public", "docs_with", &[text("read")]).expect("answered");
    assert_eq!(render(&set), format!("{ROLES} AND role IN ('read')"));
    assert_eq!(text_kept("min_role IN ('read', 'write')", text("read")).expect("answered"), ROLES);
    assert_eq!(text_kept("min_role IN ('read', 'write')", text("admin")).expect("answered"), "");
    assert_eq!(text_kept("min_role IN ('read', NULL)", text("admin")).expect("answered"), "");
    assert_eq!(text_kept("min_role IN ('read')", Value::Null).expect("answered"), "");
    for irreducible in [
        "role IN ('admin', doc_id)",
        "upper(role) IN ('ADMIN')",
        "min_role IN (1, 2)",
        "(role IN ('admin') OR doc_id IN ('d'))",
    ] {
        assert!(
            matches!(text_kept(irreducible, text("read")), Err(HelperSetError::Irreducible { .. })),
            "{irreducible}"
        );
    }
}

#[test]
fn two_conditions_on_one_column_keep_the_values_both_allow() {
    let both = |argument: &str| {
        kept(
            "min_role TEXT",
            "TEXT",
            "role IN ('admin', 'write') AND role = min_role",
            &[text(argument)],
        )
    };
    assert_eq!(both("write").expect("answered"), format!("{ROLES} AND role IN ('write')"));
    assert_eq!(both("read").expect("answered"), "", "no value is allowed by both");
    assert!(matches!(
        kept("min_role TEXT", "TEXT", "role = min_role AND doc_id = 'd'", &[text("read")]),
        Err(HelperSetError::Irreducible { .. })
    ));
}

#[test]
fn a_boolean_or_null_condition_keeps_every_row_or_none() {
    let flag =
        |condition: &str, argument: Value| kept("wide BOOLEAN", "BOOLEAN", condition, &[argument]);
    assert_eq!(flag("wide", Value::Boolean(true)).expect("answered"), ROLES);
    assert_eq!(flag("wide", Value::Boolean(false)).expect("answered"), "");
    assert_eq!(flag("wide", Value::Null).expect("answered"), "");
    assert_eq!(
        flag("(wide = TRUE OR role = 'admin')", Value::Boolean(true)).expect("answered"),
        ROLES
    );
    assert_eq!(flag("NULL", Value::Boolean(true)).expect("answered"), "");
    for irreducible in ["role", "'yes'", "wide = 'yes'"] {
        assert!(
            matches!(
                flag(irreducible, Value::Boolean(true)),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{irreducible}"
        );
    }
}

#[test]
fn numbers_compare_as_integers_and_refuse_what_needs_a_type() {
    let level =
        |condition: &str, argument: &str| kept("level INT", "INT", condition, &[number(argument)]);
    assert_eq!(level("level = 2", "2").expect("answered"), ROLES);
    assert_eq!(level("level = 02", "2").expect("answered"), ROLES, "02 and 2 are one integer");
    assert_eq!(level("level = 2", "3").expect("answered"), "");
    for irreducible in ["level = 2.0", "level = '2'"] {
        assert!(
            matches!(level(irreducible, "2"), Err(HelperSetError::Irreducible { .. })),
            "{irreducible}"
        );
    }
}

#[test]
fn literals_columns_and_parameters_are_read_in_every_spelling() {
    let text_kept = |condition: &str| kept("min_role TEXT", "TEXT", condition, &[text("write")]);
    assert_eq!(
        text_kept("role = $q$admin$q$").expect("answered"),
        format!("{ROLES} AND role IN ($q$admin$q$)")
    );
    assert_eq!(
        text_kept("role = docs_with.min_role").expect("answered"),
        format!("{ROLES} AND role IN ('write')")
    );
    assert_eq!(
        text_kept("(role) = (min_role)").expect("answered"),
        format!("{ROLES} AND role IN ('write')")
    );
    assert_eq!(
        kept("TEXT", "TEXT", "role = $1", &[text("write")]).expect("answered"),
        format!("{ROLES} AND role IN ('write')")
    );
    for irreducible in [
        "other.role = min_role",
        "role = doc_id",
        "(role = 'a' AND role LIKE 'b') OR role = 'c'",
        "role LIKE 'b' OR role = 'c'",
    ] {
        assert!(
            matches!(text_kept(irreducible), Err(HelperSetError::Irreducible { .. })),
            "{irreducible}"
        );
    }
}

#[test]
fn only_postgres_own_current_setting_with_a_literal_name_reads_the_caller() {
    let caller = |read: &str, after: &str| {
        let body = format!("SELECT doc_id FROM members WHERE user_id = {read}");
        let database = postgres(&helper(DEFINER, &body, after));
        let deployment = deployment();
        answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .map(|set| render(&set))
    };
    assert_eq!(
        caller("pg_catalog.current_setting('app.user_id', true)", "").expect("answered"),
        ROLES
    );
    assert_eq!(caller("current_setting($s$app.user_id$s$)", "").expect("answered"), ROLES);
    let shadow = "CREATE FUNCTION current_setting(name TEXT, missing BOOLEAN) RETURNS TEXT
                  LANGUAGE sql AS $$ SELECT 'x' $$;";
    for read in [
        "other.current_setting('app.user_id', true)",
        "current_setting('app.user_id', 'yes')",
        "current_setting(min_role, true)",
        "current_setting(1)",
        "current_setting('app.user_id', true, true)",
        "current_setting(DISTINCT 'app.user_id')",
    ] {
        assert!(matches!(caller(read, ""), Err(HelperSetError::ArmShape { .. })), "{read}");
    }
    assert!(
        matches!(
            caller("current_setting('app.user_id', true)", shadow),
            Err(HelperSetError::ArmShape { .. })
        ),
        "a declared current_setting the call may reach is not PostgreSQL's"
    );
    assert!(matches!(
        caller(
            "current_setting('app.user_id', true) AND doc_id = current_setting('app.user_id', true)",
            ""
        ),
        Err(HelperSetError::ArmShape { .. })
    ));
    assert!(matches!(
        caller("doc_id AND role = 'admin'", ""),
        Err(HelperSetError::ArmShape { .. })
    ));
}

#[test]
fn a_body_or_arm_outside_the_two_shapes_refuses_the_answer() {
    let shape = |sql: &str| matches!(refusal(sql), HelperSetError::ArmShape { .. });
    let atomic = helper(DEFINER, GRANT_ARM, "")
        .replace(&format!("AS $$ {GRANT_ARM} $$"), &format!("BEGIN ATOMIC {GRANT_ARM}; END"));
    assert!(shape(&atomic), "a body bound at creation");
    let returned =
        helper(DEFINER, GRANT_ARM, "").replace(&format!("AS $$ {GRANT_ARM} $$"), "RETURN 'd'");
    assert!(shape(&returned), "a RETURN body, bound at creation too");
    assert!(shape(&helper(DEFINER, &format!("{GRANT_ARM}; {GRANT_ARM}"), "")));
    let view =
        "CREATE VIEW member_view AS SELECT * FROM members; GRANT SELECT ON member_view TO owner;";
    for body in [
        format!("WITH g AS (SELECT 1) {GRANT_ARM}"),
        GRANT_ARM.replace("SELECT doc_id", "SELECT *"),
        GRANT_ARM.replace("SELECT doc_id", "SELECT 'd'"),
        "SELECT doc_id FROM members WHERE role IN ('admin')".into(),
        GRANT_ARM.replace("FROM members", "FROM ONLY members"),
        GRANT_ARM.replace("FROM members", "FROM members AS m (a, b, c)"),
        GRANT_ARM.replace("FROM members", "FROM member_view"),
        "SELECT id FROM docs WHERE upper(parent_id) IN (SELECT docs_with(min_role))".into(),
        "SELECT id FROM docs WHERE id IN (SELECT docs_with(min_role))".into(),
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(min_role) FROM docs)".into(),
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(min_role) AS x)".into(),
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(DISTINCT min_role))".into(),
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(min_role => min_role))".into(),
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(upper(min_role)))".into(),
    ] {
        assert!(shape(&helper(DEFINER, &body, view)), "{body}");
    }
    let parenthesised = format!("({GRANT_ARM}) UNION ({GRANT_ARM})");
    let database = postgres(&helper(DEFINER, &parenthesised, ""));
    let deployment = deployment();
    let set =
        answer(&database, &deployment, "public", "docs_with", &[text("read")]).expect("answered");
    assert_eq!(set.arms().len(), 2);
}

/// A tree of folders under teams, each folder's helper calling `teams_with`.
fn folders(call: &str, after: &str) -> String {
    format!(
        "CREATE ROLE owner NOLOGIN BYPASSRLS;
         CREATE TABLE teams (id TEXT PRIMARY KEY);
         CREATE TABLE team_members (team_id TEXT REFERENCES teams (id), user_id TEXT, role TEXT);
         CREATE TABLE folders (
           id TEXT PRIMARY KEY,
           parent_id TEXT REFERENCES folders (id),
           team_id TEXT REFERENCES teams (id)
         );
         CREATE FUNCTION teams_with(min_role TEXT) RETURNS SETOF TEXT
         {DEFINER} AS $$
           SELECT team_id FROM team_members
           WHERE user_id = current_setting('app.user_id', true) AND role = min_role
         $$;
         CREATE FUNCTION folders_with(TEXT) RETURNS SETOF TEXT
         {DEFINER} AS $$ SELECT id FROM folders WHERE team_id IN (SELECT {call}) $$;
         ALTER FUNCTION teams_with(TEXT) OWNER TO owner;
         ALTER FUNCTION folders_with(TEXT) OWNER TO owner;
         GRANT SELECT ON teams, team_members, folders TO owner;
         {after}"
    )
}

fn folders_with(sql: &str, argument: Value) -> Result<String, HelperSetError> {
    let database = postgres(sql);
    let deployment = deployment();
    answer(&database, &deployment, "public", "folders_with", &[argument]).map(|set| render(&set))
}

#[test]
fn a_parent_arm_passes_positional_parameters_and_literals_through() {
    let team = "team_members.team_id WHERE user_id = app.user_id AND role IN";
    assert_eq!(
        folders_with(&folders("teams_with($1)", ""), text("write")).expect("answered"),
        format!("folders.id WHERE team_id -> teams IN teams_with('write') {{{team} ('write')}}"),
        "the foreign key on parent_id is not the one the arm follows"
    );
    assert_eq!(
        folders_with(&folders("teams_with('admin')", ""), text("write")).expect("answered"),
        format!("folders.id WHERE team_id -> teams IN teams_with('admin') {{{team} ('admin')}}")
    );
}

#[test]
fn a_call_reaching_no_single_helper_refuses_the_answer() {
    assert!(matches!(
        folders_with(&folders("missing_with($1)", ""), text("read")),
        Err(HelperSetError::UnresolvedCall { call, .. }) if call == "missing_with"
    ));
    let overloaded = "CREATE FUNCTION teams_with(min_level INT) RETURNS SETOF TEXT
                      LANGUAGE sql STABLE AS $$ SELECT 'x' $$;";
    assert!(matches!(
        folders_with(&folders("teams_with($1)", overloaded), text("read")),
        Err(HelperSetError::UnresolvedCall { call, .. }) if call == "teams_with"
    ));
}

#[test]
fn an_argument_or_return_type_the_reader_cannot_compare_refuses_the_answer() {
    assert!(matches!(
        folders_with(&folders("teams_with($1)", ""), Value::Placeholder("$1".into())),
        Err(HelperSetError::UnsupportedArgument { argument, .. }) if argument == "$1"
    ));
    let record =
        helper(DEFINER, GRANT_ARM, "").replace("RETURNS SETOF TEXT", "RETURNS SETOF record");
    assert!(matches!(refusal(&record), HelperSetError::ReturnType { .. }));
}

/// [`helper`] with `members` moved into schema `grants`, `header` the
/// helper's options, `table` the name its body reads `members` by, and
/// `after` the statements after it.
fn in_grants(header: &str, table: &str, after: &str) -> String {
    let body = GRANT_ARM.replace("FROM members", &format!("FROM {table}"));
    helper(header, &body, after)
        .replace("CREATE TABLE members (", "CREATE SCHEMA grants; CREATE TABLE grants.members (")
        .replace(
            "GRANT SELECT ON docs, members TO owner;",
            "GRANT SELECT ON docs, grants.members TO owner;",
        )
}

fn answered(sql: &str) -> String {
    let database = postgres(sql);
    let deployment = deployment();
    render(
        &answer(&database, &deployment, "public", "docs_with", &[text("read")]).expect("answered"),
    )
}

#[test]
fn an_owner_lacking_usage_on_the_schema_of_a_table_it_reads_refuses_the_answer() {
    let missing = |sql: &str| {
        matches!(
            refusal(sql),
            HelperSetError::MissingUsage { schema, owner, .. } if schema == "grants" && owner == "owner"
        )
    };
    assert!(missing(&in_grants(DEFINER, "grants.members", "")));
    let reached_on_path = DEFINER.replace("search_path = public", "search_path = grants");
    assert!(
        missing(&in_grants(&reached_on_path, "members", "")),
        "an unqualified name on the path"
    );
    let granted = "GRANT USAGE ON SCHEMA grants TO owner;";
    assert_eq!(
        answered(&in_grants(DEFINER, "grants.members", granted)),
        "members.doc_id WHERE user_id = app.user_id AND role IN ('read')"
    );
    let owned = in_grants(DEFINER, "grants.members", "")
        .replace("CREATE SCHEMA grants;", "CREATE SCHEMA grants AUTHORIZATION owner;");
    answered(&owned);
}

#[test]
fn a_path_schema_the_owner_cannot_use_is_skipped_as_postgres_skips_it() {
    let path = DEFINER.replace("search_path = public", "search_path = grants, public");
    let both = in_grants(&path, "members", "")
        .replace(
            "CREATE SCHEMA grants;",
            "CREATE SCHEMA grants; CREATE TABLE members (doc_id TEXT, user_id TEXT, role TEXT);",
        )
        .replace(
            "GRANT SELECT ON docs, grants.members",
            "GRANT SELECT ON docs, members, grants.members",
        );
    let database = postgres(&both);
    let deployment = deployment();
    let set =
        answer(&database, &deployment, "public", "docs_with", &[text("read")]).expect("answered");
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    assert_eq!(grant.table().table_schema(), None, "public.members, past grants.members");

    let database = postgres(&format!("{both} GRANT USAGE ON SCHEMA grants TO owner;"));
    let set =
        answer(&database, &deployment, "public", "docs_with", &[text("read")]).expect("answered");
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    assert_eq!(grant.table().table_schema(), Some("grants"));

    let nowhere = helper(DEFINER, &GRANT_ARM.replace("FROM members", "FROM nowhere"), "");
    assert!(matches!(refusal(&nowhere), HelperSetError::ArmShape { .. }));
}

#[test]
fn revoking_publics_usage_on_public_refuses_a_helper_reading_it() {
    let revoked = helper(DEFINER, GRANT_ARM, "REVOKE USAGE ON SCHEMA public FROM PUBLIC;");
    assert!(matches!(
        refusal(&revoked),
        HelperSetError::MissingUsage { schema, .. } if schema == "public"
    ));
    answered(&format!("{revoked} GRANT USAGE ON SCHEMA public TO owner;"));
}

#[test]
fn an_owner_lacking_usage_on_the_schema_of_a_helper_it_calls_refuses_the_answer() {
    let called = |call: &str, path: &str, after: &str| {
        let sql = folders(call, after)
            .replace(
                "CREATE FUNCTION teams_with(",
                "CREATE SCHEMA authz; CREATE FUNCTION authz.teams_with(",
            )
            .replace("ALTER FUNCTION teams_with(", "ALTER FUNCTION authz.teams_with(")
            .replacen(
                "SET search_path = public AS $$ SELECT id",
                &format!("SET search_path = {path} AS $$ SELECT id"),
                1,
            );
        folders_with(&sql, text("read"))
    };
    assert!(matches!(
        called("authz.teams_with($1)", "public", ""),
        Err(HelperSetError::MissingUsage { schema, .. }) if schema == "authz"
    ));
    assert!(matches!(
        called("teams_with($1)", "authz, public", ""),
        Err(HelperSetError::MissingUsage { schema, .. }) if schema == "authz"
    ));
    called("authz.teams_with($1)", "public", "GRANT USAGE ON SCHEMA authz TO owner;")
        .expect("answered");
}

#[test]
fn select_on_every_column_an_arm_reads_is_enough() {
    let columns = |granted: &str| {
        helper(DEFINER, GRANT_ARM, &format!("GRANT SELECT ({granted}) ON members TO owner;"))
            .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON docs TO owner;")
    };
    assert_eq!(
        answered(&columns("doc_id, user_id, role")),
        "members.doc_id WHERE user_id = app.user_id AND role IN ('read')"
    );
    let twice = helper(
        DEFINER,
        &GRANT_ARM.replace("role = min_role", "(role = min_role OR members.role = 'admin')"),
        "GRANT SELECT (doc_id, user_id, role) ON members TO owner;",
    )
    .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON docs TO owner;");
    assert_eq!(
        answered(&twice),
        "members.doc_id WHERE user_id = app.user_id AND role IN ('read', 'admin')"
    );
    assert!(matches!(
        refusal(&columns("doc_id, user_id")),
        HelperSetError::MissingSelect { table, .. } if table == "members"
    ));
    let folded = helper(
        DEFINER,
        &GRANT_ARM.replace("role = min_role", "(role = 'admin' OR min_role = 'read')"),
        "GRANT SELECT (doc_id, user_id) ON members TO owner;",
    )
    .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON docs TO owner;");
    assert!(
        matches!(refusal(&folded), HelperSetError::MissingSelect { .. }),
        "a column read in a condition that reduces to TRUE is still read"
    );

    let parent = |granted: &str| {
        folders("teams_with($1)", &format!("GRANT SELECT ({granted}) ON folders TO owner;"))
            .replace(
                "GRANT SELECT ON teams, team_members, folders",
                "GRANT SELECT ON teams, team_members",
            )
    };
    folders_with(&parent("id, team_id"), text("read")).expect("answered");
    assert!(matches!(
        folders_with(&parent("id"), text("read")),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "folders"
    ));
}

#[test]
fn a_parent_argument_named_like_a_column_passes_the_column() {
    let sql = folders("teams_with(min_role)", "")
        .replace(
            "CREATE FUNCTION folders_with(TEXT)",
            "CREATE FUNCTION folders_with(min_role TEXT)",
        )
        .replace(
            "team_id TEXT REFERENCES teams (id)\n",
            "team_id TEXT REFERENCES teams (id), min_role TEXT\n",
        );
    assert!(matches!(folders_with(&sql, text("read")), Err(HelperSetError::ArmShape { .. })));
}
