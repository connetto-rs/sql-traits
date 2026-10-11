//! Submodule providing traits for describing SQL Grant-like entities.
//!
//! This module provides a hierarchy of grant traits that mirror PostgreSQL's
//! grant system:
//!
//! - [`GrantLike`]: Base trait with common grant properties (privileges,
//!   grantees, options)
//! - [`TableGrantLike`]: For table-level grants (`GRANT ... ON table`)
//! - [`ColumnGrantLike`]: For column-level grants (`GRANT ... (col1, col2) ON
//!   table`)

use core::{borrow::Borrow, fmt::Debug, hash::Hash};

use sqlparser::ast::{Action, Grantee};

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
};

/// A trait for types that can be treated as SQL grants.
///
/// This is the base trait for all grant types, containing common properties
/// like privileges, grantees, and grant options. Specific grant types
/// (table grants, column grants) extend this trait with additional methods.
///
/// Grants in SQL are used to assign privileges on database objects to
/// roles/users. A single grant represents one or more privileges on one or more
/// objects assigned to one or more grantees.
pub trait GrantLike: Debug + Clone + Hash + Ord + Eq + Metadata + Send + Sync {
    /// The database type the grant belongs to.
    type DB: DatabaseLike;

    /// Returns an iterator over the privileges (actions) granted.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::Action;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE my_role;
    /// GRANT SELECT, INSERT ON my_table TO my_role;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let privileges: Vec<_> = grant.privileges(&db).collect();
    /// assert_eq!(privileges.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn privileges<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db;

    /// Returns whether this grant represents ALL PRIVILEGES.
    ///
    /// When a grant uses `ALL PRIVILEGES`, the `privileges()` iterator
    /// will be empty. Use this method to check for that case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE admin;
    /// CREATE ROLE reader;
    /// GRANT ALL PRIVILEGES ON my_table TO admin;
    /// GRANT SELECT ON my_table TO reader;
    /// ",
    /// )?;
    /// let grants: Vec<_> = db.table_grants().collect();
    /// let all_grant = grants.iter().find(|g| g.is_all_privileges()).unwrap();
    /// let select_grant = grants.iter().find(|g| !g.is_all_privileges()).unwrap();
    /// assert!(all_grant.privileges(&db).next().is_none()); // empty for ALL
    /// assert!(select_grant.privileges(&db).next().is_some());
    /// # Ok(())
    /// # }
    /// ```
    fn is_all_privileges(&self) -> bool;

    /// Returns an iterator over the grantees (roles/users receiving the grant).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE role1;
    /// CREATE ROLE role2;
    /// GRANT SELECT ON my_table TO role1, role2;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// assert_eq!(grant.grantees(&db).count(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn grantees<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db;

    /// Returns whether the grant applies to every role.
    ///
    /// `TO PUBLIC` reaches the model in two spellings, and this reader folds
    /// both: the parser records a dedicated public grantee for the keyword,
    /// and hands back an ordinary unquoted `PUBLIC` name instead for dialects
    /// that reserve the word. Neither is safe to spot from [`Self::grantees`]
    /// without knowing that, and the second is indistinguishable there from a
    /// role somebody created called `public`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE docs (id INT);
    /// CREATE ROLE reader;
    /// GRANT SELECT ON docs TO PUBLIC;
    /// GRANT INSERT ON docs TO reader;
    /// ",
    /// )?;
    /// let grants: Vec<_> = db.table_grants().collect();
    /// assert_eq!(grants.iter().filter(|grant| grant.applies_to_public()).count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn applies_to_public(&self) -> bool;

    /// Returns the table names the grant lists, as ingestion bound them.
    ///
    /// Each carries the qualifier of the relation it reached when the grant was
    /// made, or none for one in the default schema, and keeps the quoting
    /// written on the relation name.
    ///
    /// A grant states its target in one of two ways, and only one of this
    /// reader and [`Self::target_schema_names`] ever yields: either the grant
    /// lists tables (`GRANT SELECT ON users, app.posts TO reader`), or it
    /// covers a whole schema (`GRANT SELECT ON ALL TABLES IN SCHEMA public TO
    /// reader`). A grant on anything else (a view, a function, a sequence)
    /// yields from neither, which is what [`TableGrantLike::tables`] already
    /// reports for those forms.
    ///
    /// Unlike that resolving reader this applies no resolution and cannot
    /// fail, so a caller with its own resolution rules can read the targets
    /// and resolve them itself.
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
    /// CREATE TABLE users (id INT);
    /// CREATE TABLE app.posts (id INT);
    /// CREATE ROLE reader;
    /// GRANT SELECT ON users, app.posts TO reader;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let targets: Vec<_> = grant.target_table_names().collect();
    /// assert_eq!(targets[0].name(), "users");
    /// assert_eq!(targets[0].schema(), None);
    /// assert_eq!(targets[1].name(), "posts");
    /// assert_eq!(targets[1].schema(), Some("app"));
    /// assert!(grant.target_schema_names().next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn target_table_names(&self) -> impl Iterator<Item = TargetName<'_>>;

