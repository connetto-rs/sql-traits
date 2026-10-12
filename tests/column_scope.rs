//! Integration tests for the public column-reference scope resolver
//! (`ColumnScope`), exercised through the prelude exactly as a downstream
//! consumer (pg2sqlite) would. Ports the evidence cases from pg2sqlite's
//! audit remediation plan phase 3: two tables declaring one column name, a
//! bare reference over a join, qualified references reading their own
//! table's column, quoted qualifiers, pass-through resolution through CTE
//! references and derived subqueries, opaque relations poisoning a bare
//! reference, alias resolution, and the single-table scope for definitions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{Expr, Query, SelectItem, SetExpr, Statement},
    dialect::GenericDialect,
    parser::Parser,
};

const SCHEMA: &str = "
    CREATE TABLE a(payload TEXT, id INT);
    CREATE TABLE b(payload JSON, id INT);
";

fn schema_db() -> ParserDB {
    ParserDB::parse::<GenericDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("statement parses");
    match statements.pop().expect("one statement") {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

/// Resolves the query's single projected expression against a scope built
/// from the same query, rendering the resolved table's name.
fn resolve_projection(sql: &str, db: &ParserDB) -> Result<Option<String>, LookupError> {
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, db, IdentifierCase::AsWritten)?;
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(expr) = &select.projection.first().expect("one projection") else {
        panic!("expected an expression projection")
    };
    Ok(scope.resolve_column(expr)?.map(|table| table.table_name().to_string()))
}

/// Parses a bare column reference as the expression it would appear as in a
/// projection. The `FROM` relation is never resolved against a database.
fn reference(reference: &str) -> Expr {
    let query = query(&format!("SELECT {reference} FROM placeholder"));
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    match &select.projection.first().expect("one projection") {
        SelectItem::UnnamedExpr(expr) => (*expr).clone(),
        other => panic!("expected an expression projection, got {other:?}"),
    }
}

// Evidence: `SELECT to_json(a.payload) FROM a` must resolve to `a` even when
// another table declares `payload` with a different type.
#[test]
fn qualified_reference_reads_its_own_table() {
    let db = schema_db();
    let resolved = resolve_projection("SELECT a.payload FROM a", &db).expect("scope resolves");
    assert_eq!(resolved.as_deref(), Some("a"));
}

#[test]
fn qualified_reference_over_a_join_reads_its_own_table() {
    let db = schema_db();
    let resolved =
        resolve_projection("SELECT a.payload FROM a JOIN b ON a.id = b.id", &db).expect("resolves");
    assert_eq!(resolved.as_deref(), Some("a"));
}

// Measured on PostgreSQL 18.6: `column reference "payload" is ambiguous`.
#[test]
fn bare_reference_exposed_twice_is_ambiguous() {
    let db = schema_db();
    let result = resolve_projection("SELECT payload FROM a JOIN b ON a.id = b.id", &db);
    assert!(matches!(result, Err(LookupError::AmbiguousTableLookup { .. })), "got {result:?}");
}

#[test]
fn bare_reference_over_one_table_resolves() {
    let db = schema_db();
    let resolved = resolve_projection("SELECT payload FROM b", &db).expect("resolves");
    assert_eq!(resolved.as_deref(), Some("b"));
}

#[test]
fn alias_qualifier_resolves_to_the_aliased_table() {
    let db = schema_db();
    let resolved = resolve_projection("SELECT x.payload FROM a AS x", &db).expect("resolves");
    assert_eq!(resolved.as_deref(), Some("a"));
}

// Measured on PostgreSQL 18.6: a quoted alias answers only its exact
// spelling. `a.payload` against alias `"A"` errors there; the scope answers
// nothing, the contract's conservative form of the same miss.
#[test]
fn quoted_alias_qualifier_is_quote_aware() {
    let db = schema_db();
    let quoted = resolve_projection("SELECT \"A\".payload FROM a AS \"A\"", &db).expect("resolves");
    assert_eq!(quoted.as_deref(), Some("a"));
    let folded = resolve_projection("SELECT A.payload FROM a AS A", &db).expect("resolves");
    assert_eq!(folded.as_deref(), Some("a"));
    let missed = resolve_projection("SELECT a.payload FROM a AS \"A\"", &db).expect("resolves");
    assert_eq!(missed, None);
}

// A derivable relation (a CTE reference or a derived subquery whose
// projection the resolver enumerates) carries its pass-through columns: a
// qualified reference resolves to the base table the projection passes the
// column through from, and knowing the relation's exact column set lets a
// bare reference resolve too (pass-through resolution and the bare form
// measured on PostgreSQL 18.4). A computed output column answers nothing,
// and only a relation whose columns cannot be enumerated poisons a bare
// reference.
#[test]
fn derived_subquery_passes_columns_through_and_no_longer_poisons() {
    let db = schema_db();
    let bare = resolve_projection(
        "SELECT payload FROM (SELECT 1 AS other) s JOIN b ON s.other = b.id",
        &db,
    )
    .expect("resolves");
    assert_eq!(bare.as_deref(), Some("b"));
    let qualified =
        resolve_projection("SELECT s.copy FROM (SELECT b.payload AS copy FROM b) s", &db)
            .expect("resolves");
    assert_eq!(qualified.as_deref(), Some("b"));
    let bare_through =
        resolve_projection("SELECT copy FROM (SELECT b.payload AS copy FROM b) s", &db)
            .expect("resolves");
    assert_eq!(bare_through.as_deref(), Some("b"));
}

#[test]
fn cte_reference_resolves_through_its_projection() {
    let db = schema_db();
    let qualified =
        resolve_projection("WITH t AS (SELECT payload FROM b) SELECT t.payload FROM t", &db)
            .expect("resolves");
    assert_eq!(qualified.as_deref(), Some("b"));
    let bare = resolve_projection("WITH t AS (SELECT payload FROM b) SELECT payload FROM t", &db)
        .expect("resolves");
    assert_eq!(bare.as_deref(), Some("b"));
}

#[test]
fn unnamed_computed_projection_still_poisons_a_bare_reference() {
    let db = schema_db();
    // `count(*)` written without an alias has no output name this resolver
    // models, so the subquery's columns stay unknown.
    let bare =
        resolve_projection("SELECT payload FROM (SELECT count(*) FROM b) s JOIN b ON true", &db)
            .expect("resolves");
    assert_eq!(bare, None);
    let qualified =
        resolve_projection("SELECT b.payload FROM (SELECT count(*) FROM b) s JOIN b ON true", &db)
            .expect("resolves");
    assert_eq!(qualified.as_deref(), Some("b"));
    let cte_bare = resolve_projection(
        "WITH t AS (SELECT count(*) FROM b) SELECT payload FROM t JOIN b ON true",
        &db,
    )
    .expect("resolves");
    assert_eq!(cte_bare, None);
}

#[test]
fn computed_output_column_answers_nothing() {
    let db = schema_db();
    let qualified =
        resolve_projection("WITH v AS (SELECT count(*) AS n FROM b) SELECT v.n FROM v", &db)
            .expect("resolves");
    assert_eq!(qualified, None);
    let bare = resolve_projection("WITH v AS (SELECT count(*) AS n FROM b) SELECT n FROM v", &db)
        .expect("resolves");
    assert_eq!(bare, None);
}

#[test]
fn table_function_stays_opaque() {
    let db = schema_db();
    // A table-valued function's columns are not knowable from the SQL text.
    let bare =
        resolve_projection("SELECT payload FROM generate_series(1, 10) g JOIN b ON true", &db)
            .expect("resolves");
    assert_eq!(bare, None);
    let qualified =
        resolve_projection("SELECT b.payload FROM generate_series(1, 10) g JOIN b ON true", &db)
            .expect("resolves");
    assert_eq!(qualified.as_deref(), Some("b"));
}

#[test]
fn unresolvable_relation_name_still_poisons_a_bare_reference() {
    let db = schema_db();
    let bare = resolve_projection("SELECT payload FROM does_not_exist JOIN b ON true", &db)
        .expect("resolves");
    assert_eq!(bare, None);
}

#[test]
fn set_operation_arms_agreeing_on_a_source_resolve() {
    let db = schema_db();
    let agreeing = resolve_projection(
        "WITH v AS (SELECT payload FROM b UNION ALL SELECT payload FROM b) SELECT v.payload FROM v",
        &db,
    )
    .expect("resolves");
    assert_eq!(agreeing.as_deref(), Some("b"));
    // Arms naming different tables answer nothing, mirroring how an
    // ambiguous bare reference is refused rather than guessed.
    let disagreeing = resolve_projection(
        "WITH v AS (SELECT payload FROM a UNION ALL SELECT payload FROM b) SELECT v.payload FROM v",
        &db,
    )
    .expect("resolves");
    assert_eq!(disagreeing, None);
}

#[test]
fn computed_recursive_arm_keeps_other_column_sources() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT, parent_id INT);")
        .expect("schema parses");
    let body = "WITH RECURSIVE tree AS (\
        SELECT id, 0 AS depth FROM categories \
        UNION ALL \
        SELECT c.id, t.depth + 1 FROM categories c \
        JOIN tree t ON c.parent_id = t.id\
    )";
    assert_eq!(
        resolve_projection(&format!("{body} SELECT t.id FROM tree t"), &db)
            .expect("resolves")
            .as_deref(),
        Some("categories")
    );
    assert_eq!(
        resolve_projection(&format!("{body} SELECT t.depth FROM tree t"), &db).expect("resolves"),
        None
    );
}

