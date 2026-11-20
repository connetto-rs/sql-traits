//! Text length bounds and empty-string rejections read from length
//! equalities, from `OR`s whose arms bound one column, and from `LIKE` and
//! `GLOB` patterns.

use sql_traits::errors::Error;
use sqlparser::dialect::{GenericDialect, SQLiteDialect};

use crate::support::text_check_facts::{assert_facts, assert_facts_in};

#[test]
fn a_length_equality_bounds_the_length_from_both_sides() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE notes (
            a TEXT CHECK (length(a) = 36),
            b TEXT CHECK (32 = char_length(b)),
            c TEXT CHECK (length(c) = 0),
            d TEXT CHECK (length(d) = 36 AND length(d) = 32)
        );",
        &[
            (Some(37), Some(36), true),
            (Some(33), Some(32), true),
            (Some(1), Some(0), false),
            (Some(33), Some(36), true),
        ],
    )
}

#[test]
fn an_or_bounds_a_column_every_arm_bounds() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE notes (
            a TEXT CHECK (length(a) = 32 OR a IN ('infinity', '-infinity')),
            b TEXT CHECK (length(b) <= 10 OR length(b) BETWEEN 20 AND 30),
            c TEXT CHECK (c <> '' OR c = 'default'),
            d TEXT CHECK (length(d) < 5 OR d LIKE '%'),
            e TEXT,
            f TEXT,
            CHECK (length(e) < 5 OR length(f) < 10),
            CHECK (length(e) > 3 OR length(f) > 5),
            CHECK (e <> '' OR f <> '')
        );",
        &[
            (Some(33), Some(8), true),
            (Some(31), None, false),
            (None, None, true),
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
        ],
    )
}

#[test]
fn a_like_pattern_bounds_the_length_by_its_width() -> Result<(), Error> {
    assert_facts(
        r"CREATE TABLE notes (
            a TEXT CHECK (a LIKE '____-__-__'),
            b TEXT CHECK (b LIKE 'ab%'),
            c TEXT CHECK (c LIKE 'a#%b' ESCAPE '#'),
            d TEXT CHECK ((d)::text LIKE 'été'::text),
            e TEXT CHECK (e LIKE '____' OR e = 'infinity'),
            f TEXT CHECK (f LIKE '')
        );",
        &[
            (Some(11), Some(10), true),
            (None, Some(2), true),
            (Some(4), Some(3), true),
            (Some(4), Some(3), true),
            (Some(9), Some(4), true),
            (Some(1), Some(0), false),
        ],
    )
}

#[test]
fn a_like_pattern_dialects_read_apart_keeps_no_bound() -> Result<(), Error> {
    assert_facts(
        r"CREATE TABLE notes (
            a TEXT CHECK (a LIKE 'a\_b'),
            b TEXT CHECK (b LIKE '[0-9]'),
            c TEXT CHECK (c NOT LIKE '__'),
            d TEXT CHECK (d ILIKE '__'),
            e TEXT CHECK (e LIKE '__' ESCAPE '##'),
            f INT CHECK (f::text LIKE '__')
        );",
        &[
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
        ],
    )
}

#[test]
fn a_glob_pattern_bounds_the_length_by_its_width() -> Result<(), Error> {
    assert_facts_in::<SQLiteDialect>(
        "CREATE TABLE notes (
            a TEXT CHECK (a GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
            b TEXT CHECK (b GLOB '[^]a]??'),
            c TEXT CHECK (c GLOB 'ab*'),
            d TEXT CHECK (d GLOB '[0-9'),
            e TEXT CHECK (e GLOB '*'),
            f TEXT CHECK (f GLOB '[]]' OR f IN ('infinity', '-infinity'))
        );",
        &[
            (Some(11), Some(10), true),
            (Some(4), Some(3), true),
            (None, Some(2), true),
            (None, None, false),
            (None, Some(0), false),
            (Some(10), Some(1), true),
        ],
    )
}

#[test]
fn sqlite_date_formatting_bounds_the_text_it_admits() -> Result<(), Error> {
    assert_facts_in::<SQLiteDialect>(
        "CREATE TABLE notes (
            a TEXT CHECK (date(a) IS a),
            b TEXT CHECK (b IS time(b)),
            c TEXT CHECK (DATETIME(c) IS NOT DISTINCT FROM c),
            d TEXT CHECK (date(d) = d),
            e TEXT CHECK (date(e, 'subsec') IS e),
            f TEXT CHECK (date(f) IS NOT f),
            g TEXT COLLATE RTRIM CHECK (date(g) IS g),
            h TEXT COLLATE RTRIM CHECK (h IN ('read', 'write'))
        );",
        &[
            (Some(12), Some(10), true),
            (Some(9), Some(8), true),
            (Some(21), Some(19), true),
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
            (None, None, false),
        ],
    )
}

#[test]
fn date_formatting_outside_sqlite_keeps_no_bound() -> Result<(), Error> {
    assert_facts_in::<GenericDialect>(
        "CREATE TABLE notes (a TEXT CHECK (date(a) IS NOT DISTINCT FROM a));",
        &[(None, None, false)],
    )
}

#[test]
fn checks_holding_text_to_a_uuid_or_temporal_format_are_bounded() -> Result<(), Error> {
    assert_facts_in::<SQLiteDialect>(
        "CREATE TABLE notes (
            id TEXT CHECK (id GLOB '[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-[0-9a-f][0-9a-f][0-9a-f][0-9a-f]-[0-9a-f][0-9a-f][0-9a-f][0-9a-f]-[0-9a-f][0-9a-f][0-9a-f][0-9a-f]-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'),
            at TEXT CHECK (at IN ('infinity', '-infinity') OR (at GLOB '[0-9][0-9][0-9][0-9]-[0-1][0-9]-[0-3][0-9] [0-2][0-9]:[0-5][0-9]:[0-5][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]+00:00' AND at >= '0001' AND substr(at, 12, 2) < '24' AND date(substr(at, 1, 10)) IS substr(at, 1, 10))),
            local TEXT CHECK (local IN ('infinity', '-infinity') OR (local GLOB '[0-9][0-9][0-9][0-9]-[0-1][0-9]-[0-3][0-9] [0-2][0-9]:[0-5][0-9]:[0-5][0-9]*' AND local >= '0001' AND substr(local, 12, 2) < '24' AND date(substr(local, 1, 10)) IS substr(local, 1, 10) AND (length(local) = 19 OR (length(local) <= 26 AND substr(local, 20) GLOB '.[0-9]*' AND NOT (substr(local, 21) GLOB '*[^0-9]*'))))),
            day TEXT CHECK (day IN ('infinity', '-infinity') OR (day >= '0001' AND date(day) IS day)),
            clock TEXT CHECK (clock = '24:00:00' OR (clock GLOB '[0-2][0-9]:[0-5][0-9]:[0-5][0-9]*' AND substr(clock, 1, 2) < '24' AND (length(clock) = 8 OR (length(clock) <= 15 AND substr(clock, 9) GLOB '.[0-9]*' AND NOT (substr(clock, 10) GLOB '*[^0-9]*')))))
        );",
        &[
            (Some(37), Some(36), true),
            (Some(33), Some(8), true),
            (Some(27), Some(8), true),
            (Some(12), Some(8), true),
            (Some(16), Some(8), true),
        ],
    )
}
