//! Which policies gate which row of a statement, for one role, as PostgreSQL
//! applies them.
//!
//! Every expectation was measured on PostgreSQL 18.6 as a role that does not
//! own the table, one policy clause denied at a time.
#![allow(clippy::expect_used)]

use sql_traits::{errors::RowSecurityError, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

fn security<'db>(
    database: &'db ParserDB,
    role: &str,
    statement: RowStatement,
) -> Result<RowSecurity<'db, <ParserDB as DatabaseLike>::Policy>, RowSecurityError> {
    let role = database.role(role).expect("the role exists");
    table(database, "t").row_security(statement, role, database)
}

fn gates(database: &ParserDB, role: &str, statement: RowStatement) -> Vec<Gate> {
    let security = security(database, role, statement).expect("settled");
    let mut gates: Vec<_> = security
        .gates()
        .expect("the role is not exempt")
        .iter()
        .map(|gate| (gate.command(), gate.row(), gate.failure()))
        .collect();
    gates.sort();
    gates
}

/// The permissive and restrictive terms of the one gate `statement` puts on
/// `row` for `command`, as `policy: expression`.
fn terms(
    database: &ParserDB,
    statement: RowStatement,
    command: GatedCommand,
    row: RowVersion,
) -> (Vec<String>, Vec<String>) {
    let security = security(database, "reader", statement).expect("settled");
    let gate = security
        .gates()
        .expect("the role is not exempt")
        .iter()
        .find(|gate| gate.command() == command && gate.row() == row)
        .expect("the statement has that gate");
    let render = |terms: &[GateTerm<'_, <ParserDB as DatabaseLike>::Policy>]| {
        terms
            .iter()
            .map(|term| {
                format!("{} {:?}: {}", term.policy().name(), term.clause(), term.expression())
            })
            .collect()
    };
    (render(gate.permissive()), render(gate.restrictive()))
}

/// A gate as the comparison reads it: whose policies, which row, what failing
/// does.
type Gate = (GatedCommand, RowVersion, GateFailure);

const ONE_POLICY_PER_COMMAND: &str = "
    CREATE ROLE reader NOLOGIN;
    CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
    ALTER TABLE t ENABLE ROW LEVEL SECURITY;
    CREATE POLICY s ON t FOR SELECT USING (v > 0);
    CREATE POLICY i ON t FOR INSERT WITH CHECK (v > 0);
    CREATE POLICY u ON t FOR UPDATE USING (v > 0) WITH CHECK (v > 0);
    CREATE POLICY d ON t FOR DELETE USING (v > 0);";

#[test]
fn every_statement_shape_gates_the_rows_postgresql_gates() {
    use GateFailure::{Filter, Raise};
    use GatedCommand::{Delete, Insert, Select, Update};
    use RowVersion::{Existing, Inserted, Updated};

    let database = parse(ONE_POLICY_PER_COMMAND);
    let cases: [(RowStatement, &[Gate]); 16] = [
        (RowStatement::Select, &[(Select, Existing, Filter)]),
        (RowStatement::SelectLocking, &[(Select, Existing, Filter), (Update, Existing, Filter)]),
        (RowStatement::Insert { returning: false }, &[(Insert, Inserted, Raise)]),
        (
            RowStatement::Insert { returning: true },
            &[(Select, Inserted, Raise), (Insert, Inserted, Raise)],
        ),
        (
            RowStatement::Update { reads: false },
            &[(Update, Existing, Filter), (Update, Updated, Raise)],
        ),
        (
            RowStatement::Update { reads: true },
            &[
                (Select, Existing, Filter),
                (Select, Updated, Raise),
                (Update, Existing, Filter),
                (Update, Updated, Raise),
            ],
        ),
        (RowStatement::Delete { reads: false }, &[(Delete, Existing, Filter)]),
        (
            RowStatement::Delete { reads: true },
            &[(Select, Existing, Filter), (Delete, Existing, Filter)],
        ),
        (RowStatement::InsertOnConflictDoNothing { arbiter: false }, &[(Insert, Inserted, Raise)]),
        (
            RowStatement::InsertOnConflictDoNothing { arbiter: true },
            &[(Select, Inserted, Raise), (Insert, Inserted, Raise)],
        ),
        (
            RowStatement::InsertOnConflictDoUpdate,
            &[
                (Select, Existing, Raise),
                (Select, Inserted, Raise),
                (Select, Updated, Raise),
                (Insert, Inserted, Raise),
                (Update, Existing, Raise),
                (Update, Updated, Raise),
            ],
        ),
        (
            RowStatement::MergeInsert { returning: false },
            &[(Select, Existing, Filter), (Insert, Inserted, Raise)],
        ),
        (
            RowStatement::MergeInsert { returning: true },
            &[(Select, Existing, Filter), (Select, Inserted, Raise), (Insert, Inserted, Raise)],
        ),
        (
            RowStatement::MergeUpdate,
            &[
                (Select, Existing, Filter),
                (Select, Updated, Raise),
                (Update, Existing, Raise),
                (Update, Updated, Raise),
            ],
        ),
        (RowStatement::MergeDelete, &[(Select, Existing, Filter), (Delete, Existing, Raise)]),
        (RowStatement::MergeDoNothing, &[(Select, Existing, Filter)]),
    ];
    for (statement, expected) in cases {
        let mut expected = expected.to_vec();
        expected.sort();
        assert_eq!(gates(&database, "reader", statement), expected, "{statement:?}");
    }
}