#[test]
fn computed_nested_set_arms_keep_other_column_sources() {
    let db = schema_db();
    let body = "WITH v AS (\
        SELECT id, payload FROM a \
        UNION ALL (\
            SELECT id, count(*) FROM a \
            UNION ALL SELECT id, count(*) FROM a\
        )\
    )";
    assert_eq!(
        resolve_projection(&format!("{body} SELECT v.id FROM v"), &db)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        resolve_projection(&format!("{body} SELECT v.payload FROM v"), &db).expect("resolves"),
        None
    );
}

#[test]
fn alias_lists_name_computed_left_set_arm_outputs() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT);
         CREATE TABLE categories(id INT, parent_id INT);
         CREATE VIEW v(value, key) AS
             SELECT 0, id FROM a UNION ALL SELECT 1, id FROM a;",
    )
    .expect("schema parses");
    for (sql, expected) in [
        (
            "WITH c(value, key) AS (\
                SELECT 0, id FROM a UNION ALL SELECT 1, id FROM a\
             ) SELECT c.key FROM c",
            "a",
        ),
        (
            "SELECT s.key FROM (\
                SELECT 0, id FROM a UNION ALL SELECT 1, id FROM a\
             ) s(value, key)",
            "a",
        ),
        ("SELECT v.key FROM v", "a"),
        (
            "WITH RECURSIVE tree(node, level) AS (\
                SELECT id, 0 FROM categories \
                UNION ALL \
                SELECT c.id, t.level + 1 FROM categories c \
                JOIN tree t ON c.parent_id = t.node\
             ) SELECT t.node FROM tree t",
            "categories",
        ),
    ] {
        assert_eq!(
            resolve_projection(sql, &db).expect("resolves").as_deref(),
            Some(expected),
            "{sql}"
        );
    }
}

#[test]
fn wildcard_cte_and_alias_lists_resolve() {
    let db = schema_db();
    let wildcard = resolve_projection("WITH v AS (SELECT * FROM b) SELECT v.payload FROM v", &db)
        .expect("resolves");
    assert_eq!(wildcard.as_deref(), Some("b"));
    let aliased = resolve_projection("WITH v(x) AS (SELECT payload FROM b) SELECT v.x FROM v", &db)
        .expect("resolves");
    assert_eq!(aliased.as_deref(), Some("b"));
    let subquery_alias =
        resolve_projection("SELECT s.y FROM (SELECT payload FROM b) s(y)", &db).expect("resolves");
    assert_eq!(subquery_alias.as_deref(), Some("b"));
}

#[test]
fn cte_shadows_a_base_table_of_the_same_name() {
    let db = schema_db();
    // `a` here is the CTE over `b`, not the base table `a`, so the reference
    // resolves to `b`.
    let resolved =
        resolve_projection("WITH a AS (SELECT payload FROM b) SELECT a.payload FROM a", &db)
            .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("b"));
}

#[test]
fn bare_name_exposed_by_a_cte_and_a_base_table_is_ambiguous() {
    // Measured on PostgreSQL 18.4: `column reference "id" is ambiguous`.
    let db = schema_db();
    let result = resolve_projection(
        "WITH v AS (SELECT id FROM a) SELECT id FROM v JOIN b ON v.id = b.id",
        &db,
    );
    assert!(matches!(result, Err(LookupError::AmbiguousTableLookup { .. })), "got {result:?}");
}

#[test]
fn recursive_cte_stops_at_its_self_reference() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE groups(id INT, parent_group_id INT);")
        .expect("schema parses");
    // The shape pg2sqlite reported. Building the outer scope must terminate
    // at the recursive arm's self-reference and resolve the pass-through
    // `id` to `groups`.
    let resolved = resolve_projection(
        "WITH RECURSIVE child_groups AS (\
           SELECT id FROM groups WHERE parent_group_id = 1 \
           UNION ALL \
           SELECT g.id FROM groups g JOIN child_groups cg ON g.parent_group_id = cg.id\
         ) SELECT id FROM child_groups",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("groups"));
}

