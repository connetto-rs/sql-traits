//! Generated array ownership follows PostgreSQL 18's `pg_type` naming rules.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{ArrayElemTypeDef, DataType, Ident, ObjectName, ObjectNamePart};

use super::{
    ParserDBBuilder,
    domains::{Reached, reach_in_schema},
    generated_names::{object_name, spelled},
};
use crate::{
    errors::{Error, ObjectKind},
    impls::SqlparserDialect,
    structs::{IdentifierCase, TargetName},
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema, stored_identifier},
        object_name::{
            PG_CATALOG, TEMPORARY_SCHEMA, object_name_identifiers, object_name_last_part,
            qualifier_of,
        },
    },
};

/// The schema a type stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

/// Element types with generated arrays in PostgreSQL 18's catalog.
const BUILTIN_ARRAY_ELEMENTS: &[&str] = &[
    "aclitem",
    "bit",
    "bool",
    "box",
    "bpchar",
    "bytea",
    "char",
    "cid",
    "cidr",
    "circle",
    "cstring",
    "date",
    "datemultirange",
    "daterange",
    "float4",
    "float8",
    "gtsvector",
    "inet",
    "int2",
    "int2vector",
    "int4",
    "int4multirange",
    "int4range",
    "int8",
    "int8multirange",
    "int8range",
    "interval",
    "json",
    "jsonb",
    "jsonpath",
    "line",
    "lseg",
    "macaddr",
    "macaddr8",
    "money",
    "name",
    "numeric",
    "nummultirange",
    "numrange",
    "oid",
    "oidvector",
    "path",
    "pg_attribute",
    "pg_class",
    "pg_lsn",
    "pg_proc",
    "pg_snapshot",
    "pg_type",
    "point",
    "polygon",
    "record",
    "refcursor",
    "regclass",
    "regcollation",
    "regconfig",
    "regdictionary",
    "regnamespace",
    "regoper",
    "regoperator",
    "regproc",
    "regprocedure",
    "regrole",
    "regtype",
    "text",
    "tid",
    "time",
    "timestamp",
    "timestamptz",
    "timetz",
    "tsmultirange",
    "tsquery",
    "tsrange",
    "tstzmultirange",
    "tstzrange",
    "tsvector",
    "txid_snapshot",
    "uuid",
    "varbit",
    "varchar",
    "xid",
    "xid8",
    "xml",
];

/// A completed type and its generated array.
#[derive(Debug, Clone)]
struct Entry {
    owner: ObjectName,
    array: ObjectName,
}

/// Source-created array ownership, shared through `Arc` for copy-on-write
/// ingestion.
#[derive(Debug, Clone, Default)]
pub(super) struct TypeArrays {
    entries: Vec<Entry>,
}

/// The previous array name and its current stored name.
pub(super) type ArrayRename<'a> = (ObjectName, &'a ObjectName);

impl TypeArrays {
    /// Returns the completed owner's generated array name.
    pub(super) fn array(&self, owner: &ObjectName) -> Option<&ObjectName> {
        self.entries
            .iter()
            .find(|entry| entry_matches(&entry.owner, owner, IdentifierCase::AsWritten))
            .map(|entry| &entry.array)
    }

    /// Compares an owner's array name, treating an omitted schema as `public`.
    pub(super) fn array_is_named(&self, owner: &ObjectName, name: &ObjectName) -> bool {
        self.array(owner).is_some_and(|array| entry_matches(array, name, IdentifierCase::AsWritten))
    }

