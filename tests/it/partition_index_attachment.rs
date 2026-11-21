//! `ALTER INDEX parent ATTACH PARTITION child` makes an index of a partition
//! a partition of an index of its partitioned table. A partitioned index is
//! valid once every direct partition, a foreign one included, holds a valid
//! index attached to it. Expectations were measured on PostgreSQL 18.7.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{AttachIndexRefusal, Error},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Every index the database holds, the ones backing a primary key or a
/// unique constraint included, with whether it is valid, sorted by name.
fn validity(database: &ParserDB) -> Vec<(String, bool)> {
    let mut indexes: Vec<(String, bool)> = database
        .indexes()
        .map(|index| {
            let name = index.name().expect("every index here is named").to_owned();
            (name, index.is_valid(database).expect("the database holds the index"))
        })
        .collect();
    for table in database.tables() {
        for key in table.unique_indices(database).expect("the database holds the table") {
            let name = key
                .backing_index_name(database)
                .expect("the database holds the key")
                .expect("PostgreSQL names every key index")
                .into_owned();
            indexes.push((name, key.is_valid(database).expect("the database holds the key")));
        }
    }
    indexes.sort();
    indexes
}

fn owned(expected: &[(&str, bool)]) -> Vec<(String, bool)> {
    expected.iter().map(|(name, valid)| ((*name).to_owned(), *valid)).collect()
}

/// What `pg_dump --schema-only` writes for a partitioned table with a
/// primary key, an expression index and a partitioned partition, referenced
/// by a foreign key added after the attachments.
const DUMP: &str = "CREATE SCHEMA d;
CREATE TABLE d.lines (
    order_id integer,
    region text
);
CREATE TABLE d.orders (
    id integer NOT NULL,
    region text NOT NULL,
    note text
)
PARTITION BY LIST (region);
CREATE TABLE d.p_eu (
    id integer CONSTRAINT orders_id_not_null NOT NULL,
    region text CONSTRAINT orders_region_not_null NOT NULL,
    note text
)
PARTITION BY RANGE (id);
CREATE TABLE d.p_eu_0 (
    id integer CONSTRAINT orders_id_not_null NOT NULL,
    region text CONSTRAINT orders_region_not_null NOT NULL,
    note text
);
CREATE TABLE d.p_us (
    id integer CONSTRAINT orders_id_not_null NOT NULL,
    region text CONSTRAINT orders_region_not_null NOT NULL,
    note text
);
ALTER TABLE ONLY d.orders ATTACH PARTITION d.p_eu FOR VALUES IN ('eu');
ALTER TABLE ONLY d.p_eu ATTACH PARTITION d.p_eu_0 FOR VALUES FROM (0) TO (100);
ALTER TABLE ONLY d.orders ATTACH PARTITION d.p_us FOR VALUES IN ('us');
ALTER TABLE ONLY d.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id, region);
ALTER TABLE ONLY d.p_eu
    ADD CONSTRAINT p_eu_pkey PRIMARY KEY (id, region);
ALTER TABLE ONLY d.p_eu_0
    ADD CONSTRAINT p_eu_0_pkey PRIMARY KEY (id, region);
ALTER TABLE ONLY d.p_us
    ADD CONSTRAINT p_us_pkey PRIMARY KEY (id, region);
CREATE INDEX orders_lower_idx ON ONLY d.orders USING btree (lower(note));
CREATE INDEX p_eu_lower_idx ON ONLY d.p_eu USING btree (lower(note));
CREATE INDEX p_eu_0_lower_idx ON d.p_eu_0 USING btree (lower(note));
CREATE INDEX p_us_lower_idx ON d.p_us USING btree (lower(note));
ALTER INDEX d.p_eu_lower_idx ATTACH PARTITION d.p_eu_0_lower_idx;
ALTER INDEX d.p_eu_pkey ATTACH PARTITION d.p_eu_0_pkey;
ALTER INDEX d.orders_lower_idx ATTACH PARTITION d.p_eu_lower_idx;
ALTER INDEX d.orders_pkey ATTACH PARTITION d.p_eu_pkey;
ALTER INDEX d.orders_lower_idx ATTACH PARTITION d.p_us_lower_idx;
ALTER INDEX d.orders_pkey ATTACH PARTITION d.p_us_pkey;
ALTER TABLE ONLY d.lines
    ADD CONSTRAINT lines_order_id_region_fkey FOREIGN KEY (order_id, region) REFERENCES d.orders(id, region);
