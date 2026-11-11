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
}
