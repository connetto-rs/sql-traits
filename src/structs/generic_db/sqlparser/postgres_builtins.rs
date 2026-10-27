//! The PostgreSQL functions a schema calls without declaring them.

use alloc::{boxed::Box, string::String, vec, vec::Vec};

use sqlparser::{
    ast::{
        ArgMode, ArrayElemTypeDef, CreateFunction, CreateFunctionBody, DataType, ExactNumberInfo,
        Expr, FunctionBehavior, FunctionCalledOnNull, FunctionReturnType, Ident, ObjectName,
        ObjectNamePart, OperateFunctionArg, TimezoneInfo, Value, ValueWithSpan,
    },
    tokenizer::Span,
};

use crate::structs::metadata::FunctionMetadata;

/// The type of a builtin's argument or result.
#[derive(Clone, Copy)]
enum BuiltinType {
    Any,
    /// A `VARIADIC "any"` parameter, which gathers the remaining arguments.
    VariadicAny,
    Text,
    TextArray,
    Boolean,
    Int,
    BigInt,
    Numeric,
    DoublePrecision,
    Uuid,
    Date,
    Interval,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

impl BuiltinType {
    fn data_type(self) -> DataType {
        match self {
            Self::Any | Self::VariadicAny => {
                DataType::Custom(
                    ObjectName(vec![ObjectNamePart::Identifier(Ident::with_quote('"', "any"))]),
                    vec![],
                )
            }
            Self::Text => DataType::Text,
            Self::TextArray => {
                DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(DataType::Text), None))
            }
            Self::Boolean => DataType::Boolean,
            Self::Int => DataType::Int(None),
            Self::BigInt => DataType::BigInt(None),
            Self::Numeric => DataType::Numeric(ExactNumberInfo::None),
            Self::DoublePrecision => DataType::DoublePrecision,
            Self::Uuid => DataType::Uuid,
            Self::Date => DataType::Date,
            Self::Interval => DataType::Interval { fields: None, precision: None },
            Self::Time => DataType::Time(None, TimezoneInfo::None),
            Self::TimeTz => DataType::Time(None, TimezoneInfo::WithTimeZone),
            Self::Timestamp => DataType::Timestamp(None, TimezoneInfo::None),
            Self::TimestampTz => DataType::Timestamp(None, TimezoneInfo::WithTimeZone),
        }
    }
}

/// What a `NULL` argument does to a builtin's call.
#[derive(Clone, Copy)]
enum NullInput {
    /// `pg_proc.proisstrict` is true, so any `NULL` argument makes the call
    /// `NULL`.
    Strict,
    /// Not strict, a name without a `pg_proc` row included.
    CalledOnNull,
    /// Not strict, yet `NULL` at these argument positions makes the call
    /// `NULL`.
    NullAt(&'static [usize]),
}

/// `pg_proc.provolatile`, a `Copy` stand-in for [`FunctionBehavior`] so a row
/// can be read out of the static table.
#[derive(Clone, Copy)]
enum Volatility {
    Immutable,
    Stable,
    /// Also the answer for a name without a `pg_proc` row, as for a
    /// `CREATE FUNCTION` that states none.
    Volatile,
}

impl Volatility {
    fn behavior(self) -> FunctionBehavior {
        match self {
            Self::Immutable => FunctionBehavior::Immutable,
            Self::Stable => FunctionBehavior::Stable,
            Self::Volatile => FunctionBehavior::Volatile,
        }
    }
}

/// One builtin overload: its name, arguments, result, null-input behavior and
/// volatility.
#[derive(Clone, Copy)]
pub(super) struct PostgresBuiltin(
    &'static str,
    &'static [BuiltinType],
    BuiltinType,
    NullInput,
    Volatility,
);

impl PostgresBuiltin {
    /// The declaration, qualified by `pg_catalog` so a user function may
    /// shadow it the way the server allows.
    pub(super) fn function(&self) -> CreateFunction {
        let Self(name, arguments, returns, null_input, volatility) = *self;
        let args = arguments
            .iter()
            .map(|argument| {
                OperateFunctionArg {
                    // The mode, since `VARIADIC` as a name would invent an
                    // argument called that.
                    mode: matches!(argument, BuiltinType::VariadicAny).then_some(ArgMode::Variadic),
                    name: None,
                    data_type: argument.data_type(),
                    default_expr: None,
                }
            })
            .collect::<Vec<_>>();
        CreateFunction {
            or_alter: false,
            or_replace: false,
            temporary: false,
            if_not_exists: false,
            name: ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                ObjectNamePart::Identifier(Ident::new(name)),
            ]),
            args: Some(args),
            return_type: Some(FunctionReturnType::DataType(returns.data_type())),
            function_body: Some(CreateFunctionBody::AsBeforeOptions {
                body: Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString(String::new()),
                    span: Span::empty(),
                }),
                link_symbol: None,
            }),
            behavior: Some(volatility.behavior()),
            called_on_null: matches!(null_input, NullInput::Strict)
                .then_some(FunctionCalledOnNull::Strict),
            parallel: None,
            using: None,
            language: Some(Ident::new("internal")),
            determinism_specifier: None,
            options: None,
            remote_connection: None,
            security: None,
            set_params: vec![],
        }
    }

    pub(super) fn metadata(&self) -> FunctionMetadata {
        let mut metadata = FunctionMetadata::default();
        if let NullInput::NullAt(positions) = self.3 {
            metadata.set_null_returning_arguments(positions);
        }
        metadata
    }
}

