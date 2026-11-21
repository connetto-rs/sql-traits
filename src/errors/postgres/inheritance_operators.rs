use alloc::string::String;

use super::{PostgresError, pg, quoted};
use crate::errors::{
    GenerationExpressionRefusal, InheritedChange, InheritedColumnConflict,
    InheritedConstraintConflictKind, OperatorAttribute,
};

pub(super) fn only(change: InheritedChange, subject: Option<&str>) -> PostgresError {
    let message = match change {
        InheritedChange::DropExpression => {
            return PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / DROP EXPRESSION must be applied to child tables too",
            );
        }
        InheritedChange::AddConstraint | InheritedChange::SetNotNull => {
            "constraint must be added to child tables too".into()
        }
        InheritedChange::AddColumn => "column must be added to child tables too".into(),
        InheritedChange::RenameColumn => {
            format!(
                "inherited column {} must be renamed in child tables too",
                quoted(subject.unwrap_or_default())
            )
        }
        InheritedChange::RenameConstraint => {
            format!(
                "inherited constraint {} must be renamed in child tables too",
                quoted(subject.unwrap_or_default())
            )
        }
        InheritedChange::ValidateConstraint => {
            "constraint must be validated on child tables too".into()
        }
        InheritedChange::AlterColumnType => {
            format!(
                "type of inherited column {} must be changed in child tables too",
                quoted(subject.unwrap_or_default())
            )
        }
        InheritedChange::DropColumn => {
            "cannot drop column from only the partitioned table when partitions exist".into()
        }
    };
    PostgresError::new(pg::INVALID_TABLE_DEFINITION, message)
}

/// The refusal of `ALTER COLUMN ... DROP EXPRESSION` on the column `column` of
/// the table `table`, which keeps its expression for `reason`.
pub(super) fn generation_drop(
    reason: GenerationExpressionRefusal,
    column: &str,
    table: &str,
) -> PostgresError {
    match reason {
        GenerationExpressionRefusal::NotGenerated => {
            PostgresError::new(
                pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "column {} of relation {} is not a generated column",
                    quoted(column),
                    quoted(table)
                ),
            )
        }
        GenerationExpressionRefusal::Virtual => {
            PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns",
            )
        }
        GenerationExpressionRefusal::Inherited => {
            PostgresError::new(
                pg::INVALID_TABLE_DEFINITION,
                "cannot drop generation expression from inherited column",
            )
        }
    }
}

pub(super) fn column_conflict(
    source: InheritedColumnConflict,
    column: &str,
    child: &str,
    kind: &str,
) -> String {
    match source {
        InheritedColumnConflict::Declaration => {
            format!("column {} has a {kind} conflict", quoted(column))
        }
        InheritedColumnConflict::Parents => {
            format!("inherited column {} has a {kind} conflict", quoted(column))
        }
        InheritedColumnConflict::Addition => {
            format!(
                "child table {} has different {kind} for column {}",
                quoted(child),
                quoted(column)
            )
        }
    }
}

pub(super) fn constraint_conflict(
    reason: &InheritedConstraintConflictKind,
    constraint: &str,
    table: &str,
) -> Option<PostgresError> {
    let (state, message) = match reason {
        InheritedConstraintConflictKind::NotNullNameReuse => return None,
        InheritedConstraintConflictKind::CatalogName => (pg::UNIQUE_VIOLATION, "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"".into()),
        InheritedConstraintConflictKind::NotNullStatus => (pg::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot change NO INHERIT status of NOT NULL constraint {} on relation {}", quoted(constraint), quoted(table))),
        InheritedConstraintConflictKind::Parents => (pg::DUPLICATE_OBJECT, format!("check constraint name {} appears multiple times but with different expressions", quoted(constraint))),
        InheritedConstraintConflictKind::NotNullNoInherit { column_name } => (pg::DATATYPE_MISMATCH, format!("cannot define not-null constraint with NO INHERIT on column {}", quoted(column_name))),
        InheritedConstraintConflictKind::Definition => (pg::DUPLICATE_OBJECT, format!("constraint {} for relation {} already exists", quoted(constraint), quoted(table))),
        InheritedConstraintConflictKind::InheritedNoInherit => (pg::INVALID_OBJECT_DEFINITION, format!("constraint {} conflicts with inherited constraint on relation {}", quoted(constraint), quoted(table))),
        InheritedConstraintConflictKind::LocalNoInherit => (pg::INVALID_OBJECT_DEFINITION, format!("constraint {} conflicts with non-inherited constraint on relation {}", quoted(constraint), quoted(table))),
    };
    Some(PostgresError::new(state, message))
}

pub(super) const fn attribute_requirement(attribute: OperatorAttribute) -> &'static str {
    match attribute {
        OperatorAttribute::Commutator => "have commutators",
        OperatorAttribute::Negator => "have negators",
        OperatorAttribute::Hashes => "hash",
        OperatorAttribute::Merges => "merge join",
        OperatorAttribute::Restrict => "have restriction selectivity",
        OperatorAttribute::Join => "have join selectivity",
    }
}
