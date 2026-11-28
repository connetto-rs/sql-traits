//! The affinity and storage classes SQLite gives the nodes of a query, and
//! the conversions it applies to the operands of its operators and the
//! arguments of its built-in scalar functions, as
//! <https://www.sqlite.org/datatype3.html> and
//! <https://www.sqlite.org/lang_corefunc.html> define them.

use alloc::vec::Vec;

use sqlparser::ast::{
    BinaryOperator, CastKind, CeilFloorKind, ColumnDef, DataType, DateTimeField, Expr, InTarget,
    UnaryOperator, Value,
};

use super::{
    CallApplication, ExpressionContext, Operand, OperatorApplication, call_arguments, folded,
    is_row, number_type, plain_arguments,
};
use crate::{
    errors::LookupError,
    structs::{
        Affinity, CallResolution, ColumnDefinition, ColumnDefinitionRef, OperandType,
        OperatorResolution, SqliteCall, SqliteConversion, SqliteOperator, StorageClasses,
        TargetName,
    },
    traits::{ColumnLike, DataTypeLike, DatabaseLike, TableLike},
    utils::call_typing::called_name,
};

/// The affinity of an expression and the storage classes of its values.
type Valued = (Option<Affinity>, StorageClasses);

/// How an operator reads its operands.
#[derive(Clone, Copy)]
enum Reads {
    /// As a comparison, applying the affinity section 4.2 picks, the right
    /// operand taken as having none when it is an element of an `IN` list.
    Compare {
        /// The right operand is an element of an `IN` list.
        listed: bool,
    },
    /// As numbers, an `INTEGER` widening to `REAL` against a `REAL`.
    Numbers,
    /// As `INTEGER` values.
    Integers,
    /// As `TEXT` values.
    Texts,
}

