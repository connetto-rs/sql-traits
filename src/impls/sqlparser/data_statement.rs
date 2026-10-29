//! Implementations of [`DataStatementLike`] for the sqlparser data-statement
//! AST nodes (`Query`, `Insert`, `Update`, `Delete`).
//!
//! Both accessors walk every relation sqlparser's [`Visitor`] reaches, which
//! covers subqueries, CTE bodies, set operations and the `INSERT` / `UPDATE`
//! / `DELETE` mutation targets. A bare name that a visible `WITH` item binds
//! reads that item, so it is skipped, except as a mutation target, which
//! always names a stored relation. The one relation source the walk does not
//! reach is the MySQL multi-table `DELETE ... t1, t2` list, which the `Delete`
//! implementation resolves explicitly. `referenced_relations` records what the
//! statement names, and `referenced_tables` goes on to expand every plain view
//! through the view's stored definition, which is bound at creation and read
//! with no search path.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{
    Delete, FromTable, Insert, ObjectName, Query, Statement, TableFactor, TableObject, Update,
    Visit, Visitor,
};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{DataStatementLike, DatabaseLike, Relation, ViewLike},
    utils::{
        object_name::{resolve_bound_relation_object_name, resolve_relation_object_name},
        with_scopes::WithScopes,
    },
};

/// Walks a data statement, recording every relation it names and expanding
/// the base tables each plain view reaches through the view's stored
/// definition, stopping at the first resolution error.
struct RelationCollector<'db, DB: DatabaseLike> {
    database: &'db DB,
    /// Every relation the statement names, deduplicated in first-seen order.
    relations: Vec<Relation<'db, DB>>,
    /// Every base table the statement names or reaches through a plain view,
    /// deduplicated in first-seen order.
    tables: Vec<&'db DB::Table>,
    /// The plain views expanded so far, which a definition reading its view
    /// back cannot re-expand.
    expanded: Vec<&'db DB::View>,
    scopes: WithScopes,
    /// The mutation targets met so far, compared by address.
    targets: Vec<*const ObjectName>,
    /// Whether the walk is inside a stored view's definition, where names are
    /// bound and read with no search path.
    bound: bool,
}

impl<'db, DB: DatabaseLike> RelationCollector<'db, DB> {
    fn new(database: &'db DB) -> Self {
        Self {
            database,
            relations: Vec::new(),
            tables: Vec::new(),
            expanded: Vec::new(),
            scopes: WithScopes::default(),
            targets: Vec::new(),
            bound: false,
        }
    }

    /// Records the names a mutation writes to. PostgreSQL resolves them
    /// against stored relations only, so a `WITH` item of the same name does
    /// not hide them.
    fn record_targets(&mut self, statement: &Statement) {
        match statement {
            Statement::Insert(insert) => self.record_insert_target(insert),
            Statement::Update(update) => self.record_update_target(update),
            Statement::Delete(delete) => self.record_delete_target(delete),
            _ => {}
        }
    }

    fn record_insert_target(&mut self, insert: &Insert) {
        if let TableObject::TableName(name) = &insert.table {
            self.targets.push(&raw const *name);
        }
    }

    fn record_update_target(&mut self, update: &Update) {
        if let TableFactor::Table { name, .. } = &update.table.relation {
            self.targets.push(&raw const *name);
        }
    }

    /// With a multi-table list the `FROM` items are the rows read, and the
    /// list holds the targets, which the `Delete` implementation resolves.
    fn record_delete_target(&mut self, delete: &Delete) {
        if delete.tables.is_empty() {
            let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = &delete.from;
            self.targets.extend(from.iter().filter_map(|item| {
                match &item.relation {
                    TableFactor::Table { name, .. } => Some(&raw const *name),
                    _ => None,
                }
            }));
        }
    }

