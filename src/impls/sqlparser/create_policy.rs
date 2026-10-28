//! Implementation of the `PolicyLike` trait for `CreatePolicy` struct.

use alloc::string::ToString;

use sqlparser::ast::{CreatePolicy, CreatePolicyCommand, CreatePolicyType, Expr, Function, Owner};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TargetName, metadata::PolicyMetadata},
    traits::{DatabaseLike, Metadata, PolicyLike},
    utils::{
        identifier_resolution::is_public_pseudo_role,
        object_name::{resolve_required_bound_table, target_name_of_object_name},
    },
};

impl Metadata for CreatePolicy {
    type Meta = PolicyMetadata<Self>;
}

/// Resolves the metadata `database` holds for `policy`.
fn policy_metadata<'db>(
    policy: &CreatePolicy,
    database: &'db ParserDB,
) -> Result<&'db PolicyMetadata<CreatePolicy>, LookupError> {
    database
        .policy_metadata(policy)
        .ok_or_else(|| ObjectKind::Policy.not_in_database(&policy.name.value))
}

impl PolicyLike for CreatePolicy {
    type DB = ParserDB;

    fn name(&self) -> &str {
        &self.name.value
    }

    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db,
    {
        resolve_required_bound_table(&self.table_name, database)
    }

    fn target_table_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.table_name)
    }

    fn command(&self) -> CreatePolicyCommand {
        self.command.unwrap_or(CreatePolicyCommand::All)
    }

    fn policy_type(&self) -> CreatePolicyType {
        self.policy_type.unwrap_or(CreatePolicyType::Permissive)
    }

    fn roles<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Owner>
    where
        Self: 'db,
    {
        self.to.iter().flat_map(|roles| roles.iter())
    }

    fn applies_to_public(&self) -> bool {
        match &self.to {
            // No `TO` clause at all, which PostgreSQL defaults to `PUBLIC`.
            None => true,
            Some(owners) => {
                owners.is_empty()
                    || owners.iter().any(|owner| {
                        matches!(owner, Owner::Ident(ident)
                        if is_public_pseudo_role(
                            ident.value.as_str(),
                            ident.quote_style.is_some(),
                        ))
                    })
            }
        }
    }

    fn using_expression<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        self.using.as_ref()
    }

    fn using_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        Ok(policy_metadata(self, database)?.using_calls().functions())
    }

    fn check_expression<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        self.with_check.as_ref()
    }

    fn check_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        Ok(policy_metadata(self, database)?.check_calls().functions())
    }

    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        let metadata = policy_metadata(self, database)?;
        self.using
            .as_ref()
            .and_then(|using| metadata.using_calls().reached_by(using, call))
            .or_else(|| {
                self.with_check
                    .as_ref()
                    .and_then(|check| metadata.check_calls().reached_by(check, call))
            })
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }
}
