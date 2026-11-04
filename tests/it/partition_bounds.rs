//! Tests that a partition's bound is read back as written and checked against
//! its partitioned table and its sibling partitions.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18. Values are compared only for keys the model can order without
//! the database's locale, and only for literal bounds.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, InvalidBound},
    prelude::*,
};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {name} is recorded"))
}

/// The reason `sql` is refused with, for a bound on `partition` of `root`.
fn invalid(sql: &str, root: &str, partition: &str) -> InvalidBound {
    match parse(sql) {
        Err(Error::PartitionBoundInvalid { table_name, partition_name, reason })
            if table_name == root && partition_name == partition =>
        {
            reason
        }
        other => panic!("{sql}: {other:?}"),
    }
}

/// Asserts that `sql` is refused because `partition` overlaps `other`.
fn overlaps(sql: &str, partition: &str, other: &str) {
    match parse(sql) {
        Err(Error::PartitionBoundOverlaps { partition_name, other_partition, .. })
            if partition_name == partition && other_partition == other => {}
        other => panic!("{sql}: {other:?}"),
    }
}

const RANGE: &str = "CREATE TABLE r (id INT, note TEXT) PARTITION BY RANGE (id);
     CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);";
const LIST: &str = "CREATE TABLE l (id INT) PARTITION BY LIST (id);
     CREATE TABLE l1 PARTITION OF l FOR VALUES IN (1, 2, NULL);";
const HASH: &str = "CREATE TABLE h (id INT) PARTITION BY HASH (id);
     CREATE TABLE h1 PARTITION OF h FOR VALUES WITH (MODULUS 4, REMAINDER 0);";

#[test]
fn a_partition_reads_back_its_bound() {
    let db = db(&format!(
        "{RANGE} {LIST} {HASH}
         CREATE TABLE r2 PARTITION OF r FOR VALUES FROM (MINVALUE) TO (-5);
         CREATE TABLE rd PARTITION OF r DEFAULT;
         CREATE TABLE attached (id INT, note TEXT);
         ALTER TABLE r ATTACH PARTITION attached FOR VALUES FROM (10) TO (MAXVALUE);"
    ));
    let value = |text: &str| RangeDatum::Value(text.to_owned());
    assert_eq!(
        table(&db, "r1").partition_bound(),
        Some(PartitionBound::Range { from: vec![value("0")], to: vec![value("10")] })
    );
    assert_eq!(
        table(&db, "r2").partition_bound(),
        Some(PartitionBound::Range { from: vec![RangeDatum::MinValue], to: vec![value("-5")] })
    );
    assert_eq!(
        table(&db, "attached").partition_bound(),
        Some(PartitionBound::Range { from: vec![value("10")], to: vec![RangeDatum::MaxValue] })
    );
    assert_eq!(table(&db, "rd").partition_bound(), Some(PartitionBound::Default));
    assert_eq!(
        table(&db, "l1").partition_bound(),
        Some(PartitionBound::List {
            values: vec![
                ListDatum::Value("1".to_owned()),
                ListDatum::Value("2".to_owned()),
                ListDatum::Null,
            ]
        })
    );
    assert_eq!(
        table(&db, "h1").partition_bound(),
        Some(PartitionBound::Hash { modulus: 4, remainder: 0 })
    );
    assert_eq!(table(&db, "r").partition_bound(), None);
}