impl<'query, 'db, DB: DatabaseLike> ExpressionContext<'query, 'db, DB> {
    /// The type SQLite gives `expression`, a node of the context.
    pub(super) fn sqlite_type(
        &self,
        expression: &Expr,
    ) -> Result<OperandType<'static>, LookupError> {
        Ok(match expression {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                match self.declared_column(expression) {
                    Some(column) => sqlite_column_def(column),
                    None => {
                        match self.reference(expression)? {
                            Some((definition, _)) => self.sqlite_definition_type(&definition)?,
                            None => OperandType::Unknown,
                        }
                    }
                }
            }
            // Parentheses and `COLLATE` keep the affinity of what they wrap.
            Expr::Nested(inner) | Expr::Collate { expr: inner, .. } => {
                self.type_of(inner)?.into_owned()
            }
            Expr::Value(value) => literal(&value.value),
            Expr::Cast { kind: CastKind::Cast, data_type, .. } => cast(data_type),
            Expr::Subquery(query) => self.subquery_type(query)?.into_owned(),
            Expr::Function(_)
            | Expr::Substring { .. }
            | Expr::Ceil { .. }
            | Expr::Floor { .. }
            | Expr::Trim { .. } => {
                match self.call(expression)? {
                    Some(CallApplication { resolution: CallResolution::Sqlite(call), .. }) => {
                        without_affinity(call.storage())
                    }
                    _ => OperandType::Unknown,
                }
            }
            Expr::Case { conditions, else_result, .. } => {
                let mut storage = StorageClasses::NULL;
                for result in
                    conditions.iter().map(|when| &when.result).chain(else_result.as_deref())
                {
                    let Some((_, result)) = valued(&self.type_of(result)?) else {
                        return Ok(OperandType::Unknown);
                    };
                    storage = storage.union(result);
                }
                without_affinity(storage)
            }
            _ => {
                match folded(expression) {
                    Some((digits, negative)) => without_affinity(number(digits, negative)),
                    None => self.sqlite_operator_type(expression)?,
                }
            }
        })
    }

    /// The type of what the operator `expression` applies returns, which
    /// has no affinity.
    fn sqlite_operator_type(&self, expression: &Expr) -> Result<OperandType<'static>, LookupError> {
        let storage = |operand: &Expr| -> Result<Option<StorageClasses>, LookupError> {
            Ok(valued(&self.type_of(operand)?).map(|(_, storage)| storage))
        };
        let storage = match expression {
            Expr::BinaryOp { left, op, right } if !is_row(left) && !is_row(right) => {
                match op {
                    BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo => {
                        let (Some(left), Some(right)) = (storage(left)?, storage(right)?) else {
                            return Ok(OperandType::Unknown);
                        };
                        arithmetic(read_as_number(left), read_as_number(right))
                    }
                    BinaryOperator::StringConcat => StorageClasses::TEXT,
                    BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Is
                    | BinaryOperator::IsNot
                    | BinaryOperator::Glob
                    | BinaryOperator::And
                    | BinaryOperator::Or
                    | BinaryOperator::BitwiseAnd
                    | BinaryOperator::BitwiseOr
                    | BinaryOperator::PGBitwiseShiftLeft
                    | BinaryOperator::PGBitwiseShiftRight => StorageClasses::INTEGER,
                    _ => return Ok(OperandType::Unknown),
                }
            }
            Expr::UnaryOp { op, expr } => {
                match op {
                    // `+` reads the value as it is and drops its affinity.
                    UnaryOperator::Plus => {
                        let Some(operand) = storage(expr)? else {
                            return Ok(OperandType::Unknown);
                        };
                        operand
                    }
                    UnaryOperator::Minus => {
                        let Some(operand) = storage(expr)? else {
                            return Ok(OperandType::Unknown);
                        };
                        read_as_number(operand)
                    }
                    UnaryOperator::BitwiseNot | UnaryOperator::Not => StorageClasses::INTEGER,
                    _ => return Ok(OperandType::Unknown),
                }
            }
            Expr::Like { .. }
            | Expr::IsDistinctFrom(..)
            | Expr::IsNotDistinctFrom(..)
            | Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsTrue(_)
            | Expr::IsNotTrue(_)
            | Expr::IsFalse(_)
            | Expr::IsNotFalse(_)
            | Expr::Between { .. }
            | Expr::In { .. }
            | Expr::Exists { .. } => StorageClasses::INTEGER,
            _ => return Ok(OperandType::Unknown),
        };
        Ok(without_affinity(storage))
    }

    /// The type SQLite gives the values `definition` defines.
    pub(super) fn sqlite_definition_type(
        &self,
        definition: &ColumnDefinition<'_, 'query, 'db, DB>,
    ) -> Result<OperandType<'static>, LookupError> {
        Ok(match definition {
            ColumnDefinition::Base { table, column, database } => {
                let (column, database) = (*column, *database);
                let data_type = column.data_type(database);
                if !table.is_strict() {
                    return Ok(stored(data_type.sqlite_affinity(database)));
                }
                // A `STRICT` column holds its declared class alone, `ANY`
                // keeping every value as written, as `BLOB` affinity does.
                if data_type.name(database).eq_ignore_ascii_case("any") {
                    OperandType::Sqlite {
                        affinity: Some(Affinity::Blob),
                        storage: StorageClasses::ANY,
                    }
                } else {
                    exact(data_type.sqlite_affinity(database))
                }
            }
            // A column of a subquery, a view or a CTE this query defines takes
            // the affinity of the expression defining it.
            ColumnDefinition::Expression { expression, .. } if self.holds(expression) => {
                self.type_of(expression)?.into_owned()
            }
            ColumnDefinition::SetOperation { left, right, .. } => {
                self.compound([left.definition(), right.definition()])?
            }
            ColumnDefinition::RecursiveUnion { anchor, recursive } => {
                self.compound([anchor.definition(), recursive.definition()])?
            }
            ColumnDefinition::Values { rows } => {
                self.compound(rows.iter().map(ColumnDefinitionRef::definition))?
            }
            _ => OperandType::Unknown,
        })
    }

    /// The type of a column a compound `SELECT` or a `VALUES` list combines
    /// from `definitions`, unknown when they differ in affinity, since SQLite
    /// picks the affinity of any of them.
    fn compound<'s>(
        &self,
        definitions: impl IntoIterator<Item = ColumnDefinition<'s, 'query, 'db, DB>>,
    ) -> Result<OperandType<'static>, LookupError>
    where
        'query: 's,
    {
        let mut combined: Option<Valued> = None;
        for definition in definitions {
            let Some((affinity, storage)) = valued(&self.sqlite_definition_type(&definition)?)
            else {
                return Ok(OperandType::Unknown);
            };
            combined = match combined {
                None => Some((affinity, storage)),
                Some((seen, _)) if seen != affinity => return Ok(OperandType::Unknown),
                Some((seen, union)) => Some((seen, union.union(storage))),
            };
        }
        Ok(combined.map_or(OperandType::Unknown, |(affinity, storage)| {
            OperandType::Sqlite { affinity, storage }
        }))
    }

    /// The operators SQLite applies to evaluate `expression`, `None` for a
    /// node whose operators the context does not derive.
    pub(super) fn sqlite_operators<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<Vec<OperatorApplication<'node, 'db, DB>>>, LookupError> {
        let applied: Vec<(&'static str, Option<&'node Expr>, &'node Expr, Reads)> = match expression
        {
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::Value(_)
            | Expr::Cast { kind: CastKind::Cast, .. }
            | Expr::Collate { .. } => Vec::new(),
            Expr::BinaryOp { left, op, right } if !is_row(left) && !is_row(right) => {
                let Some((name, reads)) = binary(op) else { return Ok(None) };
                Vec::from([(name, Some(left.as_ref()), right.as_ref(), reads)])
            }
            Expr::UnaryOp { op, expr } => {
                match op {
                    UnaryOperator::Plus => Vec::new(),
                    UnaryOperator::Minus => Vec::from([("-", None, expr.as_ref(), Reads::Numbers)]),
                    UnaryOperator::BitwiseNot => {
                        Vec::from([("~", None, expr.as_ref(), Reads::Integers)])
                    }
                    _ => return Ok(None),
                }
            }
            Expr::Like { negated, any: false, expr, pattern, escape_char: None } => {
                let name = if *negated { "NOT LIKE" } else { "LIKE" };
                Vec::from([(name, Some(expr.as_ref()), pattern.as_ref(), Reads::Texts)])
            }
            Expr::IsDistinctFrom(left, right) if !is_row(left) && !is_row(right) => {
                Vec::from([(
                    "IS NOT",
                    Some(left.as_ref()),
                    right.as_ref(),
                    Reads::Compare { listed: false },
                )])
            }
            Expr::IsNotDistinctFrom(left, right) if !is_row(left) && !is_row(right) => {
                Vec::from([(
                    "IS",
                    Some(left.as_ref()),
                    right.as_ref(),
                    Reads::Compare { listed: false },
                )])
            }
            Expr::Between { expr, negated, low, high }
                if !is_row(expr) && !is_row(low) && !is_row(high) =>
            {
                let (lower, upper) = if *negated { ("<", ">") } else { (">=", "<=") };
                let compare = Reads::Compare { listed: false };
                Vec::from([
                    (lower, Some(expr.as_ref()), low.as_ref(), compare),
                    (upper, Some(expr.as_ref()), high.as_ref(), compare),
                ])
            }
            Expr::In { expr, target: InTarget::List(elements), negated, .. }
                if !is_row(expr) && !elements.iter().any(is_row) =>
            {
                let name = if *negated { "<>" } else { "=" };
                elements
                    .iter()
                    .map(|element| {
                        (name, Some(expr.as_ref()), element, Reads::Compare { listed: true })
                    })
                    .collect()
            }
            _ => return Ok(None),
        };
        applied
            .into_iter()
            .map(|(name, left, right, reads)| self.sqlite_applied(name, left, right, reads))
            .collect::<Result<_, _>>()
            .map(Some)
    }

    /// The application of the operator `name` to `left` and `right`, read
    /// as `reads` says.
    fn sqlite_applied<'node>(
        &self,
        name: &'static str,
        left: Option<&'node Expr>,
        right: &'node Expr,
        reads: Reads,
    ) -> Result<OperatorApplication<'node, 'db, DB>, LookupError> {
        let right_type = valued(&self.type_of(right)?);
        let left_type = match left {
            Some(left) => Some(valued(&self.type_of(left)?)),
            None => None,
        };
        let resolution = match (left_type, right_type) {
            (Some(None), _) | (_, None) => OperatorResolution::Undetermined,
            (left_type, Some(right_type)) => {
                let left_type = left_type.flatten();
                let (left, right) = match reads {
                    Reads::Compare { listed } => {
                        let right_type = if listed { (None, right_type.1) } else { right_type };
                        left_type.map_or((None, None), |left_type| compared(left_type, right_type))
                    }
                    Reads::Numbers => {
                        let left_storage = left_type.map(|(_, storage)| storage);
                        (
                            left_storage.and_then(|left| widened(left, Some(right_type.1))),
                            widened(right_type.1, left_storage),
                        )
                    }
                    Reads::Integers => {
                        (
                            left_type
                                .and_then(|(_, left)| read_as(left, SqliteConversion::Integer)),
                            read_as(right_type.1, SqliteConversion::Integer),
                        )
                    }
                    Reads::Texts => {
                        (
                            left_type.and_then(|(_, left)| read_as(left, SqliteConversion::Text)),
                            read_as(right_type.1, SqliteConversion::Text),
                        )
                    }
                };
                OperatorResolution::Sqlite(SqliteOperator::new(left, right))
            }
        };
        Ok(OperatorApplication {
            name: TargetName::new(name, false),
            left,
            right: Operand::Node(right),
            resolution,
            refused: None,
        })
    }

    /// The call SQLite makes for `expression`, `None` for a node that is no
    /// call or a call written with clauses the context does not read. A call
    /// of a built-in scalar function reached by its name and arity, whose
    /// arguments are all of known type, answers [`CallResolution::Sqlite`],
    /// and any other call, which may reach a function the application
    /// defines, answers [`CallResolution::Undetermined`]. The calls of
    /// `substr`, `substring`, `ceil`, `floor` and `trim` that sqlparser reads
    /// as expressions of their own are calls of those functions.
    pub(super) fn sqlite_call<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<CallApplication<'node, 'db, DB>>, LookupError> {
        let (name, arguments, plain) = match expression {
            Expr::Function(function) => {
                let Some((arguments, _)) = call_arguments(function) else { return Ok(None) };
                let name = called_name(function)?;
                let plain = plain_arguments(function).is_some() && name.schema().is_none();
                (name, arguments, plain)
            }
            Expr::Substring { expr, substring_from, substring_for, shorthand, .. } => {
                let name = if *shorthand { "substr" } else { "substring" };
                let arguments = core::iter::once(expr.as_ref())
                    .chain(substring_from.as_deref())
                    .chain(substring_for.as_deref())
                    .collect();
                (TargetName::new(name, false), arguments, true)
            }
            Expr::Ceil { expr, field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) } => {
                (TargetName::new("ceil", false), Vec::from([expr.as_ref()]), true)
            }
            Expr::Floor {
                expr,
                field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
            } => (TargetName::new("floor", false), Vec::from([expr.as_ref()]), true),
            Expr::Trim { expr, trim_where: None, trim_what: None, trim_characters } => {
                let arguments = core::iter::once(expr.as_ref())
                    .chain(trim_characters.iter().flatten())
                    .collect();
                (TargetName::new("trim", false), arguments, true)
            }
            _ => return Ok(None),
        };
        let resolution = if plain {
            self.builtin(name.name(), &arguments)?
        } else {
            CallResolution::Undetermined
        };
        Ok(Some(CallApplication { name, arguments, resolution, refused: None }))
    }

    /// What the built-in scalar function `name` does with `arguments`.
    fn builtin(
        &self,
        name: &str,
        arguments: &[&Expr],
    ) -> Result<CallResolution<'db, DB>, LookupError> {
        let Some(builtin) = BUILTINS.iter().find(|builtin| {
            builtin.names.iter().any(|known| known.eq_ignore_ascii_case(name))
                && builtin.takes(arguments.len())
        }) else {
            return Ok(CallResolution::Undetermined);
        };
        let mut storages = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let Some((_, storage)) = valued(&self.type_of(argument)?) else {
                return Ok(CallResolution::Undetermined);
            };
            storages.push(storage);
        }
        let conversions = (0..storages.len())
            .map(|position| builtin.reading(position).conversion(position, &storages))
            .collect();
        Ok(CallResolution::Sqlite(SqliteCall::new(conversions, builtin.returns.storage(&storages))))
    }
}

