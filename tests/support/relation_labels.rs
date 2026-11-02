//! A label naming a relation by its kind and name.

use sql_traits::prelude::*;

/// The kind of `relation` followed by its name, such as `materialized view m`.
pub(crate) fn relation_label(relation: Relation<'_, ParserDB>) -> String {
    match relation {
        Relation::Table(table) => format!("table {}", table.table_name()),
        Relation::View(view) => format!("view {}", view.view_name()),
        Relation::MaterializedView(view) => format!("materialized view {}", view.view_name()),
        Relation::Sequence(sequence) => format!("sequence {}", sequence.sequence_name()),
    }
}
