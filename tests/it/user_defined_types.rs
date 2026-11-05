//! Tests that `CREATE TYPE` records each of the forms PostgreSQL writes, that
//! a type name reaches the type `CREATE TYPE` defined, and that types are
//! dropped, renamed and enum-altered as PostgreSQL does.
//!
//! The expected outcomes are the ones PostgreSQL 18 gives for the same
//! statements.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::{DataType, ObjectName, ObjectNamePart},
    dialect::PostgreSqlDialect,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The user-defined type `name` is stored as, or `None` when the database
/// holds no type of that name in the default schema.
fn user_type<'db>(database: &'db ParserDB, name: &str) -> Option<&'db UserDefinedType> {
    database.user_defined_types().find(|type_| type_.name() == name)
}

/// The labels an enum type `name` holds, by written value.
fn enum_labels(database: &ParserDB, name: &str) -> Vec<String> {
    use sqlparser::ast::UserDefinedTypeRepresentation;
    let representation = user_type(database, name)
        .and_then(UserDefinedType::representation)
        .expect("the type holds a representation");
    match representation {
        UserDefinedTypeRepresentation::Enum { labels } => {
            labels.iter().map(|label| label.value.clone()).collect()
        }
        _ => panic!("the type is an enum"),
    }
}

/// The last name part of a stored object name, which a range's multirange
/// carries under its schema.
fn name_only(name: &ObjectName) -> String {
    match name.0.last() {
        Some(ObjectNamePart::Identifier(ident)) => ident.value.clone(),
        _ => String::new(),
    }
}

/// The schema a two-part stored object name carries, or `None` for a bare name.
fn schema_of(name: &ObjectName) -> Option<String> {
    if name.0.len() < 2 {
        return None;
    }
    match &name.0[name.0.len() - 2] {
        ObjectNamePart::Identifier(ident) => Some(ident.value.clone()),
        ObjectNamePart::Function(_) => None,
    }
}

#[test]
fn the_fuzz_corpus_parses() {
    let corpus = include_str!("../../fuzz/corpus/create_type.sql");
    parse(corpus).expect("the corpus is a valid sequence");
}

#[test]
fn a_composite_is_stored_with_its_attributes() {
    let database = parse("CREATE TYPE point AS (x int, y int);").expect("the type is created");
    let type_ = user_type(&database, "point").expect("the type is stored");
    assert_eq!(type_.kind(), UserDefinedTypeKind::Composite);
    let attributes = type_.composite_attributes().expect("the composite has attributes");
    assert_eq!(
        attributes.iter().map(|attribute| attribute.name.value.as_str()).collect::<Vec<_>>(),
        ["x", "y"]
    );
}

#[test]
fn an_enum_is_stored_with_its_labels() {
    let database = parse("CREATE TYPE mood AS ENUM ('sad', 'ok');").expect("the type is created");
    let type_ = user_type(&database, "mood").expect("the type is stored");
    assert_eq!(type_.kind(), UserDefinedTypeKind::Enum);
    assert_eq!(enum_labels(&database, "mood"), ["sad", "ok"]);
}

#[test]
fn a_range_is_stored_with_its_subtype_and_multirange() {
    let database =
        parse("CREATE TYPE span AS RANGE (subtype = int);").expect("the type is created");
    let type_ = user_type(&database, "span").expect("the type is stored");
    assert_eq!(type_.kind(), UserDefinedTypeKind::Range);
    assert!(matches!(type_.range_subtype(), Some(DataType::Int(..))));
    let multirange = type_.multirange_name().expect("the range brings a multirange");
    assert_eq!(name_only(multirange), "span_multirange");
}

#[test]
fn the_multirange_name_replaces_the_first_range_spelling() {
    let database =
        parse("CREATE TYPE myrangeish AS RANGE (subtype = int);").expect("the type is created");
    let multirange = user_type(&database, "myrangeish")
        .and_then(UserDefinedType::multirange_name)
        .expect("the range brings a multirange");
    assert_eq!(name_only(multirange), "mymultirangeish");
}

