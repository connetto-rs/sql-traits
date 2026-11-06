//! `ALTER ROLE ... SET` and `RESET`, and the session settings `SET ... FROM
//! CURRENT` reads.
//!
//! PostgreSQL keeps one `pg_db_role_setting` row per role, or every role, and
//! database, or every database, holding each setting's value as text. A
//! session starts from those rows and `postgresql.conf`, which the input never
//! shows, so the only session values known here are the ones the input sets.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

use sql_values::PgTimeZone;
use sqlparser::ast::{
    AlterRoleOperation, ContextModifier, DiscardObject, Expr, Ident, ObjectName, ObjectNamePart,
    Reset, ResetConfig, ResetStatement, Set, SetConfigValue, Statement,
};

use super::{ParserDBBuilder, SessionRoles, stored_role_name};
use crate::{
    errors::Error,
    structs::RoleSetting,
    utils::{
        identifier_resolution::{SessionPrincipal, normalize_identifier, session_principal},
        object_name::{require_static_object_name, setting_value},
    },
};

/// One setting's value as the session holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    name: String,
    value: String,
}

/// The values a session's own `SET` statements leave, as `SET ... FROM
/// CURRENT` reads them.
///
/// A setting is absent while its value is not known, because the input never
/// set it or returned it to the value the session started with. A `SET LOCAL`
/// holds until its transaction ends, a plain `SET` lasts past it, and a
/// rollback restores the catalog copy carrying this state.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionSettings {
    committed: Vec<Held>,
    local: Option<Vec<Held>>,
}

/// What one statement does to the session's settings.
#[derive(Clone)]
enum Change {
    /// Sets `name` to a known value, or to one the input leaves unknown.
    Set { name: String, value: Option<String> },
    /// Returns every setting to the value the session started with.
    ResetAll,
}

impl SessionSettings {
    /// The value `name` holds now, compared without case.
    fn current(&self, name: &str) -> Option<&str> {
        self.local
            .as_ref()
            .unwrap_or(&self.committed)
            .iter()
            .find(|held| held.name.eq_ignore_ascii_case(name))
            .map(|held| held.value.as_str())
    }

    /// Applies what `statement` does to the session's settings. A `SET
    /// LOCAL` outside a transaction changes nothing, as PostgreSQL only warns
    /// about it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTimeZone`] for a `timezone` PostgreSQL refuses.
    pub(super) fn apply(
        &mut self,
        statement: &Statement,
        in_transaction: bool,
        hex_strings_may_be_integers: bool,
    ) -> Result<(), Error> {
        let Some((change, local)) = change_of(statement, hex_strings_may_be_integers)? else {
            return Ok(());
        };
        if local {
            if in_transaction {
                let mut overlay = self.local.take().unwrap_or_else(|| self.committed.clone());
                change.apply_to(&mut overlay);
                self.local = Some(overlay);
            }
            return Ok(());
        }
        // A plain `SET` inside a transaction also replaces what a `SET LOCAL`
        // holds until the transaction ends.
        if let Some(overlay) = &mut self.local {
            change.clone().apply_to(overlay);
        }
        change.apply_to(&mut self.committed);
        Ok(())
    }

    /// Drops what a `SET LOCAL` held, at the end of its transaction.
    pub(super) fn end_transaction(&mut self) {
        self.local = None;
    }
}

