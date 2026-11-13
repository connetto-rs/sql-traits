//! PostgreSQL operator declarations and exact implementation links.

use alloc::{borrow::Cow, string::String};
use core::borrow::Borrow;

use sqlparser::ast::{ArgMode, CreateFunction, DataType, ObjectName, OperateFunctionArg};

use super::{postgres_catalog::CatalogName, type_arrays::TypeArrays};
use crate::{
    structs::{IdentifierCase, TargetName, generic_db::sqlparser::ParserDB},
    traits::{DatabaseLike, Metadata, OperatorKind, OperatorLike},
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        object_name::PG_CATALOG,
        postgres_types::PgType,
    },
};

/// The schema an operand identity stored without one lives in.
const DEFAULT_SCHEMA: &str = "public";

/// A PostgreSQL operator's declared signature and optional implementation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostgresCatalogOperator {
    /// The operator's schema-qualified symbol.
    target: CatalogName,
    /// The operand positions, as the stored row has them.
    kind: OperatorKind,
    /// The left operand's type, absent on a prefix operator.
    left: Option<CatalogName>,
    /// The right operand's type.
    right: CatalogName,
    /// The result's type, absent on a shell.
    result: Option<CatalogName>,
    /// The implementing function, absent on a shell.
    implementation: Option<CatalogName>,
}

impl Metadata for PostgresCatalogOperator {
    type Meta = ();
}

