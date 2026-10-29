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

use crate::structs::{FunctionKind, metadata::FunctionMetadata};

/// The type of a builtin's argument or result.
#[derive(Clone, Copy)]
pub(super) enum BuiltinType {
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
    Bytea,
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
            Self::Bytea => DataType::Bytea,
            Self::Uuid => DataType::Uuid,
            Self::Date => DataType::Date,
            Self::Interval => DataType::Interval { fields: None, precision: None },
            Self::Time => DataType::Time(None, TimezoneInfo::None),
            Self::TimeTz => DataType::Time(None, TimezoneInfo::WithTimeZone),
            Self::Timestamp => DataType::Timestamp(None, TimezoneInfo::None),
            Self::TimestampTz => DataType::Timestamp(None, TimezoneInfo::WithTimeZone),
        }
    }

    /// Whether a parameter of this type gathers the remaining arguments.
    fn is_variadic(self) -> bool {
        matches!(self, Self::VariadicAny)
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
pub(super) enum Volatility {
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

/// One builtin overload: its name, arguments, result, null-input behavior,
/// volatility and `pg_proc.prokind`.
#[derive(Clone, Copy)]
pub(super) struct PostgresBuiltin(
    &'static str,
    &'static [BuiltinType],
    BuiltinType,
    NullInput,
    Volatility,
    FunctionKind,
);

impl PostgresBuiltin {
    /// The declaration, qualified by `pg_catalog` so a user function may
    /// shadow it the way the server allows.
    pub(super) fn function(&self) -> CreateFunction {
        let Self(name, arguments, returns, null_input, volatility, _) = *self;
        declaration(
            ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                ObjectNamePart::Identifier(Ident::new(name)),
            ]),
            arguments.iter().map(|argument| parameter(None, *argument)).collect(),
            returns,
            matches!(null_input, NullInput::Strict),
            volatility,
            "internal",
        )
    }

    pub(super) fn metadata(&self) -> FunctionMetadata {
        let mut metadata = FunctionMetadata::default();
        if let NullInput::NullAt(positions) = self.3 {
            metadata.set_null_returning_arguments(positions);
        }
        metadata.set_kind(self.5);
        metadata
    }
}

/// A parameter of type `data_type`, called `name` when it has one.
pub(super) fn parameter(name: Option<&'static str>, data_type: BuiltinType) -> OperateFunctionArg {
    OperateFunctionArg {
        // A mode, since `VARIADIC` as a name would invent an argument of that name.
        mode: data_type.is_variadic().then_some(ArgMode::Variadic),
        name: name.map(Ident::new),
        data_type: data_type.data_type(),
        default_expr: None,
    }
}

/// The declaration of a function the server provides, whose body the input
/// never sees.
pub(super) fn declaration(
    name: ObjectName,
    args: Vec<OperateFunctionArg>,
    returns: BuiltinType,
    strict: bool,
    volatility: Volatility,
    language: &str,
) -> CreateFunction {
    CreateFunction {
        or_alter: false,
        or_replace: false,
        temporary: false,
        if_not_exists: false,
        name,
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
        called_on_null: strict.then_some(FunctionCalledOnNull::Strict),
        parallel: None,
        using: None,
        language: Some(Ident::new(language)),
        determinism_specifier: None,
        options: None,
        remote_connection: None,
        security: None,
        set_params: vec![],
    }
}

use BuiltinType::{
    Any, BigInt, Boolean, Bytea, Date, DoublePrecision, Int, Interval, Numeric, Text, TextArray,
    Time, TimeTz, Timestamp, TimestampTz, Uuid, VariadicAny,
};
use FunctionKind::{Aggregate, Normal, Window};
use NullInput::{CalledOnNull, NullAt, Strict};
use Volatility::{Immutable, Stable, Volatile};

