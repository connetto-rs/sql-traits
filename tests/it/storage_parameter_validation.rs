//! PostgreSQL relation storage parameter validation.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

#[test]
fn refusal_messages_across_statements() {
    for (parameters, message) in [
        ("some_unknown_parameter=1", "unrecognized parameter \"some_unknown_parameter\""),
        ("fillfactor=999", "value 999 out of bounds for option \"fillfactor\""),
        ("fillfactor='bad'", "invalid value for integer option \"fillfactor\": bad"),
        ("fillfactor=10,fillfactor=100", "parameter \"fillfactor\" specified more than once"),
        ("other.fillfactor=10", "unrecognized parameter namespace \"other\""),
    ] {
        for statement in [
            format!("CREATE TABLE t(id INT) WITH({parameters});"),
            format!("CREATE TABLE t(id INT); ALTER TABLE t SET({parameters});"),
            format!("CREATE TABLE t(id INT); CREATE INDEX i ON t(id) WITH({parameters});"),
            format!("CREATE MATERIALIZED VIEW t WITH({parameters}) AS SELECT 1 AS id;"),
        ] {
            let result = ParserDB::parse::<PostgreSqlDialect>(&statement);
            assert!(result.is_err(), "{statement}");
            if let Err(error) = result {
                assert_eq!(error.to_string(), message, "{statement}");
            }
        }
    }
}

#[test]
fn typed_and_namespaced_refusals() {
    for (parameters, message) in [
        (
            "autovacuum_enabled='bad'",
            "invalid value for boolean option \"autovacuum_enabled\": bad",
        ),
        (
            "autovacuum_vacuum_scale_factor='bad'",
            "invalid value for floating point option \"autovacuum_vacuum_scale_factor\": bad",
        ),
        (
            "vacuum_index_cleanup='bad'",
            "invalid value for enum option \"vacuum_index_cleanup\": bad",
        ),
        ("toast.fillfactor=10", "unrecognized parameter \"fillfactor\""),
        (
            "autovacuum_vacuum_scale_factor='inf'",
            "value inf out of bounds for option \"autovacuum_vacuum_scale_factor\"",
        ),
        (
            "autovacuum_vacuum_scale_factor='-Infinity'",
            "value -Infinity out of bounds for option \"autovacuum_vacuum_scale_factor\"",
        ),
        (
            "log_autovacuum_min_duration='-1.5'",
            "value -1.5 out of bounds for option \"log_autovacuum_min_duration\"",
        ),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id TEXT) WITH({parameters});"
        ));
        assert!(result.is_err(), "{parameters}");
        if let Err(error) = result {
            assert_eq!(error.to_string(), message);
        }
    }
}

#[test]
fn valid_values_and_resets() -> Result<(), Error> {
    for parameters in [
        "fillfactor=10",
        "fillfactor=100",
        "autovacuum_enabled",
        "autovacuum_enabled='y'",
        "vacuum_truncate='OFF'",
        "vacuum_index_cleanup=auto",
        "toast.autovacuum_enabled=false",
        "toast.autovacuum_vacuum_cost_delay=100",
        "autovacuum_vacuum_max_threshold=-1",
        "fillfactor='99.5'",
        "log_autovacuum_min_duration='-0.6'",
        "log_autovacuum_min_duration='-0.5'",
    ] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id TEXT) WITH({parameters}); ALTER TABLE t SET({parameters}); ALTER TABLE t RESET(unknown,other.unknown);"
        ))?;
    }
    for (method, datatype, parameters) in [
        ("btree", "INT", "fillfactor=10,deduplicate_items='yes'"),
        ("hash", "INT", "fillfactor=100"),
        ("gist", "int4range", "buffering=auto,fillfactor=10"),
        ("gin", "INT[]", "fastupdate=off,gin_pending_list_limit=64"),
        ("brin", "INT", "pages_per_range=1,autosummarize=true"),
        ("spgist", "int4range", "fillfactor=10"),
        ("\"btree\"", "INT", "deduplicate_items"),
        ("\"hash\"", "INT", "fillfactor=100"),
        ("\"gist\"", "int4range", "buffering=auto"),
        ("\"gin\"", "INT[]", "fastupdate=off"),
        ("\"brin\"", "INT", "pages_per_range=1"),
        ("\"spgist\"", "int4range", "fillfactor=10"),
        ("custom_method", "INT", "unknown=999"),
    ] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id {datatype}); CREATE INDEX i ON t USING {method}(id) WITH({parameters});"
        ))?;
    }
    Ok(())
}