    /// Resolves an array through the session's type search path, respecting
    /// explicit type shadows.
    pub(super) fn element<'a>(
        &'a self,
        builder: &ParserDBBuilder,
        name: &ObjectName,
    ) -> Option<TargetName<'a>> {
        if !postgres(builder) {
            return None;
        }
        let (schema, ident) = object_name_identifiers(name).ok()?;
        let (value, quoted) = (ident.value.as_str(), ident.quote_style.is_some());
        if let Some(schema) = schema {
            let schema = (schema.value.as_str(), schema.quote_style.is_some());
            if explicit_holder(builder, ident, schema).is_some() {
                return None;
            }
            return self.element_in(schema.0, schema.1, value, quoted);
        }
        let temporary =
            (!builder.search_path().any(|(schema, quoted)| is_temporary_schema(schema, quoted)))
                .then_some((TEMPORARY_SCHEMA, false));
        let catalog = (!builder
            .search_path()
            .any(|(schema, quoted)| identifiers_match(schema, quoted, PG_CATALOG, false)))
        .then_some((PG_CATALOG, false));
        for (schema, schema_quoted) in temporary
            .into_iter()
            .chain(catalog)
            .chain(builder.search_path())
            .filter(|(schema, _)| !schema.is_empty())
        {
            if explicit_holder(builder, ident, (schema, schema_quoted)).is_some() {
                return None;
            }
            if let Some(element) = self.element_in(schema, schema_quoted, value, quoted) {
                return Some(element);
            }
        }
        None
    }

    /// Resolves a qualified stored or built-in array identity to its element.
    pub(super) fn element_in<'a>(
        &'a self,
        schema: &str,
        schema_quoted: bool,
        name: &str,
        name_quoted: bool,
    ) -> Option<TargetName<'a>> {
        self.element_in_case(IdentifierCase::AsWritten, schema, schema_quoted, name, name_quoted)
    }

    /// Binds known array aliases to qualified elements and leaves unknown names
    /// unchanged.
    pub(super) fn bind_type(&self, builder: &ParserDBBuilder, data_type: &mut DataType) {
        if !postgres(builder) {
            return;
        }
        match data_type {
            DataType::Custom(name, modifiers) if modifiers.is_empty() => {
                if let Some(element) = self.element_name(builder, name) {
                    *data_type =
                        DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(element), None));
                }
            }
            DataType::Array(kind) => {
                if let Some(element) = array_element_mut(kind) {
                    self.bind_array_element(builder, element);
                }
            }
            _ => {}
        }
    }

    /// Registers a completed owner after its declaration enters the namespace,
    /// retaining existing ownership.
    pub(super) fn create(&mut self, builder: &ParserDBBuilder, owner: &ObjectName) {
        if !postgres(builder) {
            return;
        }
        let case = builder.identifier_case();
        if self.entries.iter().any(|entry| entry_matches(&entry.owner, owner, case)) {
            return;
        }
        let Some((base, quoted)) = object_name_last_part(owner) else {
            return;
        };
        let (schema, schema_quoted) = schema_of(owner);
        let name = generated_array_name(base, quoted, |candidate, candidate_quoted| {
            self.namespace_holds(builder, (schema, schema_quoted), (candidate, candidate_quoted))
        });
        let array = named_in_schema(owner, spelled(name));
        self.entries.push(Entry { owner: owner.clone(), array });
    }

    /// Displaces a stored array before an explicit declaration takes its name.
    pub(super) fn displace(
        &mut self,
        builder: &ParserDBBuilder,
        name: &ObjectName,
    ) -> Option<ArrayRename<'_>> {
        if !postgres(builder) {
            return None;
        }
        let case = builder.identifier_case();
        let (schema, ident) = object_name_identifiers(name).ok()?;
        let (schema, schema_quoted) = match schema {
            Some(schema) => (schema.value.as_str(), schema.quote_style.is_some()),
            None => (DEFAULT_SCHEMA, false),
        };
        let (value, quoted) = (ident.value.as_str(), ident.quote_style.is_some());
        let position = self.entries.iter().position(|entry| {
            entry_holds(&entry.array, (schema, schema_quoted), (value, quoted), case)
        })?;
        let name = generated_array_name(value, quoted, |candidate, candidate_quoted| {
            self.namespace_holds(builder, (schema, schema_quoted), (candidate, candidate_quoted))
        });
        let current = named_in_schema(&self.entries[position].array, spelled(name));
        let entry = &mut self.entries[position].array;
        let previous = core::mem::replace(entry, current);
        Some((previous, entry))
    }

    /// Follows an owner rename, preserving its array name when
    /// `array_was_displaced` is set.
    pub(super) fn rename(
        &mut self,
        builder: &ParserDBBuilder,
        from: &ObjectName,
        to: &ObjectName,
        array_was_displaced: bool,
    ) -> Option<ArrayRename<'_>> {
        if !postgres(builder) {
            return None;
        }
        let case = builder.identifier_case();
        let position =
            self.entries.iter().position(|entry| entry_matches(&entry.owner, from, case))?;
        let (base, quoted) = object_name_last_part(to)?;
        let (schema, schema_quoted) = schema_of(to);
        let name = (!array_was_displaced).then(|| {
            generated_array_name(base, quoted, |candidate, candidate_quoted| {
                self.namespace_holds(
                    builder,
                    (schema, schema_quoted),
                    (candidate, candidate_quoted),
                )
            })
        });
        let entry = &mut self.entries[position];
        entry.owner = to.clone();
        let current = named_in_schema(to, spelled(name?));
        let previous = core::mem::replace(&mut entry.array, current);
        Some((previous, &entry.array))
    }

    /// Moves an array before its owner moves schemas, preserving its local name
    /// and refusing collisions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RelationNameAlreadyTaken`] for a destination collision
    /// or [`Error::UnnamedObject`] for a missing destination name.
    pub(super) fn move_schema(
        &mut self,
        builder: &ParserDBBuilder,
        from: &ObjectName,
        to: &ObjectName,
    ) -> Result<Option<ArrayRename<'_>>, Error> {
        if !postgres(builder) {
            return Ok(None);
        }
        let case = builder.identifier_case();
        let Some(position) =
            self.entries.iter().position(|entry| entry_matches(&entry.owner, from, case))
        else {
            return Ok(None);
        };
        let (to_schema, to_schema_quoted) = schema_of(to);
        let Some((to_name, to_name_quoted)) = object_name_last_part(to) else {
            return Err(Error::UnnamedObject { object_kind: ObjectKind::Type });
        };
        let ident = ident_of(to_name, to_name_quoted);
        let owner_taken =
            explicit_holder(builder, &ident, (to_schema, to_schema_quoted)).or_else(|| {
                self.entries
                    .iter()
                    .any(|entry| {
                        entry_holds(
                            &entry.array,
                            (to_schema, to_schema_quoted),
                            (to_name, to_name_quoted),
                            case,
                        )
                    })
                    .then_some(ObjectKind::Type)
            });
        if let Some(kind) = owner_taken {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: kind,
                object_name: to_name.to_string(),
            });
        }
        let entry = &self.entries[position];
        let (array_name, array_quoted) = object_name_last_part(&entry.array).unwrap_or(("", false));
        let array_ident = ident_of(array_name, array_quoted);
        let conflicting = explicit_holder(builder, &array_ident, (to_schema, to_schema_quoted))
            .or_else(|| {
                self.entries
                    .iter()
                    .any(|other| {
                        entry_holds(
                            &other.array,
                            (to_schema, to_schema_quoted),
                            (array_name, array_quoted),
                            case,
                        )
                    })
                    .then_some(ObjectKind::Type)
            })
            .or_else(|| {
                (identifiers_match(to_schema, to_schema_quoted, PG_CATALOG, false)
                    && builtin_element(array_name, array_quoted).is_some())
                .then_some(ObjectKind::Type)
            });
        if let Some(kind) = conflicting {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: kind,
                object_name: array_name.to_string(),
            });
        }
        let current = named_in_schema(to, ident_of(array_name, array_quoted));
        let entry = &mut self.entries[position];
        let previous = core::mem::replace(&mut entry.array, current);
        move_to_schema(&mut entry.owner, qualifier_of(to).named());
        Ok(Some((previous, &entry.array)))
    }

    /// Removes a dropped owner's array.
    pub(super) fn remove(&mut self, owner: &ObjectName) {
        self.entries.retain(|entry| !entry_matches(&entry.owner, owner, IdentifierCase::AsWritten));
    }

    /// Follows a schema rename without changing local names.
    pub(super) fn rename_schema(&mut self, from: (&str, bool), to: (&str, bool)) {
        for entry in &mut self.entries {
            let (schema, schema_quoted) = schema_of(&entry.owner);
            if identifiers_match(schema, schema_quoted, from.0, from.1) {
                move_to_schema(&mut entry.owner, Some((to.0, to.1)));
                move_to_schema(&mut entry.array, Some((to.0, to.1)));
            }
        }
    }

    /// Checks the explicit and generated type namespace for a collision.
    fn namespace_holds(
        &self,
        builder: &ParserDBBuilder,
        (schema, schema_quoted): (&str, bool),
        (name, name_quoted): (&str, bool),
    ) -> bool {
        let ident = ident_of(name, name_quoted);
        if reach_in_schema(builder, &ident, (schema, schema_quoted)).is_some() {
            return true;
        }
        let case = builder.identifier_case();
        if self.entries.iter().any(|entry| {
            entry_holds(&entry.array, (schema, schema_quoted), (name, name_quoted), case)
        }) {
            return true;
        }
        identifiers_match(schema, schema_quoted, PG_CATALOG, false)
            && builtin_element(name, name_quoted).is_some()
    }

    /// Resolves a stored or built-in array under the requested identifier
    /// comparison.
    fn element_in_case(
        &self,
        case: IdentifierCase,
        schema: &str,
        schema_quoted: bool,
        name: &str,
        name_quoted: bool,
    ) -> Option<TargetName<'_>> {
        if let Some(entry) = self.entries.iter().find(|entry| {
            entry_holds(&entry.array, (schema, schema_quoted), (name, name_quoted), case)
        }) {
            return Some(element_of(entry));
        }
        if identifiers_match(schema, schema_quoted, PG_CATALOG, false)
            && let Some(element) = builtin_element(name, name_quoted)
        {
            return Some(TargetName::new(element, false).with_schema(PG_CATALOG, false));
        }
        None
    }

    /// Resolves an array alias to its qualified element declaration.
    fn element_name(&self, builder: &ParserDBBuilder, name: &ObjectName) -> Option<DataType> {
        self.element(builder, name).map(|element| qualified_element(&element))
    }

    /// Binds an array's element, collapsing generated array aliases.
    fn bind_array_element(&self, builder: &ParserDBBuilder, element: &mut DataType) {
        match element {
            DataType::Array(kind) => {
                if let Some(inner) = array_element_mut(kind) {
                    self.bind_array_element(builder, inner);
                }
            }
            DataType::Custom(name, modifiers) if modifiers.is_empty() => {
                if let Some(bound) = self.element_name(builder, name) {
                    *element = bound;
                }
            }
            _ => {}
        }
    }
}

