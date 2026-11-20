//! A field of `NEW` or `OLD` that a maintenance body names and the trigger's
//! table does not declare.

use sqlparser::ast::Ident;
use sqlstate::SqlState;

use crate::utils::identifier_resolution::stored_identifier;

/// The row of a row trigger a body reads a field of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerRow {
    /// `NEW`, the row the statement writes.
    New,
    /// `OLD`, the row before the statement.
    Old,
}

impl TriggerRow {
    /// Returns the name PostgreSQL gives the row in its messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Old => "old",
        }
    }
}

/// A field of `NEW` or `OLD` that a maintenance body assigns or reads and the
/// trigger's table does not declare, as
/// [`TriggerLike::maintenance_undeclared_fields`](crate::traits::TriggerLike::maintenance_undeclared_fields)
/// lists them.
///
/// PostgreSQL loads such a body and fails each run reaching the statement or
/// `IF` condition that names the field, with the SQLSTATE
/// [`UndeclaredRowField::sqlstate`] answers and the message this type
/// displays.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE t (id INT, vv INT, w INT);
///      CREATE FUNCTION stamp() RETURNS TRIGGER LANGUAGE plpgsql AS $$
///      BEGIN
///        IF NEW.id > 5 THEN NEW.v := NEW.w; END IF;
///        RETURN NEW;
///      END $$;
///      CREATE TRIGGER stamp BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION stamp();",
/// )?;
/// let trigger = db.triggers().next().unwrap();
/// assert!(trigger.is_maintenance_trigger(&db)?);
/// let undeclared: Vec<_> = trigger.maintenance_undeclared_fields(&db)?.collect();
/// assert_eq!(undeclared.len(), 1);
/// assert_eq!(undeclared[0].row(), TriggerRow::New);
/// assert!(undeclared[0].is_assigned());
/// assert_eq!(undeclared[0].sqlstate().as_str(), "42703");
/// assert_eq!(undeclared[0].to_string(), r#"record "new" has no field "v""#);
/// assert_eq!(undeclared[0].field().span.start.line, 3);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    r#"record "{}" has no field "{}""#,
    .row.name(),
    stored_identifier(&.field.value, .field.quote_style.is_some())
)]
pub struct UndeclaredRowField {
    row: TriggerRow,
    field: Ident,
    assigned: bool,
}

impl UndeclaredRowField {
    /// The field `field` of `row`, which the body assigns when `assigned`.
    pub(crate) const fn new(row: TriggerRow, field: Ident, assigned: bool) -> Self {
        Self { row, field, assigned }
    }

    /// Returns the row the body names the field of.
    #[must_use]
    pub const fn row(&self) -> TriggerRow {
        self.row
    }

    /// Returns the field as the body writes it, carrying where, counted as
    /// [`BodyExcerpt::span`](crate::structs::BodyExcerpt::span) counts.
    #[must_use]
    pub const fn field(&self) -> &Ident {
        &self.field
    }

    /// Returns whether the body assigns the field, as in `NEW.v := 1`, rather
    /// than reads it.
    #[must_use]
    pub const fn is_assigned(&self) -> bool {
        self.assigned
    }

    /// Returns the SQLSTATE PostgreSQL raises, `42703`.
    #[must_use]
    pub const fn sqlstate(&self) -> SqlState {
        sqlstate::pg::UNDEFINED_COLUMN
    }
}
