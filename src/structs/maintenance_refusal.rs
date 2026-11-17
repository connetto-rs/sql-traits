//! Why a trigger is no maintenance trigger.

use alloc::{boxed::Box, string::String};

use sqlparser::{
    ast::{Expr, Ident},
    tokenizer::{Location, Span},
};

/// What makes a trigger no maintenance trigger, as
/// [`TriggerLike::maintenance_refusal`](crate::traits::TriggerLike::maintenance_refusal)
/// answers.
///
/// The first part of the body, in the order written, that a maintenance body
/// may not hold decides. Every [`Span`] counts lines and columns from 1 in
/// the body [`FunctionLike::body`](crate::traits::FunctionLike::body)
/// returns, as `sqlparser` counts them.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE t (txt TEXT);
///      CREATE TABLE audit (txt TEXT);
///      CREATE FUNCTION maintain() RETURNS TRIGGER LANGUAGE plpgsql AS $$
///      BEGIN
///        NEW.txt := upper(NEW.txt);
///        INSERT INTO audit (txt) VALUES (NEW.txt);
///        RETURN NEW;
///      END $$;
///      CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION maintain();",
/// )?;
/// let trigger = db.triggers().next().unwrap();
/// let Some(MaintenanceRefusal::Statement(insert)) = trigger.maintenance_refusal(&db)? else {
///     panic!("the INSERT refuses the body");
/// };
/// assert_eq!(insert.text(), "INSERT INTO audit (txt) VALUES (NEW.txt);");
/// assert_eq!(insert.span().start.line, 4);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaintenanceRefusal {
    /// The trigger runs a function the database does not declare, or one
    /// without a body.
    #[error("the trigger's function has no body")]
    NoBody,
    /// The body does not parse as PL/pgSQL.
    #[error("the body does not parse: {reason}")]
    UnparsableBody {
        /// The parser's account of what it could not read.
        reason: String,
    },
    /// The body declares a variable, named as PostgreSQL stores it.
    #[error("the body declares `{0}`")]
    Declaration(String),
    /// The body handles errors with an `EXCEPTION` clause.
    #[error("the body handles errors")]
    Handler,
    /// A statement that neither assigns one column of `NEW` nor is an `IF`
    /// over such assignments, such as `RAISE`, `PERFORM`, a query, a loop or
    /// a nested block.
    #[error(
        "`{}` at line {}, column {} neither assigns a column of NEW nor is an IF over such assignments",
        .0.text, .0.span.start.line, .0.span.start.column
    )]
    Statement(BodyExcerpt),
    /// A `RETURN` before the last statement of the body.
    #[error(
        "`{}` at line {}, column {} returns before the end of the body",
        .0.text, .0.span.start.line, .0.span.start.column
    )]
    EarlyReturn(BodyExcerpt),
    /// The last statement of the body, which does not return `NEW`.
    #[error(
        "`{}` at line {}, column {} ends the body without returning NEW",
        .0.text, .0.span.start.line, .0.span.start.column
    )]
    NoReturnOfNew(BodyExcerpt),
    /// A body without statements.
    #[error("the body has no statement")]
    NoStatement,
    /// A read of `NEW` that no expression over the row before the trigger ran
    /// writes, once a statement before it assigns a column. That is the whole
    /// row as in `NEW::text`, a wildcard as in `ROW(NEW.*)`, or a field inside
    /// an assigned column as in `NEW.a.x` after `NEW.a :=`.
    #[error(
        "`{read}` in `{}` at line {}, column {} reads NEW after a column of it is assigned",
        .statement.text, .statement.span.start.line, .statement.span.start.column
    )]
    ReadAfterAssignment {
        /// The read, whose identifiers and tokens carry where the body
        /// writes them, which [`sqlparser::ast::Spanned`] reads.
        read: Box<Expr>,
        /// The assignment or `IF` holding the read.
        statement: BodyExcerpt,
    },
    /// A body that returns `NEW` and assigns no column of it.
    #[error("the body assigns no column of NEW")]
    NothingAssigned,
    /// A field of `NEW` the body assigns or reads that the trigger's table
    /// does not declare, as the body writes it.
    #[error(
        "the table declares no column `{field}`, which the body writes at line {}, column {}",
        .field.span.start.line, .field.span.start.column
    )]
    UndeclaredColumn {
        /// The field, carrying where the body writes it.
        field: Ident,
    },
}

/// A statement of a trigger body, as the body writes it and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyExcerpt {
    text: String,
    span: Span,
}

impl BodyExcerpt {
    /// The text of `body` that `span` covers.
    pub(crate) fn new(body: &str, span: Span) -> Self {
        let text = body.get(offset(body, span.start)..offset(body, span.end)).unwrap_or_default();
        Self { text: text.into(), span }
    }

    /// Returns the statement's text, from its first token through the `;`
    /// that ends it.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns where the body writes the statement.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// The byte offset in `body` of `location`, whose line and column count from
/// 1 and whose column counts characters.
fn offset(body: &str, location: Location) -> usize {
    let skip = |count: u64| usize::try_from(count.saturating_sub(1)).unwrap_or(usize::MAX);
    let line: usize = body.split_inclusive('\n').take(skip(location.line)).map(str::len).sum();
    line + body[line..].chars().take(skip(location.column)).map(char::len_utf8).sum::<usize>()
}
