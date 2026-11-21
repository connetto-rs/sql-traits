use super::refuses;

#[test]
fn temporary_schema() {
    refuses(
        "CREATE TEMP TABLE public.t (id int);",
        "42P16",
        "cannot create temporary relation in non-temporary schema",
    );
}

#[test]
fn temporary_materialized() {
    refuses(
        "CREATE TEMP MATERIALIZED VIEW v AS SELECT 1;",
        "42601",
        "syntax error at or near \"MATERIALIZED\"",
    );
}

#[test]
fn materialized_temporary() {
    refuses(
        "CREATE TEMP TABLE t (id int); CREATE MATERIALIZED VIEW v AS SELECT * FROM t;",
        "0A000",
        "materialized views must not use temporary tables or views",
    );
}

#[test]
fn foreign_temporary() {
    refuses(
        "CREATE TABLE p (id int PRIMARY KEY); CREATE TEMP TABLE c (id int REFERENCES p);",
        "42P16",
        "constraints on temporary tables may reference only temporary tables",
    );
}

#[test]
fn foreign_permanent() {
    refuses(
        "CREATE TEMP TABLE p (id int PRIMARY KEY); CREATE TABLE c (id int REFERENCES p);",
        "42P16",
        "constraints on permanent tables may reference only permanent tables",
    );
}

#[test]
fn foreign_unlogged() {
    refuses(
        "CREATE UNLOGGED TABLE p (id int PRIMARY KEY); CREATE TABLE c (id int REFERENCES p);",
        "42P16",
        "constraints on permanent tables may reference only permanent tables",
    );
}

#[test]
fn unlogged_temp() {
    refuses(
        "CREATE UNLOGGED TABLE pg_temp.t (id int);",
        "42P16",
        "only temporary relations may be created in temporary schemas",
    );
}

#[test]
fn unlogged_partitioned() {
    refuses(
        "CREATE UNLOGGED TABLE p (id int) PARTITION BY RANGE(id);",
        "0A000",
        "partitioned tables cannot be unlogged",
    );
}

#[test]
fn fixed_temp() {
    refuses(
        "CREATE TEMP TABLE t (id int); ALTER TABLE t SET LOGGED;",
        "42P16",
        "cannot change logged status of table \"t\" because it is temporary",
    );
}

#[test]
fn fixed_partitioned() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); ALTER TABLE p SET UNLOGGED;",
        "42809",
        "ALTER action SET UNLOGGED cannot be performed on relation \"p\"",
    );
}

#[test]
fn persistence_twice() {
    refuses(
        "CREATE TABLE t (id int); ALTER TABLE t SET UNLOGGED, SET LOGGED;",
        "0A000",
        "cannot change persistence setting twice",
    );
}

#[test]
fn partition_storage() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); ALTER TABLE p SET (fillfactor=50);",
        "42809",
        "cannot specify storage parameters for a partitioned table",
    );
}

#[test]
fn permanent_inherits() {
    refuses(
        "CREATE TEMP TABLE p (id int); CREATE TABLE c () INHERITS(p);",
        "42809",
        "cannot inherit from temporary relation \"p\"",
    );
}

#[test]
fn temporary_partition() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TEMP TABLE c PARTITION OF p DEFAULT;",
        "42809",
        "cannot create a temporary relation as partition of permanent relation \"p\"",
    );
}

#[test]
fn parent_not_partitioned() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c PARTITION OF p DEFAULT;",
        "42P17",
        "\"p\" is not partitioned",
    );
}

#[test]
fn partitioned_inherits() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c () INHERITS(p) PARTITION BY RANGE(id);",
        "42P17",
        "cannot create partitioned table as inheritance child",
    );
}

#[test]
fn parent_repeated() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c () INHERITS(p,p);",
        "42P07",
        "relation \"p\" would be inherited from more than once",
    );
}

#[test]
fn default_generated() {
    refuses(
        "CREATE TABLE p (id int, g int GENERATED ALWAYS AS (id+1) STORED); CREATE TABLE c (g int DEFAULT 1) INHERITS(p);",
        "42611",
        "column \"g\" inherits from generated column but specifies default",
    );
}

#[test]
fn inherits_partitioned() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c () INHERITS(p);",
        "42809",
        "cannot inherit from partitioned table \"p\"",
    );
}

#[test]
fn change_partition() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; ALTER TABLE c NO INHERIT p;",
        "42809",
        "cannot change inheritance of a partition",
    );
}

#[test]
fn change_partitioned() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c (id int) PARTITION BY RANGE(id); ALTER TABLE c INHERIT p;",
        "42809",
        "cannot change inheritance of partitioned table",
    );
}

#[test]
fn circular() {
    refuses(
        "CREATE TABLE p (id int); ALTER TABLE p INHERIT p;",
        "42P07",
        "circular inheritance not allowed",
    );
}

#[test]
fn not_parent() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c (id int); ALTER TABLE c NO INHERIT p;",
        "42P01",
        "relation \"p\" is not a parent of relation \"c\"",
    );
}

#[test]
fn not_partition() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE p DETACH PARTITION c;",
        "42P01",
        "relation \"c\" is not a partition of relation \"p\"",
    );
}

#[test]
fn detach_transaction() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM (0) TO (10); BEGIN; ALTER TABLE p DETACH PARTITION c CONCURRENTLY;",
        "25001",
        "ALTER TABLE ... DETACH CONCURRENTLY cannot run inside a transaction block",
    );
}

#[test]
fn detach_default() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; ALTER TABLE p DETACH PARTITION c CONCURRENTLY;",
        "55000",
        "cannot detach partitions concurrently when a default partition exists",
    );
}

#[test]
fn detach_pending() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; ALTER TABLE p DETACH PARTITION c FINALIZE;",
        "55000",
        "cannot complete detaching partition \"c\"",
    );
}

#[test]
fn partition_not_alone() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; ALTER TABLE p DETACH PARTITION c, ADD COLUMN x int;",
        "42601",
        "syntax error at or near \",\"",
    );
}

#[test]
fn inherits_partition() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; CREATE TABLE t () INHERITS(c);",
        "42809",
        "cannot inherit from partition \"c\"",
    );
}

#[test]
fn missing_key() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(\"nosuch\");",
        "42703",
        "column \"nosuch\" named in partition key does not exist",
    );
}

#[test]
fn generated_key() {
    refuses(
        "CREATE TABLE p (id int, g int GENERATED ALWAYS AS (id+1) STORED) PARTITION BY RANGE(g);",
        "42P17",
        "cannot use generated column in partition key",
    );
}