";

#[test]
fn a_dumped_partition_tree_loads_with_every_index_valid() -> Result<(), Error> {
    let database = parse(DUMP)?;
    assert_eq!(
        validity(&database),
        owned(&[
            ("orders_lower_idx", true),
            ("orders_pkey", true),
            ("p_eu_0_lower_idx", true),
            ("p_eu_0_pkey", true),
            ("p_eu_lower_idx", true),
            ("p_eu_pkey", true),
            ("p_us_lower_idx", true),
            ("p_us_pkey", true),
        ])
    );
    Ok(())
}

#[test]
fn an_attached_index_cannot_be_dropped_alone() {
    for (index, parent) in
        [("d.p_us_lower_idx", "orders_lower_idx"), ("d.p_eu_0_lower_idx", "p_eu_lower_idx")]
    {
        let error = parse(&format!("{DUMP}DROP INDEX {index};"))
            .expect_err("PostgreSQL refuses dropping an attached index");
        assert!(
            matches!(&error, Error::IndexRequiredByParent { parent_index_name, .. } if parent_index_name == parent),
            "{index}: {error:?}"
        );
    }
}

#[test]
fn dropping_the_parent_index_drops_its_attached_tree() -> Result<(), Error> {
    let database = parse(&format!("{DUMP}DROP INDEX d.orders_lower_idx;"))?;
    assert!(validity(&database).iter().all(|(name, _)| !name.ends_with("lower_idx")));
    Ok(())
}

const TWO_PARTITIONS: &str = "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r0 PARTITION OF r FOR VALUES FROM (0) TO (10);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (10) TO (20);
CREATE INDEX r_idx ON ONLY r (id);
CREATE INDEX r0_idx ON r0 (id);
CREATE INDEX r1_idx ON r1 (id);
";

#[test]
fn the_parent_turns_valid_when_its_last_partition_attaches() -> Result<(), Error> {
    let first = format!("{TWO_PARTITIONS}ALTER INDEX r_idx ATTACH PARTITION r0_idx;\n");
    assert_eq!(
        validity(&parse(&first)?),
        owned(&[("r0_idx", true), ("r1_idx", true), ("r_idx", false)])
    );
    let both = format!("{first}ALTER INDEX r_idx ATTACH PARTITION r1_idx;");
    assert_eq!(
        validity(&parse(&both)?),
        owned(&[("r0_idx", true), ("r1_idx", true), ("r_idx", true)])
    );
    Ok(())
}

#[test]
fn attaching_the_same_index_again_changes_nothing() -> Result<(), Error> {
    let sql = format!(
        "{TWO_PARTITIONS}ALTER INDEX r_idx ATTACH PARTITION r0_idx;
ALTER INDEX r_idx ATTACH PARTITION r0_idx;"
    );
    assert_eq!(
        validity(&parse(&sql)?),
        owned(&[("r0_idx", true), ("r1_idx", true), ("r_idx", false)])
    );
    Ok(())
}

/// Attaching an attached index again validates its parent once the last
/// partition missing one has left.
#[test]
fn attaching_again_after_a_partition_left_validates_the_parent() -> Result<(), Error> {
    let sql = format!(
        "{TWO_PARTITIONS}ALTER INDEX r_idx ATTACH PARTITION r0_idx;
ALTER TABLE r DETACH PARTITION r1;
ALTER INDEX r_idx ATTACH PARTITION r0_idx;"
    );
    assert_eq!(
        validity(&parse(&sql)?),
        owned(&[("r0_idx", true), ("r1_idx", true), ("r_idx", true)])
    );
    Ok(())
}

/// An invalid index attaches but leaves its parent invalid, and validating it
/// later validates every index above it.
#[test]
fn validity_climbs_the_index_tree() -> Result<(), Error> {
    let tree = "CREATE TABLE b (id INT) PARTITION BY RANGE (id);
CREATE TABLE b1 PARTITION OF b FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
CREATE TABLE b11 PARTITION OF b1 FOR VALUES FROM (0) TO (10);
CREATE INDEX b_idx ON ONLY b (id);
CREATE INDEX b1_idx ON ONLY b1 (id);
CREATE INDEX b11_idx ON b11 (id);
ALTER INDEX b_idx ATTACH PARTITION b1_idx;
";
    assert_eq!(
        validity(&parse(tree)?),
        owned(&[("b11_idx", true), ("b1_idx", false), ("b_idx", false)])
    );
    let validated = format!("{tree}ALTER INDEX b1_idx ATTACH PARTITION b11_idx;");
    assert_eq!(
        validity(&parse(&validated)?),
        owned(&[("b11_idx", true), ("b1_idx", true), ("b_idx", true)])
    );
    Ok(())
}

