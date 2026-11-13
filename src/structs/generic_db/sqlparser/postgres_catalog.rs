use alloc::{borrow::Cow, string::String, sync::Arc};

use sqlparser::ast::{CreateFunction, ObjectName, OperateFunctionArg};

use super::{
    postgres_18_collations::POSTGRES_18_COLLATIONS,
    postgres_18_operators::POSTGRES_18_OPERATORS,
    postgres_cast::{POSTGRES_18_CASTS, PostgresCatalogCast},
    postgres_operator::PostgresCatalogOperator,
    type_arrays::TypeArrays,
};
use crate::{
    errors::ObjectKind,
    structs::{IdentifierCase, TargetName},
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{PG_CATALOG, object_name_last_part, qualifier_of},
    },
};

/// The schema an object stored without a schema lives in.
const DEFAULT_SCHEMA: &str = "public";

/// The types PostgreSQL 18 marks collatable in `pg_catalog`.
static POSTGRES_18_COLLATABLE_TYPES: &[PostgresCatalogType] = &[
    PostgresCatalogType::built_in("bpchar"),
    PostgresCatalogType::built_in("name"),
    PostgresCatalogType::built_in("text"),
    PostgresCatalogType::built_in("varchar"),
    PostgresCatalogType::built_in("_bpchar"),
    PostgresCatalogType::built_in("_name"),
    PostgresCatalogType::built_in("_text"),
    PostgresCatalogType::built_in("_varchar"),
];

/// PostgreSQL catalog facts used while validating DDL.
///
/// The built-in facts are borrowed from a static table, so building the
/// PostgreSQL 18 catalog allocates nothing. Adding a fact copies the table into
/// an owned list once.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostgresCatalog {
    collations: Cow<'static, [PostgresCatalogCollation]>,
    collatable_types: Cow<'static, [PostgresCatalogType]>,
    operators: Cow<'static, [PostgresCatalogOperator]>,
    casts: Cow<'static, [PostgresCatalogCast]>,
}

/// A catalog fact that keeps an object from dropping without `CASCADE`.
pub(crate) struct CatalogDependent {
    /// [`ObjectKind::Operator`] or [`ObjectKind::Cast`].
    pub(crate) kind: ObjectKind,
    /// The operator symbol or the `(source AS target)` cast spelling.
    pub(crate) name: String,
}

/// A PostgreSQL collation identity and its deterministic flag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostgresCatalogCollation {
    identity: CatalogName,
    deterministic: bool,
}

/// A PostgreSQL type whose values can carry a collation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostgresCatalogType {
    identity: CatalogName,
}

/// The optional schema and the name of a catalog fact, each with its quoting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CatalogName {
    schema: Option<Cow<'static, str>>,
    schema_is_quoted: bool,
    name: Cow<'static, str>,
    name_is_quoted: bool,
}

impl CatalogName {
    /// A name in `pg_catalog`, which is unquoted.
    pub(super) const fn in_pg_catalog(name: Cow<'static, str>, name_is_quoted: bool) -> Self {
        Self {
            schema: Some(Cow::Borrowed(PG_CATALOG)),
            schema_is_quoted: false,
            name,
            name_is_quoted,
        }
    }

    /// The name `name` in the schema `schema`, each with its quoting.
    pub(super) fn new(schema: (&str, bool), name: (&str, bool)) -> Self {
        Self {
            schema: Some(Cow::Owned(String::from(schema.0))),
            schema_is_quoted: schema.1,
            name: Cow::Owned(String::from(name.0)),
            name_is_quoted: name.1,
        }
    }

    pub(super) fn set_schema(&mut self, schema: String, schema_is_quoted: bool) {
        self.schema = Some(Cow::Owned(schema));
        self.schema_is_quoted = schema_is_quoted;
    }

    pub(crate) fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub(crate) const fn schema_is_quoted(&self) -> bool {
        self.schema_is_quoted
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    /// The name the identity reads as, with its schema.
    pub(crate) fn target_name(&self) -> TargetName<'_> {
        let mut target = TargetName::new(&self.name, self.name_is_quoted);
        if let Some(schema) = self.schema.as_deref() {
            target = target.with_schema(schema, self.schema_is_quoted);
        }
        target
    }

    /// Whether the identity's schema is the one `schema` names.
    pub(crate) fn schema_matches(&self, schema: &str, quoted: bool) -> bool {
        self.schema
            .as_deref()
            .is_some_and(|stored| identifiers_match(stored, self.schema_is_quoted, schema, quoted))
    }