#[test]
fn a_multirange_name_can_be_written() {
    let database = parse("CREATE TYPE span AS RANGE (subtype = int, multirange_type_name = mm);")
        .expect("the type is created");
    let multirange = user_type(&database, "span")
        .and_then(UserDefinedType::multirange_name)
        .expect("the range brings a multirange");
    assert_eq!(name_only(multirange), "mm");
}

#[test]
fn a_range_without_a_subtype_is_refused() {
    let error = parse("CREATE TYPE span AS RANGE ();").expect_err("a subtype is required");
    assert!(matches!(error, Error::RangeSubtypeRequired { .. }), "{error:?}");
}

#[test]
fn a_shell_is_stored_until_it_is_filled() {
    let database = parse("CREATE TYPE mood;").expect("the shell is recorded");
    let type_ = user_type(&database, "mood").expect("the shell is stored");
    assert_eq!(type_.kind(), UserDefinedTypeKind::Shell);
    assert!(type_.is_shell());
}

#[test]
fn a_second_shell_of_a_name_is_refused() {
    let error = parse("CREATE TYPE mood; CREATE TYPE mood;").expect_err("the name is held");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_type_lands_in_the_schema_it_names() {
    let database = parse(
        "
CREATE SCHEMA types;
CREATE TYPE types.mood AS ENUM ('sad');
",
    )
    .expect("the type is created");
    assert_eq!(user_type(&database, "mood").expect("the type is stored").schema(), Some("types"));
}

#[test]
fn a_type_may_not_take_a_name_a_domain_holds() {
    let error = parse(
        "
CREATE DOMAIN span AS int;
CREATE TYPE span AS (a int);
",
    )
    .expect_err("the domain holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Domain,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_sequence_may_not_take_a_name_an_enum_holds() {
    let error = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE SEQUENCE e;
",
    )
    .expect_err("the enum holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Sequence,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn an_index_may_take_a_name_an_enum_holds() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TABLE t (v int);
CREATE INDEX e ON t (v);
",
    )
    .expect("an index shares nothing with a type");
    assert!(user_type(&database, "e").is_some());
}

#[test]
fn a_domain_takes_a_shell_over() {
    let database = parse(
        "
CREATE TYPE sh;
CREATE DOMAIN sh AS int;
",
    )
    .expect("the domain takes the shell over");
    assert!(user_type(&database, "sh").is_none(), "the shell is gone");
    assert!(database.domains().any(|domain| domain.name() == "sh"), "the domain is stored");
}

#[test]
fn a_table_takes_a_shell_over() {
    let database = parse(
        "
CREATE TYPE sh;
CREATE TABLE sh (c int);
",
    )
    .expect("the table takes the shell over");
    assert!(user_type(&database, "sh").is_none(), "the shell is gone");
    assert!(
        database
            .table_by_target(TargetName::new("sh", false), IdentifierCase::AsWritten)
            .expect("the lookup is unambiguous")
            .is_some(),
        "the table is stored"
    );
}

#[test]
fn a_column_of_a_shell_type_is_refused() {
    let error = parse(
        "
CREATE TYPE sh;
CREATE TABLE t (c sh);
",
    )
    .expect_err("the type is only a shell");
    assert!(matches!(error, Error::TypeIsOnlyShell { .. }), "{error:?}");
}

#[test]
fn a_composite_attribute_of_a_shell_is_refused() {
    let error = parse(
        "
CREATE TYPE sh;
CREATE TYPE outer AS (c sh);
",
    )
    .expect_err("the type is only a shell");
    assert!(matches!(error, Error::TypeIsOnlyShell { .. }), "{error:?}");
}

#[test]
fn a_range_subtype_of_a_shell_is_refused() {
    let error = parse(
        "
CREATE TYPE sh;
CREATE TYPE r AS RANGE (subtype = sh);
",
    )
    .expect_err("the type is only a shell");
    assert!(matches!(error, Error::TypeIsOnlyShell { .. }), "{error:?}");
}

#[test]
fn a_composite_attribute_keeps_a_name_the_input_never_creates() {
    let database = parse("CREATE TYPE outer AS (c nosuch);").expect("the name is kept unbound");
    assert!(user_type(&database, "outer").is_some(), "the type is stored");
}

#[test]
fn a_range_subtype_keeps_a_name_the_input_never_creates() {
    let database =
        parse("CREATE TYPE r AS RANGE (subtype = nosuch);").expect("the name is kept unbound");
    assert!(user_type(&database, "r").is_some(), "the type is stored");
}

#[test]
fn a_rename_onto_a_shell_is_refused() {
    let error = parse(
        "
CREATE TYPE sd;
CREATE TYPE c AS (a int);
ALTER TYPE c RENAME TO sd;
",
    )
    .expect_err("the shell holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_dropping_type_with_dependent_columns_is_refused_without_cascade() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (c c);
DROP TYPE c;
",
    )
    .expect_err("a column uses the type");
    assert!(
        matches!(error, Error::TypeHasDependents { dependent_kind: ObjectKind::Column, .. }),
        "{error:?}"
    );
}

#[test]
fn a_cascading_drop_takes_the_dependent_column() {
    let database = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (c c);
DROP TYPE c CASCADE;
",
    )
    .expect("the cascade drops the column and the type");
    assert!(user_type(&database, "c").is_none(), "the type is gone");
    assert!(
        database
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("the lookup is unambiguous")
            .expect("the table exists")
            .columns(&database)
            .expect("the columns resolve")
            .all(|column| column.column_name() != "c"),
        "the column was dropped"
    );
}