#[test]
fn a_foreign_partition_keeps_the_parent_invalid() -> Result<(), Error> {
    let sql = "CREATE FOREIGN DATA WRAPPER w;
CREATE SERVER s FOREIGN DATA WRAPPER w;
CREATE TABLE q (id INT) PARTITION BY RANGE (id);
CREATE TABLE q0 PARTITION OF q FOR VALUES FROM (0) TO (10);
CREATE FOREIGN TABLE q1 PARTITION OF q FOR VALUES FROM (10) TO (20) SERVER s;
CREATE INDEX q_idx ON ONLY q (id);
CREATE INDEX q0_idx ON q0 (id);
ALTER INDEX q_idx ATTACH PARTITION q0_idx;";
    assert_eq!(validity(&parse(sql)?), owned(&[("q0_idx", true), ("q_idx", false)]));
    Ok(())
}

/// A partition created or attached while the parent is invalid receives an
/// attached copy, and a partition leaving does not validate the parent.
#[test]
fn partitions_arriving_or_leaving_keep_the_parent_invalid() -> Result<(), Error> {
    let sql = "CREATE TABLE c (id INT) PARTITION BY RANGE (id);
CREATE TABLE c0 PARTITION OF c FOR VALUES FROM (0) TO (10);
CREATE INDEX c_idx ON ONLY c (id);
CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (10) TO (20);
CREATE TABLE c2 (id INT);
ALTER TABLE c ATTACH PARTITION c2 FOR VALUES FROM (20) TO (30);
ALTER TABLE c DETACH PARTITION c0;";
    assert_eq!(
        validity(&parse(sql)?),
        owned(&[("c1_id_idx", true), ("c2_id_idx", true), ("c_idx", false)])
    );
    let dropped = "CREATE TABLE e (id INT) PARTITION BY RANGE (id);
CREATE TABLE e0 PARTITION OF e FOR VALUES FROM (0) TO (10);
CREATE INDEX e_idx ON ONLY e (id);
DROP TABLE e0;";
    assert_eq!(validity(&parse(dropped)?), owned(&[("e_idx", false)]));
    Ok(())
}

const KEYED: &str = "CREATE TABLE a (id INT NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE a0 PARTITION OF a FOR VALUES FROM (0) TO (10);
";

/// A key's index attaches under a plain unique index, and a primary key and a
/// unique constraint attach under each other.
#[test]
fn key_indexes_attach_across_kinds() -> Result<(), Error> {
    for (parent, child, names) in [
        (
            "CREATE UNIQUE INDEX k_parent ON ONLY k (id);",
            "ALTER TABLE ONLY k0 ADD CONSTRAINT k_child PRIMARY KEY (id);",
            ("k_child", "k_parent"),
        ),
        (
            "ALTER TABLE ONLY k ADD CONSTRAINT k_parent PRIMARY KEY (id);",
            "ALTER TABLE ONLY k0 ADD CONSTRAINT k_child UNIQUE (id);",
            ("k_child", "k_parent"),
        ),
        (
            "ALTER TABLE ONLY k ADD CONSTRAINT k_parent UNIQUE (id);",
            "ALTER TABLE ONLY k0 ADD CONSTRAINT k_child PRIMARY KEY (id);",
            ("k_child", "k_parent"),
        ),
    ] {
        let sql = format!(
            "CREATE TABLE k (id INT NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE k0 PARTITION OF k FOR VALUES FROM (0) TO (10);
{parent}
{child}
ALTER INDEX k_parent ATTACH PARTITION k_child;"
        );
        assert_eq!(
            validity(&parse(&sql)?),
            owned(&[(names.0, true), (names.1, true)]),
            "{parent} {child}"
        );
    }
    Ok(())
}

