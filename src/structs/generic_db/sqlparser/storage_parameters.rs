//! PostgreSQL relation storage parameter definitions and validation.

use alloc::{borrow::Cow, format, string::ToString};

use sqlparser::ast::{
    BinaryOperator, CreateIndex, CreateTable, Expr, Ident, IndexType, ObjectName, ObjectNamePart,
    SqlOption,
};

use super::SqlparserDialect;
use crate::{
    errors::Error, impls::parameter_value, utils::identifier_resolution::stored_ident_name,
};

const HEAP: u16 = 1;
const TOAST: u16 = 2;
const BTREE: u16 = 4;
const HASH: u16 = 8;
const GIST: u16 = 16;
const GIN: u16 = 32;
const BRIN: u16 = 64;
const SPGIST: u16 = 128;

#[derive(Clone, Copy)]
enum ValueType {
    Boolean,
    Integer(i32, i32),
    Real(f64, f64),
    Enum(&'static [&'static str]),
}

const PARAMETERS: &[(&str, u16, ValueType)] = &[
    ("fillfactor", HEAP | BTREE | HASH | GIST | SPGIST, ValueType::Integer(10, 100)),
    ("autovacuum_enabled", HEAP | TOAST, ValueType::Boolean),
    ("user_catalog_table", HEAP, ValueType::Boolean),
    ("vacuum_truncate", HEAP | TOAST, ValueType::Boolean),
    ("autovacuum_vacuum_threshold", HEAP | TOAST, ValueType::Integer(0, i32::MAX)),
    ("autovacuum_vacuum_max_threshold", HEAP | TOAST, ValueType::Integer(-1, i32::MAX)),
    ("autovacuum_vacuum_insert_threshold", HEAP | TOAST, ValueType::Integer(-1, i32::MAX)),
    ("autovacuum_analyze_threshold", HEAP, ValueType::Integer(0, i32::MAX)),
    ("autovacuum_vacuum_cost_limit", HEAP | TOAST, ValueType::Integer(1, 10000)),
    ("autovacuum_freeze_min_age", HEAP | TOAST, ValueType::Integer(0, 1000000000)),
    ("autovacuum_multixact_freeze_min_age", HEAP | TOAST, ValueType::Integer(0, 1000000000)),
    ("autovacuum_freeze_max_age", HEAP | TOAST, ValueType::Integer(100000, 2000000000)),
    ("autovacuum_multixact_freeze_max_age", HEAP | TOAST, ValueType::Integer(10000, 2000000000)),
    ("autovacuum_freeze_table_age", HEAP | TOAST, ValueType::Integer(0, 2000000000)),
    ("autovacuum_multixact_freeze_table_age", HEAP | TOAST, ValueType::Integer(0, 2000000000)),
    ("log_autovacuum_min_duration", HEAP | TOAST, ValueType::Integer(-1, i32::MAX)),
    ("toast_tuple_target", HEAP, ValueType::Integer(128, 8160)),
    ("parallel_workers", HEAP, ValueType::Integer(0, 1024)),
    ("autovacuum_vacuum_cost_delay", HEAP | TOAST, ValueType::Real(0.0, 100.0)),
    ("autovacuum_vacuum_scale_factor", HEAP | TOAST, ValueType::Real(0.0, 100.0)),
    ("autovacuum_vacuum_insert_scale_factor", HEAP | TOAST, ValueType::Real(0.0, 100.0)),
    ("autovacuum_analyze_scale_factor", HEAP, ValueType::Real(0.0, 100.0)),
    ("vacuum_max_eager_freeze_failure_rate", HEAP | TOAST, ValueType::Real(0.0, 1.0)),
    (
        "vacuum_index_cleanup",
        HEAP | TOAST,
        ValueType::Enum(&["auto", "on", "off", "true", "false", "yes", "no", "1", "0"]),
    ),
    ("deduplicate_items", BTREE, ValueType::Boolean),
    ("vacuum_cleanup_index_scale_factor", BTREE, ValueType::Real(0.0, 1e10)),
    ("buffering", GIST, ValueType::Enum(&["auto", "on", "off"])),
    ("fastupdate", GIN, ValueType::Boolean),
    ("gin_pending_list_limit", GIN, ValueType::Integer(64, i32::MAX)),
    ("pages_per_range", BRIN, ValueType::Integer(1, 131072)),
    ("autosummarize", BRIN, ValueType::Boolean),
];