/// Whether the builder uses PostgreSQL type semantics.
fn postgres(builder: &ParserDBBuilder) -> bool {
    matches!(builder.dialect(), SqlparserDialect::PostgreSql)
}

/// Resolves an array name through the built-in element registry.
fn builtin_element(name: &str, quoted: bool) -> Option<&'static str> {
    let folded = stored_identifier(name, quoted);
    let element = folded.as_ref().strip_prefix('_')?;
    BUILTIN_ARRAY_ELEMENTS
        .binary_search(&element)
        .ok()
        .map(|position| BUILTIN_ARRAY_ELEMENTS[position])
}

/// Chooses an available array name using PostgreSQL's `makeArrayTypeName`
/// algorithm.
fn generated_array_name(
    base: &str,
    quoted: bool,
    mut held: impl FnMut(&str, bool) -> bool,
) -> String {
    let base = stored_identifier(base, quoted);
    let mut pass = 0u32;
    loop {
        let label = (pass > 0).then(|| pass.to_string());
        let candidate = object_name("", Some(&base), label.as_deref());
        if !held(&candidate, quoted) {
            return candidate;
        }
        pass += 1;
    }
}

/// Constructs an identifier with the specified quoting.
fn ident_of(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name) } else { Ident::new(name) }
}

/// The schema a stored owner name lives in, the default when it writes none.
fn schema_of(name: &ObjectName) -> (&str, bool) {
    qualifier_of(name).named().unwrap_or((DEFAULT_SCHEMA, false))
}

