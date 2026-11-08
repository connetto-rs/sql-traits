//! Implementation of the `PolicyLike` trait for `CreatePolicy` struct.

use alloc::string::ToString;

use sqlparser::ast::{
    CreateFunction, CreatePolicy, CreatePolicyCommand, CreatePolicyType, Expr, Function, Owner,
};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{
        ParserDB, PolicyExpression, TargetName,
        metadata::{ClauseLiterals, ExpressionCalls, PolicyMetadata},
    },
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

/// The clause `stored` of `policy`, written when `literals` names a literal
/// of it whose stored value is not known.
fn clause<'db>(
    policy: &CreatePolicy,
    stored: &'db Expr,
    literals: &ClauseLiterals,
) -> PolicyExpression<'db> {
    match literals {
        ClauseLiterals::Unreadable(literal) => {
            PolicyExpression::Written {
                expression: stored,
                unreadable: literal.error(&policy.name.value),
            }
        }
        ClauseLiterals::AsWritten | ClauseLiterals::Stored(_) => PolicyExpression::Stored(stored),
    }
}

/// The clause `stored` as the input spells it.
fn written<'db>(stored: &'db Expr, literals: &'db ClauseLiterals) -> &'db Expr {
    match literals {
        ClauseLiterals::Stored(written) => written,
        ClauseLiterals::AsWritten | ClauseLiterals::Unreadable(_) => stored,
    }
}

/// The functions `call` may reach, when it is a node of the clause `stored`
/// or of that clause as written, where a call stands at the same place among
/// the calls as in the stored clause.
fn reached<'db>(
    stored: &Expr,
    literals: &ClauseLiterals,
    calls: &'db ExpressionCalls<CreateFunction>,
    call: &Function,
) -> Option<impl Iterator<Item = &'db CreateFunction> + use<'db>> {
    calls.reached_by(stored, call).or_else(|| {
        match literals {
            ClauseLiterals::Stored(written) => calls.reached_by(written, call),
            ClauseLiterals::AsWritten | ClauseLiterals::Unreadable(_) => None,
        }
    })
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
        let named = if self.applies_to_public() { None } else { self.to.as_deref() };
        named.into_iter().flatten()
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

    fn using_clause<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<PolicyExpression<'db>>, LookupError>
    where
        Self: 'db,
    {
        let literals = policy_metadata(self, database)?.using_literals();
        Ok(self.using.as_ref().map(|using| clause(self, using, literals)))
    }

    fn written_using<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db Expr>, LookupError>
    where
        Self: 'db,
    {
        let literals = policy_metadata(self, database)?.using_literals();
        Ok(self.using.as_ref().map(|using| written(using, literals)))
    }

    fn using_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        Ok(policy_metadata(self, database)?.using_calls().functions())
    }

    fn check_clause<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<PolicyExpression<'db>>, LookupError>
    where
        Self: 'db,
    {
        let literals = policy_metadata(self, database)?.check_literals();
        Ok(self.with_check.as_ref().map(|check| clause(self, check, literals)))
    }

    fn written_check<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db Expr>, LookupError>
    where
        Self: 'db,
    {
        let literals = policy_metadata(self, database)?.check_literals();
        Ok(self.with_check.as_ref().map(|check| written(check, literals)))
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
            .and_then(|using| {
                reached(using, metadata.using_literals(), metadata.using_calls(), call)
            })
            .or_else(|| {
                self.with_check.as_ref().and_then(|check| {
                    reached(check, metadata.check_literals(), metadata.check_calls(), call)
                })
            })
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }
}
