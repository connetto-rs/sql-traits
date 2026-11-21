//! Whether a primary key keeps `NULL` out of its columns, and whether the
//! catalog records the `NOT NULL` that follows.
//!
//! PostgreSQL and the SQL standard make every key column `NOT NULL`. SQLite
//! keeps an exception for compatibility with its early releases. A key
//! column of an ordinary rowid table accepts `NULL` unless it is the rowid
//! alias, and only a `STRICT` or `WITHOUT ROWID` table refuses it.
//! <https://www.sqlite.org/lang_createtable.html#the_primary_key>

use sqlparser::{
    ast::{ColumnDef, ColumnOption, CreateTable, DataType, Expr, ObjectNamePart, TableConstraint},
    keywords::Keyword,
    tokenizer::Token,
};

use super::case_idents_match;
use crate::{impls::SqlparserDialect, structs::IdentifierCase, traits::TableLike};

/// Whether the catalog records `NOT NULL` on `column`, a column of the primary
/// key of `table`, because of that key, with the key naming its columns under
/// `case`.
///
/// SQLite records it in a `WITHOUT ROWID` table, and in a `STRICT` table on
/// every key column but the rowid alias. Every other dialect records it on
/// every key column.
pub(super) fn key_states_not_null(
    table: &CreateTable,
    column: &ColumnDef,
    dialect: SqlparserDialect,
    case: IdentifierCase,
) -> bool {
    !dialect.is_sqlite()
        || table.is_without_rowid()
        || (table.is_strict() && !is_rowid_alias(table, column, case))
}

/// Whether `column`, a column of the primary key of `table`, can never hold
/// `NULL` because of that key, with the key naming its columns under `case`.
///
/// SQLite turns a `NULL` written to the rowid alias into a fresh rowid, so
/// the alias never holds one although the catalog records no `NOT NULL`.
pub(crate) fn key_excludes_null(
    table: &CreateTable,
    column: &ColumnDef,
    dialect: SqlparserDialect,
    case: IdentifierCase,
) -> bool {
    key_states_not_null(table, column, dialect, case) || is_rowid_alias(table, column, case)
}

/// Whether `column` is the alias SQLite gives the rowid of `table`, with the
/// key naming its columns under `case`.
///
/// The alias is the integer primary key of a rowid table.
/// <https://www.sqlite.org/lang_createtable.html#rowid>
fn is_rowid_alias(table: &CreateTable, column: &ColumnDef, case: IdentifierCase) -> bool {
    !table.is_without_rowid() && is_integer_primary_key(table, column, case)
}

/// Whether `column` is the sole column of the primary key of `table` with a
/// declared type of exactly `INTEGER` in any case, and keyed in an order that
/// would make it the rowid alias, with the key naming its columns under
/// `case`.
///
/// A key written on the column as `PRIMARY KEY DESC` makes no alias, while
/// the same order written in a table-level key does.
pub(super) fn is_integer_primary_key(
    table: &CreateTable,
    column: &ColumnDef,
    case: IdentifierCase,
) -> bool {
    if !declares_integer(&column.data_type) {
        return false;
    }
    let keyed_on_column =
        column.options.iter().any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)));
    if keyed_on_column {
        return !column.options.iter().any(|option| orders_descending(&option.option));
    }
    table.constraints.iter().any(|constraint| {
        let TableConstraint::PrimaryKey(key) = constraint else {
            return false;
        };
        let [only] = key.columns.as_slice() else {
            return false;
        };
        matches!(&only.column.expr, Expr::Identifier(name) if case_idents_match(name, &column.name, case))
    })
}

/// Whether the option is the `DESC` SQLite lets a column's `PRIMARY KEY`
/// carry.
fn orders_descending(option: &ColumnOption) -> bool {
    let ColumnOption::DialectSpecific(tokens) = option else {
        return false;
    };
    matches!(tokens.as_slice(), [Token::Word(word)] if word.keyword == Keyword::DESC)
}

/// Whether the declared type is spelled `INTEGER` with nothing after it,
/// quoted or not.
fn declares_integer(data_type: &DataType) -> bool {
    match data_type {
        DataType::Integer(None) => true,
        DataType::Custom(name, modifiers) => {
            modifiers.is_empty()
                && matches!(
                    name.0.as_slice(),
                    [ObjectNamePart::Identifier(part)] if part.value.eq_ignore_ascii_case("integer")
                )
        }
        _ => false,
    }
}
