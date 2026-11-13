//! Catalog operators and their declared signatures.

use core::{fmt::Debug, hash::Hash};

use crate::{
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
};

/// The operand positions of an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperatorKind {
    /// One operand to the right of the operator.
    Prefix,
    /// An operand on each side of the operator.
    Binary,
}

/// A catalog operator whose signature does not imply an overload selection.
pub trait OperatorLike: Metadata + Debug + Clone + Hash + Ord + Eq + Send + Sync {
    /// The database that owns this operator.
    type DB: DatabaseLike<Operator = Self>;

    /// Returns the operator name and its schema.
    fn target_name(&self) -> TargetName<'_>;

    /// Returns the operand positions independently of type availability.
    fn kind(&self) -> OperatorKind;

    /// Returns the qualified left operand type, or `None` when absent or
    /// unknown.
    fn left_type<'db>(&'db self, database: &'db Self::DB) -> Option<TargetName<'db>>;

    /// Returns the qualified right operand type, or `None` when unknown.
    fn right_type<'db>(&'db self, database: &'db Self::DB) -> Option<TargetName<'db>>;

    /// Returns the qualified result type, or `None` for a shell or unknown
    /// result.
    fn result_type<'db>(&'db self, database: &'db Self::DB) -> Option<TargetName<'db>>;

    /// Returns the exact implementing function, or `None` for a shell or
    /// unknown implementation.
    fn implementation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>;

    /// Returns the commutator, the operator over the swapped operands that
    /// `a op b` equals `b commutator a` under, or `None` when the operator
    /// names none or the database holds no operator it names.
    fn commutator<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Self>;

    /// Returns the negator, the operator over the same operands that
    /// `a op b` equals `NOT (a negator b)` under, or `None` when the operator
    /// names none or the database holds no operator it names.
    fn negator<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Self>;

    /// Returns whether the operator supports hash joins.
    fn can_hash(&self) -> bool;

    /// Returns whether the operator supports merge joins.
    fn can_merge(&self) -> bool;

    /// Returns the function estimating the selectivity of a restriction
    /// applying the operator, or `None` when the operator names none or the
    /// database holds no function it names.
    fn restriction_estimator<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>;

    /// Returns the function estimating the selectivity of a join applying
    /// the operator, or `None` when the operator names none or the database
    /// holds no function it names.
    fn join_estimator<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>;
}
