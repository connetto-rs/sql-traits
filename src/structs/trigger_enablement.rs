//! When a trigger fires, as PostgreSQL's `pg_trigger.tgenabled` records it.

/// Which sessions a trigger fires in, mirroring `pg_trigger.tgenabled`.
///
/// A session's `session_replication_role` decides which of these fire in it.
/// `origin` and `local` fire [`Self::Origin`] and [`Self::Always`] triggers,
/// `replica` fires [`Self::Replica`] and [`Self::Always`] triggers, and
/// [`Self::Disabled`] triggers fire in none. The setting changes no trigger's
/// enablement.
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
///      CREATE FUNCTION audit() RETURNS TRIGGER AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql;
///      CREATE TRIGGER audit AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION audit();
///      ALTER TABLE t ENABLE REPLICA TRIGGER audit;",
/// )?;
/// let trigger = db.triggers().next().unwrap();
/// assert_eq!(trigger.enablement(&db)?, TriggerEnablement::Replica);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TriggerEnablement {
    /// `tgenabled = 'O'`, firing in the `origin` and `local` roles, as a
    /// trigger does once created or after `ENABLE TRIGGER`.
    #[default]
    Origin,
    /// `tgenabled = 'D'`, firing in no session, after `DISABLE TRIGGER`.
    Disabled,
    /// `tgenabled = 'R'`, firing in the `replica` role only, after `ENABLE
    /// REPLICA TRIGGER`.
    Replica,
    /// `tgenabled = 'A'`, firing in every session, after `ENABLE ALWAYS
    /// TRIGGER`.
    Always,
}
