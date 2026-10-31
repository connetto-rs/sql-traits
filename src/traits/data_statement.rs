//! Submodule providing the [`DataStatementLike`] trait, the umbrella for SQL
//! data statements (ISO "SQL-data statements": `SELECT`, `INSERT`, `UPDATE`,
//! `DELETE`) analyzed against a [`DatabaseLike`].
//!
//! Unlike the DDL object traits, which carry an associated `DB` because each
//! object belongs to one schema, a data statement is analyzed *against* a
//! database passed in, and the same statement may be analyzed against different
//! databases. The database is therefore a trait type parameter.

use alloc::vec::Vec;

use crate::{
    errors::LookupError,
    traits::{DatabaseLike, Relation},
};

/// A parsed SQL statement that references table data, analyzed against a
/// [`DatabaseLike`].
///
/// This is the umbrella trait for the data-statement hierarchy. It is
/// implemented on the sqlparser AST nodes that model data statements
/// (`Query` for `SELECT`, and `Insert` / `Update` / `Delete` for DML),
/// mirroring how the DDL traits are implemented on `CreateTable`, `ColumnDef`,
/// and so on.
///
/// The public surface speaks only resolved schema types
/// ([`DatabaseLike::Table`], [`Relation`]), never sqlparser's `ObjectName`
/// or `TableFactor`.
pub trait DataStatementLike<DB: DatabaseLike> {
    /// Returns every base table the statement reads directly, plus every
    /// base table a plain view it reads reaches through the view's stored
    /// definition, expanded recursively, resolved against `database` and
    /// deduplicated.
    ///
    /// The direct reads cover `FROM` and `JOIN` relations, relations
    /// inside subqueries (for example in `WHERE` or `HAVING`), CTE
    /// bodies, set operations, and, for DML, the mutation target together
    /// with any subqueries it contains. A mutation target names a stored
    /// relation, and a plain view it names expands like any other, since
    /// writing through a view writes its base table.
    ///
    /// A plain view is expanded through its definition, which reads its
    /// names as bound when the view was created, with no search path, and
    /// a view's `security_invoker` setting does not change the expansion.
    /// A materialized view holds a snapshot of its definition's output,
    /// so reading one never reads its base tables, and it is absent from
    /// the answer even when a plain view reads it.
    ///
    /// A table a `FROM`, `JOIN` or `UPDATE`/`DELETE` target item names
    /// without `ONLY` also reaches every table below it, through
    /// `INHERITS` and `PARTITION OF`, recursively, as PostgreSQL scans and
    /// modifies them. `t` and `t *` read alike, and `ONLY t` or `ONLY (t)`
    /// reads `t` alone.
    ///
    /// A bare name that a common table expression in scope binds reads
    /// that expression even when a stored relation shares the name, and
    /// is skipped. The innermost `WITH` wins. An expression is in scope
    /// in the query that declares it and in the bodies of the
    /// expressions after it, and under `WITH RECURSIVE` in every body of
    /// its clause, its own included. Names that resolve to no stored
    /// relation (table functions, otherwise unknown names) are skipped
    /// rather than reported as errors.
    ///
    /// Tables are returned in first-seen order and deduplicated by their
    /// position in [`DatabaseLike`], so the same base table reached
    /// directly and through a view appears once.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when a referenced
    /// name matches more than one relation (for example an unqualified
    /// name present in several schemas), and
    /// [`LookupError::InvalidObjectName`] when a referenced name is
    /// malformed for relation lookup.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE VIEW v AS SELECT id FROM t;
    /// CREATE MATERIALIZED VIEW m AS SELECT id FROM t;
    /// ",
    /// )?;
    /// // The view reaches its base table, and the materialized view does
    /// // not.
    /// let query =
    ///     |sql| sqlparser::parser::Parser::new(&GenericDialect).try_with_sql(sql)?.parse_query();
    /// assert_eq!(query("SELECT * FROM v")?.referenced_tables(&db)?.len(), 1);
    /// assert_eq!(query("SELECT * FROM m")?.referenced_tables(&db)?.len(), 0);
    /// # Ok(())
    /// # }
    /// ```
    fn referenced_tables<'db>(&self, database: &'db DB)
    -> Result<Vec<&'db DB::Table>, LookupError>;

    /// Returns every relation the statement reads directly, each tagged
    /// with the kind it is, resolved against `database` and deduplicated.
    ///
    /// This answers what the statement names, whether a base table, a
    /// plain view or a materialized view, applying the resolution
    /// [`Self::referenced_tables`] spells out, the search path, the
    /// temporary schema first, the `WITH` shadowing and the DML rules,
    /// and the MySQL multi-table `DELETE ... t1, t2` list. A plain view
    /// the statement reads stands for its whole definition, so its
    /// definition's reads stay the view's, and a table read without `ONLY`
    /// stands for the tables below it.
    ///
    /// Relations are returned in first-seen order and deduplicated by
    /// their identity in [`DatabaseLike`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when a referenced
    /// name matches more than one relation, and
    /// [`LookupError::InvalidObjectName`] when a referenced name is
    /// malformed for relation lookup.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE VIEW v AS SELECT id FROM t;
    /// CREATE MATERIALIZED VIEW m AS SELECT id FROM t;
    /// ",
    /// )?;
    /// let sql = "SELECT * FROM t, v, m";
    /// let query = sqlparser::parser::Parser::new(&GenericDialect).try_with_sql(sql)?.parse_query()?;
    /// let read: Vec<String> = query
    ///     .referenced_relations(&db)?
    ///     .iter()
    ///     .map(|relation| {
    ///         match relation {
    ///             Relation::Table(table) => format!("table {}", table.table_name()),
    ///             Relation::View(view) => format!("view {}", view.view_name()),
    ///             Relation::MaterializedView(view) => {
    ///                 format!("materialized view {}", view.view_name())
    ///             }
    ///         }
    ///     })
    ///     .collect();
    /// assert_eq!(read, ["table t", "view v", "materialized view m"]);
    /// # Ok(())
    /// # }
    /// ```
    fn referenced_relations<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Vec<Relation<'db, DB>>, LookupError>;
}
