//! Submodule providing a trait for describing SQL Trigger-like entities.

use alloc::vec::Vec;
use core::fmt::Debug;

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{DatabaseLike, FunctionLike, Metadata},
    utils::maintenance_trigger_parser::{MaintenanceBodyError, parse_maintenance_body},
};

/// A trait for types that can be treated as SQL triggers.
pub trait TriggerLike: Clone + Debug + Metadata + Send + Sync {
    /// The database type the trigger belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the trigger.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert_eq!(trigger.name(), "my_trigger");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert_eq!(trigger.name(), "my_trigger");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns the table the trigger is associated with, the one its target
    /// reached when the trigger was created.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::TableNotFound`] when no table matches the target,
    /// and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot denote
    /// a single table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.\"MyTable\" (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON app.\"MyTable\"
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let table = trigger.table(&db)?;
    /// assert_eq!(table.table_name(), "MyTable");
    /// assert_eq!(table.table_schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// let table = trigger.table(&db)?;
    /// assert_eq!(table.table_name(), "my_table");
    /// # Ok(())
    /// # }
    /// ```
    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db;

    /// Returns the table name the trigger targets, as ingestion bound it.
    ///
    /// The name carries the qualifier of the table it reached when the trigger
    /// was created, or none for a table in the default schema, and keeps the
    /// quoting written on the table name. Unlike [`Self::table`] this applies
    /// no resolution and cannot fail, so a caller with its own resolution rules
    /// can read the target and resolve it itself.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.\"MyTable\" (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON app.\"MyTable\"
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let target = trigger.target_table_name();
    /// assert_eq!(target.name(), "MyTable");
    /// assert!(target.name_is_quoted());
    /// assert_eq!(target.schema(), Some("app"));
    /// assert!(!target.schema_is_quoted());
    /// assert_eq!(target.to_string(), "app.\"MyTable\"");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// A target in the default schema reads back with no qualifier:
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let target = trigger.target_table_name();
    /// assert_eq!(target.name(), "my_table");
    /// assert_eq!(target.schema(), None);
    /// # Ok(())
    /// # }
    /// ```
    fn target_table_name(&self) -> TargetName<'_>;

    /// Returns the events that fire the trigger.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT OR UPDATE ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let events = trigger.events();
    /// assert_eq!(events.len(), 2);
    /// assert!(matches!(events[0], sqlparser::ast::TriggerEvent::Insert));
    /// assert!(matches!(events[1], sqlparser::ast::TriggerEvent::Update(_)));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT, col1 INT);
    /// CREATE TRIGGER my_trigger
    /// AFTER UPDATE OF col1 ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// let events = trigger.events();
    /// assert_eq!(events.len(), 1);
    /// assert!(matches!(events[0], sqlparser::ast::TriggerEvent::Update(_)));
    /// # Ok(())
    /// # }
    /// ```
    fn events(&self) -> &[sqlparser::ast::TriggerEvent];

    /// Returns the timing of the trigger (BEFORE, AFTER, INSTEAD OF).
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert!(matches!(trigger.timing(), Some(sqlparser::ast::TriggerPeriod::After)));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TRIGGER my_trigger
    /// BEFORE INSERT ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert!(matches!(trigger.timing(), Some(sqlparser::ast::TriggerPeriod::Before)));
    /// # Ok(())
    /// # }
    /// ```
    fn timing(&self) -> Option<sqlparser::ast::TriggerPeriod>;

