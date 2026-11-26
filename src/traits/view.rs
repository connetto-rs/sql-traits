//! Submodule providing a trait for describing SQL View-like entities.

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::{Action, Query};

use crate::{
    errors::LookupError,
    impls::dql::query_output_names,
    structs::{ActingRole, HeldPrivileges, TablePrivileges, TargetName, UpdatableViewBase},
    traits::{
        ColumnGrantLike, DatabaseLike, Metadata, Relation, RoleLike, TableGrantLike, TriggerLike,
        role::privilege_holders,
    },
    utils::{
        identifier_resolution::{is_temporary_schema, stored_identifier},
        output_from_wildcard,
    },
};

/// A trait for types that can be treated as SQL views.
///
/// A view is a relation whose rows come from a query rather than from storage,
/// so unlike a table it carries no columns of its own: the names and types it
/// exposes follow from its definition. That is the whole of what this trait
/// answers, a name and a definition, plus the optional column list a
/// `CREATE VIEW` may write to rename what the definition produces.
///
/// Two kinds implement it. A plain view runs its definition on every read. A
/// materialized view holds a stored snapshot, so its rows are not the current
/// rows of the relations underneath, which is why
/// [`DQLLike::projection_source_table`](crate::traits::DQLLike::projection_source_table)
/// refuses to answer through one while
/// [`ColumnScope::resolve_column`](crate::structs::ColumnScope::resolve_column)
/// still does: a column's declared type is inherited and cannot go stale, but a
/// snapshot's rows can.
pub trait ViewLike: Debug + Clone + Hash + Ord + Eq + Metadata + Send + Sync {
    /// The database type the view belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the view, exactly as declared.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// assert_eq!(view.view_name(), "v");
    /// # Ok(())
    /// # }
    /// ```
    fn view_name(&self) -> &str;

