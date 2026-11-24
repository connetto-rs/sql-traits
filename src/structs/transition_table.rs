//! A transition table a trigger declares with `REFERENCING`.

use alloc::borrow::Cow;

use crate::{structs::TriggerRow, utils::identifier_resolution::stored_identifier};

/// A transition table an `AFTER` trigger declares with `REFERENCING NEW TABLE
/// AS name` or `REFERENCING OLD TABLE AS name`, as
/// [`TriggerLike::transition_tables`](crate::traits::TriggerLike::transition_tables)
/// lists them.
///
/// The trigger's function reads the rows its statement wrote, or the rows as
/// they were before it, as a table under that name. PostgreSQL records the
/// names in `pg_trigger.tgnewtable` and `pg_trigger.tgoldtable`.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE t (id INT);
///      CREATE FUNCTION audit() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
///      CREATE TRIGGER audit AFTER UPDATE ON t
///        REFERENCING OLD TABLE AS Before_Rows NEW TABLE AS \"After_Rows\"
///        FOR EACH STATEMENT EXECUTE FUNCTION audit();",
/// )?;
/// let trigger = db.triggers().next().unwrap();
/// let tables: Vec<TransitionTable<'_>> = trigger.transition_tables().collect();
/// assert_eq!(tables[0].rows(), TriggerRow::Old);
/// assert_eq!(tables[0].name(), "Before_Rows");
/// assert!(!tables[0].name_is_quoted());
/// assert_eq!(tables[0].stored_name(), "before_rows");
/// assert_eq!(tables[1].rows(), TriggerRow::New);
/// assert_eq!(tables[1].stored_name(), "After_Rows");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransitionTable<'a> {
    /// The rows the table holds.
    rows: TriggerRow,
    /// The name exactly as written.
    name: &'a str,
    /// Whether the name was quoted.
    name_is_quoted: bool,
}

impl<'a> TransitionTable<'a> {
    /// Records a transition table holding `rows` under the name `name`,
    /// quoted when `name_is_quoted`.
    #[must_use]
    pub const fn new(rows: TriggerRow, name: &'a str, name_is_quoted: bool) -> Self {
        Self { rows, name, name_is_quoted }
    }

    /// Returns the rows the table holds, [`TriggerRow::New`] for the rows
    /// the statement wrote and [`TriggerRow::Old`] for the rows as they were
    /// before it.
    #[must_use]
    pub const fn rows(self) -> TriggerRow {
        self.rows
    }

    /// Returns the name exactly as written.
    #[must_use]
    pub const fn name(self) -> &'a str {
        self.name
    }

    /// Returns whether the name was quoted.
    #[must_use]
    pub const fn name_is_quoted(self) -> bool {
        self.name_is_quoted
    }

    /// Returns the name PostgreSQL stores and the trigger's function reads
    /// the table by, where an unquoted name folds to lowercase and a quoted
    /// one keeps its case.
    #[must_use]
    pub fn stored_name(self) -> Cow<'a, str> {
        stored_identifier(self.name, self.name_is_quoted)
    }
}
