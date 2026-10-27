//! Implementation of the `RuleLike` trait for sqlparser's `CreateRule` type.

use alloc::string::ToString;

use sqlparser::ast::{CreateRule, CreateRuleAction, CreateRuleDoKind, CreateRuleEvent, Expr};

use crate::{
    errors::LookupError,
    structs::{IdentifierCase, ParserDB, TargetName},
    traits::{DatabaseLike, Metadata, RuleLike, RuleRelation},
    utils::object_name::{
        object_name_identifiers, resolve_bound_table, target_name_of_idents,
        target_name_of_object_name,
    },
};

impl Metadata for CreateRule {
    type Meta = ();
}

impl RuleLike for CreateRule {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        &self.name.value
    }

    #[inline]
    fn target_relation_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.table_name)
    }

    /// Ingestion bound the target to the relation it reached, so the name is
    /// read as written with no search path, a table first and then a view,
    /// since one name holds at most one of the two.
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<RuleRelation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        if let Some(table) = resolve_bound_table(&self.table_name, database)? {
            return Ok(RuleRelation::Table(table));
        }
        let (schema_ident, name_ident) = object_name_identifiers(&self.table_name)?;
        database
            .view_by_target(
                target_name_of_idents(schema_ident, name_ident),
                IdentifierCase::AsWritten,
            )?
            .map(RuleRelation::View)
            .ok_or_else(|| {
                LookupError::RelationNotFound { object_name: self.table_name.to_string() }
            })
    }

    #[inline]
    fn event(&self) -> CreateRuleEvent {
        self.event
    }

    #[inline]
    fn is_instead(&self) -> bool {
        matches!(self.do_kind, Some(CreateRuleDoKind::Instead))
    }

    #[inline]
    fn condition(&self) -> Option<&Expr> {
        self.condition.as_ref()
    }

    #[inline]
    fn action(&self) -> &CreateRuleAction {
        &self.action
    }
}