#[test]
fn a_multirange_is_dropped_with_its_range() {
    let error = parse(
        "
CREATE TYPE r AS RANGE (subtype = int);
DROP TYPE r_multirange;
",
    )
    .expect_err("the range requires the multirange");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
}

#[test]
fn a_row_type_is_dropped_with_its_relation() {
    let error = parse(
        "
CREATE TABLE t (a int);
DROP TYPE t;
",
    )
    .expect_err("the table requires its row type");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Table, .. }),
        "{error:?}"
    );
}

#[test]
fn a_drop_domain_of_a_composite_is_refused() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
DROP DOMAIN c;
",
    )
    .expect_err("a composite is not a domain");
    assert!(
        matches!(
            error,
            Error::RelationKindMismatch {
                expected_kind: ObjectKind::Domain,
                actual_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_drop_type_if_exists_of_a_missing_type_is_skipped() {
    parse("DROP TYPE IF EXISTS nothing;").expect("IF EXISTS skips a missing type");
}

#[test]
fn a_drop_type_of_a_catalog_type_is_refused() {
    let error = parse("DROP TYPE IF EXISTS name;").expect_err("the catalog holds the type");
    assert!(matches!(error, Error::TypeRequiredBySystem { .. }), "{error:?}");
}

#[test]
fn a_rename_moves_the_stored_name_and_its_references() {
    let database = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (c c);
ALTER TYPE c RENAME TO c2;
",
    )
    .expect("the rename is applied");
    assert!(user_type(&database, "c").is_none(), "the old name is gone");
    assert!(user_type(&database, "c2").is_some(), "the new name is stored");
}

#[test]
fn a_rename_of_a_range_keeps_its_multirange_name() {
    let database = parse(
        "
CREATE TYPE r AS RANGE (subtype = int);
ALTER TYPE r RENAME TO r2;
",
    )
    .expect("the rename is applied");
    let multirange = user_type(&database, "r2")
        .and_then(UserDefinedType::multirange_name)
        .expect("the range keeps its multirange");
    assert_eq!(name_only(multirange), "r_multirange");
}

#[test]
fn a_rename_onto_the_row_type_of_a_relation_is_refused() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (a int);
ALTER TYPE c RENAME TO t;
",
    )
    .expect_err("the row type is held");
    assert!(matches!(error, Error::TypeIsRowType { .. }), "{error:?}");
}

#[test]
fn a_rename_of_a_missing_type_is_refused() {
    let error = parse("ALTER TYPE nothing RENAME TO n;").expect_err("no such type");
    assert!(matches!(error, Error::TypeNotFound { .. }), "{error:?}");
}

