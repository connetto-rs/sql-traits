//! A label naming the rows of a `VALUES` column.

use sql_traits::prelude::*;

/// Each row's definition as `render` labels it, in row order, inside
/// `values(…)`.
pub(crate) fn values_label(
    rows: ColumnDefinitionList<'_, '_, '_, ParserDB>,
    render: impl Fn(&ColumnDefinition<'_, '_, '_, ParserDB>) -> String,
) -> String {
    let rows = rows.iter().map(|row| render(&row.definition())).collect::<Vec<_>>();
    format!("values({})", rows.join(", "))
}
