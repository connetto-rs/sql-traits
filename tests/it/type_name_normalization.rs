//! Tests that a normalized type name depends on the type rather than on how
//! it was spelled, so a parsed declaration and PostgreSQL's own print of the
//! same type normalize to one name.
#![allow(clippy::expect_used)]

use core::fmt::Write;

use sql_traits::{
    prelude::*,
    utils::{normalize_postgres_type, scalar_family::ScalarFamily},
};
use sqlparser::dialect::PostgreSqlDialect;

/// Each row is a return type as DDL may write it, beside `format_type`'s print
/// of the same type, which is how a catalog-backed profile names it.
const SPELLINGS: &[(&str, &str)] = &[
    ("VOID", "void"),
    ("TRIGGER", "trigger"),
    ("SETOF int4", "SETOF integer"),
    ("setof Text", "SETOF text"),
    ("int4[]", "integer[]"),
    ("INT[][]", "integer[]"),
    ("VARCHAR(3)[]", "character varying[]"),
    ("timestamptz", "timestamp with time zone"),
    ("bit varying", "bit varying"),
    ("bool", "boolean"),
    ("float8", "double precision"),
    ("FLOAT", "double precision"),
    ("FLOAT(10)", "real"),
    ("float(53)", "double precision"),
    ("DECIMAL(10, 2)", "numeric"),
    ("MyType", "mytype"),
    ("\"mytype\"", "mytype"),
    ("\"MyType\"", "\"MyType\""),
    ("\"char\"", "\"char\""),
    ("pg_catalog.int4", "integer"),
    ("App.Points", "app.points"),
    ("app.\"Points\"[]", "app.\"Points\"[]"),
];

/// Parses one function per row and returns each one's normalized return and
/// argument type, in row order.
fn parsed_normalized_names() -> Vec<(String, String)> {
    let mut ddl = String::new();
    for (index, (written, _)) in SPELLINGS.iter().enumerate() {
        // An argument cannot be a set, so it declares the element.
        let argument = match written.split_at_checked(6) {
            Some((keyword, element)) if keyword.eq_ignore_ascii_case("SETOF ") => element,
            _ => written,
        };
        write!(
            ddl,
            "CREATE FUNCTION f{index}(a {argument}) RETURNS {written} \
             LANGUAGE sql AS 'SELECT NULL';"
        )
        .expect("writing to a String cannot fail");
    }
    let database = ParserDB::parse::<PostgreSqlDialect>(&ddl).expect("every spelling parses");

    (0..SPELLINGS.len())
        .map(|index| {
            let function = database
                .function_by_target(
                    TargetName::new(&format!("f{index}"), false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("the DDL declares the function");
            let returned = function
                .normalized_return_type_name(&database)
                .expect("the function declares a return type")
                .into_owned();
            let [argument] = <[_; 1]>::try_from(function.normalized_argument_type_names(&database))
                .expect("the function declares one argument");
            (returned, argument.into_owned())
        })
        .collect()
}

#[test]
fn test_parsed_and_printed_spellings_normalize_alike() {
    for ((written, printed), (returned, argument)) in
        SPELLINGS.iter().zip(parsed_normalized_names())
    {
        let printed = normalize_postgres_type(printed);
        assert_eq!(returned, printed, "RETURNS {written}");
        let element = printed.strip_prefix("SETOF ").unwrap_or(&printed);
        assert_eq!(argument, element, "argument {written}");
    }
}

/// Folding never merges two types PostgreSQL keeps apart: a quoted name with
/// an upper-case letter, and the internal `"char"` beside `character`.
#[test]
fn test_quoting_keeps_distinct_types_apart() {
    for (left, right) in [
        ("\"MyType\"", "mytype"),
        ("\"MYTYPE\"", "mytype"),
        ("\"char\"", "char"),
        ("\"integer\"", "integer"),
        ("SETOF \"MyType\"", "SETOF mytype"),
        ("\"MyType\"[]", "mytype[]"),
    ] {
        assert_ne!(normalize_postgres_type(left), normalize_postgres_type(right), "{left}");
    }
}

/// Function identity runs through the same normalization, so an overload
/// differing only by spelling is a redeclaration, and one differing by a
/// quoted name is a second function.
#[test]
fn test_function_identity_follows_the_type() {
    let dropped = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(a int4[]) RETURNS INT LANGUAGE sql AS 'SELECT 1'; \
         DROP FUNCTION f(integer[]);",
    )
    .expect("the drop names the array type the create declared");
    assert_eq!(dropped.functions().filter(|function| function.name() == "f").count(), 0);

    let overloaded = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(a \"MyType\") RETURNS INT LANGUAGE sql AS 'SELECT 1'; \
         CREATE FUNCTION f(a MyType) RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("a quoted and an unquoted name are two types");
    assert_eq!(overloaded.functions().filter(|function| function.name() == "f").count(), 2);
}

/// A column's normalized type follows the same rule as a function's.
#[test]
fn test_column_type_normalizes_like_its_print() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (a int4[], b \"MyType\", c MyType, d timestamptz);",
    )
    .expect("the table parses");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the DDL declares the table");
    let names: Vec<String> = table
        .columns(&database)
        .expect("the table has columns")
        .map(|column| column.data_type(&database).normalized_name(&database).into_owned())
        .collect();
    let printed: Vec<String> = ["integer[]", "\"MyType\"", "mytype", "timestamp with time zone"]
        .into_iter()
        .map(|print| normalize_postgres_type(print).into_owned())
        .collect();
    assert_eq!(names, printed);
}

/// Each row is a column type as DDL may write it, beside PostgreSQL 18's
/// `format_type` print of the column, which carries its type modifiers.
const MODIFIED_COLUMNS: &[(&str, &str)] = &[
    ("NCHAR(4)", "character(4)"),
    ("NCHAR", "character(1)"),
    ("BPCHAR(4)", "character(4)"),
    ("bpchar(4)[]", "character(4)[]"),
    ("pg_catalog.bpchar(4)", "character(4)"),
    ("CHAR(4)", "character(4)"),
    ("bpchar", "bpchar"),
    ("\"char\"", "\"char\""),
    ("varchar(10)", "character varying(10)"),
    ("\"varchar\"(5)", "character varying(5)"),
    ("character varying(4)[]", "character varying(4)[]"),
    ("numeric(10,2)", "numeric(10,2)"),
    ("timestamptz(3)", "timestamp(3) with time zone"),
    ("timestamp(3)", "timestamp(3) without time zone"),
    ("time(2)", "time(2) without time zone"),
    ("timetz(2)", "time(2) with time zone"),
    ("bit(3)", "bit(3)"),
    ("bit varying(5)", "bit varying(5)"),
    ("interval(3)", "interval(3)"),
    ("interval year to month", "interval year to month"),
    ("interval day to second(3)", "interval day to second(3)"),
    ("interval minute", "interval minute"),
    ("float(10)", "real"),
];

/// A type modifier narrows a builtin without naming another type, so a
/// declared column and PostgreSQL's print of it answer one name wherever the
/// modifier stands.
#[test]
fn test_column_type_with_modifiers_normalizes_like_its_print() {
    let mut ddl = String::from("CREATE TABLE t (");
    for (index, (declared, _)) in MODIFIED_COLUMNS.iter().enumerate() {
        if index > 0 {
            ddl.push_str(", ");
        }
        write!(ddl, "c{index} {declared}").expect("writing to a string");
    }
    ddl.push_str(");");
    let database = ParserDB::parse::<PostgreSqlDialect>(&ddl).expect("the table parses");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the DDL declares the table");
    let columns = table.columns(&database).expect("the table has columns");
    for ((declared, printed), column) in MODIFIED_COLUMNS.iter().zip(columns) {
        assert_eq!(
            column.data_type(&database).normalized_name(&database),
            normalize_postgres_type(printed),
            "{declared} beside {printed}"
        );
    }
}

/// Every spelling PostgreSQL stores as the blank-padded `bpchar` answers
/// `CHAR`, element types of arrays included, while the one-byte `"char"`
/// stays apart.
#[test]
fn test_bpchar_spellings_answer_char() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (a NCHAR(4), b nchar, c BPCHAR(4), d pg_catalog.bpchar(4), \
         e character(4), f bpchar, g bpchar(4)[], h nchar(2)[], i \"char\");",
    )
    .expect("the table parses");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the DDL declares the table");
    let names: Vec<String> = table
        .columns(&database)
        .expect("the table has columns")
        .map(|column| {
            let data_type = column.data_type(&database);
            let element = data_type.array_element(&database).unwrap_or(data_type);
            element.normalized_name(&database).into_owned()
        })
        .collect();
    assert_eq!(names, ["CHAR", "CHAR", "CHAR", "CHAR", "CHAR", "CHAR", "CHAR", "CHAR", "\"char\""]);
}