#[test]
fn list_keys() {
    refuses(
        "CREATE TABLE p (id int, other int) PARTITION BY LIST(id,other);",
        "42P17",
        "cannot use \"list\" partition strategy with more than one column",
    );
}

#[test]
fn unique_missing() {
    refuses(
        "CREATE TABLE p (id int, other int UNIQUE) PARTITION BY RANGE(id);",
        "0A000",
        "unique constraint on partitioned table must include all partitioning columns",
    );
}

#[test]
fn unique_expression() {
    refuses(
        "CREATE TABLE p (id int PRIMARY KEY) PARTITION BY RANGE((id+1));",
        "0A000",
        "unsupported PRIMARY KEY constraint with partition key definition",
    );
}

#[test]
fn shape_missing() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c (x int); ALTER TABLE c INHERIT p;",
        "42804",
        "child table is missing column \"id\"",
    );
}

#[test]
fn shape_type() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c (id text); ALTER TABLE c INHERIT p;",
        "42804",
        "child table \"c\" has different type for column \"id\"",
    );
}

#[test]
fn shape_nullable() {
    refuses(
        "CREATE TABLE p (id int NOT NULL); CREATE TABLE c (id int); ALTER TABLE c INHERIT p;",
        "42804",
        "column \"id\" in child table \"c\" must be marked NOT NULL",
    );
}

#[test]
fn shape_generated() {
    refuses(
        "CREATE TABLE p (id int, g int GENERATED ALWAYS AS(id+1) STORED); CREATE TABLE c (id int,g int); ALTER TABLE c INHERIT p;",
        "42804",
        "column \"g\" in child table must be a generated column",
    );
}

#[test]
fn shape_not_generated() {
    refuses(
        "CREATE TABLE p (id int, g int); CREATE TABLE c (id int,g int GENERATED ALWAYS AS(id+1) STORED); ALTER TABLE c INHERIT p;",
        "42804",
        "column \"g\" in child table must not be a generated column",
    );
}

#[test]
fn shape_check_missing() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE c (id int); ALTER TABLE c INHERIT p;",
        "42804",
        "child table is missing constraint \"positive\"",
    );
}

#[test]
fn shape_check_differs() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE c (id int CONSTRAINT positive CHECK(id>1)); ALTER TABLE c INHERIT p;",
        "42804",
        "child table \"c\" has different definition for check constraint \"positive\"",
    );
}

#[test]
fn shape_check_noinherit() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE c (id int CONSTRAINT positive CHECK(id>0) NO INHERIT); ALTER TABLE c INHERIT p;",
        "42P17",
        "constraint \"positive\" conflicts with non-inherited constraint on child table \"c\"",
    );
}

#[test]
fn shape_check_invalid() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE c (id int); ALTER TABLE c ADD CONSTRAINT positive CHECK(id>0) NOT VALID; ALTER TABLE c INHERIT p;",
        "42P17",
        "constraint \"positive\" conflicts with NOT VALID constraint on child table \"c\"",
    );
}

#[test]
fn shape_collation() {
    refuses(
        "CREATE TABLE p (id text COLLATE \"C\"); CREATE TABLE c (id text COLLATE \"POSIX\"); ALTER TABLE c INHERIT p;",
        "42P21",
        "child table \"c\" has different collation for column \"id\"",
    );
}

#[test]
fn bound_range_strategy() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES IN (1);",
        "42P16",
        "invalid bound specification for a range partition",
    );
}

#[test]
fn bound_list_strategy() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0) TO(1);",
        "42P16",
        "invalid bound specification for a list partition",
    );
}

#[test]
fn bound_hash_strategy() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY HASH(id); CREATE TABLE c PARTITION OF p FOR VALUES IN (1);",
        "42P16",
        "invalid bound specification for a hash partition",
    );
}

#[test]
fn bound_count() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0,1) TO(2,3);",
        "42P16",
        "FROM must specify exactly one value per partitioning column",
    );
}

#[test]
fn bound_null() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(NULL) TO(1);",
        "42P17",
        "cannot specify NULL in range bound",
    );
}

#[test]
fn bound_reference() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(id) TO(1);",
        "0A000",
        "cannot use column reference in partition bound expression",
    );
}

#[test]
fn bound_empty() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(1) TO(1);",
        "42P17",
        "empty range bound specified for partition \"c\"",
    );
}

#[test]
fn bound_modulus() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY HASH(id); CREATE TABLE c PARTITION OF p FOR VALUES WITH(MODULUS 0, REMAINDER 0);",
        "42P16",
        "modulus for hash partition must be an integer value greater than zero",
    );
}

#[test]
fn bound_remainder() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY HASH(id); CREATE TABLE c PARTITION OF p FOR VALUES WITH(MODULUS 2, REMAINDER 3);",
        "42P16",
        "remainder for hash partition must be less than modulus",
    );
}

#[test]
fn bound_infinity() {
    refuses(
        "CREATE TABLE p (a int,b int) PARTITION BY RANGE(a,b); CREATE TABLE c PARTITION OF p FOR VALUES FROM(MINVALUE,0) TO(1,1);",
        "42804",
        "every bound following MINVALUE must also be MINVALUE",
    );
}

#[test]
fn bound_max_infinity() {
    refuses(
        "CREATE TABLE p (a int,b int) PARTITION BY RANGE(a,b); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0,0) TO(MAXVALUE,1);",
        "42804",
        "every bound following MAXVALUE must also be MAXVALUE",
    );
}

#[test]
fn bound_default_hash() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY HASH(id); CREATE TABLE c PARTITION OF p DEFAULT;",
        "42P16",
        "a hash-partitioned table may not have a default partition",
    );
}

#[test]
fn bound_default_exists() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; CREATE TABLE d PARTITION OF p DEFAULT;",
        "42P17",
        "partition \"d\" conflicts with existing default partition \"c\"",
    );
}

#[test]
fn bound_factor() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY HASH(id); CREATE TABLE c PARTITION OF p FOR VALUES WITH(MODULUS 4,REMAINDER 0); CREATE TABLE d PARTITION OF p FOR VALUES WITH(MODULUS 6,REMAINDER 1);",
        "42P17",
        "every hash partition modulus must be a factor of the next larger modulus",
    );
}

#[test]
fn bound_overlap() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0) TO(10); CREATE TABLE d PARTITION OF p FOR VALUES FROM(5) TO(15);",
        "42P17",
        "partition \"d\" would overlap partition \"c\"",
    );
}