/// The affinity and storage classes of `operand`, `None` for a type SQLite
/// typing did not derive.
const fn valued(operand: &OperandType<'_>) -> Option<Valued> {
    match operand {
        OperandType::Sqlite { affinity, storage } => Some((*affinity, *storage)),
        _ => None,
    }
}

/// A value of `storage` without affinity.
const fn without_affinity(storage: StorageClasses) -> OperandType<'static> {
    OperandType::Sqlite { affinity: None, storage }
}

/// A value of a column, or a `CAST`, of `affinity`, which holds the classes
/// the affinity stores what it converts in.
const fn stored(affinity: Affinity) -> OperandType<'static> {
    OperandType::Sqlite { affinity: Some(affinity), storage: affinity.preferred_storage() }
}

/// The type of a column the generated expression being typed declares,
/// before the table holding it exists.
fn sqlite_column_def(column: &ColumnDef) -> OperandType<'static> {
    stored(Affinity::of_written(&column.data_type))
}

/// A value of `affinity` held in the class the affinity prefers, a `BLOB`
/// for `BLOB` affinity.
const fn exact(affinity: Affinity) -> OperandType<'static> {
    match affinity {
        Affinity::Blob => {
            OperandType::Sqlite { affinity: Some(Affinity::Blob), storage: StorageClasses::BLOB }
        }
        affinity => stored(affinity),
    }
}

