//! The type each column reference of an expression or a data-modifying
//! statement outside a query takes, as PostgreSQL 18 binds it in the scope
//! the expression is written in.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::unreachable,
    reason = "Fixtures must hold the nodes they navigate to."
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{
        Assignment, CreateRuleEvent, Cte, DataType, Delete, DoUpdate, Expr, Insert, Merge,
        MergeAction, MergeClause, MergeInsertExpr, MergeInsertKind, MergeUpdateExpr,
        MergeUpdateKind, OnConflictAction, OnInsert, OutputClause, Query, SelectItem, SetExpr,
        Statement, Update,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
};

/// `t` holds `a int4` and `b numeric`, `logt` holds `a int8`, and `f` takes
/// `x int8` and an unnamed `int2`.
const SCHEMA: &str = "
CREATE TABLE t (a INT4, b NUMERIC);
CREATE TABLE logt (a INT8);
CREATE FUNCTION f(x INT8, INT2) RETURNS BOOL LANGUAGE sql RETURN true;
";

fn db() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).unwrap()
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten).unwrap().unwrap()
}

fn function(db: &ParserDB) -> &<ParserDB as DatabaseLike>::Function {
    db.functions().next().unwrap()
}

fn expression(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

fn statement(sql: &str) -> Statement {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap().remove(0)
}

fn pg(name: &str) -> OperandType<'_> {
    OperandType::Known {
        type_name: TargetName::new(name, false).with_schema("pg_catalog", false),
        typmod: None,
    }
}

/// The two operands of a binary expression.
fn operands(expression: &Expr) -> (&Expr, &Expr) {
    match expression {
        Expr::BinaryOp { left, right, .. } => (left, right),
        Expr::Nested(inner) => operands(inner),
        other => panic!("expected a binary expression, got {other:?}"),
    }
}

/// The types of both operands of `expression` in `context`.
fn both<'c>(
    context: &'c ExpressionContext<'_, '_, ParserDB>,
    expression: &Expr,
) -> Result<[OperandType<'c>; 2], LookupError> {
    let (left, right) = operands(expression);
    Ok([context.type_of(left)?, context.type_of(right)?])
}

