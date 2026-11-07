//! The kind of schema object a database engine does not have.

use alloc::string::String;
use core::{
    cmp::Ordering,
    convert::Infallible,
    fmt::{self, Debug, Formatter},
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use sqlparser::ast::{
    CreatePolicyCommand, CreatePolicyType, CreateRuleAction, CreateRuleEvent, Expr, Function,
    Owner, Query,
};

use crate::{
    errors::LookupError,
    structs::{SequenceParameters, TargetName},
    traits::{
        DatabaseLike, DomainLike, ExtensionLike, Metadata, PolicyLike, Relation, RuleLike,
        SequenceLike, SequenceOrigin, SequenceOwner, UserDefinedTypeKind, UserDefinedTypeLike,
        ViewLike,
    },
};

/// A schema object of a kind the engine behind `DB` does not have.
///
/// A [`SchemaProfile`](crate::structs::SchemaProfile) names it for every kind
/// its engine lacks, as in `type Policy = Absent<GenericDB<Self>>` for an
/// engine without row-level security. It stands for policies, rules,
/// extensions, domains, sequences and plain or materialized views. No value
/// of this type can exist, so a database holds none of that kind and every
/// listing of it is empty.
pub struct Absent<DB> {
    /// Leaves the type without values.
    never: Infallible,
    /// Ties the type to its database without owning one, so it is `Send`
    /// and `Sync` whatever `DB` is.
    database: PhantomData<fn() -> DB>,
}

impl<DB> Absent<DB> {
    /// Returns any type, since no `Absent` exists to call this on.
    const fn unreachable<T>(&self) -> T {
        match self.never {}
    }
}

impl<DB> Debug for Absent<DB> {
    fn fmt(&self, _: &mut Formatter<'_>) -> fmt::Result {
        self.unreachable()
    }
}

impl<DB> Clone for Absent<DB> {
    fn clone(&self) -> Self {
        self.unreachable()
    }
}

impl<DB> PartialEq for Absent<DB> {
    fn eq(&self, _: &Self) -> bool {
        self.unreachable()
    }
}

impl<DB> Eq for Absent<DB> {}

impl<DB> PartialOrd for Absent<DB> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<DB> Ord for Absent<DB> {
    fn cmp(&self, _: &Self) -> Ordering {
        self.unreachable()
    }
}

impl<DB> Hash for Absent<DB> {
    fn hash<H: Hasher>(&self, _: &mut H) {
        match self.never {}
    }
}

impl<DB> Metadata for Absent<DB> {
    type Meta = ();
}

impl<DB: DatabaseLike> ViewLike for Absent<DB> {
    type DB = DB;

    fn view_name(&self) -> &str {
        self.unreachable()
    }

    fn view_schema(&self) -> Option<&str> {
        self.unreachable()
    }

    fn is_materialized(&self) -> bool {
        self.unreachable()
    }

    fn definition(&self) -> &Query {
        self.unreachable()
    }

    fn owner<'db>(&self, _: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        self.unreachable()
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        self.unreachable()
    }
}

impl<DB: DatabaseLike> SequenceLike for Absent<DB> {
    type DB = DB;

    fn sequence_name(&self) -> &str {
        self.unreachable()
    }

    fn sequence_schema(&self) -> Option<&str> {
        self.unreachable()
    }

    fn parameters(&self) -> SequenceParameters {
        self.unreachable()
    }

    fn owner<'db>(&'db self, _: &'db Self::DB) -> Option<SequenceOwner<'db, Self::DB>>
    where
        Self: 'db,
    {
        self.unreachable()
    }

    fn origin(&self) -> SequenceOrigin {
        self.unreachable()
    }
}

impl<DB: DatabaseLike> ExtensionLike for Absent<DB> {
    type DB = DB;

    fn name(&self) -> &str {
        self.unreachable()
    }

    fn schema(&self) -> Option<&str> {
        self.unreachable()
    }

    fn version(&self) -> Option<&str> {
        self.unreachable()
    }
}

impl<DB: DatabaseLike> DomainLike for Absent<DB> {
    type DB = DB;

    fn name(&self) -> &str {
        self.unreachable()
    }

    fn schema(&self) -> Option<&str> {
        self.unreachable()
    }

    fn data_type<'db>(&'db self, _: &'db Self::DB) -> &'db DB::DataType {
        self.unreachable()
    }
}

impl<DB: DatabaseLike> UserDefinedTypeLike for Absent<DB> {
    type DB = DB;

    fn name(&self) -> &str {
        self.unreachable()
    }

    fn name_is_quoted(&self) -> bool {
        self.unreachable()
    }

    fn schema(&self) -> Option<&str> {
        self.unreachable()
    }

    fn schema_is_quoted(&self) -> bool {
        self.unreachable()
    }

    fn kind(&self) -> UserDefinedTypeKind {
        self.unreachable()
    }
}

impl<DB: DatabaseLike> RuleLike for Absent<DB> {
    type DB = DB;

    fn name(&self) -> &str {
        self.unreachable()
    }

    fn target_relation_name(&self) -> TargetName<'_> {
        self.unreachable()
    }

    fn relation<'db>(&'db self, _: &'db Self::DB) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        self.unreachable()
    }

    fn event(&self) -> CreateRuleEvent {
        self.unreachable()
    }

    fn is_instead(&self) -> bool {
        self.unreachable()
    }

    fn condition(&self) -> Option<&Expr> {
        self.unreachable()
    }

    fn action(&self) -> &CreateRuleAction {
        self.unreachable()
    }
}

impl<DB: DatabaseLike<Policy = Self>> PolicyLike for Absent<DB> {
    type DB = DB;

    fn name(&self) -> &str {
        self.unreachable()
    }

    fn table<'db>(&'db self, _: &'db Self::DB) -> Result<&'db DB::Table, LookupError>
    where
        Self: 'db,
    {
        self.unreachable()
    }

    fn target_table_name(&self) -> TargetName<'_> {
        self.unreachable()
    }

    fn command(&self) -> CreatePolicyCommand {
        self.unreachable()
    }

    fn policy_type(&self) -> CreatePolicyType {
        self.unreachable()
    }

    fn roles<'db>(&'db self, _: &'db Self::DB) -> impl Iterator<Item = &'db Owner>
    where
        Self: 'db,
    {
        self.unreachable::<core::iter::Empty<_>>()
    }

    fn applies_to_public(&self) -> bool {
        self.unreachable()
    }

    fn using_expression<'db>(&'db self, _: &'db Self::DB) -> Result<Option<&'db Expr>, LookupError>
    where
        Self: 'db,
    {
        self.unreachable()
    }

    fn using_functions<'db>(
        &'db self,
        _: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db DB::Function>, LookupError> {
        self.unreachable::<Result<core::iter::Empty<_>, _>>()
    }

    fn check_expression<'db>(&'db self, _: &'db Self::DB) -> Result<Option<&'db Expr>, LookupError>
    where
        Self: 'db,
    {
        self.unreachable()
    }

    fn check_functions<'db>(
        &'db self,
        _: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db DB::Function>, LookupError> {
        self.unreachable::<Result<core::iter::Empty<_>, _>>()
    }

    fn functions_reached_by<'db>(
        &'db self,
        _: &Function,
        _: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db DB::Function>, LookupError> {
        self.unreachable::<Result<core::iter::Empty<_>, _>>()
    }
}
