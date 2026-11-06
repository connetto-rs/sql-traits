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
    ast::{
        CreateTable, DataType, ObjectName, ObjectNamePart, UserDefinedTypeCompositeAttributeDef,
        UserDefinedTypeRangeOption, UserDefinedTypeRepresentation,
        UserDefinedTypeSqlDefinitionOption,
    },
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

/// The name a declared type writes, or the built-in it is.
fn custom_name(data_type: &DataType) -> String {
    match data_type {
        DataType::Custom(name, _) => name.to_string(),
        other => other.to_string(),
    }
}

/// The type the column `column_name` of `table` declares.
fn column_type(table: &CreateTable, database: &ParserDB, column_name: &str) -> DataType {
    table
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("the column is stored")
        .expect("the column exists")
        .data_type(database)
        .clone()
}

/// The type a domain named `domain_name` declares.
fn domain_type(database: &ParserDB, domain_name: &str) -> DataType {
    database
        .domains()
        .find(|domain| domain.name() == domain_name)
        .expect("the domain is stored")
        .data_type
        .clone()
}

/// The attribute a composite named `composite_name` declares.
fn composite_attribute(
    database: &ParserDB,
    composite_name: &str,
) -> UserDefinedTypeCompositeAttributeDef {
    user_type(database, composite_name)
        .expect("the composite is stored")
        .composite_attributes()
        .expect("the composite holds an attribute")[0]
        .clone()
}

/// The subtype a range named `range_name` declares.
fn range_subtype(database: &ParserDB, range_name: &str) -> DataType {
    user_type(database, range_name)
        .expect("the range is stored")
        .range_subtype()
        .expect("the range holds its subtype")
        .clone()
}

/// The name a type's input option declares.
fn input_name(type_: &UserDefinedType) -> &ObjectName {
    match type_.representation().expect("the type holds a representation") {
        UserDefinedTypeRepresentation::SqlDefinition { options } => {
            options
                .iter()
                .find_map(|option| {
                    match option {
                        UserDefinedTypeSqlDefinitionOption::Input(name) => Some(name),
                        _ => None,
                    }
                })
                .expect("the type declares its input")
        }
        _ => panic!("the type is a base type"),
    }
}

#[test]
fn the_fuzz_corpus_parses() {
    let corpus = include_str!("../../fuzz/corpus/create_type.sql");
    parse(corpus).expect("the corpus is a valid sequence");
}

/// A range that names an operator class before its subtype still binds the
/// subtype it names.
#[test]
fn a_range_that_names_an_operator_class_keeps_its_subtype_bound() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE r AS RANGE (subtype_opclass = btree, subtype = e);
ALTER TYPE e RENAME TO e2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&range_subtype(&database, "r")), "e2");
}

/// The multirange a range brings takes the `multi` prefix the type's name
/// holds in place of its first one.
#[test]
fn the_multirange_name_replaces_the_first_range_spelling() {
    let error = parse(
        "
CREATE TYPE myrangeish AS RANGE (subtype = int);
DROP TYPE mymultirangeish;
",
    )
    .expect_err("the range requires its multirange");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
}

/// A written `MULTIRANGE_TYPE_NAME` is the multirange's name.
#[test]
fn a_multirange_name_can_be_written() {
    let error = parse(
        "
CREATE TYPE span AS RANGE (subtype = int, multirange_type_name = mm);
DROP TYPE mm;
",
    )
    .expect_err("the range requires its multirange");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
}