/// The type of `CAST(... AS data_type)`, whose affinity is the one a column
/// declared `data_type` takes, the cast turning every value into the class
/// the affinity prefers.
fn cast(data_type: &DataType) -> OperandType<'static> {
    exact(Affinity::of_written(data_type))
}

/// The storage class of the numeric literal `digits`, negated when
/// `negative`, which is `REAL` with a decimal point, an exponent or a
/// magnitude no 64-bit integer holds, and `INTEGER` otherwise.
fn number(digits: &str, negative: bool) -> StorageClasses {
    match number_type(digits, negative) {
        "numeric" => StorageClasses::REAL,
        _ => StorageClasses::INTEGER,
    }
}

/// The type of the literal `value`, which has no affinity.
fn literal(value: &Value) -> OperandType<'static> {
    without_affinity(match value {
        Value::Number(digits, _) => number(digits, false),
        Value::SingleQuotedString(_)
        | Value::DoubleQuotedString(_)
        | Value::TripleSingleQuotedString(_)
        | Value::TripleDoubleQuotedString(_)
        | Value::EscapedStringLiteral(_)
        | Value::UnicodeStringLiteral(_)
        | Value::NationalStringLiteral(_)
        | Value::DollarQuotedString(_) => StorageClasses::TEXT,
        // sqlparser reads the integer `0x1F` and the blob `X'1F'` alike
        // under SQLite.
        Value::HexStringLiteral(_) => StorageClasses::INTEGER.union(StorageClasses::BLOB),
        Value::Boolean(_) => StorageClasses::INTEGER,
        Value::Null => StorageClasses::NULL,
        Value::Placeholder(_) => StorageClasses::ANY,
        _ => return OperandType::Unknown,
    })
}

