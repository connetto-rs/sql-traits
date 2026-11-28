//! Spells the type an expression context answers for a node.

use sql_traits::prelude::*;

/// How a test spells the type an expression takes.
pub(crate) fn rendered(operand: &OperandType<'_>) -> String {
    match operand {
        OperandType::Known { type_name, typmod: Some(typmod) } => format!("{type_name}({typmod})"),
        OperandType::Known { type_name, typmod: None } => type_name.to_string(),
        OperandType::UntypedLiteral => "unknown literal".to_owned(),
        OperandType::Unknown => "?".to_owned(),
        OperandType::Sqlite { .. } => "sqlite".to_owned(),
    }
}
