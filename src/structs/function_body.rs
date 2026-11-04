//! Submodule defining the forms a function body is written in.

use sqlparser::ast::{Expr, Select, Statement};

/// The body of a function as it is written, before any text it is handed
/// over in is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionBody<'f> {
    /// Text read again each time the function runs, as `AS '<body>'` and
    /// `AS $$<body>$$` write it. A body in a language other than SQL is text
    /// as well, and the function's language says how it reads.
    Text(&'f str),
    /// An expression read each time the function runs, as BigQuery's
    /// `AS (<expression>)` and SQL Server's `AS RETURN <expression>` write it.
    Expression(&'f Expr),
    /// A query read each time the function runs, as SQL Server's
    /// `AS RETURN SELECT` writes it.
    Select(&'f Select),
    /// Statements read each time the function runs, one after another, as SQL
    /// Server's `AS BEGIN ... END` writes them.
    Statements(&'f [Statement]),
    /// The expression of `RETURN <expression>`, which PostgreSQL binds when
    /// the function is created.
    Return(&'f Expr),
    /// The statements of a `BEGIN ATOMIC ... END` block, which PostgreSQL
    /// binds when the function is created.
    Atomic(&'f [Statement]),
    /// The `CREATE FUNCTION` statement a catalog prints for a body PostgreSQL
    /// bound when the function was created, as `pg_get_functiondef` prints
    /// the `RETURN <expression>` or `BEGIN ATOMIC` body `pg_proc.prosqlbody`
    /// stores as a node tree.
    ///
    /// The readers parse the statement under the database's dialect when they
    /// need the body, and read it as [`Return`](Self::Return) or
    /// [`Atomic`](Self::Atomic) from there, so a statement the parser cannot
    /// read fails only the readers that parse it, for this function alone.
    /// Each relation the body names is resolved as
    /// [`DatabaseLike::resolve_bound_relation`](crate::traits::DatabaseLike::resolve_bound_relation)
    /// resolves a stored definition's names.
    BoundDefinition(&'f str),
}

impl FunctionBody<'_> {
    /// Returns whether PostgreSQL bound the body when the function was
    /// created, which only `RETURN <expression>` and `BEGIN ATOMIC` do.
    #[must_use]
    #[inline]
    pub fn is_bound(&self) -> bool {
        matches!(self, Self::Return(_) | Self::Atomic(_) | Self::BoundDefinition(_))
    }
}