#[test]
fn qualified_names_resolve_and_unqualified_ones_follow_the_search_path() {
    let tree = "CREATE SCHEMA s;
CREATE TABLE s.\"Q\" (id INT) PARTITION BY RANGE (id);
CREATE TABLE s.\"Q0\" PARTITION OF s.\"Q\" FOR VALUES FROM (0) TO (10);
CREATE INDEX \"Q_idx\" ON ONLY s.\"Q\" (id);
CREATE INDEX \"Q0_idx\" ON s.\"Q0\" (id);
";
    assert!(matches!(
        parse(&format!("{tree}ALTER INDEX s.\"Q_idx\" ATTACH PARTITION \"Q0_idx\";")),
        Err(Error::AlterIndexNotFound { index_name }) if index_name == "Q0_idx"
    ));
    let database = parse(&format!("{tree}ALTER INDEX s.\"Q_idx\" ATTACH PARTITION s.\"Q0_idx\";"))
        .expect("both names are qualified");
    assert_eq!(validity(&database), owned(&[("Q0_idx", true), ("Q_idx", true)]));
}

/// A key a partition receives is attached to the key it came from.
#[test]
fn a_received_key_is_attached() -> Result<(), Error> {
    let sql = format!(
        "{KEYED}ALTER TABLE ONLY a ADD CONSTRAINT a_pkey PRIMARY KEY (id);
CREATE TABLE a1 PARTITION OF a FOR VALUES FROM (10) TO (20);
ALTER TABLE ONLY a0 ADD CONSTRAINT a0_pkey PRIMARY KEY (id);
ALTER INDEX a_pkey ATTACH PARTITION a0_pkey;
CREATE TABLE ar (id INT REFERENCES a);"
    );
    assert_eq!(
        validity(&parse(&sql)?),
        owned(&[("a0_pkey", true), ("a1_pkey", true), ("a_pkey", true)])
    );
    let elsewhere = "CREATE TABLE a (id INT NOT NULL) PARTITION BY RANGE (id);
ALTER TABLE a ADD CONSTRAINT a_pkey PRIMARY KEY (id);
CREATE TABLE a1 PARTITION OF a FOR VALUES FROM (10) TO (20);
CREATE UNIQUE INDEX o ON ONLY a (id);
ALTER INDEX o ATTACH PARTITION a1_pkey;";
    assert!(matches!(
        parse(elsewhere),
        Err(Error::AttachIndexRefused { reason: AttachIndexRefusal::AttachedElsewhere, .. })
    ));
    Ok(())
}

const UNATTACHED_KEYS: &str = "CREATE TABLE k (id INT NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE k0 PARTITION OF k FOR VALUES FROM (0) TO (10);
ALTER TABLE ONLY k ADD CONSTRAINT k_parent PRIMARY KEY (id);
";

/// A key attached to a key of the other kind is inherited, so its partition
/// cannot drop it and the parent's drop takes it along.
#[test]
fn an_attached_key_falls_with_its_parent() -> Result<(), Error> {
    let attached = format!(
        "{UNATTACHED_KEYS}ALTER TABLE ONLY k0 ADD CONSTRAINT k_child UNIQUE (id);
ALTER INDEX k_parent ATTACH PARTITION k_child;
"
    );
    assert!(matches!(
        parse(&format!("{attached}ALTER TABLE k0 DROP CONSTRAINT k_child;")),
        Err(Error::InheritedConstraintNotDroppable { constraint_name, .. }) if constraint_name == "k_child"
    ));
    assert_eq!(
        validity(&parse(&format!("{attached}ALTER TABLE k DROP CONSTRAINT k_parent;"))?),
        owned(&[])
    );
    Ok(())
}

/// The tables and indexes every refusal below is attempted against.
const REFUSALS: &str = "CREATE TABLE g (id INT NOT NULL, x INT) PARTITION BY RANGE (id);
CREATE TABLE g0 PARTITION OF g FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
CREATE TABLE g00 PARTITION OF g0 FOR VALUES FROM (0) TO (10);
CREATE TABLE g1 PARTITION OF g FOR VALUES FROM (10) TO (20);
CREATE INDEX g_idx ON ONLY g (id);
CREATE INDEX other_idx ON ONLY g (id);
CREATE INDEX g00_id ON g00 (id);
CREATE INDEX g0_x ON ONLY g0 (x);
CREATE UNIQUE INDEX g0_uid ON ONLY g0 (id);
CREATE INDEX g1_id ON g1 (id);
CREATE INDEX g1_extra ON g1 (id);
ALTER TABLE ONLY g ADD CONSTRAINT g_pkey PRIMARY KEY (id);
CREATE UNIQUE INDEX g1_uq ON g1 (id);
CREATE TABLE m (id INT);
CREATE INDEX m_idx ON m (id);
CREATE MATERIALIZED VIEW mv AS SELECT 1 AS id;
CREATE INDEX mv_idx ON mv (id);
ALTER INDEX g_idx ATTACH PARTITION g1_id;
";