#[test]
fn attach_already() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "\"c\" is already a partition",
    );
}

#[test]
fn attach_child() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE x (id int); CREATE TABLE c () INHERITS(x); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "cannot attach inheritance child as partition",
    );
}

#[test]
fn attach_parent() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int); CREATE TABLE x () INHERITS(c); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "cannot attach inheritance parent as partition",
    );
}

#[test]
fn attach_circular() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); ALTER TABLE p ATTACH PARTITION p DEFAULT;",
        "42P07",
        "circular inheritance not allowed",
    );
}

#[test]
fn attach_extra() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int,x int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "table \"c\" contains column \"x\" not found in parent \"p\"",
    );
}

#[test]
fn attach_identity() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int GENERATED ALWAYS AS IDENTITY); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "55000",
        "table \"c\" being attached contains an identity column \"id\"",
    );
}

#[test]
fn attach_primary() {
    refuses(
        "CREATE TABLE p (id int PRIMARY KEY,x int) PARTITION BY RANGE(id); CREATE TABLE c (id int,x int PRIMARY KEY); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "column \"id\" in child table \"c\" must be marked NOT NULL",
    );
}

#[test]
fn attach_shape_missing() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (x int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "table \"c\" contains column \"x\" not found in parent \"p\"",
    );
}

#[test]
fn attach_shape_type() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id text); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "child table \"c\" has different type for column \"id\"",
    );
}

#[test]
fn attach_shape_nullable() {
    refuses(
        "CREATE TABLE p (id int NOT NULL) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "column \"id\" in child table \"c\" must be marked NOT NULL",
    );
}

#[test]
fn attach_shape_check_missing() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "child table is missing constraint \"positive\"",
    );
}

#[test]
fn attach_shape_check_differs() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)) PARTITION BY RANGE(id); CREATE TABLE c (id int CONSTRAINT positive CHECK(id>1)); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "child table \"c\" has different definition for check constraint \"positive\"",
    );
}

#[test]
fn attach_shape_check_noinherit() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)) PARTITION BY RANGE(id); CREATE TABLE c (id int CONSTRAINT positive CHECK(id>0) NO INHERIT); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42P17",
        "constraint \"positive\" conflicts with non-inherited constraint on child table \"c\"",
    );
}

#[test]
fn attach_shape_check_invalid() {
    refuses(
        "CREATE TABLE p (id int CONSTRAINT positive CHECK(id>0)) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE c ADD CONSTRAINT positive CHECK(id>0) NOT VALID; ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42P17",
        "constraint \"positive\" conflicts with NOT VALID constraint on child table \"c\"",
    );
}

#[test]
fn attach_primary_valid() {
    refuses(
        "CREATE TABLE p (id int PRIMARY KEY,x int) PARTITION BY RANGE(id); CREATE TABLE c (id int NOT NULL,x int PRIMARY KEY); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42P16",
        "multiple primary keys for table \"c\" are not allowed",
    );
}

#[test]
fn attach_foreign() {
    refuses(
        "CREATE FOREIGN DATA WRAPPER s5_fdw; CREATE SERVER s5_server FOREIGN DATA WRAPPER s5_fdw; CREATE TABLE p (id int PRIMARY KEY) PARTITION BY RANGE(id); CREATE FOREIGN TABLE c (id int NOT NULL) SERVER s5_server; ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "cannot attach foreign table \"c\" as partition of partitioned table \"p\"",
    );
}

#[test]
fn fixed_partitioned_logged() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); ALTER TABLE p SET LOGGED;",
        "42809",
        "ALTER action SET LOGGED cannot be performed on relation \"p\"",
    );
}

#[test]
fn foreign_set_unlogged() {
    refuses(
        "CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE c (id int REFERENCES p); ALTER TABLE p SET UNLOGGED;",
        "42P16",
        "could not change table \"p\" to unlogged because it references logged table \"c\"",
    );
}

#[test]
fn foreign_set_logged() {
    refuses(
        "CREATE UNLOGGED TABLE p (id int PRIMARY KEY); CREATE UNLOGGED TABLE c (id int REFERENCES p); ALTER TABLE c SET LOGGED;",
        "42P16",
        "could not change table \"c\" to logged because it references unlogged table \"p\"",
    );
}

#[test]
fn unlogged_temp_keyword() {
    refuses(
        "CREATE TEMP UNLOGGED TABLE t (id int);",
        "42601",
        "syntax error at or near \"UNLOGGED\"",
    );
}

#[test]
fn partition_before_action() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE p ADD COLUMN x int, ATTACH PARTITION c DEFAULT;",
        "42601",
        "syntax error at or near \"ATTACH\"",
    );
}

#[test]
fn key_expression_missing() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE((NoSuch+1));",
        "42703",
        "column \"nosuch\" does not exist",
    );
}

#[test]
fn key_unique_expression() {
    refuses(
        "CREATE TABLE p (id int UNIQUE) PARTITION BY RANGE((id+1));",
        "0A000",
        "unsupported UNIQUE constraint with partition key definition",
    );
}

#[test]
fn key_index_expression() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE((id+1)); CREATE UNIQUE INDEX p_idx ON p(id);",
        "0A000",
        "unsupported UNIQUE constraint with partition key definition",
    );
}

#[test]
fn key_exclude_missing() {
    refuses(
        "CREATE TABLE p (id int, other int, EXCLUDE USING btree(other WITH =)) PARTITION BY RANGE(id);",
        "0A000",
        "unique constraint on partitioned table must include all partitioning columns",
    );
}

#[test]
fn value_0() {
    refuses(
        "CREATE TABLE p (id smallint) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(32768);",
        "22003",
        "smallint out of range",
    );
}

#[test]
fn value_1() {
    refuses(
        "CREATE TABLE p (id smallint) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('32768');",
        "22003",
        "value \"32768\" is out of range for type smallint",
    );
}

#[test]
fn value_2() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(2147483648);",
        "22003",
        "integer out of range",
    );
}

#[test]
fn value_3() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('2147483648');",
        "22003",
        "value \"2147483648\" is out of range for type integer",
    );
}

#[test]
fn value_4() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('1.5');",
        "22P02",
        "invalid input syntax for type integer: \"1.5\"",
    );
}

#[test]
fn value_5() {
    refuses(
        "CREATE TABLE p (id bigint) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(9223372036854775808);",
        "22003",
        "bigint out of range",
    );
}