/// The operator name and the reading of a binary operator SQLite applies,
/// `None` for the boolean connectives and the operators SQLite leaves to
/// functions the application defines.
const fn binary(op: &BinaryOperator) -> Option<(&'static str, Reads)> {
    let compare = Reads::Compare { listed: false };
    Some(match op {
        BinaryOperator::Eq => ("=", compare),
        BinaryOperator::NotEq => ("<>", compare),
        BinaryOperator::Lt => ("<", compare),
        BinaryOperator::LtEq => ("<=", compare),
        BinaryOperator::Gt => (">", compare),
        BinaryOperator::GtEq => (">=", compare),
        BinaryOperator::Is => ("IS", compare),
        BinaryOperator::IsNot => ("IS NOT", compare),
        BinaryOperator::Plus => ("+", Reads::Numbers),
        BinaryOperator::Minus => ("-", Reads::Numbers),
        BinaryOperator::Multiply => ("*", Reads::Numbers),
        BinaryOperator::Divide => ("/", Reads::Numbers),
        BinaryOperator::Modulo => ("%", Reads::Integers),
        BinaryOperator::BitwiseAnd => ("&", Reads::Integers),
        BinaryOperator::BitwiseOr => ("|", Reads::Integers),
        BinaryOperator::PGBitwiseShiftLeft => ("<<", Reads::Integers),
        BinaryOperator::PGBitwiseShiftRight => (">>", Reads::Integers),
        BinaryOperator::StringConcat => ("||", Reads::Texts),
        BinaryOperator::Glob => ("GLOB", Reads::Texts),
        _ => return None,
    })
}

