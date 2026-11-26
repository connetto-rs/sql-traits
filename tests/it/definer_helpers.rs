//! What a declared `STABLE SECURITY DEFINER` SQL helper returns for a caller,
//! and why it refuses to answer.
//!
//! The fixture is connetto's R100 ownership schema. Every caller's expected
//! set was measured on PostgreSQL 17.11 by calling the helpers over the rows
//! [`ROWS`] holds.
#![allow(clippy::expect_used, clippy::panic)]

extern crate alloc;

use alloc::{collections::BTreeSet, sync::Arc};

use sql_traits::{
    errors::{AccessorRefusal, HelperSetError},
    prelude::*,
};
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

/// Fixture rows, as `(table, [(column, value)])`, a column left out holding
/// `NULL`.
type Rows = &'static [(&'static str, &'static [(&'static str, &'static str)])];

/// The rows of R100's behaviour matrix.
const ROWS: Rows = &[
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
    function(database, schema, name).helper_set(arguments, deployment, database, &[])
}

/// The condition naming the grantee of a grant arm, `None` for anyone.
fn render_granted_to(granted_to: &GrantedTo<'_, ParserDB>) -> Option<String> {
    match granted_to {
        GrantedTo::Request(request) => {
            let compared = match request.value() {
                SettingValue::Set { .. } => "IN",
                SettingValue::Caller(_)
                | SettingValue::Principal { .. }
                | SettingValue::Scalar(_) => "=",
            };
            let setting = request.setting().name();
            let read = match request.normalization() {
                None => setting.to_string(),
                Some(RequestNormalization::Lower) => format!("lower({setting})"),
            };
            Some(format!("{} {compared} {read}", request.column().column_name()))
        }
        GrantedTo::Set(grantees) => {
            let arguments: Vec<String> =
                grantees.arguments().iter().map(ToString::to_string).collect();
            Some(format!(
                "{} IN {}({}) {{{}}}",
                grantees.column().column_name(),
                grantees.function().target_name(),
                arguments.join(", "),
                render(grantees.set()),
            ))
        }
        GrantedTo::Anyone => None,
    }
}

/// A cap as `(<operands>) IN <helper>(<arguments>) {<its set>}`, behind
/// `<setting> IS UNSET OR` where `''` lifts it too and `<setting> IS NULL
/// OR` where only `NULL` does, an operand on a row of `joined` qualified by
/// its table.
fn render_cap(cap: &RowCap<'_, ParserDB>, joined: &[JoinedRow<'_, ParserDB>]) -> String {
    let operands: Vec<String> = cap
        .operands()
        .iter()
        .map(|operand| {
            match operand {
                CapOperand::Column(column) => column.column_name().to_string(),
                CapOperand::Joined { row, column } => {
                    format!("{}.{}", joined[*row].table().table_name(), column.column_name())
                }
                CapOperand::Value(value) => value.to_string(),
            }
        })
        .collect();
    let arguments: Vec<String> = cap.arguments().iter().map(ToString::to_string).collect();
    let membership = format!(
        "({}) IN {}({}) {{{}}}",
        operands.join(", "),
        cap.function().target_name(),
        arguments.join(", "),
        render(cap.set()),
    );
    match cap.lift() {
        Some(lift) => {
            let unset = if lift.reads_empty_as_unset() { "UNSET" } else { "NULL" };
            format!("({} IS {unset} OR {membership})", lift.setting().name())
        }
        None => membership,
    }
}

/// A grant arm as its table and columns followed by what filters it.
fn render_grant(grant: &GrantArm<'_, ParserDB>) -> String {
    let granted_to = render_granted_to(grant.granted_to());
    let gate = grant.gate().map(|gate| {
        let name = gate.setting().name();
        let excluded: Vec<String> = gate.excluded().iter().map(ToString::to_string).collect();
        let mut gated = Vec::new();
        if gate.excludes_unset() {
            gated.push(format!("{name} IS SET"));
        }
        if !excluded.is_empty() {
            gated.push(format!("{name} NOT IN ({})", excluded.join(", ")));
        } else if !gate.excludes_unset() {
            gated.push(format!("{name} IS NOT NULL"));
        }
        gated.join(" AND ")
    });
    let unset_settings = grant.unset_settings().iter().map(|unset| {
        let unset_as = if unset.reads_empty_as_unset() { "UNSET" } else { "NULL" };
        format!("{} IS {unset_as}", unset.setting().name())
    });
    let restrictions = grant.restrictions().iter().map(render_restriction);
    let bounds = grant.time_bounds().iter().map(render_bound);
    let joins = grant.joined_rows().iter().map(render_joined_row);
    let anti_joins = grant.anti_joins().iter().map(|anti_join| {
        let correlations = anti_join.correlations().iter().map(|correlation| {
            format!(
                "{} = {}.{}",
                correlation.blocking_column().column_name(),
                grant.table().table_name(),
                correlation.arm_column().column_name()
            )
        });
        let conditions: Vec<String> = correlations
            .chain(anti_join.restrictions().iter().map(render_restriction))
            .chain(anti_join.time_bounds().iter().map(render_bound))
            .collect();
        let keyed = if anti_join.is_keyed() { "keyed " } else { "" };
        format!(
            "NOT EXISTS ({keyed}{} WHERE {})",
            anti_join.table().table_name(),
            conditions.join(" AND ")
        )
    });
    let exclusions = grant.exclusions().iter().map(render_exclusion);
    let conditions: Vec<String> = granted_to
        .into_iter()
        .chain(gate)
        .chain(unset_settings)
        .chain(restrictions)
        .chain(bounds)
        .chain(joins)
        .chain(grant.caps().iter().map(|cap| render_cap(cap, grant.joined_rows())))
        .chain(anti_joins)
        .chain(exclusions)
        .collect();
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let returned = render_returned(grant.returned(), |column| column.column_name().to_string());
    format!("{}.{returned}{filter}", grant.table().table_name())
}

/// `<column> NOT IN <helper>(<arguments>) {<set>}`.
fn render_exclusion(exclusion: &ExcludedSet<'_, ParserDB>) -> String {
    let arguments: Vec<String> = exclusion.arguments().iter().map(ToString::to_string).collect();
    format!(
        "{} NOT IN {}({}) {{{}}}",
        exclusion.column().column_name(),
        exclusion.function().target_name(),
        arguments.join(", "),
        render(exclusion.set()),
    )
}

/// The columns an arm returns, one bare and several in parentheses, each
/// read by `read` or a list of labels as `EACH(<labels>)`.
fn render_returned<S>(returned: &[Returned<S>], read: impl Fn(&S) -> String) -> String {
    let returned: Vec<String> = returned
        .iter()
        .map(|returned| {
            match returned {
                Returned::Read(source) => read(source),
                Returned::Labels(labels) => {
                    let labels: Vec<String> = labels.iter().map(ToString::to_string).collect();
                    format!("EACH({})", labels.join(", "))
                }
            }
        })
        .collect();
    if let [_] = returned.as_slice() {
        return returned.into_iter().collect();
    }
    format!("({})", returned.join(", "))
}

/// An answer as one line, each arm as its table and column followed by what
/// filters it, and the selection kept from their union last.
fn render(set: &HelperSet<'_, ParserDB>) -> String {
    let arms: Vec<String> = set
        .arms()
        .iter()
        .map(|arm| {
            match arm {
                HelperArm::Grant(grant) => render_grant(grant),
                HelperArm::Walk(walk) => {
                    let (returned, compared) = match walk.direction() {
                        WalkDirection::Down => (walk.child(), walk.parent()),
                        WalkDirection::Up => (walk.parent(), walk.child()),
                    };
                    let mut rendered = format!(
                        "{}.{} WHERE {} IN itself",
                        walk.table().table_name(),
                        render_columns(returned),
                        render_columns(compared),
                    );
                    let conditions = walk
                        .restrictions()
                        .iter()
                        .map(render_restriction)
                        .chain(walk.time_bounds().iter().map(render_bound))
                        .chain(walk.exclusions().iter().map(render_exclusion));
                    for condition in conditions {
                        rendered.push_str(" AND ");
                        rendered.push_str(&condition);
                    }
                    rendered
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
                HelperArm::Called(called) => {
                    let arguments: Vec<String> =
                        called.arguments().iter().map(ToString::to_string).collect();
                    format!(
                        "{}({}).{} {{{}}}",
                        called.function().target_name(),
                        arguments.join(", "),
                        render_returned(called.returned(), |index| format!("#{index}")),
                        render(called.set()),
                    )
                }
            }
        })
        .collect();
    let union = arms.join(" UNION ");
    match set.selection() {
        Some(selection) => {
            let kept: Vec<String> = selection.iter().map(render_grant).collect();
            format!("{union} INTERSECT ({})", kept.join(" UNION "))
        }
        None => union,
    }
}

/// `(<host columns>) -> <table> {<conditions>}`.
fn render_joined_row(joined: &JoinedRow<'_, ParserDB>) -> String {
    let host: Vec<&str> = joined.columns().iter().map(|(host, _)| host.column_name()).collect();
    let conditions: Vec<String> = joined
        .restrictions()
        .iter()
        .map(render_restriction)
        .chain(joined.time_bounds().iter().map(render_bound))
        .collect();
    format!(
        "({}) -> {} {{{}}}",
        host.join(", "),
        joined.table().table_name(),
        conditions.join(" AND ")
    )
}

/// `<column> IN (<values>)`, `<column> IS NULL` or `<column> IS NOT NULL`.
fn render_restriction(restriction: &Restriction<'_, ParserDB>) -> String {
    let column = restriction.column().column_name();
    match restriction {
        Restriction::In(set) => {
            let values: Vec<String> = set.values().iter().map(ToString::to_string).collect();
            format!("{column} IN ({})", values.join(", "))
        }
        Restriction::Null(_) => format!("{column} IS NULL"),
        Restriction::NotNull(_) => format!("{column} IS NOT NULL"),
    }
}

/// Whether `row` meets `restriction`.
fn restricts(restriction: &Restriction<'_, ParserDB>, row: &[(&str, &'static str)]) -> bool {
    let value = nullable(row, restriction.column().column_name());
    match restriction {
        Restriction::In(set) => {
            value.is_some_and(|value| set.values().iter().any(|allowed| holds(allowed, value)))
        }
        Restriction::Null(_) => value.is_none(),
        Restriction::NotNull(_) => value.is_some(),
    }
}

/// `<column> <comparison> now`, or `NULL` kept beside it.
fn render_bound(bound: &TimeBound<'_, ParserDB>) -> String {
    let column = bound.column().column_name();
    let operator = match bound.comparison() {
        TimeComparison::Later => ">",
        TimeComparison::LaterOrEqual => ">=",
        TimeComparison::Earlier => "<",
        TimeComparison::EarlierOrEqual => "<=",
    };
    if bound.keeps_null() {
        format!("({column} IS NULL OR {column} {operator} now)")
    } else {
        format!("{column} {operator} now")
    }
}

/// The fixture rows of `table`.
fn rows(
    fixture: Rows,
    table: &str,
) -> impl Iterator<Item = &'static [(&'static str, &'static str)]> + '_ {
    fixture.iter().filter(move |(name, _)| *name == table).map(|(_, row)| *row)
}

/// The value `row` holds in `column`, `None` for `NULL`.
fn nullable(row: &[(&str, &'static str)], column: &str) -> Option<&'static str> {
    row.iter().find(|(name, _)| *name == column).map(|(_, value)| *value)
}

fn cell(row: &[(&str, &'static str)], column: &str) -> &'static str {
    nullable(row, column).expect("the column has a value")
}

/// What a request carries: its caller, `None` where the caller setting reads
/// `NULL`, and the values of every other setting it sets, the elements of a
/// set or the identity of a principal, a setting it leaves unset holding
/// none.
#[derive(Clone, Copy)]
struct Request {
    caller: Option<&'static str>,
    values: &'static [(&'static str, &'static [&'static str])],
}

impl Request {
    /// A request carrying `caller` and no other setting.
    const fn of(caller: &'static str) -> Self {
        Self { caller: Some(caller), values: &[] }
    }

    /// The values the request carries in the setting named `name`, `None`
    /// where it leaves the setting unset.
    fn values(self, name: &str) -> Option<&'static [&'static str]> {
        self.values.iter().find(|(set, _)| *set == name).map(|(_, values)| *values)
    }

    /// Whether `held` is what the request carries in the setting `compared`
    /// reads, normalized as `compared` normalizes it.
    fn carries(self, compared: &SettingComparison<'_, ParserDB>, held: &str) -> bool {
        let sends = |value: &str| {
            match compared.normalization() {
                None => value == held,
                Some(RequestNormalization::Lower) => value.to_lowercase() == held,
            }
        };
        match compared.value() {
            SettingValue::Caller(_) => self.caller.is_some_and(sends),
            SettingValue::Set { .. } | SettingValue::Principal { .. } | SettingValue::Scalar(_) => {
                self.values(compared.setting().name())
                    .is_some_and(|values| values.iter().any(|value| sends(value)))
            }
        }
    }
}

/// Whether a fixture cell holding `cell` holds a restriction's `value`.
fn holds(value: &Value, cell: &str) -> bool {
    match value {
        Value::SingleQuotedString(text) => text == cell,
        Value::Boolean(boolean) => cell == if *boolean { "true" } else { "false" },
        other => panic!("the fixtures restrict by text or boolean, not {other}"),
    }
}

/// Whether `request` leaves the setting `unset` reads unset, carrying it as
/// a scalar where it sets it.
fn left_unset(unset: &UnsetSetting<'_>, request: Request) -> bool {
    match request.values(unset.setting().name()) {
        None => true,
        Some(&[value]) => value.is_empty() && unset.reads_empty_as_unset(),
        Some(values) => panic!("a scalar holds one value, not {values:?}"),
    }
}

/// The rows `cap` compares its operands with for `request` over `fixture`.
fn cap_rows(
    cap: &RowCap<'_, ParserDB>,
    request: Request,
    fixture: Rows,
) -> BTreeSet<Vec<&'static str>> {
    if let [_] = cap.operands() {
        return evaluate(cap.set(), request, fixture)
            .into_iter()
            .map(|value| vec![value])
            .collect();
    }
    arm_rows(cap.set(), request, fixture)
}

/// Evaluates an answer of one column over `fixture` for `request`, the way a
/// translator lowering it to SQL over the backing tables would.
fn evaluate(
    set: &HelperSet<'_, ParserDB>,
    request: Request,
    fixture: Rows,
) -> BTreeSet<&'static str> {
    let mut returned: BTreeSet<&'static str> = walked_rows(set, request, fixture)
        .into_iter()
        .map(|row| {
            let [value] = row[..] else { panic!("the helper returns one column") };
            value
        })
        .collect();
    if let Some(selection) = set.selection() {
        let mut kept = BTreeSet::new();
        for grant in selection {
            grant_rows(grant, request, fixture, &mut kept);
        }
        returned.retain(|value| kept.contains(&vec![*value]));
    }
    returned
}

/// The rows the arms of `set` return over `fixture` for `request` and the
/// rows its walks add to them, before its selection. A walk up the tree
/// leaves out the `NULL` a root adds.
fn walked_rows(
    set: &HelperSet<'_, ParserDB>,
    request: Request,
    fixture: Rows,
) -> BTreeSet<Vec<&'static str>> {
    let mut returned = arm_rows(set, request, fixture);
    // A walk adds the rows next to what the set holds until it holds them
    // all, as PostgreSQL's `UNION` iterates its working table.
    loop {
        let mut grew = false;
        for arm in set.arms() {
            let HelperArm::Walk(walk) = arm else { continue };
            let (reached, added) = match walk.direction() {
                WalkDirection::Down => (walk.parent(), walk.child()),
                WalkDirection::Up => (walk.child(), walk.parent()),
            };
            let excluded: Vec<_> = walk
                .exclusions()
                .iter()
                .map(|exclusion| (exclusion, evaluate(exclusion.set(), request, fixture)))
                .collect();
            for row in rows(fixture, walk.table().table_name()) {
                // `NOT IN` is `NULL` for a `NULL` compared with any value.
                let outside = excluded.iter().all(|(exclusion, excluded)| {
                    nullable(row, exclusion.column().column_name())
                        .map_or(excluded.is_empty(), |value| !excluded.contains(value))
                });
                if values(row, reached).is_some_and(|held| returned.contains(&held))
                    && walk.restrictions().iter().all(|restriction| restricts(restriction, row))
                    && outside
                    && let Some(added) = values(row, added)
                {
                    grew |= returned.insert(added);
                }
            }
        }
        if !grew {
            break;
        }
    }
    returned
}

/// The values `row` holds in `columns`, `None` where one is `NULL`.
fn values(
    row: &[(&str, &'static str)],
    columns: &[&<ParserDB as DatabaseLike>::Column],
) -> Option<Vec<&'static str>> {
    columns.iter().map(|column| nullable(row, column.column_name())).collect()
}

/// Columns by name, one bare and several in parentheses.
fn render_columns(columns: &[&<ParserDB as DatabaseLike>::Column]) -> String {
    let names: Vec<&str> = columns.iter().map(ColumnLike::column_name).collect();
    match names.as_slice() {
        [name] => (*name).to_string(),
        _ => format!("({})", names.join(", ")),
    }
}

/// Adds the rows `grant` returns over `fixture` for `request` to `returned`,
/// each as the values of its columns.
fn grant_rows(
    grant: &GrantArm<'_, ParserDB>,
    request: Request,
    fixture: Rows,
    returned: &mut BTreeSet<Vec<&'static str>>,
) {
    let gated = grant.gate().is_none_or(|gate| {
        request.caller.is_some_and(|caller| {
            !(gate.excludes_unset() && caller.is_empty())
                && gate.excluded().iter().all(|value| !holds(value, caller))
        })
    });
    if !gated || !grant.unset_settings().iter().all(|unset| left_unset(unset, request)) {
        return;
    }
    let grantees = match grant.granted_to() {
        GrantedTo::Set(grantees) => Some(evaluate(grantees.set(), request, fixture)),
        GrantedTo::Request(_) | GrantedTo::Anyone => None,
    };
    // A lifted cap keeps every row, so its rows are `None`.
    let caps: Vec<_> = grant
        .caps()
        .iter()
        .map(|cap| {
            (!cap.lift().is_some_and(|lift| left_unset(lift, request)))
                .then(|| cap_rows(cap, request, fixture))
        })
        .collect();
    let excluded: Vec<_> = grant
        .exclusions()
        .iter()
        .map(|exclusion| evaluate(exclusion.set(), request, fixture))
        .collect();
    for row in rows(fixture, grant.table().table_name()) {
        let restricted = grant.restrictions().iter().all(|restriction| restricts(restriction, row))
            && grant.joined_rows().iter().all(|joined| joins(row, joined, fixture));
        let granted = match grant.granted_to() {
            GrantedTo::Request(compared) => {
                let held = cell(row, compared.column().column_name());
                !(compared.excludes_unset() && held.is_empty()) && request.carries(compared, held)
            }
            GrantedTo::Set(compared) => {
                grantees.as_ref().is_some_and(|grantees| {
                    grantees.contains(cell(row, compared.column().column_name()))
                })
            }
            GrantedTo::Anyone => true,
        };
        let unblocked = grant.anti_joins().iter().all(|anti_join| !blocks(anti_join, row, fixture));
        // `NOT IN` is `NULL` for a `NULL` compared with any value.
        let outside = grant.exclusions().iter().zip(&excluded).all(|(exclusion, excluded)| {
            nullable(row, exclusion.column().column_name())
                .map_or(excluded.is_empty(), |value| !excluded.contains(value))
        });
        if granted && restricted && unblocked && outside && capped(grant, &caps, row, fixture) {
            let columns: Vec<Vec<&'static str>> = grant
                .returned()
                .iter()
                .map(|column| {
                    match column {
                        Returned::Read(column) => vec![cell(row, column.column_name())],
                        Returned::Labels(labels) => labels.iter().map(label).collect(),
                    }
                })
                .collect();
            returned.extend(crossed(&columns));
        }
    }
}

/// The labels the fixtures cross rows with.
const LABELS: &[&str] = &["reader", "writer", "admin"];

/// The fixture label `value` holds.
fn label(value: &Value) -> &'static str {
    let Value::SingleQuotedString(text) = value else { panic!("a label is text, not {value}") };
    LABELS
        .iter()
        .copied()
        .find(|label| label == text)
        .unwrap_or_else(|| panic!("the fixtures label no {text}"))
}

/// Every row taking one value of each column's values, in order.
fn crossed(columns: &[Vec<&'static str>]) -> Vec<Vec<&'static str>> {
    columns.iter().fold(vec![Vec::new()], |rows, values| {
        rows.iter()
            .flat_map(|row| {
                values.iter().map(move |value| {
                    let mut row = row.clone();
                    row.push(*value);
                    row
                })
            })
            .collect()
    })
}

/// The rows `set` returns over `fixture` for `request`, its walks included.
fn helper_rows(
    set: &HelperSet<'_, ParserDB>,
    request: Request,
    fixture: Rows,
) -> BTreeSet<Vec<&'static str>> {
    if set.selection().is_some() {
        return evaluate(set, request, fixture).into_iter().map(|value| vec![value]).collect();
    }
    walked_rows(set, request, fixture)
}

/// The rows the grant and parent arms of `set` return over `fixture` for
/// `request`, each as the values of its columns, leaving out what a walk
/// adds.
fn arm_rows(
    set: &HelperSet<'_, ParserDB>,
    request: Request,
    fixture: Rows,
) -> BTreeSet<Vec<&'static str>> {
    let mut returned = BTreeSet::new();
    for arm in set.arms() {
        match arm {
            HelperArm::Grant(grant) => grant_rows(grant, request, fixture, &mut returned),
            HelperArm::Parent(parent) => {
                let parents = evaluate(parent.set(), request, fixture);
                for row in rows(fixture, parent.table().table_name()) {
                    if parents.contains(cell(row, parent.column().column_name())) {
                        returned.insert(vec![cell(row, parent.key().column_name())]);
                    }
                }
            }
            HelperArm::Called(called) => {
                for row in helper_rows(called.set(), request, fixture) {
                    let columns: Vec<Vec<&'static str>> = called
                        .returned()
                        .iter()
                        .map(|column| {
                            match column {
                                Returned::Read(index) => vec![row[*index]],
                                Returned::Labels(labels) => labels.iter().map(label).collect(),
                            }
                        })
                        .collect();
                    returned.extend(crossed(&columns));
                }
            }
            HelperArm::Walk(_) => {}
        }
    }
    returned
}