#[test]
fn value_6() {
    refuses(
        "CREATE TABLE p (id bigint) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('9223372036854775808');",
        "22003",
        "value \"9223372036854775808\" is out of range for type bigint",
    );
}

#[test]
fn value_7() {
    refuses(
        "CREATE TABLE p (id numeric) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('no');",
        "22P02",
        "invalid input syntax for type numeric: \"no\"",
    );
}

#[test]
fn value_8() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(true);",
        "42804",
        "specified value cannot be cast to type integer for column \"id\"",
    );
}

#[test]
fn value_9() {
    refuses(
        "CREATE TABLE p (id numeric) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(true);",
        "42804",
        "specified value cannot be cast to type numeric for column \"id\"",
    );
}

#[test]
fn value_10() {
    refuses(
        "CREATE TABLE p (id date) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(1);",
        "42804",
        "specified value cannot be cast to type date for column \"id\"",
    );
}

#[test]
fn value_11() {
    refuses(
        "CREATE TABLE p (id date) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(true);",
        "42804",
        "specified value cannot be cast to type date for column \"id\"",
    );
}

#[test]
fn value_12() {
    refuses(
        "CREATE TABLE p (id date) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('2020-02-30');",
        "22008",
        "date/time field value out of range: \"2020-02-30\"",
    );
}

#[test]
fn value_13() {
    refuses(
        "CREATE TABLE p (id timestamp) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('2020-02-30 00:00:00');",
        "22008",
        "date/time field value out of range: \"2020-02-30 00:00:00\"",
    );
}

#[test]
fn value_14() {
    refuses(
        "CREATE TABLE p (id timestamp) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN(1);",
        "42804",
        "specified value cannot be cast to type timestamp without time zone for column \"id\"",
    );
}

#[test]
fn value_15() {
    refuses(
        "CREATE TABLE p (id timestamp) PARTITION BY LIST(id); CREATE TABLE c PARTITION OF p FOR VALUES IN('2020-01-01 25:00:00');",
        "22008",
        "date/time field value out of range: \"2020-01-01 25:00:00\"",
    );
}

#[test]
fn repeat_alter() {
    refuses(
        "CREATE TABLE p (id int); CREATE TABLE c () INHERITS(p); ALTER TABLE c INHERIT p;",
        "42P07",
        "relation \"p\" would be inherited from more than once",
    );
}

#[test]
fn permanent_partition() {
    refuses(
        "CREATE TEMP TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT;",
        "42809",
        "cannot create a permanent relation as partition of temporary relation \"p\"",
    );
}

#[test]
fn permanent_attach() {
    refuses(
        "CREATE TEMP TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "cannot attach a permanent relation as partition of temporary relation \"p\"",
    );
}

#[test]
fn temporary_attach() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TEMP TABLE c (id int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42809",
        "cannot attach a temporary relation as partition of permanent relation \"p\"",
    );
}

#[test]
fn fixed_temp_folded() {
    refuses(
        "CREATE TEMP TABLE t (ID int); ALTER TABLE t SET LOGGED;",
        "42P16",
        "cannot change logged status of table \"t\" because it is temporary",
    );
}

#[test]
fn fixed_temp_quoted() {
    refuses(
        "CREATE TEMP TABLE t (\"ID\" int); ALTER TABLE t SET LOGGED;",
        "42P16",
        "cannot change logged status of table \"t\" because it is temporary",
    );
}

#[test]
fn fixed_partitioned_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); ALTER TABLE P SET UNLOGGED;",
        "42809",
        "ALTER action SET UNLOGGED cannot be performed on relation \"p\"",
    );
}

#[test]
fn fixed_partitioned_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); ALTER TABLE \"P\" SET UNLOGGED;",
        "42809",
        "ALTER action SET UNLOGGED cannot be performed on relation \"P\"",
    );
}

#[test]
fn permanent_inherits_folded() {
    refuses(
        "CREATE TEMP TABLE P (ID int); CREATE TABLE C () INHERITS(P);",
        "42809",
        "cannot inherit from temporary relation \"p\"",
    );
}

#[test]
fn permanent_inherits_quoted() {
    refuses(
        "CREATE TEMP TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" () INHERITS(\"P\");",
        "42809",
        "cannot inherit from temporary relation \"P\"",
    );
}

#[test]
fn temporary_partition_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TEMP TABLE C PARTITION OF P DEFAULT;",
        "42809",
        "cannot create a temporary relation as partition of permanent relation \"p\"",
    );
}

#[test]
fn temporary_partition_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TEMP TABLE \"C\" PARTITION OF \"P\" DEFAULT;",
        "42809",
        "cannot create a temporary relation as partition of permanent relation \"P\"",
    );
}

#[test]
fn parent_not_partitioned_folded() {
    refuses(
        "CREATE TABLE P (ID int); CREATE TABLE C PARTITION OF P DEFAULT;",
        "42P17",
        "\"p\" is not partitioned",
    );
}

#[test]
fn parent_not_partitioned_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT;",
        "42P17",
        "\"P\" is not partitioned",
    );
}

#[test]
fn parent_repeated_folded() {
    refuses(
        "CREATE TABLE P (ID int); CREATE TABLE C () INHERITS(P,P);",
        "42P07",
        "relation \"p\" would be inherited from more than once",
    );
}

#[test]
fn parent_repeated_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" () INHERITS(\"P\",\"P\");",
        "42P07",
        "relation \"P\" would be inherited from more than once",
    );
}

#[test]
fn default_generated_folded() {
    refuses(
        "CREATE TABLE P (ID int, G int GENERATED ALWAYS AS (ID+1) STORED); CREATE TABLE C (G int DEFAULT 1) INHERITS(P);",
        "42611",
        "column \"g\" inherits from generated column but specifies default",
    );
}

#[test]
fn default_generated_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int, \"G\" int GENERATED ALWAYS AS (\"ID\"+1) STORED); CREATE TABLE \"C\" (\"G\" int DEFAULT 1) INHERITS(\"P\");",
        "42611",
        "column \"G\" inherits from generated column but specifies default",
    );
}

#[test]
fn inherits_partitioned_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C () INHERITS(P);",
        "42809",
        "cannot inherit from partitioned table \"p\"",
    );
}

#[test]
fn inherits_partitioned_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" () INHERITS(\"P\");",
        "42809",
        "cannot inherit from partitioned table \"P\"",
    );
}

#[test]
fn not_parent_folded() {
    refuses(
        "CREATE TABLE P (ID int); CREATE TABLE C (ID int); ALTER TABLE C NO INHERIT P;",
        "42P01",
        "relation \"p\" is not a parent of relation \"c\"",
    );
}