/// The affinity section 4.2 applies to the `left` and `right` operands of
/// a comparison, each recorded where it converts the operand's values.
fn compared(left: Valued, right: Valued) -> (Option<SqliteConversion>, Option<SqliteConversion>) {
    let numeric = |affinity: Option<Affinity>| affinity.is_some_and(Affinity::is_numeric);
    match (left.0, right.0) {
        (left_affinity, right_affinity) if numeric(left_affinity) && !numeric(right_affinity) => {
            (None, applied(Affinity::Numeric, right.1))
        }
        (left_affinity, right_affinity) if numeric(right_affinity) && !numeric(left_affinity) => {
            (applied(Affinity::Numeric, left.1), None)
        }
        (Some(Affinity::Text), None) => (None, applied(Affinity::Text, right.1)),
        (None, Some(Affinity::Text)) => (applied(Affinity::Text, left.1), None),
        _ => (None, None),
    }
}

/// Applying `affinity` to values of `storage`, recorded when it converts
/// them. `NUMERIC` converts `TEXT` and `TEXT` converts numbers, and neither
/// touches a `BLOB`.
fn applied(affinity: Affinity, storage: StorageClasses) -> Option<SqliteConversion> {
    let kept = match affinity {
        Affinity::Text => StorageClasses::TEXT,
        _ => StorageClasses::NUMBERS,
    };
    storage
        .converts_under(kept.union(StorageClasses::BLOB))
        .then_some(SqliteConversion::Affinity(affinity))
}

/// Reading values of `storage` as `conversion` names, recorded when it
/// converts them.
fn read_as(storage: StorageClasses, conversion: SqliteConversion) -> Option<SqliteConversion> {
    let accepted = match conversion {
        SqliteConversion::Integer => StorageClasses::INTEGER,
        SqliteConversion::Real => StorageClasses::REAL,
        SqliteConversion::Text => StorageClasses::TEXT,
        SqliteConversion::Blob => StorageClasses::BLOB,
        SqliteConversion::Numeric | SqliteConversion::Affinity(_) => StorageClasses::NUMBERS,
    };
    storage.converts_under(accepted).then_some(conversion)
}

/// The conversion an arithmetic operator applies to an operand of
/// `storage` whose other operand, if any, holds `other`. A value that is no
/// number is read as one, and an `INTEGER` widens to `REAL` against a
/// `REAL`.
fn widened(storage: StorageClasses, other: Option<StorageClasses>) -> Option<SqliteConversion> {
    read_as(storage, SqliteConversion::Numeric).or_else(|| {
        (storage == StorageClasses::INTEGER && other == Some(StorageClasses::REAL))
            .then_some(SqliteConversion::Real)
    })
}

/// The classes a value of `storage` takes once read as a number.
fn read_as_number(storage: StorageClasses) -> StorageClasses {
    if storage == StorageClasses::NULL
        || storage == StorageClasses::INTEGER
        || storage == StorageClasses::REAL
    {
        storage
    } else {
        StorageClasses::NUMBERS
    }
}

/// The classes an arithmetic operator returns over operands read as
/// numbers of `left` and `right`, `REAL` when either is and `INTEGER` when
/// both are, an integer overflow aside.
fn arithmetic(left: StorageClasses, right: StorageClasses) -> StorageClasses {
    if left == StorageClasses::NULL || right == StorageClasses::NULL {
        StorageClasses::NULL
    } else if left == StorageClasses::REAL || right == StorageClasses::REAL {
        StorageClasses::REAL
    } else if left == StorageClasses::INTEGER && right == StorageClasses::INTEGER {
        StorageClasses::INTEGER
    } else {
        StorageClasses::NUMBERS
    }
}

/// How a built-in function reads an argument.
#[derive(Clone, Copy)]
enum Reading {
    /// As it is.
    AsIs,
    /// As a number.
    Number,
    /// As an `INTEGER`.
    Integer,
    /// As a `REAL`.
    Real,
    /// As `TEXT`.
    Text,
    /// As `TEXT`, a `BLOB` read as it is.
    TextOrBlob,
    /// As `TEXT`, unless every argument is a `BLOB`.
    TextUnlessAllBlobs,
    /// As a `BLOB`.
    Blob,
}

