//! Submodule providing a trait for describing SQL View-like entities.

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::Query;

use crate::{
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
    utils::{identifier_resolution::normalize_identifier, object_name::is_temporary_schema},
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
        normalize_identifier(self.view_name(), self.view_name_is_quoted())
    }

    /// Returns the schema the view is stored in, or `None` for one stored in
    /// the default schema without a qualifier.
    ///
    /// Ingestion records where the view landed: the schema the search path
    /// selected for a bare name, and `pg_temp` for a temporary view.
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
        self.view_schema().map(|schema| normalize_identifier(schema, self.view_schema_is_quoted()))
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