/// Checks the types both operands of `sql` take in `scope`.
fn check(
    db: &ParserDB,
    scope: &ExpressionScope<'_, '_, ParserDB>,
    sql: &str,
    expected: &[OperandType<'_>; 2],
) {
    let root = expression(sql);
    let context =
        ExpressionContext::for_expression(&root, scope, db, IdentifierCase::AsWritten).unwrap();
    assert_eq!(&both(&context, &root).unwrap(), expected, "{sql}");
}

/// The lookup error typing the operands of `sql` in `scope` meets.
fn refused(db: &ParserDB, scope: &ExpressionScope<'_, '_, ParserDB>, sql: &str) -> LookupError {
    let root = expression(sql);
    let context =
        ExpressionContext::for_expression(&root, scope, db, IdentifierCase::AsWritten).unwrap();
    both(&context, &root).unwrap_err()
}

#[test]
fn a_table_scope_binds_bare_and_qualified_columns() {
    let db = db();
    let scope = || ExpressionScope::Table(table(&db, "t"));
    check(&db, &scope(), "a > b", &[pg("int4"), pg("numeric")]);
    check(&db, &scope(), "t.a > public.t.b", &[pg("int4"), pg("numeric")]);
    check(&db, &scope(), "logt.a > 1", &[OperandType::Unknown, pg("int4")]);
}

#[test]
fn a_query_nested_in_a_table_scope_reads_the_table_outside_it() {
    let db = db();
    let root = expression("EXISTS (SELECT 1 FROM logt WHERE logt.a > b)");
    let context = ExpressionContext::for_expression(
        &root,
        &ExpressionScope::Table(table(&db, "t")),
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Expr::Exists { subquery, .. } = &root else { unreachable!() };
    let SetExpr::Select(select) = subquery.body.as_ref() else { unreachable!() };
    let comparison = select.selection.as_ref().unwrap();
    assert_eq!(both(&context, comparison).unwrap(), [pg("int8"), pg("numeric")]);
    let applications = context.operators(comparison).unwrap().unwrap();
    let OperatorResolution::Selected(selected) = applications[0].resolution() else {
        unreachable!()
    };
    assert_eq!(selected.left().map(|left| left.target().name()), Some("numeric"));
}

#[test]
fn an_empty_scope_binds_no_column() {
    let db = db();
    check(&db, &ExpressionScope::Empty, "a > 1", &[OperandType::Unknown, pg("int4")]);
}

#[test]
fn a_trigger_scope_binds_new_and_old_and_finds_a_bare_column_ambiguous() {
    let db = db();
    let scope = || ExpressionScope::Trigger(table(&db, "t"));
    check(&db, &scope(), "NEW.a > OLD.b", &[pg("int4"), pg("numeric")]);
    check(&db, &scope(), "t.a > 1", &[OperandType::Unknown, pg("int4")]);
    let error = refused(&db, &scope(), "a > 1");
    assert!(matches!(error, LookupError::AmbiguousTableLookup { .. }), "{error:?}");
}

#[test]
fn a_rule_scope_reads_a_bare_column_by_its_event() {
    let db = db();
    let scope = |event| ExpressionScope::Rule { table: table(&db, "t"), event };
    check(&db, &scope(CreateRuleEvent::Insert), "a > new.b", &[pg("int4"), pg("numeric")]);
    check(&db, &scope(CreateRuleEvent::Delete), "a > old.b", &[pg("int4"), pg("numeric")]);
    check(&db, &scope(CreateRuleEvent::Delete), "new.a > 1", &[OperandType::Unknown, pg("int4")]);
    check(&db, &scope(CreateRuleEvent::Insert), "old.a > 1", &[OperandType::Unknown, pg("int4")]);
    check(&db, &scope(CreateRuleEvent::Update), "new.a > old.b", &[pg("int4"), pg("numeric")]);
    let error = refused(&db, &scope(CreateRuleEvent::Update), "a > 1");
    assert!(matches!(error, LookupError::AmbiguousTableLookup { .. }), "{error:?}");
}

#[test]
fn a_rule_action_reads_its_own_relations_before_new_and_old() {
    let db = db();
    let action = statement("UPDATE logt SET a = new.a WHERE logt.a = a");
    let context = ExpressionContext::for_statement(
        &action,
        &ExpressionScope::Rule { table: table(&db, "t"), event: CreateRuleEvent::Insert },
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Update(update) = &action else { unreachable!() };
    let Assignment { value, .. } = &update.assignments[0];
    assert_eq!(context.type_of(value).unwrap(), pg("int4"));
    assert_eq!(
        both(&context, update.selection.as_ref().unwrap()).unwrap(),
        [pg("int8"), pg("int8")]
    );
}

#[test]
fn a_domain_scope_types_value_as_the_base_type() {
    let db = db();
    let base = DataType::SmallInt(None);
    check(&db, &ExpressionScope::DomainValue(&base), "VALUE > 1", &[pg("int2"), pg("int4")]);
}

#[test]
fn a_function_body_binds_parameters_one_level_out_from_its_relations() {
    let db = db();
    let scope = || ExpressionScope::FunctionBody(function(&db));
    check(&db, &scope(), "x > $2", &[pg("int8"), pg("int2")]);
    check(&db, &scope(), "f.x > $1", &[pg("int8"), pg("int8")]);

    // A column of the same name wins over the parameter, and a parameter
    // reaches a query with no such column.
    for (sql, expected) in [
        ("SELECT x > 1 FROM (SELECT 1::int4 AS x) AS s", [pg("int4"), pg("int4")]),
        ("SELECT a > x FROM t", [pg("int4"), pg("int8")]),
    ] {
        let body = statement(sql);
        let context =
            ExpressionContext::for_statement(&body, &scope(), &db, IdentifierCase::AsWritten)
                .unwrap();
        let Statement::Query(query) = &body else { unreachable!() };
        let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
        let SelectItem::UnnamedExpr(projected) = &select.projection[0] else { unreachable!() };
        assert_eq!(both(&context, projected).unwrap(), expected, "{sql}");
    }
}

#[test]
fn data_modifying_statements_bind_their_target_and_their_other_relations() {
    let db = db();
    let delete = statement("DELETE FROM t USING logt WHERE t.a > logt.a RETURNING b > t.a");
    let context = ExpressionContext::for_statement(
        &delete,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Delete(Delete { selection, returning, .. }) = &delete else { unreachable!() };
    assert_eq!(both(&context, selection.as_ref().unwrap()).unwrap(), [pg("int4"), pg("int8")]);
    let SelectItem::UnnamedExpr(returned) = &returning.as_ref().unwrap()[0] else { unreachable!() };
    assert_eq!(both(&context, returned).unwrap(), [pg("numeric"), pg("int4")]);

    let insert = statement("INSERT INTO logt SELECT a FROM t WHERE a > b RETURNING a > 1");
    let context = ExpressionContext::for_statement(
        &insert,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Insert(Insert { source, returning, .. }) = &insert else { unreachable!() };
    let SetExpr::Select(select) = source.as_ref().unwrap().body.as_ref() else { unreachable!() };
    assert_eq!(
        both(&context, select.selection.as_ref().unwrap()).unwrap(),
        [pg("int4"), pg("numeric")]
    );
    let SelectItem::UnnamedExpr(returned) = &returning.as_ref().unwrap()[0] else { unreachable!() };
    assert_eq!(both(&context, returned).unwrap(), [pg("int8"), pg("int4")]);
}

#[test]
fn update_from_and_on_conflict_bind_their_own_relations() {
    let db = db();
    let update = statement(
        "UPDATE t SET b = l.a FROM logt AS l WHERE t.a > l.a AND EXISTS (SELECT 1 WHERE l.a > b)",
    );
    let context = ExpressionContext::for_statement(
        &update,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Update(Update { assignments, selection, .. }) = &update else { unreachable!() };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), pg("int8"));
    let (compared, exists) = operands(selection.as_ref().unwrap());
    assert_eq!(both(&context, compared).unwrap(), [pg("int4"), pg("int8")]);
    let Expr::Exists { subquery, .. } = exists else { unreachable!() };
    let SetExpr::Select(select) = subquery.body.as_ref() else { unreachable!() };
    assert_eq!(
        both(&context, select.selection.as_ref().unwrap()).unwrap(),
        [pg("int8"), pg("numeric")]
    );

    let insert = statement(
        "INSERT INTO t AS target VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = excluded.b \
         WHERE target.a > excluded.a RETURNING target.b > 1",
    );
    let context = ExpressionContext::for_statement(
        &insert,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Insert(Insert { on: Some(OnInsert::OnConflict(conflict)), returning, .. }) =
        &insert
    else {
        unreachable!()
    };
    let OnConflictAction::DoUpdate(DoUpdate { assignments, selection }) = &conflict.action else {
        unreachable!()
    };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), pg("numeric"));
    assert_eq!(both(&context, selection.as_ref().unwrap()).unwrap(), [pg("int4"), pg("int4")]);
    let SelectItem::UnnamedExpr(returned) = &returning.as_ref().unwrap()[0] else { unreachable!() };
    assert_eq!(both(&context, returned).unwrap(), [pg("numeric"), pg("int4")]);

    // A statement that is no query and modifies no data binds nothing.
    let notify = statement("NOTIFY t");
    ExpressionContext::for_statement(
        &notify,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
}

#[test]
fn joined_relations_and_unknown_insert_targets_bind_as_postgres_reads_them() {
    let db = db();
    let delete = statement(
        "DELETE FROM t USING logt JOIN logt AS l2 ON l2.a = logt.a CROSS JOIN logt AS l3 \
         WHERE t.b > l3.a",
    );
    let context = ExpressionContext::for_statement(
        &delete,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Delete(Delete { selection, using, .. }) = &delete else { unreachable!() };
    assert_eq!(both(&context, selection.as_ref().unwrap()).unwrap(), [pg("numeric"), pg("int8")]);
    let sqlparser::ast::JoinOperator::Join(sqlparser::ast::JoinConstraint::On(on)) =
        &using.as_ref().unwrap()[0].joins[0].join_operator
    else {
        unreachable!()
    };
    assert_eq!(both(&context, on).unwrap(), [pg("int8"), pg("int8")]);

    // `DEFAULT VALUES` reads no query, and a target the catalog does not
    // hold binds nothing its `RETURNING` names.
    for sql in [
        "INSERT INTO t DEFAULT VALUES RETURNING a > 1",
        "INSERT INTO missing VALUES (1) RETURNING a > 1",
    ] {
        let insert = statement(sql);
        let context = ExpressionContext::for_statement(
            &insert,
            &ExpressionScope::Empty,
            &db,
            IdentifierCase::AsWritten,
        )
        .unwrap();
        let Statement::Insert(Insert { returning, .. }) = &insert else { unreachable!() };
        let SelectItem::UnnamedExpr(returned) = &returning.as_ref().unwrap()[0] else {
            unreachable!()
        };
        let expected = if sql.contains("missing") { OperandType::Unknown } else { pg("int4") };
        assert_eq!(both(&context, returned).unwrap(), [expected, pg("int4")], "{sql}");
    }

    // A join carrying no constraint, as `CROSS APPLY`, still counts as an
    // entry before the relations after it.
    let applied = Parser::parse_sql(
        &sqlparser::dialect::MsSqlDialect {},
        "SELECT t.a FROM t CROSS APPLY (SELECT 1 AS x) AS s",
    )
    .unwrap()
    .remove(0);
    let context = ExpressionContext::for_statement(
        &applied,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let Statement::Query(query) = &applied else { unreachable!() };
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    let SelectItem::UnnamedExpr(projected) = &select.projection[0] else { unreachable!() };
    assert_eq!(context.type_of(projected).unwrap(), pg("int4"));

    // A ClickHouse table function target is no relation of the catalog.
    let insert = Parser::parse_sql(
        &sqlparser::dialect::ClickHouseDialect {},
        "INSERT INTO TABLE FUNCTION remote('localhost', default.t) VALUES (1)",
    )
    .unwrap()
    .remove(0);
    ExpressionContext::for_statement(
        &insert,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
}

#[test]
fn a_node_outside_the_root_is_refused() {
    let db = db();
    let root = expression("a > 1");
    let context = ExpressionContext::for_expression(
        &root,
        &ExpressionScope::Table(table(&db, "t")),
        &db,
        IdentifierCase::AsWritten,
    )
    .unwrap();
    let error = context.type_of(&expression("a > 1")).unwrap_err();
    assert!(matches!(error, LookupError::ExpressionNotInQuery { .. }), "{error:?}");
}

/// The statement a query body holds.
fn body_statement(query: &Query) -> &Statement {
    match query.body.as_ref() {
        SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Insert(statement)
        | SetExpr::Merge(statement) => statement,
        other => panic!("expected a data-modifying body, got {other:?}"),
    }
}

/// The expressions a query's outermost `SELECT` projects.
fn projected(query: &Query) -> Vec<&Expr> {
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    select
        .projection
        .iter()
        .map(|item| {
            let SelectItem::UnnamedExpr(expression) = item else { unreachable!() };
            expression
        })
        .collect()
}

/// The expressions a `RETURNING` list returns.
fn returned(items: &[SelectItem]) -> Vec<&Expr> {
    items
        .iter()
        .map(|item| {
            let SelectItem::UnnamedExpr(expression) = item else { unreachable!() };
            expression
        })
        .collect()
}

fn context_of<'q, 'db>(
    root: &'q Statement,
    scope: &ExpressionScope<'_, 'db, ParserDB>,
    db: &'db ParserDB,
) -> ExpressionContext<'q, 'db, ParserDB> {
    ExpressionContext::for_statement(root, scope, db, IdentifierCase::AsWritten).unwrap()
}

#[test]
fn a_with_prefixed_statement_reads_its_ctes_and_its_own_relations() {
    let db = db();
    let rule = ExpressionScope::Rule { table: table(&db, "t"), event: CreateRuleEvent::Insert };
    let root = statement(
        "WITH c AS (SELECT 1::int8 AS x) UPDATE logt SET a = c.x FROM c \
         WHERE logt.a > c.x AND logt.a = new.a AND EXISTS (SELECT 1 FROM c AS d WHERE d.x > new.b)",
    );
    let context = context_of(&root, &rule, &db);
    let Statement::Query(query) = &root else { unreachable!() };
    let Statement::Update(Update { assignments, selection, .. }) = body_statement(query) else {
        unreachable!()
    };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), pg("int8"));
    let (conditions, exists) = operands(selection.as_ref().unwrap());
    let (compared, joined) = operands(conditions);
    assert_eq!(both(&context, compared).unwrap(), [pg("int8"), pg("int8")]);
    assert_eq!(both(&context, joined).unwrap(), [pg("int8"), pg("int4")]);
    let Expr::Exists { subquery, .. } = exists else { unreachable!() };
    let SetExpr::Select(select) = subquery.body.as_ref() else { unreachable!() };
    assert_eq!(
        both(&context, select.selection.as_ref().unwrap()).unwrap(),
        [pg("int8"), pg("numeric")]
    );

    let root = statement(
        "WITH c AS (SELECT 1::int8 AS x) UPDATE t SET a = c.x FROM c WHERE t.a > c.x RETURNING t.a",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Query(query) = &root else { unreachable!() };
    let Statement::Update(Update { returning, .. }) = body_statement(query) else { unreachable!() };
    let returned = returned(returning.as_ref().unwrap());
    assert_eq!(context.type_of(returned[0]).unwrap(), pg("int4"));
}

#[test]
fn a_data_modifying_cte_outputs_its_returning_list() {
    let db = db();
    let root = statement(
        "WITH u AS (UPDATE t SET a = 1 WHERE b > 0 RETURNING a, b), \
              i AS (INSERT INTO t VALUES (1, 2) RETURNING *), \
              d AS (DELETE FROM logt WHERE a > 0) \
         SELECT u.a, u.b, i.b, i.a FROM u, i",
    );
    let Statement::Query(query) = &root else { unreachable!() };
    for context in [
        context_of(&root, &ExpressionScope::Empty, &db),
        ExpressionContext::new(query, &db, IdentifierCase::AsWritten).unwrap(),
    ] {
        let types: Vec<_> =
            projected(query).into_iter().map(|node| context.type_of(node).unwrap()).collect();
        assert_eq!(types, [pg("int4"), pg("numeric"), pg("numeric"), pg("int4")]);
        let ctes = &query.with.as_ref().unwrap().cte_tables;
        let [Cte { query: updating, .. }, _, Cte { query: deleting, .. }] = ctes.as_slice() else {
            unreachable!()
        };
        let Statement::Update(Update { selection, .. }) = body_statement(updating) else {
            unreachable!()
        };
        assert_eq!(
            both(&context, selection.as_ref().unwrap()).unwrap(),
            [pg("numeric"), pg("int4")]
        );
        let Statement::Delete(Delete { selection, .. }) = body_statement(deleting) else {
            unreachable!()
        };
        assert_eq!(both(&context, selection.as_ref().unwrap()).unwrap(), [pg("int8"), pg("int4")]);
    }
}

#[test]
fn merge_clauses_read_the_side_their_kind_names() {
    let db = db();
    let root = statement(
        "MERGE INTO t USING logt AS s ON t.a = s.a \
         WHEN MATCHED AND t.b > s.a THEN UPDATE SET b = s.a \
         WHEN NOT MATCHED AND a > 0 THEN INSERT VALUES (a, 0) \
         WHEN NOT MATCHED BY SOURCE AND a > 1 THEN DELETE \
         RETURNING s.a, b",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Merge(Merge { on, clauses, output, .. }) = &root else { unreachable!() };
    assert_eq!(both(&context, on).unwrap(), [pg("int4"), pg("int8")]);
    let [matched, not_matched, by_source] = clauses.as_slice() else { unreachable!() };
    let MergeClause { predicate, action, .. } = matched;
    assert_eq!(both(&context, predicate.as_ref().unwrap()).unwrap(), [pg("numeric"), pg("int8")]);
    let MergeAction::Update(MergeUpdateExpr { kind: MergeUpdateKind::Set(assignments), .. }) =
        action
    else {
        unreachable!()
    };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), pg("int8"));
    let MergeClause { predicate, action, .. } = not_matched;
    assert_eq!(both(&context, predicate.as_ref().unwrap()).unwrap(), [pg("int8"), pg("int4")]);
    let MergeAction::Insert(MergeInsertExpr { kind: MergeInsertKind::Values(values), .. }) = action
    else {
        unreachable!()
    };
    assert_eq!(context.type_of(&values.rows[0][0]).unwrap(), pg("int8"));
    let MergeClause { predicate, .. } = by_source;
    assert_eq!(both(&context, predicate.as_ref().unwrap()).unwrap(), [pg("int4"), pg("int4")]);
    let Some(OutputClause::Returning { select_items, .. }) = output else { unreachable!() };
    let returned = returned(select_items);
    assert_eq!(context.type_of(returned[0]).unwrap(), pg("int8"));
    assert_eq!(context.type_of(returned[1]).unwrap(), pg("numeric"));

    // A name both sides hold is ambiguous where both are in reach.
    let root = statement("MERGE INTO t USING logt AS s ON a > 0 WHEN MATCHED THEN DELETE");
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Merge(Merge { on, .. }) = &root else { unreachable!() };
    let error = both(&context, on).unwrap_err();
    assert!(matches!(error, LookupError::AmbiguousTableLookup { .. }), "{error:?}");

    // `RETURNING *` lists the source's columns before the target's.
    let root = statement(
        "WITH m AS (MERGE INTO t USING logt AS s ON t.a = s.a WHEN MATCHED THEN DELETE \
         RETURNING *) SELECT m.b FROM m",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Query(query) = &root else { unreachable!() };
    assert_eq!(context.type_of(projected(query)[0]).unwrap(), pg("numeric"));
}

/// A `MERGE` source written as a parenthesized join exposes its items, after
/// which the target stands, and under an alias only the join's own columns.
#[test]
fn a_merge_source_join_exposes_its_items() {
    let db = db();
    let root = statement(
        "MERGE INTO t USING (logt AS s JOIN logt AS u ON s.a = u.a) ON t.a = u.a \
         WHEN MATCHED AND t.b > u.a THEN UPDATE SET b = u.a \
         RETURNING u.a, t.b, s.a",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Merge(Merge { on, clauses, output, .. }) = &root else { unreachable!() };
    assert_eq!(both(&context, on).unwrap(), [pg("int4"), pg("int8")]);
    let MergeClause { predicate, action, .. } = &clauses[0];
    assert_eq!(both(&context, predicate.as_ref().unwrap()).unwrap(), [pg("numeric"), pg("int8")]);
    let MergeAction::Update(MergeUpdateExpr { kind: MergeUpdateKind::Set(assignments), .. }) =
        action
    else {
        unreachable!()
    };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), pg("int8"));
    let Some(OutputClause::Returning { select_items, .. }) = output else { unreachable!() };
    let returned: Vec<_> =
        returned(select_items).into_iter().map(|node| context.type_of(node).unwrap()).collect();
    assert_eq!(returned, [pg("int8"), pg("numeric"), pg("int8")]);

    let root = statement(
        "MERGE INTO t USING (logt AS s JOIN logt AS u ON s.a = u.a) AS j ON t.a = u.a \
         WHEN MATCHED THEN DELETE",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Merge(Merge { on, .. }) = &root else { unreachable!() };
    assert_eq!(
        both(&context, on).unwrap(),
        [pg("int4"), OperandType::Unknown],
        "the alias hides the names inside the join"
    );
}

#[test]
fn returning_reads_old_and_new_unless_a_relation_takes_the_name() {
    let db = db();
    for (sql, scope, expected) in [
        (
            "UPDATE t SET a = 1 RETURNING old.a, new.b",
            ExpressionScope::Empty,
            [pg("int4"), pg("numeric")],
        ),
        (
            "UPDATE t SET a = 1 FROM logt AS old RETURNING old.a, new.a",
            ExpressionScope::Empty,
            [pg("int8"), pg("int4")],
        ),
        (
            "UPDATE logt SET a = 1 RETURNING old.a, new.a",
            ExpressionScope::Rule { table: table(&db, "t"), event: CreateRuleEvent::Update },
            [pg("int4"), pg("int4")],
        ),
    ] {
        let root = statement(sql);
        let context = context_of(&root, &scope, &db);
        let Statement::Update(Update { returning, .. }) = &root else { unreachable!() };
        let types: Vec<_> = returned(returning.as_ref().unwrap())
            .into_iter()
            .map(|node| context.type_of(node).unwrap())
            .collect();
        assert_eq!(types, expected, "{sql}");
    }

    let root = statement("INSERT INTO logt VALUES (1) RETURNING old.a, new.a");
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Insert(Insert { returning, .. }) = &root else { unreachable!() };
    let types: Vec<_> = returned(returning.as_ref().unwrap())
        .into_iter()
        .map(|node| context.type_of(node).unwrap())
        .collect();
    assert_eq!(types, [pg("int8"), pg("int8")]);
}

#[test]
fn a_target_outside_the_catalog_lends_no_rows() {
    let db = db();
    let root = statement(
        "INSERT INTO missing VALUES (1) ON CONFLICT (a) DO UPDATE SET b = excluded.b WHERE b > 1",
    );
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Insert(Insert { on: Some(OnInsert::OnConflict(conflict)), .. }) = &root else {
        unreachable!()
    };
    let OnConflictAction::DoUpdate(DoUpdate { assignments, .. }) = &conflict.action else {
        unreachable!()
    };
    assert_eq!(context.type_of(&assignments[0].value).unwrap(), OperandType::Unknown);

    let root = statement("UPDATE (SELECT 1::int4 AS a) AS s SET a = 1 RETURNING old.a, s.a");
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Update(Update { returning, .. }) = &root else { unreachable!() };
    let types: Vec<_> = returned(returning.as_ref().unwrap())
        .into_iter()
        .map(|node| context.type_of(node).unwrap())
        .collect();
    assert_eq!(types, [OperandType::Unknown, pg("int4")]);
}

#[test]
fn a_merge_update_of_every_column_still_reads_its_condition() {
    let db = db();
    let root = Parser::parse_sql(
        &sqlparser::dialect::DatabricksDialect {},
        "MERGE INTO t USING logt AS s ON t.a = s.a WHEN MATCHED AND t.b > s.a THEN UPDATE SET *",
    )
    .unwrap()
    .remove(0);
    let context = context_of(&root, &ExpressionScope::Empty, &db);
    let Statement::Merge(Merge { clauses, .. }) = &root else { unreachable!() };
    let predicate = clauses[0].predicate.as_ref().unwrap();
    assert_eq!(both(&context, predicate).unwrap(), [pg("numeric"), pg("int8")]);
}
