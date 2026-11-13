//! The operator PostgreSQL selects for an operator invocation and the type
//! each operand takes.

use alloc::vec::Vec;

use crate::{structs::TargetName, traits::DatabaseLike};

/// The type of one operand of an operator invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OperandType<'a> {
    /// A value of the named type, carrying the type modifier `typmod`, as
    /// `pg_attribute.atttypmod` stores it, or none.
    Known {
        /// The type of the value.
        type_name: TargetName<'a>,
        /// The type modifier the value carries, or `None` for none.
        typmod: Option<i32>,
    },
    /// An untyped literal such as `'admin'` or `NULL`, of type `unknown`.
    UntypedLiteral,
    /// A value whose type the caller does not know.
    Unknown,
}

/// What PostgreSQL does with an operator invocation.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "A resolution is returned once per invocation and moved, so boxing the selection \
              would add a heap allocation to every invocation."
)]
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
    result: Option<TargetName<'db>>,
}

impl<'db, DB: DatabaseLike> SelectedOperator<'db, DB> {
    /// Creates the selection of `operator` with the evidence of each operand
    /// and the type it returns.
    pub(crate) const fn new(
        operator: &'db DB::Operator,
        left: Option<OperandEvidence<'db, DB>>,
        right: OperandEvidence<'db, DB>,
        result: Option<TargetName<'db>>,
    ) -> Self {
        Self { operator, left, right, result }
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

    /// Returns the type the invocation returns, the operator's declared
    /// result or, for a polymorphic one, the type the operands bind it to,
    /// or `None` when the database does not answer it.
    #[must_use]
    pub const fn result_type(&self) -> Option<&TargetName<'db>> {
        self.result.as_ref()
    }

    /// Moves out the type the invocation returns.
    pub(crate) fn into_result_type(self) -> Option<TargetName<'db>> {
        self.result
    }

    /// Moves out what the left operand takes.
    pub(crate) fn into_left(self) -> Option<OperandEvidence<'db, DB>> {
        self.left
    }
}

/// The type one operand takes in the selected operator and how PostgreSQL
/// converts it there.
#[derive(Debug, Clone)]
pub struct OperandEvidence<'db, DB: DatabaseLike> {
    target: TargetName<'db>,
    polymorphic_binding: bool,
    conversion: Vec<CoercionStep<'db, DB>>,
}

impl<'db, DB: DatabaseLike> OperandEvidence<'db, DB> {
    /// Creates the evidence of an operand that takes `target` by
    /// `conversion`.
    pub(crate) const fn new(
        target: TargetName<'db>,
        polymorphic_binding: bool,
        conversion: Vec<CoercionStep<'db, DB>>,
    ) -> Self {
        Self { target, polymorphic_binding, conversion }
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

    /// Returns the steps that carry the operand to the target type, in the
    /// order PostgreSQL applies them, empty when the operand already has it.
    #[must_use]
    pub fn conversion(&self) -> &[CoercionStep<'db, DB>] {
        &self.conversion
    }

    /// Moves out the type the operand takes.
    pub(crate) fn into_target(self) -> TargetName<'db> {
        self.target
    }
}

/// One step of the conversion PostgreSQL applies to an operand.
#[derive(Debug, Clone)]
pub enum CoercionStep<'db, DB: DatabaseLike> {
    /// The input function of this type reads an untyped literal, the base
    /// type's for a domain target.
    LiteralInput {
        /// The type whose input function reads the literal.
        type_name: TargetName<'db>,
    },
    /// A domain value is read as its base type.
    DomainToBase {
        /// The base type of the domain.
        base: TargetName<'db>,
    },
    /// A binary-coercible cast relabels the value.
    Relabel(&'db DB::Cast),
    /// A cast function converts the value.
    Function(&'db DB::Cast),
    /// The output function of the source type and the input function of the
    /// target type convert the value.
    InOut(&'db DB::Cast),
    /// Each element of an array is converted by this plan.
    Array {
        /// The plan applied to each element.
        element: Vec<CoercionStep<'db, DB>>,
    },
    /// A `record` value is read as the composite target type.
    RecordToComposite,
    /// A composite value, or an array of them, is read as `record`.
    CompositeToRecord,
    /// The row of a table is converted to the row of a table it inherits
    /// from or is a partition of.
    Inherited,
    /// The length coercion of the base type applies the modifier a domain
    /// declares for it.
    LengthCoercion {
        /// The cast of the base type to itself that applies the modifier.
        cast: &'db DB::Cast,
        /// The modifier, as `pg_type.typtypmod` stores it.
        typmod: i32,
    },
    /// The value is checked against the constraints of the domain.
    DomainCheck {
        /// The domain the value converts to.
        domain: TargetName<'db>,
    },
}
