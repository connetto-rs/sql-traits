//! Procedures, which PostgreSQL keeps in `pg_proc` beside the functions with
//! `prokind = 'p'`, and the statement keywords choosing among the kinds.
//!
//! A `CREATE PROCEDURE` is held as the [`CreateFunction`] declaring the same
//! routine, with no return type and its metadata answering
//! [`FunctionKind::Procedure`], so the two kinds share one store and one
//! signature space, as they share `pg_proc`.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{
    ArgMode, CreateFunction, CreateFunctionBody, CreateProcedureBody, FunctionDesc, Ident,
    ObjectName, OperateFunctionArg, ProcedureParam,
};

use crate::{
    errors::Error,
    impls::SqlparserDialect,
    structs::{FunctionKind, ParserDBBuilder},
    utils::{postgres_types::PgType, quote_identifier::quote_identifier},
};

/// The signature PostgreSQL prints for an unresolved routine reference.
pub(super) fn diagnostic_signature(
    dialect: SqlparserDialect,
    function: &FunctionDesc,
    format_type: impl Fn(&PgType) -> String,
) -> Option<String> {
    function.args.as_ref().map(|arguments| {
        let mut signature = dialect.stored_object_name(&function.name);
        append_arguments(&mut signature, arguments, ", ", &format_type);
        signature
    })
}

/// The signature PostgreSQL prints for a stored routine.
pub(super) fn diagnostic_definition(
    builder: &ParserDBBuilder,
    function: &CreateFunction,
) -> String {
    let visible = function.name.0.last().is_some_and(|last| {
        let unqualified = ObjectName(vec![last.clone()]);
        let arguments = super::function_argument_types(function.args.as_deref());
        super::routines_on_path(
            builder,
            &unqualified,
            Some(&arguments),
            super::function_argument_types,
        )
        .first()
        .is_some_and(|position| core::ptr::eq(builder.functions()[*position].0.as_ref(), function))
    });
    let mut signature = String::new();
    let parts = builder.dialect().stored_object_name_parts(&function.name);
    let first = if visible { parts.len().saturating_sub(1) } else { 0 };
    for part in &parts[first..] {
        if !signature.is_empty() {
            signature.push('.');
        }
        signature.push_str(&quote_identifier(part));
    }
    append_arguments(
        &mut signature,
        function.args.as_deref().unwrap_or_default(),
        ",",
        &|data_type| super::domains::format_type_for_builder(builder, data_type),
    );
    signature
}

/// Appends the input argument types of a routine.
fn append_arguments(
    signature: &mut String,
    arguments: &[OperateFunctionArg],
    separator: &str,
    format_type: &impl Fn(&PgType) -> String,
) {
    signature.push('(');
    let mut first = true;
    for argument in arguments.iter().filter(|argument| argument.mode != Some(ArgMode::Out)) {
        if !first {
            signature.push_str(separator);
        }
        first = false;
        signature.push_str(&format_type(&PgType::of(&argument.data_type)));
    }
    signature.push(')');
}

/// Whether `dialect` holds procedures, which the dialects following
/// PostgreSQL's `pg_proc` do. Another engine's procedures share no signature
/// space with its functions, so they are left out.
pub(super) const fn holds_procedures(dialect: SqlparserDialect) -> bool {
    matches!(dialect, SqlparserDialect::PostgreSql | SqlparserDialect::Generic)
}

/// The function node declaring the procedure a `CREATE PROCEDURE` declares.
///
/// # Errors
///
/// Returns [`Error::ProcedureBodyUnsupported`] for a body of bare statements
/// after `AS`, which PostgreSQL does not read.
pub(super) fn procedure_function(
    or_alter: bool,
    name: ObjectName,
    params: Option<Vec<ProcedureParam>>,
    language: Option<Ident>,
    body: CreateProcedureBody,
    dialect: SqlparserDialect,
) -> Result<CreateFunction, Error> {
    let function_body = match body {
        CreateProcedureBody::AsString { body, link_symbol } => {
            CreateFunctionBody::AsBeforeOptions { body, link_symbol }
        }
        CreateProcedureBody::BeginAtomic(block) => CreateFunctionBody::BeginAtomic(block),
        CreateProcedureBody::As(statements) => {
            let written_token =
                statements.to_string().split_whitespace().next().unwrap_or_default().to_string();
            return Err(Error::ProcedureBodyUnsupported {
                procedure_name: dialect.stored_object_name_last(&name),
                written_token,
            });
        }
    };
    let args = params.map(|params| {
        params
            .into_iter()
            .map(|param| {
                OperateFunctionArg {
                    mode: param.mode,
                    name: Some(param.name),
                    data_type: param.data_type,
                    default_expr: param.default,
                }
            })
            .collect()
    });
    Ok(CreateFunction {
        or_alter,
        or_replace: false,
        temporary: false,
        if_not_exists: false,
        name,
        args,
        return_type: None,
        function_body: Some(function_body),
        behavior: None,
        called_on_null: None,
        parallel: None,
        security: None,
        leakproof: None,
        window: false,
        cost: None,
        rows: None,
        support: None,
        set_params: Vec::new(),
        using: None,
        language,
        determinism_specifier: None,
        options: None,
        remote_connection: None,
    })
}

pub(super) use crate::errors::RoutineKeyword;

impl RoutineKeyword {
    /// Whether the keyword reaches a routine of `kind`.
    pub(super) const fn admits(self, kind: FunctionKind) -> bool {
        match self {
            Self::Function => !matches!(kind, FunctionKind::Procedure),
            Self::Procedure => matches!(kind, FunctionKind::Procedure),
            Self::Routine => true,
        }
    }

    /// Refuses `signature` reaching a routine of `kind` the keyword does not
    /// admit, as PostgreSQL refuses a signature it found among every routine
    /// once the kind does not match.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAFunction`] for a procedure `FUNCTION` names, and
    /// [`Error::NotAProcedure`] for any other routine `PROCEDURE` names.
    pub(super) fn refuse_other_kind(
        self,
        signature: &FunctionDesc,
        kind: FunctionKind,
        builder: &ParserDBBuilder,
    ) -> Result<(), Error> {
        if self.admits(kind) {
            return Ok(());
        }
        match self {
            Self::Procedure => {
                Err(Error::NotAProcedure {
                    procedure_name: builder.dialect().stored_object_name(&signature.name),
                    signature: diagnostic_signature(*builder.dialect(), signature, |data_type| {
                        super::domains::format_type_for_builder(builder, data_type)
                    })
                    .unwrap_or_else(|| builder.dialect().stored_object_name(&signature.name)),
                })
            }
            Self::Function | Self::Routine => {
                Err(Error::NotAFunction {
                    function_name: builder.dialect().stored_object_name(&signature.name),
                    signature: diagnostic_signature(*builder.dialect(), signature, |data_type| {
                        super::domains::format_type_for_builder(builder, data_type)
                    })
                    .unwrap_or_else(|| builder.dialect().stored_object_name(&signature.name)),
                })
            }
        }
    }
}