fn refusal(statement: &str) -> Error {
    parse(&format!("{REFUSALS}{statement}")).expect_err(statement)
}

/// A name reaching no index, or no index of a partitioned table, is refused
/// before anything about the attachment is checked.
#[test]
fn names_reaching_no_partitioned_index_are_refused() {
    for (statement, expected) in [
        ("ALTER INDEX m_idx ATTACH PARTITION missing_idx;", "m_idx"),
        ("ALTER INDEX mv_idx ATTACH PARTITION g1_extra;", "mv_idx"),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(&error, Error::AttachIndexNotSupported { index_name } if index_name == expected),
            "{statement}: {error:?}"
        );
    }
    for statement in [
        "ALTER INDEX missing_idx ATTACH PARTITION g1_extra;",
        "ALTER INDEX g_idx ATTACH PARTITION missing_idx;",
    ] {
        let error = refusal(statement);
        assert!(
            matches!(&error, Error::AlterIndexNotFound { index_name } if index_name == "missing_idx"),
            "{statement}: {error:?}"
        );
    }
    for statement in
        ["ALTER INDEX m ATTACH PARTITION g1_extra;", "ALTER INDEX g_idx ATTACH PARTITION m;"]
    {
        let error = refusal(statement);
        assert!(
            matches!(&error, Error::RelationKindMismatch { object_name, .. } if object_name == "m"),
            "{statement}: {error:?}"
        );
    }
}

#[test]
fn the_refusals_answer_as_postgresql_does() {
    for (statement, index, parent, reason) in [
        (
            "ALTER INDEX g_idx ATTACH PARTITION g00_id;",
            "g00_id",
            "g_idx",
            AttachIndexRefusal::NotOnPartition { table_name: "g".to_owned() },
        ),
        (
            "ALTER INDEX g_idx ATTACH PARTITION mv_idx;",
            "mv_idx",
            "g_idx",
            AttachIndexRefusal::NotOnPartition { table_name: "g".to_owned() },
        ),
        (
            "ALTER INDEX g_idx ATTACH PARTITION g_idx;",
            "g_idx",
            "g_idx",
            AttachIndexRefusal::NotOnPartition { table_name: "g".to_owned() },
        ),
        (
            "ALTER INDEX other_idx ATTACH PARTITION g1_id;",
            "g1_id",
            "other_idx",
            AttachIndexRefusal::AttachedElsewhere,
        ),
        (
            "ALTER INDEX other_idx ATTACH PARTITION g1_extra;
ALTER INDEX g_idx ATTACH PARTITION g1_extra;",
            "g1_extra",
            "g_idx",
            AttachIndexRefusal::PartitionHasIndex { partition_name: "g1".to_owned() },
        ),
        (
            "CREATE INDEX g0_idx ON g0 (id);
ALTER INDEX g_idx ATTACH PARTITION g00_id;",
            "g00_id",
            "g_idx",
            AttachIndexRefusal::AttachedElsewhere,
        ),
        (
            "ALTER INDEX g_idx ATTACH PARTITION g1_extra;",
            "g1_extra",
            "g_idx",
            AttachIndexRefusal::PartitionHasIndex { partition_name: "g1".to_owned() },
        ),
        (
            "ALTER INDEX g_idx ATTACH PARTITION g0_x;",
            "g0_x",
            "g_idx",
            AttachIndexRefusal::DefinitionsDiffer,
        ),
        (
            "ALTER INDEX g_idx ATTACH PARTITION g0_uid;",
            "g0_uid",
            "g_idx",
            AttachIndexRefusal::DefinitionsDiffer,
        ),
        (
            "ALTER INDEX g_pkey ATTACH PARTITION g1_extra;",
            "g1_extra",
            "g_pkey",
            AttachIndexRefusal::DefinitionsDiffer,
        ),
        (
            "ALTER INDEX g_pkey ATTACH PARTITION g1_uq;",
            "g1_uq",
            "g_pkey",
            AttachIndexRefusal::ConstraintMissing { table_name: "g".to_owned() },
        ),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(
                &error,
                Error::AttachIndexRefused { index_name, parent_index_name, reason: held }
                    if index_name == index && parent_index_name == parent && *held == reason
            ),
            "{statement}: {error:?}"
        );
    }
}
