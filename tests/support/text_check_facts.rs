//! The text length bounds and empty-string rejection each check of a table
//! answers.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{Dialect, PostgreSqlDialect};

/// The upper bound, lower bound and empty-string rejection a check answers.
pub(crate) type TextFacts = (Option<usize>, Option<usize>, bool);

/// Asserts the facts of each check of the first table `sql` declares, in
/// declaration order, parsing under PostgreSQL.
pub(crate) fn assert_facts(sql: &str, expected: &[TextFacts]) -> Result<(), Error> {
    assert_facts_in::<PostgreSqlDialect>(sql, expected)
}

/// Asserts the facts of each check of the first table `sql` declares, in
/// declaration order, parsing under `D`.
pub(crate) fn assert_facts_in<D: Dialect + Default + 'static>(
    sql: &str,
    expected: &[TextFacts],
) -> Result<(), Error> {
    let db = ParserDB::parse::<D>(sql)?;
    let table = db.tables().next().ok_or_else(|| std::io::Error::other("no table"))?;
    let checks: Vec<_> = table.check_constraints(&db)?.collect();
    assert_eq!(checks.len(), expected.len());
    for (check, expected) in checks.into_iter().zip(expected) {
        let facts = (
            check.is_upper_bounded_text_constraint(&db)?,
            check.is_lower_bounded_text_constraint(&db)?,
            check.is_not_empty_text_constraint(&db)?,
        );
        assert_eq!(facts, *expected, "{}", check.expression(&db));
    }
    Ok(())
}
