//! Submodule providing a trait for describing PostgreSQL user-defined types.

use alloc::borrow::Cow;
use core::fmt::Debug;

use crate::{
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::stored_identifier,
};

/// The form a user-defined type takes, as `CREATE TYPE` writes it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum UserDefinedTypeKind {
    /// A composite type, `CREATE TYPE name AS (attributes)`, whose name
    /// reaches both the pool of type names and the pool of relation names.
    Composite,
    /// An enum type, `CREATE TYPE name AS ENUM (labels)`.
    Enum,
    /// A range type, `CREATE TYPE name AS RANGE (options)`, which comes with
    /// a multirange of its own, named but not stored as a type of its own.
    Range,
    /// A base type, `CREATE TYPE name (options)`, defined over the functions
    /// its options name.
    Base,
    /// A shell, the bare `CREATE TYPE name;`, which holds a name until a
    /// later definition fills it or a relation or domain takes it over.
    Shell,
}

/// A trait for types that can be treated as PostgreSQL user-defined types,
/// the objects `CREATE TYPE` defines.
///
/// A user-defined type lands where a relation would, in the schema it names
/// or in the first schema on the search path the input creates. Its name
/// comes from the pool of type names of that schema, which holds every other
/// type of that schema, domain included, and the row type of every table,
/// view and materialized view. A composite type takes a relation name too,
/// which an enum, a range and a base type do not.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createtype.html).
pub trait UserDefinedTypeLike: Clone + Debug + Metadata + Send + Sync {
    /// The database type the user-defined type belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the type.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TYPE mood AS ENUM ('sad');")?;
    /// let type_ = db.user_defined_types().next().unwrap();
    /// assert_eq!(type_.name(), "mood");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns whether the type name was quoted in SQL.
    ///
    /// An unquoted name folds to lowercase when it is stored, so an
    /// implementation over a source that preserves quoting answers it from
    /// the quoting its source kept.
    fn name_is_quoted(&self) -> bool;

    /// Returns the name PostgreSQL stores for this type: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    #[inline]
    fn stored_name(&self) -> Cow<'_, str> {
        stored_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the schema the type was created in, or `None` for one created
    /// in the default schema without naming it.
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
    /// CREATE SCHEMA types;
    /// CREATE TYPE types.mood AS ENUM ('sad');
    /// CREATE TYPE mood AS ENUM ('sad');
    /// ",
    /// )?;
    /// let schema_of = |name: &str| {
    ///     db.user_defined_types()
    ///         .find(|type_| type_.name() == name && type_.schema().is_some())
    ///         .unwrap()
    ///         .schema()
    /// };
    /// assert_eq!(schema_of("mood"), Some("types"));
    /// # Ok(())
    /// # }
    /// ```
    fn schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier was quoted in SQL.
    ///
    /// This only matters when [`Self::schema`] returns `Some`.
    fn schema_is_quoted(&self) -> bool;

    /// Returns the name PostgreSQL stores for this type's schema.
    #[inline]
    fn stored_schema(&self) -> Option<Cow<'_, str>> {
        self.schema().map(|schema| stored_identifier(schema, self.schema_is_quoted()))
    }

    /// Returns the form the type takes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::{
    ///     prelude::*,
    ///     traits::{UserDefinedTypeKind, UserDefinedTypeLike},
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TYPE point AS (x int, y int);
    /// CREATE TYPE mood AS ENUM ('sad');
    /// CREATE TYPE span AS RANGE (subtype = int);
    /// CREATE TYPE moodless;
    /// ",
    /// )?;
    /// let kinds: Vec<_> = db.user_defined_types().map(|type_| type_.kind()).collect();
    /// assert!(kinds.contains(&UserDefinedTypeKind::Composite));
    /// assert!(kinds.contains(&UserDefinedTypeKind::Enum));
    /// assert!(kinds.contains(&UserDefinedTypeKind::Range));
    /// assert!(kinds.contains(&UserDefinedTypeKind::Shell));
    /// # Ok(())
    /// # }
    /// ```
    fn kind(&self) -> UserDefinedTypeKind;
}