    /// Records a relation the walk names, and expands a plain view through
    /// its stored definition.
    fn read(&mut self, relation: Relation<'db, DB>) -> Result<(), LookupError> {
        if !self.bound && !self.relations.iter().any(|seen| same_relation(seen, &relation)) {
            self.relations.push(relation);
        }
        match relation {
            Relation::Table(table) => {
                if !self.tables.iter().any(|seen| core::ptr::eq(*seen, table)) {
                    self.tables.push(table);
                }
                Ok(())
            }
            Relation::View(view) => self.expand_view(view),
            Relation::MaterializedView(_) => Ok(()),
        }
    }

    /// Expands a plain view through its stored definition, which reads its
    /// names as bound at creation, with no search path.
    fn expand_view(&mut self, view: &'db DB::View) -> Result<(), LookupError> {
        if self.expanded.iter().any(|seen| core::ptr::eq(*seen, view)) {
            return Ok(());
        }
        self.expanded.push(view);
        let was_bound = core::mem::replace(&mut self.bound, true);
        let outcome = view.definition().visit(self);
        self.bound = was_bound;
        outcome.break_value().map_or(Ok(()), Err)
    }
}

/// Whether two relations are the same one, compared by the address the
/// database hands back for it.
fn same_relation<DB: DatabaseLike>(left: &Relation<'_, DB>, right: &Relation<'_, DB>) -> bool {
    match (left, right) {
        (Relation::Table(left), Relation::Table(right)) => core::ptr::eq(*left, *right),
        (Relation::View(left), Relation::View(right)) => core::ptr::eq(*left, *right),
        (Relation::MaterializedView(left), Relation::MaterializedView(right)) => {
            core::ptr::eq(*left, *right)
        }
        _ => false,
    }
}

