//! A catalog implemented without the SQL parser, so the bodies the traits
//! carry for implementors reach a test.
//!
//! `GenericDB` overrides the faster ones, which leaves every inherited body it
//! replaces with no caller in this repository, and the library promises those
//! bodies to anybody describing a catalog from somewhere other than a parsed
//! script. The in-memory catalog here is that second implementor: it stores
//! plain strings, answers only what a catalog must answer, and inherits
//! everything else.
#![allow(clippy::expect_used)]

use std::borrow::Cow;

use sql_traits::{
    errors::LookupError,
    prelude::*,
    structs::TargetName,
    traits::{
        ColumnCollation, TypeMatch,
        grant::{GrantRelation, GrantTarget},
    },
    utils::scalar_family::ScalarFamily,
};
use sqlparser::{
    ast::{
        Action, ArgMode, ConstraintReferenceMatchKind, CreatePolicyCommand, CreatePolicyType,
        CreateRuleAction, CreateRuleEvent, Expr, Function, FunctionBehavior, FunctionCalledOnNull,
        FunctionDefinitionSetParam, FunctionSecurity, Grantee, Owner, Query, ReferentialAction,
        Statement, TriggerEvent, TriggerObjectKind, TriggerPeriod,
    },
    dialect::GenericDialect,
    parser::Parser,
};