#[test]
fn recursive_arm_qualifies_through_the_cte_alias() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE groups(id INT, parent_group_id INT);")
        .expect("schema parses");
    // pg2sqlite builds a per-arm scope with the statement's `WITH` attached
    // (each arm gets a query of `cte_clause` plus its own `SELECT`). Inside
    // the second arm, `cg.id` names the CTE aliased as `cg`, whose
    // projection passes `id` through from `groups`.
    let full = query(
        "WITH RECURSIVE child_groups AS (\
           SELECT id FROM groups WHERE parent_group_id = 1 \
           UNION ALL \
           SELECT g.id FROM groups g JOIN child_groups cg ON g.parent_group_id = cg.id\
         ) SELECT id FROM child_groups",
    );
    let with = full.with.as_ref().expect("the statement has a WITH clause");
    let cte = with.cte_tables.first().expect("one CTE");
    let SetExpr::SetOperation { right, .. } = cte.query.body.as_ref() else {
        panic!("expected a set operation CTE body")
    };
    let mut arm = (*cte.query).clone();
    arm.with = full.with.clone();
    arm.body = right.clone();
    let scope =
        ColumnScope::from_query(&arm, &db, IdentifierCase::AsWritten).expect("scope builds");
    let resolved =
        scope.resolve_column(&reference("cg.id")).expect("resolves").expect("cg.id resolves");
    assert_eq!(resolved.table_name(), "groups");
}

#[test]
fn recursive_cte_seeds_self_reference_from_anchor() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT);")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE tree AS (\
           SELECT id FROM categories \
           UNION ALL \
           SELECT t.id FROM tree t WHERE false\
         ) SELECT t.id FROM tree t",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("categories"));
}

#[test]
fn recursive_cte_seeds_parenthesized_body() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT)")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE tree AS ((\
           SELECT id FROM categories \
           UNION ALL \
           SELECT t.id FROM tree t WHERE false\
         )) SELECT t.id FROM tree t",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("categories"));
}

#[test]
fn recursive_cte_alias_columns_seed_self_reference() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT);")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE tree(node) AS (\
           SELECT id FROM categories \
           UNION ALL \
           SELECT t.node FROM tree t WHERE false\
         ) SELECT t.node FROM tree t",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("categories"));
}

#[test]
fn recursive_cte_seeds_from_multiple_anchor_arms() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT);")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE tree AS (\
           (SELECT id FROM categories WHERE id = 1 \
            UNION ALL \
            SELECT id FROM categories WHERE id = 2) \
           UNION ALL \
           SELECT t.id FROM tree t WHERE false\
         ) SELECT t.id FROM tree t",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved.as_deref(), Some("categories"));
}

#[test]
fn mutually_recursive_ctes_stay_opaque() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT)")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE \
         a(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT a.id FROM a JOIN b ON true\
         ), \
         b(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT a.id FROM a\
         ) \
         SELECT a.id FROM a",
        &db,
    )
    .expect("scope builds");
    assert_eq!(resolved, None);
}

#[test]
fn indirect_mutual_recursion_stays_opaque() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT)")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE \
         a(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT a.id FROM a JOIN b ON true\
         ), \
         b(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT b.id FROM b JOIN c ON true\
         ), \
         c(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT c.id FROM c JOIN a ON true\
         ) \
         SELECT a.id FROM a",
        &db,
    )
    .expect("scope builds");
    assert_eq!(resolved, None);
}

#[test]
fn nested_cte_shadow_does_not_create_mutual_recursion() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT)")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE \
         a(id) AS (\
             WITH b AS (SELECT id FROM categories) \
             SELECT id FROM categories \
             UNION ALL \
             SELECT a.id FROM a JOIN b ON true\
         ), \
         b(id) AS (\
             SELECT id FROM categories \
             UNION ALL \
             SELECT b.id FROM b WHERE false\
         ) \
         SELECT a.id FROM a",
        &db,
    )
    .expect("scope builds");
    assert_eq!(resolved.as_deref(), Some("categories"));
}

#[test]
fn recursive_cte_drops_source_when_recursive_arm_computes() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT);")
        .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE tree AS (\
           SELECT id FROM categories \
           UNION ALL \
           SELECT t.id + 1 AS id FROM tree t WHERE false\
         ) SELECT t.id FROM tree t",
        &db,
    )
    .expect("resolves");
    assert_eq!(resolved, None);
}

#[test]
fn anonymous_derived_table_resolves_bare_but_never_qualifies() {
    let db = schema_db();
    // PostgreSQL 16+ accepts an unaliased derived table: the bare reference
    // passes through to `b`, while a qualifier has no key to match, so the
    // qualified reference answers nothing.
    let bare = resolve_projection("SELECT copy FROM (SELECT b.payload AS copy FROM b)", &db)
        .expect("resolves");
    assert_eq!(bare.as_deref(), Some("b"));
    let qualified = resolve_projection("SELECT s.copy FROM (SELECT b.payload AS copy FROM b)", &db)
        .expect("resolves");
    assert_eq!(qualified, None);
}

// The second measured pg2sqlite defect: with only `b` declared, a qualified
// reference to undeclared `a` must answer nothing, so pg2sqlite refuses
// instead of reading `b`'s column type.
#[test]
fn reference_to_an_undeclared_relation_answers_nothing() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE b(payload JSON, id INT);")
        .expect("schema parses");
    let resolved = resolve_projection("SELECT a.payload FROM a", &db).expect("resolves");
    assert_eq!(resolved, None);
}

#[test]
fn qualifier_matching_no_relation_answers_nothing() {
    let db = schema_db();
    let resolved = resolve_projection("SELECT z.payload FROM a AS z2", &db).expect("resolves");
    assert_eq!(resolved, None);
}

#[test]
fn column_exposed_by_no_scoped_table_answers_nothing() {
    let db = schema_db();
    let resolved = resolve_projection("SELECT nope FROM a", &db).expect("resolves");
    assert_eq!(resolved, None);
}

// Measured on PostgreSQL 18.4: FROM a schema-qualified table qualifies the
// reference by the table name, and a three-part reference resolves when the
// leading part names the table's exact schema.
#[test]
fn schema_qualified_from_qualifies_by_table_name() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE SCHEMA s; CREATE TABLE s.a(payload TEXT); CREATE TABLE a(payload JSON);",
    )
    .expect("schema parses");
    let resolved = resolve_projection("SELECT a.payload FROM s.a", &db).expect("resolves");
    assert_eq!(resolved.as_deref(), Some("a"));
    let three_part = resolve_projection("SELECT s.a.payload FROM s.a", &db).expect("resolves");
    assert_eq!(three_part.as_deref(), Some("a"));
    let three_part_query = query("SELECT s.a.payload FROM s.a");
    let scope =
        ColumnScope::from_query(&three_part_query, &db, IdentifierCase::AsWritten).expect("scope");
    assert_eq!(
        scope
            .resolve_column(&reference("s.a.payload"))
            .expect("resolves")
            .map(TableLike::table_name),
        Some("a")
    );
    let table = db
        .table_by_target(
            TargetName::new("a", false).with_schema("s", true),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("schema table is found");
    assert_eq!(table.columns(&db).expect("columns resolve").count(), 1);
}

// Construction resolves the FROM relation names, so a malformed name is a
// construction error, matching projection_source_table's behavior.
#[test]
fn malformed_from_name_fails_construction() {
    let db = schema_db();
    let query = query("SELECT a.payload FROM a.b.c");
    let result = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten);
    assert!(matches!(result, Err(LookupError::InvalidObjectName { .. })), "got {:?}", result.err());
}

