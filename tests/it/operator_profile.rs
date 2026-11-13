//! An external profile's operator catalog and type facts, built into the
//! database they belong to, stay distinct from unavailable metadata.

extern crate alloc;

use alloc::{borrow::Cow, sync::Arc};
use core::convert::Infallible;

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
    structs::generic_db::GenericDBBuilder,
    traits::grant::GrantTarget,
    utils::identifier_resolution::SearchPath,
};
use sqlparser::{
    ast::{
        Action, ArgMode, ConstraintReferenceMatchKind, Expr, Function, FunctionCalledOnNull,
        FunctionDefinitionSetParam, FunctionSecurity, Grantee, ReferentialAction, TriggerEvent,
        TriggerObjectKind, TriggerPeriod,
    },
    dialect::{Dialect, PostgreSqlDialect},
};

/// An uninhabited schema object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NoneKind {
    /// Leaves the type without values.
    never: Infallible,
}

impl NoneKind {
    /// Returns any type, since no value exists to call this on.
    const fn unreach<T>(&self) -> T {
        match self.never {}
    }
}

impl Metadata for NoneKind {
    type Meta = ();
}

impl TableLike for NoneKind {
    type DB = AlternateDB;

    fn table_name(&self) -> &str {
        self.unreach()
    }

    fn table_doc<'db>(&'db self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn table_schema(&self) -> Option<&str> {
        self.unreach()
    }

    fn is_unlogged(&self) -> bool {
        self.unreach()
    }

    fn storage_parameters(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, str>)> {
        self.unreach::<core::iter::Empty<(Cow<'_, str>, Cow<'_, str>)>>()
    }

    fn toast_storage_parameters(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, str>)> {
        self.unreach::<core::iter::Empty<(Cow<'_, str>, Cow<'_, str>)>>()
    }

    fn foreign_source<'db>(
        &self,
        _database: &'db Self::DB,
    ) -> Result<Option<&'db ForeignTableSource>, LookupError> {
        self.unreach()
    }

    fn columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn local_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn inherits_from<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn partition_root<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn partition_strategy(&self) -> Option<PartitionStrategy> {
        self.unreach()
    }

    fn partition_bound(&self) -> Option<PartitionBound> {
        self.unreach()
    }

    fn primary_key_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn check_constraints<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn indices<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Index>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn unique_indices<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::UniqueIndex>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn foreign_keys<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn has_row_level_security(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        self.unreach()
    }

    fn has_forced_row_level_security(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        self.unreach()
    }

    fn owner<'db>(&self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        self.unreach()
    }

    fn owner_privileges<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db Action>, LookupError> {
        self.unreach::<Result<core::iter::Empty<&'db Action>, LookupError>>()
    }

    fn possible_privileges<'db>(
        &'db self,
        _role: impl Into<ActingRole<'db, <Self::DB as DatabaseLike>::Role>>,
        _database: &'db Self::DB,
    ) -> Result<TablePrivileges<'db, <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }
}

impl ColumnLike for NoneKind {
    type DB = AlternateDB;

    fn column_name(&self) -> &str {
        self.unreach()
    }

    fn not_null_constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.unreach()
    }

    fn column_doc<'db>(&'db self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn data_type<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> &'db <Self::DB as DatabaseLike>::DataType {
        self.unreach()
    }

    fn collation<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError> {
        self.unreach()
    }

    fn created_from_query<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<CreatedFromQuery<'db, Self::DB>> {
        self.unreach()
    }

    fn identity_generation(&self, _database: &Self::DB) -> Option<IdentityGeneration> {
        self.unreach()
    }

    fn is_serial(&self, _database: &Self::DB) -> bool {
        self.unreach()
    }

    fn is_nullable(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        self.unreach()
    }

    fn default_value(&self) -> Option<String> {
        self.unreach()
    }

    fn foreign_options(&self) -> impl Iterator<Item = (Cow<'_, str>, &str)> {
        self.unreach::<core::iter::Empty<(Cow<'_, str>, &str)>>()
    }

    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.unreach()
    }
}

impl DataTypeLike for NoneKind {
    type DB = AlternateDB;

    fn name<'db>(&'db self, _database: &'db Self::DB) -> Cow<'db, str> {
        self.unreach()
    }

    fn numeric_precision(&self, _database: &Self::DB) -> Option<u64> {
        self.unreach()
    }

    fn is_unsigned(&self, _database: &Self::DB) -> Option<bool> {
        self.unreach()
    }