    /// Moves the identity's schema from `from` to `to`, answering whether it
    /// moved.
    pub(super) fn rename_schema(
        &mut self,
        from: &str,
        from_quoted: bool,
        to: &str,
        to_quoted: bool,
    ) -> bool {
        if !self.schema_matches(from, from_quoted) {
            return false;
        }
        self.set_schema(String::from(to), to_quoted);
        true
    }

    /// Whether the identity names the type `name` writes, an unqualified
    /// name reading as `public`.
    pub(crate) fn names_type(&self, name: &ObjectName) -> bool {
        let Some((written, written_quoted)) = object_name_last_part(name) else {
            return false;
        };
        if !identifiers_match(self.name(), self.name_is_quoted(), written, written_quoted) {
            return false;
        }
        let (schema, schema_quoted) = qualifier_of(name).named().unwrap_or((DEFAULT_SCHEMA, false));
        self.schema_matches(schema, schema_quoted)
    }

    /// Stores the identity `name` writes, an absent qualifier reading as
    /// `public`.
    pub(super) fn set_object_name(&mut self, name: &ObjectName) {
        let (stored, stored_quoted) = object_name_last_part(name).unwrap_or(("", false));
        self.name = Cow::Owned(String::from(stored));
        self.name_is_quoted = stored_quoted;
        let (schema, schema_quoted) = qualifier_of(name).named().unwrap_or((DEFAULT_SCHEMA, false));
        self.set_schema(String::from(schema), schema_quoted);
    }
}

impl Default for PostgresCatalog {
    fn default() -> Self {
        Self::postgres_18()
    }
}

