//! Documentation from standalone comments above opening tokens.

use alloc::string::String;

use sqlparser::{
    ast::{CreateTable, Spanned, comments::Comments},
    tokenizer::Location,
};

use super::{column_texts::ColumnTexts, parsed_source::StatementSource};

/// Documentation declared by a `CREATE TABLE` statement.
#[derive(Debug, Default)]
pub(super) struct TableDocumentation {
    pub(super) table: Option<String>,
    pub(super) columns: ColumnTexts,
}

impl TableDocumentation {
    pub(super) fn from_source(create_table: &CreateTable, source: StatementSource<'_>) -> Self {
        Self {
            table: leading(source.comments, source.start),
            columns: create_table
                .columns
                .iter()
                .filter_map(|column| {
                    let documentation = leading(source.comments, column.span().start)?;
                    Some((column.name.clone(), documentation))
                })
                .collect(),
        }
    }
}

fn leading(comments: &Comments, node: Location) -> Option<String> {
    let mut first = None;
    let mut previous_end: Option<Location> = None;
    for comment in comments.find_before(node) {
        let start = comment.span.start;
        let standalone = comment.prev_token_end.is_none_or(|end| end.line < start.line)
            && previous_end.is_none_or(|end| end.line < start.line);
        if !standalone {
            first = None;
        } else if first.is_none()
            || previous_end.is_none_or(|end| end.line.checked_add(1) != Some(start.line))
        {
            first = Some(start);
        }
        previous_end = Some(comment.span.end);
    }
    if previous_end?.line.checked_add(1) != Some(node.line) {
        return None;
    }
    let first = first?;
    let mut documentation = String::new();
    for line in
        comments.find(first..node).flat_map(|comment| comment.as_str().lines().map(str::trim))
    {
        if documentation.is_empty() && line.is_empty() {
            continue;
        }
        if !documentation.is_empty() {
            documentation.push('\n');
        }
        documentation.push_str(line);
    }
    documentation.truncate(documentation.trim_end().len());
    (!documentation.is_empty()).then_some(documentation)
}
