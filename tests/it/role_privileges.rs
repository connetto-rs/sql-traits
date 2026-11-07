//! The privileges one role holds on another, as `pg_has_role` answers them.
//!
//! Each expected row is the answer PostgreSQL 18.6 gives for the schema in
//! [`SCHEMA`], one `target=MUSA` entry per target, where `M`, `U`, `S` and
//! `A` mark `MEMBER`, `USAGE`, `SET` and `MEMBER WITH ADMIN OPTION`, and `-`
//! marks a privilege the member lacks.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const SCHEMA: &str = "
CREATE ROLE login_r LOGIN NOINHERIT;
CREATE ROLE kind_a NOLOGIN;
CREATE ROLE kind_b NOLOGIN NOINHERIT;
CREATE ROLE mid_inh NOLOGIN;
CREATE ROLE mid_noinh NOLOGIN NOINHERIT;
CREATE ROLE top1 NOLOGIN;
CREATE ROLE top2 NOLOGIN;
CREATE ROLE top3 NOLOGIN;
CREATE ROLE top4 NOLOGIN;
CREATE ROLE top5 NOLOGIN;
CREATE ROLE top6 NOLOGIN;
CREATE ROLE top7 NOLOGIN;
CREATE ROLE super_r SUPERUSER NOLOGIN;
CREATE ROLE under_super NOLOGIN;
GRANT kind_a TO login_r WITH INHERIT FALSE, SET TRUE;
GRANT top1 TO kind_a;
GRANT top2 TO kind_a WITH INHERIT FALSE;
GRANT top3 TO kind_a WITH SET FALSE;
GRANT top4 TO kind_a WITH ADMIN TRUE;
GRANT mid_inh TO kind_a;
GRANT top5 TO mid_inh WITH ADMIN TRUE;
GRANT mid_noinh TO kind_a;
GRANT top6 TO mid_noinh;
GRANT top7 TO mid_noinh WITH SET FALSE;
GRANT top1 TO kind_b;
GRANT top3 TO kind_b WITH SET FALSE;
GRANT super_r TO under_super;
";

const PRIVILEGES: [(char, RolePrivilege); 4] = [
    ('M', RolePrivilege::Member),
    ('U', RolePrivilege::Usage),
    ('S', RolePrivilege::Set),
    ('A', RolePrivilege::Admin),
];

/// Asserts that `member` holds on each target of `row` exactly the
/// privileges the row marks.
fn assert_row(member: &str, row: &str) {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema builds");
    let member_role = database.role(member).expect("the member exists");
    for entry in row.split_whitespace() {
        let (target, marks) = entry.split_once('=').expect("an entry reads target=MUSA");
        let target_role = database.role(target).expect("the target exists");
        let holds = |privilege| member_role.has_role(target_role, privilege, &database);
        let held: String = PRIVILEGES
            .iter()
            .map(|&(mark, privilege)| if holds(privilege) { mark } else { '-' })
            .collect();
        assert_eq!(held, marks, "{member} on {target}");
    }
}

#[test]
fn inheriting_role_reaches_its_chains_by_their_options() {
    assert_row(
        "kind_a",
        "kind_a=MUS- kind_b=---- login_r=---- mid_inh=MUS- mid_noinh=MUS- super_r=---- \
         top1=MUS- top2=M-S- top3=MU-- top4=MUSA top5=MUSA top6=M-S- top7=M--- under_super=----",
    );
}

#[test]
fn non_inheriting_role_uses_no_granted_role() {
    assert_row(
        "kind_b",
        "kind_a=---- kind_b=MUS- login_r=---- mid_inh=---- mid_noinh=---- super_r=---- \
         top1=M-S- top2=---- top3=M--- top4=---- top5=---- top6=---- top7=---- under_super=----",
    );
}

#[test]
fn chain_starting_without_inherit_keeps_member_set_and_admin() {
    assert_row(
        "login_r",
        "kind_a=M-S- kind_b=---- login_r=MUS- mid_inh=M-S- mid_noinh=M-S- super_r=---- \
         top1=M-S- top2=M-S- top3=M--- top4=M-SA top5=M-SA top6=M-S- top7=M--- under_super=----",
    );
}

#[test]
fn admin_option_is_held_by_the_role_granted_it() {
    assert_row(
        "mid_inh",
        "kind_a=---- kind_b=---- login_r=---- mid_inh=MUS- mid_noinh=---- super_r=---- \
         top1=---- top2=---- top3=---- top4=---- top5=MUSA top6=---- top7=---- under_super=----",
    );
}

#[test]
fn non_inheriting_middle_role_sets_but_does_not_use() {
    assert_row(
        "mid_noinh",
        "kind_a=---- kind_b=---- login_r=---- mid_inh=---- mid_noinh=MUS- super_r=---- \
         top1=---- top2=---- top3=---- top4=---- top5=---- top6=M-S- top7=M--- under_super=----",
    );
}

#[test]
fn superuser_holds_every_privilege_on_every_role_itself_included() {
    assert_row(
        "super_r",
        "kind_a=MUSA kind_b=MUSA login_r=MUSA mid_inh=MUSA mid_noinh=MUSA super_r=MUSA \
         top1=MUSA top2=MUSA top3=MUSA top4=MUSA top5=MUSA top6=MUSA top7=MUSA under_super=MUSA",
    );
}

#[test]
fn member_of_a_superuser_is_no_superuser() {
    assert_row(
        "under_super",
        "kind_a=---- kind_b=---- login_r=---- mid_inh=---- mid_noinh=---- super_r=MUS- \
         top1=---- top2=---- top3=---- top4=---- top5=---- top6=---- top7=---- under_super=MUS-",
    );
}

#[test]
fn role_without_memberships_holds_only_itself_without_admin() {
    assert_row(
        "top4",
        "kind_a=---- kind_b=---- login_r=---- mid_inh=---- mid_noinh=---- super_r=---- \
         top1=---- top2=---- top3=---- top4=MUS- top5=---- top6=---- top7=---- under_super=----",
    );
}
