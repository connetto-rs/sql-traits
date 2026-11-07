//! One `ALTER ROLE ... SET` entry, keyed as `pg_db_role_setting` keys it.

use alloc::string::String;

use sql_values::PgTimeZone;

/// The value a role, or every role, starts a session with in one database, or
/// in every database, as `ALTER ROLE { name | ALL } [IN DATABASE d] SET`
/// leaves it.
///
/// An entry for every role in one database is the one `ALTER DATABASE d SET`
/// writes too.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE ROLE reader;
///      ALTER ROLE reader IN DATABASE app SET TimeZone = 'Europe/Rome';
///      ALTER ROLE ALL SET statement_timeout = '8s';",
/// )?;
/// let entries: Vec<_> = db.role_settings().collect();
///
/// assert_eq!(entries[0].role(), Some("reader"));
/// assert_eq!(entries[0].database(), Some("app"));
/// assert!(entries[0].is_named("timezone"));
/// assert_eq!(entries[0].value(), "Europe/Rome");
///
/// assert_eq!(entries[1].role(), None);
/// assert_eq!(entries[1].database(), None);
/// assert_eq!(entries[1].value(), "8s");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoleSetting {
    pub(crate) role: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) zone: Option<PgTimeZone>,
}

impl RoleSetting {
    /// The role the entry applies to, by the name PostgreSQL stores, or
    /// [`None`] for every role.
    #[must_use]
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    /// The database the entry applies in, by the name PostgreSQL stores, or
    /// [`None`] for every database.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// The setting's name, folded as PostgreSQL folds an unquoted identifier.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the entry sets `name`, compared without case as PostgreSQL
    /// compares setting names.
    #[must_use]
    pub fn is_named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// The value, as PostgreSQL prints it after `name=` in `setconfig`.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether the entry has the key of one `pg_db_role_setting` row.
    pub(crate) fn is_keyed(&self, role: Option<&str>, database: Option<&str>) -> bool {
        self.role.as_deref() == role && self.database.as_deref() == database
    }

    /// The zone a `TimeZone` entry sets, as [`PgTimeZone::parse`] reads its
    /// value, or [`None`] for an entry of another name.
    pub(crate) fn time_zone(&self) -> Option<&PgTimeZone> {
        self.zone.as_ref()
    }
}