#[derive(Debug, Clone, Default)]
struct MemoryCatalog {
    catalog_name: String,
    dialect: MemoryDialect,
    tables: Vec<MemoryTable>,
    columns: Vec<MemoryColumn>,
    schemas: Vec<MemorySchema>,
    views: Vec<MemoryView>,
    materialized_views: Vec<MemoryView>,
    indexes: Vec<MemoryIndex>,
    unique_indexes: Vec<MemoryUniqueIndex>,
    foreign_keys: Vec<MemoryForeignKey>,
    check_constraints: Vec<MemoryCheckConstraint>,
    functions: Vec<MemoryFunction>,
    triggers: Vec<MemoryTrigger>,
    rules: Vec<MemoryRule>,
    extensions: Vec<MemoryExtension>,
    policies: Vec<MemoryPolicy>,
    roles: Vec<MemoryRole>,
    role_memberships: Vec<MemoryRoleMembership>,
    table_grants: Vec<MemoryTableGrant>,
    column_grants: Vec<MemoryColumnGrant>,
    /// Schemas an unqualified name resolves against, in order. Empty answers
    /// `public` alone, as the inherited body does.
    search_path: Vec<(String, bool)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryDialect;

/// The collection a relation of [`MemoryCatalog`] sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum MemoryRelationKind {
    Table,
    View,
    MaterializedView,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryTable {
    schema: Option<String>,
    schema_is_quoted: bool,
    name: String,
    name_is_quoted: bool,
    /// A key column the table does not declare, which a catalog read from a
    /// live server can hand back when the key and the column list disagree.
    undeclared_key_column: Option<MemoryColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryColumn {
    table_schema: Option<String>,
    table_name: String,
    name: String,
    name_is_quoted: bool,
    data_type: MemoryDataType,
    nullable: bool,
    primary_key: bool,
}

/// A declared type the catalog stores as its name alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryDataType(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemorySchema {
    name: String,
    name_is_quoted: bool,
}

/// Never built by these tests: a catalog with no views still has to name the
/// type its views would have.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryView {
    schema: Option<String>,
    name: String,
    materialized: bool,
    definition: Query,
    declared_column_names: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryIndex {
    name: Option<String>,
    schema: Option<String>,
    table_name: String,
    expression: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryUniqueIndex(MemoryIndex);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryForeignKey {
    name: Option<String>,
    host_table_name: String,
    referenced_table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryCheckConstraint {
    table_name: String,
    expression: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryFunction {
    name: String,
    schema: Option<String>,
    argument_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryTrigger {
    name: String,
    table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryRule {
    name: String,
    table_name: String,
    action: CreateRuleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryExtension {
    name: String,
    schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryPolicy {
    name: String,
    table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryRole {
    name: String,
    name_is_quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryRoleMembership {
    role_name: String,
    member_name: String,
    set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryTableGrant {
    table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MemoryColumnGrant {
    table_name: String,
}

impl Metadata for MemoryCatalog {
    type Meta = ();
}

macro_rules! plain_metadata {
    ($($kind:ty),+ $(,)?) => {
        $(
            impl Metadata for $kind {
                type Meta = ();
            }
        )+
    };
}

plain_metadata!(
    MemoryTable,
    MemoryColumn,
    MemorySchema,
    MemoryView,
    MemoryIndex,
    MemoryUniqueIndex,
    MemoryForeignKey,
    MemoryCheckConstraint,
    MemoryFunction,
    MemoryTrigger,
    MemoryRule,
    MemoryExtension,
    MemoryPolicy,
    MemoryRole,
    MemoryRoleMembership,
    MemoryTableGrant,
    MemoryColumnGrant,
);

impl DatabaseLike for MemoryCatalog {
    type Table = MemoryTable;
    type View = MemoryView;
    type MaterializedView = MemoryView;
    type Column = MemoryColumn;
    type DataType = MemoryDataType;
    type Index = MemoryIndex;
    type ForeignKey = MemoryForeignKey;
    type Function = MemoryFunction;
    type UniqueIndex = MemoryUniqueIndex;
    type CheckConstraint = MemoryCheckConstraint;
    type Trigger = MemoryTrigger;
    type Rule = MemoryRule;
    type Extension = MemoryExtension;
    type Policy = MemoryPolicy;
    type Role = MemoryRole;
    type RoleMembership = MemoryRoleMembership;
    type TableGrant = MemoryTableGrant;
    type ColumnGrant = MemoryColumnGrant;
    type Schema = MemorySchema;
    type Dialect = MemoryDialect;
    /// Which collection holds the relation, and its position there, stable
    /// because nothing in this catalog renames or drops.
    type RelationId = (MemoryRelationKind, usize);
    type ColumnId = usize;

    fn dialect(&self) -> &Self::Dialect {
        &self.dialect
    }

    fn catalog_name(&self) -> &str {
        &self.catalog_name
    }

    fn number_of_tables(&self) -> usize {
        self.tables.len()
    }

    fn timezone(&self) -> Option<&str> {
        None
    }

    fn search_path(&self) -> impl Iterator<Item = (&str, bool)> {
        self.search_path
            .iter()
            .map(|(name, quoted)| (name.as_str(), *quoted))
            .chain(core::iter::once(("public", false)).filter(|_| self.search_path.is_empty()))
    }

    fn tables(&self) -> impl Iterator<Item = &Self::Table> {
        self.tables.iter()
    }

    fn triggers(&self) -> impl Iterator<Item = &Self::Trigger> {
        self.triggers.iter()
    }

    fn rules(&self) -> impl Iterator<Item = &Self::Rule> {
        self.rules.iter()
    }

    fn extensions(&self) -> impl Iterator<Item = &Self::Extension> {
        self.extensions.iter()
    }

    fn indexes(&self) -> impl Iterator<Item = &Self::Index> {
        self.indexes.iter()
    }

    fn functions(&self) -> impl Iterator<Item = &Self::Function> {
        self.functions.iter()
    }

    fn views(&self) -> impl Iterator<Item = &Self::View> {
        self.views.iter()
    }

    fn materialized_views(&self) -> impl Iterator<Item = &Self::MaterializedView> {
        self.materialized_views.iter()
    }

    fn table_id(&self, table: &Self::Table) -> Option<usize> {
        self.tables.iter().position(|candidate| candidate == table)
    }

    fn table_identity(&self, table: &Self::Table) -> Option<Self::RelationId> {
        self.table_id(table).map(|position| (MemoryRelationKind::Table, position))
    }

    fn view_identity(&self, view: &Self::View) -> Option<Self::RelationId> {
        self.views
            .iter()
            .position(|candidate| candidate == view)
            .map(|position| (MemoryRelationKind::View, position))
    }

    fn materialized_view_identity(
        &self,
        view: &Self::MaterializedView,
    ) -> Option<Self::RelationId> {
        self.materialized_views
            .iter()
            .position(|candidate| candidate == view)
            .map(|position| (MemoryRelationKind::MaterializedView, position))
    }

    fn column_identity(&self, column: &Self::Column) -> Option<Self::ColumnId> {
        self.columns.iter().position(|candidate| candidate == column)
    }

    fn policies(&self) -> impl Iterator<Item = &Self::Policy> {
        self.policies.iter()
    }

    fn roles(&self) -> impl Iterator<Item = &Self::Role> {
        self.roles.iter()
    }

    fn role_memberships(&self) -> impl Iterator<Item = &Self::RoleMembership> {
        self.role_memberships.iter()
    }

    fn table_grants(&self) -> impl Iterator<Item = &Self::TableGrant> {
        self.table_grants.iter()
    }

    fn column_grants(&self) -> impl Iterator<Item = &Self::ColumnGrant> {
        self.column_grants.iter()
    }

    fn schemas(&self) -> impl Iterator<Item = &Self::Schema> {
        self.schemas.iter()
    }
}

impl DialectLike for MemoryDialect {
    type DB = MemoryCatalog;
    type Match = TypeMatch;

    fn is_bool(&self, database: &Self::DB, column: &MemoryColumn) -> Self::Match {
        if column.data_type(database).name(database).eq_ignore_ascii_case("boolean") {
            TypeMatch::Yes
        } else {
            TypeMatch::No
        }
    }

    fn is_uuid(&self, database: &Self::DB, column: &MemoryColumn) -> Self::Match {
        if column.data_type(database).name(database).eq_ignore_ascii_case("uuid") {
            TypeMatch::Yes
        } else {
            TypeMatch::No
        }
    }
}

impl DataTypeLike for MemoryDataType {
    type DB = MemoryCatalog;

    fn name<'db>(&'db self, _database: &'db Self::DB) -> Cow<'db, str> {
        Cow::Borrowed(&self.0)
    }
}

impl TableLike for MemoryTable {
    type DB = MemoryCatalog;

    fn table_name(&self) -> &str {
        &self.name
    }

    fn table_name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    fn table_schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    fn table_schema_is_quoted(&self) -> bool {
        self.schema_is_quoted
    }

    fn table_doc<'db>(&'db self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(None)
    }

    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError>
    where
        Self: 'db,
    {
        Ok(database
            .columns
            .iter()
            .filter(|column| column.table_name == self.name && column.table_schema == self.schema))
    }

    fn local_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError>
    where
        Self: 'db,
    {
        self.columns(database)
    }

    fn inherits_from<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryTable>, LookupError>
    where
        Self: 'db,
    {
        Ok(core::iter::empty())
    }

    fn partition_root<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<&'db MemoryTable>, LookupError>
    where
        Self: 'db,
    {
        Ok(None)
    }

    fn partition_strategy(&self) -> Option<PartitionStrategy> {
        None
    }

    fn is_unlogged(&self) -> bool {
        false
    }

    fn storage_parameters(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, str>)> {
        std::iter::empty()
    }

    fn primary_key_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError>
    where
        Self: 'db,
    {
        Ok(self
            .columns(database)?
            .filter(|column| column.primary_key)
            .chain(self.undeclared_key_column.as_ref()))
    }

    fn check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryCheckConstraint>, LookupError>
    where
        Self: 'db,
    {
        Ok(database
            .check_constraints
            .iter()
            .filter(|constraint| constraint.table_name == self.name))
    }

    fn indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryIndex>, LookupError>
    where
        Self: 'db,
    {
        Ok(database.indexes.iter().filter(|index| index.table_name == self.name))
    }

    fn unique_indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryUniqueIndex>, LookupError>
    where
        Self: 'db,
    {
        Ok(database.unique_indexes.iter().filter(|index| index.0.table_name == self.name))
    }

    fn foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryForeignKey>, LookupError>
    where
        Self: 'db,
    {
        Ok(database.foreign_keys.iter().filter(|key| key.host_table_name == self.name))
    }

    fn has_row_level_security(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(false)
    }

    fn has_forced_row_level_security(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(false)
    }

    fn owner<'db>(&self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        Ok(None)
    }

    /// This catalog records no owner, so no entry of the owner's is kept.
    fn owner_privileges<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db sqlparser::ast::Action>, LookupError> {
        Ok(core::iter::empty())
    }
}

impl ColumnLike for MemoryColumn {
    type DB = MemoryCatalog;

    fn column_name(&self) -> &str {
        &self.name
    }

    fn column_name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    fn column_doc<'db>(&'db self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(None)
    }

    fn data_type<'db>(&'db self, _database: &'db Self::DB) -> &'db MemoryDataType {
        &self.data_type
    }

    fn collation<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError> {
        Ok(ColumnCollation::DatabaseDefault)
    }

    fn identity_generation(&self, _database: &Self::DB) -> Option<IdentityGeneration> {
        None
    }

    fn is_serial(&self) -> bool {
        false
    }

    fn is_nullable(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.nullable)
    }

    fn default_value(&self) -> Option<String> {
        None
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db MemoryTable
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name && table.schema == self.table_schema)
            .expect("every column of this catalog belongs to one of its tables")
    }
}

impl SchemaLike for MemorySchema {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    fn authorization(&self) -> Option<&str> {
        None
    }
}

impl ViewLike for MemoryView {
    type DB = MemoryCatalog;

    fn view_name(&self) -> &str {
        &self.name
    }

    fn view_schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn definition(&self) -> &Query {
        &self.definition
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        &self.declared_column_names
    }
}

impl IndexLike for MemoryIndex {
    type DB = MemoryCatalog;

    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    fn expression<'db>(&'db self, _database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db,
    {
        Ok(&self.expression)
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db MemoryTable
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name)
            .expect("every index of this catalog belongs to one of its tables")
    }

    fn is_unique(&self) -> bool {
        false
    }
}

impl IndexLike for MemoryUniqueIndex {
    type DB = MemoryCatalog;

    fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    fn schema(&self) -> Option<&str> {
        self.0.schema.as_deref()
    }

    fn expression<'db>(&'db self, database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db,
    {
        self.0.expression(database)
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db MemoryTable
    where
        Self: 'db,
    {
        self.0.table(database)
    }

    fn is_unique(&self) -> bool {
        true
    }
}

impl ForeignKeyLike for MemoryForeignKey {
    type DB = MemoryCatalog;

    fn foreign_key_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn on_delete(&self, _database: &Self::DB) -> ReferentialAction {
        ReferentialAction::NoAction
    }

    fn on_update(&self, _database: &Self::DB) -> ReferentialAction {
        ReferentialAction::NoAction
    }

    fn host_table<'db>(&'db self, database: &'db Self::DB) -> &'db MemoryTable
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.host_table_name)
            .expect("every foreign key of this catalog is declared on one of its tables")
    }

    fn referenced_table<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<&'db MemoryTable, LookupError> {
        database
            .tables
            .iter()
            .find(|table| table.name == self.referenced_table_name)
            .ok_or(LookupError::TableNotFound { object_name: self.referenced_table_name.clone() })
    }

    fn referenced_table_name(&self) -> TargetName<'_> {
        TargetName::new(&self.referenced_table_name, false)
    }

    fn host_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError>
    where
        Self: 'db,
    {
        Ok(core::iter::empty())
    }

    fn match_kind(&self, _database: &Self::DB) -> ConstraintReferenceMatchKind {
        ConstraintReferenceMatchKind::Simple
    }

    fn referenced_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError>
    where
        Self: 'db,
    {
        Ok(core::iter::empty())
    }
}

impl CheckConstraintLike for MemoryCheckConstraint {
    type DB = MemoryCatalog;

    fn expression<'db>(&'db self, _database: &'db Self::DB) -> &'db Expr {
        &self.expression
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> Result<&'db MemoryTable, LookupError> {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name)
            .ok_or(LookupError::TableNotFound { object_name: self.table_name.clone() })
    }

    fn columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryColumn>, LookupError> {
        Ok(core::iter::empty())
    }

    fn functions<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryFunction> + 'db, LookupError> {
        Ok(core::iter::empty())
    }

    fn functions_reached_by<'db>(
        &'db self,
        _call: &Function,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryFunction> + 'db, LookupError> {
        Ok(core::iter::empty())
    }
}

impl FunctionLike for MemoryFunction {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn target_name(&self) -> TargetName<'_> {
        let target = TargetName::new(&self.name, false);
        match self.schema.as_deref() {
            Some(schema) => target.with_schema(schema, false),
            None => target,
        }
    }