#[test]
fn not_parent_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"C\" NO INHERIT \"P\";",
        "42P01",
        "relation \"P\" is not a parent of relation \"C\"",
    );
}

#[test]
fn not_partition_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C (ID int); ALTER TABLE P DETACH PARTITION C;",
        "42P01",
        "relation \"c\" is not a partition of relation \"p\"",
    );
}

#[test]
fn not_partition_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"P\" DETACH PARTITION \"C\";",
        "42P01",
        "relation \"C\" is not a partition of relation \"P\"",
    );
}

#[test]
fn detach_pending_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P DEFAULT; ALTER TABLE P DETACH PARTITION C FINALIZE;",
        "55000",
        "cannot complete detaching partition \"c\"",
    );
}

#[test]
fn detach_pending_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT; ALTER TABLE \"P\" DETACH PARTITION \"C\" FINALIZE;",
        "55000",
        "cannot complete detaching partition \"C\"",
    );
}

#[test]
fn inherits_partition_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P DEFAULT; CREATE TABLE t () INHERITS(C);",
        "42809",
        "cannot inherit from partition \"c\"",
    );
}

#[test]
fn inherits_partition_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT; CREATE TABLE t () INHERITS(\"C\");",
        "42809",
        "cannot inherit from partition \"C\"",
    );
}

#[test]
fn missing_key_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(\"nosuch\");",
        "42703",
        "column \"nosuch\" named in partition key does not exist",
    );
}

#[test]
fn missing_key_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"nosuch\");",
        "42703",
        "column \"nosuch\" named in partition key does not exist",
    );
}

#[test]
fn shape_missing_folded() {
    refuses(
        "CREATE TABLE P (ID int); CREATE TABLE C (x int); ALTER TABLE C INHERIT P;",
        "42804",
        "child table is missing column \"id\"",
    );
}

#[test]
fn shape_missing_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" (x int); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "child table is missing column \"ID\"",
    );
}

#[test]
fn shape_type_folded() {
    refuses(
        "CREATE TABLE P (ID int); CREATE TABLE C (ID text); ALTER TABLE C INHERIT P;",
        "42804",
        "child table \"c\" has different type for column \"id\"",
    );
}

#[test]
fn shape_type_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int); CREATE TABLE \"C\" (\"ID\" text); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "child table \"C\" has different type for column \"ID\"",
    );
}

#[test]
fn shape_nullable_folded() {
    refuses(
        "CREATE TABLE P (ID int NOT NULL); CREATE TABLE C (ID int); ALTER TABLE C INHERIT P;",
        "42804",
        "column \"id\" in child table \"c\" must be marked NOT NULL",
    );
}

#[test]
fn shape_nullable_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int NOT NULL); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "column \"ID\" in child table \"C\" must be marked NOT NULL",
    );
}

#[test]
fn shape_generated_folded() {
    refuses(
        "CREATE TABLE P (ID int, G int GENERATED ALWAYS AS(ID+1) STORED); CREATE TABLE C (ID int,G int); ALTER TABLE C INHERIT P;",
        "42804",
        "column \"g\" in child table must be a generated column",
    );
}

#[test]
fn shape_generated_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int, \"G\" int GENERATED ALWAYS AS(\"ID\"+1) STORED); CREATE TABLE \"C\" (\"ID\" int,\"G\" int); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "column \"G\" in child table must be a generated column",
    );
}

#[test]
fn shape_not_generated_folded() {
    refuses(
        "CREATE TABLE P (ID int, G int); CREATE TABLE C (ID int,G int GENERATED ALWAYS AS(ID+1) STORED); ALTER TABLE C INHERIT P;",
        "42804",
        "column \"g\" in child table must not be a generated column",
    );
}

#[test]
fn shape_not_generated_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int, \"G\" int); CREATE TABLE \"C\" (\"ID\" int,\"G\" int GENERATED ALWAYS AS(\"ID\"+1) STORED); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "column \"G\" in child table must not be a generated column",
    );
}

#[test]
fn shape_check_missing_folded() {
    refuses(
        "CREATE TABLE P (ID int CONSTRAINT POSITIVE CHECK(ID>0)); CREATE TABLE C (ID int); ALTER TABLE C INHERIT P;",
        "42804",
        "child table is missing constraint \"positive\"",
    );
}

#[test]
fn shape_check_missing_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0)); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "child table is missing constraint \"POSITIVE\"",
    );
}

#[test]
fn shape_check_differs_folded() {
    refuses(
        "CREATE TABLE P (ID int CONSTRAINT POSITIVE CHECK(ID>0)); CREATE TABLE C (ID int CONSTRAINT POSITIVE CHECK(ID>1)); ALTER TABLE C INHERIT P;",
        "42804",
        "child table \"c\" has different definition for check constraint \"positive\"",
    );
}

#[test]
fn shape_check_differs_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0)); CREATE TABLE \"C\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">1)); ALTER TABLE \"C\" INHERIT \"P\";",
        "42804",
        "child table \"C\" has different definition for check constraint \"POSITIVE\"",
    );
}

#[test]
fn shape_check_noinherit_folded() {
    refuses(
        "CREATE TABLE P (ID int CONSTRAINT POSITIVE CHECK(ID>0)); CREATE TABLE C (ID int CONSTRAINT POSITIVE CHECK(ID>0) NO INHERIT); ALTER TABLE C INHERIT P;",
        "42P17",
        "constraint \"positive\" conflicts with non-inherited constraint on child table \"c\"",
    );
}

#[test]
fn shape_check_noinherit_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0)); CREATE TABLE \"C\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0) NO INHERIT); ALTER TABLE \"C\" INHERIT \"P\";",
        "42P17",
        "constraint \"POSITIVE\" conflicts with non-inherited constraint on child table \"C\"",
    );
}

#[test]
fn shape_check_invalid_folded() {
    refuses(
        "CREATE TABLE P (ID int CONSTRAINT POSITIVE CHECK(ID>0)); CREATE TABLE C (ID int); ALTER TABLE C ADD CONSTRAINT POSITIVE CHECK(ID>0) NOT VALID; ALTER TABLE C INHERIT P;",
        "42P17",
        "constraint \"positive\" conflicts with NOT VALID constraint on child table \"c\"",
    );
}

#[test]
fn shape_check_invalid_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0)); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"C\" ADD CONSTRAINT \"POSITIVE\" CHECK(\"ID\">0) NOT VALID; ALTER TABLE \"C\" INHERIT \"P\";",
        "42P17",
        "constraint \"POSITIVE\" conflicts with NOT VALID constraint on child table \"C\"",
    );
}