// A set operation has no single outer FROM, so its scope answers nothing.
#[test]
fn set_operation_body_answers_nothing() {
    let db = schema_db();
    let query = query("SELECT payload FROM a UNION SELECT payload FROM b");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert_eq!(scope.resolve_column(&reference("payload")).expect("resolves"), None);
    assert_eq!(scope.resolve_column(&reference("a.payload")).expect("resolves"), None);
}

// GROUP BY and DISTINCT do not change which relations are in scope.
#[test]
fn scope_survives_group_by_and_distinct() {
    let db = schema_db();
    let grouped = resolve_projection("SELECT payload FROM a GROUP BY payload", &db).expect("ok");
    assert_eq!(grouped.as_deref(), Some("a"));
    let distinct = resolve_projection("SELECT DISTINCT payload FROM a", &db).expect("ok");
    assert_eq!(distinct.as_deref(), Some("a"));
}

// Definition contexts (constraint checks, computed columns, index
// expressions, policy conditions, trigger bodies) resolve against the
// defined table's own columns, measured on PostgreSQL 18.6: a CHECK sees
// its own columns bare or qualified by its own name, and a CHECK naming
// another table errors. The single-table scope answers nothing for the
// other table instead of erroring.
#[test]
fn single_table_scope_resolves_the_defined_tables_columns() {
    let db = schema_db();
    let table = db
        .table_by_target(TargetName::new("a", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table a exists");
    let scope = ColumnScope::for_table(table, &db, IdentifierCase::AsWritten);
    assert_eq!(
        scope
            .resolve_column(&reference("payload"))
            .expect("bare resolves")
            .map(TableLike::table_name),
        Some("a")
    );
    assert_eq!(
        scope
            .resolve_column(&reference("a.payload"))
            .expect("qualified resolves")
            .map(TableLike::table_name),
        Some("a")
    );
    assert_eq!(scope.resolve_column(&reference("b.payload")).expect("resolves"), None);
    assert_eq!(scope.resolve_column(&reference("nope")).expect("resolves"), None);
}

#[test]
fn single_table_scope_qualifier_is_quote_aware() {
    let db =
        ParserDB::parse::<GenericDialect>("CREATE TABLE \"Mix\"(v INT);").expect("schema parses");
    let table = db
        .table_by_target(TargetName::new("Mix", true), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("quoted table is found");
    let scope = ColumnScope::for_table(table, &db, IdentifierCase::AsWritten);
    assert_eq!(
        scope
            .resolve_column(&reference("\"Mix\".v"))
            .expect("exact qualifier resolves")
            .map(TableLike::table_name),
        Some("Mix")
    );
    assert_eq!(scope.resolve_column(&reference("mix.v")).expect("resolves"), None);
}

// The scope borrows the query only for the FROM names, so a scope built
// from a parsed query and database can resolve references parsed from
// elsewhere (a policy condition re-parsed from a string, an index
// expression).
#[test]
fn scope_resolves_references_from_outside_the_query() {
    let db = schema_db();
    let query = query("SELECT 1 FROM a JOIN b ON a.id = b.id");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert_eq!(
        scope.resolve_column(&reference("a.payload")).expect("resolves").map(TableLike::table_name),
        Some("a")
    );
    assert_eq!(
        scope.resolve_column(&reference("b.payload")).expect("resolves").map(TableLike::table_name),
        Some("b")
    );
}

// Measured on PostgreSQL 18.4: a `USING` join carries the merged column once
// in its output and the coalesced value belongs to neither table, so the
// scope answers nothing for it, while a third relation exposing the same name
// makes the bare reference ambiguous exactly as PostgreSQL reports it.
#[test]
fn using_merged_column_answers_nothing() {
    let db = schema_db();
    assert_eq!(resolve_projection("SELECT id FROM a JOIN b USING (id)", &db), Ok(None));
    let three = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(payload TEXT, id INT);
         CREATE TABLE b(payload JSON, id INT);
         CREATE TABLE c(id INT);",
    )
    .expect("schema parses");
    let error = resolve_projection("SELECT id FROM a JOIN b USING (id) JOIN c ON true", &three)
        .expect_err("the third exposure is ambiguous");
    assert!(matches!(error, LookupError::AmbiguousTableLookup { .. }));
}

#[test]
fn using_merged_column_over_cte_join_answers_nothing() {
    let db = schema_db();
    assert_eq!(
        resolve_projection("WITH v AS (SELECT id FROM a) SELECT id FROM v JOIN b USING (id)", &db),
        Ok(None)
    );
}

#[test]
fn cte_exposing_merged_column_passes_other_names_through() {
    let db = schema_db();
    assert_eq!(
        resolve_projection(
            "WITH j AS (SELECT id FROM a JOIN b USING (id)) \
             SELECT payload FROM b JOIN j ON true",
            &db
        )
        .expect("resolves")
        .as_deref(),
        Some("b")
    );
}

// Measured on PostgreSQL 18.4: `NATURAL` merges the shared name (the bare
// reference answers nothing here, where PostgreSQL resolves it to the merged
// column), while a qualified reference still reads its own table's column.
#[test]
fn natural_join_merges_shared_name() {
    let db = schema_db();
    assert_eq!(resolve_projection("SELECT payload FROM a NATURAL JOIN b", &db), Ok(None));
    let qualified =
        resolve_projection("SELECT a.payload FROM a NATURAL JOIN b", &db).expect("resolves");
    assert_eq!(qualified.as_deref(), Some("a"));
}

// Measured on PostgreSQL 18.4: a relation whose output exposes one name twice
// answers neither a qualified nor a bare reference to it (`column reference
// ... is ambiguous`).
#[test]
fn derived_relation_exposing_a_name_twice_is_ambiguous() {
    let db = schema_db();
    let qualified = resolve_projection(
        "SELECT v2.payload FROM (SELECT * FROM a JOIN b ON a.id = b.id) v2",
        &db,
    );
    assert!(matches!(qualified, Err(LookupError::AmbiguousTableLookup { .. })));
    let bare =
        resolve_projection("SELECT payload FROM (SELECT * FROM a JOIN b ON a.id = b.id) v2", &db);
    assert!(matches!(bare, Err(LookupError::AmbiguousTableLookup { .. })));
    let aliased = resolve_projection(
        "SELECT s.id FROM (SELECT a.id, b.id FROM a JOIN b ON a.id = b.id) s",
        &db,
    );
    assert!(matches!(aliased, Err(LookupError::AmbiguousTableLookup { .. })));
}

// Measured on PostgreSQL 18.4: inside `WITH RECURSIVE` a sibling name binds
// to the sibling CTE, never to a same-named base table. The scope cannot
// derive a forward sibling's shape, so it answers nothing instead of reading
// the shadowed base table.
#[test]
fn recursive_forward_sibling_shadows_the_base_table() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE shadow(x TEXT); CREATE TABLE other(y TEXT);",
    )
    .expect("schema parses");
    let resolved = resolve_projection(
        "WITH RECURSIVE q AS (SELECT x FROM shadow), shadow AS (SELECT 'c' AS x) \
         SELECT q.x FROM q",
        &db,
    )
    .expect("scope resolves");
    assert_eq!(resolved, None);
}

