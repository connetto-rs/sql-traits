//! Role-independent policy clauses, target routing and diagnostics.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{CreatePolicyCommand, CreatePolicyType},
    dialect::PostgreSqlDialect,
};

#[test]
fn definitions_survive_clauseless_and_outside_role_runtime_results() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE ROLE app;
         CREATE ROLE other;
         CREATE TABLE docs(id INTEGER);
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p_clauseless ON docs AS RESTRICTIVE FOR SELECT;
         CREATE POLICY p_allwc ON docs FOR ALL WITH CHECK (true);
         CREATE POLICY p_other ON docs FOR SELECT TO other USING (true);",
    )
    .expect("schema builds");
    let policy = |name: &str| db.policies().find(|policy| policy.name() == name).expect("policy");
    let clauseless = policy("p_clauseless").clause_analysis(&db).expect("analysis");
    assert_eq!(clauseless.command(), CreatePolicyCommand::Select);
    assert_eq!(clauseless.mode(), CreatePolicyType::Restrictive);
    assert_eq!(clauseless.invalid_clause(), None);
    assert_eq!(clauseless.targets(PolicyClause::Using).collect::<Vec<_>>(), []);
    assert_eq!(
        clauseless
            .missing_clauses()
            .map(|missing| (missing.clause(), missing.commands().collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
        [(PolicyClause::Using, vec![GatedCommand::Select])]
    );

    let all = policy("p_allwc").clause_analysis(&db).expect("analysis");
    assert_eq!(all.command(), CreatePolicyCommand::All);
    assert_eq!(all.mode(), CreatePolicyType::Permissive);
    assert_eq!(all.targets(PolicyClause::Using).collect::<Vec<_>>(), []);
    assert_eq!(
        all.targets(PolicyClause::WithCheck).collect::<Vec<_>>(),
        [PolicyTarget::Insert, PolicyTarget::UpdateCheck]
    );
    assert_eq!(
        all.missing_clauses()
            .map(|missing| (missing.clause(), missing.commands().collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
        [(
            PolicyClause::Using,
            vec![GatedCommand::Select, GatedCommand::Update, GatedCommand::Delete]
        )]
    );

    let outside = policy("p_other").clause_analysis(&db).expect("analysis");
    assert_eq!(outside.command(), CreatePolicyCommand::Select);
    assert_eq!(outside.targets(PolicyClause::Using).collect::<Vec<_>>(), [PolicyTarget::Select]);
    assert_eq!(outside.source(PolicyTarget::Select), Some(PolicyClause::Using));
    assert_eq!(
        outside.clause(PolicyClause::Using).expect("stored clause").expression().to_string(),
        "true"
    );

    let docs = db
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("docs");
    let runtime = docs
        .row_security(RowStatement::Select, db.role("app").expect("app"), &db)
        .expect("runtime");
    let gate = &runtime.gates().expect("RLS applies")[0];
    assert_eq!(gate.permissive(), []);
    assert_eq!(gate.restrictive(), []);
}

#[test]
fn unreadable_stored_check_blocks_using_fallback() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE docs(expires TIMESTAMPTZ);
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p ON docs USING (true) WITH CHECK (expires > '2026-01-01 09:00');",
    )
    .expect("schema builds");
    let policy = db.policies().next().expect("policy");
    let analysis = policy.clause_analysis(&db).expect("analysis");
    assert_eq!(analysis.source(PolicyTarget::Insert), Some(PolicyClause::WithCheck));
    assert_eq!(analysis.source(PolicyTarget::UpdateCheck), Some(PolicyClause::WithCheck));
    assert_eq!(
        analysis.targets(PolicyClause::Using).collect::<Vec<_>>(),
        [PolicyTarget::Select, PolicyTarget::UpdateUsing, PolicyTarget::Delete]
    );
    assert!(matches!(
        analysis.clause(PolicyClause::WithCheck),
        Some(PolicyExpression::Written { .. })
    ));
    let docs = policy.table(&db).expect("docs");
    let runtime = docs
        .row_security(RowStatement::Insert { returning: false }, ActingRole::Public, &db)
        .expect("runtime");
    let term = &runtime.gates().expect("RLS applies")[0].permissive()[0];
    assert_eq!(term.clause(), PolicyClause::WithCheck);
    assert_eq!(Some(term.expression()), analysis.clause(PolicyClause::WithCheck));
}

#[test]
fn update_check_fallback_retains_nominal_context() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE docs(id INT);
         CREATE POLICY p ON docs AS RESTRICTIVE FOR UPDATE USING (id > 0);",
    )
    .expect("schema builds");
    let analysis = db.policies().next().expect("policy").clause_analysis(&db).expect("analysis");
    assert_eq!(
        analysis.targets(PolicyClause::Using).collect::<Vec<_>>(),
        [PolicyTarget::UpdateUsing, PolicyTarget::UpdateCheck]
    );
    assert_eq!(PolicyTarget::UpdateCheck.command(), GatedCommand::Update);
    assert_eq!(PolicyTarget::UpdateCheck.clause(), PolicyClause::WithCheck);
    assert_eq!(analysis.source(PolicyTarget::UpdateCheck), Some(PolicyClause::Using));
    assert_eq!(analysis.clause(PolicyClause::WithCheck), None);
    assert_eq!(analysis.missing_clauses().collect::<Vec<_>>(), []);
}