#[test]
fn an_all_policy_gates_every_command_and_its_using_stands_in_for_a_missing_check() {
    let database = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY a ON t USING (v = 1);",
    );
    let using = (vec!["a Using: v = 1".to_string()], Vec::new());
    for (statement, command, row) in [
        (RowStatement::Select, GatedCommand::Select, RowVersion::Existing),
        (RowStatement::Insert { returning: false }, GatedCommand::Insert, RowVersion::Inserted),
        (RowStatement::Update { reads: false }, GatedCommand::Update, RowVersion::Updated),
        (RowStatement::Delete { reads: false }, GatedCommand::Delete, RowVersion::Existing),
    ] {
        assert_eq!(terms(&database, statement, command, row), using, "{statement:?}");
    }
}

#[test]
fn a_select_gate_on_a_new_row_reads_using_where_the_policy_also_has_a_check() {
    // `INSERT ... RETURNING` of a row with `v = 2` raises on PostgreSQL 18.6.
    let database = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY a ON t USING (v = 1) WITH CHECK (true);",
    );
    let returning = RowStatement::Insert { returning: true };
    assert_eq!(
        terms(&database, returning, GatedCommand::Select, RowVersion::Inserted),
        (vec!["a Using: v = 1".to_string()], Vec::new())
    );
    assert_eq!(
        terms(&database, returning, GatedCommand::Insert, RowVersion::Inserted),
        (vec!["a WithCheck: true".to_string()], Vec::new())
    );
}

#[test]
fn a_policy_lacking_the_clause_adds_no_term_whether_permissive_or_restrictive() {
    // `CREATE POLICY s ON t FOR SELECT` hides every row on PostgreSQL 18.6, and
    // a restrictive policy without `USING` hides none.
    let database = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY s ON t FOR SELECT;
         CREATE POLICY r ON t AS RESTRICTIVE FOR SELECT;
         CREATE POLICY i ON t FOR INSERT;",
    );
    let none = (Vec::<String>::new(), Vec::<String>::new());
    assert_eq!(
        terms(&database, RowStatement::Select, GatedCommand::Select, RowVersion::Existing),
        none
    );
    assert_eq!(
        terms(
            &database,
            RowStatement::Insert { returning: false },
            GatedCommand::Insert,
            RowVersion::Inserted
        ),
        none
    );
}

#[test]
fn restrictive_terms_stay_apart_from_permissive_ones() {
    let database = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p1 ON t USING (v = 1);
         CREATE POLICY p2 ON t FOR SELECT USING (v = 2);
         CREATE POLICY r ON t AS RESTRICTIVE USING (v <> 2);",
    );
    assert_eq!(
        terms(&database, RowStatement::Select, GatedCommand::Select, RowVersion::Existing),
        (
            vec!["p1 Using: v = 1".to_string(), "p2 Using: v = 2".to_string()],
            vec!["r Using: v <> 2".to_string()]
        )
    );
}

