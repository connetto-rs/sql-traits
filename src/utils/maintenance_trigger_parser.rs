//! Utilities for parsing maintenance trigger bodies.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::iter::Filter;

use sqlparser::{
    ast::Expr,
    dialect::PostgreSqlDialect,
    keywords::Keyword,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{DatabaseLike, TableLike},
};

/// Result type for maintenance assignments.
pub type MaintenanceAssignments<'a, T> =
    Vec<(&'a <<T as TableLike>::DB as DatabaseLike>::Column, Expr)>;

/// Why a trigger body is not usable as a maintenance trigger body.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MaintenanceBodyError {
    /// The body is not a run of `NEW.column = expression;` assignments followed
    /// by `RETURN NEW;`, or it assigns to a column the host table does not
    /// declare.
    #[error("The trigger body is not a maintenance trigger body.")]
    NotMaintenanceBody,
    /// Resolving an assigned column against the database failed, so whether the
    /// body is a maintenance body cannot be decided.
    #[error(transparent)]
    Lookup(#[from] LookupError),
}

/// Parses the body of a trigger function.
///
/// # Arguments
///
/// * `body` - The SQL body of the trigger function.
/// * `table` - The table the trigger is defined on.
/// * `database` - The database context for looking up columns.
///
/// # Returns
///
/// * `Ok(Vec<(&Column, Expr)>)` - A vector of column-expression pairs
///   representing assignments.
///
/// # Errors
///
/// Returns [`MaintenanceBodyError::NotMaintenanceBody`] when the body does not
/// have the shape of a maintenance trigger body or assigns to a column `table`
/// does not declare, and [`MaintenanceBodyError::Lookup`] when `database` does
/// not hold `table`, in which case the question cannot be decided either way.
pub fn parse_maintenance_body<'a, T>(
    body: &str,
    table: &'a T,
    database: &'a T::DB,
) -> Result<MaintenanceAssignments<'a, T>, MaintenanceBodyError>
where
    T: TableLike,
{
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, body);
    let Ok(tokens) = tokenizer.tokenize() else {
        return Err(MaintenanceBodyError::NotMaintenanceBody);
    };

    let mut assignments = Vec::new();
    let mut iter = MaintenanceBodyIterator::new(tokens);

    loop {
        match iter.next() {
            Some(Ok(MaintenanceToken::Assignment(col_name, expr))) => {
                // Verify column exists
                let Some(column) = table.column(&col_name, database, IdentifierCase::AsWritten)?
                else {
                    return Err(MaintenanceBodyError::NotMaintenanceBody);
                };
                assignments.push((column, *expr));
            }
            Some(Ok(MaintenanceToken::End)) => {
                if iter.finalize().is_err() {
                    return Err(MaintenanceBodyError::NotMaintenanceBody);
                }
                break;
            }
            // Reached EOF without RETURN NEW
            Some(Err(())) | None => return Err(MaintenanceBodyError::NotMaintenanceBody),
        }
    }

    if assignments.is_empty() {
        return Err(MaintenanceBodyError::NotMaintenanceBody);
    }

    Ok(assignments)
}

/// A token representing a high-level construct in a maintenance trigger body.
pub enum MaintenanceToken {
    /// An assignment to a column (e.g., `NEW.col = expr;`).
    Assignment(String, Box<Expr>),
    /// The end of the maintenance statements (e.g., `RETURN NEW;`).
    End,
}

type TokenFilterIter = Filter<alloc::vec::IntoIter<Token>, fn(&Token) -> bool>;

/// An iterator over the high-level tokens of a maintenance trigger body.
struct MaintenanceBodyIterator {
    tokens: core::iter::Peekable<TokenFilterIter>,
}

/// Predicate to skip whitespace tokens.
fn skip_whitespace(t: &Token) -> bool {
    !matches!(t, Token::Whitespace(_))
}

impl MaintenanceBodyIterator {
    /// Creates a new iterator from a list of tokens.
    ///
    /// Filters out whitespace and handles the optional `BEGIN` block start.
    fn new(tokens: Vec<Token>) -> Self {
        let mut tokens =
            tokens.into_iter().filter(skip_whitespace as fn(&Token) -> bool).peekable();

        // Optional: Skip BEGIN [;]
        if matches!(tokens.peek(), Some(Token::Word(w)) if w.keyword == Keyword::BEGIN) {
            tokens.next();
            if let Some(Token::SemiColon) = tokens.peek() {
                tokens.next();
            }
        }

        Self { tokens }
    }

    /// Verifies that the remaining tokens are valid (i.e., optional `END`
    /// block).
    ///
    /// Returns `Err(())` if there are unexpected tokens remaining.
    fn finalize(&mut self) -> Result<(), ()> {
        // Optional: Skip END [;]
        if matches!(self.tokens.peek(), Some(Token::Word(w)) if w.keyword == Keyword::END) {
            self.tokens.next();
            if let Some(Token::SemiColon) = self.tokens.peek() {
                self.tokens.next();
            }
        }

        // Ensure we are at EOF
        match self.tokens.peek() {
            Some(Token::EOF) | None => Ok(()),
            _ => Err(()),
        }
    }

    /// Peeks at the current token.
    fn peek_token(&mut self) -> Option<&Token> {
        self.tokens.peek()
    }

    /// Checks if the current token matches the given keyword.
    fn matches_keyword(&mut self, k: Keyword) -> bool {
        matches!(self.peek_token(), Some(Token::Word(w)) if w.keyword == k)
    }

    /// Consumes the current token if it matches the keyword.
    fn consume_keyword(&mut self, k: Keyword) -> bool {
        if self.matches_keyword(k) {
            self.tokens.next();
            true
        } else {
            false
        }
    }