    fn max_binary_length(&self, _database: &Self::DB) -> Option<u64> {
        self.unreach()
    }

    fn max_bit_length(&self, _database: &Self::DB) -> Option<u64> {
        self.unreach()
    }

    fn character_length_unit(&self, _database: &Self::DB) -> Option<CharacterLengthUnit> {
        self.unreach()
    }

    fn time_zone<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db str> {
        self.unreach()
    }

    fn send_function(&self, _database: &Self::DB) -> Option<bool> {
        self.unreach()
    }

    fn admits_null(&self, _database: &Self::DB) -> bool {
        self.unreach()
    }

    fn range_subtype<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        self.unreach()
    }

    fn structure_kind(&self, _database: &Self::DB) -> Option<StructureKind> {
        self.unreach()
    }

    fn structured_fields<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<impl Iterator<Item = StructuredField<'db, Self>>> {
        self.unreach::<Option<core::iter::Empty<StructuredField<'db, Self>>>>()
    }

    fn map_entry<'db>(&'db self, _database: &'db Self::DB) -> Option<MapEntry<'db, Self>> {
        self.unreach()
    }

    fn enumeration<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<Enumeration<impl Iterator<Item = EnumerationMember<'db>>>> {
        self.unreach::<Option<Enumeration<core::iter::Empty<EnumerationMember<'db>>>>>()
    }

    fn array_dimension_declarations<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<impl Iterator<Item = ArrayDimension>> {
        self.unreach::<Option<core::iter::Empty<ArrayDimension>>>()
    }

    fn enforces_array_dimensions(&self, _database: &Self::DB) -> Option<bool> {
        self.unreach()
    }
}

impl IndexLike for NoneKind {
    type DB = AlternateDB;

    fn name(&self) -> Option<&str> {
        self.unreach()
    }

    fn schema(&self) -> Option<&str> {
        self.unreach()
    }

    fn constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.unreach()
    }

    fn expression<'db>(&'db self, _database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn is_unique(&self) -> bool {
        self.unreach()
    }
}

impl ForeignKeyLike for NoneKind {
    type DB = AlternateDB;

    fn foreign_key_name(&self) -> Option<&str> {
        self.unreach()
    }

    fn constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.unreach()
    }

    fn on_delete(&self, _database: &Self::DB) -> ReferentialAction {
        self.unreach()
    }

    fn on_update(&self, _database: &Self::DB) -> ReferentialAction {
        self.unreach()
    }

    fn host_table<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn referenced_table<'db>(
        &self,
        _database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError> {
        self.unreach()
    }

    fn referenced_table_name(&self) -> TargetName<'_> {
        self.unreach()
    }

    fn host_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn match_kind(&self, _database: &Self::DB) -> ConstraintReferenceMatchKind {
        self.unreach()
    }

    fn referenced_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }
}

impl FunctionLike for NoneKind {
    type DB = AlternateDB;

    fn name(&self) -> &str {
        self.unreach()
    }

    fn target_name(&self) -> TargetName<'_> {
        self.unreach()
    }

    fn argument_type_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Cow<'db, str>> {
        self.unreach::<core::iter::Empty<Cow<'db, str>>>()
    }

    fn argument_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Option<TargetName<'db>>> {
        self.unreach::<core::iter::Empty<Option<TargetName<'db>>>>()
    }

    fn return_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        self.unreach()
    }

    fn returns_set(&self) -> bool {
        self.unreach()
    }

    fn argument_modes(&self) -> impl Iterator<Item = ArgMode> {
        self.unreach::<core::iter::Empty<ArgMode>>()
    }

    fn argument_default_count(&self) -> usize {
        self.unreach()
    }

    fn result_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        self.unreach()
    }

    fn kind(&self, _database: &Self::DB) -> Result<FunctionKind, LookupError> {
        self.unreach()
    }

    fn language(&self) -> Option<&str> {
        self.unreach()
    }

    fn language_is_quoted(&self) -> bool {
        self.unreach()
    }

    fn written_body(&self) -> Option<FunctionBody<'_>> {
        self.unreach()
    }

    fn configuration_parameters(&self) -> &[FunctionDefinitionSetParam] {
        self.unreach()
    }

    fn search_path<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<SearchPath<'db>>, LookupError> {
        self.unreach()
    }

    fn bound_body_functions<'db>(
        &self,
        _database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + use<'db>,
        LookupError,
    >
    where
        Self: 'db,
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn null_input_behavior(&self) -> FunctionCalledOnNull {
        self.unreach()
    }

    fn returns_null_on_null_argument(&self, _database: &Self::DB, _index: usize) -> bool {
        self.unreach()
    }

    fn security_mode(&self) -> FunctionSecurity {
        self.unreach()
    }

    fn owner<'db>(&self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        self.unreach()
    }

    fn is_executable_by_public(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        self.unreach()
    }

    fn can_execute(
        &self,
        _role: &<Self::DB as DatabaseLike>::Role,
        _database: &Self::DB,
    ) -> Result<bool, LookupError> {
        self.unreach()
    }

    fn execute_holders<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = PrivilegeHolder<'db>>, LookupError> {
        self.unreach::<Result<core::iter::Empty<PrivilegeHolder<'db>>, LookupError>>()
    }

    fn can_grant_execute(
        &self,
        _role: &<Self::DB as DatabaseLike>::Role,
        _database: &Self::DB,
    ) -> Result<bool, LookupError> {
        self.unreach()
    }
}