// Measured on PostgreSQL 18.4: a `schema.table.column` reference resolves
// when the leading part is the base table's own schema (a bare table lives
// in `public`), and never for a CTE (`invalid reference to FROM-clause
// entry`). The database part is not modeled, so four-part references answer
// nothing.
#[test]
fn three_part_qualifier_matches_the_stored_schema() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE b(id INT); CREATE SCHEMA s; CREATE TABLE s.t(id INT);",
    )
    .expect("schema parses");
    assert_eq!(
        resolve_projection("SELECT public.b.id FROM b", &db).expect("resolves").as_deref(),
        Some("b")
    );
    assert_eq!(
        resolve_projection("SELECT s.t.id FROM s.t", &db).expect("resolves").as_deref(),
        Some("t")
    );
    assert_eq!(resolve_projection("SELECT other.b.id FROM b", &db).expect("resolves"), None);
    assert_eq!(
        resolve_projection("SELECT postgres.public.b.id FROM b", &db).expect("resolves"),
        None
    );
    let resolved =
        resolve_projection("WITH g AS (SELECT id FROM b) SELECT public.g.id FROM g", &db)
            .expect("resolves");
    assert_eq!(resolved, None);
}

// A three-part projection inside a CTE body keeps PostgreSQL's label (the
// trailing name) and passes its source through. A body whose reference names
// the wrong schema is a statement PostgreSQL rejects, so the body stays
// opaque and answers nothing.
#[test]
fn three_part_column_in_cte_body_keeps_its_label() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE b(id INT);").expect("schema parses");
    assert_eq!(
        resolve_projection("WITH v AS (SELECT public.b.id FROM b) SELECT v.id FROM v", &db)
            .expect("resolves")
            .as_deref(),
        Some("b")
    );
    let opaque = resolve_projection("WITH v AS (SELECT other.b.id FROM b) SELECT v.id FROM v", &db)
        .expect("resolves");
    assert_eq!(opaque, None);
}

// A three-part reference on the null-extended side of an outer join still
// answers the type question but never row identity.
#[test]
fn three_part_reference_on_null_extended_side_refuses_identity() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")
        .expect("schema parses");
    let query = query("SELECT public.b.id FROM a LEFT JOIN b ON true");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert_eq!(
        scope
            .resolve_column(&reference("public.b.id"))
            .expect("type resolves")
            .map(TableLike::table_name),
        Some("b")
    );
    assert_eq!(query.projection_source_table(&db).expect("identity succeeds"), None);
}

// A derived relation on the null-extended side of an outer join still answers
// the type question, but never the row-identity question.
#[test]
fn null_extended_derived_relation_still_answers_types() {
    let db = schema_db();
    let query = query("SELECT s.payload FROM b LEFT JOIN (SELECT payload FROM a) s ON true");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(reference) = &select.projection.first().expect("one projection")
    else {
        panic!("expected an expression projection")
    };
    assert_eq!(
        scope.resolve_column(reference).expect("type resolves").map(TableLike::table_name),
        Some("a")
    );
    assert_eq!(query.projection_source_table(&db).expect("row identity succeeds"), None);
}

// A wildcard over a `USING` join carries each merged name once with no
// source (asking it answers nothing, and never an ambiguity against itself),
// while the sides' remaining columns keep their own sources.
#[test]
fn wildcard_over_using_join_pushes_merged_column_once() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT, pa INT); CREATE TABLE b(id INT, pb INT);",
    )
    .expect("schema parses");
    assert_eq!(
        resolve_projection("WITH v AS (SELECT * FROM a JOIN b USING (id)) SELECT v.pa FROM v", &db)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        resolve_projection("WITH v AS (SELECT * FROM a JOIN b USING (id)) SELECT v.id FROM v", &db)
            .expect("resolves"),
        None
    );
}

/// Schema mirroring the PostgreSQL 18.4 measurements behind the wildcard
/// output order, the alias column list, and the qualified prefix rules.
fn wildcard_db() -> ParserDB {
    ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT, x INT);
         CREATE TABLE b(id INT, y INT);
         CREATE TABLE c(x INT, id INT);
         CREATE TABLE m(id INT, mm INT);
         CREATE TABLE users(id INT, name TEXT);
         CREATE SCHEMA \"App\";
         CREATE TABLE \"App\".docs(id INT, body TEXT);",
    )
    .expect("schema parses")
}

// PostgreSQL pairs set-operation arms by ordinal and names the output from
// the left arm. Measured header of `SELECT * FROM a JOIN b USING (id)` is
// `id, x, y` (merged column first), so a hand-written arm listing
// `x, y, id` pairs `x` against the merged `id` and the answer is nothing.
#[test]
fn set_operation_arms_pair_by_postgres_ordinal() {
    let db = wildcard_db();
    assert_eq!(
        resolve_projection(
            "WITH v AS (SELECT * FROM a JOIN b USING (id) \
             UNION ALL SELECT x, y, id FROM a JOIN b USING (id)) \
             SELECT v.x FROM v",
            &db,
        )
        .expect("resolves"),
        None
    );
    // Names come from the left arm, sources pair by ordinal: `x` (ordinal 2)
    // unions `m.id AS x` with `a.x` (two tables, no source), `mm` unions
    // `m.mm` with `a.id` (no source), and `id` (ordinal 1) unions `a.id`
    // with `a.x`, which keeps table `a`.
    for (reference, expected) in [("v.x", None), ("v.mm", None), ("v.id", Some("a"))] {
        assert_eq!(
            resolve_projection(
                &format!(
                    "WITH v AS (SELECT m.mm, a.id, m.id AS x FROM m JOIN a ON true \
                     UNION ALL SELECT a.id, a.x, a.x FROM a) \
                     SELECT {reference} FROM v"
                ),
                &db,
            )
            .expect("resolves")
            .as_deref(),
            expected,
            "{reference}"
        );
    }
    // Control: two arms written in the same column order pair name with
    // name, and the shared source answers.
    assert_eq!(
        resolve_projection(
            "WITH v AS (SELECT x FROM a JOIN b ON a.id = b.id \
             UNION ALL SELECT x FROM a JOIN b ON a.id = b.id) \
             SELECT v.x FROM v",
            &db,
        )
        .expect("resolves")
        .as_deref(),
        Some("a")
    );
}