#[test]
fn shape_collation_folded() {
    refuses(
        "CREATE TABLE P (ID text COLLATE \"C\"); CREATE TABLE C (ID text COLLATE \"POSIX\"); ALTER TABLE C INHERIT P;",
        "42P21",
        "child table \"c\" has different collation for column \"id\"",
    );
}

#[test]
fn shape_collation_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" text COLLATE \"C\"); CREATE TABLE \"C\" (\"ID\" text COLLATE \"POSIX\"); ALTER TABLE \"C\" INHERIT \"P\";",
        "42P21",
        "child table \"C\" has different collation for column \"ID\"",
    );
}

#[test]
fn bound_empty_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P FOR VALUES FROM(1) TO(1);",
        "42P17",
        "empty range bound specified for partition \"c\"",
    );
}

#[test]
fn bound_empty_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" FOR VALUES FROM(1) TO(1);",
        "42P17",
        "empty range bound specified for partition \"C\"",
    );
}

#[test]
fn bound_default_exists_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P DEFAULT; CREATE TABLE D PARTITION OF P DEFAULT;",
        "42P17",
        "partition \"d\" conflicts with existing default partition \"c\"",
    );
}

#[test]
fn bound_default_exists_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT; CREATE TABLE \"D\" PARTITION OF \"P\" DEFAULT;",
        "42P17",
        "partition \"D\" conflicts with existing default partition \"C\"",
    );
}

#[test]
fn bound_overlap_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P FOR VALUES FROM(0) TO(10); CREATE TABLE D PARTITION OF P FOR VALUES FROM(5) TO(15);",
        "42P17",
        "partition \"d\" would overlap partition \"c\"",
    );
}

#[test]
fn bound_overlap_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" FOR VALUES FROM(0) TO(10); CREATE TABLE \"D\" PARTITION OF \"P\" FOR VALUES FROM(5) TO(15);",
        "42P17",
        "partition \"D\" would overlap partition \"C\"",
    );
}

#[test]
fn attach_already_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P DEFAULT; ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "42809",
        "\"c\" is already a partition",
    );
}

#[test]
fn attach_already_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT; ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "42809",
        "\"C\" is already a partition",
    );
}

#[test]
fn attach_extra_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C (ID int,x int); ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "42804",
        "table \"c\" contains column \"x\" not found in parent \"p\"",
    );
}

#[test]
fn attach_extra_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" (\"ID\" int,x int); ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "42804",
        "table \"C\" contains column \"x\" not found in parent \"P\"",
    );
}

#[test]
fn attach_identity_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C (ID int GENERATED ALWAYS AS IDENTITY); ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "55000",
        "table \"c\" being attached contains an identity column \"id\"",
    );
}

#[test]
fn attach_identity_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" (\"ID\" int GENERATED ALWAYS AS IDENTITY); ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "55000",
        "table \"C\" being attached contains an identity column \"ID\"",
    );
}

#[test]
fn attach_primary_valid_folded() {
    refuses(
        "CREATE TABLE P (ID int PRIMARY KEY,x int) PARTITION BY RANGE(ID); CREATE TABLE C (ID int NOT NULL,x int PRIMARY KEY); ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "42P16",
        "multiple primary keys for table \"c\" are not allowed",
    );
}

#[test]
fn attach_primary_valid_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int PRIMARY KEY,x int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" (\"ID\" int NOT NULL,x int PRIMARY KEY); ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "42P16",
        "multiple primary keys for table \"C\" are not allowed",
    );
}

#[test]
fn foreign_set_unlogged_folded() {
    refuses(
        "CREATE TABLE P (ID int PRIMARY KEY); CREATE TABLE C (ID int REFERENCES P); ALTER TABLE P SET UNLOGGED;",
        "42P16",
        "could not change table \"p\" to unlogged because it references logged table \"c\"",
    );
}

#[test]
fn foreign_set_unlogged_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int PRIMARY KEY); CREATE TABLE \"C\" (\"ID\" int REFERENCES \"P\"); ALTER TABLE \"P\" SET UNLOGGED;",
        "42P16",
        "could not change table \"P\" to unlogged because it references logged table \"C\"",
    );
}

#[test]
fn foreign_set_logged_folded() {
    refuses(
        "CREATE UNLOGGED TABLE P (ID int PRIMARY KEY); CREATE UNLOGGED TABLE C (ID int REFERENCES P); ALTER TABLE C SET LOGGED;",
        "42P16",
        "could not change table \"c\" to logged because it references unlogged table \"p\"",
    );
}

#[test]
fn foreign_set_logged_quoted() {
    refuses(
        "CREATE UNLOGGED TABLE \"P\" (\"ID\" int PRIMARY KEY); CREATE UNLOGGED TABLE \"C\" (\"ID\" int REFERENCES \"P\"); ALTER TABLE \"C\" SET LOGGED;",
        "42P16",
        "could not change table \"C\" to logged because it references unlogged table \"P\"",
    );
}

#[test]
fn permanent_partition_folded() {
    refuses(
        "CREATE TEMP TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C PARTITION OF P DEFAULT;",
        "42809",
        "cannot create a permanent relation as partition of temporary relation \"p\"",
    );
}

#[test]
fn permanent_partition_quoted() {
    refuses(
        "CREATE TEMP TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" PARTITION OF \"P\" DEFAULT;",
        "42809",
        "cannot create a permanent relation as partition of temporary relation \"P\"",
    );
}

#[test]
fn permanent_attach_folded() {
    refuses(
        "CREATE TEMP TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TABLE C (ID int); ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "42809",
        "cannot attach a permanent relation as partition of temporary relation \"p\"",
    );
}

#[test]
fn permanent_attach_quoted() {
    refuses(
        "CREATE TEMP TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TABLE \"C\" (\"ID\" int); ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "42809",
        "cannot attach a permanent relation as partition of temporary relation \"P\"",
    );
}

#[test]
fn temporary_attach_folded() {
    refuses(
        "CREATE TABLE P (ID int) PARTITION BY RANGE(ID); CREATE TEMP TABLE C (ID int); ALTER TABLE P ATTACH PARTITION C DEFAULT;",
        "42809",
        "cannot attach a temporary relation as partition of permanent relation \"p\"",
    );
}