impl PostgresCatalogOperator {
    /// Creates a shell in `pg_catalog`, with `None` on the left marking a
    /// prefix operator.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        left: Option<TargetName<'_>>,
        right: TargetName<'_>,
    ) -> Self {
        Self {
            target: CatalogName::in_pg_catalog(Cow::Owned(name.into()), false),
            kind: match left.as_ref() {
                Some(_) => OperatorKind::Binary,
                None => OperatorKind::Prefix,
            },
            left: left.map(operand_type),
            right: operand_type(right),
            result: None,
            implementation: None,
        }
    }

    /// Stores the schema that owns this operator.
    #[must_use]
    pub fn with_schema(mut self, schema: impl Into<String>, schema_is_quoted: bool) -> Self {
        self.target.set_schema(schema.into(), schema_is_quoted);
        self
    }

    /// Sets the implementing function and result type.
    #[must_use]
    pub fn with_implementation(mut self, function: TargetName<'_>, result: TargetName<'_>) -> Self {
        self.implementation = Some(operand_type(function));
        self.result = Some(operand_type(result));
        self
    }

    /// Creates a built-in declaration with borrowed names.
    pub(super) const fn built_in(
        name: &'static str,
        left: Option<&'static str>,
        right: &'static str,
        result: &'static str,
        function: &'static str,
    ) -> Self {
        Self {
            target: CatalogName::in_pg_catalog(Cow::Borrowed(name), false),
            kind: match left {
                Some(_) => OperatorKind::Binary,
                None => OperatorKind::Prefix,
            },
            left: match left {
                Some(left) => Some(CatalogName::in_pg_catalog(Cow::Borrowed(left), false)),
                None => None,
            },
            right: CatalogName::in_pg_catalog(Cow::Borrowed(right), false),
            result: Some(CatalogName::in_pg_catalog(Cow::Borrowed(result), false)),
            implementation: Some(CatalogName::in_pg_catalog(Cow::Borrowed(function), false)),
        }
    }

    /// Returns the schema that owns the operator.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.target.schema()
    }

    /// Returns whether the operator's schema is quoted.
    #[must_use]
    pub const fn schema_is_quoted(&self) -> bool {
        self.target.schema_is_quoted()
    }

    /// Returns the operator symbol.
    #[must_use]
    pub fn name(&self) -> &str {
        self.target.name()
    }

    /// Returns whether the operator symbol is quoted.
    #[must_use]
    pub const fn name_is_quoted(&self) -> bool {
        self.target.name_is_quoted()
    }

    /// Matches the operator symbol and operand identities for catalog
    /// replacement.
    pub(super) fn repeats(&self, other: &Self) -> bool {
        schema_repeats(&self.target, &other.target)
            && self.target.name() == other.target.name()
            && operand_repeats(self.left.as_ref(), other.left.as_ref())
            && operand_repeats(Some(&self.right), Some(&other.right))
    }

    /// Tests whether the operator belongs to `schema`.
    pub(super) fn lives_in_schema(&self, schema: &str, quoted: bool) -> bool {
        self.target.schema_matches(schema, quoted)
    }

    /// Whether any identity the fact holds lives in `schema`.
    pub(super) fn touches_schema(&self, schema: &str, quoted: bool) -> bool {
        self.target.schema_matches(schema, quoted)
            || self.left.as_ref().is_some_and(|type_| type_.schema_matches(schema, quoted))
            || self.right.schema_matches(schema, quoted)
            || self.result.as_ref().is_some_and(|type_| type_.schema_matches(schema, quoted))
            || self
                .implementation
                .as_ref()
                .is_some_and(|function| function.schema_matches(schema, quoted))
    }

    /// Moves every identity the fact holds in `from` to `to`.
    pub(super) fn follow_schema_rename(
        &mut self,
        from: &str,
        from_quoted: bool,
        to: &str,
        to_quoted: bool,
    ) {
        self.target.rename_schema(from, from_quoted, to, to_quoted);
        if let Some(left) = &mut self.left {
            left.rename_schema(from, from_quoted, to, to_quoted);
        }
        self.right.rename_schema(from, from_quoted, to, to_quoted);
        if let Some(result) = &mut self.result {
            result.rename_schema(from, from_quoted, to, to_quoted);
        }
        if let Some(implementation) = &mut self.implementation {
            implementation.rename_schema(from, from_quoted, to, to_quoted);
        }
    }

    /// Tests whether an operand or result names `name`, defaulting its schema
    /// to `public`.
    pub(super) fn uses_type(&self, name: &ObjectName) -> bool {
        [self.left.as_ref(), Some(&self.right), self.result.as_ref()]
            .into_iter()
            .flatten()
            .any(|type_| type_.names_type(name))
    }

    /// Matches the declared function's identity and input signature.
    pub(super) fn declares_function(
        &self,
        type_arrays: &TypeArrays,
        name: &ObjectName,
        args: Option<&[OperateFunctionArg]>,
        case: IdentifierCase,
    ) -> bool {
        let Some(implementation) = self.implementation.as_ref() else {
            return false;
        };
        function_name_matches(implementation, name, case)
            && takes_exactly(self.operands(), type_arrays, args)
    }

    /// Repoints the implementing function's stored identity.
    pub(super) fn rename_implementation(&mut self, to: &ObjectName) {
        if let Some(implementation) = self.implementation.as_mut() {
            implementation.set_object_name(to);
        }
    }

    /// Repoints operands and results at the identity the type is stored under
    /// after a rename or a schema move.
    pub(super) fn rename_type_use(&mut self, from: &ObjectName, to: &ObjectName) {
        for type_ in [self.left.as_mut(), Some(&mut self.right), self.result.as_mut()] {
            if let Some(type_) = type_.filter(|type_| type_.names_type(from)) {
                type_.set_object_name(to);
            }
        }
    }

    /// The operand types, the left one first when there is one.
    fn operands(&self) -> impl Iterator<Item = &CatalogName> {
        self.left.iter().chain([&self.right])
    }
}

impl OperatorLike for PostgresCatalogOperator {
    type DB = ParserDB;

    fn target_name(&self) -> TargetName<'_> {
        self.target.target_name()
    }

    fn kind(&self) -> OperatorKind {
        self.kind
    }

    fn left_type<'db>(&'db self, _database: &'db ParserDB) -> Option<TargetName<'db>> {
        self.left.as_ref().map(CatalogName::target_name)
    }

    fn right_type<'db>(&'db self, _database: &'db ParserDB) -> Option<TargetName<'db>> {
        Some(self.right.target_name())
    }

    fn result_type<'db>(&'db self, _database: &'db ParserDB) -> Option<TargetName<'db>> {
        self.result.as_ref().map(CatalogName::target_name)
    }

    /// Finds the declared function without coercion or overload selection.
    fn implementation<'db>(&'db self, database: &'db ParserDB) -> Option<&'db CreateFunction> {
        let implementation = self.implementation.as_ref()?;
        let case = database.identifier_case();
        let type_arrays = &database.ingestion.type_arrays;
        database.functions().find(|function| {
            function_name_matches(implementation, &function.name, case)
                && takes_exactly(self.operands(), type_arrays, function.args.as_deref())
        })
    }
}