// An alias column list on a derived `FROM` binds positionally to
// PostgreSQL's join output order: measured header of
// `SELECT * FROM a JOIN b USING (id)` is `id, x, y`, and of the chained
// `a JOIN b USING (id) JOIN c USING (x)` it is `x, id, y, id`.
#[test]
fn alias_column_list_binds_postgres_join_order() {
    let db = wildcard_db();
    for (reference, expected) in [("s.x", Some("a")), ("s.y", Some("b"))] {
        assert_eq!(
            resolve_projection(
                &format!("SELECT {reference} FROM (SELECT * FROM a JOIN b USING (id)) s(id,x,y)"),
                &db,
            )
            .expect("resolves")
            .as_deref(),
            expected,
            "{reference}"
        );
    }
    // A partial list renames only the leading ordinals.
    assert_eq!(
        resolve_projection("SELECT s.q FROM (SELECT * FROM a JOIN b USING (id)) s(p,q)", &db,)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
    // A derived relation first: ordinal 0 is its `mm`, sourced by `m`.
    assert_eq!(
        resolve_projection(
            "SELECT s.p FROM (SELECT * FROM (SELECT mm FROM m) cv JOIN a ON true) s(p,q,r)",
            &db,
        )
        .expect("resolves")
        .as_deref(),
        Some("m")
    );
    // Chained `USING` joins: measured ordinal 2 of `x, id, y, id` is `b.y`.
    assert_eq!(
        resolve_projection(
            "SELECT s.r FROM (SELECT * FROM a JOIN b USING (id) JOIN c USING (x)) s(p,q,r,z)",
            &db,
        )
        .expect("resolves")
        .as_deref(),
        Some("b")
    );
}

// An alias column list on a base table renames the relation's exposed
// columns positionally and hides the originals (measured on PostgreSQL
// 18.4: `SELECT u.n FROM users u(n)` returns the id values, `u.id` errors
// with `column u.id does not exist`, and `u.name` still resolves because a
// partial list keeps the tail's own names).
#[test]
fn base_table_alias_list_renames_columns() {
    let db = wildcard_db();
    for (reference, expected) in
        [("u.n", Some("users")), ("u.id", None), ("u.name", Some("users")), ("n", Some("users"))]
    {
        assert_eq!(
            resolve_projection(&format!("SELECT {reference} FROM users u(n)"), &db)
                .expect("resolves")
                .as_deref(),
            expected,
            "{reference}"
        );
    }
    // More aliases than columns is a mismatch PostgreSQL rejects, so the
    // relation is not modeled and a bare reference answers nothing.
    assert_eq!(resolve_projection("SELECT id FROM users u(a,b,c)", &db).expect("resolves"), None);
    // The rename travels through a derived relation's wildcard: measured
    // `SELECT s.y FROM (SELECT * FROM users u(z, y)) s` reads the renamed
    // `name` column, and the tail keeps its own name through
    // `SELECT s.name FROM (SELECT * FROM users u(z)) s`.
    for (sql, expected) in [
        ("SELECT s.y FROM (SELECT * FROM users u(z, y)) s", Some("users")),
        ("SELECT s.name FROM (SELECT * FROM users u(z)) s", Some("users")),
    ] {
        assert_eq!(resolve_projection(sql, &db).expect("resolves").as_deref(), expected, "{sql}");
    }
}

// A qualified wildcard's prefix resolves by the qualified column reference
// rules: one part matches a relation alias or key, two parts must name a
// base relation's own schema (a CTE has none, measured rejection of
// `sch.cv.*`), and three parts match nothing because the database name is
// not modeled (a documented divergence: PostgreSQL accepts an exact
// `database.schema.table.*`).
#[test]
fn qualified_wildcard_prefix_matches_the_column_rules() {
    let db = wildcard_db();
    let source = |sql: &str| {
        query(sql)
            .projection_source_table(&db)
            .expect("row identity succeeds")
            .map(|table| table.table_name().to_string())
    };
    // Measured: PostgreSQL accepts `public.a.*` for a schema-less table.
    assert_eq!(source("SELECT public.a.* FROM a").as_deref(), Some("a"));
    assert_eq!(source("SELECT nosch.a.* FROM a"), None);
    assert_eq!(source("WITH cv AS (SELECT x FROM a) SELECT sch.cv.* FROM cv"), None);
    assert_eq!(source("SELECT probe.public.a.* FROM a"), None);
    // Quoted schema names must match exactly, as for column references.
    assert_eq!(source("SELECT \"App\".docs.* FROM \"App\".docs").as_deref(), Some("docs"));
    assert_eq!(source("SELECT app.docs.* FROM \"App\".docs"), None);
}

// A second `NATURAL` join merges on the names the first join's output
// exposes, including those the joined side contributed. Measured on
// PostgreSQL 18.4: `SELECT * FROM a NATURAL JOIN b NATURAL JOIN c` outputs
// `y, id, x, z`, so `y` (from `b`) is merged by the second join and bare `y`
// is accepted rather than ambiguous, which makes it answer nothing here.
#[test]
fn chained_natural_join_merges_a_name_from_the_joined_side() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT, x INT);
         CREATE TABLE b(id INT, y INT);
         CREATE TABLE c(y INT, z INT);",
    )
    .expect("schema parses");
    assert_eq!(
        resolve_projection("SELECT y FROM a NATURAL JOIN b NATURAL JOIN c", &db).expect("resolves"),
        None
    );
    // Ordinals of the measured `y, id, x, z` output: both merged names carry
    // no source, ordinal 2 is `a.x` and ordinal 3 is `c.z`.
    for (reference, expected) in
        [("s.p", None), ("s.q", None), ("s.r", Some("a")), ("s.t", Some("c"))]
    {
        assert_eq!(
            resolve_projection(
                &format!(
                    "SELECT {reference} \
                     FROM (SELECT * FROM a NATURAL JOIN b NATURAL JOIN c) s(p,q,r,t)"
                ),
                &db,
            )
            .expect("resolves")
            .as_deref(),
            expected,
            "{reference}"
        );
    }
}

