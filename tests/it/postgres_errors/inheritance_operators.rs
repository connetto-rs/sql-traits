use super::refuses;

#[test]
fn missing_explicit_schemas() {
    for sql in [
        "CREATE TABLE c(id int REFERENCES absent.t);",
        "CREATE TABLE c() INHERITS(absent.t);",
        "CREATE TABLE c(LIKE absent.t);",
        "CREATE COLLATION cc FROM absent.c;",
        "CREATE OPERATOR ### (FUNCTION=absent.f,LEFTARG=int,RIGHTARG=int);",
        "CREATE OPERATOR ### (FUNCTION=absent.f,RIGHTARG=int);",
    ] {
        refuses(sql, "3F000", "schema \"absent\" does not exist");
    }
}

#[test]
fn source_relation_kinds() {
    for (sql, message) in [
        (
            "CREATE SEQUENCE seq; CREATE TABLE copy (LIKE public.seq);",
            "relation \"seq\" is invalid in LIKE clause",
        ),
        (
            "CREATE TABLE t(id int); CREATE INDEX idx ON t(id); CREATE TABLE copy (LIKE public.idx);",
            "relation \"idx\" is invalid in LIKE clause",
        ),
        (
            "CREATE TYPE v AS (id int); CREATE TABLE copy () INHERITS(v);",
            "cannot open relation \"v\"",
        ),
    ] {
        refuses(sql, "42809", message);
    }
    for sql in [
        "CREATE VIEW v AS SELECT 1 AS id; CREATE TABLE copy (LIKE v);",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 AS id; CREATE TABLE copy (LIKE v);",
        "CREATE TYPE v AS (id int); CREATE TABLE copy (LIKE v);",
    ] {
        super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>(sql);
    }
}

#[test]
fn inherited_not_null_name_collisions() {
    refuses(
        "CREATE TABLE p (id int, CONSTRAINT nn CHECK(id>0)); CREATE TABLE c (CONSTRAINT nn NOT NULL id) INHERITS(p);",
        "23505",
        "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"",
    );
}

#[test]
fn inherited_not_null_name_conflicts_without_postgres_counterpart() {
    for sql in [
        "CREATE TABLE cpn (id int, CONSTRAINT nn NOT NULL id); CREATE TABLE ccx (id int, CONSTRAINT nn CHECK(id>0)) INHERITS(cpn);",
        "CREATE TABLE p (id int, CONSTRAINT nn NOT NULL id); CREATE TABLE c (val int, CONSTRAINT nn NOT NULL val) INHERITS(p);",
        "CREATE TABLE p (id int, CONSTRAINT nn NOT NULL id); CREATE TABLE c (val int, CONSTRAINT nn NOT NULL val NO INHERIT) INHERITS(p);",
    ] {
        super::refuses_without_counterpart::<sqlparser::dialect::PostgreSqlDialect>(sql);
    }
}

