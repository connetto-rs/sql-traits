//! Spells a `TablePrivileges` answer as a list of privilege names.

use sql_traits::prelude::*;

type Column = <ParserDB as DatabaseLike>::Column;

/// Every table privilege, in the order [`privilege_names`] spells them.
pub(crate) const EVERYTHING: [&str; 8] =
    ["SELECT", "INSERT", "UPDATE", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER", "MAINTAIN"];

/// Each privilege a table answer gives, the column ones spelled with their
/// columns when it gives them on columns alone.
pub(crate) fn privilege_names(privileges: &TablePrivileges<&Column>) -> Vec<String> {
    spelled(privileges, ColumnLike::column_name)
}

/// Each privilege a view answer gives, the column ones spelled with the
/// output column names `outputs` lists by position when it gives them on
/// columns alone.
pub(crate) fn view_privilege_names(
    privileges: &TablePrivileges<usize>,
    outputs: &[&str],
) -> Vec<String> {
    spelled(privileges, |position| outputs[position])
}

fn spelled<'a, C: Copy>(
    privileges: &TablePrivileges<C>,
    name: impl Fn(C) -> &'a str,
) -> Vec<String> {
    let mut names = Vec::new();
    for (privilege_name, privilege) in [
        ("SELECT", &privileges.select),
        ("INSERT", &privileges.insert),
        ("UPDATE", &privileges.update),
        ("REFERENCES", &privileges.references),
    ] {
        match privilege {
            ColumnPrivilege::Table => names.push(privilege_name.to_string()),
            ColumnPrivilege::Columns(columns) if columns.is_empty() => {}
            ColumnPrivilege::Columns(columns) => {
                let columns: Vec<&str> = columns.iter().map(|column| name(*column)).collect();
                names.push(format!("{privilege_name} ({})", columns.join(", ")));
            }
        }
    }
    for (privilege_name, holds) in [
        ("DELETE", privileges.delete),
        ("TRUNCATE", privileges.truncate),
        ("TRIGGER", privileges.trigger),
        ("MAINTAIN", privileges.maintain),
    ] {
        if holds {
            names.push(privilege_name.to_string());
        }
    }
    names
}