// Measured on PostgreSQL 18.4: an alias column list longer than the relation
// it renames fails with `table "s" has 1 columns available but 2 columns
// specified`, for a derived subquery and for a CTE reference alike, so the
// relation is not modeled and a reference through it answers nothing.
#[test]
fn derived_relation_with_too_many_aliases_is_opaque() {
    let db = wildcard_db();
    assert_eq!(
        resolve_projection("SELECT s.p FROM (SELECT id FROM a) s(p, q)", &db).expect("resolves"),
        None
    );
    assert_eq!(
        resolve_projection("WITH v AS (SELECT id FROM a) SELECT s.p FROM v s(p, q)", &db)
            .expect("resolves"),
        None
    );
    // Control: a list that fits renames positionally.
    assert_eq!(
        resolve_projection("SELECT s.p FROM (SELECT id FROM a) s(p)", &db)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
}

// A relation body carries its own `WITH` clause and its own parentheses, and
// a reference still follows the column through every level to the base table.
#[test]
fn nested_with_and_parenthesized_bodies_resolve() {
    let db = schema_db();
    assert_eq!(
        resolve_projection(
            "WITH ov AS (SELECT id FROM a) \
             SELECT s.n FROM (WITH iv AS (SELECT id FROM ov) SELECT id AS n FROM iv) s",
            &db,
        )
        .expect("resolves")
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        resolve_projection("WITH v AS ((SELECT id FROM a)) SELECT v.id FROM v", &db)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
}

// PostgreSQL rejects a repeated name in a `USING` list, and the resolver
// tolerates it by merging the name once, so a wildcard over the join carries
// one `id` and a reference to it stays unambiguous.
#[test]
fn duplicate_using_name_merges_once() {
    let db = schema_db();
    assert_eq!(
        resolve_projection(
            "WITH v AS (SELECT * FROM a JOIN b USING (id, id)) SELECT v.id FROM v",
            &db,
        )
        .expect("resolves"),
        None
    );
}

// Set arms must have equal enumerable widths, with the left arm naming output.
#[test]
fn set_operation_arms_that_cannot_pair_answer_nothing() {
    let db = schema_db();
    for body in [
        "SELECT id FROM a UNION ALL SELECT id, payload FROM a",
        "SELECT count(*) FROM a UNION ALL SELECT id FROM a",
        "SELECT id FROM a UNION ALL VALUES (1)",
        "VALUES (1) UNION ALL SELECT id FROM a",
    ] {
        let sql = format!("WITH v AS ({body}) SELECT v.id FROM v");
        assert_eq!(resolve_projection(&sql, &db).expect("resolves"), None, "{body}");
    }
    for body in ["SELECT id FROM a UNION ALL VALUES (1)", "VALUES (1) UNION ALL SELECT id FROM a"] {
        let sql = format!("WITH RECURSIVE v(id) AS ({body}) SELECT v.id FROM v");
        assert_eq!(resolve_projection(&sql, &db).expect("resolves"), None, "{body}");
    }
}

// An aliased projection whose expression is ambiguous inside the body still
// names an output column, and that column carries no source rather than
// failing the whole derivation.
#[test]
fn ambiguous_aliased_projection_degrades_to_no_source() {
    let db = schema_db();
    assert_eq!(
        resolve_projection(
            "WITH v AS (SELECT payload AS p FROM a JOIN b ON a.id = b.id) SELECT v.p FROM v",
            &db,
        )
        .expect("resolves"),
        None
    );
}

// A reference of four or more parts carries no output name, since the leading
// database part is not modeled, so a body projecting one stays opaque. The
// same holds for Spark's `expr AS (a, b)`, which names several outputs for
// one expression.
#[test]
fn projections_without_a_modeled_output_name_leave_the_body_opaque() {
    let db = schema_db();
    assert_eq!(
        resolve_projection("WITH v AS (SELECT d.s.t.c FROM a) SELECT v.c FROM v", &db)
            .expect("resolves"),
        None
    );
    assert_eq!(
        resolve_projection("WITH v AS (SELECT payload AS (p, q) FROM a) SELECT v.p FROM v", &db)
            .expect("resolves"),
        None
    );
}

// A qualified wildcard inside a body expands a derived relation as well as a
// base table, and the columns keep the sources the inner relation gave them.
#[test]
fn qualified_wildcard_over_a_derived_relation_passes_through() {
    let db = schema_db();
    assert_eq!(
        resolve_projection(
            "WITH v AS (SELECT s.* FROM (SELECT id FROM a) s) SELECT v.id FROM v",
            &db,
        )
        .expect("resolves")
        .as_deref(),
        Some("a")
    );
    // A prefix matching no relation in the body belongs to a statement
    // PostgreSQL rejects, so the body stays opaque.
    assert_eq!(
        resolve_projection("WITH v AS (SELECT nosuch.* FROM a) SELECT v.id FROM v", &db)
            .expect("resolves"),
        None
    );
}

// An ambiguity inside one derived relation names that relation, spelled as
// the SQL wrote it, and an unaliased subquery is named as such.
#[test]
fn ambiguity_candidates_name_the_relation() {
    let db = schema_db();
    let Err(LookupError::AmbiguousTableLookup { object_name, candidates }) =
        resolve_projection("SELECT x FROM (SELECT id AS x, payload AS x FROM a)", &db)
    else {
        panic!("expected an ambiguity for the anonymous subquery")
    };
    assert_eq!(object_name, "x");
    assert_eq!(candidates, vec!["(subquery)".to_string()]);

    let Err(LookupError::AmbiguousTableLookup { candidates, .. }) =
        resolve_projection("SELECT x FROM (SELECT id AS x, payload AS x FROM a) \"S\"", &db)
    else {
        panic!("expected an ambiguity for the quoted alias")
    };
    assert_eq!(candidates, vec!["\"S\"".to_string()]);
}

// A bare name exposed only by a null-extended derived relation still answers
// the type question, because the column is that table's, while the
// row-identity question refuses it: the row may be absent from the output.
#[test]
fn bare_name_through_a_null_extended_derived_relation() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT, x INT); CREATE TABLE b(id INT, y INT);",
    )
    .expect("schema parses");
    let sql = "SELECT y FROM a LEFT JOIN (SELECT y FROM b) s ON true";
    assert_eq!(resolve_projection(sql, &db).expect("resolves").as_deref(), Some("b"));
    assert_eq!(query(sql).projection_source_table(&db).expect("identity succeeds"), None);
}

/// Resolves the query's single projected expression against a scope built
/// from the same query under `case`, rendering the resolved table's name.
fn resolve_projection_with_case(
    sql: &str,
    db: &ParserDB,
    case: IdentifierCase,
) -> Result<Option<String>, LookupError> {
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, db, case)?;
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(expr) = &select.projection.first().expect("one projection") else {
        panic!("expected an expression projection")
    };
    Ok(scope.resolve_column(expr)?.map(|table| table.table_name().to_string()))
}

/// Resolves the query's single projected expression's binding against a
/// scope built from the same query under `case`, rendering the owned
/// relation and exposed names.
#[expect(clippy::type_complexity, reason = "an owned mirror of ColumnBinding's two fields")]
fn resolve_binding(
    sql: &str,
    db: &ParserDB,
    case: IdentifierCase,
) -> Result<Option<(Option<(String, bool)>, Option<(String, bool)>)>, LookupError> {
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, db, case)?;
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(expr) = &select.projection.first().expect("one projection") else {
        panic!("expected an expression projection")
    };
    Ok(scope.resolve_column_binding(expr)?.map(|(_, binding)| {
        (
            binding.relation().map(|(name, quoted)| (name.to_string(), quoted)),
            binding.exposed().map(|(name, quoted)| (name.to_string(), quoted)),
        )
    }))
}

// `IdentifierCase` selects the lexical comparison `ColumnScope` applies to
// every `FROM`/column lookup. SQLite folds every identifier for ASCII
// whether or not it was quoted, MySQL's `lower_case_table_names = 0` folds
// neither side, and PostgreSQL's own `AsWritten` rule used everywhere else
// in this file folds only the unquoted side. These cases exercise the
// folding on every path `ColumnScope` reaches it through: a bare column, a
// table alias, a `NATURAL` join's computed intersection, a qualified
// wildcard, and a `WITH` reference.

#[test]
fn folded_case_matches_a_quoted_column_as_written_would_refuse() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);").expect("schema parses");
    assert_eq!(
        resolve_projection_with_case("SELECT \"ID\" FROM a", &db, IdentifierCase::AsWritten)
            .expect("resolves"),
        None
    );
    assert_eq!(
        resolve_projection_with_case("SELECT \"ID\" FROM a", &db, IdentifierCase::Folded)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
}