impl Reading {
    /// The conversion reading the argument at `position` applies, the
    /// arguments holding `storages`.
    fn conversion(self, position: usize, storages: &[StorageClasses]) -> Option<SqliteConversion> {
        let storage = storages[position];
        match self {
            Self::AsIs => None,
            Self::Number => read_as(storage, SqliteConversion::Numeric),
            Self::Integer => read_as(storage, SqliteConversion::Integer),
            Self::Real => read_as(storage, SqliteConversion::Real),
            Self::Text => read_as(storage, SqliteConversion::Text),
            Self::TextOrBlob => {
                storage
                    .converts_under(StorageClasses::TEXT.union(StorageClasses::BLOB))
                    .then_some(SqliteConversion::Text)
            }
            Self::TextUnlessAllBlobs => {
                let another_is_no_blob = storages.iter().enumerate().any(|(other, storage)| {
                    other != position && storage.converts_under(StorageClasses::BLOB)
                });
                let converted = if storage.intersects(StorageClasses::BLOB) {
                    another_is_no_blob
                } else {
                    true
                };
                (converted && storage.converts_under(StorageClasses::TEXT))
                    .then_some(SqliteConversion::Text)
            }
            Self::Blob => read_as(storage, SqliteConversion::Blob),
        }
    }
}

/// What a built-in function returns.
#[derive(Clone, Copy)]
enum Returns {
    /// Values of these classes.
    Class(StorageClasses),
    /// The first argument.
    First,
    /// One of the arguments.
    Any,
    /// One of the values a condition picks, each argument after a condition
    /// and a trailing one.
    Picked,
    /// The first argument read as a number.
    Number,
    /// Part of the first argument, a `BLOB` for a `BLOB` and `TEXT`
    /// otherwise.
    Part,
}

impl Returns {
    /// The classes returned over arguments holding `storages`.
    fn storage(self, storages: &[StorageClasses]) -> StorageClasses {
        let first = storages.first().copied().unwrap_or(StorageClasses::NULL);
        let union = |picked: &mut dyn Iterator<Item = &StorageClasses>| {
            picked.fold(StorageClasses::NULL, |union, storage| union.union(*storage))
        };
        match self {
            Self::Class(storage) => storage,
            Self::First => first,
            Self::Any => union(&mut storages.iter()),
            Self::Picked => {
                let trailing = storages.len() % 2 == 1;
                union(&mut storages.iter().enumerate().filter_map(|(position, storage)| {
                    (position % 2 == 1 || (trailing && position + 1 == storages.len()))
                        .then_some(storage)
                }))
            }
            Self::Number => read_as_number(first),
            Self::Part => {
                if first == StorageClasses::NULL || first == StorageClasses::BLOB {
                    first
                } else if first.intersects(StorageClasses::BLOB) {
                    StorageClasses::TEXT.union(StorageClasses::BLOB)
                } else {
                    StorageClasses::TEXT
                }
            }
        }
    }
}

/// A built-in scalar function of SQLite.
struct Builtin {
    /// The names it is called by.
    names: &'static [&'static str],
    /// The fewest arguments it takes.
    least: usize,
    /// The most arguments it takes, `None` for any number.
    most: Option<usize>,
    /// How it reads each argument, the last reading repeating for the
    /// arguments past it.
    readings: &'static [Reading],
    /// What it returns.
    returns: Returns,
}

impl Builtin {
    /// Whether it takes `count` arguments.
    fn takes(&self, count: usize) -> bool {
        count >= self.least && self.most.is_none_or(|most| count <= most)
    }

    /// How it reads the argument at `position`.
    fn reading(&self, position: usize) -> Reading {
        self.readings
            .get(position)
            .or_else(|| self.readings.last())
            .copied()
            .unwrap_or(Reading::AsIs)
    }
}

/// A built-in taking from `least` to `most` arguments.
const fn builtin(
    names: &'static [&'static str],
    least: usize,
    most: Option<usize>,
    readings: &'static [Reading],
    returns: Returns,
) -> Builtin {
    Builtin { names, least, most, readings, returns }
}

const INTEGER: Returns = Returns::Class(StorageClasses::INTEGER);
const REAL: Returns = Returns::Class(StorageClasses::REAL);
const TEXT: Returns = Returns::Class(StorageClasses::TEXT);
const BLOB: Returns = Returns::Class(StorageClasses::BLOB);

