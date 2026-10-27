//! Implementation of the `SequenceLike` trait for the stored sequence.

use sqlparser::ast::SequenceOptions;

use crate::{
    structs::{ParserDB, Sequence},
    traits::{Metadata, SequenceLike},
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
}