    /// Consumes the current token if it matches the given token.
    fn consume_token(&mut self, t: &Token) -> bool {
        if self.peek_token() == Some(t) {
            self.tokens.next();
            true
        } else {
            false
        }
    }
}

impl Iterator for MaintenanceBodyIterator {
    type Item = Result<MaintenanceToken, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        self.tokens.peek()?;

        // Check for RETURN NEW; (End condition for assignments)
        if self.matches_keyword(Keyword::RETURN) {
            self.tokens.next();
            if !self.consume_keyword(Keyword::NEW) {
                return Some(Err(()));
            }
            if !self.consume_token(&Token::SemiColon) {
                return Some(Err(()));
            }
            return Some(Ok(MaintenanceToken::End));
        }

        // Must be NEW.col = expr;
        if !self.consume_keyword(Keyword::NEW) {
            if let Some(Token::EOF) = self.tokens.peek() {
                return None;
            }
            return Some(Err(()));
        }
        if !self.consume_token(&Token::Period) {
            return Some(Err(()));
        }

        // Column Name
        let col_name = match self.tokens.peek() {
            Some(Token::Word(w)) => {
                if w.quote_style.is_some() {
                    format!("\"{}\"", w.value.replace('\"', "\"\""))
                } else {
                    w.value.clone()
                }
            }
            Some(Token::SingleQuotedString(s)) => s.clone(),
            _ => return Some(Err(())),
        };
        self.tokens.next();

        // Assignment Op (= or :=)
        match self.tokens.peek() {
            Some(Token::Eq | Token::Assignment) => {
                self.tokens.next();
            }
            _ => return Some(Err(())),
        }

        // Parse Expression
        let mut expr_tokens = Vec::new();
        let mut balance_parens = 0;
        let mut found_semi = false;

        loop {
            match self.tokens.peek() {
                Some(Token::LParen) => balance_parens += 1,
                Some(Token::RParen) if balance_parens > 0 => balance_parens -= 1,
                Some(Token::SemiColon) if balance_parens == 0 => {
                    found_semi = true;
                    break;
                }
                // Should not hit END inside expression unless missing
                // semicolon.
                Some(Token::Word(w)) if w.keyword == Keyword::END && balance_parens == 0 => break,
                Some(Token::EOF) | None => break,
                _ => {}
            }

            if let Some(t) = self.tokens.next() {
                expr_tokens.push(t);
            }
        }

        if !found_semi {
            return Some(Err(()));
        }

        self.tokens.next(); // Consume semicolon

        if expr_tokens.is_empty() {
            return Some(Err(()));
        }

        let mut parser = Parser::new(&PostgreSqlDialect {}).with_tokens(expr_tokens);
        let Ok(expr) = parser.parse_expr() else {
            return Some(Err(()));
        };

        Some(Ok(MaintenanceToken::Assignment(col_name, Box::new(expr))))
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::{
        structs::{IdentifierCase, ParserDB, TargetName},
        traits::DatabaseLike,
    };

    fn parse(schema_sql: &str, body: &str) -> Result<usize, MaintenanceBodyError> {
        let db =
            ParserDB::parse::<GenericDialect>(schema_sql).expect("Failed to create DB from schema");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
            .expect("Failed to find table 't'");
        parse_maintenance_body(body, &table, &db).map(|v| v.len())
    }

    #[test]
    fn test_valid_single_assignment() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = 1; RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(1));
    }

    #[test]
    fn test_valid_multiple_assignments() {
        let schema = "CREATE TABLE t (a INT, b TEXT)";
        let body = "BEGIN NEW.a = 10; NEW.b = 'foo'; RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(2));
    }

    #[test]
    fn test_optional_begin_end() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "NEW.a = 1; RETURN NEW;";
        assert_eq!(parse(schema, body), Ok(1));
    }

    #[test]
    fn test_assignment_operator_variants() {
        let schema = "CREATE TABLE t (a INT, b INT)";
        let body = "BEGIN NEW.a = 1; NEW.b := 2; RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(2));
    }

    #[test]
    fn test_incorrect_column_name() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.b = 1; RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_missing_return_new() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = 1; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_extra_statements_fail() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = 1; SELECT 1; RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_trailing_garbage_after_end() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = 1; RETURN NEW; END; SELECT 1;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_old_is_invalid() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN OLD.a = 1; RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_quoted_identifiers() {
        let schema = "CREATE TABLE t (\"ColA\" INT)";
        let body = "BEGIN NEW.\"ColA\" = 1; RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(1));
    }

    #[test]
    fn test_complex_expression() {
        let schema = "CREATE TABLE t (a INT, b INT)";
        let body = "BEGIN NEW.a = OLD.b + 5 * (2 - 1); RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(1));
    }

    #[test]
    fn test_malformed_expression_parens() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = (1 + 2; RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_empty_assignment_rhs() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN NEW.a = ; RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_function_call_expression() {
        let schema = "CREATE TABLE t (a INT)";
        // uuid_generate_v4() is a function call
        let body = "BEGIN NEW.a = uuid_generate_v4(); RETURN NEW; END;";
        assert_eq!(parse(schema, body), Ok(1));
    }

    #[test]
    fn test_return_new_intermediate_fails() {
        let schema = "CREATE TABLE t (a INT, b INT)";
        // RETURN NEW must be last
        let body = "BEGIN NEW.a = 1; RETURN NEW; NEW.b = 2; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_no_assignments_fails() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN RETURN NEW; END;";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_empty_body_fails() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "";
        assert!(parse(schema, body).is_err());
    }

    #[test]
    fn test_empty_begin_end_block_fails() {
        let schema = "CREATE TABLE t (a INT)";
        let body = "BEGIN END;";
        assert!(parse(schema, body).is_err());
    }
}
