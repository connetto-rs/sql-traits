//! The metadata ingestion records about a trigger.

use sqlparser::ast::CreateFunction;

use super::ExpressionCalls;
use crate::structs::TriggerEnablement;

/// What ingestion records about a trigger beyond its declaration.
#[derive(Debug, Clone)]
pub struct TriggerMetadata {
    /// The calls of the trigger's `WHEN` condition.
    calls: ExpressionCalls<CreateFunction>,
    /// Whether PostgreSQL created the trigger on a partition as the copy of
    /// a row trigger on the table the partition belongs to.
    cloned: bool,
    /// Which sessions the trigger fires in.
    enablement: TriggerEnablement,
}

impl TriggerMetadata {
    /// The metadata of a trigger a statement creates, which fires in the
    /// `origin` and `local` roles.
    pub(crate) fn new(calls: ExpressionCalls<CreateFunction>) -> Self {
        Self { calls, cloned: false, enablement: TriggerEnablement::Origin }
    }

    /// The metadata of the copy PostgreSQL makes of this trigger on a
    /// partition, which starts with the trigger's enablement.
    pub(crate) fn for_clone(&self) -> Self {
        Self { calls: self.calls.clone(), cloned: true, enablement: self.enablement }
    }

    /// Returns the calls of the trigger's `WHEN` condition, bound when it was
    /// written.
    #[must_use]
    pub fn calls(&self) -> &ExpressionCalls<CreateFunction> {
        &self.calls
    }

    pub(crate) fn calls_mut(&mut self) -> &mut ExpressionCalls<CreateFunction> {
        &mut self.calls
    }

    /// Returns whether the trigger is the copy PostgreSQL keeps on a
    /// partition of a row trigger on the partitioned table above it.
    #[must_use]
    pub fn is_clone(&self) -> bool {
        self.cloned
    }

    /// Returns which sessions the trigger fires in.
    #[must_use]
    pub fn enablement(&self) -> TriggerEnablement {
        self.enablement
    }

    pub(crate) fn set_enablement(&mut self, enablement: TriggerEnablement) {
        self.enablement = enablement;
    }
}
