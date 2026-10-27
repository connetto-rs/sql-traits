//! Implementation of the `SequenceLike` trait for the stored sequence.

use sqlparser::ast::SequenceOptions;

use crate::{
    structs::{ParserDB, Sequence, StoredSequenceOwner},
    traits::{DatabaseLike, Metadata, SequenceLike, SequenceOwner},
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

    fn options(&self) -> &[SequenceOptions] {
        Sequence::options(self)
    }

    fn owner<'db>(&'db self, database: &'db Self::DB) -> Option<SequenceOwner<'db, Self::DB>>
    where
        Self: 'db,
    {
        match self.stored_owner()? {
            StoredSequenceOwner::Column(identity) => {
                database.column_by_identity(identity).map(SequenceOwner::Column)
            }
            StoredSequenceOwner::ViewColumn { view, ordinal } => {
                database
                    .view_by_identity(view)
                    .map(|view| SequenceOwner::ViewColumn { view, ordinal })
            }
        }
    }
}
