//! The PostgreSQL functions a schema calls without declaring them.

use alloc::{boxed::Box, string::String, vec, vec::Vec};

use sqlparser::{
    ast::{
        ArgMode, ArrayElemTypeDef, CreateFunction, CreateFunctionBody, DataType, ExactNumberInfo,
        Expr, FunctionCalledOnNull, FunctionReturnType, Ident, ObjectName, ObjectNamePart,
        OperateFunctionArg, TimezoneInfo, Value, ValueWithSpan,
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

/// One builtin overload: its name, arguments, result and null-input
/// behavior.
#[derive(Clone, Copy)]
pub(super) struct PostgresBuiltin(&'static str, &'static [BuiltinType], BuiltinType, NullInput);

impl PostgresBuiltin {
    /// The declaration, qualified by `pg_catalog` so a user function may
    /// shadow it the way the server allows.
    pub(super) fn function(&self) -> CreateFunction {
        let Self(name, arguments, returns, null_input) = *self;
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
            behavior: None,
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
    Any, BigInt, Boolean, Date, Int, Interval, Numeric, Text, TextArray, Time, TimeTz, Timestamp,
    TimestampTz, Uuid, VariadicAny,
};
use NullInput::{CalledOnNull, NullAt, Strict};

/// The builtins a parsed schema can call, strict as PostgreSQL 18 declares.
pub(super) static POSTGRES_BUILTINS: &[PostgresBuiltin] = &[
    PostgresBuiltin("length", &[Text], Int, Strict),
    PostgresBuiltin("len", &[Text], Int, CalledOnNull),
    PostgresBuiltin("char_length", &[Text], Int, Strict),
    PostgresBuiltin("character_length", &[Text], Int, Strict),
    PostgresBuiltin("octet_length", &[Text], Int, Strict),
    PostgresBuiltin("lower", &[Text], Text, Strict),
    PostgresBuiltin("upper", &[Text], Text, Strict),
    PostgresBuiltin("btrim", &[Text], Text, Strict),
    PostgresBuiltin("btrim", &[Text, Text], Text, Strict),
    PostgresBuiltin("split_part", &[Text, Text, Int], Text, Strict),
    PostgresBuiltin("string_to_array", &[Text, Text], TextArray, NullAt(&[0])),
    PostgresBuiltin("string_to_array", &[Text, Text, Text], TextArray, NullAt(&[0])),
    PostgresBuiltin("current_setting", &[Text], Text, Strict),
    PostgresBuiltin("current_setting", &[Text, Boolean], Text, Strict),
    PostgresBuiltin("coalesce", &[VariadicAny], Any, CalledOnNull),
    PostgresBuiltin("nullif", &[Any, Any], Any, NullAt(&[0])),
    PostgresBuiltin("now", &[], TimestampTz, Strict),
    PostgresBuiltin("current_timestamp", &[], TimestampTz, CalledOnNull),
    PostgresBuiltin("current_date", &[], Date, CalledOnNull),
    PostgresBuiltin("current_time", &[], TimeTz, CalledOnNull),
    PostgresBuiltin("localtimestamp", &[], Timestamp, CalledOnNull),
    PostgresBuiltin("localtime", &[], Time, CalledOnNull),
    PostgresBuiltin("gen_random_uuid", &[], Uuid, Strict),
    PostgresBuiltin("uuidv4", &[], Uuid, Strict),
    PostgresBuiltin("uuidv7", &[], Uuid, Strict),
    PostgresBuiltin("uuidv7", &[Interval], Uuid, Strict),
    PostgresBuiltin("count", &[Any], BigInt, CalledOnNull),
    PostgresBuiltin("sum", &[Any], Numeric, CalledOnNull),
    PostgresBuiltin("avg", &[Any], Numeric, CalledOnNull),
    PostgresBuiltin("min", &[Any], Any, CalledOnNull),
    PostgresBuiltin("max", &[Any], Any, CalledOnNull),
    PostgresBuiltin("current_user", &[], Text, Strict),
    PostgresBuiltin("session_user", &[], Text, Strict),
    PostgresBuiltin("user", &[], Text, CalledOnNull),
];