#[test]
fn a_range_without_a_subtype_is_refused() {
    let error = parse("CREATE TYPE span AS RANGE ();").expect_err("a subtype is required");
    assert!(matches!(error, Error::RangeSubtypeRequired { .. }), "{error:?}");
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

/// A composite holds a relation name as well, so an index may not take it.
#[test]
fn an_index_may_not_take_a_name_a_composite_holds() {
    let error = parse(
        "
CREATE TYPE c AS (a int);
CREATE TABLE t (v int);
CREATE INDEX c ON t (v);
",
    )
    .expect_err("the composite holds the relation name");
    assert!(
        matches!(error, Error::RelationNameAlreadyTaken { conflicting_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
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
    let error = parse(
        "
CREATE TYPE r AS RANGE (subtype = int);
ALTER TYPE r RENAME TO r2;
DROP TYPE r_multirange;
",
    )
    .expect_err("the range requires its multirange");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
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

/// A schema rename re-targets the multirange its ranges bring.
#[test]
fn a_schema_rename_retargets_a_ranges_multirange() {
    let error = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.r AS RANGE (subtype = int);
ALTER SCHEMA s RENAME TO s2;
DROP TYPE s2.r_multirange;
",
    )
    .expect_err("the range requires its multirange");
    assert!(
        matches!(error, Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Type, .. }),
        "{error:?}"
    );
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

/// The name a type takes must not be one a relation holds in the schema,
/// because a composite of the name would take it too.
#[test]
fn a_composite_may_not_take_a_relation_name() {
    let error = parse(
        "
CREATE TABLE ct (a int);
CREATE TYPE ct AS (a int);
",
    )
    .expect_err("the table holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Table,
                ..
            }
        ),
        "{error:?}"
    );
}

/// A range's multirange may not take a name a table, a domain or a type
/// holds in the schema.
#[test]
fn a_range_multirange_may_not_take_a_name_in_use() {
    let statements = [
        (
            ObjectKind::Table,
            "CREATE TABLE r_multirange (a int); CREATE TYPE r AS RANGE (subtype = int);",
        ),
        (
            ObjectKind::Domain,
            "CREATE DOMAIN r_multirange AS int4; CREATE TYPE r AS RANGE (subtype = int);",
        ),
        (
            ObjectKind::Type,
            "CREATE TYPE r_multirange AS ENUM ('a'); CREATE TYPE r AS RANGE (subtype = int);",
        ),
    ];
    for (holder, sql) in statements {
        let error = parse(sql).expect_err(sql);
        assert!(
            matches!(
                error,
                Error::RelationNameAlreadyTaken {
                    conflicting_kind, ..
                } if conflicting_kind == holder
            ),
            "{sql}: {error:?}"
        );
    }
}

/// A composite attribute may name a type of pg_catalog, as a column of one
/// may.
#[test]
fn a_composite_attribute_may_name_a_catalog_type() {
    parse("CREATE TYPE c AS (x oid);").expect("oid is a type of the database");
}

/// A composite attribute keeps a three-part name the input never resolves.
#[test]
fn a_composite_attribute_keeps_a_three_part_name() {
    parse("CREATE TYPE c AS (x a.b.c);").expect("the name is kept");
}

/// A composite attribute may name a range's multirange.
#[test]
fn a_composite_attribute_may_name_a_multirange() {
    parse("CREATE TYPE r AS RANGE (subtype = int); CREATE TYPE c AS (x r_multirange);")
        .expect("the attribute names the multirange");
}

/// A qualified name of a shell is refused, as a bare one is.
#[test]
fn a_qualified_shell_attribute_is_refused() {
    let error = parse(
        "
CREATE SCHEMA ms;
CREATE TYPE ms.sh;
CREATE TYPE c AS (x ms.sh);
",
    )
    .expect_err("the shell types no value");
    assert!(
        matches!(
            error,
            Error::TypeIsOnlyShell { ref type_name, .. } if type_name.as_str() == "ms.sh"
        ),
        "{error:?}"
    );
}

/// A range declares its canonical and subtype difference functions, which
/// the range's stored name re-points when it moves.
#[test]
fn a_range_declares_its_canonical_and_difference_functions() {
    parse(
        "
CREATE TYPE r;
CREATE FUNCTION canon(r) RETURNS r LANGUAGE internal IMMUTABLE AS 'int4in';
CREATE FUNCTION sdiff(r, r) RETURNS boolean LANGUAGE internal IMMUTABLE AS 'int4eq';
CREATE TYPE r AS RANGE (canonical = canon, subtype_diff = sdiff, subtype = int);
",
    )
    .expect("the range is stored with its functions");
}

/// A range's subtype difference function may be one `pg_catalog` provides,
/// which the range then holds in place.
#[test]
fn a_range_holds_a_provided_difference_function() {
    let error = parse(
        "
CREATE TYPE floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
DROP FUNCTION pg_catalog.float8mi(double precision, double precision);
",
    )
    .expect_err("the range references the function");
    assert!(matches!(error, Error::FunctionReferenced { .. }), "{error:?}");
}

/// A rename may not take a name a domain holds.
#[test]
fn a_rename_onto_a_domain_name_is_refused() {
    let error = parse(
        "
CREATE DOMAIN d AS int;
CREATE TYPE c AS (a int);
ALTER TYPE c RENAME TO d;
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

/// ALTER TYPE names a domain and renames it, as PostgreSQL does.
#[test]
fn an_alter_type_renames_the_domain_it_names() {
    let database =
        parse("CREATE DOMAIN d AS int; ALTER TYPE d RENAME TO d2;").expect("the domain is renamed");
    let names: Vec<_> = database.domains().map(|domain| domain.name().to_string()).collect();
    assert_eq!(names, ["d2"]);
}

/// A qualified rename re-points the qualified references it reaches, and
/// leaves the same name in another schema alone.
#[test]
fn a_qualified_rename_repoints_the_references_it_reaches() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE SCHEMA s2;
CREATE TYPE s.t AS ENUM ('a');
CREATE TYPE s2.t AS ENUM ('b');
CREATE TYPE u AS ENUM ('c');
CREATE TYPE span AS RANGE (subtype = int);
CREATE TABLE docs (e u, c s.t, d s2.t, f int);
ALTER TYPE s.t RENAME TO t2;
",
    )
    .expect("the rename is applied");
    let table = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists");
    assert_eq!(custom_name(&column_type(table, &database, "c")), "s.t2");
    assert_eq!(custom_name(&column_type(table, &database, "d")), "s2.t");
    assert_eq!(custom_name(&column_type(table, &database, "e")), "u");
}

/// A rename re-points the columns and the domains declared over the type,
/// and skips a shell that holds the name.
#[test]
fn a_rename_repoints_the_columns_and_domains_over_it() {
    let database = parse(
        "
CREATE TYPE sh;
CREATE TYPE t AS ENUM ('a');
CREATE TABLE docs (c t);
CREATE DOMAIN d AS t;
ALTER TYPE t RENAME TO t2;
",
    )
    .expect("the rename is applied");
    let table = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists");
    assert_eq!(custom_name(&column_type(table, &database, "c")), "t2");
    assert_eq!(custom_name(&domain_type(&database, "d")), "t2");
}

/// A range's subtype over a bare name holds the type of the default schema,
/// not one of the same name in another schema.
#[test]
fn a_range_subtype_over_a_bare_name_holds_the_default_schema_type() {
    let database = parse(
        "
CREATE SCHEMA other;
CREATE TYPE other.e AS ENUM ('a');
CREATE TYPE e AS ENUM ('b');
CREATE TYPE r AS RANGE (subtype = e);
ALTER TYPE e RENAME TO e2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&range_subtype(&database, "r")), "e2");
}

/// A domain over a bare name holds the type of the default schema, not one of
/// the same name in another schema.
#[test]
fn a_domain_over_a_bare_name_holds_the_default_schema_type() {
    let error = parse(
        "
CREATE SCHEMA other;
CREATE TYPE other.e AS ENUM ('a');
CREATE TYPE e AS ENUM ('b');
CREATE DOMAIN d AS e;
DROP TYPE e;
",
    )
    .expect_err("the domain holds the type of the default schema");
    assert!(
        matches!(error, Error::TypeHasDependents { dependent_kind: ObjectKind::Domain, .. }),
        "{error:?}"
    );
}

/// A rename re-points the subtype a range declares over the type.
#[test]
fn a_rename_repoints_the_subtype_of_a_range_over_it() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE r AS RANGE (subtype = e);
ALTER TYPE e RENAME TO e2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&range_subtype(&database, "r")), "e2");
}