    fn argument_type_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Cow<'db, str>> {
        self.argument_types.iter().map(|declared| Cow::Borrowed(declared.as_str()))
    }

    fn argument_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Option<TargetName<'db>>> {
        core::iter::empty()
    }

    fn return_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        None
    }

    fn returns_set(&self) -> bool {
        false
    }

    fn argument_modes(&self) -> impl Iterator<Item = ArgMode> {
        self.argument_types.iter().map(|_| ArgMode::In)
    }

    fn argument_default_count(&self) -> usize {
        0
    }

    fn result_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        None
    }

    /// This catalog holds only functions it declares, which are normal.
    fn kind(&self, _database: &Self::DB) -> Result<FunctionKind, LookupError> {
        Ok(FunctionKind::Normal)
    }

    fn language(&self) -> Option<&str> {
        None
    }

    fn language_is_quoted(&self) -> bool {
        false
    }

    fn body(&self) -> Option<&str> {
        None
    }

    fn body_expression(&self) -> Option<&Expr> {
        None
    }

    fn configuration_parameters(&self) -> &[FunctionDefinitionSetParam] {
        &[]
    }

    fn null_input_behavior(&self) -> FunctionCalledOnNull {
        FunctionCalledOnNull::CalledOnNullInput
    }

    fn returns_null_on_null_argument(&self, _database: &Self::DB, _index: usize) -> bool {
        false
    }

    fn security_mode(&self) -> FunctionSecurity {
        FunctionSecurity::Invoker
    }

    fn owner<'db>(&self, _database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        Ok(None)
    }

    /// This catalog records no grants, so every function keeps PostgreSQL's
    /// default.
    fn is_executable_by_public(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(true)
    }

    fn can_execute(&self, _role: &MemoryRole, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(true)
    }

    fn can_grant_execute(
        &self,
        _role: &MemoryRole,
        _database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(false)
    }
}