impl CheckConstraintLike for NoneKind {
    type DB = AlternateDB;

    fn expression<'db>(&'db self, _database: &'db Self::DB) -> &'db Expr {
        self.unreach()
    }

    fn constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.unreach()
    }

    fn table<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError> {
        self.unreach()
    }

    fn columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError> {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn functions<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }

    fn functions_reached_by<'db>(
        &'db self,
        _call: &Function,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        self.unreach::<Result<core::iter::Empty<&'db NoneKind>, LookupError>>()
    }
}

impl TriggerLike for NoneKind {
    type DB = AlternateDB;

    fn name(&self) -> &str {
        self.unreach()
    }

    fn table<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn target_table_name(&self) -> TargetName<'_> {
        self.unreach()
    }

    fn events(&self) -> &[TriggerEvent] {
        self.unreach()
    }

    fn timing(&self) -> Option<TriggerPeriod> {
        self.unreach()
    }

    fn orientation(&self) -> Option<TriggerObjectKind> {
        self.unreach()
    }

    fn function<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>
    where
        Self: 'db,
    {
        self.unreach()
    }

    fn function_name(&self) -> Option<&str> {
        self.unreach()
    }
}

impl RoleLike for NoneKind {
    type DB = AlternateDB;

    fn name(&self) -> &str {
        self.unreach()
    }

    fn is_superuser(&self) -> bool {
        self.unreach()
    }

    fn can_create_db(&self) -> bool {
        self.unreach()
    }

    fn can_create_role(&self) -> bool {
        self.unreach()
    }

    fn inherits(&self) -> bool {
        self.unreach()
    }

    fn can_login(&self) -> bool {
        self.unreach()
    }

    fn can_bypass_rls(&self) -> bool {
        self.unreach()
    }

    fn is_replication(&self) -> bool {
        self.unreach()
    }

    fn connection_limit(&self) -> Option<i32> {
        self.unreach()
    }

    fn policies<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Policy> {
        self.unreach::<core::iter::Empty<&'db Absent<AlternateDB>>>()
    }
}

impl RoleMembershipLike for NoneKind {
    type DB = AlternateDB;

    fn role_name(&self) -> &str {
        self.unreach()
    }

    fn member_name(&self) -> &str {
        self.unreach()
    }

    fn admin_option(&self) -> bool {
        self.unreach()
    }

    fn inherit_option(&self) -> bool {
        self.unreach()
    }

    fn set_option(&self) -> bool {
        self.unreach()
    }
}

impl SchemaLike for NoneKind {
    type DB = AlternateDB;

    fn name(&self) -> &str {
        self.unreach()
    }

    fn authorization(&self) -> Option<&str> {
        self.unreach()
    }

    fn public_holds_usage(&self) -> bool {
        self.unreach()
    }

    fn owner_holds_usage(&self) -> bool {
        self.unreach()
    }
}

impl GrantLike for NoneKind {
    type DB = AlternateDB;

    fn privileges<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db,
    {
        self.unreach::<core::iter::Empty<&'db Action>>()
    }

    fn is_all_privileges(&self) -> bool {
        self.unreach()
    }

    fn grantees<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db,
    {
        self.unreach::<core::iter::Empty<&'db Grantee>>()
    }

    fn applies_to_public(&self) -> bool {
        self.unreach()
    }

