//! The set of object kinds a [`GenericDB`] holds.
//!
//! [`GenericDB`] is generic over every kind of thing a schema contains, and
//! naming those one type parameter each made the container's parameter list
//! grow by one for every kind added, repeated at every impl block. A profile
//! carries them as associated types instead, so the container takes a single
//! parameter and a new kind costs one associated type and one field.

use alloc::sync::Arc;

use crate::{
    structs::GenericDB,
    traits::{
        CheckConstraintLike, ColumnGrantLike, ColumnLike, DialectLike, ExtensionLike,
        ForeignKeyLike, FunctionLike, IndexLike, Metadata, PolicyLike, RoleLike,
        RoleMembershipLike, RuleLike, SchemaLike, TableGrantLike, TableLike, TriggerLike,
        UniqueIndexLike, ViewLike,
    },
};

/// The metadata a schema object carries.
///
/// A shorthand for the [`Metadata::Meta`] projection, which a profile's
/// associated types would otherwise spell out in full at every field and
/// signature.
pub type Meta<K> = <K as Metadata>::Meta;

/// One schema object of a kind, paired with the metadata recorded for it.
///
/// This is how every collection in a [`GenericDB`] and its builder stores a
/// kind, so an object and its metadata can never drift apart. Crate-private:
/// it describes storage, and no public signature names it.
pub(crate) type Stored<K> = (Arc<K>, Meta<K>);

/// One schema object of a kind that keeps an identity, paired with its
/// metadata and that identity.
pub(crate) type Identified<K, I> = (Arc<K>, Meta<K>, I);

/// The concrete types a [`GenericDB`] stores for each kind of schema object.
///
/// One implementation exists per source representation. The parser's is
/// [`SqlparserProfile`](crate::structs::SqlparserProfile), reached through the
/// [`ParserDB`](crate::structs::ParserDB) alias.
///
/// Each associated type is bound to the database it belongs to, so an
/// implementation cannot mix kinds from two different databases.
pub trait SchemaProfile: Sized {
    /// The tables the schema holds.
    type Table: TableLike<DB = GenericDB<Self>>;
    /// The plain views the schema holds.
    type View: ViewLike<DB = GenericDB<Self>>;
    /// The materialized views the schema holds.
    type MaterializedView: ViewLike<DB = GenericDB<Self>>;
    /// The columns the schema holds.
    type Column: ColumnLike<DB = GenericDB<Self>>;
    /// The indexes the schema holds.
    type Index: IndexLike<DB = GenericDB<Self>>;
    /// The unique indexes the schema holds.
    type UniqueIndex: UniqueIndexLike<DB = GenericDB<Self>>;
    /// The foreign keys the schema holds.
    type ForeignKey: ForeignKeyLike<DB = GenericDB<Self>>;
    /// The functions the schema holds.
    type Function: FunctionLike<DB = GenericDB<Self>>;
    /// The check constraints the schema holds.
    type CheckConstraint: CheckConstraintLike<DB = GenericDB<Self>>;
    /// The triggers the schema holds.
    type Trigger: TriggerLike<DB = GenericDB<Self>>;
    /// The rewrite rules the schema holds.
    type Rule: RuleLike<DB = GenericDB<Self>>;
    /// The extensions the database holds.
    type Extension: ExtensionLike<DB = GenericDB<Self>>;
    /// The row-security policies the schema holds.
    type Policy: PolicyLike<DB = GenericDB<Self>>;
    /// The roles the schema holds.
    type Role: RoleLike<DB = GenericDB<Self>>;
    /// The role memberships the database holds.
    type RoleMembership: RoleMembershipLike<DB = GenericDB<Self>>;
    /// The schemas the database holds.
    type Schema: SchemaLike<DB = GenericDB<Self>>;
    /// The table-level grants the schema holds.
    type TableGrant: TableGrantLike<DB = GenericDB<Self>>;
    /// The column-level grants the schema holds.
    type ColumnGrant: ColumnGrantLike<DB = GenericDB<Self>>;
    /// The SQL dialect the schema is expressed in.
    type Dialect: DialectLike<DB = GenericDB<Self>>;
    /// The state a statement-by-statement ingestion carries beyond the
    /// schema objects themselves, preserved inside every built database so
    /// ingestion can resume from it.
    ///
    /// The parser profile stores its access-resolution mode, the active
    /// PostgreSQL catalog, and metadata for collations the ingested DDL
    /// created.
    type Ingestion: Clone + core::fmt::Debug + Send + Sync;

    /// Returns the state a fresh ingestion starts from under the given
    /// dialect, before any statement applies.
    ///
    /// Dialect-aware so a directly built database resumes under its
    /// dialect's defaults: the parser profile answers PostgreSQL with the
    /// full default catalog and every other dialect with an empty one.
    fn default_ingestion(dialect: &Self::Dialect) -> Self::Ingestion;
}