impl TriggerLike for MemoryTrigger {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> Result<&'db MemoryTable, LookupError>
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name)
            .ok_or(LookupError::TableNotFound { object_name: self.table_name.clone() })
    }

    fn target_table_name(&self) -> TargetName<'_> {
        TargetName::new(&self.table_name, false)
    }

    fn events(&self) -> &[TriggerEvent] {
        &[]
    }

    fn timing(&self) -> Option<TriggerPeriod> {
        None
    }

    fn orientation(&self) -> Option<TriggerObjectKind> {
        None
    }

    fn function<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db MemoryFunction>
    where
        Self: 'db,
    {
        None
    }

    fn function_name(&self) -> Option<&str> {
        None
    }
}

impl ExtensionLike for MemoryExtension {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    fn version(&self) -> Option<&str> {
        None
    }
}

impl RuleLike for MemoryRule {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn target_relation_name(&self) -> TargetName<'_> {
        TargetName::new(&self.table_name, false)
    }

    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<RuleRelation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name)
            .map(RuleRelation::Table)
            .ok_or(LookupError::RelationNotFound { object_name: self.table_name.clone() })
    }

    fn event(&self) -> CreateRuleEvent {
        CreateRuleEvent::Insert
    }

    fn is_instead(&self) -> bool {
        false
    }

    fn condition(&self) -> Option<&Expr> {
        None
    }

    fn action(&self) -> &CreateRuleAction {
        &self.action
    }
}

impl PolicyLike for MemoryPolicy {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn table<'db>(&'db self, database: &'db Self::DB) -> Result<&'db MemoryTable, LookupError>
    where
        Self: 'db,
    {
        database
            .tables
            .iter()
            .find(|table| table.name == self.table_name)
            .ok_or(LookupError::TableNotFound { object_name: self.table_name.clone() })
    }

    fn target_table_name(&self) -> TargetName<'_> {
        TargetName::new(&self.table_name, false)
    }

    fn command(&self) -> CreatePolicyCommand {
        CreatePolicyCommand::All
    }

    fn policy_type(&self) -> CreatePolicyType {
        CreatePolicyType::Permissive
    }

    fn roles<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Owner>
    where
        Self: 'db,
    {
        core::iter::empty()
    }

    fn applies_to_public(&self) -> bool {
        true
    }

    fn using_expression<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        None
    }

    fn using_functions<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryFunction>, LookupError> {
        Ok(core::iter::empty())
    }

    fn check_expression<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        None
    }

    fn check_functions<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryFunction>, LookupError> {
        Ok(core::iter::empty())
    }

    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db MemoryFunction>, LookupError> {
        // The policy holds no expression, so no call is one of its nodes.
        Err::<core::iter::Empty<_>, _>(LookupError::CallNotInExpression { call: call.to_string() })
    }
}

impl RoleLike for MemoryRole {
    type DB = MemoryCatalog;

    fn name(&self) -> &str {
        &self.name
    }

    fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    fn is_superuser(&self) -> bool {
        false
    }

    fn can_create_db(&self) -> bool {
        false
    }

    fn can_create_role(&self) -> bool {
        false
    }

    fn inherits(&self) -> bool {
        true
    }

    fn can_login(&self) -> bool {
        false
    }

    fn can_bypass_rls(&self) -> bool {
        false
    }

    fn is_replication(&self) -> bool {
        false
    }

    fn connection_limit(&self) -> Option<i32> {
        None
    }

    fn policies<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db MemoryPolicy> {
        core::iter::empty()
    }
}

impl RoleMembershipLike for MemoryRoleMembership {
    type DB = MemoryCatalog;

    fn role_name(&self) -> &str {
        &self.role_name
    }

    fn member_name(&self) -> &str {
        &self.member_name
    }

    fn admin_option(&self) -> bool {
        false
    }

    fn inherit_option(&self) -> bool {
        true
    }

    fn set_option(&self) -> bool {
        self.set
    }
}

impl GrantLike for MemoryTableGrant {
    type DB = MemoryCatalog;

    fn privileges<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db,
    {
        core::iter::empty()
    }

    fn is_all_privileges(&self) -> bool {
        true
    }

    fn grantees<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db,
    {
        core::iter::empty()
    }

    fn applies_to_public(&self) -> bool {
        true
    }

    fn target(&self) -> GrantTarget<impl Iterator<Item = TargetName<'_>>> {
        GrantTarget::Tables(core::iter::once(TargetName::new(&self.table_name, false)))
    }

    fn with_grant_option(&self) -> bool {
        false
    }

    fn granted_by<'a>(&'a self, _database: &'a Self::DB) -> Option<&'a MemoryRole> {
        None
    }

    fn applies_to_role(&self, _role: &MemoryRole) -> bool {
        true
    }
}

impl TableGrantLike for MemoryTableGrant {
    fn tables<'a>(&'a self, database: &'a Self::DB) -> impl Iterator<Item = &'a MemoryTable> {
        database.tables.iter().filter(|table| table.name == self.table_name)
    }

    fn relations<'a>(
        &'a self,
        _database: &'a Self::DB,
    ) -> impl Iterator<Item = GrantRelation<'a, Self::DB>> {
        core::iter::empty()
    }

    fn applies_to_table(&self, table: &MemoryTable, _database: &Self::DB) -> bool {
        table.name == self.table_name
    }
}

impl GrantLike for MemoryColumnGrant {
    type DB = MemoryCatalog;

    fn privileges<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db,
    {
        core::iter::empty()
    }