#[test]
fn other_dialects_leave_storage_parameters_unchecked() -> Result<(), Error> {
    ParserDB::parse::<GenericDialect>(
        "CREATE TABLE t(id INT) WITH(unknown=999);
         CREATE MATERIALIZED VIEW m WITH(unknown=999) AS SELECT id FROM t;
         ALTER TABLE m SET(fillfactor=999);",
    )?;
    let refused = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id); ALTER TABLE root SET (fillfactor = 70);",
    );
    assert!(
        matches!(
            &refused,
            Err(Error::StorageParametersOnPartitionedTable { table_name }) if table_name == "root"
        ),
        "got {refused:?}"
    );
    Ok(())
}

#[test]
fn index_kinds_refuse_unsupported_parameters() {
    for (method, datatype, parameters, message) in [
        ("hash", "INT", "deduplicate_items=true", "unrecognized parameter \"deduplicate_items\""),
        ("gist", "int4range", "buffering=bad", "invalid value for enum option \"buffering\": bad"),
        ("gin", "INT[]", "fillfactor=10", "unrecognized parameter \"fillfactor\""),
        (
            "brin",
            "INT",
            "pages_per_range=0",
            "value 0 out of bounds for option \"pages_per_range\"",
        ),
        ("spgist", "int4range", "fillfactor=9", "value 9 out of bounds for option \"fillfactor\""),
        (
            "btree",
            "INT",
            "toast.autovacuum_enabled=false",
            "unrecognized parameter namespace \"toast\"",
        ),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id {datatype}); CREATE INDEX i ON t USING {method}(id) WITH({parameters});"
        ));
        assert!(result.is_err(), "{method} {parameters}");
        if let Err(error) = result {
            assert_eq!(error.to_string(), message);
        }
    }
}

#[test]
fn bounds_carry_engine_details() {
    for (parameters, expected_detail) in [
        ("fillfactor=5", "Valid values are between \"10\" and \"100\"."),
        (
            "autovacuum_vacuum_scale_factor=101",
            "Valid values are between \"0.000000\" and \"100.000000\".",
        ),
        ("toast_tuple_target=8161", "Valid values are between \"128\" and \"8160\"."),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id TEXT) WITH({parameters});"
        ));
        assert!(
            matches!(result, Err(Error::StorageParameterOutOfBounds { detail, .. }) if detail == expected_detail)
        );
    }
}

#[test]
fn materialized_and_partitioned_targets() {
    for statement in [
        "CREATE MATERIALIZED VIEW t AS SELECT 1 AS id; CREATE INDEX i ON t(id) WITH(fillfactor=999);",
        "CREATE MATERIALIZED VIEW t AS SELECT 1 AS id; ALTER MATERIALIZED VIEW t SET(fillfactor=999);",
        "CREATE MATERIALIZED VIEW t AS SELECT 1 AS id; ALTER TABLE t SET(fillfactor=999);",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(statement);
        assert!(matches!(result, Err(Error::StorageParameterOutOfBounds { .. })), "{statement}");
    }
    for statement in [
        "CREATE TABLE t(id INT) WITH(fillfactor=10) PARTITION BY RANGE(id);",
        "CREATE TABLE t(id INT) PARTITION BY RANGE(id); ALTER TABLE t SET(fillfactor=10);",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(statement);
        assert!(
            matches!(result, Err(Error::StorageParametersOnPartitionedTable { .. })),
            "{statement} {result:?}"
        );
    }
}

