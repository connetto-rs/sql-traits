//! Native parsing with each statement's opening-token location.

use alloc::{string::String, vec::Vec};

use sqlparser::{
    ast::{Statement, comments::Comments},
    dialect::Dialect,
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Location, Token, Tokenizer},
};

/// The statements of one SQL text, with the comments and line starts that
/// tie them back to it.
pub(super) struct ParsedSource {
    pub(super) statements: Vec<(Statement, Location)>,
    pub(super) comments: Comments,
    /// The byte offset each line of the text starts at.
    pub(super) line_starts: Vec<usize>,
}

/// One statement's place in the SQL text it was parsed from.
#[derive(Clone, Copy)]
pub(super) struct StatementSource<'a> {
    pub(super) comments: &'a Comments,
    pub(super) start: Location,
    pub(super) text: SourceText<'a>,
}

/// The SQL text a statement was parsed from, addressable by token location.
#[derive(Clone, Copy)]
pub(super) struct SourceText<'a> {
    pub(super) sql: &'a str,
    pub(super) line_starts: &'a [usize],
}

impl SourceText<'_> {
    /// The byte offset of `location`, or `None` for an empty location or one
    /// outside the text.
    pub(super) fn offset(&self, location: Location) -> Option<usize> {
        let line_index = usize::try_from(location.line).ok()?.checked_sub(1)?;
        let column_index = usize::try_from(location.column).ok()?.checked_sub(1)?;
        let line_start = *self.line_starts.get(line_index)?;
        let line = &self.sql[line_start..];
        let within = line
            .char_indices()
            .map(|(at, _)| at)
            .chain(core::iter::once(line.len()))
            .nth(column_index)?;
        Some(line_start + within)
    }
}

impl ParsedSource {
    /// Uses the default `Parser::parse_statements` boundary policy.
    pub(super) fn parse(sql: &str, dialect: &dyn Dialect) -> Result<Self, ParserError> {
        let mut parser = Parser::new(dialect).try_with_sql(sql)?;
        let mut statements = Vec::new();
        let mut expecting_delimiter = false;
        loop {
            while parser.consume_token(&Token::SemiColon) {
                expecting_delimiter = false;
            }
            let token = parser.peek_token_ref();
            match &token.token {
                Token::EOF => break,
                Token::Word(word) if expecting_delimiter && word.keyword == Keyword::END => break,
                _ => {}
            }
            if expecting_delimiter {
                return parser.expected_ref("end of statement", token);
            }
            let start = token.span.start;
            let statement = parser.parse_statement()?;
            statements.push((statement, start));
            expecting_delimiter = true;
        }
        let line_starts =
            core::iter::once(0).chain(sql.match_indices('\n').map(|(at, _)| at + 1)).collect();
        Ok(Self { statements, comments: parser.into_comments(), line_starts })
    }
}

/// The text of `text` from `start` up to `end`, the start of the statement
/// after, or to the end of the text for the last statement.
pub(super) fn statement_text(text: SourceText<'_>, start: Location, end: Option<Location>) -> &str {
    let length = text.sql.len();
    let from = text.offset(start).unwrap_or(length);
    let to = end.and_then(|end| text.offset(end)).unwrap_or(length);
    text.sql.get(from..to.max(from)).unwrap_or_default()
}

/// Respells `token`, a keyword in its uppercase spelling, as the first word of
/// `statement` spelling that keyword, which is how PostgreSQL echoes the token
/// a syntax error is reported at. A token no word of `statement` spells, or a
/// statement the tokenizer refuses, leaves `token` as it is.
pub(super) fn respell_written_token(token: &mut String, statement: &str, dialect: &dyn Dialect) {
    let Ok(tokens) = Tokenizer::new(dialect, statement).tokenize() else { return };
    let written = tokens.into_iter().find_map(|candidate| {
        match candidate {
            Token::Word(word)
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(token) =>
            {
                Some(word.value)
            }
            _ => None,
        }
    });
    if let Some(written) = written {
        *token = written;
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::PostgreSqlDialect;

    use super::*;

    /// A keyword is respelled as the failing statement writes it, never from a
    /// statement before or after it.
    #[test]
    fn written_token_comes_from_its_own_statement() {
        let sql = "create temp table a (id int);\nCREATE Temp\n  MATERIALIZED VIEW v AS SELECT 1;\nselect 1";
        let dialect = PostgreSqlDialect {};
        let source = ParsedSource::parse(sql, &dialect).expect("the statements parse");
        let text = SourceText { sql, line_starts: &source.line_starts };
        let starts: Vec<Location> = source.statements.iter().map(|(_, start)| *start).collect();
        let statement = statement_text(text, starts[1], starts.get(2).copied());
        assert_eq!(statement, "CREATE Temp\n  MATERIALIZED VIEW v AS SELECT 1;\n");
        let mut token = String::from("TEMP");
        respell_written_token(&mut token, statement, &dialect);
        assert_eq!(token, "Temp");
        let mut absent = String::from("UNLOGGED");
        respell_written_token(&mut absent, statement, &dialect);
        assert_eq!(absent, "UNLOGGED");
        assert_eq!(statement_text(text, starts[2], None), "select 1");
    }
}