impl PostgresCatalog {
    /// Creates a catalog with no configured facts.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            collations: Cow::Borrowed(&[]),
            collatable_types: Cow::Borrowed(&[]),
            operators: Cow::Borrowed(&[]),
            casts: Cow::Borrowed(&[]),
        }
    }

    /// Creates the built-in PostgreSQL 18 catalog facts.
    #[must_use]
    pub const fn postgres_18() -> Self {
        Self {
            collations: Cow::Borrowed(POSTGRES_18_COLLATIONS),
            collatable_types: Cow::Borrowed(POSTGRES_18_COLLATABLE_TYPES),
            operators: Cow::Borrowed(POSTGRES_18_OPERATORS),
            casts: Cow::Borrowed(POSTGRES_18_CASTS),
        }
    }

    /// Adds or replaces a collation fact.
    #[must_use]
    pub fn with_collation(mut self, collation: PostgresCatalogCollation) -> Self {
        let collations = self.collations.to_mut();
        collations.retain(|held| held.identity != collation.identity);
        collations.push(collation);
        self
    }

    /// Adds or replaces a collatable type fact.
    #[must_use]
    pub fn with_collatable_type(mut self, ty: PostgresCatalogType) -> Self {
        let collatable_types = self.collatable_types.to_mut();
        collatable_types.retain(|held| held.identity != ty.identity);
        collatable_types.push(ty);
        self
    }

    /// Adds or replaces the operator with the same symbol and operand types.
    #[must_use]
    pub fn with_operator(mut self, operator: PostgresCatalogOperator) -> Self {
        let operators = self.operators.to_mut();
        operators.retain(|held| !held.repeats(&operator));
        operators.push(operator);
        self
    }

    /// The operator facts as an owned list, copying the borrowed table once.
    pub(super) fn operators_mut(&mut self) -> &mut Vec<PostgresCatalogOperator> {
        self.operators.to_mut()
    }

    /// The operator facts in insertion order, as one slice.
    pub(super) fn operators_slice(&self) -> &[PostgresCatalogOperator] {
        &self.operators
    }

    /// Adds or replaces the cast between the same source and target types.
    #[must_use]
    pub fn with_cast(mut self, cast: PostgresCatalogCast) -> Self {
        let casts = self.casts.to_mut();
        casts.retain(|held| !held.repeats(&cast));
        casts.push(cast);
        self
    }

    /// Returns the collation facts in insertion order.
    // Clippy versions disagree on whether an opaque iterator return needs an
    // explicit `must_use`, so carry the attribute and silence the redundancy.
    #[allow(clippy::double_must_use)]
    #[must_use]
    pub fn collations(&self) -> impl DoubleEndedIterator<Item = &PostgresCatalogCollation> {
        self.collations.iter()
    }

    /// Returns the collatable type facts in insertion order.
    pub fn collatable_types(&self) -> impl Iterator<Item = &PostgresCatalogType> {
        self.collatable_types.iter()
    }

    /// Returns the operator facts in insertion order, shells included.
    #[expect(
        clippy::double_must_use,
        reason = "some clippy versions read an opaque iterator return as already must_use"
    )]
    #[must_use]
    pub fn operators(&self) -> impl Iterator<Item = &PostgresCatalogOperator> {
        self.operators.iter()
    }

    /// Returns the cast facts in insertion order.
    #[expect(
        clippy::double_must_use,
        reason = "some clippy versions read an opaque iterator return as already must_use"
    )]
    #[must_use]
    pub fn casts(&self) -> impl Iterator<Item = &PostgresCatalogCast> {
        self.casts.iter()
    }

    /// Moves the facts the schema rename carries, collations, operators and
    /// casts, leaving every fact it does not touch in place.
    pub(crate) fn rename_schema(
        &mut self,
        from: &str,
        from_quoted: bool,
        to: &str,
        to_quoted: bool,
    ) {
        let in_renamed_schema = |collation: &PostgresCatalogCollation| {
            let identity = &collation.identity;
            identity.schema().is_some_and(|schema| {
                identifiers_match(schema, identity.schema_is_quoted(), from, from_quoted)
            })
        };
        if self.collations.iter().any(in_renamed_schema) {
            for collation in self.collations.to_mut() {
                if in_renamed_schema(collation) {
                    collation.identity.set_schema(String::from(to), to_quoted);
                }
            }
        }
        if self.operators.iter().any(|operator| operator.holds_schema(from, from_quoted)) {
            for operator in self.operators.to_mut() {
                operator.follow_schema_rename(from, from_quoted, to, to_quoted);
            }
        }
        if self.casts.iter().any(|cast| cast.touches_schema(from, from_quoted)) {
            for cast in
                self.casts.to_mut().iter_mut().filter(|cast| cast.touches_schema(from, from_quoted))
            {
                cast.follow_schema_rename(from, from_quoted, to, to_quoted);
            }
        }
    }

    /// Whether an operator the catalog holds lives in `schema`, which a
    /// schema drop without `CASCADE` would leave behind.
    pub(crate) fn holds_operators_in_schema(&self, schema: &str, quoted: bool) -> bool {
        self.operators.iter().any(|operator| operator.lives_in_schema(schema, quoted))
    }

    /// Drops the operators of `schema` and every operator or cast a dropped
    /// object of the schema still runs, which PostgreSQL drops with it.
    pub(crate) fn drop_schema(&mut self, schema: &str, quoted: bool) {
        self.remove_operators(|_, operator| operator.touches_schema(schema, quoted));
        if self.casts.iter().any(|cast| cast.touches_schema(schema, quoted)) {
            self.casts.to_mut().retain(|cast| !cast.touches_schema(schema, quoted));
        }
    }

    /// The operator or cast that names the type `name` writes, which a drop
    /// without `CASCADE` of the type would leave behind.
    pub(crate) fn dependent_naming_type(&self, name: &ObjectName) -> Option<CatalogDependent> {
        if let Some(operator) = self.operators.iter().find(|operator| operator.uses_type(name)) {
            return Some(CatalogDependent {
                kind: ObjectKind::Operator,
                name: String::from(operator.name()),
            });
        }
        self.casts
            .iter()
            .find(|cast| cast.uses_type(name))
            .map(|cast| CatalogDependent { kind: ObjectKind::Cast, name: cast.described() })
    }

    /// Drops the operators and casts naming a type of `names`, which
    /// PostgreSQL drops with the type under `CASCADE`.
    pub(crate) fn drop_facts_naming_types(&mut self, names: &[ObjectName]) {
        self.remove_operators(|_, operator| names.iter().any(|name| operator.uses_type(name)));
        let cast_naming =
            |cast: &PostgresCatalogCast| names.iter().any(|name| cast.uses_type(name));
        if self.casts.iter().any(cast_naming) {
            self.casts.to_mut().retain(|cast| !cast_naming(cast));
        }
    }

    /// Tests whether an operator or a cast runs this exact function signature.
    pub(super) fn fact_declares_function(
        &self,
        type_arrays: &TypeArrays,
        name: &ObjectName,
        args: Option<&[OperateFunctionArg]>,
        case: IdentifierCase,
    ) -> bool {
        self.operators
            .iter()
            .any(|operator| operator.declares_function(type_arrays, name, args, case))
            || self.casts.iter().any(|cast| cast.declares_function(type_arrays, name, args, case))
    }

    /// Repoints operators and casts running the renamed function's exact
    /// signature.
    pub(super) fn rename_function(
        &mut self,
        type_arrays: &TypeArrays,
        from: &ObjectName,
        from_args: Option<&[OperateFunctionArg]>,
        to: &ObjectName,
        case: IdentifierCase,
    ) {
        let moves = |operator: &PostgresCatalogOperator| {
            operator.declares_function(type_arrays, from, from_args, case)
        };
        if self.operators.iter().any(moves) {
            for operator in self.operators.to_mut().iter_mut().filter(|operator| moves(operator)) {
                operator.follow_function_rename(type_arrays, from, from_args, to, case);
            }
        }
        if self.casts.iter().any(|cast| cast.declares_function(type_arrays, from, from_args, case))
        {
            for cast in self.casts.to_mut() {
                cast.follow_function_rename(type_arrays, from, from_args, to, case);
            }
        }
    }

    /// Repoints every operator and cast identity naming the type `from` at
    /// `to`, the identity the type is stored under afterwards.
    pub(crate) fn rename_type_identity(&mut self, from: &ObjectName, to: &ObjectName) {
        let renames = |operator: &PostgresCatalogOperator| operator.uses_type(from);
        if self.operators.iter().any(renames) {
            for operator in self.operators.to_mut().iter_mut().filter(|operator| renames(operator))
            {
                operator.rename_type_use(from, to);
            }
        }
        if self.casts.iter().any(|cast| cast.holds_type(from)) {
            for cast in self.casts.to_mut().iter_mut().filter(|cast| cast.holds_type(from)) {
                cast.rename_type_use(from, to);
            }
        }
    }

    /// Removes operators and casts running the dropped function's exact
    /// signature.
    pub(super) fn drop_facts_declaring_function(
        &mut self,
        type_arrays: &TypeArrays,
        name: &ObjectName,
        args: Option<&[OperateFunctionArg]>,
        case: IdentifierCase,
    ) {
        self.drop_facts_running(type_arrays, case, &core::iter::once((name, args)));
    }

    /// Removes operators and casts running any of `functions`, leaving the
    /// borrowed lists alone when none does.
    pub(super) fn drop_facts_declaring_functions(
        &mut self,
        type_arrays: &TypeArrays,
        functions: &[Arc<CreateFunction>],
        case: IdentifierCase,
    ) {
        let signatures =
            functions.iter().map(|function| (&function.name, function.args.as_deref()));
        self.drop_facts_running(type_arrays, case, &signatures);
    }

    /// Removes operators and casts running any of `signatures`.
    fn drop_facts_running<'f>(
        &mut self,
        type_arrays: &TypeArrays,
        case: IdentifierCase,
        signatures: &(impl Iterator<Item = (&'f ObjectName, Option<&'f [OperateFunctionArg]>)> + Clone),
    ) {
        self.remove_operators(|_, operator| {
            signatures
                .clone()
                .any(|(name, args)| operator.declares_function(type_arrays, name, args, case))
        });
        let cast_runs = |cast: &PostgresCatalogCast| {
            signatures
                .clone()
                .any(|(name, args)| cast.declares_function(type_arrays, name, args, case))
        };
        if self.casts.iter().any(cast_runs) {
            self.casts.to_mut().retain(|cast| !cast_runs(cast));
        }
    }

    /// Removes the operators `doomed` holds for, given each one's position,
    /// clearing the commutator and negator links the others keep to them,
    /// and leaves the borrowed list alone when it holds for none.
    pub(super) fn remove_operators(
        &mut self,
        doomed: impl Fn(usize, &PostgresCatalogOperator) -> bool,
    ) {
        if !self.operators.iter().enumerate().any(|(position, operator)| doomed(position, operator))
        {
            return;
        }
        let operators = self.operators.to_mut();
        let mut position = 0;
        let removed: Vec<PostgresCatalogOperator> = operators
            .extract_if(.., |operator| {
                let gone = doomed(position, operator);
                position += 1;
                gone
            })
            .collect();
        for operator in operators.iter_mut() {
            if removed.iter().any(|gone| operator.commutes_with(gone)) {
                operator.commutator = None;
            }
            if removed.iter().any(|gone| operator.negates(gone)) {
                operator.negator = None;
            }
        }
    }

    /// Moves the operator at `position` to `schema`, the commutator and
    /// negator links the others keep to it following.
    pub(super) fn move_operator(&mut self, position: usize, schema: (&str, bool)) {
        let operators = self.operators.to_mut();
        let moved = &operators[position];
        let linking: Vec<(bool, bool)> = operators
            .iter()
            .map(|operator| (operator.commutes_with(moved), operator.negates(moved)))
            .collect();
        operators[position].target.set_schema(String::from(schema.0), schema.1);
        for (operator, (commutes, negates)) in operators.iter_mut().zip(linking) {
            let links = [
                commutes.then_some(&mut operator.commutator),
                negates.then_some(&mut operator.negator),
            ];
            for link in links.into_iter().flatten().flatten() {
                link.set_schema(String::from(schema.0), schema.1);
            }
        }
    }
}