/// The change `statement` makes to the session's settings and whether it
/// wrote `LOCAL`.
fn change_of(
    statement: &Statement,
    hex_strings_may_be_integers: bool,
) -> Result<Option<(Change, bool)>, Error> {
    let set = |name: String, values: &[Expr]| -> Result<Change, Error> {
        let value = match values {
            [Expr::Identifier(ident)]
                if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("DEFAULT") =>
            {
                None
            }
            [value] => literal(value, hex_strings_may_be_integers),
            // A list such as a `search_path` is printed with quoting rules
            // of its own, so its text stays unknown.
            _ => None,
        };
        if let Some(value) = &value {
            check_time_zone(&name, value)?;
        }
        Ok(Change::Set { name, value })
    };
    Ok(Some(match statement {
        Statement::Set(Set::SingleAssignment { scope, hivevar: false, variable, values }) => {
            (set(setting_name(variable)?, values)?, *scope == Some(ContextModifier::Local))
        }
        Statement::Set(Set::SetTimeZone { local, value }) => {
            let reset = matches!(value, Expr::Identifier(ident)
                if ident.quote_style.is_none()
                    && (ident.value.eq_ignore_ascii_case("LOCAL")
                        || ident.value.eq_ignore_ascii_case("DEFAULT")));
            let values = if reset { &[][..] } else { core::slice::from_ref(value) };
            (set("timezone".to_string(), values)?, *local)
        }
        Statement::Reset(ResetStatement { reset: Reset::ALL })
        | Statement::Discard { object_type: DiscardObject::ALL } => (Change::ResetAll, false),
        Statement::Reset(ResetStatement { reset: Reset::ConfigurationParameter(name) }) => {
            (Change::Set { name: setting_name(name)?, value: None }, false)
        }
        _ => return Ok(None),
    }))
}

impl Change {
    fn apply_to(self, held: &mut Vec<Held>) {
        match self {
            Self::ResetAll => held.clear(),
            Self::Set { name, value } => {
                let position = held.iter().position(|kept| kept.name.eq_ignore_ascii_case(&name));
                match (position, value) {
                    (Some(position), Some(value)) => held[position].value = value,
                    (Some(position), None) => {
                        held.remove(position);
                    }
                    (None, Some(value)) => held.push(Held { name, value }),
                    (None, None) => {}
                }
            }
        }
    }
}

/// A setting's or a database's name, each part folded as PostgreSQL folds an
/// identifier.
///
/// # Errors
///
/// Returns [`Error::IdentifierLookupError`] for a part built at run time.
fn setting_name(name: &ObjectName) -> Result<String, Error> {
    require_static_object_name(name)?;
    let mut parts = name
        .0
        .iter()
        .filter_map(ObjectNamePart::as_ident)
        .map(|part| normalize_identifier(&part.value, part.quote_style.is_some()));
    let mut joined = parts.next().map(Cow::into_owned).unwrap_or_default();
    for part in parts {
        joined.push('.');
        joined.push_str(&part);
    }
    Ok(joined)
}

/// A setting's value as PostgreSQL prints it, or [`None`] for an expression
/// its grammar refuses. A bare name folds its ASCII letters, as PostgreSQL's
/// scanner downcases an identifier, and every other value stays as written.
fn literal(value: &Expr, hex_strings_may_be_integers: bool) -> Option<String> {
    setting_value(value, hex_strings_may_be_integers)
        .map(|(text, exact)| if exact { text.into_owned() } else { text.to_ascii_lowercase() })
}

/// Refuses a `timezone` value PostgreSQL refuses.
fn check_time_zone(name: &str, value: &str) -> Result<(), Error> {
    if name.eq_ignore_ascii_case("timezone")
        && let Err(reason) = PgTimeZone::parse(value)
    {
        return Err(Error::InvalidTimeZone { value: value.to_string(), reason });
    }
    Ok(())
}

