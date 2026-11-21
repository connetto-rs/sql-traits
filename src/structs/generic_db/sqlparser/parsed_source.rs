//! Native parsing with each statement's opening-token location.

use alloc::vec::Vec;

use sqlparser::{
    ast::{Statement, comments::Comments},
    dialect::Dialect,
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Location, Token},
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