/// Whether `row` of `grant` meets every cap, each beside the rows it
/// compares with, `None` where it is lifted.
fn capped(
    grant: &GrantArm<'_, ParserDB>,
    compared: &[Option<BTreeSet<Vec<&'static str>>>],
    row: &[(&str, &'static str)],
    fixture: Rows,
) -> bool {
    grant.caps().iter().zip(compared).all(|(cap, allowed)| {
        let Some(allowed) = allowed else { return true };
        // A `NULL` operand matches no row.
        let operands: Option<Vec<&str>> = cap
            .operands()
            .iter()
            .map(|operand| {
                match operand {
                    CapOperand::Column(column) => nullable(row, column.column_name()),
                    CapOperand::Joined { row: joined, column } => {
                        let named = named_row(row, &grant.joined_rows()[*joined], fixture)
                            .expect("the arm keeps only rows naming their joined rows");
                        nullable(named, column.column_name())
                    }
                    CapOperand::Value(Value::SingleQuotedString(text)) => Some(text.as_str()),
                    CapOperand::Value(Value::Null) => None,
                    CapOperand::Value(other) => panic!("the fixtures cap by text, not {other}"),
                }
            })
            .collect();
        operands.is_some_and(|operands| allowed.contains(&operands))
    })
}

/// Whether a row of `fixture` blocks `row` of a grant arm through
/// `anti_join`, a `NULL` in a correlated column of `row` matching nothing.
fn blocks(anti_join: &AntiJoin<'_, ParserDB>, row: &[(&str, &'static str)], fixture: Rows) -> bool {
    assert!(anti_join.time_bounds().is_empty(), "the fixtures bound no blocking row in time");
    rows(fixture, anti_join.table().table_name()).any(|blocking| {
        anti_join.correlations().iter().all(|correlation| {
            nullable(row, correlation.arm_column().column_name()).is_some_and(|value| {
                nullable(blocking, correlation.blocking_column().column_name()) == Some(value)
            })
        }) && anti_join.restrictions().iter().all(|restriction| restricts(restriction, blocking))
    })
}

/// The row of the joined table `row` names through the foreign key of
/// `joined`, `None` where a `NULL` in the key names none.
fn named_row(
    row: &[(&str, &'static str)],
    joined: &JoinedRow<'_, ParserDB>,
    fixture: Rows,
) -> Option<&'static [(&'static str, &'static str)]> {
    rows(fixture, joined.table().table_name()).find(|other| {
        joined.columns().iter().all(|(host, referenced)| {
            nullable(row, host.column_name()).is_some()
                && nullable(row, host.column_name()) == nullable(other, referenced.column_name())
        })
    })
}

/// Whether `row` names a row of the joined table meeting its restrictions.
fn joins(row: &[(&str, &'static str)], joined: &JoinedRow<'_, ParserDB>, fixture: Rows) -> bool {
    named_row(row, joined, fixture).is_some_and(|other| {
        joined.restrictions().iter().all(|restriction| restricts(restriction, other))
    })
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
        let returned: Vec<_> = evaluate(&set, Request::of(caller), ROWS).into_iter().collect();
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
fn a_column_of_a_composite_foreign_key_returns_keys_of_the_table_it_references() {
    let deployment = deployment();
    let body = "SELECT folder_id FROM folder_members \
                WHERE user_id = current_setting('app.user_id', true) AND role = min_role";
    for (members, expected) in [
        (
            "folder_kind TEXT NOT NULL, \
             FOREIGN KEY (folder_kind, folder_id) REFERENCES folders (kind, id)",
            Some("folders"),
        ),
        (
            "folder_kind TEXT, \
             FOREIGN KEY (folder_kind, folder_id) REFERENCES folders (kind, id) MATCH FULL",
            Some("folders"),
        ),
        (
            "folder_kind TEXT NOT NULL, \
             FOREIGN KEY (folder_kind, folder_id) REFERENCES folders (kind, id), \
             FOREIGN KEY (folder_id) REFERENCES folders (id)",
            Some("folders"),
        ),
        (
            "folder_kind TEXT, \
             FOREIGN KEY (folder_kind, folder_id) REFERENCES folders (kind, id)",
            None,
        ),
        (
            "folder_kind TEXT NOT NULL, \
             FOREIGN KEY (folder_id, folder_kind) REFERENCES folders (kind, id)",
            None,
        ),
        (
            "folder_kind TEXT NOT NULL, \
             FOREIGN KEY (folder_kind, folder_id) REFERENCES folders (kind, id), \
             FOREIGN KEY (folder_id) REFERENCES shelves (id)",
            None,
        ),
    ] {
        let after = format!(
            "CREATE TABLE folders (id TEXT PRIMARY KEY, kind TEXT NOT NULL, UNIQUE (kind, id));
             CREATE TABLE shelves (id TEXT PRIMARY KEY);
             CREATE TABLE folder_members (folder_id TEXT, user_id TEXT, role TEXT, {members});
             GRANT SELECT ON folders, shelves, folder_members TO owner;"
        );
        let database = postgres(&helper(DEFINER, body, &after));
        let set = answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .expect("answered");
        assert_eq!(covered(&set), expected, "{members}");
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
        let returned: Vec<_> = evaluate(&set, Request::of(caller), ROWS).into_iter().collect();
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
    assert_eq!(
        notes(GRANT_ARM, "SELECT id FROM notes WHERE id IN (SELECT docs_with(min_role))")
            .expect("answered"),
        "notes.id WHERE (id) IN docs_with('read') \
         {members.doc_id WHERE user_id = app.user_id AND role IN ('read')}",
        "the key the arm returns is no parent, so the set caps it"
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
        matches!(grant.granted_to(), GrantedTo::Request(request) if request.raises_when_unset()),
        "current_setting without missing_ok raises on an unset setting"
    );

    let set = answer(&database, &deployment, "public", "docs_of_kind", &[text("note")])
        .expect("answered");
    assert_eq!(render(&set), "members.doc_id WHERE user_id = app.user_id AND kind IN ('note')");
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    assert!(
        matches!(grant.granted_to(), GrantedTo::Request(request) if !request.raises_when_unset())
    );
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

/// A deployment declaring `app.user_id` as `value`.
fn declaring(value: SettingValue) -> Deployment {
    let setting = SessionSetting::value("app.user_id", value).expect("a custom setting name");
    Deployment::new("app", ["app"], [setting]).expect("a valid deployment")
}

fn delimited(delimiter: &str) -> SettingValue {
    SettingValue::Set { element: DataType::Text, packing: SetPacking::Delimited(delimiter.into()) }
}

#[test]
fn a_compared_setting_must_be_declared_as_what_the_comparison_reads() {
    let undeclared = helper(DEFINER, &GRANT_ARM.replace("app.user_id", "app.tenant"), "");
    assert!(matches!(
        refusal(&undeclared),
        HelperSetError::UndeclaredSetting { setting, .. } if setting == "app.tenant"
    ));
    let equal = helper(DEFINER, GRANT_ARM, "");
    assert!(matches!(
        refusal_with(&equal, &declaring(delimited(","))),
        HelperSetError::SettingNotSingle { setting, .. } if setting == "app.user_id"
    ));
    let database = postgres(&equal);
    let rendered = |value: SettingValue| {
        let deployment = declaring(value);
        answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .map(|set| render(&set))
    };
    let installation =
        SettingValue::Principal { kind: "installation".into(), identity: DataType::Text };
    for value in [installation, SettingValue::Scalar(DataType::Text)] {
        let declared = format!("{value:?}");
        assert_eq!(
            rendered(value),
            rendered(SettingValue::Caller(DataType::Text)),
            "{declared} compares as the caller does"
        );
    }
    let split = helper(
        DEFINER,
        "SELECT doc_id FROM members WHERE user_id = ANY (string_to_array(current_setting('app.user_id', true), ','))",
        "",
    );
    assert!(matches!(
        refusal(&split),
        HelperSetError::SettingNotSet { setting, .. } if setting == "app.user_id"
    ));
    let json = helper(
        DEFINER,
        "SELECT doc_id FROM members WHERE user_id IN (SELECT jsonb_array_elements_text(current_setting('app.user_id', true)::jsonb))",
        "",
    );
    let json_array = SettingValue::Set { element: DataType::Text, packing: SetPacking::JsonArray };
    for (sql, value) in
        [(&split, delimited(";")), (&split, json_array.clone()), (&json, delimited(","))]
    {
        assert!(
            matches!(
                refusal_with(sql, &declaring(value.clone())),
                HelperSetError::MispackedSet { setting, .. } if setting == "app.user_id"
            ),
            "{value:?}"
        );
    }
    for (sql, value) in [(&split, delimited(",")), (&json, json_array)] {
        let database = postgres(sql);
        let deployment = declaring(value);
        let set = answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .expect("answered");
        assert_eq!(render(&set), "members.doc_id WHERE user_id IN app.user_id");
    }
}

#[test]
fn an_arm_comparing_no_setting_keeps_the_rows_its_literals_and_arguments_select() {
    let answered = |body: &str| {
        let database = postgres(&helper(DEFINER, body, ""));
        let deployment = deployment();
        answer(&database, &deployment, "public", "docs_with", &[text("u1")])
            .map(|set| render(&set))
            .expect("answered")
    };
    assert_eq!(
        answered("SELECT doc_id FROM members WHERE user_id = min_role"),
        "members.doc_id WHERE user_id IN ('u1')",
        "the argument chooses the rows of one call, as PostgreSQL substitutes it"
    );
    assert_eq!(
        answered("SELECT doc_id FROM members WHERE role = 'admin' AND user_id = min_role"),
        "members.doc_id WHERE role IN ('admin') AND user_id IN ('u1')"
    );
    assert_eq!(answered("SELECT doc_id FROM members"), "members.doc_id");
    assert_eq!(answered("SELECT doc_id FROM members WHERE TRUE"), "members.doc_id");
    assert_eq!(answered("SELECT doc_id FROM members WHERE min_role = 'u2'"), "");
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
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) AND role = 'a' AND user_id = current_setting('app.user_id')",
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) GROUP BY doc_id",
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) INTERSECT SELECT id FROM docs",
        "SELECT id FROM docs WHERE parent_id IN (SELECT docs_with(min_role)) AND id = 'x'",
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
fn conditions_intersect_per_column_and_hold_together_across_columns() {
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
    let across = |argument: &str| {
        kept(
            "min_role TEXT",
            "TEXT",
            "doc_id = 'd' AND role IN ('admin', min_role) AND doc_id IN ('d', 'e')",
            &[text(argument)],
        )
    };
    assert_eq!(
        across("write").expect("answered"),
        format!("{ROLES} AND doc_id IN ('d') AND role IN ('admin', 'write')")
    );
    assert_eq!(
        kept(
            "min_role TEXT",
            "TEXT",
            "doc_id = 'd' AND role = min_role AND role = 'admin'",
            &[text("read")]
        )
        .expect("answered"),
        "",
        "one column allowing no value keeps no row whatever the others allow"
    );
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
fn a_null_test_requires_a_column_to_hold_no_value_or_any_value() {
    let text_kept =
        |condition: &str, argument: Value| kept("min_role TEXT", "TEXT", condition, &[argument]);
    for (condition, expected) in [
        ("role IS NULL", " AND role IS NULL"),
        ("members.role IS NOT NULL", " AND role IS NOT NULL"),
        ("doc_id IS NULL AND role = min_role", " AND doc_id IS NULL AND role IN ('write')"),
        ("role IS NOT NULL AND role IN ('admin', min_role)", " AND role IN ('admin', 'write')"),
        ("role = min_role AND role IS NOT NULL", " AND role IN ('write')"),
        ("(role IS NULL OR role IS NULL)", " AND role IS NULL"),
        ("(role = 'admin' OR role IS NOT NULL)", " AND role IS NOT NULL"),
        ("(role IS NULL OR role IS NOT NULL)", ""),
        ("min_role IS NOT NULL", ""),
    ] {
        assert_eq!(
            text_kept(condition, text("write")).expect("answered"),
            format!("{ROLES}{expected}"),
            "{condition}"
        );
    }
    for condition in ["role IS NULL AND role = min_role", "role IS NOT NULL AND role IS NULL"] {
        assert_eq!(text_kept(condition, text("write")).expect("answered"), "", "{condition}");
    }
    assert_eq!(text_kept("min_role IS NULL", Value::Null).expect("answered"), ROLES);
    assert_eq!(text_kept("min_role IS NULL", text("write")).expect("answered"), "");
    assert_eq!(text_kept("min_role IS NOT NULL", Value::Null).expect("answered"), "");
    for irreducible in [
        "role IS NULL OR role = 'admin'",
        "role IS NULL OR doc_id IS NULL",
        "upper(role) IS NULL",
        "NOT role IS NULL",
    ] {
        assert!(
            matches!(
                text_kept(irreducible, text("write")),
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

/// A helper over `objects` whose grant arm filters by `condition` on its
/// `threshold`, with `authz.role` declared in another order than its labels
/// sort in.
fn ranked(condition: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TYPE authz.role AS ENUM ('reader', 'admin');
         ALTER TYPE authz.role ADD VALUE 'writer' BEFORE 'admin';
         ALTER TYPE authz.role ADD VALUE 'auditor' AFTER 'admin';
         CREATE TABLE objects (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, role authz.role NOT NULL);
         CREATE FUNCTION authz.ranked(threshold authz.role) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT o.id FROM public.objects o
           WHERE o.owner_id = current_setting('app.user_id', true) AND {condition}
         $$;
         ALTER FUNCTION authz.ranked(authz.role) OWNER TO authz_owner;
         GRANT SELECT ON objects TO authz_owner;"
    )
}

/// The rows of `objects` the [`ranked`] helpers read.
const RANKED_ROWS: Rows = &[
    ("objects", &[("id", "r"), ("owner_id", "ann"), ("role", "reader")]),
    ("objects", &[("id", "w"), ("owner_id", "ann"), ("role", "writer")]),
    ("objects", &[("id", "a"), ("owner_id", "ann"), ("role", "admin")]),
    ("objects", &[("id", "d"), ("owner_id", "ann"), ("role", "auditor")]),
    ("objects", &[("id", "x"), ("owner_id", "ben"), ("role", "admin")]),
];

/// What PostgreSQL 18.6 returns to `ann` for `(condition, threshold)`, the
/// [`ranked`] helper called over [`RANKED_ROWS`].
const RANKED_MEASURED: &[(&str, &str, &[&str])] = &[
    ("o.role >= threshold", "reader", &["a", "d", "r", "w"]),
    ("o.role >= threshold", "writer", &["a", "d", "w"]),
    ("o.role >= threshold", "admin", &["a", "d"]),
    ("o.role >= threshold", "auditor", &["d"]),
    ("threshold < o.role", "reader", &["a", "d", "w"]),
    ("threshold < o.role", "writer", &["a", "d"]),
    ("threshold < o.role", "admin", &["d"]),
    ("threshold < o.role", "auditor", &[]),
    ("threshold >= o.role", "reader", &["r"]),
    ("threshold >= o.role", "writer", &["r", "w"]),
    ("threshold >= o.role", "admin", &["a", "r", "w"]),
    ("threshold >= o.role", "auditor", &["a", "d", "r", "w"]),
    ("o.role < threshold", "reader", &[]),
    ("o.role < threshold", "writer", &["r"]),
    ("o.role < threshold", "admin", &["r", "w"]),
    ("o.role < threshold", "auditor", &["a", "r", "w"]),
];

#[test]
fn an_enum_threshold_keeps_the_labels_on_its_side_in_declaration_order() {
    let deployment = deployment();
    for &(condition, threshold, expected) in RANKED_MEASURED {
        let database = postgres(&ranked(condition));
        let set = answer(&database, &deployment, "authz", "ranked", &[text(threshold)])
            .expect("answered");
        let returned: Vec<_> =
            evaluate(&set, Request::of("ann"), RANKED_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{condition} for '{threshold}'");
        let null =
            answer(&database, &deployment, "authz", "ranked", &[Value::Null]).expect("answered");
        assert_eq!(render(&null), "", "{condition} keeps no row for NULL");
    }
    let rendered = |condition: &str| {
        let database = postgres(&ranked(condition));
        answer(&database, &deployment, "authz", "ranked", &[text("reader")]).map(|set| render(&set))
    };
    let arm = "objects.id WHERE owner_id = app.user_id AND role IN";
    assert_eq!(
        rendered("o.role >= threshold").expect("answered"),
        format!("{arm} ('reader', 'writer', 'admin', 'auditor')")
    );
    assert_eq!(
        rendered("o.role > 'writer'").expect("answered"),
        format!("{arm} ('admin', 'auditor')")
    );
    assert_eq!(
        rendered("(o.role >= 'admin' OR o.role = threshold)").expect("answered"),
        format!("{arm} ('admin', 'auditor', 'reader')")
    );
    assert_eq!(rendered("o.role < threshold AND o.role = 'writer'").expect("answered"), "");
    for irreducible in
        ["o.role >= 'owner'", "o.role::TEXT >= threshold", "o.role >= o.role", "o.role >= 1"]
    {
        assert!(
            matches!(rendered(irreducible), Err(HelperSetError::Irreducible { .. })),
            "{irreducible}"
        );
    }
    let database = postgres(&ranked("o.role >= threshold"));
    assert!(
        matches!(
            answer(&database, &deployment, "authz", "ranked", &[text("owner")]),
            Err(HelperSetError::Irreducible { .. })
        ),
        "PostgreSQL refuses a label the type does not declare"
    );
    assert!(
        matches!(
            kept("min_role TEXT", "TEXT", "role >= min_role", &[text("read")]),
            Err(HelperSetError::Irreducible { .. })
        ),
        "text compares by its collation, which declares no order of labels"
    );
    let set = ranked("o.role >= threshold")
        .replace("role authz.role NOT NULL", "role SET('reader', 'admin') NOT NULL");
    let database = postgres(&set);
    assert!(
        matches!(
            answer(&database, &deployment, "authz", "ranked", &[text("reader")]),
            Err(HelperSetError::Irreducible { .. })
        ),
        "a SET holds a subset of its labels, which declaration order does not rank"
    );
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
        "role = 'c' OR (role = 'a' AND doc_id = 'd')",
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
    // A read that is not the caller leaves a comparison no literal decides.
    let irreducible = |read: &str, after: &str| {
        matches!(caller(read, after), Err(HelperSetError::Irreducible { .. }))
    };
    for read in [
        "other.current_setting('app.user_id', true)",
        "current_setting('app.user_id', 'yes')",
        "current_setting(min_role, true)",
        "current_setting(1)",
        "current_setting('app.user_id', true, true)",
        "current_setting(DISTINCT 'app.user_id')",
    ] {
        assert!(irreducible(read, ""), "{read}");
    }
    assert!(
        irreducible("current_setting('app.user_id', true)", shadow),
        "a declared current_setting the call may reach is not PostgreSQL's"
    );
    assert!(matches!(
        caller(
            "current_setting('app.user_id', true) AND doc_id = current_setting('app.user_id', true)",
            ""
        ),
        Err(HelperSetError::ArmShape { .. })
    ));
    assert!(irreducible("doc_id AND role = 'admin'", ""));
}

#[test]
fn only_postgres_own_functions_unpack_a_set_in_either_spelling() {
    let link_ids = SessionSetting::value("app.link_ids", delimited(",")).expect("valid");
    let link_list = SessionSetting::value(
        "app.link_list",
        SettingValue::Set { element: DataType::Text, packing: SetPacking::JsonArray },
    )
    .expect("valid");
    let deployment = Deployment::new("app", ["app"], [link_ids, link_list]).expect("valid");
    let compared = |condition: &str, after: &str| {
        let body = format!("SELECT doc_id FROM members WHERE {condition}");
        let database = postgres(&helper(DEFINER, &body, after));
        answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .map(|set| render(&set))
    };
    let split = "string_to_array(current_setting('app.link_ids', true), ',')";
    let elements = "jsonb_array_elements_text(current_setting('app.link_list')::jsonb)";
    for (condition, setting) in [
        (format!("doc_id = ANY ({split})"), "app.link_ids"),
        (format!("doc_id = SOME (pg_catalog.{split})"), "app.link_ids"),
        (format!("doc_id = ANY (({split}))"), "app.link_ids"),
        (format!("doc_id IN (SELECT unnest({split}))"), "app.link_ids"),
        (format!("doc_id IN (SELECT pg_catalog.unnest({split}))"), "app.link_ids"),
        (format!("doc_id = ANY (ARRAY(SELECT unnest({split})))"), "app.link_ids"),
        (format!("doc_id IN (SELECT {elements})"), "app.link_list"),
        (format!("doc_id = ANY (ARRAY(SELECT {elements}))"), "app.link_list"),
        (
            "doc_id IN (SELECT jsonb_array_elements_text(CAST(current_setting('app.link_list') AS jsonb)))"
                .into(),
            "app.link_list",
        ),
    ] {
        assert_eq!(
            compared(&condition, "").expect("answered"),
            format!("members.doc_id WHERE doc_id IN {setting}"),
            "{condition}"
        );
    }
    let shadow = "CREATE FUNCTION string_to_array(value TEXT, delimiter TEXT) RETURNS TEXT[]
                  LANGUAGE sql AS $$ SELECT ARRAY['x'] $$;";
    assert!(
        matches!(
            compared(&format!("doc_id = ANY ({split})"), shadow),
            Err(HelperSetError::Irreducible { .. })
        ),
        "a declared string_to_array the call may reach is not PostgreSQL's"
    );
    for condition in [
        "doc_id = ANY (string_to_array(current_setting('app.link_ids', true), ',', ''))",
        "doc_id = ANY (string_to_array(current_setting('app.link_ids', true), NULL))",
        "doc_id <> ANY (string_to_array(current_setting('app.link_ids', true), ','))",
        "doc_id NOT IN (SELECT unnest(string_to_array(current_setting('app.link_ids'), ',')))",
    ] {
        assert!(
            matches!(compared(condition, ""), Err(HelperSetError::Irreducible { .. })),
            "{condition}"
        );
    }
    assert!(
        matches!(
            compared("user_id = 'u' AND doc_id IN (SELECT docs_with(min_role))", ""),
            Err(HelperSetError::CallCycle { function }) if function == "docs_with"
        ),
        "a subquery unpacking no set calls a helper returning grantees, here the helper itself"
    );
    // A subquery unpacking no set is read as a helper's call.
    for condition in [
        "doc_id IN (SELECT jsonb_array_elements_text(current_setting('app.link_list')::json))",
        "doc_id IN (SELECT jsonb_array_elements_text(current_setting('app.link_list')))",
        "doc_id IN (SELECT jsonb_array_elements_text(current_setting('app.link_list')::jsonb, 1))",
    ] {
        assert!(
            matches!(compared(condition, ""), Err(HelperSetError::ArmShape { .. })),
            "{condition}"
        );
    }
    assert!(
        matches!(
            compared(&format!("doc_id = ANY ({split}) AND user_id = ANY ({split})"), ""),
            Err(HelperSetError::ArmShape { .. })
        ),
        "an arm compares one column with the request"
    );
}

/// A helper over `UUID` keys reading the caller as `read`, under `path`.
fn blocking(path: &str, read: &str, after: &str) -> String {
    blocking_where(path, &format!("b.principal_id = {read}"), after)
}

/// A helper over `UUID` keys keeping the blocks `condition` keeps, under
/// `path`.
fn blocking_where(path: &str, condition: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE organization_blocks (
           organization_id UUID NOT NULL, principal_id UUID NOT NULL,
           PRIMARY KEY (organization_id, principal_id));
         GRANT SELECT ON organization_blocks TO authz_owner;
         {after}
         CREATE FUNCTION authz.blocking_organizations() RETURNS SETOF UUID
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = {path} AS $$
           SELECT b.organization_id FROM public.organization_blocks b
           WHERE {condition}
         $$;
         ALTER FUNCTION authz.blocking_organizations() OWNER TO authz_owner;"
    )
}

/// What `read` makes of `authz.blocking_organizations` in `sql`, the caller
/// declared as `caller`.
fn blocked_by<T>(
    sql: &str,
    caller: DataType,
    read: impl FnOnce(&HelperSet<'_, ParserDB>) -> T,
) -> Result<T, HelperSetError> {
    let database = postgres(sql);
    let setting = SessionSetting::value("app.user_id", SettingValue::Caller(caller))
        .expect("a custom setting name");
    let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
    answer(&database, &deployment, "authz", "blocking_organizations", &[]).map(|set| read(&set))
}

const BLOCKING_ARM: &str = "organization_blocks.organization_id WHERE principal_id = app.user_id";

#[test]
fn a_caller_read_cast_to_its_declared_type_reads_the_caller() {
    for read in [
        "current_setting('app.user_id', true)::uuid",
        "(current_setting('app.user_id', true))::UUID",
        "CAST(current_setting('app.user_id', true) AS uuid)",
        "pg_catalog.current_setting('app.user_id', true)::pg_catalog.uuid",
    ] {
        let answered = blocked_by(&blocking("''", read, ""), DataType::Uuid, render);
        assert_eq!(answered.as_deref(), Ok(BLOCKING_ARM), "{read}");
    }
    let raises = |read: &str| {
        blocked_by(&blocking("''", read, ""), DataType::Uuid, |set| {
            let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
            let GrantedTo::Request(request) = grant.granted_to() else { panic!("the caller") };
            request.raises_when_unset()
        })
    };
    assert_eq!(raises("current_setting('app.user_id')::uuid"), Ok(true));
    assert_eq!(raises("current_setting('app.user_id', true)::uuid"), Ok(false));
}

#[test]
fn a_caller_read_cast_to_any_other_type_refuses_the_answer() {
    let cast = |sql: &str, caller: DataType| {
        matches!(
            blocked_by(sql, caller, render),
            Err(HelperSetError::SettingCast { setting, .. }) if setting == "app.user_id"
        )
    };
    let read = "current_setting('app.user_id', true)::uuid";
    assert!(cast(&blocking("''", read, ""), DataType::Text), "a text caller cast to uuid");
    for read in [
        "current_setting('app.user_id', true)::varchar(8)",
        "current_setting('app.user_id', true)::text",
    ] {
        assert!(cast(&blocking("''", read, ""), DataType::Varchar(None)), "{read}");
    }
    let shadow = "CREATE DOMAIN public.uuid AS pg_catalog.uuid CHECK (VALUE IS NOT NULL);";
    assert!(
        cast(&blocking("public, pg_catalog", read, shadow), DataType::Uuid),
        "a uuid the path finds before pg_catalog's is not the declared type"
    );
    let catalog = "current_setting('app.user_id', true)::pg_catalog.uuid";
    for (path, read) in [("public", read), ("public, pg_catalog", catalog)] {
        let answered = blocked_by(&blocking(path, read, shadow), DataType::Uuid, render);
        assert_eq!(answered.as_deref(), Ok(BLOCKING_ARM), "{path}: {read}");
    }
}

/// A helper over `UUID` share links keeping those `condition` keeps, under
/// `path`.
fn presenting(path: &str, condition: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE share_links (id UUID PRIMARY KEY);
         GRANT SELECT ON share_links TO authz_owner;
         {after}
         CREATE FUNCTION authz.presented_links() RETURNS SETOF UUID
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = {path} AS $$
           SELECT l.id FROM public.share_links l WHERE {condition}
         $$;
         ALTER FUNCTION authz.presented_links() OWNER TO authz_owner;"
    )
}

/// What `authz.presented_links` in `sql` answers, read by `read`,
/// `app.link_ids` declared as `value`.
fn presented_as<T>(
    sql: &str,
    value: SettingValue,
    read: impl FnOnce(&HelperSet<'_, ParserDB>) -> T,
) -> Result<T, HelperSetError> {
    let database = postgres(sql);
    let setting = SessionSetting::value("app.link_ids", value).expect("a custom setting name");
    let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
    answer(&database, &deployment, "authz", "presented_links", &[]).map(|set| read(&set))
}

/// What `authz.presented_links` in `sql` answers, `app.link_ids` declared
/// as a set of `element` packed as `packing`.
fn presented(sql: &str, element: DataType, packing: SetPacking) -> Result<String, HelperSetError> {
    presented_as(sql, SettingValue::Set { element, packing }, render)
}

const PRESENTED_ARM: &str = "share_links.id WHERE id IN app.link_ids";

const SPLIT: &str = "string_to_array(current_setting('app.link_ids', true), ',')";
const ELEMENTS: &str = "jsonb_array_elements_text(current_setting('app.link_ids', true)::jsonb)";

/// What `presented_links` answers for `condition`, `app.link_ids` declared
/// as a set of `element`, packed as a JSON array where `condition` reads it
/// through `ELEMENTS` and delimited by commas elsewhere.
fn presented_for(condition: &str, element: DataType) -> Result<String, HelperSetError> {
    let packing = if condition.contains(ELEMENTS) {
        SetPacking::JsonArray
    } else {
        SetPacking::Delimited(",".into())
    };
    presented(&presenting("''", condition, ""), element, packing)
}

#[test]
fn a_set_unpacked_as_its_declared_element_type_reads_the_set() {
    for condition in [
        format!("l.id = ANY ({SPLIT}::uuid[])"),
        format!("l.id = ANY (CAST({SPLIT} AS UUID[]))"),
        format!("l.id = ANY (({SPLIT})::uuid[])"),
        format!("l.id = ANY (pg_catalog.{SPLIT}::pg_catalog.uuid[])"),
        format!("l.id IN (SELECT unnest({SPLIT}::uuid[]))"),
        format!("l.id = ANY (ARRAY(SELECT unnest({SPLIT}::uuid[])))"),
        format!("l.id IN (SELECT unnest({SPLIT})::uuid)"),
        format!("l.id = ANY (ARRAY(SELECT CAST(unnest({SPLIT}) AS uuid)))"),
        format!("l.id IN (SELECT {ELEMENTS}::uuid)"),
        format!("l.id = ANY (ARRAY(SELECT {ELEMENTS}::uuid))"),
    ] {
        assert_eq!(
            presented_for(&condition, DataType::Uuid).as_deref(),
            Ok(PRESENTED_ARM),
            "{condition}"
        );
    }
}

#[test]
fn a_comparison_answers_the_value_its_setting_declares() {
    let single = presenting("''", "l.id = current_setting('app.link_ids', true)::uuid", "");
    let split = presenting("''", &format!("l.id = ANY ({SPLIT}::uuid[])"), "");
    let installation =
        SettingValue::Principal { kind: "installation".into(), identity: DataType::Uuid };
    let links =
        SettingValue::Set { element: DataType::Uuid, packing: SetPacking::Delimited(",".into()) };
    for (sql, value) in [
        (&single, SettingValue::Caller(DataType::Uuid)),
        (&single, installation),
        (&single, SettingValue::Scalar(DataType::Uuid)),
        (&split, links),
    ] {
        let answered = presented_as(sql, value.clone(), |set| {
            let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
            let GrantedTo::Request(request) = grant.granted_to() else { panic!("the request") };
            request.value() == &value
        });
        assert_eq!(answered, Ok(true), "{value:?}");
    }
}

#[test]
fn a_set_unpacked_as_any_other_type_refuses_the_answer() {
    let cast = |result: Result<String, HelperSetError>| {
        matches!(
            result,
            Err(HelperSetError::SettingCast { setting, .. }) if setting == "app.link_ids"
        )
    };
    let uuids = format!("l.id = ANY ({SPLIT}::uuid[])");
    assert!(cast(presented_for(&uuids, DataType::Text)), "a text set cast to uuid[]");
    for condition in [
        format!("l.id = ANY ({SPLIT}::text[])"),
        format!("l.id IN (SELECT unnest({SPLIT}::varchar(36)[]))"),
        format!("l.id IN (SELECT unnest({SPLIT})::uuid[])"),
        format!("l.id IN (SELECT {ELEMENTS}::text)"),
    ] {
        assert!(cast(presented_for(&condition, DataType::Uuid)), "{condition}");
    }
    let shadow = "CREATE DOMAIN public.uuid AS pg_catalog.uuid CHECK (VALUE IS NOT NULL);";
    let comma = SetPacking::Delimited(",".into());
    assert!(
        cast(presented(&presenting("public, pg_catalog", &uuids, shadow), DataType::Uuid, comma)),
        "a uuid the path finds before pg_catalog's is not the declared element type"
    );
    for condition in [
        format!("l.id = ANY ({SPLIT}::uuid)"),
        format!("l.id = ANY (ARRAY(SELECT unnest({SPLIT}))::uuid[])"),
    ] {
        assert!(
            matches!(
                presented_for(&condition, DataType::Uuid),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{condition}"
        );
    }
    for condition in [
        format!("l.id IN (SELECT unnest({SPLIT}::uuid[])::uuid)"),
        "l.id IN (SELECT '00000000-0000-0000-0000-000000000000')".to_string(),
    ] {
        assert!(
            matches!(
                presented_for(&condition, DataType::Uuid),
                Err(HelperSetError::ArmShape { .. })
            ),
            "a subquery casting its elements twice or calling nothing unpacks no set: {condition}"
        );
    }
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
        "SELECT doc_id FROM members WHERE role IN ('admin') GROUP BY doc_id".into(),
        GRANT_ARM.replace("FROM members", "FROM ONLY members"),
        GRANT_ARM.replace("FROM members", "FROM members AS m (a, b, c)"),
        GRANT_ARM.replace("FROM members", "FROM member_view"),
        "SELECT id FROM docs WHERE upper(parent_id) IN (SELECT docs_with(min_role))".into(),
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

/// connetto's group tree, its helper `authz.groups_with` written as `body`.
fn groups(body: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE TABLE groups (id TEXT PRIMARY KEY, parent_id TEXT REFERENCES groups (id));
         CREATE TABLE group_grants (
           group_id TEXT NOT NULL REFERENCES groups (id),
           principal_id TEXT NOT NULL,
           role TEXT NOT NULL CHECK (role IN ('read', 'write')),
           PRIMARY KEY (group_id, principal_id)
         );
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         GRANT SELECT ON groups, group_grants TO authz_owner;
         CREATE FUNCTION authz.groups_with(min_role TEXT) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$ {body} $$;
         ALTER FUNCTION authz.groups_with(TEXT) OWNER TO authz_owner;
         {after}"
    )
}

const GROUP_GRANT_ARM: &str = "SELECT g.group_id FROM public.group_grants g \
     WHERE g.principal_id = current_setting('app.user_id', true) \
     AND (g.role = 'write' OR min_role = 'read')";
const CHILD_JOIN: &str =
    "SELECT child.id FROM public.groups child JOIN reached ON child.parent_id = reached.id";

/// A body walking from [`GROUP_GRANT_ARM`] down the tree through `term`.
fn walking(term: &str) -> String {
    format!(
        "WITH RECURSIVE reached (id) AS ({GROUP_GRANT_ARM} UNION {term}) SELECT id FROM reached"
    )
}

/// `team` under `org`, `squad` under `team`, and `other` apart.
const TREE: Rows = &[
    ("groups", &[("id", "org")]),
    ("groups", &[("id", "team"), ("parent_id", "org")]),
    ("groups", &[("id", "squad"), ("parent_id", "team")]),
    ("groups", &[("id", "other")]),
    ("group_grants", &[("group_id", "team"), ("principal_id", "alice"), ("role", "read")]),
    ("group_grants", &[("group_id", "org"), ("principal_id", "bob"), ("role", "write")]),
];

/// [`TREE`] with `org` moved under `squad`, closing a cycle.
const CYCLIC_TREE: Rows = &[
    ("groups", &[("id", "org"), ("parent_id", "squad")]),
    ("groups", &[("id", "team"), ("parent_id", "org")]),
    ("groups", &[("id", "squad"), ("parent_id", "team")]),
    ("groups", &[("id", "other")]),
    ("group_grants", &[("group_id", "team"), ("principal_id", "alice"), ("role", "read")]),
    ("group_grants", &[("group_id", "org"), ("principal_id", "bob"), ("role", "write")]),
];

/// What PostgreSQL 18.6 returns for `(rows, caller, minimum role)`, calling
/// `authz.groups_with` written as `walking(CHILD_JOIN)`.
const TREE_MEASURED: &[(Rows, &str, &str, &[&str])] = &[
    (TREE, "alice", "read", &["squad", "team"]),
    (TREE, "alice", "write", &[]),
    (TREE, "bob", "read", &["org", "squad", "team"]),
    (TREE, "bob", "write", &["org", "squad", "team"]),
    (CYCLIC_TREE, "alice", "read", &["org", "squad", "team"]),
];

fn groups_with<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
    role: &str,
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    answer(database, deployment, "authz", "groups_with", &[text(role)])
}

/// Checks `authz.groups_with`, written with the walk `term`, against what
/// PostgreSQL returns for each `(rows, caller, minimum role)` of `measured`.
fn assert_walk_measured(
    term: &str,
    measured: &[(Rows, &'static str, &str, &'static [&'static str])],
) {
    let database = postgres(&groups(&walking(term), ""));
    let deployment = deployment();
    for &(fixture, caller, role, expected) in measured {
        let set = groups_with(&database, &deployment, role).expect("answered");
        let returned: Vec<_> = evaluate(&set, Request::of(caller), fixture).into_iter().collect();
        assert_eq!(returned, expected, "{caller} calling groups_with('{role}')");
    }
}

#[test]
fn a_recursive_walk_answers_its_arms_and_the_walk_down_the_parent_column() {
    let deployment = deployment();
    for body in [
        walking(CHILD_JOIN),
        walking(
            "SELECT child.id FROM reached r JOIN public.groups AS child ON r.id = child.parent_id",
        ),
        format!(
            "WITH RECURSIVE reached AS ({GROUP_GRANT_ARM} UNION SELECT child.id \
             FROM public.groups child JOIN reached ON child.parent_id = reached.group_id) \
             SELECT reached.group_id FROM reached"
        ),
    ] {
        let database = postgres(&groups(&body, ""));
        let set = groups_with(&database, &deployment, "read").expect("answered");
        assert_eq!(
            render(&set),
            "group_grants.group_id WHERE principal_id = app.user_id \
             UNION groups.id WHERE parent_id IN itself",
            "{body}"
        );
        assert_eq!(covered(&set), Some("groups"), "{body}");
    }

    let database = postgres(&groups(&walking(CHILD_JOIN), ""));
    let set = groups_with(&database, &deployment, "read").expect("answered");
    let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
    let referenced = walk.foreign_key().referenced_table(&database).expect("resolves");
    assert_eq!(referenced.table_name(), "groups");
    assert_walk_measured(CHILD_JOIN, TREE_MEASURED);
}

#[test]
fn a_walk_over_arms_keeping_no_row_answers_no_arm() {
    let body =
        walking(CHILD_JOIN).replace("(g.role = 'write' OR min_role = 'read')", "min_role = 'read'");
    let database = postgres(&groups(&body, ""));
    let deployment = deployment();
    let set = groups_with(&database, &deployment, "write").expect("answered");
    assert!(set.arms().is_empty(), "the walk starts from nothing");
    assert_eq!(covered(&set), Some("groups"));
}

#[test]
fn a_walk_with_union_all_refuses_the_answer() {
    let body = walking(CHILD_JOIN).replace(" UNION SELECT child", " UNION ALL SELECT child");
    let database = postgres(&groups(&body, ""));
    let deployment = deployment();
    assert!(matches!(
        groups_with(&database, &deployment, "read"),
        Err(HelperSetError::UnionAllWalk { function }) if function == "authz.groups_with"
    ));
}

/// [`walking`] down [`CHILD_JOIN`] with `last` as its final query.
fn reselecting(last: &str) -> String {
    walking(CHILD_JOIN).replace("SELECT id FROM reached", last)
}

/// [`groups`] whose rows carry a kind.
fn kinded(body: &str, after: &str) -> String {
    groups(body, after).replace(
        "parent_id TEXT REFERENCES groups (id));",
        "parent_id TEXT REFERENCES groups (id), kind TEXT NOT NULL, label TEXT);",
    )
}

/// [`TREE`] with a kind on every group.
const KINDED_TREE: Rows = &[
    ("groups", &[("id", "org"), ("kind", "folder")]),
    ("groups", &[("id", "team"), ("parent_id", "org"), ("kind", "team")]),
    ("groups", &[("id", "squad"), ("parent_id", "team"), ("kind", "project")]),
    ("groups", &[("id", "other"), ("kind", "folder")]),
    ("group_grants", &[("group_id", "team"), ("principal_id", "alice"), ("role", "read")]),
    ("group_grants", &[("group_id", "org"), ("principal_id", "bob"), ("role", "write")]),
];

const KIND_RESELECTION: &str = "SELECT g.id FROM public.groups g \
     WHERE g.id IN (SELECT id FROM reached) AND (g.kind IN ('folder', 'project') OR min_role = 'read')";

/// What PostgreSQL 18.6 returns for `(caller, minimum role)`, calling
/// `authz.groups_with` written as `reselecting(KIND_RESELECTION)` over
/// [`KINDED_TREE`].
const KIND_RESELECTION_MEASURED: &[(&str, &str, &[&str])] = &[
    ("alice", "read", &["squad", "team"]),
    ("alice", "write", &[]),
    ("bob", "read", &["org", "squad", "team"]),
    ("bob", "write", &["org", "squad"]),
];

#[test]
fn a_walk_reselecting_the_keys_it_reached_answers_the_walk_alone() {
    let deployment = deployment();
    let direct = postgres(&groups(&walking(CHILD_JOIN), ""));
    let expected = render(&groups_with(&direct, &deployment, "read").expect("answered"));
    for last in [
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached)",
        "SELECT groups.id AS id FROM public.groups WHERE (groups.id IN (SELECT r.id FROM reached r))",
        "SELECT id FROM public.groups WHERE id IN (SELECT reached.id FROM reached)",
    ] {
        let database = postgres(&groups(&reselecting(last), ""));
        let set = groups_with(&database, &deployment, "read").expect("answered");
        assert_eq!(render(&set), expected, "{last}");
        assert!(set.selection().is_none(), "{last}");
        assert_eq!(covered(&set), Some("groups"), "{last}");
    }
}

#[test]
fn a_walk_reselecting_under_conditions_answers_the_rows_they_keep() {
    let database = postgres(&kinded(&reselecting(KIND_RESELECTION), ""));
    let deployment = deployment();
    let set = groups_with(&database, &deployment, "write").expect("answered");
    assert_eq!(
        render(&set),
        "group_grants.group_id WHERE principal_id = app.user_id AND role IN ('write') \
         UNION groups.id WHERE parent_id IN itself \
         INTERSECT (groups.id WHERE kind IN ('folder', 'project'))"
    );
    let set = groups_with(&database, &deployment, "read").expect("answered");
    assert!(set.selection().is_none(), "the condition holds for every row read");
    for &(caller, role, expected) in KIND_RESELECTION_MEASURED {
        let set = groups_with(&database, &deployment, role).expect("answered");
        let returned: Vec<_> =
            evaluate(&set, Request::of(caller), KINDED_TREE).into_iter().collect();
        assert_eq!(returned, expected, "{caller} calling groups_with('{role}')");
    }

    let nothing = reselecting(
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) AND min_role = 'admin'",
    );
    let database = postgres(&kinded(&nothing, ""));
    let set = groups_with(&database, &deployment, "read").expect("answered");
    assert!(set.arms().is_empty(), "the final query keeps no row");
    assert!(set.selection().is_none());
    assert_eq!(covered(&set), Some("groups"));

    let keyed = reselecting(
        "SELECT g.id FROM public.groups g \
         WHERE g.id IN (SELECT id FROM reached) AND g.id IN ('org', 'squad')",
    );
    let database = postgres(&kinded(&keyed, ""));
    let set = groups_with(&database, &deployment, "read").expect("answered");
    let returned: Vec<_> = evaluate(&set, Request::of("bob"), KINDED_TREE).into_iter().collect();
    assert_eq!(returned, ["org", "squad"], "the key is a condition beside the walk");
}

#[test]
fn a_reselection_needs_the_owner_to_read_the_columns_its_conditions_name() {
    let deployment = deployment();
    let columns = |granted: &str| {
        kinded(
            &reselecting(KIND_RESELECTION),
            &format!("GRANT SELECT ({granted}) ON groups TO authz_owner;"),
        )
        .replace("GRANT SELECT ON groups, group_grants", "GRANT SELECT ON group_grants")
    };
    let database = postgres(&columns("id, parent_id, kind"));
    groups_with(&database, &deployment, "write").expect("answered");
    let database = postgres(&columns("id, parent_id"));
    assert!(matches!(
        groups_with(&database, &deployment, "write"),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "groups"
    ));
}

#[test]
fn a_reselection_of_another_table_or_shape_refuses_the_answer() {
    let deployment = deployment();
    let with_orgs = |body: &str| {
        kinded(
            body,
            "CREATE TABLE orgs (id TEXT PRIMARY KEY);
             GRANT SELECT ON orgs TO authz_owner;",
        )
    };
    let database = postgres(&with_orgs(&reselecting(
        "SELECT o.id FROM public.orgs o WHERE o.id IN (SELECT id FROM reached)",
    )));
    assert!(matches!(
        groups_with(&database, &deployment, "read"),
        Err(HelperSetError::UncoveredReselection { function, table })
            if function == "authz.groups_with" && table == "orgs"
    ));

    for last in [
        "SELECT g.parent_id FROM public.groups g WHERE g.id IN (SELECT id FROM reached)",
        "SELECT g.id FROM public.groups g WHERE g.parent_id IN (SELECT id FROM reached)",
        "SELECT g.id FROM public.groups g WHERE g.id NOT IN (SELECT id FROM reached)",
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached WHERE id <> 'x')",
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) OR g.kind = 'team'",
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) \
         AND g.id IN (SELECT id FROM reached)",
        "SELECT g.id FROM public.groups g",
        "SELECT g.id FROM public.groups g JOIN public.orgs o ON o.id = g.id \
         WHERE g.id IN (SELECT id FROM reached)",
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) ORDER BY g.id",
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) GROUP BY g.id",
        "SELECT * FROM public.groups g WHERE g.id IN (SELECT id FROM reached)",
    ] {
        let database = postgres(&with_orgs(&reselecting(last)));
        assert!(
            matches!(
                groups_with(&database, &deployment, "read"),
                Err(HelperSetError::ArmShape { function, .. }) if function == "authz.groups_with"
            ),
            "{last}"
        );
    }

    let database = postgres(&with_orgs(&reselecting(
        "SELECT g.id FROM public.groups g WHERE g.id IN (SELECT id FROM reached) AND g.label LIKE 'x%'",
    )));
    assert!(matches!(
        groups_with(&database, &deployment, "read"),
        Err(HelperSetError::Irreducible { .. })
    ));
}

#[test]
fn a_walk_of_any_other_shape_refuses_the_answer() {
    let deployment = deployment();
    let wider = "CREATE TABLE groups (id TEXT PRIMARY KEY, parent_id TEXT REFERENCES groups (id));";
    let shape = |sql: &str| {
        let database = postgres(sql);
        matches!(
            groups_with(&database, &deployment, "read"),
            Err(HelperSetError::ArmShape { function, .. }) if function == "authz.groups_with"
        )
    };
    for body in [
        walking(
            "SELECT child.parent_id FROM public.groups child JOIN reached ON child.parent_id = reached.id",
        ),
        walking("SELECT child.id FROM public.groups child JOIN reached ON child.id = reached.id"),
        walking(
            "SELECT child.id FROM public.groups child JOIN reached ON child.org_id = reached.id",
        ),
        walking(
            "SELECT child.id FROM public.groups child JOIN reached ON child.label = reached.id",
        ),
        walking(
            "SELECT child.org_id FROM public.groups child JOIN reached ON child.id = reached.id",
        ),
        walking(
            "SELECT child.label FROM public.groups child JOIN reached ON child.id = reached.id",
        ),
        walking(&format!("{CHILD_JOIN} JOIN public.group_grants g ON g.group_id = child.id")),
        walking(&CHILD_JOIN.replace(" JOIN ", " LEFT JOIN ")),
        walking(&CHILD_JOIN.replace(" JOIN ", " GLOBAL JOIN ")),
        walking(&format!("{CHILD_JOIN} GROUP BY child.id")),
        walking(
            &CHILD_JOIN.replace("child.parent_id = reached.id", "child.parent_id <> reached.id"),
        ),
        walking(&CHILD_JOIN.replace("SELECT child.id", "SELECT id")),
        walking(
            "SELECT child.id FROM public.groups child JOIN public.groups other ON child.parent_id = other.id",
        ),
        walking("SELECT child.id FROM reached, public.groups child, public.groups other"),
        walking(
            "SELECT child.id FROM public.groups child, reached WHERE child.parent_id = reached.id",
        ),
        walking(
            "SELECT child.id FROM public.groups child WHERE child.parent_id IN (SELECT id FROM reached)",
        ),
        walking(CHILD_JOIN).replace("WITH RECURSIVE", "WITH"),
        walking(CHILD_JOIN)
            .replace("SELECT id FROM reached", "SELECT id FROM reached WHERE id <> 'x'"),
        walking(CHILD_JOIN).replace("SELECT id FROM reached", "SELECT 'x' FROM reached"),
        walking(CHILD_JOIN).replace("SELECT id FROM reached", "SELECT * FROM reached"),
        walking(CHILD_JOIN).replace(
            ") SELECT id FROM reached",
            "), other (id) AS (SELECT 1) SELECT id FROM reached",
        ),
        walking(GROUP_GRANT_ARM),
    ] {
        let sql = groups(&body, "").replace(
            wider,
            "CREATE TABLE orgs (id TEXT PRIMARY KEY);
             CREATE TABLE groups (
               id TEXT PRIMARY KEY,
               parent_id TEXT REFERENCES groups (id),
               org_id TEXT REFERENCES orgs (id),
               label TEXT
             );",
        );
        assert!(shape(&sql), "{body}");
    }
    let two_columns = groups(&walking(CHILD_JOIN), "")
        .replace("RETURNS SETOF TEXT", "RETURNS TABLE (id TEXT, role TEXT)");
    assert!(shape(&two_columns), "a walk of one column under a helper returning two");

    let shadowed = format!(
        "WITH RECURSIVE reached (id) AS (SELECT id FROM reached \
         WHERE user_id = current_setting('app.user_id', true) UNION {CHILD_JOIN}) SELECT id FROM reached"
    );
    let sql = groups(
        &shadowed,
        "CREATE TABLE reached (id TEXT REFERENCES groups (id), user_id TEXT);
         GRANT SELECT ON reached TO authz_owner;",
    )
    .replace("search_path = ''", "search_path = public");
    assert!(shape(&sql), "the walk's own name in its first arm reads the walk");

    let joined = format!(
        "WITH RECURSIVE reached (id) AS (SELECT s.reached_id FROM seeds s \
         JOIN reached ON reached.id = s.reached_id \
         WHERE s.user_id = current_setting('app.user_id', true) UNION {CHILD_JOIN}) \
         SELECT id FROM reached"
    );
    let sql = groups(
        &joined,
        "CREATE TABLE reached (id TEXT PRIMARY KEY REFERENCES groups (id));
         CREATE TABLE seeds (reached_id TEXT REFERENCES reached (id), user_id TEXT);
         GRANT SELECT ON reached, seeds TO authz_owner;",
    )
    .replace("search_path = ''", "search_path = public");
    assert!(shape(&sql), "the walk's own name joined in its first arm reads the walk");
}

#[test]
fn a_walk_needs_the_owner_to_read_the_key_and_the_parent_column() {
    let deployment = deployment();
    let columns = |granted: &str| {
        groups(&walking(CHILD_JOIN), &format!("GRANT SELECT ({granted}) ON groups TO authz_owner;"))
            .replace("GRANT SELECT ON groups, group_grants", "GRANT SELECT ON group_grants")
    };
    let database = postgres(&columns("id, parent_id"));
    groups_with(&database, &deployment, "read").expect("answered");
    let database = postgres(&columns("id"));
    assert!(matches!(
        groups_with(&database, &deployment, "read"),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "groups"
    ));

    let bound = groups(&walking(CHILD_JOIN), "ALTER TABLE groups ENABLE ROW LEVEL SECURITY;")
        .replace("NOLOGIN BYPASSRLS", "NOLOGIN");
    let database = postgres(&bound);
    assert!(matches!(
        groups_with(&database, &deployment, "read"),
        Err(HelperSetError::NotExempt { table, .. }) if table == "groups"
    ));
}

const NAMESPACED: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TABLE grantees (
  namespace_id TEXT NOT NULL, id TEXT NOT NULL, kind TEXT NOT NULL,
  PRIMARY KEY (namespace_id, id)
);
CREATE TABLE teams (
  namespace_id TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY (namespace_id, id),
  FOREIGN KEY (namespace_id, id) REFERENCES grantees (namespace_id, id)
);
CREATE TABLE team_members (
  team_namespace_id TEXT NOT NULL, team_id TEXT NOT NULL,
  user_namespace_id TEXT NOT NULL, user_id TEXT NOT NULL,
  PRIMARY KEY (team_namespace_id, team_id, user_namespace_id, user_id),
  FOREIGN KEY (team_namespace_id, team_id) REFERENCES teams (namespace_id, id),
  FOREIGN KEY (user_namespace_id, user_id) REFERENCES grantees (namespace_id, id)
);
CREATE TABLE team_parents (
  child_namespace_id TEXT NOT NULL, child_id TEXT NOT NULL,
  parent_namespace_id TEXT NOT NULL, parent_id TEXT NOT NULL,
  PRIMARY KEY (child_namespace_id, child_id),
  FOREIGN KEY (child_namespace_id, child_id) REFERENCES teams (namespace_id, id),
  FOREIGN KEY (parent_namespace_id, parent_id) REFERENCES teams (namespace_id, id)
);
CREATE TABLE shared_parents (
  namespace_id TEXT NOT NULL, child_id TEXT NOT NULL, parent_id TEXT NOT NULL,
  PRIMARY KEY (namespace_id, child_id),
  FOREIGN KEY (namespace_id, child_id) REFERENCES teams (namespace_id, id),
  FOREIGN KEY (namespace_id, parent_id) REFERENCES teams (namespace_id, id)
);
GRANT SELECT ON grantees, teams, team_members, team_parents, shared_parents TO authz_owner;
";

const NAMESPACED_ARMS: &str = "SELECT grantee.namespace_id, grantee.id FROM public.grantees grantee \
     WHERE grantee.id = current_setting('app.user_id', true) AND grantee.kind = 'user' \
     UNION SELECT member.team_namespace_id, member.team_id FROM public.team_members member \
     WHERE member.user_id = current_setting('app.user_id', true)";
const PAIR_UP: &str = "SELECT parent.parent_namespace_id, parent.parent_id \
     FROM public.team_parents parent JOIN reached \
     ON (parent.child_namespace_id, parent.child_id) = (reached.namespace_id, reached.id)";
const PAIR_DOWN: &str = "SELECT parent.child_namespace_id, parent.child_id \
     FROM public.team_parents parent JOIN reached \
     ON (parent.parent_namespace_id, parent.parent_id) = (reached.namespace_id, reached.id)";
const PAIR_LAST: &str = "SELECT reached.namespace_id, reached.id FROM reached";

/// A body walking from [`NAMESPACED_ARMS`] along the team tree through
/// `term`.
fn pair_walking(term: &str) -> String {
    format!(
        "WITH RECURSIVE reached (namespace_id, id) AS ({NAMESPACED_ARMS} UNION {term}) {PAIR_LAST}"
    )
}

/// [`NAMESPACED`] with `authz.caller_grantees()` written as `body`.
fn namespaced(body: &str) -> String {
    format!(
        "{NAMESPACED}
         CREATE FUNCTION authz.caller_grantees() RETURNS TABLE (namespace_id TEXT, id TEXT)
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$ {body} $$;
         ALTER FUNCTION authz.caller_grantees() OWNER TO authz_owner;"
    )
}

const NAMESPACED_TEAMS: Rows = &[
    ("grantees", &[("namespace_id", "a0"), ("id", "u1"), ("kind", "user")]),
    ("grantees", &[("namespace_id", "b0"), ("id", "u2"), ("kind", "user")]),
    ("grantees", &[("namespace_id", "a1"), ("id", "t11"), ("kind", "team")]),
    ("grantees", &[("namespace_id", "a1"), ("id", "t12"), ("kind", "team")]),
    ("grantees", &[("namespace_id", "a1"), ("id", "t14"), ("kind", "team")]),
    ("grantees", &[("namespace_id", "b1"), ("id", "t11"), ("kind", "team")]),
    ("grantees", &[("namespace_id", "b1"), ("id", "t13"), ("kind", "team")]),
    ("teams", &[("namespace_id", "a1"), ("id", "t11")]),
    ("teams", &[("namespace_id", "a1"), ("id", "t12")]),
    ("teams", &[("namespace_id", "a1"), ("id", "t14")]),
    ("teams", &[("namespace_id", "b1"), ("id", "t11")]),
    ("teams", &[("namespace_id", "b1"), ("id", "t13")]),
    (
        "team_members",
        &[
            ("team_namespace_id", "a1"),
            ("team_id", "t11"),
            ("user_namespace_id", "a0"),
            ("user_id", "u1"),
        ],
    ),
    (
        "team_members",
        &[
            ("team_namespace_id", "b1"),
            ("team_id", "t13"),
            ("user_namespace_id", "b0"),
            ("user_id", "u2"),
        ],
    ),
    (
        "team_parents",
        &[
            ("child_namespace_id", "a1"),
            ("child_id", "t11"),
            ("parent_namespace_id", "a1"),
            ("parent_id", "t12"),
        ],
    ),
    (
        "team_parents",
        &[
            ("child_namespace_id", "a1"),
            ("child_id", "t12"),
            ("parent_namespace_id", "a1"),
            ("parent_id", "t14"),
        ],
    ),
    (
        "team_parents",
        &[
            ("child_namespace_id", "b1"),
            ("child_id", "t11"),
            ("parent_namespace_id", "b1"),
            ("parent_id", "t13"),
        ],
    ),
    ("shared_parents", &[("namespace_id", "a1"), ("child_id", "t11"), ("parent_id", "t12")]),
    ("shared_parents", &[("namespace_id", "a1"), ("child_id", "t12"), ("parent_id", "t14")]),
    ("shared_parents", &[("namespace_id", "b1"), ("child_id", "t11"), ("parent_id", "t13")]),
];

/// What PostgreSQL 18.7 returns for a caller from `authz.caller_grantees()`
/// over [`NAMESPACED_TEAMS`] written as `pair_walking(PAIR_UP)`, then as
/// `pair_walking(PAIR_DOWN)`. The walk up over `shared_parents` returns what
/// the walk up over `team_parents` does.
type PairsMeasured = (&'static str, &'static [[&'static str; 2]], &'static [[&'static str; 2]]);

const NAMESPACED_MEASURED: &[PairsMeasured] = &[
    (
        "u1",
        &[["a0", "u1"], ["a1", "t11"], ["a1", "t12"], ["a1", "t14"]],
        &[["a0", "u1"], ["a1", "t11"]],
    ),
    ("u2", &[["b0", "u2"], ["b1", "t13"]], &[["b0", "u2"], ["b1", "t11"], ["b1", "t13"]]),
    ("zed", &[], &[]),
];

fn caller_grantees<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    answer(database, deployment, "authz", "caller_grantees", &[])
}

/// Checks `set`, written with `body` walking in `direction`, against what
/// PostgreSQL returns for each caller of [`NAMESPACED_MEASURED`].
fn assert_pairs_measured(set: &HelperSet<'_, ParserDB>, direction: WalkDirection, body: &str) {
    for &(caller, up, down) in NAMESPACED_MEASURED {
        let expected = match direction {
            WalkDirection::Up => up,
            WalkDirection::Down => down,
        };
        let expected: Vec<Vec<&str>> = expected.iter().map(|pair| pair.to_vec()).collect();
        let returned: Vec<_> =
            helper_rows(set, Request::of(caller), NAMESPACED_TEAMS).into_iter().collect();
        assert_eq!(returned, expected, "{caller} walking {body}");
    }
}

#[test]
fn a_walk_over_a_composite_key_follows_each_edge_by_the_whole_key() {
    let deployment = deployment();
    let up = "team_parents.(parent_namespace_id, parent_id) \
         WHERE (child_namespace_id, child_id) IN itself";
    let down = "team_parents.(child_namespace_id, child_id) \
         WHERE (parent_namespace_id, parent_id) IN itself";
    let shared =
        "shared_parents.(namespace_id, parent_id) WHERE (namespace_id, child_id) IN itself";
    let team_parents = ("(child_namespace_id, child_id)", "(parent_namespace_id, parent_id)");
    for (body, rendered, direction, (child, parent)) in [
        (pair_walking(PAIR_UP), up, WalkDirection::Up, team_parents),
        (
            pair_walking(&PAIR_UP.replace(
                "(parent.child_namespace_id, parent.child_id) = (reached.namespace_id, reached.id)",
                "parent.child_namespace_id = reached.namespace_id AND parent.child_id = reached.id",
            )),
            up,
            WalkDirection::Up,
            team_parents,
        ),
        (
            pair_walking(
                "SELECT parent.parent_namespace_id, parent.parent_id FROM reached r \
                 JOIN public.team_parents AS parent \
                 ON (r.namespace_id, r.id) = (parent.child_namespace_id, parent.child_id)",
            ),
            up,
            WalkDirection::Up,
            team_parents,
        ),
        (
            pair_walking(PAIR_UP)
                .replace("reached (namespace_id, id) AS", "reached AS")
                .replace(PAIR_LAST, "SELECT namespace_id, id FROM reached"),
            up,
            WalkDirection::Up,
            team_parents,
        ),
        (
            pair_walking(
                "SELECT step.parent_namespace_id, step.parent_id FROM reached CROSS JOIN LATERAL \
                 (SELECT parent.parent_namespace_id, parent.parent_id FROM public.team_parents parent \
                 WHERE (parent.child_namespace_id, parent.child_id) = (reached.namespace_id, reached.id)) step",
            ),
            up,
            WalkDirection::Up,
            team_parents,
        ),
        (
            pair_walking(
                "SELECT s.ns, s.node FROM reached, LATERAL \
                 (SELECT parent_namespace_id, parent_id FROM public.team_parents \
                 WHERE child_id = reached.id AND child_namespace_id = reached.namespace_id) AS s (ns, node)",
            ),
            up,
            WalkDirection::Up,
            team_parents,
        ),
        (pair_walking(PAIR_DOWN), down, WalkDirection::Down, team_parents),
        (
            pair_walking(
                "SELECT parent.namespace_id, parent.parent_id FROM public.shared_parents parent \
                 JOIN reached ON (parent.namespace_id, parent.child_id) = (reached.namespace_id, reached.id)",
            ),
            shared,
            WalkDirection::Up,
            ("(namespace_id, child_id)", "(namespace_id, parent_id)"),
        ),
    ] {
        let database = postgres(&namespaced(&body));
        let set = caller_grantees(&database, &deployment).expect("answered");
        assert_eq!(
            render(&set),
            format!(
                "grantees.(namespace_id, id) WHERE id = app.user_id AND kind IN ('user') \
                 UNION team_members.(team_namespace_id, team_id) WHERE user_id = app.user_id \
                 UNION {rendered}"
            ),
            "{body}"
        );
        assert_eq!(covered(&set), None, "{body}");
        let HelperArm::Walk(walk) = &set.arms()[2] else { panic!("the third arm is the walk") };
        assert_eq!(walk.direction(), direction, "{body}");
        assert_eq!(walk.tree().table_name(), "teams", "{body}");
        assert_eq!(render_columns(walk.key()), "(namespace_id, id)", "{body}");
        assert_eq!(render_columns(walk.child()), child, "{body}");
        assert_eq!(render_columns(walk.parent()), parent, "{body}");
        let referenced = walk.foreign_key().referenced_table(&database).expect("resolves");
        assert_eq!(referenced.table_name(), "teams", "{body}");
        assert_pairs_measured(&set, direction, &body);
    }
}

#[test]
fn a_walk_pairing_its_columns_with_other_key_columns_refuses_the_answer() {
    let deployment = deployment();
    let returning = |returned: &str| {
        pair_walking(&PAIR_UP.replace("parent.parent_namespace_id, parent.parent_id", returned))
    };
    for body in [
        returning("parent.parent_id, parent.parent_namespace_id"),
        returning("parent.child_namespace_id, parent.parent_id"),
        returning("parent.child_namespace_id, parent.child_id"),
        pair_walking(
            &PAIR_UP.replace(
                "(reached.namespace_id, reached.id)",
                "(reached.id, reached.namespace_id)",
            ),
        ),
        pair_walking(&PAIR_UP.replace(
            "(parent.child_namespace_id, parent.child_id) = (reached.namespace_id, reached.id)",
            "parent.child_id = reached.id",
        )),
        pair_walking(&PAIR_UP.replace(") = (reached", ") <> (reached")),
        pair_walking(PAIR_UP)
            .replace(PAIR_LAST, "SELECT reached.id, reached.namespace_id FROM reached"),
        pair_walking(PAIR_UP)
            .replace(PAIR_LAST, "SELECT reached.namespace_id, reached.namespace_id FROM reached"),
    ] {
        let database = postgres(&namespaced(&body));
        assert!(
            matches!(
                caller_grantees(&database, &deployment),
                Err(HelperSetError::ArmShape { function, .. }) if function == "authz.caller_grantees"
            ),
            "{body}"
        );
    }
}

#[test]
fn a_walk_over_a_composite_key_needs_the_owner_to_read_every_edge_column() {
    let deployment = deployment();
    let columns = |granted: &str| {
        namespaced(&pair_walking(PAIR_UP)).replace(
            "GRANT SELECT ON grantees, teams, team_members, team_parents, shared_parents",
            &format!(
                "GRANT SELECT ({granted}) ON team_parents TO authz_owner;
                 GRANT SELECT ON grantees, teams, team_members, shared_parents"
            ),
        )
    };
    let database =
        postgres(&columns("child_namespace_id, child_id, parent_namespace_id, parent_id"));
    caller_grantees(&database, &deployment).expect("answered");
    for granted in
        ["child_id, parent_namespace_id, parent_id", "child_namespace_id, child_id, parent_id"]
    {
        let database = postgres(&columns(granted));
        assert!(
            matches!(
                caller_grantees(&database, &deployment),
                Err(HelperSetError::MissingSelect { table, .. }) if table == "team_parents"
            ),
            "{granted}"
        );
    }
}

/// Share links a request presents and app installations capped per table and
/// command, each read by a helper comparing with no caller.
const REQUEST_VALUES: &str = "
CREATE ROLE app NOLOGIN;
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TABLE share_links (id TEXT PRIMARY KEY, revoked BOOLEAN NOT NULL DEFAULT FALSE);
CREATE TABLE installation_permissions (
  installation_id TEXT NOT NULL,
  table_name TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN ('select', 'insert', 'update', 'delete')),
  PRIMARY KEY (installation_id, table_name, action)
);
GRANT SELECT ON share_links, installation_permissions TO authz_owner;
CREATE FUNCTION authz.presented_links() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.id FROM public.share_links l
  WHERE l.id = ANY (string_to_array(current_setting('app.link_ids', true), ',')) AND l.revoked = FALSE
$$;
CREATE FUNCTION authz.listed_links() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.id FROM public.share_links l
  WHERE l.id IN (SELECT unnest(string_to_array(current_setting('app.link_ids', true), ','))) AND l.revoked = FALSE
$$;
CREATE FUNCTION authz.json_links() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.id FROM public.share_links l
  WHERE l.id IN (SELECT jsonb_array_elements_text(current_setting('app.link_list', true)::jsonb)) AND l.revoked = FALSE
$$;
CREATE FUNCTION authz.json_array_links() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.id FROM public.share_links l
  WHERE l.id = ANY (ARRAY(SELECT jsonb_array_elements_text(CAST(current_setting('app.link_list', true) AS jsonb)))) AND l.revoked = FALSE
$$;
CREATE FUNCTION authz.installations_allowing(target TEXT, command TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT ip.installation_id FROM public.installation_permissions ip WHERE ip.table_name = target AND ip.action = command
$$;
ALTER FUNCTION authz.presented_links() OWNER TO authz_owner;
ALTER FUNCTION authz.listed_links() OWNER TO authz_owner;
ALTER FUNCTION authz.json_links() OWNER TO authz_owner;
ALTER FUNCTION authz.json_array_links() OWNER TO authz_owner;
ALTER FUNCTION authz.installations_allowing(TEXT, TEXT) OWNER TO authz_owner;
";

/// The rows [`REQUEST_VALUES`] was measured over.
const REQUEST_VALUE_ROWS: Rows = &[
    ("share_links", &[("id", "l1"), ("revoked", "false")]),
    ("share_links", &[("id", "l2"), ("revoked", "true")]),
    ("share_links", &[("id", "l3"), ("revoked", "false")]),
    (
        "installation_permissions",
        &[("installation_id", "i1"), ("table_name", "docs"), ("action", "select")],
    ),
    (
        "installation_permissions",
        &[("installation_id", "i1"), ("table_name", "docs"), ("action", "insert")],
    ),
    (
        "installation_permissions",
        &[("installation_id", "i2"), ("table_name", "docs"), ("action", "select")],
    ),
    (
        "installation_permissions",
        &[("installation_id", "i2"), ("table_name", "notes"), ("action", "select")],
    ),
    (
        "installation_permissions",
        &[("installation_id", "i3"), ("table_name", "notes"), ("action", "delete")],
    ),
];

/// What PostgreSQL 18.6 returns for `(helper, the elements the request sets,
/// returned)`, as `app` over [`REQUEST_VALUE_ROWS`]. No elements is the
/// setting set to `''` or `'[]'`, and a request setting neither leaves it
/// unset, which `current_setting(name, true)` reads as `NULL`.
const SET_MEASURED: &[(&str, Request, &[&str])] = &[
    ("presented_links", Request { caller: None, values: &[("app.link_ids", &[])] }, &[]),
    ("presented_links", Request { caller: None, values: &[("app.link_ids", &["l1"])] }, &["l1"]),
    (
        "presented_links",
        Request { caller: None, values: &[("app.link_ids", &["l1", "l2"])] },
        &["l1"],
    ),
    (
        "presented_links",
        Request { caller: None, values: &[("app.link_ids", &["l2", "l3", "lx"])] },
        &["l3"],
    ),
    (
        "presented_links",
        Request { caller: None, values: &[("app.link_ids", &["l3", "l1"])] },
        &["l1", "l3"],
    ),
    ("presented_links", Request::of(""), &[]),
    ("listed_links", Request { caller: None, values: &[("app.link_ids", &["l1", "l2"])] }, &["l1"]),
    (
        "listed_links",
        Request { caller: None, values: &[("app.link_ids", &["l3", "l1"])] },
        &["l1", "l3"],
    ),
    ("listed_links", Request::of(""), &[]),
    ("json_links", Request { caller: None, values: &[("app.link_list", &[])] }, &[]),
    ("json_links", Request { caller: None, values: &[("app.link_list", &["l1", "l2"])] }, &["l1"]),
    (
        "json_links",
        Request { caller: None, values: &[("app.link_list", &["l3", "lx", "l1"])] },
        &["l1", "l3"],
    ),
    ("json_links", Request::of(""), &[]),
    ("json_array_links", Request { caller: None, values: &[("app.link_list", &[])] }, &[]),
    (
        "json_array_links",
        Request { caller: None, values: &[("app.link_list", &["l1", "l2"])] },
        &["l1"],
    ),
    (
        "json_array_links",
        Request { caller: None, values: &[("app.link_list", &["l3", "lx", "l1"])] },
        &["l1", "l3"],
    ),
    ("json_array_links", Request::of(""), &[]),
];

/// What PostgreSQL 18.6 returns for `installations_allowing(table, command)`
/// over [`REQUEST_VALUE_ROWS`].
const INSTALLATIONS_ALLOWING_MEASURED: &[(&str, &str, &[&str])] = &[
    ("docs", "select", &["i1", "i2"]),
    ("docs", "insert", &["i1"]),
    ("notes", "delete", &["i3"]),
    ("notes", "update", &[]),
    ("notes", "select", &["i2"]),
];

/// A deployment declaring the caller, a scalar installation and the presented
/// links packed both ways.
fn request_deployment() -> Deployment {
    let setting = |name: &str, value: SettingValue| {
        SessionSetting::value(name, value).expect("a custom setting name").may_be_unset()
    };
    Deployment::new(
        "app",
        ["app"],
        [
            setting("app.user_id", SettingValue::Caller(DataType::Text)),
            setting("app.installation_id", SettingValue::Scalar(DataType::Text)),
            setting("app.link_ids", delimited(",")),
            setting(
                "app.link_list",
                SettingValue::Set { element: DataType::Text, packing: SetPacking::JsonArray },
            ),
        ],
    )
    .expect("a valid deployment")
}

#[test]
fn a_set_arm_returns_what_postgres_returns_for_every_presented_set() {
    let database = postgres(REQUEST_VALUES);
    let deployment = request_deployment();
    for (helper, setting) in [
        ("presented_links", "app.link_ids"),
        ("listed_links", "app.link_ids"),
        ("json_links", "app.link_list"),
        ("json_array_links", "app.link_list"),
    ] {
        let set = answer(&database, &deployment, "authz", helper, &[]).expect("answered");
        assert_eq!(
            render(&set),
            format!("share_links.id WHERE id IN {setting} AND revoked IN (false)"),
            "{helper}"
        );
        assert_eq!(covered(&set), Some("share_links"), "{helper}");
        let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
        assert!(
            matches!(grant.granted_to(), GrantedTo::Request(request) if !request.raises_when_unset())
        );
    }
    for &(helper, request, expected) in SET_MEASURED {
        let set = answer(&database, &deployment, "authz", helper, &[]).expect("answered");
        let returned: Vec<_> = evaluate(&set, request, REQUEST_VALUE_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{helper} for {:?}", request.values);
    }
}

#[test]
fn a_caller_free_arm_returns_what_postgres_returns_for_every_argument() {
    let database = postgres(REQUEST_VALUES);
    let deployment = request_deployment();
    for &(table, command, expected) in INSTALLATIONS_ALLOWING_MEASURED {
        let set = answer(
            &database,
            &deployment,
            "authz",
            "installations_allowing",
            &[text(table), text(command)],
        )
        .expect("answered");
        assert_eq!(
            render(&set),
            format!(
                "installation_permissions.installation_id WHERE table_name IN ('{table}') AND action IN ('{command}')"
            )
        );
        let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
        assert!(
            matches!(grant.granted_to(), GrantedTo::Anyone),
            "no column is compared with the request"
        );
        let returned: Vec<_> =
            evaluate(&set, Request::of(""), REQUEST_VALUE_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "installations_allowing('{table}', '{command}')");
    }
}

const PARENT_JOIN: &str =
    "SELECT parent.parent_id FROM public.groups parent JOIN reached ON parent.id = reached.id";

/// What PostgreSQL 18.6 returns for `(rows, caller, minimum role)`, calling
/// `authz.groups_with` written as `walking(PARENT_JOIN)`, the `NULL` a root
/// adds left out.
const TREE_UP_MEASURED: &[(Rows, &str, &str, &[&str])] = &[
    (TREE, "alice", "read", &["org", "team"]),
    (TREE, "alice", "write", &[]),
    (TREE, "bob", "read", &["org"]),
    (TREE, "bob", "write", &["org"]),
    (CYCLIC_TREE, "alice", "read", &["org", "squad", "team"]),
    (CYCLIC_TREE, "bob", "write", &["org", "squad", "team"]),
];

#[test]
fn a_recursive_walk_answers_its_arms_and_the_walk_up_the_parent_column() {
    let deployment = deployment();
    for body in [
        walking(PARENT_JOIN),
        walking(
            "SELECT parent.parent_id FROM reached r JOIN public.groups AS parent ON r.id = parent.id",
        ),
    ] {
        let database = postgres(&groups(&body, ""));
        let set = groups_with(&database, &deployment, "read").expect("answered");
        assert_eq!(
            render(&set),
            "group_grants.group_id WHERE principal_id = app.user_id \
             UNION groups.parent_id WHERE id IN itself",
            "{body}"
        );
        assert_eq!(covered(&set), Some("groups"), "{body}");
        let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
        assert_eq!(walk.direction(), WalkDirection::Up);
        assert_eq!(render_columns(walk.parent()), "parent_id");
    }

    assert_walk_measured(PARENT_JOIN, TREE_UP_MEASURED);
}

/// One grantee per grant row, organizations and a team tree, the helper
/// `authz.caller_grantees()` returning every grantee the caller is and
/// `authz.projects_granted(min_role)` the projects granted to any of them.
const GRANTEES: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TABLE grantees (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('user', 'team', 'organization', 'everyone', 'signed_in'))
);
CREATE TABLE organizations (id TEXT PRIMARY KEY REFERENCES grantees (id));
CREATE TABLE memberships (
  organization_id TEXT NOT NULL REFERENCES organizations (id),
  user_id TEXT NOT NULL REFERENCES grantees (id),
  state TEXT NOT NULL CHECK (state IN ('active', 'deactivated')),
  PRIMARY KEY (organization_id, user_id)
);
CREATE TABLE teams (id TEXT PRIMARY KEY REFERENCES grantees (id), parent_id TEXT REFERENCES teams (id));
CREATE TABLE team_members (
  team_id TEXT NOT NULL REFERENCES teams (id),
  user_id TEXT NOT NULL REFERENCES grantees (id),
  PRIMARY KEY (team_id, user_id)
);
CREATE TABLE projects (id TEXT PRIMARY KEY);
CREATE TABLE project_grants (
  project_id TEXT NOT NULL REFERENCES projects (id),
  grantee_id TEXT NOT NULL REFERENCES grantees (id),
  role TEXT NOT NULL CHECK (role IN ('read', 'admin')),
  PRIMARY KEY (project_id, grantee_id)
);
GRANT SELECT ON grantees, memberships, teams, team_members, project_grants TO authz_owner;
CREATE FUNCTION authz.caller_grantees() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  WITH RECURSIVE reached (id) AS (
    SELECT g.id FROM public.grantees g WHERE g.id = current_setting('app.user_id', true)
    UNION
    SELECT g.id FROM public.grantees g WHERE g.kind = 'everyone'
    UNION
    SELECT g.id FROM public.grantees g
    WHERE g.kind = 'signed_in' AND current_setting('app.user_id', true) <> ''
    UNION
    SELECT m.organization_id FROM public.memberships m
    WHERE m.user_id = current_setting('app.user_id', true) AND m.state = 'active'
    UNION
    SELECT tm.team_id FROM public.team_members tm
    WHERE tm.user_id = current_setting('app.user_id', true)
    UNION
    SELECT t.parent_id FROM public.teams t JOIN reached ON t.id = reached.id
  ) SELECT id FROM reached
$$;
CREATE FUNCTION authz.projects_granted(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT pg.project_id FROM public.project_grants pg
  WHERE pg.grantee_id IN (SELECT authz.caller_grantees())
    AND (pg.role = 'admin' OR min_role = 'read')
$$;
ALTER FUNCTION authz.caller_grantees() OWNER TO authz_owner;
ALTER FUNCTION authz.projects_granted(TEXT) OWNER TO authz_owner;
";

/// `alice` an active member of `org`, `bob` a deactivated one on team
/// `platform` under `eng`, `carol` on `core` under `platform`, and `dave`
/// on no team.
const GRANTEE_ROWS: Rows = &[
    ("grantees", &[("id", "alice"), ("kind", "user")]),
    ("grantees", &[("id", "bob"), ("kind", "user")]),
    ("grantees", &[("id", "carol"), ("kind", "user")]),
    ("grantees", &[("id", "dave"), ("kind", "user")]),
    ("grantees", &[("id", "everyone"), ("kind", "everyone")]),
    ("grantees", &[("id", "signed_in"), ("kind", "signed_in")]),
    ("grantees", &[("id", "org"), ("kind", "organization")]),
    ("grantees", &[("id", "eng"), ("kind", "team")]),
    ("grantees", &[("id", "platform"), ("kind", "team")]),
    ("grantees", &[("id", "core"), ("kind", "team")]),
    ("grantees", &[("id", "other"), ("kind", "team")]),
    ("organizations", &[("id", "org")]),
    ("memberships", &[("organization_id", "org"), ("user_id", "alice"), ("state", "active")]),
    ("memberships", &[("organization_id", "org"), ("user_id", "bob"), ("state", "deactivated")]),
    ("teams", &[("id", "eng")]),
    ("teams", &[("id", "platform"), ("parent_id", "eng")]),
    ("teams", &[("id", "core"), ("parent_id", "platform")]),
    ("teams", &[("id", "other")]),
    ("team_members", &[("team_id", "core"), ("user_id", "carol")]),
    ("team_members", &[("team_id", "platform"), ("user_id", "bob")]),
    ("project_grants", &[("project_id", "p_alice"), ("grantee_id", "alice"), ("role", "admin")]),
    (
        "project_grants",
        &[("project_id", "p_everyone"), ("grantee_id", "everyone"), ("role", "read")],
    ),
    (
        "project_grants",
        &[("project_id", "p_signed_in"), ("grantee_id", "signed_in"), ("role", "read")],
    ),
    ("project_grants", &[("project_id", "p_org"), ("grantee_id", "org"), ("role", "admin")]),
    ("project_grants", &[("project_id", "p_eng"), ("grantee_id", "eng"), ("role", "read")]),
    (
        "project_grants",
        &[("project_id", "p_platform"), ("grantee_id", "platform"), ("role", "admin")],
    ),
    ("project_grants", &[("project_id", "p_core"), ("grantee_id", "core"), ("role", "read")]),
    ("project_grants", &[("project_id", "p_other"), ("grantee_id", "other"), ("role", "admin")]),
];

/// A caller setting reading the caller, `None` for one never set, and the
/// sets three helper calls return for it.
type GranteesMeasured = (
    Option<&'static str>,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

/// What PostgreSQL 18.6 returns over [`GRANTEE_ROWS`] for
/// `authz.caller_grantees()`, the `NULL` a root team adds left out, then for
/// `authz.projects_granted('read')` and `authz.projects_granted('admin')`.
const GRANTEES_MEASURED: &[GranteesMeasured] = &[
    (
        Some("alice"),
        &["alice", "everyone", "org", "signed_in"],
        &["p_alice", "p_everyone", "p_org", "p_signed_in"],
        &["p_alice", "p_org"],
    ),
    (
        Some("bob"),
        &["bob", "eng", "everyone", "platform", "signed_in"],
        &["p_eng", "p_everyone", "p_platform", "p_signed_in"],
        &["p_platform"],
    ),
    (
        Some("carol"),
        &["carol", "core", "eng", "everyone", "platform", "signed_in"],
        &["p_core", "p_eng", "p_everyone", "p_platform", "p_signed_in"],
        &["p_platform"],
    ),
    (Some("dave"), &["dave", "everyone", "signed_in"], &["p_everyone", "p_signed_in"], &[]),
    (Some("zed"), &["everyone", "signed_in"], &["p_everyone", "p_signed_in"], &[]),
    (Some(""), &["everyone"], &["p_everyone"], &[]),
    (None, &["everyone"], &["p_everyone"], &[]),
];

const CALLER_GRANTEES: &str = "grantees.id WHERE id = app.user_id \
     UNION grantees.id WHERE kind IN ('everyone') \
     UNION grantees.id WHERE app.user_id IS SET AND kind IN ('signed_in') \
     UNION memberships.organization_id WHERE user_id = app.user_id AND state IN ('active') \
     UNION team_members.team_id WHERE user_id = app.user_id \
     UNION teams.parent_id WHERE id IN itself";

#[test]
fn a_grant_arm_compares_its_grantee_with_every_grantee_the_caller_is() {
    let database = postgres(GRANTEES);
    let deployment = deployment();
    let grantees =
        answer(&database, &deployment, "authz", "caller_grantees", &[]).expect("answered");
    assert_eq!(render(&grantees), CALLER_GRANTEES);
    assert_eq!(covered(&grantees), None, "the arms return keys of four tables");
    let HelperArm::Grant(everyone) = &grantees.arms()[1] else { panic!("a grant arm") };
    assert!(matches!(everyone.granted_to(), GrantedTo::Anyone));

    let admin = answer(&database, &deployment, "authz", "projects_granted", &[text("admin")])
        .expect("answered");
    assert_eq!(
        render(&admin),
        format!(
            "project_grants.project_id WHERE grantee_id IN authz.caller_grantees() \
             {{{CALLER_GRANTEES}}} AND role IN ('admin')"
        )
    );
    let HelperArm::Grant(grant) = &admin.arms()[0] else { panic!("a grant arm") };
    let GrantedTo::Set(set) = grant.granted_to() else { panic!("granted to a set") };
    let referenced = set.foreign_key().expect("typed").referenced_table(&database).expect("found");
    assert_eq!(referenced.table_name(), "grantees");
    assert_eq!(covered(&admin), Some("projects"));

    let read = answer(&database, &deployment, "authz", "projects_granted", &[text("read")])
        .expect("answered");
    for &(caller, expected_grantees, expected_read, expected_admin) in GRANTEES_MEASURED {
        let request = Request { caller, values: &[] };
        for (set, expected, called) in [
            (&grantees, expected_grantees, "caller_grantees()"),
            (&read, expected_read, "projects_granted('read')"),
            (&admin, expected_admin, "projects_granted('admin')"),
        ] {
            let returned: Vec<_> = evaluate(set, request, GRANTEE_ROWS).into_iter().collect();
            assert_eq!(returned, expected, "{caller:?} calling {called}");
        }
    }

    let beside_caller = GRANTEES.replace(
        "AND (pg.role = 'admin' OR min_role = 'read')",
        "AND pg.grantee_id = current_setting('app.user_id', true)",
    );
    assert_ne!(beside_caller, GRANTEES, "the condition is replaced");
    let capped = answer(
        &postgres(&beside_caller),
        &deployment,
        "authz",
        "projects_granted",
        &[text("read")],
    )
    .map(|set| render(&set));
    assert!(
        capped.as_deref().is_ok_and(|capped| capped.starts_with(
            "project_grants.project_id WHERE grantee_id = app.user_id AND (grantee_id) IN authz.caller_grantees() {"
        )),
        "{capped:?}"
    );
    let twice = beside_caller.replace(
        "pg.grantee_id IN (SELECT authz.caller_grantees())",
        "pg.project_id = current_setting('app.user_id', true)",
    );
    assert_ne!(twice, beside_caller, "the condition is replaced");
    assert!(
        matches!(
            answer(&postgres(&twice), &deployment, "authz", "projects_granted", &[text("read")]),
            Err(HelperSetError::ArmShape { .. })
        ),
        "an arm names one grantee"
    );
}

/// A grant table holding a start and an expiry, each `NULL` for a grant
/// open at that end.
const EXPIRING: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE TABLE docs (id TEXT PRIMARY KEY);
CREATE TABLE doc_grants (
  doc_id TEXT NOT NULL REFERENCES docs (id),
  user_id TEXT NOT NULL,
  starts_at TIMESTAMPTZ,
  expires_at TIMESTAMPTZ
);
GRANT SELECT ON doc_grants TO authz_owner;
CREATE FUNCTION docs_live() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.doc_id FROM public.doc_grants g
  WHERE g.user_id = current_setting('app.user_id', true)
    AND g.starts_at <= now() AND (g.expires_at IS NULL OR g.expires_at > now())
$$;
CREATE FUNCTION docs_flipped() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.doc_id FROM public.doc_grants g
  WHERE g.user_id = current_setting('app.user_id', true)
    AND (current_timestamp >= g.starts_at OR g.starts_at IS NULL)
    AND transaction_timestamp() <= g.expires_at
$$;
ALTER FUNCTION docs_live() OWNER TO authz_owner;
ALTER FUNCTION docs_flipped() OWNER TO authz_owner;
";

/// Where a fixture time lies against the statement time, `NULL` for none.
const INSTANTS: [Option<&str>; 4] = [None, Some("past"), Some("now"), Some("future")];

/// The rank of a fixture time, the statement time being `now`.
fn instant(name: &str) -> core::cmp::Ordering {
    match name {
        "past" => core::cmp::Ordering::Less,
        "now" => core::cmp::Ordering::Equal,
        "future" => core::cmp::Ordering::Greater,
        other => panic!("no fixture time {other}"),
    }
}

/// What a grant arm of [`EXPIRING`] returns to `caller` over one grant row
/// to `alice` per start and expiry in [`INSTANTS`], each document named
/// `<start>-<expiry>`, the way a translator checking at the statement time
/// would answer it.
fn evaluate_expiring(set: &HelperSet<'_, ParserDB>, caller: &str) -> Vec<String> {
    let mut returned = Vec::new();
    for start in INSTANTS {
        for expiry in INSTANTS {
            let row = [("starts_at", start), ("expires_at", expiry)];
            let kept = set.arms().iter().any(|arm| {
                let HelperArm::Grant(grant) = arm else { panic!("only grant arms") };
                assert!(grant.restrictions().is_empty());
                caller == "alice"
                    && grant.time_bounds().iter().all(|bound| {
                        let (_, time) = row
                            .iter()
                            .find(|(column, _)| *column == bound.column().column_name())
                            .expect("a fixture column");
                        time.map_or(bound.keeps_null(), |time| {
                            let ordering = instant(time);
                            match bound.comparison() {
                                TimeComparison::Later => ordering.is_gt(),
                                TimeComparison::LaterOrEqual => ordering.is_ge(),
                                TimeComparison::Earlier => ordering.is_lt(),
                                TimeComparison::EarlierOrEqual => ordering.is_le(),
                            }
                        })
                    })
            });
            if kept {
                returned.push(format!("{}-{}", start.unwrap_or("null"), expiry.unwrap_or("null")));
            }
        }
    }
    returned.sort();
    returned
}

#[test]
fn a_grant_arm_keeps_each_bound_against_the_statement_time() {
    let database = postgres(EXPIRING);
    let deployment = deployment();
    // Measured on PostgreSQL 16.15 in one transaction, which holds the
    // statement time, the `now` rows written at it.
    let cases = [
        (
            "docs_live",
            "doc_grants.doc_id WHERE user_id = app.user_id \
             AND starts_at <= now AND (expires_at IS NULL OR expires_at > now)",
            &["now-future", "now-null", "past-future", "past-null"][..],
        ),
        (
            "docs_flipped",
            "doc_grants.doc_id WHERE user_id = app.user_id \
             AND (starts_at IS NULL OR starts_at <= now) AND expires_at >= now",
            &["now-future", "now-now", "null-future", "null-now", "past-future", "past-now"][..],
        ),
    ];
    for (helper, rendered, measured) in cases {
        let set = answer(&database, &deployment, "public", helper, &[]).expect("answered");
        assert_eq!(render(&set), rendered, "{helper}");
        assert_eq!(evaluate_expiring(&set, "alice"), measured, "{helper}");
        assert!(evaluate_expiring(&set, "bob").is_empty(), "{helper}");
    }
}

/// What `docs_with('read')` answers, rendered, with `condition` filtering
/// its grant arm after the caller, over members holding times of every
/// type, `after` following the helper.
fn bounded(condition: &str, after: &str) -> Result<String, HelperSetError> {
    let body = format!(
        "SELECT doc_id FROM members WHERE user_id = current_setting('app.user_id', true) AND ({condition})"
    );
    let sql = helper(DEFINER, &body, after).replace(
        "user_id TEXT, role TEXT)",
        "user_id TEXT, role TEXT, starts_at TIMESTAMPTZ, expires_at TIMESTAMPTZ, local_end TIMESTAMP, end_day DATE)",
    );
    let database = postgres(&sql);
    let deployment = deployment();
    answer(&database, &deployment, "public", "docs_with", &[text("read")]).map(|set| render(&set))
}

#[test]
fn the_statement_time_is_read_in_every_spelling_postgres_takes_for_it() {
    for (condition, expected) in [
        ("expires_at > now()", "expires_at > now"),
        ("expires_at > NOW()", "expires_at > now"),
        ("expires_at > pg_catalog.now()", "expires_at > now"),
        ("expires_at > \"pg_catalog\".now()", "expires_at > now"),
        ("members.expires_at > CURRENT_TIMESTAMP", "expires_at > now"),
        ("(expires_at) > (current_timestamp)", "expires_at > now"),
        ("expires_at > transaction_timestamp()", "expires_at > now"),
        ("expires_at >= now()", "expires_at >= now"),
        ("now() < expires_at", "expires_at > now"),
        ("now() <= expires_at", "expires_at >= now"),
        ("now() > expires_at", "expires_at < now"),
        ("now() >= expires_at", "expires_at <= now"),
        ("expires_at IS NULL OR now() < expires_at", "(expires_at IS NULL OR expires_at > now)"),
        (
            "(expires_at > now()) OR (expires_at IS NULL)",
            "(expires_at IS NULL OR expires_at > now)",
        ),
        (
            "role IN ('admin', min_role) AND starts_at < now() AND expires_at > now()",
            "role IN ('admin', 'read') AND starts_at < now AND expires_at > now",
        ),
    ] {
        assert_eq!(
            bounded(condition, "").expect("answered"),
            format!("{ROLES} AND {expected}"),
            "{condition}"
        );
    }
    assert_eq!(
        bounded("role = 'admin' AND role = 'write' AND expires_at > now()", "").expect("answered"),
        "",
        "an arm whose literals keep no row is left out whatever its bounds"
    );
}

#[test]
fn a_gate_on_the_caller_setting_excludes_the_values_it_compares_with() {
    let deployment = deployment();
    let gated = |gate: &str| {
        let sql = GRANTEES.replace("current_setting('app.user_id', true) <> ''", gate);
        assert_ne!(sql, GRANTEES, "the gate is replaced");
        let database = postgres(&sql);
        let set = answer(&database, &deployment, "authz", "caller_grantees", &[])?;
        let HelperArm::Grant(grant) = &set.arms()[2] else { panic!("a grant arm") };
        Ok::<_, HelperSetError>(grant.gate().map(|gate| {
            let excluded: Vec<String> = gate.excluded().iter().map(ToString::to_string).collect();
            (
                gate.setting().name().to_string(),
                gate.raises_when_unset(),
                gate.excludes_unset(),
                excluded,
            )
        }))
    };
    let nil = "'00000000-0000-0000-0000-000000000000'";
    for (gate, raises, excludes_unset, excluded) in [
        ("current_setting('app.user_id', true) IS NOT NULL".to_string(), false, false, vec![]),
        ("'' <> current_setting('app.user_id', true)".to_string(), false, true, vec![]),
        (format!("current_setting('app.user_id', true) <> {nil}"), false, false, vec![nil]),
        (format!("{nil} != current_setting('app.user_id')"), true, false, vec![nil]),
        (
            format!(
                "current_setting('app.user_id', true) <> '' AND current_setting('app.user_id') <> {nil} \
                 AND current_setting('app.user_id', true) IS NOT NULL AND '' <> current_setting('app.user_id', true)"
            ),
            true,
            true,
            vec![nil],
        ),
    ] {
        let excluded = excluded.into_iter().map(str::to_string).collect();
        let expected = ("app.user_id".to_string(), raises, excludes_unset, excluded);
        assert_eq!(gated(&gate), Ok(Some(expected)), "{gate}");
    }

    let database = postgres(&GRANTEES.replace(
        "current_setting('app.user_id', true) <> ''",
        "current_setting('app.user_id', true) <> NULL",
    ));
    let set = answer(&database, &deployment, "authz", "caller_grantees", &[]).expect("answered");
    assert!(
        !render(&set).contains("signed_in"),
        "a gate excluding NULL keeps no row: {}",
        render(&set)
    );

    assert!(matches!(
        gated("current_setting('app.user_id', true) <> 0"),
        Err(HelperSetError::Irreducible { .. })
    ));
    assert!(matches!(
        gated("current_setting('app.tenant', true) <> ''"),
        Err(HelperSetError::UndeclaredSetting { setting, .. }) if setting == "app.tenant"
    ));
    let two_callers = Deployment::new(
        "app",
        ["app"],
        ["app.user_id", "app.agent_id"].map(|name| {
            SessionSetting::value(name, SettingValue::Caller(DataType::Text))
                .expect("a custom setting name")
                .may_be_unset()
        }),
    )
    .expect("a valid deployment");
    let sql = GRANTEES.replace(
        "current_setting('app.user_id', true) <> ''",
        "current_setting('app.user_id', true) <> '' AND current_setting('app.agent_id', true) <> ''",
    );
    let database = postgres(&sql);
    assert!(
        matches!(
            answer(&database, &two_callers, "authz", "caller_grantees", &[]),
            Err(HelperSetError::ArmShape { .. })
        ),
        "a gate reads one setting"
    );

    let with_set = Deployment::new(
        "app",
        ["app"],
        [
            SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))
                .expect("a custom setting name")
                .may_be_unset(),
            SessionSetting::value("app.team_ids", delimited(",")).expect("a custom setting name"),
        ],
    )
    .expect("a valid deployment");
    let database = postgres(&GRANTEES.replace(
        "current_setting('app.user_id', true) <> ''",
        "current_setting('app.team_ids', true) <> ''",
    ));
    assert!(
        matches!(
            answer(&database, &with_set, "authz", "caller_grantees", &[]),
            Err(HelperSetError::SettingNotCaller { setting, .. }) if setting == "app.team_ids"
        ),
        "a gate reads a caller setting"
    );
}

/// A helper over `UUID` grantees returning the signed-in grantee behind
/// `gate`, under `path`.
fn signing(path: &str, gate: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE grantees (id UUID PRIMARY KEY, kind TEXT NOT NULL);
         GRANT SELECT ON grantees TO authz_owner;
         {after}
         CREATE FUNCTION authz.signed_in() RETURNS SETOF UUID
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = {path} AS $$
           SELECT g.id FROM public.grantees g WHERE g.kind = 'signed_in' AND {gate}
         $$;
         ALTER FUNCTION authz.signed_in() OWNER TO authz_owner;"
    )
}

/// Whether a gate raises when unset, whether it compares in the declared
/// type, whether it excludes the unset caller, and the values it excludes.
type SignedInGate = Option<(bool, bool, bool, Vec<String>)>;

/// The gate `authz.signed_in` in `sql` reads, the caller declared as a
/// `uuid`.
fn signed_in_gate(sql: &str) -> Result<SignedInGate, HelperSetError> {
    let database = postgres(sql);
    let setting = SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Uuid))
        .expect("a custom setting name")
        .may_be_unset();
    let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
    let set = answer(&database, &deployment, "authz", "signed_in", &[])?;
    let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
    Ok(grant.gate().map(|gate| {
        let excluded = gate.excluded().iter().map(ToString::to_string).collect();
        (
            gate.raises_when_unset(),
            gate.compares_in_declared_type(),
            gate.excludes_unset(),
            excluded,
        )
    }))
}

const NIL: &str = "'00000000-0000-0000-0000-000000000000'";
const CALLER: &str = "current_setting('app.user_id', true)";

#[test]
fn a_gate_on_the_caller_cast_to_its_declared_type_compares_in_that_type() {
    for (gate, raises, in_declared_type, excluded) in [
        (format!("{CALLER}::uuid <> {NIL}"), false, true, vec![NIL]),
        (format!("{CALLER}::uuid <> {NIL}::uuid"), false, true, vec![NIL]),
        (
            format!("CAST({NIL} AS UUID) <> current_setting('app.user_id')::uuid"),
            true,
            true,
            vec![NIL],
        ),
        (
            format!("pg_catalog.{CALLER}::pg_catalog.uuid <> {NIL}::pg_catalog.uuid"),
            false,
            true,
            vec![NIL],
        ),
        (format!("{CALLER}::uuid IS NOT NULL"), false, true, vec![]),
        (format!("{CALLER}::uuid IS NOT NULL AND {CALLER}::uuid <> {NIL}"), false, true, vec![NIL]),
        (format!("{CALLER} <> {NIL}"), false, false, vec![NIL]),
    ] {
        let excluded = excluded.into_iter().map(str::to_string).collect();
        assert_eq!(
            signed_in_gate(&signing("''", &gate, "")),
            Ok(Some((raises, in_declared_type, false, excluded))),
            "{gate}"
        );
    }
}

#[test]
fn a_gate_on_the_caller_cast_to_any_other_type_refuses_the_answer() {
    let cast = |sql: &str| {
        matches!(
            signed_in_gate(sql),
            Err(HelperSetError::SettingCast { setting, .. }) if setting == "app.user_id"
        )
    };
    assert!(cast(&signing("''", &format!("{CALLER}::text <> {NIL}"), "")));
    let shadow = "CREATE DOMAIN public.uuid AS pg_catalog.uuid CHECK (VALUE IS NOT NULL);";
    assert!(
        cast(&signing("public, pg_catalog", &format!("{CALLER}::uuid <> {NIL}"), shadow)),
        "a uuid the path finds before pg_catalog's is not the declared type"
    );
    for gate in [
        format!("{CALLER} <> {NIL}::uuid"),
        format!("{CALLER}::uuid <> {NIL}::text"),
        format!("{CALLER}::uuid <> 0"),
        format!("{CALLER}::text::uuid <> {NIL}"),
    ] {
        assert!(
            matches!(
                signed_in_gate(&signing("''", &gate, "")),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{gate}"
        );
    }
    assert_eq!(
        signed_in_gate(&signing("''", "g.id IS NOT NULL", "")),
        Ok(None),
        "a column tested for NULL gates nothing"
    );
    let mixed = format!("{CALLER}::uuid <> {NIL} AND {CALLER} IS NOT NULL");
    assert!(
        matches!(signed_in_gate(&signing("''", &mixed, "")), Err(HelperSetError::ArmShape { .. })),
        "a gate reads the caller in one type"
    );
}

/// `CALLER` read through `NULLIF`, `NULL` both before the setting was ever
/// set and once the transaction that set it ended.
const ABSENT: &str = "NULLIF(current_setting('app.user_id', true), '')";

#[test]
fn a_gate_on_the_caller_read_through_nullif_excludes_the_unset_caller() {
    for (gate, raises, in_declared_type, excluded) in [
        (format!("{ABSENT}::uuid IS NOT NULL"), false, true, vec![]),
        (format!("{ABSENT} IS NOT NULL"), false, false, vec![]),
        (format!("(NULLIF(({CALLER}), ''))::pg_catalog.uuid IS NOT NULL"), false, true, vec![]),
        (format!("nullif(pg_catalog.{CALLER}, '')::uuid IS NOT NULL"), false, true, vec![]),
        (
            format!("{NIL} <> NULLIF(current_setting('app.user_id'), '')::uuid"),
            true,
            true,
            vec![NIL],
        ),
        (
            format!("{ABSENT}::uuid IS NOT NULL AND {ABSENT}::uuid <> {NIL}::uuid"),
            false,
            true,
            vec![NIL],
        ),
        (format!("{ABSENT} <> ''"), false, false, vec![]),
    ] {
        let excluded = excluded.into_iter().map(str::to_string).collect();
        assert_eq!(
            signed_in_gate(&signing("''", &gate, "")),
            Ok(Some((raises, in_declared_type, true, excluded))),
            "{gate}"
        );
    }
}

#[test]
fn a_nullif_over_anything_but_a_setting_read_and_the_empty_string_refuses_the_answer() {
    for gate in [
        format!("NULLIF({CALLER}, ' ')::uuid IS NOT NULL"),
        format!("NULLIF({CALLER}, NULL)::uuid IS NOT NULL"),
        format!("NULLIF({CALLER}, ''::text)::uuid IS NOT NULL"),
        format!("NULLIF('', {CALLER})::uuid IS NOT NULL"),
        format!("NULLIF({CALLER}::uuid, NULL) IS NOT NULL"),
        format!("NULLIF({ABSENT}, '')::uuid IS NOT NULL"),
        format!("\"nullif\"({CALLER}, '')::uuid IS NOT NULL"),
        format!("public.nullif({CALLER}, '')::uuid IS NOT NULL"),
        format!("COALESCE({CALLER}, '')::uuid IS NOT NULL"),
    ] {
        assert!(
            matches!(
                signed_in_gate(&signing("''", &gate, "")),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{gate}"
        );
    }
    assert!(
        matches!(
            signed_in_gate(&signing("''", &format!("{ABSENT}::text IS NOT NULL"), "")),
            Err(HelperSetError::SettingCast { setting, .. }) if setting == "app.user_id"
        ),
        "a read through NULLIF casts to the declared type alone"
    );
    assert!(
        matches!(
            signed_in_gate(&signing(
                "''",
                "NULLIF(current_setting('app.tenant', true), '') IS NOT NULL",
                ""
            )),
            Err(HelperSetError::UndeclaredSetting { setting, .. }) if setting == "app.tenant"
        ),
        "a read through NULLIF reads the setting it names"
    );
    let mixed = format!("{ABSENT}::uuid IS NOT NULL AND {CALLER}::uuid <> {NIL}");
    assert!(
        matches!(signed_in_gate(&signing("''", &mixed, "")), Err(HelperSetError::ArmShape { .. })),
        "a gate reads the caller through NULLIF in every conjunct or in none"
    );
}

#[test]
fn a_comparison_with_the_caller_read_through_nullif_excludes_the_unset_caller() {
    let read = |read: &str| blocked_through("", read);
    for (written, raises, excludes_unset) in [
        (format!("{ABSENT}::uuid"), false, true),
        ("CAST(NULLIF(current_setting('app.user_id'), '') AS uuid)".to_string(), true, true),
        ("current_setting('app.user_id', true)::uuid".to_string(), false, false),
    ] {
        let expected = (BLOCKING_ARM.to_string(), raises, excludes_unset);
        assert_eq!(read(&written), Ok(expected), "{written}");
    }
    assert!(
        matches!(
            read(&format!("NULLIF({CALLER}, 'none')::uuid")),
            Err(HelperSetError::Irreducible { .. })
        ),
        "a NULLIF against another string reads no caller"
    );
}

#[test]
fn a_set_read_excludes_the_unset_request_unless_it_raises_on_the_empty_string() {
    let excludes_unset = |condition: &str, packing: SetPacking| {
        let database = postgres(&presenting("''", condition, ""));
        let setting = SessionSetting::value(
            "app.link_ids",
            SettingValue::Set { element: DataType::Uuid, packing },
        )
        .expect("a custom setting name");
        let deployment = Deployment::new("app", ["app"], [setting]).expect("a valid deployment");
        answer(&database, &deployment, "authz", "presented_links", &[]).map(|set| {
            let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
            let GrantedTo::Request(request) = grant.granted_to() else { panic!("the set") };
            request.excludes_unset()
        })
    };
    let absent_split = "string_to_array(NULLIF(current_setting('app.link_ids', true), ''), ',')";
    let absent_elements =
        "jsonb_array_elements_text(NULLIF(current_setting('app.link_ids', true), '')::jsonb)";
    let comma = || SetPacking::Delimited(",".into());
    for (condition, packing, expected) in [
        (format!("l.id = ANY ({SPLIT}::uuid[])"), comma(), true),
        (format!("l.id = ANY ({absent_split}::uuid[])"), comma(), true),
        (format!("l.id IN (SELECT unnest({absent_split})::uuid)"), comma(), true),
        (format!("l.id IN (SELECT {ELEMENTS}::uuid)"), SetPacking::JsonArray, false),
        (format!("l.id IN (SELECT {absent_elements}::uuid)"), SetPacking::JsonArray, true),
    ] {
        assert_eq!(excludes_unset(&condition, packing), Ok(expected), "{condition}");
    }
}

#[test]
fn a_grantee_column_is_typed_by_its_one_foreign_key_or_by_none() {
    let deployment = deployment();
    let typed = |from: &str, to: &str| {
        let sql = GRANTEES.replace(from, to);
        assert_ne!(sql, GRANTEES, "{from} is replaced");
        let database = postgres(&sql);
        answer(&database, &deployment, "authz", "projects_granted", &[text("admin")]).map(|set| {
            let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
            let GrantedTo::Set(grantees) = grant.granted_to() else { panic!("granted to a set") };
            grantees.foreign_key().is_some()
        })
    };
    let column = "grantee_id TEXT NOT NULL REFERENCES grantees (id),";
    assert_eq!(typed(column, "grantee_id TEXT NOT NULL,"), Ok(false), "a column without one");
    assert!(
        matches!(
            typed(
                column,
                "grantee_id TEXT NOT NULL REFERENCES grantees (id) REFERENCES teams (id),"
            ),
            Err(HelperSetError::ArmShape { .. })
        ),
        "a column with two names no one grantee table"
    );
    assert!(
        matches!(
            typed("WHERE pg.grantee_id IN", "WHERE lower(pg.grantee_id) IN"),
            Err(HelperSetError::ArmShape { .. })
        ),
        "a set compared with no column"
    );
}

#[test]
fn a_time_condition_no_bound_represents_is_irreducible() {
    for condition in [
        "expires_at > statement_timestamp()",
        "expires_at > clock_timestamp()",
        "expires_at > localtimestamp",
        "expires_at > current_timestamp(3)",
        "expires_at > now() + interval '1 day'",
        "expires_at > app.now()",
        "expires_at = now()",
        "expires_at <> now()",
        "now() = expires_at",
        "local_end > now()",
        "end_day > now()",
        "starts_at < expires_at",
        "expires_at IS NULL OR starts_at > now()",
        "expires_at > now() OR expires_at > now()",
        "role = 'admin' OR expires_at > now()",
        "NOT expires_at <= now()",
    ] {
        assert!(
            matches!(bounded(condition, ""), Err(HelperSetError::Irreducible { .. })),
            "{condition}"
        );
    }
    let shadow = "CREATE FUNCTION now() RETURNS TIMESTAMPTZ LANGUAGE sql STABLE AS 'SELECT NULL';";
    assert!(
        matches!(bounded("expires_at > now()", shadow), Err(HelperSetError::Irreducible { .. })),
        "a declared now the call may reach is not PostgreSQL's"
    );
}

/// The attributes of an accessor running as its owner on its own path.
const ACCESSOR: &str = "LANGUAGE sql STABLE SECURITY DEFINER SET search_path = ''";

/// The accessor `authz.<name>()` returning `body` as `returns`, declared
/// with `attributes` and owned by `authz_owner`.
fn accessor(name: &str, returns: &str, attributes: &str, body: &str) -> String {
    format!(
        "CREATE FUNCTION authz.{name}() RETURNS {returns} {attributes} AS $$ SELECT {body} $$;
         ALTER FUNCTION authz.{name}() OWNER TO authz_owner;"
    )
}

/// `authz.caller_id()` returning the caller read through `NULLIF` as its
/// declared `uuid`.
fn caller_id(attributes: &str) -> String {
    accessor("caller_id", "UUID", attributes, &format!("{ABSENT}::uuid"))
}

/// `authz.raw_caller()` returning the caller as the text it is set to,
/// raising where it is unset.
fn raw_caller() -> String {
    accessor("raw_caller", "text", "LANGUAGE sql STABLE", "current_setting('app.user_id')")
}

/// What `authz.blocking_organizations` reading the caller as `read` answers
/// once `after` declares its accessors: the arm, and whether the comparison
/// raises where the caller is unset and excludes the unset caller.
fn blocked_through(after: &str, read: &str) -> Result<(String, bool, bool), HelperSetError> {
    blocked_by(&blocking("''", read, after), DataType::Uuid, |set| {
        let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
        let GrantedTo::Request(request) = grant.granted_to() else { panic!("the caller") };
        (render(set), request.raises_when_unset(), request.excludes_unset())
    })
}

#[test]
fn a_caller_read_through_an_accessor_reads_as_the_read_it_returns() {
    let nested = format!(
        "{}{}",
        raw_caller(),
        accessor("caller_id", "uuid", "LANGUAGE sql STABLE", "authz.raw_caller()::uuid")
    );
    let volatile = accessor(
        "caller_id",
        "uuid",
        "LANGUAGE sql VOLATILE SECURITY INVOKER",
        "CAST(current_setting('app.user_id') AS uuid)",
    );
    for (after, read, raises, excludes_unset) in [
        (caller_id(ACCESSOR), "authz.caller_id()", false, true),
        (caller_id(ACCESSOR), "(authz.caller_id())", false, true),
        (volatile, "authz.caller_id()", true, false),
        (raw_caller(), "authz.raw_caller()::uuid", true, false),
        (raw_caller(), "NULLIF(authz.raw_caller(), '')::uuid", true, true),
        (nested, "authz.caller_id()", true, false),
    ] {
        let expected = (BLOCKING_ARM.to_string(), raises, excludes_unset);
        assert_eq!(blocked_through(&after, read), Ok(expected), "{read} after {after}");
    }
    assert_eq!(
        blocked_through(&caller_id(ACCESSOR), &format!("{ABSENT}::uuid")),
        blocked_through("", &format!("{ABSENT}::uuid")),
        "an accessor reads as the read inlined"
    );
}

#[test]
fn a_gate_reads_the_caller_through_an_accessor() {
    let gate = |after: &str, gate: &str| signed_in_gate(&signing("''", gate, after));
    assert_eq!(
        gate(&caller_id(ACCESSOR), "authz.caller_id() IS NOT NULL"),
        Ok(Some((false, true, true, vec![])))
    );
    assert_eq!(
        gate(&caller_id(ACCESSOR), &format!("authz.caller_id() <> {NIL}::uuid")),
        Ok(Some((false, true, true, vec![NIL.to_string()])))
    );
    assert_eq!(
        gate(&raw_caller(), "authz.raw_caller() <> ''"),
        Ok(Some((true, false, true, vec![])))
    );
}

#[test]
fn a_set_read_through_an_accessor_returning_its_text_reads_the_set() {
    let link_ids =
        accessor("link_ids", "text", ACCESSOR, "NULLIF(current_setting('app.link_ids', true), '')");
    let comma = || SetPacking::Delimited(",".into());
    for (condition, packing) in [
        ("l.id = ANY (string_to_array(authz.link_ids(), ',')::uuid[])", comma()),
        ("l.id IN (SELECT unnest(string_to_array(authz.link_ids(), ','))::uuid)", comma()),
        (
            "l.id IN (SELECT jsonb_array_elements_text(authz.link_ids()::jsonb)::uuid)",
            SetPacking::JsonArray,
        ),
    ] {
        let sql = presenting("''", condition, &link_ids);
        assert_eq!(
            presented(&sql, DataType::Uuid, packing).as_deref(),
            Ok(PRESENTED_ARM),
            "{condition}"
        );
    }
}

/// `authz.link_ids()` returning `body` as `returns`, declared as an accessor.
fn link_ids(returns: &str, body: &str) -> String {
    accessor("link_ids", returns, ACCESSOR, body)
}

/// What `presented_links` answers once `after` declares its accessors,
/// `app.link_ids` a set of `uuid` packed as `packing`: the arm, and whether
/// the comparison raises where the set is unset and excludes the unset set.
fn presented_through(
    after: &str,
    condition: &str,
    packing: SetPacking,
) -> Result<(String, bool, bool), HelperSetError> {
    let sql = presenting("''", condition, after);
    presented_as(&sql, SettingValue::Set { element: DataType::Uuid, packing }, |set| {
        let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
        let GrantedTo::Request(request) = grant.granted_to() else { panic!("the set") };
        (render(set), request.raises_when_unset(), request.excludes_unset())
    })
}

#[test]
fn a_set_read_through_an_accessor_reads_as_the_set_it_returns() {
    let comma = || SetPacking::Delimited(",".into());
    let absent_split = "string_to_array(NULLIF(current_setting('app.link_ids', true), ''), ',')";
    let uuids = link_ids("UUID[]", &format!("{absent_split}::uuid[]"));
    let texts = link_ids("text[]", "string_to_array(current_setting('app.link_ids'), ',')");
    let elements = link_ids("uuid[]", &format!("ARRAY(SELECT {ELEMENTS}::uuid)"));
    let unnested = link_ids("uuid[]", &format!("ARRAY(SELECT unnest({SPLIT})::uuid)"));
    let nested = format!(
        "{texts}{}",
        accessor("typed_link_ids", "uuid[]", ACCESSOR, "authz.link_ids()::uuid[]")
    );
    for (after, condition, packing, raises, excludes_unset) in [
        (&uuids, "l.id = ANY (authz.link_ids())", comma(), false, true),
        (&uuids, "l.id = ANY ((authz.link_ids()))", comma(), false, true),
        (&uuids, "l.id IN (SELECT unnest(authz.link_ids()))", comma(), false, true),
        (&uuids, "l.id = ANY (ARRAY(SELECT unnest(authz.link_ids())))", comma(), false, true),
        (&texts, "l.id = ANY (authz.link_ids()::uuid[])", comma(), true, true),
        (&texts, "l.id IN (SELECT unnest(authz.link_ids())::uuid)", comma(), true, true),
        (&elements, "l.id = ANY (authz.link_ids())", SetPacking::JsonArray, false, false),
        (&unnested, "l.id = ANY (authz.link_ids())", comma(), false, true),
        (&nested, "l.id = ANY (authz.typed_link_ids())", comma(), true, true),
        (&nested, "l.id IN (SELECT unnest(authz.typed_link_ids()))", comma(), true, true),
    ] {
        let expected = (PRESENTED_ARM.to_string(), raises, excludes_unset);
        assert_eq!(
            presented_through(after, condition, packing),
            Ok(expected),
            "{condition} after {after}"
        );
    }
    assert_eq!(
        presented_through(&uuids, "l.id = ANY (authz.link_ids())", comma()),
        presented_through("", &format!("l.id = ANY ({absent_split}::uuid[])"), comma()),
        "an accessor reads as the set read inlined"
    );
}

#[test]
fn a_set_accessor_postgres_does_not_run_as_its_read_refuses_the_answer() {
    let comma = || SetPacking::Delimited(",".into());
    let body = format!("{SPLIT}::uuid[]");
    let refused = |after: &str| {
        match presented_through(after, "l.id = ANY (authz.link_ids())", comma()) {
            Err(HelperSetError::Accessor { function, accessor, setting, reason })
                if function == "authz.presented_links"
                    && accessor == "authz.link_ids"
                    && setting == "app.link_ids" =>
            {
                Some(reason)
            }
            _ => None,
        }
    };
    let immutable = accessor(
        "link_ids",
        "uuid[]",
        "LANGUAGE sql IMMUTABLE SECURITY DEFINER SET search_path = ''",
        &body,
    );
    assert_eq!(refused(&immutable), Some(AccessorRefusal::Immutable));
    let sets = accessor("link_ids", "uuid[]", &format!("{ACCESSOR} SET app.link_ids = ''"), &body);
    assert_eq!(refused(&sets), Some(AccessorRefusal::SetsSetting));
    for (returns, body) in [
        ("uuid", body.as_str()),
        ("SETOF uuid[]", body.as_str()),
        ("text[]", body.as_str()),
        ("uuid[]", SPLIT),
    ] {
        assert_eq!(
            refused(&link_ids(returns, body)),
            Some(AccessorRefusal::ReturnType),
            "{returns}: {body}"
        );
    }
    assert!(
        matches!(
            presented_through(&link_ids("text[]", &format!("{SPLIT}::text[]")), "l.id = ANY (authz.link_ids())", comma()),
            Err(HelperSetError::SettingCast { function, .. }) if function == "authz.link_ids"
        ),
        "the accessor casts to the declared element type"
    );
    assert!(
        matches!(
            presented_through(
                &link_ids("uuid[]", &body),
                "l.id = ANY (authz.link_ids())",
                SetPacking::JsonArray
            ),
            Err(HelperSetError::MispackedSet { .. })
        ),
        "the accessor unpacks the set as the deployment packs it"
    );
    let not_a_set = presenting("''", "l.id = ANY (authz.link_ids())", &link_ids("uuid[]", &body));
    assert_eq!(
        presented_as(&not_a_set, SettingValue::Caller(DataType::Uuid), render),
        Err(HelperSetError::SettingCast {
            function: "authz.link_ids".to_string(),
            setting: "app.link_ids".to_string(),
            cast: "UUID[]".to_string(),
        }),
        "a setting not declared as a set declares no element type to cast to"
    );
    for condition in ["l.id = ANY (authz.link_ids()::uuid[])", "l.id = ANY (authz.link_ids(NULL))"]
    {
        assert!(
            matches!(
                presented_through(&link_ids("uuid[]", &body), condition, comma()),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{condition}"
        );
    }
}

#[test]
fn an_accessor_resolves_its_body_on_its_own_path_or_on_the_helpers() {
    let shadow = "CREATE FUNCTION public.current_setting(text, boolean) RETURNS text
                  LANGUAGE sql STABLE AS $$ SELECT 'someone' $$;";
    let read = |attributes: &str| {
        let after = format!("{shadow}{}", caller_id(attributes));
        blocked_by(
            &blocking("public, pg_catalog", "authz.caller_id()", &after),
            DataType::Uuid,
            render,
        )
    };
    assert_eq!(read(ACCESSOR).as_deref(), Ok(BLOCKING_ARM), "its own path skips the shadow");
    assert!(
        matches!(read("LANGUAGE sql STABLE"), Err(HelperSetError::Irreducible { .. })),
        "the helper's path reaches the shadow"
    );
}

#[test]
fn an_accessor_postgres_does_not_run_as_its_read_refuses_the_answer() {
    let refused = |after: &str| {
        let Err(error) = blocked_through(after, "authz.caller_id()") else { return None };
        let HelperSetError::Accessor { function, accessor, setting, reason } = &error else {
            return None;
        };
        assert_eq!(
            (function.as_str(), accessor.as_str(), setting.as_str()),
            ("authz.blocking_organizations", "authz.caller_id", "app.user_id"),
            "{after}"
        );
        Some((*reason, error.to_string()))
    };
    let refusal = |reason, which: &str| {
        Some((
            reason,
            format!(
                "helper `authz.blocking_organizations` reads setting `app.user_id` through \
                 `authz.caller_id`, which {which}"
            ),
        ))
    };
    let immutable = "LANGUAGE sql IMMUTABLE SECURITY DEFINER SET search_path = ''";
    assert_eq!(refused(&caller_id(immutable)), refusal(AccessorRefusal::Immutable, "is IMMUTABLE"));
    for setting in ["app.user_id", "App.User_Id"] {
        let sets = format!("{ACCESSOR} SET {setting} = '{}'", NIL.trim_matches('\''));
        assert_eq!(
            refused(&caller_id(&sets)),
            refusal(AccessorRefusal::SetsSetting, "sets the setting it reads"),
            "{setting}"
        );
    }
    let other = format!("{ACCESSOR} SET app.tenant = 'x'");
    assert_eq!(
        blocked_through(&caller_id(&other), "authz.caller_id()").map(|read| read.0).as_deref(),
        Ok(BLOCKING_ARM),
        "an accessor setting another setting"
    );
    let returns_nothing = format!(
        "CREATE FUNCTION authz.caller_id() {ACCESSOR} AS $$ SELECT {ABSENT}::uuid $$;
         ALTER FUNCTION authz.caller_id() OWNER TO authz_owner;"
    );
    let wrong_type =
        refusal(AccessorRefusal::ReturnType, "returns another type than its body reads");
    assert_eq!(refused(&returns_nothing), wrong_type, "an accessor declaring no result");
    for (returns, body) in [
        ("text", format!("{ABSENT}::uuid")),
        ("varchar", "current_setting('app.user_id')".to_string()),
        ("SETOF uuid", format!("{ABSENT}::uuid")),
    ] {
        let after = accessor("caller_id", returns, ACCESSOR, &body);
        assert_eq!(refused(&after), wrong_type, "{returns}: {body}");
    }
}

#[test]
fn an_accessor_is_checked_as_a_call_and_its_read_as_a_read() {
    let read = |after: &str| blocked_through(after, "authz.caller_id()");
    let revoked = format!(
        "CREATE ROLE accessor_owner NOLOGIN;
         {}ALTER FUNCTION authz.caller_id() OWNER TO accessor_owner;
         REVOKE EXECUTE ON FUNCTION authz.caller_id() FROM PUBLIC;",
        caller_id(ACCESSOR)
    );
    assert!(
        matches!(
            read(&revoked),
            Err(HelperSetError::MissingExecute { function, called, .. })
                if function == "authz.blocking_organizations" && called == "authz.caller_id"
        ),
        "the helper's owner executes the accessor"
    );
    let hidden = format!(
        "CREATE SCHEMA keys;
         {}",
        caller_id(ACCESSOR).replace("authz.", "keys.")
    );
    assert!(
        matches!(
            blocked_through(&hidden, "keys.caller_id()"),
            Err(HelperSetError::MissingUsage { schema, .. }) if schema == "keys"
        ),
        "the helper's owner uses the accessor's schema"
    );
    let text =
        accessor("caller_id", "text", ACCESSOR, "current_setting('app.user_id', true)::text");
    assert!(
        matches!(
            read(&text),
            Err(HelperSetError::SettingCast { function, .. }) if function == "authz.caller_id"
        ),
        "the accessor casts to the declared type"
    );
    let tenant =
        accessor("caller_id", "uuid", ACCESSOR, "current_setting('app.tenant', true)::uuid");
    assert!(
        matches!(
            read(&tenant),
            Err(HelperSetError::UndeclaredSetting { function, setting })
                if function == "authz.caller_id" && setting == "app.tenant"
        ),
        "the accessor reads a declared setting"
    );
    let cycle = format!(
        "{}{}CREATE OR REPLACE FUNCTION authz.raw_caller() RETURNS text LANGUAGE sql STABLE
         AS $$ SELECT authz.echo() $$;",
        raw_caller(),
        accessor("echo", "text", "LANGUAGE sql STABLE", "authz.raw_caller()")
    );
    assert!(
        matches!(
            blocked_through(&cycle, "authz.raw_caller()::uuid"),
            Err(HelperSetError::CallCycle { function }) if function == "authz.raw_caller"
        ),
        "an accessor reaching itself"
    );
}

#[test]
fn a_call_returning_anything_but_a_setting_read_is_irreducible() {
    let after = |body: &str| accessor("caller_id", "uuid", ACCESSOR, body);
    for (after, read) in [
        (after("o.principal_id FROM public.organization_blocks o"), "authz.caller_id()"),
        (after(&format!("{ABSENT}::uuid WHERE true")), "authz.caller_id()"),
        (caller_id(ACCESSOR), "authz.caller_id()::uuid"),
        (
            "CREATE FUNCTION authz.caller_id() RETURNS uuid LANGUAGE sql STABLE
             RETURN NULLIF(current_setting('app.user_id', true), '')::uuid;"
                .to_string(),
            "authz.caller_id()",
        ),
        (
            "CREATE FUNCTION authz.caller_id() RETURNS uuid LANGUAGE plpgsql STABLE
             AS $$ BEGIN RETURN current_setting('app.user_id')::uuid; END $$;"
                .to_string(),
            "authz.caller_id()",
        ),
        (
            "CREATE FUNCTION authz.caller_id(fallback uuid) RETURNS uuid LANGUAGE sql STABLE
             AS $$ SELECT coalesce(current_setting('app.user_id', true)::uuid, fallback) $$;"
                .to_string(),
            "authz.caller_id(NULL)",
        ),
        (String::new(), "authz.caller_id()"),
    ] {
        assert!(
            matches!(blocked_through(&after, read), Err(HelperSetError::Irreducible { .. })),
            "{read} after {after}"
        );
    }
}

const INSTALLATION: &str = "app.installation_id";

/// A deployment whose caller is `app.user_id` and whose app installation
/// acting for it is `app.installation_id`, both of `identity`, either may be
/// unset, beside a scalar `app.tenant`, a set `app.link_ids` and a JSON
/// `request.jwt.claims`.
fn principals(identity: &DataType) -> Deployment {
    let setting = |name: &str, value: SettingValue| {
        SessionSetting::value(name, value).expect("a custom setting name").may_be_unset()
    };
    Deployment::new(
        "app",
        ["app"],
        [
            setting("app.user_id", SettingValue::Caller(identity.clone())),
            setting(
                INSTALLATION,
                SettingValue::Principal { kind: "installation".into(), identity: identity.clone() },
            ),
            setting("app.tenant", SettingValue::Scalar(identity.clone())),
            setting("app.link_ids", delimited(",")),
            SessionSetting::json(
                "request.jwt.claims",
                [JsonField::new(["sub"], SettingValue::Caller(identity.clone())).expect("a path")],
            )
            .expect("a custom setting name"),
        ],
    )
    .expect("a valid deployment")
}

/// Organizations blocking the caller, the installation acting for it, or
/// everyone, listed in one `IN`.
const LISTED_BLOCKS: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TABLE organization_blocks (
  organization_id TEXT NOT NULL,
  principal_id TEXT NOT NULL,
  active BOOLEAN NOT NULL,
  PRIMARY KEY (organization_id, principal_id)
);
GRANT SELECT ON organization_blocks TO authz_owner;
CREATE FUNCTION authz.installation_id() RETURNS TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT NULLIF(current_setting('app.installation_id', true), '')
$$;
ALTER FUNCTION authz.installation_id() OWNER TO authz_owner;
CREATE FUNCTION authz.blocked_listed() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT b.organization_id FROM public.organization_blocks b
  WHERE b.principal_id IN (NULLIF(current_setting('app.user_id', true), ''), 'everyone', authz.installation_id(), NULL)
    AND b.active = true
$$;
ALTER FUNCTION authz.blocked_listed() OWNER TO authz_owner;
";

const LISTED_BLOCK_ROWS: Rows = &[
    (
        "organization_blocks",
        &[("organization_id", "o1"), ("principal_id", "u1"), ("active", "true")],
    ),
    (
        "organization_blocks",
        &[("organization_id", "o2"), ("principal_id", "i1"), ("active", "true")],
    ),
    (
        "organization_blocks",
        &[("organization_id", "o3"), ("principal_id", "everyone"), ("active", "true")],
    ),
    (
        "organization_blocks",
        &[("organization_id", "o4"), ("principal_id", "u1"), ("active", "false")],
    ),
    (
        "organization_blocks",
        &[("organization_id", "o5"), ("principal_id", "i2"), ("active", "true")],
    ),
    ("organization_blocks", &[("organization_id", "o6"), ("principal_id", ""), ("active", "true")]),
];

/// What PostgreSQL 18.6 returns for `authz.blocked_listed()` over
/// [`LISTED_BLOCK_ROWS`], a setting the request leaves out never set.
const LISTED_BLOCKS_MEASURED: &[(Request, &[&str])] = &[
    (Request { caller: Some("u1"), values: &[(INSTALLATION, &["i1"])] }, &["o1", "o2", "o3"]),
    (Request::of("u1"), &["o1", "o3"]),
    (Request { caller: Some("u2"), values: &[(INSTALLATION, &["i1"])] }, &["o2", "o3"]),
    (Request { caller: None, values: &[(INSTALLATION, &["i2"])] }, &["o3", "o5"]),
    (Request { caller: None, values: &[] }, &["o3"]),
    (Request { caller: Some(""), values: &[(INSTALLATION, &[""])] }, &["o3"]),
];

#[test]
fn an_in_list_of_principals_answers_one_grant_arm_per_principal_and_one_for_its_literals() {
    let database = postgres(LISTED_BLOCKS);
    let deployment = principals(&DataType::Text);
    let set = answer(&database, &deployment, "authz", "blocked_listed", &[]).expect("answered");
    assert_eq!(
        render(&set),
        "organization_blocks.organization_id WHERE principal_id = app.user_id AND active IN (true) \
         UNION organization_blocks.organization_id WHERE active IN (true) AND principal_id IN ('everyone') \
         UNION organization_blocks.organization_id WHERE principal_id = app.installation_id AND active IN (true)"
    );
    assert_eq!(covered(&set), None, "a block's organization is no key of its table");
    for arm in set.arms() {
        let HelperArm::Grant(grant) = arm else { panic!("grant arms") };
        if let GrantedTo::Request(request) = grant.granted_to() {
            assert!(!request.raises_when_unset() && request.excludes_unset());
        }
    }
    for &(request, expected) in LISTED_BLOCKS_MEASURED {
        let returned: Vec<_> = evaluate(&set, request, LISTED_BLOCK_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{:?} with {:?}", request.caller, request.values);
    }
}

/// What `authz.blocking_organizations` keeping the blocks `condition` keeps
/// answers once `after` declares its accessors, the caller and the
/// installation `uuid` principals.
fn listing(condition: &str, after: &str) -> Result<String, HelperSetError> {
    let database = postgres(&blocking_where("''", condition, after));
    let deployment = principals(&DataType::Uuid);
    answer(&database, &deployment, "authz", "blocking_organizations", &[]).map(|set| render(&set))
}

#[test]
fn a_listed_principal_is_read_in_every_spelling_a_compared_one_is() {
    let both = "organization_blocks.organization_id WHERE principal_id = app.user_id \
                UNION organization_blocks.organization_id WHERE principal_id = app.installation_id";
    let installation_id = accessor(
        "installation_id",
        "UUID",
        ACCESSOR,
        "NULLIF(current_setting('app.installation_id', true), '')::uuid",
    );
    let accessors = format!("{}{installation_id}", caller_id(ACCESSOR));
    for (list, after) in [
        (
            "NULLIF(current_setting('app.user_id', true), '')::uuid, \
             NULLIF(current_setting('app.installation_id', true), '')::uuid",
            "",
        ),
        (
            "current_setting('app.user_id')::uuid, \
             CAST(current_setting('app.installation_id', true) AS uuid)",
            "",
        ),
        ("authz.caller_id(), authz.installation_id()", accessors.as_str()),
    ] {
        let condition = format!("b.principal_id IN ({list})");
        assert_eq!(listing(&condition, after).as_deref(), Ok(both), "{list}");
    }
    let alone = "organization_blocks.organization_id WHERE principal_id = app.installation_id";
    let condition = "b.principal_id IN (authz.installation_id(), NULL)";
    assert_eq!(listing(condition, &installation_id).as_deref(), Ok(alone), "NULL keeps no row");
    assert_eq!(
        listing("b.principal_id = authz.installation_id()", &installation_id).as_deref(),
        Ok(alone),
        "a principal compared alone"
    );
}

#[test]
fn a_listed_scalar_is_read_in_every_spelling_a_listed_principal_is() {
    let user = "NULLIF(current_setting('app.user_id', true), '')::uuid";
    let both = "organization_blocks.organization_id WHERE principal_id = app.user_id \
                UNION organization_blocks.organization_id WHERE principal_id = app.tenant";
    let tenant_id = accessor(
        "tenant_id",
        "UUID",
        ACCESSOR,
        "NULLIF(current_setting('app.tenant', true), '')::uuid",
    );
    for (listed, after) in [
        ("current_setting('app.tenant', true)::uuid", ""),
        ("authz.tenant_id()", tenant_id.as_str()),
    ] {
        let condition = format!("b.principal_id IN ({user}, {listed})");
        assert_eq!(listing(&condition, after).as_deref(), Ok(both), "{listed}");
    }
}

/// What `authz.blocking_organizations` keeping the blocks `condition` keeps
/// answers, read with its `IS NULL` tests, once `authz.caller_id()` and
/// `authz.installation_id()` return their `uuid` principals.
fn principal_reading(condition: &str) -> Result<String, HelperSetError> {
    let installation_id = accessor(
        "installation_id",
        "UUID",
        ACCESSOR,
        "NULLIF(current_setting('app.installation_id', true), '')::uuid",
    );
    let accessors = format!("{}{installation_id}", caller_id(ACCESSOR));
    let database = postgres(&blocking_where("''", condition, &accessors));
    let deployment = principals(&DataType::Uuid);
    function(&database, "authz", "blocking_organizations")
        .helper_set(&[], &deployment, &database, &[ArmCondition::UnsetSetting])
        .map(|set| render(&set))
}

#[test]
fn a_read_in_a_scalar_subquery_returning_its_one_row_is_the_read() {
    let caller = "b.principal_id = authz.caller_id()";
    let absent = &format!("b.principal_id = {ABSENT}::uuid");
    let raw_installation = "current_setting('app.installation_id', true)";
    for (bare, wrapped) in [
        (caller, "b.principal_id = (SELECT authz.caller_id())"),
        (caller, "(SELECT authz.caller_id()) = b.principal_id"),
        (caller, "b.principal_id = ((SELECT (SELECT authz.caller_id())))"),
        (caller, "b.principal_id = (SELECT DISTINCT authz.caller_id())"),
        (
            &format!("{caller} AND authz.installation_id() IS NULL"),
            &format!("{caller} AND (SELECT authz.installation_id()) IS NULL"),
        ),
        (
            &format!("{caller} AND {raw_installation} IS NULL"),
            &format!("{caller} AND (SELECT {raw_installation}) IS NULL"),
        ),
        (
            "b.principal_id IN (authz.caller_id(), authz.installation_id())",
            "b.principal_id IN ((SELECT authz.caller_id()), (SELECT authz.installation_id()))",
        ),
        (absent, &format!("b.principal_id = (SELECT {ABSENT})::uuid")),
        (absent, &format!("b.principal_id = (SELECT {ABSENT}::uuid)")),
        (
            absent,
            "b.principal_id = NULLIF((SELECT current_setting('app.user_id', true)), '')::uuid",
        ),
    ] {
        let answered = principal_reading(bare);
        assert!(answered.is_ok(), "{bare}: {answered:?}");
        assert_eq!(principal_reading(wrapped), answered, "{wrapped}");
    }
    for (bare, wrapped) in [
        ("current_setting('app.user_id')::uuid", "(SELECT current_setting('app.user_id'))::uuid"),
        ("authz.caller_id()", "(SELECT authz.caller_id())"),
    ] {
        let answered = blocked_through(&caller_id(ACCESSOR), bare);
        assert!(answered.is_ok(), "{bare}: {answered:?}");
        assert_eq!(blocked_through(&caller_id(ACCESSOR), wrapped), answered, "{wrapped}");
    }
    for (bare, wrapped) in [
        ("authz.caller_id() IS NOT NULL", "(SELECT authz.caller_id()) IS NOT NULL"),
        (
            &format!("authz.caller_id() <> {NIL}::uuid") as &str,
            &format!("(SELECT authz.caller_id()) <> {NIL}::uuid"),
        ),
    ] {
        let gate = |gate: &str| signed_in_gate(&signing("''", gate, &caller_id(ACCESSOR)));
        assert!(gate(bare).is_ok_and(|gate| gate.is_some()), "{bare}");
        assert_eq!(gate(wrapped), gate(bare), "{wrapped}");
    }
}

#[test]
fn a_scalar_subquery_that_may_change_or_drop_its_row_is_no_read() {
    for wrapped in [
        "(SELECT authz.caller_id() WHERE false)",
        "(SELECT authz.caller_id() HAVING false)",
        "(SELECT authz.caller_id() LIMIT 0)",
        "(SELECT authz.caller_id() OFFSET 1)",
        "(SELECT authz.caller_id() FROM public.organization_blocks)",
        "(SELECT authz.caller_id() UNION ALL SELECT authz.installation_id())",
        "(SELECT b.principal_id)",
        "(SELECT COALESCE(authz.caller_id(), b.principal_id))",
    ] {
        let condition = format!("b.principal_id = {wrapped}");
        assert!(
            matches!(principal_reading(&condition), Err(HelperSetError::Irreducible { .. })),
            "{wrapped}"
        );
    }
}

#[test]
fn a_list_naming_anything_but_request_values_literals_and_parameters_refuses_the_answer() {
    let user = "NULLIF(current_setting('app.user_id', true), '')::uuid";
    let refused = |condition: String, after: &str| listing(&condition, after).map(|_| ());
    let not_single = |setting: &str| {
        Err(HelperSetError::SettingNotSingle {
            function: "authz.blocking_organizations".into(),
            setting: setting.into(),
        })
    };
    assert_eq!(
        refused(format!("b.principal_id IN ({user}, current_setting('app.link_ids', true))"), ""),
        not_single("app.link_ids"),
        "a set read as one value"
    );
    let links = accessor("links", "UUID", ACCESSOR, "current_setting('app.link_ids', true)::uuid");
    assert!(
        matches!(
            refused(format!("b.principal_id IN ({user}, authz.links())"), &links),
            Err(HelperSetError::SettingCast { function, setting, .. })
                if function == "authz.links" && setting == "app.link_ids"
        ),
        "a set cast to one value's type"
    );
    assert!(matches!(
        refused(format!("b.principal_id IN ({user}, current_setting('app.nope', true)::uuid)"), ""),
        Err(HelperSetError::UndeclaredSetting { setting, .. }) if setting == "app.nope"
    ));
    for condition in [
        format!("b.principal_id IN ({user}, b.organization_id)"),
        format!("b.principal_id IN ({user}, '00000000-0000-0000-0000-000000000000'::uuid)"),
        format!("b.principal_id IN ({user}, gen_random_uuid())"),
        format!("b.principal_id NOT IN ({user})"),
        format!("{user} IN (b.principal_id, b.organization_id)"),
    ] {
        assert!(
            matches!(refused(condition.clone(), ""), Err(HelperSetError::Irreducible { .. })),
            "{condition}"
        );
    }
    assert!(
        matches!(
            refused(format!("b.principal_id IN ({user}) AND b.organization_id = {user}"), ""),
            Err(HelperSetError::ArmShape { .. })
        ),
        "an arm names one grantee"
    );
    assert_eq!(
        refused(
            format!(
                "b.principal_id IN ({user}) AND current_setting('{INSTALLATION}', true) IS NOT NULL"
            ),
            ""
        ),
        Err(HelperSetError::SettingNotCaller {
            function: "authz.blocking_organizations".into(),
            setting: INSTALLATION.into(),
        }),
        "a gate reads the caller alone"
    );
    assert_eq!(
        refused(
            format!(
                "b.principal_id IN ({user}, current_setting('request.jwt.claims', true)::uuid)"
            ),
            ""
        ),
        not_single("request.jwt.claims"),
        "a JSON document compared as one value"
    );
    let claims = "CREATE FUNCTION authz.claims() RETURNS UUID LANGUAGE sql STABLE
                  AS $$ SELECT current_setting('request.jwt.claims', true)::uuid $$;";
    assert!(
        matches!(
            refused(format!("b.principal_id IN ({user}, authz.claims())"), claims),
            Err(HelperSetError::SettingCast { function, setting, .. })
                if function == "authz.claims" && setting == "request.jwt.claims"
        ),
        "a JSON document cast to one value's type"
    );
}

#[test]
fn every_arm_of_a_list_keeps_the_conditions_beside_it() {
    let user = "NULLIF(current_setting('app.user_id', true), '')::uuid";
    let installation = "NULLIF(current_setting('app.installation_id', true), '')::uuid";
    let list = format!("b.principal_id IN ({user}, {installation})");
    let expiring = "ALTER TABLE organization_blocks ADD COLUMN expires_at TIMESTAMPTZ;";
    assert_eq!(
        listing(&format!("{list} AND b.expires_at > now()"), expiring).as_deref(),
        Ok(
            "organization_blocks.organization_id WHERE principal_id = app.user_id AND expires_at > now \
            UNION organization_blocks.organization_id WHERE principal_id = app.installation_id AND expires_at > now"
        )
    );
    assert_eq!(
        listing(&format!("{list} AND 1 = 2"), "").as_deref(),
        Ok(""),
        "a list kept from no row"
    );
}

/// App installations capped to organization and kind pairs, as connetto's
/// core schema writes them.
const CAPS: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TYPE authz.resource_kind AS ENUM ('organization', 'folder', 'project');
CREATE TABLE memberships (
  organization_id TEXT NOT NULL, user_id TEXT NOT NULL, PRIMARY KEY (organization_id, user_id));
CREATE TABLE resources (
  id TEXT PRIMARY KEY, organization_id TEXT NOT NULL, kind authz.resource_kind NOT NULL,
  owner_id TEXT NOT NULL);
CREATE TABLE installation_permissions (
  installation_id TEXT NOT NULL, organization_id TEXT NOT NULL,
  kind authz.resource_kind NOT NULL, max_role TEXT NOT NULL CHECK (max_role IN ('reader', 'admin')),
  PRIMARY KEY (installation_id, organization_id, kind));
GRANT SELECT ON memberships, resources, installation_permissions TO authz_owner;
CREATE FUNCTION authz.capped_kinds(min_role TEXT)
RETURNS TABLE (organization_id TEXT, kind authz.resource_kind)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT permission.organization_id, permission.kind FROM public.installation_permissions permission
  WHERE permission.installation_id = NULLIF(current_setting('app.installation_id', true), '')
    AND (permission.max_role = 'admin' OR permission.max_role = min_role)
$$;
CREATE FUNCTION authz.member_organizations() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT membership.organization_id FROM public.memberships membership
  WHERE membership.user_id = NULLIF(current_setting('app.user_id', true), '')
    AND (NULLIF(current_setting('app.installation_id', true), '') IS NULL
      OR (membership.organization_id, 'organization'::authz.resource_kind)
        IN (SELECT * FROM authz.capped_kinds('reader')))
$$;
CREATE FUNCTION authz.owned_resources(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE resource.owner_id = NULLIF(current_setting('app.user_id', true), '')
    AND (current_setting('app.installation_id', true) IS NULL
      OR (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role)))
$$;
CREATE FUNCTION authz.capped_resources(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))
$$;
ALTER FUNCTION authz.capped_kinds(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.member_organizations() OWNER TO authz_owner;
ALTER FUNCTION authz.owned_resources(TEXT) OWNER TO authz_owner;
ALTER FUNCTION authz.capped_resources(TEXT) OWNER TO authz_owner;
";

/// The rows [`CAPS`] was measured over. Installation `i1` holds organization
/// `o1` and kind `folder` and organization `o2` and kind `project`, but not
/// `o2` with `folder`.
const CAP_ROWS: Rows = &[
    ("memberships", &[("organization_id", "o1"), ("user_id", "alice")]),
    ("memberships", &[("organization_id", "o2"), ("user_id", "alice")]),
    ("memberships", &[("organization_id", "o1"), ("user_id", "bob")]),
    (
        "resources",
        &[("id", "r1"), ("organization_id", "o1"), ("kind", "organization"), ("owner_id", "alice")],
    ),
    (
        "resources",
        &[("id", "r2"), ("organization_id", "o1"), ("kind", "project"), ("owner_id", "alice")],
    ),
    (
        "resources",
        &[("id", "r3"), ("organization_id", "o2"), ("kind", "project"), ("owner_id", "alice")],
    ),
    (
        "resources",
        &[("id", "r4"), ("organization_id", "o2"), ("kind", "folder"), ("owner_id", "alice")],
    ),
    (
        "resources",
        &[("id", "r5"), ("organization_id", "o1"), ("kind", "folder"), ("owner_id", "bob")],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i1"),
            ("organization_id", "o1"),
            ("kind", "organization"),
            ("max_role", "reader"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i1"),
            ("organization_id", "o1"),
            ("kind", "folder"),
            ("max_role", "reader"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i1"),
            ("organization_id", "o2"),
            ("kind", "project"),
            ("max_role", "admin"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i2"),
            ("organization_id", "o2"),
            ("kind", "organization"),
            ("max_role", "admin"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i2"),
            ("organization_id", "o2"),
            ("kind", "folder"),
            ("max_role", "reader"),
        ],
    ),
];

/// A deployment declaring the caller, the acting installation, another
/// principal, a tenant, a scalar, and the organizations a request presents,
/// a set, each of which a request may leave unset.
fn installation_deployment() -> Deployment {
    let setting = |name: &str, value: SettingValue| {
        SessionSetting::value(name, value).expect("a custom setting name").may_be_unset()
    };
    Deployment::new(
        "app",
        ["app"],
        [
            setting("app.user_id", SettingValue::Caller(DataType::Text)),
            setting(
                "app.installation_id",
                SettingValue::Principal { kind: "installation".into(), identity: DataType::Text },
            ),
            setting("app.tenant_id", SettingValue::Scalar(DataType::Text)),
            setting(
                "app.organization_ids",
                SettingValue::Set { element: DataType::Text, packing: SetPacking::JsonArray },
            ),
        ],
    )
    .expect("a valid deployment")
}

/// Requests of `alice`, leaving the installation unset, setting it to `''`
/// and acting through `i1`, `i2` and an installation holding no row.
const ALICE: Request = Request::of("alice");
const ALICE_EMPTY: Request =
    Request { caller: Some("alice"), values: &[("app.installation_id", &[""])] };
const ALICE_I1: Request =
    Request { caller: Some("alice"), values: &[("app.installation_id", &["i1"])] };
const ALICE_I2: Request =
    Request { caller: Some("alice"), values: &[("app.installation_id", &["i2"])] };
const ALICE_IX: Request =
    Request { caller: Some("alice"), values: &[("app.installation_id", &["ix"])] };
const BOB_I1: Request =
    Request { caller: Some("bob"), values: &[("app.installation_id", &["i1"])] };
const BOB_I2: Request =
    Request { caller: Some("bob"), values: &[("app.installation_id", &["i2"])] };

/// What PostgreSQL 18.6 returns for `(helper, arguments, request, returned)`
/// over [`CAP_ROWS`], each returned row its columns joined by `,`. An unset
/// installation was never set in the session, which `current_setting(name,
/// true)` reads as `NULL`.
const CAPS_MEASURED: &[(&str, &[&str], Request, &[&str])] = &[
    ("capped_kinds", &["reader"], ALICE, &[]),
    ("capped_kinds", &["reader"], ALICE_EMPTY, &[]),
    ("capped_kinds", &["reader"], ALICE_I1, &["o1,folder", "o1,organization", "o2,project"]),
    ("capped_kinds", &["admin"], ALICE_I1, &["o2,project"]),
    ("capped_kinds", &["reader"], ALICE_I2, &["o2,folder", "o2,organization"]),
    ("capped_kinds", &["admin"], ALICE_I2, &["o2,organization"]),
    ("capped_kinds", &["reader"], ALICE_IX, &[]),
    ("member_organizations", &[], ALICE, &["o1", "o2"]),
    ("member_organizations", &[], ALICE_EMPTY, &["o1", "o2"]),
    ("member_organizations", &[], ALICE_I1, &["o1"]),
    ("member_organizations", &[], ALICE_I2, &["o2"]),
    ("member_organizations", &[], ALICE_IX, &[]),
    ("member_organizations", &[], BOB_I1, &["o1"]),
    ("member_organizations", &[], BOB_I2, &[]),
    ("owned_resources", &["reader"], ALICE, &["r1", "r2", "r3", "r4"]),
    ("owned_resources", &["reader"], ALICE_EMPTY, &[]),
    ("owned_resources", &["reader"], ALICE_I1, &["r1", "r3"]),
    ("owned_resources", &["admin"], ALICE_I1, &["r3"]),
    ("owned_resources", &["reader"], ALICE_I2, &["r4"]),
    ("owned_resources", &["admin"], ALICE_I2, &[]),
    ("owned_resources", &["reader"], ALICE_IX, &[]),
    ("owned_resources", &["reader"], BOB_I1, &["r5"]),
    ("capped_resources", &["reader"], ALICE, &[]),
    ("capped_resources", &["reader"], ALICE_I1, &["r1", "r3", "r5"]),
    ("capped_resources", &["admin"], ALICE_I1, &["r3"]),
    ("capped_resources", &["reader"], ALICE_I2, &["r4"]),
];

#[test]
fn a_cap_keeps_the_rows_whose_pair_the_capping_helper_returns() {
    let database = postgres(CAPS);
    let deployment = installation_deployment();
    let capped_kinds = |role: &str| {
        format!(
            "authz.capped_kinds('{role}') {{installation_permissions.(organization_id, kind) WHERE installation_id = app.installation_id AND max_role IN ('admin', '{role}')}}"
        )
    };
    let cases = [
        (
            "capped_kinds",
            "installation_permissions.(organization_id, kind) WHERE installation_id = app.installation_id AND max_role IN ('admin', 'reader')"
                .to_string(),
        ),
        (
            "member_organizations",
            format!(
                "memberships.organization_id WHERE user_id = app.user_id AND (app.installation_id IS UNSET OR (organization_id, 'organization') IN {})",
                capped_kinds("reader")
            ),
        ),
        (
            "owned_resources",
            format!(
                "resources.id WHERE owner_id = app.user_id AND (app.installation_id IS NULL OR (organization_id, kind) IN {})",
                capped_kinds("reader")
            ),
        ),
        (
            "capped_resources",
            format!("resources.id WHERE (organization_id, kind) IN {}", capped_kinds("reader")),
        ),
    ];
    for (helper, expected) in cases {
        let arguments: &[Value] =
            if helper == "member_organizations" { &[] } else { &[text("reader")] };
        let set = answer(&database, &deployment, "authz", helper, arguments).expect("answered");
        assert_eq!(render(&set), expected, "{helper}");
    }
    for &(helper, arguments, request, expected) in CAPS_MEASURED {
        let arguments: Vec<Value> = arguments.iter().map(|argument| text(argument)).collect();
        let set = answer(&database, &deployment, "authz", helper, &arguments).expect("answered");
        let returned: Vec<String> =
            arm_rows(&set, request, CAP_ROWS).into_iter().map(|row| row.join(",")).collect();
        assert_eq!(returned, expected, "{helper}({arguments:?}) for {:?}", request.values);
    }
}

/// The condition of `authz.capped_resources` in [`CAPS`].
const CAPPED_CONDITION: &str = "WHERE (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))";

/// `authz.capped_resources` with `condition` as its condition.
fn capping(condition: &str) -> Result<String, HelperSetError> {
    let database = postgres(&CAPS.replace(CAPPED_CONDITION, &format!("WHERE {condition}")));
    let deployment = installation_deployment();
    answer(&database, &deployment, "authz", "capped_resources", &[text("reader")])
        .map(|set| render(&set))
}

#[test]
fn a_cap_compares_one_operand_per_column_the_capping_helper_returns() {
    for condition in [
        "(resource.organization_id) IN (SELECT * FROM authz.capped_kinds(min_role))",
        "(resource.organization_id, resource.kind, resource.id) IN (SELECT * FROM authz.capped_kinds(min_role))",
        "(resource.organization_id, resource.kind) IN (SELECT authz.capped_kinds(min_role))",
        "(resource.organization_id, 'project'::TEXT) IN (SELECT * FROM authz.capped_kinds(min_role))",
        "(resource.organization_id, current_setting('app.user_id', true)) IN (SELECT * FROM authz.capped_kinds(min_role))",
        "(resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role) WHERE true)",
        "(resource.organization_id, resource.kind) IN (VALUES ('o1', 'folder'))",
        "(resource.organization_id, resource.kind) IN (SELECT * FROM public.installation_permissions)",
        "current_setting('app.installation_id', true) IS NULL OR resource.id IN (SELECT * FROM authz.capped_kinds(min_role))",
        "current_setting('app.installation_id', true) IS NULL OR 'o1'::INT IN (SELECT authz.member_organizations())",
    ] {
        let answer = capping(condition);
        assert!(matches!(answer, Err(HelperSetError::ArmShape { .. })), "{condition}: {answer:?}");
    }
    for (operand, rendered) in
        [("'project'::authz.resource_kind", "'project'"), ("min_role", "'reader'")]
    {
        assert_eq!(
            capping(&format!("(resource.organization_id, {operand}) IN (SELECT * FROM authz.capped_kinds(min_role) AS capped)"))
                .expect("answered")
                .split(" {")
                .next(),
            Some(format!("resources.id WHERE (organization_id, {rendered}) IN authz.capped_kinds('reader')").as_str()),
            "{operand}"
        );
    }
    assert_eq!(
        capping("current_setting('app.installation_id', true)::TEXT IS NULL OR 'o1'::TEXT IN (SELECT authz.member_organizations())")
            .expect("answered")
            .split(" {")
            .next(),
        Some("resources.id WHERE (app.installation_id IS NULL OR ('o1') IN authz.member_organizations()")
    );
    assert_eq!(
        capping("current_setting('app.installation_id', true) IS NULL OR resource.organization_id IN (SELECT authz.member_organizations())")
            .expect("answered"),
        "resources.id WHERE (app.installation_id IS NULL OR (organization_id) IN authz.member_organizations() {memberships.organization_id WHERE user_id = app.user_id AND (app.installation_id IS UNSET OR (organization_id, 'organization') IN authz.capped_kinds('reader') {installation_permissions.(organization_id, kind) WHERE installation_id = app.installation_id AND max_role IN ('admin', 'reader')})})"
    );
    for setting in ["app.installation_id", "app.organization_ids"] {
        let answer = capping(&format!(
            "current_setting('{setting}', true)::INT IS NULL OR (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))"
        ));
        assert!(
            matches!(&answer, Err(HelperSetError::SettingCast { setting: cast, .. }) if cast == setting),
            "{answer:?}"
        );
    }
    assert!(matches!(
        capping("current_setting('app.region_id', true) IS NULL OR (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))"),
        Err(HelperSetError::UndeclaredSetting { setting, .. }) if setting == "app.region_id"
    ));
    let unlifted = capping(
        "current_setting('app.installation_id', true) IS NULL OR resource.kind = 'project'",
    );
    assert!(matches!(unlifted, Err(HelperSetError::Irreducible { .. })), "{unlifted:?}");
}

#[test]
fn a_lift_records_whether_reading_its_setting_raises_when_unset() {
    for (setting, missing_ok, raises) in [
        ("app.installation_id", "", true),
        ("app.installation_id", ", true", false),
        ("app.tenant_id", "", true),
        ("app.tenant_id", ", true", false),
    ] {
        let read = format!("current_setting('{setting}'{missing_ok})::TEXT");
        let lifted = format!("{CAPPED_CONDITION} OR {read} IS NULL");
        let database = postgres(&CAPS.replace(CAPPED_CONDITION, &lifted));
        let deployment = installation_deployment();
        let set = answer(&database, &deployment, "authz", "capped_resources", &[text("reader")])
            .expect("answered");
        let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
        let [cap] = grant.caps() else { panic!("one cap") };
        let lift = cap.lift().expect("a lifted cap");
        assert_eq!(lift.setting().name(), setting);
        assert_eq!(lift.raises_when_unset(), raises, "{read}");
        assert!(!lift.reads_empty_as_unset(), "{read}");
    }
}

#[test]
fn every_arm_an_in_list_of_principals_answers_keeps_the_caps() {
    let answer = capping(
        "resource.owner_id IN (current_setting('app.user_id', true), 'nobody') \
         AND (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))",
    )
    .expect("answered");
    let cap = "(organization_id, kind) IN authz.capped_kinds('reader')";
    let arms: Vec<_> = answer.split(" UNION ").collect();
    assert_eq!(arms.len(), 2, "{answer}");
    assert!(arms[0].starts_with("resources.id WHERE owner_id = app.user_id AND "), "{answer}");
    assert!(arms[1].starts_with("resources.id WHERE owner_id IN ('nobody') AND "), "{answer}");
    assert!(arms.iter().all(|arm| arm.contains(cap)), "{answer}");
}

const CALLER_CONDITIONS: &str = "
CREATE FUNCTION authz.member_resources() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE resource.organization_id IN (SELECT authz.member_organizations())
    AND resource.owner_id = NULLIF(current_setting('app.user_id', true), '')
$$;
CREATE FUNCTION authz.personal_resources() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE resource.owner_id = NULLIF(current_setting('app.user_id', true), '')
    AND NULLIF(current_setting('app.installation_id', true), '') IS NULL
$$;
CREATE FUNCTION authz.unpooled_resources() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE resource.owner_id = NULLIF(current_setting('app.user_id', true), '')
    AND current_setting('app.installation_id', true) IS NULL
$$;
ALTER FUNCTION authz.member_resources() OWNER TO authz_owner;
ALTER FUNCTION authz.personal_resources() OWNER TO authz_owner;
ALTER FUNCTION authz.unpooled_resources() OWNER TO authz_owner;
";

/// What PostgreSQL 18.7 returns for `(helper, request, returned)` over
/// [`CAP_ROWS`], the helpers of [`CALLER_CONDITIONS`] declared after
/// [`CAPS`].
const CALLER_CONDITIONS_MEASURED: &[(&str, Request, &[&str])] = &[
    ("member_resources", ALICE, &["r1", "r2", "r3", "r4"]),
    ("member_resources", ALICE_EMPTY, &["r1", "r2", "r3", "r4"]),
    ("member_resources", ALICE_I1, &["r1", "r2"]),
    ("member_resources", ALICE_I2, &["r3", "r4"]),
    ("member_resources", ALICE_IX, &[]),
    ("member_resources", BOB_I1, &["r5"]),
    ("member_resources", BOB_I2, &[]),
    ("personal_resources", ALICE, &["r1", "r2", "r3", "r4"]),
    ("personal_resources", ALICE_EMPTY, &["r1", "r2", "r3", "r4"]),
    ("personal_resources", ALICE_I1, &[]),
    ("personal_resources", BOB_I1, &[]),
    ("unpooled_resources", ALICE, &["r1", "r2", "r3", "r4"]),
    ("unpooled_resources", ALICE_EMPTY, &[]),
    ("unpooled_resources", ALICE_I1, &[]),
];

/// [`CAPS`] and [`CALLER_CONDITIONS`], the condition of `member_resources`
/// replaced by `condition` where one is given.
fn caller_conditions(condition: Option<&str>) -> ParserDB {
    let mut sql = format!("{CAPS}{CALLER_CONDITIONS}");
    if let Some(condition) = condition {
        let written = "WHERE resource.organization_id IN (SELECT authz.member_organizations())\n    AND resource.owner_id = NULLIF(current_setting('app.user_id', true), '')";
        assert!(sql.contains(written), "the condition is replaced");
        sql = sql.replace(written, &format!("WHERE {condition}"));
    }
    postgres(&sql)
}

fn read_unset<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
    name: &str,
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    function(database, "authz", name).helper_set(
        &[],
        deployment,
        database,
        &[ArmCondition::UnsetSetting],
    )
}

#[test]
fn a_set_beside_a_grantee_the_request_names_caps_the_arm() {
    let deployment = installation_deployment();
    let member_organizations = "authz.member_organizations() {memberships.organization_id WHERE user_id = app.user_id AND (app.installation_id IS UNSET OR (organization_id, 'organization') IN authz.capped_kinds('reader') {installation_permissions.(organization_id, kind) WHERE installation_id = app.installation_id AND max_role IN ('admin', 'reader')})}";
    let expected = format!(
        "resources.id WHERE owner_id = app.user_id AND (organization_id) IN {member_organizations}"
    );
    let owner = "resource.owner_id = NULLIF(current_setting('app.user_id', true), '')";
    let member = "resource.organization_id IN (SELECT authz.member_organizations())";
    for condition in [None, Some(format!("{owner} AND {member}"))] {
        let database = caller_conditions(condition.as_deref());
        let set =
            answer(&database, &deployment, "authz", "member_resources", &[]).expect("answered");
        assert_eq!(render(&set), expected, "{condition:?}");
    }
    let database = caller_conditions(None);
    for &(helper, request, expected) in CALLER_CONDITIONS_MEASURED {
        let set = read_unset(&database, &deployment, helper).expect("answered");
        let returned: Vec<_> = evaluate(&set, request, CAP_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{helper}() for {:?}", request.values);
    }
    // Two sets beside no grantee the request names leave the grantee open.
    let database = caller_conditions(Some(&format!(
        "{member} AND resource.owner_id IN (SELECT authz.member_organizations())"
    )));
    let answered = answer(&database, &deployment, "authz", "member_resources", &[]);
    assert!(matches!(answered, Err(HelperSetError::ArmShape { .. })), "{answered:?}");
}

#[test]
fn a_setting_required_unset_keeps_the_arm_only_for_a_request_leaving_it_unset() {
    let database = caller_conditions(None);
    let deployment = installation_deployment();
    for (helper, unset) in [("personal_resources", "UNSET"), ("unpooled_resources", "NULL")] {
        let set = read_unset(&database, &deployment, helper).expect("answered");
        assert_eq!(
            render(&set),
            format!("resources.id WHERE owner_id = app.user_id AND app.installation_id IS {unset}")
        );
        let refused = answer(&database, &deployment, "authz", helper, &[]);
        assert!(
            matches!(
                &refused,
                Err(HelperSetError::UnreadCondition { function, kind: ArmCondition::UnsetSetting, .. })
                    if function == &format!("authz.{helper}")
            ),
            "{refused:?}"
        );
    }
}

/// Grants a membership takes away once deactivated, and objects whose
/// archived parent hides them.
const ANTI_JOINS: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE TYPE membership_state AS ENUM ('active', 'deactivated');
CREATE TABLE organizations (id TEXT PRIMARY KEY);
CREATE TABLE memberships (
  organization_id TEXT NOT NULL REFERENCES organizations (id),
  user_id TEXT NOT NULL,
  state membership_state NOT NULL,
  PRIMARY KEY (organization_id, user_id)
);
CREATE TABLE grants (
  resource_id TEXT NOT NULL,
  organization_id TEXT REFERENCES organizations (id),
  grantee_id TEXT NOT NULL
);
CREATE TABLE objects (
  id TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES objects (id),
  owner_id TEXT NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT false
);
CREATE FUNCTION resources_with() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT permission.resource_id FROM public.grants permission
  WHERE permission.grantee_id = current_setting('app.user_id', true)
    AND NOT EXISTS (
      SELECT 1 FROM public.memberships membership
      WHERE membership.organization_id = permission.organization_id
        AND membership.user_id = permission.grantee_id
        AND membership.state = 'deactivated'
    )
$$;
CREATE FUNCTION objects_unblocked() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT object.id FROM public.objects object
  WHERE object.owner_id = current_setting('app.user_id', true)
    AND NOT (EXISTS (
      SELECT * FROM public.objects blocked WHERE object.parent_id = blocked.id AND blocked.archived
    ))
$$;
ALTER FUNCTION resources_with() OWNER TO authz_owner;
ALTER FUNCTION objects_unblocked() OWNER TO authz_owner;
GRANT SELECT ON organizations, memberships, grants, objects TO authz_owner;
";

/// The rows the helpers of [`ANTI_JOINS`] were called over.
const ANTI_JOIN_ROWS: Rows = &[
    ("memberships", &[("organization_id", "o1"), ("user_id", "alice"), ("state", "active")]),
    ("memberships", &[("organization_id", "o2"), ("user_id", "alice"), ("state", "deactivated")]),
    ("memberships", &[("organization_id", "o2"), ("user_id", "bob"), ("state", "active")]),
    ("memberships", &[("organization_id", "o3"), ("user_id", "carol"), ("state", "deactivated")]),
    ("grants", &[("resource_id", "r1"), ("organization_id", "o1"), ("grantee_id", "alice")]),
    ("grants", &[("resource_id", "r2"), ("organization_id", "o2"), ("grantee_id", "alice")]),
    ("grants", &[("resource_id", "r3"), ("grantee_id", "alice")]),
    ("grants", &[("resource_id", "r4"), ("organization_id", "o3"), ("grantee_id", "alice")]),
    ("grants", &[("resource_id", "r5"), ("organization_id", "o2"), ("grantee_id", "bob")]),
    ("grants", &[("resource_id", "r6"), ("organization_id", "o3"), ("grantee_id", "carol")]),
    ("objects", &[("id", "a"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "x"), ("owner_id", "bob"), ("archived", "true")]),
    ("objects", &[("id", "b"), ("parent_id", "a"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "c"), ("parent_id", "x"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "d"), ("parent_id", "x"), ("owner_id", "bob"), ("archived", "false")]),
    ("objects", &[("id", "y"), ("parent_id", "b"), ("owner_id", "alice"), ("archived", "true")]),
];

/// What PostgreSQL 16.15 returns for `(caller, helper)`, the helpers of
/// [`ANTI_JOINS`] called over [`ANTI_JOIN_ROWS`].
const ANTI_JOINS_MEASURED: &[(&str, &str, &[&str])] = &[
    ("alice", "resources_with", &["r1", "r3", "r4"]),
    ("alice", "objects_unblocked", &["a", "b", "y"]),
    ("bob", "resources_with", &["r5"]),
    ("bob", "objects_unblocked", &["x"]),
    ("carol", "resources_with", &[]),
    ("carol", "objects_unblocked", &[]),
    ("zed", "resources_with", &[]),
    ("zed", "objects_unblocked", &[]),
];

#[test]
fn an_anti_join_keeps_a_grant_only_where_no_blocking_row_matches_it() {
    let database = postgres(ANTI_JOINS);
    let deployment = deployment();
    let resources =
        answer(&database, &deployment, "public", "resources_with", &[]).expect("answered");
    assert_eq!(
        render(&resources),
        "grants.resource_id WHERE grantee_id = app.user_id AND NOT EXISTS (keyed memberships \
         WHERE organization_id = grants.organization_id AND user_id = grants.grantee_id \
         AND state IN ('deactivated'))"
    );
    let objects =
        answer(&database, &deployment, "public", "objects_unblocked", &[]).expect("answered");
    assert_eq!(
        render(&objects),
        "objects.id WHERE owner_id = app.user_id AND NOT EXISTS (keyed objects \
         WHERE id = objects.parent_id AND archived IN (true))"
    );
    for &(caller, helper, measured) in ANTI_JOINS_MEASURED {
        let set = if helper == "resources_with" { &resources } else { &objects };
        let returned: Vec<_> =
            evaluate(set, Request::of(caller), ANTI_JOIN_ROWS).into_iter().collect();
        assert_eq!(returned, measured, "{caller:?} calling {helper}()");
    }
}

/// What `resources_with()` of [`ANTI_JOINS`] answers, rendered, with
/// `blocking` in place of its `NOT EXISTS` subquery, `after` following the
/// helper.
fn excluded_by(blocking: &str, after: &str) -> Result<String, HelperSetError> {
    let written = "SELECT 1 FROM public.memberships membership
      WHERE membership.organization_id = permission.organization_id
        AND membership.user_id = permission.grantee_id
        AND membership.state = 'deactivated'";
    let sql = format!("{}{after}", ANTI_JOINS.replace(written, blocking));
    assert!(sql.contains(blocking), "the subquery is replaced");
    let database = postgres(&sql);
    let deployment = deployment();
    answer(&database, &deployment, "public", "resources_with", &[]).map(|set| render(&set))
}

#[test]
fn an_anti_join_reads_its_correlations_by_postgres_name_resolution() {
    let arm = "grants.resource_id WHERE grantee_id = app.user_id";
    for (blocking, expected) in [
        (
            "SELECT 1 FROM public.memberships WHERE grantee_id = user_id AND organization_id = permission.organization_id",
            "NOT EXISTS (keyed memberships WHERE user_id = grants.grantee_id \
             AND organization_id = grants.organization_id)",
        ),
        (
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
            "NOT EXISTS (memberships WHERE user_id = grants.grantee_id)",
        ),
        (
            "SELECT NULL FROM public.memberships m WHERE m.user_id = permission.grantee_id \
             AND m.state IN ('deactivated', 'active') AND m.state = 'deactivated'",
            "NOT EXISTS (memberships WHERE user_id = grants.grantee_id AND state IN ('deactivated'))",
        ),
        (
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
             AND m.state IS NOT NULL",
            "NOT EXISTS (memberships WHERE user_id = grants.grantee_id AND state IS NOT NULL)",
        ),
        (
            "SELECT 1 FROM public.organizations o WHERE o.id = permission.organization_id",
            "NOT EXISTS (keyed organizations WHERE id = grants.organization_id)",
        ),
    ] {
        assert_eq!(
            excluded_by(blocking, "").expect("answered"),
            format!("{arm} AND {expected}"),
            "{blocking}"
        );
    }
    assert_eq!(
        excluded_by(
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id AND m.state = 'deactivated' AND false",
            ""
        )
        .expect("answered"),
        arm,
        "an anti-join keeping no blocking row takes no row away"
    );
    assert_eq!(
        excluded_by(
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
            "CREATE UNIQUE INDEX one_membership ON memberships (user_id);"
        )
        .expect("answered"),
        format!("{arm} AND NOT EXISTS (keyed memberships WHERE user_id = grants.grantee_id)"),
        "a unique index keys the blocking rows"
    );
    assert_eq!(
        excluded_by(
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
            "ALTER TABLE memberships ADD CONSTRAINT one_membership UNIQUE (user_id);"
        )
        .expect("answered"),
        format!("{arm} AND NOT EXISTS (keyed memberships WHERE user_id = grants.grantee_id)"),
        "a unique constraint keys the blocking rows"
    );
    assert_eq!(
        excluded_by(
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
            "CREATE UNIQUE INDEX one_active ON memberships (user_id) WHERE state = 'active';"
        )
        .expect("answered"),
        format!("{arm} AND NOT EXISTS (memberships WHERE user_id = grants.grantee_id)"),
        "a partial unique index keys only the rows its predicate holds for"
    );
}

const EXCLUSIONS: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE ROLE app NOLOGIN;
CREATE TABLE teams (id TEXT PRIMARY KEY);
CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  team_id TEXT REFERENCES teams (id)
);
CREATE TABLE project_blocks (
  project_id TEXT NOT NULL REFERENCES projects (id),
  principal_id TEXT NOT NULL,
  PRIMARY KEY (project_id, principal_id)
);
CREATE TABLE team_blocks (
  team_id TEXT NOT NULL REFERENCES teams (id),
  principal_id TEXT NOT NULL,
  PRIMARY KEY (team_id, principal_id)
);
CREATE FUNCTION blocked_projects() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT block.project_id FROM public.project_blocks block
  WHERE block.principal_id = current_setting('app.user_id', true)
$$;
CREATE FUNCTION blocked_teams() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT block.team_id FROM public.team_blocks block
  WHERE block.principal_id = current_setting('app.user_id', true)
$$;
CREATE FUNCTION visible_projects() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT project.id FROM public.projects project
  WHERE project.owner_id = current_setting('app.user_id', true)
    AND NOT (project.id = ANY (ARRAY(SELECT public.blocked_projects())))
$$;
CREATE FUNCTION unblocked_team_projects() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT project.id FROM public.projects project
  WHERE project.team_id NOT IN (SELECT public.blocked_teams())
$$;
ALTER FUNCTION blocked_projects() OWNER TO authz_owner;
ALTER FUNCTION blocked_teams() OWNER TO authz_owner;
ALTER FUNCTION visible_projects() OWNER TO authz_owner;
ALTER FUNCTION unblocked_team_projects() OWNER TO authz_owner;
REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC;
GRANT EXECUTE ON FUNCTION visible_projects(), unblocked_team_projects() TO app;
GRANT SELECT ON teams, projects, project_blocks, team_blocks TO authz_owner;
";

const EXCLUSION_ROWS: Rows = &[
    ("teams", &[("id", "t1")]),
    ("teams", &[("id", "t2")]),
    ("projects", &[("id", "p1"), ("owner_id", "alice"), ("team_id", "t1")]),
    ("projects", &[("id", "p2"), ("owner_id", "alice"), ("team_id", "t2")]),
    ("projects", &[("id", "p3"), ("owner_id", "alice")]),
    ("projects", &[("id", "p4"), ("owner_id", "bob"), ("team_id", "t1")]),
    ("projects", &[("id", "p5"), ("owner_id", "bob")]),
    ("project_blocks", &[("project_id", "p2"), ("principal_id", "alice")]),
    ("project_blocks", &[("project_id", "p4"), ("principal_id", "alice")]),
    ("project_blocks", &[("project_id", "p4"), ("principal_id", "bob")]),
    ("team_blocks", &[("team_id", "t2"), ("principal_id", "alice")]),
    ("team_blocks", &[("team_id", "t1"), ("principal_id", "carol")]),
];

/// What PostgreSQL 18.7 returns for `(caller, helper)`, the helpers of
/// [`EXCLUSIONS`] called as `app` over [`EXCLUSION_ROWS`].
const EXCLUSIONS_MEASURED: &[(&str, &str, &[&str])] = &[
    ("alice", "visible_projects", &["p1", "p3"]),
    ("alice", "unblocked_team_projects", &["p1", "p4"]),
    ("bob", "visible_projects", &["p5"]),
    ("bob", "unblocked_team_projects", &["p1", "p2", "p3", "p4", "p5"]),
    ("carol", "visible_projects", &[]),
    ("carol", "unblocked_team_projects", &["p2"]),
    ("zed", "visible_projects", &[]),
    ("zed", "unblocked_team_projects", &["p1", "p2", "p3", "p4", "p5"]),
];

const VISIBLE_PROJECTS: &str = "projects.id WHERE owner_id = app.user_id AND id NOT IN \
     blocked_projects() {project_blocks.project_id WHERE principal_id = app.user_id}";

fn excluding<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
    name: &str,
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    function(database, "public", name).helper_set(
        &[],
        deployment,
        database,
        &[ArmCondition::Exclusion],
    )
}

#[test]
fn an_exclusion_keeps_the_rows_outside_another_helpers_set() {
    let database = postgres(EXCLUSIONS);
    let deployment = deployment();
    let visible = excluding(&database, &deployment, "visible_projects").expect("answered");
    assert_eq!(render(&visible), VISIBLE_PROJECTS);
    let unblocked = excluding(&database, &deployment, "unblocked_team_projects").expect("answered");
    assert_eq!(
        render(&unblocked),
        "projects.id WHERE team_id NOT IN blocked_teams() \
         {team_blocks.team_id WHERE principal_id = app.user_id}"
    );
    for &(caller, helper, measured) in EXCLUSIONS_MEASURED {
        let set = if helper == "visible_projects" { &visible } else { &unblocked };
        let returned: Vec<_> =
            evaluate(set, Request::of(caller), EXCLUSION_ROWS).into_iter().collect();
        assert_eq!(returned, measured, "{caller:?} calling {helper}()");
    }
}

/// `visible_projects()` of [`EXCLUSIONS`] with `condition` in place of its
/// exclusion, read with exclusions or without.
fn excluded_as(condition: &str, reads_exclusions: bool) -> Result<String, HelperSetError> {
    let written = "NOT (project.id = ANY (ARRAY(SELECT public.blocked_projects())))";
    let sql = EXCLUSIONS.replace(written, condition);
    assert!(sql.contains(condition), "the exclusion is replaced");
    let caller = SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))
        .expect("a custom setting name");
    let blocked =
        SessionSetting::value("app.blocked", delimited(",")).expect("a custom setting name");
    let deployment =
        Deployment::new("app", ["app"], [caller, blocked]).expect("a valid deployment");
    let database = postgres(&sql);
    let visible = function(&database, "public", "visible_projects");
    let conditions: &[ArmCondition] =
        if reads_exclusions { &[ArmCondition::Exclusion] } else { &[] };
    visible.helper_set(&[], &deployment, &database, conditions).map(|set| render(&set))
}

#[test]
fn every_spelling_of_an_exclusion_reads_alike() {
    for condition in [
        "project.id NOT IN (SELECT public.blocked_projects())",
        "NOT (project.id IN (SELECT public.blocked_projects()))",
        "project.id <> ALL (ARRAY(SELECT public.blocked_projects()))",
        "project.id <> ALL (SELECT public.blocked_projects())",
        "NOT (project.id = ANY (SELECT public.blocked_projects()))",
        "NOT ((id) = ANY (ARRAY(SELECT public.blocked_projects())))",
    ] {
        assert_eq!(excluded_as(condition, true).as_deref(), Ok(VISIBLE_PROJECTS), "{condition}");
    }
}

/// A consumer reading no exclusion would keep the rows the helper excludes.
#[test]
fn a_reading_leaving_exclusions_unread_refuses_one() {
    let condition = "project.id NOT IN (SELECT public.blocked_projects())";
    let refused = excluded_as(condition, false);
    assert!(
        matches!(
            &refused,
            Err(HelperSetError::UnreadCondition { function, condition, kind: ArmCondition::Exclusion })
                if function == "visible_projects"
                    && condition == "project.id NOT IN (SELECT public.blocked_projects())"
        ),
        "{refused:?}"
    );
    let calling = format!(
        "{EXCLUSIONS}CREATE FUNCTION visible_teams() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT project.team_id FROM public.projects project
           WHERE project.id IN (SELECT public.visible_projects())
         $$;
         ALTER FUNCTION visible_teams() OWNER TO authz_owner;"
    );
    let database = postgres(&calling);
    let deployment = deployment();
    let teams = answer(&database, &deployment, "public", "visible_teams", &[]);
    assert!(
        matches!(
            &teams,
            Err(HelperSetError::UnreadCondition { function, kind: ArmCondition::Exclusion, .. })
                if function == "visible_projects"
        ),
        "{teams:?}"
    );
    assert!(excluding(&database, &deployment, "visible_teams").is_ok());
}

#[test]
fn a_negated_comparison_with_no_helper_is_no_exclusion() {
    for condition in [
        "project.id NOT IN (SELECT unnest(string_to_array(current_setting('app.blocked', true), ',')))",
        "NOT (project.id = ANY (string_to_array(current_setting('app.blocked', true), ',')))",
        "'p1' NOT IN (SELECT public.blocked_projects())",
        "project.id <> ANY (ARRAY(SELECT public.blocked_projects()))",
        "project.id <> ALL ('{p9}'::TEXT[])",
        "NOT (lower(project.id) = ANY (ARRAY(SELECT public.blocked_projects())))",
        "project.id NOT IN (SELECT public.blocked_projects() LIMIT 1)",
        "project.id NOT IN (SELECT public.blocked_projects() UNION SELECT 'p9')",
    ] {
        for reads_exclusions in [false, true] {
            let answered = excluded_as(condition, reads_exclusions);
            assert!(
                matches!(answered, Err(HelperSetError::Irreducible { .. })),
                "{condition}: {answered:?}"
            );
        }
    }
}

#[test]
fn an_exclusion_calls_one_helper_returning_one_column() {
    let unresolved = excluded_as("project.id NOT IN (SELECT public.unblocked())", true);
    assert!(
        matches!(
            &unresolved,
            Err(HelperSetError::UnresolvedCall { function, call })
                if function == "visible_projects" && call == "public.unblocked"
        ),
        "{unresolved:?}"
    );
    let pairs = EXCLUSIONS.replace(
        "CREATE FUNCTION blocked_projects() RETURNS SETOF TEXT\nLANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$\n  SELECT block.project_id FROM",
        "CREATE FUNCTION blocked_projects() RETURNS TABLE (project_id TEXT, principal_id TEXT)\nLANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$\n  SELECT block.project_id, block.principal_id FROM",
    );
    assert_ne!(pairs, EXCLUSIONS, "the excluded helper returns two columns");
    let database = postgres(&pairs);
    let deployment = deployment();
    let refused = excluding(&database, &deployment, "visible_projects");
    assert!(matches!(refused, Err(HelperSetError::ArmShape { .. })), "{refused:?}");
}

#[test]
fn an_exclusion_of_a_set_holding_nothing_takes_no_row_away() {
    let condition = "block.principal_id = current_setting('app.user_id', true)\n$$;\nCREATE FUNCTION blocked_teams";
    let empty = EXCLUSIONS.replace(
        condition,
        "block.principal_id = current_setting('app.user_id', true) AND false\n$$;\nCREATE FUNCTION blocked_teams",
    );
    assert_ne!(empty, EXCLUSIONS, "the excluded helper keeps no row");
    let database = postgres(&empty);
    let deployment = deployment();
    assert_eq!(
        excluding(&database, &deployment, "visible_projects").map(|set| render(&set)).as_deref(),
        Ok("projects.id WHERE owner_id = app.user_id")
    );
}

/// PostgreSQL runs the excluded helper as the owner of the helper excluding
/// it, so the owner needs `EXECUTE` on it.
#[test]
fn an_exclusion_needs_its_owner_to_execute_the_helper_excluded() {
    let sql = EXCLUSIONS.replace(
        "ALTER FUNCTION blocked_projects() OWNER TO authz_owner;",
        "CREATE ROLE blocks_owner NOLOGIN BYPASSRLS;
         GRANT SELECT ON project_blocks TO blocks_owner;
         ALTER FUNCTION blocked_projects() OWNER TO blocks_owner;",
    );
    let database = postgres(&sql);
    let deployment = deployment();
    let refused = excluding(&database, &deployment, "visible_projects");
    assert!(
        matches!(
            &refused,
            Err(HelperSetError::MissingExecute { function, owner, called })
                if function == "visible_projects"
                    && owner == "authz_owner"
                    && called == "blocked_projects"
        ),
        "{refused:?}"
    );
    let granted = format!("{sql}GRANT EXECUTE ON FUNCTION blocked_projects() TO authz_owner;");
    let database = postgres(&granted);
    assert_eq!(
        excluding(&database, &deployment, "visible_projects").map(|set| render(&set)).as_deref(),
        Ok(VISIBLE_PROJECTS)
    );
}

/// A unique index created only on a partitioned table with a partition
/// enforces nothing on that partition, so it keys no blocking row.
#[test]
fn an_invalid_unique_index_keys_no_blocking_row() {
    let blocking = "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id";
    let arm = "grants.resource_id WHERE grantee_id = app.user_id";
    let partitioned = ANTI_JOINS
        .replace(
            "  PRIMARY KEY (organization_id, user_id)\n);",
            "  PRIMARY KEY (organization_id, user_id)\n) PARTITION BY LIST (user_id);
CREATE TABLE memberships_alice PARTITION OF memberships FOR VALUES IN ('alice');",
        )
        .replace(
            "SELECT 1 FROM public.memberships membership
      WHERE membership.organization_id = permission.organization_id
        AND membership.user_id = permission.grantee_id
        AND membership.state = 'deactivated'",
            blocking,
        );
    for (index, keyed) in [
        ("CREATE UNIQUE INDEX one_membership ON ONLY memberships (user_id);", ""),
        ("CREATE UNIQUE INDEX one_membership ON memberships (user_id);", "keyed "),
    ] {
        let database = postgres(&format!("{partitioned}{index}"));
        let deployment = deployment();
        let set =
            answer(&database, &deployment, "public", "resources_with", &[]).expect("answered");
        assert_eq!(
            render(&set),
            format!("{arm} AND NOT EXISTS ({keyed}memberships WHERE user_id = grants.grantee_id)"),
            "{index}"
        );
    }
}

#[test]
fn an_anti_join_of_any_other_shape_refuses_the_answer() {
    for blocking in [
        "SELECT 1 FROM public.memberships m WHERE m.state = 'deactivated'",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
         AND m.organization_id <> permission.organization_id",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
         AND resource_id = 'r1'",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
         AND m.user_id = current_setting('app.user_id', true)",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
         AND m.organization_id = lower(permission.organization_id)",
        "SELECT m.user_id FROM public.memberships m WHERE m.user_id = permission.grantee_id",
        "SELECT 1 FROM public.memberships m JOIN public.organizations o \
         ON o.id = m.organization_id WHERE m.user_id = permission.grantee_id",
        "SELECT 1 FROM public.memberships permission WHERE permission.user_id = permission.user_id",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id LIMIT 1",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id GROUP BY m.state",
        "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id \
         UNION SELECT 1 FROM public.organizations o WHERE o.id = permission.organization_id",
        "SELECT 1 FROM public.memberships m",
        "SELECT 1, 2 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
    ] {
        assert!(
            matches!(excluded_by(blocking, ""), Err(HelperSetError::Irreducible { .. })),
            "{blocking}"
        );
    }
    let positive = ANTI_JOINS.replace("AND NOT EXISTS (", "AND EXISTS (");
    assert_ne!(positive, ANTI_JOINS, "the anti-join is replaced");
    assert!(
        matches!(
            answer(&postgres(&positive), &deployment(), "public", "resources_with", &[]),
            Err(HelperSetError::Irreducible { .. })
        ),
        "a semi-join keeps no grant an anti-join reads"
    );
    let unreadable = ANTI_JOINS.replace(
        "GRANT SELECT ON organizations, memberships, grants, objects",
        "GRANT SELECT ON organizations, grants, objects",
    );
    assert!(
        matches!(
            answer(&postgres(&unreadable), &deployment(), "public", "resources_with", &[]),
            Err(HelperSetError::MissingSelect { table, .. }) if table == "memberships"
        ),
        "the owner reads the blocking rows"
    );
    let some_columns = "ALTER TABLE memberships ADD COLUMN note TEXT;
        REVOKE SELECT ON memberships FROM authz_owner;
        GRANT SELECT (organization_id, user_id, state) ON memberships TO authz_owner;";
    assert!(
        excluded_by(
            "SELECT 1 FROM public.memberships m WHERE m.user_id = permission.grantee_id",
            some_columns
        )
        .is_ok(),
        "the owner reads every column the subquery names"
    );
    assert!(
        matches!(
            excluded_by("SELECT * FROM public.memberships m WHERE m.user_id = permission.grantee_id", some_columns),
            Err(HelperSetError::MissingSelect { table, .. }) if table == "memberships"
        ),
        "`*` reads every column, as PostgreSQL 16.15 refuses it"
    );
}

#[test]
fn a_null_test_on_a_time_column_stays_apart_from_its_bound() {
    assert_eq!(
        bounded("expires_at IS NULL", "").expect("answered"),
        format!("{ROLES} AND expires_at IS NULL")
    );
    assert_eq!(
        bounded("starts_at IS NULL AND (expires_at IS NULL OR expires_at > now())", "")
            .expect("answered"),
        format!("{ROLES} AND starts_at IS NULL AND (expires_at IS NULL OR expires_at > now)")
    );
}

/// Share links revoked by stamping `revoked_at`, active while it is `NULL`.
const REVOCABLE: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE TABLE docs (id TEXT PRIMARY KEY);
CREATE TABLE share_links (
  doc_id TEXT NOT NULL REFERENCES docs (id),
  user_id TEXT NOT NULL,
  revoked_at TIMESTAMPTZ
);
GRANT SELECT ON share_links TO authz_owner;
CREATE FUNCTION docs_shared() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.doc_id FROM public.share_links l
  WHERE l.user_id = current_setting('app.user_id', true) AND l.revoked_at IS NULL
$$;
CREATE FUNCTION docs_revoked() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT l.doc_id FROM public.share_links l
  WHERE l.user_id = current_setting('app.user_id', true) AND l.revoked_at IS NOT NULL
$$;
ALTER FUNCTION docs_shared() OWNER TO authz_owner;
ALTER FUNCTION docs_revoked() OWNER TO authz_owner;
";

const REVOCABLE_ROWS: Rows = &[
    ("share_links", &[("doc_id", "a"), ("user_id", "alice")]),
    (
        "share_links",
        &[("doc_id", "b"), ("user_id", "alice"), ("revoked_at", "2026-01-01 00:00:00+00")],
    ),
    ("share_links", &[("doc_id", "c"), ("user_id", "bob")]),
];

/// What PostgreSQL 18.6 returns for `(caller, docs_shared(), docs_revoked())`
/// over [`REVOCABLE_ROWS`], `None` for a caller setting never set.
const REVOCABLE_MEASURED: &[(Option<&str>, &[&str], &[&str])] = &[
    (None, &[], &[]),
    (Some("alice"), &["a"], &["b"]),
    (Some("bob"), &["c"], &[]),
    (Some("zed"), &[], &[]),
];

#[test]
fn a_revocation_stamp_keeps_a_link_while_it_holds_no_value() {
    let database = postgres(REVOCABLE);
    let deployment = deployment();
    let shared = answer(&database, &deployment, "public", "docs_shared", &[]).expect("answered");
    assert_eq!(
        render(&shared),
        "share_links.doc_id WHERE user_id = app.user_id AND revoked_at IS NULL"
    );
    let revoked = answer(&database, &deployment, "public", "docs_revoked", &[]).expect("answered");
    assert_eq!(
        render(&revoked),
        "share_links.doc_id WHERE user_id = app.user_id AND revoked_at IS NOT NULL"
    );
    for &(caller, expected_shared, expected_revoked) in REVOCABLE_MEASURED {
        let request = Request { caller, values: &[] };
        for (set, expected, called) in [
            (&shared, expected_shared, "docs_shared()"),
            (&revoked, expected_revoked, "docs_revoked()"),
        ] {
            let returned: Vec<_> = evaluate(set, request, REVOCABLE_ROWS).into_iter().collect();
            assert_eq!(returned, expected, "{caller:?} calling {called}");
        }
    }
}

/// A folder tree kept in the edge table `folder_parents`, one row per folder
/// that has a parent, and projects filed in folders through
/// `project_folders`. `authz.folders_reached()` walks from the folders granted
/// to the caller along `term`, and `authz.projects_reached()` returns the
/// projects filed in them.
fn edges(term: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE organizations (id TEXT PRIMARY KEY);
         CREATE TABLE folders (
           id TEXT PRIMARY KEY,
           organization_id TEXT NOT NULL REFERENCES organizations (id),
           UNIQUE (organization_id, id)
         );
         CREATE TABLE folder_parents (
           organization_id TEXT NOT NULL REFERENCES organizations (id),
           folder_id TEXT NOT NULL REFERENCES folders (id),
           parent_id TEXT NOT NULL REFERENCES folders (id),
           PRIMARY KEY (organization_id, folder_id),
           FOREIGN KEY (organization_id, folder_id) REFERENCES folders (organization_id, id),
           FOREIGN KEY (organization_id, parent_id) REFERENCES folders (organization_id, id)
         );
         CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id);
         CREATE TABLE projects (
           id TEXT PRIMARY KEY,
           organization_id TEXT NOT NULL REFERENCES organizations (id),
           UNIQUE (organization_id, id)
         );
         CREATE TABLE project_folders (
           organization_id TEXT NOT NULL REFERENCES organizations (id),
           project_id TEXT NOT NULL REFERENCES projects (id),
           folder_id TEXT NOT NULL REFERENCES folders (id),
           PRIMARY KEY (organization_id, project_id),
           FOREIGN KEY (organization_id, project_id) REFERENCES projects (organization_id, id),
           FOREIGN KEY (organization_id, folder_id) REFERENCES folders (organization_id, id)
         );
         CREATE UNIQUE INDEX project_folders_project ON project_folders (project_id);
         CREATE TABLE folder_grants (
           folder_id TEXT NOT NULL REFERENCES folders (id),
           user_id TEXT NOT NULL,
           PRIMARY KEY (folder_id, user_id)
         );
         GRANT SELECT ON folders, folder_parents, project_folders, folder_grants TO authz_owner;
         CREATE FUNCTION authz.folders_reached() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           WITH RECURSIVE reached (id) AS (
             SELECT fg.folder_id FROM public.folder_grants fg
             WHERE fg.user_id = current_setting('app.user_id', true)
             UNION {term}
           ) SELECT id FROM reached
         $$;
         CREATE FUNCTION authz.projects_reached() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT pf.project_id FROM public.project_folders pf
           WHERE pf.folder_id IN (SELECT authz.folders_reached())
         $$;
         ALTER FUNCTION authz.folders_reached() OWNER TO authz_owner;
         ALTER FUNCTION authz.projects_reached() OWNER TO authz_owner;
         {after}"
    )
}

const EDGE_DOWN: &str =
    "SELECT fp.folder_id FROM public.folder_parents fp JOIN reached ON fp.parent_id = reached.id";
const EDGE_UP: &str =
    "SELECT fp.parent_id FROM public.folder_parents fp JOIN reached ON fp.folder_id = reached.id";

/// `team` under `org`, `squad` under `team`, `other` apart, and a project
/// filed in each but `p_loose`.
const EDGE_TREE: Rows = &[
    ("folder_parents", &[("folder_id", "team"), ("parent_id", "org")]),
    ("folder_parents", &[("folder_id", "squad"), ("parent_id", "team")]),
    ("folder_grants", &[("folder_id", "team"), ("user_id", "alice")]),
    ("folder_grants", &[("folder_id", "org"), ("user_id", "bob")]),
    ("project_folders", &[("project_id", "p_org"), ("folder_id", "org")]),
    ("project_folders", &[("project_id", "p_team"), ("folder_id", "team")]),
    ("project_folders", &[("project_id", "p_squad"), ("folder_id", "squad")]),
    ("project_folders", &[("project_id", "p_other"), ("folder_id", "other")]),
];

/// [`EDGE_TREE`] with `org` moved under `squad`, closing a cycle.
const CYCLIC_EDGE_TREE: Rows = &[
    ("folder_parents", &[("folder_id", "team"), ("parent_id", "org")]),
    ("folder_parents", &[("folder_id", "squad"), ("parent_id", "team")]),
    ("folder_parents", &[("folder_id", "org"), ("parent_id", "squad")]),
    ("folder_grants", &[("folder_id", "team"), ("user_id", "alice")]),
    ("folder_grants", &[("folder_id", "org"), ("user_id", "bob")]),
    ("project_folders", &[("project_id", "p_org"), ("folder_id", "org")]),
    ("project_folders", &[("project_id", "p_team"), ("folder_id", "team")]),
    ("project_folders", &[("project_id", "p_squad"), ("folder_id", "squad")]),
    ("project_folders", &[("project_id", "p_other"), ("folder_id", "other")]),
];

/// What PostgreSQL 18.6 returns for `(rows, caller)` from
/// `authz.folders_reached()` written with [`EDGE_DOWN`], then with
/// [`EDGE_UP`], then from `authz.projects_reached()` over the walk down.
type EdgesMeasured =
    (Rows, &'static str, &'static [&'static str], &'static [&'static str], &'static [&'static str]);

const EDGES_MEASURED: &[EdgesMeasured] = &[
    (EDGE_TREE, "alice", &["squad", "team"], &["org", "team"], &["p_squad", "p_team"]),
    (EDGE_TREE, "bob", &["org", "squad", "team"], &["org"], &["p_org", "p_squad", "p_team"]),
    (EDGE_TREE, "zed", &[], &[], &[]),
    (
        CYCLIC_EDGE_TREE,
        "alice",
        &["org", "squad", "team"],
        &["org", "squad", "team"],
        &["p_org", "p_squad", "p_team"],
    ),
    (
        CYCLIC_EDGE_TREE,
        "bob",
        &["org", "squad", "team"],
        &["org", "squad", "team"],
        &["p_org", "p_squad", "p_team"],
    ),
];

const FOLDER_GRANT_ARM: &str = "folder_grants.folder_id WHERE user_id = app.user_id";

#[test]
fn a_walk_reads_an_edge_table_down_and_up_its_unique_column() {
    let deployment = deployment();
    for (term, rendered, direction) in [
        (EDGE_DOWN, "folder_parents.folder_id WHERE parent_id IN itself", WalkDirection::Down),
        (
            "SELECT fp.folder_id FROM reached r JOIN public.folder_parents AS fp ON r.id = fp.parent_id",
            "folder_parents.folder_id WHERE parent_id IN itself",
            WalkDirection::Down,
        ),
        (EDGE_UP, "folder_parents.parent_id WHERE folder_id IN itself", WalkDirection::Up),
        (
            "SELECT fp.parent_id FROM reached r JOIN public.folder_parents AS fp ON r.id = fp.folder_id",
            "folder_parents.parent_id WHERE folder_id IN itself",
            WalkDirection::Up,
        ),
    ] {
        let database = postgres(&edges(term, ""));
        let set =
            answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
        assert_eq!(render(&set), format!("{FOLDER_GRANT_ARM} UNION {rendered}"), "{term}");
        assert_eq!(covered(&set), Some("folders"), "{term}");
        let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
        assert_eq!(walk.direction(), direction, "{term}");
        assert_eq!(walk.tree().table_name(), "folders", "{term}");
        assert_eq!(render_columns(walk.key()), "id", "{term}");
        assert_eq!(render_columns(walk.child()), "folder_id", "{term}");
        let referenced = walk.foreign_key().referenced_table(&database).expect("resolves");
        assert_eq!(referenced.table_name(), "folders", "{term}");
    }

    for (term, direction) in [(EDGE_DOWN, WalkDirection::Down), (EDGE_UP, WalkDirection::Up)] {
        let database = postgres(&edges(term, ""));
        let set =
            answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
        let measured =
            EDGES_MEASURED.iter().map(|&(rows, caller, down, up, _)| (rows, caller, down, up));
        assert_walked(&set, direction, term, measured);
    }

    let index = "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id);";
    for (description, keyed) in [
        (
            "an edge table keyed by its child",
            edges(EDGE_DOWN, "")
                .replace("PRIMARY KEY (organization_id, folder_id),", "PRIMARY KEY (folder_id),")
                .replace(index, ""),
        ),
        (
            "an edge table with its child unique by constraint",
            edges(EDGE_DOWN, "")
                .replace(index, "ALTER TABLE folder_parents ADD UNIQUE (folder_id);"),
        ),
    ] {
        let database = postgres(&keyed);
        let set =
            answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
        assert_eq!(
            render(&set),
            format!("{FOLDER_GRANT_ARM} UNION folder_parents.folder_id WHERE parent_id IN itself"),
            "{description}"
        );
    }
}

#[test]
fn a_self_referencing_walk_reads_its_key_as_the_child() {
    let database = postgres(&groups(&walking(CHILD_JOIN), ""));
    let deployment = deployment();
    let set = groups_with(&database, &deployment, "read").expect("answered");
    let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
    assert_eq!(walk.tree().table_name(), "groups");
    assert_eq!(walk.table().table_name(), "groups");
    assert_eq!(walk.child(), walk.key());
}

#[test]
fn an_edge_table_of_any_other_shape_refuses_the_walk() {
    let deployment = deployment();
    let organization_parent: &[(&str, &str)] = &[
        (
            "parent_id TEXT NOT NULL REFERENCES folders (id),",
            "parent_id TEXT NOT NULL REFERENCES organizations (id),",
        ),
        (
            "FOREIGN KEY (organization_id, parent_id) REFERENCES folders (organization_id, id)",
            "FOREIGN KEY (organization_id) REFERENCES organizations (id)",
        ),
    ];
    for (term, replacements) in [
        (EDGE_DOWN, organization_parent),
        (
            "SELECT fp.parent_id FROM public.folder_parents fp JOIN reached ON fp.parent_id = reached.id",
            &[],
        ),
        (
            "SELECT fp.folder_id FROM public.folder_parents fp JOIN reached ON fp.folder_id = reached.id",
            &[],
        ),
        (
            "SELECT fp.organization_id FROM public.folder_parents fp JOIN reached ON fp.folder_id = reached.id",
            &[],
        ),
    ] {
        let sql =
            replacements.iter().fold(edges(term, ""), |sql, (from, to)| sql.replace(from, to));
        let database = postgres(&sql);
        assert!(
            matches!(
                answer(&database, &deployment, "authz", "folders_reached", &[]),
                Err(HelperSetError::ArmShape { function, .. }) if function == "authz.folders_reached"
            ),
            "{term} with {replacements:?}"
        );
    }
}

#[test]
fn an_edge_walk_needs_the_owner_to_read_both_edge_columns() {
    let deployment = deployment();
    let columns = |granted: &str| {
        edges(EDGE_DOWN, &format!("GRANT SELECT ({granted}) ON folder_parents TO authz_owner;"))
            .replace("GRANT SELECT ON folders, folder_parents,", "GRANT SELECT ON folders,")
    };
    let database = postgres(&columns("folder_id, parent_id"));
    answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
    let database = postgres(&columns("folder_id"));
    assert!(matches!(
        answer(&database, &deployment, "authz", "folders_reached", &[]),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "folder_parents"
    ));
}

fn templates(term: &str, after: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE templates (id TEXT PRIMARY KEY);
         CREATE TABLE template_grants (
           template_id TEXT NOT NULL REFERENCES templates (id),
           user_id TEXT NOT NULL,
           PRIMARY KEY (template_id, user_id)
         );
         CREATE TABLE template_steps (
           parent_id TEXT NOT NULL REFERENCES templates (id),
           child_id TEXT NOT NULL REFERENCES templates (id),
           kind TEXT NOT NULL,
           completed BOOLEAN NOT NULL,
           retired_at TIMESTAMPTZ,
           organization_id TEXT,
           PRIMARY KEY (parent_id, child_id)
         );
         CREATE TABLE organization_blocks (organization_id TEXT NOT NULL, user_id TEXT NOT NULL);
         GRANT SELECT ON templates, template_grants, template_steps TO authz_owner;
         GRANT SELECT ON organization_blocks TO authz_owner;
         CREATE FUNCTION authz.blocking_organizations() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT ob.organization_id FROM public.organization_blocks ob
           WHERE ob.user_id = current_setting('app.user_id', true)
         $$;
         ALTER FUNCTION authz.blocking_organizations() OWNER TO authz_owner;
         CREATE FUNCTION authz.templates_reached(wanted TEXT) RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           WITH RECURSIVE reached (id) AS (
             SELECT tg.template_id FROM public.template_grants tg
             WHERE tg.user_id = current_setting('app.user_id', true)
             UNION {term}
           ) SELECT id FROM reached
         $$;
         ALTER FUNCTION authz.templates_reached(TEXT) OWNER TO authz_owner;
         {after}"
    )
}

const STEP_DOWN: &str =
    "SELECT ts.child_id FROM public.template_steps ts JOIN reached ON ts.parent_id = reached.id";
const STEP_UP: &str =
    "SELECT ts.parent_id FROM public.template_steps ts JOIN reached ON ts.child_id = reached.id";
const COMPLETED_INTO: &str = "ts.completed AND ts.kind = 'into'";
const TEMPLATE_GRANT_ARM: &str = "template_grants.template_id WHERE user_id = app.user_id";

// The edges carry the organization owning them, alice blocking `org_b`.
const TEMPLATE_DAG: Rows = &[
    ("template_grants", &[("template_id", "root"), ("user_id", "alice")]),
    ("template_grants", &[("template_id", "mid_a"), ("user_id", "bob")]),
    ("organization_blocks", &[("organization_id", "org_b"), ("user_id", "alice")]),
    (
        "template_steps",
        &[
            ("parent_id", "root"),
            ("child_id", "mid_a"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_a"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "root"),
            ("child_id", "mid_b"),
            ("kind", "into"),
            ("completed", "false"),
            ("organization_id", "org_b"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "mid_a"),
            ("child_id", "leaf"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_b"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "mid_b"),
            ("child_id", "leaf"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_a"),
        ],
    ),
    (
        "template_steps",
        &[("parent_id", "mid_a"), ("child_id", "side"), ("kind", "out"), ("completed", "true")],
    ),
];

const CYCLIC_TEMPLATE_DAG: Rows = &[
    ("template_grants", &[("template_id", "root"), ("user_id", "alice")]),
    ("template_grants", &[("template_id", "mid_a"), ("user_id", "bob")]),
    ("organization_blocks", &[("organization_id", "org_b"), ("user_id", "alice")]),
    (
        "template_steps",
        &[
            ("parent_id", "root"),
            ("child_id", "mid_a"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_a"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "root"),
            ("child_id", "mid_b"),
            ("kind", "into"),
            ("completed", "false"),
            ("organization_id", "org_b"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "mid_a"),
            ("child_id", "leaf"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_b"),
        ],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "mid_b"),
            ("child_id", "leaf"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_a"),
        ],
    ),
    (
        "template_steps",
        &[("parent_id", "mid_a"), ("child_id", "side"), ("kind", "out"), ("completed", "true")],
    ),
    (
        "template_steps",
        &[
            ("parent_id", "leaf"),
            ("child_id", "root"),
            ("kind", "into"),
            ("completed", "true"),
            ("organization_id", "org_a"),
        ],
    ),
];

/// `(rows, caller, wanted, returned)` as PostgreSQL 18.6 answers
/// `authz.templates_reached(wanted)`.
type TemplatesMeasured = &'static [(Rows, &'static str, &'static str, &'static [&'static str])];

/// `authz.templates_reached(wanted)`, read with the conditions and the
/// exclusions its walk sets on its edges.
fn templates_reached<'db>(
    database: &'db ParserDB,
    deployment: &'db Deployment,
    wanted: &str,
) -> Result<HelperSet<'db, ParserDB>, HelperSetError> {
    function(database, "authz", "templates_reached").helper_set(
        &[text(wanted)],
        deployment,
        database,
        &[ArmCondition::EdgeCondition, ArmCondition::Exclusion],
    )
}

/// Checks `authz.templates_reached`, written with the walk `term`, against
/// what PostgreSQL returns for each case of `measured`.
fn assert_templates_measured(term: &str, measured: TemplatesMeasured) {
    let database = postgres(&templates(term, ""));
    let deployment = deployment();
    for &(fixture, caller, wanted, expected) in measured {
        let set = templates_reached(&database, &deployment, wanted).expect("answered");
        let returned: Vec<_> = evaluate(&set, Request::of(caller), fixture).into_iter().collect();
        assert_eq!(
            returned, expected,
            "{caller} calling templates_reached('{wanted}') over {term}"
        );
    }
}

#[test]
fn a_walk_follows_an_edge_table_no_unique_key_orients() {
    let deployment = deployment();
    for (term, rendered) in [
        (STEP_DOWN, "template_steps.child_id WHERE parent_id IN itself"),
        (STEP_UP, "template_steps.parent_id WHERE child_id IN itself"),
    ] {
        let database = postgres(&templates(term, ""));
        let set = answer(&database, &deployment, "authz", "templates_reached", &[text("into")])
            .expect("answered");
        assert_eq!(render(&set), format!("{TEMPLATE_GRANT_ARM} UNION {rendered}"), "{term}");
        assert_eq!(covered(&set), Some("templates"), "{term}");
        let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
        assert_eq!(walk.tree().table_name(), "templates", "{term}");
        assert_eq!(walk.direction(), WalkDirection::Down, "{term}");
    }
    assert_templates_measured(
        STEP_DOWN,
        &[
            (TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
            (TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "side"]),
            (CYCLIC_TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
            (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
        ],
    );
    assert_templates_measured(
        STEP_UP,
        &[
            (TEMPLATE_DAG, "alice", "into", &["root"]),
            (TEMPLATE_DAG, "bob", "into", &["mid_a", "root"]),
            (CYCLIC_TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "mid_b", "root"]),
            (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "mid_b", "root"]),
        ],
    );

    let unique = "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id);";
    for index in [
        "",
        "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id) WHERE parent_id <> 'x';",
        "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (lower(folder_id));",
        "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id, parent_id);",
        "CREATE INDEX folder_parents_folder ON folder_parents (folder_id);",
        "CREATE UNIQUE INDEX folder_parents_folder ON folder_parents (folder_id);
         CREATE UNIQUE INDEX folder_parents_parent ON folder_parents (parent_id);",
    ] {
        let database = postgres(&edges(EDGE_UP, "").replace(unique, index));
        let set =
            answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
        assert_eq!(
            render(&set),
            format!("{FOLDER_GRANT_ARM} UNION folder_parents.parent_id WHERE folder_id IN itself"),
            "{index}"
        );
    }
}

#[test]
fn a_walk_follows_only_the_edges_its_conditions_keep() {
    let deployment = deployment();
    let filtered = "template_steps.child_id WHERE parent_id IN itself \
                    AND completed IN (true) AND kind IN ('into')";
    let lateral = format!(
        "SELECT step.id FROM reached CROSS JOIN LATERAL (SELECT ts.child_id \
         FROM public.template_steps ts WHERE ts.parent_id = reached.id AND {COMPLETED_INTO}) step (id)"
    );
    let terms = [
        format!("{STEP_DOWN} WHERE {COMPLETED_INTO}"),
        format!("{STEP_DOWN} AND {COMPLETED_INTO}"),
        format!("{STEP_DOWN} AND ts.completed WHERE ts.kind = 'into'"),
        "SELECT ts.child_id FROM public.template_steps ts \
         JOIN reached ON ts.completed AND reached.id = ts.parent_id WHERE ts.kind IN ('into')"
            .to_string(),
        lateral,
    ];
    for term in &terms {
        let database = postgres(&templates(term, ""));
        let set = templates_reached(&database, &deployment, "into").expect("answered");
        assert_eq!(render(&set), format!("{TEMPLATE_GRANT_ARM} UNION {filtered}"), "{term}");
        assert_eq!(covered(&set), Some("templates"), "{term}");
        assert_templates_measured(
            term,
            &[
                (TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "root"]),
                (TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a"]),
                (CYCLIC_TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "root"]),
                (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "root"]),
            ],
        );
    }

    let wanted = format!("{STEP_DOWN} AND ts.kind = wanted");
    let database = postgres(&templates(&wanted, ""));
    let set = templates_reached(&database, &deployment, "out").expect("answered");
    assert_eq!(
        render(&set),
        format!(
            "{TEMPLATE_GRANT_ARM} UNION template_steps.child_id WHERE parent_id IN itself \
             AND kind IN ('out')"
        )
    );
    assert_templates_measured(
        &wanted,
        &[
            (TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "mid_b", "root"]),
            (TEMPLATE_DAG, "alice", "out", &["root"]),
            (TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a"]),
            (TEMPLATE_DAG, "bob", "out", &["mid_a", "side"]),
            (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "mid_b", "root"]),
            (CYCLIC_TEMPLATE_DAG, "bob", "out", &["mid_a", "side"]),
        ],
    );

    let retired = format!(
        "{STEP_DOWN} WHERE ts.completed AND (ts.retired_at IS NULL OR ts.retired_at > now())"
    );
    let database = postgres(&templates(&retired, ""));
    let set = templates_reached(&database, &deployment, "into").expect("answered");
    assert_eq!(
        render(&set),
        format!(
            "{TEMPLATE_GRANT_ARM} UNION template_steps.child_id WHERE parent_id IN itself \
             AND completed IN (true) AND (retired_at IS NULL OR retired_at > now)"
        )
    );
}

#[test]
fn a_walk_whose_conditions_keep_no_edge_is_left_out() {
    let term = format!("{STEP_DOWN} WHERE wanted = 'into'");
    let database = postgres(&templates(&term, ""));
    let deployment = deployment();
    let set = templates_reached(&database, &deployment, "out").expect("answered");
    assert_eq!(render(&set), TEMPLATE_GRANT_ARM);
    assert_eq!(covered(&set), Some("templates"));
    let set = templates_reached(&database, &deployment, "into").expect("answered");
    assert_eq!(
        render(&set),
        format!("{TEMPLATE_GRANT_ARM} UNION template_steps.child_id WHERE parent_id IN itself")
    );
    assert_templates_measured(
        &term,
        &[
            (TEMPLATE_DAG, "alice", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
            (TEMPLATE_DAG, "alice", "out", &["root"]),
            (TEMPLATE_DAG, "bob", "out", &["mid_a"]),
            (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
        ],
    );
}

#[test]
fn an_edge_condition_reading_the_walk_or_reducing_to_nothing_refuses_the_answer() {
    let deployment = deployment();
    for (term, condition) in [
        (format!("{STEP_DOWN} WHERE reached.id <> 'x'"), "reached.id <> 'x'"),
        (format!("{STEP_DOWN} AND reached.id = 'x'"), "reached.id = 'x'"),
        (format!("{STEP_DOWN} WHERE id = 'x'"), "id = 'x'"),
        (format!("{STEP_DOWN} AND ts.child_id = reached.id"), "ts.child_id = reached.id"),
        (format!("{STEP_DOWN} WHERE ts.kind <> 'x'"), "ts.kind <> 'x'"),
        (
            format!("{STEP_DOWN} WHERE ts.kind = current_setting('app.user_id', true)"),
            "ts.kind = current_setting('app.user_id', true)",
        ),
        (
            "SELECT step.id FROM reached CROSS JOIN LATERAL (SELECT ts.child_id \
             FROM public.template_steps ts WHERE ts.parent_id = reached.id AND id = 'x') step (id)"
                .to_string(),
            "id = 'x'",
        ),
        (
            "SELECT step.id FROM reached CROSS JOIN LATERAL (SELECT ts.child_id \
             FROM public.template_steps ts WHERE ts.parent_id = reached.id \
             AND ts.child_id <> reached.id) step (id)"
                .to_string(),
            "ts.child_id <> reached.id",
        ),
    ] {
        let database = postgres(&templates(&term, ""));
        let refused = templates_reached(&database, &deployment, "into");
        assert!(
            matches!(
                &refused,
                Err(HelperSetError::Irreducible { function, condition: refusing })
                    if function == "authz.templates_reached" && refusing == condition
            ),
            "{term}: {refused:?}"
        );
    }

    let lateral = "SELECT step.id FROM reached CROSS JOIN LATERAL (SELECT ts.child_id \
                   FROM public.template_steps ts WHERE ts.parent_id = reached.id) step (id) \
                   WHERE step.id <> 'x'";
    let database = postgres(&templates(lateral, ""));
    assert!(matches!(
        templates_reached(&database, &deployment, "into"),
        Err(HelperSetError::ArmShape { function, .. }) if function == "authz.templates_reached"
    ));
}

#[test]
fn an_edge_condition_is_read_only_where_the_consumer_names_it() {
    let deployment = deployment();
    for (term, first) in [
        (format!("{STEP_DOWN} WHERE {COMPLETED_INTO}"), "ts.completed"),
        (format!("{STEP_DOWN} WHERE wanted = 'into'"), "wanted = 'into'"),
    ] {
        let database = postgres(&templates(&term, ""));
        let unread = answer(&database, &deployment, "authz", "templates_reached", &[text("out")]);
        assert!(
            matches!(
                &unread,
                Err(HelperSetError::UnreadCondition {
                    function,
                    condition,
                    kind: ArmCondition::EdgeCondition,
                }) if function == "authz.templates_reached" && condition == first
            ),
            "{term}: {unread:?}"
        );
    }
    let database = postgres(&templates(STEP_DOWN, ""));
    answer(&database, &deployment, "authz", "templates_reached", &[text("into")])
        .expect("a walk setting no condition on its edges needs none read");
}

#[test]
fn a_walk_follows_only_the_edges_outside_what_another_helper_excludes() {
    let deployment = deployment();
    let excluded = "organization_id NOT IN authz.blocking_organizations() \
                    {organization_blocks.organization_id WHERE user_id = app.user_id}";
    let measured: [(String, String, TemplatesMeasured); 2] = [
        (
            format!(
                "{STEP_DOWN} WHERE NOT (ts.organization_id = ANY \
                 (ARRAY(SELECT authz.blocking_organizations())))"
            ),
            format!("template_steps.child_id WHERE parent_id IN itself AND {excluded}"),
            &[
                (TEMPLATE_DAG, "alice", "into", &["mid_a", "root"]),
                (TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "side"]),
                (CYCLIC_TEMPLATE_DAG, "alice", "into", &["mid_a", "root"]),
                (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "mid_b", "root", "side"]),
            ],
        ),
        (
            format!(
                "{STEP_DOWN} WHERE ts.completed \
                 AND ts.organization_id NOT IN (SELECT authz.blocking_organizations())"
            ),
            format!(
                "template_steps.child_id WHERE parent_id IN itself AND completed IN (true) \
                 AND {excluded}"
            ),
            &[
                (TEMPLATE_DAG, "alice", "into", &["mid_a", "root"]),
                (TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "side"]),
                (CYCLIC_TEMPLATE_DAG, "alice", "into", &["mid_a", "root"]),
                (CYCLIC_TEMPLATE_DAG, "bob", "into", &["leaf", "mid_a", "root", "side"]),
            ],
        ),
    ];
    for (term, rendered, measured) in measured {
        let database = postgres(&templates(&term, ""));
        let set = templates_reached(&database, &deployment, "into").expect("answered");
        assert_eq!(render(&set), format!("{TEMPLATE_GRANT_ARM} UNION {rendered}"), "{term}");
        assert_templates_measured(&term, measured);

        let unread = function(&database, "authz", "templates_reached").helper_set(
            &[text("into")],
            &deployment,
            &database,
            &[ArmCondition::EdgeCondition],
        );
        assert!(
            matches!(
                &unread,
                Err(HelperSetError::UnreadCondition { kind: ArmCondition::Exclusion, .. })
            ),
            "{term}: {unread:?}"
        );
    }
}

#[test]
fn an_edge_exclusion_reaching_the_walking_helper_is_a_call_cycle() {
    let term =
        format!("{STEP_DOWN} WHERE ts.child_id NOT IN (SELECT authz.templates_reached(wanted))");
    let database = postgres(&templates(&term, ""));
    let deployment = deployment();
    assert!(matches!(
        templates_reached(&database, &deployment, "into"),
        Err(HelperSetError::CallCycle { function }) if function == "authz.templates_reached"
    ));
}

#[test]
fn an_edge_exclusion_of_a_set_holding_nothing_keeps_every_edge() {
    let term = format!(
        "{STEP_DOWN} WHERE ts.organization_id NOT IN (SELECT authz.blocking_organizations())"
    );
    let condition = "WHERE ob.user_id = current_setting('app.user_id', true)";
    let fixture = templates(&term, "");
    let empty = fixture.replace(condition, &format!("{condition} AND false"));
    assert_ne!(empty, fixture, "the excluded helper keeps no row");
    let database = postgres(&empty);
    let deployment = deployment();
    let set = templates_reached(&database, &deployment, "into").expect("answered");
    assert_eq!(
        render(&set),
        format!("{TEMPLATE_GRANT_ARM} UNION template_steps.child_id WHERE parent_id IN itself")
    );
}

#[test]
fn an_edge_condition_needs_the_owner_to_read_the_columns_it_names() {
    let deployment = deployment();
    let term = format!("{STEP_DOWN} WHERE {COMPLETED_INTO}");
    let columns = |granted: &str| {
        templates(&term, &format!("GRANT SELECT ({granted}) ON template_steps TO authz_owner;"))
            .replace(
                "GRANT SELECT ON templates, template_grants, template_steps",
                "GRANT SELECT ON templates, template_grants",
            )
    };
    let database = postgres(&columns("parent_id, child_id, completed, kind"));
    templates_reached(&database, &deployment, "into").expect("answered");
    let database = postgres(&columns("parent_id, child_id, completed"));
    assert!(matches!(
        templates_reached(&database, &deployment, "into"),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "template_steps"
    ));
}

#[test]
fn an_arm_returning_a_unique_column_keyed_by_another_table_is_a_parent_arm() {
    let database = postgres(&edges(EDGE_DOWN, ""));
    let deployment = deployment();
    let set = answer(&database, &deployment, "authz", "projects_reached", &[]).expect("answered");
    assert_eq!(
        render(&set),
        format!(
            "project_folders.project_id WHERE folder_id -> folders IN authz.folders_reached() \
             {{{FOLDER_GRANT_ARM} UNION folder_parents.folder_id WHERE parent_id IN itself}}"
        )
    );
    assert_eq!(covered(&set), Some("projects"));
    let HelperArm::Parent(parent) = &set.arms()[0] else { panic!("a parent arm") };
    assert_eq!(parent.table().table_name(), "project_folders");
    assert_eq!(parent.child_table().table_name(), "projects");
    assert_eq!(parent.parent_table().table_name(), "folders");
    for (fixture, caller, _, _, projects) in EDGES_MEASURED {
        let returned: Vec<_> = evaluate(&set, Request::of(caller), fixture).into_iter().collect();
        assert_eq!(returned, *projects, "{caller} calling projects_reached()");
    }

    let set = answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
    let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the walk") };
    assert_eq!(walk.table().table_name(), "folder_parents");

    let computed = edges(EDGE_DOWN, "").replace(
        "WHERE pf.folder_id IN (SELECT authz.folders_reached())",
        "WHERE lower(pf.folder_id) IN (SELECT authz.folders_reached())",
    );
    let database = postgres(&computed);
    assert!(matches!(
        answer(&database, &deployment, "authz", "projects_reached", &[]),
        Err(HelperSetError::ArmShape { .. })
    ));
}

#[test]
fn an_arm_returning_a_column_no_unique_key_covers_alone_stays_a_grant_arm() {
    let deployment = deployment();
    let unique = "CREATE UNIQUE INDEX project_folders_project ON project_folders (project_id);";
    let folder = "folder_id TEXT NOT NULL REFERENCES folders (id),\n           PRIMARY KEY (organization_id, project_id)";
    for (from, to) in [
        (unique, ""),
        (
            unique,
            "CREATE UNIQUE INDEX project_folders_project ON project_folders (project_id) WHERE folder_id <> 'x';",
        ),
        (
            unique,
            "CREATE UNIQUE INDEX project_folders_project ON project_folders (project_id, folder_id);",
        ),
        (folder, "folder_id TEXT NOT NULL,\n           PRIMARY KEY (organization_id, project_id)"),
    ] {
        let sql = edges(EDGE_DOWN, "").replace(from, to);
        assert_ne!(sql, edges(EDGE_DOWN, ""), "{from} is replaced");
        let database = postgres(&sql);
        let set =
            answer(&database, &deployment, "authz", "projects_reached", &[]).expect("answered");
        assert!(
            matches!(&set.arms()[0], HelperArm::Grant(grant) if matches!(grant.granted_to(), GrantedTo::Set(_))),
            "{to}"
        );
    }
}

#[test]
fn an_edge_column_reaches_the_tree_through_any_foreign_key_naming_its_key() {
    let deployment = deployment();
    let parent = "parent_id TEXT NOT NULL REFERENCES folders (id),";
    let child = "folder_id TEXT NOT NULL REFERENCES folders (id),";
    for (from, to) in [
        (parent, "parent_id TEXT NOT NULL,"),
        (child, "folder_id TEXT NOT NULL,"),
        (parent, "parent_id TEXT NOT NULL REFERENCES folders (id) REFERENCES folders (id),"),
    ] {
        let sql = edges(EDGE_DOWN, "").replace(from, to);
        let database = postgres(&sql);
        let set =
            answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
        assert_eq!(
            render(&set),
            format!("{FOLDER_GRANT_ARM} UNION folder_parents.folder_id WHERE parent_id IN itself"),
            "{to}"
        );
        let HelperArm::Walk(walk) = &set.arms()[1] else { panic!("the second arm is the walk") };
        assert_eq!(walk.tree().table_name(), "folders", "{to}");
        let referenced = walk.foreign_key().referenced_table(&database).expect("resolves");
        assert_eq!(referenced.table_name(), "folders", "{to}");
    }
}

/// [`edges`] with `resources` beside the folders and a tenant key holding each
/// of `keyed` at the position of `resources.id`, its one-column primary key.
fn tenant_keyed(term: &str, keyed: &[&str]) -> String {
    use core::fmt::Write as _;
    let keys = keyed.iter().fold(String::new(), |mut keys, column| {
        write!(
            keys,
            "ALTER TABLE folder_parents ADD FOREIGN KEY (organization_id, {column}) \
             REFERENCES resources (organization_id, id);"
        )
        .expect("writing to a String succeeds");
        keys
    });
    edges(
        term,
        &format!(
            "CREATE TABLE resources (
               id TEXT PRIMARY KEY,
               organization_id TEXT NOT NULL,
               UNIQUE (organization_id, id)
             );
             {keys}"
        ),
    )
}

#[test]
fn a_walk_is_over_the_one_tree_both_edge_columns_reference() {
    let deployment = deployment();
    for (term, rendered, direction) in [
        (EDGE_DOWN, "folder_parents.folder_id WHERE parent_id IN itself", WalkDirection::Down),
        (EDGE_UP, "folder_parents.parent_id WHERE folder_id IN itself", WalkDirection::Up),
    ] {
        for keyed in ["parent_id", "folder_id"] {
            let database = postgres(&tenant_keyed(term, &[keyed]));
            let set =
                answer(&database, &deployment, "authz", "folders_reached", &[]).expect("answered");
            assert_eq!(render(&set), format!("{FOLDER_GRANT_ARM} UNION {rendered}"), "{keyed}");
            assert_eq!(covered(&set), Some("folders"), "{keyed}");
            let HelperArm::Walk(walk) = &set.arms()[1] else {
                panic!("the second arm is the walk")
            };
            assert_eq!(walk.tree().table_name(), "folders", "{keyed}");
            let referenced = walk.foreign_key().referenced_table(&database).expect("resolves");
            assert_eq!(referenced.table_name(), "folders", "{keyed}");
            let measured =
                EDGES_MEASURED.iter().map(|&(rows, caller, down, up, _)| (rows, caller, down, up));
            assert_walked(&set, direction, term, measured);
        }

        let database = postgres(&tenant_keyed(term, &["parent_id", "folder_id"]));
        assert!(
            matches!(
                answer(&database, &deployment, "authz", "folders_reached", &[]),
                Err(HelperSetError::ArmShape { function, .. }) if function == "authz.folders_reached"
            ),
            "{term} with both edge columns naming folders and resources"
        );
    }
}

/// Resources of every kind kept in `resources`, folders and projects filed
/// under a folder through `resource_parents` and ownables filed in a project
/// through `ownables`. Each edge column references `resources` only through a
/// composite foreign key pinning the kind it names. `authz.reached()` walks
/// from the resources granted to the caller along `term`.
fn hierarchy(term: &str) -> String {
    format!(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE TABLE organizations (id TEXT PRIMARY KEY);
         CREATE TABLE resources (
           id TEXT PRIMARY KEY,
           kind TEXT NOT NULL,
           organization_id TEXT NOT NULL REFERENCES organizations (id),
           UNIQUE (id, kind)
         );
         CREATE TABLE resource_parents (
           id TEXT PRIMARY KEY,
           kind TEXT NOT NULL CHECK (kind IN ('folder', 'project')),
           parent_id TEXT NOT NULL,
           parent_kind TEXT GENERATED ALWAYS AS ('folder') STORED NOT NULL,
           FOREIGN KEY (id, kind) REFERENCES resources (id, kind),
           FOREIGN KEY (parent_id, parent_kind) REFERENCES resources (id, kind)
         );
         CREATE TABLE ownables (
           id TEXT PRIMARY KEY,
           kind TEXT GENERATED ALWAYS AS ('ownable') STORED NOT NULL,
           project_id TEXT NOT NULL,
           project_kind TEXT GENERATED ALWAYS AS ('project') STORED NOT NULL,
           FOREIGN KEY (id, kind) REFERENCES resources (id, kind),
           FOREIGN KEY (project_id, project_kind) REFERENCES resources (id, kind)
         );
         CREATE TABLE resource_grants (
           resource_id TEXT NOT NULL REFERENCES resources (id),
           user_id TEXT NOT NULL,
           PRIMARY KEY (resource_id, user_id)
         );
         GRANT SELECT ON resources, resource_parents, ownables, resource_grants TO authz_owner;
         CREATE FUNCTION authz.reached() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           WITH RECURSIVE reached (id) AS (
             SELECT g.resource_id FROM public.resource_grants g
             WHERE g.user_id = current_setting('app.user_id', true)
             UNION {term}
           ) SELECT id FROM reached
         $$;
         ALTER FUNCTION authz.reached() OWNER TO authz_owner;"
    )
}

const LATERAL_DOWN: &str = "SELECT child.id FROM reached CROSS JOIN LATERAL (
       SELECT p.id FROM public.resource_parents p WHERE p.parent_id = reached.id
       UNION ALL
       SELECT o.id FROM public.ownables o WHERE o.project_id = reached.id
     ) child";
const LATERAL_UP: &str = "SELECT parent.id FROM reached, LATERAL (
       SELECT p.parent_id FROM public.resource_parents p WHERE p.id = reached.id
       UNION
       SELECT o.project_id FROM public.ownables o WHERE reached.id = o.id
     ) parent (id)";

/// `f_team` under `f_root`, the project `p1` under `f_team`, the project
/// `p_other` filed under no folder, and the ownables `o1` in `p1` and `o2` in
/// `p_other`.
const HIERARCHY: Rows = &[
    ("resource_parents", &[("id", "f_team"), ("parent_id", "f_root")]),
    ("resource_parents", &[("id", "p1"), ("parent_id", "f_team")]),
    ("ownables", &[("id", "o1"), ("project_id", "p1")]),
    ("ownables", &[("id", "o2"), ("project_id", "p_other")]),
    ("resource_grants", &[("resource_id", "f_team"), ("user_id", "alice")]),
    ("resource_grants", &[("resource_id", "f_root"), ("user_id", "bob")]),
    ("resource_grants", &[("resource_id", "p_other"), ("user_id", "carol")]),
    ("resource_grants", &[("resource_id", "o1"), ("user_id", "dave")]),
];

/// [`HIERARCHY`] with `f_root` moved under `f_team`, closing a cycle.
const CYCLIC_HIERARCHY: Rows = &[
    ("resource_parents", &[("id", "f_team"), ("parent_id", "f_root")]),
    ("resource_parents", &[("id", "p1"), ("parent_id", "f_team")]),
    ("resource_parents", &[("id", "f_root"), ("parent_id", "f_team")]),
    ("ownables", &[("id", "o1"), ("project_id", "p1")]),
    ("ownables", &[("id", "o2"), ("project_id", "p_other")]),
    ("resource_grants", &[("resource_id", "f_team"), ("user_id", "alice")]),
    ("resource_grants", &[("resource_id", "f_root"), ("user_id", "bob")]),
    ("resource_grants", &[("resource_id", "p_other"), ("user_id", "carol")]),
    ("resource_grants", &[("resource_id", "o1"), ("user_id", "dave")]),
];

/// What PostgreSQL 18.6 returns for `(rows, caller)` from `authz.reached()`
/// written with [`LATERAL_DOWN`], then with [`LATERAL_UP`].
type HierarchyMeasured = (Rows, &'static str, &'static [&'static str], &'static [&'static str]);

const HIERARCHY_MEASURED: &[HierarchyMeasured] = &[
    (HIERARCHY, "alice", &["f_team", "o1", "p1"], &["f_root", "f_team"]),
    (HIERARCHY, "bob", &["f_root", "f_team", "o1", "p1"], &["f_root"]),
    (HIERARCHY, "carol", &["o2", "p_other"], &["p_other"]),
    (HIERARCHY, "dave", &["o1"], &["f_root", "f_team", "o1", "p1"]),
    (HIERARCHY, "zed", &[], &[]),
    (CYCLIC_HIERARCHY, "alice", &["f_root", "f_team", "o1", "p1"], &["f_root", "f_team"]),
    (CYCLIC_HIERARCHY, "bob", &["f_root", "f_team", "o1", "p1"], &["f_root", "f_team"]),
    (CYCLIC_HIERARCHY, "dave", &["o1"], &["f_root", "f_team", "o1", "p1"]),
];

const RESOURCE_GRANT_ARM: &str = "resource_grants.resource_id WHERE user_id = app.user_id";

#[test]
fn a_lateral_step_walks_every_edge_its_union_reads() {
    let deployment = deployment();
    for (term, rendered, direction) in [
        (
            LATERAL_DOWN,
            "resource_parents.id WHERE parent_id IN itself UNION ownables.id WHERE project_id IN itself",
            WalkDirection::Down,
        ),
        (
            LATERAL_UP,
            "resource_parents.parent_id WHERE id IN itself UNION ownables.project_id WHERE id IN itself",
            WalkDirection::Up,
        ),
    ] {
        let database = postgres(&hierarchy(term));
        let set = answer(&database, &deployment, "authz", "reached", &[]).expect("answered");
        assert_eq!(render(&set), format!("{RESOURCE_GRANT_ARM} UNION {rendered}"), "{term}");
        assert_eq!(covered(&set), Some("resources"), "{term}");
        for arm in &set.arms()[1..] {
            let HelperArm::Walk(walk) = arm else { panic!("every arm after the first walks") };
            assert_eq!(walk.direction(), direction, "{term}");
            assert_eq!(walk.tree().table_name(), "resources", "{term}");
            assert_eq!(render_columns(walk.key()), "id", "{term}");
            assert_eq!(render_columns(walk.child()), "id", "{term}");
        }
        assert_walked(&set, direction, term, HIERARCHY_MEASURED.iter().copied());
    }
}

/// Checks `set`, written with `term` walking in `direction`, against what
/// PostgreSQL returns for each `(rows, caller, walked down, walked up)`.
fn assert_walked(
    set: &HelperSet<'_, ParserDB>,
    direction: WalkDirection,
    term: &str,
    measured: impl IntoIterator<Item = HierarchyMeasured>,
) {
    for (fixture, caller, down, up) in measured {
        let expected = match direction {
            WalkDirection::Down => down,
            WalkDirection::Up => up,
        };
        let returned: Vec<_> = evaluate(set, Request::of(caller), fixture).into_iter().collect();
        assert_eq!(returned, expected, "{caller} walking {term}");
    }
}

#[test]
fn a_lateral_step_over_one_edge_walks_as_the_join_does() {
    let deployment = deployment();
    for term in [
        "SELECT child.id FROM reached CROSS JOIN LATERAL \
         (SELECT g.id FROM public.groups g WHERE g.parent_id = reached.id) child",
        "SELECT c.node FROM reached r, LATERAL \
         (SELECT id FROM public.groups WHERE parent_id = r.id) AS c (node)",
        "SELECT node FROM reached CROSS JOIN LATERAL \
         (SELECT g.id AS node FROM public.groups g WHERE reached.id = g.parent_id) child",
    ] {
        let database = postgres(&groups(&walking(term), ""));
        let set = groups_with(&database, &deployment, "read").expect("answered");
        assert_eq!(
            render(&set),
            "group_grants.group_id WHERE principal_id = app.user_id \
             UNION groups.id WHERE parent_id IN itself",
            "{term}"
        );
        assert_walk_measured(term, TREE_MEASURED);
    }
}

#[test]
fn a_lateral_step_of_any_other_shape_refuses_the_answer() {
    let deployment = deployment();
    let edge = "SELECT g.id FROM public.groups g WHERE g.parent_id = reached.id";
    for term in [
        format!("SELECT child.id FROM reached CROSS JOIN ({edge}) child"),
        format!("SELECT child.id FROM LATERAL ({edge}) child CROSS JOIN reached"),
        format!("SELECT reached.id FROM reached CROSS JOIN LATERAL ({edge}) child"),
        format!("SELECT id FROM reached CROSS JOIN LATERAL ({edge}) child"),
        format!(
            "SELECT child.id FROM reached CROSS JOIN LATERAL ({edge}) child WHERE child.id <> 'x'"
        ),
        format!("SELECT child.id FROM reached LEFT JOIN LATERAL ({edge}) child ON TRUE"),
        format!("SELECT reached.id FROM reached CROSS JOIN LATERAL ({edge}) reached"),
        format!("SELECT child.a FROM reached CROSS JOIN LATERAL ({edge}) child (a, b)"),
        format!("SELECT child.id FROM reached CROSS JOIN LATERAL ({edge} LIMIT 1) child"),
        format!(
            "SELECT child.id FROM reached CROSS JOIN LATERAL \
             ({edge} EXCEPT SELECT g.id FROM public.groups g WHERE g.parent_id = reached.id) child"
        ),
        "SELECT child.id FROM reached CROSS JOIN LATERAL \
         (SELECT reached.id FROM public.groups reached WHERE reached.parent_id = reached.id) child"
            .to_string(),
        "SELECT child.id FROM reached CROSS JOIN LATERAL \
         (SELECT g.id FROM public.groups g WHERE g.parent_id = id) child"
            .to_string(),
        "SELECT child.parent_id FROM reached CROSS JOIN LATERAL \
         (SELECT g.parent_id FROM public.groups g WHERE g.parent_id = reached.id) child"
            .to_string(),
        "SELECT child.id FROM reached CROSS JOIN LATERAL \
         (SELECT g.id FROM public.groups g JOIN public.group_grants gg ON gg.group_id = g.id \
          WHERE g.parent_id = reached.id) child"
            .to_string(),
    ] {
        let database = postgres(&groups(&walking(&term), ""));
        assert!(
            matches!(
                groups_with(&database, &deployment, "read"),
                Err(HelperSetError::ArmShape { function, .. }) if function == "authz.groups_with"
            ),
            "{term}"
        );
    }
}

#[test]
fn an_edge_not_naming_the_tree_key_through_a_checked_foreign_key_refuses_the_walk() {
    let deployment = deployment();
    let checked = "project_kind TEXT GENERATED ALWAYS AS ('project') STORED NOT NULL,";
    let unchecked = "project_kind TEXT CHECK (project_kind = 'project'),";
    let shape = |sql: &str| {
        let database = postgres(sql);
        matches!(
            answer(&database, &deployment, "authz", "reached", &[]),
            Err(HelperSetError::ArmShape { function, .. }) if function == "authz.reached"
        )
    };
    let organization_children = "SELECT child.id FROM reached CROSS JOIN LATERAL (
       SELECT r.id FROM public.resources r WHERE r.organization_id = reached.id
     ) child";
    assert!(shape(&hierarchy(organization_children)), "a child of another tree");
    assert!(
        shape(&hierarchy(
            &organization_children.replace("= reached.id", "= reached.id AND r.id <> reached.id")
        )),
        "a child of another tree but the reached row"
    );
    assert!(
        shape(&hierarchy(LATERAL_DOWN).replace(checked, unchecked)),
        "a foreign key checking nothing where its other column is NULL"
    );
    let full = hierarchy(LATERAL_DOWN).replace(checked, unchecked).replace(
        "FOREIGN KEY (project_id, project_kind) REFERENCES resources (id, kind)",
        "FOREIGN KEY (project_id, project_kind) REFERENCES resources (id, kind) MATCH FULL",
    );
    let database = postgres(&full);
    let set = answer(&database, &deployment, "authz", "reached", &[]).expect("answered");
    assert_eq!(set.arms().len(), 3, "MATCH FULL refuses a key holding NULL in part");
}

#[test]
fn a_lateral_step_needs_the_owner_to_read_every_edge_it_walks() {
    let deployment = deployment();
    let sql = hierarchy(LATERAL_DOWN).replace(
        "GRANT SELECT ON resources, resource_parents, ownables, resource_grants",
        "GRANT SELECT (id) ON ownables TO authz_owner;
         GRANT SELECT ON resources, resource_parents, resource_grants",
    );
    let database = postgres(&sql);
    assert!(matches!(
        answer(&database, &deployment, "authz", "reached", &[]),
        Err(HelperSetError::MissingSelect { table, .. }) if table == "ownables"
    ));
}

/// connetto's team membership, which keeps a team member only while their
/// organization membership is active, and a tree of objects owned by users.
const JOINED: &str = "
CREATE ROLE owner NOLOGIN BYPASSRLS;
CREATE TABLE memberships (
  organization_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  state TEXT NOT NULL,
  expires_at TIMESTAMPTZ,
  PRIMARY KEY (organization_id, user_id)
);
CREATE TABLE team_members (
  team_id TEXT NOT NULL,
  organization_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  team_role TEXT NOT NULL,
  PRIMARY KEY (team_id, user_id),
  FOREIGN KEY (organization_id, user_id) REFERENCES memberships (organization_id, user_id)
);
CREATE TABLE objects (
  id TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES objects (id),
  owner_id TEXT NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT false
);
CREATE FUNCTION member_teams(min_role TEXT) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT member.team_id FROM public.team_members member
  JOIN public.memberships membership
    ON membership.organization_id = member.organization_id AND membership.user_id = member.user_id
  WHERE member.user_id = current_setting('app.user_id', true)
    AND (member.team_role = 'admin' OR min_role = 'member')
    AND membership.state = 'active'
$$;
CREATE FUNCTION child_objects() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT object.id FROM public.objects object
  JOIN public.objects parent ON parent.id = object.parent_id
  WHERE object.owner_id = NULLIF(current_setting('app.user_id', true), '') AND parent.archived = false
$$;
ALTER FUNCTION member_teams(TEXT) OWNER TO owner;
ALTER FUNCTION child_objects() OWNER TO owner;
GRANT SELECT ON memberships, team_members, objects TO owner;
";

/// Alice is an active member of `o1` and a suspended one of `o2`, Bob a
/// suspended member of `o1`. `c2` sits under the archived `r2`.
const JOINED_ROWS: Rows = &[
    ("memberships", &[("organization_id", "o1"), ("user_id", "alice"), ("state", "active")]),
    ("memberships", &[("organization_id", "o1"), ("user_id", "bob"), ("state", "suspended")]),
    ("memberships", &[("organization_id", "o1"), ("user_id", "carol"), ("state", "active")]),
    ("memberships", &[("organization_id", "o2"), ("user_id", "alice"), ("state", "suspended")]),
    (
        "team_members",
        &[
            ("team_id", "T"),
            ("organization_id", "o1"),
            ("user_id", "alice"),
            ("team_role", "admin"),
        ],
    ),
    (
        "team_members",
        &[("team_id", "T"), ("organization_id", "o1"), ("user_id", "bob"), ("team_role", "admin")],
    ),
    (
        "team_members",
        &[
            ("team_id", "U"),
            ("organization_id", "o1"),
            ("user_id", "carol"),
            ("team_role", "member"),
        ],
    ),
    (
        "team_members",
        &[
            ("team_id", "V"),
            ("organization_id", "o2"),
            ("user_id", "alice"),
            ("team_role", "admin"),
        ],
    ),
    (
        "team_members",
        &[
            ("team_id", "W"),
            ("organization_id", "o1"),
            ("user_id", "alice"),
            ("team_role", "member"),
        ],
    ),
    ("objects", &[("id", "r1"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "c1"), ("parent_id", "r1"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "r2"), ("owner_id", "bob"), ("archived", "true")]),
    ("objects", &[("id", "c2"), ("parent_id", "r2"), ("owner_id", "alice"), ("archived", "false")]),
    ("objects", &[("id", "c3"), ("parent_id", "r1"), ("owner_id", "bob"), ("archived", "false")]),
];

/// What PostgreSQL 17.11 returns for `(caller, helper, argument)`, the
/// helpers of [`JOINED`] called over [`JOINED_ROWS`].
const JOINED_MEASURED: &[(&str, &str, Option<&str>, &[&str])] = &[
    ("alice", "member_teams", Some("admin"), &["T"]),
    ("alice", "member_teams", Some("member"), &["T", "W"]),
    ("alice", "child_objects", None, &["c1"]),
    ("bob", "member_teams", Some("admin"), &[]),
    ("bob", "member_teams", Some("member"), &[]),
    ("bob", "child_objects", None, &["c3"]),
    ("carol", "member_teams", Some("admin"), &[]),
    ("carol", "member_teams", Some("member"), &["U"]),
    ("carol", "child_objects", None, &[]),
    ("", "member_teams", Some("admin"), &[]),
    ("", "member_teams", Some("member"), &[]),
    ("", "child_objects", None, &[]),
];

#[test]
fn a_joined_row_keeps_the_rows_whose_foreign_key_names_a_row_meeting_its_conditions() {
    let database = postgres(JOINED);
    let deployment = deployment();
    let set = answer(&database, &deployment, "public", "member_teams", &[text("admin")])
        .expect("answered");
    assert_eq!(
        render(&set),
        "team_members.team_id WHERE user_id = app.user_id AND team_role IN ('admin') \
         AND (organization_id, user_id) -> memberships {state IN ('active')}"
    );
    let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
    let [joined] = grant.joined_rows() else { panic!("one joined row") };
    let referenced: Vec<&str> =
        joined.columns().iter().map(|(_, referenced)| referenced.column_name()).collect();
    assert_eq!(referenced, ["organization_id", "user_id"]);
    assert_eq!(
        joined.foreign_key().referenced_table(&database).expect("resolves").table_name(),
        "memberships"
    );

    let set = answer(&database, &deployment, "public", "child_objects", &[]).expect("answered");
    assert_eq!(
        render(&set),
        "objects.id WHERE owner_id = app.user_id AND (parent_id) -> objects {archived IN (false)}"
    );

    for &(caller, helper, argument, expected) in JOINED_MEASURED {
        let arguments: Vec<Value> = argument.into_iter().map(text).collect();
        let set = answer(&database, &deployment, "public", helper, &arguments).expect("answered");
        let returned: Vec<_> =
            evaluate(&set, Request::of(caller), JOINED_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{caller} calling {helper}({argument:?})");
    }
}

/// `member_teams('admin')` with `body` as its body, rendered.
fn joined(body: &str) -> Result<String, HelperSetError> {
    let sql = JOINED.replace(
        "SELECT member.team_id FROM public.team_members member
  JOIN public.memberships membership
    ON membership.organization_id = member.organization_id AND membership.user_id = member.user_id
  WHERE member.user_id = current_setting('app.user_id', true)
    AND (member.team_role = 'admin' OR min_role = 'member')
    AND membership.state = 'active'",
        body,
    );
    assert_ne!(sql, JOINED, "the body is replaced");
    let database = postgres(&sql);
    let deployment = deployment();
    answer(&database, &deployment, "public", "member_teams", &[text("admin")])
        .map(|set| render(&set))
}

const MEMBER_TEAMS: &str = "team_members.team_id WHERE user_id = app.user_id \
     AND (organization_id, user_id) -> memberships {state IN ('active')}";

#[test]
fn a_joined_row_is_read_from_on_and_where_in_every_spelling_of_the_join() {
    for body in [
        "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
         ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
         WHERE m.user_id = current_setting('app.user_id', true) AND o.state = 'active'",
        "SELECT m.team_id FROM public.team_members m INNER JOIN public.memberships o \
         ON m.user_id = o.user_id AND (m.organization_id = o.organization_id AND o.state = 'active') \
         WHERE m.user_id = current_setting('app.user_id', true)",
        "SELECT team_members.team_id FROM public.team_members JOIN public.memberships \
         ON memberships.organization_id = team_members.organization_id \
         WHERE memberships.user_id = team_members.user_id AND state = 'active' \
         AND team_members.user_id = current_setting('app.user_id', true)",
        "SELECT team_id FROM public.team_members m JOIN public.memberships o \
         ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
         AND m.user_id = current_setting('app.user_id', true) \
         WHERE state IN ('active', 'invited') AND o.state = min_role OR o.state = 'active'",
        "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
         ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
         AND m.user_id = o.user_id \
         WHERE m.user_id = current_setting('app.user_id', true) AND o.state = 'active'",
    ] {
        assert_eq!(joined(body).as_deref(), Ok(MEMBER_TEAMS), "{body}");
    }
    assert_eq!(
        joined(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
             WHERE m.user_id = current_setting('app.user_id', true) AND o.state = 'active' \
             AND (o.expires_at IS NULL OR o.expires_at > now())"
        )
        .as_deref(),
        Ok("team_members.team_id WHERE user_id = app.user_id \
            AND (organization_id, user_id) -> memberships \
            {state IN ('active') AND (expires_at IS NULL OR expires_at > now)}")
    );
    assert_eq!(
        joined(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
             WHERE m.user_id = current_setting('app.user_id', true) \
             AND o.state = 'active' AND o.state = min_role"
        )
        .as_deref(),
        Ok(""),
        "a joined row no row meets keeps no row of the arm"
    );
    let database = postgres(&helper(
        DEFINER,
        "SELECT doc_id FROM members JOIN docs ON docs.id = members.doc_id \
         WHERE user_id = current_setting('app.user_id', true) AND parent_id = 'root'",
        "",
    ));
    let deployment = deployment();
    assert_eq!(
        answer(&database, &deployment, "public", "docs_with", &[text("read")])
            .map(|set| render(&set))
            .as_deref(),
        Ok(
            "members.doc_id WHERE user_id = app.user_id AND (doc_id) -> docs {parent_id IN ('root')}"
        ),
        "a bare name is the column of the one row holding it"
    );
}

#[test]
fn a_join_other_than_a_whole_foreign_key_to_the_row_it_names_refuses_the_answer() {
    let caller = "m.user_id = current_setting('app.user_id', true)";
    for body in [
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.user_id = m.user_id WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
             AND o.state = m.team_role WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
             AND o.state = m.user_id WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.team_id WHERE {caller}"
        ),
        format!(
            "SELECT o.organization_id FROM public.memberships o JOIN public.team_members m \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m LEFT JOIN public.memberships o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
             USING (organization_id, user_id) WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m NATURAL JOIN public.memberships o \
             WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m CROSS JOIN public.memberships o \
             WHERE o.organization_id = m.organization_id AND o.user_id = m.user_id AND {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships o ON TRUE \
             WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN (SELECT * FROM public.memberships) o \
             ON o.organization_id = m.organization_id AND o.user_id = m.user_id WHERE {caller}"
        ),
        format!(
            "SELECT m.team_id FROM public.team_members m JOIN public.memberships m \
             ON m.organization_id = m.organization_id AND m.user_id = m.user_id WHERE {caller}"
        ),
        "SELECT team_id FROM public.team_members m JOIN public.memberships o \
         ON o.organization_id = m.organization_id AND o.user_id = m.user_id \
         WHERE user_id = current_setting('app.user_id', true)"
            .to_string(),
    ] {
        assert!(matches!(joined(&body), Err(HelperSetError::ArmShape { .. })), "{body}");
    }

    let on_joined = "SELECT m.team_id FROM public.team_members m JOIN public.memberships o \
         ON o.organization_id = m.organization_id AND o.user_id = m.user_id WHERE ";
    for condition in [
        "o.user_id = current_setting('app.user_id', true)",
        "o.organization_id IN (SELECT member_teams(min_role))",
        "o.state <> m.team_role",
        "o.state = 'active' OR m.team_role = 'admin'",
        "lower(o.state) = m.team_role",
    ] {
        assert!(
            matches!(
                joined(&format!("{on_joined}{caller} AND {condition}")),
                Err(HelperSetError::Irreducible { .. })
            ),
            "{condition}"
        );
    }

    let parent = helper(
        DEFINER,
        "SELECT d.id FROM docs d JOIN docs p ON p.id = d.parent_id \
         WHERE d.parent_id IN (SELECT docs_with(min_role))",
        "",
    );
    assert!(
        matches!(refusal(&parent), HelperSetError::ArmShape { .. }),
        "a parent arm takes no joined row"
    );
}

#[test]
fn a_join_along_no_foreign_key_or_two_of_the_arm_table_refuses_the_answer() {
    let docs_arm = refusal(&helper(
        DEFINER,
        "SELECT d.id FROM docs d JOIN members m ON m.doc_id = d.id \
         WHERE m.user_id = current_setting('app.user_id', true)",
        "",
    ));
    assert!(
        matches!(docs_arm, HelperSetError::ArmShape { .. }),
        "the arm's table holds no foreign key to the row it joins"
    );

    let key =
        "FOREIGN KEY (organization_id, user_id) REFERENCES memberships (organization_id, user_id)";
    let twice = JOINED.replace(key, &format!("{key},\n  {key}"));
    assert_ne!(twice, JOINED, "the foreign key is declared twice");
    assert!(
        matches!(
            answer(&postgres(&twice), &deployment(), "public", "member_teams", &[text("admin")]),
            Err(HelperSetError::ArmShape { .. })
        ),
        "two foreign keys equally join the row"
    );
}

#[test]
fn the_owner_has_to_read_a_joined_table_past_its_policies() {
    let body = "SELECT doc_id FROM members JOIN docs d ON d.id = members.doc_id \
         WHERE user_id = current_setting('app.user_id', true) AND d.parent_id = 'root'";
    let ungranted = helper(DEFINER, body, "")
        .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON members TO owner;");
    assert!(matches!(
        refusal(&ungranted),
        HelperSetError::MissingSelect { table, .. } if table == "docs"
    ));
    let columns = helper(DEFINER, body, "GRANT SELECT (id, parent_id) ON docs TO owner;")
        .replace("GRANT SELECT ON docs, members TO owner;", "GRANT SELECT ON members TO owner;");
    let database = postgres(&columns);
    let deployment = deployment();
    answer(&database, &deployment, "public", "docs_with", &[text("read")])
        .expect("SELECT on the joined columns the arm reads is enough");
    let bound = helper(DEFINER, body, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY;")
        .replace("CREATE ROLE owner NOLOGIN BYPASSRLS;", "CREATE ROLE owner NOLOGIN;");
    assert!(matches!(
        refusal(&bound),
        HelperSetError::NotExempt { table, .. } if table == "docs"
    ));
}

/// connetto's team administration as connetto-schema `f276257` writes it,
/// an app installation acting for a team admin capped by the organization of
/// the admin's membership, which the arm reads off the joined row.
const ADMINISTERED: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
CREATE TYPE authz.resource_kind AS ENUM ('organization', 'folder', 'project', 'ownable');
CREATE TYPE authz.membership_state AS ENUM ('active', 'deactivated');
CREATE TYPE authz.team_role AS ENUM ('member', 'admin');
CREATE TABLE memberships (
  organization_id UUID NOT NULL,
  user_id UUID NOT NULL,
  state authz.membership_state NOT NULL,
  PRIMARY KEY (organization_id, user_id)
);
CREATE TABLE team_members (
  team_id UUID NOT NULL,
  organization_id UUID NOT NULL,
  user_id UUID NOT NULL,
  team_role authz.team_role NOT NULL,
  PRIMARY KEY (team_id, user_id),
  FOREIGN KEY (organization_id, user_id) REFERENCES memberships (organization_id, user_id) ON DELETE CASCADE
);
CREATE TABLE installation_permissions (
  installation_id UUID NOT NULL,
  organization_id UUID NOT NULL,
  kind authz.resource_kind NOT NULL,
  max_role authz.role NOT NULL,
  PRIMARY KEY (installation_id, kind)
);
GRANT SELECT ON memberships, team_members, installation_permissions TO authz_owner;
CREATE FUNCTION authz.caller_id() RETURNS UUID
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT NULLIF(current_setting('app.user_id', true), '')::uuid
$$;
CREATE FUNCTION authz.acting_installation_id() RETURNS UUID
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT NULLIF(current_setting('app.installation_id', true), '')::uuid
$$;
CREATE FUNCTION authz.capped_kinds(min_role authz.role)
RETURNS TABLE (organization_id UUID, kind authz.resource_kind)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT permission.organization_id, permission.kind FROM public.installation_permissions permission
  WHERE permission.installation_id = authz.acting_installation_id()
    AND permission.max_role >= min_role
$$;
CREATE FUNCTION authz.administered_teams() RETURNS SETOF UUID
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT member.team_id FROM public.team_members member
  JOIN public.memberships membership ON membership.organization_id = member.organization_id AND membership.user_id = member.user_id
  WHERE member.user_id = authz.caller_id()
    AND member.team_role = 'admin' AND membership.state = 'active'
    AND (authz.acting_installation_id() IS NULL
      OR (membership.organization_id, 'organization'::authz.resource_kind)
        IN (SELECT * FROM authz.capped_kinds('admin')))
$$;
ALTER FUNCTION authz.caller_id() OWNER TO authz_owner;
ALTER FUNCTION authz.acting_installation_id() OWNER TO authz_owner;
ALTER FUNCTION authz.capped_kinds(authz.role) OWNER TO authz_owner;
ALTER FUNCTION authz.administered_teams() OWNER TO authz_owner;
";

/// Alice is an active member of `o1` and `o2` and Bob a deactivated one of
/// `o1`. Alice administers `T1` in `o1` and `T2` in `o2` and is a plain
/// member of `T3`, and Bob administers `T4`. Installation `i1` holds `o1` as
/// an organization and `o2` only as a folder, and `i2` holds `o2` as an
/// organization below `admin`. Each name stands for one `uuid`.
const ADMINISTERED_ROWS: Rows = &[
    ("memberships", &[("organization_id", "o1"), ("user_id", "alice"), ("state", "active")]),
    ("memberships", &[("organization_id", "o2"), ("user_id", "alice"), ("state", "active")]),
    ("memberships", &[("organization_id", "o1"), ("user_id", "bob"), ("state", "deactivated")]),
    (
        "team_members",
        &[
            ("team_id", "T1"),
            ("organization_id", "o1"),
            ("user_id", "alice"),
            ("team_role", "admin"),
        ],
    ),
    (
        "team_members",
        &[
            ("team_id", "T2"),
            ("organization_id", "o2"),
            ("user_id", "alice"),
            ("team_role", "admin"),
        ],
    ),
    (
        "team_members",
        &[
            ("team_id", "T3"),
            ("organization_id", "o1"),
            ("user_id", "alice"),
            ("team_role", "member"),
        ],
    ),
    (
        "team_members",
        &[("team_id", "T4"), ("organization_id", "o1"), ("user_id", "bob"), ("team_role", "admin")],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i1"),
            ("organization_id", "o1"),
            ("kind", "organization"),
            ("max_role", "admin"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i1"),
            ("organization_id", "o2"),
            ("kind", "folder"),
            ("max_role", "admin"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i2"),
            ("organization_id", "o2"),
            ("kind", "organization"),
            ("max_role", "reader"),
        ],
    ),
    (
        "installation_permissions",
        &[
            ("installation_id", "i2"),
            ("organization_id", "o1"),
            ("kind", "project"),
            ("max_role", "admin"),
        ],
    ),
];

/// What PostgreSQL 16.15 returns for `authz.administered_teams()` over
/// [`ADMINISTERED_ROWS`] for each request.
const ADMINISTERED_MEASURED: &[(Request, &[&str])] = &[
    (ALICE, &["T1", "T2"]),
    (ALICE_EMPTY, &["T1", "T2"]),
    (ALICE_I1, &["T1"]),
    (ALICE_I2, &[]),
    (ALICE_IX, &[]),
    (Request::of("bob"), &[]),
    (BOB_I1, &[]),
];

#[test]
fn a_cap_reads_its_operands_off_the_row_the_arm_joins() {
    let database = postgres(ADMINISTERED);
    let deployment = principals(&DataType::Uuid);
    let set = answer(&database, &deployment, "authz", "administered_teams", &[]).expect("answered");
    assert_eq!(
        render(&set),
        "team_members.team_id WHERE user_id = app.user_id AND team_role IN ('admin') \
         AND (organization_id, user_id) -> memberships {state IN ('active')} \
         AND (app.installation_id IS UNSET OR (memberships.organization_id, 'organization') \
         IN authz.capped_kinds('admin') {installation_permissions.(organization_id, kind) \
         WHERE installation_id = app.installation_id AND max_role IN ('admin')})"
    );
    let [HelperArm::Grant(grant)] = set.arms() else { panic!("one grant arm") };
    let ([joined], [cap]) = (grant.joined_rows(), grant.caps()) else {
        panic!("one joined row and one cap")
    };
    let [CapOperand::Joined { row: 0, column }, CapOperand::Value(_)] = cap.operands() else {
        panic!("the organization of the joined row beside a label")
    };
    assert!(core::ptr::eq(column.table(&database), joined.table()));
    for &(request, expected) in ADMINISTERED_MEASURED {
        let returned: Vec<_> = evaluate(&set, request, ADMINISTERED_ROWS).into_iter().collect();
        assert_eq!(returned, expected, "{:?} for {:?}", request.caller, request.values);
    }
}

#[test]
fn a_bare_name_in_a_joined_cap_is_the_joined_column_before_a_parameter() {
    let administering = |condition: &str| {
        let sql = ADMINISTERED
            .replace(
                "authz.administered_teams() RETURNS",
                "authz.administered_teams(state authz.role) RETURNS",
            )
            .replace(
                "authz.administered_teams() OWNER",
                "authz.administered_teams(authz.role) OWNER",
            )
            .replace(
                "(authz.acting_installation_id() IS NULL
      OR (membership.organization_id, 'organization'::authz.resource_kind)
        IN (SELECT * FROM authz.capped_kinds('admin')))",
                condition,
            );
        let database = postgres(&sql);
        let deployment = principals(&DataType::Uuid);
        answer(&database, &deployment, "authz", "administered_teams", &[text("admin")])
            .map(|set| render(&set))
    };
    assert!(
        administering("true").is_ok_and(|rendered| !rendered.contains("IN authz.capped_kinds")),
        "the cap is replaced"
    );
    let argument = administering(
        "(authz.acting_installation_id() IS NULL OR (membership.organization_id, \
         'organization'::authz.resource_kind) IN (SELECT * FROM authz.capped_kinds(state)))",
    );
    assert!(matches!(argument, Err(HelperSetError::ArmShape { .. })), "{argument:?}");
    let lift = administering(
        "(state IS NULL OR (membership.organization_id, 'organization'::authz.resource_kind) \
         IN (SELECT * FROM authz.capped_kinds('admin')))",
    );
    assert!(matches!(lift, Err(HelperSetError::Irreducible { .. })), "{lift:?}");
}

const VERIFIED_EMAIL: &str = "app.verified_email";

/// Invitations addressed to the email a request verified, read by helpers
/// comparing with `authz.verified_email()`, which returns it lowered, with
/// `authz.raw_email()`, which returns it as sent, and with the setting
/// lowered in the arm itself.
const INVITATIONS: &str = "
CREATE ROLE app NOLOGIN;
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO app, authz_owner;
CREATE TABLE invitations (id TEXT PRIMARY KEY, email TEXT NOT NULL CHECK (email = lower(email)));
GRANT SELECT ON invitations TO authz_owner;
CREATE FUNCTION authz.verified_email() RETURNS TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT lower(NULLIF(current_setting('app.verified_email', true), ''))
$$;
CREATE FUNCTION authz.raw_email() RETURNS TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT NULLIF(current_setting('app.verified_email', true), '')
$$;
CREATE FUNCTION authz.normalized_invitations() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT invitation.id FROM public.invitations invitation WHERE invitation.email = authz.verified_email()
$$;
CREATE FUNCTION authz.raw_invitations() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT invitation.id FROM public.invitations invitation WHERE invitation.email = authz.raw_email()
$$;
CREATE FUNCTION authz.bare_invitations() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT invitation.id FROM public.invitations invitation
  WHERE invitation.email = lower(current_setting('app.verified_email', true))
$$;
ALTER FUNCTION authz.verified_email() OWNER TO authz_owner;
ALTER FUNCTION authz.raw_email() OWNER TO authz_owner;
ALTER FUNCTION authz.normalized_invitations() OWNER TO authz_owner;
ALTER FUNCTION authz.raw_invitations() OWNER TO authz_owner;
ALTER FUNCTION authz.bare_invitations() OWNER TO authz_owner;
";

/// The rows [`INVITATIONS`] was measured over.
const INVITATION_ROWS: Rows = &[
    ("invitations", &[("id", "i1"), ("email", "eve@example.com")]),
    ("invitations", &[("id", "i2"), ("email", "bob@example.com")]),
    ("invitations", &[("id", "i3"), ("email", "")]),
];

/// The rows one helper returns.
type HelperRows = &'static [&'static str];

/// What PostgreSQL 18.6 returns as `app` over [`INVITATION_ROWS`] for
/// `(request, normalized_invitations, raw_invitations, bare_invitations)`.
/// A request sending no email leaves the setting unset, and one whose
/// transaction ended reads `''`, as one sending `''` does.
const INVITATIONS_MEASURED: &[(Request, HelperRows, HelperRows, HelperRows)] = &[
    (Request { caller: None, values: &[] }, &[], &[], &[]),
    (Request { caller: None, values: &[(VERIFIED_EMAIL, &[""])] }, &[], &[], &["i3"]),
    (
        Request { caller: None, values: &[(VERIFIED_EMAIL, &["eve@example.com"])] },
        &["i1"],
        &["i1"],
        &["i1"],
    ),
    (
        Request { caller: None, values: &[(VERIFIED_EMAIL, &["Eve@Example.COM"])] },
        &["i1"],
        &[],
        &["i1"],
    ),
    (
        Request { caller: None, values: &[(VERIFIED_EMAIL, &["BOB@EXAMPLE.COM"])] },
        &["i2"],
        &[],
        &["i2"],
    ),
    (
        Request { caller: None, values: &[(VERIFIED_EMAIL, &["mallory@example.com"])] },
        &[],
        &[],
        &[],
    ),
];

/// A deployment declaring the caller, the email a request verified as a
/// scalar and a delimited set of links, each of which a request may leave
/// unset.
fn verifying() -> Deployment {
    let setting = |name: &str, value: SettingValue| {
        SessionSetting::value(name, value).expect("a custom setting name").may_be_unset()
    };
    Deployment::new(
        "app",
        ["app"],
        [
            setting("app.user_id", SettingValue::Caller(DataType::Text)),
            setting(VERIFIED_EMAIL, SettingValue::Scalar(DataType::Text)),
            setting("app.link_ids", delimited(",")),
        ],
    )
    .expect("a valid deployment")
}

#[test]
fn a_column_compared_with_a_scalar_returns_what_postgres_returns_for_every_request_value() {
    let database = postgres(INVITATIONS);
    let deployment = verifying();
    let answered =
        |helper: &str| answer(&database, &deployment, "authz", helper, &[]).expect("answered");
    let normalized = answered("normalized_invitations");
    let raw = answered("raw_invitations");
    let bare = answered("bare_invitations");
    let lowered = "invitations.id WHERE email = lower(app.verified_email)";
    assert_eq!(render(&normalized), lowered);
    assert_eq!(render(&raw), "invitations.id WHERE email = app.verified_email");
    assert_eq!(render(&bare), lowered);
    for (set, excludes_unset) in [(&normalized, true), (&raw, true), (&bare, false)] {
        assert_eq!(covered(set), Some("invitations"));
        let HelperArm::Grant(grant) = &set.arms()[0] else { panic!("a grant arm") };
        let GrantedTo::Request(request) = grant.granted_to() else {
            panic!("the email names the grantee")
        };
        assert!(!request.raises_when_unset());
        assert_eq!(request.excludes_unset(), excludes_unset, "{}", render(set));
    }
    for &(request, normalized_returns, raw_returns, bare_returns) in INVITATIONS_MEASURED {
        for (set, expected) in
            [(&normalized, normalized_returns), (&raw, raw_returns), (&bare, bare_returns)]
        {
            let returned: Vec<_> = evaluate(set, request, INVITATION_ROWS).into_iter().collect();
            assert_eq!(returned, expected, "{} for {:?}", render(set), request.values);
        }
    }
}

/// What `authz.invited()` keeping the invitations `condition` keeps answers
/// once `after` declares what else it calls.
fn inviting(condition: &str, after: &str) -> Result<String, HelperSetError> {
    let sql = format!(
        "{INVITATIONS}
         CREATE FUNCTION authz.invited() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT invitation.id FROM public.invitations invitation WHERE {condition}
         $$;
         ALTER FUNCTION authz.invited() OWNER TO authz_owner;
         {after}"
    );
    let database = postgres(&sql);
    let deployment = verifying();
    answer(&database, &deployment, "authz", "invited", &[]).map(|set| render(&set))
}

#[test]
fn a_lowered_scalar_is_read_in_every_spelling_of_its_read() {
    let lowered = "invitations.id WHERE email = lower(app.verified_email)";
    let nulled = accessor(
        "nulled_email",
        "TEXT",
        ACCESSOR,
        "NULLIF(lower(current_setting('app.verified_email', true)), '')",
    );
    for (condition, after) in [
        ("invitation.email = authz.verified_email()", ""),
        ("authz.verified_email() = invitation.email", ""),
        ("invitation.email = authz.nulled_email()", nulled.as_str()),
        ("invitation.email = lower(NULLIF(current_setting('app.verified_email', true), ''))", ""),
        ("invitation.email = NULLIF(lower(current_setting('app.verified_email', true)), '')", ""),
        ("invitation.email = pg_catalog.lower(current_setting('app.verified_email'))", ""),
        ("lower(current_setting('app.verified_email', true))::text = invitation.email", ""),
        ("invitation.email = CAST(lower(current_setting('app.verified_email', true)) AS TEXT)", ""),
    ] {
        assert_eq!(inviting(condition, after).as_deref(), Ok(lowered), "{condition}");
    }
    assert_eq!(
        inviting("invitation.email IN (authz.verified_email(), 'admin@example.com')", "")
            .as_deref(),
        Ok("invitations.id WHERE email = lower(app.verified_email) \
            UNION invitations.id WHERE email IN ('admin@example.com')"),
        "a listed scalar"
    );
    assert_eq!(
        inviting("invitation.email IN (authz.raw_email())", "").as_deref(),
        Ok("invitations.id WHERE email = app.verified_email"),
        "a scalar listed as sent"
    );
}

#[test]
fn a_read_normalized_otherwise_or_of_anything_but_a_scalar_refuses_the_answer() {
    let normalized = |setting: &str| {
        Err(HelperSetError::SettingNormalized {
            function: "authz.invited".into(),
            setting: setting.into(),
            normalization: RequestNormalization::Lower,
        })
    };
    let lowered_caller = accessor(
        "lowered_caller",
        "TEXT",
        ACCESSOR,
        "lower(NULLIF(current_setting('app.user_id', true), ''))",
    );
    for (condition, after) in [
        ("invitation.email = lower(current_setting('app.user_id', true))", ""),
        ("invitation.email = authz.lowered_caller()", lowered_caller.as_str()),
    ] {
        assert_eq!(inviting(condition, after), normalized("app.user_id"), "a lowered caller");
    }
    assert_eq!(
        inviting(
            "invitation.email = ANY (string_to_array(lower(current_setting('app.link_ids', true)), ','))",
            ""
        ),
        normalized("app.link_ids"),
        "a lowered set"
    );
    assert_eq!(
        inviting("invitation.email = lower(current_setting('app.user_id', true))", "")
            .map_err(|refusal| refusal.to_string()),
        Err("helper `authz.invited` compares a column with `lower` of setting `app.user_id`, \
             which is not a scalar"
            .to_string()),
        "the refusal names the normalization"
    );
    let lowered_lift = capping(
        "lower(current_setting('app.installation_id', true)) IS NULL \
         OR (resource.organization_id, resource.kind) IN (SELECT * FROM authz.capped_kinds(min_role))",
    );
    assert!(
        matches!(lowered_lift, Err(HelperSetError::Irreducible { .. })),
        "a lowered read lifts no cap: {lowered_lift:?}"
    );
    let shadow =
        "CREATE FUNCTION authz.lower(TEXT) RETURNS TEXT LANGUAGE sql STABLE AS $$ SELECT $1 $$;";
    for (condition, after) in [
        ("invitation.email = upper(current_setting('app.verified_email', true))", ""),
        ("invitation.email = lower(lower(current_setting('app.verified_email', true)))", ""),
        ("invitation.email = authz.lower(current_setting('app.verified_email', true))", shadow),
        ("invitation.email = lower(current_setting('app.verified_email', true) COLLATE \"C\")", ""),
        (
            "invitation.email = authz.verified_email() \
             AND lower(current_setting('app.user_id', true)) IS NOT NULL",
            "",
        ),
        (
            "invitation.email = authz.verified_email() \
             AND lower(current_setting('app.user_id', true)) <> 'root'",
            "",
        ),
    ] {
        let refused = inviting(condition, after);
        assert!(
            matches!(refused, Err(HelperSetError::Irreducible { .. })),
            "{condition}: {refused:?}"
        );
    }
    assert!(
        matches!(
            inviting(
                "invitation.email = authz.verified_email() \
                 AND invitation.email = current_setting('app.user_id', true)",
                ""
            ),
            Err(HelperSetError::ArmShape { .. })
        ),
        "an arm names one grantee"
    );
}

/// Grants a caller may share, as connetto's core schema writes them, an
/// admin of a resource sharing every role on it and a writer sharing the
/// roles up to writer on a resource whose writers may share.
const SHARES: &str = "
CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
CREATE SCHEMA authz;
GRANT USAGE ON SCHEMA authz TO authz_owner;
CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
CREATE TABLE resources (id TEXT PRIMARY KEY, writers_can_share BOOLEAN NOT NULL DEFAULT false);
CREATE TABLE grants (
  resource_id TEXT NOT NULL REFERENCES resources (id), principal_id TEXT NOT NULL,
  role authz.role NOT NULL, PRIMARY KEY (resource_id, principal_id));
GRANT SELECT ON resources, grants TO authz_owner;
CREATE FUNCTION authz.resources_with(min_role authz.role) RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT g.resource_id FROM public.grants g
  WHERE g.principal_id = current_setting('app.user_id', true) AND g.role >= min_role
$$;
CREATE FUNCTION authz.sharable_grants() RETURNS TABLE (resource_id TEXT, role authz.role)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id, allowed.role
  FROM authz.resources_with('admin') resource(id)
  CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)
  UNION
  SELECT resource.id, allowed.role FROM public.resources resource
  CROSS JOIN (VALUES ('reader'::authz.role), ('writer'::authz.role)) allowed(role)
  WHERE resource.writers_can_share AND resource.id IN (SELECT authz.resources_with('writer'))
$$;
CREATE FUNCTION authz.shared_resources() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT share.resource_id FROM authz.sharable_grants() share(resource_id, role)
$$;
CREATE FUNCTION authz.sharing_writers() RETURNS SETOF TEXT
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
  SELECT resource.id FROM public.resources resource
  WHERE resource.writers_can_share AND resource.id IN (SELECT authz.resources_with('writer'))
$$;
ALTER FUNCTION authz.resources_with(authz.role) OWNER TO authz_owner;
ALTER FUNCTION authz.sharable_grants() OWNER TO authz_owner;
ALTER FUNCTION authz.shared_resources() OWNER TO authz_owner;
ALTER FUNCTION authz.sharing_writers() OWNER TO authz_owner;
";

/// The rows [`SHARES`] was measured over.
const SHARE_ROWS: Rows = &[
    ("resources", &[("id", "r1"), ("writers_can_share", "true")]),
    ("resources", &[("id", "r2"), ("writers_can_share", "false")]),
    ("resources", &[("id", "r3"), ("writers_can_share", "true")]),
    ("resources", &[("id", "r4"), ("writers_can_share", "true")]),
    ("grants", &[("resource_id", "r1"), ("principal_id", "alice"), ("role", "writer")]),
    ("grants", &[("resource_id", "r2"), ("principal_id", "alice"), ("role", "admin")]),
    ("grants", &[("resource_id", "r3"), ("principal_id", "alice"), ("role", "reader")]),
    ("grants", &[("resource_id", "r4"), ("principal_id", "bob"), ("role", "writer")]),
    ("grants", &[("resource_id", "r1"), ("principal_id", "bob"), ("role", "admin")]),
    ("grants", &[("resource_id", "r3"), ("principal_id", "bob"), ("role", "writer")]),
];

/// What PostgreSQL 18.6 returns for `(helper, caller, returned)` over
/// [`SHARE_ROWS`], each returned row its columns joined by `,`.
const SHARES_MEASURED: &[(&str, &str, &[&str])] = &[
    ("sharable_grants", "alice", &["r1,reader", "r1,writer", "r2,admin", "r2,reader", "r2,writer"]),
    (
        "sharable_grants",
        "bob",
        &["r1,admin", "r1,reader", "r1,writer", "r3,reader", "r3,writer", "r4,reader", "r4,writer"],
    ),
    ("sharable_grants", "carol", &[]),
    ("shared_resources", "alice", &["r1", "r2"]),
    ("shared_resources", "bob", &["r1", "r3", "r4"]),
    ("sharing_writers", "alice", &["r1"]),
    ("sharing_writers", "bob", &["r1", "r3", "r4"]),
];

/// `authz.resources_with` as answered at `role`, rendered.
fn resources_with(role: &str) -> String {
    format!(
        "authz.resources_with('{role}') {{grants.resource_id WHERE principal_id = app.user_id AND role IN ({})}}",
        match role {
            "admin" => "'admin'",
            "writer" => "'writer', 'admin'",
            other => panic!("the fixtures call no {other}"),
        }
    )
}

#[test]
fn a_helper_returning_labelled_keys_answers_each_label_beside_its_rows() {
    let database = postgres(SHARES);
    let deployment = deployment();
    let sharable =
        answer(&database, &deployment, "authz", "sharable_grants", &[]).expect("answered");
    let admin = resources_with("admin");
    let (called, set) = admin.split_once(" {").expect("a set follows the call");
    assert_eq!(
        render(&sharable),
        format!(
            "{called}.(#0, EACH('reader', 'writer', 'admin')) {{{set} \
             UNION resources.(id, EACH('reader', 'writer')) WHERE writers_can_share IN (true) AND (id) IN {}",
            resources_with("writer"),
        )
    );
    assert!(sharable.covered_table().is_none(), "a pair is no key of one table");
    let shared =
        answer(&database, &deployment, "authz", "shared_resources", &[]).expect("answered");
    let [HelperArm::Called(projected)] = shared.arms() else { panic!("one arm calls the pairs") };
    assert_eq!(projected.returned(), [Returned::Read(0)]);
    assert_eq!(covered(&shared), None, "a column of pairs covers no table");
    let administered = SHARES.to_string()
        + "CREATE FUNCTION authz.administered() RETURNS SETOF TEXT
           LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
             SELECT administered FROM authz.resources_with('admin') administered
           $$;
           ALTER FUNCTION authz.administered() OWNER TO authz_owner;";
    let administered = postgres(&administered);
    let set = answer(&administered, &deployment, "authz", "administered", &[]).expect("answered");
    assert_eq!(covered(&set), Some("resources"), "the called set's keys");
    let writers =
        answer(&database, &deployment, "authz", "sharing_writers", &[]).expect("answered");
    assert_eq!(
        render(&writers),
        format!(
            "resources.id WHERE writers_can_share IN (true) AND (id) IN {}",
            resources_with("writer")
        )
    );
    assert_eq!(covered(&writers), Some("resources"));
    for &(helper, caller, expected) in SHARES_MEASURED {
        let set = answer(&database, &deployment, "authz", helper, &[]).expect("answered");
        let returned: Vec<String> = arm_rows(&set, Request::of(caller), SHARE_ROWS)
            .into_iter()
            .map(|row| row.join(","))
            .collect();
        assert_eq!(returned, expected, "{helper} for {caller}");
    }
}

#[test]
fn the_keys_of_labelled_rows_are_their_union_over_every_label() {
    let database = postgres(SHARES);
    let deployment = deployment();
    let sharable =
        answer(&database, &deployment, "authz", "sharable_grants", &[]).expect("answered");
    // `resource_id IN (SELECT share.resource_id FROM authz.sharable_grants()
    // share)`, as PostgreSQL 18.6 answers it over [`SHARE_ROWS`].
    for (caller, measured) in [("alice", ["r1", "r2"].as_slice()), ("bob", &["r1", "r3", "r4"])] {
        let keys: BTreeSet<&str> = arm_rows(&sharable, Request::of(caller), SHARE_ROWS)
            .into_iter()
            .map(|row| row[0])
            .collect();
        assert_eq!(keys.into_iter().collect::<Vec<_>>(), measured, "{caller}");
    }
}

/// `authz.sharable_grants` with its first arm written as `first` and its
/// second as `second`, beside `after`.
fn sharing(first: &str, second: &str, after: &str) -> Result<String, HelperSetError> {
    let first_arm = "SELECT resource.id, allowed.role
  FROM authz.resources_with('admin') resource(id)
  CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)";
    let second_arm = "SELECT resource.id, allowed.role FROM public.resources resource
  CROSS JOIN (VALUES ('reader'::authz.role), ('writer'::authz.role)) allowed(role)
  WHERE resource.writers_can_share AND resource.id IN (SELECT authz.resources_with('writer'))";
    let sql = SHARES.replace(first_arm, first).replace(second_arm, second) + after;
    let database = postgres(&sql);
    let deployment = deployment();
    answer(&database, &deployment, "authz", "sharable_grants", &[]).map(|set| render(&set))
}

#[test]
fn labels_are_read_in_every_spelling_postgres_resolves_to_the_declared_type() {
    let keep = "SELECT resource.id, allowed.role FROM public.resources resource CROSS JOIN (VALUES ('admin'::authz.role)) allowed(role)";
    for (labels, rendered) in [
        ("unnest(enum_range(NULL::authz.role)) allowed(role)", "EACH('reader', 'writer', 'admin')"),
        ("unnest(enum_range(NULL::authz.role)) role", "EACH('reader', 'writer', 'admin')"),
        (
            "(VALUES ('writer'), ('reader'::authz.role), ('writer')) allowed(role)",
            "EACH('writer', 'reader')",
        ),
    ] {
        let first =
            format!("SELECT resource.id, role FROM public.resources resource CROSS JOIN {labels}");
        assert_eq!(
            sharing(&first, keep, "").as_deref().map(|set| set.split(" UNION ").next()),
            Ok(Some(format!("resources.(id, {rendered})").as_str())),
            "{labels}"
        );
    }
    let unaliased = "SELECT resource.id, unnest FROM public.resources resource \
                     CROSS JOIN unnest(enum_range(NULL::authz.role))";
    assert_eq!(
        sharing(unaliased, keep, "").as_deref().map(|set| set.split(" UNION ").next()),
        Ok(Some("resources.(id, EACH('reader', 'writer', 'admin'))")),
        "an unaliased unnest names its column unnest"
    );
    let (called, set) = resources_with("admin")
        .split_once(" {")
        .map(|(called, set)| (called.to_string(), set.to_string()))
        .expect("a set follows the call");
    for (first, returned) in [
        (
            "SELECT id, allowed.role FROM authz.resources_with('admin') id CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)",
            "(#0, EACH('reader', 'writer', 'admin'))",
        ),
        (
            "SELECT resources_with, allowed.role FROM authz.resources_with('admin') CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)",
            "(#0, EACH('reader', 'writer', 'admin'))",
        ),
    ] {
        let answered = sharing(first, keep, "");
        let expected = format!("{called}.{returned} {{{set}");
        assert_eq!(
            answered.as_deref().map(|set| set.split(" UNION ").next()),
            Ok(Some(expected.as_str())),
            "{first}"
        );
    }
    // An unqualified enum type is the one in `public`.
    let public = "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
        CREATE TYPE access AS ENUM ('reader', 'writer');
        CREATE TABLE resources (id TEXT PRIMARY KEY);
        GRANT SELECT ON resources TO authz_owner;
        CREATE FUNCTION accesses() RETURNS TABLE (resource_id TEXT, access access)
        LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
          SELECT resource.id, allowed.access FROM resources resource
          CROSS JOIN unnest(enum_range(NULL::access)) allowed(access)
        $$;
        ALTER FUNCTION accesses() OWNER TO authz_owner;";
    let database = postgres(public);
    let deployment = deployment();
    let accesses =
        answer(&database, &deployment, "public", "accesses", &[]).map(|set| render(&set));
    assert_eq!(accesses.as_deref(), Ok("resources.(id, EACH('reader', 'writer'))"));
}

#[test]
fn labels_postgres_refuses_or_names_beside_the_rows_refuse_the_answer() {
    let first = "SELECT resource.id, allowed.role
  FROM authz.resources_with('admin') resource(id)
  CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)";
    let crossing = |labels: &str, condition: &str| {
        format!(
            "SELECT resource.id, allowed.role FROM public.resources resource CROSS JOIN {labels} {condition}"
        )
    };
    let values = "(VALUES ('reader'::authz.role)) allowed(role)";
    for second in [
        crossing("(VALUES ('reader'), ('writer')) allowed(role)", ""),
        crossing("(VALUES ('owner'::authz.role)) allowed(role)", ""),
        crossing("(VALUES ('reader'::TEXT)) allowed(role)", ""),
        crossing("(VALUES ('reader'::authz.role, 1)) allowed(role)", ""),
        crossing("(VALUES (resource.id::authz.role)) allowed(role)", ""),
        crossing("unnest(enum_range(NULL::TEXT)) allowed(role)", ""),
        crossing("unnest(enum_range('reader'::authz.role)) allowed(role)", ""),
        crossing("unnest(enum_range(NULL::authz.role, NULL)) allowed(role)", ""),
        crossing("unnest(enum_range(NULL)) allowed(role)", ""),
        crossing("unnest(enum_range(NULL::authz.role)) allowed(role authz.role)", ""),
        // PostgreSQL answers these two, but a limited list or one the arm
        // does not return is no list of labels the rows are paired with.
        crossing("(VALUES ('reader'::authz.role) LIMIT 1) allowed(role)", ""),
        crossing(
            "unnest(enum_range(NULL::authz.role)) allowed(role) \
             CROSS JOIN (VALUES ('reader'::authz.role)) granted(other)",
            "",
        ),
        crossing(
            "unnest(enum_range(NULL::authz.role)) WITH ORDINALITY allowed(role, position)",
            "",
        ),
        crossing("public.grants allowed", ""),
        crossing(values, "WHERE allowed.role = 'reader'"),
        crossing(values, "WHERE role = 'reader'"),
        crossing("(VALUES ('reader'::authz.role)) resource(role)", ""),
        "SELECT resource.id, allowed.role FROM public.resources resource \
         CROSS JOIN (VALUES ('reader'::authz.role)) allowed(role) \
         CROSS JOIN (VALUES ('writer'::authz.role)) allowed(role)"
            .to_string(),
        "SELECT resource.id, allowed.role FROM authz.resources_with('writer') resource(id) \
         CROSS JOIN (VALUES ('reader'::authz.role)) allowed(role) WHERE resource.id = 'r1'"
            .to_string(),
        "SELECT resource.id, allowed.role FROM authz.resources_with(resource.id) resource(id) \
         CROSS JOIN (VALUES ('reader'::authz.role)) allowed(role)"
            .to_string(),
        "SELECT resource.id, allowed.role FROM authz.resources_with('writer') resource(id, other) \
         CROSS JOIN (VALUES ('reader'::authz.role)) allowed(role)"
            .to_string(),
        // PostgreSQL refuses `text` returned as `authz.role`.
        "SELECT allowed.role, resource.id FROM authz.resources_with('admin') resource(id) \
         CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)"
            .to_string(),
    ] {
        let answered = sharing(first, &second, "");
        assert!(matches!(answered, Err(HelperSetError::ArmShape { .. })), "{second}: {answered:?}");
    }
    let missing = "SELECT resource.id, allowed.role FROM authz.missing('writer') resource(id) \
                   CROSS JOIN (VALUES ('reader'::authz.role)) allowed(role)";
    let answered = sharing(first, missing, "");
    assert!(
        matches!(&answered, Err(HelperSetError::UnresolvedCall { call, .. }) if call == "authz.missing"),
        "{answered:?}"
    );
    // `SET search_path = authz` puts `pg_catalog` first, but a reachable
    // overload is refused as it is for every catalog function.
    let pathed = |after: &str| {
        let sql = SHARES.replace("SET search_path = ''", "SET search_path = authz") + after;
        let database = postgres(&sql);
        let deployment = deployment();
        answer(&database, &deployment, "authz", "sharable_grants", &[]).map(|set| render(&set))
    };
    assert!(pathed("").is_ok(), "{:?}", pathed(""));
    let shadowing = "CREATE FUNCTION authz.unnest(anyarray) RETURNS SETOF anyelement \
                     LANGUAGE sql IMMUTABLE AS $$ SELECT NULL $$;";
    let answered = pathed(shadowing);
    assert!(matches!(answered, Err(HelperSetError::ArmShape { .. })), "{answered:?}");
    // PostgreSQL refuses two lists of one name crossed with a call.
    let crossing_call = |alias: &str| {
        let sql = format!(
            "{SHARES}CREATE FUNCTION authz.role_pairs()
             RETURNS TABLE (resource_id TEXT, role authz.role, other authz.role)
             LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
               SELECT resource.id, allowed.role, {alias}.other
               FROM authz.resources_with('admin') resource(id)
               CROSS JOIN unnest(enum_range(NULL::authz.role)) allowed(role)
               CROSS JOIN (VALUES ('reader'::authz.role)) {alias}(other)
             $$;
             ALTER FUNCTION authz.role_pairs() OWNER TO authz_owner;"
        );
        let database = postgres(&sql);
        answer(&database, &deployment(), "authz", "role_pairs", &[]).map(|set| render(&set))
    };
    assert!(crossing_call("granted").is_ok(), "{:?}", crossing_call("granted"));
    let answered = crossing_call("allowed");
    assert!(matches!(answered, Err(HelperSetError::ArmShape { .. })), "{answered:?}");
}

#[test]
fn a_key_compared_with_a_set_alone_is_capped_by_it() {
    let alone = SHARES.replace(
        "WHERE resource.writers_can_share AND resource.id IN (SELECT authz.resources_with('writer'))\n$$;\nALTER",
        "WHERE resource.id IN (SELECT * FROM authz.resources_with('writer'))\n$$;\nALTER",
    );
    let database = postgres(&alone);
    let deployment = deployment();
    let writers =
        answer(&database, &deployment, "authz", "sharing_writers", &[]).expect("answered");
    assert_eq!(
        render(&writers),
        format!("resources.id WHERE (id) IN {}", resources_with("writer"))
    );
}

/// `authz.ids_0()` returning the caller's items, then `authz.ids_<k>()` for
/// every `k` up to `depth`, each the items `ids_<k - 1>` returns and their
/// children, which reads `ids_<k - 1>()` twice.
fn item_chain(depth: usize) -> String {
    let mut sql = String::from(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE TABLE items (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, parent_id TEXT REFERENCES items (id));
         GRANT SELECT ON items TO authz_owner;
         CREATE SCHEMA authz;
         GRANT USAGE ON SCHEMA authz TO authz_owner;
         CREATE FUNCTION authz.ids_0() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
           SELECT item.id FROM public.items item WHERE item.owner_id = current_setting('app.user_id', true)
         $$;
         ALTER FUNCTION authz.ids_0() OWNER TO authz_owner;",
    );
    sql.extend((1..=depth).map(|level| {
        let below = level - 1;
        format!(
            "CREATE FUNCTION authz.ids_{level}() RETURNS SETOF TEXT
             LANGUAGE sql STABLE SECURITY DEFINER SET search_path = '' AS $$
               SELECT item.id FROM public.items item WHERE item.id IN (SELECT authz.ids_{below}())
               UNION
               SELECT item.id FROM public.items item WHERE item.parent_id IN (SELECT authz.ids_{below}())
             $$;
             ALTER FUNCTION authz.ids_{level}() OWNER TO authz_owner;"
        )
    }));
    sql
}

#[test]
fn a_helper_called_twice_with_the_same_arguments_is_answered_once() {
    const DEPTH: usize = 40;
    let database = postgres(&item_chain(DEPTH));
    let deployment = deployment();
    let answered =
        answer(&database, &deployment, "authz", &format!("ids_{DEPTH}"), &[]).expect("answered");
    let mut set = &answered;
    for level in (1..=DEPTH).rev() {
        let [HelperArm::Grant(capped), HelperArm::Parent(parent)] = set.arms() else {
            panic!("ids_{level} answers a capped arm and a parent arm: {}", render(set))
        };
        let [cap] = capped.caps() else { panic!("ids_{level} caps its first arm once") };
        assert_eq!(cap.function().name(), format!("ids_{}", level - 1));
        assert!(
            Arc::ptr_eq(cap.set(), parent.set()),
            "both arms of ids_{level} read the one answer of ids_{}",
            level - 1
        );
        set = parent.set();
    }
    assert_eq!(render(set), "items.id WHERE owner_id = app.user_id");
}