impl PostgresCatalogCollation {
    /// Creates a collation in `pg_catalog`.
    #[must_use]
    pub fn new(name: impl Into<String>, name_is_quoted: bool) -> Self {
        Self {
            identity: CatalogName::in_pg_catalog(Cow::Owned(name.into()), name_is_quoted),
            deterministic: true,
        }
    }

    /// Creates a deterministic built-in collation in `pg_catalog`.
    pub(super) const fn built_in(name: &'static str, name_is_quoted: bool) -> Self {
        Self {
            identity: CatalogName::in_pg_catalog(Cow::Borrowed(name), name_is_quoted),
            deterministic: true,
        }
    }

    /// Stores the schema that owns this collation.
    #[must_use]
    pub fn with_schema(mut self, schema: impl Into<String>, schema_is_quoted: bool) -> Self {
        self.identity.set_schema(schema.into(), schema_is_quoted);
        self
    }

    /// Stores whether this collation is deterministic.
    #[must_use]
    pub const fn with_deterministic(mut self, deterministic: bool) -> Self {
        self.deterministic = deterministic;
        self
    }

    /// Returns the owning schema.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.identity.schema()
    }

    /// Returns whether the schema is quoted.
    #[must_use]
    pub const fn schema_is_quoted(&self) -> bool {
        self.identity.schema_is_quoted
    }

    /// Returns the collation name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.identity.name
    }

    /// Returns whether the collation name is quoted.
    #[must_use]
    pub const fn name_is_quoted(&self) -> bool {
        self.identity.name_is_quoted
    }

    /// Returns whether the collation is deterministic.
    #[must_use]
    pub const fn deterministic(&self) -> bool {
        self.deterministic
    }

    pub(crate) const fn identity(&self) -> &CatalogName {
        &self.identity
    }
}