use BuiltinType::{
    Any, BigInt, Boolean, Date, DoublePrecision, Int, Interval, Numeric, Text, TextArray, Time,
    TimeTz, Timestamp, TimestampTz, Uuid, VariadicAny,
};
use NullInput::{CalledOnNull, NullAt, Strict};
use Volatility::{Immutable, Stable, Volatile};

/// The builtins a parsed schema can call, strict and volatile as PostgreSQL 18
/// declares. The SQL value functions such as `current_date` have no
/// `pg_proc` row, and the planner holds every one of them stable.
pub(super) static POSTGRES_BUILTINS: &[PostgresBuiltin] = &[
    PostgresBuiltin("length", &[Text], Int, Strict, Immutable),
    PostgresBuiltin("len", &[Text], Int, CalledOnNull, Volatile),
    PostgresBuiltin("char_length", &[Text], Int, Strict, Immutable),
    PostgresBuiltin("character_length", &[Text], Int, Strict, Immutable),
    PostgresBuiltin("octet_length", &[Text], Int, Strict, Immutable),
    PostgresBuiltin("lower", &[Text], Text, Strict, Immutable),
    PostgresBuiltin("upper", &[Text], Text, Strict, Immutable),
    PostgresBuiltin("btrim", &[Text], Text, Strict, Immutable),
    PostgresBuiltin("btrim", &[Text, Text], Text, Strict, Immutable),
    PostgresBuiltin("split_part", &[Text, Text, Int], Text, Strict, Immutable),
    PostgresBuiltin("string_to_array", &[Text, Text], TextArray, NullAt(&[0]), Immutable),
    PostgresBuiltin("string_to_array", &[Text, Text, Text], TextArray, NullAt(&[0]), Immutable),
    PostgresBuiltin("current_setting", &[Text], Text, Strict, Stable),
    PostgresBuiltin("current_setting", &[Text, Boolean], Text, Strict, Stable),
    PostgresBuiltin("coalesce", &[VariadicAny], Any, CalledOnNull, Immutable),
    PostgresBuiltin("nullif", &[Any, Any], Any, NullAt(&[0]), Immutable),
    PostgresBuiltin("now", &[], TimestampTz, Strict, Stable),
    PostgresBuiltin("current_timestamp", &[], TimestampTz, CalledOnNull, Stable),
    PostgresBuiltin("current_date", &[], Date, CalledOnNull, Stable),
    PostgresBuiltin("current_time", &[], TimeTz, CalledOnNull, Stable),
    PostgresBuiltin("localtimestamp", &[], Timestamp, CalledOnNull, Stable),
    PostgresBuiltin("localtime", &[], Time, CalledOnNull, Stable),
    PostgresBuiltin("gen_random_uuid", &[], Uuid, Strict, Volatile),
    PostgresBuiltin("uuidv4", &[], Uuid, Strict, Volatile),
    PostgresBuiltin("uuidv7", &[], Uuid, Strict, Volatile),
    PostgresBuiltin("uuidv7", &[Interval], Uuid, Strict, Volatile),
    PostgresBuiltin("count", &[Any], BigInt, CalledOnNull, Immutable),
    PostgresBuiltin("sum", &[Any], Numeric, CalledOnNull, Immutable),
    PostgresBuiltin("avg", &[Any], Numeric, CalledOnNull, Immutable),
    PostgresBuiltin("min", &[Any], Any, CalledOnNull, Immutable),
    PostgresBuiltin("max", &[Any], Any, CalledOnNull, Immutable),
    PostgresBuiltin("current_user", &[], Text, Strict, Stable),
    PostgresBuiltin("session_user", &[], Text, Strict, Stable),
    PostgresBuiltin("user", &[], Text, CalledOnNull, Stable),
    PostgresBuiltin("random", &[], DoublePrecision, Strict, Volatile),
    PostgresBuiltin("random", &[Int, Int], Int, Strict, Volatile),
    PostgresBuiltin("random", &[BigInt, BigInt], BigInt, Strict, Volatile),
    PostgresBuiltin("random", &[Numeric, Numeric], Numeric, Strict, Volatile),
    PostgresBuiltin("clock_timestamp", &[], TimestampTz, Strict, Volatile),
    PostgresBuiltin("statement_timestamp", &[], TimestampTz, Strict, Stable),
    PostgresBuiltin("transaction_timestamp", &[], TimestampTz, Strict, Stable),
    PostgresBuiltin("timeofday", &[], Text, Strict, Volatile),
    PostgresBuiltin("md5", &[Text], Text, Strict, Immutable),
    PostgresBuiltin("substring", &[Text, Int], Text, Strict, Immutable),
    PostgresBuiltin("substring", &[Text, Int, Int], Text, Strict, Immutable),
    PostgresBuiltin("substring", &[Text, Text], Text, Strict, Immutable),
    PostgresBuiltin("substring", &[Text, Text, Text], Text, Strict, Immutable),
];
