//! Which columns a table's logical replication stream identifies a row by,
//! as `pg_class.relreplident` records it.

use alloc::string::String;

use crate::traits::DatabaseLike;

/// What a table's `UPDATE` and `DELETE` write to the logical replication
/// stream to identify the old row, as `ALTER TABLE ... REPLICA IDENTITY` sets
/// it.
///
/// A publication that publishes updates or deletes refuses them on a table
/// whose identity is [`Nothing`](Self::Nothing), [`Default`](Self::Default)
/// with no primary key, or [`Index`](Self::Index) naming no index.
#[derive(Debug, Clone)]
pub enum ReplicaIdentity<'db, DB: DatabaseLike> {
    /// `DEFAULT`, the primary key's columns, and nothing where the table has
    /// none. Every table starts here.
    Default,
    /// `NOTHING`, no column at all.
    Nothing,
    /// `FULL`, every column of the old row.
    Full,
    /// `USING INDEX`, the columns of the index, or [`None`] once that index is
    /// dropped. PostgreSQL then keeps the setting and reads it as
    /// [`Nothing`](Self::Nothing).
    Index(Option<ReplicaIdentityIndex<'db, DB>>),
}

impl<DB: DatabaseLike> Copy for ReplicaIdentity<'_, DB> {}

/// The index a [`ReplicaIdentity::Index`] names.
#[derive(Debug, Clone)]
pub enum ReplicaIdentityIndex<'db, DB: DatabaseLike> {
    /// A unique index `CREATE UNIQUE INDEX` built, also once a `PRIMARY KEY`
    /// or `UNIQUE` constraint adopts it with `USING INDEX`.
    Index(&'db DB::Index),
    /// The index behind a `PRIMARY KEY` or `UNIQUE` constraint, which shares
    /// the constraint's name.
    UniqueIndex(&'db DB::UniqueIndex),
}

impl<DB: DatabaseLike> Copy for ReplicaIdentityIndex<'_, DB> {}

/// The replica identity a table's metadata holds, its index named as
/// PostgreSQL stores the name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum StoredReplicaIdentity {
    /// `DEFAULT`.
    #[default]
    Default,
    /// `NOTHING`.
    Nothing,
    /// `FULL`.
    Full,
    /// `USING INDEX`, [`None`] once the index is dropped.
    Index(Option<String>),
}

impl StoredReplicaIdentity {
    /// The name of the index the setting names, while one is held.
    pub(crate) fn index_name(&self) -> Option<&str> {
        match self {
            Self::Index(name) => name.as_deref(),
            Self::Default | Self::Nothing | Self::Full => None,
        }
    }
}