    /// Returns the schema names of an `ALL TABLES IN SCHEMA` grant, exactly as
    /// written.
    ///
    /// Each yielded name is the schema itself, so
    /// [`TargetName::name`](crate::structs::TargetName::name) is the schema
    /// and [`TargetName::schema`](crate::structs::TargetName::schema) is the
    /// catalog qualifier when the SQL wrote one. Every other grant form yields
    /// nothing, see [`Self::target_table_names`].
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
    /// CREATE TABLE public.users (id INT);
    /// CREATE ROLE reader;
    /// GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let schemas: Vec<_> = grant.target_schema_names().collect();
    /// assert_eq!(schemas.len(), 1);
    /// assert_eq!(schemas[0].name(), "public");
    /// assert!(grant.target_table_names().next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn target_schema_names(&self) -> impl Iterator<Item = TargetName<'_>>;

    /// Returns whether this grant includes the `WITH GRANT OPTION`.
    ///
    /// When `WITH GRANT OPTION` is specified, the grantee can grant
    /// the same privileges to other roles.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE role1;
    /// CREATE ROLE role2;
    /// GRANT SELECT ON my_table TO role1 WITH GRANT OPTION;
    /// GRANT INSERT ON my_table TO role2;
    /// ",
    /// )?;
    /// let grants: Vec<_> = db.table_grants().collect();
    /// let grant_with_option = grants.iter().find(|g| g.with_grant_option()).unwrap();
    /// let grant_without_option = grants.iter().find(|g| !g.with_grant_option()).unwrap();
    /// assert!(grant_with_option.with_grant_option());
    /// assert!(!grant_without_option.with_grant_option());
    /// # Ok(())
    /// # }
    /// ```
    fn with_grant_option(&self) -> bool;

    /// Returns the role that granted this privilege, if specified.
    ///
    /// This is the `GRANTED BY` clause in PostgreSQL. Note that the `GRANTED
    /// BY` clause cannot use pseudo-roles, only actual database roles.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE admin;
    /// CREATE ROLE app_user;
    /// GRANT SELECT ON my_table TO app_user GRANTED BY admin;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let grantor = grant.granted_by(&db).unwrap();
    /// assert_eq!(grantor.name(), "admin");
    /// # Ok(())
    /// # }
    /// ```
    fn granted_by<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Role>;

    /// Returns whether this grant applies to a specific role.
    ///
    /// A grant that applies to every role applies to this one: whenever
    /// [`Self::applies_to_public`] answers `true`, so does this, for every
    /// role. An implementation that recognises `PUBLIC` in one reader and not
    /// the other reports a grant as not applying to a role it does apply to,
    /// so both must share a single notion of what "everyone" means.
    ///
    /// # Arguments
    ///
    /// * `role` - The role to check against.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE app_user;
    /// CREATE ROLE admin;
    /// CREATE ROLE other_user;
    /// GRANT SELECT ON my_table TO app_user, admin;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let app_user = db.role("app_user").unwrap();
    /// let admin = db.role("admin").unwrap();
    /// let other_user = db.role("other_user").unwrap();
    /// assert!(grant.applies_to_role(app_user));
    /// assert!(grant.applies_to_role(admin));
    /// assert!(!grant.applies_to_role(other_user));
    /// # Ok(())
    /// # }
    /// ```
    fn applies_to_role(&self, role: &<Self::DB as DatabaseLike>::Role) -> bool;
}

impl<T: GrantLike> GrantLike for &T {
    type DB = T::DB;

    fn privileges<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db,
    {
        (*self).privileges(database)
    }

    fn is_all_privileges(&self) -> bool {
        (*self).is_all_privileges()
    }

    fn grantees<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db,
    {
        (*self).grantees(database)
    }

    fn applies_to_public(&self) -> bool {
        (*self).applies_to_public()
    }

