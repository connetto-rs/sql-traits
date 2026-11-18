//! Procedures, which PostgreSQL keeps in `pg_proc` beside the functions with
//! `prokind = 'p'`, and the statement keywords choosing among the kinds.
//!
//! A `CREATE PROCEDURE` is held as the [`CreateFunction`] declaring the same
//! routine, with no return type and its metadata answering
//! [`FunctionKind::Procedure`], so the two kinds share one store and one
//! signature space, as they share `pg_proc`.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{
    CreateFunction, CreateFunctionBody, CreateProcedureBody, FunctionDesc, Ident, ObjectName,
    OperateFunctionArg, ProcedureParam,
};

use crate::{errors::Error, impls::SqlparserDialect, structs::FunctionKind, utils::last_str};

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
) -> Result<CreateFunction, Error> {
    let function_body = match body {
        CreateProcedureBody::AsString { body, link_symbol } => {
            CreateFunctionBody::AsBeforeOptions { body, link_symbol }
        }
        CreateProcedureBody::BeginAtomic(block) => CreateFunctionBody::BeginAtomic(block),
        CreateProcedureBody::As(_) => {
            return Err(Error::ProcedureBodyUnsupported {
                procedure_name: last_str(&name).to_string(),
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

/// The kind of routine a statement names, by its keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoutineKeyword {
    /// `FUNCTION`, any routine but a procedure.
    Function,
    /// `PROCEDURE`, a procedure alone.
    Procedure,
    /// `ROUTINE`, any routine.
    Routine,
}

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
    ) -> Result<(), Error> {
        if self.admits(kind) {
            return Ok(());
        }
        match self {
            Self::Procedure => Err(Error::NotAProcedure { procedure_name: signature.to_string() }),
            Self::Function | Self::Routine => {
                Err(Error::NotAFunction { function_name: signature.to_string() })
            }
        }
    }
}
