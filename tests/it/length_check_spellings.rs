//! Text length bounds and empty-string rejections read from `BETWEEN` length
//! ranges and from lower length bounds.

use sql_traits::errors::Error;

use crate::support::text_check_facts::assert_facts;

#[test]
fn a_length_range_bounds_the_length_from_both_sides() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE notes (
            a TEXT CHECK (length(a) BETWEEN 1 AND 200),
            b TEXT CHECK (char_length(b) BETWEEN 0 AND 10),
            c TEXT CHECK (octet_length(c) BETWEEN 3 AND 3),
            d TEXT CHECK ((character_length(d) BETWEEN 2 AND 8)),
            e TEXT CHECK (length(e) BETWEEN 1 AND 200 AND length(e) <= 50)
        );",
        &[
            (Some(201), Some(1), true),
            (Some(11), Some(0), false),
            (Some(4), Some(3), true),
            (Some(9), Some(2), true),
            (Some(51), Some(1), true),
        ],
    )
}

#[test]
fn a_length_range_outside_the_read_forms_keeps_no_bound() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE notes (
            a TEXT CHECK (length(a) NOT BETWEEN 1 AND 200),
            b INT CHECK (b BETWEEN 1 AND 200),
            c TEXT CHECK (length(c) BETWEEN 1 AND b),
            d TEXT CHECK (length(d) BETWEEN b AND 9),
            e TEXT CHECK (length(e || 'x') BETWEEN 1 AND 9)
        );",
        &[
            (None, None, false),
            (None, None, false),
            (None, Some(1), true),
            (Some(10), None, false),
            (None, None, false),
        ],
    )
}

#[test]
fn a_lower_length_bound_of_at_least_one_rejects_the_empty_string() -> Result<(), Error> {
    assert_facts(
        "CREATE TABLE notes (
            a TEXT CHECK (length(a) >= 1),
            b TEXT CHECK (length(b) > 0),
            c TEXT CHECK (1 <= char_length(c)),
            d TEXT CHECK (0 < octet_length(d)),
            e TEXT CHECK (length(e) >= 1 AND length(e) <= 200),
            f TEXT CHECK (length(f) >= 0),
            h TEXT CHECK (length(h) < 1),
            i TEXT CHECK (length(i) > length(a)),
            j INT CHECK (j > 0)
        );",
        &[
            (None, Some(1), true),
            (None, Some(1), true),
            (None, Some(1), true),
            (None, Some(1), true),
            (Some(201), Some(1), true),
            (None, Some(0), false),
            (Some(1), None, false),
            (None, Some(2), false),
            (None, None, false),
        ],
    )
}