    fn target(&self) -> GrantTarget<impl Iterator<Item = TargetName<'_>>> {
        self.unreach::<GrantTarget<core::iter::Empty<TargetName<'_>>>>()
    }

    fn with_grant_option(&self) -> bool {
        self.unreach()
    }

    fn granted_by<'a>(&'a self, _database: &'a Self::DB) -> Option<&'a str> {
        self.unreach()
    }

    fn applies_to_role(&self, _role: &<Self::DB as DatabaseLike>::Role) -> bool {
        self.unreach()
    }
}

impl TableGrantLike for NoneKind {
    fn tables<'a>(
        &'a self,
        _database: &'a Self::DB,
    ) -> impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Table> {
        self.unreach::<core::iter::Empty<&'a NoneKind>>()
    }

    fn relations<'a>(
        &'a self,
        _database: &'a Self::DB,
    ) -> impl Iterator<Item = Relation<'a, Self::DB>> {
        self.unreach::<core::iter::Empty<Relation<'a, AlternateDB>>>()
    }

    fn applies_to_table(
        &self,
        _table: &<Self::DB as DatabaseLike>::Table,
        _database: &Self::DB,
    ) -> bool {
        self.unreach()
    }
}

impl ColumnGrantLike for NoneKind {
    fn columns<'a>(
        &'a self,
        _table: &'a <Self::DB as DatabaseLike>::Table,
        _database: &'a Self::DB,
    ) -> Result<impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Column>, LookupError> {
        self.unreach::<Result<core::iter::Empty<&'a NoneKind>, LookupError>>()
    }

    fn table<'a>(
        &'a self,
        _database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Table> {
        self.unreach()
    }

    fn relation<'a>(&'a self, _database: &'a Self::DB) -> Option<Relation<'a, Self::DB>> {
        self.unreach()
    }
}

/// One entry of a catalog loaded outside the parser.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CatalogOperator {
    /// Schema holding the operator.
    schema: &'static str,
    /// Operator symbol.
    name: &'static str,
    /// The `pg_catalog` type of the operand of a prefix operator, `None` for
    /// a binary operator of unknown operand types.
    right: Option<&'static str>,
}

impl Metadata for CatalogOperator {
    type Meta = ();
}

impl OperatorLike for CatalogOperator {
    type DB = AlternateDB;

    fn target_name(&self) -> TargetName<'_> {
        TargetName::new(self.name, false).with_schema(self.schema, false)
    }

    fn kind(&self) -> OperatorKind {
        if self.right.is_some() { OperatorKind::Prefix } else { OperatorKind::Binary }
    }

    fn left_type<'db>(&'db self, _database: &'db Self::DB) -> Option<TargetName<'db>> {
        None
    }

    fn right_type<'db>(&'db self, _database: &'db Self::DB) -> Option<TargetName<'db>> {
        self.right.map(|right| TargetName::new(right, false).with_schema("pg_catalog", false))
    }

    fn result_type<'db>(&'db self, _database: &'db Self::DB) -> Option<TargetName<'db>> {
        None
    }

    fn implementation<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function> {
        None
    }

    fn commutator<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        None
    }

    fn negator<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        None
    }

    fn can_hash(&self) -> bool {
        false
    }

    fn can_merge(&self) -> bool {
        false
    }

    fn restriction_estimator<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function> {
        None
    }

    fn join_estimator<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function> {
        None
    }
}

/// A type the profile loads, with the array type over it.
#[derive(Debug, Clone)]
struct CatalogType {
    /// The type's name.
    name: String,
    /// The type's `pg_type.typcategory`.
    category: TypeCategory,
    /// The name of the array type over the type.
    array: String,
    /// The type's `pg_type.typtype`, `None` when the catalog leaves it out.
    kind: Option<TypeKind>,
}

/// The catalogs the profile loads, each `None` when it is unavailable.
#[derive(Debug, Clone, Default)]
struct AlternateCatalog {
    /// The operators the catalog holds.
    operators: Option<Arc<[CatalogOperator]>>,
    /// The types the catalog holds.
    types: Option<Arc<[CatalogType]>>,
}

impl AlternateCatalog {
    /// The catalog holding `operators` and no type facts.
    fn of_operators(operators: Arc<[CatalogOperator]>) -> Self {
        Self { operators: Some(operators), types: None }
    }
}

/// PostgreSQL syntax for an empty schema profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct AlternateDialect;

impl DialectLike for AlternateDialect {
    type DB = AlternateDB;
    type Match = TypeMatch;

