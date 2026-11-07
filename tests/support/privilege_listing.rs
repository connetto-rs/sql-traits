//! Spells a `TablePrivileges` answer as a list of privilege names.

use sql_traits::prelude::*;

type Column = <ParserDB as DatabaseLike>::Column;

/// Every table privilege, in the order [`privilege_names`] spells them.
pub(crate) const EVERYTHING: [&str; 8] =
    ["SELECT", "INSERT", "UPDATE", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER", "MAINTAIN"];

/// Each privilege the answer gives, the column ones spelled with their
/// columns when it gives them on columns alone.
pub(crate) fn privilege_names(privileges: &TablePrivileges<'_, Column>) -> Vec<String> {
    let mut names = Vec::new();
    for (name, privilege) in [
        ("SELECT", &privileges.select),
        ("INSERT", &privileges.insert),
        ("UPDATE", &privileges.update),
        ("REFERENCES", &privileges.references),
    ] {
        match privilege {
            ColumnPrivilege::Table => names.push(name.to_string()),
            ColumnPrivilege::Columns(columns) if columns.is_empty() => {}
            ColumnPrivilege::Columns(columns) => {
                let columns: Vec<&str> = columns.iter().map(ColumnLike::column_name).collect();
                names.push(format!("{name} ({})", columns.join(", ")));
            }
        }
    }
    for (name, holds) in [
        ("DELETE", privileges.delete),
        ("TRUNCATE", privileges.truncate),
        ("TRIGGER", privileges.trigger),
        ("MAINTAIN", privileges.maintain),
    ] {
        if holds {
            names.push(name.to_string());
        }
    }
    names
}
