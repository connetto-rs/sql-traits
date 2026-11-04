//! The sequences a schema holds.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{DataType, SequenceOptions};

use crate::{
    errors::SequenceParameterError,
    structs::{ColumnIdentity, RelationIdentity, SequenceParameters, SequenceType},
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
    /// The values it counts with.
    parameters: SequenceParameters,
    /// What owns the sequence, if anything does.
    owner: Option<StoredSequenceOwner>,
    /// Why the sequence exists.
    origin: SequenceOrigin,
}

impl Sequence {
    /// Records a sequence ingestion has placed, whose `parameters` it read
    /// from `options` and whose owner it resolved.
    pub(crate) fn new(
        (name, name_is_quoted): (String, bool),
        schema: Option<(String, bool)>,
        options: Vec<SequenceOptions>,
        parameters: SequenceParameters,
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
            parameters,
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
    ///
    /// This is the declaration's own text, which a live catalog does not
    /// keep, and [`SequenceLike::parameters`](crate::traits::SequenceLike::parameters)
    /// answers the values it amounts to. A sequence created for a column
    /// starts with the `AS` type PostgreSQL gives it, the column's type for an
    /// identity, and goes on with the options the identity wrote.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::SequenceOptions, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE tickets (id BIGINT);
    ///      CREATE SEQUENCE ticket_seq AS BIGINT START WITH 100 OWNED BY tickets.id;",
    /// )?;
    /// let sequence = db.sequences().next().unwrap();
    /// assert!(matches!(
    ///     sequence.written_options(),
    ///     [SequenceOptions::DataType(_), SequenceOptions::StartWith(_, true)]
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn written_options(&self) -> &[SequenceOptions] {
        &self.options
    }

    /// Returns the values the sequence counts with.
    pub(crate) fn parameters(&self) -> SequenceParameters {
        self.parameters
    }

    /// Returns what owns the sequence, if anything does.
    pub(crate) fn stored_owner(&self) -> Option<StoredSequenceOwner> {
        self.owner
    }

    /// Returns why the sequence exists.
    pub(crate) fn origin(&self) -> SequenceOrigin {
        self.origin
    }

    /// Gives an identity sequence the type its column changed to, as the
    /// `ALTER SEQUENCE ... AS` PostgreSQL issues for the change does.
    ///
    /// # Errors
    ///
    /// Returns [`SequenceParameterError::UnsupportedType`] for a type a
    /// sequence cannot count in, and the refusal of
    /// [`SequenceParameters::new`] for a value the new type cannot hold.
    pub(crate) fn set_data_type(
        &mut self,
        data_type: DataType,
    ) -> Result<(), SequenceParameterError> {
        let counted_in = SequenceType::of(&data_type).ok_or_else(|| {
            SequenceParameterError::UnsupportedType { data_type: data_type.to_string() }
        })?;
        self.parameters = self.parameters.with_data_type(counted_in)?;
        self.options.retain(|option| !matches!(option, SequenceOptions::DataType(_)));
        self.options.insert(0, SequenceOptions::DataType(data_type));
        Ok(())
    }

    /// Ties the sequence to its owner, which a column's sequence receives
    /// once the table holding the column is stored.
    pub(crate) fn set_owner(&mut self, owner: StoredSequenceOwner) {
        self.owner = Some(owner);
    }

    /// Moves the sequence to `schema`, which a renamed schema does to what it
    /// holds.
    pub(crate) fn set_schema(&mut self, schema: String, quoted: bool) {
        self.schema = Some(schema);
        self.schema_is_quoted = quoted;
    }
}
