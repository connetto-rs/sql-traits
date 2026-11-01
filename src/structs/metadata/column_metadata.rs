//! Column metadata that parsed DDL keeps outside the column node.

use alloc::{string::String, sync::Arc, vec::Vec};

use sqlparser::ast::Query;

use crate::{structs::ParserDB, traits::MySqlCollationPadding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedCollation {
    DatabaseDefault,
    Named { schema: Option<String>, schema_quoted: bool, name: String, name_quoted: bool },
}

/// The query of a `CREATE TABLE … AS` statement, with its output names and the
/// catalog it read, frozen as the statement saw them.
#[derive(Debug)]
pub(crate) struct CreatingQuery {
    query: Query,
    output_names: Vec<(String, bool)>,
    catalog: ParserDB,
}

impl CreatingQuery {
    pub(crate) fn new(query: Query, output_names: Vec<(String, bool)>, catalog: ParserDB) -> Self {
        Self { query, output_names, catalog }
    }
}

/// The output of a `CREATE TABLE … AS` query a column was created from.
#[derive(Debug, Clone)]
pub(crate) struct ColumnCreation {
    query: Arc<CreatingQuery>,
    ordinal: usize,
}

impl ColumnCreation {
    pub(crate) fn new(query: Arc<CreatingQuery>, ordinal: usize) -> Self {
        Self { query, ordinal }
    }

    pub(crate) fn query(&self) -> &Query {
        &self.query.query
    }

    pub(crate) fn output_names(&self) -> &[(String, bool)] {
        &self.query.output_names
    }

    pub(crate) fn catalog(&self) -> &ParserDB {
        &self.query.catalog
    }

    pub(crate) fn ordinal(&self) -> usize {
        self.ordinal
    }
}

impl PartialEq for ColumnCreation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.query, &other.query) && self.ordinal == other.ordinal
    }
}

impl Eq for ColumnCreation {}

/// Metadata attached to a parsed column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnMetadata {
    postgres_deterministic: Option<bool>,
    postgres_collation: Option<ResolvedCollation>,
    mysql_padding: Option<MySqlCollationPadding>,
    documentation: Option<String>,
    creation: Option<ColumnCreation>,
}

impl ColumnMetadata {
    /// Stores PostgreSQL determinism when it is known.
    #[must_use]
    pub fn with_postgres_deterministic(mut self, deterministic: Option<bool>) -> Self {
        self.postgres_deterministic = deterministic;
        self
    }

    /// Stores MySQL padding when it is known.
    #[must_use]
    pub fn with_mysql_padding(mut self, padding: Option<MySqlCollationPadding>) -> Self {
        self.mysql_padding = padding;
        self
    }

    /// Stores the comment block written above the column.
    #[must_use]
    pub fn with_documentation(mut self, documentation: Option<String>) -> Self {
        self.documentation = documentation;
        self
    }

    pub(crate) fn with_postgres_default_collation(mut self) -> Self {
        self.postgres_collation = Some(ResolvedCollation::DatabaseDefault);
        self
    }

    pub(crate) fn with_postgres_collation(
        mut self,
        schema: Option<(&str, bool)>,
        name: (&str, bool),
    ) -> Self {
        self.postgres_collation = Some(ResolvedCollation::Named {
            schema: schema.map(|(schema, _)| String::from(schema)),
            schema_quoted: schema.is_some_and(|(_, quoted)| quoted),
            name: String::from(name.0),
            name_quoted: name.1,
        });
        self
    }

    pub(crate) fn with_creation(mut self, creation: ColumnCreation) -> Self {
        self.creation = Some(creation);
        self
    }

    /// The query output the column was created from, whatever type it has
    /// been declared since.
    pub(crate) fn creation(&self) -> Option<&ColumnCreation> {
        self.creation.as_ref()
    }

    pub(crate) fn postgres_collation_matches(&self, other: &Self) -> Option<bool> {
        Some(self.postgres_collation.as_ref()? == other.postgres_collation.as_ref()?)
    }

    /// Returns PostgreSQL determinism when it is known.
    #[must_use]
    pub fn postgres_deterministic(&self) -> Option<bool> {
        self.postgres_deterministic
    }

    /// Returns MySQL padding when it is known.
    #[must_use]
    pub fn mysql_padding(&self) -> Option<MySqlCollationPadding> {
        self.mysql_padding
    }

    /// Returns the comment block written above the column.
    #[must_use]
    pub fn documentation(&self) -> Option<&str> {
        self.documentation.as_deref()
    }
}