    fn is_all_privileges(&self) -> bool {
        true
    }

    fn grantees<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db,
    {
        core::iter::empty()
    }

    fn applies_to_public(&self) -> bool {
        true
    }

    fn target(&self) -> GrantTarget<impl Iterator<Item = TargetName<'_>>> {
        GrantTarget::Tables(core::iter::once(TargetName::new(&self.table_name, false)))
    }

    fn with_grant_option(&self) -> bool {
        false
    }

    fn granted_by<'a>(&'a self, _database: &'a Self::DB) -> Option<&'a MemoryRole> {
        None
    }

    fn applies_to_role(&self, _role: &MemoryRole) -> bool {
        true
    }
}

impl ColumnGrantLike for MemoryColumnGrant {
    fn columns<'a>(
        &'a self,
        table: &'a MemoryTable,
        database: &'a Self::DB,
    ) -> Result<impl Iterator<Item = &'a MemoryColumn>, LookupError> {
        table.columns(database)
    }

    fn table<'a>(&'a self, database: &'a Self::DB) -> Option<&'a MemoryTable> {
        database.tables.iter().find(|table| table.name == self.table_name)
    }

    fn relation<'a>(&'a self, _database: &'a Self::DB) -> Option<GrantRelation<'a, Self::DB>> {
        None
    }
}

fn table(schema: Option<&str>, name: &str) -> MemoryTable {
    MemoryTable {
        schema: schema.map(String::from),
        schema_is_quoted: false,
        name: String::from(name),
        name_is_quoted: false,
        undeclared_key_column: None,
    }
}

fn column(table_schema: Option<&str>, table_name: &str, name: &str) -> MemoryColumn {
    MemoryColumn {
        table_schema: table_schema.map(String::from),
        table_name: String::from(table_name),
        name: String::from(name),
        name_is_quoted: false,
        data_type: MemoryDataType(String::from("INT")),
        nullable: false,
        primary_key: name == "id",
    }
}

fn function(schema: Option<&str>, name: &str, argument_types: &[&str]) -> MemoryFunction {
    MemoryFunction {
        name: String::from(name),
        schema: schema.map(String::from),
        argument_types: argument_types.iter().map(|declared| String::from(*declared)).collect(),
    }
}

/// A relation of a view kind, defined by a projection nobody inspects here.
fn view(schema: Option<&str>, name: &str, materialized: bool) -> MemoryView {
    let statements = Parser::parse_sql(&GenericDialect {}, "SELECT id FROM docs")
        .expect("the projection parses");
    let definition = statements
        .into_iter()
        .find_map(|statement| {
            match statement {
                Statement::Query(query) => Some(*query),
                _ => None,
            }
        })
        .expect("the projection is a query");
    MemoryView {
        schema: schema.map(String::from),
        name: String::from(name),
        materialized,
        definition,
        declared_column_names: Vec::new(),
    }
}

/// Two tables named the same in two schemas, one of them the default schema
/// the inherited resolver walks.
fn catalog() -> MemoryCatalog {
    MemoryCatalog {
        catalog_name: String::from("memory"),
        tables: vec![table(None, "docs"), table(Some("app"), "docs"), table(Some("app"), "notes")],
        columns: vec![
            column(None, "docs", "id"),
            column(Some("app"), "docs", "id"),
            column(Some("app"), "docs", "body"),
            column(Some("app"), "notes", "id"),
        ],
        functions: vec![
            function(None, "touch", &[]),
            function(Some("app"), "touch", &[]),
            function(Some("audit"), "hidden", &[]),
            function(Some("app"), "overloaded", &["INT"]),
            function(Some("app"), "overloaded", &["TEXT"]),
        ],
        schemas: vec![MemorySchema { name: String::from("app"), name_is_quoted: false }],
        roles: vec![
            MemoryRole { name: String::from("reader"), name_is_quoted: false },
            MemoryRole { name: String::from("writer"), name_is_quoted: false },
            MemoryRole { name: String::from("Writer"), name_is_quoted: true },
        ],
        role_memberships: vec![
            MemoryRoleMembership {
                role_name: String::from("READER"),
                member_name: String::from("WRITER"),
                set: false,
            },
            MemoryRoleMembership {
                role_name: String::from("platform"),
                member_name: String::from("Writer"),
                set: true,
            },
            MemoryRoleMembership {
                role_name: String::from("writer"),
                member_name: String::from("reader"),
                set: true,
            },
        ],
        ..MemoryCatalog::default()
    }
}

/// The inherited membership readers fold unquoted names before comparing, so
/// a membership reaches the role its member name folds to and never the
/// quoted role spelled alike.
#[test]
fn the_inherited_membership_readers_compare_folded_names() {
    let catalog = catalog();
    let writer = catalog.role("writer").expect("the unquoted role resolves");
    let quoted_writer = catalog.role("Writer").expect("the quoted role resolves");

    let memberships: Vec<_> = writer
        .memberships(&catalog)
        .map(|membership| (membership.stored_role_name().into_owned(), membership.set_option()))
        .collect();
    assert_eq!(memberships, [(String::from("reader"), false), (String::from("platform"), true)]);
    assert_eq!(quoted_writer.memberships(&catalog).count(), 0);

    let parents: Vec<_> = writer.member_of(&catalog).map(RoleLike::name).collect();
    assert_eq!(parents, ["reader"]);

    let first = catalog.role_memberships().next().expect("one membership");
    assert_eq!(first.member(&catalog).map(RoleLike::name), Some("writer"));
    assert_eq!(first.role(&catalog).map(RoleLike::name), Some("reader"));
}

