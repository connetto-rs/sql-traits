//! The metadata ingestion records about a domain.

use sqlparser::ast::CreateFunction;

use super::ExpressionCalls;
use crate::structs::TargetName;

/// What ingestion records about a domain beyond its declaration.
#[derive(Debug, Clone)]
pub struct DomainMetadata {
    /// The calls of the domain's default and checks.
    calls: ExpressionCalls<CreateFunction>,
    /// The collation the domain's `COLLATE` named when it was created.
    collation: Option<TargetName<'static>>,
}

impl DomainMetadata {
    pub(crate) fn new(
        calls: ExpressionCalls<CreateFunction>,
        collation: Option<TargetName<'static>>,
    ) -> Self {
        Self { calls, collation }
    }

    /// Returns the calls of the domain's default and checks, bound when it
    /// was written.
    #[must_use]
    pub fn calls(&self) -> &ExpressionCalls<CreateFunction> {
        &self.calls
    }

    pub(crate) fn calls_mut(&mut self) -> &mut ExpressionCalls<CreateFunction> {
        &mut self.calls
    }

    /// Returns the collation the domain's `COLLATE` named on the search path
    /// in force when the domain was created, qualified with its schema, or
    /// `None` when it names none the database knew then.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN code AS TEXT COLLATE \"C\";")?;
    /// let domain = db.domains().next().expect("the domain is recorded");
    /// let collation = db.domain_metadata(domain).and_then(|metadata| metadata.collation());
    /// assert_eq!(collation, Some(&TargetName::new("C", true).with_schema("pg_catalog", false)));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn collation(&self) -> Option<&TargetName<'static>> {
        self.collation.as_ref()
    }
}
