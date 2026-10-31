//! The sequences a schema holds.

use alloc::{string::String, vec::Vec};

use sqlparser::ast::SequenceOptions;

/// A sequence, the number generator `CREATE SEQUENCE` declares.
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
    /// The options the declaration wrote, in order.
    options: Vec<SequenceOptions>,
}

impl Sequence {
    /// Records a sequence ingestion has placed.
    pub(crate) fn new(
        (name, name_is_quoted): (String, bool),
        schema: Option<(String, bool)>,
        options: Vec<SequenceOptions>,
    ) -> Self {
        let schema_is_quoted = schema.as_ref().is_some_and(|(_, quoted)| *quoted);
        Self {
            name,
            name_is_quoted,
            schema: schema.map(|(schema, _)| schema),
            schema_is_quoted,
            options,
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

    /// Returns the options the declaration wrote, in order.
    pub(crate) fn options(&self) -> &[SequenceOptions] {
        &self.options
    }
}