#[test]
fn a_rename_onto_a_taken_name_is_refused() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
CREATE TYPE e AS ENUM ('x');
ALTER TYPE c RENAME TO e;
",
    )
    .expect_err("the enum holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_composite_rename_onto_an_index_name_is_refused() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (a int);
CREATE INDEX i ON t (a);
ALTER TYPE c RENAME TO i;
",
    )
    .expect_err("the index holds the relation name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Index,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn an_added_enum_value_is_stored() {
    let database = parse(
        "
CREATE TYPE mood AS ENUM ('sad');
ALTER TYPE mood ADD VALUE 'ok';
",
    )
    .expect("the value is added");
    assert_eq!(enum_labels(&database, "mood"), ["sad", "ok"]);
}

#[test]
fn an_added_enum_value_before_a_label_is_stored_in_order() {
    let database = parse(
        "
CREATE TYPE mood AS ENUM ('a', 'c');
ALTER TYPE mood ADD VALUE 'b' BEFORE 'c';
",
    )
    .expect("the value is added");
    assert_eq!(enum_labels(&database, "mood"), ["a", "b", "c"]);
}

#[test]
fn an_existing_enum_value_is_refused() {
    let error = parse(
        "
CREATE TYPE mood AS ENUM ('a');
ALTER TYPE mood ADD VALUE 'a';
",
    )
    .expect_err("the label exists");
    assert!(matches!(error, Error::EnumLabelExists { .. }), "{error:?}");
}

#[test]
fn an_existing_enum_value_is_skipped_with_if_not_exists() {
    parse(
        "
CREATE TYPE mood AS ENUM ('a');
ALTER TYPE mood ADD VALUE IF NOT EXISTS 'a';
",
    )
    .expect("IF NOT EXISTS skips a held label");
}

#[test]
fn an_added_value_before_an_unknown_label_is_refused() {
    let error = parse(
        "
CREATE TYPE mood AS ENUM ('a');
ALTER TYPE mood ADD VALUE 'b' BEFORE 'nope';
",
    )
    .expect_err("no such label");
    assert!(matches!(error, Error::EnumLabelNotFound { .. }), "{error:?}");
}

#[test]
fn an_enum_value_is_renamed() {
    let database = parse(
        "
CREATE TYPE mood AS ENUM ('sad');
ALTER TYPE mood RENAME VALUE 'sad' TO 'ok';
",
    )
    .expect("the value is renamed");
    assert_eq!(enum_labels(&database, "mood"), ["ok"]);
}

#[test]
fn a_rename_value_of_an_unknown_label_is_refused() {
    let error = parse(
        "
CREATE TYPE mood AS ENUM ('a');
ALTER TYPE mood RENAME VALUE 'nope' TO 'b';
",
    )
    .expect_err("no such label");
    assert!(matches!(error, Error::EnumLabelNotFound { .. }), "{error:?}");
}

#[test]
fn an_enum_value_renamed_onto_a_held_label_is_refused() {
    let error = parse(
        "
CREATE TYPE mood AS ENUM ('a', 'b');
ALTER TYPE mood RENAME VALUE 'a' TO 'b';
",
    )
    .expect_err("the label exists");
    assert!(matches!(error, Error::EnumLabelExists { .. }), "{error:?}");
}

#[test]
fn an_add_value_on_a_composite_is_refused() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
ALTER TYPE c ADD VALUE 'x';
",
    )
    .expect_err("a composite is no enum");
    assert!(matches!(error, Error::NotAnEnum { .. }), "{error:?}");
}

#[test]
fn an_add_value_on_a_domain_is_refused() {
    let error = parse(
        "
CREATE DOMAIN d AS int;
ALTER TYPE d ADD VALUE 'x';
",
    )
    .expect_err("a domain is no enum");
    assert!(matches!(error, Error::NotAnEnum { .. }), "{error:?}");
}

#[test]
fn a_duplicated_enum_label_is_refused() {
    let error = parse("CREATE TYPE mood AS ENUM ('a', 'a');").expect_err("the label is doubled");
    assert!(matches!(error, Error::DuplicateEnumLabel { .. }), "{error:?}");
}