/// A `pg_catalog` qualifier names the type its bare spelling names, so both
/// spellings answer one scalar family.
#[test]
fn test_pg_catalog_qualified_type_has_the_family_of_its_bare_spelling() {
    let pairs = [
        ("pg_catalog.timestamptz", "timestamptz", ScalarFamily::TimestampTz),
        ("\"pg_catalog\".\"timestamptz\"", "timestamptz", ScalarFamily::TimestampTz),
        ("pg_catalog.timestamptz(3)", "timestamptz(3)", ScalarFamily::TimestampTz),
        ("pg_catalog.timestamp", "timestamp", ScalarFamily::Timestamp),
        ("pg_catalog.timetz", "timetz", ScalarFamily::Time),
        ("pg_catalog.time(3)", "time(3)", ScalarFamily::Time),
        ("PG_CATALOG.Int4", "int4", ScalarFamily::Int),
        ("pg_catalog.bool", "bool", ScalarFamily::Bool),
        ("pg_catalog.float8", "float8", ScalarFamily::Float),
        ("pg_catalog.numeric(10, 2)", "numeric(10, 2)", ScalarFamily::Decimal),
        ("pg_catalog.varchar(10)", "varchar(10)", ScalarFamily::String),
        ("pg_catalog.bpchar(3)", "bpchar(3)", ScalarFamily::String),
        ("pg_catalog.bytea", "bytea", ScalarFamily::Bytes),
        ("pg_catalog.uuid", "uuid", ScalarFamily::Uuid),
        ("pg_catalog.date", "date", ScalarFamily::Date),
        ("pg_catalog.interval", "interval", ScalarFamily::Interval),
        ("pg_catalog.jsonb", "jsonb", ScalarFamily::Jsonb),
    ];
    let mut ddl = String::from("CREATE TABLE t (");
    for (index, (qualified, bare, _)) in pairs.iter().enumerate() {
        if index > 0 {
            ddl.push_str(", ");
        }
        write!(ddl, "q{index} {qualified}, b{index} {bare}").expect("writing to a string");
    }
    ddl.push_str(");");
    let database = ParserDB::parse::<PostgreSqlDialect>(&ddl).expect("the table parses");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the DDL declares the table");
    let families: Vec<_> = table
        .columns(&database)
        .expect("the table has columns")
        .map(|column| column.data_type(&database).scalar_family(&database))
        .collect();
    for ((qualified, bare, family), spellings) in pairs.iter().zip(families.chunks(2)) {
        assert_eq!(spellings, [Some(*family); 2], "{qualified} beside {bare}");
    }
}
