//! One key of an index, its expression and the options written for it.

use sqlparser::ast::{Expr, ObjectName, OrderBySort};

/// One key of an index: the expression it orders by, and the sort direction,
/// null ordering and operator class the statement wrote for it.
///
/// Each option is `None` when the statement left it out and the engine
/// default applies. A default is kept apart from a written option even where
/// the two agree, as `ASC` does everywhere, because only the statement can say
/// which one it was.
///
/// Two indexes over the same expressions serve different lookups when an
/// option differs: a `text_pattern_ops` key answers `LIKE 'abc%'` under any
/// collation, and a `DESC NULLS LAST` key returns rows in another order. A
/// collation is part of the expression, as `Expr::Collate`.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::{ast::OrderBySort, dialect::PostgreSqlDialect};
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE docs (id INT, title TEXT);
///      CREATE INDEX docs_title ON docs (title text_pattern_ops DESC NULLS LAST, id);",
/// )?;
/// let index = db.indexes().next().unwrap();
/// let keys: Vec<IndexKey<'_>> = index.keys(&db)?.collect();
/// assert_eq!(keys[0].expression().to_string(), "title");
/// assert_eq!(keys[0].order(), Some(&OrderBySort::Desc));
/// assert_eq!(keys[0].nulls_first(), Some(false));
/// assert_eq!(
///     keys[0].operator_class().map(ToString::to_string).as_deref(),
///     Some("text_pattern_ops")
/// );
/// assert_eq!(keys[1].order(), None);
/// assert_eq!(keys[1].nulls_first(), None);
/// assert_eq!(keys[1].operator_class(), None);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexKey<'a> {
    expression: &'a Expr,
    order: Option<&'a OrderBySort>,
    nulls_first: Option<bool>,
    operator_class: Option<&'a ObjectName>,
}

impl<'a> IndexKey<'a> {
    /// Returns a key over `expression` written with no options.
    #[must_use]
    #[inline]
    pub const fn new(expression: &'a Expr) -> Self {
        Self { expression, order: None, nulls_first: None, operator_class: None }
    }

    /// Returns this key written with the sort direction `order`.
    #[must_use]
    #[inline]
    pub const fn with_order(mut self, order: &'a OrderBySort) -> Self {
        self.order = Some(order);
        self
    }

    /// Returns this key written with `NULLS FIRST` when `nulls_first` is
    /// `true`, and with `NULLS LAST` otherwise.
    #[must_use]
    #[inline]
    pub const fn with_nulls_first(mut self, nulls_first: bool) -> Self {
        self.nulls_first = Some(nulls_first);
        self
    }

    /// Returns this key written with the operator class `operator_class`.
    #[must_use]
    #[inline]
    pub const fn with_operator_class(mut self, operator_class: &'a ObjectName) -> Self {
        self.operator_class = Some(operator_class);
        self
    }

    /// Returns the expression the key orders by, a plain column or any
    /// expression, its collation included.
    #[must_use]
    #[inline]
    pub const fn expression(&self) -> &'a Expr {
        self.expression
    }

    /// Returns the sort direction written for the key, or `None` when the
    /// statement wrote none and the key sorts ascending.
    #[must_use]
    #[inline]
    pub const fn order(&self) -> Option<&'a OrderBySort> {
        self.order
    }

    /// Returns `Some(true)` for a key written with `NULLS FIRST`,
    /// `Some(false)` for one written with `NULLS LAST`, and `None` when the
    /// statement wrote neither and the engine default for its direction
    /// applies.
    #[must_use]
    #[inline]
    pub const fn nulls_first(&self) -> Option<bool> {
        self.nulls_first
    }

    /// Returns the operator class written for the key with its schema
    /// qualification, or `None` when the statement wrote none and the default
    /// class of the key's type for the access method applies.
    #[must_use]
    #[inline]
    pub const fn operator_class(&self) -> Option<&'a ObjectName> {
        self.operator_class
    }
}
