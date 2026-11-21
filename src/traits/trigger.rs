//! Submodule providing a trait for describing SQL Trigger-like entities.

use core::fmt::Debug;

use crate::{
    errors::LookupError,
    impls::maintenance_body,
    structs::{
        ConstraintDeferral, MaintenanceRefusal, TargetName, TriggerEnablement, UndeclaredRowField,
    },
    traits::{DatabaseLike, Metadata, Relation},
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

    /// Returns the table or view the trigger is defined on, the one its
    /// target reached when the trigger was created.
    ///
    /// A trigger on a table answers [`Relation::Table`], one on a foreign
    /// table [`Relation::ForeignTable`], and one on a plain view, such as the
    /// `INSTEAD OF` triggers that make a view writable, [`Relation::View`].
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   relation from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::RelationNotFound`] when neither a table nor a
    /// view matches the target, and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot
    /// denote a single relation.
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
    /// let Relation::Table(table) = trigger.relation(&db)? else { panic!("a table") };
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
    /// CREATE VIEW my_view AS SELECT id FROM my_table;
    /// CREATE TRIGGER my_trigger
    /// INSTEAD OF INSERT ON my_view
    /// BEGIN
    ///     INSERT INTO my_table (id) VALUES (NEW.id);
    /// END;
    /// ";
    ///
    /// let dialect = SQLiteDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql)?;
    /// let db = ParserDB::from_statements(statements, "test".to_string())?;
    /// let trigger = db.triggers().next().unwrap();
    /// let Relation::View(view) = trigger.relation(&db)? else { panic!("a view") };
    /// assert_eq!(view.view_name(), "my_view");
    /// # Ok(())
    /// # }
    /// ```
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db;

    /// Returns the relation name the trigger targets, as ingestion bound it.
    ///
    /// The name carries the qualifier of the table or view it reached when the
    /// trigger was created, or none for a relation in the default schema, and
    /// keeps the quoting written on the name. Unlike [`Self::relation`] this
    /// applies no resolution and cannot fail, so a caller with its own
    /// resolution rules can read the target and resolve it itself.
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
    /// let target = trigger.target_relation_name();
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
    /// let target = trigger.target_relation_name();
    /// assert_eq!(target.name(), "my_table");
    /// assert_eq!(target.schema(), None);
    /// # Ok(())
    /// # }
    /// ```
    fn target_relation_name(&self) -> TargetName<'_>;

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

    /// Returns the `WHEN` condition that decides whether the trigger fires,
    /// or `None` for a trigger without one.
    ///
    /// The condition comes without the parentheses PostgreSQL requires around
    /// it, so `WHEN (OLD.* IS DISTINCT FROM NEW.*)` reads back as
    /// `OLD.* IS DISTINCT FROM NEW.*`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE notes (id INT PRIMARY KEY, body TEXT NOT NULL);
    /// CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER filtered AFTER UPDATE ON notes FOR EACH ROW
    ///     WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION touch();
    /// CREATE TRIGGER unfiltered AFTER UPDATE ON notes FOR EACH ROW EXECUTE FUNCTION touch();
    /// ",
    /// )?;
    /// let conditions: Vec<(&str, Option<String>)> = db
    ///     .triggers()
    ///     .map(|trigger| (trigger.name(), trigger.condition().map(ToString::to_string)))
    ///     .collect();
    /// assert_eq!(
    ///     conditions,
    ///     [("filtered", Some("OLD.* IS DISTINCT FROM NEW.*".to_owned())), ("unfiltered", None)]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # SQLite Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::SQLiteDialect;
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>(
    ///     "
    /// CREATE TABLE notes (id INT, body TEXT);
    /// CREATE TABLE audit (id INT);
    /// CREATE TRIGGER filtered
    /// AFTER UPDATE ON notes
    /// FOR EACH ROW WHEN NEW.body <> OLD.body
    /// BEGIN
    ///     INSERT INTO audit VALUES (NEW.id);
    /// END;
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// assert_eq!(trigger.condition().unwrap().to_string(), "NEW.body <> OLD.body");
    /// # Ok(())
    /// # }
    /// ```
    fn condition(&self) -> Option<&sqlparser::ast::Expr>;

    /// Returns the trigger this one is the partition's copy of.
    ///
    /// PostgreSQL copies a `FOR EACH ROW` trigger on a partitioned table onto
    /// each of its partitions, under the same name, and the copies on a
    /// partitioned partition onto its own partitions in turn. The copy is
    /// answered with the trigger on the partitioned table directly above it,
    /// and any other trigger with [`None`].
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance holding the trigger.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this trigger, and a [`LookupError`] when the partition holding a
    /// copy, or the table above it, cannot be looked up.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE events (id INT) PARTITION BY RANGE (id);
    /// CREATE TABLE events_low PARTITION OF events FOR VALUES FROM (0) TO (10);
    /// CREATE FUNCTION audit() RETURNS TRIGGER AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql;
    /// CREATE TRIGGER audit AFTER INSERT ON events FOR EACH ROW EXECUTE FUNCTION audit();
    /// ",
    /// )?;
    /// let low = db
    ///     .table_by_target(TargetName::new("events_low", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let copy = low.triggers(&db)?.next().unwrap();
    /// let parent = copy.parent_trigger(&db)?.unwrap();
    /// assert_eq!(parent.name(), "audit");
    /// assert_eq!(parent.target_relation_name().name(), "events");
    /// assert!(parent.parent_trigger(&db)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn parent_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Trigger>, LookupError>
    where
        Self: 'db;

    /// Returns which sessions the trigger fires in, as `pg_trigger.tgenabled`
    /// records it.
    ///
    /// A trigger is created [`TriggerEnablement::Origin`]. `ALTER TABLE`
    /// changes it with `DISABLE TRIGGER`, `ENABLE TRIGGER`, `ENABLE REPLICA
    /// TRIGGER` and `ENABLE ALWAYS TRIGGER`, naming one trigger or all of the
    /// table's with `ALL` or `USER`. Without `ONLY` the change reaches the
    /// copies of a row trigger on the partitions below, each of which keeps
    /// its own state otherwise.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance holding the trigger.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this trigger.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql;
    /// CREATE TRIGGER audit AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
    /// CREATE TRIGGER other AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
    /// ALTER TABLE t DISABLE TRIGGER ALL;
    /// ALTER TABLE t ENABLE ALWAYS TRIGGER audit;
    /// ",
    /// )?;
    /// let t = db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let enablement = |name| db.trigger(name, t).unwrap().enablement(&db);
    /// assert_eq!(enablement("audit")?, TriggerEnablement::Always);
    /// assert_eq!(enablement("other")?, TriggerEnablement::Disabled);
    /// # Ok(())
    /// # }
    /// ```
    fn enablement(&self, database: &Self::DB) -> Result<TriggerEnablement, LookupError>;

    /// Returns when a constraint trigger fires, or [`None`] for an ordinary
    /// trigger, as `pg_trigger.tgconstraint`, `tgdeferrable` and
    /// `tginitdeferred` record it.
    ///
    /// Only `CREATE CONSTRAINT TRIGGER` makes a constraint trigger, and only
    /// a constraint trigger may declare `DEFERRABLE` and `INITIALLY`, so an
    /// ordinary trigger has no deferral. The copies of a constraint trigger on
    /// the partitions below its table answer the deferral of the trigger they
    /// copy.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE parents (id INT PRIMARY KEY);
    ///      CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
    ///      CREATE CONSTRAINT TRIGGER deferred AFTER INSERT ON parents
    ///        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION f();
    ///      CREATE CONSTRAINT TRIGGER immediate AFTER INSERT ON parents
    ///        DEFERRABLE FOR EACH ROW EXECUTE FUNCTION f();
    ///      CREATE CONSTRAINT TRIGGER fixed AFTER INSERT ON parents
    ///        FOR EACH ROW EXECUTE FUNCTION f();
    ///      CREATE TRIGGER plain AFTER INSERT ON parents FOR EACH ROW EXECUTE FUNCTION f();",
    /// )?;
    /// let deferrals: Vec<(&str, Option<ConstraintDeferral>)> =
    ///     db.triggers().map(|trigger| (trigger.name(), trigger.deferral())).collect();
    /// assert_eq!(
    ///     deferrals,
    ///     [
    ///         ("deferred", Some(ConstraintDeferral::InitiallyDeferred)),
    ///         ("fixed", Some(ConstraintDeferral::NotDeferrable)),
    ///         ("immediate", Some(ConstraintDeferral::InitiallyImmediate)),
    ///         ("plain", None),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn deferral(&self) -> Option<ConstraintDeferral>;

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
    /// A maintenance trigger's PL/pgSQL body solely assigns fields of `NEW`,
    /// alone or under `IF`, `ELSIF` and `ELSE`, and concludes by returning
    /// `NEW`. Any other statement, an early `RETURN` included, makes it none.
    /// A field the table does not declare keeps the body a maintenance body,
    /// since PostgreSQL loads it and fails only the run reaching it, and
    /// [`TriggerLike::maintenance_undeclared_fields`] lists it.
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
    /// // A body with any other statement is no maintenance trigger
    /// let db2 = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE brands (id INT);
    /// CREATE OR REPLACE FUNCTION complex_trigger() RETURNS TRIGGER AS $$
    /// BEGIN
    ///     RAISE NOTICE 'updating %', NEW.id;
    ///     NEW.id = 10;
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
    /// Returns [`LookupError::RelationNotFound`] when neither a table nor a
    /// view matches the target, and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot
    /// denote a single relation. The assigned columns are resolved against
    /// the table the trigger is on, so the question cannot be decided without
    /// it. A trigger on a view is no maintenance trigger, as
    /// [`MaintenanceRefusal::OnView`] says.
    #[inline]
    fn is_maintenance_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(maintenance_body(self, database)?.is_ok())
    }

    /// Returns what makes the trigger no maintenance trigger, as
    /// [`TriggerLike::is_maintenance_trigger`] describes one, or [`None`] for
    /// a maintenance trigger.
    ///
    /// The first part of the body, in the order written, that a maintenance
    /// body may not hold decides, quoted with where the body writes it, so a
    /// consumer refusing the trigger can name the statement that makes it
    /// refuse. [`MaintenanceRefusal`] lists every answer.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (n INT);
    ///      CREATE FUNCTION maintain() RETURNS TRIGGER LANGUAGE plpgsql AS $$
    ///      BEGIN NEW.n := 1; RAISE NOTICE 'set'; RETURN NEW; END $$;
    ///      CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION maintain();",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let Some(MaintenanceRefusal::Statement(raise)) = trigger.maintenance_refusal(&db)? else {
    ///     panic!("RAISE refuses the body");
    /// };
    /// assert_eq!(raise.text(), "RAISE NOTICE 'set';");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the errors [`TriggerLike::is_maintenance_trigger`] returns.
    #[inline]
    fn maintenance_refusal<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<MaintenanceRefusal>, LookupError> {
        Ok(maintenance_body(self, database)?.err())
    }

    /// Returns the assignments in a maintenance trigger, as
    /// [`TriggerLike::is_maintenance_trigger`] describes one, each assigned
    /// column with the expression it holds when the body returns `NEW`, in
    /// the order the body first assigns them.
    ///
    /// An `IF` folds into a `CASE` over its conditions in order, whose arms
    /// are the values each branch assigns and whose `ELSE` is the column's
    /// value before the `IF`. A column read through `NEW` after the body
    /// assigned it reads the assigned expression, so every expression reads
    /// `NEW` as the row was before the trigger ran. The body of a trigger
    /// that is no maintenance trigger yields nothing.
    ///
    /// An assignment to a field the table does not declare is none of them,
    /// and an expression keeps a read of such a field as written. The runs
    /// these assignments describe are those reaching no naming of such a
    /// field, which [`TriggerLike::maintenance_undeclared_fields`] lists.
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
    ///
    /// // Columns assigned under `IF` fold into a `CASE`
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE brands (id INT);
    /// CREATE OR REPLACE FUNCTION clamp_id() RETURNS TRIGGER AS $$
    /// BEGIN
    ///     IF NEW.id > 10 THEN
    ///         NEW.id = 10;
    ///     END IF;
    ///     RETURN NEW;
    /// END;
    /// $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER clamp
    /// BEFORE UPDATE ON brands
    /// FOR EACH ROW EXECUTE FUNCTION clamp_id();
    /// ",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let assignments: Vec<_> = trigger.maintenance_assignments(&db)?.collect();
    /// assert_eq!(assignments.len(), 1);
    /// assert_eq!(assignments[0].0.column_name(), "id");
    /// assert_eq!(assignments[0].1.to_string(), "CASE WHEN NEW.id > 10 THEN 10 ELSE NEW.id END");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::RelationNotFound`] when neither a table nor a
    /// view matches the target, and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot
    /// denote a single relation. The assigned columns are resolved against
    /// the table the trigger is on.
    #[inline]
    fn maintenance_assignments<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = (&'db <Self::DB as DatabaseLike>::Column, sqlparser::ast::Expr)>,
        LookupError,
    > {
        Ok(maintenance_body(self, database)?
            .map(|body| body.assignments)
            .unwrap_or_default()
            .into_iter())
    }

    /// Returns each field of `NEW` or `OLD` that the body of a maintenance
    /// trigger, as [`TriggerLike::is_maintenance_trigger`] describes one,
    /// assigns or reads and the trigger's table does not declare, every
    /// naming in the order PostgreSQL raises for them, which is statement by
    /// statement as written, an assignment's target before its value, and
    /// the fields of one expression from left to right.
    ///
    /// PostgreSQL loads such a body and fails each run reaching the
    /// statement, or the `IF` or `ELSIF` condition, that names one of them,
    /// with the error [`UndeclaredRowField`] describes, so a body reaching
    /// none of them runs as [`TriggerLike::maintenance_assignments`] says.
    /// The body of a trigger that is no maintenance trigger yields nothing.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// // `v` was renamed to `vv` after the function was created
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT, vv INT, w INT);
    ///      CREATE FUNCTION stamp() RETURNS TRIGGER LANGUAGE plpgsql AS $$
    ///      BEGIN
    ///        NEW.w := NEW.id;
    ///        IF NEW.id > 5 THEN NEW.v := NEW.w; END IF;
    ///        RETURN NEW;
    ///      END $$;
    ///      CREATE TRIGGER stamp BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION stamp();",
    /// )?;
    /// let trigger = db.triggers().next().unwrap();
    /// let assignments: Vec<_> = trigger.maintenance_assignments(&db)?.collect();
    /// assert_eq!(assignments.len(), 1);
    /// assert_eq!(assignments[0].0.column_name(), "w");
    /// let undeclared: Vec<_> =
    ///     trigger.maintenance_undeclared_fields(&db)?.map(|field| field.to_string()).collect();
    /// assert_eq!(undeclared, [r#"record "new" has no field "v""#]);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the errors [`TriggerLike::is_maintenance_trigger`] returns.
    #[inline]
    fn maintenance_undeclared_fields<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = UndeclaredRowField>, LookupError> {
        Ok(maintenance_body(self, database)?
            .map(|body| body.undeclared)
            .unwrap_or_default()
            .into_iter())
    }
}