#[test]
fn temporary_attach_quoted() {
    refuses(
        "CREATE TABLE \"P\" (\"ID\" int) PARTITION BY RANGE(\"ID\"); CREATE TEMP TABLE \"C\" (\"ID\" int); ALTER TABLE \"P\" ATTACH PARTITION \"C\" DEFAULT;",
        "42809",
        "cannot attach a temporary relation as partition of permanent relation \"P\"",
    );
}

#[test]
fn foreign_unlogged_temporary() {
    refuses(
        "CREATE TEMP TABLE p (id int PRIMARY KEY); CREATE UNLOGGED TABLE c (id int REFERENCES p);",
        "42P16",
        "constraints on unlogged tables may reference only permanent or unlogged tables",
    );
}

#[test]
fn foreign_temporary_unlogged() {
    refuses(
        "CREATE UNLOGGED TABLE p (id int PRIMARY KEY); CREATE TEMP TABLE c (id int REFERENCES p);",
        "42P16",
        "constraints on temporary tables may reference only temporary tables",
    );
}

#[test]
fn bound_upper_count() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0) TO(1,2);",
        "42P16",
        "TO must specify exactly one value per partitioning column",
    );
}

#[test]
fn exclude_expression() {
    refuses(
        "CREATE TABLE p (id int, EXCLUDE USING btree(id WITH =)) PARTITION BY RANGE((id+1));",
        "0A000",
        "unsupported EXCLUDE constraint with partition key definition",
    );
}

#[test]
fn temporary_view_schema() {
    refuses(
        "CREATE TEMP TABLE t (id int); CREATE VIEW public.v AS SELECT * FROM t;",
        "42P16",
        "cannot create temporary relation in non-temporary schema",
    );
}

#[test]
fn temporary_sequence_schema() {
    refuses(
        "CREATE TEMP SEQUENCE public.s;",
        "42P16",
        "cannot create temporary relation in non-temporary schema",
    );
}

#[test]
fn alter_permanent_inherits() {
    refuses(
        "CREATE TEMP TABLE p (id int); CREATE TABLE c (id int); ALTER TABLE c INHERIT p;",
        "42809",
        "cannot inherit from temporary relation \"p\"",
    );
}

#[test]
fn alter_partitioned_parent() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int); ALTER TABLE c INHERIT p;",
        "42809",
        "cannot inherit from partitioned table \"p\"",
    );
}

#[test]
fn alter_partition_parent() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; CREATE TABLE t (id int); ALTER TABLE t INHERIT c;",
        "42809",
        "cannot inherit from a partition",
    );
}

#[test]
fn alter_partition_child() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT; CREATE TABLE t (id int); ALTER TABLE c INHERIT t;",
        "42809",
        "cannot change inheritance of a partition",
    );
}

#[test]
fn sqlite_databases_have_no_postgres_counterpart() {
    super::refuses_without_counterpart::<sqlparser::dialect::SQLiteDialect>(
        "ATTACH DATABASE 'x' AS main;",
    );
    super::refuses_without_counterpart::<sqlparser::dialect::SQLiteDialect>(
        "ATTACH DATABASE 'x' AS other; CREATE TABLE other.t (id int); CREATE VIEW v AS SELECT * FROM other.t;",
    );
}
#[test]
fn fixed_partitioned_qualified() {
    refuses(
        "CREATE TABLE public.p (id int) PARTITION BY RANGE(id); ALTER TABLE public.p SET UNLOGGED;",
        "42809",
        "ALTER action SET UNLOGGED cannot be performed on relation \"p\"",
    );
}

#[test]
fn parent_not_partitioned_qualified() {
    refuses(
        "CREATE TABLE public.p (id int); CREATE TABLE public.c PARTITION OF public.p DEFAULT;",
        "42P17",
        "\"p\" is not partitioned",
    );
}

#[test]
fn parent_repeated_qualified() {
    refuses(
        "CREATE TABLE public.p (id int); CREATE TABLE public.c () INHERITS(public.p,public.p);",
        "42P07",
        "relation \"p\" would be inherited from more than once",
    );
}

#[test]
fn inherits_partitioned_qualified() {
    refuses(
        "CREATE TABLE public.p (id int) PARTITION BY RANGE(id); CREATE TABLE public.c () INHERITS(public.p);",
        "42809",
        "cannot inherit from partitioned table \"p\"",
    );
}

#[test]
fn not_parent_qualified() {
    refuses(
        "CREATE TABLE public.p (id int); CREATE TABLE public.c (id int); ALTER TABLE public.c NO INHERIT public.p;",
        "42P01",
        "relation \"p\" is not a parent of relation \"c\"",
    );
}

#[test]
fn not_partition_qualified() {
    refuses(
        "CREATE TABLE public.p (id int) PARTITION BY RANGE(id); CREATE TABLE public.c (id int); ALTER TABLE public.p DETACH PARTITION public.c;",
        "42P01",
        "relation \"c\" is not a partition of relation \"p\"",
    );
}

#[test]
fn detach_pending_qualified() {
    refuses(
        "CREATE TABLE public.p (id int) PARTITION BY RANGE(id); CREATE TABLE public.c PARTITION OF public.p DEFAULT; ALTER TABLE public.p DETACH PARTITION public.c FINALIZE;",
        "55000",
        "cannot complete detaching partition \"c\"",
    );
}

#[test]
fn inherits_partition_qualified() {
    refuses(
        "CREATE TABLE public.p (id int) PARTITION BY RANGE(id); CREATE TABLE public.c PARTITION OF public.p DEFAULT; CREATE TABLE t () INHERITS(public.c);",
        "42809",
        "cannot inherit from partition \"c\"",
    );
}

#[test]
fn shape_missing_qualified() {
    refuses(
        "CREATE TABLE public.p (id int); CREATE TABLE public.c (x int); ALTER TABLE public.c INHERIT public.p;",
        "42804",
        "child table is missing column \"id\"",
    );
}

#[test]
fn shape_type_qualified() {
    refuses(
        "CREATE TABLE public.p (id int); CREATE TABLE public.c (id text); ALTER TABLE public.c INHERIT public.p;",
        "42804",
        "child table \"c\" has different type for column \"id\"",
    );
}

#[test]
fn shape_nullable_qualified() {
    refuses(
        "CREATE TABLE public.p (id int NOT NULL); CREATE TABLE public.c (id int); ALTER TABLE public.c INHERIT public.p;",
        "42804",
        "column \"id\" in child table \"c\" must be marked NOT NULL",
    );
}