#[test]
fn qualified_operator_signatures() {
    for (sql, state, message) in [
        (
            "DROP OPERATOR -(NONE,int);",
            "2BP01",
            "cannot drop operator -(NONE,integer) because it is required by the database system",
        ),
        (
            "CREATE SCHEMA s2_ops; ALTER OPERATOR +(int,int) SET SCHEMA s2_ops; DROP OPERATOR s2_ops.+(int,int);",
            "2BP01",
            "cannot drop operator s2_ops.+(integer,integer) because it is required by the database system",
        ),
        (
            "CREATE SCHEMA s2_ops; ALTER OPERATOR +(int,int) SET SCHEMA s2_ops; DROP SCHEMA s2_ops CASCADE;",
            "2BP01",
            "cannot drop operator s2_ops.+(integer,integer) because it is required by the database system",
        ),
        (
            "CREATE TYPE \"Token\" AS ENUM ('x'); CREATE OPERATOR ### (FUNCTION=NoSuch,LEFTARG=\"Token\"[],RIGHTARG=\"Token\");",
            "42883",
            "function nosuch(\"Token\"[], \"Token\") does not exist",
        ),
        (
            "CREATE SCHEMA s2_types; CREATE TYPE s2_types.token AS ENUM ('x'); CREATE OPERATOR ### (FUNCTION=NoSuch,LEFTARG=s2_types.token,RIGHTARG=s2_types.token[]);",
            "42883",
            "function nosuch(s2_types.token, s2_types.token[]) does not exist",
        ),
        (
            "CREATE SCHEMA s2_types; CREATE TYPE public.token AS ENUM ('x'); CREATE TYPE s2_types.token AS ENUM ('x'); SET search_path=public,s2_types; CREATE OPERATOR ### (FUNCTION=NoSuch,LEFTARG=s2_types.token,RIGHTARG=public.token);",
            "42883",
            "function nosuch(s2_types.token, token) does not exist",
        ),
        (
            "CREATE FUNCTION eq(int,int) RETURNS bool LANGUAGE sql AS 'SELECT $1=$2'; CREATE OPERATOR <^ (FUNCTION=eq,LEFTARG=int,RIGHTARG=int); ALTER OPERATOR <^ (int,int) SET(NEGATOR = <^);",
            "42P13",
            "operator cannot be its own negator",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one row per measured statement")]
fn stored_error_names() {
    for (sql, state, message) in [
        (
            "CREATE TABLE host (id int REFERENCES MiXeD);",
            "42P01",
            "relation \"mixed\" does not exist",
        ),
        ("CREATE TABLE child () INHERITS(MiXeD);", "42P01", "relation \"mixed\" does not exist"),
        ("CREATE TABLE child (LIKE MiXeD);", "42P01", "relation \"mixed\" does not exist"),
        (
            "CREATE TABLE child (id int); ALTER TABLE child INHERIT MiXeD;",
            "42P01",
            "relation \"mixed\" does not exist",
        ),
        (
            "CREATE VIEW MiXeD AS SELECT 1 AS id; CREATE TABLE child () INHERITS(public.MiXeD);",
            "42809",
            "inherited relation \"mixed\" is not a table or foreign table",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child DROP COLUMN MiXeD;",
            "42P16",
            "cannot drop inherited column \"mixed\"",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child RENAME COLUMN MiXeD TO other;",
            "42P16",
            "cannot rename inherited column \"mixed\"",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child ALTER COLUMN MiXeD TYPE bigint;",
            "42P16",
            "cannot alter inherited column \"mixed\"",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent RENAME COLUMN MiXeD TO other;",
            "42P16",
            "inherited column \"mixed\" must be renamed in child tables too",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent ALTER COLUMN MiXeD TYPE bigint;",
            "42P16",
            "type of inherited column \"mixed\" must be changed in child tables too",
        ),
        (
            "CREATE TABLE parent (id int, CONSTRAINT MiXeD CHECK(id>0)); CREATE TABLE child () INHERITS(parent); ALTER TABLE child DROP CONSTRAINT MiXeD;",
            "42P16",
            "cannot drop inherited constraint \"mixed\" of relation \"child\"",
        ),
        (
            "CREATE TABLE parent (id int, CONSTRAINT MiXeD CHECK(id>0)); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent RENAME CONSTRAINT MiXeD TO other;",
            "42P16",
            "inherited constraint \"mixed\" must be renamed in child tables too",
        ),
        (
            "CREATE TABLE MiXeD (id int CHECK(id>0) NO INHERIT) PARTITION BY RANGE(id);",
            "42P16",
            "cannot add NO INHERIT constraint to partitioned table \"mixed\"",
        ),
        (
            "CREATE TABLE tab (MiXeD int PRIMARY KEY); ALTER TABLE tab ALTER COLUMN MiXeD DROP NOT NULL;",
            "42P16",
            "column \"mixed\" is in a primary key",
        ),
        (
            "CREATE TABLE tab (MiXeD int); ALTER TABLE tab ALTER COLUMN MiXeD ADD GENERATED ALWAYS AS IDENTITY;",
            "55000",
            "column \"mixed\" of relation \"tab\" must be declared NOT NULL before identity can be added",
        ),
        (
            "CREATE TABLE tab (MiXeD int NOT NULL, NOT NULL MiXeD NO INHERIT);",
            "42601",
            "conflicting NO INHERIT declaration for not-null constraint on column \"mixed\"",
        ),
        (
            "CREATE TABLE parent (MiXeD int); CREATE TABLE child (MiXeD text) INHERITS(parent);",
            "42804",
            "column \"mixed\" has a type conflict",
        ),
        (
            "CREATE COLLATION copy FROM MiXeD;",
            "42704",
            "collation \"mixed\" for encoding \"UTF8\" does not exist",
        ),
        (
            "CREATE COLLATION MiXeD FROM \"C\"; CREATE COLLATION public.MiXeD FROM \"C\";",
            "42710",
            "collation \"mixed\" already exists",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=public.MiXeD,LEFTARG=int,RIGHTARG=text);",
            "42883",
            "function public.mixed(integer, text) does not exist",
        ),
        (
            "CREATE TABLE host (id int REFERENCES \"MiXeD\");",
            "42P01",
            "relation \"MiXeD\" does not exist",
        ),
        (
            "CREATE TABLE child () INHERITS(\"MiXeD\");",
            "42P01",
            "relation \"MiXeD\" does not exist",
        ),
        ("CREATE TABLE child (LIKE \"MiXeD\");", "42P01", "relation \"MiXeD\" does not exist"),
        (
            "CREATE TABLE child (id int); ALTER TABLE child INHERIT \"MiXeD\";",
            "42P01",
            "relation \"MiXeD\" does not exist",
        ),
        (
            "CREATE VIEW \"MiXeD\" AS SELECT 1 AS id; CREATE TABLE child () INHERITS(public.\"MiXeD\");",
            "42809",
            "inherited relation \"MiXeD\" is not a table or foreign table",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child DROP COLUMN \"MiXeD\";",
            "42P16",
            "cannot drop inherited column \"MiXeD\"",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child RENAME COLUMN \"MiXeD\" TO other;",
            "42P16",
            "cannot rename inherited column \"MiXeD\"",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child () INHERITS(parent); ALTER TABLE child ALTER COLUMN \"MiXeD\" TYPE bigint;",
            "42P16",
            "cannot alter inherited column \"MiXeD\"",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent RENAME COLUMN \"MiXeD\" TO other;",
            "42P16",
            "inherited column \"MiXeD\" must be renamed in child tables too",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent ALTER COLUMN \"MiXeD\" TYPE bigint;",
            "42P16",
            "type of inherited column \"MiXeD\" must be changed in child tables too",
        ),
        (
            "CREATE TABLE parent (id int, CONSTRAINT \"MiXeD\" CHECK(id>0)); CREATE TABLE child () INHERITS(parent); ALTER TABLE child DROP CONSTRAINT \"MiXeD\";",
            "42P16",
            "cannot drop inherited constraint \"MiXeD\" of relation \"child\"",
        ),
        (
            "CREATE TABLE parent (id int, CONSTRAINT \"MiXeD\" CHECK(id>0)); CREATE TABLE child () INHERITS(parent); ALTER TABLE ONLY parent RENAME CONSTRAINT \"MiXeD\" TO other;",
            "42P16",
            "inherited constraint \"MiXeD\" must be renamed in child tables too",
        ),
        (
            "CREATE TABLE \"MiXeD\" (id int CHECK(id>0) NO INHERIT) PARTITION BY RANGE(id);",
            "42P16",
            "cannot add NO INHERIT constraint to partitioned table \"MiXeD\"",
        ),
        (
            "CREATE TABLE tab (\"MiXeD\" int PRIMARY KEY); ALTER TABLE tab ALTER COLUMN \"MiXeD\" DROP NOT NULL;",
            "42P16",
            "column \"MiXeD\" is in a primary key",
        ),
        (
            "CREATE TABLE tab (\"MiXeD\" int); ALTER TABLE tab ALTER COLUMN \"MiXeD\" ADD GENERATED ALWAYS AS IDENTITY;",
            "55000",
            "column \"MiXeD\" of relation \"tab\" must be declared NOT NULL before identity can be added",
        ),
        (
            "CREATE TABLE tab (\"MiXeD\" int NOT NULL, NOT NULL \"MiXeD\" NO INHERIT);",
            "42601",
            "conflicting NO INHERIT declaration for not-null constraint on column \"MiXeD\"",
        ),
        (
            "CREATE TABLE parent (\"MiXeD\" int); CREATE TABLE child (\"MiXeD\" text) INHERITS(parent);",
            "42804",
            "column \"MiXeD\" has a type conflict",
        ),
        (
            "CREATE COLLATION copy FROM \"MiXeD\";",
            "42704",
            "collation \"MiXeD\" for encoding \"UTF8\" does not exist",
        ),
        (
            "CREATE COLLATION \"MiXeD\" FROM \"C\"; CREATE COLLATION public.\"MiXeD\" FROM \"C\";",
            "42710",
            "collation \"MiXeD\" already exists",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=public.\"MiXeD\",LEFTARG=int,RIGHTARG=text);",
            "42883",
            "function public.MiXeD(integer, text) does not exist",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn every_operator_dependent_kind() {
    let setup = "SET search_path = public, app;\n     CREATE SCHEMA app;\n     CREATE FUNCTION app.lt(int4, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';\n     CREATE FUNCTION app.lt8(int8, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';\n     CREATE FUNCTION app.lt84(int8, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';\n     CREATE FUNCTION app.ltt(text, text) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';\n     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';\n     CREATE TABLE t (a INT);\nCREATE OPERATOR app.<<< (LEFTARG = int4, RIGHTARG = int4, FUNCTION = app.lt);";
    for dependent in [
        "CREATE TABLE d (a BOOL DEFAULT (1 <<< 2));",
        "CREATE TABLE d (x INT, a BOOL GENERATED ALWAYS AS (x <<< 1) STORED);",
        "CREATE TABLE d (a INT CONSTRAINT ck CHECK (a <<< 5));",
        "ALTER TABLE t ALTER COLUMN a SET DEFAULT CASE WHEN 1 <<< 2 THEN 1 ELSE 0 END;",
        "CREATE INDEX i ON t ((a <<< 1));",
        "CREATE INDEX i ON t (a) WHERE a <<< 1;",
        "CREATE TABLE x (a INT, CONSTRAINT xc EXCLUDE USING btree ((a <<< 1) WITH =));",
        "CREATE VIEW v AS SELECT 1 <<< 2 AS one;",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 <<< 2 AS one;",
        "CREATE POLICY pol ON t USING (a <<< 1);",
        "CREATE RULE r AS ON INSERT TO t WHERE new.a <<< 1 DO INSTEAD NOTHING;",
        "CREATE DOMAIN d AS INT CHECK (VALUE <<< 10);",
        "CREATE DOMAIN d AS BOOL DEFAULT (1 <<< 2);",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (new.a <<< 1)\n         EXECUTE FUNCTION trg();",
        "CREATE TABLE p (a INT) PARTITION BY RANGE ((a <<< 1));",
        "CREATE FUNCTION caller() RETURNS BOOL LANGUAGE sql RETURN 1 <<< 2;",
    ] {
        refuses(
            &format!("{setup} {dependent} DROP OPERATOR app.<<< (int4,int4);"),
            "2BP01",
            "cannot drop operator <<<(integer,integer) because other objects depend on it",
        );
    }
}

#[test]
fn inherited_constraint_merge_contexts() {
    for (sql, state, message) in [
        (
            "CREATE TABLE mp1 (id int, CONSTRAINT ck CHECK(id>0)); CREATE TABLE mp2 (id int, CONSTRAINT ck CHECK(id<0)); CREATE TABLE mc () INHERITS(mp1,mp2);",
            "42710",
            "check constraint name \"ck\" appears multiple times but with different expressions",
        ),
        (
            "CREATE TABLE cpn (id int, CONSTRAINT nn NOT NULL id); CREATE TABLE ccn (id int, CONSTRAINT nn NOT NULL id NO INHERIT) INHERITS(cpn);",
            "42804",
            "cannot define not-null constraint with NO INHERIT on column \"id\"",
        ),
        (
            "CREATE TABLE dap (id int); CREATE TABLE dac (CONSTRAINT ck CHECK(id>0) NO INHERIT) INHERITS(dap); ALTER TABLE dap ADD CONSTRAINT ck CHECK(id>0);",
            "42P17",
            "constraint \"ck\" conflicts with non-inherited constraint on relation \"dac\"",
        ),
        (
            "CREATE TABLE npar (id int NOT NULL); CREATE TABLE nchi (id int NOT NULL) INHERITS(npar); ALTER TABLE nchi ALTER COLUMN id DROP NOT NULL;",
            "42P16",
            "cannot drop inherited constraint \"nchi_id_not_null\" of relation \"nchi\"",
        ),
        (
            "CREATE TABLE npp (id int NOT NULL) PARTITION BY RANGE(id); CREATE TABLE npc PARTITION OF npp FOR VALUES FROM(0) TO(5); ALTER TABLE npc DROP CONSTRAINT npp_id_not_null;",
            "42P16",
            "cannot drop inherited constraint \"npp_id_not_null\" of relation \"npc\"",
        ),
        (
            "CREATE TABLE npp (id int NOT NULL) PARTITION BY RANGE(id); CREATE TABLE npc PARTITION OF npp FOR VALUES FROM(0) TO(5); ALTER TABLE npc ALTER COLUMN id DROP NOT NULL;",
            "42P16",
            "column \"id\" is marked NOT NULL in parent table",
        ),
        (
            "CREATE TABLE noinherit (id int, CONSTRAINT ck NOT NULL id NO INHERIT); ALTER TABLE noinherit ALTER COLUMN id SET NOT NULL;",
            "0A000",
            "cannot change NO INHERIT status of NOT NULL constraint \"ck\" on relation \"noinherit\"",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn operator_function_privileges() {
    refuses(
        &format!(
            "{FUNCTIONS} CREATE ROLE s2_creator; GRANT USAGE, CREATE ON SCHEMA public TO s2_creator; REVOKE EXECUTE ON FUNCTION eq(int,int) FROM PUBLIC; SET ROLE s2_creator; CREATE OPERATOR #!# (FUNCTION=eq,LEFTARG=int,RIGHTARG=int);"
        ),
        "42501",
        "permission denied for function eq",
    );
}

#[test]
fn absent_source_relations() {
    for (sql, name) in [
        ("CREATE TABLE c (id int REFERENCES NoSuch);", "nosuch"),
        ("CREATE TABLE c () INHERITS (NoSuch);", "nosuch"),
        ("CREATE TABLE c (LIKE NoSuch);", "nosuch"),
        ("CREATE TABLE c (id int REFERENCES public.\"NoSuch\");", "public.NoSuch"),
        ("CREATE TABLE c () INHERITS (public.\"NoSuch\");", "public.NoSuch"),
        ("CREATE TABLE c (LIKE public.\"NoSuch\");", "public.NoSuch"),
    ] {
        refuses(sql, "42P01", &format!("relation \"{name}\" does not exist"));
    }
}

#[test]
fn inherited_columns_and_constraints() {
    let setup =
        "CREATE TABLE p (id int, CONSTRAINT ck CHECK(id>0)); CREATE TABLE c () INHERITS(p);";
    for (statement, message) in [
        ("ALTER TABLE c DROP COLUMN id;", "cannot drop inherited column \"id\""),
        ("ALTER TABLE c RENAME COLUMN id TO val;", "cannot rename inherited column \"id\""),
        ("ALTER TABLE c ALTER COLUMN id TYPE bigint;", "cannot alter inherited column \"id\""),
        (
            "ALTER TABLE c DROP CONSTRAINT ck;",
            "cannot drop inherited constraint \"ck\" of relation \"c\"",
        ),
        ("ALTER TABLE ONLY p ADD COLUMN val int;", "column must be added to child tables too"),
        (
            "ALTER TABLE ONLY p RENAME COLUMN id TO val;",
            "inherited column \"id\" must be renamed in child tables too",
        ),
        (
            "ALTER TABLE ONLY p ALTER COLUMN id TYPE bigint;",
            "type of inherited column \"id\" must be changed in child tables too",
        ),
        (
            "ALTER TABLE ONLY p ADD CONSTRAINT ck2 CHECK(id<9);",
            "constraint must be added to child tables too",
        ),
        (
            "ALTER TABLE ONLY p RENAME CONSTRAINT ck TO cknew;",
            "inherited constraint \"ck\" must be renamed in child tables too",
        ),
        (
            "ALTER TABLE p ADD CONSTRAINT unval CHECK(id<10) NOT VALID; ALTER TABLE ONLY p VALIDATE CONSTRAINT unval;",
            "constraint must be validated on child tables too",
        ),
    ] {
        refuses(&format!("{setup} {statement}"), "42P16", message);
    }
    refuses(
        &format!("{setup} ALTER TABLE ONLY p ADD PRIMARY KEY(id);"),
        "XX000",
        "column \"id\" of table \"c\" is not marked NOT NULL",
    );
    refuses(
        "CREATE VIEW v AS SELECT 1 AS id; CREATE TABLE c () INHERITS (v);",
        "42809",
        "inherited relation \"v\" is not a table or foreign table",
    );
}

#[test]
fn partition_column_and_constraint_changes() {
    let setup = "CREATE TABLE r (id int, val int) PARTITION BY RANGE(id); CREATE TABLE rp PARTITION OF r FOR VALUES FROM(0) TO(10);";
    for (statement, state, message) in [
        ("ALTER TABLE rp ADD COLUMN added int;", "42809", "cannot add column to a partition"),
        (
            "ALTER TABLE rp ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
            "42P16",
            "cannot add identity to a column of a partition",
        ),
        (
            "ALTER TABLE ONLY r ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
            "42P16",
            "cannot add identity to a column of only the partitioned table",
        ),
        (
            "ALTER TABLE ONLY r DROP COLUMN val;",
            "42P16",
            "cannot drop column from only the partitioned table when partitions exist",
        ),
        (
            "ALTER TABLE ONLY r ALTER COLUMN id SET NOT NULL;",
            "42P16",
            "constraint must be added to child tables too",
        ),
        (
            "ALTER TABLE r ADD CHECK(id>0) NO INHERIT;",
            "42P16",
            "cannot add NO INHERIT constraint to partitioned table \"r\"",
        ),
        (
            "ALTER TABLE r ADD NOT NULL id NO INHERIT;",
            "0A000",
            "not-null constraints on partitioned tables cannot be NO INHERIT",
        ),
        (
            "CREATE TABLE pk (id int PRIMARY KEY); ALTER TABLE ONLY r ADD FOREIGN KEY(id) REFERENCES pk;",
            "42809",
            "cannot use ONLY for foreign key on partitioned table \"r\" referencing relation \"pk\"",
        ),
    ] {
        refuses(&format!("{setup} {statement}"), state, message);
    }
}

#[test]
fn not_null_requirements() {
    for (sql, state, message) in [
        (
            "CREATE TABLE pk (id int PRIMARY KEY); ALTER TABLE pk ALTER COLUMN id DROP NOT NULL;",
            "42P16",
            "column \"id\" is in a primary key",
        ),
        (
            "CREATE TABLE identity_t (id int); ALTER TABLE identity_t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
            "55000",
            "column \"id\" of relation \"identity_t\" must be declared NOT NULL before identity can be added",
        ),
        (
            "CREATE TABLE nn (id int, CONSTRAINT a NOT NULL id, CONSTRAINT b NOT NULL id);",
            "42601",
            "conflicting not-null constraint names \"a\" and \"b\"",
        ),
        (
            "CREATE TABLE nn (id int NOT NULL, NOT NULL id NO INHERIT);",
            "42601",
            "conflicting NO INHERIT declaration for not-null constraint on column \"id\"",
        ),
        (
            "CREATE TABLE nn (id int NOT NULL); ALTER TABLE nn ADD CONSTRAINT b NOT NULL id;",
            "55000",
            "cannot create not-null constraint \"b\" on column \"id\" of table \"nn\"",
        ),
        (
            "CREATE TABLE nn (id int NOT NULL); ALTER TABLE nn ADD NOT NULL id NO INHERIT;",
            "55000",
            "cannot change NO INHERIT status of NOT NULL constraint \"nn_id_not_null\" on relation \"nn\"",
        ),
        (
            "CREATE TABLE nnpar (id int NOT NULL); CREATE TABLE nnchi () INHERITS(nnpar); ALTER TABLE nnchi ALTER COLUMN id DROP NOT NULL;",
            "42P16",
            "cannot drop inherited constraint \"nnpar_id_not_null\" of relation \"nnchi\"",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn conflicting_inherited_definitions() {
    for (sql, state, message) in [
        (
            "CREATE TABLE p (id int); CREATE TABLE x (id text) INHERITS(p);",
            "42804",
            "column \"id\" has a type conflict",
        ),
        (
            "CREATE TABLE tp (id int); CREATE TABLE tc (val text) INHERITS(tp); ALTER TABLE tp ADD COLUMN val int;",
            "42804",
            "child table \"tc\" has different type for column \"val\"",
        ),
        (
            "CREATE TABLE cp (id text COLLATE \"C\"); CREATE TABLE cc2 (id text COLLATE \"POSIX\") INHERITS(cp);",
            "42P21",
            "column \"id\" has a collation conflict",
        ),
        (
            "CREATE TABLE cp (id text COLLATE \"C\"); CREATE TABLE cp2 (id text COLLATE \"POSIX\"); CREATE TABLE cc2 () INHERITS(cp,cp2);",
            "42P21",
            "inherited column \"id\" has a collation conflict",
        ),
        (
            "CREATE TABLE tpc (id int); CREATE TABLE tcc (val text COLLATE \"C\") INHERITS(tpc); ALTER TABLE tpc ADD COLUMN val text COLLATE \"POSIX\";",
            "42P21",
            "child table \"tcc\" has different collation for column \"val\"",
        ),
        (
            "CREATE TABLE p (id int, CONSTRAINT ck CHECK(id>0)); CREATE TABLE conflict (CONSTRAINT ck CHECK(id<0)) INHERITS(p);",
            "42710",
            "constraint \"ck\" for relation \"conflict\" already exists",
        ),
        (
            "CREATE TABLE p (id int, CONSTRAINT ck CHECK(id>0)); CREATE TABLE conflict (CONSTRAINT ck CHECK(id>0) NO INHERIT) INHERITS(p);",
            "42P17",
            "constraint \"ck\" conflicts with inherited constraint on relation \"conflict\"",
        ),
    ] {
        refuses(sql, state, message);
    }
}

#[test]
fn collation_sources_and_duplicates() {
    for (sql, state, message) in [
        (
            "CREATE COLLATION cc FROM \"default\";",
            "42P17",
            "collation \"default\" cannot be copied",
        ),
        (
            "CREATE COLLATION cc FROM pg_catalog.\"default\";",
            "42P17",
            "collation \"default\" cannot be copied",
        ),
        (
            "CREATE COLLATION cc FROM NoSuch;",
            "42704",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "CREATE COLLATION cc FROM public.\"NoSuch\";",
            "42704",
            "collation \"public.NoSuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "CREATE COLLATION cc FROM \"C\"; CREATE COLLATION public.cc FROM \"C\";",
            "42710",
            "collation \"cc\" already exists",
        ),
    ] {
        refuses(sql, state, message);
    }
}

const FUNCTIONS: &str = "CREATE FUNCTION eq(int,int) RETURNS bool LANGUAGE sql AS 'SELECT $1=$2'; CREATE FUNCTION add(int,int) RETURNS int LANGUAGE sql AS 'SELECT $1+$2'; CREATE FUNCTION yes(int) RETURNS bool LANGUAGE sql AS 'SELECT true';";

#[test]
fn operator_definitions() {
    for (sql, state, message) in [
        (
            "CREATE OPERATOR ### (FUNCTION=yes,LEFTARG=int);",
            "42P13",
            "operator right argument type must be specified",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=NoSuch,LEFTARG=int,RIGHTARG=text);",
            "42883",
            "function nosuch(integer, text) does not exist",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,RESTRICT=NoSuch);",
            "42883",
            "function nosuch(internal, oid, internal, integer) does not exist",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,JOIN=NoSuch);",
            "42883",
            "function nosuch(internal, oid, internal, smallint, internal) does not exist",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=eq,LEFTARG=int,RIGHTARG=int); CREATE OPERATOR ### (FUNCTION=eq,LEFTARG=int,RIGHTARG=int);",
            "42723",
            "operator ### already exists",
        ),
        (
            "CREATE OPERATOR <^ (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,NEGATOR = <^);",
            "42P13",
            "operator cannot be its own negator",
        ),
        ("DROP OPERATOR #?#(int,int);", "42883", "operator does not exist: integer #?# integer"),
        (
            "DROP OPERATOR public.#?#(NONE,int);",
            "42883",
            "operator does not exist: public.#?# integer",
        ),
        (
            "DROP OPERATOR +(int,int);",
            "2BP01",
            "cannot drop operator +(integer,integer) because it is required by the database system",
        ),
        (
            "CREATE OPERATOR ### (FUNCTION=eq,LEFTARG=int,RIGHTARG=int); CREATE TABLE opdep (id int CHECK(id OPERATOR(public.###) 1)); DROP OPERATOR ###(int,int);",
            "2BP01",
            "cannot drop operator ###(integer,integer) because other objects depend on it",
        ),
        (
            "CREATE OPERATOR <^ (FUNCTION=eq,LEFTARG=int,RIGHTARG=int); CREATE SCHEMA ops; CREATE OPERATOR ops.<^ (FUNCTION=eq,LEFTARG=int,RIGHTARG=int); ALTER OPERATOR <^ (int,int) SET SCHEMA ops;",
            "23505",
            "duplicate key value violates unique constraint \"pg_operator_oprname_l_r_n_index\"",
        ),
    ] {
        refuses(&format!("{FUNCTIONS} {sql}"), state, message);
    }
}

#[test]
fn operator_attributes() {
    for (option, message) in [
        ("NEGATOR = ~>", "only boolean operators can have negators"),
        ("HASHES", "only boolean operators can hash"),
        ("MERGES", "only boolean operators can merge join"),
        ("RESTRICT=eqsel", "only boolean operators can have restriction selectivity"),
        ("JOIN=eqjoinsel", "only boolean operators can have join selectivity"),
    ] {
        refuses(
            &format!(
                "{FUNCTIONS} CREATE OPERATOR ### (FUNCTION=add,LEFTARG=int,RIGHTARG=int,{option});"
            ),
            "42P13",
            message,
        );
    }
    for (option, message) in [
        ("COMMUTATOR = ~>", "only binary operators can have commutators"),
        ("HASHES", "only binary operators can hash"),
        ("MERGES", "only binary operators can merge join"),
        ("JOIN=eqjoinsel", "only binary operators can have join selectivity"),
    ] {
        refuses(
            &format!("{FUNCTIONS} CREATE OPERATOR ### (FUNCTION=yes,RIGHTARG=int,{option});"),
            "42P13",
            message,
        );
    }
    let setup = format!(
        "{FUNCTIONS} CREATE OPERATOR <^ (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,COMMUTATOR = ~>, NEGATOR = <<=);"
    );
    for (sql, message) in [
        (
            "CREATE OPERATOR &< (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,COMMUTATOR = ~>);",
            "commutator operator ~> is already the commutator of operator <^",
        ),
        (
            "CREATE OPERATOR &< (FUNCTION=eq,LEFTARG=int,RIGHTARG=int,NEGATOR = <<=);",
            "negator operator <<= is already the negator of operator <^",
        ),
        (
            "ALTER OPERATOR <^ (int,int) SET(COMMUTATOR = +);",
            "operator attribute \"commutator\" cannot be changed if it has already been set",
        ),
        (
            "ALTER OPERATOR <^ (int,int) SET(NEGATOR = <>);",
            "operator attribute \"negator\" cannot be changed if it has already been set",
        ),
    ] {
        refuses(&format!("{setup} {sql}"), "42P13", message);
    }
}
