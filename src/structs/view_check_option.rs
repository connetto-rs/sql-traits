//! Which new rows PostgreSQL checks on a write through a view, as the view's
//! `check_option` records it.

/// The check a view's `check_option` makes on every row an `INSERT` or
/// `UPDATE` through the view writes, which PostgreSQL refuses unless the
/// conditions checked hold on it.
///
/// A view without the option checks no new row, which
/// [`ViewLike::check_option`](crate::traits::ViewLike::check_option) answers as
/// [`None`].
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE docs (id INT, owner_id INT);
///      CREATE VIEW owned WITH (check_option = cascaded) AS
///        SELECT id, owner_id FROM docs WHERE owner_id > 0;",
/// )?;
/// let view = db.view_by_target(TargetName::new("owned", false), IdentifierCase::AsWritten)?;
/// assert_eq!(view.expect("recorded").check_option(), Some(ViewCheckOption::Cascaded));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewCheckOption {
    /// `local`, which checks the view's own conditions and those of every view
    /// underneath that carries a check option of its own.
    Local,
    /// `cascaded`, which checks the view's own conditions and those of every
    /// view underneath, whatever check option each carries.
    Cascaded,
}