#[test]
fn a_to_clause_applies_through_inheriting_memberships_only() {
    let database = parse(
        "CREATE ROLE staff NOLOGIN;
         CREATE ROLE reader NOLOGIN IN ROLE staff;
         CREATE ROLE auditor NOLOGIN;
         CREATE ROLE outsider NOLOGIN;
         GRANT auditor TO reader WITH INHERIT FALSE;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY own ON t USING (v = 1);
         CREATE POLICY staff_only ON t TO staff USING (v = 2);
         CREATE POLICY audit_only ON t TO auditor USING (v = 3);
         CREATE POLICY outsider_only ON t TO outsider USING (v = 4);",
    );
    assert_eq!(
        terms(&database, RowStatement::Select, GatedCommand::Select, RowVersion::Existing),
        (vec!["own Using: v = 1".to_string(), "staff_only Using: v = 2".to_string()], Vec::new())
    );
}

/// A schema read open-world, as `pg2sqlite` and `rls2fga` read theirs, keeps a
/// policy naming a role it never declares, since roles are cluster objects a
/// dump leaves out.
#[test]
fn a_to_role_the_schema_cannot_settle_refuses_the_answer() {
    for (to, role) in [
        ("ghost", "ghost"),
        ("CURRENT_USER", "CURRENT_USER"),
        ("CURRENT_ROLE", "CURRENT_ROLE"),
        ("SESSION_USER", "SESSION_USER"),
    ] {
        let database = ParseOptions::default()
            .with_access_resolution(AccessResolution::OpenWorld)
            .parse::<PostgreSqlDialect>(&format!(
                "CREATE ROLE reader NOLOGIN;
                 CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
                 ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY p ON t TO {to} USING (true);"
            ))
            .expect("schema builds");
        assert_eq!(
            security(&database, "reader", RowStatement::Select).map(|_| ()),
            Err(RowSecurityError::UnsettledRole { policy: "p".into(), role: role.into() }),
            "TO {to}"
        );
    }
}

#[test]
fn owners_superusers_and_bypassers_are_exempt_as_postgresql_exempts_them() {
    let schema = |force: &str| {
        parse(&format!(
            "CREATE ROLE owner_r NOLOGIN;
             CREATE ROLE heir NOLOGIN IN ROLE owner_r;
             CREATE ROLE reader NOLOGIN;
             CREATE ROLE bypasser NOLOGIN BYPASSRLS;
             CREATE ROLE root NOLOGIN SUPERUSER;
             CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
             ALTER TABLE t OWNER TO owner_r;
             ALTER TABLE t ENABLE ROW LEVEL SECURITY;
             {force}"
        ))
    };
    let exemption = |database: &ParserDB, role: &str| {
        security(database, role, RowStatement::Select).expect("settled").exemption()
    };

    let unforced = schema("");
    assert_eq!(exemption(&unforced, "owner_r"), Some(RowSecurityExemption::Owner));
    assert_eq!(exemption(&unforced, "heir"), Some(RowSecurityExemption::Owner));
    assert_eq!(exemption(&unforced, "reader"), None);
    assert_eq!(exemption(&unforced, "bypasser"), Some(RowSecurityExemption::BypassRls));
    assert_eq!(exemption(&unforced, "root"), Some(RowSecurityExemption::Superuser));

    let forced = schema("ALTER TABLE t FORCE ROW LEVEL SECURITY;");
    assert_eq!(exemption(&forced, "owner_r"), None);
    assert_eq!(exemption(&forced, "heir"), None);
    assert_eq!(exemption(&forced, "bypasser"), Some(RowSecurityExemption::BypassRls));
    assert_eq!(exemption(&forced, "root"), Some(RowSecurityExemption::Superuser));

    let disabled = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);",
    );
    assert_eq!(exemption(&disabled, "reader"), Some(RowSecurityExemption::Disabled));
}

#[test]
fn a_table_whose_owner_is_never_named_binds_every_declared_role() {
    let database = parse(
        "CREATE ROLE reader NOLOGIN;
         CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;",
    );
    let security = security(&database, "reader", RowStatement::Select).expect("settled");
    assert_eq!(security.exemption(), None);
}