/// A rename re-points the element a base type declares over the type, and
/// leaves a built-in element as written.
#[test]
fn a_rename_repoints_the_element_of_a_base_type_over_it() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE t (element = e);
CREATE TYPE u (element = float4);
ALTER TYPE e RENAME TO e2;
",
    )
    .expect("the rename is applied");
    let element = user_type(&database, "t")
        .expect("the type is stored")
        .element()
        .expect("the type holds its element");
    assert_eq!(custom_name(element), "e2");
    let builtin = user_type(&database, "u").expect("the type is stored").element();
    assert!(matches!(builtin, Some(DataType::Float4)), "a built-in element is left as written");
}

/// A type or domain named twice in one drop is dropped once.
#[test]
fn a_dropped_object_named_twice_goes_once() {
    parse(
        "
CREATE TYPE t AS ENUM ('a');
DROP TYPE t, public.t;
",
    )
    .expect("the doubled name is dropped once");
    parse(
        "
CREATE DOMAIN d AS int;
DROP TYPE d, public.d;
",
    )
    .expect("the domain is dropped under its name");
}

/// A dropped schema takes its types with it when the drop cascades.
#[test]
fn a_cascading_schema_drop_takes_its_types() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.t AS ENUM ('a');
DROP SCHEMA s CASCADE;
",
    )
    .expect("the cascade drops the type");
    assert!(database.user_defined_types().count() == 0, "the type is gone with the schema");
}

