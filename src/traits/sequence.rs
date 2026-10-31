//! Submodule providing a trait for describing SQL sequences.

use alloc::borrow::Cow;
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::SequenceOptions;

use crate::{
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
    utils::{identifier_resolution::normalize_identifier, object_name::is_temporary_schema},
};

/// A trait for types that can be treated as SQL sequences, the number
/// generators `CREATE SEQUENCE` defines.
///
/// A sequence is a relation. It takes its name from the pool tables, views
/// and indexes of its schema share, and a client reads or advances it through
/// the privileges granted on it.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createsequence.html).
pub trait SequenceLike: Debug + Clone + Hash + Ord + Eq + Metadata + Send + Sync {
    /// The database type the sequence belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the sequence, exactly as declared.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SEQUENCE ticket_seq;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_name(), "ticket_seq");
    /// # Ok(())
    /// # }
    /// ```
    fn sequence_name(&self) -> &str;

    /// Returns whether the sequence identifier was quoted in SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn sequence_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this sequence, an unquoted
    /// identifier folded to lowercase and a quoted one kept in its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SEQUENCE Ticket_Seq;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_name(), "Ticket_Seq");
    /// assert_eq!(sequence.stored_sequence_name(), "ticket_seq");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_sequence_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.sequence_name(), self.sequence_name_is_quoted())
    }

    /// Returns the schema the sequence is stored in, or `None` for one stored
    /// in the default schema without a qualifier.
    ///
    /// Ingestion records where the sequence landed, which is the schema the
    /// search path selected for a bare name and `pg_temp` for a temporary
    /// sequence.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      SET search_path = app, public;
    ///      CREATE SEQUENCE ticket_seq;",
    /// )?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    fn sequence_schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier of this sequence was quoted in
    /// SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn sequence_schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this sequence's schema.
    #[inline]
    fn stored_sequence_schema(&self) -> Option<Cow<'_, str>> {
        self.sequence_schema()
            .map(|schema| normalize_identifier(schema, self.sequence_schema_is_quoted()))
    }

    /// Returns whether this sequence lives in the session's temporary schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TEMP SEQUENCE scratch;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert!(sequence.is_temporary());
    /// assert_eq!(sequence.sequence_schema(), Some("pg_temp"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_temporary(&self) -> bool {
        self.sequence_schema()
            .is_some_and(|schema| is_temporary_schema(schema, self.sequence_schema_is_quoted()))
    }

    /// Returns the sequence name exactly as declared, including its optional
    /// schema.
    #[inline]
    fn target_name(&self) -> TargetName<'_> {
        let name = TargetName::new(self.sequence_name(), self.sequence_name_is_quoted());
        match self.sequence_schema() {
            Some(schema) => name.with_schema(schema, self.sequence_schema_is_quoted()),
            None => name,
        }
    }

    /// Returns the options the declaration wrote, in the order it wrote them.
    ///
    /// An option the declaration left out takes PostgreSQL's default, which
    /// this does not spell out.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::SequenceOptions, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SEQUENCE ticket_seq AS BIGINT START WITH 100;",
    /// )?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert!(matches!(
    ///     sequence.options(),
    ///     [SequenceOptions::DataType(_), SequenceOptions::StartWith(_, true)]
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn options(&self) -> &[SequenceOptions];
}