/// Reads decimal and hexadecimal floating spellings with range checking.
fn real(value: &str) -> Option<f64> {
    let text = value.trim_ascii();
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    if unsigned.eq_ignore_ascii_case("inf") || unsigned.eq_ignore_ascii_case("infinity") {
        return Some(if text.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    let number = if let Some(hexadecimal) =
        unsigned.strip_prefix("0x").or_else(|| unsigned.strip_prefix("0X"))
    {
        let (mantissa, exponent) = hexadecimal.split_once(['p', 'P']).map_or(
            Some((hexadecimal, 0)),
            |(mantissa, exponent)| {
                exponent.parse::<i32>().ok().map(|exponent| (mantissa, exponent))
            },
        )?;
        let mut number = 0.0;
        let mut fractional = None;
        let mut digits = 0;
        for character in mantissa.chars() {
            if character == '.' && fractional.is_none() {
                fractional = Some(0_i32);
                continue;
            }
            number = number * 16.0 + f64::from(character.to_digit(16)?);
            digits += 1;
            if let Some(fractional) = &mut fractional {
                *fractional = fractional.checked_add(4)?;
            }
        }
        if digits == 0 {
            return None;
        }
        let mut exponent = exponent.checked_sub(fractional.unwrap_or(0))?;
        if number != 0.0 {
            if !(-2098..=1024).contains(&exponent) {
                return None;
            }
            while exponent != 0 {
                let step = exponent.clamp(-1022, 1023);
                let biased = u64::try_from(step + 1023).ok()?;
                number *= f64::from_bits(biased << 52);
                exponent -= step;
            }
        }
        if text.starts_with('-') { -number } else { number }
    } else {
        text.parse::<f64>().ok()?
    };
    if !number.is_finite() || number.is_subnormal() {
        return None;
    }
    if number == 0.0 {
        let mantissa = unsigned.split(['e', 'E', 'p', 'P']).next()?;
        let mantissa =
            mantissa.strip_prefix("0x").or_else(|| mantissa.strip_prefix("0X")).unwrap_or(mantissa);
        if mantissa.chars().any(|character| character != '0' && character != '.') {
            return None;
        }
    }
    Some(number)
}

/// Reads PostgreSQL's rounded integer spelling without allocating.
#[expect(
    clippy::cast_possible_truncation,
    reason = "The bounded value is deliberately truncated before ties-to-even rounding"
)]
#[expect(
    clippy::float_cmp,
    reason = "Ties-to-even rounding distinguishes exact binary half-integers"
)]
fn integer(value: &str) -> Option<f64> {
    let text = value.trim_ascii();
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let negative = text.starts_with('-');
    if let Some(hexadecimal) = unsigned.strip_prefix("0x").or_else(|| unsigned.strip_prefix("0X"))
        && !hexadecimal.contains('.')
    {
        let number = i32::from_str_radix(hexadecimal, 16).ok()?;
        return Some(f64::from(if negative { -number } else { number }));
    }
    if unsigned.starts_with('0') && !unsigned.contains(['.', 'e', 'E']) {
        let number = i32::from_str_radix(unsigned, 8).ok()?;
        return Some(f64::from(if negative { -number } else { number }));
    }
    let number = real(text)?;
    if !number.is_finite()
        || number < f64::from(i32::MIN) - 0.5
        || number >= f64::from(i32::MAX) + 0.5
    {
        return None;
    }
    // Truncation supplies the integer part for ties-to-even quantization.
    let truncated = number as i32;
    let fraction = number - f64::from(truncated);
    let rounded = f64::from(truncated)
        + if fraction > 0.5 || (fraction == 0.5 && truncated % 2 != 0) {
            1.0
        } else if fraction < -0.5 || (fraction == -0.5 && truncated % 2 != 0) {
            -1.0
        } else {
            0.0
        };
    (rounded >= f64::from(i32::MIN) && rounded <= f64::from(i32::MAX)).then_some(rounded)
}

/// Checks a parameter's value against its type and inclusive bounds.
fn validate_value(name: &str, value: &str, specification: ValueType) -> Result<(), Error> {
    let invalid = |value_type| {
        Error::InvalidStorageParameterValue {
            parameter_name: name.to_string(),
            value_type,
            value: value.to_string(),
        }
    };
    let (number, minimum, maximum, detail) = match specification {
        ValueType::Boolean => {
            if !value.is_empty()
                && ["true", "false", "yes", "no", "on", "off", "1", "0"]
                    .iter()
                    .filter(|spelling| {
                        spelling
                            .get(..value.len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(value))
                    })
                    .count()
                    == 1
            {
                return Ok(());
            }
            return Err(invalid("boolean"));
        }
        ValueType::Enum(values) => {
            return if values.iter().any(|expected| expected.eq_ignore_ascii_case(value)) {
                Ok(())
            } else {
                Err(invalid("enum"))
            };
        }
        ValueType::Integer(minimum, maximum) => {
            let number = integer(value).ok_or_else(|| invalid("integer"))?;
            if number >= f64::from(minimum) && number <= f64::from(maximum) {
                return Ok(());
            }
            (
                number,
                f64::from(minimum),
                f64::from(maximum),
                format!("Valid values are between \"{minimum}\" and \"{maximum}\"."),
            )
        }
        ValueType::Real(minimum, maximum) => {
            let number = real(value).ok_or_else(|| invalid("floating point"))?;
            if number >= minimum && number <= maximum {
                return Ok(());
            }
            (
                number,
                minimum,
                maximum,
                format!("Valid values are between \"{minimum:.6}\" and \"{maximum:.6}\"."),
            )
        }
    };
    debug_assert!(number < minimum || number > maximum);
    Err(Error::StorageParameterOutOfBounds {
        parameter_name: name.to_string(),
        value: value.to_string(),
        detail,
    })
}