/// A type a composite attribute uses is refused without CASCADE.
#[test]
fn a_type_a_composite_attribute_uses_is_refused_without_cascade() {
    let error = parse(
        "
CREATE TYPE t AS ENUM ('a');
CREATE TYPE c AS (x t, y int);
DROP TYPE t;
",
    )
    .expect_err("the attribute uses the type");
    assert!(
        matches!(
            error,
            Error::TypeHasDependents {
                dependent_kind: ObjectKind::Column,
                ref dependent_name,
                ..
            } if dependent_name.as_str() == "c.x"
        ),
        "{error:?}"
    );
}

/// A cascading drop keeps the composite with the attributes it does not
/// take.
#[test]
fn a_cascading_drop_keeps_the_composite_with_its_other_attributes() {
    let database = parse(
        "
CREATE TYPE t AS ENUM ('a');
CREATE TYPE u AS ENUM ('b');
CREATE TYPE c AS (x t, y int);
DROP TYPE t CASCADE;
",
    )
    .expect("the cascade takes the attribute");
    let composite = user_type(&database, "c").expect("the composite stays");
    let attributes = composite.composite_attributes().expect("the composite keeps an attribute");
    assert_eq!(
        attributes.iter().map(|attribute| attribute.name.value.as_str()).collect::<Vec<_>>(),
        ["y"]
    );
}

/// The qualified array name of a stored type is refused, as its bare one is.
#[test]
fn a_qualified_array_name_of_a_stored_type_is_refused() {
    let error = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.t AS ENUM ('a');
DROP TYPE s._t;
",
    )
    .expect_err("the type requires its array");
    assert!(
        matches!(
            error,
            Error::TypeDropRequiredBy { ref type_name, .. } if type_name.as_str() == "t[]"
        ),
        "{error:?}"
    );
}

/// A value written AFTER a label lands after it, in order.
#[test]
fn an_added_value_after_a_label_is_stored_in_order() {
    let database = parse(
        "
CREATE TYPE mood AS ENUM ('a', 'c');
ALTER TYPE mood ADD VALUE 'b' AFTER 'a';
",
    )
    .expect("the value is added");
    assert_eq!(enum_labels(&database, "mood"), ["a", "b", "c"]);
}

/// A held value is skipped with IF NOT EXISTS before its position is
/// checked.
#[test]
fn a_held_value_is_skipped_before_an_unknown_neighbor() {
    parse(
        "
CREATE TYPE mood AS ENUM ('a');
ALTER TYPE mood ADD VALUE IF NOT EXISTS 'a' AFTER 'nope';
",
    )
    .expect("the held label is skipped, not refused for its position");
}

/// An ADD VALUE on a shell is refused, as PostgreSQL refuses it.
#[test]
fn an_add_value_on_a_shell_is_refused() {
    let error = parse(
        "
CREATE TYPE sh;
ALTER TYPE sh ADD VALUE 'x';
",
    )
    .expect_err("the shell is only a name");
    assert!(
        matches!(
            error,
            Error::TypeIsOnlyShell { ref type_name, .. } if type_name.as_str() == "sh"
        ),
        "{error:?}"
    );
}

/// A RENAME VALUE of a domain is refused, as PostgreSQL refuses it.
#[test]
fn a_rename_value_of_a_domain_is_refused() {
    let error = parse(
        "
CREATE DOMAIN d AS int;
ALTER TYPE d RENAME VALUE 'a' TO 'b';
",
    )
    .expect_err("a domain is no enum");
    assert!(
        matches!(
            error,
            Error::NotAnEnum { ref type_name, .. } if type_name.as_str() == "d"
        ),
        "{error:?}"
    );
}

/// A function a type's options name is respelled when it moves, as
/// PostgreSQL respells it.
#[test]
fn a_moved_function_a_type_declares_is_respelled() {
    let database = parse(
        "
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION t_out(t) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE t (input = t_in, output = t_out);
ALTER FUNCTION t_in(cstring) RENAME TO t_ing;
",
    )
    .expect("the rename is applied");
    assert_eq!(
        name_only(input_name(user_type(&database, "t").expect("the type is stored"))),
        "t_ing"
    );
}

/// A replaced function is re-pointed in the types that declare it.
#[test]
fn a_replaced_function_is_repointed_in_the_types_that_declare_it() {
    parse(
        "
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in);
CREATE OR REPLACE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
",
    )
    .expect("the replacement is applied");
}