    fn target_table_names(&self) -> impl Iterator<Item = TargetName<'_>> {
        (*self).target_table_names()
    }

    fn target_schema_names(&self) -> impl Iterator<Item = TargetName<'_>> {
        (*self).target_schema_names()
    }

    fn with_grant_option(&self) -> bool {
        (*self).with_grant_option()
    }

    fn granted_by<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Role> {
        (*self).granted_by(database)
    }

    fn applies_to_role(&self, role: &<Self::DB as DatabaseLike>::Role) -> bool {
        (*self).applies_to_role(role)
    }
}

/// One relation a grant covers, saying which kind it is.
///
/// A grant reaches tables, views and materialized views alike, so a caller
/// wanting the whole picture asks [`TableGrantLike::relations`] and handles
/// each case. [`TableGrantLike::tables`] answers the narrower question, which
/// tables a grant covers, and quietly omits the views: that is the right
/// answer to that question and the wrong answer to this one.
#[derive(Clone)]
pub enum GrantRelation<'a, DB: DatabaseLike> {
    /// A table.
    Table(&'a DB::Table),
    /// A plain view.
    View(&'a DB::View),
    /// A materialized view.
    MaterializedView(&'a DB::MaterializedView),
}

impl<DB: DatabaseLike> Copy for GrantRelation<'_, DB> {}

impl<DB: DatabaseLike> Debug for GrantRelation<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Table(table) => f.debug_tuple("Table").field(table).finish(),
            Self::View(view) => f.debug_tuple("View").field(view).finish(),
            Self::MaterializedView(view) => f.debug_tuple("MaterializedView").field(view).finish(),
        }
    }
}

/// A trait for relation-level grants.
///
/// A grant applies privileges to whole relations, whether written as a direct
/// grant (`GRANT ... ON name`) or a schema-wide one (`GRANT ... ON ALL TABLES
/// IN SCHEMA`), and PostgreSQL lets both reach views and materialized views as
/// well as tables.
///
/// This trait corresponds to PostgreSQL's `role_table_grants` system view.
pub trait TableGrantLike:
    GrantLike + Borrow<<<Self as GrantLike>::DB as DatabaseLike>::TableGrant> + Send + Sync
{
    /// Returns an iterator over the tables this grant applies to.
    ///
    /// This method handles both direct table grants (`GRANT ... ON table1,
    /// table2`) and schema-wide table grants (`GRANT ... ON ALL TABLES IN
    /// SCHEMA`). Returns an empty iterator if this grant does not apply to
    /// tables.
    ///
    /// # Example
    ///
    /// Direct table grant:
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE users (id INT);
    /// CREATE TABLE posts (id INT);
    /// CREATE ROLE reader;
    /// GRANT SELECT ON users, posts TO reader;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let tables: Vec<_> = grant.tables(&db).collect();
    /// assert_eq!(tables.len(), 2);
    /// assert!(tables.iter().any(|t| t.table_name() == "users"));
    /// assert!(tables.iter().any(|t| t.table_name() == "posts"));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Schema-wide table grant:
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE SCHEMA other_schema;
    /// CREATE TABLE public.users (id INT);
    /// CREATE TABLE public.posts (id INT);
    /// CREATE TABLE other_schema.data (id INT);
    /// CREATE ROLE reader;
    /// GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let tables: Vec<_> = grant.tables(&db).collect();
    /// // Only tables in the 'public' schema are included
    /// assert_eq!(tables.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn tables<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Table>;

    /// Returns every relation this grant applies to, each saying which kind it
    /// is.
    ///
    /// This is the complete answer, where [`Self::tables`] answers the
    /// narrower question of which tables a grant covers. A caller that means
    /// "everything this grant reaches" has to ask this one, because a grant on
    /// a view is ordinary PostgreSQL and `ALL TABLES IN SCHEMA` covers views
    /// too.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::{prelude::*, traits::grant::GrantRelation};
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE VIEW v AS SELECT id FROM t;
    /// CREATE ROLE reader;
    /// GRANT SELECT ON t, v TO reader;
    /// ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// let kinds: Vec<&str> = grant
    ///     .relations(&db)
    ///     .map(|relation| {
    ///         match relation {
    ///             GrantRelation::Table(_) => "table",
    ///             GrantRelation::View(_) => "view",
    ///             GrantRelation::MaterializedView(_) => "materialized view",
    ///         }
    ///     })
    ///     .collect();
    /// assert_eq!(kinds, vec!["table", "view"]);
    /// // The narrower question still answers only the table.
    /// assert_eq!(grant.tables(&db).count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn relations<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = GrantRelation<'a, Self::DB>>;

    /// Returns whether this grant applies to a specific table.
    ///
    /// # Arguments
    ///
    /// * `table` - The table to check against.
    /// * `database` - The database context.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// CREATE TABLE table2 (id INT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT ON table1 TO app_user;
    /// ",
    /// )?;
    /// let table1 =
    ///     db.table_by_target(TargetName::new("table1", false), IdentifierCase::AsWritten)?.unwrap();
    /// let table2 =
    ///     db.table_by_target(TargetName::new("table2", false), IdentifierCase::AsWritten)?.unwrap();
    /// let grant = db.table_grants().next().unwrap();
    /// assert!(grant.applies_to_table(table1, &db));
    /// assert!(!grant.applies_to_table(table2, &db));
    /// # Ok(())
    /// # }
    /// ```
    fn applies_to_table(
        &self,
        table: &<Self::DB as DatabaseLike>::Table,
        database: &Self::DB,
    ) -> bool;
}