/// Records what an `ALTER ROLE ... SET` or `RESET` statement leaves in
/// `pg_db_role_setting`, returning whether `statement` was one.
///
/// `ALL` unquoted names every role, and `CURRENT_USER`, `CURRENT_ROLE` and
/// `SESSION_USER` the role the session runs as. A role the input never
/// created is kept under the name it was set on, as Supabase's platform roles
/// are.
///
/// # Errors
///
/// Returns [`Error::NonLiteralSetting`] for a value PostgreSQL's grammar
/// refuses, [`Error::UnknownCurrentSetting`] for `FROM CURRENT` on a value the
/// input leaves unknown, [`Error::InvalidTimeZone`] for a `timezone`
/// PostgreSQL refuses and [`Error::UnnamedSessionRoleSetting`] for a session
/// role the input never names.
pub(super) fn alter_role(
    builder: &mut ParserDBBuilder,
    session_roles: &SessionRoles,
    session_settings: &SessionSettings,
    statement: &Statement,
    hex_strings_may_be_integers: bool,
) -> Result<bool, Error> {
    let database = |in_database: &Option<ObjectName>| in_database.as_ref().map(setting_name);
    match statement {
        Statement::AlterRole {
            name: role,
            operation: AlterRoleOperation::Set { config_name, config_value, in_database },
        } => {
            let role = setting_role(role, session_roles, statement)?;
            let name = setting_name(config_name)?;
            let database = database(in_database).transpose()?;
            let value = match config_value {
                SetConfigValue::Default => None,
                SetConfigValue::FromCurrent => {
                    let Some(value) = session_settings.current(&name) else {
                        return Err(Error::UnknownCurrentSetting {
                            setting: name,
                            statement: statement.to_string(),
                        });
                    };
                    Some(value.to_string())
                }
                SetConfigValue::Value(value) => {
                    let Some(value) = literal(value, hex_strings_may_be_integers) else {
                        return Err(Error::NonLiteralSetting { statement: statement.to_string() });
                    };
                    check_time_zone(&name, &value)?;
                    Some(value)
                }
            };
            let entries = builder.role_settings_mut();
            let position = entries.iter().position(|entry| {
                entry.is_keyed(role.as_deref(), database.as_deref()) && entry.is_named(&name)
            });
            match (position, value) {
                (Some(position), Some(value)) => entries[position].value = value,
                (Some(position), None) => {
                    entries.remove(position);
                }
                (None, Some(value)) => entries.push(RoleSetting { role, database, name, value }),
                (None, None) => {}
            }
        }
        Statement::AlterRole {
            name: role,
            operation: AlterRoleOperation::Reset { config_name, in_database },
        } => {
            let role = setting_role(role, session_roles, statement)?;
            let database = database(in_database).transpose()?;
            let name = match config_name {
                ResetConfig::ALL => None,
                ResetConfig::ConfigName(name) => Some(setting_name(name)?),
            };
            builder.role_settings_mut().retain(|entry| {
                !(entry.is_keyed(role.as_deref(), database.as_deref())
                    && name.as_deref().is_none_or(|name| entry.is_named(name)))
            });
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The stored name of the role `ALTER ROLE` names, [`None`] for `ALL`.
fn setting_role(
    role: &Ident,
    session_roles: &SessionRoles,
    statement: &Statement,
) -> Result<Option<String>, Error> {
    let quoted = role.quote_style.is_some();
    if !quoted && role.value.eq_ignore_ascii_case("ALL") {
        return Ok(None);
    }
    let named = match session_principal(&role.value, quoted) {
        Some(SessionPrincipal::CurrentUser) => session_roles.creating_role(),
        Some(SessionPrincipal::SessionUser) => session_roles.session_role(),
        None => return Ok(Some(stored_role_name(role))),
    };
    named
        .map(|named| Some(named.to_string()))
        .ok_or_else(|| Error::UnnamedSessionRoleSetting { statement: statement.to_string() })
}

/// Moves the entries of the role stored as `old` to `new`, as PostgreSQL keys
/// them by the role's oid.
pub(super) fn rename_role(builder: &mut ParserDBBuilder, old: &Ident, new: &Ident) {
    let old = stored_role_name(old);
    let new = stored_role_name(new);
    for entry in builder.role_settings_mut() {
        if entry.role.as_deref() == Some(old.as_str()) {
            entry.role = Some(new.clone());
        }
    }
}

/// Removes the entries of a dropped role, as `DROP ROLE` deletes its rows.
pub(super) fn drop_role(builder: &mut ParserDBBuilder, role: &Ident) {
    let role = stored_role_name(role);
    builder.role_settings_mut().retain(|entry| entry.role.as_deref() != Some(role.as_str()));
}