/// Stores a supplied identity, defaulting its schema to `pg_catalog`.
pub(super) fn operand_type(target: TargetName<'_>) -> CatalogName {
    let (name, name_is_quoted, schema, schema_is_quoted) = target.into_parts();
    let mut identity = CatalogName::in_pg_catalog(Cow::Owned(name.into_owned()), name_is_quoted);
    if let Some(schema) = schema {
        identity.set_schema(schema.into_owned(), schema_is_quoted);
    }
    identity
}

/// Compares a stored function identity without allocating an AST name.
pub(super) fn function_name_matches(
    identity: &CatalogName,
    name: &ObjectName,
    case: IdentifierCase,
) -> bool {
    let Some((written, quoted)) = super::object_name_last_part(name) else {
        return false;
    };
    let qualifier = super::qualifier_of(name);
    !matches!(qualifier, super::Qualifier::RunTime)
        && case.identifiers_match(identity.name(), identity.name_is_quoted(), written, quoted)
        && super::schema_qualifiers_match(
            identity.schema().map(|schema| (schema, identity.schema_is_quoted())),
            qualifier.named(),
            case,
        )
}

/// Whether the input arguments of `args` take exactly the types `inputs`
/// identifies, in order.
pub(super) fn takes_exactly(
    inputs: impl IntoIterator<Item = impl Borrow<CatalogName>>,
    type_arrays: &TypeArrays,
    args: Option<&[OperateFunctionArg]>,
) -> bool {
    let mut written =
        args.unwrap_or_default().iter().filter(|argument| argument.mode != Some(ArgMode::Out));
    let mut inputs = inputs.into_iter();
    loop {
        match (written.next(), inputs.next()) {
            (None, None) => return true,
            (Some(argument), Some(input)) => {
                if !type_identity(type_arrays, &argument.data_type)
                    .is_some_and(|written| identity_repeats(&written, input.borrow()))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// The `pg_type` identity a bound declaration names, or `None` for a spelling
/// no stored type has.
pub(super) fn type_identity(type_arrays: &TypeArrays, data_type: &DataType) -> Option<CatalogName> {
    pg_type_identity(type_arrays, PgType::of(data_type))
}

/// The `pg_type` identity of `pg_type`, an array reaching the stored array
/// type of its element.
fn pg_type_identity(type_arrays: &TypeArrays, pg_type: PgType) -> Option<CatalogName> {
    match pg_type {
        PgType::Catalog(name) => Some(CatalogName::new((PG_CATALOG, false), (name, false))),
        PgType::Named(target) => Some(target_identity(&target)),
        PgType::Array(element) => {
            let element = pg_type_identity(type_arrays, *element)?;
            let array = type_arrays.array_in(
                element.schema()?,
                element.schema_is_quoted(),
                element.name(),
                element.name_is_quoted(),
            )?;
            Some(target_identity(&array))
        }
        PgType::Other(_) => None,
    }
}

/// The identity `target` names in its canonical spelling, quoted only where
/// folding would change it, an absent schema reading as `public`.
fn target_identity(target: &TargetName<'_>) -> CatalogName {
    let canonical = |name, quoted| {
        let stored = stored_identifier(name, quoted);
        let quoted = stored.bytes().any(|byte| byte.is_ascii_uppercase());
        (stored, quoted)
    };
    let (schema, schema_quoted) =
        target.schema().map_or((Cow::Borrowed(DEFAULT_SCHEMA), false), |schema| {
            canonical(schema, target.schema_is_quoted())
        });
    let (name, name_quoted) = canonical(target.name(), target.name_is_quoted());
    CatalogName::new((&schema, schema_quoted), (&name, name_quoted))
}

/// Compares schema identities using PostgreSQL identifier rules.
fn schema_repeats(left: &CatalogName, right: &CatalogName) -> bool {
    match (left.schema(), right.schema()) {
        (Some(schema), Some(other)) => {
            identifiers_match(schema, left.schema_is_quoted(), other, right.schema_is_quoted())
        }
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// Whether two catalog names hold the same schema-qualified identifier.
pub(super) fn identity_repeats(left: &CatalogName, right: &CatalogName) -> bool {
    schema_repeats(left, right)
        && identifiers_match(
            left.name(),
            left.name_is_quoted(),
            right.name(),
            right.name_is_quoted(),
        )
}

/// Whether two operand identities hold the same type, or both hold none.
fn operand_repeats(left: Option<&CatalogName>, right: Option<&CatalogName>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => identity_repeats(left, right),
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
    }
}
