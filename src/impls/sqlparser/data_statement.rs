//! Implementations of [`DataStatementLike`] for the sqlparser data-statement
//! AST nodes (`Query`, `Insert`, `Update`, `Delete`).
//!
//! `referenced_tables` walks every relation sqlparser's [`Visitor`] reaches,
//! which covers subqueries, CTE bodies, set operations and the `INSERT` /
//! `UPDATE` / `DELETE` mutation targets. A bare name that a visible `WITH`
//! item binds reads that item, so it is skipped, except as a mutation target,
//! which always names a stored table. The one relation source the walk does
//! not reach is the MySQL multi-table `DELETE ... t1, t2` list, which the
//! `Delete` implementation resolves explicitly.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{
    Delete, FromTable, Insert, ObjectName, Query, Statement, TableFactor, TableObject, Update,
    Visit, Visitor,
};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{DataStatementLike, DatabaseLike},
    utils::{object_name::resolve_object_name, with_scopes::WithScopes},
};

/// Accumulates resolved base tables, deduplicating by their position in the
/// database so the same table reached through several aliases or a self-join is
/// recorded once. Tables are kept in first-seen order.
struct ReferencedTables<'db, DB: DatabaseLike> {
    ids: Vec<usize>,
    tables: Vec<&'db DB::Table>,
}

impl<'db, DB: DatabaseLike> ReferencedTables<'db, DB> {
    fn new() -> Self {
        Self { ids: Vec::new(), tables: Vec::new() }
    }

    fn add(&mut self, table: &'db DB::Table, database: &'db DB) {
        match database.table_id(table) {
            Some(id) => {
                if !self.ids.contains(&id) {
                    self.ids.push(id);
                    self.tables.push(table);
                }
            }
            // Defensive: a table resolved from `database` should always have an
            // id. Fall back to pointer identity rather than dropping it.
            None => {
                if !self.tables.iter().any(|existing| core::ptr::eq(*existing, table)) {
                    self.tables.push(table);
                }
            }
        }
    }

    fn resolve_and_add(
        &mut self,
        object_name: &ObjectName,
        database: &'db DB,
    ) -> Result<(), LookupError> {
        if let Some(table) = resolve_object_name(object_name, database, IdentifierCase::AsWritten)?
        {
            self.add(table, database);
        }
        Ok(())
    }
}

/// Walks a data statement, recording the base table each relation it reads
/// resolves to and stopping at the first resolution error.
struct RelationCollector<'db, 'acc, DB: DatabaseLike> {
    database: &'db DB,
    accumulator: &'acc mut ReferencedTables<'db, DB>,
    scopes: WithScopes,
    /// The mutation targets met so far, compared by address.
    targets: Vec<*const ObjectName>,
}

impl<DB: DatabaseLike> RelationCollector<'_, '_, DB> {
    /// Records the names a mutation writes to. PostgreSQL resolves them
    /// against stored tables only, so a `WITH` item of the same name does not
    /// hide them.
    fn record_targets(&mut self, statement: &Statement) {
        let relation_name = |factor: &TableFactor| {
            match factor {
                TableFactor::Table { name, .. } => Some(&raw const *name),
                _ => None,
            }
        };
        match statement {
            Statement::Insert(Insert { table: TableObject::TableName(name), .. }) => {
                self.targets.push(name);
            }
            Statement::Update(Update { table, .. }) => {
                self.targets.extend(relation_name(&table.relation));
            }
            // With a multi-table list the `FROM` items are the rows read, and
            // the list holds the targets.
            Statement::Delete(Delete { tables, from, .. }) if tables.is_empty() => {
                let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
                self.targets.extend(from.iter().filter_map(|item| relation_name(&item.relation)));
            }
            _ => {}
        }
    }
}

impl<DB: DatabaseLike> Visitor for RelationCollector<'_, '_, DB> {
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

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        if self.scopes.reads_item(relation)
            && !self.targets.iter().any(|target| core::ptr::eq(*target, relation))
        {
            return ControlFlow::Continue(());
        }
        match resolve_object_name(relation, self.database, IdentifierCase::AsWritten) {
            Ok(Some(table)) => {
                self.accumulator.add(table, self.database);
                ControlFlow::Continue(())
            }
            Ok(None) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
}

/// Resolves every relation reachable from `node` and records the base tables,
/// propagating the first resolution error.
fn collect_referenced_tables<'db, N: Visit, DB: DatabaseLike>(
    node: &N,
    database: &'db DB,
    accumulator: &mut ReferencedTables<'db, DB>,
) -> Result<(), LookupError> {
    let mut collector = RelationCollector {
        database,
        accumulator,
        scopes: WithScopes::default(),
        targets: Vec::new(),
    };
    match node.visit(&mut collector) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Query {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        let mut accumulator = ReferencedTables::new();
        collect_referenced_tables(self, database, &mut accumulator)?;
        Ok(accumulator.tables)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Insert {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        let mut accumulator = ReferencedTables::new();
        collect_referenced_tables(self, database, &mut accumulator)?;
        Ok(accumulator.tables)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Update {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        let mut accumulator = ReferencedTables::new();
        collect_referenced_tables(self, database, &mut accumulator)?;
        Ok(accumulator.tables)
    }
}

impl<DB: DatabaseLike> DataStatementLike<DB> for Delete {
    fn referenced_tables<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<&'db DB::Table>, LookupError> {
        let mut accumulator = ReferencedTables::new();
        collect_referenced_tables(self, database, &mut accumulator)?;
        // `visit_relations` does not descend into the MySQL multi-table
        // `DELETE t1, t2 FROM ...` list (a `Vec<ObjectName>` rather than an
        // annotated relation), so resolve those targets explicitly.
        for object_name in &self.tables {
            accumulator.resolve_and_add(object_name, database)?;
        }
        Ok(accumulator.tables)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec::Vec};

    use sqlparser::{
        ast::Statement,
        dialect::{GenericDialect, MySqlDialect},
        parser::Parser,
    };

    use crate::{
        errors::LookupError,
        prelude::ParserDB,
        traits::{DataStatementLike, TableLike},
    };

    const SCHEMA: &str = "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
        CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT);
        CREATE TABLE audit (id INT PRIMARY KEY, note TEXT);
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
        // a base table and is skipped.
        assert_eq!(referenced_names(as_query(&statement), &db), vec!["orders".to_string()]);
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
    fn unknown_relation_is_skipped() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM nonexistent_table");
        assert_eq!(referenced_names(as_query(&statement), &db), Vec::<String>::new());
    }

    #[test]
    fn insert_select_includes_target_and_source() {
        let db = schema_db();
        let statement = parse_one("INSERT INTO audit (note) SELECT name FROM users");
        let Statement::Insert(insert) = &statement else { panic!("expected insert") };
        assert_eq!(referenced_names(insert, &db), vec!["audit".to_string(), "users".to_string()]);
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
    fn overqualified_relation_propagates_resolver_error() {
        let db = schema_db();
        let statement = parse_one("SELECT * FROM a.b.c");
        let result = as_query(&statement).referenced_tables(&db);
        assert!(matches!(result, Err(LookupError::InvalidObjectName { .. })), "got {result:?}");
    }
}
