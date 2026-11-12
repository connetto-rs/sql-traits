//! Catalog casts between data types.

use core::{fmt::Debug, hash::Hash};

use crate::{
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
};

/// The contexts in which PostgreSQL applies a cast without being asked, as
/// `pg_cast.castcontext` records them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CastContext {
    /// Only an explicit `CAST` or `::` applies the cast.
    Explicit,
    /// Assignment to a column of the target type also applies the cast.
    Assignment,
    /// Any expression context also applies the cast.
    Implicit,
}

/// How a cast converts a value, as `pg_cast.castmethod` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CastMethod {
    /// A function converts the value.
    Function,
    /// The source type's output function and the target type's input
    /// function convert the value.
    InOut,
    /// The types share a representation, so the value is relabelled.
    Binary,
}

/// A catalog cast from one data type to another.
pub trait CastLike: Metadata + Debug + Clone + Hash + Ord + Eq + Send + Sync {
    /// The database that owns this cast.
    type DB: DatabaseLike<Cast = Self>;

    /// Returns the qualified source type.
    fn source_type<'db>(&'db self, database: &'db Self::DB) -> TargetName<'db>;

    /// Returns the qualified target type.
    fn target_type<'db>(&'db self, database: &'db Self::DB) -> TargetName<'db>;

    /// Returns the contexts that apply the cast without being asked.
    fn context(&self) -> CastContext;

    /// Returns how the cast converts a value.
    fn method(&self) -> CastMethod;

    /// Returns the exact implementing function of a [`CastMethod::Function`]
    /// cast, or `None` when the database does not hold it.
    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>;
}