/// The inherited resolver reads an unqualified name through the default schema
/// and a qualified one exactly, which is the body `GenericDB` replaces with an
/// indexed one.
#[test]
fn the_inherited_resolver_walks_the_default_schema() -> Result<(), LookupError> {
    let catalog = catalog();

    let bare = catalog
        .resolve_target_table(TargetName::new("docs", false), IdentifierCase::AsWritten)?
        .expect("the default schema holds a table of that name");
    assert_eq!(bare.table_schema(), None);

    let qualified = catalog
        .resolve_target_table(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .expect("the qualified name resolves in its own schema");
    assert_eq!(qualified.table_schema(), Some("app"));

    // `notes` sits in a schema the path does not carry, so an unqualified
    // reference to it resolves to nothing.
    assert!(
        catalog
            .resolve_target_table(TargetName::new("notes", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        catalog
            .resolve_target_table(TargetName::new("absent", false), IdentifierCase::AsWritten)?
            .is_none()
    );

    Ok(())
}

/// The inherited identity lookup compares both parts as stored, so it neither
/// folds a name nor reads the absent schema as `public`, unlike the written
/// lookup right beside it.
#[test]
fn the_inherited_identity_lookup_compares_stored_parts() {
    let catalog = catalog();

    let bare = catalog
        .table_by_stored_identity(None, "docs")
        .expect("one table is stored without a schema");
    assert_eq!(bare.table_schema(), None);

    let qualified =
        catalog.table_by_stored_identity(Some("app"), "docs").expect("one is stored in `app`");
    assert_eq!(qualified.table_schema(), Some("app"));

    assert!(catalog.table_by_stored_identity(Some("public"), "docs").is_none());
    assert!(catalog.table_by_stored_identity(None, "Docs").is_none());
    assert!(catalog.table_by_stored_identity(Some("audit"), "docs").is_none());

    // The written lookup keeps folding and keeps reading both spellings of the
    // default schema as one place.
    assert!(
        catalog
            .table_by_target(
                TargetName::new("docs", false).with_schema("public", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous lookup")
            .is_some()
    );
    assert!(
        catalog
            .table_by_target(TargetName::new("DOCS", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
}

/// The inherited function resolver reads a qualified reference in its own
/// schema, an unqualified one through the default schema, and says so when a
/// name carries several declarations.
#[test]
fn the_inherited_function_resolver_walks_the_default_schema() -> Result<(), LookupError> {
    let catalog = catalog();

    let bare = catalog
        .resolve_target_function(TargetName::new("touch", false), IdentifierCase::AsWritten)?
        .expect("the default schema holds one");
    assert_eq!(bare.target_name().schema(), None);

    let qualified = catalog
        .resolve_target_function(
            TargetName::new("touch", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .expect("the qualified reference resolves");
    assert_eq!(qualified.target_name().schema(), Some("app"));

    // `audit` is not on the path, so nothing unqualified reaches into it.
    assert!(
        catalog
            .resolve_target_function(TargetName::new("hidden", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        catalog
            .resolve_target_function(TargetName::new("absent", false), IdentifierCase::AsWritten)?
            .is_none()
    );

    let ambiguous = TargetName::new("overloaded", false).with_schema("app", false);
    assert!(matches!(
        catalog.resolve_target_function(ambiguous, IdentifierCase::AsWritten),
        Err(LookupError::AmbiguousFunctionLookup { .. })
    ));

    Ok(())
}

#[test]
fn functions_without_volatility_metadata_report_volatile() -> Result<(), LookupError> {
    let catalog = catalog();
    let function = catalog
        .resolve_target_function(TargetName::new("touch", false), IdentifierCase::AsWritten)?
        .expect("the default schema holds one");

    assert_eq!(function.volatility(), FunctionBehavior::Volatile);
    Ok(())
}

/// An extension that keeps no quoting reads as unquoted, so its stored name
/// and schema fold to lowercase.
#[test]
fn extensions_without_quoting_records_store_folded_names() {
    let extension =
        MemoryExtension { name: String::from("PgCrypto"), schema: Some(String::from("Lab")) };

    assert_eq!(extension.stored_name(), "pgcrypto");
    assert_eq!(extension.stored_schema().as_deref(), Some("lab"));
}

/// A catalog that keeps no `NOT VALID` state reports every constraint
/// validated.
#[test]
fn constraints_without_validation_records_report_validated() -> Result<(), LookupError> {
    let catalog = catalog();
    let check = MemoryCheckConstraint {
        table_name: String::from("docs"),
        expression: Expr::Identifier("id".into()),
    };
    let key = MemoryForeignKey {
        name: None,
        host_table_name: String::from("notes"),
        referenced_table_name: String::from("docs"),
    };

    assert!(check.is_validated(&catalog)?);
    assert!(key.is_validated(&catalog)?);
    Ok(())
}

/// A catalog that records no index clauses reports every index as written
/// without `USING`, `INCLUDE` or `WHERE`.
#[test]
fn indexes_without_clause_records_report_none_written() -> Result<(), LookupError> {
    let catalog = catalog();
    let index = MemoryIndex {
        name: Some(String::from("docs_id")),
        schema: None,
        table_name: String::from("docs"),
        expression: Expr::Identifier("id".into()),
    };

    assert_eq!(index.access_method(), None);
    assert_eq!(index.include_columns(&catalog)?.count(), 0);
    assert_eq!(index.predicate(), None);
    Ok(())
}

/// The inherited function identity lookup compares both parts as stored, and
/// reports a name carrying several declarations rather than choosing one.
#[test]
fn the_inherited_function_identity_lookup_compares_stored_parts() -> Result<(), LookupError> {
    let catalog = catalog();

    assert!(catalog.function_by_stored_identity(None, "touch")?.is_some());
    assert!(catalog.function_by_stored_identity(Some("app"), "touch")?.is_some());
    assert!(catalog.function_by_stored_identity(Some("public"), "touch")?.is_none());
    assert!(catalog.function_by_stored_identity(Some("app"), "Touch")?.is_none());
    assert!(matches!(
        catalog.function_by_stored_identity(Some("app"), "overloaded"),
        Err(LookupError::AmbiguousFunctionLookup { .. })
    ));

    // The written lookup takes the qualifier apart from the name and folds it.
    assert!(
        catalog
            .function_by_target(
                TargetName::new("touch", false).with_schema("APP", false),
                IdentifierCase::AsWritten
            )?
            .is_some()
    );
    assert!(
        catalog
            .function_by_target(TargetName::new("hidden", false), IdentifierCase::AsWritten)?
            .is_none()
    );

    Ok(())
}

/// The inherited view resolvers answer nothing for a catalog holding no views,
/// rather than reaching for a table of that name.
#[test]
fn the_inherited_view_resolvers_answer_only_views() -> Result<(), LookupError> {
    let catalog = catalog();

    assert!(
        catalog
            .resolve_target_view(TargetName::new("docs", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        catalog
            .resolve_target_materialized_view(
                TargetName::new("docs", false),
                IdentifierCase::AsWritten
            )?
            .is_none()
    );
    assert!(catalog.views().next().is_none());
    assert!(catalog.materialized_views().next().is_none());

    Ok(())
}

/// The inherited resolvers take the comparison from the caller, which is the
/// half of the contract a folding engine depends on and `GenericDB` answers
/// through its index instead.
#[test]
fn the_inherited_resolvers_take_the_comparison_from_the_caller() -> Result<(), LookupError> {
    let mut catalog = catalog();
    let mut quoted = table(None, "Docs");
    quoted.name_is_quoted = true;
    catalog.tables = vec![quoted, table(None, "Notes")];
    catalog.columns = vec![column(None, "Docs", "id"), column(None, "Notes", "id")];
    catalog.functions = vec![function(None, "Touch", &[])];

    let docs = || TargetName::new("docs", false);
    assert!(catalog.resolve_target_table(docs(), IdentifierCase::AsWritten)?.is_none());
    assert_eq!(
        catalog.resolve_target_table(docs(), IdentifierCase::Folded)?.map(TableLike::table_name),
        Some("Docs")
    );
    assert!(catalog.resolve_target_table(docs(), IdentifierCase::Exact)?.is_none());

    // A stored name nobody quoted folds under PostgreSQL's rule and stands as
    // written under an exact one.
    let notes = || TargetName::new("notes", false);
    assert!(catalog.resolve_target_table(notes(), IdentifierCase::AsWritten)?.is_some());
    assert!(catalog.resolve_target_table(notes(), IdentifierCase::Exact)?.is_none());
    assert!(
        catalog
            .resolve_target_table(TargetName::new("Notes", false), IdentifierCase::Exact)?
            .is_some()
    );

    let touch = || TargetName::new("touch", false);
    assert!(catalog.resolve_target_function(touch(), IdentifierCase::AsWritten)?.is_some());
    assert!(catalog.resolve_target_function(touch(), IdentifierCase::Exact)?.is_none());

    Ok(())
}

/// The inherited parts lookups consult no search path, so an unqualified name
/// is the default schema's, and they carry the comparison too.
#[test]
fn the_inherited_parts_lookups_ignore_the_search_path() -> Result<(), LookupError> {
    let mut catalog = catalog();
    catalog.views = vec![view(None, "Recent", false)];
    catalog.materialized_views = vec![view(None, "Counted", true)];

    // `docs` sits in both the default schema and `app`, and the parts lookup
    // answers only the one the target names.
    assert_eq!(
        catalog
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?
            .and_then(TableLike::table_schema),
        None
    );
    let quoted_qualifier = || TargetName::new("docs", false).with_schema("APP", true);
    assert!(catalog.table_by_target(quoted_qualifier(), IdentifierCase::AsWritten)?.is_none());
    assert_eq!(
        catalog
            .table_by_target(quoted_qualifier(), IdentifierCase::Folded)?
            .and_then(TableLike::table_schema),
        Some("app")
    );

    let touch = || TargetName::new("TOUCH", false).with_schema("app", false);
    assert!(catalog.function_by_target(touch(), IdentifierCase::AsWritten)?.is_some());
    assert!(catalog.function_by_target(touch(), IdentifierCase::Exact)?.is_none());

    // Both view lookups thread the comparison too: a stored name nobody quoted
    // folds under PostgreSQL's rule and stands as written under an exact one.
    let recent = || TargetName::new("recent", false);
    assert!(catalog.view_by_target(recent(), IdentifierCase::AsWritten)?.is_some());
    assert!(catalog.view_by_target(recent(), IdentifierCase::Exact)?.is_none());
    assert!(
        catalog.view_by_target(TargetName::new("Recent", false), IdentifierCase::Exact)?.is_some()
    );
    let counted = || TargetName::new("counted", false);
    assert!(catalog.materialized_view_by_target(counted(), IdentifierCase::Folded)?.is_some());
    assert!(catalog.materialized_view_by_target(counted(), IdentifierCase::Exact)?.is_none());

    // Each kind answers only its own pool of names.
    assert!(catalog.view_by_target(counted(), IdentifierCase::Folded)?.is_none());
    assert!(catalog.materialized_view_by_target(recent(), IdentifierCase::Folded)?.is_none());
    assert!(
        catalog.view_by_target(TargetName::new("docs", false), IdentifierCase::Folded)?.is_none()
    );

    Ok(())
}

/// Tables, views and materialized views share one pool of names, so a schema
/// holding the name under any kind ends the walk, which is what the database
/// does and what the indexed resolver already did.
#[test]
fn the_inherited_resolver_reads_one_pool_of_names() -> Result<(), LookupError> {
    let mut catalog = catalog();
    catalog.search_path = vec![(String::from("app"), false), (String::from("public"), false)];
    catalog.tables = vec![table(None, "docs")];
    catalog.columns = vec![column(None, "docs", "id")];
    catalog.views = vec![view(Some("app"), "docs", false)];

    // `app` holds the name as a view, so the table in the default schema is
    // not reached, and asking for the view answers it.
    assert!(
        catalog
            .resolve_target_table(TargetName::new("docs", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        catalog
            .resolve_target_view(TargetName::new("docs", false), IdentifierCase::AsWritten)?
            .is_some()
    );

    // The mirror: a table earlier on the path shadows a view later on it.
    catalog.tables = vec![table(Some("app"), "notes")];
    catalog.columns = vec![column(Some("app"), "notes", "id")];
    catalog.views = vec![view(None, "notes", false)];
    assert!(
        catalog
            .resolve_target_view(TargetName::new("notes", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        catalog
            .resolve_target_table(TargetName::new("notes", false), IdentifierCase::AsWritten)?
            .is_some()
    );

    Ok(())
}

/// The rest of what a catalog inherits, answered without a parser in sight.
#[test]
fn the_inherited_accessors_answer_from_the_catalog() -> Result<(), LookupError> {
    let catalog = catalog();

    assert!(catalog.has_tables());
    assert_eq!(catalog.number_of_tables(), 3);
    assert_eq!(catalog.table_by_id(0).map(TableLike::table_name), Some("docs"));
    assert_eq!(catalog.maximum_number_of_columns()?, 2);
    assert!(catalog.schema("app").is_some());
    assert!(catalog.schema("\"App\"").is_none());
    assert!(catalog.role("reader").is_some());
    assert!(!catalog.has_policies());
    assert!(!catalog.has_table_grants());
    assert!(!catalog.has_column_grants());
    assert!(catalog.has_schemas());
    assert!(catalog.has_roles());
    assert!(!catalog.has_rls_tables()?);
    assert_eq!(catalog.number_of_rls_tables()?, 0);
    assert_eq!(catalog.table_dag().expect("the catalog has no cycles").len(), 3);
    // No table extends another here, and a table outside every extension
    // relationship is not a root.
    assert_eq!(catalog.root_tables()?.count(), 0);

    let docs = catalog
        .table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .expect("the qualified lookup finds it");
    assert_eq!(docs.columns(&catalog)?.count(), 2);
    assert_eq!(docs.primary_key_columns(&catalog)?.count(), 1);
    let first_column = docs.columns(&catalog)?.next().expect("a column");
    assert_eq!(catalog.dialect().is_bool(&catalog, first_column), TypeMatch::No);
    let first_type = first_column.data_type(&catalog);
    assert_eq!(first_type.scalar_family(&catalog), Some(ScalarFamily::Int));
    assert_eq!(first_type.normalized_name(&catalog), "INT");
    assert!(!first_type.is_textual(&catalog));
    assert_eq!(first_type.fixed_binary_length(&catalog), None);
    assert_eq!(first_type.max_character_length(&catalog), None);
    assert_eq!(first_type.fractional_seconds_precision(&catalog), None);
    assert_eq!(first_type.numeric_scale(&catalog), None);
    assert_eq!(first_type.interval_fields(&catalog), None);
    assert_eq!(first_type.vector(&catalog), None);
    assert!(first_type.array_element(&catalog).is_none());
    assert_eq!(first_type.array_dimensions(&catalog), None);

    Ok(())
}

/// The inherited identity lookups find each relation in its own kind only,
/// and a column through the table that holds it.
#[test]
fn the_inherited_identity_lookups_answer_each_kind() -> Result<(), LookupError> {
    let mut catalog = catalog();
    catalog.views.push(view(Some("app"), "recent", false));
    catalog.materialized_views.push(view(Some("app"), "recent", true));

    let docs = catalog
        .table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .expect("the qualified lookup finds it");
    let table = catalog.table_identity(docs).expect("the catalog holds the table");
    assert_eq!(catalog.table_by_identity(table), Some(docs));
    assert_eq!(catalog.view_by_identity(table), None);

    let recent = catalog.views().next().expect("a view");
    let view = catalog.view_identity(recent).expect("the catalog holds the view");
    assert_eq!(catalog.view_by_identity(view), Some(recent));
    assert_eq!(catalog.materialized_view_by_identity(view), None);
    let snapshot = catalog.materialized_views().next().expect("a materialized view");
    let materialized =
        catalog.materialized_view_identity(snapshot).expect("the catalog holds the view");
    assert_eq!(catalog.materialized_view_by_identity(materialized), Some(snapshot));
    assert_eq!(catalog.table_by_identity(materialized), None);

    let body = docs.columns(&catalog)?.nth(1).expect("a second column");
    let identity = catalog.column_identity(body).expect("the catalog holds the column");
    assert_eq!(catalog.column_by_identity(identity), Some(body));
    assert_eq!(catalog.column_identity(&column(Some("app"), "docs", "absent")), None);

    Ok(())
}

/// A key column the table does not declare is an error, not a shorter key.
#[test]
fn an_unresolvable_key_column_stops_the_ordinals() -> Result<(), LookupError> {
    let mut catalog = catalog();
    let mut docs = table(Some("app"), "docs");
    docs.undeclared_key_column = Some(column(Some("app"), "docs", "dropped_id"));
    catalog.tables = vec![docs];
    catalog.columns = vec![
        column(Some("app"), "docs", "body"),
        column(Some("app"), "docs", "id"),
        column(Some("app"), "docs", "extra"),
    ];

    let docs = catalog
        .table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .expect("the qualified lookup finds it");

    assert_eq!(docs.column_id_by_name("id", &catalog, IdentifierCase::AsWritten)?, Some(1));
    assert_eq!(docs.column_name_by_id(2, &catalog)?, Some("extra"));
    assert_eq!(docs.column_name_by_id(3, &catalog)?, None);
    assert_eq!(
        docs.primary_key_column_ids(&catalog),
        Err(LookupError::ColumnNotFound {
            table_name: String::from("docs"),
            column_name: String::from("dropped_id"),
        })
    );

    Ok(())
}

/// A key with no resolvable column at all is that same error, not an empty
/// key, and the first key column is the one named.
#[test]
fn a_wholly_unresolvable_key_names_its_first_column() {
    let mut catalog = catalog();
    let mut docs = table(Some("app"), "docs");
    docs.undeclared_key_column = Some(column(Some("app"), "docs", "dropped_id"));
    catalog.tables = vec![docs];
    catalog.columns = vec![column(Some("app"), "docs", "body")];

    let docs = catalog
        .table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the qualified lookup finds it");

    assert_eq!(
        docs.primary_key_column_ids(&catalog),
        Err(LookupError::ColumnNotFound {
            table_name: String::from("docs"),
            column_name: String::from("dropped_id"),
        })
    );
}
