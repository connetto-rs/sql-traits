//! The operator PostgreSQL selects for an operator invocation and the type
//! each operand takes.

use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::{structs::TargetName, traits::DatabaseLike};

/// The type of one operand of an operator invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OperandType<'a> {
    /// A value of the named type.
    Known(TargetName<'a>),
    /// An untyped literal such as `'admin'` or `NULL`, of type `unknown`.
    UntypedLiteral,
    /// A value whose type the caller does not know.
    Unknown,
}

/// What PostgreSQL does with an operator invocation.
#[derive(Debug, Clone)]
pub enum OperatorResolution<'db, DB: DatabaseLike> {
    /// PostgreSQL runs this operator.
    Selected(SelectedOperator<'db, DB>),
    /// No operator of the name takes the operand types.
    NotFound,
    /// These operators take the operand types and nothing tells them apart.
    Ambiguous(Vec<&'db DB::Operator>),
    /// PostgreSQL selects this operator, then refuses the invocation, since
    /// only untyped literals reach a polymorphic operand.
    UnboundPolymorphic(&'db DB::Operator),
    /// The operator catalog, an operand type or a fact selection needs is
    /// missing.
    Undetermined,
}

/// The operator PostgreSQL selects and what each operand takes.
#[derive(Debug, Clone)]
pub struct SelectedOperator<'db, DB: DatabaseLike> {
    operator: &'db DB::Operator,
    left: Option<OperandEvidence<'db, DB>>,
    right: OperandEvidence<'db, DB>,
}

impl<'db, DB: DatabaseLike> SelectedOperator<'db, DB> {
    /// Creates the selection of `operator` with the evidence of each operand.
    pub(crate) const fn new(
        operator: &'db DB::Operator,
        left: Option<OperandEvidence<'db, DB>>,
        right: OperandEvidence<'db, DB>,
    ) -> Self {
        Self { operator, left, right }
    }

    /// Returns the selected operator.
    #[must_use]
    pub const fn operator(&self) -> &'db DB::Operator {
        self.operator
    }

    /// Returns what the left operand takes, or `None` for a prefix operator.
    #[must_use]
    pub const fn left(&self) -> Option<&OperandEvidence<'db, DB>> {
        self.left.as_ref()
    }

    /// Returns what the right operand takes.
    #[must_use]
    pub const fn right(&self) -> &OperandEvidence<'db, DB> {
        &self.right
    }
}

/// The type one operand takes in the selected operator.
#[derive(Debug, Clone)]
pub struct OperandEvidence<'db, DB: DatabaseLike> {
    target: TargetName<'db>,
    polymorphic_binding: bool,
    database: PhantomData<&'db DB>,
}

impl<'db, DB: DatabaseLike> OperandEvidence<'db, DB> {
    /// Creates the evidence of an operand that takes `target`.
    pub(crate) const fn new(target: TargetName<'db>, polymorphic_binding: bool) -> Self {
        Self { target, polymorphic_binding, database: PhantomData }
    }

    /// Returns the type the operand takes, the operator's declared type or,
    /// for a polymorphic operand, the type the binding chose.
    #[must_use]
    pub const fn target(&self) -> &TargetName<'db> {
        &self.target
    }

    /// Returns whether a polymorphic binding chose the target type.
    #[must_use]
    pub const fn is_polymorphic_binding(&self) -> bool {
        self.polymorphic_binding
    }
}