impl<DB: DatabaseLike> Visitor for RelationCollector<'_, DB> {
    type Break = LookupError;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.leave(query);
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        self.record_targets(statement);
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, object_name: &ObjectName) -> ControlFlow<Self::Break> {
        if self.scopes.reads_item(object_name)
            && !self.targets.iter().any(|target| core::ptr::eq(*target, object_name))
        {
            return ControlFlow::Continue(());
        }
        let resolved = if self.bound {
            resolve_bound_relation_object_name(object_name, self.database)
        } else {
            resolve_relation_object_name(object_name, self.database, IdentifierCase::AsWritten)
        };
        match resolved.and_then(|relation| relation.map_or(Ok(()), |relation| self.read(relation)))
        {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
}

/// Walks `node` after `record` notes its own mutation targets, then reads the
/// MySQL multi-table `DELETE` targets `listed`, which the visitor does not
/// reach.
fn collect<'db, N: Visit, DB: DatabaseLike>(
    node: &N,
    database: &'db DB,
    record: impl FnOnce(&mut RelationCollector<'db, DB>),
    listed: &[ObjectName],
) -> Result<RelationCollector<'db, DB>, LookupError> {
    let mut collector = RelationCollector::new(database);
    record(&mut collector);
    if let ControlFlow::Break(error) = node.visit(&mut collector) {
        return Err(error);
    }
    for object_name in listed {
        if let Some(relation) =
            resolve_relation_object_name(object_name, database, IdentifierCase::AsWritten)?
        {
            collector.read(relation)?;
        }
    }
    Ok(collector)
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Query {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        Ok(collect(self, database, |_| {}, &[])?.tables)
    }

    fn referenced_relations<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<Relation<'db, DB>>, LookupError> {
        Ok(collect(self, database, |_| {}, &[])?.relations)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Insert {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_insert_target(self), &[])?.tables)
    }

    fn referenced_relations<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<Relation<'db, DB>>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_insert_target(self), &[])?
            .relations)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Update {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_update_target(self), &[])?.tables)
    }

    fn referenced_relations<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<Relation<'db, DB>>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_update_target(self), &[])?
            .relations)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Delete {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_delete_target(self), &self.tables)?
            .tables)
    }

    fn referenced_relations<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<Relation<'db, DB>>, LookupError> {
        Ok(collect(self, database, |collector| collector.record_delete_target(self), &self.tables)?
            .relations)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec::Vec};

    use sqlparser::{
        ast::Statement,
        dialect::{ClickHouseDialect, GenericDialect, MySqlDialect},
        parser::Parser,
    };

    use crate::{
        errors::LookupError,
        prelude::ParserDB,
        structs::{IdentifierCase, TargetName},
        traits::{DataStatementLike, DatabaseLike, Relation, TableLike, ViewLike},
    };

    const SCHEMA: &str = "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
        CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT);
        CREATE TABLE audit (id INT PRIMARY KEY, note TEXT);
        CREATE VIEW open_orders AS SELECT user_id, total FROM orders;
        CREATE VIEW open_high AS SELECT user_id FROM open_orders WHERE total > 100;
        CREATE MATERIALIZED VIEW big_orders AS SELECT user_id, total FROM orders;
    ";

    fn schema_db() -> ParserDB {
        ParserDB::parse::<GenericDialect>(SCHEMA).expect("schema parses")
    }

    fn parse_one(sql: &str) -> Statement {
        let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("statement parses");
        assert_eq!(statements.len(), 1, "expected exactly one statement");
        statements.pop().unwrap()
    }

    fn referenced_names<S: DataStatementLike<ParserDB>>(
        statement: &S,
        db: &ParserDB,
    ) -> Vec<String> {
        statement
            .referenced_tables(db)
            .expect("referenced_tables succeeds")
            .iter()
            .map(|table| table.table_name().to_string())
            .collect()
    }

    fn relation_names<S: DataStatementLike<ParserDB>>(statement: &S, db: &ParserDB) -> Vec<String> {
        statement
            .referenced_relations(db)
            .expect("referenced_relations succeeds")
            .iter()
            .map(|relation| {
                match relation {
                    Relation::Table(table) => format!("table {}", table.table_name()),
                    Relation::View(view) => format!("view {}", view.view_name()),
                    Relation::MaterializedView(view) => {
                        format!("materialized view {}", view.view_name())
                    }
                }
            })
            .collect()
    }

    fn as_query(statement: &Statement) -> &sqlparser::ast::Query {
        match statement {
            Statement::Query(query) => query,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn single_table_select() {
        let db = schema_db();
        let statement = parse_one("SELECT id, name FROM users");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn join_collects_both_sides() {
        let db = schema_db();
        let statement =
            parse_one("SELECT o.id, u.name FROM orders o JOIN users u ON o.user_id = u.id");
        assert_eq!(
            referenced_names(as_query(&statement), &db),
            vec!["orders".to_string(), "users".to_string()]
        );
    }

    #[test]
    fn self_join_deduplicates_to_one_table() {
        let db = schema_db();
        let statement = parse_one("SELECT a.id FROM users a JOIN users b ON a.id = b.id");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn subquery_in_where_is_included() {
        let db = schema_db();
        let statement = parse_one(
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 0)",
        );
        assert_eq!(
            referenced_names(as_query(&statement), &db),
            vec!["users".to_string(), "orders".to_string()]
        );
    }

    #[test]
    fn set_operation_unions_both_arms() {
        let db = schema_db();
        let statement = parse_one("SELECT id FROM users UNION SELECT id FROM orders");
        assert_eq!(
            referenced_names(as_query(&statement), &db),
            vec!["users".to_string(), "orders".to_string()]
        );
    }

    #[test]
    fn cte_body_tables_resolve_and_cte_name_is_skipped() {
        let db = schema_db();
        let statement =
            parse_one("WITH recent AS (SELECT user_id FROM orders) SELECT user_id FROM recent");
        // `orders` (CTE body) resolves; `recent` (the CTE name) does not match
        // a stored relation and is skipped.
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
        assert_eq!(relation_names(as_query(&statement), &db), vec!["table orders".to_string()]);
    }

    #[test]
    fn cte_shadows_base_table_of_the_same_name() {
        let db = schema_db();
        let statement = parse_one("WITH users AS (SELECT 1 AS id) SELECT id FROM users");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn cte_shadowing_folds_unquoted_names() {
        let db = schema_db();
        let statement = parse_one("WITH USERS AS (SELECT 1 AS id) SELECT id FROM Users");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        let statement = parse_one("WITH \"USERS\" AS (SELECT 1 AS id) SELECT id FROM users");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn schema_qualified_name_reaches_the_shadowed_table() {
        let db = schema_db();
        let statement = parse_one("WITH users AS (SELECT 1 AS id) SELECT id FROM public.users");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn non_recursive_cte_body_naming_itself_reads_the_table() {
        let db = schema_db();
        let statement = parse_one("WITH users AS (SELECT id FROM users) SELECT id FROM users");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn recursive_cte_body_reads_itself() {
        let db = schema_db();
        let statement = parse_one(
            "WITH RECURSIVE users AS (SELECT 1 AS id UNION ALL SELECT id + 1 FROM users WHERE id < 3) \
             SELECT id FROM users",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn cte_body_sees_only_earlier_ctes() {
        let db = schema_db();
        let statement = parse_one(
            "WITH users AS (SELECT 1 AS id), later AS (SELECT id FROM users) SELECT id FROM later",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        let statement = parse_one(
            "WITH earlier AS (SELECT id FROM users), users AS (SELECT 2 AS id) SELECT id FROM earlier",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn with_scope_ends_at_its_query() {
        let db = schema_db();
        let statement = parse_one(
            "SELECT id FROM users WHERE id IN (WITH users AS (SELECT 1 AS id) SELECT id FROM users)",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
        let statement = parse_one(
            "WITH orders AS (SELECT 1 AS user_id) \
             SELECT id FROM users WHERE id IN (SELECT user_id FROM orders)",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["users".to_string()]);
    }

    #[test]
    fn mutation_targets_ignore_ctes_of_the_same_name() {
        let db = schema_db();
        for sql in [
            "WITH audit AS (SELECT 'x' AS note) INSERT INTO audit (note) VALUES ('y')",
            "WITH audit AS (SELECT 1 AS id) UPDATE audit SET note = 'y'",
            "WITH audit AS (SELECT 1 AS id) DELETE FROM audit",
        ] {
            let statement = parse_one(sql);
            assert_eq!(
                referenced_names(as_query(&statement), &db),
                vec!["audit".to_string()],
                "{sql}"
            );
        }
    }

    #[test]
    fn mutation_sources_see_ctes() {
        let db = schema_db();
        let statement = parse_one(
            "WITH users AS (SELECT 1 AS id) \
             DELETE FROM orders USING users WHERE orders.user_id = users.id",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
        let statement = parse_one(
            "WITH users AS (SELECT 1 AS id) \
             UPDATE orders SET total = 0 FROM users WHERE orders.user_id = users.id",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
    }

    #[test]
    fn mysql_multi_table_delete_from_items_see_ctes() {
        let db = schema_db();
        let statements = Parser::parse_sql(
            &MySqlDialect {},
            "WITH users AS (SELECT 1 AS id) \
             DELETE orders FROM users JOIN orders ON orders.user_id = users.id",
        )
        .expect("statement parses");
        assert_eq!(referenced_names(as_query(&statements[0]), &db), vec!["orders".to_string()]);
    }

    #[test]
    fn derived_mutation_target_reads_ctes() {
        let db = schema_db();
        let statement = parse_one(
            "WITH users AS (SELECT 1 AS id) UPDATE (SELECT id FROM users) AS d SET id = 1",
        );
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn unknown_relation_is_skipped() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM nonexistent_table");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        assert_eq!(relation_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn insert_select_includes_target_and_source() {
        let db = schema_db();
        let statement = parse_one("INSERT INTO audit (note) SELECT name FROM users");
        let Statement::Insert(insert) = &statement else { panic!("expected insert") };
        assert_eq!(referenced_names(insert, &db), vec!["audit".to_string(), "users".to_string()]);
        assert_eq!(
            relation_names(insert, &db),
            vec!["table audit".to_string(), "table users".to_string()]
        );
    }

    #[test]
    fn update_with_from_includes_target_and_source() {
        let db = schema_db();
        let statement =
            parse_one("UPDATE orders SET total = 0 FROM users WHERE orders.user_id = users.id");
        let Statement::Update(update) = &statement else { panic!("expected update") };
        assert_eq!(referenced_names(update, &db), vec!["orders".to_string(), "users".to_string()]);
    }

    #[test]
    fn delete_includes_target_and_subquery() {
        let db = schema_db();
        let statement = parse_one(
            "DELETE FROM orders WHERE user_id IN (SELECT id FROM users WHERE name = 'x')",
        );
        let Statement::Delete(delete) = &statement else { panic!("expected delete") };
        assert_eq!(referenced_names(delete, &db), vec!["orders".to_string(), "users".to_string()]);
    }

    #[test]
    fn mysql_multi_table_delete_unions_listed_tables() {
        let db = schema_db();
        let statements = Parser::parse_sql(
            &MySqlDialect {},
            "DELETE users, orders FROM users JOIN orders ON users.id = orders.user_id",
        )
        .expect("statement parses");
        let Statement::Delete(delete) = &statements[0] else { panic!("expected delete") };
        let mut names = referenced_names(delete, &db);
        names.sort();
        assert_eq!(names, vec!["orders".to_string(), "users".to_string()]);
    }

    #[test]
    fn referenced_relations_tag_every_kind_the_statement_reads() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM users, open_orders, big_orders");
        assert_eq!(
            relation_names(as_query(&statement), &db),
            vec![
                "table users".to_string(),
                "view open_orders".to_string(),
                "materialized view big_orders".to_string(),
            ],
        );
    }

    #[test]
    fn view_expands_to_its_base_table() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM open_orders");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
        assert_eq!(relation_names(as_query(&statement), &db), vec!["view open_orders".to_string()],);
    }

    #[test]
    fn view_over_a_view_expands_to_the_base_table() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM open_high");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
        assert_eq!(relation_names(as_query(&statement), &db), vec!["view open_high".to_string()],);
    }

    #[test]
    fn view_in_a_subquery_expands() {
        let db = schema_db();
        let statement =
            parse_one("SELECT id FROM users WHERE id IN (SELECT user_id FROM open_orders)");
        assert_eq!(
            referenced_names(as_query(&statement), &db),
            vec!["users".to_string(), "orders".to_string()],
        );
    }

    #[test]
    fn with_item_shadows_a_view() {
        let db = schema_db();
        let statement =
            parse_one("WITH open_orders AS (SELECT 1 AS user_id) SELECT user_id FROM open_orders");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        assert_eq!(relation_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn materialized_view_is_never_expanded() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM big_orders");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        assert_eq!(
            relation_names(as_query(&statement), &db),
            vec!["materialized view big_orders".to_string()],
        );
    }

    #[test]
    fn view_expansion_dedupes_the_base_table() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM orders, open_orders");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
        // The table reached through two views of it still appears once.
        let statement = parse_one("SELECT * FROM open_orders, open_high");
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
    }

    /// Writing through an updatable view writes its base table, so a target
    /// view expands like any view the statement names.
    #[test]
    fn a_dml_target_view_expands_to_the_table_it_writes() {
        let db = schema_db();
        let insert = parse_one("INSERT INTO open_orders (user_id) VALUES (1)");
        let Statement::Insert(insert) = &insert else { panic!("expected insert") };
        assert_eq!(referenced_names(insert, &db), vec!["orders".to_string()]);
        assert_eq!(relation_names(insert, &db), vec!["view open_orders".to_string()]);

        let update = parse_one("UPDATE open_orders SET total = 0 FROM users");
        let Statement::Update(update) = &update else { panic!("expected update") };
        assert_eq!(referenced_names(update, &db), vec!["orders".to_string(), "users".to_string()]);
        assert_eq!(
            relation_names(update, &db),
            vec!["view open_orders".to_string(), "table users".to_string()]
        );

        let delete = parse_one("DELETE FROM open_orders WHERE user_id IN (SELECT id FROM users)");
        let Statement::Delete(delete) = &delete else { panic!("expected delete") };
        assert_eq!(referenced_names(delete, &db), vec!["orders".to_string(), "users".to_string()]);
        assert_eq!(
            relation_names(delete, &db),
            vec!["view open_orders".to_string(), "table users".to_string()]
        );
    }

    #[test]
    fn a_view_cycle_terminates() {
        let db = ParserDB::parse::<GenericDialect>(
            "
            CREATE TABLE t (id INT);
            CREATE VIEW c1 AS SELECT id FROM t;
            CREATE VIEW c2 AS SELECT id FROM c1;
            CREATE OR REPLACE VIEW c1 AS SELECT id FROM c2;
            ",
        )
        .expect("schema parses");
        let statement = parse_one("SELECT * FROM c1");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
        assert_eq!(relation_names(as_query(&statement), &db), vec!["view c1".to_string()],);
    }

    #[test]
    fn a_view_definition_reads_its_bound_name() {
        let db = ParserDB::parse::<GenericDialect>(
            "
            CREATE TABLE orders (id INT, user_id INT, total INT);
            CREATE SCHEMA alt;
            CREATE TABLE alt.orders (id INT, user_id INT, total INT);
            CREATE VIEW open_orders AS SELECT user_id FROM orders;
            SET search_path TO alt, public;
            ",
        )
        .expect("schema parses");
        let statement = parse_one("SELECT * FROM open_orders");
        // A bound read applies no search path, so the path's leading `alt`
        // does not pull the expansion over to `alt.orders`.
        let tables = as_query(&statement).referenced_tables(&db).expect("resolves");
        assert_eq!(tables.len(), 1, "one base table is reached");
        assert_eq!(tables[0].table_schema(), None, "the bound table is the bare one");
        assert_eq!(relation_names(as_query(&statement), &db), vec!["view open_orders".to_string()],);
        assert_eq!(
            db.table_by_target(
                TargetName::new("orders", false).with_schema("alt", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous")
            .expect("the shadowing table")
            .table_name(),
            "orders"
        );
    }

    #[test]
    fn overqualified_relation_propagates_resolver_error() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM a.b.c");
        let result = as_query(&statement).referenced_tables(&db);
        assert!(matches!(result, Err(LookupError::InvalidObjectName { .. })), "got {result:?}");
    }

    #[test]
    fn a_materialized_view_named_twice_is_recorded_once() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM big_orders, big_orders AS again");
        assert_eq!(
            relation_names(as_query(&statement), &db),
            vec!["materialized view big_orders".to_string()]
        );
    }

    #[test]
    fn a_listed_delete_target_naming_nothing_is_skipped() {
        let db = schema_db();
        let statements = Parser::parse_sql(&MySqlDialect {}, "DELETE nowhere, orders FROM orders")
            .expect("statement parses");
        let Statement::Delete(delete) = &statements[0] else { panic!("expected delete") };
        assert_eq!(relation_names(delete, &db), vec!["table orders".to_string()]);
    }

    #[test]
    fn a_table_function_target_names_no_stored_relation() {
        let db = schema_db();
        let statements = Parser::parse_sql(
            &ClickHouseDialect {},
            "INSERT INTO FUNCTION remote('host', 'db', 'orders') SELECT id FROM users",
        )
        .expect("statement parses");
        let Statement::Insert(insert) = &statements[0] else { panic!("expected insert") };
        assert_eq!(relation_names(insert, &db), vec!["table users".to_string()]);
    }

    #[test]
    fn a_derived_delete_target_names_no_stored_relation() {
        let db = schema_db();
        let statement = parse_one("DELETE FROM (SELECT id FROM orders) AS d");
        let Statement::Delete(delete) = &statement else { panic!("expected delete") };
        assert_eq!(relation_names(delete, &db), vec!["table orders".to_string()]);
    }

    #[test]
    fn a_whole_select_statement_reads_its_relations() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM users");
        let collected = super::collect(&statement, &db, |_| {}, &[]).expect("resolves");
        assert_eq!(collected.tables.len(), 1);
    }
}
