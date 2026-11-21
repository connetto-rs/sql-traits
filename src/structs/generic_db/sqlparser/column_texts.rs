//! Text a statement gives each column of a table, keyed by the column's name.

use alloc::{string::String, vec::Vec};

use sqlparser::ast::Ident;

/// Text for some columns of one table, each keyed by its column's name.
#[derive(Debug, Default)]
pub(super) struct ColumnTexts(Vec<(Ident, String)>);

impl ColumnTexts {
    /// Moves out the text of the column `name`.
    pub(super) fn take(&mut self, name: &Ident) -> Option<String> {
        let position = self.0.iter().position(|(column, _)| {
            column.value == name.value && column.quote_style.is_some() == name.quote_style.is_some()
        })?;
        Some(self.0.swap_remove(position).1)
    }
}

impl FromIterator<(Ident, String)> for ColumnTexts {
    fn from_iter<I: IntoIterator<Item = (Ident, String)>>(texts: I) -> Self {
        Self(texts.into_iter().collect())
    }
}