impl<T: TriggerLike> TriggerLike for &T {
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        (*self).relation(database)
    }

    fn target_relation_name(&self) -> TargetName<'_> {
        (*self).target_relation_name()
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

    fn condition(&self) -> Option<&sqlparser::ast::Expr> {
        (*self).condition()
    }

    fn parent_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Trigger>, LookupError>
    where
        Self: 'db,
    {
        (*self).parent_trigger(database)
    }

    fn enablement(&self, database: &Self::DB) -> Result<TriggerEnablement, LookupError> {
        (*self).enablement(database)
    }

    fn deferral(&self) -> Option<ConstraintDeferral> {
        (*self).deferral()
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

    fn maintenance_refusal<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<MaintenanceRefusal>, LookupError> {
        (*self).maintenance_refusal(database)
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

    fn maintenance_undeclared_fields<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = UndeclaredRowField>, LookupError> {
        (*self).maintenance_undeclared_fields(database)
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
                NEW.edited_by = OLD.id;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            CREATE TRIGGER my_trigger
            BEFORE UPDATE ON users
            FOR EACH ROW WHEN (OLD.id IS DISTINCT FROM NEW.id)
            EXECUTE FUNCTION update_timestamp();
        ";

        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("No trigger found");

        // Use reference to trigger
        let trigger_ref = &trigger;

        assert_eq!(trigger_ref.name(), "my_trigger");

        assert!(matches!(
            trigger_ref.relation(&db),
            Ok(Relation::Table(table)) if table.table_name() == "users"
        ));

        let events = trigger_ref.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], sqlparser::ast::TriggerEvent::Update(_)));

        assert!(matches!(trigger_ref.timing(), Some(sqlparser::ast::TriggerPeriod::Before)));

        assert!(matches!(
            trigger_ref.orientation(),
            Some(sqlparser::ast::TriggerObjectKind::ForEach(sqlparser::ast::TriggerObject::Row))
        ));

        assert_eq!(
            trigger_ref.condition().map(ToString::to_string).as_deref(),
            Some("OLD.id IS DISTINCT FROM NEW.id")
        );

        assert_eq!(trigger_ref.enablement(&db), Ok(crate::structs::TriggerEnablement::Origin));
        assert_eq!(trigger_ref.parent_trigger(&db), Ok(None));

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

        let undeclared = trigger_ref
            .maintenance_undeclared_fields(&db)
            .expect("undeclared fields")
            .map(|field| field.to_string())
            .collect::<Vec<_>>();
        assert_eq!(undeclared, [r#"record "new" has no field "edited_by""#]);
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
    fn deferral_through_a_reference_answers_the_referenced_trigger() {
        let db = ParserDB::parse::<GenericDialect>(
            r"
            CREATE TABLE parents (id INT);
            CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NULL; END; $$ LANGUAGE plpgsql;
            CREATE CONSTRAINT TRIGGER checked AFTER INSERT ON parents
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION f();
        ",
        )
        .expect("Failed to parse SQL");
        let trigger = db.triggers().next().expect("No trigger found");

        assert_eq!(
            <&_ as TriggerLike>::deferral(&trigger),
            Some(crate::structs::ConstraintDeferral::InitiallyDeferred)
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
        assert_eq!(
            trigger_ref.maintenance_refusal(&db),
            Ok(Some(crate::structs::MaintenanceRefusal::NoBody))
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
        assert!(matches!(
            trigger.relation(&db),
            Ok(Relation::Table(table))
                if table.table_name() == "docs" && table.table_schema() == Some("app")
        ));
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
        assert!(matches!(
            trigger.relation(&db),
            Ok(Relation::Table(table))
                if table.table_name() == "MyTable" && table.table_schema().is_none()
        ));
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
        assert!(matches!(
            trigger.relation(&db),
            Ok(Relation::Table(table)) if table.table_name() == "people"
        ));
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
            trigger.relation(&elsewhere).err(),
            Some(LookupError::RelationNotFound { object_name: "users".to_string() })
        );
    }
}