#[test]
fn a_base_type_holds_the_function_it_declares() {
    let database = parse(
        "
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t
LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in);
",
    )
    .expect("the base type is defined");
    assert_eq!(
        user_type(&database, "t").expect("the type is stored").kind(),
        UserDefinedTypeKind::Base
    );
}

#[test]
fn a_function_a_base_type_declares_may_not_be_dropped() {
    let error = parse(
        "
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t
LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in);
DROP FUNCTION t_in(cstring);
",
    )
    .expect_err("the type references the function");
    assert!(matches!(error, Error::FunctionReferenced { .. }), "{error:?}");
}

#[test]
fn a_schema_with_a_type_may_not_be_dropped_restricted() {
    let error = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.m AS ENUM ('a');
DROP SCHEMA s;
",
    )
    .expect_err("the schema holds a type");
    assert!(matches!(error, Error::SchemaNotEmpty { .. }), "{error:?}");
}

#[test]
fn a_schema_rename_keeps_its_types_reachable() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.m AS ENUM ('a');
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    let type_ = database
        .user_defined_types()
        .find(|type_| type_.name() == "m")
        .expect("the type is stored");
    assert_eq!(type_.schema(), Some("s2"));
}

#[test]
fn a_drop_type_of_an_array_of_a_stored_type_is_refused() {
    let error = parse(
        "
CREATE TYPE ccomp AS ENUM ('a');
DROP TYPE _ccomp;
",
    )
    .expect_err("the type requires its array");
    assert!(
        matches!(
            error,
            Error::TypeDropRequiredBy {
                ref type_name,
                requiring_kind: ObjectKind::Type,
                ref requiring_name,
            } if type_name.as_str() == "ccomp[]" && requiring_name.as_str() == "ccomp"
        ),
        "{error:?}"
    );
}

/// A quoted array name reaches the array of the type its folded base names.
#[test]
fn a_quoted_array_name_reaches_the_array_of_its_type() {
    for sql in [
        "CREATE TYPE ccomp AS ENUM ('a'); DROP TYPE \"_ccomp\";",
        "CREATE TYPE \"Cq\" AS ENUM ('a'); DROP TYPE \"_Cq\";",
    ] {
        let error = parse(sql).expect_err(sql);
        assert!(matches!(error, Error::TypeDropRequiredBy { .. }), "{sql}: {error:?}");
    }
    let error =
        parse("CREATE TYPE \"Cq\" AS ENUM ('a'); DROP TYPE _Cq;").expect_err("folds to _cq");
    assert!(matches!(error, Error::TypeNotFound { .. }), "{error:?}");
}

#[test]
fn an_alter_type_of_an_array_of_a_stored_type_is_refused() {
    let error = parse(
        "
CREATE TYPE ccomp AS ENUM ('a');
ALTER TYPE _ccomp RENAME TO x;
",
    )
    .expect_err("the array type is not altered on its own");
    assert!(
        matches!(
            error,
            Error::CannotAlterArrayType { ref array_name } if array_name.as_str() == "ccomp[]"
        ),
        "{error:?}"
    );
}

#[test]
fn a_schema_rename_retargs_a_ranges_multirange() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.r AS RANGE (subtype = int);
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    let multirange = database
        .user_defined_types()
        .find(|type_| type_.name() == "r")
        .and_then(UserDefinedType::multirange_name)
        .expect("the range brings a multirange");
    assert_eq!(name_only(multirange), "r_multirange");
    assert_eq!(schema_of(multirange).as_deref(), Some("s2"));
}

#[test]
fn a_schema_rename_keeps_a_domain_dependent_on_a_moved_type() {
    let error = parse(
        "
CREATE SCHEMA s3;
CREATE TYPE s3.m AS ENUM ('a');
CREATE DOMAIN d3 AS s3.m;
ALTER SCHEMA s3 RENAME TO s3b;
DROP TYPE s3b.m;
",
    )
    .expect_err("the domain depends on the type");
    assert!(
        matches!(
            error,
            Error::TypeHasDependents {
                dependent_kind: ObjectKind::Domain,
                ref dependent_name,
                ..
            } if dependent_name.as_str() == "d3"
        ),
        "{error:?}"
    );
}

