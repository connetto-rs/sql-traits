//! Submodule providing a trait for describing SQL sequences.

use alloc::borrow::Cow;
use core::{fmt::Debug, hash::Hash};

use crate::{
    errors::LookupError,
    structs::{SequenceParameters, TargetName},
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::{is_temporary_schema, stored_identifier},
};

/// The column a sequence is owned by, saying which kind of relation holds it.
///
/// PostgreSQL lets a column of a table or of a plain view own a sequence, and
/// drops the sequence when that column or its relation goes.
#[derive(Clone, Debug)]
pub enum SequenceOwner<'a, DB: DatabaseLike> {
    /// A column of a table.
    Column(&'a DB::Column),
    /// An output column of a plain view, by its position in the view's
    /// output, counted from zero.
    ///
    /// A view keeps a column at its position through every rename, and a
    /// replacement may only add columns after the last, so the position is
    /// what stays put.
    ViewColumn {
        /// The view.
        view: &'a DB::View,
        /// Position of the column in the view's output.
        ordinal: usize,
    },
}

impl<DB: DatabaseLike> Copy for SequenceOwner<'_, DB> {}

/// Why a sequence exists.
///
/// PostgreSQL creates a sequence of its own for every `serial` column and
/// every identity column, besides the ones `CREATE SEQUENCE` declares, and
/// the column it serves owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SequenceOrigin {
    /// Declared by `CREATE SEQUENCE`.
    Declared,
    /// Created for a `serial`, `bigserial` or `smallserial` column, whose
    /// default draws from it.
    Serial,
    /// Created for an identity column, which generates its values from it
    /// and cannot be left without it.
    Identity,
}

/// A trait for types that can be treated as SQL sequences, the number
/// generators `CREATE SEQUENCE` defines and PostgreSQL creates for `serial`
/// and identity columns.
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
        stored_identifier(self.sequence_name(), self.sequence_name_is_quoted())
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
            .map(|schema| stored_identifier(schema, self.sequence_schema_is_quoted()))
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

    /// Returns the values the sequence counts with, as `pg_sequence` records
    /// them.
    ///
    /// Every option a declaration leaves out takes PostgreSQL's default for
    /// the type and direction, so two declarations that differ only in
    /// spelling a default out answer alike. A sequence created for a column
    /// counts in the column's type, which an identity sequence follows through
    /// every type change of its column.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SEQUENCE implicit;
    ///      CREATE SEQUENCE spelled START WITH 1 INCREMENT BY 1;
    ///      CREATE SEQUENCE countdown AS integer INCREMENT -1;",
    /// )?;
    /// let parameters = |name| {
    ///     db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(|sequence| sequence.map(SequenceLike::parameters))
    /// };
    /// assert_eq!(parameters("implicit")?, parameters("spelled")?);
    /// let countdown = parameters("countdown")?.unwrap();
    /// assert_eq!(countdown.data_type(), SequenceType::Integer);
    /// assert_eq!((countdown.start(), countdown.max_value()), (-1, -1));
    /// assert_eq!(countdown.min_value(), i64::from(i32::MIN));
    /// # Ok(())
    /// # }
    /// ```
    fn parameters(&self) -> SequenceParameters;

    /// Returns the column owning this sequence, if one does.
    ///
    /// A declared sequence is owned by the column its `OWNED BY` names, and
    /// one created for a column by that column. The column keeps owning it
    /// through every rename of the column and of its relation, and dropping
    /// either drops the sequence.
    ///
    /// `Ok(None)` means the sequence is unowned, including `OWNED BY NONE`.
    ///
    /// # Errors
    ///
    /// Returns a [`LookupError`] when the recorded owning column or relation is
    /// absent from `database`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE tickets (id BIGINT);
    ///      CREATE SEQUENCE ticket_seq OWNED BY tickets.id;
    ///      ALTER TABLE tickets RENAME COLUMN id TO ticket_id;",
    /// )?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert!(matches!(
    ///     sequence.owner(&db)?,
    ///     Some(SequenceOwner::Column(column)) if column.column_name() == "ticket_id"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<SequenceOwner<'db, Self::DB>>, LookupError>
    where
        Self: 'db;

    /// Returns why the sequence exists.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE tickets (id BIGINT GENERATED ALWAYS AS IDENTITY, n serial);",
    /// )?;
    /// let origin = |name| {
    ///     db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(|sequence| sequence.map(SequenceLike::origin))
    /// };
    /// assert_eq!(origin("tickets_id_seq")?, Some(SequenceOrigin::Identity));
    /// assert_eq!(origin("tickets_n_seq")?, Some(SequenceOrigin::Serial));
    /// # Ok(())
    /// # }
    /// ```
    fn origin(&self) -> SequenceOrigin;
}