    /// Returns whether the view identifier was quoted in SQL.
    ///
    /// Quoted identifiers are resolved case-sensitively in PostgreSQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn view_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this view: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW My_View AS SELECT id FROM t;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("my_view", false), IdentifierCase::AsWritten)?
    ///     .expect("an unquoted name folds down");
    /// assert_eq!(view.view_name(), "My_View");
    /// assert_eq!(view.stored_view_name(), "my_view");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_view_name(&self) -> Cow<'_, str> {
        stored_identifier(self.view_name(), self.view_name_is_quoted())
    }

    /// Returns the schema the view is stored in, or `None` for one stored in
    /// the default schema without a qualifier.
    ///
    /// Ingestion records where the view landed: the schema the search path
    /// selected for a bare name, and `pg_temp` for a temporary view, or
    /// `temp` under SQLite.
    fn view_schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier of this view was quoted in SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn view_schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this view's schema.
    #[inline]
    fn stored_view_schema(&self) -> Option<Cow<'_, str>> {
        self.view_schema().map(|schema| stored_identifier(schema, self.view_schema_is_quoted()))
    }

    /// Returns whether this view lives in the session's temporary schema.
    ///
    /// PostgreSQL makes a view temporary when the statement writes `TEMP`,
    /// when it names `pg_temp`, when the search path begins there, and when
    /// the definition reads a temporary relation even though the statement
    /// wrote nothing of the kind. The model records each of those in
    /// `pg_temp`, so this reads the stored schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TEMP TABLE staging (id INT);
    ///      CREATE VIEW declared AS SELECT 1 AS id;
    ///      CREATE VIEW promoted AS SELECT id FROM staging;",
    /// )?;
    /// let declared = db
    ///     .resolve_target_view(TargetName::new("declared", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let promoted = db
    ///     .resolve_target_view(TargetName::new("promoted", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!declared.is_temporary());
    /// // Reading a temporary table made the view temporary.
    /// assert!(promoted.is_temporary());
    /// assert_eq!(promoted.view_schema(), Some("pg_temp"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_temporary(&self) -> bool {
        self.view_schema()
            .is_some_and(|schema| is_temporary_schema(schema, self.view_schema_is_quoted()))
    }

    /// Returns the view name exactly as declared, including its optional
    /// schema.
    #[inline]
    fn target_name(&self) -> TargetName<'_> {
        let name = TargetName::new(self.view_name(), self.view_name_is_quoted());
        match self.view_schema() {
            Some(schema) => name.with_schema(schema, self.view_schema_is_quoted()),
            None => name,
        }
    }

    /// Returns whether this view holds a stored snapshot of its definition's
    /// output rather than running the definition on every read.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT);
    ///      CREATE VIEW v AS SELECT id FROM t;
    ///      CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// assert!(
    ///     !db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///         .expect("plain view")
    ///         .is_materialized()
    /// );
    /// assert!(
    ///     db.materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)?
    ///         .expect("stored view")
    ///         .is_materialized()
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn is_materialized(&self) -> bool;

    /// Returns the query that defines the view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// assert_eq!(view.definition().to_string(), "SELECT id FROM t");
    /// # Ok(())
    /// # }
    /// ```
    fn definition(&self) -> &Query;

    /// Returns whether the definition as written took the view's columns from
    /// a wildcard: `*` or `t.*` in its outermost select list or in an arm of a
    /// set operation at the top, or a `TABLE t` query.
    ///
    /// PostgreSQL expands such a wildcard into the columns it reaches when the
    /// view is created, so a column added to the relation later never joins
    /// the view. A wildcard nested in a `FROM` subquery, a common table
    /// expression or a predicate does not count, since only the outer select
    /// list decides what the view exposes.
    ///
    /// The default reads [`Self::definition`], so an implementation whose
    /// definition no longer holds the wildcards it was written with, as the
    /// [`ParserDB`](crate::structs::ParserDB) views spelled out at creation,
    /// must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT, name TEXT);
    ///      CREATE VIEW starred AS SELECT * FROM t;
    ///      CREATE VIEW listed AS SELECT id, name FROM t;",
    /// )?;
    /// let view = |name| db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten);
    /// let starred = view("starred")?.expect("the view is recorded");
    /// let listed = view("listed")?.expect("the view is recorded");
    /// // Both definitions now read the same, but only one wrote the wildcard.
    /// assert_eq!(starred.definition().to_string(), listed.definition().to_string());
    /// assert!(starred.selects_wildcard());
    /// assert!(!listed.selects_wildcard());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn selects_wildcard(&self) -> bool {
        output_from_wildcard(self.definition())
    }

    /// Returns whether the relations underneath this view are checked against
    /// the rights and row-level security policies of whoever reads through it,
    /// rather than against the view owner's.
    ///
    /// PostgreSQL sets this with the `security_invoker` option of `CREATE VIEW
    /// ... WITH (...)` and of `ALTER VIEW` or `ALTER TABLE ... SET (...)`,
    /// which takes any spelling of a boolean the server reads, and reads the
    /// option named alone as `true`. A `RESET (...)` naming it turns it off.
    /// `CREATE OR REPLACE VIEW` replaces every option, so a replacement that
    /// writes none turns it off too. A materialized view takes no such option,
    /// and the default answers `false`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id INT, title TEXT);
    ///      CREATE VIEW owner_rights AS SELECT id, title FROM docs;
    ///      CREATE VIEW caller_rights WITH (security_invoker = on) AS SELECT id, title FROM docs;",
    /// )?;
    /// let view = |name| db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten);
    /// assert!(!view("owner_rights")?.expect("recorded").is_security_invoker());
    /// assert!(view("caller_rights")?.expect("recorded").is_security_invoker());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_security_invoker(&self) -> bool {
        false
    }

    /// Returns whether this view is a security barrier, so the planner applies
    /// its own conditions before any function or operator not marked
    /// `LEAKPROOF` that a reader's query adds, which then never sees the rows
    /// those conditions filter out.
    ///
    /// PostgreSQL sets this with the `security_barrier` option, on the same
    /// terms as [`Self::is_security_invoker`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id INT);
    ///      CREATE VIEW v AS SELECT id FROM docs;
    ///      ALTER TABLE v SET (security_barrier = true);",
    /// )?;
    /// let view = db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(view.is_security_barrier());
    /// assert!(!view.is_security_invoker());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_security_barrier(&self) -> bool {
        false
    }

    /// Returns the role the input hands this view to, which PostgreSQL checks
    /// the relations underneath against unless the view
    /// [runs with its reader's rights](Self::is_security_invoker).
    ///
    /// The owner is the role the view was created under, as `SET ROLE` and
    /// `SET SESSION AUTHORIZATION` leave it, until `ALTER VIEW` or `ALTER
    /// TABLE ... OWNER TO <role>` hands it to another. A view created while the
    /// input had set no role has none, and neither has one handed to
    /// `CURRENT_ROLE`, `CURRENT_USER` or `SESSION_USER`, which name whoever
    /// runs the statement rather than a role the input declares. A
    /// replacement keeps the owner.
    ///
    /// The role is reported as the statement spelled it, with no case folding,
    /// which is how [`DatabaseLike::role`] stores the names it matches against.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the view
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app_owner;
    ///      CREATE ROLE app_writer;
    ///      CREATE TABLE docs (id INT);
    ///      CREATE VIEW owned AS SELECT id FROM docs;
    ///      CREATE VIEW unowned AS SELECT id FROM docs;
    ///      ALTER TABLE owned OWNER TO app_owner;
    ///      SET ROLE app_writer;
    ///      CREATE VIEW written AS SELECT id FROM docs;",
    /// )?;
    /// let view = |name| db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten);
    /// assert_eq!(view("owned")?.expect("recorded").owner(&db)?, Some("app_owner"));
    /// assert_eq!(view("written")?.expect("recorded").owner(&db)?, Some("app_writer"));
    /// assert_eq!(view("unowned")?.expect("recorded").owner(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>;

    /// Iterates over the relation-level privileges the owner's own entry on
    /// this view holds.
    ///
    /// PostgreSQL gives the owner every table privilege when it creates the
    /// view, through an entry the owner can revoke and grant back like any
    /// other, as it does for a table. A `REVOKE` naming the
    /// [`owner`](Self::owner) takes from it, and an owner change hands it to
    /// the new owner as it stands.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app_owner;
    ///      CREATE TABLE docs (id INT);
    ///      CREATE VIEW v AS SELECT id FROM docs;
    ///      ALTER VIEW v OWNER TO app_owner;
    ///      REVOKE TRUNCATE, TRIGGER, MAINTAIN ON v FROM app_owner;",
    /// )?;
    /// let view = db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?.unwrap();
    /// let held: Vec<_> = view.owner_privileges(&db)?.map(ToString::to_string).collect();
    /// assert_eq!(held, ["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES"]);
    /// # Ok(())
    /// # }
    /// ```
    fn owner_privileges<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db Action>, LookupError>;

    /// Returns the privileges `role` holds on this view, per command and per
    /// output column, as PostgreSQL's `has_table_privilege`,
    /// `has_column_privilege` and `has_any_column_privilege` answer them.
    ///
    /// The privileges reach `role` as
    /// [`TableLike::privileges`](crate::traits::TableLike::privileges)
    /// resolves them on a table, through a grant naming the role itself,
    /// `PUBLIC` or a role it inherits from, and through the owner's own entry,
    /// as [`owner_privileges`](Self::owner_privileges) reports it, when the
    /// role or one it inherits from is the view's [`owner`](Self::owner). A
    /// superuser holds every privilege.
    ///
    /// A column is named by its position among the view's output columns,
    /// counted from zero, the order
    /// [`updatable_base`](Self::updatable_base) lists them in. A column grant
    /// holds on the output columns whose names match the names it lists, the
    /// names the declaration's column list gives or else the definition's own.
    /// One whose name cannot be worked out, as behind a call returning an
    /// unknown row type, holds no column grant.
    ///
    /// These are the privileges checked against whoever names the view in a
    /// statement. A write the view passes on to its base relation is checked
    /// there against the view's owner, or against the caller for a view that
    /// [runs with its reader's rights](Self::is_security_invoker).
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this view, and the lookup errors resolving the definition's
    /// relation and column names runs into.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app;
    ///      CREATE ROLE mover;
    ///      CREATE TABLE resources (id INT PRIMARY KEY, parent_id INT);
    ///      CREATE VIEW resource_moves AS SELECT id, parent_id FROM resources WHERE parent_id > 0;
    ///      ALTER VIEW resource_moves OWNER TO mover;
    ///      GRANT SELECT, UPDATE (parent_id) ON resource_moves TO app;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("resource_moves", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let app = view.privileges(db.role("app").unwrap(), &db)?;
    /// assert!(app.select.on_table());
    /// assert!(!app.update.on_column(0));
    /// assert!(app.update.on_column(1));
    /// assert!(!app.insert.on_any_column());
    /// assert!(!app.delete);
    ///
    /// let mover = view.privileges(db.role("mover").unwrap(), &db)?;
    /// assert!(mover.insert.on_table() && mover.update.on_table() && mover.delete);
    /// # Ok(())
    /// # }
    /// ```
    fn privileges<'db>(
        &'db self,
        role: impl Into<ActingRole<'db, <Self::DB as DatabaseLike>::Role>>,
        database: &'db Self::DB,
    ) -> Result<TablePrivileges<usize>, LookupError>
    where
        Self: 'db,
    {
        let owner = self.owner(database)?;
        let holders: Vec<&<Self::DB as DatabaseLike>::Role> = match role.into() {
            ActingRole::Declared(role) if role.is_superuser() => {
                return Ok(TablePrivileges::all());
            }
            ActingRole::Declared(role) => privilege_holders(role, database).collect(),
            ActingRole::Public => Vec::new(),
        };
        let output = output_names(self, database)?;
        let names: Vec<(&str, bool)> =
            output.iter().map(|(name, quoted)| (name.as_str(), *quoted)).collect();
        let mut held = HeldPrivileges::new(&names);

        if let Some(owner) = owner
            && holders.iter().any(|holder| holder.stored_name() == owner)
        {
            for privilege in self.owner_privileges(database)? {
                held.take(privilege, None);
            }
        }
        let target = self.target_name();
        let is_this = |relation: Relation<'_, Self::DB>| {
            match relation {
                Relation::View(view) => !self.is_materialized() && view.target_name() == target,
                Relation::MaterializedView(view) => {
                    self.is_materialized() && view.target_name() == target
                }
                Relation::Table(_) | Relation::ForeignTable(_) | Relation::Sequence(_) => false,
            }
        };
        for grant in database.table_grants().filter(|grant| grant.relations(database).any(is_this))
        {
            held.take_grant(grant, &holders, None, database);
        }
        let case = database.identifier_case();
        for grant in
            database.column_grants().filter(|grant| grant.relation(database).is_some_and(is_this))
        {
            let covered: Vec<bool> = names
                .iter()
                .map(|(name, quoted)| {
                    grant.column_names().any(|(granted, granted_quoted)| {
                        case.identifiers_match(name, *quoted, granted, granted_quoted)
                    })
                })
                .collect();
            held.take_grant(grant, &holders, Some(&covered), database);
        }
        let positions: Vec<usize> = (0..names.len()).collect();
        Ok(held.answer(&positions))
    }

    /// Returns the triggers defined on the view, such as the `INSTEAD OF`
    /// triggers that make it writable.
    ///
    /// They are the triggers `database` holds on the view of this view's
    /// name. A materialized view carries none, since PostgreSQL refuses a
    /// trigger on one.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   triggers from.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::SQLiteDialect;
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>(
    ///     "CREATE TABLE t (a INTEGER);
    ///      CREATE VIEW v AS SELECT a FROM t;
    ///      CREATE TRIGGER v_insert INSTEAD OF INSERT ON v
    ///      BEGIN INSERT INTO t (a) VALUES (NEW.a); END;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// let triggers: Vec<&str> = view.triggers(&db).map(TriggerLike::name).collect();
    /// assert_eq!(triggers, ["v_insert"]);
    /// # Ok(())
    /// # }
    /// ```
    fn triggers<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Trigger>
    where
        Self: 'db,
    {
        let target = self.target_name();
        let materialized = self.is_materialized();
        database.triggers().filter(move |trigger| {
            !materialized
                && matches!(
                    trigger.relation(database),
                    Ok(Relation::View(view)) if view.target_name() == target
                )
        })
    }

    /// Returns the relation PostgreSQL rewrites an `INSERT`, `UPDATE`,
    /// `DELETE` or `MERGE` targeting this view onto, with what each output
    /// column writes there and the condition the view puts on the write, or
    /// `None` when PostgreSQL does not make the view
    /// [automatically updatable](https://www.postgresql.org/docs/18/sql-createview.html#SQL-CREATEVIEW-UPDATABLE-VIEWS).
    ///
    /// The view is automatically updatable when its definition reads exactly
    /// one table, foreign table or plain view, without `TABLESAMPLE`, with no
    /// `WITH`, `DISTINCT`, `GROUP BY`, `HAVING`, `LIMIT`, `OFFSET`, `FETCH` or
    /// set operation at the top level and no aggregate, window function or
    /// set-returning function computed over its rows. An output column writes
    /// the column of that relation it is a simple reference to, whatever the
    /// declaration names it, and is `None` otherwise. A view none of whose
    /// columns PostgreSQL can write still answers, since a `DELETE` goes
    /// through it. The definition's `WHERE` is the condition, with the column
    /// of the relation each of its column references reads.
    ///
    /// One call follows one level. A view over a view answers with the inner
    /// view and this view's own condition, whatever the inner view's
    /// definition. An `INSTEAD OF` trigger or an `INSTEAD` rule on a view
    /// takes a write to it before this rewriting does, which the caller checks
    /// against the triggers and rules in force.
    ///
    /// A materialized view takes no write, and neither does a view under
    /// SQLite, so both answer `None`.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance holding the
    ///   relations the definition reads, whose names it reads as bound when the
    ///   view was created.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors resolving the definition's relation and
    /// column names runs into.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE invitations (id INT PRIMARY KEY, email TEXT, accepted_by_id INT);
    ///      CREATE VIEW open_invitations AS
    ///        SELECT id, lower(email) AS email, accepted_by_id AS acceptor FROM invitations AS i
    ///        WHERE i.accepted_by_id IS NULL;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("open_invitations", false), IdentifierCase::Folded)?
    ///     .expect("the view is recorded");
    /// let Some(base) = view.updatable_base(&db)? else { unreachable!("the view is updatable") };
    /// let WrittenRelation::Table { table, columns, condition_columns } = base.relation else {
    ///     unreachable!("the view writes one table")
    /// };
    /// assert_eq!(table.table_name(), "invitations");
    /// let written: Vec<_> =
    ///     columns.iter().map(|column| column.map(ColumnLike::column_name)).collect();
    /// assert_eq!(written, [Some("id"), None, Some("accepted_by_id")]);
    /// assert_eq!(
    ///     base.condition.map(ToString::to_string).as_deref(),
    ///     Some("i.accepted_by_id IS NULL")
    /// );
    /// // The `IS NULL` test is the walk's first node, and `i.accepted_by_id` its second.
    /// let read: Vec<_> =
    ///     condition_columns.iter().map(|(index, column)| (*index, column.column_name())).collect();
    /// assert_eq!(read, [(1, "accepted_by_id")]);
    /// # Ok(())
    /// # }
    /// ```
    fn updatable_base<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<UpdatableViewBase<'db, Self::DB>>, LookupError> {
        if self.is_materialized() {
            return Ok(None);
        }
        UpdatableViewBase::of_definition(self.definition(), database)
    }

    /// Returns the column names the declaration wrote, each with its quote
    /// state, or an empty slice when it wrote none.
    ///
    /// PostgreSQL applies these positionally over what the definition
    /// produces, replacing those names. A shorter list leaves the tail's own
    /// names alone, and a longer one is refused at creation.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (a INT, b INT); CREATE VIEW v (x) AS SELECT a, b FROM t;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// assert_eq!(view.declared_column_names(), &[("x".to_string(), false)]);
    /// # Ok(())
    /// # }
    /// ```
    fn declared_column_names(&self) -> &[(String, bool)];

    /// Returns the names this view exposes, in output order, each with its
    /// quote state, or `None` when they cannot be worked out.
    ///
    /// The declaration's own column list answers this outright when it covers
    /// every output column. Otherwise the definition has to be read, which a
    /// caller does through
    /// [`ColumnScope`](crate::structs::ColumnScope), so the default here
    /// answers only from what the declaration wrote.
    #[inline]
    fn declared_output_names(&self) -> Option<Vec<(String, bool)>> {
        let declared = self.declared_column_names();
        (!declared.is_empty()).then(|| declared.to_vec())
    }
}

/// The name of each output column of `view`, with its quote state, in output
/// order: the declaration's column list over the names the definition
/// produces, as PostgreSQL applies it. When the definition leaves a name
/// unknown, the declared names alone are known.
fn output_names<V: ViewLike>(
    view: &V,
    database: &V::DB,
) -> Result<Vec<(String, bool)>, LookupError> {
    let mut names = query_output_names(view.definition(), database, database.identifier_case())?
        .unwrap_or_default();
    for (position, declared) in view.declared_column_names().iter().enumerate() {
        match names.get_mut(position) {
            Some(name) => name.clone_from(declared),
            None => names.push(declared.clone()),
        }
    }
    Ok(names)
}