/// The core and math scalar functions of SQLite, as
/// <https://www.sqlite.org/lang_corefunc.html> and
/// <https://www.sqlite.org/lang_mathfunc.html> document them.
static BUILTINS: &[Builtin] = &[
    builtin(&["abs"], 1, Some(1), &[Reading::Number], Returns::Number),
    builtin(&["changes", "last_insert_rowid", "random", "total_changes"], 0, Some(0), &[], INTEGER),
    builtin(&["char"], 0, None, &[Reading::Integer], TEXT),
    builtin(&["coalesce"], 2, None, &[Reading::AsIs], Returns::Any),
    builtin(&["concat"], 1, None, &[Reading::Text], TEXT),
    builtin(&["concat_ws"], 2, None, &[Reading::Text], TEXT),
    builtin(&["current_date", "current_time", "current_timestamp"], 0, Some(0), &[], TEXT),
    builtin(&["format", "printf"], 0, None, &[Reading::Text, Reading::AsIs], TEXT),
    builtin(&["glob"], 2, Some(2), &[Reading::Text], INTEGER),
    builtin(&["hex"], 1, Some(1), &[Reading::Blob], TEXT),
    builtin(&["ifnull"], 2, Some(2), &[Reading::AsIs], Returns::Any),
    builtin(&["iif", "if"], 2, None, &[Reading::AsIs], Returns::Picked),
    builtin(&["instr"], 2, Some(2), &[Reading::TextUnlessAllBlobs], INTEGER),
    builtin(&["length", "octet_length"], 1, Some(1), &[Reading::TextOrBlob], INTEGER),
    builtin(&["like"], 2, Some(3), &[Reading::Text], INTEGER),
    builtin(&["likelihood"], 2, Some(2), &[Reading::AsIs], Returns::First),
    builtin(&["likely", "unlikely"], 1, Some(1), &[Reading::AsIs], Returns::First),
    builtin(&["lower", "soundex", "unistr", "upper"], 1, Some(1), &[Reading::Text], TEXT),
    builtin(&["ltrim", "rtrim", "trim"], 1, Some(2), &[Reading::Text], TEXT),
    builtin(&["max", "min"], 2, None, &[Reading::AsIs], Returns::Any),
    builtin(&["nullif"], 2, Some(2), &[Reading::AsIs], Returns::First),
    builtin(&["quote", "typeof", "unistr_quote"], 1, Some(1), &[Reading::AsIs], TEXT),
    builtin(&["randomblob", "zeroblob"], 1, Some(1), &[Reading::Integer], BLOB),
    builtin(&["replace"], 3, Some(3), &[Reading::Text], TEXT),
    builtin(&["round"], 1, Some(2), &[Reading::Real, Reading::Integer], REAL),
    builtin(&["sign"], 1, Some(1), &[Reading::Number], INTEGER),
    builtin(&["sqlite_compileoption_get"], 1, Some(1), &[Reading::Integer], TEXT),
    builtin(&["sqlite_compileoption_used"], 1, Some(1), &[Reading::Text], INTEGER),
    builtin(&["sqlite_source_id", "sqlite_version"], 0, Some(0), &[], TEXT),
    builtin(
        &["substr", "substring"],
        2,
        Some(3),
        &[Reading::TextOrBlob, Reading::Integer],
        Returns::Part,
    ),
    builtin(&["unhex"], 1, Some(2), &[Reading::Text], BLOB),
    builtin(&["unicode"], 1, Some(1), &[Reading::Text], INTEGER),
    builtin(
        &[
            "acos", "acosh", "asin", "asinh", "atan", "atanh", "cos", "cosh", "degrees", "exp",
            "ln", "log10", "log2", "radians", "sin", "sinh", "sqrt", "tan", "tanh",
        ],
        1,
        Some(1),
        &[Reading::Real],
        REAL,
    ),
    builtin(&["atan2", "mod", "pow", "power"], 2, Some(2), &[Reading::Real], REAL),
    builtin(&["log"], 1, Some(2), &[Reading::Real], REAL),
    builtin(&["pi"], 0, Some(0), &[], REAL),
    builtin(
        &["ceil", "ceiling", "floor", "trunc"],
        1,
        Some(1),
        &[Reading::Number],
        Returns::Number,
    ),
];