#[test]
fn shape_check_missing_qualified() {
    refuses(
        "CREATE TABLE public.p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE public.c (id int); ALTER TABLE public.c INHERIT public.p;",
        "42804",
        "child table is missing constraint \"positive\"",
    );
}

#[test]
fn shape_check_differs_qualified() {
    refuses(
        "CREATE TABLE public.p (id int CONSTRAINT positive CHECK(id>0)); CREATE TABLE public.c (id int CONSTRAINT positive CHECK(id>1)); ALTER TABLE public.c INHERIT public.p;",
        "42804",
        "child table \"c\" has different definition for check constraint \"positive\"",
    );
}

#[test]
fn missing_folded() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(NoSuch);",
        "42703",
        "column \"nosuch\" named in partition key does not exist",
    );
}

#[test]
fn missing_quoted() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(\"NoSuch\");",
        "42703",
        "column \"NoSuch\" named in partition key does not exist",
    );
}

#[test]
fn missing_expression_quoted() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE((\"NoSuch\"+1));",
        "42703",
        "column \"NoSuch\" does not exist",
    );
}

#[test]
fn attach_missing_column() {
    refuses(
        "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "child table is missing column \"id\"",
    );
}

#[test]
fn attach_generated() {
    refuses(
        "CREATE TABLE p (id int,g int GENERATED ALWAYS AS(id+1) STORED) PARTITION BY RANGE(id); CREATE TABLE c (id int,g int); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "column \"g\" in child table must be a generated column",
    );
}

#[test]
fn attach_not_generated() {
    refuses(
        "CREATE TABLE p (id int,g int) PARTITION BY RANGE(id); CREATE TABLE c (id int,g int GENERATED ALWAYS AS(id+1) STORED); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42804",
        "column \"g\" in child table must not be a generated column",
    );
}

#[test]
fn attach_collation() {
    refuses(
        "CREATE TABLE p (id text COLLATE \"C\") PARTITION BY RANGE(id); CREATE TABLE c (id text COLLATE \"POSIX\"); ALTER TABLE p ATTACH PARTITION c DEFAULT;",
        "42P21",
        "child table \"c\" has different collation for column \"id\"",
    );
}

#[test]
fn partition_index_copies_drop_with_their_parent() {
    const ROOT: &str = "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);";
    const ATTACH: &str = "ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);";
    for (sql, message) in [
        (
            format!(
                "{ROOT} CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10); CREATE INDEX pix ON r (id); DROP INDEX r1_id_idx;"
            ),
            "cannot drop index r1_id_idx because index pix requires it",
        ),
        (
            format!(
                "{ROOT} CREATE INDEX pix ON r (id); CREATE TABLE r1 (id INT, v INT); CREATE INDEX own ON r1 (id); {ATTACH} DROP INDEX own;"
            ),
            "cannot drop index own because index pix requires it",
        ),
        (
            format!(
                "{ROOT} CREATE UNIQUE INDEX pix ON r (id); CREATE TABLE r1 (id INT CONSTRAINT own PRIMARY KEY, v INT); {ATTACH} ALTER TABLE r1 DROP CONSTRAINT own;"
            ),
            "cannot drop index own because index pix requires it",
        ),
        (
            format!(
                "{ROOT} CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id); CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5); CREATE INDEX pix ON r (id); DROP INDEX r11_id_idx;"
            ),
            "cannot drop index r11_id_idx because index r1_id_idx requires it",
        ),
        (
            "CREATE TABLE \"R\" (id INT, v INT) PARTITION BY RANGE (id); CREATE TABLE \"R1\" PARTITION OF \"R\" FOR VALUES FROM (0) TO (10); CREATE INDEX \"Pix\" ON \"R\" (id); DROP INDEX \"R1_id_idx\";".to_string(),
            "cannot drop index \"R1_id_idx\" because index \"Pix\" requires it",
        ),
        (
            format!(
                "{ROOT} CREATE UNIQUE INDEX pix ON r (id); CREATE TABLE r1 (id INT CONSTRAINT own UNIQUE, v INT); {ATTACH} DROP INDEX pix;"
            ),
            "cannot drop index pix because other objects depend on it",
        ),
    ] {
        refuses(&sql, "2BP01", message);
    }
}

#[test]
fn index_attachment() {
    const TABLES: &str = "CREATE TABLE p (a INT, b INT) PARTITION BY RANGE (a); CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10); CREATE TABLE t (a INT);";
    for (statements, sqlstate, message) in [
        (
            "CREATE INDEX ti ON t (a); CREATE INDEX \"Ti2\" ON t (a); ALTER INDEX ti ATTACH PARTITION \"Ti2\";",
            "42809",
            "ALTER action ATTACH PARTITION cannot be performed on relation \"ti\"",
        ),
        (
            "CREATE INDEX \"Pi\" ON ONLY p (a); CREATE INDEX ti ON t (a); ALTER INDEX \"Pi\" ATTACH PARTITION ti;",
            "55000",
            "cannot attach index \"ti\" as a partition of index \"Pi\"",
        ),
        (
            "CREATE INDEX \"Pi\" ON ONLY p (a); CREATE INDEX i1 ON p1 (b); ALTER INDEX \"Pi\" ATTACH PARTITION i1;",
            "42P17",
            "cannot attach index \"i1\" as a partition of index \"Pi\"",
        ),
        (
            "CREATE INDEX \"Pi\" ON ONLY p (a); CREATE INDEX i1 ON p1 (a); CREATE INDEX i1b ON p1 (a); ALTER INDEX \"Pi\" ATTACH PARTITION i1; ALTER INDEX \"Pi\" ATTACH PARTITION i1b;",
            "55000",
            "cannot attach index \"i1b\" as a partition of index \"Pi\"",
        ),
        (
            "CREATE INDEX \"Pi\" ON ONLY p (a); CREATE INDEX \"Qi\" ON ONLY p (a); CREATE INDEX i1 ON p1 (a); ALTER INDEX \"Pi\" ATTACH PARTITION i1; ALTER INDEX \"Qi\" ATTACH PARTITION i1;",
            "55000",
            "cannot attach index \"i1\" as a partition of index \"Qi\"",
        ),
        (
            "ALTER TABLE ONLY p ADD CONSTRAINT k UNIQUE (a); CREATE UNIQUE INDEX i1 ON p1 (a); ALTER INDEX k ATTACH PARTITION i1;",
            "42P17",
            "cannot attach index \"i1\" as a partition of index \"k\"",
        ),
    ] {
        refuses(&format!("{TABLES} {statements}"), sqlstate, message);
    }
}
