//! Implementations for [`ParserDB`] - a database schema parsed from SQL text.

use alloc::{
    borrow::{Cow, ToOwned},
    boxed::Box,
    collections::BTreeSet,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;
#[cfg(feature = "std")]
use std::path::{Path, PathBuf};

#[cfg(feature = "git")]
use git2::Repository;
use sql_docs::SqlDoc;
#[cfg(feature = "std")]
use sqlparser::parser::ParserError;
use sqlparser::{
    ast::{
        Action, AlterColumnOperation, AlterFunction, AlterFunctionAction, AlterFunctionKind,
        AlterFunctionOperation, AlterIndexOperation, AlterPolicy, AlterPolicyOperation,
        AlterRoleOperation, AlterSchema, AlterSchemaOperation, AlterTableOperation, ArgMode,
        ArrayElemTypeDef, CheckConstraint, ColumnDef, ColumnOption, ColumnOptionDef,
        CreateCollation, CreateCollationDefinition, CreateFunction, CreateFunctionBody,
        CreateIndex, CreatePolicy, CreateRole, CreateTable, CreateTrigger, DataType, DropBehavior,
        ExactNumberInfo, Expr, ForeignKeyConstraint, FunctionDesc, FunctionReturnType, GeneratedAs,
        Grant, GrantObjects, Grantee, GranteeName, GranteesType, Ident, IndexColumn,
        MySQLColumnPosition, ObjectName, ObjectNamePart, OperateFunctionArg, OrderByExpr,
        OrderByOptions, Owner, Privileges, Query, RenameTableNameKind, ResetConfig, SchemaName,
        SqlOption, Statement, TableConstraint, TimezoneInfo, TriggerEvent, UniqueConstraint, Value,
        ValueWithSpan, Visit, VisitMut, Visitor, VisitorMut, visit_relations,
    },
    dialect::Dialect,
    parser::Parser,
    tokenizer::Span,
};

use crate::{
    errors::{LookupError, ObjectKind},
    impls::SqlparserDialect,
    structs::{
        ColumnMetadata, GenericDB, IdentifierCase, MaterializedView, Schema, SchemaProfile,
        TableAttribute, TableMetadata, View,
        metadata::{
            CheckMetadata, FunctionMetadata, IndexMetadata, PolicyMetadata, UniqueIndexMetadata,
        },
    },
    traits::{ColumnLike, FunctionLike, IndexLike, TableLike, ViewLike},
    utils::{
        columns_in_expression,
        identifier_resolution::{
            identifiers_match, is_public_pseudo_role, normalize_identifier, session_principal,
        },
        last_str, normalize_postgres_type_cow, normalize_sqlparser_type,
        object_name::{
            Qualifier, RelationKey, TEMPORARY_SCHEMA, is_temporary_schema, object_name_identifiers,
            object_name_last_part, overqualified_object_name, qualifier_of, relation_search_path,
            require_local_object_name, require_static_object_name,
            resolve_table_object_name_in_iter, resolve_table_object_name_on_search_path_in_iter,
            resolve_target_on_search_path_in_iter, resolve_view_on_search_path_in_iter,
            stored_table_key, table_matches_key, table_matches_object_name, target_name_of_idents,
            target_name_of_object_name, view_matches_key,
        },
    },
};

mod column_copy;
mod functions_in_expression;
mod inheritance;
mod like;
mod parse_options;
mod postgres_18_collations;
mod postgres_catalog;
mod views;

pub use parse_options::{AccessResolution, ParseOptions};
pub use postgres_catalog::{PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType};

/// The object kinds a schema parsed from SQL text holds, each an `sqlparser`
/// AST node or a wrapper pairing one with the table it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlparserProfile;

impl SchemaProfile for SqlparserProfile {
    type Table = CreateTable;
    type View = View;
    type MaterializedView = MaterializedView;
    type Column = TableAttribute<CreateTable, ColumnDef>;
    type Index = TableAttribute<CreateTable, CreateIndex>;
    type UniqueIndex = TableAttribute<CreateTable, UniqueConstraint>;
    type ForeignKey = TableAttribute<CreateTable, ForeignKeyConstraint>;
    type Function = CreateFunction;
    type CheckConstraint = TableAttribute<CreateTable, CheckConstraint>;
    type Trigger = CreateTrigger;
    type Policy = CreatePolicy;
    type Role = CreateRole;
    type Schema = Schema;
    type TableGrant = Grant;
    type ColumnGrant = Grant;
    type Dialect = SqlparserDialect;
    type Ingestion = Arc<ParserIngestion>;

    fn default_ingestion(dialect: &SqlparserDialect) -> Arc<ParserIngestion> {
        Arc::new(ParserIngestion {
            access_resolution: AccessResolution::default(),
            active_postgres_catalog: Arc::new(if matches!(dialect, SqlparserDialect::PostgreSql) {
                PostgresCatalog::default()
            } else {
                PostgresCatalog::empty()
            }),
            collation_metadata: Vec::new(),
        })
    }
}

/// A type alias for a `GenericDBBuilder` specialized for `sqlparser`'s
/// `CreateTable`.
pub type ParserDBBuilder = super::GenericDBBuilder<SqlparserProfile>;

#[derive(Debug, Clone)]
struct CreatedCollationMetadata {
    name: ObjectName,
    postgres_deterministic: Option<bool>,
}

/// The continuation state the parser carries inside every built database so
/// statement-by-statement ingestion can resume from it: the
/// access-resolution mode, the active PostgreSQL catalog, and metadata for
/// collations the ingested DDL created.
///
/// The catalog sits behind an [`Arc`], so cloning a database or taking a
/// snapshot bumps a reference count rather than copying the catalog's
/// collation list. The rare statement that changes the catalog copies on
/// write.
#[derive(Clone)]
pub struct ParserIngestion {
    access_resolution: AccessResolution,
    active_postgres_catalog: Arc<PostgresCatalog>,
    collation_metadata: Vec<CreatedCollationMetadata>,
}

impl core::fmt::Debug for ParserIngestion {
    /// Summarizes the catalog by size: the default one alone holds hundreds
    /// of collations.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParserIngestion")
            .field("access_resolution", &self.access_resolution)
            .field("catalog_collations", &self.active_postgres_catalog.collations().count())
            .field(
                "catalog_collatable_types",
                &self.active_postgres_catalog.collatable_types().count(),
            )
            .field("created_collations", &self.collation_metadata.len())
            .finish()
    }
}

/// A statement-by-statement builder that produces queryable schema snapshots.
pub struct ParserDBIngestor {
    builder: ParserDBBuilder,
    ingestion: Arc<ParserIngestion>,
}

#[derive(Clone, Copy)]
struct ActiveCollations<'a> {
    created: &'a [CreatedCollationMetadata],
    catalog: &'a PostgresCatalog,
}

enum CollationResolution<'a> {
    Created(&'a CreatedCollationMetadata),
    Catalog(&'a PostgresCatalogCollation),
}

impl CollationResolution<'_> {
    fn into_column_metadata(self) -> ColumnMetadata {
        match self {
            Self::Created(metadata) => column_metadata_from_created_collation(metadata),
            Self::Catalog(metadata) => column_metadata_from_catalog_collation(metadata),
        }
    }

    fn matches_catalog_default(&self) -> bool {
        matches!(
            self,
            Self::Catalog(metadata)
                if identifiers_match("default", false, metadata.name(), metadata.name_is_quoted())
                    && metadata.schema().is_some_and(|schema| {
                        identifiers_match(
                            "pg_catalog",
                            false,
                            schema,
                            metadata.schema_is_quoted(),
                        )
                    })
        )
    }
}

#[derive(Debug, Clone)]
struct PreservedColumnMetadata {
    column_name: String,
    column_name_quoted: bool,
    collation: Option<ObjectName>,
    metadata: ColumnMetadata,
}

fn qualify_created_collation_name(
    builder: &ParserDBBuilder,
    name: &ObjectName,
) -> Result<ObjectName, crate::errors::Error> {
    if name.0.len() != 1 {
        return Ok(name.clone());
    }
    for (schema, schema_quoted) in builder.search_path().filter(|(schema, _)| !schema.is_empty()) {
        if identifiers_match("public", false, schema, schema_quoted) {
            return Ok(name.clone());
        }
        if identifiers_match("pg_catalog", false, schema, schema_quoted) {
            return Ok(ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                name.0[0].clone(),
            ]));
        }
        if let Some(schema) = declared_schema(builder, schema, schema_quoted) {
            let schema = if schema.is_quoted() {
                Ident::with_quote('"', schema.name())
            } else {
                Ident::new(schema.name())
            };
            return Ok(ObjectName(vec![ObjectNamePart::Identifier(schema), name.0[0].clone()]));
        }
    }
    Err(crate::errors::Error::NoSchemaSelectedForCollation { collation_name: name.to_string() })
}

fn create_collation_metadata(
    builder: &ParserDBBuilder,
    create_collation: &CreateCollation,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> Result<Option<CreatedCollationMetadata>, crate::errors::Error> {
    validate_created_collation_schema(builder, &create_collation.name)?;
    let name = qualify_created_collation_name(builder, &create_collation.name)?;
    let postgres_deterministic = create_collation_deterministic(
        &create_collation.definition,
        collations,
        catalog,
        search_path,
    )?;
    if collation_already_exists(&name, collations, catalog) {
        if create_collation.if_not_exists {
            return Ok(None);
        }
        return Err(crate::errors::Error::CollationAlreadyExists {
            collation_name: name.to_string().into_boxed_str(),
        });
    }
    Ok(Some(CreatedCollationMetadata { name, postgres_deterministic }))
}

fn create_collation_deterministic(
    definition: &CreateCollationDefinition,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> Result<Option<bool>, crate::errors::Error> {
    match definition {
        CreateCollationDefinition::Options(options) => {
            create_collation_options_deterministic(options)
        }
        CreateCollationDefinition::From(source) => {
            if collation_source_is_pg_catalog_default(source, collations, catalog, search_path) {
                return Err(crate::errors::Error::CollationCannotBeCopied {
                    collation_name: source.to_string().into_boxed_str(),
                });
            }
            column_metadata_for_collation_name(source, collations, catalog, search_path)
                .map(|metadata| metadata.postgres_deterministic())
                .ok_or_else(|| {
                    crate::errors::Error::CollationNotFound {
                        collation_name: source.to_string().into_boxed_str(),
                    }
                })
        }
    }
}

fn create_collation_options_deterministic(
    options: &[SqlOption],
) -> Result<Option<bool>, crate::errors::Error> {
    let mut seen: Vec<&Ident> = Vec::new();
    let mut deterministic = Some(true);
    for option in options {
        let SqlOption::KeyValue { key, value } = option else {
            return Err(invalid_collation_option("option", option.to_string()));
        };
        if seen.iter().any(|seen| idents_match(seen, key)) {
            return Err(crate::errors::Error::RepeatedCollationOption {
                option_name: key.to_string().into_boxed_str(),
            });
        }
        seen.push(key);
        if collation_option_key_matches(key, "provider") {
            validate_collation_provider(key, value)?;
        } else if collation_option_key_matches(key, "deterministic") {
            deterministic = Some(collation_option_bool(key, value)?);
        } else if !collation_option_key_matches(key, "locale")
            && !collation_option_key_matches(key, "lc_collate")
            && !collation_option_key_matches(key, "lc_ctype")
            && !collation_option_key_matches(key, "rules")
            && !collation_option_key_matches(key, "version")
        {
            return Err(invalid_collation_option(&key.to_string(), value.to_string()));
        }
    }
    Ok(deterministic)
}

fn collation_option_key_matches(key: &Ident, lookup: &str) -> bool {
    identifiers_match(lookup, false, &key.value, key.quote_style.is_some())
}

fn collation_option_bool(key: &Ident, value: &Expr) -> Result<bool, crate::errors::Error> {
    match value {
        Expr::Value(ValueWithSpan { value: Value::Boolean(value), .. }) => Ok(*value),
        Expr::Identifier(ident) if ident.quote_style.is_none() => {
            if ident.value.eq_ignore_ascii_case("true") {
                Ok(true)
            } else if ident.value.eq_ignore_ascii_case("false") {
                Ok(false)
            } else {
                Err(invalid_collation_option(&key.to_string(), value.to_string()))
            }
        }
        _ => Err(invalid_collation_option(&key.to_string(), value.to_string())),
    }
}

fn validate_collation_provider(key: &Ident, value: &Expr) -> Result<(), crate::errors::Error> {
    let Expr::Identifier(provider) = value else {
        return Err(invalid_collation_option(&key.to_string(), value.to_string()));
    };
    if provider.quote_style.is_none()
        && (provider.value.eq_ignore_ascii_case("builtin")
            || provider.value.eq_ignore_ascii_case("libc")
            || provider.value.eq_ignore_ascii_case("icu"))
    {
        Ok(())
    } else {
        Err(invalid_collation_option(&key.to_string(), value.to_string()))
    }
}

fn invalid_collation_option(
    option_name: &str,
    option_value: impl Into<Box<str>>,
) -> crate::errors::Error {
    crate::errors::Error::InvalidCollationOption {
        option_name: option_name.into(),
        option_value: option_value.into(),
    }
}

fn column_collation_name(
    column: &ColumnDef,
    validate_postgres: bool,
) -> Result<Option<&ObjectName>, crate::errors::Error> {
    if !validate_postgres {
        return Ok(stored_column_collation_name(column));
    }
    let mut collation = None;
    for option in &column.options {
        if let ColumnOption::Collation(name) = &option.option {
            if collation.is_some() {
                return Err(crate::errors::Error::RepeatedColumnCollation {
                    column_name: column.name.to_string().into_boxed_str(),
                });
            }
            collation = Some(name);
        }
    }
    Ok(collation)
}

pub(super) fn stored_column_collation_name(column: &ColumnDef) -> Option<&ObjectName> {
    column.options.iter().find_map(|option| {
        match &option.option {
            ColumnOption::Collation(name) => Some(name),
            _ => None,
        }
    })
}
fn collation_names_match(stored: &ObjectName, lookup: &ObjectName) -> bool {
    let Some((stored_name, stored_quoted)) = object_name_last_part(stored) else {
        return false;
    };
    let Some((lookup_name, lookup_quoted)) = object_name_last_part(lookup) else {
        return false;
    };
    identifiers_match(stored_name, stored_quoted, lookup_name, lookup_quoted)
}

fn collation_names_match_parts(
    stored_name: &str,
    stored_quoted: bool,
    lookup: &ObjectName,
) -> bool {
    let Some((lookup_name, lookup_quoted)) = object_name_last_part(lookup) else {
        return false;
    };
    identifiers_match(stored_name, stored_quoted, lookup_name, lookup_quoted)
}

fn collation_schema_matches(stored: &ObjectName, lookup_schema: &str, lookup_quoted: bool) -> bool {
    match qualifier_of(stored) {
        Qualifier::Named(stored_schema, stored_quoted) => {
            identifiers_match(stored_schema, stored_quoted, lookup_schema, lookup_quoted)
        }
        Qualifier::Absent => identifiers_match("public", false, lookup_schema, lookup_quoted),
        // A qualifier built at run time names no schema to compare against.
        Qualifier::RunTime => false,
    }
}

fn created_collation_metadata_for_schema<'a>(
    lookup: &ObjectName,
    collations: &'a [CreatedCollationMetadata],
    schema: &str,
    schema_quoted: bool,
) -> Option<&'a CreatedCollationMetadata> {
    collations.iter().rev().find(|metadata| {
        collation_names_match(&metadata.name, lookup)
            && collation_schema_matches(&metadata.name, schema, schema_quoted)
    })
}

fn rename_created_collation_schemas(
    collations: &mut [CreatedCollationMetadata],
    from: &str,
    from_quoted: bool,
    to: &str,
    to_quoted: bool,
) {
    for metadata in collations {
        let renamed = |quoted: bool| {
            if quoted { Ident::with_quote('"', to) } else { Ident::new(to) }
        };
        match qualifier_of(&metadata.name) {
            Qualifier::Named(schema, schema_quoted) => {
                if identifiers_match(schema, schema_quoted, from, from_quoted) {
                    metadata.name.0[0] = ObjectNamePart::Identifier(renamed(to_quoted));
                }
            }
            Qualifier::Absent => {
                if identifiers_match("public", false, from, from_quoted) {
                    metadata.name.0.insert(0, ObjectNamePart::Identifier(renamed(to_quoted)));
                }
            }
            // Which schema the name will carry is decided when the statement
            // runs, so a rename of one schema cannot claim it.
            Qualifier::RunTime => {}
        }
    }
}

/// The schema a collation name resolves in, `public` when it names none.
///
/// A creation refuses a run-time qualifier before reaching here, and the
/// resolvers refuse one on their own, so only a readable qualifier or none
/// arrives.
fn collation_effective_schema(name: &ObjectName) -> (&str, bool) {
    qualifier_of(name).named().unwrap_or(("public", false))
}

fn collation_already_exists(
    name: &ObjectName,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
) -> bool {
    let (schema, schema_quoted) = collation_effective_schema(name);
    collation_resolution_for_schema(name, collations, catalog, schema, schema_quoted).is_some()
}

fn search_path_names_pg_catalog(search_path: &[(String, bool)]) -> bool {
    search_path
        .iter()
        .any(|(schema, quoted)| identifiers_match("pg_catalog", false, schema, *quoted))
}

fn collation_name_is_default_builtin(lookup: &ObjectName) -> bool {
    object_name_last_part(lookup)
        .is_some_and(|(name, quoted)| identifiers_match("default", false, name, quoted))
}

fn collation_source_is_pg_catalog_default(
    source: &ObjectName,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> bool {
    if !collation_name_is_default_builtin(source) {
        return false;
    }
    collation_resolution_for_name(source, collations, catalog, search_path)
        .is_some_and(|resolution| resolution.matches_catalog_default())
}

fn preserved_column_metadata_for_table(
    builder: &ParserDBBuilder,
    table: &CreateTable,
) -> Vec<PreservedColumnMetadata> {
    builder
        .columns()
        .iter()
        .filter(|(column, _)| TableAttribute::table(column) == table)
        .map(|(column, metadata)| {
            PreservedColumnMetadata {
                column_name: column.column_name().to_string(),
                column_name_quoted: column.column_name_is_quoted(),
                collation: stored_column_collation_name(column.attribute()).cloned(),
                metadata: metadata.clone(),
            }
        })
        .collect()
}

fn preserved_column_metadata_for_column(
    column: &ColumnDef,
    preserved: &[PreservedColumnMetadata],
) -> Option<ColumnMetadata> {
    let collation = stored_column_collation_name(column).cloned();
    preserved
        .iter()
        .find(|metadata| {
            identifiers_match(
                &metadata.column_name,
                metadata.column_name_quoted,
                column.name.value.as_str(),
                column.name.quote_style.is_some(),
            ) && metadata.collation == collation
        })
        .map(|metadata| metadata.metadata.clone())
}

fn rename_preserved_column_metadata(
    preserved: &mut [PreservedColumnMetadata],
    from: &NamedColumn,
    to: &Ident,
) {
    for metadata in preserved {
        if identifiers_match(
            &metadata.column_name,
            metadata.column_name_quoted,
            &from.name,
            from.quoted,
        ) {
            metadata.column_name.clone_from(&to.value);
            metadata.column_name_quoted = to.quote_style.is_some();
        }
    }
}

fn column_metadata_from_created_collation(metadata: &CreatedCollationMetadata) -> ColumnMetadata {
    let Some(name) = object_name_last_part(&metadata.name) else {
        return ColumnMetadata::default()
            .with_postgres_deterministic(metadata.postgres_deterministic);
    };
    ColumnMetadata::default()
        .with_postgres_deterministic(metadata.postgres_deterministic)
        .with_postgres_collation(qualifier_of(&metadata.name).named(), name)
}

fn column_metadata_from_catalog_collation(metadata: &PostgresCatalogCollation) -> ColumnMetadata {
    let base =
        ColumnMetadata::default().with_postgres_deterministic(Some(metadata.deterministic()));
    if identifiers_match("default", false, metadata.name(), metadata.name_is_quoted())
        && metadata.schema().is_some_and(|schema| {
            identifiers_match("pg_catalog", false, schema, metadata.schema_is_quoted())
        })
    {
        return base.with_postgres_default_collation();
    }
    base.with_postgres_collation(
        metadata.schema().map(|schema| (schema, metadata.schema_is_quoted())),
        (metadata.name(), metadata.name_is_quoted()),
    )
}

fn collation_resolution_for_schema<'a>(
    lookup: &ObjectName,
    collations: &'a [CreatedCollationMetadata],
    catalog: &'a PostgresCatalog,
    schema: &str,
    schema_quoted: bool,
) -> Option<CollationResolution<'a>> {
    created_collation_metadata_for_schema(lookup, collations, schema, schema_quoted)
        .map(CollationResolution::Created)
        .or_else(|| {
            catalog_collation_for_schema(lookup, catalog, schema, schema_quoted)
                .map(CollationResolution::Catalog)
        })
}

fn catalog_collation_for_schema<'a>(
    lookup: &ObjectName,
    catalog: &'a PostgresCatalog,
    schema: &str,
    schema_quoted: bool,
) -> Option<&'a PostgresCatalogCollation> {
    catalog.collations().rev().find(|metadata| {
        let Some(metadata_schema) = metadata.schema() else {
            return false;
        };
        collation_names_match_parts(metadata.name(), metadata.name_is_quoted(), lookup)
            && identifiers_match(
                metadata_schema,
                metadata.schema_is_quoted(),
                schema,
                schema_quoted,
            )
    })
}

fn collation_resolution_for_name<'a>(
    name: &ObjectName,
    collations: &'a [CreatedCollationMetadata],
    catalog: &'a PostgresCatalog,
    search_path: &[(String, bool)],
) -> Option<CollationResolution<'a>> {
    match qualifier_of(name) {
        Qualifier::Named(schema, schema_quoted) => {
            return collation_resolution_for_schema(
                name,
                collations,
                catalog,
                schema,
                schema_quoted,
            );
        }
        // A name built at run time resolves to nothing rather than being
        // walked along the search path as though it were unqualified.
        Qualifier::RunTime => return None,
        Qualifier::Absent => {}
    }

    if !search_path_names_pg_catalog(search_path)
        && let Some(resolution) =
            collation_resolution_for_schema(name, collations, catalog, "pg_catalog", false)
    {
        return Some(resolution);
    }

    for (schema, schema_quoted) in search_path {
        if let Some(resolution) =
            collation_resolution_for_schema(name, collations, catalog, schema, *schema_quoted)
        {
            return Some(resolution);
        }
    }

    None
}
fn catalog_type_for_schema<'a>(
    lookup: &ObjectName,
    catalog: &'a PostgresCatalog,
    schema: &str,
    schema_quoted: bool,
) -> Option<&'a PostgresCatalogType> {
    catalog.collatable_types().find(|metadata| {
        let Some(metadata_schema) = metadata.schema() else {
            return false;
        };
        collation_names_match_parts(metadata.name(), metadata.name_is_quoted(), lookup)
            && identifiers_match(
                metadata_schema,
                metadata.schema_is_quoted(),
                schema,
                schema_quoted,
            )
    })
}

fn catalog_type_for_name<'a>(
    name: &ObjectName,
    catalog: &'a PostgresCatalog,
    search_path: &[(String, bool)],
) -> Option<&'a PostgresCatalogType> {
    match qualifier_of(name) {
        Qualifier::Named(schema, schema_quoted) => {
            return catalog_type_for_schema(name, catalog, schema, schema_quoted);
        }
        Qualifier::RunTime => return None,
        Qualifier::Absent => {}
    }
    if !search_path_names_pg_catalog(search_path)
        && let Some(metadata) = catalog_type_for_schema(name, catalog, "pg_catalog", false)
    {
        return Some(metadata);
    }
    for (schema, schema_quoted) in search_path {
        if let Some(metadata) = catalog_type_for_schema(name, catalog, schema, *schema_quoted) {
            return Some(metadata);
        }
    }
    None
}

fn postgres_builtin_type_name(data_type: &DataType) -> Option<&'static str> {
    match data_type {
        DataType::Text => Some("text"),
        DataType::Varchar(_) | DataType::CharacterVarying(_) | DataType::CharVarying(_) => {
            Some("varchar")
        }
        DataType::Char(_) | DataType::Character(_) => Some("bpchar"),
        _ => None,
    }
}

fn array_element_data_type(data_type: &DataType) -> Option<&DataType> {
    match data_type {
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element) | ArrayElemTypeDef::SquareBracket(element, _),
        ) => Some(element),
        _ => None,
    }
}

fn object_name_from_unquoted_ident(name: &str) -> ObjectName {
    ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))])
}

fn validate_postgres_column_collation_type(
    column: &ColumnDef,
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> Result<(), crate::errors::Error> {
    validate_postgres_collatable_type(&column.data_type, catalog, search_path).map_err(|error| {
        error.into_column_error(
            &column.name.to_string(),
            &normalize_sqlparser_type(&column.data_type),
        )
    })
}

enum CollatableTypeError {
    NonCollatable,
    MissingCatalogFact,
}

impl CollatableTypeError {
    fn into_column_error(self, column_name: &str, type_name: &str) -> crate::errors::Error {
        match self {
            Self::NonCollatable => {
                crate::errors::Error::NonCollatableColumnType {
                    column_name: column_name.into(),
                    type_name: type_name.into(),
                }
            }
            Self::MissingCatalogFact => {
                crate::errors::Error::ColumnTypeCollatabilityNotInCatalog {
                    column_name: column_name.into(),
                    type_name: type_name.into(),
                }
            }
        }
    }
}

fn validate_postgres_collatable_type(
    data_type: &DataType,
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> Result<(), CollatableTypeError> {
    if let Some(element) = array_element_data_type(data_type) {
        return validate_postgres_collatable_type(element, catalog, search_path);
    }
    if let Some(name) = postgres_builtin_type_name(data_type) {
        let lookup = object_name_from_unquoted_ident(name);
        return catalog_type_for_schema(&lookup, catalog, "pg_catalog", false)
            .map(|_| ())
            .ok_or(CollatableTypeError::NonCollatable);
    }
    if let DataType::Custom(name, _) = data_type {
        return catalog_type_for_name(name, catalog, search_path)
            .map(|_| ())
            .ok_or(CollatableTypeError::MissingCatalogFact);
    }
    Err(CollatableTypeError::NonCollatable)
}

fn column_metadata_for_collation_name(
    name: &ObjectName,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
) -> Option<ColumnMetadata> {
    collation_resolution_for_name(name, collations, catalog, search_path)
        .map(CollationResolution::into_column_metadata)
}

fn should_validate_missing_collations(dialect: SqlparserDialect) -> bool {
    matches!(dialect, SqlparserDialect::PostgreSql)
}

fn column_metadata_for_collations(
    column: &ColumnDef,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    search_path: &[(String, bool)],
    preserved: &[PreservedColumnMetadata],
    validate_missing: bool,
) -> Result<ColumnMetadata, crate::errors::Error> {
    if let Some(metadata) = preserved_column_metadata_for_column(column, preserved) {
        return Ok(metadata);
    }
    let Some(name) = column_collation_name(column, validate_missing)? else {
        return Ok(ColumnMetadata::default().with_postgres_default_collation());
    };
    let metadata = column_metadata_for_collation_name(name, collations, catalog, search_path)
        .map_or_else(
            || {
                if validate_missing {
                    Err(crate::errors::Error::CollationNotFound {
                        collation_name: name.to_string().into_boxed_str(),
                    })
                } else {
                    Ok(ColumnMetadata::default())
                }
            },
            Ok,
        )?;
    if validate_missing {
        validate_postgres_column_collation_type(column, catalog, search_path)?;
    }
    Ok(metadata)
}

fn existing_child_column_collation_conflict(
    builder: &ParserDBBuilder,
    parent: &StoredTable,
    child: &StoredTable,
    added: &NamedColumn,
    inherited_def: &ColumnDef,
    inherited_metadata: &ColumnMetadata,
) -> Option<crate::errors::Error> {
    let (child_column, child_metadata) = builder.columns().iter().find(|(column, _)| {
        child.matches(TableAttribute::table(column), builder.identifier_case())
            && added.matches(&column.attribute().name)
    })?;
    if child_metadata.postgres_collation_matches(inherited_metadata) != Some(false) {
        return None;
    }
    Some(crate::errors::Error::InheritedColumnCollationConflict {
        column_name: added.name.clone().into_boxed_str(),
        child_table: child.name.clone().into_boxed_str(),
        child_collation: stored_column_collation_name(child_column.attribute())
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
        parent_table: parent.name.clone().into_boxed_str(),
        parent_collation: stored_column_collation_name(inherited_def)
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
    })
}

impl ParserDBBuilder {
    /// Checks if a function with the given name is referenced by any schema
    /// object.
    ///
    /// Returns `true` if the function is used by:
    /// - Check constraints (via their metadata)
    /// - Policies (via USING or WITH CHECK expressions)
    /// - Triggers (via EXECUTE FUNCTION)
    fn is_function_used(&self, function_name: &str, function_name_quoted: bool) -> bool {
        use crate::traits::{FunctionLike, TriggerLike};

        // Check if any check constraint references the function
        for (_, metadata) in self.check_constraints() {
            if metadata.functions().any(|f| {
                identifiers_match(f.name(), f.name_is_quoted(), function_name, function_name_quoted)
            }) {
                return true;
            }
        }

        // Check if any policy references the function
        for (_, metadata) in self.policies() {
            if metadata.using_functions().any(|f| {
                identifiers_match(f.name(), f.name_is_quoted(), function_name, function_name_quoted)
            }) {
                return true;
            }
            if metadata.check_functions().any(|f| {
                identifiers_match(f.name(), f.name_is_quoted(), function_name, function_name_quoted)
            }) {
                return true;
            }
        }

        // Check if any trigger executes the function
        for (trigger, ()) in self.triggers() {
            if trigger.function_name_ident().is_some_and(|(name, quoted)| {
                identifiers_match(name, quoted, function_name, function_name_quoted)
            }) {
                return true;
            }
        }

        false
    }

    /// Checks if a table with the given name is referenced by foreign keys from
    /// other tables.
    ///
    /// Returns `true` if any other table has a foreign key pointing to this
    /// table.
    fn is_table_referenced(
        &self,
        table_name: &str,
        table_name_quoted: bool,
        schema_name: Option<&str>,
        schema_quoted: bool,
    ) -> bool {
        let case = self.identifier_case();
        for (fk, ()) in self.foreign_keys() {
            // Check if this FK references the table being dropped
            // and is NOT from the same table (self-referential FKs are OK to
            // drop)
            let Some(referenced_table) = resolve_table_object_name_in_iter(
                self.tables().iter().map(|(table, _)| table.as_ref()),
                &fk.attribute().foreign_table,
                self.identifier_case(),
            )
            .ok()
            .flatten() else {
                continue;
            };
            let Some(host_table) = resolve_table_object_name_in_iter(
                self.tables().iter().map(|(table, _)| table.as_ref()),
                &fk.table().name,
                self.identifier_case(),
            )
            .ok()
            .flatten() else {
                continue;
            };

            let referenced_matches = table_matches_resolved_identity(
                referenced_table,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
                case,
            );
            let host_matches = table_matches_resolved_identity(
                host_table,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
                case,
            );

            if referenced_matches && !host_matches {
                return true;
            }
        }

        false
    }

    /// Removes a table and all its associated schema objects.
    ///
    /// This removes:
    /// - The table itself
    /// - All columns belonging to the table
    /// - All indices on the table
    /// - All unique indices on the table
    /// - All foreign keys from the table
    /// - All check constraints on the table
    /// - All triggers on the table
    /// - All policies on the table
    /// - All grants on the table
    fn remove_table(
        &mut self,
        table_name: &str,
        table_name_quoted: bool,
        schema_name: Option<&str>,
        schema_quoted: bool,
    ) {
        let case = self.identifier_case();
        let is_target = |table: &CreateTable| {
            table_matches_resolved_identity(
                table,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
                case,
            )
        };

        self.tables_mut().retain(|(table, _)| !is_target(table));
        self.columns_mut().retain(|(column, _)| !is_target(TableAttribute::table(column)));
        self.indices_mut().retain(|(index, _)| !is_target(TableAttribute::table(index)));
        self.unique_indices_mut().retain(|(index, _)| !is_target(TableAttribute::table(index)));
        self.foreign_keys_mut().retain(|(key, ())| !is_target(TableAttribute::table(key)));
        self.check_constraints_mut()
            .retain(|(constraint, _)| !is_target(TableAttribute::table(constraint)));

        // Remove triggers on this table
        self.triggers_mut().retain(|(t, ())| {
            !object_name_matches_resolved_identity(
                &t.table_name,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
            )
        });

        // Remove policies on this table
        self.policies_mut().retain(|(p, _)| {
            !object_name_matches_resolved_identity(
                &p.table_name,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
            )
        });

        // Remove table grants for this table
        self.table_grants_mut().retain(|(g, ())| {
            use sqlparser::ast::GrantObjects;
            !matches!(&g.objects, Some(GrantObjects::Tables(tables)) if tables.iter().any(|t| {
                object_name_matches_resolved_identity(
                    t,
                    table_name,
                    table_name_quoted,
                    schema_name,
                    schema_quoted,
                )
            }))
        });

        // Remove column grants for this table
        self.column_grants_mut().retain(|(g, ())| {
            use sqlparser::ast::GrantObjects;
            !matches!(&g.objects, Some(GrantObjects::Tables(tables)) if tables.iter().any(|t| {
                object_name_matches_resolved_identity(
                    t,
                    table_name,
                    table_name_quoted,
                    schema_name,
                    schema_quoted,
                )
            }))
        });
    }

    /// Rewrites every stored reference to a renamed table that names it
    /// directly rather than wrapping its node.
    ///
    /// Mirrors [`Self::remove_table`]: the same stores carry the same
    /// references, and a rename rewrites where a drop discards. Objects that
    /// wrap the node instead of naming it are rebuilt from the replacement
    /// node, so they are not touched here.
    fn rewrite_table_references(&mut self, renamed: &StoredTable, target: &RenameTarget) {
        for (trigger, ()) in self.triggers_mut() {
            if renamed.named_by(&trigger.table_name) {
                target.rewrite(&mut Arc::make_mut(trigger).table_name);
            }
        }

        for (policy, _) in self.policies_mut() {
            if renamed.named_by(&policy.table_name) {
                target.rewrite(&mut Arc::make_mut(policy).table_name);
            }
        }

        for (grant, ()) in self.table_grants_mut() {
            rewrite_grant_tables(Arc::make_mut(grant), renamed, target);
        }

        for (grant, ()) in self.column_grants_mut() {
            rewrite_grant_tables(Arc::make_mut(grant), renamed, target);
        }
    }

    /// Whether anything outside the table depends on one of its columns.
    ///
    /// The indexes and constraints that live on the table are dropped along
    /// with the column, so only a foreign key from another table, a policy that
    /// reads the column, and a trigger that fires on updates to it call for
    /// `CASCADE`.
    fn column_has_outside_dependents(
        &self,
        table: &StoredTable,
        declaring: &[StoredTable],
        column: &NamedColumn,
    ) -> bool {
        let case = self.identifier_case();
        self.tables().iter().any(|(host, _)| {
            !table.matches(host.as_ref(), case) && refers_to_column(host.as_ref(), table, column)
        }) || self.policies().iter().any(|(policy, _)| {
            table.named_by(&policy.table_name)
                && column.in_expressions(policy.as_ref(), table, declaring)
        }) || self.triggers().iter().any(|(trigger, ())| {
            table.named_by(&trigger.table_name) && trigger_fires_on_column(trigger, column)
        })
    }

    /// Removes what a column carries with it inside its own table: the indexes
    /// that name it and its entry in the permissions granted on it.
    ///
    /// A grant is recorded in both grant stores, so both copies of the
    /// statement have to lose the column, the same way
    /// [`Self::remove_table`] sweeps both.
    fn take_column_dependents(
        &mut self,
        table: &StoredTable,
        declaring: &[StoredTable],
        column: &NamedColumn,
    ) {
        let case = self.identifier_case();
        self.indices_mut().retain(|(index, _)| {
            !(table.matches(TableAttribute::table(index), case)
                && (column.in_expressions(index.attribute(), table, declaring)
                    || column.in_idents(&index.attribute().include)))
        });

        self.table_grants_mut().retain_mut(|(grant, ())| {
            !grant_names_table(grant, table) || drop_grant_column(Arc::make_mut(grant), column)
        });
        self.column_grants_mut().retain_mut(|(grant, ())| {
            !grant_names_table(grant, table) || drop_grant_column(Arc::make_mut(grant), column)
        });
    }

    /// Removes the objects outside the table that depend on one of its columns,
    /// and returns the tables whose node changed so the caller can rebuild
    /// them. Only reached once the statement said `CASCADE`.
    fn take_column_outside_dependents(
        &mut self,
        table: &StoredTable,
        declaring: &[StoredTable],
        column: &NamedColumn,
    ) -> Vec<StoredTable> {
        let case = self.identifier_case();
        self.policies_mut().retain(|(policy, _)| {
            !(table.named_by(&policy.table_name)
                && column.in_expressions(policy.as_ref(), table, declaring))
        });
        self.triggers_mut().retain(|(trigger, ())| {
            !(table.named_by(&trigger.table_name) && trigger_fires_on_column(trigger, column))
        });

        self.tables()
            .iter()
            .map(|(host, _)| host.as_ref())
            .filter(|host| !table.matches(host, case) && refers_to_column(host, table, column))
            .map(StoredTable::of)
            .collect()
    }

    /// Renames a column in the objects that name it from outside the table's
    /// own node: the indexes built by `CREATE INDEX`, the policies that
    /// read it, the triggers that fire on it, and the permissions granted
    /// on it.
    fn rewrite_column_references(
        &mut self,
        table: &StoredTable,
        declaring: &[StoredTable],
        from: &NamedColumn,
        to: &Ident,
    ) {
        let case = self.identifier_case();
        for (index, _) in self.indices_mut() {
            if table.matches(TableAttribute::table(index), case) {
                let index = Arc::make_mut(index).attribute_mut();
                rename_column_in_expressions(index, table, declaring, from, to);
                rename_column_idents(&mut index.include, from, to);
            }
        }

        for (policy, _) in self.policies_mut() {
            if table.named_by(&policy.table_name) {
                rename_column_in_expressions(Arc::make_mut(policy), table, declaring, from, to);
            }
        }

        for (trigger, ()) in self.triggers_mut() {
            if table.named_by(&trigger.table_name) {
                for event in &mut Arc::make_mut(trigger).events {
                    if let TriggerEvent::Update(columns) = event {
                        rename_column_idents(columns, from, to);
                    }
                }
            }
        }

        for (grant, ()) in self.table_grants_mut() {
            if grant_names_table(grant, table) {
                rename_grant_columns(Arc::make_mut(grant), from, to);
            }
        }
        for (grant, ()) in self.column_grants_mut() {
            if grant_names_table(grant, table) {
                rename_grant_columns(Arc::make_mut(grant), from, to);
            }
        }
    }

    /// Detaches every model object derived from the stored node of a table,
    /// leaving the table entry itself in place.
    ///
    /// Derived objects wrap the node they were built from, so a statement that
    /// edits the node has to rebuild them. Columns and constraint-implied
    /// objects follow from the node and are recomputed, while `CREATE INDEX`
    /// indexes do not and are returned with their expressions so the caller can
    /// re-attach them.
    fn take_table_derived_objects(
        &mut self,
        table_name: &str,
        table_name_quoted: bool,
        schema_name: Option<&str>,
        schema_quoted: bool,
    ) -> Vec<(CreateIndex, Expr)> {
        let case = self.identifier_case();
        let belongs_to = |table: &CreateTable| {
            table_matches_resolved_identity(
                table,
                table_name,
                table_name_quoted,
                schema_name,
                schema_quoted,
                case,
            )
        };

        let mut detached_indices = Vec::new();
        self.indices_mut().retain(|(index, metadata)| {
            if belongs_to(TableAttribute::table(index)) {
                detached_indices.push((index.attribute().clone(), metadata.expression().clone()));
                return false;
            }
            true
        });

        self.columns_mut().retain(|(column, _)| !belongs_to(TableAttribute::table(column)));
        self.unique_indices_mut().retain(|(index, _)| !belongs_to(TableAttribute::table(index)));
        self.foreign_keys_mut().retain(|(fk, ())| !belongs_to(TableAttribute::table(fk)));
        self.check_constraints_mut().retain(|(check, _)| !belongs_to(TableAttribute::table(check)));

        detached_indices
    }

    /// Returns whether a modeled blocking dependency names the role.
    fn is_role_referenced(&self, role_ident: &Ident) -> bool {
        let stored_name = stored_role_name(role_ident);
        let grantees_name_role = |grantees: &[Grantee]| {
            grantees.iter().any(|grantee| {
                grantee_role_ident(grantee)
                    .is_some_and(|grantee_ident| idents_match(grantee_ident, role_ident))
            })
        };

        self.table_grants().iter().any(|(grant, ())| grantees_name_role(&grant.grantees))
            || self.column_grants().iter().any(|(grant, ())| grantees_name_role(&grant.grantees))
            || self
                .tables()
                .iter()
                .any(|(_, metadata)| metadata.owner() == Some(stored_name.as_str()))
            || self
                .functions()
                .iter()
                .any(|(_, metadata)| metadata.owner() == Some(stored_name.as_str()))
            || views::view_owner_names(self).iter().any(|owner| owner == stored_name.as_str())
            || self
                .schemas()
                .iter()
                .any(|(schema, ())| schema.authorization() == Some(stored_name.as_str()))
            || self.policies().iter().any(|(policy, _)| {
                policy
                    .to
                    .iter()
                    .flatten()
                    .filter_map(policy_role_ident)
                    .any(|policy_ident| idents_match(policy_ident, role_ident))
            })
    }

    /// Whether any relation belongs to this schema.
    ///
    /// A view counts as much as a table: PostgreSQL refuses `DROP SCHEMA`
    /// without `CASCADE` while a schema holds either.
    fn is_schema_non_empty(&self, schema_name: &str, schema_quoted: bool) -> bool {
        let in_schema = |stored: Option<(&str, bool)>| {
            stored.is_some_and(|(stored_schema, stored_quoted)| {
                identifiers_match(stored_schema, stored_quoted, schema_name, schema_quoted)
            })
        };
        self.tables().iter().any(|(table, _)| {
            in_schema(table.table_schema().map(|schema| (schema, table.table_schema_is_quoted())))
        }) || self.views().iter().any(|(view, _)| {
            in_schema(view.view_schema().map(|schema| (schema, view.view_schema_is_quoted())))
        }) || self.materialized_views().iter().any(|(view, _)| {
            in_schema(view.view_schema().map(|schema| (schema, view.view_schema_is_quoted())))
        })
    }

    fn resolve_schema_ident(&self, ident: &Ident) -> Option<&Schema> {
        resolve_schema_ident_in_iter(
            self.schemas().iter().map(|(schema, ())| schema.as_ref()),
            ident,
        )
    }

    /// Resolves a table the input has created so far through the session's
    /// path, the first schema holding the name under any relation kind ending
    /// the walk, so every statement reaching for a bare name answers alike.
    fn resolve_table_object_name(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&CreateTable>, LookupError> {
        let case = self.identifier_case();
        resolve_table_object_name_on_search_path_in_iter(
            self.tables().iter().map(|(table, _)| table.as_ref()),
            object_name,
            || self.search_path(),
            case,
            |key| self.relation_key_is_held(key, case),
        )
    }

    /// Resolves a plain view the input has created so far, walking the path
    /// the way [`Self::resolve_table_object_name`] does.
    fn resolve_view_object_name(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&View>, LookupError> {
        let (schema_ident, name_ident) = object_name_identifiers(object_name)?;
        let case = self.identifier_case();
        resolve_view_on_search_path_in_iter(
            self.views().iter().map(|(view, _)| view.as_ref()),
            &target_name_of_idents(schema_ident, name_ident),
            || self.search_path(),
            case,
            |key| self.relation_key_is_held(key, case),
        )
    }

    /// Resolves a materialized view the input has created so far, walking the
    /// path the way [`Self::resolve_table_object_name`] does.
    fn resolve_materialized_view_object_name(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&MaterializedView>, LookupError> {
        let (schema_ident, name_ident) = object_name_identifiers(object_name)?;
        let case = self.identifier_case();
        resolve_view_on_search_path_in_iter(
            self.materialized_views().iter().map(|(view, _)| view.as_ref()),
            &target_name_of_idents(schema_ident, name_ident),
            || self.search_path(),
            case,
            |key| self.relation_key_is_held(key, case),
        )
    }

    /// The kind holding the relation name a statement writes and the schema
    /// holding it, a bare name reaching the first schema on the session's
    /// relation path that holds it under any kind, an index included.
    fn relation_reached<'name>(
        &'name self,
        name: &'name ObjectName,
    ) -> Option<(ObjectKind, (&'name str, bool))> {
        let ident = object_name_last_identifier(name)?;
        let holder = |schema: (&'name str, bool)| {
            relation_name_holder(self, ident, Some(schema)).map(|kind| (kind, schema))
        };
        match qualifier_of(name) {
            Qualifier::Named(schema, quoted) => holder((schema, quoted)),
            Qualifier::RunTime => None,
            Qualifier::Absent => relation_search_path(|| self.search_path()).find_map(holder),
        }
    }

    /// Whether any table, view or materialized view is stored under `key`.
    fn relation_key_is_held(&self, key: &RelationKey, case: IdentifierCase) -> bool {
        self.tables().iter().any(|(table, _)| table_matches_key(table.as_ref(), key, case))
            || self.views().iter().any(|(view, _)| view_matches_key(view.as_ref(), key, case))
            || self
                .materialized_views()
                .iter()
                .any(|(view, _)| view_matches_key(view.as_ref(), key, case))
    }

    /// Binds every relation a grant or revoke names to the one it reaches
    /// through the session's path, leaving a name reaching none as written.
    fn bind_access_targets(&self, objects: &mut Option<GrantObjects>) {
        let Some(GrantObjects::Tables(names)) = objects else {
            return;
        };
        for name in names {
            if let Some(bound) = self.bound_relation(name) {
                bind_reference(name, bound.qualifier);
            }
        }
    }

    /// Binds a reference to the table it reaches through the session's path,
    /// leaving one that reaches none as written.
    fn bind_table_reference(&self, reference: &mut ObjectName) {
        if let Ok(Some(table)) = self.resolve_table_object_name(reference) {
            let qualifier = stored_qualifier(&table.name);
            bind_reference(reference, qualifier);
        }
    }

    /// Resolves a reference a stored node carries, which ingestion bound, so
    /// a bare one means the default schema and no path is walked.
    fn resolve_bound_table_name(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&CreateTable>, LookupError> {
        resolve_table_object_name_in_iter(
            self.tables().iter().map(|(table, _)| table.as_ref()),
            object_name,
            self.identifier_case(),
        )
    }

    /// The position of the index a written name reaches through the shared
    /// relation pool, refusing a name another kind holds there.
    fn index_position(&self, name: &ObjectName) -> Result<Option<usize>, crate::errors::Error> {
        let Some((index_name, index_quoted)) = object_name_last_part(name) else {
            return Ok(None);
        };
        let case = self.identifier_case();
        match self.relation_reached(name) {
            None => Ok(None),
            Some((ObjectKind::Index, schema)) => {
                Ok(self.indices().iter().position(|(index, _)| {
                    index.attribute().name.as_ref().and_then(object_name_last_part).is_some_and(
                        |(stored, quoted)| {
                            case.identifiers_match(stored, quoted, index_name, index_quoted)
                        },
                    ) && schema_qualifiers_match(
                        table_schema_qualifier(TableAttribute::table(index.as_ref())),
                        Some(schema),
                        case,
                    )
                }))
            }
            Some((actual_kind, _)) => {
                Err(crate::errors::Error::RelationKindMismatch {
                    object_name: name.to_string(),
                    expected_kind: ObjectKind::Index,
                    actual_kind,
                })
            }
        }
    }

    /// The table, view or materialized view a reference reaches through the
    /// session's path, the first schema holding the name under any kind ending
    /// the walk.
    fn bound_relation(&self, reference: &ObjectName) -> Option<BoundRelation> {
        let (schema_ident, name_ident) = object_name_identifiers(reference).ok()?;
        let target = target_name_of_idents(schema_ident, name_ident);
        let case = self.identifier_case();
        let claimed = |key: &RelationKey| self.relation_key_is_held(key, case);
        let path = || self.search_path();
        if let Some(table) = resolve_target_on_search_path_in_iter(
            self.tables().iter().map(|(table, _)| table.as_ref()),
            &target,
            path,
            case,
            claimed,
        )
        .ok()?
        {
            return Some(BoundRelation {
                qualifier: stored_qualifier(&table.name),
                temporary: table.is_temporary(),
            });
        }
        if let Some(view) = resolve_view_on_search_path_in_iter(
            self.views().iter().map(|(view, _)| view.as_ref()),
            &target,
            path,
            case,
            claimed,
        )
        .ok()?
        {
            return Some(BoundRelation {
                qualifier: stored_view_qualifier(view),
                temporary: view.is_temporary(),
            });
        }
        resolve_view_on_search_path_in_iter(
            self.materialized_views().iter().map(|(view, _)| view.as_ref()),
            &target,
            path,
            case,
            claimed,
        )
        .ok()?
        .map(|view| {
            BoundRelation { qualifier: stored_view_qualifier(view), temporary: view.is_temporary() }
        })
    }
}

/// A type alias for the result of processing check constraints.
type CheckConstraintResult =
    (Vec<Arc<TableAttribute<CreateTable, ColumnDef>>>, Vec<Arc<CreateFunction>>);

/// A type alias for the result of processing unique constraints.
type UniqueConstraintResult = (
    Arc<TableAttribute<CreateTable, UniqueConstraint>>,
    UniqueIndexMetadata<TableAttribute<CreateTable, UniqueConstraint>>,
);

fn object_name_last_identifier(object_name: &ObjectName) -> Option<&Ident> {
    match object_name.0.last() {
        Some(ObjectNamePart::Identifier(ident)) => Some(ident),
        _ => None,
    }
}

/// Returns an error when an object name carries no parts, or a part built
/// while the statement runs.
///
/// The parser never produces an empty `ObjectName`, so a caller reaching that
/// branch built the name by hand. A part some dialects spell as a call,
/// `IDENTIFIER('docs')`, names whatever the call returns when the statement
/// runs, so a schema cannot record the object under any spelling.
fn require_named(
    object_name: &ObjectName,
    kind: crate::errors::ObjectKind,
) -> Result<(), crate::errors::Error> {
    if object_name.0.is_empty() {
        return Err(crate::errors::Error::UnnamedObject { object_kind: kind });
    }
    require_local_object_name(object_name)?;
    Ok(())
}

/// Refuses a creation name this catalog cannot hold, and drops a leading part
/// that names this catalog.
///
/// A server lets a table, a function or a view carry the catalog outermost
/// when it is the one being written to, measured on PostgreSQL 18, and
/// refuses any other catalog as a cross-database reference. It refuses a
/// qualifier outright on an index or a schema name, which stay on
/// [`require_named`].
///
/// # Errors
///
/// Returns [`crate::errors::Error::UnnamedObject`] for a name with no parts,
/// and [`LookupError::InvalidObjectName`] for a part built at run time or a
/// leading part naming another catalog.
fn require_named_in_catalog(
    object_name: &mut ObjectName,
    kind: crate::errors::ObjectKind,
    catalog_name: &str,
) -> Result<(), crate::errors::Error> {
    if object_name.0.is_empty() {
        return Err(crate::errors::Error::UnnamedObject { object_kind: kind });
    }
    require_static_object_name(object_name)?;
    if object_name.0.len() <= 2 {
        return Ok(());
    }

    let names_this_catalog = object_name.0.len() == 3
        && matches!(
            &object_name.0[0],
            ObjectNamePart::Identifier(ident)
                if identifiers_match(
                    ident.value.as_str(),
                    ident.quote_style.is_some(),
                    catalog_name,
                    false,
                )
        );
    if !names_this_catalog {
        return Err(overqualified_object_name(object_name).into());
    }

    object_name.0.remove(0);
    Ok(())
}

/// Refuses every name a grant or a revoke carries that is built while the
/// statement runs, whichever form the statement takes.
///
/// The match is exhaustive on purpose: a form nobody reads today still
/// records a grant, and a grant recorded against a name nothing can read
/// covers nothing and says so nowhere.
///
/// Qualification is left alone here: a grant may name a table this catalog
/// does not hold, and the open world keeps that as an unresolved reference
/// rather than refusing the statement.
fn require_static_access_names(objects: Option<&GrantObjects>) -> Result<(), LookupError> {
    let Some(objects) = objects else {
        return Ok(());
    };
    let names: &[ObjectName] = match objects {
        GrantObjects::AllSequencesInSchema { schemas }
        | GrantObjects::AllTablesInSchema { schemas }
        | GrantObjects::AllViewsInSchema { schemas }
        | GrantObjects::AllMaterializedViewsInSchema { schemas }
        | GrantObjects::AllExternalTablesInSchema { schemas }
        | GrantObjects::AllFunctionsInSchema { schemas }
        | GrantObjects::FutureTablesInSchema { schemas }
        | GrantObjects::FutureViewsInSchema { schemas }
        | GrantObjects::FutureExternalTablesInSchema { schemas }
        | GrantObjects::FutureMaterializedViewsInSchema { schemas }
        | GrantObjects::FutureSequencesInSchema { schemas } => schemas,
        GrantObjects::FutureSchemasInDatabase { databases }
        | GrantObjects::Databases(databases) => databases,
        GrantObjects::Schemas(names)
        | GrantObjects::Sequences(names)
        | GrantObjects::Tables(names)
        | GrantObjects::Views(names)
        | GrantObjects::Warehouses(names)
        | GrantObjects::Integrations(names)
        | GrantObjects::ResourceMonitors(names)
        | GrantObjects::Users(names)
        | GrantObjects::ComputePools(names)
        | GrantObjects::Connections(names)
        | GrantObjects::FailoverGroup(names)
        | GrantObjects::ReplicationGroup(names)
        | GrantObjects::ExternalVolumes(names) => names,
        GrantObjects::Procedure { name, .. } | GrantObjects::Function { name, .. } => {
            return require_static_object_name(name);
        }
    };
    for name in names {
        require_static_object_name(name)?;
    }
    Ok(())
}

fn resolve_schema_ident_in_iter<'a>(
    mut schemas: impl Iterator<Item = &'a Schema>,
    ident: &Ident,
) -> Option<&'a Schema> {
    schemas.find(|schema| {
        identifiers_match(
            schema.name(),
            schema.is_quoted(),
            ident.value.as_str(),
            ident.quote_style.is_some(),
        )
    })
}

fn table_matches_resolved_identity(
    table: &CreateTable,
    table_name: &str,
    table_name_quoted: bool,
    schema_name: Option<&str>,
    schema_quoted: bool,
    case: IdentifierCase,
) -> bool {
    if !case.identifiers_match(
        table.table_name(),
        table.table_name_is_quoted(),
        table_name,
        table_name_quoted,
    ) {
        return false;
    }

    match (table.table_schema(), schema_name) {
        (None, None) => true,
        (Some(table_schema), Some(schema_name)) => {
            case.identifiers_match(
                table_schema,
                table.table_schema_is_quoted(),
                schema_name,
                schema_quoted,
            )
        }
        _ => false,
    }
}

fn object_name_matches_resolved_identity(
    object_name: &ObjectName,
    table_name: &str,
    table_name_quoted: bool,
    schema_name: Option<&str>,
    schema_quoted: bool,
) -> bool {
    let Ok((schema_ident, table_ident)) = object_name_identifiers(object_name) else {
        return false;
    };

    if !identifiers_match(
        table_ident.value.as_str(),
        table_ident.quote_style.is_some(),
        table_name,
        table_name_quoted,
    ) {
        return false;
    }

    match (schema_ident, schema_name) {
        (None, None) => true,
        (Some(schema_ident), Some(schema_name)) => {
            identifiers_match(
                schema_ident.value.as_str(),
                schema_ident.quote_style.is_some(),
                schema_name,
                schema_quoted,
            )
        }
        _ => false,
    }
}

/// The four values every table lookup in this module keys on, owned so that it
/// survives the mutations a rename performs on the stores it walks.
#[derive(Clone, PartialEq, Eq)]
struct StoredTable {
    name: String,
    name_quoted: bool,
    schema: Option<String>,
    schema_quoted: bool,
}

impl StoredTable {
    fn of(table: &CreateTable) -> Self {
        Self {
            name: table.table_name().to_string(),
            name_quoted: table.table_name_is_quoted(),
            schema: table.table_schema().map(str::to_string),
            schema_quoted: table.table_schema_is_quoted(),
        }
    }

    fn matches(&self, table: &CreateTable, case: IdentifierCase) -> bool {
        table_matches_resolved_identity(
            table,
            &self.name,
            self.name_quoted,
            self.schema.as_deref(),
            self.schema_quoted,
            case,
        )
    }

    fn named_by(&self, object_name: &ObjectName) -> bool {
        object_name_matches_resolved_identity(
            object_name,
            &self.name,
            self.name_quoted,
            self.schema.as_deref(),
            self.schema_quoted,
        )
    }

    /// Whether an identifier used to qualify a column reference denotes this
    /// table.
    fn qualifies(&self, ident: &Ident) -> bool {
        identifiers_match(
            &self.name,
            self.name_quoted,
            ident.value.as_str(),
            ident.quote_style.is_some(),
        )
    }
}

/// The name a rename moves a table to, and whether the schema moved with it.
///
/// A rename within a schema leaves every existing spelling of the reference
/// resolving, so only the table identifier is replaced and the caller's
/// qualification survives. A rename across schemas leaves no part of the old
/// spelling resolving, so the reference is replaced whole.
struct RenameTarget {
    name: ObjectName,
    schema_changed: bool,
}

/// The two clauses an `ALTER TABLE` header carries into every operation it
/// lists: whether an absent table is excused, and whether the tables taking
/// their shape from this one are to be left alone.
#[derive(Clone, Copy)]
struct AlterTableScope {
    if_exists: bool,
    only: bool,
}

impl RenameTarget {
    /// `ALTER TABLE ... RENAME TO` spells the new name without a schema and
    /// cannot move a table between schemas, so a bare new name inherits the
    /// qualification the table already carried. `RENAME TABLE a TO b` may
    /// spell both sides, and a qualified new name is taken as written.
    fn new(new_name: ObjectName, current_name: &ObjectName) -> Result<Self, LookupError> {
        let (new_schema, new_table) = object_name_identifiers(&new_name)?;

        let Some(new_schema) = new_schema else {
            let new_table = ObjectNamePart::Identifier(new_table.clone());
            let mut parts = current_name.0.clone();
            match parts.last_mut() {
                Some(last) => *last = new_table,
                None => parts.push(new_table),
            }
            return Ok(Self { name: ObjectName(parts), schema_changed: false });
        };

        let (current_schema, _) = object_name_identifiers(current_name)?;
        let schema_changed = !current_schema.is_some_and(|current_schema| {
            identifiers_match(
                current_schema.value.as_str(),
                current_schema.quote_style.is_some(),
                new_schema.value.as_str(),
                new_schema.quote_style.is_some(),
            )
        });

        Ok(Self { name: new_name, schema_changed })
    }

    fn rewrite(&self, reference: &mut ObjectName) {
        if self.schema_changed {
            *reference = self.name.clone();
            return;
        }
        if let Some(new_table) = self.name.0.last()
            && let Some(last) = reference.0.last_mut()
        {
            *last = new_table.clone();
        }
    }
}

/// Rewrites every foreign key in `node` that targets `renamed`.
///
/// A foreign key reaches a table either through the constraint list or inline
/// on a column, and the node is the source of truth for both, so both
/// spellings are rewritten here rather than in the objects derived from them.
fn rewrite_foreign_key_targets(
    node: &mut CreateTable,
    renamed: &StoredTable,
    target: &RenameTarget,
) -> bool {
    let mut rewritten = false;

    for constraint in &mut node.constraints {
        if let TableConstraint::ForeignKey(foreign_key) = constraint
            && renamed.named_by(&foreign_key.foreign_table)
        {
            target.rewrite(&mut foreign_key.foreign_table);
            rewritten = true;
        }
    }

    for column in &mut node.columns {
        for option in &mut column.options {
            if let ColumnOption::ForeignKey(foreign_key) = &mut option.option
                && renamed.named_by(&foreign_key.foreign_table)
            {
                target.rewrite(&mut foreign_key.foreign_table);
                rewritten = true;
            }
        }
    }

    rewritten
}

/// Rewrites the table names a grant lists when they name the renamed table.
fn rewrite_grant_tables(grant: &mut Grant, renamed: &StoredTable, target: &RenameTarget) {
    if let Some(GrantObjects::Tables(tables)) = &mut grant.objects {
        for table in tables {
            if renamed.named_by(table) {
                target.rewrite(table);
            }
        }
    }
}

/// Whether a grant lists a table among its targets.
fn grant_names_table(grant: &Grant, table: &StoredTable) -> bool {
    matches!(&grant.objects, Some(GrantObjects::Tables(tables))
        if tables.iter().any(|named| table.named_by(named)))
}

/// Restates a column's type and options, which is what MySQL's `CHANGE COLUMN`
/// and `MODIFY COLUMN` do: the clause carries the whole declaration, so what it
/// leaves out is dropped rather than kept.
fn redeclare_column(declared: &mut ColumnDef, data_type: DataType, options: Vec<ColumnOption>) {
    declared.data_type = data_type;
    declared.options =
        options.into_iter().map(|option| ColumnOptionDef { name: None, option }).collect();
}

/// Whether any foreign key on `node` targets `renamed`.
fn table_references(node: &CreateTable, renamed: &StoredTable) -> bool {
    let in_constraints = node.constraints.iter().any(|constraint| {
        matches!(constraint, TableConstraint::ForeignKey(foreign_key)
            if renamed.named_by(&foreign_key.foreign_table))
    });

    in_constraints
        || node.columns.iter().any(|column| {
            column.options.iter().any(|option| {
                matches!(&option.option, ColumnOption::ForeignKey(foreign_key)
                    if renamed.named_by(&foreign_key.foreign_table))
            })
        })
}

/// Every foreign key a table declares, in either spelling.
fn foreign_keys_of(node: &CreateTable) -> impl Iterator<Item = &ForeignKeyConstraint> {
    node.constraints
        .iter()
        .filter_map(|constraint| {
            match constraint {
                TableConstraint::ForeignKey(foreign_key) => Some(foreign_key),
                _ => None,
            }
        })
        .chain(node.columns.iter().flat_map(|column| {
            column.options.iter().filter_map(|option| {
                match &option.option {
                    ColumnOption::ForeignKey(foreign_key) => Some(foreign_key),
                    _ => None,
                }
            })
        }))
}

/// A column identifier as a statement spells it, for matching against the
/// identifiers and expressions the model stores.
struct NamedColumn {
    name: String,
    quoted: bool,
    /// Comparison the parse states, which every engine answers its own way for
    /// a column: only PostgreSQL keeps a quoted spelling apart.
    case: IdentifierCase,
}

impl NamedColumn {
    fn of(ident: &Ident, case: IdentifierCase) -> Self {
        Self { name: ident.value.clone(), quoted: ident.quote_style.is_some(), case }
    }

    fn matches(&self, ident: &Ident) -> bool {
        self.case.identifiers_match(
            &self.name,
            self.quoted,
            ident.value.as_str(),
            ident.quote_style.is_some(),
        )
    }

    fn declared_by(&self, node: &CreateTable) -> bool {
        node.columns.iter().any(|column| self.matches(&column.name))
    }

    fn in_idents(&self, idents: &[Ident]) -> bool {
        idents.iter().any(|ident| self.matches(ident))
    }

    /// Whether any expression the node carries names this column of `table`.
    fn in_expressions<N: Visit>(
        &self,
        node: &N,
        table: &StoredTable,
        declaring: &[StoredTable],
    ) -> bool {
        let mut mentioned = ColumnMentioned { scope: ColumnScope::new(self, table, declaring) };
        node.visit(&mut mentioned).is_break()
    }
}

/// Decides, while walking a condition, whether a column mention belongs to the
/// table being altered.
///
/// A mention carrying a table prefix belongs to that table. A mention without
/// one belongs to a nested query when that query reads a table declaring the
/// same name, and otherwise can only be the altered table's.
struct ColumnScope<'a> {
    column: &'a NamedColumn,
    table: &'a StoredTable,
    /// The other tables of the input that declare a column of this name.
    declaring: &'a [StoredTable],
    /// One entry per nested query currently open, set when that query reads a
    /// table declaring the name.
    nested: Vec<bool>,
}

impl<'a> ColumnScope<'a> {
    fn new(column: &'a NamedColumn, table: &'a StoredTable, declaring: &'a [StoredTable]) -> Self {
        Self { column, table, declaring, nested: Vec::new() }
    }

    fn enter(&mut self, query: &Query) {
        let reads_declaring = !self.declaring.is_empty()
            && visit_relations(query, |relation| {
                if self.declaring.iter().any(|declared| declared.named_by(relation)) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_break();
        self.nested.push(reads_declaring);
    }

    fn leave(&mut self) {
        self.nested.pop();
    }

    /// Whether the expression names the altered table's column.
    fn names(&self, expr: &Expr) -> bool {
        match expr {
            // A bare mention belongs to an enclosing nested query when one of
            // them reads a table declaring the name.
            Expr::Identifier(ident) => {
                self.column.matches(ident) && !self.nested.iter().any(|reads| *reads)
            }
            // A prefix that is an alias rather than the table name denotes
            // nothing here, which errs towards leaving a mention alone.
            Expr::CompoundIdentifier(parts) => {
                match parts.as_slice() {
                    [.., qualifier, column] => {
                        self.column.matches(column) && self.table.qualifies(qualifier)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
}

/// Breaks on the first expression naming the column.
struct ColumnMentioned<'a> {
    scope: ColumnScope<'a>,
}

impl Visitor for ColumnMentioned<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        self.scope.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<()> {
        self.scope.leave();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if self.scope.names(expr) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }
}

/// Rewrites every expression naming the column.
struct ColumnRenamer<'a> {
    scope: ColumnScope<'a>,
    to: &'a Ident,
}

impl VisitorMut for ColumnRenamer<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        self.scope.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<()> {
        self.scope.leave();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        if self.scope.names(expr) {
            match expr {
                Expr::Identifier(ident) => *ident = self.to.clone(),
                Expr::CompoundIdentifier(parts) => {
                    if let Some(last) = parts.last_mut() {
                        *last = self.to.clone();
                    }
                }
                _ => {}
            }
        }
        ControlFlow::Continue(())
    }
}

/// Renames every mention of a column in the expressions a node carries.
///
/// The identifier lists a node also carries, a foreign key's column list among
/// them, are not expressions and are rewritten by the caller.
fn rename_column_in_expressions<N: VisitMut>(
    node: &mut N,
    table: &StoredTable,
    declaring: &[StoredTable],
    from: &NamedColumn,
    to: &Ident,
) {
    let mut renamer = ColumnRenamer { scope: ColumnScope::new(from, table, declaring), to };
    let _: ControlFlow<()> = node.visit(&mut renamer);
}

/// The tables other than `altered` that declare a column of this name.
///
/// A bare mention inside a nested query reading one of them belongs to it
/// rather than to the altered table.
fn tables_declaring_column(
    builder: &ParserDBBuilder,
    altered: &StoredTable,
    column: &NamedColumn,
) -> Vec<StoredTable> {
    let case = builder.identifier_case();
    builder
        .tables()
        .iter()
        .map(|(table, _)| table.as_ref())
        .filter(|table| !altered.matches(table, case) && column.declared_by(table))
        .map(StoredTable::of)
        .collect()
}

fn rename_column_idents(idents: &mut [Ident], from: &NamedColumn, to: &Ident) {
    for ident in idents {
        if from.matches(ident) {
            *ident = to.clone();
        }
    }
}

/// Renames a column everywhere the table's own node names it.
///
/// A foreign key's local column list names this table's columns and always
/// follows. Its referred column list names the target table's columns, so it
/// follows only when the target is this same table.
fn rename_column_in_node(
    node: &mut CreateTable,
    table: &StoredTable,
    declaring: &[StoredTable],
    from: &NamedColumn,
    to: &Ident,
) {
    for column in &mut node.columns {
        if from.matches(&column.name) {
            column.name = to.clone();
        }
    }

    rename_column_in_expressions(node, table, declaring, from, to);

    for foreign_key in foreign_keys_of_mut(node) {
        rename_column_idents(&mut foreign_key.columns, from, to);
        if table.named_by(&foreign_key.foreign_table) {
            rename_column_idents(&mut foreign_key.referred_columns, from, to);
        }
    }

    // An exclusion constraint's `INCLUDE` list is plain identifiers rather than
    // expressions, so the walker above does not reach it.
    for constraint in &mut node.constraints {
        if let TableConstraint::Exclude(exclude) = constraint {
            rename_column_idents(&mut exclude.include, from, to);
        }
    }
}

/// Renames the columns a node's foreign keys name in `target`.
fn rename_referred_columns(
    node: &mut CreateTable,
    target: &StoredTable,
    from: &NamedColumn,
    to: &Ident,
) {
    for foreign_key in foreign_keys_of_mut(node) {
        if target.named_by(&foreign_key.foreign_table) {
            rename_column_idents(&mut foreign_key.referred_columns, from, to);
        }
    }
}

/// Every foreign key a table declares, in either spelling, for mutation.
fn foreign_keys_of_mut(node: &mut CreateTable) -> impl Iterator<Item = &mut ForeignKeyConstraint> {
    node.constraints
        .iter_mut()
        .filter_map(|constraint| {
            match constraint {
                TableConstraint::ForeignKey(foreign_key) => Some(foreign_key),
                _ => None,
            }
        })
        .chain(node.columns.iter_mut().flat_map(|column| {
            column.options.iter_mut().filter_map(|option| {
                match &mut option.option {
                    ColumnOption::ForeignKey(foreign_key) => Some(foreign_key),
                    _ => None,
                }
            })
        }))
}

/// Whether a constraint involves a column of the table it is attached to.
fn constraint_involves_column(
    constraint: &TableConstraint,
    table: &StoredTable,
    declaring: &[StoredTable],
    column: &NamedColumn,
) -> bool {
    match constraint {
        TableConstraint::ForeignKey(foreign_key) => {
            column.in_idents(&foreign_key.columns)
                || (table.named_by(&foreign_key.foreign_table)
                    && column.in_idents(&foreign_key.referred_columns))
        }
        // The `INCLUDE` list is plain identifiers, which the expression walk
        // does not reach.
        TableConstraint::Exclude(exclude) => {
            column.in_idents(&exclude.include) || column.in_expressions(exclude, table, declaring)
        }
        other => column.in_expressions(other, table, declaring),
    }
}

/// Removes a column and everything the table's own node hangs off it.
///
/// The real database drops the indexes and table constraints that involve the
/// column along with it, so a constraint naming it goes even when it names
/// other columns too. A constraint reaches a table inline on a sibling column
/// as well as through the constraint list, and the inline spelling is not
/// validated when the node is rebuilt, so it has to be swept here too.
fn drop_column_from_node(
    node: &mut CreateTable,
    table: &StoredTable,
    declaring: &[StoredTable],
    column: &NamedColumn,
) {
    node.columns.retain(|declared| !column.matches(&declared.name));
    node.constraints
        .retain(|constraint| !constraint_involves_column(constraint, table, declaring, column));

    for declared in &mut node.columns {
        declared
            .options
            .retain(|option| !option_involves_column(&option.option, table, declaring, column));
    }
}

/// Whether a column option involves another column of the same table.
fn option_involves_column(
    option: &ColumnOption,
    table: &StoredTable,
    declaring: &[StoredTable],
    column: &NamedColumn,
) -> bool {
    match option {
        ColumnOption::ForeignKey(foreign_key) => {
            column.in_idents(&foreign_key.columns)
                || (table.named_by(&foreign_key.foreign_table)
                    && column.in_idents(&foreign_key.referred_columns))
        }
        other => column.in_expressions(other, table, declaring),
    }
}

/// Removes the foreign keys a node declares against a column of `target`.
fn drop_foreign_keys_to_column(node: &mut CreateTable, target: &StoredTable, column: &NamedColumn) {
    let names = |foreign_key: &ForeignKeyConstraint| {
        target.named_by(&foreign_key.foreign_table)
            && column.in_idents(&foreign_key.referred_columns)
    };

    node.constraints.retain(|constraint| {
        !matches!(constraint, TableConstraint::ForeignKey(foreign_key) if names(foreign_key))
    });
    for declared in &mut node.columns {
        declared.options.retain(|option| {
            !matches!(&option.option, ColumnOption::ForeignKey(foreign_key) if names(foreign_key))
        });
    }
}

/// Whether a foreign key on `node` names `column` of the table it targets.
fn refers_to_column(node: &CreateTable, target: &StoredTable, column: &NamedColumn) -> bool {
    foreign_keys_of(node).any(|foreign_key| {
        target.named_by(&foreign_key.foreign_table)
            && column.in_idents(&foreign_key.referred_columns)
    })
}

/// Whether a trigger fires on updates to a named column.
fn trigger_fires_on_column(trigger: &CreateTrigger, column: &NamedColumn) -> bool {
    trigger
        .events
        .iter()
        .any(|event| matches!(event, TriggerEvent::Update(columns) if column.in_idents(columns)))
}

/// Renames the columns a grant names, and reports whether it still names any.
fn rename_grant_columns(grant: &mut Grant, from: &NamedColumn, to: &Ident) {
    if let Privileges::Actions(actions) = &mut grant.privileges {
        for action in actions {
            if let Some(columns) = action_columns_mut(action) {
                rename_column_idents(columns, from, to);
            }
        }
    }
}

/// Drops a column from every list a grant names, and reports whether the grant
/// still has anything to say.
///
/// Every action is stripped before the answer is decided, because a grant may
/// name columns under more than one action. An action left naming no column has
/// nothing to grant, so it goes, and a grant left with no action goes with it.
fn drop_grant_column(grant: &mut Grant, column: &NamedColumn) -> bool {
    let Privileges::Actions(actions) = &mut grant.privileges else {
        // A grant of every privilege names no column of its own.
        return true;
    };

    if !actions.iter().any(|action| action_columns(action).is_some()) {
        return true;
    }

    for action in actions.iter_mut() {
        retain_action_columns(action, |named| !column.matches(named));
    }
    actions.retain(|action| action_columns(action).is_none_or(|columns| !columns.is_empty()));

    !actions.is_empty()
}

/// The columns an action names, when it is one of the column-scoped actions.
fn action_columns(action: &Action) -> Option<&[Ident]> {
    match action {
        Action::Select { columns }
        | Action::Insert { columns }
        | Action::Update { columns }
        | Action::References { columns } => columns.as_deref(),
        _ => None,
    }
}

fn action_columns_mut(action: &mut Action) -> Option<&mut [Ident]> {
    match action {
        Action::Select { columns }
        | Action::Insert { columns }
        | Action::Update { columns }
        | Action::References { columns } => columns.as_deref_mut(),
        _ => None,
    }
}

/// Keeps only the columns an action names that `keep` answers for.
fn retain_action_columns(action: &mut Action, keep: impl Fn(&Ident) -> bool) {
    let (Action::Select { columns }
    | Action::Insert { columns }
    | Action::Update { columns }
    | Action::References { columns }) = action
    else {
        return;
    };
    if let Some(columns) = columns {
        columns.retain(keep);
    }
}

/// Returns whether two identifiers name the same object under PostgreSQL
/// folding: an unquoted one is case-insensitive, a quoted one exact.
fn case_idents_match(left: &Ident, right: &Ident, case: IdentifierCase) -> bool {
    case.identifiers_match(
        left.value.as_str(),
        left.quote_style.is_some(),
        right.value.as_str(),
        right.quote_style.is_some(),
    )
}

fn idents_match(left: &Ident, right: &Ident) -> bool {
    identifiers_match(
        left.value.as_str(),
        left.quote_style.is_some(),
        right.value.as_str(),
        right.quote_style.is_some(),
    )
}

/// Returns whether two object names carry the same final identifier under
/// PostgreSQL folding.
///
/// A qualifier is left out of the comparison: what scopes the name is decided
/// by the caller, since a trigger is scoped by its table and an index by the
/// schema of the table it is on rather than by anything the name spells.
fn object_names_match(left: &ObjectName, right: &ObjectName, case: IdentifierCase) -> bool {
    match (object_name_last_part(left), object_name_last_part(right)) {
        (Some((left, left_quoted)), Some((right, right_quoted))) => {
            case.identifiers_match(left, left_quoted, right, right_quoted)
        }
        _ => false,
    }
}

fn function_configuration_names_match(left: &ObjectName, right: &ObjectName) -> bool {
    left.0.len() == right.0.len()
        && left.0.iter().zip(&right.0).all(|(left, right)| {
            match (left.as_ident(), right.as_ident()) {
                (Some(left), Some(right)) => idents_match(left, right),
                // A part built at run time names no parameter until it runs,
                // so it is the same parameter as nothing, another such part
                // included.
                _ => false,
            }
        })
}

/// Returns the argument types that make up a function's identity.
///
/// An `OUT` parameter describes the result rather than the call, so PostgreSQL
/// leaves it out and reads `f(int)` and `f(int, OUT o int)` as one function.
/// Aliases fold, so `integer` and `int4` are one type while `varchar` and
/// `text` are two.
fn function_argument_types(args: Option<&[OperateFunctionArg]>) -> Vec<Cow<'_, str>> {
    args.unwrap_or_default()
        .iter()
        .filter(|argument| argument.mode != Some(ArgMode::Out))
        .map(|argument| normalize_postgres_type_cow(normalize_sqlparser_type(&argument.data_type)))
        .collect()
}

/// Returns whether two functions are the same function to PostgreSQL, which
/// identifies one by its schema, its name and the types it takes, and not by
/// what it returns.
fn function_signatures_match(
    left: &ObjectName,
    left_args: Option<&[OperateFunctionArg]>,
    right: &ObjectName,
    right_args: Option<&[OperateFunctionArg]>,
    case: IdentifierCase,
) -> bool {
    if !object_names_match(left, right, case)
        || matches!(qualifier_of(left), Qualifier::RunTime)
        || matches!(qualifier_of(right), Qualifier::RunTime)
        || !schema_qualifiers_match(qualifier_of(left).named(), qualifier_of(right).named(), case)
    {
        return false;
    }
    let left_types = function_argument_types(left_args);
    let right_types = function_argument_types(right_args);
    left_types.len() == right_types.len()
        && left_types.iter().zip(&right_types).all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn role_matches_lookup_ident(role: &CreateRole, lookup_ident: &Ident) -> bool {
    role.names.iter().any(|role_name| {
        object_name_last_identifier(role_name)
            .is_some_and(|role_ident| idents_match(role_ident, lookup_ident))
    })
}

fn stored_role_name(role_ident: &Ident) -> String {
    normalize_identifier(&role_ident.value, role_ident.quote_style.is_some()).into_owned()
}

/// Returns the identifier a grantee resolves a role by, or `None` when the
/// grantee names no role of its own: the `PUBLIC` pseudo-role, however
/// spelled, or a grantee whose name is not a plain identifier.
fn grantee_role_ident(grantee: &Grantee) -> Option<&Ident> {
    if grantee.grantee_type == GranteesType::Public {
        return None;
    }

    let Some(GranteeName::ObjectName(grantee_name)) = &grantee.name else {
        return None;
    };
    let grantee_ident = object_name_last_identifier(grantee_name)?;

    if is_public_pseudo_role(grantee_ident.value.as_str(), grantee_ident.quote_style.is_some())
        || session_principal(grantee_ident.value.as_str(), grantee_ident.quote_style.is_some())
            .is_some()
    {
        return None;
    }

    Some(grantee_ident)
}

/// Returns the role a policy target names, or `None` when it names no role of
/// its own.
///
/// The grammar spells the pseudo-roles as keywords, and an unquoted `PUBLIC`
/// means every role rather than one called `public`, so neither ever demanded a
/// `CREATE ROLE`. This mirrors [`grantee_role_ident`].
fn policy_role_ident(owner: &Owner) -> Option<&Ident> {
    let Owner::Ident(role_ident) = owner else {
        return None;
    };

    if is_public_pseudo_role(role_ident.value.as_str(), role_ident.quote_style.is_some()) {
        return None;
    }

    Some(role_ident)
}

/// Enforces [`AccessResolution::ClosedWorld`] on the roles one policy
/// statement names: every one of them is a role the input has created up to
/// this statement.
///
/// A policy follows the grant setting rather than the stricter rule for tables,
/// because a schema dump omits role creation for a policy exactly as it does
/// for a grant.
fn validate_policy_roles(
    builder: &ParserDBBuilder,
    policy_name: &str,
    owners: &[Owner],
) -> Result<(), crate::errors::Error> {
    for owner in owners {
        let Some(role_ident) = policy_role_ident(owner) else {
            continue;
        };

        let role_exists =
            builder.roles().iter().any(|(role, ())| role_matches_lookup_ident(role, role_ident));
        if !role_exists {
            return Err(crate::errors::Error::RoleNotFoundForPolicy {
                role_name: role_ident.value.clone(),
                policy_name: policy_name.to_string(),
            });
        }
    }

    Ok(())
}

/// Enforces [`AccessResolution::ClosedWorld`] on a role named as an owner.
///
/// The database refuses `ALTER TABLE ... OWNER TO`, `ALTER SCHEMA ... OWNER TO`
/// and `CREATE SCHEMA ... AUTHORIZATION` when the role is absent, so the
/// default refuses them too. The setting excuses them for the same reason it
/// excuses a grantee: a dump emits `ALTER SCHEMA app OWNER TO appowner` while
/// creating no role at all.
fn validate_owner_role_ident(
    builder: &ParserDBBuilder,
    role_ident: &Ident,
    object_name: &str,
) -> Result<(), crate::errors::Error> {
    if builder.roles().iter().any(|(role, ())| role_matches_lookup_ident(role, role_ident)) {
        return Ok(());
    }

    Err(crate::errors::Error::RoleNotFoundForOwner {
        role_name: role_ident.value.clone(),
        object_name: object_name.to_string(),
    })
}

/// Enforces the same rule on an `OWNER TO` clause, which may name one of the
/// keyword pseudo-roles instead of a role, and those name no role to find.
fn validate_owner_role(
    builder: &ParserDBBuilder,
    owner: &Owner,
    object_name: &str,
) -> Result<(), crate::errors::Error> {
    let Owner::Ident(role_ident) = owner else {
        return Ok(());
    };
    validate_owner_role_ident(builder, role_ident, object_name)
}

/// Returns the column names of an index column list, or `None` when any entry
/// is an expression rather than a plain column.
///
/// An expression index cannot back a foreign key, so a key it would describe is
/// simply not a candidate.
fn plain_key_columns(columns: &[IndexColumn]) -> Option<Vec<&Ident>> {
    columns
        .iter()
        .map(|column| {
            match &column.column.expr {
                Expr::Identifier(ident) => Some(ident),
                _ => None,
            }
        })
        .collect()
}

/// Returns whether two column lists name the same set of columns.
fn key_columns_match(left: &[&Ident], right: &[&Ident]) -> bool {
    left.len() == right.len()
        && left.iter().all(|wanted| right.iter().any(|have| idents_match(wanted, have)))
}

/// Collects every set of columns on `table` that a foreign key may point at:
/// its primary key and each of its unique constraints, in both the inline and
/// the table-constraint spelling.
///
/// A `CREATE UNIQUE INDEX` counts too, and lives in the builder rather than on
/// the node, so it is gathered separately by the caller.
fn declared_candidate_keys(table: &CreateTable) -> Vec<Vec<&Ident>> {
    let mut keys: Vec<Vec<&Ident>> = Vec::new();

    for column in &table.columns {
        if column.options.iter().any(|option| {
            matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
        }) {
            keys.push(vec![&column.name]);
        }
    }

    for constraint in &table.constraints {
        let columns = match constraint {
            TableConstraint::PrimaryKey(pk) => &pk.columns,
            TableConstraint::Unique(unique) => &unique.columns,
            _ => continue,
        };
        if let Some(key) = plain_key_columns(columns) {
            keys.push(key);
        }
    }

    keys
}

/// Returns whether the primary key of `table` exists, which is what a foreign
/// key naming no columns points at.
fn has_primary_key(table: &CreateTable) -> bool {
    table.columns.iter().any(|column| {
        column.options.iter().any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
    }) || table.constraints.iter().any(|c| matches!(c, TableConstraint::PrimaryKey(_)))
}

/// Reads one entry of a `SET search_path` value list as a schema name and its
/// quoting.
///
/// PostgreSQL accepts both a bare identifier and a single-quoted string, and
/// neither is a quoted identifier in the case-sensitivity sense. Anything else,
/// such as an expression, names no schema and is skipped.
fn search_path_entry(value: &Expr) -> Option<(String, bool)> {
    match value {
        Expr::Identifier(ident) => Some((ident.value.clone(), ident.quote_style.is_some())),
        Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(name), .. }) => {
            Some((name.clone(), false))
        }
        _ => None,
    }
}

/// Returns whether two table references name the same table.
///
/// A policy name and a trigger name are unique per table rather than per
/// database, so the statements that create, alter or drop one are resolved by
/// both. Two statements need not spell the table the same way, and an
/// unqualified name means schema `public`, which is the allowance
/// [`ParserDB::resolve_table_object_name_on_search_path`] already makes when
/// resolving such a target.
fn target_tables_match(left: &ObjectName, right: &ObjectName, case: IdentifierCase) -> bool {
    let (Ok((left_schema, left_table)), Ok((right_schema, right_table))) =
        (object_name_identifiers(left), object_name_identifiers(right))
    else {
        return false;
    };

    if !case_idents_match(left_table, right_table, case) {
        return false;
    }

    match (left_schema, right_schema) {
        (None, None) => true,
        (Some(schema), None) | (None, Some(schema)) => {
            identifiers_match(schema.value.as_str(), schema.quote_style.is_some(), "public", false)
        }
        (Some(left), Some(right)) => idents_match(left, right),
    }
}

/// Returns the schema the input creates by this name, if it creates one.
fn declared_schema<'builder>(
    builder: &'builder ParserDBBuilder,
    name: &str,
    quoted: bool,
) -> Option<&'builder Schema> {
    builder
        .schemas()
        .iter()
        .map(|(schema, ())| schema.as_ref())
        .find(|schema| identifiers_match(schema.name(), schema.is_quoted(), name, quoted))
}

/// Returns whether a schema by this name is one a relation may be created in.
///
/// The default schema is exempt from being declared, since no dump emits a
/// statement creating it, which is the same allowance
/// [`ParserDB::resolve_table_object_name_on_search_path`] makes when resolving
/// a name against it. So is the session's temporary schema, which always
/// exists and which no statement can create.
fn schema_is_declared(builder: &ParserDBBuilder, name: &str, quoted: bool) -> bool {
    identifiers_match(name, quoted, "public", false)
        || is_temporary_schema(name, quoted)
        || declared_schema(builder, name, quoted).is_some()
}

fn collation_schema_is_declared(builder: &ParserDBBuilder, name: &str, quoted: bool) -> bool {
    identifiers_match(name, quoted, "pg_catalog", false)
        || schema_is_declared(builder, name, quoted)
}

fn validate_created_collation_schema(
    builder: &ParserDBBuilder,
    name: &ObjectName,
) -> Result<(), crate::errors::Error> {
    let Some((schema_name, schema_quoted)) = qualifier_of(name).named() else {
        return Ok(());
    };
    if collation_schema_is_declared(builder, schema_name, schema_quoted) {
        return Ok(());
    }
    Err(crate::errors::Error::SchemaNotFoundForCollation {
        schema_name: schema_name.to_string(),
        collation_name: name.to_string(),
    })
}

/// Checks that a relation qualified with a schema names one the input creates.
fn validate_relation_schema(
    builder: &ParserDBBuilder,
    schema: Option<(&str, bool)>,
    object_kind: crate::errors::ObjectKind,
    relation_name: &str,
) -> Result<(), crate::errors::Error> {
    let Some((schema_name, schema_quoted)) = schema else {
        return Ok(());
    };

    if schema_is_declared(builder, schema_name, schema_quoted) {
        return Ok(());
    }

    Err(crate::errors::Error::SchemaNotFoundForRelation {
        schema_name: schema_name.to_string(),
        object_kind,
        relation_name: relation_name.to_string(),
    })
}

/// Checks that a table qualified with a schema names one the input creates.
fn validate_table_schema(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
) -> Result<(), crate::errors::Error> {
    validate_relation_schema(
        builder,
        create_table.table_schema().map(|schema| (schema, create_table.table_schema_is_quoted())),
        crate::errors::ObjectKind::Table,
        create_table.table_name(),
    )
}

/// Refuses a `NO INHERIT` check written on a partitioned table.
///
/// PostgreSQL enforces every constraint of a partitioned table on its
/// partitions, so the one spelling that would keep a check from them is
/// refused, written as a table constraint or on a column alike.
fn refuse_no_inherit_check_on_partitioned(
    create_table: &CreateTable,
) -> Result<(), crate::errors::Error> {
    if create_table.partition_by.is_none() {
        return Ok(());
    }
    let on_table = create_table
        .constraints
        .iter()
        .any(|constraint| matches!(constraint, TableConstraint::Check(check) if check.no_inherit));
    let on_column = create_table
        .columns
        .iter()
        .flat_map(|column| &column.options)
        .any(|option| matches!(&option.option, ColumnOption::Check(check) if check.no_inherit));
    if on_table || on_column {
        return Err(crate::errors::Error::NoInheritCheckOnPartitionedTable {
            table_name: create_table.table_name().to_string(),
        });
    }
    Ok(())
}

/// Qualifies the name of a relation being created with the schema it lands in,
/// answering whether that is the temporary schema.
fn place_relation(
    builder: &ParserDBBuilder,
    name: &mut ObjectName,
    temporary: bool,
    object_kind: ObjectKind,
) -> Result<bool, crate::errors::Error> {
    // A hand-assembled node may carry no name part, which names nothing to
    // place.
    let Some((relation_name, _)) = object_name_last_part(name) else {
        return Ok(temporary);
    };
    match qualifier_of(name) {
        Qualifier::Named(schema, quoted) if is_temporary_schema(schema, quoted) => {}
        Qualifier::Named(schema, quoted) => {
            validate_relation_schema(builder, Some((schema, quoted)), object_kind, relation_name)?;
            if temporary {
                return Err(crate::errors::Error::TemporaryRelationInPermanentSchema {
                    object_kind,
                    relation_name: relation_name.to_string(),
                    schema_name: schema.to_string(),
                });
            }
            return Ok(false);
        }
        Qualifier::RunTime => return Ok(temporary),
        Qualifier::Absent if temporary => {}
        Qualifier::Absent => {
            let qualifier = search_path_qualifier(builder, object_kind, relation_name)?;
            let Some(qualifier) = qualifier else {
                return Ok(false);
            };
            if !is_temporary_schema(&qualifier.value, qualifier.quote_style.is_some()) {
                bind_reference(name, Some(qualifier));
                return Ok(false);
            }
        }
    }
    // One spelling, so every comparison rule reaches it.
    bind_reference(name, Some(Ident::new(TEMPORARY_SCHEMA)));
    Ok(true)
}

/// Replaces a reference's qualifier, keeping the identifier it names as
/// written.
fn bind_reference(reference: &mut ObjectName, qualifier: Option<Ident>) {
    let Some(name) = reference.0.pop() else {
        return;
    };
    reference.0.clear();
    reference.0.extend(qualifier.map(ObjectNamePart::Identifier));
    reference.0.push(name);
}

/// The qualifier a relation name is stored under, if any.
fn stored_qualifier(name: &ObjectName) -> Option<Ident> {
    match name.0.as_slice() {
        [.., ObjectNamePart::Identifier(schema), _] => Some(schema.clone()),
        _ => None,
    }
}

/// The qualifier a view is stored under, if any.
fn stored_view_qualifier<V: ViewLike>(view: &V) -> Option<Ident> {
    view.view_schema().map(|schema| {
        if view.view_schema_is_quoted() {
            Ident::with_quote('"', schema)
        } else {
            Ident::new(schema)
        }
    })
}

/// The relation a reference reached when the statement holding it ran.
struct BoundRelation {
    /// Qualifier that relation is stored under.
    qualifier: Option<Ident>,
    /// Whether it lives in the session's temporary schema.
    temporary: bool,
}

/// Binds every foreign key target in `node` to the table it reaches now, the
/// node itself included, leaving one reaching nothing for the key check to
/// refuse.
fn bind_foreign_key_targets(builder: &ParserDBBuilder, node: &mut CreateTable) {
    let case = builder.identifier_case();
    let qualifiers: Vec<Option<Option<Ident>>> = foreign_keys_of(node)
        .map(|foreign_key| {
            resolve_table_object_name_on_search_path_in_iter(
                builder
                    .tables()
                    .iter()
                    .map(|(table, _)| table.as_ref())
                    .chain(core::iter::once(&*node)),
                &foreign_key.foreign_table,
                || builder.search_path(),
                case,
                |key| builder.relation_key_is_held(key, case),
            )
            .ok()
            .flatten()
            .map(|table| stored_qualifier(&table.name))
        })
        .collect();
    for (foreign_key, qualifier) in foreign_keys_of_mut(node).zip(qualifiers) {
        if let Some(qualifier) = qualifier {
            bind_reference(&mut foreign_key.foreign_table, qualifier);
        }
    }
}

/// The schema qualifier the search path selects for a relation name written
/// without one, or [`None`] when the path selects the default schema, which
/// this model leaves unwritten.
///
/// PostgreSQL creates in the first schema on the path that exists, so the walk
/// passes an entry naming nothing and takes the next. An entry spelled empty
/// names no schema, so it is passed over like one naming a schema the input
/// never creates. The temporary schema always exists, so an entry naming it
/// selects it.
///
/// # Errors
///
/// Returns
/// [`SchemaNotFoundForRelation`](crate::errors::Error::SchemaNotFoundForRelation)
/// when the path names only schemas the input never creates, and
/// [`NoSchemaSelectedForRelation`](crate::errors::Error::NoSchemaSelectedForRelation)
/// when the path names none at all.
fn search_path_qualifier(
    builder: &ParserDBBuilder,
    object_kind: crate::errors::ObjectKind,
    relation_name: &str,
) -> Result<Option<Ident>, crate::errors::Error> {
    let mut named = None;
    for (entry, quoted) in builder.search_path().filter(|(entry, _)| !entry.is_empty()) {
        if identifiers_match(entry, quoted, "public", false) {
            return Ok(None);
        }
        if is_temporary_schema(entry, quoted) {
            return Ok(Some(Ident::new(TEMPORARY_SCHEMA)));
        }

        if let Some(schema) = declared_schema(builder, entry, quoted) {
            // The catalog spelling wins over the one the path used, since the
            // two only differ where quoting makes them the same name anyway.
            return Ok(Some(if schema.is_quoted() {
                Ident::with_quote('"', schema.name())
            } else {
                Ident::new(schema.name())
            }));
        }

        named.get_or_insert(entry);
    }

    match named {
        Some(schema_name) => {
            Err(crate::errors::Error::SchemaNotFoundForRelation {
                schema_name: schema_name.to_string(),
                object_kind,
                relation_name: relation_name.to_string(),
            })
        }
        None => {
            Err(crate::errors::Error::NoSchemaSelectedForRelation {
                object_kind,
                relation_name: relation_name.to_string(),
            })
        }
    }
}

/// Checks that a table declares no column name twice.
///
/// PostgreSQL folds an unquoted identifier, so `a` and `A` are one column while
/// `a` and `"A"` are two.
fn validate_distinct_columns(
    create_table: &CreateTable,
    case: IdentifierCase,
) -> Result<(), crate::errors::Error> {
    for (position, column) in create_table.columns.iter().enumerate() {
        let repeated = create_table.columns[..position].iter().any(|earlier| {
            case.identifiers_match(
                earlier.name.value.as_str(),
                earlier.name.quote_style.is_some(),
                column.name.value.as_str(),
                column.name.quote_style.is_some(),
            )
        });
        if repeated {
            return Err(crate::errors::Error::ColumnAlreadyExists {
                table_name: create_table.table_name().to_string(),
                column_name: column.name.value.clone(),
            });
        }
    }
    Ok(())
}

/// A schema qualifier together with whether it was quoted.
type SchemaQualifier<'a> = Option<(&'a str, bool)>;

/// Returns the schema qualifier of a table, if it carries one.
fn table_schema_qualifier(table: &CreateTable) -> SchemaQualifier<'_> {
    table.table_schema().map(|schema| (schema, table.table_schema_is_quoted()))
}

/// Returns whether two qualifiers name the same schema, reading a missing one
/// as `public`, which is the allowance the table store already makes when it
/// refuses a lookup ambiguity.
fn schema_qualifiers_match(
    left: SchemaQualifier<'_>,
    right: SchemaQualifier<'_>,
    case: IdentifierCase,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some((left, left_quoted)), Some((right, right_quoted))) => {
            case.identifiers_match(left, left_quoted, right, right_quoted)
        }
        (Some((schema, quoted)), None) | (None, Some((schema, quoted))) => {
            case.identifiers_match(schema, quoted, "public", false)
        }
    }
}

/// Returns the name a `UNIQUE` or `PRIMARY KEY` column option was declared
/// with, which PostgreSQL gives to the index behind it.
///
/// sqlparser keeps the name on the option rather than inside the constraint it
/// wraps, so the inline spelling has to be read here while the table-constraint
/// spelling carries its own.
fn column_constraint_index_name(option: &ColumnOptionDef) -> Option<&Ident> {
    matches!(option.option, ColumnOption::Unique(_) | ColumnOption::PrimaryKey(_))
        .then_some(option.name.as_ref())
        .flatten()
}

/// Every name a table node puts into the relation pool of its schema: its own,
/// and the name of each `UNIQUE` or `PRIMARY KEY` constraint, whose backing
/// index PostgreSQL creates under that name.
fn relation_names_of(create_table: &CreateTable) -> Vec<(ObjectKind, &Ident)> {
    let mut names = Vec::new();
    if let Some(ObjectNamePart::Identifier(table_name)) = create_table.name.0.last() {
        names.push((ObjectKind::Table, table_name));
    }
    let inline = create_table
        .columns
        .iter()
        .flat_map(|column| column.options.iter())
        .filter_map(column_constraint_index_name);
    let declared = create_table.constraints.iter().filter_map(|constraint| {
        match constraint {
            TableConstraint::Unique(unique) => unique.name.as_ref().or(unique.index_name.as_ref()),
            TableConstraint::PrimaryKey(primary_key) => {
                primary_key.name.as_ref().or(primary_key.index_name.as_ref())
            }
            _ => None,
        }
    });
    names.extend(inline.chain(declared).map(|name| (ObjectKind::UniqueIndex, name)));
    names
}

/// Returns whether an index holds `name` in `schema`. An index takes its schema
/// from the table it is on.
fn index_holds_name<A>(
    index: &TableAttribute<CreateTable, A>,
    name: &Ident,
    schema: SchemaQualifier<'_>,
    case: IdentifierCase,
) -> bool
where
    TableAttribute<CreateTable, A>: IndexLike,
{
    IndexLike::name(index).is_some_and(|candidate| {
        case.identifiers_match(
            candidate,
            index.name_is_quoted(),
            name.value.as_str(),
            name.quote_style.is_some(),
        )
    }) && schema_qualifiers_match(table_schema_qualifier(index.table()), schema, case)
}

/// Returns the kind of index already holding `name` in `schema`, if any,
/// whether it came from a `CREATE INDEX` or from a named `UNIQUE` or
/// `PRIMARY KEY` constraint.
fn index_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    let case = builder.identifier_case();
    if builder.indices().iter().any(|(index, _)| index_holds_name(index, name, schema, case)) {
        return Some(ObjectKind::Index);
    }
    builder
        .unique_indices()
        .iter()
        .any(|(index, _)| index_holds_name(index, name, schema, case))
        .then_some(ObjectKind::UniqueIndex)
}

/// Returns the kind of view already holding `name` in `schema`, if any.
fn view_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    let case = builder.identifier_case();
    if builder.views().iter().any(|(view, _)| view_holds_name(view.as_ref(), name, schema, case)) {
        return Some(ObjectKind::View);
    }
    builder
        .materialized_views()
        .iter()
        .any(|(view, _)| view_holds_name(view.as_ref(), name, schema, case))
        .then_some(ObjectKind::MaterializedView)
}

/// Returns whether a stored view answers `name` in `schema`.
fn view_holds_name<V: ViewLike>(
    view: &V,
    name: &Ident,
    schema: SchemaQualifier<'_>,
    case: IdentifierCase,
) -> bool {
    case.identifiers_match(
        view.view_name(),
        view.view_name_is_quoted(),
        name.value.as_str(),
        name.quote_style.is_some(),
    ) && schema_qualifiers_match(
        view.view_schema().map(|value| (value, view.view_schema_is_quoted())),
        schema,
        case,
    )
}

/// Returns the kind of relation already holding `name` in `schema`, if any.
///
/// Tables in a schema, views and materialized views over them, and indexes on
/// them all share one pool of names. A relation the caller is about to replace
/// has to be out of the stores before this is asked.
fn relation_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    let case = builder.identifier_case();
    let table = builder.tables().iter().any(|(table, _)| {
        case.identifiers_match(
            table.table_name(),
            table.table_name_is_quoted(),
            name.value.as_str(),
            name.quote_style.is_some(),
        ) && schema_qualifiers_match(table_schema_qualifier(table), schema, case)
    });
    if table {
        return Some(ObjectKind::Table);
    }
    view_name_holder(builder, name, schema).or_else(|| index_name_holder(builder, name, schema))
}

/// Checks that nothing in the schema of a table node already holds a name the
/// node introduces, neither the table name itself nor a constraint-backed index
/// name, and that the node does not repeat one of its own.
///
/// One table name against another is left to the builder, which refuses it as a
/// lookup ambiguity and names both spellings, so it is not asked for here.
fn validate_relation_names(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
) -> Result<(), crate::errors::Error> {
    let schema = table_schema_qualifier(create_table);
    let introduced = relation_names_of(create_table);

    for (position, (object_kind, name)) in introduced.iter().enumerate() {
        let against_stores = match object_kind {
            // A table name against another table name is left to the builder,
            // which refuses it as a lookup ambiguity naming both spellings.
            // Views and indexes are asked for here.
            ObjectKind::Table => {
                view_name_holder(builder, name, schema)
                    .or_else(|| index_name_holder(builder, name, schema))
            }
            _ => relation_name_holder(builder, name, schema),
        };
        let conflicting_kind = introduced[..position]
            .iter()
            .find(|(_, earlier)| idents_match(earlier, name))
            .map(|(earlier_kind, _)| *earlier_kind)
            .or(against_stores);
        if let Some(conflicting_kind) = conflicting_kind {
            return Err(crate::errors::Error::RelationNameAlreadyTaken {
                object_kind: *object_kind,
                conflicting_kind,
                object_name: name.value.clone(),
            });
        }
    }
    Ok(())
}

/// Checks that every plain column an index names is declared by its table.
///
/// Entries that are expressions rather than plain columns name no single
/// column, so they are left alone, matching how an index-shaped constraint is
/// checked.
fn validate_index_columns(
    columns: &[IndexColumn],
    include: &[Ident],
    create_table: &CreateTable,
    case: IdentifierCase,
) -> Result<(), LookupError> {
    let absent = |ident: &Ident| {
        (!NamedColumn::of(ident, case).declared_by(create_table)).then(|| {
            LookupError::ColumnNotFound {
                table_name: create_table.name.to_string(),
                column_name: ident.value.clone(),
            }
        })
    };

    for column in columns {
        if let Expr::Identifier(name) = &column.column.expr
            && let Some(error) = absent(name)
        {
            return Err(error);
        }
    }

    for name in include {
        if let Some(error) = absent(name) {
            return Err(error);
        }
    }

    Ok(())
}

/// Enforces [`AccessResolution::ClosedWorld`] on one `GRANT` or `REVOKE`: every
/// relation target names a table or a view, and every grantee names a role,
/// that the input has created up to this statement.
///
/// The two statements carry the same targets and the database answers for them
/// alike, reporting an absent relation before an absent role, which is the
/// order here. A view is a legal target: granting on one is ordinary
/// PostgreSQL, and a blanket grant over a schema covers views too.
fn validate_access_targets_against_builder(
    builder: &ParserDBBuilder,
    grantees: &[Grantee],
    objects: Option<&GrantObjects>,
) -> Result<(), crate::errors::Error> {
    if let Some(GrantObjects::Tables(tables)) = objects {
        for table_obj in tables {
            if builder.resolve_table_object_name(table_obj)?.is_none()
                && views::holds_view(builder, table_obj).is_none()
            {
                return Err(crate::errors::Error::TableNotFoundForGrant {
                    table_name: last_str(table_obj).to_string(),
                });
            }
        }
    }

    for grantee in grantees {
        let Some(grantee_ident) = grantee_role_ident(grantee) else {
            continue;
        };

        let role_exists =
            builder.roles().iter().any(|(role, ())| role_matches_lookup_ident(role, grantee_ident));
        if !role_exists {
            return Err(crate::errors::Error::RoleNotFoundForGrant {
                role_name: grantee_ident.value.clone(),
            });
        }
    }

    Ok(())
}

/// Returns whether a table constraint was declared under `name`.
///
/// PostgreSQL spells the name as a constraint name while MySQL spells some of
/// them as an index name, and sqlparser keeps the two in separate fields. A
/// constraint declared without a name has no name to be found by.
fn table_constraint_has_name(constraint: &TableConstraint, name: &Ident) -> bool {
    let declared = match constraint {
        TableConstraint::Unique(unique) => unique.name.as_ref().or(unique.index_name.as_ref()),
        TableConstraint::PrimaryKey(pk) => pk.name.as_ref().or(pk.index_name.as_ref()),
        TableConstraint::ForeignKey(fk) => fk.name.as_ref(),
        TableConstraint::Check(check) => check.name.as_ref(),
        TableConstraint::Index(index) => index.name.as_ref(),
        TableConstraint::FulltextOrSpatial(index) => index.opt_index_name.as_ref(),
        TableConstraint::PrimaryKeyUsingIndex(using_index)
        | TableConstraint::UniqueUsingIndex(using_index) => using_index.name.as_ref(),
        TableConstraint::Exclude(exclude) => exclude.name.as_ref(),
    };

    declared.is_some_and(|declared| {
        identifiers_match(
            declared.value.as_str(),
            declared.quote_style.is_some(),
            name.value.as_str(),
            name.quote_style.is_some(),
        )
    })
}

/// Whether an index-shaped constraint names the column among its own.
fn table_constraint_covers_column(constraint: &TableConstraint, column: &str) -> bool {
    let columns = match constraint {
        TableConstraint::PrimaryKey(primary_key) => &primary_key.columns,
        TableConstraint::Unique(unique) => &unique.columns,
        _ => return false,
    };
    plain_column_names(columns).iter().any(|named| {
        identifiers_match(named.value.as_str(), named.quote_style.is_some(), column, false)
    })
}

/// Whether a named option written on a column is a constraint at all.
///
/// PostgreSQL lets `CONSTRAINT name` precede a `DEFAULT` or a `NULL` and then
/// records nothing for it, so a `DROP CONSTRAINT` naming one of those is
/// refused. Every other option it accepts a name for becomes a constraint,
/// `NOT NULL` included since PostgreSQL 18 records that as one of its own.
fn column_option_is_constraint(option: &ColumnOption) -> bool {
    matches!(
        option,
        ColumnOption::Check(_)
            | ColumnOption::Unique(_)
            | ColumnOption::PrimaryKey(_)
            | ColumnOption::ForeignKey(_)
            | ColumnOption::NotNull
    )
}

/// The column carrying a constraint written on it under `name`, and where in
/// its option list it sits.
///
/// A constraint reaches a table either in its constraint list or on one of its
/// columns, and `DROP CONSTRAINT` names one without saying which, so both are
/// searched.
fn column_constraint_position(create_table: &CreateTable, name: &Ident) -> Option<(usize, usize)> {
    create_table.columns.iter().enumerate().find_map(|(column, declared)| {
        declared
            .options
            .iter()
            .position(|option| {
                option.name.as_ref().is_some_and(|declared| idents_match(declared, name))
                    && column_option_is_constraint(&option.option)
            })
            .map(|option| (column, option))
    })
}

/// Whether the column already states that it must hold a value.
fn states_not_null(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| matches!(option.option, ColumnOption::NotNull))
}

/// Writes down the `NOT NULL` a key or an identity implies, so that the node
/// carries it rather than leaving every reader to work it out again.
///
/// PostgreSQL keeps it as a constraint of its own rather than as a shadow of
/// the key, which has two consequences this mirrors: a child copying its
/// parent's columns receives it without the key coming too, and dropping the
/// key afterwards leaves the column still requiring a value.
///
/// Called wherever a table node is about to be stored, so every path that
/// builds or rewrites one upholds the invariant without knowing about it.
fn record_implied_not_null(create_table: &mut CreateTable) {
    let keyed: Vec<Ident> = create_table
        .constraints
        .iter()
        .filter_map(|constraint| {
            match constraint {
                TableConstraint::PrimaryKey(primary_key) => Some(&primary_key.columns),
                _ => None,
            }
        })
        .flat_map(|columns| plain_column_names(columns))
        .collect();

    for column in &mut create_table.columns {
        let implied = keyed.iter().any(|keyed| idents_match(keyed, &column.name))
            || column.options.iter().any(|option| {
                matches!(option.option, ColumnOption::PrimaryKey(_))
                    || crate::utils::is_identity(&option.option)
            });
        if implied {
            state_not_null(column);
        }
    }
}

/// The plain columns an index-shaped constraint names, skipping any entry that
/// is an expression rather than a column.
fn plain_column_names(columns: &[IndexColumn]) -> Vec<Ident> {
    columns
        .iter()
        .filter_map(|column| {
            match &column.column.expr {
                Expr::Identifier(ident) => Some(ident.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Writes down that the column must hold a value, unless it already says so.
fn state_not_null(column: &mut ColumnDef) {
    if states_not_null(column) {
        return;
    }
    // An explicit `NULL` cannot stand beside the requirement, the same way
    // `SET NOT NULL` clears one.
    column.options.retain(|option| !matches!(option.option, ColumnOption::Null));
    column.options.push(ColumnOptionDef { name: None, option: ColumnOption::NotNull });
}

/// Takes the constraint written on a column under `name` off that column.
///
/// Used on the table the statement names, where the name identifies exactly one
/// constraint.
fn remove_named_column_constraint(
    create_table: &mut CreateTable,
    written_on: &Ident,
    name: &Ident,
) {
    let Some(column) =
        create_table.columns.iter_mut().find(|declared| idents_match(&declared.name, written_on))
    else {
        return;
    };
    column.options.retain(|held| {
        !(held.name.as_ref().is_some_and(|declared| idents_match(declared, name))
            && column_option_is_constraint(&held.option))
    });
}

/// Takes a table's copy of a constraint written on a column off that column.
///
/// Used on the tables below, whose copy may carry a name of its own, so the
/// match looks past the name a copy would have been given.
fn remove_copied_column_constraint(
    create_table: &mut CreateTable,
    written_on: &Ident,
    option: &ColumnOptionDef,
) {
    let Some(column) =
        create_table.columns.iter_mut().find(|declared| idents_match(&declared.name, written_on))
    else {
        return;
    };
    if let Some(at) =
        column.options.iter().position(|held| inheritance::is_copy_of_option(held, option))
    {
        column.options.remove(at);
    }
}

/// Rewrites every grant grantee naming `previous`.
fn rename_grantee_role(grants: &mut [(Arc<Grant>, ())], previous: &Ident, replacement: &Ident) {
    let names_previous = |grantee: &Grantee| {
        matches!(
            &grantee.name,
            Some(GranteeName::ObjectName(name))
                if object_name_last_identifier(name)
                    .is_some_and(|ident| idents_match(ident, previous))
        )
    };

    for (grant, ()) in grants {
        if !grant.grantees.iter().any(names_previous) {
            continue;
        }
        for grantee in &mut Arc::make_mut(grant).grantees {
            if names_previous(grantee) {
                grantee.name =
                    Some(GranteeName::ObjectName(ObjectName(vec![ObjectNamePart::Identifier(
                        replacement.clone(),
                    )])));
            }
        }
    }
}

fn rename_role_references(builder: &mut ParserDBBuilder, previous: &Ident, replacement: &Ident) {
    let previous_name = stored_role_name(previous);
    let replacement_name = stored_role_name(replacement);

    for (_, metadata) in builder.tables_mut() {
        if metadata.owner() == Some(previous_name.as_str()) {
            metadata.set_owner(Some(replacement_name.clone()));
        }
    }
    for (_, metadata) in builder.functions_mut() {
        if metadata.owner() == Some(previous_name.as_str()) {
            metadata.set_owner(Some(replacement_name.clone()));
        }
    }
    // A view's owner is as much a reference to the role as a table's, so a
    // rename has to reach it too, otherwise the view keeps naming a role that
    // no longer exists and a later `DROP ROLE` sees nothing depending on it.
    for (_, metadata) in builder.views_mut() {
        if metadata.owner() == Some(previous_name.as_str()) {
            metadata.set_owner(Some(replacement_name.clone()));
        }
    }
    for (_, metadata) in builder.materialized_views_mut() {
        if metadata.owner() == Some(previous_name.as_str()) {
            metadata.set_owner(Some(replacement_name.clone()));
        }
    }
    for (schema, ()) in builder.schemas_mut() {
        if schema.authorization() == Some(previous_name.as_str()) {
            let replacement_schema = Schema::with_authorization_and_quoted(
                schema.name().to_string(),
                replacement_name.clone(),
                schema.is_quoted(),
            );
            *schema = Arc::new(replacement_schema);
        }
    }
    for (policy, _) in builder.policies_mut() {
        if let Some(owners) = &mut Arc::make_mut(policy).to {
            for owner in owners {
                if let Owner::Ident(role_ident) = owner
                    && idents_match(role_ident, previous)
                {
                    *role_ident = replacement.clone();
                }
            }
        }
    }
    for (role, ()) in builder.roles_mut() {
        for parent in &mut Arc::make_mut(role).in_role {
            if idents_match(parent, previous) {
                *parent = replacement.clone();
            }
        }
    }
}

fn remove_role_memberships(roles: &mut [(Arc<CreateRole>, ())], removed: &Ident) {
    for (role, ()) in roles {
        Arc::make_mut(role).in_role.retain(|parent| !idents_match(parent, removed));
    }
}

/// Subtracts a revoke from a grant store and returns an unsupported shape.
fn apply_revoke_to_grant_store(
    grants: &mut Vec<(Arc<Grant>, ())>,
    revoke: &sqlparser::ast::Revoke,
) -> Option<&'static str> {
    let mut unsupported = None;
    let mut updated_grants = Vec::with_capacity(grants.len());
    let original_grants = core::mem::take(grants);

    for (grant, ()) in original_grants {
        let (targeted_grantees, untouched_grantees) =
            crate::impls::partition_grantees_for_revoke(&grant.grantees, &revoke.grantees);

        if targeted_grantees.is_empty() {
            updated_grants.push((grant, ()));
            continue;
        }

        let mut targeted_grant = grant.as_ref().clone();
        targeted_grant.grantees = targeted_grantees;
        let application = crate::impls::apply_revoke_to_grant(&targeted_grant, revoke);

        if application.unsupported {
            unsupported.get_or_insert(
                if revoke.grant_option_for
                    && matches!(
                        (&targeted_grant.privileges, &revoke.privileges),
                        (Privileges::All { .. }, Privileges::Actions(_))
                    )
                {
                    "grant-option revoke of a subset from ALL PRIVILEGES is not representable in \
                     this model"
                } else {
                    "column-scoped REVOKE against a table-wide action grant is not representable \
                     in this model"
                },
            );
            updated_grants.push((grant, ()));
            continue;
        }

        if !application.matched {
            updated_grants.push((grant, ()));
            continue;
        }

        if matches!(application.updated_grants.as_slice(), [updated] if updated == &targeted_grant)
        {
            updated_grants.push((grant, ()));
            continue;
        }

        if !untouched_grantees.is_empty() {
            let mut untouched_grant = grant.as_ref().clone();
            untouched_grant.grantees = untouched_grantees;
            updated_grants.push((Arc::new(untouched_grant), ()));
        }

        updated_grants
            .extend(application.updated_grants.into_iter().map(|updated| (Arc::new(updated), ())));
    }

    *grants = updated_grants;
    unsupported
}

/// A database schema parsed from SQL text.
///
/// This is the main type for working with SQL schemas parsed from SQL text.
/// It provides methods for parsing SQL from strings, files, or git
/// repositories.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::GenericDialect;
///
/// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT PRIMARY KEY);")?;
/// let table =
///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
/// assert_eq!(table.table_name(), "users");
/// # Ok(())
/// # }
/// ```
///
/// # Using PostgreSQL dialect
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let sql = "CREATE ROLE admin SUPERUSER LOGIN;";
/// let db = ParserDB::parse::<PostgreSqlDialect>(sql)?;
/// let role = db.role("admin").unwrap();
/// assert!(role.is_superuser());
/// # Ok(())
/// # }
/// ```
///
/// # Constraints declared after the table
///
/// A key declared by `ALTER TABLE ... ADD CONSTRAINT`, which is how `pg_dump`
/// always spells one, answers exactly as the inline declaration does, and
/// `DROP CONSTRAINT` removes it again.
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "
///     CREATE TABLE t (id uuid NOT NULL);
///     ALTER TABLE ONLY t ADD CONSTRAINT t_pkey PRIMARY KEY (id);
///     ",
/// )?;
/// let table =
///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
/// assert_eq!(table.primary_key_column(&db)?.unwrap().column_name(), "id");
/// # Ok(())
/// # }
/// ```
pub type ParserDB = GenericDB<SqlparserProfile>;

/// An access control reference that the database it was read from does not
/// hold.
///
/// Yielded by [`ParserDB::unresolved_access_references`]. Each case names the
/// statement it came from, since a `GRANT` and a `CREATE POLICY` can leave the
/// same role unresolved and are reported apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnresolvedAccessReference<'a> {
    /// A grantee naming a role no `CREATE ROLE` in the input creates.
    GranteeRole(&'a Ident),
    /// A grant target naming a table no `CREATE TABLE` in the input creates.
    GrantTable(&'a ObjectName),
    /// A policy applying to a role no `CREATE ROLE` in the input creates.
    PolicyRole {
        /// Name of the policy.
        policy: &'a Ident,
        /// The role it applies to.
        role: &'a Ident,
    },
}

impl ParserDBIngestor {
    /// Starts an empty schema using the named parser dialect.
    #[must_use]
    pub fn new<D: Dialect + 'static>(catalog_name: String) -> Self {
        Self::with_options::<D>(catalog_name, ParseOptions::default())
    }

    fn with_options<D: Dialect + 'static>(catalog_name: String, options: ParseOptions) -> Self {
        Self::with_dialect(catalog_name, SqlparserDialect::of::<D>(), options)
    }

    fn with_dialect(
        catalog_name: String,
        dialect: SqlparserDialect,
        options: ParseOptions,
    ) -> Self {
        let (access_resolution, postgres_catalog, identifier_case) = options.into_parts();
        let active_postgres_catalog =
            Arc::new(if matches!(dialect, SqlparserDialect::PostgreSql) {
                postgres_catalog
            } else {
                PostgresCatalog::empty()
            });
        let ingestion = Arc::new(ParserIngestion {
            access_resolution,
            active_postgres_catalog,
            collation_metadata: Vec::new(),
        });
        let mut builder: ParserDBBuilder = super::GenericDBBuilder::new(catalog_name, dialect)
            .with_identifier_case(identifier_case);

        let any_type = DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::with_quote('"', "any"))]),
            vec![],
        );

        let arg = |data_type: DataType| {
            OperateFunctionArg { mode: None, name: None, data_type, default_expr: None }
        };

        // The mode, not a name: an argument reader hands back what the input
        // declares, and `VARIADIC` as a name would invent an argument called
        // that.
        let variadic_arg = |data_type: DataType| {
            OperateFunctionArg {
                mode: Some(ArgMode::Variadic),
                name: None,
                data_type,
                default_expr: None,
            }
        };

        let builtins = vec![
            ("length", vec![arg(DataType::Text)], DataType::Int(None)),
            ("len", vec![arg(DataType::Text)], DataType::Int(None)),
            ("char_length", vec![arg(DataType::Text)], DataType::Int(None)),
            ("character_length", vec![arg(DataType::Text)], DataType::Int(None)),
            ("octet_length", vec![arg(DataType::Text)], DataType::Int(None)),
            ("coalesce", vec![variadic_arg(any_type.clone())], any_type.clone()),
            ("nullif", vec![arg(any_type.clone()), arg(any_type.clone())], any_type.clone()),
            ("now", vec![], DataType::Timestamp(None, TimezoneInfo::WithTimeZone)),
            ("current_timestamp", vec![], DataType::Timestamp(None, TimezoneInfo::WithTimeZone)),
            ("current_date", vec![], DataType::Date),
            ("current_time", vec![], DataType::Time(None, TimezoneInfo::WithTimeZone)),
            ("localtimestamp", vec![], DataType::Timestamp(None, TimezoneInfo::None)),
            ("localtime", vec![], DataType::Time(None, TimezoneInfo::None)),
            ("gen_random_uuid", vec![], DataType::Uuid),
            ("uuidv4", vec![], DataType::Uuid),
            ("uuidv7", vec![], DataType::Uuid),
            (
                "uuidv7",
                vec![arg(DataType::Interval { fields: None, precision: None })],
                DataType::Uuid,
            ),
            ("count", vec![arg(any_type.clone())], DataType::BigInt(None)),
            ("sum", vec![arg(any_type.clone())], DataType::Numeric(ExactNumberInfo::None)),
            ("avg", vec![arg(any_type.clone())], DataType::Numeric(ExactNumberInfo::None)),
            ("min", vec![arg(any_type.clone())], any_type.clone()),
            ("max", vec![arg(any_type.clone())], any_type.clone()),
            ("current_user", vec![], DataType::Text),
            ("session_user", vec![], DataType::Text),
            ("user", vec![], DataType::Text),
        ];

        // Qualified the way PostgreSQL holds them, in `pg_catalog` rather than
        // in the schema a `CREATE FUNCTION` lands in. A user function that
        // shadows a builtin is accepted by the server for exactly that reason,
        // so without the qualifier the duplicate check would refuse it.
        for (name, args, return_type) in builtins {
            let create_function = CreateFunction {
                or_alter: false,
                or_replace: false,
                temporary: false,
                if_not_exists: false,
                name: ObjectName(vec![
                    ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                    ObjectNamePart::Identifier(Ident::new(name)),
                ]),
                args: Some(args),
                return_type: Some(FunctionReturnType::DataType(return_type)),
                function_body: Some(CreateFunctionBody::AsBeforeOptions {
                    body: Expr::Value(ValueWithSpan {
                        value: Value::SingleQuotedString(String::new()),
                        span: Span::empty(),
                    }),
                    link_symbol: None,
                }),
                behavior: None,
                called_on_null: None,
                parallel: None,
                using: None,
                language: Some(Ident::new("internal")),
                determinism_specifier: None,
                options: None,
                remote_connection: None,
                security: None,
                set_params: vec![],
            };
            builder = builder.add_function(Arc::new(create_function), FunctionMetadata::default());
        }

        Self { builder, ingestion }
    }

    fn apply_statements(
        self,
        statements: impl IntoIterator<Item = Statement>,
    ) -> Result<Self, crate::errors::Error> {
        let Self { builder, ingestion } = self;
        let ParserIngestion { access_resolution, active_postgres_catalog, collation_metadata } =
            Arc::try_unwrap(ingestion).unwrap_or_else(|shared| (*shared).clone());
        let mut statements = statements.into_iter();
        let (builder, active_postgres_catalog, collation_metadata) = ParserDB::apply_statements(
            builder,
            active_postgres_catalog,
            collation_metadata,
            access_resolution,
            &mut statements,
        )?;
        let ingestion = Arc::new(ParserIngestion {
            access_resolution,
            active_postgres_catalog,
            collation_metadata,
        });
        Ok(Self { builder, ingestion })
    }

    /// Applies one statement to the current schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement is invalid for the current schema.
    pub fn apply_statement(self, statement: Statement) -> Result<Self, crate::errors::Error> {
        self.apply_statements(core::iter::once(statement))
    }

    /// Returns a queryable snapshot without consuming the builder.
    #[must_use]
    pub fn snapshot(&self) -> ParserDB {
        self.builder.snapshot_with(Arc::clone(&self.ingestion))
    }

    /// Returns the final queryable schema.
    #[must_use]
    pub fn finish(self) -> ParserDB {
        self.builder.into_database(self.ingestion)
    }
}

impl ParserDB {
    /// Reopens this database for further statement-by-statement ingestion.
    ///
    /// The returned ingestor continues under the [`ParseOptions`] the
    /// database was built with, and remembers the collations its DDL
    /// created, so resumed ingestion behaves exactly like an uninterrupted
    /// one.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT PRIMARY KEY);")?;
    /// let statement =
    ///     Parser::parse_sql(&PostgreSqlDialect {}, "ALTER TABLE t ADD COLUMN label TEXT;")
    ///         .expect("SQL parses")
    ///         .pop()
    ///         .expect("one statement");
    /// let db = db.into_ingestor().apply_statement(statement)?.finish();
    /// let table = db
    ///     .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
    ///     .expect("table exists");
    /// assert!(
    ///     table
    ///         .column("label", &db, IdentifierCase::AsWritten)
    ///         .expect("column lookup runs")
    ///         .is_some()
    /// );
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn into_ingestor(self) -> ParserDBIngestor {
        let (builder, ingestion) = ParserDBBuilder::from_database(self);
        ParserDBIngestor { builder, ingestion }
    }

    /// Resolves a schema using a parsed SQL identifier.
    ///
    /// Resolution follows PostgreSQL identifier rules:
    /// - quoted identifiers are exact/case-sensitive;
    /// - unquoted identifiers are folded to lowercase.
    #[must_use]
    pub fn resolve_schema_ident(&self, ident: &Ident) -> Option<&Schema> {
        resolve_schema_ident_in_iter(self.schemas.iter().map(|(schema, ())| schema.as_ref()), ident)
    }

    /// Resolves a table from a one-part or two-part SQL object name.
    ///
    /// A one-part name reaches a schema-less table or one stored in the
    /// default schema `public`, two spellings of one place. For two-part
    /// names, the first part is treated as schema and the second part as
    /// table.
    ///
    /// # Errors
    ///
    /// Returns an error when the object name is malformed for table lookup, or
    /// when lookup is ambiguous.
    pub fn resolve_table_object_name(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&CreateTable>, LookupError> {
        let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
        self.resolve_target_table_strict(
            &target_name_of_idents(schema_ident, table_ident),
            IdentifierCase::AsWritten,
        )
    }

    /// Resolves a table from an SQL object name, trying each schema on the
    /// database's search path for an unqualified name.
    ///
    /// The path is walked in order and the first schema holding a match wins,
    /// with the temporary schema `pg_temp` searched first unless the path
    /// names it. A table stored without a schema is found where `public` sits
    /// on the path.
    ///
    /// # Errors
    ///
    /// Returns an error when the object name is malformed for table lookup, or
    /// when lookup is ambiguous.
    pub fn resolve_table_object_name_on_search_path(
        &self,
        object_name: &ObjectName,
    ) -> Result<Option<&CreateTable>, LookupError> {
        let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
        self.resolve_target_table_on_path(
            &target_name_of_idents(schema_ident, table_ident),
            IdentifierCase::AsWritten,
        )
    }

    /// Whether either view kind holds the relation name a grant wrote.
    ///
    /// A grant target is a relation, so a view answers it as readily as a
    /// table, and the caller only needs to know that something holds the name.
    ///
    /// # Errors
    ///
    /// Returns an error when the object name is malformed for relation lookup,
    /// or when lookup is ambiguous.
    fn resolve_grant_view(&self, object_name: &ObjectName) -> Result<Option<()>, LookupError> {
        use crate::traits::DatabaseLike as _;

        let (schema_ident, name_ident) = object_name_identifiers(object_name)?;
        let target = target_name_of_idents(schema_ident, name_ident);
        if self.view_by_target(target.clone(), IdentifierCase::AsWritten)?.is_some() {
            return Ok(Some(()));
        }
        Ok(self.materialized_view_by_target(target, IdentifierCase::AsWritten)?.map(|_| ()))
    }

    /// Reports the roles and table targets that this database's access control
    /// statements name and the database does not itself hold: the grantees and
    /// table targets of its grants, and the roles its policies apply to. The
    /// grant object shapes the closed world never resolved either, `ALL TABLES
    /// IN SCHEMA` and the sequence and schema forms, are left alone here
    /// too.
    ///
    /// An [`AccessResolution::ClosedWorld`] parse rejects such a reference on
    /// the spot, so one surfaces here because the database was parsed under
    /// [`AccessResolution::OpenWorld`]. The walk is order-insensitive, running
    /// against the fully ingested database, so a grant preceding the `CREATE
    /// ROLE` it names resolves. A table target is read as ingestion bound it,
    /// which is how the reading accessors read it, so one the grant reached
    /// keeps resolving whatever the input does to the search path afterwards.
    /// Each distinct reference is reported once, in a deterministic order.
    ///
    /// # Errors
    ///
    /// Returns a [`LookupError`] when a grant target is malformed as a table
    /// name or matches more than one table.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParseOptions::default()
    ///     .with_access_resolution(AccessResolution::OpenWorld)
    ///     .parse::<PostgreSqlDialect>(
    ///         "CREATE TABLE docs (id uuid PRIMARY KEY);
    ///          GRANT SELECT ON docs TO app;
    ///          CREATE POLICY docs_owner ON docs TO auditor USING (true);",
    ///     )?;
    ///
    /// let unresolved: Vec<_> = db.unresolved_access_references()?.collect();
    /// assert!(matches!(
    ///     unresolved[..],
    ///     [
    ///         UnresolvedAccessReference::GranteeRole(grantee),
    ///         UnresolvedAccessReference::PolicyRole { policy, role },
    ///     ] if grantee.value == "app" && policy.value == "docs_owner" && role.value == "auditor"
    /// ));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn unresolved_access_references(
        &self,
    ) -> Result<impl Iterator<Item = UnresolvedAccessReference<'_>>, LookupError> {
        // The parse path records each `GRANT` in both stores, so the set
        // collapses the two views back into one reference per name.
        let grants = self
            .table_grants
            .iter()
            .chain(self.column_grants.iter())
            .map(|(grant, ())| grant.as_ref());

        let declares = |role_ident: &Ident| {
            self.roles.iter().any(|(role, ())| role_matches_lookup_ident(role.as_ref(), role_ident))
        };

        let mut unresolved = BTreeSet::new();
        for grant in grants {
            for grantee in &grant.grantees {
                let Some(grantee_ident) = grantee_role_ident(grantee) else {
                    continue;
                };
                if !declares(grantee_ident) {
                    unresolved.insert(UnresolvedAccessReference::GranteeRole(grantee_ident));
                }
            }

            if let Some(GrantObjects::Tables(tables)) = &grant.objects {
                for table_obj in tables {
                    // A view is as legal a target as a table, so only a name
                    // no relation holds is unresolved.
                    if self.resolve_table_object_name(table_obj)?.is_none()
                        && self.resolve_grant_view(table_obj)?.is_none()
                    {
                        unresolved.insert(UnresolvedAccessReference::GrantTable(table_obj));
                    }
                }
            }
        }

        for (policy, _) in &self.policies {
            for owner in policy.to.iter().flatten() {
                let Some(role_ident) = policy_role_ident(owner) else {
                    continue;
                };
                if !declares(role_ident) {
                    unresolved.insert(UnresolvedAccessReference::PolicyRole {
                        policy: &policy.name,
                        role: role_ident,
                    });
                }
            }
        }

        Ok(unresolved.into_iter())
    }

    /// Checks that every access control statement resolves: each grantee names
    /// a role, each table target names a table, and each policy applies to
    /// a role that this database holds.
    ///
    /// This is the [`AccessResolution::ClosedWorld`] verdict on a database
    /// parsed under [`AccessResolution::OpenWorld`], deferred until the whole
    /// input is in and therefore insensitive to statement order.
    ///
    /// # Errors
    ///
    /// Returns the first unresolved reference as
    /// [`RoleNotFoundForGrant`](crate::errors::Error::RoleNotFoundForGrant),
    /// [`TableNotFoundForGrant`](crate::errors::Error::TableNotFoundForGrant)
    /// or
    /// [`RoleNotFoundForPolicy`](crate::errors::Error::RoleNotFoundForPolicy).
    /// A malformed or ambiguous target name surfaces as
    /// [`IdentifierLookupError`](crate::errors::Error::IdentifierLookupError).
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let options = ParseOptions::default().with_access_resolution(AccessResolution::OpenWorld);
    ///
    /// let db = options.clone().parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id uuid PRIMARY KEY);
    ///      GRANT SELECT ON docs TO app;
    ///      CREATE POLICY docs_app ON docs TO app USING (true);
    ///      CREATE ROLE app;",
    /// )?;
    /// assert!(db.validate_access_targets().is_ok());
    ///
    /// let dangling = options.parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id uuid PRIMARY KEY);
    ///      CREATE POLICY docs_app ON docs TO app USING (true);",
    /// )?;
    /// assert!(dangling.validate_access_targets().is_err());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn validate_access_targets(&self) -> Result<(), crate::errors::Error> {
        match self.unresolved_access_references()?.next() {
            Some(UnresolvedAccessReference::GranteeRole(grantee_ident)) => {
                Err(crate::errors::Error::RoleNotFoundForGrant {
                    role_name: grantee_ident.value.clone(),
                })
            }
            Some(UnresolvedAccessReference::GrantTable(table_obj)) => {
                Err(crate::errors::Error::TableNotFoundForGrant {
                    table_name: last_str(table_obj).to_string(),
                })
            }
            Some(UnresolvedAccessReference::PolicyRole { policy, role }) => {
                Err(crate::errors::Error::RoleNotFoundForPolicy {
                    role_name: role.value.clone(),
                    policy_name: policy.value.clone(),
                })
            }
            None => Ok(()),
        }
    }

    /// Helper function to process check constraints.
    fn process_check_constraint(
        check_expr: &Expr,
        create_table: &Arc<CreateTable>,
        table_metadata: &TableMetadata<CreateTable>,
        builder: &ParserDBBuilder,
    ) -> Result<CheckConstraintResult, crate::errors::Error> {
        let columns_in_expression =
            columns_in_expression::<Arc<TableAttribute<CreateTable, ColumnDef>>>(
                check_expr,
                builder.catalog_name(),
                &target_name_of_object_name(&create_table.name),
                table_metadata.column_arc_slice(),
            )?;
        let functions_in_expression = functions_in_expression::functions_in_expression::<Self>(
            check_expr,
            builder.function_arc_vec().as_slice(),
        );
        Ok((columns_in_expression, functions_in_expression))
    }

    /// Helper function to create an index expression from columns.
    ///
    /// The shape mirrors a parsed parenthesized list, which is what
    /// [`crate::traits::IndexLike::is_simple`] reads: one key nests, several
    /// keys form a tuple. An ordering qualifier or an operator class qualifies
    /// a key rather than forming part of the expression, so neither appears
    /// here.
    fn create_index_expression(columns: &[IndexColumn]) -> Option<Expr> {
        match columns {
            [] => None,
            [single] => Some(Expr::Nested(Box::new(single.column.expr.clone()))),
            _ => {
                Some(Expr::Tuple(columns.iter().map(|column| column.column.expr.clone()).collect()))
            }
        }
    }

    /// Helper function to process unique constraints.
    fn process_unique_constraint(
        unique_constraint: UniqueConstraint,
        create_table: &Arc<CreateTable>,
    ) -> Option<UniqueConstraintResult> {
        let unique_index = Arc::new(TableAttribute::new(create_table.clone(), unique_constraint));
        let expression = Self::create_index_expression(&unique_index.attribute().columns)?;
        let unique_index_metadata = UniqueIndexMetadata::new(expression, create_table.clone());
        Some((unique_index, unique_index_metadata))
    }

    #[allow(clippy::type_complexity)]
    /// Helper function to process create index statements.
    fn process_create_index(
        create_index: CreateIndex,
        builder: &ParserDBBuilder,
    ) -> Result<
        (
            Arc<TableAttribute<CreateTable, CreateIndex>>,
            IndexMetadata<TableAttribute<CreateTable, CreateIndex>>,
        ),
        crate::errors::Error,
    > {
        let table_name = last_str(&create_index.table_name);

        let Some(table) = builder.resolve_table_object_name(&create_index.table_name)? else {
            return Err(crate::errors::Error::TableNotFoundForIndex {
                table_name: table_name.to_string(),
                index_name: create_index.name.as_ref().map_or("<unnamed>", last_str).to_string(),
            });
        };
        validate_index_columns(
            &create_index.columns,
            &create_index.include,
            table,
            builder.identifier_case(),
        )?;

        let index_arc = Arc::new(TableAttribute::new(Arc::new(table.clone()), create_index));
        let Some(expression) = Self::create_index_expression(&index_arc.attribute().columns) else {
            return Err(crate::errors::Error::InvalidIndex {
                index_name: index_arc
                    .attribute()
                    .name
                    .as_ref()
                    .map_or("<unnamed>", last_str)
                    .to_string(),
                reason: "index has no columns".to_string(),
            });
        };
        let metadata = IndexMetadata::new(expression, Arc::new(table.clone()));
        Ok((index_arc, metadata))
    }

    /// Renames a table and carries every reference to it along.
    ///
    /// The rename happens before the referencing tables are rebuilt, because
    /// rebuilding a table resolves its foreign key targets against the stores
    /// and would not find the table under either name in between.
    fn rename_table_checked(
        mut builder: ParserDBBuilder,
        old_name: &ObjectName,
        new_name: ObjectName,
        if_exists: bool,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(resolved_table) = builder.resolve_table_object_name(old_name)? else {
            if if_exists {
                return Ok(builder);
            }
            return Err(crate::errors::Error::RenameTableNotFound {
                table_name: last_str(old_name).to_string(),
            });
        };
        let renamed = StoredTable::of(resolved_table);
        let target = RenameTarget::new(new_name, &resolved_table.name)?;

        // Collected before the rename, while the old name is still the key.
        // A self-reference is not in here: the renamed node rewrites its own.
        let hosts: Vec<StoredTable> = builder
            .tables()
            .iter()
            .map(|(table, _)| table.as_ref())
            .filter(|table| {
                !renamed.matches(table, builder.identifier_case())
                    && (table_references(table, &renamed)
                        || inheritance::names_parent(table, &renamed))
            })
            .map(StoredTable::of)
            .collect();

        builder = Self::replace_table_node(builder, &renamed, |_, node| {
            node.name = target.name.clone();
            rewrite_foreign_key_targets(node, &renamed, &target);
            inheritance::rewrite_parent_names(node, &renamed, &target);
            Ok(())
        })?;

        for host in &hosts {
            builder = Self::replace_table_node(builder, host, |_, node| {
                rewrite_foreign_key_targets(node, &renamed, &target);
                inheritance::rewrite_parent_names(node, &renamed, &target);
                Ok(())
            })?;
        }

        builder.rewrite_table_references(&renamed, &target);

        Ok(builder)
    }

    /// Replaces the stored node of a table with the one `edit` produces and
    /// recomputes every model object derived from it.
    ///
    /// Follows [`Self::alter_table_constraints`]: the node is the single source
    /// of truth, so objects that follow from it are rebuilt rather than
    /// patched, and `CREATE INDEX` indexes are detached and re-attached
    /// because they do not follow from it. An index names its table, so a node
    /// whose name changed takes its indexes with it.
    fn replace_table_node(
        builder: ParserDBBuilder,
        stored: &StoredTable,
        edit: impl FnOnce(&CreateTable, &mut CreateTable) -> Result<(), crate::errors::Error>,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        Self::replace_table_node_with_collations(builder, stored, &[], None, |_| {}, edit)
    }

    fn replace_table_node_with_collations(
        mut builder: ParserDBBuilder,
        stored: &StoredTable,
        collations: &[CreatedCollationMetadata],
        catalog: Option<&PostgresCatalog>,
        preserve: impl FnOnce(&mut Vec<PreservedColumnMetadata>),
        edit: impl FnOnce(&CreateTable, &mut CreateTable) -> Result<(), crate::errors::Error>,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(position) = builder
            .tables()
            .iter()
            .position(|(table, _)| stored.matches(table.as_ref(), builder.identifier_case()))
        else {
            return Err(ObjectKind::Table.not_in_database(&stored.name).into());
        };
        let empty_catalog = PostgresCatalog::empty();
        let catalog = catalog.unwrap_or(&empty_catalog);

        let (previous_node, previous_metadata) = builder.tables_mut().remove(position);
        let mut replacement = (*previous_node).clone();
        edit(&previous_node, &mut replacement)?;
        record_implied_not_null(&mut replacement);

        let mut metadata: TableMetadata<CreateTable> = TableMetadata::default();
        metadata.set_rls_enabled(previous_metadata.rls_enabled());
        metadata.set_rls_forced(previous_metadata.rls_forced());
        metadata.set_owner(previous_metadata.owner().map(str::to_string));
        // Which columns came from a parent is not spelled by the node, so it
        // has to survive the rebuild the way the other unspelled settings do.
        metadata.set_inherited_column_names(previous_metadata.inherited_column_names().to_vec());
        metadata.set_inherited_constraints(inheritance::follow_constraint_rewrite(
            previous_metadata.inherited_constraints(),
            &previous_node.constraints,
            &replacement.constraints,
        ));

        let mut preserved_collations =
            preserved_column_metadata_for_table(&builder, &previous_node);
        preserve(&mut preserved_collations);

        let detached_indices = builder.take_table_derived_objects(
            &stored.name,
            stored.name_quoted,
            stored.schema.as_deref(),
            stored.schema_quoted,
        );

        let replacement = Arc::new(replacement);

        let renamed = !stored.matches(&replacement, builder.identifier_case());
        for (mut index, expression) in detached_indices {
            if renamed {
                index.table_name = replacement.name.clone();
            }
            // The expression follows the column list, which a column rename
            // rewrites, so it is recomputed rather than carried over.
            let expression = Self::create_index_expression(&index.columns).unwrap_or(expression);
            let index = Arc::new(TableAttribute::new(replacement.clone(), index));
            metadata.add_index(index.clone());
            builder = builder.add_index(index, IndexMetadata::new(expression, replacement.clone()));
        }

        builder = Self::ingest_table_node_with_collations(
            builder,
            replacement,
            metadata,
            collations,
            catalog,
            &preserved_collations,
        )?;
        builder.tables_mut().sort_by(|(a, _), (b, _)| {
            (a.table_schema(), a.table_name()).cmp(&(b.table_schema(), b.table_name()))
        });

        Ok(builder)
    }

    /// Helper function to process column options.
    fn process_column_options(
        column: &Arc<TableAttribute<CreateTable, ColumnDef>>,
        create_table: &Arc<CreateTable>,
        table_metadata: &mut TableMetadata<CreateTable>,
        mut builder: ParserDBBuilder,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        for option in &column.attribute().options {
            match option.option.clone() {
                ColumnOption::Check(check_constraint) => {
                    let check_arc = Arc::new(TableAttribute::new(
                        create_table.clone(),
                        check_constraint.clone(),
                    ));
                    table_metadata.add_check_constraint(check_arc.clone());
                    let (columns_in_expression, functions_in_expression) =
                        Self::process_check_constraint(
                            &check_constraint.expr,
                            create_table,
                            table_metadata,
                            &builder,
                        )?;
                    builder = builder.add_check_constraint(
                        check_arc,
                        CheckMetadata::new(
                            *check_constraint.expr.clone(),
                            create_table.clone(),
                            columns_in_expression,
                            functions_in_expression,
                        ),
                    );
                }
                // The same constraint written inline on the column rather than
                // as a table constraint, so it takes the same path: both
                // spellings must resolve their target alike.
                ColumnOption::ForeignKey(mut foreign_key) => {
                    foreign_key.columns.push(column.attribute().name.clone());
                    builder = Self::process_foreign_key_table_constraint(
                        &foreign_key,
                        create_table,
                        table_metadata,
                        builder,
                    )?;
                }
                ColumnOption::Unique(mut unique_constraint) => {
                    // sqlparser keeps the name of an inline constraint on the
                    // option rather than inside the constraint, so without this
                    // the index behind `CONSTRAINT c UNIQUE` would read back
                    // anonymous while the table-constraint spelling of the same
                    // thing reads back as `c`.
                    unique_constraint.name.clone_from(&option.name);
                    unique_constraint.columns.push(IndexColumn {
                        column: OrderByExpr {
                            expr: Expr::Identifier(column.attribute().name.clone()),
                            options: OrderByOptions::default(),
                            with_fill: None,
                        },
                        operator_class: None,
                    });
                    if let Some((unique_index, unique_index_metadata)) =
                        Self::process_unique_constraint(unique_constraint, create_table)
                    {
                        table_metadata.add_unique_index(unique_index.clone());
                        builder = builder.add_unique_index(unique_index, unique_index_metadata);
                    }
                }
                ColumnOption::PrimaryKey(_) => {
                    let primary_key_unique_constraint = UniqueConstraint {
                        name: option.name.clone(),
                        index_name: None,
                        index_type_display: sqlparser::ast::KeyOrIndexDisplay::None,
                        index_type: None,
                        columns: vec![IndexColumn {
                            column: OrderByExpr {
                                expr: Expr::Identifier(column.attribute().name.clone()),
                                options: OrderByOptions::default(),
                                with_fill: None,
                            },
                            operator_class: None,
                        }],
                        include: vec![],
                        index_options: vec![],
                        characteristics: None,
                        nulls_distinct: sqlparser::ast::NullsDistinctOption::None,
                    };

                    if let Some((unique_index, unique_index_metadata)) =
                        Self::process_unique_constraint(primary_key_unique_constraint, create_table)
                    {
                        table_metadata.add_unique_index(unique_index.clone());
                        builder = builder.add_unique_index(unique_index, unique_index_metadata);
                    }

                    table_metadata.set_primary_key(vec![column.clone()]);
                }
                _ => {}
            }
        }
        Ok(builder)
    }

    /// Helper function to process a foreign key table constraint.
    fn process_foreign_key_table_constraint(
        fk: &ForeignKeyConstraint,
        create_table: &Arc<CreateTable>,
        table_metadata: &mut TableMetadata<CreateTable>,
        builder: ParserDBBuilder,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        for col_ident in &fk.columns {
            let column_exists = table_metadata.column_arcs().any(|col| {
                identifiers_match(
                    col.column_name(),
                    col.column_name_is_quoted(),
                    col_ident.value.as_str(),
                    col_ident.quote_style.is_some(),
                )
            });

            if !column_exists {
                return Err(crate::errors::Error::HostColumnNotFoundForForeignKey {
                    host_column: col_ident.value.clone(),
                    host_table: create_table.name.to_string(),
                });
            }
        }

        let referenced_table_name = fk.foreign_table.to_string();

        // An unqualified target resolves through the search path, which carries
        // `public`, so a bare `parent` reaches `public.parent` as it does in
        // the database. The table being created is chained in so a
        // table may reference itself.
        let case = builder.identifier_case();
        let referenced_table = resolve_table_object_name_on_search_path_in_iter(
            builder
                .tables()
                .iter()
                .map(|(t, _)| t.as_ref())
                .chain(core::iter::once(create_table.as_ref())),
            &fk.foreign_table,
            || builder.search_path(),
            case,
            |key| builder.relation_key_is_held(key, case),
        )?;
        let Some(referenced_table) = referenced_table else {
            // A view holding the name is a different complaint: the relation
            // exists and simply cannot be referenced, which is what the
            // database reports.
            if let Some(actual_kind) = views::holds_view(&builder, &fk.foreign_table) {
                return Err(crate::errors::Error::RelationKindMismatch {
                    object_name: referenced_table_name.clone(),
                    expected_kind: ObjectKind::Table,
                    actual_kind,
                });
            }
            return Err(crate::errors::Error::ReferencedTableNotFoundForForeignKey {
                referenced_table: referenced_table_name.clone(),
                host_table: create_table.name.to_string(),
            });
        };
        if referenced_table.is_temporary() != create_table.is_temporary() {
            return Err(crate::errors::Error::ForeignKeyCrossesTemporaryStorage {
                host_table: create_table.name.to_string(),
                referenced_table: referenced_table_name,
                host_is_temporary: create_table.is_temporary(),
            });
        }

        for ref_col_ident in &fk.referred_columns {
            let column_exists = referenced_table.columns.iter().any(|col| {
                identifiers_match(
                    col.name.value.as_str(),
                    col.name.quote_style.is_some(),
                    ref_col_ident.value.as_str(),
                    ref_col_ident.quote_style.is_some(),
                )
            });

            if !column_exists {
                return Err(crate::errors::Error::ReferencedColumnNotFoundForForeignKey {
                    referenced_column: ref_col_ident.value.clone(),
                    referenced_table: referenced_table_name.clone(),
                    host_table: create_table.name.to_string(),
                });
            }
        }

        // Without a unique key on the far side a child row could match more
        // than one parent, so PostgreSQL, MySQL and SQLite all refuse it. A
        // `CREATE UNIQUE INDEX` counts and lives in the builder rather than on
        // the node, so both sources are gathered.
        let referred: Vec<&Ident> = fk.referred_columns.iter().collect();
        let backed = if referred.is_empty() {
            has_primary_key(referenced_table)
        } else {
            let mut candidates = declared_candidate_keys(referenced_table);
            candidates.extend(builder.indices().iter().filter_map(|(index, _)| {
                let node = index.attribute();
                if !node.unique || !table_matches_object_name(referenced_table, &node.table_name) {
                    return None;
                }
                plain_key_columns(&node.columns)
            }));
            candidates.iter().any(|candidate| key_columns_match(&referred, candidate))
        };

        if !backed {
            return Err(crate::errors::Error::ReferencedColumnsNotUniqueForForeignKey {
                referenced_columns: referred
                    .iter()
                    .map(|ident| ident.value.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                referenced_table: referenced_table_name.clone(),
                host_table: create_table.name.to_string(),
            });
        }

        let fk_arc = Arc::new(TableAttribute::new(create_table.clone(), fk.clone()));
        table_metadata.add_foreign_key(fk_arc.clone());
        let builder = builder.add_foreign_key(fk_arc, ());
        Ok(builder)
    }

    /// Helper function to process table constraints.
    fn process_table_constraints(
        constraints: &[TableConstraint],
        create_table: &Arc<CreateTable>,
        table_metadata: &mut TableMetadata<CreateTable>,
        mut builder: ParserDBBuilder,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        for constraint in constraints {
            match constraint {
                TableConstraint::Unique(uc) => {
                    Self::validate_constraint_columns(&uc.columns, create_table, table_metadata)?;
                    if let Some((unique_index, unique_index_metadata)) =
                        Self::process_unique_constraint(uc.clone(), create_table)
                    {
                        table_metadata.add_unique_index(unique_index.clone());
                        builder = builder.add_unique_index(unique_index, unique_index_metadata);
                    }
                }
                TableConstraint::ForeignKey(fk) => {
                    builder = Self::process_foreign_key_table_constraint(
                        fk,
                        create_table,
                        table_metadata,
                        builder,
                    )?;
                }
                TableConstraint::Check(check) => {
                    let check_arc =
                        Arc::new(TableAttribute::new(create_table.clone(), check.clone()));
                    table_metadata.add_check_constraint(check_arc.clone());
                    let (columns_in_expression, functions_in_expression) =
                        Self::process_check_constraint(
                            &check.expr,
                            create_table,
                            table_metadata,
                            &builder,
                        )?;
                    builder = builder.add_check_constraint(
                        check_arc,
                        CheckMetadata::new(
                            *check.expr.clone(),
                            create_table.clone(),
                            columns_in_expression,
                            functions_in_expression,
                        ),
                    );
                }
                TableConstraint::PrimaryKey(pk) => {
                    Self::validate_constraint_columns(&pk.columns, create_table, table_metadata)?;
                    let mut primary_key_columns = Vec::new();
                    for col_name in &pk.columns {
                        let Expr::Identifier(column_name) = &col_name.column.expr else {
                            return Err(crate::errors::Error::InvalidPrimaryKey {
                                table_name: create_table.name.to_string(),
                                reason: format!(
                                    "primary key entries must be plain columns, found expression `{}`",
                                    col_name.column.expr
                                ),
                            });
                        };
                        primary_key_columns.extend(
                            table_metadata
                                .column_arcs()
                                .filter(|col: &&Arc<TableAttribute<CreateTable, ColumnDef>>| {
                                    identifiers_match(
                                        col.column_name(),
                                        col.column_name_is_quoted(),
                                        column_name.value.as_str(),
                                        column_name.quote_style.is_some(),
                                    )
                                })
                                .cloned(),
                        );
                    }

                    let primary_key_unique_constraint = UniqueConstraint {
                        name: pk.name.clone(),
                        index_name: None,
                        index_type_display: sqlparser::ast::KeyOrIndexDisplay::None,
                        index_type: None,
                        columns: pk.columns.clone(),
                        include: pk.include.clone(),
                        index_options: vec![],
                        characteristics: pk.characteristics,
                        nulls_distinct: sqlparser::ast::NullsDistinctOption::None,
                    };

                    if let Some((unique_index, unique_index_metadata)) =
                        Self::process_unique_constraint(primary_key_unique_constraint, create_table)
                    {
                        table_metadata.add_unique_index(unique_index.clone());
                        builder = builder.add_unique_index(unique_index, unique_index_metadata);
                    }

                    table_metadata.set_primary_key(primary_key_columns);
                }
                _ => {}
            }
        }
        Ok(builder)
    }

    /// Checks that every plain column an index-shaped constraint names is
    /// declared by the table the constraint is attached to.
    ///
    /// Entries that are expressions rather than plain columns name no single
    /// column, so they are left alone.
    fn validate_constraint_columns(
        columns: &[IndexColumn],
        create_table: &CreateTable,
        table_metadata: &TableMetadata<CreateTable>,
    ) -> Result<(), LookupError> {
        for column in columns {
            let Expr::Identifier(column_name) = &column.column.expr else {
                continue;
            };
            let declared = table_metadata.column_arcs().any(|declared| {
                identifiers_match(
                    declared.column_name(),
                    declared.column_name_is_quoted(),
                    column_name.value.as_str(),
                    column_name.quote_style.is_some(),
                )
            });
            if !declared {
                return Err(LookupError::ColumnNotFound {
                    table_name: create_table.name.to_string(),
                    column_name: column_name.value.clone(),
                });
            }
        }
        Ok(())
    }

    /// Ingests a table node, deriving its columns and everything its column
    /// options and table constraints imply.
    ///
    /// `table_metadata` carries the state the node does not express: row level
    /// security flags and `CREATE INDEX` indexes.
    fn ingest_table_node_with_collations(
        mut builder: ParserDBBuilder,
        create_table: Arc<CreateTable>,
        mut table_metadata: TableMetadata<CreateTable>,
        collations: &[CreatedCollationMetadata],
        catalog: &PostgresCatalog,
        preserved: &[PreservedColumnMetadata],
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        validate_table_schema(&builder, &create_table)?;
        validate_distinct_columns(&create_table, builder.identifier_case())?;
        validate_relation_names(&builder, &create_table)?;

        let search_path: Vec<_> =
            builder.search_path().map(|(schema, quoted)| (schema.to_string(), quoted)).collect();
        let validate_missing = should_validate_missing_collations(*builder.dialect());

        for column in create_table.columns.clone() {
            table_metadata.add_column(Arc::new(TableAttribute::new(create_table.clone(), column)));
        }

        for column in table_metadata.clone().column_arcs() {
            builder =
                Self::process_column_options(column, &create_table, &mut table_metadata, builder)?;
            let metadata = column_metadata_for_collations(
                column.attribute(),
                collations,
                catalog,
                &search_path,
                preserved,
                validate_missing,
            )?;
            builder = builder.add_column(column.clone(), metadata);
        }

        builder = Self::process_table_constraints(
            &create_table.constraints,
            &create_table,
            &mut table_metadata,
            builder,
        )?;

        Ok(builder.add_table(create_table, table_metadata)?)
    }

    /// Applies `edit` to the constraint list of the table an `ALTER TABLE`
    /// statement targets, then rebuilds the model objects derived from it.
    ///
    /// A constraint reaches a table either inline in `CREATE TABLE` or later
    /// through `ALTER TABLE`, and both spellings have to answer alike, so the
    /// table node stays the single source of truth and everything derived from
    /// it is recomputed rather than patched.
    fn alter_table_constraints(
        builder: ParserDBBuilder,
        stored: &StoredTable,
        edit: impl FnOnce(&CreateTable, &mut Vec<TableConstraint>) -> Result<(), crate::errors::Error>,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        Self::replace_table_node(builder, stored, |previous, replacement| {
            edit(previous, &mut replacement.constraints)
        })
    }

    /// Refuses the spellings of `ALTER TABLE [ONLY] ... ADD CONSTRAINT` that
    /// PostgreSQL refuses, all measured on 18.4.
    ///
    /// A check would have to reach the tables below, so `ONLY` is refused for
    /// one while any exist, except written `NO INHERIT`, when it never
    /// travels and `ONLY` changes nothing, though a partitioned table refuses
    /// that spelling outright. A unique constraint and a foreign key stay
    /// with the named table even where tables inherit, a foreign key on a
    /// partitioned table taking no `ONLY` at all. A primary key is granted
    /// `ONLY` where every table below already requires the keyed columns,
    /// because the `NOT NULL` it implies is the one part that cannot stop at
    /// the named table.
    fn refuse_unaddable_constraint(
        builder: &ParserDBBuilder,
        stored: &StoredTable,
        scope: AlterTableScope,
        constraint: &TableConstraint,
    ) -> Result<(), crate::errors::Error> {
        match constraint {
            TableConstraint::Check(check) if check.no_inherit => {
                if Self::stored_node(builder, stored)?.partition_by.is_some() {
                    return Err(crate::errors::Error::NoInheritCheckOnPartitionedTable {
                        table_name: stored.name.clone(),
                    });
                }
            }
            TableConstraint::Check(_) => {
                Self::refuse_only_with_children(
                    builder,
                    stored,
                    scope,
                    crate::errors::InheritedChange::AddConstraint,
                )?;
            }
            TableConstraint::ForeignKey(_) => {
                if scope.only && Self::stored_node(builder, stored)?.partition_by.is_some() {
                    return Err(crate::errors::Error::OnlyForeignKeyOnPartitionedTable {
                        table_name: stored.name.clone(),
                    });
                }
            }
            TableConstraint::PrimaryKey(primary_key) if scope.only => {
                let keyed = plain_column_names(&primary_key.columns);
                for descendant in inheritance::descendants(builder, stored) {
                    let node = Self::stored_node(builder, &descendant)?;
                    if let Some(column) = node.columns.iter().find(|column| {
                        keyed.iter().any(|keyed| idents_match(keyed, &column.name))
                            && !states_not_null(column)
                    }) {
                        return Err(crate::errors::Error::OnlyPrimaryKeyOnNullableColumn {
                            table_name: descendant.name.clone(),
                            column_name: column.name.value.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Adds a constraint to a table and to every table that takes its shape
    /// from it.
    ///
    /// The walk goes one edge at a time rather than over the whole descendant
    /// set, because how much of a parent's declaration a table receives
    /// depends on the spelling of the edge it arrives through: an `INHERITS`
    /// child receives a check and nothing else, while a partition receives the
    /// root's keys and foreign keys as well.
    fn alter_table_add_constraint(
        builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        mut constraint: TableConstraint,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };
        Self::refuse_unaddable_constraint(&builder, &stored, scope, &constraint)?;
        if let TableConstraint::ForeignKey(foreign_key) = &mut constraint {
            builder.bind_table_reference(&mut foreign_key.foreign_table);
        }

        let mut builder = Self::alter_table_constraints(builder, &stored, |_, constraints| {
            constraints.push(constraint.clone());
            Ok(())
        })?;

        // Nothing below the named table changes under `ONLY`: a table created
        // afterwards still receives its copy at creation, which is also what
        // the server leaves behind.
        if scope.only {
            return Ok(builder);
        }

        // The requirement to hold a value reaches every table below, even where
        // the key itself stays put: PostgreSQL records it as a constraint of
        // its own and passes that one down. The named table already carries it,
        // because storing its node writes it in.
        if let TableConstraint::PrimaryKey(primary_key) = &constraint {
            let keyed = plain_column_names(&primary_key.columns);
            for child in inheritance::descendants(&builder, &stored) {
                builder = Self::replace_table_node(builder, &child, |_, node| {
                    for column in &mut node.columns {
                        if keyed.iter().any(|keyed| idents_match(keyed, &column.name)) {
                            state_not_null(column);
                        }
                    }
                    Ok(())
                })?;
            }
        }

        let mut frontier = alloc::vec![(stored, constraint)];
        let mut reached: Vec<StoredTable> = Vec::new();

        while let Some((current, passed)) = frontier.pop() {
            for (kind, child) in inheritance::direct_children(&builder, &current) {
                // Whether the edge carries the constraint is a property of the
                // edge, so it is asked before the table is counted as visited.
                if !inheritance::passes_down(kind, &passed) || reached.contains(&child) {
                    continue;
                }
                reached.push(child.clone());

                // A table already holding an equivalent constraint keeps its
                // own, the way PostgreSQL merges the two rather than adding a
                // second, and keeps it as its own so the parent's drop spares
                // it. Asked before a name is built, since the copy is then
                // never made.
                let node = Self::stored_node(&builder, &child)?;
                if let Some(held) =
                    node.constraints.iter().find(|held| inheritance::is_copy_of(held, &passed))
                {
                    frontier.push((child, held.clone()));
                    continue;
                }

                let mut taken = Vec::new();
                let Some(copy) =
                    inheritance::received_constraint(&builder, kind, node, &passed, &mut taken)
                else {
                    continue;
                };

                // The merge above took the exact copies, so a held constraint
                // still carrying the name cannot merge with the arriving one,
                // which PostgreSQL refuses.
                if let Some(name) = inheritance::declared_name(&copy)
                    && node.constraints.iter().any(|held| table_constraint_has_name(held, name))
                {
                    return Err(crate::errors::Error::InheritedConstraintConflict {
                        table_name: child.name.clone(),
                        constraint_name: name.value.clone(),
                    });
                }

                let recorded = copy.clone();
                builder = Self::alter_table_constraints(builder, &child, |_, constraints| {
                    constraints.push(recorded);
                    Ok(())
                })?;
                inheritance::mark_inherited_constraint(&mut builder, &child, &copy);
                frontier.push((child, copy));
            }
        }

        Ok(builder)
    }

    /// Drops a named constraint from a table, taking it out of every table that
    /// received it.
    ///
    /// A constraint reaches a table either in its constraint list or written on
    /// one of its columns, and the statement names one without saying which, so
    /// both are searched. The two are removed from different places and travel
    /// by different routes, so each has its own body below.
    fn alter_table_drop_constraint(
        builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        name: &Ident,
        if_exists: bool,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };

        let node = Self::stored_node(&builder, &stored)?;
        if let Some(dropped) =
            node.constraints.iter().find(|held| table_constraint_has_name(held, name)).cloned()
        {
            return Self::drop_table_constraint(builder, &stored, scope, name, dropped);
        }

        if let Some((column, option)) = column_constraint_position(node, name) {
            let written_on = node.columns[column].name.clone();
            let option = node.columns[column].options[option].clone();
            return Self::drop_column_constraint(
                builder,
                &stored,
                scope,
                name,
                &written_on,
                &option,
            );
        }

        if if_exists {
            return Ok(builder);
        }
        Err(crate::errors::Error::DropConstraintNotFound {
            table_name: node.name.to_string(),
            constraint_name: name.value.clone(),
        })
    }

    /// Drops a constraint held in the table's own constraint list.
    ///
    /// A table that declared an equivalent constraint itself keeps its own, and
    /// so does one still receiving it from another parent. `ONLY` stops the
    /// walk at the named table and leaves the copies behind as their
    /// holders' own, which is what PostgreSQL leaves.
    fn drop_table_constraint(
        builder: ParserDBBuilder,
        stored: &StoredTable,
        scope: AlterTableScope,
        name: &Ident,
        dropped: TableConstraint,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let node = Self::stored_node(&builder, stored)?;

        // Only the table that holds a constraint as its own may drop it, which
        // is checked before anything moves so a refusal changes nothing.
        if inheritance::receives_constraint(&builder, node, &dropped) {
            return Err(crate::errors::Error::InheritedConstraintNotDroppable {
                table_name: stored.name.clone(),
                constraint_name: name.value.clone(),
            });
        }

        let mut builder = Self::alter_table_constraints(builder, stored, |_, constraints| {
            constraints.retain(|held| !table_constraint_has_name(held, name));
            Ok(())
        })?;

        if scope.only {
            // The tables below keep their copies, and each becomes the holder's
            // own now that nothing passes it down. A grandchild is untouched,
            // because its own parent still holds one.
            for (_, child) in inheritance::direct_children(&builder, stored) {
                let node = Self::stored_node(&builder, &child)?;
                let held: Vec<TableConstraint> = node
                    .constraints
                    .iter()
                    .filter(|held| inheritance::is_copy_of(held, &dropped))
                    .cloned()
                    .collect();
                for constraint in &held {
                    inheritance::unmark_inherited_constraint(&mut builder, &child, constraint);
                }
            }
            return Ok(builder);
        }

        let mut frontier = alloc::vec![(stored.clone(), dropped)];
        let mut reached: Vec<StoredTable> = Vec::new();

        while let Some((current, passed)) = frontier.pop() {
            for (kind, child) in inheritance::direct_children(&builder, &current) {
                if reached.contains(&child) || !inheritance::passes_down(kind, &passed) {
                    continue;
                }
                reached.push(child.clone());

                let node = Self::stored_node(&builder, &child)?;
                let Some(copy) = node
                    .constraints
                    .iter()
                    .find(|held| inheritance::is_copy_of(held, &passed))
                    .cloned()
                else {
                    continue;
                };

                // A copy the table declared itself stays, and so does one
                // another parent still passes down.
                if !inheritance::records_inherited_constraint(&builder, &child, &copy)
                    || inheritance::receives_constraint(&builder, node, &copy)
                {
                    continue;
                }

                let removed = copy.clone();
                builder = Self::alter_table_constraints(builder, &child, |_, constraints| {
                    constraints.retain(|held| *held != removed);
                    Ok(())
                })?;
                frontier.push((child, copy));
            }
        }

        Ok(builder)
    }

    /// Drops a constraint written on one of the table's columns.
    ///
    /// Nothing has to be recorded about where such a copy came from, unlike one
    /// in the constraint list: a child's copy always arrives with the column it
    /// is written on, so it can never be the child's own, and whether a parent
    /// still writes it is read from the parents.
    fn drop_column_constraint(
        builder: ParserDBBuilder,
        stored: &StoredTable,
        scope: AlterTableScope,
        name: &Ident,
        written_on: &Ident,
        option: &ColumnOptionDef,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let node = Self::stored_node(&builder, stored)?;
        if inheritance::receives_column_constraint(&builder, node, written_on, option) {
            return Err(crate::errors::Error::InheritedConstraintNotDroppable {
                table_name: stored.name.clone(),
                constraint_name: name.value.clone(),
            });
        }

        let mut builder = Self::replace_table_node(builder, stored, |_, node| {
            remove_named_column_constraint(node, written_on, name);
            Ok(())
        })?;

        if scope.only {
            return Ok(builder);
        }

        // One edge at a time, because a partition's copy of a key carries a
        // name of its own, so each level is recognised against the copy the
        // level above holds rather than against the original.
        let mut frontier = alloc::vec![(stored.clone(), option.clone())];
        let mut reached: Vec<StoredTable> = Vec::new();

        while let Some((current, passed)) = frontier.pop() {
            for (kind, child) in inheritance::direct_children(&builder, &current) {
                if !inheritance::option_passes_down(kind, &passed.option)
                    || reached.contains(&child)
                {
                    continue;
                }
                reached.push(child.clone());

                let node = Self::stored_node(&builder, &child)?;
                let Some(held) = node
                    .columns
                    .iter()
                    .find(|declared| idents_match(&declared.name, written_on))
                    .and_then(|declared| {
                        declared
                            .options
                            .iter()
                            .find(|held| inheritance::is_copy_of_option(held, &passed))
                    })
                    .cloned()
                else {
                    continue;
                };

                // Another parent may still write it, in which case the copy
                // stays, the same way it does for one in the constraint list.
                if inheritance::receives_column_constraint(&builder, node, written_on, &held) {
                    continue;
                }

                let removed = held.clone();
                builder = Self::replace_table_node(builder, &child, |_, node| {
                    remove_copied_column_constraint(node, written_on, &removed);
                    Ok(())
                })?;
                frontier.push((child, held));
            }
        }

        Ok(builder)
    }

    /// Applies `edit` to the metadata of the table an `ALTER TABLE` statement
    /// targets.
    ///
    /// Metadata carries settings the table node does not spell, so unlike
    /// [`Self::alter_table_constraints`] nothing derived from the node needs
    /// rebuilding.
    ///
    /// # Errors
    ///
    /// Returns [`crate::errors::Error::AlterTableNotFound`] when the statement
    /// names a table the input never created and does not say `IF EXISTS`.
    fn alter_table_metadata(
        mut builder: ParserDBBuilder,
        table_name: &ObjectName,
        if_exists: bool,
        edit: impl FnOnce(&mut TableMetadata<CreateTable>),
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(resolved_table) = builder.resolve_table_object_name(table_name)? else {
            if if_exists {
                return Ok(builder);
            }
            return Err(crate::errors::Error::AlterTableNotFound {
                table_name: last_str(table_name).to_string(),
            });
        };
        let resolved_table_name = resolved_table.table_name().to_string();
        let resolved_table_quoted = resolved_table.table_name_is_quoted();
        let resolved_schema_name = resolved_table.table_schema().map(str::to_string);
        let resolved_schema_quoted = resolved_table.table_schema_is_quoted();

        let case = builder.identifier_case();
        let Some(entry) = builder.tables_mut().iter_mut().find(|(table, _)| {
            table_matches_resolved_identity(
                table.as_ref(),
                &resolved_table_name,
                resolved_table_quoted,
                resolved_schema_name.as_deref(),
                resolved_schema_quoted,
                case,
            )
        }) else {
            // The identity came from a table this builder resolved, so this is
            // unreachable. `IF EXISTS` does not apply: the statement's table
            // was found, and it is the stored entry that went
            // missing.
            return Err(ObjectKind::Table.not_in_database(&resolved_table_name).into());
        };

        edit(&mut entry.1);

        Ok(builder)
    }

    /// Resolves the table an `ALTER TABLE` names, or reports that it is absent
    /// unless the statement said `IF EXISTS`.
    fn alter_table_target(
        builder: &ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
    ) -> Result<Option<StoredTable>, crate::errors::Error> {
        match builder.resolve_table_object_name(table_name)? {
            Some(resolved) => Ok(Some(StoredTable::of(resolved))),
            None if scope.if_exists => Ok(None),
            None => {
                Err(crate::errors::Error::AlterTableNotFound {
                    table_name: last_str(table_name).to_string(),
                })
            }
        }
    }

    /// Resolves the stored function an `ALTER FUNCTION` names.
    ///
    /// A statement that spells the argument list names one function, and one
    /// that omits it names whichever function carries the name, so long as only
    /// one does.
    fn alter_function_target(
        builder: &ParserDBBuilder,
        func_desc: &FunctionDesc,
    ) -> Result<usize, crate::errors::Error> {
        let case = builder.identifier_case();
        let matching: Vec<usize> = builder
            .functions()
            .iter()
            .enumerate()
            .filter(|(_, (function, _))| {
                match &func_desc.args {
                    Some(args) => {
                        function_signatures_match(
                            &function.name,
                            function.args.as_deref(),
                            &func_desc.name,
                            Some(args),
                            case,
                        )
                    }
                    None => object_names_match(&function.name, &func_desc.name, case),
                }
            })
            .map(|(position, _)| position)
            .collect();

        let Some(&position) = matching.first() else {
            return Err(crate::errors::Error::AlterFunctionNotFound {
                function_name: last_str(&func_desc.name).to_string(),
            });
        };

        if func_desc.args.is_none() && matching.len() > 1 {
            return Err(crate::errors::Error::AmbiguousAlterFunction {
                function_name: last_str(&func_desc.name).to_string(),
            });
        }

        Ok(position)
    }

    /// Returns the stored node of a table whose identity is known to be
    /// present.
    fn stored_node<'builder>(
        builder: &'builder ParserDBBuilder,
        stored: &StoredTable,
    ) -> Result<&'builder CreateTable, crate::errors::Error> {
        builder
            .tables()
            .iter()
            .map(|(table, _)| table.as_ref())
            .find(|table| stored.matches(table, builder.identifier_case()))
            .ok_or_else(|| ObjectKind::Table.not_in_database(&stored.name).into())
    }

    /// Refuses a change `ONLY` would withhold from tables that inherit it.
    ///
    /// PostgreSQL grants `ONLY` only where the change can stand on one table
    /// alone. Adding and renaming cannot: the tables below would end up
    /// disagreeing with the one their shape comes from, so the whole statement
    /// is refused rather than half applied.
    fn refuse_only_with_children(
        builder: &ParserDBBuilder,
        stored: &StoredTable,
        scope: AlterTableScope,
        change: crate::errors::InheritedChange,
    ) -> Result<(), crate::errors::Error> {
        if scope.only && !inheritance::direct_children(builder, stored).is_empty() {
            return Err(crate::errors::Error::OnlyRefusedWithChildren {
                table_name: stored.name.clone(),
                change,
            });
        }
        Ok(())
    }

    /// Adds a column to a table.
    fn alter_table_add_column(
        builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        column_def: ColumnDef,
        if_not_exists: bool,
        position: Option<&MySQLColumnPosition>,
        active_collations: ActiveCollations<'_>,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };
        let added = NamedColumn::of(&column_def.name, builder.identifier_case());

        if added.declared_by(Self::stored_node(&builder, &stored)?) {
            if if_not_exists {
                return Ok(builder);
            }
            return Err(crate::errors::Error::ColumnAlreadyExists {
                table_name: stored.name.clone(),
                column_name: added.name.clone(),
            });
        }
        Self::refuse_only_with_children(
            &builder,
            &stored,
            scope,
            crate::errors::InheritedChange::AddColumn,
        )?;

        // A check riding the column as `NO INHERIT` stays with the named
        // table, and a partitioned table cannot hold one at all.
        if column_def
            .options
            .iter()
            .any(|option| matches!(&option.option, ColumnOption::Check(check) if check.no_inherit))
            && Self::stored_node(&builder, &stored)?.partition_by.is_some()
        {
            return Err(crate::errors::Error::NoInheritCheckOnPartitionedTable {
                table_name: stored.name.clone(),
            });
        }

        // PostgreSQL gives the column to every table inheriting this one,
        // added at the end of each because their own columns already hold
        // their places. Read before the parent changes, so the list is the
        // one the statement found.
        let inheritors = inheritance::descendants(&builder, &stored);
        let mut inherited_def = column_def.clone();
        inherited_def.options.retain(
            |option| !matches!(&option.option, ColumnOption::Check(check) if check.no_inherit),
        );
        let search_path: Vec<_> =
            builder.search_path().map(|(schema, quoted)| (schema.to_string(), quoted)).collect();
        let inherited_metadata = column_metadata_for_collations(
            &inherited_def,
            active_collations.created,
            active_collations.catalog,
            &search_path,
            &[],
            should_validate_missing_collations(*builder.dialect()),
        )?;

        let case = builder.identifier_case();
        let mut builder = Self::replace_table_node_with_collations(
            builder,
            &stored,
            active_collations.created,
            Some(active_collations.catalog),
            |_| {},
            |_, node| {
                let at = match position {
                    Some(MySQLColumnPosition::First) => 0,
                    Some(MySQLColumnPosition::After(after)) => {
                        let after = NamedColumn::of(after, case);
                        node.columns
                            .iter()
                            .position(|declared| after.matches(&declared.name))
                            .map_or(node.columns.len(), |index| index + 1)
                    }
                    None => node.columns.len(),
                };
                node.columns.insert(at, column_def);
                Ok(())
            },
        )?;

        for child in &inheritors {
            // A child already declaring the name keeps its own, the way
            // PostgreSQL merges the two rather than adding a second.
            if added.declared_by(Self::stored_node(&builder, child)?) {
                if let Some(error) = existing_child_column_collation_conflict(
                    &builder,
                    &stored,
                    child,
                    &added,
                    &inherited_def,
                    &inherited_metadata,
                ) {
                    return Err(error);
                }
                continue;
            }
            let inherited_def = inherited_def.clone();
            builder = Self::replace_table_node_with_collations(
                builder,
                child,
                active_collations.created,
                Some(active_collations.catalog),
                |_| {},
                |_, node| {
                    node.columns.push(inherited_def);
                    Ok(())
                },
            )?;
            inheritance::mark_inherited(&mut builder, child, &added.name);
        }

        Ok(builder)
    }

    /// Drops columns from a table, taking with them what the real database
    /// takes: the indexes and constraints on the table itself go along with the
    /// column, and anything outside the table calls for `CASCADE`.
    fn alter_table_drop_columns(
        mut builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        column_names: &[Ident],
        column_if_exists: bool,
        cascade: bool,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };

        for column_name in column_names {
            let column = NamedColumn::of(column_name, builder.identifier_case());

            if !column.declared_by(Self::stored_node(&builder, &stored)?) {
                if column_if_exists {
                    continue;
                }
                return Err(LookupError::ColumnNotFound {
                    table_name: stored.name.clone(),
                    column_name: column.name.clone(),
                }
                .into());
            }

            // A child cannot drop a column it receives from a parent, which
            // PostgreSQL refuses outright.
            if inheritance::is_inherited_column(
                &builder,
                Self::stored_node(&builder, &stored)?,
                column_name,
            ) {
                return Err(crate::errors::Error::InheritedColumnNotDroppable {
                    table_name: stored.name.clone(),
                    column_name: column.name.clone(),
                });
            }

            // The column leaves every table inheriting this one along with it,
            // unless `ONLY` asked for the named table, which leaves each
            // direct child holding the column as its own.
            let inheritors =
                if scope.only { Vec::new() } else { inheritance::descendants(&builder, &stored) };

            // Which other tables declare a column of this name decides who a
            // mention inside a nested query belongs to, so it is read before
            // anything moves.
            let declaring = tables_declaring_column(&builder, &stored, &column);

            if !cascade && builder.column_has_outside_dependents(&stored, &declaring, &column) {
                return Err(crate::errors::Error::ColumnReferenced {
                    table_name: stored.name.clone(),
                    column_name: column.name.clone(),
                });
            }

            // The referencing tables lose their foreign key before the column
            // goes, so that rebuilding them never resolves against a column
            // that is on its way out.
            for host in builder.take_column_outside_dependents(&stored, &declaring, &column) {
                builder = Self::replace_table_node(builder, &host, |_, node| {
                    drop_foreign_keys_to_column(node, &stored, &column);
                    Ok(())
                })?;
            }

            builder.take_column_dependents(&stored, &declaring, &column);
            builder = Self::replace_table_node(builder, &stored, |_, node| {
                drop_column_from_node(node, &stored, &declaring, &column);
                Ok(())
            })?;

            for child in &inheritors {
                let declaring = tables_declaring_column(&builder, child, &column);
                builder.take_column_dependents(child, &declaring, &column);
                builder = Self::replace_table_node(builder, child, |_, node| {
                    drop_column_from_node(node, child, &declaring, &column);
                    Ok(())
                })?;
            }

            if scope.only {
                for (_, child) in inheritance::direct_children(&builder, &stored) {
                    inheritance::unmark_inherited(&mut builder, &child, &column.name);
                }
            }
        }

        Ok(builder)
    }

    /// Renames a column and carries every mention of it along.
    ///
    /// The mentions outside the table's own node are rewritten first, so that
    /// the indexes detached and rebuilt with the node already carry the new
    /// name and their expressions are recomputed from it.
    fn alter_table_rename_column(
        mut builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        old_column_name: &Ident,
        new_column_name: &Ident,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };
        Self::refuse_only_with_children(
            &builder,
            &stored,
            scope,
            crate::errors::InheritedChange::RenameColumn,
        )?;
        let from = NamedColumn::of(old_column_name, builder.identifier_case());
        let to = NamedColumn::of(new_column_name, builder.identifier_case());
        let node = Self::stored_node(&builder, &stored)?;

        if !from.declared_by(node) {
            return Err(LookupError::ColumnNotFound {
                table_name: stored.name.clone(),
                column_name: from.name.clone(),
            }
            .into());
        }
        if !from.matches(new_column_name) && to.declared_by(node) {
            return Err(crate::errors::Error::ColumnAlreadyExists {
                table_name: stored.name.clone(),
                column_name: to.name.clone(),
            });
        }

        // Both lists are read while the old name is still the key. Which other
        // tables declare a column of this name decides who a mention inside a
        // nested query belongs to.
        let hosts: Vec<StoredTable> = builder
            .tables()
            .iter()
            .map(|(table, _)| table.as_ref())
            .filter(|host| {
                !stored.matches(host, builder.identifier_case())
                    && refers_to_column(host, &stored, &from)
            })
            .map(StoredTable::of)
            .collect();
        let declaring = tables_declaring_column(&builder, &stored, &from);
        // The new name reaches every table inheriting this one, because a
        // child holds its own copy of an inherited column.
        let inheritors = inheritance::descendants(&builder, &stored);

        builder.rewrite_column_references(&stored, &declaring, &from, new_column_name);
        builder = Self::replace_table_node_with_collations(
            builder,
            &stored,
            &[],
            None,
            |preserved| rename_preserved_column_metadata(preserved, &from, new_column_name),
            |_, node| {
                rename_column_in_node(node, &stored, &declaring, &from, new_column_name);
                Ok(())
            },
        )?;
        for host in &hosts {
            builder = Self::replace_table_node(builder, host, |_, node| {
                rename_referred_columns(node, &stored, &from, new_column_name);
                Ok(())
            })?;
        }
        for child in &inheritors {
            let declaring = tables_declaring_column(&builder, child, &from);
            builder.rewrite_column_references(child, &declaring, &from, new_column_name);
            builder = Self::replace_table_node_with_collations(
                builder,
                child,
                &[],
                None,
                |preserved| rename_preserved_column_metadata(preserved, &from, new_column_name),
                |_, node| {
                    rename_column_in_node(node, child, &declaring, &from, new_column_name);
                    Ok(())
                },
            )?;
            inheritance::rename_inherited(&mut builder, child, &from.name, new_column_name);
        }

        Ok(builder)
    }

    /// Applies `edit` to the declaration of one column of a table, and to the
    /// same column of every table the change reaches.
    ///
    /// `installs` names the column option the operation is about, which decides
    /// how far it travels: a requirement to hold a value or a default reaches
    /// every table below, while an identity reaches a partition and stops at an
    /// `INHERITS` child, because that child would otherwise own a sequence of
    /// its own. The walk goes one edge at a time for that reason, since the
    /// answer depends on the spelling of each edge rather than on the table.
    ///
    /// `ONLY` stops it at the named table. PostgreSQL grants that here, because
    /// a table below may differ on what a column holds without disagreeing on
    /// what the column is.
    fn alter_table_column_def(
        builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        column_name: &Ident,
        installs: &ColumnOption,
        edit: impl Fn(&mut ColumnDef),
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? else {
            return Ok(builder);
        };
        let column = NamedColumn::of(column_name, builder.identifier_case());

        if !column.declared_by(Self::stored_node(&builder, &stored)?) {
            return Err(LookupError::ColumnNotFound {
                table_name: stored.name.clone(),
                column_name: column.name.clone(),
            }
            .into());
        }

        let apply = |node: &mut CreateTable| {
            if let Some(declared) =
                node.columns.iter_mut().find(|declared| column.matches(&declared.name))
            {
                edit(declared);
            }
        };

        let mut builder = Self::replace_table_node(builder, &stored, |_, node| {
            apply(node);
            Ok(())
        })?;

        if scope.only {
            return Ok(builder);
        }

        let mut frontier = alloc::vec![stored];
        let mut reached: Vec<StoredTable> = Vec::new();

        while let Some(current) = frontier.pop() {
            for (kind, child) in inheritance::direct_children(&builder, &current) {
                if !inheritance::option_passes_down(kind, installs) || reached.contains(&child) {
                    continue;
                }
                reached.push(child.clone());
                builder = Self::replace_table_node(builder, &child, |_, node| {
                    apply(node);
                    Ok(())
                })?;
                frontier.push(child);
            }
        }

        Ok(builder)
    }

    /// Applies an `ALTER COLUMN` operation, refusing the ones PostgreSQL will
    /// not perform before anything moves.
    fn alter_table_alter_column(
        builder: ParserDBBuilder,
        table_name: &ObjectName,
        scope: AlterTableScope,
        column_name: &Ident,
        operation: &AlterColumnOperation,
    ) -> Result<ParserDBBuilder, crate::errors::Error> {
        if let Some(stored) = Self::alter_table_target(&builder, table_name, scope)? {
            let node = Self::stored_node(&builder, &stored)?;
            let column = NamedColumn::of(column_name, builder.identifier_case());
            if column.declared_by(node) {
                Self::refuse_alter_column(&builder, &stored, node, &column, operation)?;
            }
        }

        let installs = Self::altered_column_option(operation);
        Self::alter_table_column_def(
            builder,
            table_name,
            scope,
            column_name,
            &installs,
            |declared| Self::apply_alter_column(declared, operation.clone()),
        )
    }

    /// Applies an `ALTER COLUMN` operation to a column declaration.
    ///
    /// Every operation lands as the column option or data type the same clause
    /// would have spelled inline in `CREATE TABLE`, so both spellings answer
    /// alike.
    fn apply_alter_column(declared: &mut ColumnDef, operation: AlterColumnOperation) {
        let set = |declared: &mut ColumnDef, option: ColumnOption| {
            declared.options.push(ColumnOptionDef { name: None, option });
        };

        match operation {
            AlterColumnOperation::SetNotNull => {
                declared.options.retain(|option| !matches!(option.option, ColumnOption::Null));
                if !declared
                    .options
                    .iter()
                    .any(|option| matches!(option.option, ColumnOption::NotNull))
                {
                    set(declared, ColumnOption::NotNull);
                }
            }
            AlterColumnOperation::DropNotNull => {
                declared.options.retain(|option| !matches!(option.option, ColumnOption::NotNull));
            }
            AlterColumnOperation::SetDefault { value } => {
                declared
                    .options
                    .retain(|option| !matches!(option.option, ColumnOption::Default(_)));
                set(declared, ColumnOption::Default(value));
            }
            AlterColumnOperation::DropDefault => {
                declared
                    .options
                    .retain(|option| !matches!(option.option, ColumnOption::Default(_)));
            }
            AlterColumnOperation::SetDataType { data_type, .. } => {
                declared.data_type = data_type;
            }
            AlterColumnOperation::AddGenerated { generated_as, sequence_options } => {
                let generated_keyword = generated_as.is_some();
                set(
                    declared,
                    ColumnOption::Generated {
                        generated_as: generated_as.unwrap_or(GeneratedAs::ByDefault),
                        sequence_options,
                        generation_expr: None,
                        generation_expr_mode: None,
                        generated_keyword,
                    },
                );
            }
        }
    }

    /// The column option an `ALTER COLUMN` operation is about, which decides
    /// how far down the change travels.
    ///
    /// A removal names the option it removes, since what travels is the same
    /// either way: a table below that received the option receives its removal.
    fn altered_column_option(operation: &AlterColumnOperation) -> ColumnOption {
        match operation {
            AlterColumnOperation::SetNotNull | AlterColumnOperation::DropNotNull => {
                ColumnOption::NotNull
            }
            AlterColumnOperation::SetDefault { value } => ColumnOption::Default(value.clone()),
            AlterColumnOperation::DropDefault => {
                ColumnOption::Default(Expr::Value(sqlparser::ast::Value::Null.with_empty_span()))
            }
            AlterColumnOperation::SetDataType { .. } => ColumnOption::NotNull,
            AlterColumnOperation::AddGenerated { generated_as, sequence_options } => {
                ColumnOption::Generated {
                    generated_as: generated_as.unwrap_or(GeneratedAs::ByDefault),
                    sequence_options: sequence_options.clone(),
                    generation_expr: None,
                    generation_expr_mode: None,
                    generated_keyword: generated_as.is_some(),
                }
            }
        }
    }

    /// Refuses an `ALTER COLUMN` operation PostgreSQL will not perform.
    ///
    /// Removing the requirement to hold a value is refused where a key covers
    /// the column, which PostgreSQL reports as the column being in a primary
    /// key, and where a parent enforces it, which only the parent may lift.
    /// Adding an identity is refused while the column may still hold nothing,
    /// since an identity supplies a value on every row.
    fn refuse_alter_column(
        builder: &ParserDBBuilder,
        stored: &StoredTable,
        node: &CreateTable,
        column: &NamedColumn,
        operation: &AlterColumnOperation,
    ) -> Result<(), crate::errors::Error> {
        let declared = node.columns.iter().find(|declared| column.matches(&declared.name));

        match operation {
            AlterColumnOperation::DropNotNull => {
                if node.constraints.iter().any(|constraint| {
                    matches!(constraint, TableConstraint::PrimaryKey(_))
                        && table_constraint_covers_column(constraint, &column.name)
                }) || declared.is_some_and(|declared| {
                    declared
                        .options
                        .iter()
                        .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
                }) {
                    return Err(crate::errors::Error::RequiredValueNotDroppable {
                        table_name: stored.name.clone(),
                        column_name: column.name.clone(),
                        reason: crate::errors::RequiredValue::CoveredByKey,
                    });
                }
                if inheritance::requires_a_value(builder, node, &column.name) {
                    return Err(crate::errors::Error::RequiredValueNotDroppable {
                        table_name: stored.name.clone(),
                        column_name: column.name.clone(),
                        reason: crate::errors::RequiredValue::EnforcedByParent,
                    });
                }
            }
            AlterColumnOperation::AddGenerated { .. } => {
                let requires = declared.is_some_and(|declared| {
                    declared
                        .options
                        .iter()
                        .any(|option| matches!(option.option, ColumnOption::NotNull))
                });
                if !requires {
                    return Err(crate::errors::Error::IdentityNeedsRequiredValue {
                        table_name: stored.name.clone(),
                        column_name: column.name.clone(),
                    });
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Creates a new `ParserDB` from a vector of SQL statements and a catalog
    /// name.
    ///
    /// A statement the model tracks nothing for is discarded rather than
    /// refused, so a script carrying `VACUUM` or `START TRANSACTION` builds the
    /// schema its other statements describe.
    ///
    /// # Arguments
    ///
    /// * `statements` - A vector of SQL statements to parse.
    /// * `catalog_name` - The name of the database catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if validation fails (e.g., foreign key references
    /// non-existent tables or columns).
    ///
    /// # Example
    ///
    /// ```
    /// use sql_traits::prelude::ParserDB;
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let sql = "
    /// CREATE TABLE users (
    ///     id INTEGER PRIMARY KEY,
    ///     name VARCHAR(100)
    /// );
    /// ";
    ///
    /// let dialect = PostgreSqlDialect {};
    /// let statements = Parser::parse_sql(&dialect, sql).unwrap();
    /// let db = ParserDB::from_statements(statements, "test".to_string()).unwrap();
    /// assert_eq!(db.catalog_name(), "test");
    /// ```
    pub fn from_statements(
        statements: Vec<Statement>,
        catalog_name: String,
    ) -> Result<Self, crate::errors::Error> {
        Self::from_statements_with_dialect(statements, catalog_name, SqlparserDialect::default())
    }

    /// Same as [`Self::from_statements`] but explicitly attaches the SQL
    /// dialect the statements were parsed under. Used by [`Self::parse`] to
    /// route dialect-conditional predicates (see
    /// [`crate::traits::DialectLike::is_bool`]).
    ///
    /// # Errors
    ///
    /// Returns an error if validation fails (e.g. a foreign key references a
    /// non-existent table or column).
    pub fn from_statements_with_dialect(
        statements: Vec<Statement>,
        catalog_name: String,
        dialect: SqlparserDialect,
    ) -> Result<Self, crate::errors::Error> {
        Self::from_statements_with_options(
            statements,
            catalog_name,
            dialect,
            ParseOptions::default(),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn apply_statements(
        mut builder: ParserDBBuilder,
        mut active_postgres_catalog: Arc<PostgresCatalog>,
        mut collation_metadata: Vec<CreatedCollationMetadata>,
        access_resolution: AccessResolution,
        statements: &mut dyn Iterator<Item = Statement>,
    ) -> Result<
        (ParserDBBuilder, Arc<PostgresCatalog>, Vec<CreatedCollationMetadata>),
        crate::errors::Error,
    > {
        let dialect = *builder.dialect();
        for statement in statements {
            match statement {
                Statement::CreateFunction(mut create_function) => {
                    // Two functions may share a name as long as they take
                    // different arguments. A `CREATE OR REPLACE` replaces the
                    // stored node rather than appending a second one, which
                    // would leave the stale node answering every lookup.
                    require_named_in_catalog(
                        &mut create_function.name,
                        crate::errors::ObjectKind::Function,
                        builder.catalog_name(),
                    )?;
                    let case = builder.identifier_case();
                    let existing = builder.functions().iter().position(|(existing, _)| {
                        function_signatures_match(
                            &existing.name,
                            existing.args.as_deref(),
                            &create_function.name,
                            create_function.args.as_deref(),
                            case,
                        )
                    });
                    let replaced = match (existing, create_function.or_replace) {
                        (Some(_), false) => {
                            return Err(crate::errors::Error::FunctionAlreadyExists {
                                function_name: last_str(&create_function.name).to_string(),
                            });
                        }
                        (Some(position), true) => Some(builder.functions_mut().remove(position)),
                        (None, _) => None,
                    };

                    // PostgreSQL keeps the same `pg_proc` entry across a
                    // replacement, so the owner a later statement set on the
                    // old definition still owns the new one.
                    let (replaced, metadata) = match replaced {
                        Some((stale, metadata)) => (Some(stale), metadata),
                        None => (None, FunctionMetadata::default()),
                    };

                    let fresh = Arc::new(create_function);
                    builder = builder.add_function(Arc::clone(&fresh), metadata);

                    // Policies and check constraints cache the function
                    // nodes their expressions call, so a replacement has to
                    // reach those caches too. PostgreSQL keeps the same
                    // pg_proc entry across CREATE OR REPLACE, and dependent
                    // objects see the new definition.
                    if let Some(stale) = replaced {
                        for (_, metadata) in builder.policies_mut() {
                            metadata.replace_function(&stale, &fresh);
                        }
                        for (_, metadata) in builder.check_constraints_mut() {
                            metadata.replace_function(&stale, &fresh);
                        }
                    }
                }
                Statement::DropFunction(drop_function) => {
                    for func_desc in &drop_function.func_desc {
                        let Some((function_name, function_quoted)) =
                            object_name_last_part(&func_desc.name)
                        else {
                            return Err(crate::errors::Error::DropFunctionNotFound {
                                function_name: last_str(&func_desc.name).to_string(),
                            });
                        };

                        // A statement that spells the argument list names one
                        // function, and one that omits it names whichever
                        // function carries the name, so long as only one does.
                        let case = builder.identifier_case();
                        let matching: Vec<usize> = builder
                            .functions()
                            .iter()
                            .enumerate()
                            .filter(|(_, (function, _))| {
                                match &func_desc.args {
                                    Some(args) => {
                                        function_signatures_match(
                                            &function.name,
                                            function.args.as_deref(),
                                            &func_desc.name,
                                            Some(args),
                                            case,
                                        )
                                    }
                                    None => {
                                        object_names_match(&function.name, &func_desc.name, case)
                                    }
                                }
                            })
                            .map(|(position, _)| position)
                            .collect();

                        let Some(&position) = matching.first() else {
                            if drop_function.if_exists {
                                continue;
                            }
                            return Err(crate::errors::Error::DropFunctionNotFound {
                                function_name: function_name.to_string(),
                            });
                        };

                        if func_desc.args.is_none() && matching.len() > 1 {
                            return Err(crate::errors::Error::AmbiguousDropFunction {
                                function_name: function_name.to_string(),
                            });
                        }

                        // Check for references in check constraints, policies,
                        // or triggers
                        if builder.is_function_used(function_name, function_quoted) {
                            return Err(crate::errors::Error::FunctionReferenced {
                                function_name: function_name.to_string(),
                            });
                        }

                        builder.functions_mut().remove(position);
                    }
                }
                Statement::Drop {
                    object_type: sqlparser::ast::ObjectType::Table,
                    if_exists,
                    names,
                    cascade,
                    ..
                } => {
                    for name in names {
                        let table_name = last_str(&name);

                        // A view is refused rather than dropped, and named as
                        // the wrong kind, as PostgreSQL does.
                        views::refuse_dropping_view_as_table(&builder, &name)?;

                        // Check if table exists and resolve the canonical
                        // stored table.
                        let maybe_table = builder.resolve_table_object_name(&name)?;

                        let Some(table) = maybe_table else {
                            if if_exists {
                                continue;
                            }
                            return Err(crate::errors::Error::DropTableNotFound {
                                table_name: table_name.to_string(),
                            });
                        };
                        let resolved_table_name = table.table_name().to_string();
                        let resolved_table_quoted = table.table_name_is_quoted();
                        let resolved_schema_name = table.table_schema().map(str::to_string);
                        let resolved_schema_quoted = table.table_schema_is_quoted();
                        let dropped = StoredTable::of(table);

                        // A view reading the table would name nothing once
                        // the table left, so it blocks the drop and `CASCADE`
                        // takes it along, as PostgreSQL does.
                        let relation_key = stored_table_key(table, IdentifierCase::AsWritten);
                        if cascade {
                            views::remove_dependent_views(&mut builder, &relation_key);
                        } else {
                            views::refuse_dependent_views(
                                &builder,
                                &relation_key,
                                ObjectKind::Table,
                                &resolved_table_name,
                            )?;
                        }

                        // A child builds its column list out of its parents,
                        // so a parent cannot leave while a child still names
                        // it. `CASCADE` takes the children with it.
                        let children = inheritance::descendants(&builder, &dropped);
                        if !cascade && let Some(child) = children.first() {
                            return Err(crate::errors::Error::DropTableInheritedFrom {
                                parent_table: resolved_table_name.clone(),
                                child_table: child.name.clone(),
                            });
                        }
                        for child in &children {
                            builder.remove_table(
                                &child.name,
                                child.name_quoted,
                                child.schema.as_deref(),
                                child.schema_quoted,
                            );
                        }

                        // Check for references from other tables (unless
                        // CASCADE)
                        if !cascade
                            && builder.is_table_referenced(
                                &resolved_table_name,
                                resolved_table_quoted,
                                resolved_schema_name.as_deref(),
                                resolved_schema_quoted,
                            )
                        {
                            return Err(crate::errors::Error::TableReferenced {
                                table_name: resolved_table_name.clone(),
                            });
                        }

                        // Remove the table and all associated objects
                        builder.remove_table(
                            &resolved_table_name,
                            resolved_table_quoted,
                            resolved_schema_name.as_deref(),
                            resolved_schema_quoted,
                        );
                    }
                }
                Statement::Drop {
                    object_type:
                        object_type @ (sqlparser::ast::ObjectType::View
                        | sqlparser::ast::ObjectType::MaterializedView),
                    if_exists,
                    names,
                    cascade,
                    ..
                } => {
                    builder = views::drop_views(
                        builder,
                        &names,
                        object_type == sqlparser::ast::ObjectType::MaterializedView,
                        if_exists,
                        cascade,
                    )?;
                }
                Statement::Drop {
                    object_type: sqlparser::ast::ObjectType::Index,
                    if_exists,
                    names,
                    ..
                } => {
                    for name in names {
                        let Some(position) = builder.index_position(&name)? else {
                            if if_exists {
                                continue;
                            }
                            return Err(crate::errors::Error::DropIndexNotFound {
                                index_name: last_str(&name).to_string(),
                            });
                        };

                        let (removed, _) = builder.indices_mut().remove(position);
                        for (_, table_meta) in builder.tables_mut() {
                            table_meta.retain_indices(|index| !Arc::ptr_eq(index, &removed));
                        }
                    }
                }
                Statement::AlterIndex {
                    name,
                    operation: AlterIndexOperation::RenameIndex { index_name: new_name },
                } => {
                    let Some(position) = builder.index_position(&name)? else {
                        return Err(crate::errors::Error::AlterIndexNotFound {
                            index_name: last_str(&name).to_string(),
                        });
                    };

                    let (stored, metadata) = builder.indices()[position].clone();
                    let schema = table_schema_qualifier(TableAttribute::table(stored.as_ref()));
                    // The renamed index lands in the same pool the old name
                    // came out of, so the new name has to be free there.
                    if let Some(ObjectNamePart::Identifier(new_ident)) = new_name.0.last()
                        && let Some(conflicting_kind) =
                            relation_name_holder(&builder, new_ident, schema)
                    {
                        return Err(crate::errors::Error::RelationNameAlreadyTaken {
                            object_kind: ObjectKind::Index,
                            conflicting_kind,
                            object_name: new_ident.value.clone(),
                        });
                    }

                    let mut renamed = (*stored).clone();
                    renamed.attribute_mut().name = Some(new_name);
                    let renamed = Arc::new(renamed);

                    // The same handle sits in the index store and in the
                    // metadata of the table the index is on, so both take the
                    // replacement.
                    builder.indices_mut()[position] = (renamed.clone(), metadata);
                    for (_, table_metadata) in builder.tables_mut() {
                        table_metadata.replace_index(&stored, &renamed);
                    }
                }
                Statement::CreateTrigger(mut create_trigger) => {
                    require_named(&create_trigger.name, crate::errors::ObjectKind::Trigger)?;
                    let table_name = last_str(&create_trigger.table_name);
                    let table_exists =
                        builder.resolve_table_object_name(&create_trigger.table_name)?.is_some();

                    if !table_exists {
                        // A view holding the name is a different complaint.
                        // PostgreSQL does allow an `INSTEAD OF` row trigger on
                        // a view, which this model cannot record yet, so the
                        // refusal names the kind rather than claiming the
                        // relation is absent.
                        if let Some(actual_kind) =
                            views::holds_view(&builder, &create_trigger.table_name)
                        {
                            return Err(crate::errors::Error::RelationKindMismatch {
                                object_name: table_name.to_string(),
                                expected_kind: crate::errors::ObjectKind::Table,
                                actual_kind,
                            });
                        }
                        return Err(crate::errors::Error::TableNotFoundForTrigger {
                            table_name: table_name.to_string(),
                            trigger_name: last_str(&create_trigger.name).to_string(),
                        });
                    }

                    if let Some(exec_body) = &create_trigger.exec_body {
                        let Some((function_name, function_quoted)) =
                            object_name_last_part(&exec_body.func_desc.name)
                        else {
                            return Err(crate::errors::Error::FunctionNotFoundForTrigger {
                                function_name: last_str(&exec_body.func_desc.name).to_string(),
                                trigger_name: last_str(&create_trigger.name).to_string(),
                            });
                        };
                        let function_exists = builder.function_arc_vec().iter().any(|f| {
                            identifiers_match(
                                f.name(),
                                f.name_is_quoted(),
                                function_name,
                                function_quoted,
                            )
                        });

                        if !function_exists {
                            return Err(crate::errors::Error::FunctionNotFoundForTrigger {
                                function_name: function_name.to_string(),
                                trigger_name: last_str(&create_trigger.name).to_string(),
                            });
                        }
                    }

                    builder.bind_table_reference(&mut create_trigger.table_name);
                    if let Some(referenced) = &mut create_trigger.referenced_table_name {
                        builder.bind_table_reference(referenced);
                    }

                    // A trigger name is unique per table, so the same name on
                    // another table is fine and the match takes both. A
                    // `CREATE OR REPLACE` replaces the stored node rather than
                    // appending a second one, which would leave the stale node
                    // answering every lookup.
                    let case = builder.identifier_case();
                    let existing = builder.triggers().iter().position(|(existing, ())| {
                        object_names_match(&existing.name, &create_trigger.name, case)
                            && target_tables_match(
                                &existing.table_name,
                                &create_trigger.table_name,
                                case,
                            )
                    });
                    match (existing, create_trigger.or_replace) {
                        (Some(_), false) => {
                            return Err(crate::errors::Error::TriggerAlreadyExists {
                                trigger_name: last_str(&create_trigger.name).to_string(),
                                table_name: last_str(&create_trigger.table_name).to_string(),
                            });
                        }
                        (Some(position), true) => {
                            builder.triggers_mut().remove(position);
                        }
                        (None, _) => {}
                    }

                    builder = builder.add_trigger(Arc::new(create_trigger), ());
                }
                Statement::DropTrigger(mut drop_trigger) => {
                    if let Some(table_name) = &mut drop_trigger.table_name {
                        builder.bind_table_reference(table_name);
                    }
                    let trigger_name = last_str(&drop_trigger.trigger_name);

                    // A trigger belongs to the table it was created on, so the
                    // statement names both and both have to match. Dropping by
                    // name alone reached a trigger of the same name on another
                    // table, which the database refuses to do.
                    let case = builder.identifier_case();
                    let matches = |trigger: &CreateTrigger| {
                        object_names_match(&trigger.name, &drop_trigger.trigger_name, case)
                            && drop_trigger.table_name.as_ref().is_none_or(|table_name| {
                                target_tables_match(&trigger.table_name, table_name, case)
                            })
                    };

                    let Some(position) =
                        builder.triggers().iter().position(|(trigger, ())| matches(trigger))
                    else {
                        if drop_trigger.if_exists {
                            continue;
                        }
                        return Err(crate::errors::Error::DropTriggerNotFound {
                            trigger_name: trigger_name.to_string(),
                        });
                    };

                    builder.triggers_mut().remove(position);
                }
                Statement::DropPolicy(mut drop_policy) => {
                    builder.bind_table_reference(&mut drop_policy.table_name);
                    let Some(index) = builder.policies().iter().position(|(policy, _)| {
                        idents_match(&policy.name, &drop_policy.name)
                            && target_tables_match(
                                &policy.table_name,
                                &drop_policy.table_name,
                                IdentifierCase::AsWritten,
                            )
                    }) else {
                        if drop_policy.if_exists {
                            continue;
                        }
                        return Err(crate::errors::Error::DropPolicyNotFound {
                            policy_name: drop_policy.name.value.clone(),
                        });
                    };

                    builder.policies_mut().remove(index);
                }
                Statement::Drop {
                    object_type: sqlparser::ast::ObjectType::Role,
                    if_exists,
                    names,
                    ..
                } => {
                    // Note: DROP ROLE doesn't support CASCADE/RESTRICT in
                    // PostgreSQL syntax. We always use
                    // RESTRICT semantics (fail if role is referenced).
                    for name in names {
                        let Some(role_ident) = object_name_last_identifier(&name) else {
                            continue;
                        };
                        let role_name = role_ident.value.as_str();

                        // Check if role exists
                        let role_exists = builder
                            .roles()
                            .iter()
                            .any(|(role, ())| role_matches_lookup_ident(role, role_ident));

                        if !role_exists {
                            if if_exists {
                                continue;
                            }
                            return Err(crate::errors::Error::DropRoleNotFound {
                                role_name: role_name.to_string(),
                            });
                        }

                        if builder.is_role_referenced(role_ident) {
                            return Err(crate::errors::Error::RoleReferenced {
                                role_name: role_name.to_string(),
                            });
                        }

                        remove_role_memberships(builder.roles_mut(), role_ident);
                        builder
                            .roles_mut()
                            .retain(|(role, ())| !role_matches_lookup_ident(role, role_ident));
                    }
                }
                Statement::AlterRole {
                    name,
                    operation: AlterRoleOperation::RenameRole { role_name: new_name },
                } => {
                    let Some(position) = builder
                        .roles()
                        .iter()
                        .position(|(role, ())| role_matches_lookup_ident(role, &name))
                    else {
                        return Err(crate::errors::Error::AlterRoleNotFound {
                            role_name: name.value.clone(),
                        });
                    };

                    if builder
                        .roles()
                        .iter()
                        .any(|(role, ())| role_matches_lookup_ident(role, &new_name))
                    {
                        return Err(crate::errors::Error::RoleAlreadyExists {
                            role_name: new_name.value.clone(),
                        });
                    }

                    let (role, ()) = &mut builder.roles_mut()[position];
                    for stored in &mut Arc::make_mut(role).names {
                        if object_name_last_identifier(stored)
                            .is_some_and(|stored| idents_match(stored, &name))
                        {
                            *stored =
                                ObjectName(vec![ObjectNamePart::Identifier(new_name.clone())]);
                        }
                    }

                    rename_role_references(&mut builder, &name, &new_name);
                    rename_grantee_role(builder.table_grants_mut(), &name, &new_name);
                    rename_grantee_role(builder.column_grants_mut(), &name, &new_name);
                }
                Statement::Drop {
                    object_type: sqlparser::ast::ObjectType::Schema,
                    if_exists,
                    names,
                    cascade,
                    ..
                } => {
                    for name in names {
                        let schema_name = last_str(&name);
                        let maybe_schema = object_name_last_identifier(&name)
                            .and_then(|ident| builder.resolve_schema_ident(ident));

                        let Some(schema) = maybe_schema else {
                            if if_exists {
                                continue;
                            }
                            return Err(crate::errors::Error::DropSchemaNotFound {
                                schema_name: schema_name.to_string(),
                            });
                        };
                        let resolved_schema_name = schema.name().to_string();
                        let resolved_schema_quoted = schema.is_quoted();

                        // Check for contained objects unless CASCADE is
                        // specified
                        if !cascade
                            && builder
                                .is_schema_non_empty(&resolved_schema_name, resolved_schema_quoted)
                        {
                            return Err(crate::errors::Error::SchemaNotEmpty {
                                schema_name: resolved_schema_name.clone(),
                            });
                        }

                        // If CASCADE, remove all tables in the schema first
                        if cascade {
                            use crate::traits::TableLike;
                            let tables_to_remove: Vec<_> = builder
                                .tables()
                                .iter()
                                .filter(|(t, _)| {
                                    t.table_schema().is_some_and(|table_schema| {
                                        identifiers_match(
                                            table_schema,
                                            t.table_schema_is_quoted(),
                                            &resolved_schema_name,
                                            resolved_schema_quoted,
                                        )
                                    })
                                })
                                .map(|(t, _)| {
                                    (
                                        t.table_name().to_string(),
                                        t.table_name_is_quoted(),
                                        t.table_schema().map(str::to_string),
                                        t.table_schema_is_quoted(),
                                    )
                                })
                                .collect();

                            for (
                                table_name,
                                table_name_quoted,
                                table_schema_name,
                                table_schema_quoted,
                            ) in tables_to_remove
                            {
                                builder.remove_table(
                                    &table_name,
                                    table_name_quoted,
                                    table_schema_name.as_deref(),
                                    table_schema_quoted,
                                );
                            }
                        }

                        // Remove the schema
                        builder.schemas_mut().retain(|(s, ())| {
                            !identifiers_match(
                                s.name(),
                                s.is_quoted(),
                                &resolved_schema_name,
                                resolved_schema_quoted,
                            )
                        });
                    }
                }
                Statement::CreateIndex(mut create_index) => {
                    if let Some(index_name) = create_index.name.as_ref() {
                        require_named(index_name, crate::errors::ObjectKind::Index)?;
                    }
                    builder.bind_table_reference(&mut create_index.table_name);
                    let if_not_exists = create_index.if_not_exists;
                    let (index, metadata) = Self::process_create_index(create_index, &builder)?;
                    let resolved_table = index.table();
                    let resolved_table_name = resolved_table.table_name().to_string();
                    let resolved_table_quoted = resolved_table.table_name_is_quoted();
                    let resolved_schema_name = resolved_table.table_schema().map(str::to_string);
                    let resolved_schema_quoted = resolved_table.table_schema_is_quoted();

                    // An index takes its schema from its table, and shares one
                    // pool of names there with tables and with the indexes
                    // behind named constraints. An unnamed index is named by
                    // the server and contests nothing.
                    if let Some(index_name) = index.attribute().name.as_ref()
                        && let Some(ObjectNamePart::Identifier(index_name)) = index_name.0.last()
                        && let Some(conflicting_kind) = relation_name_holder(
                            &builder,
                            index_name,
                            table_schema_qualifier(resolved_table),
                        )
                    {
                        if if_not_exists {
                            continue;
                        }
                        return Err(crate::errors::Error::RelationNameAlreadyTaken {
                            object_kind: ObjectKind::Index,
                            conflicting_kind,
                            object_name: index_name.value.clone(),
                        });
                    }

                    let case = builder.identifier_case();
                    if let Some(entry) = builder.tables_mut().iter_mut().find(|(table, _)| {
                        table_matches_resolved_identity(
                            table.as_ref(),
                            &resolved_table_name,
                            resolved_table_quoted,
                            resolved_schema_name.as_deref(),
                            resolved_schema_quoted,
                            case,
                        )
                    }) {
                        entry.1.add_index(index.clone());
                    }
                    builder = builder.add_index(index, metadata);
                }
                Statement::AlterTable(alter_table) => {
                    let scope = AlterTableScope {
                        if_exists: alter_table.if_exists,
                        only: alter_table.only,
                    };
                    for operation in alter_table.operations {
                        // `ALTER TABLE` names a relation, and PostgreSQL
                        // accepts it against a view for the actions a view
                        // supports. A view takes over here so the table path
                        // never sees a name it does not hold.
                        if let Some(kind) = views::holds_view(&builder, &alter_table.name) {
                            builder =
                                views::alter_view(builder, &alter_table.name, kind, &operation)?;
                            continue;
                        }
                        match operation {
                            AlterTableOperation::EnableRowLevelSecurity => {
                                builder = Self::alter_table_metadata(
                                    builder,
                                    &alter_table.name,
                                    alter_table.if_exists,
                                    |metadata| metadata.set_rls_enabled(true),
                                )?;
                            }
                            AlterTableOperation::DisableRowLevelSecurity => {
                                builder = Self::alter_table_metadata(
                                    builder,
                                    &alter_table.name,
                                    alter_table.if_exists,
                                    |metadata| metadata.set_rls_enabled(false),
                                )?;
                            }
                            AlterTableOperation::ForceRowLevelSecurity => {
                                builder = Self::alter_table_metadata(
                                    builder,
                                    &alter_table.name,
                                    alter_table.if_exists,
                                    |metadata| metadata.set_rls_forced(true),
                                )?;
                            }
                            AlterTableOperation::NoForceRowLevelSecurity => {
                                builder = Self::alter_table_metadata(
                                    builder,
                                    &alter_table.name,
                                    alter_table.if_exists,
                                    |metadata| metadata.set_rls_forced(false),
                                )?;
                            }
                            AlterTableOperation::RenameTable { table_name } => {
                                let new_name = match table_name {
                                    RenameTableNameKind::As(name)
                                    | RenameTableNameKind::To(name) => name,
                                };
                                builder = Self::rename_table_checked(
                                    builder,
                                    &alter_table.name,
                                    new_name,
                                    alter_table.if_exists,
                                )?;
                            }
                            AlterTableOperation::AddConstraint { constraint, .. } => {
                                builder = Self::alter_table_add_constraint(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    constraint,
                                )?;
                            }
                            AlterTableOperation::DropConstraint { if_exists, name, .. } => {
                                builder = Self::alter_table_drop_constraint(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &name,
                                    if_exists,
                                )?;
                            }
                            AlterTableOperation::AddColumn {
                                if_not_exists,
                                column_def,
                                column_position,
                                ..
                            } => {
                                builder = Self::alter_table_add_column(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    column_def,
                                    if_not_exists,
                                    column_position.as_ref(),
                                    ActiveCollations {
                                        created: &collation_metadata,
                                        catalog: &active_postgres_catalog,
                                    },
                                )?;
                            }
                            AlterTableOperation::DropColumn {
                                column_names,
                                if_exists,
                                drop_behavior,
                                ..
                            } => {
                                builder = Self::alter_table_drop_columns(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &column_names,
                                    if_exists,
                                    drop_behavior == Some(DropBehavior::Cascade),
                                )?;
                            }
                            AlterTableOperation::RenameColumn {
                                old_column_name,
                                new_column_name,
                            } => {
                                builder = Self::alter_table_rename_column(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &old_column_name,
                                    &new_column_name,
                                )?;
                            }
                            AlterTableOperation::AlterColumn { column_name, op } => {
                                builder = Self::alter_table_alter_column(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &column_name,
                                    &op,
                                )?;
                            }
                            AlterTableOperation::ChangeColumn {
                                old_name,
                                new_name,
                                data_type,
                                options,
                                ..
                            } => {
                                builder = Self::alter_table_rename_column(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &old_name,
                                    &new_name,
                                )?;
                                builder = Self::alter_table_column_def(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &new_name,
                                    &ColumnOption::NotNull,
                                    |declared| {
                                        redeclare_column(
                                            declared,
                                            data_type.clone(),
                                            options.clone(),
                                        );
                                    },
                                )?;
                            }
                            AlterTableOperation::ModifyColumn {
                                col_name,
                                data_type,
                                options,
                                ..
                            } => {
                                builder = Self::alter_table_column_def(
                                    builder,
                                    &alter_table.name,
                                    scope,
                                    &col_name,
                                    &ColumnOption::NotNull,
                                    |declared| {
                                        redeclare_column(
                                            declared,
                                            data_type.clone(),
                                            options.clone(),
                                        );
                                    },
                                )?;
                            }

                            // Refused: each of these changes part of the schema
                            // the model represents, so discarding it would
                            // leave the model wrong rather than merely coarse.
                            // An absent table is reported first, so that a
                            // statement wrong in both ways names the plainer
                            // fault and `IF EXISTS` still excuses it.
                            operation @ (AlterTableOperation::DropPrimaryKey { .. }
                            | AlterTableOperation::DropForeignKey { .. }
                            | AlterTableOperation::DropIndex { .. }
                            | AlterTableOperation::RenameConstraint { .. }
                            | AlterTableOperation::SwapWith { .. }) => {
                                if Self::alter_table_target(
                                    &builder,
                                    &alter_table.name,
                                    scope,
                                )?
                                .is_some()
                                {
                                    return Err(
                                        crate::errors::Error::UnsupportedAlterTableOperation {
                                            table_name: last_str(&alter_table.name).to_string(),
                                            operation: operation.to_string(),
                                        },
                                    );
                                }
                            }

                            AlterTableOperation::OwnerTo { new_owner } => {
                                let role_ident = match &new_owner {
                                    Owner::Ident(ident) => Some(ident.clone()),
                                    _ => None,
                                };
                                let owner = match new_owner {
                                    Owner::Ident(ident) => Some(stored_role_name(&ident)),
                                    // These name whoever runs the statement,
                                    // so the owner changed to one the input
                                    // never spells and the model can no longer
                                    // name it either.
                                    Owner::CurrentRole
                                    | Owner::CurrentUser
                                    | Owner::SessionUser => None,
                                };
                                builder = Self::alter_table_metadata(
                                    builder,
                                    &alter_table.name,
                                    alter_table.if_exists,
                                    |metadata| metadata.set_owner(owner),
                                )?;
                                // After the table, because the database reports
                                // an absent table first, and skipped entirely
                                // when `IF EXISTS` excused an absent one, since
                                // the statement then did nothing at all.
                                if access_resolution == AccessResolution::ClosedWorld
                                    && let Some(ident) = &role_ident
                                    && builder.resolve_table_object_name(&alter_table.name)?.is_some()
                                {
                                    validate_owner_role_ident(
                                        &builder,
                                        ident,
                                        last_str(&alter_table.name),
                                    )?;
                                }
                            }

                            // Ignored: each of these changes something the
                            // model carries no representation of, so nothing it
                            // answers can change.
                            //
                            // Physical layout: storage parameters, clustering
                            // and sort keys describe how rows are kept, which
                            // the model does not describe at all.
                            AlterTableOperation::SetTblProperties { .. }
                            | AlterTableOperation::SetOptionsParens { .. }
                            | AlterTableOperation::ClusterBy { .. }
                            | AlterTableOperation::DropClusteringKey
                            | AlterTableOperation::AlterSortKey { .. }
                            | AlterTableOperation::ModifyOrderBy { .. }
                            | AlterTableOperation::SuspendRecluster
                            | AlterTableOperation::ResumeRecluster
                            // Partitions: the model describes a table, never the
                            // partitions it is split across.
                            | AlterTableOperation::AttachPartition { .. }
                            | AlterTableOperation::DetachPartition { .. }
                            | AlterTableOperation::AddPartitions { .. }
                            | AlterTableOperation::DropPartitions { .. }
                            | AlterTableOperation::RenamePartitions { .. }
                            | AlterTableOperation::FreezePartition { .. }
                            | AlterTableOperation::UnfreezePartition { .. }
                            // Projections are a ClickHouse object the model has
                            // no equivalent of.
                            | AlterTableOperation::AddProjection { .. }
                            | AlterTableOperation::DropProjection { .. }
                            | AlterTableOperation::MaterializeProjection { .. }
                            | AlterTableOperation::ClearProjection { .. }
                            // Rewrite rules are not modelled, so their
                            // enablement has nothing to attach to.
                            | AlterTableOperation::EnableRule { .. }
                            | AlterTableOperation::EnableAlwaysRule { .. }
                            | AlterTableOperation::EnableReplicaRule { .. }
                            | AlterTableOperation::DisableRule { .. }
                            // A trigger is modelled, but whether it is armed is
                            // not, so enablement changes nothing answerable.
                            | AlterTableOperation::EnableTrigger { .. }
                            | AlterTableOperation::EnableAlwaysTrigger { .. }
                            | AlterTableOperation::EnableReplicaTrigger { .. }
                            | AlterTableOperation::DisableTrigger { .. }
                            // Replication identity selects which columns
                            // identify a row downstream, which the model does
                            // not track.
                            | AlterTableOperation::ReplicaIdentity { .. }
                            // Constraint validity is not modelled: a constraint
                            // added `NOT VALID` is already stored as declared,
                            // so validating it changes nothing.
                            | AlterTableOperation::ValidateConstraint { .. }
                            // The next value of an auto-increment counter is
                            // table data rather than schema.
                            | AlterTableOperation::AutoIncrement { .. }
                            // Refresh, suspend and resume drive a Snowflake
                            // dynamic table's schedule, not its shape.
                            | AlterTableOperation::Refresh { .. }
                            | AlterTableOperation::Suspend
                            | AlterTableOperation::Resume
                            // MySQL hints choosing how the server performs the
                            // change, with no effect on the result.
                            | AlterTableOperation::Algorithm { .. }
                            | AlterTableOperation::Lock { .. }
                            // Whether a table is write-ahead logged is a
                            // durability choice the model does not describe.
                            | AlterTableOperation::SetLogged
                            | AlterTableOperation::SetUnlogged => {}
                        }
                    }
                }
                Statement::CreateCollation(create_collation) => {
                    if matches!(dialect, SqlparserDialect::PostgreSql) {
                        let search_path: Vec<_> = builder
                            .search_path()
                            .map(|(schema, quoted)| (schema.to_string(), quoted))
                            .collect();
                        if let Some(metadata) = create_collation_metadata(
                            &builder,
                            &create_collation,
                            &collation_metadata,
                            &active_postgres_catalog,
                            &search_path,
                        )? {
                            collation_metadata.push(metadata);
                        }
                    }
                }
                Statement::CreateTable(mut create_table) => {
                    require_named_in_catalog(
                        &mut create_table.name,
                        crate::errors::ObjectKind::Table,
                        builder.catalog_name(),
                    )?;
                    // Placed first, so `IF NOT EXISTS` checks the schema it
                    // truly creates in.
                    create_table.temporary = place_relation(
                        &builder,
                        &mut create_table.name,
                        create_table.temporary,
                        ObjectKind::Table,
                    )?;

                    // `IF NOT EXISTS` skips the statement whole when anything
                    // in the relation pool of the schema already holds the
                    // name, an index as much as a table.
                    if create_table.if_not_exists
                        && let Some(ObjectNamePart::Identifier(table_name)) =
                            create_table.name.0.last()
                        && relation_name_holder(
                            &builder,
                            table_name,
                            table_schema_qualifier(&create_table),
                        )
                        .is_some()
                    {
                        continue;
                    }

                    // A `LIKE` copy becomes the table's own columns, so it
                    // runs before the parents contribute theirs.
                    like::apply_like(&builder, &mut create_table)?;

                    // PostgreSQL copies the parent's shape into the child
                    // while running this statement, so the node carries the
                    // inherited columns from here on and everything derived
                    // from it sees them. A parent has to exist by now, the
                    // way a foreign key target does, which is also what
                    // leaves the edges acyclic.
                    let inherited = inheritance::apply_parents(
                        &builder,
                        &mut create_table,
                        &collation_metadata,
                        &active_postgres_catalog,
                    )?;
                    refuse_no_inherit_check_on_partitioned(&create_table)?;
                    record_implied_not_null(&mut create_table);
                    bind_foreign_key_targets(&builder, &mut create_table);
                    let mut metadata = TableMetadata::default();
                    metadata.set_inherited_column_names(inherited.columns);
                    metadata.set_inherited_constraints(inherited.constraints);
                    builder = Self::ingest_table_node_with_collations(
                        builder,
                        Arc::new(create_table),
                        metadata,
                        &collation_metadata,
                        &active_postgres_catalog,
                        &inherited.column_metadata,
                    )?;
                }
                Statement::CreateView(create_view) => {
                    builder = views::create_view(builder, create_view)?;
                }
                Statement::CreatePolicy(mut policy) => {
                    require_named(&policy.table_name, crate::errors::ObjectKind::Table)?;
                    if access_resolution == AccessResolution::ClosedWorld {
                        validate_policy_roles(
                            &builder,
                            &policy.name.value,
                            policy.to.as_deref().unwrap_or_default(),
                        )?;
                    }

                    // A policy exists only on its table, so an absent one is
                    // refused outright, as the database does and as the
                    // trigger arm above already did. This is not governed by
                    // the access setting: that excuses an absent role, which a
                    // dump legitimately omits, and never an absent table.
                    if builder.resolve_table_object_name(&policy.table_name)?.is_none() {
                        // A view holding the name is a different complaint:
                        // the relation exists and simply cannot carry a
                        // policy, which is what the database reports.
                        if let Some(actual_kind) = views::holds_view(&builder, &policy.table_name) {
                            return Err(crate::errors::Error::RelationKindMismatch {
                                object_name: last_str(&policy.table_name).to_string(),
                                expected_kind: crate::errors::ObjectKind::Table,
                                actual_kind,
                            });
                        }
                        return Err(crate::errors::Error::TableNotFoundForPolicy {
                            table_name: last_str(&policy.table_name).to_string(),
                            policy_name: policy.name.value.clone(),
                        });
                    }
                    builder.bind_table_reference(&mut policy.table_name);

                    // A policy name is unique per table, whatever command the
                    // policy is declared `FOR`. Matched the way `DROP POLICY`
                    // and `ALTER POLICY` match, so the three agree by
                    // construction rather than by inspection.
                    if builder.policies().iter().any(|(existing, _)| {
                        idents_match(&existing.name, &policy.name)
                            && target_tables_match(
                                &existing.table_name,
                                &policy.table_name,
                                IdentifierCase::AsWritten,
                            )
                    }) {
                        return Err(crate::errors::Error::PolicyAlreadyExists {
                            policy_name: policy.name.value.clone(),
                            table_name: last_str(&policy.table_name).to_string(),
                        });
                    }

                    let using_functions = if let Some(using_expr) = &policy.using {
                        functions_in_expression::functions_in_expression::<Self>(
                            using_expr,
                            builder.function_arc_vec().as_slice(),
                        )
                    } else {
                        Vec::new()
                    };

                    let check_functions = if let Some(check_expr) = &policy.with_check {
                        functions_in_expression::functions_in_expression::<Self>(
                            check_expr,
                            builder.function_arc_vec().as_slice(),
                        )
                    } else {
                        Vec::new()
                    };

                    let metadata = PolicyMetadata::new(using_functions, check_functions);
                    builder = builder.add_policy(Arc::new(policy), metadata);
                }
                Statement::CreateRole(create_role) => {
                    // A role name is cluster-wide. Unlike the checks on a role
                    // a grant or a policy names, this one is ungated: the
                    // access setting excuses a dump that omits role creation,
                    // and this statement is the creation.
                    for role_name in &create_role.names {
                        require_named(role_name, crate::errors::ObjectKind::Role)?;
                        let Some(role_ident) = object_name_last_identifier(role_name) else {
                            continue;
                        };
                        if builder
                            .roles()
                            .iter()
                            .any(|(existing, ())| role_matches_lookup_ident(existing, role_ident))
                        {
                            return Err(crate::errors::Error::RoleAlreadyExists {
                                role_name: role_ident.value.clone(),
                            });
                        }
                    }
                    builder = builder.add_role(Arc::new(create_role), ());
                }
                Statement::CreateSchema { schema_name, if_not_exists, .. } => {
                    if let SchemaName::Simple(name) | SchemaName::NamedAuthorization(name, _) =
                        &schema_name
                    {
                        require_named(name, crate::errors::ObjectKind::Schema)?;
                    }
                    let (name, quoted, authorization) = match &schema_name {
                        SchemaName::Simple(name) => {
                            let schema_ident = object_name_last_identifier(name);
                            (
                                schema_ident.map_or_else(
                                    || last_str(name).to_string(),
                                    |ident| ident.value.clone(),
                                ),
                                schema_ident.is_some_and(|ident| ident.quote_style.is_some()),
                                None,
                            )
                        }
                        SchemaName::UnnamedAuthorization(auth) => {
                            // CREATE SCHEMA AUTHORIZATION admin creates schema
                            // named "admin"
                            (
                                auth.value.clone(),
                                auth.quote_style.is_some(),
                                Some(stored_role_name(auth)),
                            )
                        }
                        SchemaName::NamedAuthorization(name, auth) => {
                            let schema_ident = object_name_last_identifier(name);
                            (
                                schema_ident.map_or_else(
                                    || last_str(name).to_string(),
                                    |ident| ident.value.clone(),
                                ),
                                schema_ident.is_some_and(|ident| ident.quote_style.is_some()),
                                Some(stored_role_name(auth)),
                            )
                        }
                    };

                    if access_resolution == AccessResolution::ClosedWorld {
                        let authorization_ident = match &schema_name {
                            SchemaName::Simple(_) => None,
                            SchemaName::UnnamedAuthorization(auth)
                            | SchemaName::NamedAuthorization(_, auth) => Some(auth),
                        };
                        if let Some(auth) = authorization_ident {
                            validate_owner_role_ident(&builder, auth, &name)?;
                        }
                    }

                    // Check if schema already exists
                    let schema_exists = builder
                        .schemas()
                        .iter()
                        .any(|(s, ())| identifiers_match(s.name(), s.is_quoted(), &name, quoted));

                    if schema_exists {
                        if !if_not_exists {
                            return Err(crate::errors::Error::SchemaAlreadyExists {
                                schema_name: name.clone(),
                            });
                        }
                        // IF NOT EXISTS - skip adding duplicate
                    } else {
                        let schema = match authorization {
                            Some(auth) => Schema::with_authorization_and_quoted(name, auth, quoted),
                            None => Schema::with_quoted(name, quoted),
                        };
                        builder = builder.add_schema(Arc::new(schema), ());
                    }
                }
                Statement::Grant(mut grant) => {
                    // Every name the statement carries first, including the
                    // schemas of a blanket grant, since a grant recorded
                    // against a name nothing can read covers nothing and says
                    // so nowhere.
                    require_static_access_names(grant.objects.as_ref())?;
                    if access_resolution == AccessResolution::ClosedWorld {
                        validate_access_targets_against_builder(
                            &builder,
                            &grant.grantees,
                            grant.objects.as_ref(),
                        )?;
                    }

                    // Ungated: a column list is checked whenever its table
                    // resolved, since a dump omits roles and never the table
                    // it grants on.
                    let tables: Vec<&CreateTable> =
                        builder.tables().iter().map(|(table, _)| table.as_ref()).collect();
                    let path: Vec<(&str, bool)> = builder.search_path().collect();
                    crate::impls::validate_granted_columns(
                        &grant.privileges,
                        grant.objects.as_ref(),
                        &tables,
                        &path,
                        builder.identifier_case(),
                        |key| builder.relation_key_is_held(key, builder.identifier_case()),
                    )?;
                    builder.bind_access_targets(&mut grant.objects);

                    builder = builder.add_table_grant(Arc::new(grant.clone()), ());
                    builder = builder.add_column_grant(Arc::new(grant), ());
                }
                Statement::Revoke(mut revoke) => {
                    require_static_access_names(revoke.objects.as_ref())?;
                    // A revoke naming no recorded grant is a no-op, as it is in
                    // the database.
                    if access_resolution == AccessResolution::ClosedWorld {
                        validate_access_targets_against_builder(
                            &builder,
                            &revoke.grantees,
                            revoke.objects.as_ref(),
                        )?;
                    }

                    let tables: Vec<&CreateTable> =
                        builder.tables().iter().map(|(table, _)| table.as_ref()).collect();
                    let path: Vec<(&str, bool)> = builder.search_path().collect();
                    crate::impls::validate_granted_columns(
                        &revoke.privileges,
                        revoke.objects.as_ref(),
                        &tables,
                        &path,
                        builder.identifier_case(),
                        |key| builder.relation_key_is_held(key, builder.identifier_case()),
                    )?;
                    builder.bind_access_targets(&mut revoke.objects);

                    let unsupported =
                        apply_revoke_to_grant_store(builder.table_grants_mut(), &revoke).or_else(
                            || apply_revoke_to_grant_store(builder.column_grants_mut(), &revoke),
                        );

                    if let Some(reason) = unsupported {
                        return Err(crate::errors::Error::UnsupportedRevoke {
                            statement: revoke.to_string(),
                            reason: reason.to_string(),
                        });
                    }
                }
                Statement::Set(sqlparser::ast::Set::SetTimeZone { local, value }) => {
                    if local {
                        builder = builder.timezone("LOCAL".to_string());
                    } else if let Expr::Value(ValueWithSpan {
                        value: Value::SingleQuotedString(lit),
                        ..
                    }) = value
                    {
                        builder = builder.timezone(lit);
                    }
                    // Ignore unsupported SET TIME ZONE expressions (e.g.,
                    // binary ops)
                }
                Statement::Set(sqlparser::ast::Set::SingleAssignment {
                    variable, values, ..
                }) if object_name_last_part(&variable)
                    .is_some_and(|(name, _)| name.eq_ignore_ascii_case("search_path")) =>
                {
                    // `SET` replaces the path rather than extending it, so
                    // `public` stops being reachable unless it is listed.
                    // `TO DEFAULT` restores the starting path, as `RESET` does.
                    let restores_default = matches!(
                        values.as_slice(),
                        [Expr::Identifier(ident)] if ident.quote_style.is_none()
                            && ident.value.eq_ignore_ascii_case("DEFAULT")
                    );
                    let path = if restores_default {
                        ParserDBBuilder::default_search_path()
                    } else {
                        values.iter().filter_map(search_path_entry).collect()
                    };
                    builder.set_search_path(path);
                }
                Statement::Reset(reset) => {
                    if let sqlparser::ast::Reset::ConfigurationParameter(name) = &reset.reset
                        && object_name_last_part(name)
                            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("search_path"))
                    {
                        builder.set_search_path(ParserDBBuilder::default_search_path());
                    }
                }
                Statement::RenameTable(renames) => {
                    for rename in renames {
                        builder = Self::rename_table_checked(
                            builder,
                            &rename.old_name,
                            rename.new_name,
                            false,
                        )?;
                    }
                }
                Statement::AlterFunction(AlterFunction {
                    kind,
                    function: func_desc,
                    operation,
                    ..
                }) => {
                    // Owner, security, and configuration changes update the
                    // stored function. Other operations remain ignored.
                    match operation {
                        AlterFunctionOperation::OwnerTo(new_owner) => {
                            // An aggregate reaches here as this same statement,
                            // and PostgreSQL refuses to reach a function
                            // through it, so a same-named function must not
                            // answer for one.
                            //
                            // TODO: record the aggregate and its owner once
                            // upstream parses `CREATE AGGREGATE`, tracked in
                            // `upstream/sqlparser-create-aggregate.md`. Until
                            // then no aggregate can be in this model, so the
                            // statement is refused rather than dropped.
                            if matches!(kind, AlterFunctionKind::Aggregate) {
                                return Err(crate::errors::Error::AggregateOwnerUnsupported {
                                    aggregate_name: last_str(&func_desc.name).to_string(),
                                });
                            }

                            let position = Self::alter_function_target(&builder, &func_desc)?;

                            // After the function, because the database reports
                            // an absent function first. The keyword owners name
                            // no role, so there is nothing to look for.
                            if access_resolution == AccessResolution::ClosedWorld
                                && let Owner::Ident(ident) = &new_owner
                            {
                                validate_owner_role_ident(
                                    &builder,
                                    ident,
                                    last_str(&func_desc.name),
                                )?;
                            }

                            let owner = match new_owner {
                                Owner::Ident(ident) => Some(stored_role_name(&ident)),
                                // These name whoever runs the statement, so the
                                // owner changed to one the input never spells
                                // and the model can no longer name it either.
                                Owner::CurrentRole | Owner::CurrentUser | Owner::SessionUser => {
                                    None
                                }
                            };
                            builder.functions_mut()[position].1.set_owner(owner);
                        }
                        AlterFunctionOperation::Actions { actions, .. } => {
                            let tracked = actions.iter().any(|action| {
                                matches!(
                                    action,
                                    AlterFunctionAction::Security { .. }
                                        | AlterFunctionAction::Set(_)
                                        | AlterFunctionAction::Reset(_)
                                )
                            });
                            if !tracked {
                                continue;
                            }

                            let position = Self::alter_function_target(&builder, &func_desc)?;
                            let function_arc = &mut builder.functions_mut()[position].0;
                            let stale = Arc::clone(function_arc);
                            let function = Arc::make_mut(function_arc);
                            for action in actions {
                                match action {
                                    AlterFunctionAction::Security { security, .. } => {
                                        function.security = Some(security);
                                    }
                                    AlterFunctionAction::Set(parameter) => {
                                        if let Some(position) =
                                            function.set_params.iter().position(|stored| {
                                                function_configuration_names_match(
                                                    &stored.name,
                                                    &parameter.name,
                                                )
                                            })
                                        {
                                            function.set_params[position] = parameter;
                                        } else {
                                            function.set_params.push(parameter);
                                        }
                                    }
                                    AlterFunctionAction::Reset(ResetConfig::ALL) => {
                                        function.set_params.clear();
                                    }
                                    AlterFunctionAction::Reset(ResetConfig::ConfigName(name)) => {
                                        function.set_params.retain(|stored| {
                                            !function_configuration_names_match(&stored.name, &name)
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            let fresh = Arc::clone(function_arc);

                            // Policies and check constraints cache function
                            // nodes.
                            for (_, metadata) in builder.policies_mut() {
                                metadata.replace_function(&stale, &fresh);
                            }
                            for (_, metadata) in builder.check_constraints_mut() {
                                metadata.replace_function(&stale, &fresh);
                            }
                        }
                        AlterFunctionOperation::RenameTo { .. }
                        | AlterFunctionOperation::SetSchema { .. }
                        | AlterFunctionOperation::DependsOnExtension { .. } => {}
                    }
                }
                Statement::AlterPolicy(AlterPolicy { name, mut table_name, operation }) => {
                    builder.bind_table_reference(&mut table_name);
                    let Some(index) = builder.policies().iter().position(|(policy, _)| {
                        idents_match(&policy.name, &name)
                            && target_tables_match(
                                &policy.table_name,
                                &table_name,
                                IdentifierCase::AsWritten,
                            )
                    }) else {
                        return Err(crate::errors::Error::AlterPolicyNotFound {
                            policy_name: name.value.clone(),
                        });
                    };

                    match operation {
                        AlterPolicyOperation::Rename { new_name } => {
                            Arc::make_mut(&mut builder.policies_mut()[index].0).name = new_name;
                        }
                        AlterPolicyOperation::Apply { to, using, with_check } => {
                            if access_resolution == AccessResolution::ClosedWorld {
                                validate_policy_roles(
                                    &builder,
                                    &name.value,
                                    to.as_deref().unwrap_or_default(),
                                )?;
                            }

                            // The functions an expression calls are resolved
                            // against the builder, so they are collected before
                            // the store is borrowed mutably.
                            let functions = |expression: Option<&Expr>| {
                                expression.map(|expression| {
                                    functions_in_expression::functions_in_expression::<Self>(
                                        expression,
                                        builder.function_arc_vec().as_slice(),
                                    )
                                })
                            };
                            let using_functions = functions(using.as_ref());
                            let check_functions = functions(with_check.as_ref());

                            // PostgreSQL applies each clause on its own, so one
                            // the statement omits is left as it was rather than
                            // cleared.
                            let (policy, metadata) = &mut builder.policies_mut()[index];
                            let policy = Arc::make_mut(policy);
                            if to.is_some() {
                                policy.to = to;
                            }
                            if using.is_some() {
                                policy.using = using;
                            }
                            if with_check.is_some() {
                                policy.with_check = with_check;
                            }
                            if let Some(using_functions) = using_functions {
                                metadata.set_using_functions(using_functions);
                            }
                            if let Some(check_functions) = check_functions {
                                metadata.set_check_functions(check_functions);
                            }
                        }
                    }
                }
                Statement::AlterSchema(AlterSchema { name, if_exists, operations }) => {
                    let schema_name = last_str(&name);

                    // Check if schema exists
                    let resolved_schema = object_name_last_identifier(&name)
                        .and_then(|ident| builder.resolve_schema_ident(ident));

                    let Some(resolved_schema) = resolved_schema else {
                        if if_exists {
                            continue;
                        }
                        return Err(crate::errors::Error::AlterSchemaNotFound {
                            schema_name: schema_name.to_string(),
                        });
                    };

                    let mut current_schema_name = resolved_schema.name().to_string();
                    let mut current_schema_quoted = resolved_schema.is_quoted();

                    for operation in &operations {
                        match operation {
                            AlterSchemaOperation::Rename { name: new_name } => {
                                let new_schema_ident = object_name_last_identifier(new_name);
                                let new_schema_name = new_schema_ident.map_or_else(
                                    || last_str(new_name).to_string(),
                                    |ident| ident.value.clone(),
                                );
                                let new_schema_quoted = new_schema_ident
                                    .is_some_and(|ident| ident.quote_style.is_some());
                                let schemas = builder.schemas_mut();
                                let Some(idx) = schemas.iter().position(|(schema, ())| {
                                    identifiers_match(
                                        schema.name(),
                                        schema.is_quoted(),
                                        &current_schema_name,
                                        current_schema_quoted,
                                    )
                                }) else {
                                    continue;
                                };

                                let duplicate_exists = schemas.iter().enumerate().any(
                                    |(existing_idx, (schema, ()))| {
                                        existing_idx != idx
                                            && identifiers_match(
                                                schema.name(),
                                                schema.is_quoted(),
                                                &new_schema_name,
                                                new_schema_quoted,
                                            )
                                    },
                                );
                                if duplicate_exists {
                                    return Err(crate::errors::Error::SchemaAlreadyExists {
                                        schema_name: new_schema_name.clone(),
                                    });
                                }

                                let (old_schema, ()) = schemas.remove(idx);
                                let new_schema = if let Some(auth) = old_schema.authorization() {
                                    Schema::with_authorization_and_quoted(
                                        new_schema_name.clone(),
                                        auth.to_string(),
                                        new_schema_quoted,
                                    )
                                } else {
                                    Schema::with_quoted(new_schema_name.clone(), new_schema_quoted)
                                };
                                schemas.push((Arc::new(new_schema), ()));
                                schemas.sort_by(|(a, ()), (b, ())| a.name().cmp(b.name()));
                                rename_created_collation_schemas(
                                    &mut collation_metadata,
                                    &current_schema_name,
                                    current_schema_quoted,
                                    &new_schema_name,
                                    new_schema_quoted,
                                );
                                Arc::make_mut(&mut active_postgres_catalog).rename_schema(
                                    &current_schema_name,
                                    current_schema_quoted,
                                    &new_schema_name,
                                    new_schema_quoted,
                                );
                                current_schema_name = new_schema_name;
                                current_schema_quoted = new_schema_quoted;
                            }
                            AlterSchemaOperation::OwnerTo { owner } => {
                                if access_resolution == AccessResolution::ClosedWorld {
                                    validate_owner_role(&builder, owner, &current_schema_name)?;
                                }
                                // Update the authorization
                                let owner_name = match owner {
                                    sqlparser::ast::Owner::Ident(ident) => stored_role_name(ident),
                                    sqlparser::ast::Owner::CurrentRole
                                    | sqlparser::ast::Owner::CurrentUser
                                    | sqlparser::ast::Owner::SessionUser => continue,
                                };
                                let schemas = builder.schemas_mut();
                                let Some(idx) = schemas.iter().position(|(schema, ())| {
                                    identifiers_match(
                                        schema.name(),
                                        schema.is_quoted(),
                                        &current_schema_name,
                                        current_schema_quoted,
                                    )
                                }) else {
                                    continue;
                                };
                                let (old_schema, ()) = schemas.remove(idx);
                                let new_schema = Schema::with_authorization_and_quoted(
                                    old_schema.name().to_string(),
                                    owner_name,
                                    old_schema.is_quoted(),
                                );
                                schemas.push((Arc::new(new_schema), ()));
                            }
                            // Other operations don't affect our schema tracking
                            AlterSchemaOperation::SetDefaultCollate { .. }
                            | AlterSchemaOperation::AddReplica { .. }
                            | AlterSchemaOperation::DropReplica { .. }
                            | AlterSchemaOperation::SetOptionsParens { .. } => {}
                        }
                    }
                }
                _ => {
                    // Statements this model tracks nothing for.
                    //
                    // TODO: `ALTER TRIGGER ... RENAME TO` lands here once
                    // upstream parses it, and discarding it leaves a freed
                    // trigger name looking taken. Needs an arm beside
                    // `AlterIndex`.
                }
            }
        }
        Ok((builder, active_postgres_catalog, collation_metadata))
    }

    /// Same as [`Self::from_statements_with_dialect`] but under caller-chosen
    /// [`ParseOptions`].
    pub(crate) fn from_statements_with_options(
        statements: Vec<Statement>,
        catalog_name: String,
        dialect: SqlparserDialect,
        options: ParseOptions,
    ) -> Result<Self, crate::errors::Error> {
        let ingestor = ParserDBIngestor::with_dialect(catalog_name, dialect, options);
        Ok(ingestor.apply_statements(statements)?.finish())
    }

    /// Parses SQL using the specified dialect.
    ///
    /// The dialect type parameter `D` must implement both `Dialect` and
    /// `Default`. This allows calling the method with turbofish syntax to
    /// specify the dialect.
    ///
    /// # Arguments
    ///
    /// * `sql` - The SQL string to parse.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL cannot be parsed or if there are
    /// validation errors.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};
    ///
    /// // Using GenericDialect
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT PRIMARY KEY);")?;
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .table_name(),
    ///     "users"
    /// );
    ///
    /// // Using PostgreSqlDialect
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE ROLE admin SUPERUSER;")?;
    /// assert!(db.role("admin").unwrap().is_superuser());
    /// # Ok(())
    /// # }
    /// ```
    pub fn parse<D: Dialect + Default + 'static>(sql: &str) -> Result<Self, crate::errors::Error> {
        Self::parse_with_options::<D>(sql, ParseOptions::default())
    }

    /// Same as [`Self::parse`] but under caller-chosen [`ParseOptions`].
    pub(crate) fn parse_with_options<D: Dialect + Default + 'static>(
        sql: &str,
        options: ParseOptions,
    ) -> Result<Self, crate::errors::Error> {
        let dialect = D::default();
        let mut parser = Parser::new(&dialect).try_with_sql(sql)?;
        let statements = parser.parse_statements()?;
        let mut db = Self::from_statements_with_options(
            statements,
            "unknown_catalog".to_string(),
            SqlparserDialect::of::<D>(),
            options,
        )?;

        if let Ok(documentation) = SqlDoc::builder_from_str(sql).build::<D>() {
            for (table, metadata) in db.tables_metadata_mut() {
                if let Ok(table_doc) = documentation.table(table.table_name(), table.table_schema())
                {
                    metadata.set_doc(table_doc.to_owned());
                }
            }
        }
        Ok(db)
    }

    /// Constructs a `ParserDB` from a git URL.
    ///
    /// # Example
    ///
    /// ```
    /// use sql_traits::prelude::ParserDB;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let url = "https://github.com/earth-metabolome-initiative/asset-procedure-schema.git";
    /// let db = ParserDB::from_git_url::<PostgreSqlDialect>(url).unwrap();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the repository cannot be cloned or if the SQL files
    /// cannot be parsed.
    #[cfg(feature = "git")]
    pub fn from_git_url<D: Dialect + Default>(url: &str) -> Result<Self, crate::errors::Error> {
        let dir = tempfile::tempdir()?;
        Repository::clone(url, dir.path())?;
        Self::from_path::<D>(dir.path())
    }

    /// Constructs a `ParserDB` from a git URL using a specific dialect.
    ///
    /// # Errors
    ///
    /// Returns an error if the repository cannot be cloned or if the SQL files
    /// cannot be parsed.
    #[cfg(feature = "git")]
    pub fn from_git_url_with_dialect<D: Dialect + Default>(
        url: &str,
    ) -> Result<Self, crate::errors::Error> {
        let dir = tempfile::tempdir()?;
        Repository::clone(url, dir.path())?;
        Self::from_path::<D>(dir.path())
    }

    /// Parses SQL from a file or directory path.
    ///
    /// If the path is a directory, all `.sql` files (except `down.sql`) will be
    /// parsed recursively.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to a SQL file or directory containing SQL files.
    /// * `dialect` - The SQL dialect to use for parsing.
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist, files can't be read, or
    /// parsing fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::path::Path;
    ///
    /// use sql_traits::prelude::ParserDB;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::from_path::<PostgreSqlDialect>(Path::new("migrations/")).unwrap();
    /// ```
    #[cfg(feature = "std")]
    pub fn from_path<D: Dialect + Default>(path: &Path) -> Result<Self, crate::errors::Error> {
        Self::from_paths::<D>(&[path])
    }

    /// Parses SQL from multiple file or directory paths.
    ///
    /// # Arguments
    ///
    /// * `paths` - A slice of paths to SQL files or directories.
    /// * `dialect` - The SQL dialect to use for parsing.
    ///
    /// # Errors
    ///
    /// Returns an error if any path doesn't exist, files can't be read, or
    /// parsing fails.
    #[cfg(feature = "std")]
    pub fn from_paths<D: Dialect + Default>(paths: &[&Path]) -> Result<Self, crate::errors::Error> {
        Self::from_paths_with_options::<D>(paths, ParseOptions::default())
    }

    /// Same as [`Self::from_paths`] but under caller-chosen [`ParseOptions`].
    #[cfg(feature = "std")]
    pub(crate) fn from_paths_with_options<D: Dialect + Default>(
        paths: &[&Path],
        options: ParseOptions,
    ) -> Result<Self, crate::errors::Error> {
        let mut statements = Vec::new();
        let mut sql_str: Vec<(String, PathBuf)> = Vec::new();

        for path in paths {
            if !path.exists() {
                return Err(ParserError::TokenizerError(format!(
                    "Path does not exist: {}",
                    path.display()
                ))
                .into());
            }

            let mut sql_paths = search_sql_documents(path);
            sql_paths.sort_unstable();

            for sql_path in sql_paths {
                let sql_content = std::fs::read_to_string(&sql_path)
                    .map_err(|e| ParserError::TokenizerError(e.to_string()))
                    .map_err(|e| {
                        crate::errors::Error::SqlParserError {
                            error: e,
                            file: Some(sql_path.clone()),
                        }
                    })?;

                let dialect = D::default();
                let mut parser = Parser::new(&dialect).try_with_sql(&sql_content).map_err(|e| {
                    crate::errors::Error::SqlParserError { error: e, file: Some(sql_path.clone()) }
                })?;
                statements.extend(parser.parse_statements().map_err(|e| {
                    crate::errors::Error::SqlParserError { error: e, file: Some(sql_path.clone()) }
                })?);
                sql_str.push((sql_content, sql_path));
            }
        }

        let mut db = Self::from_statements_with_options(
            statements,
            "unknown_catalog".to_string(),
            SqlparserDialect::of::<D>(),
            options,
        )?;

        if let Ok(documentation) = SqlDoc::builder_from_strs_with_paths(&sql_str).build::<D>() {
            for (table, metadata) in db.tables_metadata_mut() {
                if let Ok(table_doc) = documentation.table(table.table_name(), table.table_schema())
                {
                    metadata.set_doc(table_doc.to_owned());
                }
            }
        }
        Ok(db)
    }
}

#[cfg(feature = "std")]
fn search_sql_documents(path: &Path) -> Vec<PathBuf> {
    let mut sql_files = Vec::new();
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return sql_files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sql_files.extend(search_sql_documents(&path));
            } else if let Some(extension) = path.extension()
                && extension == "sql"
                && path.file_name().is_some_and(|name| name != "down.sql")
            {
                sql_files.push(path);
            }
        }
    } else if let Some(extension) = path.extension()
        && extension == "sql"
    {
        sql_files.push(path.to_path_buf());
    }
    sql_files
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::{
        errors::{Error, LookupError},
        structs::{IdentifierCase, TargetName},
        traits::{DatabaseLike, TableLike},
    };

    /// A collation or type name built while the statement runs resolves to
    /// nothing, and is not walked along the search path as though it were
    /// unqualified.
    mod run_time_names {
        use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction};

        use super::*;

        fn run_time_qualified(name: &str) -> ObjectName {
            ObjectName(vec![
                ObjectNamePart::Function(ObjectNamePartFunction {
                    name: Ident::new("IDENTIFIER"),
                    args: Vec::new(),
                }),
                ObjectNamePart::Identifier(Ident::new(name)),
            ])
        }

        fn qualified(schema: &str, name: &str) -> ObjectName {
            ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new(schema)),
                ObjectNamePart::Identifier(Ident::new(name)),
            ])
        }

        /// Every form a grant or a revoke can name objects in is guarded, so
        /// no form quietly records a grant against a name nothing can read.
        /// A form nobody parses today still reaches the recorder through
        /// another dialect.
        #[test]
        fn every_access_form_refuses_a_run_time_name() {
            let names = alloc::vec![run_time_qualified("c")];
            let name = run_time_qualified("c");
            let forms = alloc::vec![
                GrantObjects::AllSequencesInSchema { schemas: names.clone() },
                GrantObjects::AllTablesInSchema { schemas: names.clone() },
                GrantObjects::AllViewsInSchema { schemas: names.clone() },
                GrantObjects::AllMaterializedViewsInSchema { schemas: names.clone() },
                GrantObjects::AllExternalTablesInSchema { schemas: names.clone() },
                GrantObjects::AllFunctionsInSchema { schemas: names.clone() },
                GrantObjects::FutureTablesInSchema { schemas: names.clone() },
                GrantObjects::FutureViewsInSchema { schemas: names.clone() },
                GrantObjects::FutureExternalTablesInSchema { schemas: names.clone() },
                GrantObjects::FutureMaterializedViewsInSchema { schemas: names.clone() },
                GrantObjects::FutureSequencesInSchema { schemas: names.clone() },
                GrantObjects::FutureSchemasInDatabase { databases: names.clone() },
                GrantObjects::Databases(names.clone()),
                GrantObjects::Schemas(names.clone()),
                GrantObjects::Sequences(names.clone()),
                GrantObjects::Tables(names.clone()),
                GrantObjects::Views(names.clone()),
                GrantObjects::Warehouses(names.clone()),
                GrantObjects::Integrations(names.clone()),
                GrantObjects::ResourceMonitors(names.clone()),
                GrantObjects::Users(names.clone()),
                GrantObjects::ComputePools(names.clone()),
                GrantObjects::Connections(names.clone()),
                GrantObjects::FailoverGroup(names.clone()),
                GrantObjects::ReplicationGroup(names.clone()),
                GrantObjects::ExternalVolumes(names.clone()),
                GrantObjects::Procedure { name: name.clone(), arg_types: Vec::new() },
                GrantObjects::Function { name, arg_types: Vec::new() },
            ];

            for form in &forms {
                assert!(
                    require_static_access_names(Some(form)).is_err(),
                    "{form} recorded a name built at run time"
                );
            }

            // A statement naming no objects has nothing to refuse.
            assert!(require_static_access_names(None).is_ok());
            assert!(
                require_static_access_names(Some(&GrantObjects::Tables(alloc::vec![qualified(
                    "app", "docs"
                )])))
                .is_ok()
            );
        }

        /// A configuration parameter whose name is built at run time is not
        /// the same parameter as another such name, nor as a parameter called
        /// after the call that builds it.
        #[test]
        fn a_run_time_configuration_name_matches_nothing() {
            let run_time = ObjectName(vec![ObjectNamePart::Function(ObjectNamePartFunction {
                name: Ident::new("IDENTIFIER"),
                args: Vec::new(),
            })]);
            let producer_name =
                ObjectName(vec![ObjectNamePart::Identifier(Ident::new("IDENTIFIER"))]);
            let plain = ObjectName(vec![ObjectNamePart::Identifier(Ident::new("search_path"))]);

            assert!(function_configuration_names_match(&plain, &plain));
            assert!(!function_configuration_names_match(&run_time, &run_time));
            assert!(!function_configuration_names_match(&run_time, &producer_name));
            assert!(!function_configuration_names_match(&producer_name, &run_time));
        }

        #[test]
        fn a_run_time_qualifier_matches_no_collation_schema() {
            assert!(collation_schema_matches(&qualified("app", "c"), "app", false));
            assert!(!collation_schema_matches(&run_time_qualified("c"), "app", false));
            assert!(!collation_schema_matches(&run_time_qualified("c"), "public", false));
        }

        #[test]
        fn a_schema_rename_leaves_a_run_time_collation_name_alone() {
            let mut collations = vec![
                CreatedCollationMetadata {
                    name: qualified("app", "c"),
                    postgres_deterministic: None,
                },
                CreatedCollationMetadata {
                    name: run_time_qualified("c"),
                    postgres_deterministic: None,
                },
                CreatedCollationMetadata {
                    name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("bare"))]),
                    postgres_deterministic: None,
                },
            ];

            rename_created_collation_schemas(&mut collations, "app", false, "renamed", false);
            assert_eq!(collations[0].name.to_string(), "renamed.c");
            // Which schema this one will carry is unknown, so no rename of a
            // schema may claim it, and none may be inserted into it either.
            assert_eq!(collations[1].name.to_string(), "IDENTIFIER().c");

            rename_created_collation_schemas(&mut collations, "public", false, "moved", false);
            assert_eq!(collations[1].name.to_string(), "IDENTIFIER().c");
            assert_eq!(collations[2].name.to_string(), "moved.bare");
        }

        #[test]
        fn a_run_time_qualifier_resolves_to_no_collation() {
            let catalog = PostgresCatalog::default();
            let path = [("public".to_string(), false)];

            assert!(
                collation_resolution_for_name(
                    &qualified("pg_catalog", "\"en-US-x-icu\""),
                    &[],
                    &catalog,
                    &path,
                )
                .is_none(),
                "the quoted spelling is part of the stored name, so this one misses"
            );
            assert!(
                collation_resolution_for_name(&run_time_qualified("c"), &[], &catalog, &path)
                    .is_none()
            );
        }

        #[test]
        fn a_run_time_qualifier_resolves_to_no_catalog_type() {
            let catalog = PostgresCatalog::default();
            let path = [("public".to_string(), false)];

            assert!(
                catalog_type_for_name(&qualified("pg_catalog", "text"), &catalog, &path).is_some()
            );
            assert!(catalog_type_for_name(&qualified("app", "text"), &catalog, &path).is_none());
            assert!(catalog_type_for_name(&run_time_qualified("text"), &catalog, &path).is_none());
        }
    }

    mod ingestion_resumption {
        use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

        use super::*;
        use crate::traits::{ColumnCollation, ColumnLike};

        /// A builder-constructed PostgreSQL database resumes under the
        /// profile's PostgreSQL defaults, so an ICU collation the server
        /// ships resolves after resumption.
        #[test]
        fn builder_constructed_postgres_database_resumes_with_default_catalog() {
            let database: ParserDB =
                ParserDB::new("test".to_owned(), SqlparserDialect::PostgreSql).into();

            let statements = Parser::parse_sql(
                &PostgreSqlDialect {},
                "CREATE TABLE t (name TEXT COLLATE \"en-US-x-icu\");",
            )
            .expect("SQL parses");
            let input = statements
                .into_iter()
                .try_fold(database.into_ingestor(), ParserDBIngestor::apply_statement)
                .expect("PostgreSQL default catalog resolves an ICU collation after resumption");

            let database = input.finish();
            let table = database
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table exists");
            let column = table
                .column("name", &database, IdentifierCase::AsWritten)
                .expect("column lookup runs")
                .expect("column exists");
            let ColumnCollation::Named(collation) =
                column.collation(&database).expect("collation metadata resolves")
            else {
                panic!("expected a named collation")
            };
            assert_eq!(collation.name().name(), "en-US-x-icu");
        }

        /// The debug view names the catalog's size rather than dumping its
        /// hundreds of collations, and a non-PostgreSQL dialect starts from
        /// an empty one.
        #[test]
        fn database_debug_summarizes_ingestion_state() {
            let database: ParserDB =
                ParserDB::new("test".to_owned(), SqlparserDialect::PostgreSql).into();
            let debug = alloc::format!("{database:?}");
            assert!(debug.contains("access_resolution"));
            assert!(
                !debug.contains("en-US-x-icu"),
                "debug output must summarize the catalog, not dump it"
            );

            let database: ParserDB =
                ParserDB::new("test".to_owned(), SqlparserDialect::Generic).into();
            let debug = alloc::format!("{database:?}");
            assert!(debug.contains("catalog_collations: 0"));
        }
    }

    mod identifier_aware_lookup {
        use sqlparser::{
            ast::{Ident, ObjectName, ObjectNamePart},
            dialect::PostgreSqlDialect,
        };

        use super::*;

        fn ident(value: &str, quoted: bool) -> Ident {
            if quoted { Ident::with_quote('"', value) } else { Ident::new(value) }
        }

        fn object_name(parts: &[(&str, bool)]) -> ObjectName {
            ObjectName(
                parts
                    .iter()
                    .map(|(value, quoted)| ObjectNamePart::Identifier(ident(value, *quoted)))
                    .collect(),
            )
        }

        fn parse_postgres(sql: &str) -> ParserDB {
            ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse PostgreSQL SQL")
        }

        #[test]
        fn quoted_table_lookup_requires_exact_case() {
            let db = parse_postgres("CREATE TABLE \"Camel\" (id INT);");

            assert!(
                db.resolve_table_object_name(&object_name(&[("Camel", true)]))
                    .expect("Lookup should succeed")
                    .is_some()
            );
            assert!(
                db.resolve_table_object_name(&object_name(&[("camel", true)]))
                    .expect("Lookup should succeed")
                    .is_none()
            );
            assert!(
                db.resolve_table_object_name(&object_name(&[("camel", false)]))
                    .expect("Lookup should succeed")
                    .is_none()
            );
        }

        #[test]
        fn unquoted_table_lookup_resolves_via_folding() {
            let db = parse_postgres("CREATE TABLE Foo (id INT);");

            assert!(
                db.resolve_table_object_name(&object_name(&[("foo", false)]))
                    .expect("Lookup should succeed")
                    .is_some()
            );
            assert!(
                db.resolve_table_object_name(&object_name(&[("FOO", false)]))
                    .expect("Lookup should succeed")
                    .is_some()
            );
            assert!(
                db.resolve_table_object_name(&object_name(&[("foo", true)]))
                    .expect("Lookup should succeed")
                    .is_some()
            );
            assert!(
                db.resolve_table_object_name(&object_name(&[("Foo", true)]))
                    .expect("Lookup should succeed")
                    .is_none()
            );
        }

        #[test]
        fn schema_ident_resolution_handles_quoted_and_unquoted() {
            let db = parse_postgres(
                r#"
                CREATE SCHEMA Foo;
                CREATE SCHEMA "Bar";
                "#,
            );

            assert!(db.resolve_schema_ident(&ident("foo", false)).is_some());
            assert!(db.resolve_schema_ident(&ident("FOO", false)).is_some());
            assert!(db.resolve_schema_ident(&ident("foo", true)).is_some());
            assert!(db.resolve_schema_ident(&ident("Foo", true)).is_none());

            assert!(db.resolve_schema_ident(&ident("Bar", true)).is_some());
            assert!(db.resolve_schema_ident(&ident("bar", false)).is_none());
        }

        #[test]
        fn alter_table_rls_lookup_uses_resolver_rules() {
            let db = parse_postgres(
                r"
                CREATE TABLE Foo (id INT);
                ALTER TABLE FOO ENABLE ROW LEVEL SECURITY;
                ",
            );
            let foo = db
                .table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Expected `foo` table to exist after ALTER TABLE");
            assert!(
                foo.has_row_level_security(&db).expect("rls check"),
                "Unquoted ALTER TABLE lookup should resolve via identifier folding"
            );

            let result = ParserDB::parse::<PostgreSqlDialect>(
                r#"
                CREATE TABLE Foo (id INT);
                ALTER TABLE "Foo" ENABLE ROW LEVEL SECURITY;
                "#,
            );
            assert!(
                matches!(
                    result,
                    Err(Error::AlterTableNotFound { table_name }) if table_name == "Foo"
                ),
                "Quoted ALTER TABLE name should not match unquoted table with different case"
            );
        }

        #[test]
        fn grant_table_lookup_uses_resolver_rules() {
            let sql = r"
                CREATE TABLE Foo (id INT);
                CREATE ROLE app_role;
                GRANT SELECT ON FOO TO app_role;
            ";
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(result.is_ok());

            let sql = r#"
                CREATE TABLE Foo (id INT);
                CREATE ROLE app_role;
                GRANT SELECT ON "Foo" TO app_role;
            "#;
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::TableNotFoundForGrant { table_name }) if table_name == "Foo"
            ));
        }

        #[test]
        fn create_index_attaches_to_correct_schema_table() {
            let db = parse_postgres(
                r"
                CREATE SCHEMA s1;
                CREATE SCHEMA s2;
                CREATE TABLE s1.t (id INT);
                CREATE TABLE s2.t (id INT);
                CREATE INDEX idx_s2_t_id ON s2.t (id);
                ",
            );

            let s1_t = db
                .resolve_table_object_name(&object_name(&[("s1", false), ("t", false)]))
                .expect("Lookup should succeed")
                .expect("Expected table s1.t to exist");
            let s2_t = db
                .resolve_table_object_name(&object_name(&[("s2", false), ("t", false)]))
                .expect("Lookup should succeed")
                .expect("Expected table s2.t to exist");

            assert_eq!(s1_t.indices(&db).expect("indices").count(), 0);
            assert_eq!(s2_t.indices(&db).expect("indices").count(), 1);
        }

        #[test]
        fn create_index_attachment_respects_quoted_schema_and_table_identity() {
            let db = parse_postgres(
                r#"
                CREATE SCHEMA s;
                CREATE SCHEMA "S";
                CREATE TABLE s.t (id INT);
                CREATE TABLE "S"."T" (id INT);
                CREATE INDEX idx_quoted_t ON "S"."T" (id);
                "#,
            );

            let unquoted = db
                .resolve_table_object_name(&object_name(&[("s", false), ("t", false)]))
                .expect("Lookup should succeed")
                .expect("Expected table s.t to exist");
            let quoted = db
                .resolve_table_object_name(&object_name(&[("S", true), ("T", true)]))
                .expect("Lookup should succeed")
                .expect("Expected table \"S\".\"T\" to exist");

            assert_eq!(unquoted.indices(&db).expect("indices").count(), 0);
            assert_eq!(quoted.indices(&db).expect("indices").count(), 1);
        }

        #[test]
        fn rename_table_lookup_uses_resolver_rules() {
            let sql = r"
                CREATE TABLE Foo (id INT);
                ALTER TABLE FOO RENAME TO bar;
            ";
            let db = parse_postgres(sql);
            assert!(
                db.table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("bar", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            let sql = r#"
                CREATE TABLE Foo (id INT);
                ALTER TABLE "Foo" RENAME TO bar;
            "#;
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::RenameTableNotFound { table_name }) if table_name == "Foo"
            ));
        }

        #[test]
        fn rename_table_statement_lookup_uses_resolver_rules() {
            let sql = r"
                CREATE TABLE Foo (id INT);
                RENAME TABLE FOO TO bar;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql)
                .expect("Expected unquoted RENAME TABLE lookup to resolve");
            assert!(
                db.table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("bar", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            let sql = r#"
                CREATE TABLE Foo (id INT);
                RENAME TABLE "Foo" TO bar;
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::RenameTableNotFound { table_name }) if table_name == "Foo"
            ));
        }

        #[test]
        fn alter_schema_rename_rejects_semantic_duplicate() {
            let sql = r"
                CREATE SCHEMA foo;
                CREATE SCHEMA bar;
                ALTER SCHEMA bar RENAME TO FOO;
            ";

            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::SchemaAlreadyExists { schema_name })
                    if schema_name.eq_ignore_ascii_case("foo")
            ));
        }

        #[test]
        fn alter_schema_rename_rejects_quoted_unquoted_equivalent_duplicate() {
            let sql = r#"
                CREATE SCHEMA foo;
                CREATE SCHEMA bar;
                ALTER SCHEMA bar RENAME TO "foo";
            "#;

            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::SchemaAlreadyExists { schema_name }) if schema_name == "foo"
            ));
        }

        #[test]
        fn implicit_public_helper_handles_mixed_public_cases() {
            let db = parse_postgres(
                r#"
                CREATE TABLE public.foo (id INT);
                CREATE SCHEMA "Public";
                CREATE TABLE "Public".bar (id INT);
                "#,
            );

            let strict = db
                .resolve_table_object_name(&object_name(&[("foo", false)]))
                .expect("Lookup should succeed")
                .expect("a bare name reaches the table stored in public");
            assert_eq!(strict.table_schema(), Some("public"));

            let resolved = db
                .resolve_table_object_name_on_search_path(&object_name(&[("foo", false)]))
                .expect("Lookup should succeed");
            let resolved = resolved.expect("Expected implicit public fallback to resolve");
            assert_eq!(
                resolved.table_schema(),
                Some("public"),
                "Unqualified lookup should fallback to schema public"
            );

            assert!(
                db.resolve_table_object_name_on_search_path(&object_name(&[("bar", false)]))
                    .expect("Lookup should succeed")
                    .is_none()
            );

            assert!(
                db.resolve_table_object_name(&object_name(&[("Public", true), ("bar", false)]))
                    .expect("Lookup should succeed")
                    .is_some()
            );
        }

        #[test]
        fn invalid_object_name_is_reported() {
            let db = parse_postgres("CREATE TABLE t (id INT);");
            let invalid = object_name(&[("a", false), ("b", false), ("c", false)]);

            let result = db.resolve_table_object_name(&invalid);
            assert!(matches!(
                result,
                Err(LookupError::InvalidObjectName { object_name, .. }) if object_name == "a.b.c"
            ));
        }

        #[test]
        fn ambiguous_unqualified_and_public_tables_fail_at_build_time() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE TABLE public.t (id INT);
            ";

            let result = ParserDB::parse::<PostgreSqlDialect>(sql);
            assert!(matches!(
                result,
                Err(Error::IdentifierLookupError(LookupError::TableLookupConflict {
                    table,
                    conflicting_table,
                    ..
                })) if table == "public.t" && conflicting_table == "t"
            ));
        }
    }

    mod unnamed_object {
        use sqlparser::{
            ast::{ObjectName, SchemaName, Statement},
            dialect::{GenericDialect, PostgreSqlDialect},
            parser::Parser,
        };

        use super::*;
        use crate::errors::Error;

        fn empty_name() -> ObjectName {
            ObjectName(vec![])
        }

        fn parse_generic(sql: &str) -> Statement {
            Parser::parse_sql(&GenericDialect {}, sql).expect("parse")[0].clone()
        }

        fn parse_postgres(sql: &str) -> Statement {
            Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("parse")[0].clone()
        }

        fn apply(stmt: Statement) -> Result<ParserDB, Error> {
            ParserDB::from_statements(vec![stmt], "cat".into())
        }

        #[test]
        fn create_table_with_empty_name_returns_unnamed_object() {
            let Statement::CreateTable(mut ct) = parse_generic("CREATE TABLE t (id INT);") else {
                panic!("expected CreateTable")
            };
            ct.name = empty_name();
            assert!(matches!(apply(Statement::CreateTable(ct)), Err(Error::UnnamedObject { .. })));
        }

        #[test]
        fn create_function_with_empty_name_returns_unnamed_object() {
            let Statement::CreateFunction(mut cf) =
                parse_postgres("CREATE FUNCTION f() RETURNS void LANGUAGE sql AS $$ SELECT 1 $$;")
            else {
                panic!("expected CreateFunction")
            };
            cf.name = empty_name();
            assert!(matches!(
                apply(Statement::CreateFunction(cf)),
                Err(Error::UnnamedObject { .. })
            ));
        }

        #[test]
        fn create_trigger_with_empty_name_returns_unnamed_object() {
            let Statement::CreateTrigger(mut ct) =
                parse_postgres("CREATE TRIGGER tr BEFORE INSERT ON t EXECUTE FUNCTION f();")
            else {
                panic!("expected CreateTrigger")
            };
            ct.name = empty_name();
            assert!(matches!(
                apply(Statement::CreateTrigger(ct)),
                Err(Error::UnnamedObject { .. })
            ));
        }

        #[test]
        fn create_policy_with_empty_table_name_returns_unnamed_object() {
            let Statement::CreatePolicy(mut cp) = parse_postgres("CREATE POLICY pol ON t;") else {
                panic!("expected CreatePolicy")
            };
            cp.table_name = empty_name();
            assert!(matches!(apply(Statement::CreatePolicy(cp)), Err(Error::UnnamedObject { .. })));
        }

        #[test]
        fn create_role_with_empty_name_returns_unnamed_object() {
            let Statement::CreateRole(mut cr) = parse_postgres("CREATE ROLE r;") else {
                panic!("expected CreateRole")
            };
            cr.names = vec![empty_name()];
            assert!(matches!(apply(Statement::CreateRole(cr)), Err(Error::UnnamedObject { .. })));
        }

        #[test]
        fn create_schema_with_empty_name_returns_unnamed_object() {
            let mut stmt = parse_postgres("CREATE SCHEMA s;");
            let Statement::CreateSchema { ref mut schema_name, .. } = stmt else {
                panic!("expected CreateSchema")
            };
            *schema_name = SchemaName::Simple(empty_name());
            assert!(matches!(apply(stmt), Err(Error::UnnamedObject { .. })));
        }

        #[test]
        fn create_index_with_empty_name_returns_unnamed_object() {
            let Statement::CreateIndex(mut ci) = parse_generic("CREATE INDEX idx ON t (id);")
            else {
                panic!("expected CreateIndex")
            };
            ci.name = Some(empty_name());
            assert!(matches!(apply(Statement::CreateIndex(ci)), Err(Error::UnnamedObject { .. })));
        }
    }

    mod parser_variant_compatibility {
        use sqlparser::dialect::{MsSqlDialect, PostgreSqlDialect};

        use super::*;

        #[test]
        fn waitfor_statement_is_ignored_without_breaking_parse() {
            let sql = "
                WAITFOR DELAY '00:00:00';
                CREATE TABLE t (id INT);
            ";
            let db = ParserDB::parse::<MsSqlDialect>(sql).expect("WAITFOR should be ignored");
            assert!(
                db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn comment_on_role_statement_is_ignored_without_breaking_parse() {
            let sql = "
                CREATE ROLE app_role;
                COMMENT ON ROLE app_role IS 'Application role';
            ";
            let db =
                ParserDB::parse::<PostgreSqlDialect>(sql).expect("COMMENT ON ROLE should parse");
            assert!(db.role("app_role").is_some());
        }
    }

    mod drop_function_errors {
        use super::*;

        #[test]
        fn test_drop_function_not_found_error_type() {
            let sql = "DROP FUNCTION nonexistent_func;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropFunctionNotFound { function_name }) if function_name == "nonexistent_func"
            ));
        }

        #[test]
        fn test_drop_function_referenced_error_type() {
            let sql = r"
                CREATE FUNCTION is_valid(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
                CREATE TABLE t (id INT CHECK (is_valid(id)));
                DROP FUNCTION is_valid;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::FunctionReferenced { function_name }) if function_name == "is_valid"
            ));
        }

        #[test]
        fn test_drop_function_if_exists_not_found_succeeds() {
            let sql = "DROP FUNCTION IF EXISTS nonexistent_func;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_ok());
        }

        #[test]
        fn test_drop_function_if_exists_referenced_still_fails() {
            let sql = r"
                CREATE FUNCTION is_valid(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
                CREATE TABLE t (id INT CHECK (is_valid(id)));
                DROP FUNCTION IF EXISTS is_valid;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            // IF EXISTS doesn't bypass the reference check
            assert!(matches!(
                result,
                Err(Error::FunctionReferenced { function_name }) if function_name == "is_valid"
            ));
        }

        #[test]
        fn test_drop_quoted_function_succeeds() {
            let sql = r#"
                CREATE FUNCTION "FooBar"() RETURNS INT AS 'SELECT 1;';
                DROP FUNCTION "FooBar";
            "#;
            let db = ParserDB::parse::<GenericDialect>(sql)
                .expect("Quoted DROP FUNCTION should match quoted CREATE FUNCTION");

            assert!(
                db.function_by_target(TargetName::new("FooBar", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_drop_unquoted_does_not_match_quoted_function() {
            let sql = r#"
                CREATE FUNCTION "FooBar"() RETURNS INT AS 'SELECT 1;';
                DROP FUNCTION foobar;
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropFunctionNotFound { function_name }) if function_name == "foobar"
            ));
        }
    }

    mod drop_table_errors {
        use super::*;

        #[test]
        fn test_drop_table_not_found_error_type() {
            let sql = "DROP TABLE nonexistent_table;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropTableNotFound { table_name }) if table_name == "nonexistent_table"
            ));
        }

        #[test]
        fn test_drop_table_referenced_error_type() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE parent;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::TableReferenced { table_name }) if table_name == "parent"
            ));
        }

        #[test]
        fn test_drop_table_if_exists_not_found_succeeds() {
            let sql = "DROP TABLE IF EXISTS nonexistent_table;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_ok());
        }

        #[test]
        fn test_drop_table_if_exists_referenced_still_fails() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE IF EXISTS parent;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            // IF EXISTS doesn't bypass the reference check
            assert!(matches!(
                result,
                Err(Error::TableReferenced { table_name }) if table_name == "parent"
            ));
        }

        #[test]
        fn test_drop_table_cascade_bypasses_reference_check() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE parent CASCADE;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_ok());
            let db = result.unwrap();
            assert!(
                db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_multiple_tables() {
            let sql = r"
                CREATE TABLE t1 (id INT PRIMARY KEY);
                CREATE TABLE t2 (id INT PRIMARY KEY);
                CREATE TABLE t3 (id INT PRIMARY KEY);
                DROP TABLE t1, t2;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_ok());
            let db = result.unwrap();
            assert!(
                db.table_by_target(TargetName::new("t1", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("t3", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_multiple_tables_one_referenced_fails() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                CREATE TABLE other (id INT PRIMARY KEY);
                DROP TABLE parent, other;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            // Should fail because parent is referenced
            assert!(matches!(
                result,
                Err(Error::TableReferenced { table_name }) if table_name == "parent"
            ));
        }

        #[test]
        fn test_drop_table_with_same_name_in_other_schema_only_removes_target() {
            let sql = r"
                CREATE SCHEMA s1;
                CREATE SCHEMA s2;
                CREATE TABLE s1.t (id INT PRIMARY KEY);
                CREATE TABLE s2.t (id INT PRIMARY KEY);
                CREATE INDEX idx_s2_t_id ON s2.t (id);
                DROP TABLE s1.t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert!(
                db.table_by_target(
                    TargetName::new("t", false).with_schema("s1", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_none()
            );
            let s2_t = db
                .table_by_target(
                    TargetName::new("t", false).with_schema("s2", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("s2.t should still exist");
            assert_eq!(
                s2_t.indices(&db).expect("indices").count(),
                1,
                "Dropping s1.t must not remove indices attached to s2.t",
            );
        }

        #[test]
        fn test_drop_table_reference_check_is_schema_aware() {
            let sql = r"
                CREATE SCHEMA s1;
                CREATE SCHEMA s2;
                CREATE TABLE s1.parent (id INT PRIMARY KEY);
                CREATE TABLE s1.child (id INT, parent_id INT REFERENCES s1.parent(id));
                CREATE TABLE s2.parent (id INT PRIMARY KEY);
                DROP TABLE s2.parent;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect(
                "References to s1.parent must not block dropping same-name table s2.parent",
            );

            assert!(
                db.table_by_target(
                    TargetName::new("parent", false).with_schema("s2", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_none()
            );
            assert!(
                db.table_by_target(
                    TargetName::new("parent", false).with_schema("s1", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );
            assert!(
                db.table_by_target(
                    TargetName::new("child", false).with_schema("s1", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );
        }
    }

    mod is_function_used_tests {
        use super::*;

        #[test]
        fn test_is_function_used_returns_false_when_no_references() {
            // Parse SQL with a function but no references
            let sql = r"
                CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
                CREATE TABLE t (id INT);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // The function exists but isn't used by any schema object
            assert!(
                db.function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_function_used_by_check_constraint() {
            let sql = r"
                CREATE FUNCTION is_positive(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
                CREATE TABLE t (id INT CHECK (is_positive(id)));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Function should exist and be used
            assert!(
                db.function_by_target(
                    TargetName::new("is_positive", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );

            // Verify dropping it would fail
            let drop_sql = format!("{sql}\nDROP FUNCTION is_positive;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
        }

        #[test]
        fn test_function_used_by_policy_using_clause() {
            let sql = r"
                CREATE FUNCTION check_access() RETURNS BOOLEAN AS 'SELECT true;';
                CREATE TABLE t (id INT);
                CREATE POLICY my_policy ON t USING (check_access());
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Function should exist
            assert!(
                db.function_by_target(
                    TargetName::new("check_access", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );

            // Verify dropping it would fail
            let drop_sql = format!("{sql}\nDROP FUNCTION check_access;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
        }

        #[test]
        fn test_function_used_by_policy_with_check_clause() {
            let sql = r"
                CREATE FUNCTION validate() RETURNS BOOLEAN AS 'SELECT true;';
                CREATE TABLE t (id INT);
                CREATE POLICY my_policy ON t WITH CHECK (validate());
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Function should exist
            assert!(
                db.function_by_target(
                    TargetName::new("validate", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );

            // Verify dropping it would fail
            let drop_sql = format!("{sql}\nDROP FUNCTION validate;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
        }

        #[test]
        fn test_function_used_by_trigger() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION trigger_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Function should exist
            assert!(
                db.function_by_target(
                    TargetName::new("trigger_fn", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );

            // Verify dropping it would fail
            let drop_sql = format!("{sql}\nDROP FUNCTION trigger_fn;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
        }

        #[test]
        fn test_function_used_by_schema_qualified_call() {
            let sql = r"
                CREATE SCHEMA s;
                CREATE FUNCTION check_access() RETURNS BOOLEAN AS 'SELECT true;';
                CREATE TABLE t (id INT CHECK (s.check_access()));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            assert!(
                db.function_by_target(
                    TargetName::new("check_access", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );

            let drop_sql = format!("{sql}\nDROP FUNCTION check_access;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
        }

        #[test]
        fn test_quoted_function_used_by_check_constraint() {
            let sql = r#"
                CREATE FUNCTION "FooBar"(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
                CREATE TABLE t (id INT CHECK ("FooBar"(id)));
                DROP FUNCTION "FooBar";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::FunctionReferenced { function_name }) if function_name == "FooBar"
            ));
        }

        #[test]
        fn test_quoted_function_used_by_policy() {
            let sql = r#"
                CREATE FUNCTION "FooBar"() RETURNS BOOLEAN AS 'SELECT true;';
                CREATE TABLE t (id INT);
                CREATE POLICY p ON t USING ("FooBar"());
                DROP FUNCTION "FooBar";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::FunctionReferenced { function_name }) if function_name == "FooBar"
            ));
        }

        #[test]
        fn test_quoted_function_used_by_trigger() {
            let sql = r#"
                CREATE TABLE t (id INT);
                CREATE FUNCTION "FooBar"() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION "FooBar"();
                DROP FUNCTION "FooBar";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::FunctionReferenced { function_name }) if function_name == "FooBar"
            ));
        }
    }

    mod function_lookup_identifier_semantics {
        use super::*;
        use crate::traits::DatabaseLike;

        #[test]
        fn quoted_function_lookup_requires_exact_quoted_name() {
            let sql = r#"
                CREATE FUNCTION "FooBar"() RETURNS INT AS 'SELECT 1;';
            "#;
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            assert!(
                db.function_by_target(TargetName::new("FooBar", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.function_by_target(TargetName::new("foobar", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.function_by_target(TargetName::new("foobar", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn unquoted_function_lookup_uses_identifier_folding() {
            let sql = r"
                CREATE FUNCTION foobar() RETURNS INT AS 'SELECT 1;';
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            assert!(
                db.function_by_target(TargetName::new("foobar", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.function_by_target(TargetName::new("FOOBAR", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.function_by_target(TargetName::new("FOOBAR", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn database_like_function_lookup_is_identifier_aware() {
            let sql = r#"
                CREATE FUNCTION "FooBar"() RETURNS INT AS 'SELECT 1;';
            "#;
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            let case = IdentifierCase::AsWritten;
            assert!(
                db.function_by_target(TargetName::new("FooBar", true), case)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.function_by_target(TargetName::new("foobar", false), case)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.function_by_target(TargetName::new("foobar", true), case)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }
    }

    mod is_table_referenced_tests {
        use super::*;

        #[test]
        fn test_table_not_referenced() {
            let sql = r"
                CREATE TABLE standalone (id INT PRIMARY KEY);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Table exists
            assert!(
                db.table_by_target(TargetName::new("standalone", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            // Can be dropped
            let drop_sql = format!("{sql}\nDROP TABLE standalone;");
            let result = ParserDB::parse::<GenericDialect>(&drop_sql);
            assert!(result.is_ok());
        }

        #[test]
        fn test_table_referenced_by_single_fk() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Both tables exist
            assert!(
                db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            // Parent cannot be dropped (referenced)
            let drop_parent = format!("{sql}\nDROP TABLE parent;");
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(&drop_parent),
                Err(Error::TableReferenced { table_name }) if table_name == "parent"
            ));

            // Child can be dropped (not referenced)
            let drop_child = format!("{sql}\nDROP TABLE child;");
            assert!(ParserDB::parse::<GenericDialect>(&drop_child).is_ok());
        }

        #[test]
        fn test_table_referenced_by_multiple_fks() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child1 (id INT, parent_id INT REFERENCES parent(id));
                CREATE TABLE child2 (id INT, parent_id INT REFERENCES parent(id));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            assert!(
                db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            // Parent cannot be dropped
            let drop_sql = format!("{sql}\nDROP TABLE parent;");
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(&drop_sql),
                Err(Error::TableReferenced { .. })
            ));
        }

        #[test]
        fn test_self_referential_table_not_blocked() {
            let sql = r"
                CREATE TABLE tree (id INT PRIMARY KEY, parent_id INT REFERENCES tree(id));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            assert!(
                db.table_by_target(TargetName::new("tree", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );

            // Self-referential table CAN be dropped
            let drop_sql = format!("{sql}\nDROP TABLE tree;");
            assert!(ParserDB::parse::<GenericDialect>(&drop_sql).is_ok());
        }

        #[test]
        fn test_chain_of_references() {
            let sql = r"
                CREATE TABLE grandparent (id INT PRIMARY KEY);
                CREATE TABLE parent (id INT PRIMARY KEY, gp_id INT REFERENCES grandparent(id));
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
            ";

            // Cannot drop grandparent (referenced by parent)
            let drop_gp = format!("{sql}\nDROP TABLE grandparent;");
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(&drop_gp),
                Err(Error::TableReferenced { table_name }) if table_name == "grandparent"
            ));

            // Cannot drop parent (referenced by child)
            let drop_parent = format!("{sql}\nDROP TABLE parent;");
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(&drop_parent),
                Err(Error::TableReferenced { table_name }) if table_name == "parent"
            ));

            // Can drop child (not referenced)
            let drop_child = format!("{sql}\nDROP TABLE child;");
            assert!(ParserDB::parse::<GenericDialect>(&drop_child).is_ok());
        }
    }

    mod remove_table_tests {
        use super::*;
        use crate::traits::{DatabaseLike, TableLike};

        #[test]
        fn test_remove_table_removes_columns() {
            let sql = r"
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT, age INT);
                CREATE TABLE t2 (id INT PRIMARY KEY);
                DROP TABLE t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // t1 should be gone
            assert!(
                db.table_by_target(TargetName::new("t1", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );

            // t2 should still have its column
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.columns(&db).expect("columns").count(), 1);
        }

        #[test]
        fn test_remove_table_removes_indices() {
            let sql = r"
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX idx_name ON t1(name);
                CREATE TABLE t2 (id INT PRIMARY KEY, value TEXT);
                CREATE INDEX idx_value ON t2(value);
                DROP TABLE t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // t1's index should be gone, t2's should remain
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.indices(&db).expect("indices").count(), 1);

            // Total indices across all tables should be 1 (only t2's index)
            let total_indices: usize =
                db.tables().map(|t| t.indices(&db).expect("indices").count()).sum();
            assert_eq!(total_indices, 1);
        }

        #[test]
        fn test_remove_table_removes_foreign_keys() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE child;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Parent should exist with no foreign keys (parent doesn't have any
            // FKs pointing out)
            let parent = db
                .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("parent should exist");
            assert_eq!(parent.foreign_keys(&db).expect("foreign keys").count(), 0);

            // No foreign keys in the database (child's FK was removed with
            // child)
            let total_fks: usize =
                db.tables().map(|t| t.foreign_keys(&db).expect("foreign keys").count()).sum();
            assert_eq!(total_fks, 0);
        }

        #[test]
        fn test_remove_table_removes_check_constraints() {
            let sql = r"
                CREATE TABLE t1 (id INT CHECK (id > 0));
                CREATE TABLE t2 (value INT CHECK (value < 100));
                DROP TABLE t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Only t2's check constraint should remain
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.check_constraints(&db).expect("check constraints").count(), 1);
        }

        #[test]
        fn test_remove_table_removes_triggers() {
            let sql = r"
                CREATE TABLE t1 (id INT);
                CREATE TABLE t2 (id INT);
                CREATE FUNCTION fn1() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE FUNCTION fn2() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER trg1 BEFORE INSERT ON t1 FOR EACH ROW EXECUTE FUNCTION fn1();
                CREATE TRIGGER trg2 BEFORE INSERT ON t2 FOR EACH ROW EXECUTE FUNCTION fn2();
                DROP TABLE t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Only t2's trigger should remain
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.triggers(&db).expect("triggers").count(), 1);
        }

        #[test]
        fn test_remove_table_removes_policies() {
            let sql = r"
                CREATE TABLE t1 (id INT);
                CREATE TABLE t2 (id INT);
                CREATE POLICY p1 ON t1 USING (true);
                CREATE POLICY p2 ON t2 USING (true);
                DROP TABLE t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse");

            // Only t2's policy should remain
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.policies(&db).expect("policies").count(), 1);
        }
    }

    mod drop_index_tests {
        use super::*;

        #[test]
        fn test_drop_index_basic() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX idx_name ON t(name);
                DROP INDEX idx_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Index should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.indices(&db).expect("indices").count(), 0);
        }

        #[test]
        fn test_drop_index_if_exists_when_exists() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX idx_name ON t(name);
                DROP INDEX IF EXISTS idx_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Index should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.indices(&db).expect("indices").count(), 0);
        }

        #[test]
        fn test_drop_index_if_exists_when_not_exists() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY);
                DROP INDEX IF EXISTS nonexistent_idx;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Should succeed without error
            assert!(
                db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_index_not_found_error_type() {
            let sql = "DROP INDEX nonexistent_idx;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropIndexNotFound { index_name }) if index_name == "nonexistent_idx"
            ));
        }

        #[test]
        fn test_drop_multiple_indices() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT);
                CREATE INDEX idx_name ON t(name);
                CREATE INDEX idx_age ON t(age);
                DROP INDEX idx_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Only idx_age should remain
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.indices(&db).expect("indices").count(), 1);
        }

        #[test]
        fn test_drop_index_then_recreate() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX idx_name ON t(name);
                DROP INDEX idx_name;
                CREATE INDEX idx_name ON t(name);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Index should exist again
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.indices(&db).expect("indices").count(), 1);
        }

        #[test]
        fn test_drop_index_keeps_other_table_indices() {
            let sql = r"
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE t2 (id INT PRIMARY KEY, value TEXT);
                CREATE INDEX idx_t1_name ON t1(name);
                CREATE INDEX idx_t2_value ON t2(value);
                DROP INDEX idx_t1_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // t1 should have no indices
            let t1 = db
                .table_by_target(TargetName::new("t1", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t1 should exist");
            assert_eq!(t1.indices(&db).expect("indices").count(), 0);

            // t2 should still have its index
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.indices(&db).expect("indices").count(), 1);
        }

        #[test]
        fn test_drop_index_table_still_exists() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT);
                CREATE INDEX idx_name ON t(name);
                DROP INDEX idx_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should still exist with its columns
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.columns(&db).expect("columns").count(), 3);
        }
    }

    mod index_enumeration_tests {
        use super::*;
        use crate::traits::IndexLike;

        #[test]
        fn test_indexes_enumerates_named_indexes() {
            let sql = r"
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX idx_name ON t1(name);
                CREATE TABLE t2 (id INT PRIMARY KEY, value TEXT);
                CREATE INDEX idx_value ON t2(value);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let names: Vec<String> =
                db.indexes().filter_map(|i| i.name().map(ToString::to_string)).collect();
            assert_eq!(names, vec!["idx_name", "idx_value"]);
        }

        #[test]
        fn test_anonymous_index_has_no_name() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT);
                CREATE INDEX ON t(name);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let index = db.indexes().next().expect("index should exist");
            assert!(index.name().is_none());
        }

        #[test]
        fn test_indexes_excludes_unique_constraints() {
            let sql = r"
                CREATE TABLE t (id INT PRIMARY KEY, name TEXT, UNIQUE (name));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.indexes().count(), 0);

            // `UNIQUE (name)` declares no name, so the constraint viewed as an
            // `IndexLike` is anonymous.
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table should exist");
            for ui in table.unique_indices(&db).expect("unique indices") {
                assert!(IndexLike::name(ui).is_none());
            }
        }
    }

    mod drop_trigger_tests {
        use super::*;

        #[test]
        fn test_drop_trigger_basic() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION trigger_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
                DROP TRIGGER my_trigger ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Trigger should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.triggers(&db).expect("triggers").count(), 0);
        }

        #[test]
        fn test_drop_trigger_if_exists_when_exists() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION trigger_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
                DROP TRIGGER IF EXISTS my_trigger ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Trigger should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.triggers(&db).expect("triggers").count(), 0);
        }

        #[test]
        fn test_drop_trigger_if_exists_when_not_exists() {
            let sql = r"
                CREATE TABLE t (id INT);
                DROP TRIGGER IF EXISTS nonexistent_trigger ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Should succeed without error
            assert!(
                db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_trigger_not_found_error_type() {
            let sql = r"
                CREATE TABLE t (id INT);
                DROP TRIGGER nonexistent_trigger ON t;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropTriggerNotFound { trigger_name }) if trigger_name == "nonexistent_trigger"
            ));
        }

        #[test]
        fn test_drop_one_of_multiple_triggers() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION fn1() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE FUNCTION fn2() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER trigger1 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION fn1();
                CREATE TRIGGER trigger2 BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION fn2();
                DROP TRIGGER trigger1 ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Only trigger2 should remain
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.triggers(&db).expect("triggers").count(), 1);
        }

        #[test]
        fn test_drop_trigger_then_recreate() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION trigger_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
                DROP TRIGGER my_trigger ON t;
                CREATE TRIGGER my_trigger AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Trigger should exist again
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.triggers(&db).expect("triggers").count(), 1);
        }

        #[test]
        fn test_drop_trigger_keeps_other_table_triggers() {
            let sql = r"
                CREATE TABLE t1 (id INT);
                CREATE TABLE t2 (id INT);
                CREATE FUNCTION fn1() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE FUNCTION fn2() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER trigger1 BEFORE INSERT ON t1 FOR EACH ROW EXECUTE FUNCTION fn1();
                CREATE TRIGGER trigger2 BEFORE INSERT ON t2 FOR EACH ROW EXECUTE FUNCTION fn2();
                DROP TRIGGER trigger1 ON t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // t1 should have no triggers
            let t1 = db
                .table_by_target(TargetName::new("t1", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t1 should exist");
            assert_eq!(t1.triggers(&db).expect("triggers").count(), 0);

            // t2 should still have its trigger
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.triggers(&db).expect("triggers").count(), 1);
        }

        #[test]
        fn test_drop_trigger_function_still_exists() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE FUNCTION trigger_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
                CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trigger_fn();
                DROP TRIGGER my_trigger ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Function should still exist after dropping trigger
            assert!(
                db.function_by_target(
                    TargetName::new("trigger_fn", false),
                    IdentifierCase::AsWritten
                )
                .expect("unambiguous lookup")
                .is_some()
            );
        }
    }

    mod drop_policy_tests {
        use super::*;

        #[test]
        fn test_drop_policy_basic() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE POLICY my_policy ON t USING (true);
                DROP POLICY my_policy ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Policy should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.policies(&db).expect("policies").count(), 0);
        }

        #[test]
        fn test_drop_policy_if_exists_when_exists() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE POLICY my_policy ON t USING (true);
                DROP POLICY IF EXISTS my_policy ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Policy should be removed
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.policies(&db).expect("policies").count(), 0);
        }

        #[test]
        fn test_drop_policy_if_exists_when_not_exists() {
            let sql = r"
                CREATE TABLE t (id INT);
                DROP POLICY IF EXISTS nonexistent_policy ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Should succeed without error
            assert!(
                db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_policy_not_found_error_type() {
            let sql = r"
                CREATE TABLE t (id INT);
                DROP POLICY nonexistent_policy ON t;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropPolicyNotFound { policy_name }) if policy_name == "nonexistent_policy"
            ));
        }

        #[test]
        fn test_drop_one_of_multiple_policies() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE POLICY policy1 ON t USING (true);
                CREATE POLICY policy2 ON t USING (false);
                DROP POLICY policy1 ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Only policy2 should remain
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.policies(&db).expect("policies").count(), 1);
        }

        #[test]
        fn test_drop_policy_then_recreate() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE POLICY my_policy ON t USING (true);
                DROP POLICY my_policy ON t;
                CREATE POLICY my_policy ON t USING (false);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Policy should exist again
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.policies(&db).expect("policies").count(), 1);
        }

        #[test]
        fn test_drop_policy_keeps_other_table_policies() {
            let sql = r"
                CREATE TABLE t1 (id INT);
                CREATE TABLE t2 (id INT);
                CREATE POLICY policy1 ON t1 USING (true);
                CREATE POLICY policy2 ON t2 USING (true);
                DROP POLICY policy1 ON t1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // t1 should have no policies
            let t1 = db
                .table_by_target(TargetName::new("t1", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t1 should exist");
            assert_eq!(t1.policies(&db).expect("policies").count(), 0);

            // t2 should still have its policy
            let t2 = db
                .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("t2 should exist");
            assert_eq!(t2.policies(&db).expect("policies").count(), 1);
        }

        #[test]
        fn test_drop_policy_table_still_exists() {
            let sql = r"
                CREATE TABLE t (id INT, name TEXT);
                CREATE POLICY my_policy ON t USING (true);
                DROP POLICY my_policy ON t;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should still exist with its columns
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.columns(&db).expect("columns").count(), 2);
        }
    }

    mod drop_role_tests {
        use super::*;

        #[test]
        fn test_drop_role_basic() {
            let sql = r"
                CREATE ROLE my_role;
                DROP ROLE my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Role should be removed
            assert!(db.role("my_role").is_none());
        }

        #[test]
        fn test_drop_role_if_exists_when_exists() {
            let sql = r"
                CREATE ROLE my_role;
                DROP ROLE IF EXISTS my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Role should be removed
            assert!(db.role("my_role").is_none());
        }

        #[test]
        fn test_drop_role_if_exists_when_not_exists() {
            let sql = r"
                DROP ROLE IF EXISTS nonexistent_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Should succeed without error
            assert_eq!(db.roles().count(), 0);
        }

        #[test]
        fn test_drop_role_not_found_error_type() {
            let sql = "DROP ROLE nonexistent_role;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::DropRoleNotFound { role_name }) if role_name == "nonexistent_role"
            ));
        }

        #[test]
        fn test_drop_role_referenced_by_grant_fails() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE my_role;
                GRANT SELECT ON t TO my_role;
                DROP ROLE my_role;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::RoleReferenced { role_name }) if role_name == "my_role"
            ));
        }

        #[test]
        fn test_drop_role_after_revoking_grant_succeeds() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE my_role;
                GRANT SELECT ON t TO my_role;
                REVOKE SELECT ON t FROM my_role;
                DROP ROLE my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Should succeed after revoking");

            // Role should be removed
            assert!(db.role("my_role").is_none());
        }

        #[test]
        fn test_drop_one_of_multiple_roles() {
            let sql = r"
                CREATE ROLE role1;
                CREATE ROLE role2;
                DROP ROLE role1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // role1 should be removed
            assert!(db.role("role1").is_none());

            // role2 should still exist
            assert!(db.role("role2").is_some());
        }

        #[test]
        fn test_drop_role_then_recreate() {
            let sql = r"
                CREATE ROLE my_role;
                DROP ROLE my_role;
                CREATE ROLE my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Role should exist again
            assert!(db.role("my_role").is_some());
        }
    }

    mod grant_revoke_semantics {
        use sqlparser::{ast::Action, dialect::PostgreSqlDialect};

        use super::*;
        use crate::traits::{GrantLike, TableLike};

        #[test]
        fn test_revoke_partial_privilege_preserves_other_actions() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE my_role;
                GRANT SELECT, INSERT ON t TO my_role;
                REVOKE SELECT ON t FROM my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let grants: Vec<_> = db.table_grants().collect();
            assert_eq!(grants.len(), 1);
            let grant = grants[0];
            assert!(!grant.is_all_privileges());

            let remaining_privileges: Vec<_> = grant.privileges(&db).collect();
            assert_eq!(remaining_privileges.len(), 1);
            assert!(matches!(remaining_privileges[0], Action::Insert { .. }));

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role = db.role("my_role").expect("Role should exist");
            assert!(
                !table.can_select(role, &db).expect("can_select"),
                "SELECT should be revoked while INSERT remains"
            );
            assert!(table.can_insert(role, &db).expect("can_insert"));
        }

        #[test]
        fn test_revoke_grant_option_preserves_privilege() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r;
                GRANT SELECT ON t TO r WITH GRANT OPTION;
                REVOKE GRANT OPTION FOR SELECT ON t FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
            let grant = db.table_grants().next().expect("grant remains");
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table exists");
            let role = db.role("r").expect("role exists");

            assert!(!grant.with_grant_option());
            assert!(table.can_select(role, &db).expect("select resolves"));
        }

        #[test]
        fn test_revoke_grant_option_splits_actions_and_grantees() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE a;
                CREATE ROLE b;
                GRANT SELECT, INSERT ON t TO a, b WITH GRANT OPTION;
                REVOKE GRANT OPTION FOR SELECT ON t FROM a;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
            let mut states = Vec::new();

            for grant in db.table_grants() {
                let has_select =
                    grant.privileges(&db).any(|action| matches!(action, Action::Select { .. }));
                let has_insert =
                    grant.privileges(&db).any(|action| matches!(action, Action::Insert { .. }));
                for grantee in grant.grantees(&db) {
                    states.push((
                        grantee.to_string(),
                        has_select,
                        has_insert,
                        grant.with_grant_option(),
                    ));
                }
            }
            states.sort();

            assert_eq!(
                states,
                vec![
                    ("a".to_string(), false, true, true),
                    ("a".to_string(), true, false, false),
                    ("b".to_string(), true, true, true),
                ]
            );
        }

        #[test]
        fn test_revoke_grant_option_splits_columns() {
            let sql = r"
                CREATE TABLE t (a INT, b INT);
                CREATE ROLE r;
                GRANT SELECT (a, b) ON t TO r WITH GRANT OPTION;
                REVOKE GRANT OPTION FOR SELECT (a) ON t FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
            let mut states = Vec::new();

            for grant in db.table_grants() {
                for action in grant.privileges(&db) {
                    if let Action::Select { columns: Some(columns) } = action {
                        states.extend(
                            columns
                                .iter()
                                .map(|column| (column.value.clone(), grant.with_grant_option())),
                        );
                    }
                }
            }
            states.sort();

            assert_eq!(states, vec![("a".to_string(), false), ("b".to_string(), true)]);
        }

        #[test]
        fn test_revoke_grant_option_for_all_preserves_all_privileges() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r;
                GRANT ALL PRIVILEGES ON t TO r WITH GRANT OPTION;
                REVOKE GRANT OPTION FOR ALL PRIVILEGES ON t FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
            let grant = db.table_grants().next().expect("grant remains");

            assert!(grant.is_all_privileges());
            assert!(!grant.with_grant_option());
        }

        #[test]
        fn test_revoke_grant_option_subset_from_all_is_unsupported() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r;
                GRANT ALL PRIVILEGES ON t TO r WITH GRANT OPTION;
                REVOKE GRANT OPTION FOR SELECT ON t FROM r;
            ";
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::UnsupportedRevoke { reason, .. })
                    if reason.contains("subset from ALL PRIVILEGES")
            ));
        }

        #[test]
        fn test_revoke_absent_grant_option_is_a_noop() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r;
                GRANT SELECT ON t TO r;
                REVOKE GRANT OPTION FOR SELECT ON t FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
            let grant = db.table_grants().next().expect("grant remains");

            assert!(!grant.with_grant_option());
            assert_eq!(db.table_grants().count(), 1);
        }

        #[test]
        fn test_revoke_column_scoped_against_table_wide_action_is_unsupported() {
            let sql = r"
                CREATE TABLE t (id INT, name TEXT);
                CREATE ROLE my_role;
                GRANT SELECT ON t TO my_role;
                REVOKE SELECT (id) ON t FROM my_role;
            ";
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);

            assert!(matches!(
                result,
                Err(Error::UnsupportedRevoke { reason, .. })
                    if reason.contains("column-scoped REVOKE against a table-wide action grant")
            ));
        }

        #[test]
        fn test_revoke_column_scoped_from_column_scoped_grant_keeps_remaining_columns() {
            let sql = r"
                CREATE TABLE t (id INT, name TEXT);
                CREATE ROLE my_role;
                GRANT SELECT (id, name) ON t TO my_role;
                REVOKE SELECT (id) ON t FROM my_role;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse SQL");

            let grant = db.table_grants().next().expect("Expected a remaining grant");
            let remaining_privileges: Vec<_> = grant.privileges(&db).collect();

            assert_eq!(remaining_privileges.len(), 1);
            match remaining_privileges[0] {
                Action::Select { columns: Some(columns) } => {
                    assert_eq!(columns.len(), 1);
                    assert_eq!(columns[0].value, "name");
                }
                other => panic!("Expected SELECT with one remaining column, got {other:?}"),
            }
        }

        #[test]
        fn test_revoke_from_first_grantee_keeps_second_grantee_privileges() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE a;
                CREATE ROLE b;
                GRANT SELECT ON t TO a, b;
                REVOKE SELECT ON t FROM a;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role_a = db.role("a").expect("Role a should exist");
            let role_b = db.role("b").expect("Role b should exist");

            assert!(
                !table.can_select(role_a, &db).expect("can_select"),
                "SELECT should be revoked for a"
            );
            assert!(
                table.can_select(role_b, &db).expect("can_select"),
                "SELECT should remain for b"
            );
        }

        #[test]
        fn test_revoke_from_second_grantee_keeps_first_grantee_privileges() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE a;
                CREATE ROLE b;
                GRANT SELECT ON t TO a, b;
                REVOKE SELECT ON t FROM b;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role_a = db.role("a").expect("Role a should exist");
            let role_b = db.role("b").expect("Role b should exist");

            assert!(
                table.can_select(role_a, &db).expect("can_select"),
                "SELECT should remain for a"
            );
            assert!(
                !table.can_select(role_b, &db).expect("can_select"),
                "SELECT should be revoked for b"
            );
        }

        #[test]
        fn test_revoke_partial_privilege_from_one_grantee_preserves_other_grantee_actions() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE a;
                CREATE ROLE b;
                GRANT SELECT, INSERT ON t TO a, b;
                REVOKE SELECT ON t FROM a;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role_a = db.role("a").expect("Role a should exist");
            let role_b = db.role("b").expect("Role b should exist");

            assert_eq!(db.table_grants().count(), 2);
            assert!(
                !table.can_select(role_a, &db).expect("can_select"),
                "SELECT should be revoked for a"
            );
            assert!(
                table.can_insert(role_a, &db).expect("can_insert"),
                "INSERT should remain for a"
            );
            assert!(
                table.can_select(role_b, &db).expect("can_select"),
                "SELECT should remain for b"
            );
            assert!(
                table.can_insert(role_b, &db).expect("can_insert"),
                "INSERT should remain for b"
            );
        }

        #[test]
        fn test_revoke_all_from_one_grantee_preserves_other_grantee_actions() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE a;
                CREATE ROLE b;
                GRANT SELECT, INSERT ON t TO a, b;
                REVOKE ALL ON t FROM a;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role_a = db.role("a").expect("Role a should exist");
            let role_b = db.role("b").expect("Role b should exist");

            assert!(
                !table.can_select(role_a, &db).expect("can_select"),
                "SELECT should be revoked for a"
            );
            assert!(
                !table.can_insert(role_a, &db).expect("can_insert"),
                "INSERT should be revoked for a"
            );
            assert!(
                table.can_select(role_b, &db).expect("can_select"),
                "SELECT should remain for b"
            );
            assert!(
                table.can_insert(role_b, &db).expect("can_insert"),
                "INSERT should remain for b"
            );
        }

        #[test]
        fn test_revoke_all_removes_matching_grants() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE my_role;
                GRANT SELECT, INSERT ON t TO my_role;
                REVOKE ALL ON t FROM my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 0);
            assert_eq!(db.column_grants().count(), 0);
        }

        #[test]
        fn test_public_grant_applies_to_non_public_role() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE app_user;
                GRANT SELECT ON t TO PUBLIC;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let app_user = db.role("app_user").expect("Role should exist");
            assert!(table.can_select(app_user, &db).expect("can_select"));
        }

        #[test]
        fn test_schema_qualified_grant_applies_only_to_target_table() {
            let sql = r"
                CREATE SCHEMA s1;
                CREATE SCHEMA s2;
                CREATE TABLE s1.t (id INT);
                CREATE TABLE s2.t (id INT);
                CREATE ROLE app_role;
                GRANT SELECT ON s1.t TO app_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let role = db.role("app_role").expect("Role should exist");
            let s1_t = db
                .table_by_target(
                    TargetName::new("t", false).with_schema("s1", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("s1.t should exist");
            let s2_t = db
                .table_by_target(
                    TargetName::new("t", false).with_schema("s2", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("s2.t should exist");

            assert!(s1_t.can_select(role, &db).expect("can_select"));
            assert!(!s2_t.can_select(role, &db).expect("can_select"));
        }

        #[test]
        fn test_revoke_object_matching_is_case_insensitive_for_unquoted_identifiers() {
            let sql = r"
                CREATE TABLE T (id INT);
                CREATE ROLE my_role;
                GRANT SELECT ON T TO my_role;
                REVOKE SELECT ON t FROM my_role;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            let role = db.role("my_role").expect("Role should exist");
            assert!(!table.can_select(role, &db).expect("can_select"));
            assert_eq!(db.table_grants().count(), 0);
        }

        /// An unquoted `CREATE TABLE T` stores `t`, so a quoted `"T"` reaches
        /// no table at all and the database says so before it ever looks for a
        /// grant to subtract.
        #[test]
        fn test_revoke_object_matching_preserves_quoted_identifier_semantics() {
            let sql = r#"
                CREATE TABLE T (id INT);
                CREATE ROLE my_role;
                GRANT SELECT ON T TO my_role;
                REVOKE SELECT ON "T" FROM my_role;
            "#;
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);

            assert!(
                matches!(&result, Err(Error::TableNotFoundForGrant { table_name }) if table_name == "T"),
                "got {result:?}"
            );
        }

        #[test]
        fn test_revoke_object_matching_does_not_match_quoted_grant_with_unquoted_lookup() {
            let sql = r#"
                CREATE TABLE "T" (id INT);
                CREATE ROLE my_role;
                GRANT SELECT ON "T" TO my_role;
                REVOKE SELECT ON t FROM my_role;
            "#;
            let result = ParserDB::parse::<PostgreSqlDialect>(sql);

            assert!(
                matches!(&result, Err(Error::TableNotFoundForGrant { table_name }) if table_name == "t"),
                "got {result:?}"
            );
        }

        #[test]
        fn test_revoke_function_object_matching_is_case_insensitive_for_unquoted_identifiers() {
            let sql = r"
                CREATE ROLE my_role;
                GRANT EXECUTE ON FUNCTION F() TO my_role;
                REVOKE EXECUTE ON FUNCTION f() FROM my_role;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 0);
            assert_eq!(db.column_grants().count(), 0);
        }

        /// A quoted `"F"` and an unquoted `F` are two different functions, so
        /// the revoke subtracts nothing and the grant stands.
        #[test]
        fn test_revoke_function_object_matching_preserves_quoted_identifier_semantics() {
            let sql = r#"
                CREATE ROLE my_role;
                GRANT EXECUTE ON FUNCTION F() TO my_role;
                REVOKE EXECUTE ON FUNCTION "F"() FROM my_role;
            "#;
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 1, "the grant was left alone");
        }

        #[test]
        fn test_revoke_function_object_matching_does_not_match_quoted_grant_with_unquoted_lookup() {
            let sql = r#"
                CREATE ROLE my_role;
                GRANT EXECUTE ON FUNCTION "F"() TO my_role;
                REVOKE EXECUTE ON FUNCTION f() FROM my_role;
            "#;
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 1, "the grant was left alone");
        }

        // ----------------------------------------------------------------
        // Coverage-targeted tests for `impls/sqlparser/grant.rs`:
        // `partition_grantees_for_revoke`, `apply_revoke_to_grant`,
        // `grant_objects_inner_match`, the per-column accessor branches,
        // and the GrantObjects variants the existing tests don't reach.
        // ----------------------------------------------------------------

        /// `apply_revoke_to_grant` `(Privileges::All, Privileges::Actions)`
        /// arm: the implementation cannot represent "ALL minus X" in
        /// its grant model, so a partial revoke against a `GRANT ALL
        /// PRIVILEGES` is treated as matched-but-no-op — the grant
        /// stays intact at ALL PRIVILEGES. This is a documented
        /// limitation in `impls/sqlparser/grant.
        /// rs::apply_revoke_to_grant`.
        #[test]
        fn test_revoke_select_against_all_privileges_is_a_documented_noop() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE my_role;
                GRANT ALL PRIVILEGES ON t TO my_role;
                REVOKE SELECT ON t FROM my_role;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let grant = db.table_grants().next().expect("grant must remain");
            assert!(
                grant.is_all_privileges(),
                "grant stays ALL PRIVILEGES; the model cannot represent ALL-minus-Actions",
            );
        }

        /// `tables()` `GrantObjects::AllTablesInSchema` branch: a grant on
        /// `ALL TABLES IN SCHEMA` should parse and produce one entry in the
        /// `table_grants` iterator without panicking.
        #[test]
        fn test_grant_all_tables_in_schema_parses_and_is_indexed() {
            let sql = r"
                CREATE SCHEMA s;
                CREATE TABLE s.t (id INT);
                CREATE ROLE r;
                GRANT SELECT ON ALL TABLES IN SCHEMA s TO r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 1);
        }

        /// `grant_objects_inner_match` Function arm: an EXECUTE grant on a
        /// concrete function object lands in `table_grants`.
        #[test]
        fn test_grant_execute_on_function() {
            let sql = r"
                CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT $1';
                CREATE ROLE r;
                GRANT EXECUTE ON FUNCTION f(INT) TO r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 1);
        }

        /// `apply_revoke_to_grant`'s object-mismatch fast-return: revoking
        /// against a different table than the grant covers must leave the
        /// original grant untouched and itself be a no-op (no error), which is
        /// what this test always said it wanted and now gets.
        #[test]
        fn test_revoke_on_different_table_leaves_original_grant_untouched() {
            let sql = r"
                CREATE TABLE t1 (id INT);
                CREATE TABLE t2 (id INT);
                CREATE ROLE r;
                GRANT SELECT ON t1 TO r;
                REVOKE SELECT ON t2 FROM r;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 1, "the grant on `t1` stands");
        }

        /// `partition_grantees_for_revoke` grantee-mismatch path: revoking from
        /// a role that holds no such grant subtracts nothing, and the database
        /// takes no exception to it.
        #[test]
        fn test_revoke_from_different_grantee_leaves_the_grant_alone() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r1;
                CREATE ROLE r2;
                GRANT SELECT ON t TO r1;
                REVOKE SELECT ON t FROM r2;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert_eq!(db.table_grants().count(), 1, "the grant to `r1` stands");
        }

        /// `columns()` per-column INSERT/UPDATE/REFERENCES arms: a multi-
        /// action column-scoped grant must surface column grants for each
        /// privileged action.
        #[test]
        fn test_grant_per_column_insert_update_references_creates_column_grants() {
            let sql = r"
                CREATE TABLE t (a INT, b INT);
                CREATE ROLE r;
                GRANT INSERT (a), UPDATE (b), REFERENCES (a) ON t TO r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            // Each per-column action creates its own column grant.
            assert!(db.column_grants().count() >= 1, "at least one column grant expected");
        }

        /// `apply_revoke_to_grant`'s "drop the whole grant when no actions
        /// remain" path: REVOKE ALL from a single-grantee grant removes
        /// the grant entirely.
        #[test]
        fn test_revoke_all_privileges_from_only_grantee_drops_grant() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r;
                GRANT SELECT, INSERT ON t TO r;
                REVOKE ALL PRIVILEGES ON t FROM r;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `apply_revoke_action_to_grant_action` partial-column path: a
        /// column-scoped revoke that drops some columns but keeps others
        /// must preserve the remaining columns under the same action.
        #[test]
        fn test_revoke_subset_of_column_scoped_grant_keeps_unrevoked_columns() {
            let sql = r"
                CREATE TABLE t (a INT, b INT, c INT);
                CREATE ROLE r;
                GRANT SELECT (a, b, c) ON t TO r;
                REVOKE SELECT (b) ON t FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");

            let grant = db.table_grants().next().expect("grant must remain");
            let privileges: Vec<_> = grant.privileges(&db).collect();
            assert_eq!(privileges.len(), 1);
            match privileges[0] {
                Action::Select { columns: Some(columns) } => {
                    let names: Vec<_> = columns.iter().map(|c| c.value.as_str()).collect();
                    assert_eq!(names, vec!["a", "c"], "only `b` should be revoked");
                }
                other => panic!("expected SELECT with columns, got {other:?}"),
            }
        }

        /// `partition_grantees_for_revoke` multi-grantee path: revoking
        /// from one of two grantees must keep the grant alive for the
        /// other grantee (covers the `unmatched` partition + split).
        #[test]
        fn test_revoke_select_from_one_of_two_grantees_keeps_grant_for_other() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE ROLE r1;
                CREATE ROLE r2;
                GRANT SELECT ON t TO r1, r2;
                REVOKE SELECT ON t FROM r1;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();
            let r1 = db.role("r1").unwrap();
            let r2 = db.role("r2").unwrap();
            assert!(
                !table.can_select(r1, &db).expect("can_select"),
                "r1 should have had SELECT revoked"
            );
            assert!(table.can_select(r2, &db).expect("can_select"), "r2 should still have SELECT");
        }

        /// `grant_objects_inner_match` schema-list arm: REVOKE on
        /// `ALL TABLES IN SCHEMA` matching a same-shape GRANT goes
        /// through the giant merged arm that covers AllSequences/
        /// AllTables/AllViews/AllMaterializedViews/... InSchema.
        #[test]
        fn test_revoke_all_tables_in_schema_matches_corresponding_grant() {
            let sql = r"
                CREATE SCHEMA s;
                CREATE TABLE s.t (id INT);
                CREATE ROLE r;
                GRANT SELECT ON ALL TABLES IN SCHEMA s TO r;
                REVOKE SELECT ON ALL TABLES IN SCHEMA s FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0, "matching revoke removes the grant");
        }

        /// `grant_objects_inner_match` Schemas object-list arm:
        /// GRANT USAGE ON SCHEMA + matching REVOKE traverses the
        /// merged ObjectName-list arm.
        #[test]
        fn test_revoke_usage_on_schema_matches_grant() {
            let sql = r"
                CREATE SCHEMA s;
                CREATE ROLE r;
                GRANT USAGE ON SCHEMA s TO r;
                REVOKE USAGE ON SCHEMA s FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `grant_objects_inner_match` Function arm: REVOKE on a
        /// specific function-by-signature matches the corresponding
        /// GRANT EXECUTE on that function.
        #[test]
        fn test_revoke_execute_on_function_matches_grant() {
            let sql = r"
                CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT $1';
                CREATE ROLE r;
                GRANT EXECUTE ON FUNCTION f(INT) TO r;
                REVOKE EXECUTE ON FUNCTION f(INT) FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `applies_to_table` AllTablesInSchema branch: querying whether
        /// an `ALL TABLES IN SCHEMA s` grant applies to a specific table
        /// returns true iff the table's schema matches.
        #[test]
        fn test_all_tables_in_schema_grant_applies_to_matching_table() {
            use crate::traits::TableGrantLike;

            let sql = r"
                CREATE SCHEMA s;
                CREATE TABLE s.in_scope (id INT);
                CREATE TABLE out_of_scope (id INT);
                CREATE ROLE r;
                GRANT SELECT ON ALL TABLES IN SCHEMA s TO r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            let grant = db.table_grants().next().expect("grant");
            let in_scope = db
                .table_by_target(
                    TargetName::new("in_scope", false).with_schema("s", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("in_scope table");
            let out_of_scope = db
                .table_by_target(TargetName::new("out_of_scope", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("out_of_scope table");

            assert!(grant.applies_to_table(in_scope, &db));
            assert!(!grant.applies_to_table(out_of_scope, &db));
        }

        /// `grant_objects_inner_match` Sequences object-list arm:
        /// GRANT USAGE ON SEQUENCE + matching REVOKE traverses the
        /// merged ObjectName-list arm (covers another row of the merged
        /// pattern at lines ~239–261).
        #[test]
        fn test_revoke_usage_on_sequence_matches_grant() {
            let sql = r"
                CREATE ROLE r;
                GRANT USAGE ON SEQUENCE my_seq TO r;
                REVOKE USAGE ON SEQUENCE my_seq FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `grant_objects_inner_match` All-future-tables-in-schema arm.
        #[test]
        fn test_revoke_all_sequences_in_schema_matches_grant() {
            let sql = r"
                CREATE SCHEMA s;
                CREATE ROLE r;
                GRANT USAGE ON ALL SEQUENCES IN SCHEMA s TO r;
                REVOKE USAGE ON ALL SEQUENCES IN SCHEMA s FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `grant_objects_inner_match` fallback arm (`(left, right) =>
        /// left == right`): a GRANT and REVOKE on the same simple
        /// pseudo-object that doesn't match a specific variant goes
        /// through the structural-equality arm.
        #[test]
        fn test_revoke_on_database_object_matches_grant() {
            let sql = r"
                CREATE ROLE r;
                GRANT CREATE ON DATABASE my_db TO r;
                REVOKE CREATE ON DATABASE my_db FROM r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            assert_eq!(db.table_grants().count(), 0);
        }

        /// `ColumnGrantLike::columns` per-action branches: a multi-action
        /// column grant (`INSERT (a), UPDATE (b), REFERENCES (c)`) surfaces
        /// the union of columns across all three arms in `columns()`.
        #[test]
        fn test_column_grant_columns_iterator_covers_all_action_arms() {
            use crate::traits::ColumnGrantLike;

            let sql = r"
                CREATE TABLE t (a INT, b INT, c INT);
                CREATE ROLE r;
                GRANT INSERT (a), UPDATE (b), REFERENCES (c) ON t TO r;
            ";
            let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table");

            // Iterate every column grant and call `.columns(table, &db)`.
            // This routes through the Insert/Update/References match
            // arms in `impls/sqlparser/grant.rs::columns`.
            let mut all_cols: Vec<&str> = Vec::new();
            for cg in db.column_grants() {
                for col in cg.columns(table, &db).expect("column grant columns") {
                    all_cols.push(col.column_name());
                }
            }
            all_cols.sort_unstable();
            all_cols.dedup();
            // The three actions reference three distinct columns.
            assert!(all_cols.contains(&"a"));
            assert!(all_cols.contains(&"b"));
            assert!(all_cols.contains(&"c"));
        }
    }

    mod foreign_key_target_validation {
        use sqlparser::dialect::PostgreSqlDialect;

        use super::*;

        /// A foreign key written inline on the column and one written as a
        /// table constraint are the same constraint, so they take the same
        /// path and answer alike. The inline spelling used to skip both target
        /// checks entirely.
        #[test]
        fn both_spellings_refuse_alike() {
            let inline = "CREATE TABLE child (pid INT REFERENCES parent(id));";
            let table_level =
                "CREATE TABLE child (pid INT, FOREIGN KEY (pid) REFERENCES parent(id));";

            for sql in [inline, table_level] {
                assert!(
                    matches!(
                        ParserDB::parse::<GenericDialect>(sql),
                        Err(Error::ReferencedTableNotFoundForForeignKey { ref referenced_table, .. })
                            if referenced_table == "parent"
                    ),
                    "both spellings must refuse an absent target: {sql}"
                );
            }
        }

        #[test]
        fn column_option_reference_to_existing_target_is_accepted() {
            let sql = "
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
            ";
            assert!(ParserDB::parse::<GenericDialect>(sql).is_ok());
        }

        #[test]
        fn table_constraint_reference_to_existing_target_is_accepted() {
            let sql = "
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (
                    id INT PRIMARY KEY,
                    parent_id INT,
                    FOREIGN KEY (parent_id) REFERENCES parent(id)
                );
            ";
            assert!(ParserDB::parse::<GenericDialect>(sql).is_ok());
        }

        #[test]
        fn reference_to_missing_table_is_refused_naming_target_and_host() {
            let sql = "
                CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES orders(id));
            ";
            match ParserDB::parse::<GenericDialect>(sql) {
                Err(Error::ReferencedTableNotFoundForForeignKey {
                    referenced_table,
                    host_table,
                }) => {
                    assert_eq!(referenced_table, "orders");
                    assert_eq!(host_table, "child");
                }
                other => panic!("expected dangling-table error, got {other:?}"),
            }
        }

        #[test]
        fn reference_to_missing_column_is_refused_naming_column() {
            let sql = "
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(missing));
            ";
            match ParserDB::parse::<GenericDialect>(sql) {
                Err(Error::ReferencedColumnNotFoundForForeignKey {
                    referenced_column,
                    referenced_table,
                    host_table,
                }) => {
                    assert_eq!(referenced_column, "missing");
                    assert_eq!(referenced_table, "parent");
                    assert_eq!(host_table, "child");
                }
                other => panic!("expected dangling-column error, got {other:?}"),
            }
        }

        #[test]
        fn bare_and_public_qualified_targets_resolve_identically() {
            let bare = "
                CREATE TABLE public.parent (id INT PRIMARY KEY);
                CREATE TABLE public.child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
            ";
            let qualified = "
                CREATE TABLE public.parent (id INT PRIMARY KEY);
                CREATE TABLE public.child (
                    id INT PRIMARY KEY,
                    parent_id INT REFERENCES public.parent(id)
                );
            ";
            assert!(ParserDB::parse::<PostgreSqlDialect>(bare).is_ok());
            assert!(ParserDB::parse::<PostgreSqlDialect>(qualified).is_ok());
        }

        /// PostgreSQL refuses a reference to a table declared later, so this
        /// crate does too. Verified against a real server.
        #[test]
        fn forward_reference_is_refused() {
            let sql = "
                CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
                CREATE TABLE parent (id INT PRIMARY KEY);
            ";
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(sql),
                Err(Error::ReferencedTableNotFoundForForeignKey { .. })
            ));
        }

        /// A table may reference itself, which the database accepts, so the
        /// table being created counts as present while its own constraints are
        /// resolved.
        #[test]
        fn self_referential_reference_is_accepted() {
            let sql = "
                CREATE TABLE tree (
                    id INT PRIMARY KEY,
                    parent_id INT REFERENCES tree(id)
                );
            ";
            assert!(ParserDB::parse::<GenericDialect>(sql).is_ok());
        }

        /// The read stops at the first dangling constraint in statement order,
        /// so the error names that one and not a later one.
        #[test]
        fn the_first_dangling_constraint_in_order_is_the_one_reported() {
            let sql = "
                CREATE TABLE a (id INT PRIMARY KEY, x INT REFERENCES missing_a(id));
                CREATE TABLE b (id INT PRIMARY KEY, y INT REFERENCES missing_b(id));
            ";
            match ParserDB::parse::<GenericDialect>(sql) {
                Err(Error::ReferencedTableNotFoundForForeignKey { referenced_table, .. }) => {
                    assert_eq!(referenced_table, "missing_a");
                }
                other => panic!("expected dangling-table error, got {other:?}"),
            }
        }

        /// Without a unique key on the far side a child row could match more
        /// than one parent. PostgreSQL, MySQL 8 and SQLite all refuse it, each
        /// verified against a running server for the first two.
        #[test]
        fn a_target_column_with_nothing_unique_behind_it_is_refused() {
            let sql = "
                CREATE TABLE parent (id INT, tag TEXT);
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";
            match ParserDB::parse::<GenericDialect>(sql) {
                Err(Error::ReferencedColumnsNotUniqueForForeignKey {
                    referenced_columns,
                    referenced_table,
                    host_table,
                }) => {
                    assert_eq!(referenced_columns, "tag");
                    assert_eq!(referenced_table, "parent");
                    assert_eq!(host_table, "child");
                }
                other => panic!("expected a missing-unique-key error, got {other:?}"),
            }
        }

        #[test]
        fn every_way_of_declaring_the_key_backs_the_target() {
            let inline_unique = "
                CREATE TABLE parent (id INT, tag TEXT UNIQUE);
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";
            let table_unique = "
                CREATE TABLE parent (id INT, tag TEXT, UNIQUE (tag));
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";
            let primary_key = "
                CREATE TABLE parent (tag TEXT PRIMARY KEY);
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";
            // A unique index is not on the table node at all, it arrives as its
            // own statement, and the database accepts it as the backing key.
            let unique_index = "
                CREATE TABLE parent (id INT, tag TEXT);
                CREATE UNIQUE INDEX parent_tag ON parent (tag);
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";
            // A plain index is not enough, which is what MySQL 8 also answers.
            let plain_index = "
                CREATE TABLE parent (id INT, tag TEXT);
                CREATE INDEX parent_tag ON parent (tag);
                CREATE TABLE child (t TEXT REFERENCES parent(tag));
            ";

            for sql in [inline_unique, table_unique, primary_key, unique_index] {
                assert!(ParserDB::parse::<GenericDialect>(sql).is_ok(), "should be backed: {sql}");
            }
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(plain_index),
                Err(Error::ReferencedColumnsNotUniqueForForeignKey { .. })
            ));
        }

        #[test]
        fn a_composite_key_is_matched_whole_and_not_in_part() {
            let whole = "
                CREATE TABLE parent (id1 INT, id2 INT, PRIMARY KEY (id1, id2));
                CREATE TABLE child (a INT, b INT, FOREIGN KEY (a, b) REFERENCES parent(id1, id2));
            ";
            // Order within the key does not matter, the set does.
            let reordered = "
                CREATE TABLE parent (id1 INT, id2 INT, PRIMARY KEY (id1, id2));
                CREATE TABLE child (a INT, b INT, FOREIGN KEY (a, b) REFERENCES parent(id2, id1));
            ";
            let part = "
                CREATE TABLE parent (id1 INT, id2 INT, PRIMARY KEY (id1, id2));
                CREATE TABLE child (a INT REFERENCES parent(id1));
            ";

            assert!(ParserDB::parse::<GenericDialect>(whole).is_ok());
            assert!(ParserDB::parse::<GenericDialect>(reordered).is_ok());
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(part),
                Err(Error::ReferencedColumnsNotUniqueForForeignKey { .. })
            ));
        }

        /// `REFERENCES parent` with no column list points at the primary key,
        /// so the target needs one.
        #[test]
        fn a_reference_naming_no_column_needs_a_primary_key() {
            let with_pk = "
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (pid INT REFERENCES parent);
            ";
            let without_pk = "
                CREATE TABLE parent (id INT UNIQUE);
                CREATE TABLE child (pid INT REFERENCES parent);
            ";

            assert!(ParserDB::parse::<GenericDialect>(with_pk).is_ok());
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(without_pk),
                Err(Error::ReferencedColumnsNotUniqueForForeignKey { .. })
            ));
        }
    }

    mod primary_key_validation {
        use super::*;
        use crate::traits::ColumnLike;

        #[test]
        fn expression_primary_key_errors_instead_of_panicking() {
            match ParserDB::parse::<GenericDialect>(
                "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a - b));",
            ) {
                Err(Error::InvalidPrimaryKey { table_name, reason }) => {
                    assert_eq!(table_name, "t");
                    assert!(
                        reason.contains("a - b"),
                        "reason should name the offending expression"
                    );
                }
                other => panic!("expected InvalidPrimaryKey, got {other:?}"),
            }
        }

        #[test]
        fn function_call_primary_key_errors() {
            assert!(matches!(
                ParserDB::parse::<GenericDialect>(
                    "CREATE TABLE t (a INT, PRIMARY KEY (LOWER(a)));"
                ),
                Err(Error::InvalidPrimaryKey { .. })
            ));
        }

        #[test]
        fn plain_column_primary_key_still_parses() {
            let db = ParserDB::parse::<GenericDialect>(
                "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b));",
            )
            .expect("parse");
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table t");
            let pk: Vec<&str> = table
                .primary_key_columns(&db)
                .expect("pk columns")
                .map(ColumnLike::column_name)
                .collect();
            assert_eq!(pk, vec!["a", "b"]);
        }
    }

    mod alter_table_constraints {
        use sqlparser::dialect::PostgreSqlDialect;

        use super::*;
        use crate::traits::{ColumnLike, ForeignKeyLike, IndexLike, PolicyLike};

        fn parse(sql: &str) -> ParserDB {
            ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse")
        }

        fn primary_key(db: &ParserDB, table_name: &str) -> Vec<String> {
            db.table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table")
                .primary_key_columns(db)
                .expect("pk columns")
                .map(|column| column.column_name().to_string())
                .collect()
        }

        fn unique_index_count(db: &ParserDB, table_name: &str) -> usize {
            db.table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table")
                .unique_indices(db)
                .expect("unique indices")
                .count()
        }

        fn foreign_key_count(db: &ParserDB, table_name: &str) -> usize {
            db.table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table")
                .foreign_keys(db)
                .expect("foreign keys")
                .count()
        }

        #[test]
        fn added_primary_key_answers_as_the_inline_one() {
            let inline = parse("CREATE TABLE t (id uuid PRIMARY KEY, o uuid);");
            let altered = parse(
                "CREATE TABLE t (id uuid NOT NULL, o uuid);
                 ALTER TABLE ONLY t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
            );

            assert_eq!(primary_key(&inline, "t"), primary_key(&altered, "t"));
            assert_eq!(primary_key(&altered, "t"), vec!["id".to_string()]);
            assert_eq!(unique_index_count(&inline, "t"), unique_index_count(&altered, "t"));
            assert_eq!(unique_index_count(&altered, "t"), 1);
        }

        #[test]
        fn unnamed_added_primary_key_is_applied() {
            let db = parse(
                "CREATE TABLE t (id uuid NOT NULL);
                 ALTER TABLE t ADD PRIMARY KEY (id);",
            );
            assert_eq!(primary_key(&db, "t"), vec!["id".to_string()]);
        }

        #[test]
        fn added_unique_constraint_registers_a_unique_index() {
            let db = parse(
                "CREATE TABLE t (id uuid NOT NULL);
                 ALTER TABLE ONLY t ADD CONSTRAINT t_id_key UNIQUE (id);",
            );
            assert_eq!(unique_index_count(&db, "t"), 1);
            assert!(primary_key(&db, "t").is_empty(), "UNIQUE alone is not a primary key");
        }

        #[test]
        fn schema_qualified_alter_table_applies_the_constraint() {
            let db = parse(
                "CREATE TABLE public.t (id uuid NOT NULL);
                 ALTER TABLE ONLY public.t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
            );
            let table = db
                .table_by_target(
                    TargetName::new("t", false).with_schema("public", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .expect("table");
            let pk: Vec<&str> = table
                .primary_key_columns(&db)
                .expect("pk columns")
                .map(ColumnLike::column_name)
                .collect();
            assert_eq!(pk, vec!["id"]);
        }

        #[test]
        fn added_foreign_key_resolves_a_later_declared_target() {
            let db = parse(
                "CREATE TABLE t (id uuid NOT NULL, o uuid);
                 CREATE TABLE u (id uuid PRIMARY KEY);
                 ALTER TABLE ONLY t ADD CONSTRAINT t_o_fkey FOREIGN KEY (o) REFERENCES u(id);",
            );
            assert_eq!(foreign_key_count(&db, "t"), 1);
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table");
            let foreign_key = table
                .foreign_keys(&db)
                .expect("t is in this database")
                .next()
                .expect("the added key");
            assert_eq!(foreign_key.referenced_table_name().name(), "u");
        }

        #[test]
        fn added_check_constraint_is_registered() {
            let db = parse(
                "CREATE TABLE t (id INT NOT NULL);
                 ALTER TABLE t ADD CONSTRAINT t_id_positive CHECK (id > 0);",
            );
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table");
            assert_eq!(table.check_constraints(&db).expect("check constraints").count(), 1);
        }

        #[test]
        fn altering_constraints_preserves_the_rest_of_the_table() {
            let db = parse(
                "CREATE TABLE t (id uuid NOT NULL, o uuid);
                 CREATE INDEX t_o_idx ON t (o);
                 ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                 ALTER TABLE t FORCE ROW LEVEL SECURITY;
                 CREATE POLICY t_all ON t USING (true);
                 ALTER TABLE ONLY t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
            );
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table");

            assert!(table.has_row_level_security(&db).expect("rls"));
            assert!(table.has_forced_row_level_security(&db).expect("forced rls"));

            let columns: Vec<&str> =
                table.columns(&db).expect("columns").map(ColumnLike::column_name).collect();
            assert_eq!(columns, vec!["id", "o"]);
            for column in table.columns(&db).expect("columns") {
                assert!(
                    db.column_metadata(column).is_some(),
                    "re-seated columns must stay findable in the database"
                );
            }

            let indices: Vec<_> = table.indices(&db).expect("indices").collect();
            assert_eq!(indices.len(), 1);
            for index in indices {
                assert_eq!(index.name(), Some("t_o_idx"));
                assert!(
                    db.index_metadata(index).is_some(),
                    "re-seated indexes must stay findable in the database"
                );
            }
            for unique_index in table.unique_indices(&db).expect("unique indices") {
                assert!(
                    db.unique_index_metadata(unique_index).is_some(),
                    "unique indexes must stay findable in the database"
                );
            }

            let policy = db.policies().next().expect("policy");
            assert_eq!(
                PolicyLike::table(policy, &db).expect("policy table").table_name(),
                "t",
                "a policy must still resolve the table it guards"
            );
        }

        #[test]
        fn dropping_a_constraint_undoes_it() {
            let db = parse(
                "CREATE TABLE u (id uuid NOT NULL, PRIMARY KEY (id));
                 CREATE TABLE t (id uuid NOT NULL, o uuid);
                 ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);
                 ALTER TABLE t ADD CONSTRAINT t_o_key UNIQUE (o);
                 ALTER TABLE t ADD CONSTRAINT t_o_fkey FOREIGN KEY (o) REFERENCES u(id);
                 ALTER TABLE t DROP CONSTRAINT t_o_fkey;
                 ALTER TABLE t DROP CONSTRAINT t_o_key;",
            );

            assert_eq!(foreign_key_count(&db, "t"), 0);
            assert_eq!(unique_index_count(&db, "t"), 1, "the primary key index survives");
            assert_eq!(primary_key(&db, "t"), vec!["id".to_string()]);

            let dropped_pk = parse(
                "CREATE TABLE t (id uuid NOT NULL);
                 ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id);
                 ALTER TABLE t DROP CONSTRAINT t_pkey;",
            );
            assert_eq!(primary_key(&dropped_pk, "t"), Vec::<String>::new());
            assert_eq!(unique_index_count(&dropped_pk, "t"), 0);
        }

        #[test]
        fn dropping_an_undeclared_constraint_is_reported() {
            match ParserDB::parse::<PostgreSqlDialect>(
                "CREATE TABLE t (id uuid NOT NULL);
                 ALTER TABLE t DROP CONSTRAINT t_pkey;",
            ) {
                Err(Error::DropConstraintNotFound { table_name, constraint_name }) => {
                    assert_eq!(table_name, "t");
                    assert_eq!(constraint_name, "t_pkey");
                }
                other => panic!("expected DropConstraintNotFound, got {other:?}"),
            }

            let tolerated = parse(
                "CREATE TABLE t (id uuid NOT NULL);
                 ALTER TABLE t DROP CONSTRAINT IF EXISTS t_pkey;",
            );
            assert_eq!(primary_key(&tolerated, "t"), Vec::<String>::new());
        }

        #[test]
        fn altering_an_absent_table_is_reported() {
            match ParserDB::parse::<PostgreSqlDialect>(
                "ALTER TABLE ONLY t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
            ) {
                Err(Error::AlterTableNotFound { table_name }) => assert_eq!(table_name, "t"),
                other => panic!("expected AlterTableNotFound, got {other:?}"),
            }

            let tolerated = ParserDB::parse::<PostgreSqlDialect>(
                "ALTER TABLE IF EXISTS t ADD CONSTRAINT t_pkey PRIMARY KEY (id);",
            )
            .expect("IF EXISTS tolerates an absent table");
            assert!(
                tolerated
                    .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn constraints_naming_an_undeclared_column_are_reported() {
            for sql in [
                "ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (missing);",
                "ALTER TABLE t ADD CONSTRAINT t_key UNIQUE (missing);",
            ] {
                let sql = format!("CREATE TABLE t (id uuid NOT NULL);\n{sql}");
                assert!(
                    matches!(
                        ParserDB::parse::<PostgreSqlDialect>(&sql),
                        Err(Error::IdentifierLookupError(LookupError::ColumnNotFound {
                            ref table_name,
                            ref column_name,
                        })) if table_name == "t" && column_name == "missing"
                    ),
                    "expected ColumnNotFound for `{sql}`"
                );
            }

            assert!(matches!(
                ParserDB::parse::<PostgreSqlDialect>(
                    "CREATE TABLE u (id uuid PRIMARY KEY);
                     CREATE TABLE t (id uuid NOT NULL);
                     ALTER TABLE t ADD CONSTRAINT t_fkey FOREIGN KEY (missing) REFERENCES u(id);",
                ),
                Err(Error::HostColumnNotFoundForForeignKey { .. })
            ));

            assert!(matches!(
                ParserDB::parse::<PostgreSqlDialect>(
                    "CREATE TABLE t (id uuid NOT NULL, o uuid);
                     ALTER TABLE t ADD CONSTRAINT t_fkey FOREIGN KEY (o) REFERENCES u(id);",
                ),
                Err(Error::ReferencedTableNotFoundForForeignKey { .. })
            ));
        }
    }
}