/// Validates borrowed name parts and values against one relation kind.
fn validate<'parameter>(
    kind: Option<u16>,
    parameters: impl Iterator<
        Item = Result<
            (Option<&'parameter Ident>, &'parameter Ident, Option<&'parameter Expr>),
            Error,
        >,
    >,
) -> Result<(), Error> {
    let mut seen = [[false; PARAMETERS.len()]; 2];
    for parameter in parameters {
        let (namespace, identifier, value) = parameter?;
        let Some(kind) = kind else {
            continue;
        };
        let namespace = namespace.map(stored_ident_name);
        let selected = match namespace.as_deref() {
            None => kind,
            Some("toast") if kind == HEAP => TOAST,
            Some(namespace) => {
                return Err(Error::UnrecognizedStorageParameterNamespace {
                    namespace: namespace.to_string(),
                });
            }
        };
        let name = stored_ident_name(identifier);
        let Some((position, (_, _, specification))) = PARAMETERS
            .iter()
            .enumerate()
            .find(|(_, (parameter, kinds, _))| *parameter == name && kinds & selected != 0)
        else {
            return Err(Error::UnrecognizedStorageParameter { parameter_name: name.into_owned() });
        };
        let repeated = &mut seen[usize::from(selected == TOAST)][position];
        if *repeated {
            return Err(Error::RepeatedStorageParameter { parameter_name: name.into_owned() });
        }
        *repeated = true;
        let value = value.map_or(Cow::Borrowed("true"), parameter_value);
        validate_value(&name, &value, *specification)?;
    }
    Ok(())
}

/// Checks heap and TOAST options for PostgreSQL ingestion.
pub(super) fn heap(relation_name: &ObjectName, options: &[SqlOption]) -> Result<(), Error> {
    validate(
        Some(HEAP),
        options.iter().map(|option| {
            let invalid = || {
                Error::StorageParameterNotNamed {
                    option: option.to_string(),
                    relation_name: SqlparserDialect::PostgreSql.stored_object_name(relation_name),
                }
            };
            let (key, value) = match option {
                SqlOption::KeyValue { key, value } => (key, Some(value)),
                SqlOption::Ident(key) => (key, None),
                _ => return Err(invalid()),
            };
            match key.0.as_slice() {
                [ObjectNamePart::Identifier(name)] => Ok((None, name, value)),
                [ObjectNamePart::Identifier(namespace), ObjectNamePart::Identifier(name)] => {
                    Ok((Some(namespace), name, value))
                }
                _ => Err(invalid()),
            }
        }),
    )
}

/// Checks a table's storage options before recording it.
pub(super) fn table(table: &CreateTable, options: &[SqlOption]) -> Result<(), Error> {
    if table.partition_by.is_some()
        && options
            .iter()
            .any(|option| super::storage_parameter_key(option).is_some_and(|key| key.0.len() == 1))
    {
        return Err(Error::StorageParametersOnPartitionedTable {
            table_name: SqlparserDialect::PostgreSql.stored_object_name_last(&table.name),
        });
    }
    heap(&table.name, options)
}

/// Checks built-in index parameters while leaving extension methods opaque.
pub(super) fn index(index: &CreateIndex) -> Result<(), Error> {
    let kind = match &index.using {
        None | Some(IndexType::BTree) => Some(BTREE),
        Some(IndexType::Hash) => Some(HASH),
        Some(IndexType::GiST) => Some(GIST),
        Some(IndexType::GIN) => Some(GIN),
        Some(IndexType::BRIN) => Some(BRIN),
        Some(IndexType::SPGiST) => Some(SPGIST),
        Some(IndexType::Custom(name)) => {
            match stored_ident_name(name).as_ref() {
                "btree" => Some(BTREE),
                "hash" => Some(HASH),
                "gist" => Some(GIST),
                "gin" => Some(GIN),
                "brin" => Some(BRIN),
                "spgist" => Some(SPGIST),
                _ => None,
            }
        }
        Some(IndexType::Bloom) => None,
    };
    validate(
        kind,
        index.with.iter().map(|expression| {
            let (key, value) = match expression {
                Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
                    (left.as_ref(), Some(right.as_ref()))
                }
                expression => (expression, None),
            };
            match key {
                Expr::Identifier(name) => Ok((None, name, value)),
                Expr::CompoundIdentifier(names) if names.len() == 2 => {
                    Ok((names.first(), &names[1], value))
                }
                _ => {
                    Err(Error::StorageParameterNotNamed {
                        option: expression.to_string(),
                        relation_name: SqlparserDialect::PostgreSql
                            .stored_object_name(index.name.as_ref().unwrap_or(&index.table_name)),
                    })
                }
            }
        }),
    )
}
