//! Submodule defining a name a function body leaves to the search path.

use alloc::vec::Vec;
use core::fmt::Debug;

use crate::{
    structs::TargetName,
    traits::{DatabaseLike, Relation},
};

/// A relation or function name a function body writes without a schema, so
/// the search path in force where it runs decides what it reaches, as
/// [`FunctionLike::unqualified_names`](crate::traits::FunctionLike::unqualified_names)
/// answers it.
///
/// The name is spelled as the body writes it, with whether it was quoted,
/// and carries no schema. What it reached is read under the path the reader
/// resolved it with, so a caller running the function under another path may
/// reach something else, which is why the name is reported.
pub enum UnqualifiedName<'db, DB: DatabaseLike> {
    /// A relation name, with the relation it reached, or [`None`] when no
    /// relation the database holds answers it.
    Relation {
        /// The name as written.
        name: TargetName<'static>,
        /// The relation the name reached.
        relation: Option<Relation<'db, DB>>,
    },
    /// The name of a call, with every function it may reach, which is empty
    /// when no function the database holds answers it.
    Function {
        /// The name as written.
        name: TargetName<'static>,
        /// Every function the call may reach, each overload accepting its
        /// argument count included.
        functions: Vec<&'db DB::Function>,
    },
}

impl<DB: DatabaseLike> UnqualifiedName<'_, DB> {
    /// Returns the name as the body writes it.
    #[must_use]
    #[inline]
    pub fn name(&self) -> &TargetName<'static> {
        match self {
            Self::Relation { name, .. } | Self::Function { name, .. } => name,
        }
    }
}

impl<DB: DatabaseLike> Clone for UnqualifiedName<'_, DB> {
    fn clone(&self) -> Self {
        match self {
            Self::Relation { name, relation } => {
                Self::Relation { name: name.clone(), relation: *relation }
            }
            Self::Function { name, functions } => {
                Self::Function { name: name.clone(), functions: functions.clone() }
            }
        }
    }
}

impl<DB: DatabaseLike> Debug for UnqualifiedName<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Relation { name, relation } => {
                f.debug_struct("Relation").field("name", name).field("relation", relation).finish()
            }
            Self::Function { name, functions } => {
                f.debug_struct("Function")
                    .field("name", name)
                    .field("functions", functions)
                    .finish()
            }
        }
    }
}