#[test]
fn numeric_and_boolean_spellings() -> Result<(), Error> {
    for value in ["10", "100", "'0x10'", "'014'", "'10.5'", "'1e2'", "' 10 '"] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id INT) WITH(fillfactor={value});"
        ))?;
    }
    for value in ["'t'", "'f'", "'y'", "'n'", "'on'", "'of'", "'OFF'", "1", "0"] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id INT) WITH(autovacuum_enabled={value});"
        ))?;
    }
    for parameters in [
        "fillfactor='010'",
        "fillfactor='2147483648'",
        "autovacuum_enabled='o'",
        "autovacuum_enabled=' yes '",
        "autovacuum_vacuum_scale_factor='NaN'",
    ] {
        assert!(
            ParserDB::parse::<PostgreSqlDialect>(&format!(
                "CREATE TABLE t(id INT) WITH({parameters});"
            ))
            .is_err(),
            "{parameters}"
        );
    }
    Ok(())
}

#[test]
fn hexadecimal_reals_and_numeric_range_errors() -> Result<(), Error> {
    for parameters in [
        "fillfactor='0x1.8p4'",
        "autovacuum_vacuum_scale_factor='0x1p0'",
        "autovacuum_vacuum_scale_factor='0x1p1'",
        "autovacuum_vacuum_scale_factor='0x1p-1'",
        "autovacuum_vacuum_scale_factor='0x0p0'",
    ] {
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id INT) WITH({parameters});"
        ))?;
    }
    for value in ["1e-999", "1e999", "0x1pbad", "0x.p1", "0x1p9999", "0x1p-9999"] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id INT) WITH(autovacuum_vacuum_scale_factor='{value}');"
        ));
        assert!(result.is_err(), "{value}");
        if let Err(error) = result {
            assert_eq!(
                error.to_string(),
                format!(
                    "invalid value for floating point option \"autovacuum_vacuum_scale_factor\": {value}"
                )
            );
        }
    }
    Ok(())
}

#[test]
fn partitioned_tables_accept_toast_options() -> Result<(), Error> {
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t(id INT) WITH(toast.autovacuum_enabled=false) PARTITION BY RANGE(id); ALTER TABLE t SET(toast.autovacuum_enabled=true);",
    )?;
    Ok(())
}

#[test]
fn heap_numeric_parameter_boundaries() -> Result<(), Error> {
    for (name, minimum, maximum, toast) in [
        ("autovacuum_vacuum_threshold", 0, i32::MAX, true),
        ("autovacuum_vacuum_max_threshold", -1, i32::MAX, true),
        ("autovacuum_vacuum_insert_threshold", -1, i32::MAX, true),
        ("autovacuum_analyze_threshold", 0, i32::MAX, false),
        ("autovacuum_vacuum_cost_limit", 1, 10000, true),
        ("autovacuum_freeze_min_age", 0, 1000000000, true),
        ("autovacuum_multixact_freeze_min_age", 0, 1000000000, true),
        ("autovacuum_freeze_max_age", 100000, 2000000000, true),
        ("autovacuum_multixact_freeze_max_age", 10000, 2000000000, true),
        ("autovacuum_freeze_table_age", 0, 2000000000, true),
        ("autovacuum_multixact_freeze_table_age", 0, 2000000000, true),
        ("log_autovacuum_min_duration", -1, i32::MAX, true),
        ("toast_tuple_target", 128, 8160, false),
        ("parallel_workers", 0, 1024, false),
        ("autovacuum_vacuum_cost_delay", 0, 100, true),
        ("autovacuum_vacuum_scale_factor", 0, 100, true),
        ("autovacuum_vacuum_insert_scale_factor", 0, 100, true),
        ("autovacuum_analyze_scale_factor", 0, 100, false),
        ("vacuum_max_eager_freeze_failure_rate", 0, 1, true),
    ] {
        for namespace in ["", "toast."] {
            if !namespace.is_empty() && !toast {
                continue;
            }
            for value in [minimum, maximum] {
                ParserDB::parse::<PostgreSqlDialect>(&format!(
                    "CREATE TABLE t(id TEXT) WITH({namespace}{name}={value});"
                ))?;
            }
            let invalid = minimum - 1;
            let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
                "CREATE TABLE t(id TEXT) WITH({namespace}{name}={invalid});"
            ));
            assert!(
                matches!(result, Err(Error::StorageParameterOutOfBounds { .. })),
                "{namespace}{name}"
            );
        }
    }
    Ok(())
}