/// The builtins a parsed schema can call, strict, volatile and of the kind
/// PostgreSQL 18 declares. The SQL value functions such as `current_date` have
/// no `pg_proc` row, and the planner holds every one of them stable. An
/// aggregate's own row is never strict, since the strictness of its transition
/// function decides which input rows it skips.
pub(super) static POSTGRES_BUILTINS: &[PostgresBuiltin] = &[
    PostgresBuiltin("length", &[Text], Int, Strict, Immutable, Normal),
    PostgresBuiltin("len", &[Text], Int, CalledOnNull, Volatile, Normal),
    PostgresBuiltin("char_length", &[Text], Int, Strict, Immutable, Normal),
    PostgresBuiltin("character_length", &[Text], Int, Strict, Immutable, Normal),
    PostgresBuiltin("octet_length", &[Text], Int, Strict, Immutable, Normal),
    PostgresBuiltin("lower", &[Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("upper", &[Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("btrim", &[Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("btrim", &[Text, Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("split_part", &[Text, Text, Int], Text, Strict, Immutable, Normal),
    PostgresBuiltin("string_to_array", &[Text, Text], TextArray, NullAt(&[0]), Immutable, Normal),
    PostgresBuiltin(
        "string_to_array",
        &[Text, Text, Text],
        TextArray,
        NullAt(&[0]),
        Immutable,
        Normal,
    ),
    PostgresBuiltin("current_setting", &[Text], Text, Strict, Stable, Normal),
    PostgresBuiltin("current_setting", &[Text, Boolean], Text, Strict, Stable, Normal),
    PostgresBuiltin("coalesce", &[VariadicAny], Any, CalledOnNull, Immutable, Normal),
    PostgresBuiltin("nullif", &[Any, Any], Any, NullAt(&[0]), Immutable, Normal),
    PostgresBuiltin("decode", &[Text, Text], Bytea, Strict, Immutable, Normal),
    PostgresBuiltin("now", &[], TimestampTz, Strict, Stable, Normal),
    PostgresBuiltin("current_timestamp", &[], TimestampTz, CalledOnNull, Stable, Normal),
    PostgresBuiltin("current_date", &[], Date, CalledOnNull, Stable, Normal),
    PostgresBuiltin("current_time", &[], TimeTz, CalledOnNull, Stable, Normal),
    PostgresBuiltin("localtimestamp", &[], Timestamp, CalledOnNull, Stable, Normal),
    PostgresBuiltin("localtime", &[], Time, CalledOnNull, Stable, Normal),
    PostgresBuiltin("gen_random_uuid", &[], Uuid, Strict, Volatile, Normal),
    PostgresBuiltin("uuidv4", &[], Uuid, Strict, Volatile, Normal),
    PostgresBuiltin("uuidv7", &[], Uuid, Strict, Volatile, Normal),
    PostgresBuiltin("uuidv7", &[Interval], Uuid, Strict, Volatile, Normal),
    // `count(*)` calls the parameterless overload.
    PostgresBuiltin("count", &[], BigInt, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("count", &[Any], BigInt, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("sum", &[Any], Numeric, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("avg", &[Any], Numeric, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("min", &[Any], Any, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("max", &[Any], Any, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("array_agg", &[Any], Any, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("string_agg", &[Text, Text], Text, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("bool_and", &[Boolean], Boolean, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("bool_or", &[Boolean], Boolean, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("every", &[Boolean], Boolean, CalledOnNull, Immutable, Aggregate),
    // The hypothetical-set aggregates, called `rank(...) WITHIN GROUP (ORDER BY
    // ...)`, share their names with the window functions below.
    PostgresBuiltin("rank", &[VariadicAny], BigInt, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin("dense_rank", &[VariadicAny], BigInt, CalledOnNull, Immutable, Aggregate),
    PostgresBuiltin(
        "percent_rank",
        &[VariadicAny],
        DoublePrecision,
        CalledOnNull,
        Immutable,
        Aggregate,
    ),
    PostgresBuiltin(
        "cume_dist",
        &[VariadicAny],
        DoublePrecision,
        CalledOnNull,
        Immutable,
        Aggregate,
    ),
    PostgresBuiltin("row_number", &[], BigInt, CalledOnNull, Immutable, Window),
    PostgresBuiltin("rank", &[], BigInt, CalledOnNull, Immutable, Window),
    PostgresBuiltin("dense_rank", &[], BigInt, CalledOnNull, Immutable, Window),
    PostgresBuiltin("percent_rank", &[], DoublePrecision, CalledOnNull, Immutable, Window),
    PostgresBuiltin("cume_dist", &[], DoublePrecision, CalledOnNull, Immutable, Window),
    PostgresBuiltin("ntile", &[Int], Int, Strict, Immutable, Window),
    PostgresBuiltin("lag", &[Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("lag", &[Any, Int], Any, Strict, Immutable, Window),
    PostgresBuiltin("lag", &[Any, Int, Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("lead", &[Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("lead", &[Any, Int], Any, Strict, Immutable, Window),
    PostgresBuiltin("lead", &[Any, Int, Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("first_value", &[Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("last_value", &[Any], Any, Strict, Immutable, Window),
    PostgresBuiltin("nth_value", &[Any, Int], Any, Strict, Immutable, Window),
    PostgresBuiltin("current_user", &[], Text, Strict, Stable, Normal),
    PostgresBuiltin("session_user", &[], Text, Strict, Stable, Normal),
    PostgresBuiltin("user", &[], Text, CalledOnNull, Stable, Normal),
    PostgresBuiltin("random", &[], DoublePrecision, Strict, Volatile, Normal),
    PostgresBuiltin("random", &[Int, Int], Int, Strict, Volatile, Normal),
    PostgresBuiltin("random", &[BigInt, BigInt], BigInt, Strict, Volatile, Normal),
    PostgresBuiltin("random", &[Numeric, Numeric], Numeric, Strict, Volatile, Normal),
    PostgresBuiltin("clock_timestamp", &[], TimestampTz, Strict, Volatile, Normal),
    PostgresBuiltin("statement_timestamp", &[], TimestampTz, Strict, Stable, Normal),
    PostgresBuiltin("transaction_timestamp", &[], TimestampTz, Strict, Stable, Normal),
    PostgresBuiltin("timeofday", &[], Text, Strict, Volatile, Normal),
    PostgresBuiltin("md5", &[Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("substring", &[Text, Int], Text, Strict, Immutable, Normal),
    PostgresBuiltin("substring", &[Text, Int, Int], Text, Strict, Immutable, Normal),
    PostgresBuiltin("substring", &[Text, Text], Text, Strict, Immutable, Normal),
    PostgresBuiltin("substring", &[Text, Text, Text], Text, Strict, Immutable, Normal),
];
