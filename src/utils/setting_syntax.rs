//! PostgreSQL setting syntax tokens.

use alloc::string::{String, ToString};

use sqlparser::{
    ast::Expr,
    dialect::PostgreSqlDialect,
    keywords::Keyword,
    tokenizer::{Token, Tokenizer},
};

/// The first token that cannot continue a scalar setting value, or the whole
/// rendered value when the tokenizer refuses it.
pub(crate) fn setting_syntax_token(value: &Expr) -> String {
    let rendered = value.to_string();
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, &rendered).tokenize() else {
        return rendered;
    };
    let mut tokens = tokens.into_iter().filter(|token| !matches!(token, Token::Whitespace(_)));
    let first = tokens.next().unwrap_or(Token::EOF);
    if matches!(&first, Token::Word(word) if matches!(word.keyword,
        Keyword::NULL | Keyword::ARRAY | Keyword::CASE | Keyword::CAST))
        || !matches!(
            first,
            Token::Word(_)
                | Token::Number(..)
                | Token::SingleQuotedString(_)
                | Token::Plus
                | Token::Minus
        )
    {
        return first.to_string();
    }
    if matches!(first, Token::Plus | Token::Minus) {
        let signed = tokens.next().unwrap_or(Token::EOF);
        if !matches!(signed, Token::Number(..)) {
            return signed.to_string();
        }
    }
    tokens.next().unwrap_or(Token::EOF).to_string()
}