    /// Returns the orientation of the trigger (ROW, STATEMENT).
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert!(matches!(
    ///     trigger.orientation(),
    ///     Some(sqlparser::ast::TriggerObjectKind::ForEach(sqlparser::ast::TriggerObject::Row))
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert!(matches!(
    ///     trigger.orientation(),
    ///     Some(sqlparser::ast::TriggerObjectKind::ForEach(sqlparser::ast::TriggerObject::Row))
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn orientation(&self) -> Option<sqlparser::ast::TriggerObjectKind>;

    /// Returns the function the trigger executes.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   function from.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE FUNCTION my_function() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// EXECUTE FUNCTION my_function();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let function = trigger.function(&db).unwrap();
    /// assert_eq!(function.name(), "my_function");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::SQLiteDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TRIGGER my_trigger
    /// AFTER INSERT ON my_table
    /// FOR EACH ROW
    /// BEGIN
    ///     UPDATE my_table SET id = id + 1;
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// // SQLite triggers do not call a function object.
    /// assert!(trigger.function(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>
    where
        Self: 'db;

    /// Returns the name of the function the trigger executes, if any.
    ///
    /// This method returns just the function name string without requiring
    /// a database reference, making it useful for dependency checking during
    /// schema construction.
    ///
    /// Returns `None` for triggers that don't execute a function (e.g., SQLite
    /// triggers with inline statements).
    fn function_name(&self) -> Option<&str>;

    /// Returns the trigger function identifier and its quotedness.
    ///
    /// The default implementation falls back to [`Self::function_name`]
    /// and treats it as unquoted.
    #[inline]
    fn function_name_ident(&self) -> Option<(&str, bool)> {
        self.function_name().map(|name| (name, false))
    }

    /// Returns whether the trigger is a maintenance trigger.
    ///
    /// A maintenance trigger is defined as a trigger that solely consists of
    /// updating values in `NEW.{column_name} = ...` and concludes by
    /// returning `NEW`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// // Example of a maintenance trigger
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE brands (id INT, edited_at TIMESTAMP);
    /// CREATE OR REPLACE FUNCTION update_brands_edited_at() RETURNS TRIGGER AS $$
    /// BEGIN
    ///     NEW.edited_at = CURRENT_TIMESTAMP;
    ///     RETURN NEW;
    /// END;
    /// $$ LANGUAGE plpgsql;
    ///
    /// CREATE TRIGGER trigger_update_brands_edited_at
    /// BEFORE UPDATE ON brands
    /// FOR EACH ROW EXECUTE FUNCTION update_brands_edited_at();
    /// ",
    /// )?;
    ///
    /// let trigger = db.triggers().next().unwrap();
    /// assert!(trigger.is_maintenance_trigger(&db)?);
    ///
    /// // Example of a non-maintenance trigger (extra logic)
    /// let db2 = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE brands (id INT);
    /// CREATE OR REPLACE FUNCTION complex_trigger() RETURNS TRIGGER AS $$
    /// BEGIN
    ///     IF NEW.id > 10 THEN
    ///         NEW.id = 10;
    ///     END IF;
    ///     RETURN NEW;
    /// END;
    /// $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER complex
    /// BEFORE UPDATE ON brands
    /// FOR EACH ROW EXECUTE FUNCTION complex_trigger();
    /// ",
    /// )?;
    /// let complex = db2.triggers().next().unwrap();
    /// assert!(!complex.is_maintenance_trigger(&db2)?);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::TableNotFound`] when no table matches the target,
    /// and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot denote
    /// a single table. The assigned columns are resolved against that table, so
    /// the question cannot be decided without it.
    #[inline]
    fn is_maintenance_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<bool, LookupError> {
        let Some(function) = self.function(database) else {
            return Ok(false);
        };
        let Some(body) = function.body() else {
            return Ok(false);
        };
        let table = self.table(database)?;

        match parse_maintenance_body(body, table, database) {
            Ok(_) => Ok(true),
            Err(MaintenanceBodyError::NotMaintenanceBody) => Ok(false),
            Err(MaintenanceBodyError::Lookup(error)) => Err(error),
        }
    }

    /// Returns the assignments in a maintenance trigger.
    /// Returns iterator of (column, expression_ast).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE brands (id INT, edited_at TIMESTAMP, name TEXT);
    /// CREATE OR REPLACE FUNCTION update_stuff() RETURNS TRIGGER AS $$
    /// BEGIN
    ///     NEW.edited_at = CURRENT_TIMESTAMP;
    ///     NEW.name = lower(NEW.name);
    ///     RETURN NEW;
    /// END;
    /// $$ LANGUAGE plpgsql;
    ///
    /// CREATE TRIGGER trigger_update
    /// BEFORE UPDATE ON brands
    /// FOR EACH ROW EXECUTE FUNCTION update_stuff();
    /// ",
    /// )?;
    ///
    /// let trigger = db.triggers().next().unwrap();
    /// let assignments: Vec<_> = trigger.maintenance_assignments(&db)?.collect();
    /// let brands_table =
    ///     db.table_by_target(TargetName::new("brands", false), IdentifierCase::AsWritten)?.unwrap();
    /// let edited_at_column =
    ///     brands_table.column("edited_at", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let name_column = brands_table.column("name", &db, IdentifierCase::AsWritten)?.unwrap();
    ///
    /// assert_eq!(assignments.len(), 2);
    /// assert_eq!(assignments[0].0, edited_at_column);
    /// assert_eq!(assignments[0].1.to_string(), "CURRENT_TIMESTAMP");
    /// assert_eq!(assignments[1].0, name_column);
    /// assert_eq!(assignments[1].1.to_string(), "lower(NEW.name)");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::TableNotFound`] when no table matches the target,
    /// and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot denote
    /// a single table. The assigned columns are resolved against that table.
    #[inline]
    fn maintenance_assignments<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = (&'db <Self::DB as DatabaseLike>::Column, sqlparser::ast::Expr)>,
        LookupError,
    > {
        let Some(function) = self.function(database) else {
            return Ok(Vec::new().into_iter());
        };
        let Some(body) = function.body() else {
            return Ok(Vec::new().into_iter());
        };
        let table = self.table(database)?;

        match parse_maintenance_body(body, table, database) {
            Ok(assignments) => Ok(assignments.into_iter()),
            Err(MaintenanceBodyError::NotMaintenanceBody) => Ok(Vec::new().into_iter()),
            Err(MaintenanceBodyError::Lookup(error)) => Err(error),
        }
    }
}

impl<T: TriggerLike> TriggerLike for &T {
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db,
    {
        (*self).table(database)
    }

    fn target_table_name(&self) -> TargetName<'_> {
        (*self).target_table_name()
    }

    fn events(&self) -> &[sqlparser::ast::TriggerEvent] {
        (*self).events()
    }

    fn timing(&self) -> Option<sqlparser::ast::TriggerPeriod> {
        (*self).timing()
    }

    fn orientation(&self) -> Option<sqlparser::ast::TriggerObjectKind> {
        (*self).orientation()
    }

    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>
    where
        Self: 'db,
    {
        (*self).function(database)
    }

    fn function_name(&self) -> Option<&str> {
        (*self).function_name()
    }

    fn function_name_ident(&self) -> Option<(&str, bool)> {
        (*self).function_name_ident()
    }

    fn is_maintenance_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<bool, LookupError> {
        (*self).is_maintenance_trigger(database)
    }

    fn maintenance_assignments<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = (&'db <Self::DB as DatabaseLike>::Column, sqlparser::ast::Expr)>,
        LookupError,
    > {
        (*self).maintenance_assignments(database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::{
        structs::ParserDB,
        traits::{ColumnLike, DatabaseLike, FunctionLike, TableLike},
    };

    #[test]
    fn test_trigger_ref_implementation() {
        let sql = r"
            CREATE TABLE users (id INT, updated_at TIMESTAMP);
            CREATE FUNCTION update_timestamp() RETURNS TRIGGER AS $$
            BEGIN
                NEW.updated_at = CURRENT_TIMESTAMP;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            CREATE TRIGGER my_trigger
            BEFORE UPDATE ON users
            FOR EACH ROW
            EXECUTE FUNCTION update_timestamp();
        ";

        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("No trigger found");

        // Use reference to trigger
        let trigger_ref = &trigger;

        assert_eq!(trigger_ref.name(), "my_trigger");

        let table = trigger_ref.table(&db).expect("Table not found");
        assert_eq!(table.table_name(), "users");

        let events = trigger_ref.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], sqlparser::ast::TriggerEvent::Update(_)));

        assert!(matches!(trigger_ref.timing(), Some(sqlparser::ast::TriggerPeriod::Before)));

        assert!(matches!(
            trigger_ref.orientation(),
            Some(sqlparser::ast::TriggerObjectKind::ForEach(sqlparser::ast::TriggerObject::Row))
        ));

        let function = trigger_ref.function(&db).expect("Function should exist");
        assert_eq!(function.name(), "update_timestamp");

        assert!(trigger_ref.is_maintenance_trigger(&db).expect("maintenance check"));

        let assignments = trigger_ref
            .maintenance_assignments(&db)
            .expect("maintenance assignments")
            .collect::<Vec<_>>();
        assert_eq!(assignments.len(), 1);

        let (col, expr) = &assignments[0];
        assert_eq!(col.column_name(), "updated_at");
        assert_eq!(expr.to_string(), "CURRENT_TIMESTAMP");
    }

    #[test]
    fn test_trigger_missing_function() {
        use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

        let sql = r"
            CREATE TRIGGER my_trigger
            BEFORE UPDATE ON users
            FOR EACH ROW
            EXECUTE FUNCTION non_existent_function();
        ";
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).expect("Parse SQL");
        let statement = statements.pop().unwrap();

        let sqlparser::ast::Statement::CreateTrigger(trigger) = statement else {
            panic!("Expected CreateTrigger")
        };

        // Create a separate DB that doesn't have the function
        let db = ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT);")
            .expect("Failed to create DB");

        // function() should return None because "non_existent_function" is not
        // in db
        assert!(trigger.function(&db).is_none());
        assert!(!trigger.is_maintenance_trigger(&db).expect("maintenance check"));
        assert_eq!(
            trigger.maintenance_assignments(&db).expect("maintenance assignments").count(),
            0
        );
    }

    #[test]
    fn test_trigger_function_no_body() {
        // Defines a function with RETURN expression which is not a string
        // literal block. FunctionLike implementation returns None for
        // body() in this case.
        let sql = r"
            CREATE TABLE users (id INT, val INT);
            CREATE FUNCTION atomic_calc() RETURNS INT RETURN 1;
            
            CREATE TRIGGER my_trigger
            BEFORE UPDATE ON users
            FOR EACH ROW
            EXECUTE FUNCTION atomic_calc();
        ";

        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("No trigger found");
        let trigger_ref = &trigger;

        // Function exists
        assert!(trigger_ref.function(&db).is_some());

        // But body() is None because it's not a string literal body (internal
        // logic of impls/sqlparser/create_function.rs)
        assert!(trigger_ref.function(&db).unwrap().body().is_none());

        assert!(!trigger_ref.is_maintenance_trigger(&db).expect("maintenance check"));
        assert_eq!(
            trigger_ref.maintenance_assignments(&db).expect("maintenance assignments").count(),
            0
        );
    }

    #[test]
    fn test_trigger_on_schema_qualified_table_resolves() {
        let sql = r"
            CREATE SCHEMA app;
            CREATE TABLE app.docs (id INT);
            CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER tg AFTER INSERT ON app.docs FOR EACH ROW EXECUTE FUNCTION audit();
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("Trigger not found");
        let table = trigger.table(&db).expect("Trigger target should resolve");

        assert_eq!(table.table_name(), "docs");
        assert_eq!(table.table_schema(), Some("app"));
    }

    #[test]
    fn test_trigger_on_quoted_table_resolves() {
        let sql = r#"
            CREATE TABLE "MyTable" (id INT);
            CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER tg AFTER INSERT ON "MyTable" FOR EACH ROW EXECUTE FUNCTION audit();
        "#;
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("Trigger not found");
        let table = trigger.table(&db).expect("Trigger target should resolve");

        assert_eq!(table.table_name(), "MyTable");
        assert_eq!(table.table_schema(), None);
    }

    #[test]
    fn test_trigger_follows_a_table_rename() {
        let sql = r"
            CREATE TABLE users (id INT);
            CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER tg AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit();
            ALTER TABLE users RENAME TO people;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("Trigger not found");
        let table = trigger.table(&db).expect("the rename carried the trigger along");

        assert_eq!(table.table_name(), "people");
    }

    #[test]
    fn test_trigger_table_reports_a_target_absent_from_the_database() {
        // A trigger answers about whichever database it is asked, so different
        // input is how the absent target is reached now that a rename carries
        // the trigger along.
        let with_trigger = ParserDB::parse::<GenericDialect>(
            r"
            CREATE TABLE users (id INT);
            CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER tg AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit();
        ",
        )
        .expect("Failed to parse SQL");
        let elsewhere = ParserDB::parse::<GenericDialect>("CREATE TABLE people (id INT);")
            .expect("Failed to parse SQL");
        let trigger = with_trigger.triggers().next().expect("Trigger not found");

        assert_eq!(
            trigger.table(&elsewhere).err(),
            Some(LookupError::TableNotFound { object_name: "users".to_string() })
        );
    }
}
