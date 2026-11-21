use super::{PostgresError, pg, quoted};
use crate::{
    errors::{ChildMismatch, FixedPersistence, InvalidBound, UnattachablePartition},
    traits::PartitionStrategy,
};

pub(super) fn persistence(
    table_name: &str,
    reason: FixedPersistence,
    unlogged: bool,
) -> PostgresError {
    match reason {
        FixedPersistence::Temporary => {
            PostgresError::new(
                pg::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change logged status of table {} because it is temporary",
                    quoted(table_name)
                ),
            )
        }
        FixedPersistence::Partitioned => {
            PostgresError::new(
                pg::WRONG_OBJECT_TYPE,
                format!(
                    "ALTER action SET {} cannot be performed on relation {}",
                    if unlogged { "UNLOGGED" } else { "LOGGED" },
                    quoted(table_name)
                ),
            )
        }
    }
}

pub(super) fn shape(table_name: &str, reason: &ChildMismatch) -> PostgresError {
    let (sqlstate, message) = match reason {
        ChildMismatch::MissingColumn { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!("child table is missing column {}", quoted(column_name)),
            )
        }
        ChildMismatch::ColumnType { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "child table {} has different type for column {}",
                    quoted(table_name),
                    quoted(column_name)
                ),
            )
        }
        ChildMismatch::ColumnCollation { column_name } => {
            (
                pg::COLLATION_MISMATCH,
                format!(
                    "child table {} has different collation for column {}",
                    quoted(table_name),
                    quoted(column_name)
                ),
            )
        }
        ChildMismatch::ColumnNullable { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "column {} in child table {} must be marked NOT NULL",
                    quoted(column_name),
                    quoted(table_name)
                ),
            )
        }
        ChildMismatch::MustBeGenerated { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!("column {} in child table must be a generated column", quoted(column_name)),
            )
        }
        ChildMismatch::MustNotBeGenerated { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "column {} in child table must not be a generated column",
                    quoted(column_name)
                ),
            )
        }
        ChildMismatch::MissingConstraint { constraint_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!("child table is missing constraint {}", quoted(constraint_name)),
            )
        }
        ChildMismatch::ConstraintDiffers { constraint_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "child table {} has different definition for check constraint {}",
                    quoted(table_name),
                    quoted(constraint_name)
                ),
            )
        }
        ChildMismatch::ConstraintNotInherited { constraint_name } => {
            (
                pg::INVALID_OBJECT_DEFINITION,
                format!(
                    "constraint {} conflicts with non-inherited constraint on child table {}",
                    quoted(constraint_name),
                    quoted(table_name)
                ),
            )
        }
        ChildMismatch::ConstraintNotValid { constraint_name } => {
            (
                pg::INVALID_OBJECT_DEFINITION,
                format!(
                    "constraint {} conflicts with NOT VALID constraint on child table {}",
                    quoted(constraint_name),
                    quoted(table_name)
                ),
            )
        }
    };
    PostgresError::new(sqlstate, message)
}

