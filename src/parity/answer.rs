//! An engine's answer for one row, and how it differs from PostgreSQL's.

use alloc::string::String;
use core::fmt;

use serde::{Deserialize, Serialize};

/// An engine's answer for one row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// A read returns the row.
    Visible,
    /// A read leaves the row out.
    Hidden,
    /// A write changed the row, or `OpenFGA` permits the write.
    Changed,
    /// A write ran and left the row alone because no policy let it reach the
    /// row.
    Skipped,
    /// A write aborted with this SQLSTATE.
    Rejected(String),
    /// `OpenFGA` refuses the write, which agrees with a skipped or a rejected
    /// row.
    Denied,
    /// The translator refused the schema or the statement, for this reason.
    Refused(String),
    /// The statement failed with this message.
    Error(String),
}

/// The translated engine being compared.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// pg2sqlite's SQLite replica.
    Sqlite,
    /// rls2fga's `OpenFGA` model.
    Openfga,
}

impl fmt::Display for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Sqlite => "sqlite",
            Self::Openfga => "openfga",
        })
    }
}

/// The way an answer differs from PostgreSQL.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The engine shows or changes a row PostgreSQL does not.
    Wider,
    /// The engine hides or spares a row PostgreSQL shows or changes.
    Narrower,
    /// Both refuse the write, one by skipping the row and the other by
    /// aborting, or with different SQLSTATEs.
    DifferentDenial,
    /// The translator refused what PostgreSQL ran.
    Refused,
    /// The engine failed, or answered in a kind PostgreSQL's answer has no
    /// counterpart in.
    Error,
}

/// Classify an engine answer against PostgreSQL's. An engine error or refusal
/// carrying PostgreSQL's own error agrees with it, and any other error is a
/// discrepancy.
#[must_use]
pub fn direction(oracle: &Answer, actual: &Answer) -> Option<Direction> {
    let agrees = match (oracle, actual) {
        (Answer::Error(expected), Answer::Error(message) | Answer::Refused(message)) => {
            expected == message
        }
        (Answer::Error(_), _) => false,
        _ => oracle == actual,
    };
    if agrees {
        return None;
    }
    Some(match (oracle, actual) {
        (_, Answer::Refused(_)) => Direction::Refused,
        (Answer::Skipped | Answer::Rejected(_), Answer::Denied) => return None,
        (Answer::Visible, Answer::Hidden)
        | (Answer::Changed, Answer::Skipped | Answer::Rejected(_) | Answer::Denied) => {
            Direction::Narrower
        }
        (Answer::Hidden, Answer::Visible)
        | (Answer::Skipped | Answer::Rejected(_), Answer::Changed) => Direction::Wider,
        (Answer::Skipped | Answer::Rejected(_), Answer::Skipped | Answer::Rejected(_)) => {
            Direction::DifferentDenial
        }
        _ => Direction::Error,
    })
}