/// Compares a qualified identity, treating an omitted stored schema as
/// `public`.
fn entry_holds(
    stored: &ObjectName,
    (schema, schema_quoted): (&str, bool),
    (name, name_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    let (stored_schema, stored_schema_quoted) = schema_of(stored);
    let (stored_name, stored_name_quoted) = object_name_last_part(stored).unwrap_or(("", false));
    case.identifiers_match(stored_schema, stored_schema_quoted, schema, schema_quoted)
        && case.identifiers_match(stored_name, stored_name_quoted, name, name_quoted)
}

/// Compares stored and written owner identities.
fn entry_matches(stored: &ObjectName, lookup: &ObjectName, case: IdentifierCase) -> bool {
    let Some((name, name_quoted)) = object_name_last_part(lookup) else {
        return false;
    };
    entry_holds(stored, schema_of(lookup), (name, name_quoted), case)
}

/// Borrows the stored element identity.
fn element_of(entry: &Entry) -> TargetName<'_> {
    let (name, name_quoted) = object_name_last_part(&entry.owner).unwrap_or(("", false));
    let element = TargetName::new(name, name_quoted);
    match qualifier_of(&entry.owner).named() {
        Some((schema, schema_quoted)) => element.with_schema(schema, schema_quoted),
        None => element,
    }
}

