//! Column metadata that parsed DDL keeps outside the column node.

use alloc::string::String;

use crate::traits::MySqlCollationPadding;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedCollation {
    DatabaseDefault,
    Named { schema: Option<String>, schema_quoted: bool, name: String, name_quoted: bool },
}

/// Metadata attached to a parsed column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnMetadata {
    postgres_deterministic: Option<bool>,
    postgres_collation: Option<ResolvedCollation>,
    mysql_padding: Option<MySqlCollationPadding>,
    documentation: Option<String>,
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
