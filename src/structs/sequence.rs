//! The sequences a schema holds.

use alloc::{string::String, vec::Vec};

use sqlparser::ast::{DataType, SequenceOptions};

use crate::{
    structs::{ColumnIdentity, RelationIdentity},
    traits::SequenceOrigin,
};

/// What owns a stored sequence, kept by identity so it follows the owner
/// through renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum StoredSequenceOwner {
    /// A column of a table.
    Column(ColumnIdentity),
    /// An output column of a plain view, at this position.
    ViewColumn {
        /// The view.
        view: RelationIdentity,
        /// Position of the column in the view's output.
        ordinal: usize,
    },
}

/// A sequence, declared by `CREATE SEQUENCE` or created for a column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence {
    /// The sequence's own identifier, exactly as written.
    name: String,
    /// Whether that identifier was quoted.
    name_is_quoted: bool,
    /// The schema the sequence landed in, unless it is the default one.
    schema: Option<String>,
    /// Whether that schema identifier was quoted.
    schema_is_quoted: bool,
    /// The options the sequence was created with, in order, `OWNED BY`
    /// excepted.
    options: Vec<SequenceOptions>,
    /// What owns the sequence, if anything does.
    owner: Option<StoredSequenceOwner>,
    /// Why the sequence exists.
    origin: SequenceOrigin,
}

impl Sequence {
    /// Records a sequence ingestion has placed and whose owner it resolved.
    pub(crate) fn new(
        (name, name_is_quoted): (String, bool),
        schema: Option<(String, bool)>,
        options: Vec<SequenceOptions>,
        owner: Option<StoredSequenceOwner>,
        origin: SequenceOrigin,
    ) -> Self {
        let schema_is_quoted = schema.as_ref().is_some_and(|(_, quoted)| *quoted);
        Self {
            name,
            name_is_quoted,
            schema: schema.map(|(schema, _)| schema),
            schema_is_quoted,
            options,
            owner,
            origin,
        }
    }

    /// Returns the sequence's own identifier, exactly as written.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether that identifier was quoted.
    pub(crate) fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    /// Returns the schema the sequence landed in, or `None` for the default
    /// one.
    pub(crate) fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Returns whether the schema identifier was quoted.
    pub(crate) fn schema_is_quoted(&self) -> bool {
        self.schema_is_quoted
    }

    /// Returns the options the declaration wrote, in order, `OWNED BY`
    /// excepted.
    pub(crate) fn options(&self) -> &[SequenceOptions] {
        &self.options
    }

    /// Returns what owns the sequence, if anything does.
    pub(crate) fn stored_owner(&self) -> Option<StoredSequenceOwner> {
        self.owner
    }

    /// Returns why the sequence exists.
    pub(crate) fn origin(&self) -> SequenceOrigin {
        self.origin
    }

    /// Replaces the `AS` type, which an identity sequence takes from its
    /// column whenever the column's type changes.
    pub(crate) fn set_data_type(&mut self, data_type: DataType) {
        self.options.retain(|option| !matches!(option, SequenceOptions::DataType(_)));
        self.options.insert(0, SequenceOptions::DataType(data_type));
    }
}