/// Finds an explicit type occupying the name, excluding catalog-generated
/// arrays.
fn explicit_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    (schema, schema_quoted): (&str, bool),
) -> Option<ObjectKind> {
    if identifiers_match(schema, schema_quoted, PG_CATALOG, false)
        && builtin_element(&name.value, name.quote_style.is_some()).is_some()
    {
        return None;
    }
    match reach_in_schema(builder, name, (schema, schema_quoted)) {
        Some(Reached::Domain(_)) => Some(ObjectKind::Domain),
        Some(Reached::UserType(_, _) | Reached::Catalog) => Some(ObjectKind::Type),
        Some(Reached::RowType(kind)) => Some(kind),
        None => None,
    }
}

/// Constructs a qualified element declaration, including the default schema.
fn qualified_element(element: &TargetName) -> DataType {
    let (schema, schema_quoted) = match element.schema() {
        Some(schema) => (schema, element.schema_is_quoted()),
        None => (DEFAULT_SCHEMA, false),
    };
    let name = if element.name_is_quoted() {
        Ident::with_quote('"', element.name())
    } else {
        Ident::new(element.name())
    };
    DataType::Custom(
        ObjectName(vec![
            ObjectNamePart::Identifier(ident_of(schema, schema_quoted)),
            ObjectNamePart::Identifier(name),
        ]),
        Vec::new(),
    )
}

/// Borrows an array declaration's element when present.
fn array_element_mut(kind: &mut ArrayElemTypeDef) -> Option<&mut DataType> {
    match kind {
        ArrayElemTypeDef::None => None,
        ArrayElemTypeDef::AngleBracket(element)
        | ArrayElemTypeDef::SquareBracket(element, _)
        | ArrayElemTypeDef::Parenthesis(element)
        | ArrayElemTypeDef::ParenthesisNotNull(element)
        | ArrayElemTypeDef::Qualified(element, _) => Some(element),
    }
}

/// Combines a stored qualifier with a local identifier.
fn named_in_schema(owner: &ObjectName, local: Ident) -> ObjectName {
    let qualifier = qualifier_of(owner).named();
    let mut parts = Vec::with_capacity(1 + usize::from(qualifier.is_some()));
    if let Some((schema, quoted)) = qualifier {
        parts.push(ObjectNamePart::Identifier(ident_of(schema, quoted)));
    }
    parts.push(ObjectNamePart::Identifier(local));
    ObjectName(parts)
}

/// Replaces a stored name's schema while retaining its local identifier.
fn move_to_schema(stored: &mut ObjectName, to: Option<(&str, bool)>) {
    let Some(last) = stored.0.pop() else {
        return;
    };
    stored.0.clear();
    if let Some((schema, quoted)) = to {
        stored.0.push(ObjectNamePart::Identifier(ident_of(schema, quoted)));
    }
    stored.0.push(last);
}