#[test]
fn exact_case_refuses_an_unquoted_match_as_written_folds() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);").expect("schema parses");
    assert_eq!(
        resolve_projection_with_case("SELECT ID FROM a", &db, IdentifierCase::AsWritten)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        resolve_projection_with_case("SELECT ID FROM a", &db, IdentifierCase::Exact)
            .expect("resolves"),
        None
    );
}

#[test]
fn folded_case_matches_a_table_alias_through_a_differently_cased_qualifier() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);").expect("schema parses");
    let query = query("SELECT id FROM a AS \"MiXeD\"");
    let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded).expect("scope builds");
    assert_eq!(
        scope.resolve_column(&reference("mixed.id")).expect("resolves").map(TableLike::table_name),
        Some("a")
    );
    assert_eq!(
        scope
            .resolve_column(&reference("\"MIXED\".id"))
            .expect("resolves")
            .map(TableLike::table_name),
        Some("a")
    );
}

#[test]
fn folded_case_computes_a_natural_join_intersection_by_folded_name() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(\"ID\" INT, x INT); CREATE TABLE b(id INT, y INT);",
    )
    .expect("schema parses");
    let query = query("SELECT * FROM a NATURAL JOIN b");
    let as_written =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert_eq!(
        as_written.resolve_column(&reference("x")).expect("resolves").map(TableLike::table_name),
        Some("a")
    );
    let folded =
        ColumnScope::from_query(&query, &db, IdentifierCase::Folded).expect("scope builds");
    assert_eq!(folded.resolve_column(&reference("id")).expect("resolves"), None);
}

#[test]
fn folded_case_expands_a_qualified_wildcard_through_a_differently_cased_alias() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);").expect("schema parses");
    let sql = "WITH v AS (SELECT \"T\".* FROM a AS t) SELECT v.id FROM v";
    assert_eq!(
        resolve_projection_with_case(sql, &db, IdentifierCase::AsWritten).expect("resolves"),
        None
    );
    assert_eq!(
        resolve_projection_with_case(sql, &db, IdentifierCase::Folded)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
}

#[test]
fn folded_case_matches_a_cte_reference_regardless_of_case() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);").expect("schema parses");
    let sql = "WITH \"V\" AS (SELECT id FROM a) SELECT v.id FROM v";
    assert_eq!(
        resolve_projection_with_case(sql, &db, IdentifierCase::AsWritten).expect("resolves"),
        None
    );
    assert_eq!(
        resolve_projection_with_case(sql, &db, IdentifierCase::Folded)
            .expect("resolves")
            .as_deref(),
        Some("a")
    );
}

// `ColumnScope::resolve_column_binding` returns, alongside the usual
// `ColumnDefinition`, the relation alias or declared name a reference's
// qualifier matched and the name it is exposed under at that binding.

#[test]
fn binding_reports_the_tables_own_name_when_unaliased() {
    let db = schema_db();
    let (relation, exposed) =
        resolve_binding("SELECT payload FROM a", &db, IdentifierCase::AsWritten)
            .expect("resolves")
            .expect("found");
    assert_eq!(relation, Some(("a".to_string(), false)));
    assert_eq!(exposed, Some(("payload".to_string(), false)));
}

#[test]
fn binding_reports_the_alias_when_the_base_table_is_aliased() {
    let db = schema_db();
    let sql = "SELECT \"Mixed\".payload FROM a AS \"Mixed\"";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::AsWritten).expect("resolves").expect("found");
    assert_eq!(relation, Some(("Mixed".to_string(), true)));
    assert_eq!(exposed, Some(("payload".to_string(), false)));
}

#[test]
fn binding_reports_no_relation_for_an_anonymous_derived_relation() {
    let db = schema_db();
    let sql = "SELECT x FROM (SELECT payload AS x FROM a)";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::AsWritten).expect("resolves").expect("found");
    assert_eq!(relation, None);
    assert_eq!(exposed, Some(("x".to_string(), false)));
}

#[test]
fn binding_reports_the_alias_for_an_aliased_derived_relation() {
    let db = schema_db();
    let sql = "SELECT \"S\".x FROM (SELECT payload AS x FROM a) AS \"S\"";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::AsWritten).expect("resolves").expect("found");
    assert_eq!(relation, Some(("S".to_string(), true)));
    assert_eq!(exposed, Some(("x".to_string(), false)));
}

#[test]
fn folded_case_binding_reports_the_ctes_own_declared_name_not_the_reference_spelling() {
    let db = schema_db();
    let sql = "WITH \"MiXeD\" AS (SELECT payload AS x FROM a) SELECT \"mixed\".x FROM \"mixed\"";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::Folded).expect("resolves").expect("found");
    assert_eq!(relation, Some(("MiXeD".to_string(), true)));
    assert_eq!(exposed, Some(("x".to_string(), false)));
}

#[test]
fn binding_reports_the_with_column_list_alias_as_the_exposed_name() {
    let db = schema_db();
    let sql = "WITH v(renamed) AS (SELECT payload FROM a) SELECT v.renamed FROM v";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::AsWritten).expect("resolves").expect("found");
    assert_eq!(relation, Some(("v".to_string(), false)));
    assert_eq!(exposed, Some(("renamed".to_string(), false)));
}

#[test]
fn binding_reports_nothing_for_an_opaque_match() {
    let db = schema_db();
    let sql = "SELECT n FROM generate_series(1, 3) AS g(n)";
    let (relation, exposed) = resolve_binding(sql, &db, IdentifierCase::AsWritten)
        .expect("resolves")
        .expect("found as opaque");
    assert_eq!(relation, None);
    assert_eq!(exposed, None);
}

#[test]
fn binding_reports_no_relation_for_a_using_merged_column() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(id INT, x INT); CREATE TABLE b(id INT, y INT);",
    )
    .expect("schema parses");
    let sql = "SELECT id FROM a JOIN b USING (id)";
    let (relation, exposed) = resolve_binding(sql, &db, IdentifierCase::AsWritten)
        .expect("resolves")
        .expect("found as merged");
    assert_eq!(relation, None);
    assert_eq!(exposed, Some(("id".to_string(), false)));
}

#[test]
fn binding_still_errors_on_an_ambiguous_match() {
    let db = schema_db();
    let query = query("SELECT id FROM a JOIN b ON a.id = b.id");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope.resolve_column_binding(&reference("id")),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
}

#[test]
fn folded_case_binding_reports_the_views_own_declared_name_not_the_reference_spelling() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE t(a INT); CREATE VIEW \"MyView\" AS SELECT a FROM t;",
    )
    .expect("schema parses");
    let sql = "SELECT \"myview\".a FROM \"myview\"";
    let (relation, exposed) =
        resolve_binding(sql, &db, IdentifierCase::Folded).expect("resolves").expect("found");
    assert_eq!(relation, Some(("MyView".to_string(), true)));
    assert_eq!(exposed, Some(("a".to_string(), false)));
}