impl<T: TableGrantLike> TableGrantLike for &T
where
    Self: Borrow<<<T as GrantLike>::DB as DatabaseLike>::TableGrant>,
{
    fn tables<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Table> {
        (*self).tables(database)
    }

    fn relations<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = GrantRelation<'a, Self::DB>> {
        (*self).relations(database)
    }

    fn applies_to_table(
        &self,
        table: &<Self::DB as DatabaseLike>::Table,
        database: &Self::DB,
    ) -> bool {
        (*self).applies_to_table(table, database)
    }
}

/// A trait for column-level grants.
///
/// Column grants apply privileges to specific columns within a table. This
/// allows fine-grained access control where users can be granted SELECT,
/// INSERT, UPDATE, or REFERENCES on individual columns.
///
/// This trait corresponds to PostgreSQL's `role_column_grants` system view.
pub trait ColumnGrantLike:
    GrantLike + Borrow<<<Self as GrantLike>::DB as DatabaseLike>::ColumnGrant> + Send + Sync
{
    /// Returns an iterator over the columns that have privileges granted.
    ///
    /// Column-level privileges allow granting SELECT, INSERT, UPDATE, or
    /// REFERENCES on specific columns rather than the entire table.
    /// The iterator yields references to column objects from the database.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold `table`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT, name TEXT, secret TEXT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT (id, name) ON my_table TO app_user;
    /// ",
    /// )?;
    /// let grant = db.column_grants().next().unwrap();
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let columns: Vec<_> = grant.columns(table, &db)?.collect();
    /// assert_eq!(columns.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn columns<'a>(
        &'a self,
        table: &'a <Self::DB as DatabaseLike>::Table,
        database: &'a Self::DB,
    ) -> Result<impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Column>, LookupError>;

    /// Returns the table this column grant applies to.
    ///
    /// Answers [`None`] when the grant's target is a view, which PostgreSQL
    /// allows: ask [`Self::relation`] for the answer that says which kind the
    /// target is.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT, name TEXT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT (id, name) ON my_table TO app_user;
    /// ",
    /// )?;
    /// let grant = db.column_grants().next().unwrap();
    /// let table = grant.table(&db).unwrap();
    /// assert_eq!(table.table_name(), "my_table");
    /// # Ok(())
    /// # }
    /// ```
    fn table<'a>(&'a self, database: &'a Self::DB)
    -> Option<&'a <Self::DB as DatabaseLike>::Table>;

    /// Returns the relation this column grant applies to, saying which kind it
    /// is.
    ///
    /// [`Self::table`] answers the narrower question and cannot distinguish a
    /// grant on a view from a grant on nothing, because both answer [`None`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::{prelude::*, traits::grant::GrantRelation};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE VIEW v AS SELECT id FROM t;
    /// CREATE ROLE app_user;
    /// GRANT SELECT (id) ON v TO app_user;
    /// ",
    /// )?;
    /// let grant = db.column_grants().next().unwrap();
    /// assert!(grant.table(&db).is_none(), "the target is not a table");
    /// assert!(matches!(grant.relation(&db), Some(GrantRelation::View(_))));
    /// # Ok(())
    /// # }
    /// ```
    fn relation<'a>(&'a self, database: &'a Self::DB) -> Option<GrantRelation<'a, Self::DB>>;
}