impl PostgresCatalogType {
    /// Creates a collatable type in `pg_catalog`.
    #[must_use]
    pub fn new(name: impl Into<String>, name_is_quoted: bool) -> Self {
        Self { identity: CatalogName::in_pg_catalog(Cow::Owned(name.into()), name_is_quoted) }
    }

    /// Creates a built-in collatable type in `pg_catalog`, whose name is
    /// unquoted.
    const fn built_in(name: &'static str) -> Self {
        Self { identity: CatalogName::in_pg_catalog(Cow::Borrowed(name), false) }
    }

    /// Stores the schema that owns this type.
    #[must_use]
    pub fn with_schema(mut self, schema: impl Into<String>, schema_is_quoted: bool) -> Self {
        self.identity.set_schema(schema.into(), schema_is_quoted);
        self
    }

    /// Returns the owning schema.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.identity.schema()
    }

    /// Returns whether the schema is quoted.
    #[must_use]
    pub const fn schema_is_quoted(&self) -> bool {
        self.identity.schema_is_quoted
    }

    /// Returns the type name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.identity.name
    }

    /// Returns whether the type name is quoted.
    #[must_use]
    pub const fn name_is_quoted(&self) -> bool {
        self.identity.name_is_quoted
    }

    pub(crate) const fn identity(&self) -> &CatalogName {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType};

    /// A borrowed built-in fact and the same fact built from owned text are one
    /// value, which is what lets a supplied fact replace a built-in.
    #[test]
    fn built_in_facts_equal_their_owned_spelling() {
        assert_eq!(
            PostgresCatalogCollation::built_in("C", true),
            PostgresCatalogCollation::new("C", true)
        );
        assert_eq!(PostgresCatalogType::built_in("text"), PostgresCatalogType::new("text", false));
    }

    /// `with_collation` replaces by identity, so the built-in table it starts
    /// from must already hold each identity once.
    #[test]
    fn built_in_facts_hold_each_identity_once() {
        let catalog = PostgresCatalog::postgres_18();
        let mut collations: Vec<_> = catalog
            .collations()
            .map(|c| (c.schema(), c.schema_is_quoted(), c.name(), c.name_is_quoted()))
            .collect();
        let held = collations.len();
        collations.sort_unstable();
        collations.dedup();
        assert_eq!(collations.len(), held);

        let mut types: Vec<_> = catalog
            .collatable_types()
            .map(|t| (t.schema(), t.schema_is_quoted(), t.name(), t.name_is_quoted()))
            .collect();
        let held = types.len();
        types.sort_unstable();
        types.dedup();
        assert_eq!(types.len(), held);
    }
}