#[test]
fn a_bound_takes_the_form_of_its_tables_strategy() {
    assert_eq!(
        invalid(&format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES IN (1);"), "r", "p"),
        InvalidBound::StrategyMismatch { strategy: PartitionStrategy::Range }
    );
    assert_eq!(
        invalid(
            &format!(
                "{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES WITH (MODULUS 2, REMAINDER 0);"
            ),
            "r",
            "p"
        ),
        InvalidBound::StrategyMismatch { strategy: PartitionStrategy::Range }
    );
    assert_eq!(
        invalid(
            &format!("{LIST} CREATE TABLE p PARTITION OF l FOR VALUES FROM (0) TO (1);"),
            "l",
            "p"
        ),
        InvalidBound::StrategyMismatch { strategy: PartitionStrategy::List }
    );
    assert_eq!(
        invalid(
            &format!(
                "{RANGE} CREATE TABLE p (id INT, note TEXT);
                 ALTER TABLE r ATTACH PARTITION p FOR VALUES IN (1);"
            ),
            "r",
            "p"
        ),
        InvalidBound::StrategyMismatch { strategy: PartitionStrategy::Range }
    );
}

#[test]
fn a_range_bound_is_well_formed() {
    assert_eq!(
        invalid(
            &format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM (0, 1) TO (10, 2);"),
            "r",
            "p"
        ),
        InvalidBound::ValueCount { expected: 1, found: 2 }
    );
    assert_eq!(
        invalid(
            &format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM (NULL) TO (1);"),
            "r",
            "p"
        ),
        InvalidBound::NullInRange
    );
    assert_eq!(
        invalid(
            "CREATE TABLE rm (a INT, b INT) PARTITION BY RANGE (a, b);
             CREATE TABLE p PARTITION OF rm FOR VALUES FROM (MINVALUE, 0) TO (0, 0);",
            "rm",
            "p"
        ),
        InvalidBound::ValueAfterInfinity
    );
    assert_eq!(
        invalid(
            &format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM (x) TO (1);"),
            "r",
            "p"
        ),
        InvalidBound::ColumnReference
    );
    for (bound, check) in [("(5) TO (5)", "equal bounds"), ("(2.5) TO (3)", "2.5 rounds up to 3")] {
        assert_eq!(
            invalid(
                &format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM {bound};"),
                "r",
                "p"
            ),
            InvalidBound::EmptyRange,
            "{check}"
        );
    }
    // -1.5 rounds away from zero to -2, so the range holds -2.
    db(&format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM (-1.5) TO (-1);"));
}

#[test]
fn only_one_partition_is_the_default_and_never_of_a_hash_table() {
    let second = format!(
        "{RANGE} CREATE TABLE d1 PARTITION OF r DEFAULT; CREATE TABLE d2 PARTITION OF r DEFAULT;"
    );
    assert_eq!(
        invalid(&second, "r", "d2"),
        InvalidBound::DefaultExists { default_partition: "d1".to_owned() }
    );
    let attached = format!(
        "{RANGE} CREATE TABLE d1 PARTITION OF r DEFAULT; CREATE TABLE d2 (id INT, note TEXT);
         ALTER TABLE r ATTACH PARTITION d2 DEFAULT;"
    );
    assert_eq!(
        invalid(&attached, "r", "d2"),
        InvalidBound::DefaultExists { default_partition: "d1".to_owned() }
    );
    assert_eq!(
        invalid(&format!("{HASH} CREATE TABLE p PARTITION OF h DEFAULT;"), "h", "p"),
        InvalidBound::DefaultOnHash
    );
    // Dropping the default frees its place.
    db(&second.replace(
        "CREATE TABLE d2 PARTITION OF r DEFAULT;",
        "DROP TABLE d1; CREATE TABLE d2 PARTITION OF r DEFAULT;",
    ));
}

#[test]
fn hash_bounds_divide_one_another_and_take_distinct_classes() {
    for (bound, reason) in [
        ("(MODULUS 0, REMAINDER 0)", InvalidBound::ModulusNotPositive),
        ("(MODULUS 2, REMAINDER 3)", InvalidBound::RemainderNotBelowModulus),
        (
            "(MODULUS 6, REMAINDER 1)",
            InvalidBound::ModulusNotFactor { other_partition: "h1".to_owned() },
        ),
    ] {
        assert_eq!(
            invalid(
                &format!("{HASH} CREATE TABLE p PARTITION OF h FOR VALUES WITH {bound};"),
                "h",
                "p"
            ),
            reason,
            "{bound}"
        );
    }
    for bound in
        ["(MODULUS 2, REMAINDER 0)", "(MODULUS 8, REMAINDER 4)", "(MODULUS 4, REMAINDER 0)"]
    {
        overlaps(
            &format!("{HASH} CREATE TABLE p PARTITION OF h FOR VALUES WITH {bound};"),
            "p",
            "h1",
        );
    }
    db(&format!(
        "{HASH}
         CREATE TABLE h2 PARTITION OF h FOR VALUES WITH (MODULUS 4, REMAINDER 1);
         CREATE TABLE h3 PARTITION OF h FOR VALUES WITH (MODULUS 8, REMAINDER 2);
         CREATE TABLE h4 PARTITION OF h FOR VALUES WITH (MODULUS 8, REMAINDER 6);"
    ));
}

#[test]
fn range_partitions_do_not_overlap() {
    for bound in ["(5) TO (15)", "(MINVALUE) TO (1)", "(9) TO (MAXVALUE)", "(-1.4) TO (0.5)"] {
        overlaps(
            &format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM {bound};"),
            "p",
            "r1",
        );
    }
    overlaps(
        &format!(
            "{RANGE} CREATE TABLE p (id INT, note TEXT);
             ALTER TABLE r ATTACH PARTITION p FOR VALUES FROM (5) TO (15);"
        ),
        "p",
        "r1",
    );
    db(&format!(
        "{RANGE}
         CREATE TABLE r2 PARTITION OF r FOR VALUES FROM (10) TO (20);
         CREATE TABLE r3 PARTITION OF r FOR VALUES FROM (MINVALUE) TO (0);
         CREATE TABLE r4 PARTITION OF r FOR VALUES FROM (' 20 ') TO ('+30');"
    ));
    // A multi-column bound compares row-wise, with MINVALUE below and
    // MAXVALUE above every value.
    let multi = "CREATE TABLE rm (a INT, b INT) PARTITION BY RANGE (a, b);
         CREATE TABLE e1 PARTITION OF rm FOR VALUES FROM (0, MAXVALUE) TO (1, MINVALUE);";
    overlaps(
        &format!(
            "{multi} CREATE TABLE e2 PARTITION OF rm FOR VALUES FROM (0, MAXVALUE) TO (1, 0);"
        ),
        "e2",
        "e1",
    );
    db(&format!(
        "{multi} CREATE TABLE e3 PARTITION OF rm FOR VALUES FROM (1, MINVALUE) TO (1, 5);"
    ));
    // A dropped partition frees its range.
    db(&format!(
        "{RANGE} DROP TABLE r1; CREATE TABLE p PARTITION OF r FOR VALUES FROM (5) TO (15);"
    ));
}

#[test]
fn list_partitions_do_not_share_a_value() {
    for (bound, check) in [
        ("(2)", "a value"),
        ("(NULL)", "NULL"),
        ("(1.4)", "1.4 rounds to 1"),
        ("('02')", "'02' is 2"),
    ] {
        let sql = format!("{LIST} CREATE TABLE p PARTITION OF l FOR VALUES IN {bound};");
        match parse(&sql) {
            Err(Error::PartitionBoundOverlaps { partition_name, other_partition, .. })
                if partition_name == "p" && other_partition == "l1" => {}
            other => panic!("{check}: {other:?}"),
        }
    }
    db(&format!("{LIST} CREATE TABLE l2 PARTITION OF l FOR VALUES IN (3, 3);"));
    for (schema, partition) in [
        (
            "CREATE TABLE n (v NUMERIC) PARTITION BY LIST (v);
             CREATE TABLE n1 PARTITION OF n FOR VALUES IN (1.50);
             CREATE TABLE p PARTITION OF n FOR VALUES IN ('1.5');",
            "n1",
        ),
        (
            "CREATE TABLE t (s TEXT COLLATE \"C\") PARTITION BY LIST (s);
             CREATE TABLE t1 PARTITION OF t FOR VALUES IN (1);
             CREATE TABLE p PARTITION OF t FOR VALUES IN ('1');",
            "t1",
        ),
        (
            "CREATE TABLE d (x DATE) PARTITION BY LIST (x);
             CREATE TABLE d1 PARTITION OF d FOR VALUES IN ('2024-01-05');
             CREATE TABLE p PARTITION OF d FOR VALUES IN ('2024-1-5');",
            "d1",
        ),
    ] {
        overlaps(schema, "p", partition);
    }
}

#[test]
fn a_bound_value_has_the_keys_type() {
    for (sql, root, value) in [
        (
            format!("{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM ('1.5') TO (3);"),
            "r",
            "'1.5'",
        ),
        (
            format!(
                "{RANGE} CREATE TABLE p PARTITION OF r FOR VALUES FROM (3000000000) TO (3000000001);"
            ),
            "r",
            "3000000000",
        ),
        (
            "CREATE TABLE si (x SMALLINT) PARTITION BY LIST (x);
             CREATE TABLE p PARTITION OF si FOR VALUES IN (40000);"
                .to_owned(),
            "si",
            "40000",
        ),
        (
            "CREATE TABLE d (x DATE) PARTITION BY LIST (x);
             CREATE TABLE p PARTITION OF d FOR VALUES IN ('2024-02-30');"
                .to_owned(),
            "d",
            "'2024-02-30'",
        ),
    ] {
        assert!(
            matches!(
                invalid(&sql, root, "p"),
                InvalidBound::ValueNotOfKeyType { value: written, .. } if written == value
            ),
            "{sql}"
        );
    }
    db("CREATE TABLE bi (x BIGINT) PARTITION BY RANGE (x);
        CREATE TABLE p PARTITION OF bi FOR VALUES FROM (3000000000) TO (3000000001);");
}

#[test]
fn values_the_model_cannot_order_are_not_compared() {
    // Under the database's own collation `('a', 'm')` and `('B', 'C')`
    // overlap, and the model does not know that collation.
    db("CREATE TABLE rt (t TEXT) PARTITION BY RANGE (t);
        CREATE TABLE rt1 PARTITION OF rt FOR VALUES FROM ('a') TO ('m');
        CREATE TABLE rt2 PARTITION OF rt FOR VALUES FROM ('B') TO ('C');");
    // An expression bound is not evaluated.
    db("CREATE TABLE rd (d DATE) PARTITION BY RANGE (d);
        CREATE TABLE rd1 PARTITION OF rd FOR VALUES FROM (CURRENT_DATE) TO (CURRENT_DATE + 1);
        CREATE TABLE rd2 PARTITION OF rd FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');");
    // Under the "C" collation the same pair is ordered and kept apart.
    db("CREATE TABLE rc (t TEXT COLLATE \"C\") PARTITION BY RANGE (t);
        CREATE TABLE rc1 PARTITION OF rc FOR VALUES FROM ('a') TO ('m');
        CREATE TABLE rc2 PARTITION OF rc FOR VALUES FROM ('B') TO ('C');");
}

#[test]
fn numeric_and_timestamp_bounds_are_ordered_exactly() {
    let numeric = "CREATE TABLE n (v NUMERIC) PARTITION BY RANGE (v);
         CREATE TABLE n1 PARTITION OF n FOR VALUES FROM (-1.5) TO (0.25);";
    for bound in ["('-1') TO (1e-1)", "(0) TO (0.0001)"] {
        overlaps(
            &format!("{numeric} CREATE TABLE p PARTITION OF n FOR VALUES FROM {bound};"),
            "p",
            "n1",
        );
    }
    for bound in ["(10) TO (10.0)", "(-20) TO (-30)"] {
        assert_eq!(
            invalid(
                &format!("{numeric} CREATE TABLE p PARTITION OF n FOR VALUES FROM {bound};"),
                "n",
                "p"
            ),
            InvalidBound::EmptyRange,
            "{bound}"
        );
    }
    assert!(matches!(
        invalid(
            &format!("{numeric} CREATE TABLE p PARTITION OF n FOR VALUES FROM ('abc') TO (100);"),
            "n",
            "p"
        ),
        InvalidBound::ValueNotOfKeyType { .. }
    ));
    db(&format!(
        "{numeric}
         CREATE TABLE n3 PARTITION OF n FOR VALUES FROM (0.25) TO ('2.5e0');
         CREATE TABLE n4 PARTITION OF n FOR VALUES FROM (-3) TO (-2);
         CREATE TABLE n5 PARTITION OF n FOR VALUES FROM ('Infinity') TO ('NaN');"
    ));

    let stamps = "CREATE TABLE ts (t TIMESTAMP) PARTITION BY RANGE (t);
         CREATE TABLE t1 PARTITION OF ts
             FOR VALUES FROM ('2024-01-01 00:00') TO ('2024-01-01 12:00:00.5');";
    overlaps(
        &format!(
            "{stamps} CREATE TABLE p PARTITION OF ts
                 FOR VALUES FROM ('2024-01-01T06:30:00') TO ('2024-01-02');"
        ),
        "p",
        "t1",
    );
    for value in ["'2024-01-01 25:00'", "'2023-02-29'", "'2024-04-31'", "'2025-01-01 10:60'"] {
        assert!(
            matches!(
                invalid(
                    &format!(
                        "{stamps} CREATE TABLE p PARTITION OF ts
                             FOR VALUES FROM ({value}) TO ('2026-01-01');"
                    ),
                    "ts",
                    "p"
                ),
                InvalidBound::ValueNotOfKeyType { value: written, .. } if written == value
            ),
            "{value}"
        );
    }
    db(&format!(
        "{stamps}
         CREATE TABLE t3 PARTITION OF ts
             FOR VALUES FROM ('2024-01-01 12:00:00.5') TO ('2024-01-01 12:00:00.500001');
         CREATE TABLE t4 PARTITION OF ts FOR VALUES FROM ('2024-02-29') TO ('2024-03-01');
         CREATE TABLE t5 PARTITION OF ts FOR VALUES FROM ('2024-03-01 24:00') TO ('2024-03-03');"
    ));
}

#[test]
fn other_literal_spellings_read_or_stay_undecided() {
    overlaps(
        "CREATE TABLE vc (s VARCHAR COLLATE \"POSIX\") PARTITION BY RANGE (s);
         CREATE TABLE v1 PARTITION OF vc FOR VALUES FROM ('b') TO ('d');
         CREATE TABLE v2 PARTITION OF vc FOR VALUES FROM ('c') TO ('e');",
        "v2",
        "v1",
    );
    let integers = "CREATE TABLE i (x INT) PARTITION BY RANGE (x);
         CREATE TABLE i1 PARTITION OF i FOR VALUES FROM (+3) TO ((5));";
    overlaps(
        &format!("{integers} CREATE TABLE i2 PARTITION OF i FOR VALUES FROM ((4)) TO (6);"),
        "i2",
        "i1",
    );
    // PostgreSQL 16 and later read underscores in integer text.
    db(&format!(
        "{integers} CREATE TABLE i3 PARTITION OF i FOR VALUES FROM ('1_000') TO ('2_000');"
    ));
    assert_eq!(
        invalid(
            "CREATE TABLE rm (a INT, b INT) PARTITION BY RANGE (a, b);
             CREATE TABLE p PARTITION OF rm FOR VALUES FROM (MINVALUE, MAXVALUE) TO (0, 0);",
            "rm",
            "p"
        ),
        InvalidBound::ValueAfterInfinity
    );
    assert_eq!(
        invalid(&format!("{LIST} CREATE TABLE p PARTITION OF l FOR VALUES IN (x);"), "l", "p"),
        InvalidBound::ColumnReference
    );
    // An expression key is not compared, so these two are read and kept.
    db("CREATE TABLE e (a INT) PARTITION BY RANGE ((a + 1));
        CREATE TABLE e1 PARTITION OF e FOR VALUES FROM (0) TO (10);
        CREATE TABLE e2 PARTITION OF e FOR VALUES FROM (10) TO (20);");
    // A reference to a partition answers as the partition does.
    let db = db(RANGE);
    let r1 = table(&db, "r1");
    assert_eq!(<&CreateTable as TableLike>::partition_bound(&r1), r1.partition_bound());
}

#[test]
fn literal_edges_read_as_postgresql_reads_them() {
    let numeric = "CREATE TABLE n (v NUMERIC) PARTITION BY RANGE (v);";
    db(&format!("{numeric} CREATE TABLE a PARTITION OF n FOR VALUES FROM ('+1') TO (2);"));
    assert_eq!(
        invalid(
            &format!("{numeric} CREATE TABLE b PARTITION OF n FOR VALUES FROM (0) TO (0.0);"),
            "n",
            "b"
        ),
        InvalidBound::EmptyRange
    );
    let integers = "CREATE TABLE i (x INT) PARTITION BY RANGE (x);";
    // 0.04 rounds to 0, which leaves the range one value wide.
    db(&format!("{integers} CREATE TABLE d PARTITION OF i FOR VALUES FROM (0.04) TO (1);"));
    assert!(matches!(
        invalid(
            &format!(
                "{integers} CREATE TABLE e PARTITION OF i FOR VALUES FROM ('3000000000') TO ('3000000001');"
            ),
            "i",
            "e"
        ),
        InvalidBound::ValueNotOfKeyType { .. }
    ));
    // A date key reads a time of day and other spellings PostgreSQL accepts,
    // which the model leaves undecided.
    db("CREATE TABLE dt (x DATE) PARTITION BY RANGE (x);
        CREATE TABLE f PARTITION OF dt FOR VALUES FROM ('2024-01-01 10:00') TO ('2024-02-01');
        CREATE TABLE g PARTITION OF dt FOR VALUES FROM ('2024/03/01') TO ('2024-04-01');");
}