/// A bare name of a type in a searched schema is written qualified in the
/// column it declares.
#[test]
fn a_bare_reference_of_a_searched_schema_is_written_qualified() {
    let database = parse(
        "
CREATE SCHEMA ms;
CREATE TYPE ms.t AS ENUM ('a');
SET search_path TO ms, public;
CREATE TABLE docs (c t);
",
    )
    .expect("the column is bound to the searched type");
    let table =
        database.tables().find(|table| table.table_name() == "docs").expect("the table is stored");
    assert_eq!(custom_name(&column_type(table, &database, "c")), "ms.t");
}

/// A table may not be renamed onto a name a domain holds.
#[test]
fn a_table_rename_onto_a_domain_is_refused() {
    let error = parse(
        "
CREATE DOMAIN d AS int;
CREATE TABLE rt (a int);
ALTER TABLE rt RENAME TO d;
",
    )
    .expect_err("the domain holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Table,
                conflicting_kind: ObjectKind::Domain,
                ..
            }
        ),
        "{error:?}"
    );
}

/// A schema rename re-points the columns of the types it moves.
#[test]
fn a_schema_rename_repoints_the_columns_of_its_types() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.t AS ENUM ('a');
CREATE TABLE docs (c s.t, d int);
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    let table = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists");
    assert_eq!(custom_name(&column_type(table, &database, "c")), "s2.t");
}

/// A rename of the default schema keeps its types reachable under their
/// bare names.
#[test]
fn a_rename_of_the_default_schema_keeps_its_types_reachable() {
    let database = parse(
        "
CREATE TYPE p AS ENUM ('a');
CREATE TYPE pr AS RANGE (subtype = int);
ALTER SCHEMA public RENAME TO p2;
",
    )
    .expect("the rename is applied");
    assert!(user_type(&database, "p").is_some(), "the enum stays reachable");
    assert!(user_type(&database, "pr").is_some(), "the range stays reachable");
}

/// A rename re-points a column of a type the dialect builds at run time.
#[test]
fn a_rename_keeps_a_column_whose_type_the_dialect_builds() {
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TYPE mood AS ENUM ('a'); CREATE TABLE docs (c mood, d IDENTIFIER('x')); ALTER TYPE mood RENAME TO mood2;",
    )
    .expect("the rename is applied");
    let table = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists");
    assert_eq!(custom_name(&column_type(table, &database, "c")), "mood2");
}

/// An enum may not take a name a relation holds in the schema.
#[test]
fn an_enum_may_not_take_a_relation_name() {
    let error = parse(
        "
CREATE TABLE tt (a int);
CREATE TYPE tt AS ENUM ('a');
",
    )
    .expect_err("the table holds the name");
    assert!(
        matches!(
            error,
            Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Table,
                ..
            }
        ),
        "{error:?}"
    );
}

/// A range's canonical and difference functions are respelled when they move.
#[test]
fn a_moved_function_a_range_declares_is_respelled() {
    let database = parse(
        "
CREATE TYPE r;
CREATE FUNCTION canon(r) RETURNS r LANGUAGE internal IMMUTABLE AS 'int4in';
CREATE FUNCTION sdiff(r, r) RETURNS boolean LANGUAGE internal IMMUTABLE AS 'int4eq';
CREATE TYPE r AS RANGE (canonical = canon, subtype_diff = sdiff, subtype = int);
ALTER FUNCTION canon(r) RENAME TO canon2;
",
    )
    .expect("the rename is applied");
    let UserDefinedTypeRepresentation::Range { options } = user_type(&database, "r")
        .expect("the range is stored")
        .representation()
        .expect("the range holds its options")
    else {
        panic!("the type is a range");
    };
    let canonical = options
        .iter()
        .find_map(|option| {
            match option {
                UserDefinedTypeRangeOption::Canonical(name) => Some(name),
                _ => None,
            }
        })
        .expect("the range declares its canonical");
    assert_eq!(name_only(canonical), "canon2");
}

/// A function a base type declares is respelled when it moves, an element
/// the type declares left as written.
#[test]
fn a_moved_function_keeps_the_element_declared_by_the_type() {
    let database = parse(
        "
CREATE TYPE e AS ENUM ('a');
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in, element = e);
ALTER FUNCTION t_in(cstring) RENAME TO t_ing;
",
    )
    .expect("the rename is applied");
    let type_ = user_type(&database, "t").expect("the type is stored");
    assert_eq!(name_only(input_name(type_)), "t_ing");
    let element = type_.element().expect("the type holds its element");
    assert_eq!(custom_name(element), "e");
}

