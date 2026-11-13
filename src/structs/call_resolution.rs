//! The function PostgreSQL selects for a call and the type each argument
//! takes.

use alloc::vec::Vec;

use crate::{
    structs::{OperandEvidence, TargetName},
    traits::DatabaseLike,
};

/// What PostgreSQL does with a function call.
#[derive(Debug, Clone)]
pub enum CallResolution<'db, DB: DatabaseLike> {
    /// PostgreSQL runs this function.
    Selected(SelectedFunction<'db, DB>),
    /// No function of the name takes the argument types.
    NotFound,
    /// These functions take the argument types and nothing tells them apart.
    Ambiguous(Vec<&'db DB::Function>),
    /// PostgreSQL selects this function, then refuses the call, since only
    /// untyped literals reach a polymorphic parameter.
    UnboundPolymorphic(&'db DB::Function),
    /// An argument type or a fact selection needs is missing.
    Undetermined,
}

/// The function PostgreSQL selects and what each argument takes.
#[derive(Debug, Clone)]
pub struct SelectedFunction<'db, DB: DatabaseLike> {
    function: &'db DB::Function,
    arguments: Vec<ArgumentEvidence<'db, DB>>,
    defaulted: usize,
    variadic: Option<TargetName<'db>>,
    result: Option<TargetName<'db>>,
}

impl<'db, DB: DatabaseLike> SelectedFunction<'db, DB> {
    /// Creates the selection of `function`, with the evidence of each
    /// written argument, how many parameters a default fills, the array a
    /// `VARIADIC` parameter collects arguments into and the type the call
    /// returns.
    pub(crate) const fn new(
        function: &'db DB::Function,
        arguments: Vec<ArgumentEvidence<'db, DB>>,
        defaulted: usize,
        variadic: Option<TargetName<'db>>,
        result: Option<TargetName<'db>>,
    ) -> Self {
        Self { function, arguments, defaulted, variadic, result }
    }

    /// Returns the selected function.
    #[must_use]
    pub const fn function(&self) -> &'db DB::Function {
        self.function
    }

    /// Returns what each argument takes, one entry per argument the call
    /// writes, in the order it writes them.
    #[must_use]
    pub fn arguments(&self) -> &[ArgumentEvidence<'db, DB>] {
        &self.arguments
    }

    /// Returns how many input parameters the call leaves to their defaults.
    #[must_use]
    pub const fn defaulted(&self) -> usize {
        self.defaulted
    }

    /// Returns the array type a `VARIADIC` parameter collects the trailing
    /// positional arguments into, or `None` when the call passes it none
    /// that way, as a call in named notation and a `VARIADIC "any"`
    /// parameter do.
    #[must_use]
    pub const fn variadic(&self) -> Option<&TargetName<'db>> {
        self.variadic.as_ref()
    }

    /// Returns the type the call returns, the function's declared result
    /// or, for a polymorphic one, the type the arguments bind it to, or
    /// `None` when the database does not answer it.
    #[must_use]
    pub const fn result_type(&self) -> Option<&TargetName<'db>> {
        self.result.as_ref()
    }

    /// Moves out the type the call returns.
    pub(crate) fn into_result_type(self) -> Option<TargetName<'db>> {
        self.result
    }
}

/// The parameter one written argument passes and what it takes there.
#[derive(Debug, Clone)]
pub struct ArgumentEvidence<'db, DB: DatabaseLike> {
    parameter: usize,
    evidence: OperandEvidence<'db, DB>,
}

impl<'db, DB: DatabaseLike> ArgumentEvidence<'db, DB> {
    /// Creates the evidence of an argument passing the input parameter at
    /// `parameter`.
    pub(crate) const fn new(parameter: usize, evidence: OperandEvidence<'db, DB>) -> Self {
        Self { parameter, evidence }
    }

    /// Returns the position of the parameter the argument passes among the
    /// function's input parameters, the `VARIADIC` one for every argument it
    /// collects.
    #[must_use]
    pub const fn parameter(&self) -> usize {
        self.parameter
    }

    /// Returns the type the argument takes and how PostgreSQL converts it
    /// there.
    #[must_use]
    pub const fn evidence(&self) -> &OperandEvidence<'db, DB> {
        &self.evidence
    }
}
