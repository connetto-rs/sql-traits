//! Implementation of the `SequenceLike` trait for the stored sequence.

use alloc::format;

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, Sequence, SequenceParameters, StoredSequenceOwner},
    traits::{DatabaseLike, Metadata, SequenceLike, SequenceOrigin, SequenceOwner},
};

impl Metadata for Sequence {
    type Meta = ();
}

impl SequenceLike for Sequence {
    type DB = ParserDB;

    fn sequence_name(&self) -> &str {
        self.name()
    }

    fn sequence_name_is_quoted(&self) -> bool {
        self.name_is_quoted()
    }

    fn sequence_schema(&self) -> Option<&str> {
        self.schema()
    }

    fn sequence_schema_is_quoted(&self) -> bool {
        self.schema_is_quoted()
    }

    fn parameters(&self) -> SequenceParameters {
        Sequence::parameters(self)
    }

    fn owner<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<SequenceOwner<'db, Self::DB>>, LookupError>
    where
        Self: 'db,
    {
        let Some(stored_owner) = self.stored_owner() else {
            return Ok(None);
        };
        let (owner, object_kind) = match stored_owner {
            StoredSequenceOwner::Column(identity) => {
                (
                    database.column_by_identity(identity).map(SequenceOwner::Column),
                    ObjectKind::Column,
                )
            }
            StoredSequenceOwner::ViewColumn { view, ordinal } => {
                (
                    database
                        .view_by_identity(view)
                        .map(|view| SequenceOwner::ViewColumn { view, ordinal }),
                    ObjectKind::View,
                )
            }
        };
        owner.map(Some).ok_or_else(|| {
            LookupError::ObjectNotInDatabase {
                object_kind,
                object: format!("owning sequence `{}`", self.target_name()),
            }
        })
    }

    fn origin(&self) -> SequenceOrigin {
        Sequence::origin(self)
    }
}
