//! The `OPTIONS ( [ ADD | SET | DROP ] option ['value'] [, ...] )` changes an
//! `ALTER` makes to a foreign table and to its columns.

use alloc::{borrow::Cow, string::String, vec::Vec};

use sqlparser::ast::{
    AlterGenericOption, ColumnDef, ColumnOption, ColumnOptionDef, CreateServerOption,
};

use crate::utils::identifier_resolution::normalize_identifier;

/// Why PostgreSQL refuses a change, with the key it names as PostgreSQL
/// stores it.
pub(super) enum Refusal {
    /// `ADD` names a key the list already holds.
    Repeated(String),
    /// `SET` or `DROP` names a key the list does not hold.
    Missing(String),
}

impl Refusal {
    /// The error naming the refused key on `table_name`, and on `column_name`
    /// when the list is a column's.
    pub(super) fn into_error(
        self,
        table_name: String,
        column_name: Option<String>,
    ) -> crate::errors::Error {
        match self {
            Self::Repeated(option_name) => {
                crate::errors::Error::RepeatedForeignOption { table_name, column_name, option_name }
            }
            Self::Missing(option_name) => {
                crate::errors::Error::ForeignOptionNotFound { table_name, column_name, option_name }
            }
        }
    }
}

/// The key of `option`, as PostgreSQL stores it.
fn stored_key(option: &CreateServerOption) -> Cow<'_, str> {
    normalize_identifier(&option.key.value, option.key.quote_style.is_some())
}

/// Applies `changes` to `options` one after the other, as PostgreSQL does:
/// `ADD` appends, `SET` replaces the value in place and `DROP` removes.
///
/// `key_of` reads the stored key of an entry and `entry` builds one from a
/// written option.
pub(super) fn change<T>(
    options: &mut Vec<T>,
    changes: &[AlterGenericOption],
    key_of: impl Fn(&T) -> Cow<'_, str>,
    entry: impl Fn(&CreateServerOption) -> T,
) -> Result<(), Refusal> {
    for change in changes {
        let (key, written) = match change {
            AlterGenericOption::Add { option, .. } | AlterGenericOption::Set(option) => {
                (stored_key(option), Some(option))
            }
            AlterGenericOption::Drop(key) => {
                (normalize_identifier(&key.value, key.quote_style.is_some()), None)
            }
        };
        let held = options.iter().position(|option| key_of(option) == key);
        match (change, held, written) {
            (AlterGenericOption::Add { .. }, None, Some(option)) => options.push(entry(option)),
            (AlterGenericOption::Add { .. }, Some(_), _) => {
                return Err(Refusal::Repeated(key.into_owned()));
            }
            (AlterGenericOption::Set(_), Some(at), Some(option)) => options[at] = entry(option),
            (AlterGenericOption::Drop(_), Some(at), _) => {
                options.remove(at);
            }
            _ => return Err(Refusal::Missing(key.into_owned())),
        }
    }
    Ok(())
}

/// Applies `changes` to the options `column` declares, dropping the clause
/// once it holds none.
pub(super) fn change_column(
    column: &mut ColumnDef,
    changes: &[AlterGenericOption],
) -> Result<(), Refusal> {
    let mut options: Vec<CreateServerOption> = column
        .options
        .iter()
        .filter_map(|option| {
            match &option.option {
                ColumnOption::GenericOptions(options) => Some(options),
                _ => None,
            }
        })
        .flatten()
        .cloned()
        .collect();
    change(&mut options, changes, stored_key, CreateServerOption::clone)?;
    let at = column
        .options
        .iter()
        .position(|option| matches!(option.option, ColumnOption::GenericOptions(_)));
    column.options.retain(|option| !matches!(option.option, ColumnOption::GenericOptions(_)));
    if !options.is_empty() {
        let clause = ColumnOptionDef { name: None, option: ColumnOption::GenericOptions(options) };
        column.options.insert(at.unwrap_or(column.options.len()), clause);
    }
    Ok(())
}

/// Applies `changes` to the table-level options of a foreign table, each
/// stored as its key and value.
pub(super) fn change_table(
    options: &mut Vec<(String, String)>,
    changes: &[AlterGenericOption],
) -> Result<(), Refusal> {
    change(
        options,
        changes,
        |(key, _)| Cow::Borrowed(key.as_str()),
        |option| (stored_key(option).into_owned(), option.value.value.clone()),
    )
}