    fn is_bool(
        &self,
        _database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Self::Match {
        column.unreach()
    }

    fn is_uuid(
        &self,
        _database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Self::Match {
        column.unreach()
    }

    fn sqlparser_dialect(&self) -> &dyn Dialect {
        &PostgreSqlDialect {}
    }
}

/// A profile whose operator catalog is loaded outside the parser.
struct AlternateProfile;

/// The database the profile describes.
type AlternateDB = GenericDB<AlternateProfile>;

impl SchemaProfile for AlternateProfile {
    type Table = NoneKind;
    type View = Absent<AlternateDB>;
    type MaterializedView = Absent<AlternateDB>;
    type Sequence = Absent<AlternateDB>;
    type Column = NoneKind;
    type DataType = NoneKind;
    type Index = NoneKind;
    type UniqueIndex = NoneKind;
    type ForeignKey = NoneKind;
    type Function = NoneKind;
    type Operator = CatalogOperator;
    type Cast = Absent<AlternateDB>;
    type CheckConstraint = NoneKind;
    type Trigger = NoneKind;
    type Rule = Absent<AlternateDB>;
    type Extension = Absent<AlternateDB>;
    type Domain = Absent<AlternateDB>;
    type UserDefinedType = Absent<AlternateDB>;
    type Policy = Absent<AlternateDB>;
    type Role = NoneKind;
    type RoleMembership = NoneKind;
    type Schema = NoneKind;
    type TableGrant = NoneKind;
    type ColumnGrant = NoneKind;
    type Dialect = AlternateDialect;
    type Ingestion = AlternateCatalog;

    fn default_ingestion(_dialect: &Self::Dialect) -> Self::Ingestion {
        AlternateCatalog::default()
    }

    fn operators<'a>(
        _dialect: &'a Self::Dialect,
        ingestion: &'a Self::Ingestion,
    ) -> Option<impl Iterator<Item = &'a Self::Operator>>
    where
        Self::Operator: 'a,
    {
        ingestion.operators.as_deref().map(<[CatalogOperator]>::iter)
    }

    fn type_facts<'a>(
        _database: &'a GenericDB<Self>,
        ingestion: &'a Self::Ingestion,
        type_name: TargetName<'_>,
    ) -> Option<TypeFacts<'a>> {
        let loaded =
            ingestion.types.as_deref()?.iter().find(|loaded| loaded.name == type_name.name())?;
        let facts = TypeFacts::new(loaded.category, false)
            .with_array_type(TargetName::new(&loaded.array, false));
        Some(match loaded.kind {
            Some(kind) => facts.with_kind(kind),
            None => facts,
        })
    }

    fn initial_schemas(
        _dialect: &Self::Dialect,
    ) -> impl IntoIterator<Item = (Arc<Self::Schema>, Meta<Self::Schema>)> {
        core::iter::empty::<(Arc<NoneKind>, ())>()
    }

    fn is_foreign_table(_metadata: &Meta<Self::Table>) -> bool {
        false
    }
}

/// The inherited operator and cast catalogs and type facts remain
/// unavailable under PostgreSQL syntax.
#[test]
fn unavailable_catalogs_stay_distinct_from_empty_ones() -> Result<(), Error> {
    let catalog_name = String::from("profile");
    let database: GenericDB<AlternateProfile> =
        GenericDBBuilder::<AlternateProfile>::new(catalog_name, AlternateDialect).into();
    assert!(database.operators().is_none());
    assert!(database.casts().is_none());
    assert!(
        database
            .type_facts(TargetName::new("int4", false).with_schema("pg_catalog", false))
            .is_none()
    );

    let empty = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty())
        .parse::<PostgreSqlDialect>("")?;
    assert!(empty.operators().is_some_and(|mut operators| operators.next().is_none()));
    assert!(empty.casts().is_some_and(|mut casts| casts.next().is_none()));
    Ok(())
}

/// The builder for an empty database of the profile.
fn alternate_builder() -> GenericDBBuilder<AlternateProfile> {
    GenericDBBuilder::new(String::from("profile"), AlternateDialect)
}