pub(super) fn bound(partition_name: &str, reason: &InvalidBound) -> PostgresError {
    let (sqlstate, message) = match reason {
        InvalidBound::StrategyMismatch { strategy } => return strategy_mismatch(*strategy),
        InvalidBound::ValueCount { upper, .. } => {
            (
                pg::INVALID_TABLE_DEFINITION,
                format!(
                    "{} must specify exactly one value per partitioning column",
                    if *upper { "TO" } else { "FROM" }
                ),
            )
        }
        InvalidBound::NullInRange => {
            (pg::INVALID_OBJECT_DEFINITION, "cannot specify NULL in range bound".into())
        }
        InvalidBound::ValueAfterInfinity { maximum } => {
            let infinity = if *maximum { "MAXVALUE" } else { "MINVALUE" };
            (
                pg::DATATYPE_MISMATCH,
                format!("every bound following {infinity} must also be {infinity}"),
            )
        }
        InvalidBound::ColumnReference => {
            (
                pg::FEATURE_NOT_SUPPORTED,
                "cannot use column reference in partition bound expression".into(),
            )
        }
        InvalidBound::DefaultOnHash => {
            (
                pg::INVALID_TABLE_DEFINITION,
                "a hash-partitioned table may not have a default partition".into(),
            )
        }
        InvalidBound::DefaultExists { default_partition } => {
            (
                pg::INVALID_OBJECT_DEFINITION,
                format!(
                    "partition {} conflicts with existing default partition {}",
                    quoted(partition_name),
                    quoted(default_partition)
                ),
            )
        }
        InvalidBound::ModulusNotPositive => {
            (
                pg::INVALID_TABLE_DEFINITION,
                "modulus for hash partition must be an integer value greater than zero".into(),
            )
        }
        InvalidBound::RemainderNotBelowModulus => {
            (
                pg::INVALID_TABLE_DEFINITION,
                "remainder for hash partition must be less than modulus".into(),
            )
        }
        InvalidBound::ModulusNotFactor { .. } => {
            (
                pg::INVALID_OBJECT_DEFINITION,
                "every hash partition modulus must be a factor of the next larger modulus".into(),
            )
        }
        InvalidBound::EmptyRange => {
            (
                pg::INVALID_OBJECT_DEFINITION,
                format!("empty range bound specified for partition {}", quoted(partition_name)),
            )
        }
        InvalidBound::ValueNotOfKeyType { value, type_name } => {
            (
                pg::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type {type_name}: {}", quoted(value)),
            )
        }
        InvalidBound::ValueOutOfRange { value, type_name, quoted: is_quoted } => {
            return out_of_range(value, type_name, *is_quoted);
        }
        InvalidBound::DateTimeOutOfRange { value } => {
            (
                pg::DATETIME_FIELD_OVERFLOW,
                format!("date/time field value out of range: {}", quoted(value)),
            )
        }
        InvalidBound::ValueCannotCast { type_name, column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "specified value cannot be cast to type {type_name} for column {}",
                    quoted(column_name)
                ),
            )
        }
    };
    PostgresError::new(sqlstate, message)
}

fn strategy_mismatch(strategy: PartitionStrategy) -> PostgresError {
    let strategy = match strategy {
        PartitionStrategy::Range => "range",
        PartitionStrategy::List => "list",
        PartitionStrategy::Hash => "hash",
    };
    PostgresError::new(
        pg::INVALID_TABLE_DEFINITION,
        format!("invalid bound specification for a {strategy} partition"),
    )
}

fn out_of_range(value: &str, type_name: &str, is_quoted: bool) -> PostgresError {
    PostgresError::new(
        pg::NUMERIC_VALUE_OUT_OF_RANGE,
        if is_quoted {
            format!("value {} is out of range for type {type_name}", quoted(value))
        } else {
            format!("{type_name} out of range")
        },
    )
}

pub(super) fn attachment(
    table_name: &str,
    partition_name: &str,
    reason: &UnattachablePartition,
) -> PostgresError {
    let (sqlstate, message) = match reason {
        UnattachablePartition::AlreadyPartition => {
            (pg::WRONG_OBJECT_TYPE, format!("{} is already a partition", quoted(partition_name)))
        }
        UnattachablePartition::InheritanceChild => {
            (pg::WRONG_OBJECT_TYPE, "cannot attach inheritance child as partition".into())
        }
        UnattachablePartition::InheritanceParent => {
            (pg::WRONG_OBJECT_TYPE, "cannot attach inheritance parent as partition".into())
        }
        UnattachablePartition::Circular => {
            (pg::DUPLICATE_TABLE, "circular inheritance not allowed".into())
        }
        UnattachablePartition::ExtraColumn { column_name } => {
            (
                pg::DATATYPE_MISMATCH,
                format!(
                    "table {} contains column {} not found in parent {}",
                    quoted(partition_name),
                    quoted(column_name),
                    quoted(table_name)
                ),
            )
        }
        UnattachablePartition::IdentityColumn { column_name } => {
            (
                pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "table {} being attached contains an identity column {}",
                    quoted(partition_name),
                    quoted(column_name)
                ),
            )
        }
        UnattachablePartition::Shape(reason) => return shape(partition_name, reason),
        UnattachablePartition::ForeignTableUnderUniqueIndex => {
            (
                pg::WRONG_OBJECT_TYPE,
                format!(
                    "cannot attach foreign table {} as partition of partitioned table {}",
                    quoted(partition_name),
                    quoted(table_name)
                ),
            )
        }
    };
    PostgresError::new(sqlstate, message)
}
