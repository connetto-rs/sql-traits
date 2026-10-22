//! Native parsing with each statement's opening-token location.

use alloc::vec::Vec;

use sqlparser::{
    ast::{Statement, comments::Comments},
    dialect::Dialect,
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Location, Token},
};

pub(super) struct ParsedSource {
    pub(super) statements: Vec<(Statement, Location)>,
    pub(super) comments: Comments,
}

#[derive(Clone, Copy)]
pub(super) struct StatementSource<'a> {
    pub(super) comments: &'a Comments,
    pub(super) start: Location,
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
        Ok(Self { statements, comments: parser.into_comments() })
    }
}