/// A loaded catalog is read back entry by entry, by the database and by its
/// clone alike, as the very entries the loader stored.
#[test]
fn a_loaded_catalog_reaches_the_database_and_its_clones() {
    let catalog: Arc<[CatalogOperator]> = Arc::from([
        CatalogOperator { schema: "pg_catalog", name: "+", right: None },
        CatalogOperator { schema: "public", name: "+", right: None },
    ]);
    let database =
        alternate_builder().into_database(AlternateCatalog::of_operators(Arc::clone(&catalog)));
    let clone = database.clone();

    for database in [&database, &clone] {
        let operators: Vec<_> = database.operators().into_iter().flatten().collect();
        assert_eq!(operators.len(), catalog.len());
        assert!(
            operators.iter().zip(catalog.iter()).all(|(read, stored)| core::ptr::eq(*read, stored))
        );
    }
}

/// An operator whose operand types the catalog does not know may take any
/// operands, so an invocation it could answer stays open.
#[test]
fn operators_of_unknown_operand_types_leave_an_invocation_open() {
    let catalog: Arc<[CatalogOperator]> =
        Arc::from([CatalogOperator { schema: "pg_catalog", name: "+", right: None }]);
    let database = alternate_builder().into_database(AlternateCatalog::of_operators(catalog));
    let int4 = || OperandType::Known { type_name: TargetName::new("int4", false), typmod: None };
    assert!(matches!(
        database.resolve_operator(
            TargetName::new("+", false),
            Some(int4()),
            int4(),
            IdentifierCase::AsWritten
        ),
        OperatorResolution::Undetermined
    ));
}

/// `anyenum` takes a type of the enum category only on evidence of the enum
/// kind, since a base type may declare that category too.
#[test]
fn enum_operands_need_the_enum_kind() {
    let resolution = |kind: Option<TypeKind>| {
        let operators = Arc::from([CatalogOperator {
            schema: "pg_catalog",
            name: "#?",
            right: Some("anyenum"),
        }]);
        let types = Arc::from([CatalogType {
            name: String::from("mood"),
            category: TypeCategory::ENUM,
            array: String::from("_mood"),
            kind,
        }]);
        let database = alternate_builder()
            .into_database(AlternateCatalog { operators: Some(operators), types: Some(types) });
        let mood = OperandType::Known { type_name: TargetName::new("mood", false), typmod: None };
        match database.resolve_operator(
            TargetName::new("#?", false),
            None,
            mood,
            IdentifierCase::AsWritten,
        ) {
            OperatorResolution::Selected(_) => "selected",
            OperatorResolution::NotFound => "not found",
            OperatorResolution::Ambiguous(_) => "ambiguous",
            OperatorResolution::UnboundPolymorphic(_) => "unbound",
            OperatorResolution::Undetermined => "undetermined",
        }
    };
    assert_eq!(resolution(Some(TypeKind::Enum)), "selected");
    assert_eq!(resolution(Some(TypeKind::Base)), "not found");
    assert_eq!(resolution(None), "undetermined");
}

/// A loaded catalog without entries answers an empty catalog, which the
/// default state answering unavailable stays distinct from.
#[test]
fn a_loaded_empty_catalog_stays_distinct_from_unavailable() {
    let empty = alternate_builder().into_database(AlternateCatalog::of_operators(Arc::from([])));
    assert!(empty.operators().is_some_and(|mut operators| operators.next().is_none()));
    assert!(empty.clone().operators().is_some_and(|mut operators| operators.next().is_none()));

    let unavailable: AlternateDB = alternate_builder().into();
    assert!(unavailable.operators().is_none());
}

/// The database holding the given types and no operators.
fn database_of_types(types: &[(&str, TypeCategory, &str)]) -> AlternateDB {
    let types = types
        .iter()
        .map(|&(name, category, array)| {
            CatalogType {
                name: String::from(name),
                category,
                array: String::from(array),
                kind: None,
            }
        })
        .collect();
    alternate_builder().into_database(AlternateCatalog { operators: None, types: Some(types) })
}

/// Each database answers type facts from the types its own loaded catalog
/// stores, so two databases loading different facts for one name each
/// answer their own.
#[test]
fn type_facts_read_the_catalog_their_database_loaded() {
    let numeric = database_of_types(&[("money", TypeCategory::NUMERIC, "_money")]);
    let string = database_of_types(&[("money", TypeCategory::STRING, "money_list")]);

    for (database, category, array) in
        [(&numeric, TypeCategory::NUMERIC, "_money"), (&string, TypeCategory::STRING, "money_list")]
    {
        assert_eq!(
            database.type_facts(TargetName::new("money", false)),
            Some(TypeFacts::new(category, false).with_array_type(TargetName::new(array, false)))
        );
        assert!(database.type_facts(TargetName::new("int4", false)).is_none());
    }
}