/// A schema rename re-points the reference a type outside the moved schema
/// holds onto one of its types.
#[test]
fn a_schema_rename_repoints_a_reference_holding_outside_the_moved_schema() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.e AS ENUM ('a');
CREATE TYPE c AS (x s.e, y int);
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&composite_attribute(&database, "c").data_type), "s2.e");
}

/// A `DROP DOMAIN` of a user type is refused, as PostgreSQL refuses it.
#[test]
fn a_drop_domain_of_a_user_type_is_refused() {
    let error = parse(
        "
CREATE TYPE mood AS ENUM ('a');
DROP DOMAIN mood;
",
    )
    .expect_err("a type is no domain");
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

/// An `ALTER TYPE` naming a table's row type is refused, as PostgreSQL refuses
/// it.
#[test]
fn an_alter_type_of_a_row_type_is_refused() {
    let error = parse(
        "
CREATE TABLE rt (a int);
ALTER TYPE rt RENAME TO rt2;
",
    )
    .expect_err("a row type is not altered");
    assert!(
        matches!(error, Error::TypeIsRowType { relation_kind: ObjectKind::Table, .. }),
        "{error:?}"
    );
}

/// A rename re-points a composite's typed attribute and leaves its built-in
/// one as written.
#[test]
fn a_rename_keeps_a_composite_with_its_builtin_attributes() {
    let database = parse(
        "
CREATE TYPE t AS ENUM ('a');
CREATE TYPE c AS (x t, y int);
ALTER TYPE t RENAME TO t2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&composite_attribute(&database, "c").data_type), "t2");
}

/// A schema rename re-points the subtype a range outside the moved schema
/// declares over one of its types.
#[test]
fn a_schema_rename_repoints_the_subtype_of_a_range_outside_the_moved_schema() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.e AS ENUM ('a');
CREATE TYPE r AS RANGE (subtype = s.e);
CREATE TYPE span AS RANGE (subtype = int);
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    assert_eq!(custom_name(&range_subtype(&database, "r")), "s2.e");
}

/// A schema rename re-points the element a base type outside the moved schema
/// declares, and leaves its built-in and function options as written.
#[test]
fn a_schema_rename_repoints_the_element_of_a_base_type_outside_the_moved_schema() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.e AS ENUM ('a');
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in, element = s.e);
CREATE DOMAIN d AS int;
CREATE TYPE u (input = t_in, element = float4);
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    let element = user_type(&database, "t")
        .expect("the type is stored")
        .element()
        .expect("the type holds its element");
    assert_eq!(custom_name(element), "s2.e");
    assert!(
        matches!(domain_type(&database, "d"), DataType::Int(..)),
        "the domain over a built-in is left as written"
    );
    let builtin = user_type(&database, "u").expect("the type is stored").element();
    assert!(matches!(builtin, Some(DataType::Float4)), "a built-in element is left as written");
}

/// A schema rename re-points a shell it moves, which holds no representation.
#[test]
fn a_schema_rename_repoints_a_shell_it_moves() {
    let database = parse(
        "
CREATE SCHEMA s;
CREATE TYPE s.sh;
ALTER SCHEMA s RENAME TO s2;
",
    )
    .expect("the rename is applied");
    let type_ = database
        .user_defined_types()
        .find(|type_| type_.name() == "sh")
        .expect("the shell is stored");
    assert_eq!(type_.schema(), Some("s2"));
}

/// A `RENAME VALUE` of a type that is no enum is refused, as PostgreSQL
/// refuses it.
#[test]
fn a_rename_value_of_a_non_enum_is_refused() {
    let error = parse(
        "
CREATE TYPE t;
CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE TYPE t (input = t_in);
ALTER TYPE t RENAME VALUE 'a' TO 'b';
",
    )
    .expect_err("the type is no enum");
    assert!(matches!(error, Error::NotAnEnum { .. }), "{error:?}");
}

/// A drop proceeds past a surviving type whose element names no stored type,
/// since an overqualified name reaches none.
#[test]
fn a_drop_proceeds_past_an_overqualified_element_of_a_surviving_type() {
    parse(
        "
CREATE TYPE other;
CREATE TYPE t (element = x.y.z);
DROP TYPE other;
",
    )
    .expect("the element names no stored type");
}