impl<T: ColumnGrantLike> ColumnGrantLike for &T
where
    Self: Borrow<<<T as GrantLike>::DB as DatabaseLike>::ColumnGrant>,
{
    fn columns<'a>(
        &'a self,
        table: &'a <Self::DB as DatabaseLike>::Table,
        database: &'a Self::DB,
    ) -> Result<impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Column>, LookupError> {
        (*self).columns(table, database)
    }

    fn table<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Table> {
        (*self).table(database)
    }

    fn relation<'a>(&'a self, database: &'a Self::DB) -> Option<GrantRelation<'a, Self::DB>> {
        (*self).relation(database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::{
        structs::{IdentifierCase, ParserDB, TargetName},
        traits::{DatabaseLike, TableLike},
    };

    #[test]
    fn test_table_grant_ref_implementation() {
        let sql = r"
            CREATE TABLE my_table (id INT);
            CREATE VIEW my_view AS SELECT id FROM my_table;
            CREATE MATERIALIZED VIEW my_materialized AS SELECT id FROM my_table;
            CREATE ROLE app_user;
            GRANT SELECT, INSERT ON my_table, my_view, my_materialized
                TO app_user WITH GRANT OPTION;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let grant = db.table_grants().next().expect("Grant not found");

        // Explicitly route through `impl <Trait> for &T` so the blanket
        // forwarding bodies (lines 208–243, 351–367) are exercised under
        // tarpaulin.
        let grant_ref: &<ParserDB as DatabaseLike>::TableGrant = grant;

        let privileges: Vec<_> = <&_ as GrantLike>::privileges(&grant_ref, &db).collect();
        assert_eq!(privileges.len(), 2);

        assert!(!<&_ as GrantLike>::is_all_privileges(&grant_ref));
        assert!(<&_ as GrantLike>::with_grant_option(&grant_ref));
        assert!(<&_ as GrantLike>::granted_by(&grant_ref, &db).is_none());

        let grantees: Vec<_> = <&_ as GrantLike>::grantees(&grant_ref, &db).collect();
        assert_eq!(grantees.len(), 1);

        let table = db
            .table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table not found");
        assert!(<&_ as TableGrantLike>::applies_to_table(&grant_ref, table, &db));
        let tables: Vec<_> = <&_ as TableGrantLike>::tables(&grant_ref, &db).collect();
        assert_eq!(tables.len(), 1);

        let relations: Vec<_> = <&_ as TableGrantLike>::relations(&grant_ref, &db).collect();
        assert_eq!(relations.len(), 3);
        let rendered: Vec<_> = relations.iter().map(|relation| format!("{relation:?}")).collect();
        assert!(rendered.iter().any(|relation| relation.starts_with("Table(")));
        assert!(rendered.iter().any(|relation| relation.starts_with("View(")));
        assert!(rendered.iter().any(|relation| relation.starts_with("MaterializedView(")));

        let app_user = db.role("app_user").expect("Role not found");
        assert!(<&_ as GrantLike>::applies_to_role(&grant_ref, app_user));
    }

    /// Exercises the `impl ColumnGrantLike for &T` blanket forwarding —
    /// the trait's three methods (`columns`, `table`, plus inherited
    /// `GrantLike`) all need explicit route-through for tarpaulin.
    #[test]
    fn test_column_grant_ref_implementation() {
        let sql = r"
            CREATE TABLE users (id INT, secret TEXT);
            CREATE ROLE r;
            GRANT SELECT (id) ON users TO r;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let column_grant = db.column_grants().next().expect("column grant should exist");

        let cg_ref: &<ParserDB as DatabaseLike>::ColumnGrant = column_grant;

        // GrantLike methods routed through &T.
        assert!(<&_ as GrantLike>::with_grant_option(&cg_ref).then_some(()).is_none());
        let _ = <&_ as GrantLike>::privileges(&cg_ref, &db).count();

        // ColumnGrantLike methods routed through &T.
        let table = <&_ as ColumnGrantLike>::table(&cg_ref, &db).expect("column grant has a table");
        assert_eq!(table.table_name(), "users");
        assert!(matches!(
            <&_ as ColumnGrantLike>::relation(&cg_ref, &db),
            Some(GrantRelation::Table(_))
        ));
        let cols: Vec<_> =
            <&_ as ColumnGrantLike>::columns(&cg_ref, table, &db).expect("columns").collect();
        assert!(!cols.is_empty(), "column grant must surface at least one column");
    }
}
