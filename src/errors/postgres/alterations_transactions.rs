//! Index alterations PostgreSQL refuses, and transaction commands it refuses
//! outside a transaction.

use super::{PostgresError, pg, quoted};
use crate::errors::ReplicaIdentityRefusal;

/// The refusal of `ALTER TABLE ... REPLICA IDENTITY USING INDEX` naming the
/// index `index` of the table, unfit for `reason`.
pub(super) fn replica_identity_refused(
    index: &str,
    reason: &ReplicaIdentityRefusal,
) -> PostgresError {
    let index = quoted(index);
    match reason {
        ReplicaIdentityRefusal::NotUnique => {
            PostgresError::new(
                pg::WRONG_OBJECT_TYPE,
                format!("cannot use non-unique index {index} as replica identity"),
            )
        }
        ReplicaIdentityRefusal::NotImmediate => {
            PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                format!("cannot use non-immediate index {index} as replica identity"),
            )
        }
        ReplicaIdentityRefusal::Expression => {
            PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                format!("cannot use expression index {index} as replica identity"),
            )
        }
        ReplicaIdentityRefusal::Partial => {
            PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                format!("cannot use partial index {index} as replica identity"),
            )
        }
        ReplicaIdentityRefusal::NullableColumn(column) => {
            PostgresError::new(
                pg::WRONG_OBJECT_TYPE,
                format!(
                    "index {index} cannot be used as replica identity because column {} is nullable",
                    quoted(column)
                ),
            )
        }
        ReplicaIdentityRefusal::UnvalidatedNotNull(_) => {
            PostgresError::new(
                pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot use index {index} as replica identity"),
            )
        }
    }
}

/// The refusal for commands that require an open transaction in PostgreSQL.
pub(super) fn no_transaction(statement: &str) -> Option<PostgresError> {
    let command = if statement.starts_with("SAVEPOINT ") {
        "SAVEPOINT"
    } else if statement.starts_with("RELEASE ") {
        "RELEASE SAVEPOINT"
    } else if statement.starts_with("ROLLBACK TO ") {
        "ROLLBACK TO SAVEPOINT"
    } else if statement == "COMMIT AND CHAIN" || statement == "END AND CHAIN" {
        "COMMIT AND CHAIN"
    } else if statement == "ROLLBACK AND CHAIN" {
        "ROLLBACK AND CHAIN"
    } else {
        return None;
    };
    Some(PostgresError::new(
        pg::NO_ACTIVE_SQL_TRANSACTION,
        format!("{command} can only be used in transaction blocks"),
    ))
}