#[test]
fn storage_refusal_order() {
    let mut mismatches = Vec::new();
    for (statement, expected) in [
        (
            "CREATE TABLE t(a int,a int) WITH(fillfactor=999);",
            "value 999 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE TABLE t(a int PRIMARY KEY,b int PRIMARY KEY) WITH(fillfactor=999);",
            "multiple primary keys for table \"t\" are not allowed",
        ),
        (
            "CREATE TABLE t(LIKE absent) WITH(fillfactor=999);",
            "Table `absent` not found for the `LIKE` in table `t`.",
        ),
        (
            "CREATE TABLE t(a int) INHERITS(absent) WITH(fillfactor=999);",
            "Parent table `absent` not found for table `t`.",
        ),
        (
            "CREATE TYPE shell_type; CREATE TABLE t(a shell_type) WITH(fillfactor=999);",
            "Type `shell_type` is only a shell.",
        ),
        (
            "CREATE TABLE t(a int); CREATE INDEX i ON t(absent) WITH(fillfactor=999);",
            "value 999 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE TABLE t(a int); CREATE INDEX i ON t(a); CREATE INDEX i ON t(a) WITH(fillfactor=999);",
            "value 999 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE TYPE shell_type; CREATE TABLE t(a int); ALTER TABLE t SET(fillfactor=999), ADD COLUMN b shell_type;",
            "Type `shell_type` is only a shell.",
        ),
        (
            "CREATE TYPE shell_type; CREATE TABLE t(a int); ALTER TABLE t ADD COLUMN b shell_type, SET(fillfactor=999);",
            "Type `shell_type` is only a shell.",
        ),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(statement);
        match result {
            Err(error) if error.to_string() == expected => {}
            result => mismatches.push(format!("{statement} expected {expected}, got {result:?}")),
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn missing_altered_column_precedes_storage_options() {
    for operations in [
        "SET(fillfactor=999), ALTER COLUMN absent SET NOT NULL",
        "ALTER COLUMN absent SET NOT NULL, SET(fillfactor=999)",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(a int); ALTER TABLE t {operations};"
        ));
        assert!(
            matches!(result, Err(Error::IdentifierLookupError(sql_traits::errors::LookupError::ColumnNotFound { table_name, column_name })) if table_name == "t" && column_name == "absent")
        );
    }
}

#[test]
fn storage_options_require_names() -> Result<(), Error> {
    for (statement, option, relation) in [
        ("CREATE TABLE t(a int); CREATE INDEX i ON t(a) WITH(1=2);", "1 = 2", "i"),
        (
            "CREATE TABLE t(a int); CREATE INDEX i ON t USING custom_method(a) WITH(1=2);",
            "1 = 2",
            "i",
        ),
        (
            "CREATE TABLE t(a int) WITH(toast.extra.fillfactor=10);",
            "toast.extra.fillfactor = 10",
            "t",
        ),
        (
            "CREATE TABLE t(a int); CREATE INDEX i ON t(a) WITH(toast.extra.fillfactor=10);",
            "toast.extra.fillfactor = 10",
            "i",
        ),
        (
            "CREATE TABLE t(a int) WITH(CLUSTERED COLUMNSTORE INDEX);",
            "CLUSTERED COLUMNSTORE INDEX",
            "t",
        ),
    ] {
        let statements = sqlparser::parser::Parser::parse_sql(&GenericDialect {}, statement)?;
        let result = ParserDB::from_statements_with_dialect(
            statements,
            "postgres".into(),
            SqlparserDialect::PostgreSql,
        );
        assert!(result.is_err(), "{statement}");
        if let Err(error) = result {
            assert_eq!(
                error.to_string(),
                format!(
                    "Storage parameter `{option}` of relation `{relation}` must be a name with an optional value."
                )
            );
        }
    }
    Ok(())
}