#[test]
fn a_base_type_element_blocks_a_drop_of_its_element() {
    let error = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE t (element = e);
DROP TYPE e;
",
    )
    .expect_err("the type uses the element");
    assert!(
        matches!(
            error,
            Error::TypeHasDependents {
                dependent_kind: ObjectKind::Type,
                ref dependent_name,
                ..
            } if dependent_name.as_str() == "t"
        ),
        "{error:?}"
    );
}

#[test]
fn a_cascading_drop_of_an_element_takes_its_base_type() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE t (element = e);
DROP TYPE e CASCADE;
",
    )
    .expect("the cascade drops the type and its element");
    assert!(user_type(&database, "t").is_none(), "the type is gone");
    assert!(user_type(&database, "e").is_none(), "the element is gone");
}

#[test]
fn a_table_rename_onto_a_shell_type_is_refused() {
    let error = parse(
        "
CREATE TYPE sh3;
CREATE TABLE rt2 (a int);
ALTER TABLE rt2 RENAME TO sh3;
",
    )
    .expect_err("the shell holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Table,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_view_rename_onto_a_user_type_is_refused() {
    let error = parse(
        "
CREATE TYPE ev AS ENUM ('a');
CREATE VIEW v AS SELECT 1;
ALTER VIEW v RENAME TO ev;
",
    )
    .expect_err("the type holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::View,
                conflicting_kind: ObjectKind::Type,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_composite_with_a_duplicated_attribute_is_refused() {
    let error = parse("CREATE TYPE duc AS (a int, a int);").expect_err("the attribute is doubled");
    assert!(
        matches!(
            error,
            Error::DuplicateCompositeAttribute { ref attribute, .. } if attribute.as_str() == "a"
        ),
        "{error:?}"
    );
}

#[test]
fn an_add_value_on_a_renamed_multirange_names_the_written_name() {
    let error = parse(
        "
CREATE TYPE r AS RANGE (subtype = int);
ALTER TYPE r_multirange RENAME TO mr2;
ALTER TYPE mr2 ADD VALUE 'x';
",
    )
    .expect_err("a multirange is no enum");
    assert!(
        matches!(error, Error::NotAnEnum { ref type_name } if type_name.as_str() == "mr2"),
        "{error:?}"
    );
}

#[test]
fn a_typmod_in_function_is_resolved_with_one_argument() {
    let error = parse(
        "
CREATE TYPE tm;
CREATE FUNCTION tm_mod_in(cstring[]) RETURNS int4
LANGUAGE internal IMMUTABLE STRICT AS 'foo';
CREATE TYPE tm (typmod_in = tm_mod_in);
DROP FUNCTION tm_mod_in(cstring[]);
",
    )
    .expect_err("the type references the one-argument function");
    assert!(matches!(error, Error::FunctionReferenced { .. }), "{error:?}");
}

#[test]
fn a_stored_type_named_underscore_is_not_its_base_array() {
    let database = parse(
        "
CREATE TYPE x AS ENUM ('a');
CREATE TYPE _x AS ENUM ('b');
DROP TYPE _x;
",
    )
    .expect("the type is dropped, not refused as x's array");
    assert!(user_type(&database, "_x").is_none(), "the type is gone");
    assert!(user_type(&database, "x").is_some(), "x survives");
}

#[test]
fn a_stored_type_named_underscore_is_renamed_not_refused_as_array() {
    let database = parse(
        "
CREATE TYPE x AS ENUM ('a');
CREATE TYPE _x AS ENUM ('b');
ALTER TYPE _x RENAME TO y;
",
    )
    .expect("the rename is applied, not refused as x's array");
    assert!(user_type(&database, "_x").is_none(), "the old name is gone");
    assert!(user_type(&database, "y").is_some(), "the new name is stored");
    assert!(user_type(&database, "x").is_some(), "x survives");
}
