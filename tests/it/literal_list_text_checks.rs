//! Text length bounds and empty-string rejections read from checks that admit
//! only listed string literals.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect};

/// The upper bound, lower bound and empty-string rejection a check answers.
type TextFacts = (Option<usize>, Option<usize>, bool);

/// Asserts the facts of each check of the first table `sql` declares, in
/// declaration order, parsing under PostgreSQL.
fn assert_facts(sql: &str, expected: &[TextFacts]) -> Result<(), Error> {
    assert_facts_in::<PostgreSqlDialect>(sql, expected)
}

/// Asserts the facts of each check of the first table `sql` declares, in
/// declaration order, parsing under `D`.
fn assert_facts_in<D: Dialect + Default + 'static>(
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

#[test]
fn listed_literals_bound_the_length_and_reject_the_empty_string_unless_listed() -> Result<(), Error>
{
    assert_facts(
        "CREATE TABLE labels (
            a TEXT CHECK (a IN ('read', 'write')),
            b TEXT CHECK (b = ANY (ARRAY['read', 'write'])),
            c TEXT CHECK (c = 'read' OR c = 'write'),
            d TEXT CHECK (d IN ('read', '')),
            e TEXT CHECK ('read' = e OR ((e = 'write'))),
            f TEXT CHECK (f = SOME (ARRAY['été'])),
            g VARCHAR(40) CHECK ((g IN ('a', 'bb', 'ccc'))),
            h TEXT CHECK (h = '')
        );",
        &[
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
            (Some(5), Some(0), false),
            (Some(6), Some(4), true),
            (Some(4), Some(3), true),
            (Some(4), Some(1), true),
            (Some(1), Some(0), false),
        ],
    )
}

#[test]
fn the_spellings_postgres_stores_are_read_through_their_text_casts() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE labels (
            a TEXT CHECK ((a = ANY (ARRAY['read'::text, 'write'::text]))),
            v VARCHAR(9) CHECK (((v)::text = ANY ((ARRAY['read'::character varying, 'write'::character varying])::text[]))),
            c VARCHAR CHECK ((((c)::text = 'read'::text) OR ((c)::text = 'write'::text))),
            d TEXT CHECK (d IN (CAST('read' AS TEXT), 'write'::varchar)),
            n VARCHAR CHECK ((((n)::national character varying = 'read'::text) OR ((n)::national char varying = 'write'::text)))
        );",
        &[
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
        ],
    )
}

#[test]
fn lists_admitting_other_values_keep_no_bound() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE labels (
            a TEXT CHECK (a NOT IN ('read', 'write')),
            b TEXT CHECK (b IN ('read', NULL)),
            c TEXT CHECK (c IN ('read', a)),
            f TEXT CHECK (f <> ANY (ARRAY['read'])),
            g TEXT CHECK (g = ALL (ARRAY['read'])),
            h TEXT CHECK (h = ANY (ARRAY['read', upper(h)])),
            i TEXT CHECK (lower(i) IN ('read')),
            j TEXT CHECK (j IN ('read'::varchar(2))),
            k TEXT CHECK ((k)::char(2) IN ('re')),
            l TEXT CHECK (l = ANY ((ARRAY['read', 'write'])::varchar(2)[])),
            n INT CHECK (n IN (1, 2)),
            m INT CHECK (CAST(m AS TEXT) IN ('1'))
        );",
        &[(None, None, false); 12],
    )
}

#[test]
fn comparison_rules_that_equate_longer_strings_keep_no_bound() -> Result<(), Error> {
    assert_facts(
        r#"CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
        CREATE COLLATION exact (provider = icu, locale = 'und');
        CREATE TABLE labels (
            a TEXT COLLATE ci CHECK (a IN ('read', 'write')),
            b TEXT CHECK (b COLLATE ci IN ('read', 'write')),
            c TEXT COLLATE exact CHECK (c IN ('read', 'write')),
            d TEXT COLLATE "C" CHECK (d IN ('read', 'write'))
        );"#,
        &[
            (None, None, false),
            (None, None, false),
            (Some(6), Some(4), true),
            (Some(6), Some(4), true),
        ],
    )
}

#[test]
fn a_quoted_column_name_is_matched_with_its_quoting() -> Result<(), Error> {
    assert_facts(
        r#"CREATE TABLE labels (
            "Kind" TEXT CHECK ("Kind" IN ('read', 'write')),
            "kind" TEXT CHECK (KIND = ANY (ARRAY['read']))
        );"#,
        &[(Some(6), Some(4), true), (Some(5), Some(4), true)],
    )
}

#[test]
fn a_character_set_that_changes_the_comparison_rule_keeps_no_bound() -> Result<(), Error> {
    assert_facts_in::<MySqlDialect>(
        "CREATE TABLE labels (
            a TEXT CHARACTER SET utf8mb4 CHECK (a IN ('read', 'write')),
            b TEXT CHECK (b IN ('read', 'write'))
        );",
        &[(None, None, false), (Some(6), Some(4), true)],
    )
}

#[test]
fn listed_literals_compose_with_the_other_text_checks() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE labels (
            a TEXT CHECK (a IN ('x') AND length(a) <= 3),
            b TEXT CHECK (length(b) <= 3 AND b IN ('write')),
            c TEXT CHECK (c IN ('', 'x') AND c <> ''),
            d TEXT CHECK (d = 'read' OR length(d) > 2),
            e TEXT CHECK (e = 'read' OR d = 'write'),
            s TEXT CHECK (length(s) < length(r)),
            r TEXT CHECK (r IN ('read', 'write'))
        );",
        &[
            (Some(2), Some(1), true),
            (Some(4), Some(5), true),
            (Some(2), Some(0), true),
            (None, Some(3), false),
            (None, Some(4), false),
            (Some(6), None, false),
            (Some(6), Some(4), true),
        ],
    )
}

#[test]
fn a_listed_literal_contradicting_a_length_check_is_a_negation() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE grants (
            role TEXT CHECK (role IN ('read', 'write') AND length(role) > 10),
            verb TEXT CHECK (verb IN ('read', 'write') AND length(verb) > 4)
        );",
    )?;
    let table = db.tables().next().ok_or_else(|| std::io::Error::other("no table"))?;
    let negations = table
        .check_constraints(&db)?
        .map(|check| check.is_negation(&db))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(negations, [true, false]);
    Ok(())
}
