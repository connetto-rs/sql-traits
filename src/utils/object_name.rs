//! Shared resolution of sqlparser [`ObjectName`] values against the tables of a
//! [`DatabaseLike`].
//!
//! These helpers are the single source of truth for turning a parsed SQL object
//! name (`table`, `schema.table`) into a resolved table, applying PostgreSQL
//! identifier semantics through [`identifiers_match`].
//! They are generic over [`TableLike`] so that both the concrete `ParserDB`
//! resolution paths and the trait-on-AST data-statement analysis share one
//! implementation.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{Ident, ObjectName, ObjectNamePart};

use crate::{
    errors::LookupError,
    structs::{IdentifierCase, TargetName},
    traits::{DatabaseLike, FunctionLike, TableLike, ViewLike},
    utils::identifier_resolution::{identifiers_match, normalize_identifier},
};

/// Reports a name a statement builds while it runs, which no static reader can
/// resolve.
pub(crate) fn run_time_object_name(object_name: &ObjectName) -> LookupError {
    LookupError::InvalidObjectName {
        object_name: object_name.to_string(),
        reason: "a part of this name is built when the statement runs, so the object it \
                 denotes is not known here"
            .to_string(),
    }
}

/// Reports a name carrying more parts than a schema and an object, which
/// reaches for a catalog this one does not model.
pub(crate) fn overqualified_object_name(object_name: &ObjectName) -> LookupError {
    LookupError::InvalidObjectName {
        object_name: object_name.to_string(),
        reason: "only one-part or two-part object names are supported".to_string(),
    }
}

/// Refuses a name carrying a part built while the statement runs.
///
/// A recording path calls this whatever it does about qualification: a name
/// that only exists at run time cannot be stored, matched or reported under
/// any spelling.
///
/// # Errors
///
/// Returns [`LookupError::InvalidObjectName`] when a part is built at run
/// time.
pub(crate) fn require_static_object_name(object_name: &ObjectName) -> Result<(), LookupError> {
    if object_name.0.iter().any(|part| matches!(part, ObjectNamePart::Function(_))) {
        return Err(run_time_object_name(object_name));
    }
    Ok(())
}

/// Refuses a name this catalog cannot hold: one carrying a part built while
/// the statement runs, or more parts than a schema and an object.
///
/// Every path that records an object calls this first. A name that only
/// exists at run time cannot be stored under any spelling, and a name with a
/// leading catalog part denotes an object elsewhere, which the two-part model
/// here cannot represent. Reading either as a local object is what recorded
/// tables nobody declared.
///
/// # Errors
///
/// Returns [`LookupError::InvalidObjectName`] when a part is built at run
/// time, or when the name carries more than two parts.
pub(crate) fn require_local_object_name(object_name: &ObjectName) -> Result<(), LookupError> {
    require_static_object_name(object_name)?;
    if object_name.0.len() > 2 {
        return Err(overqualified_object_name(object_name));
    }
    Ok(())
}

/// Returns the written identifier of a single object name part, and [`None`]
/// when the part is a call producing the name at run time.
pub(crate) fn object_name_part_value(part: &ObjectNamePart) -> Option<&str> {
    match part {
        ObjectNamePart::Identifier(ident) => Some(ident.value.as_str()),
        ObjectNamePart::Function(_) => None,
    }
}

/// Returns the last identifier part of an object name as `(value, quoted)`.
///
/// A part built at run time yields [`None`], the same answer as an empty name:
/// there is no identifier to read either way.
pub(crate) fn object_name_last_part(object_name: &ObjectName) -> Option<(&str, bool)> {
    match object_name.0.last() {
        Some(ObjectNamePart::Identifier(ident)) => {
            Some((ident.value.as_str(), ident.quote_style.is_some()))
        }
        Some(ObjectNamePart::Function(_)) | None => None,
    }
}

/// What an object name says about the schema it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Qualifier<'a> {
    /// A one-part name, so resolution supplies the schema.
    Absent,
    /// Qualified by this identifier, with whether it was quoted.
    Named(&'a str, bool),
    /// Qualified by a call producing the schema name at run time, so which
    /// schema it is cannot be known here.
    RunTime,
}

impl<'a> Qualifier<'a> {
    /// The qualifier a static reader can use, and [`None`] when there is none
    /// to read.
    pub(crate) fn named(self) -> Option<(&'a str, bool)> {
        match self {
            Self::Named(value, quoted) => Some((value, quoted)),
            Self::Absent | Self::RunTime => None,
        }
    }
}

/// Reads the schema component (the second-to-last part) of an object name.
///
/// For `schema.table` this reports the `schema` part, for a bare `table` it
/// reports [`Qualifier::Absent`], and for a qualifier built at run time it
/// reports [`Qualifier::RunTime`] rather than claiming the name is
/// unqualified.
pub(crate) fn qualifier_of(object_name: &ObjectName) -> Qualifier<'_> {
    if object_name.0.len() < 2 {
        return Qualifier::Absent;
    }
    match &object_name.0[object_name.0.len() - 2] {
        ObjectNamePart::Identifier(ident) => {
            Qualifier::Named(ident.value.as_str(), ident.quote_style.is_some())
        }
        ObjectNamePart::Function(_) => Qualifier::RunTime,
    }
}

/// Reads an object name as an unresolved [`TargetName`], taking the last part
/// as the name and the second-to-last (if any) as its qualifier.
///
/// Returns `None` only for a name with no parts, which sqlparser does not
/// produce.
pub(crate) fn target_name_from_object_name(object_name: &ObjectName) -> Option<TargetName<'_>> {
    let (name, quoted) = object_name_last_part(object_name)?;
    let target = TargetName::new(name, quoted);
    Some(match qualifier_of(object_name) {
        Qualifier::Named(schema, schema_quoted) => target.with_schema(schema, schema_quoted),
        Qualifier::Absent => target,
        // A schema nothing can read is not the same as no schema, so the name
        // denotes nothing a reader may act on.
        Qualifier::RunTime => return None,
    })
}

/// Returns whether a table matches an object name using lenient part matching:
/// the last part is the table name and the second-to-last (if any) is the
/// schema. Leading parts beyond those are ignored.
///
/// This is the matching style used by grant resolution, where object names may
/// carry catalog-qualified prefixes. A table stored without a schema resides in
/// the default schema, so a `public` qualifier reaches it and a table stored in
/// `public` answers an unqualified name.
pub(crate) fn table_matches_object_name<T: TableLike>(table: &T, object_name: &ObjectName) -> bool {
    let Some((table_lookup_name, table_lookup_quoted)) = object_name_last_part(object_name) else {
        return false;
    };

    if !identifiers_match(
        table.table_name(),
        table.table_name_is_quoted(),
        table_lookup_name,
        table_lookup_quoted,
    ) {
        return false;
    }

    match (qualifier_of(object_name), table.table_schema()) {
        (Qualifier::Absent, None) => true,
        (Qualifier::Named(schema_lookup, schema_lookup_quoted), Some(table_schema)) => {
            identifiers_match(
                table_schema,
                table.table_schema_is_quoted(),
                schema_lookup,
                schema_lookup_quoted,
            )
        }
        (Qualifier::Named(schema_lookup, schema_lookup_quoted), None) => {
            identifiers_match("public", false, schema_lookup, schema_lookup_quoted)
        }
        (Qualifier::Absent, Some(table_schema)) => {
            identifiers_match(table_schema, table.table_schema_is_quoted(), "public", false)
        }
        // A qualifier built at run time names no schema a comparison can use.
        (Qualifier::RunTime, _) => false,
    }
}

/// Splits a one-part or two-part object name into optional schema and required
/// table identifiers, rejecting names that cannot denote a table.
///
/// # Errors
///
/// Returns [`LookupError::InvalidObjectName`] when the name is empty, has more
/// than two parts, or contains a function part.
pub(crate) fn object_name_identifiers(
    object_name: &ObjectName,
) -> Result<(Option<&Ident>, &Ident), LookupError> {
    if object_name.0.is_empty() {
        return Err(LookupError::InvalidObjectName {
            object_name: object_name.to_string(),
            reason: "name has no identifier parts".to_string(),
        });
    }
    require_local_object_name(object_name)?;

    let mut idents: Vec<&Ident> = Vec::with_capacity(object_name.0.len());
    for part in &object_name.0 {
        if let ObjectNamePart::Identifier(ident) = part {
            idents.push(ident);
        }
    }

    if idents.len() == 1 { Ok((None, idents[0])) } else { Ok((Some(idents[0]), idents[1])) }
}

/// Reads an object name as a [`TargetName`], using an empty string for the
/// name when nothing static can be read from its last part: an empty parts
/// list, or a part built when the statement runs. The parser never produces
/// an empty name, and a recording path refuses a run-time one, so a caller
/// receiving an empty-string `TargetName` is looking at a name built by hand
/// or at one it should already have refused.
pub(crate) fn target_name_of_object_name(object_name: &ObjectName) -> TargetName<'_> {
    target_name_from_object_name(object_name).unwrap_or_else(|| TargetName::new("", false))
}

/// Reads a table's own stored name as a [`TargetName`].
pub(crate) fn target_name_of_table<T: TableLike>(table: &T) -> TargetName<'_> {
    let name = TargetName::new(table.table_name(), table.table_name_is_quoted());
    match table.table_schema() {
        Some(schema) => name.with_schema(schema, table.table_schema_is_quoted()),
        None => name,
    }
}

/// Builds a [`TargetName`] from the identifiers a strict table lookup yields.
pub(crate) fn target_name_of_idents<'a>(
    schema_ident: Option<&'a Ident>,
    table_ident: &'a Ident,
) -> TargetName<'a> {
    let name = TargetName::new(table_ident.value.as_str(), table_ident.quote_style.is_some());
    match schema_ident {
        Some(schema_ident) => {
            name.with_schema(schema_ident.value.as_str(), schema_ident.quote_style.is_some())
        }
        None => name,
    }
}

/// Normalized identity a relation is found under.
///
/// PostgreSQL keeps tables, views and materialized views in one pool of
/// names, so all three are indexed under this key and a name can hold at most
/// one of them. A relation stored without a schema and one stored in `public`
/// share a key, mirroring that the two spellings name one place.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RelationKey {
    /// Normalized schema, `public` for a schema-less relation.
    pub schema: String,
    /// Normalized relation name.
    pub name: String,
}

impl RelationKey {
    /// The key a folded lookup reaches this one by.
    pub(crate) fn folded(&self) -> Self {
        Self {
            schema: IdentifierCase::Folded.compared_form(&self.schema, false).into_owned(),
            name: IdentifierCase::Folded.compared_form(&self.name, false).into_owned(),
        }
    }
}

/// Normalizes a stored schema under `case`, folding a schema-less table into
/// `public`.
fn stored_schema_key<T: TableLike>(table: &T, case: IdentifierCase) -> Cow<'_, str> {
    table.table_schema().map_or(Cow::Borrowed("public"), |schema| {
        case.compared_form(schema, table.table_schema_is_quoted())
    })
}

/// Normalizes a stored table name under `case`.
fn stored_name_key<T: TableLike>(table: &T, case: IdentifierCase) -> Cow<'_, str> {
    case.compared_form(table.table_name(), table.table_name_is_quoted())
}

/// Key of a written target name under `case`, normalizing each part once.
pub(crate) fn target_key(target: &TargetName<'_>, case: IdentifierCase) -> RelationKey {
    RelationKey {
        schema: target.schema().map_or_else(
            || String::from("public"),
            |schema| case.compared_form(schema, target.schema_is_quoted()).into_owned(),
        ),
        name: case.compared_form(target.name(), target.name_is_quoted()).into_owned(),
    }
}

/// Key a stored table is indexed under, comparing each part under `case`.
pub(crate) fn stored_table_key<T: TableLike>(table: &T, case: IdentifierCase) -> RelationKey {
    RelationKey {
        schema: stored_schema_key(table, case).into_owned(),
        name: stored_name_key(table, case).into_owned(),
    }
}

/// Key a stored identity is found under, taking each part exactly as stored.
///
/// The index folds a schema-less relation into `public`, so this key reaches
/// the bucket holding both spellings and the caller separates them by
/// comparing the stored parts.
pub(crate) fn stored_identity_key(schema: Option<&str>, name: &str) -> RelationKey {
    RelationKey {
        schema: schema.map_or_else(|| String::from("public"), String::from),
        name: String::from(name),
    }
}

/// Key a stored function is found under under `case`, folding a function
/// declared without a schema into `public` exactly as a relation is folded.
///
/// Functions have their own pool of names, so this key indexes them apart
/// from relations, and a name carrying several argument lists holds several
/// functions.
pub(crate) fn stored_function_key<F: FunctionLike>(
    function: &F,
    case: IdentifierCase,
) -> RelationKey {
    target_key(&function.target_name(), case)
}

/// Whether a function is stored under exactly this identity, with both parts
/// read as the catalog holds them.
pub(crate) fn function_has_stored_identity<F: FunctionLike>(
    function: &F,
    schema: Option<&str>,
    name: &str,
) -> bool {
    let target = function.target_name();
    let stored_schema =
        target.schema().map(|schema| normalize_identifier(schema, target.schema_is_quoted()));
    stored_schema.as_deref() == schema && function.stored_name() == name
}

/// Resolves a single function from the declarations a name matched.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries more
/// than one declaration, since resolution here is by name alone and cannot
/// choose between argument lists.
pub(crate) fn resolve_one_function<'a, F: FunctionLike>(
    name: &str,
    candidates: &[&'a F],
) -> Result<Option<&'a F>, LookupError> {
    match candidates {
        [] => Ok(None),
        [only] => Ok(Some(only)),
        many => {
            Err(LookupError::AmbiguousFunctionLookup {
                object_name: name.to_string(),
                candidates: many.iter().map(|f| f.target_name().to_string()).collect(),
            })
        }
    }
}

/// Resolves a written function reference against an iterator of declarations,
/// trying each schema on `search_path` in turn for an unqualified name.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries more
/// than one declaration in the schema that wins.
pub(crate) fn resolve_function_on_search_path_in_iter<'a, 'path, F: FunctionLike>(
    functions: impl Iterator<Item = &'a F>,
    target: &TargetName<'_>,
    search_path: impl Iterator<Item = (&'path str, bool)>,
    case: IdentifierCase,
) -> Result<Option<&'a F>, LookupError> {
    let indexed: Vec<(RelationKey, &'a F)> =
        functions.map(|function| (stored_function_key(function, case), function)).collect();
    let written = target.to_string();
    let matching = |key: &RelationKey| -> Vec<&'a F> {
        indexed
            .iter()
            .filter_map(|(stored, function)| (stored == key).then_some(*function))
            .collect()
    };

    if target.schema().is_some() {
        return resolve_one_function(&written, &matching(&target_key(target, case)));
    }

    let name = case.compared_form(target.name(), target.name_is_quoted()).into_owned();
    for (entry_schema, entry_quoted) in search_path {
        let key = RelationKey {
            schema: case.compared_form(entry_schema, entry_quoted).into_owned(),
            name: name.clone(),
        };
        let candidates = matching(&key);
        if !candidates.is_empty() {
            return resolve_one_function(&written, &candidates);
        }
    }

    Ok(None)
}

/// Key a stored view is indexed under under `case`, folding a schema-less
/// view into `public` exactly as a table is folded.
pub(crate) fn stored_view_key<V: ViewLike>(view: &V, case: IdentifierCase) -> RelationKey {
    RelationKey {
        schema: view.view_schema().map_or_else(
            || String::from("public"),
            |schema| case.compared_form(schema, view.view_schema_is_quoted()).into_owned(),
        ),
        name: case.compared_form(view.view_name(), view.view_name_is_quoted()).into_owned(),
    }
}

/// Returns whether a stored table answers a key normalized under `case`,
/// normalizing only the stored side.
pub(crate) fn table_matches_key<T: TableLike>(
    table: &T,
    key: &RelationKey,
    case: IdentifierCase,
) -> bool {
    key.name == stored_name_key(table, case) && key.schema == stored_schema_key(table, case)
}

/// Returns whether a stored view answers a key normalized under `case`,
/// normalizing only the stored side.
pub(crate) fn view_matches_key<V: ViewLike>(
    view: &V,
    key: &RelationKey,
    case: IdentifierCase,
) -> bool {
    key.name == case.compared_form(view.view_name(), view.view_name_is_quoted())
        && view.view_schema().map_or(key.schema == "public", |schema| {
            key.schema == case.compared_form(schema, view.view_schema_is_quoted())
        })
}

/// Reference matcher used by tests: compares a table against a written target
/// without precomputing a key.
#[cfg(test)]
pub(crate) fn table_matches_target<T: TableLike>(
    table: &T,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> bool {
    table_matches_key(table, &target_key(target, case), case)
}

/// Renders a table for inclusion in an ambiguity error, quoting parts that were
/// originally quoted.
pub(crate) fn render_table_candidate<T: TableLike>(table: &T) -> String {
    target_name_of_table(table).to_string()
}

/// Resolves a single relation from a list of candidate matches.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when more than one candidate
/// matches.
pub(crate) fn resolve_one_relation<'a, R>(
    target: &TargetName<'_>,
    candidates: &[&'a R],
    render: impl Fn(&R) -> String,
) -> Result<Option<&'a R>, LookupError> {
    match candidates {
        [] => Ok(None),
        [relation] => Ok(Some(*relation)),
        _ => {
            let mut rendered: Vec<String> = candidates.iter().copied().map(&render).collect();
            rendered.sort_unstable();
            rendered.dedup();
            Err(LookupError::AmbiguousTableLookup {
                object_name: target.to_string(),
                candidates: rendered,
            })
        }
    }
}

/// Renders a view for inclusion in an ambiguity error, quoting parts that were
/// originally quoted.
pub(crate) fn render_view_candidate<V: ViewLike>(view: &V) -> String {
    view.target_name().to_string()
}

/// Resolves a single table from a list of candidate matches.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when more than one candidate
/// matches.
pub(crate) fn resolve_target_from_candidates<'a, T: TableLike>(
    target: &TargetName<'_>,
    candidates: &[&'a T],
) -> Result<Option<&'a T>, LookupError> {
    resolve_one_relation(target, candidates, render_table_candidate)
}

/// Resolves a written target name against an iterator of tables, without
/// consulting any search path.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
/// than one table.
pub(crate) fn resolve_target_in_iter<'a, T: TableLike>(
    tables: impl Iterator<Item = &'a T>,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> Result<Option<&'a T>, LookupError> {
    let key = target_key(target, case);
    let candidates: Vec<&T> =
        tables.filter(|table| table_matches_key(*table, &key, case)).collect();
    resolve_target_from_candidates(target, &candidates)
}

/// Resolves a table from a one-part or two-part object name against an
/// iterator of tables, comparing identifiers the way PostgreSQL does.
///
/// The object-name resolvers serve this crate's own ingestion, whose creation
/// and dependency rules are PostgreSQL's, so they carry no comparison
/// argument. A caller reading a reference on another engine's behalf builds a
/// [`TargetName`] and states its rule through
/// [`DatabaseLike::resolve_target_table`].
///
/// # Errors
///
/// Returns an error when the object name is malformed for table lookup, or when
/// the lookup is ambiguous.
pub(crate) fn resolve_table_object_name_in_iter<'a, T: TableLike>(
    tables: impl Iterator<Item = &'a T>,
    object_name: &ObjectName,
    case: IdentifierCase,
) -> Result<Option<&'a T>, LookupError> {
    let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
    resolve_target_in_iter(tables, &target_name_of_idents(schema_ident, table_ident), case)
}

/// Resolves a written target name against an iterator of relations of one
/// kind, trying each schema on `search_path` in turn for an unqualified name.
///
/// The path is walked in order and the first schema holding a match wins,
/// which is what the database does. A relation stored without a schema resides
/// in the default schema, so it is found where `public` sits on the path
/// rather than ahead of it.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
/// than one relation in the schema that wins, reported under the name the
/// statement wrote.
fn resolve_relation_on_search_path<'a, 'path, R>(
    relations: impl Iterator<Item = &'a R>,
    target: &TargetName<'_>,
    search_path: impl Iterator<Item = (&'path str, bool)>,
    case: IdentifierCase,
    key_of: impl Fn(&R, IdentifierCase) -> RelationKey,
    claimed: impl Fn(&RelationKey) -> bool,
    render: impl Fn(&R) -> String,
) -> Result<Option<&'a R>, LookupError> {
    if target.schema().is_some() {
        let key = target_key(target, case);
        let candidates: Vec<&R> =
            relations.filter(|relation| key_of(relation, case) == key).collect();
        return resolve_one_relation(target, &candidates, render);
    }

    let indexed: Vec<(RelationKey, &'a R)> =
        relations.map(|relation| (key_of(relation, case), relation)).collect();
    let mut key = RelationKey {
        schema: String::new(),
        name: case.compared_form(target.name(), target.name_is_quoted()).into_owned(),
    };
    for (entry_schema, entry_quoted) in search_path {
        key.schema.clear();
        key.schema.push_str(&case.compared_form(entry_schema, entry_quoted));
        let candidates: Vec<&'a R> = indexed
            .iter()
            .filter_map(|(stored, relation)| (*stored == key).then_some(*relation))
            .collect();
        if candidates.is_empty() && !claimed(&key) {
            continue;
        }
        // Reported under the written name: the entry qualifier is resolution
        // machinery, not something the statement spelled.
        return resolve_one_relation(target, &candidates, render);
    }

    Ok(None)
}

/// Whether any relation of any kind in `database` is stored under `key`.
///
/// Tables, views and materialized views share one pool of names, so this is
/// what ends a search-path walk: a schema holding the name under another kind
/// is not looked past.
pub(crate) fn relation_name_is_claimed<DB: DatabaseLike>(
    database: &DB,
    key: &RelationKey,
    case: IdentifierCase,
) -> bool {
    database.tables().any(|table| stored_table_key(table, case) == *key)
        || database.views().any(|view| stored_view_key(view, case) == *key)
        || database.materialized_views().any(|view| stored_view_key(view, case) == *key)
}

/// Resolves a written target name against an iterator of tables, trying each
/// schema on `search_path` in turn for an unqualified name.
///
/// `claimed` answers whether a schema holds the name under any relation kind,
/// which ends the walk even when no table of that name lives there.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
/// than one table in the schema that wins, reported under the name the
/// statement wrote.
pub(crate) fn resolve_target_on_search_path_in_iter<'a, 'path, T: TableLike>(
    tables: impl Iterator<Item = &'a T>,
    target: &TargetName<'_>,
    search_path: impl Iterator<Item = (&'path str, bool)>,
    case: IdentifierCase,
    claimed: impl Fn(&RelationKey) -> bool,
) -> Result<Option<&'a T>, LookupError> {
    resolve_relation_on_search_path(
        tables,
        target,
        search_path,
        case,
        stored_table_key,
        claimed,
        render_table_candidate,
    )
}

/// Resolves a written target name against an iterator of views of one kind,
/// trying each schema on `search_path` in turn for an unqualified name.
///
/// `claimed` carries the same meaning it has for
/// [`resolve_target_on_search_path_in_iter`].
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
/// than one view in the schema that wins, reported under the name the
/// statement wrote.
pub(crate) fn resolve_view_on_search_path_in_iter<'a, 'path, V: ViewLike>(
    views: impl Iterator<Item = &'a V>,
    target: &TargetName<'_>,
    search_path: impl Iterator<Item = (&'path str, bool)>,
    case: IdentifierCase,
    claimed: impl Fn(&RelationKey) -> bool,
) -> Result<Option<&'a V>, LookupError> {
    resolve_relation_on_search_path(
        views,
        target,
        search_path,
        case,
        stored_view_key,
        claimed,
        render_view_candidate,
    )
}

/// Resolves a written target name against an iterator of views of one kind,
/// consulting no search path, so an unqualified name names the default
/// schema.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
/// than one view.
pub(crate) fn resolve_view_in_iter<'a, V: ViewLike>(
    views: impl Iterator<Item = &'a V>,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> Result<Option<&'a V>, LookupError> {
    let key = target_key(target, case);
    let candidates: Vec<&V> = views.filter(|view| stored_view_key(*view, case) == key).collect();
    resolve_one_relation(target, &candidates, render_view_candidate)
}

/// Resolves a written function reference against an iterator of declarations,
/// consulting no search path, so an unqualified name names the default
/// schema.
///
/// # Errors
///
/// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries
/// more than one declaration, since resolution by name alone cannot choose
/// between argument lists.
pub(crate) fn resolve_function_in_iter<'a, F: FunctionLike>(
    functions: impl Iterator<Item = &'a F>,
    target: &TargetName<'_>,
    case: IdentifierCase,
) -> Result<Option<&'a F>, LookupError> {
    let key = target_key(target, case);
    let candidates: Vec<&F> =
        functions.filter(|function| stored_function_key(*function, case) == key).collect();
    resolve_one_function(&target.to_string(), &candidates)
}

/// Resolves a table from a one-part or two-part object name, honouring
/// `search_path` for an unqualified name.
///
/// # Errors
///
/// Returns an error when the object name is malformed for table lookup, or when
/// the lookup is ambiguous.
pub(crate) fn resolve_table_object_name_on_search_path_in_iter<'a, 'path, T: TableLike>(
    tables: impl Iterator<Item = &'a T>,
    object_name: &ObjectName,
    search_path: impl Iterator<Item = (&'path str, bool)>,
    case: IdentifierCase,
) -> Result<Option<&'a T>, LookupError> {
    let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
    resolve_target_on_search_path_in_iter(
        tables,
        &target_name_of_idents(schema_ident, table_ident),
        search_path,
        case,
        // Only tables are in hand here, so a view holding the name does not
        // end the walk, which is the creation-time namespace question this
        // crate answers separately.
        |_| false,
    )
}

/// Resolves a one-part or two-part object name to a base table of `database`.
///
/// This is the canonical entry point for trait-on-AST analysis that needs to
/// turn an [`ObjectName`] into a [`DatabaseLike::Table`]. `Ok(None)` means no
/// table matched (for example a CTE name or a table function); an error means
/// the name is malformed or the lookup is ambiguous.
///
/// # Errors
///
/// Returns an error when the object name is malformed for table lookup, or when
/// the lookup is ambiguous.
pub(crate) fn resolve_object_name<'db, DB: DatabaseLike>(
    object_name: &ObjectName,
    database: &'db DB,
) -> Result<Option<&'db DB::Table>, LookupError> {
    let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
    // Through the trait method, so a catalog overriding it answers every
    // accessor the same way it answers a direct lookup.
    database.resolve_target_table(
        target_name_of_idents(schema_ident, table_ident),
        IdentifierCase::AsWritten,
    )
}

/// Resolves an object name that is required to denote an existing base table of
/// `database`.
///
/// Unlike [`resolve_object_name`], a name that matches no table is an error
/// rather than `Ok(None)`: use this when the referencing object could not exist
/// without its target, as with a policy or a trigger.
///
/// # Errors
///
/// Returns [`LookupError::TableNotFound`] when no table matches, and an error
/// when the object name is malformed for table lookup or the lookup is
/// ambiguous.
pub(crate) fn resolve_required_table<'db, DB: DatabaseLike>(
    object_name: &ObjectName,
    database: &'db DB,
) -> Result<&'db DB::Table, LookupError> {
    resolve_object_name(object_name, database)?
        .ok_or_else(|| LookupError::TableNotFound { object_name: object_name.to_string() })
}

/// Resolves a reference a stored object carries to the base table it names.
///
/// Ingestion rewrites every stored reference to the qualifier of the relation
/// it reached when the object was created, leaving it bare only for a
/// relation stored bare. So the name is read as written, with no search path,
/// and a later path does not move it, exactly as PostgreSQL keeps the object
/// it bound.
///
/// # Errors
///
/// Returns an error when the object name is malformed for table lookup, or when
/// the lookup is ambiguous.
pub(crate) fn resolve_bound_table<'db, DB: DatabaseLike>(
    object_name: &ObjectName,
    database: &'db DB,
) -> Result<Option<&'db DB::Table>, LookupError> {
    let (schema_ident, table_ident) = object_name_identifiers(object_name)?;
    database.table_by_target(
        target_name_of_idents(schema_ident, table_ident),
        IdentifierCase::AsWritten,
    )
}

/// Resolves a reference a stored object carries to the base table it cannot
/// exist without, as [`resolve_bound_table`] does.
///
/// # Errors
///
/// Returns [`LookupError::TableNotFound`] when no table matches, and an error
/// when the object name is malformed for table lookup or the lookup is
/// ambiguous.
pub(crate) fn resolve_required_bound_table<'db, DB: DatabaseLike>(
    object_name: &ObjectName,
    database: &'db DB,
) -> Result<&'db DB::Table, LookupError> {
    resolve_bound_table(object_name, database)?
        .ok_or_else(|| LookupError::TableNotFound { object_name: object_name.to_string() })
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{CreateTable, Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction, Statement},
        dialect::GenericDialect,
        parser::Parser,
    };

    use super::{
        Qualifier, object_name_identifiers, object_name_last_part, qualifier_of,
        relation_name_is_claimed, render_table_candidate, render_view_candidate,
        require_local_object_name, resolve_object_name, resolve_table_object_name_in_iter,
        resolve_table_object_name_on_search_path_in_iter, resolve_target_from_candidates,
        resolve_target_in_iter, resolve_view_on_search_path_in_iter, table_matches_object_name,
        table_matches_target, target_name_from_object_name, target_name_of_object_name,
    };
    use crate::{
        errors::LookupError,
        prelude::ParserDB,
        structs::{IdentifierCase, TargetName},
        traits::{DatabaseLike, TableLike},
    };

    fn ident(value: &str, quoted: bool) -> Ident {
        if quoted { Ident::with_quote('"', value) } else { Ident::new(value) }
    }

    /// The path a database starts with, which is what these helpers used to
    /// hardcode.
    fn default_path() -> impl Iterator<Item = (&'static str, bool)> {
        core::iter::once(("public", false))
    }

    /// Builds an `ObjectName` from `(value, quoted)` identifier parts.
    fn obj(parts: &[(&str, bool)]) -> ObjectName {
        ObjectName(parts.iter().map(|&(v, q)| ObjectNamePart::Identifier(ident(v, q))).collect())
    }

    fn function_part(name: &str) -> ObjectNamePart {
        ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::new(name),
            args: Vec::new(),
        })
    }

    /// Parses one `CREATE TABLE` into an owned `CreateTable`. Parsing tables
    /// individually lets a test hold relations the `ParserDB` builder would
    /// reject together (for example a schema-less and a `public` table of the
    /// same name), which the resolver functions still must handle.
    fn create_table(sql: &str) -> CreateTable {
        let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("table parses");
        match statements.pop().expect("one statement") {
            Statement::CreateTable(create_table) => create_table,
            other => panic!("expected CREATE TABLE, got {other:?}"),
        }
    }

    /// A spread of relations covering schema-less, schema-qualified, quoted,
    /// and `public`/schema-less name collisions.
    fn fixtures() -> Vec<CreateTable> {
        vec![
            create_table("CREATE TABLE users (id INT)"),
            create_table("CREATE TABLE s.scoped (id INT)"),
            create_table("CREATE TABLE things (id INT)"),
            create_table("CREATE TABLE public.things (id INT)"),
            create_table("CREATE TABLE s.users (id INT)"),
            create_table("CREATE TABLE public.only_pub (id INT)"),
            create_table(r#"CREATE TABLE "Bar" (id INT)"#),
            create_table(r#"CREATE TABLE "S"."T" (id INT)"#),
        ]
    }

    fn find<'a>(tables: &'a [CreateTable], name: &str) -> &'a CreateTable {
        tables.iter().find(|table| table.table_name() == name).expect("table present")
    }

    #[test]
    fn object_name_last_part_variants() {
        assert_eq!(object_name_last_part(&obj(&[("t", false)])), Some(("t", false)));
        assert_eq!(object_name_last_part(&obj(&[("T", true)])), Some(("T", true)));
        // A name built when the statement runs carries no identifier to read.
        assert_eq!(object_name_last_part(&ObjectName(vec![function_part("IDENTIFIER")])), None);
        assert_eq!(object_name_last_part(&ObjectName(Vec::new())), None);
    }

    #[test]
    fn qualifier_of_variants() {
        assert_eq!(qualifier_of(&obj(&[("t", false)])), Qualifier::Absent);
        assert_eq!(qualifier_of(&obj(&[("s", false), ("t", false)])), Qualifier::Named("s", false));
        let run_time = ObjectName(vec![
            function_part("IDENTIFIER"),
            ObjectNamePart::Identifier(ident("t", false)),
        ]);
        // An unreadable qualifier is not an absent one, so nothing may read
        // the name as unqualified.
        assert_eq!(qualifier_of(&run_time), Qualifier::RunTime);
        assert_eq!(qualifier_of(&run_time).named(), None);
        assert_eq!(target_name_from_object_name(&run_time), None);
    }

    #[test]
    fn a_run_time_part_is_refused_and_matches_nothing() {
        let tables = fixtures();
        let users = find(&tables, "users");
        let terminal = ObjectName(vec![function_part("IDENTIFIER")]);
        let qualifier = ObjectName(vec![
            function_part("IDENTIFIER"),
            ObjectNamePart::Identifier(ident("users", false)),
        ]);

        assert!(require_local_object_name(&obj(&[("users", false)])).is_ok());
        for name in [&terminal, &qualifier] {
            assert!(matches!(
                require_local_object_name(name),
                Err(LookupError::InvalidObjectName { .. })
            ));
            assert!(matches!(
                object_name_identifiers(name),
                Err(LookupError::InvalidObjectName { .. })
            ));
            assert!(!table_matches_object_name(users, name));
        }
    }

    /// The fallback answers an empty name for both inputs it cannot read, an
    /// empty parts list and a name built at run time, and an empty name
    /// reaches no relation.
    #[test]
    fn the_unreadable_fallback_names_nothing() {
        let tables = fixtures();
        let empty = ObjectName(Vec::new());
        let run_time = ObjectName(vec![function_part("IDENTIFIER")]);

        for name in [&empty, &run_time] {
            let target = target_name_of_object_name(name);
            assert_eq!(target.name(), "");
            assert_eq!(target.schema(), None);
            assert!(
                resolve_target_in_iter(tables.iter(), &target, IdentifierCase::AsWritten)
                    .expect("nothing is ambiguous")
                    .is_none()
            );
        }
    }

    #[test]
    fn table_matches_object_name_cases() {
        let tables = fixtures();
        let users = find(&tables, "users");
        let scoped = find(&tables, "scoped");

        assert!(!table_matches_object_name(users, &ObjectName(Vec::new())));
        assert!(!table_matches_object_name(users, &obj(&[("orders", false)])));
        assert!(table_matches_object_name(users, &obj(&[("users", false)])));
        assert!(table_matches_object_name(scoped, &obj(&[("s", false), ("scoped", false)])));
        // The two spellings of the default schema are one place.
        assert!(table_matches_object_name(users, &obj(&[("public", false), ("users", false)])));
        assert!(table_matches_object_name(find(&tables, "only_pub"), &obj(&[("only_pub", false)])));
        // A non-public qualifier misses a schema-less table.
        assert!(!table_matches_object_name(users, &obj(&[("nope", false), ("users", false)])));
    }

    #[test]
    fn object_name_identifiers_cases() {
        assert!(matches!(object_name_identifiers(&obj(&[("t", false)])), Ok((None, _))));
        assert!(matches!(
            object_name_identifiers(&obj(&[("s", false), ("t", false)])),
            Ok((Some(_), _))
        ));
        assert!(matches!(
            object_name_identifiers(&ObjectName(Vec::new())),
            Err(LookupError::InvalidObjectName { .. })
        ));
        assert!(matches!(
            object_name_identifiers(&obj(&[("a", false), ("b", false), ("c", false)])),
            Err(LookupError::InvalidObjectName { .. })
        ));
        assert!(matches!(
            object_name_identifiers(&ObjectName(vec![function_part("f")])),
            Err(LookupError::InvalidObjectName { .. })
        ));
    }

    #[test]
    fn table_matches_target_cases() {
        let tables = fixtures();
        let users = find(&tables, "users");
        let scoped = find(&tables, "scoped");

        let case = IdentifierCase::AsWritten;
        assert!(table_matches_target(users, &TargetName::new("users", false), case));
        assert!(!table_matches_target(users, &TargetName::new("orders", false), case));
        assert!(table_matches_target(
            scoped,
            &TargetName::new("scoped", false).with_schema("s", false),
            case
        ));
        // The two spellings of the default schema are one place.
        assert!(table_matches_target(
            users,
            &TargetName::new("users", false).with_schema("public", false),
            case
        ));
        assert!(table_matches_target(
            find(&tables, "only_pub"),
            &TargetName::new("only_pub", false),
            case
        ));
        // A table in another schema stays unreachable without its qualifier.
        assert!(!table_matches_target(scoped, &TargetName::new("scoped", false), case));
        assert!(!table_matches_target(
            users,
            &TargetName::new("users", false).with_schema("s", false),
            case
        ));
    }

    #[test]
    fn render_table_candidate_quoting_and_schema() {
        let tables = fixtures();
        assert_eq!(render_table_candidate(find(&tables, "users")), "users");
        assert_eq!(render_table_candidate(find(&tables, "Bar")), "\"Bar\"");
        assert_eq!(render_table_candidate(find(&tables, "scoped")), "s.scoped");
        assert_eq!(render_table_candidate(find(&tables, "T")), "\"S\".\"T\"");
    }

    #[test]
    fn resolve_target_from_candidates_cases() {
        let tables = fixtures();
        let users = find(&tables, "users");
        let scoped = find(&tables, "scoped");
        let target = TargetName::new("users", false);

        let empty: [&CreateTable; 0] = [];
        assert!(resolve_target_from_candidates(&target, &empty).unwrap().is_none());
        assert!(resolve_target_from_candidates(&target, &[users]).unwrap().is_some());
        assert!(matches!(
            resolve_target_from_candidates(&target, &[users, scoped]),
            Err(LookupError::AmbiguousTableLookup { .. })
        ));
    }

    #[test]
    fn resolve_in_iter_rejects_overqualified_names() {
        let tables = fixtures();
        let resolved = resolve_table_object_name_in_iter(
            tables.iter(),
            &obj(&[("users", false)]),
            IdentifierCase::AsWritten,
        )
        .expect("resolves")
        .expect("matches");
        assert_eq!(resolved.table_name(), "users");

        assert!(matches!(
            resolve_table_object_name_in_iter(
                tables.iter(),
                &obj(&[("a", false), ("b", false), ("c", false)]),
                IdentifierCase::AsWritten,
            ),
            Err(LookupError::InvalidObjectName { .. })
        ));
    }

    #[test]
    fn implicit_public_fallback_cases() {
        let tables = fixtures();

        // Qualified name delegates to the strict resolver.
        let scoped = resolve_table_object_name_on_search_path_in_iter(
            tables.iter(),
            &obj(&[("s", false), ("scoped", false)]),
            default_path(),
            IdentifierCase::AsWritten,
        )
        .expect("resolves")
        .expect("matches");
        assert_eq!(scoped.table_name(), "scoped");

        // Resolved only through the implicit `public` schema.
        let only_pub = resolve_table_object_name_on_search_path_in_iter(
            tables.iter(),
            &obj(&[("only_pub", false)]),
            default_path(),
            IdentifierCase::AsWritten,
        )
        .expect("resolves")
        .expect("matches");
        assert_eq!(only_pub.table_name(), "only_pub");

        // `things` exists both schema-less and in `public`, two tables claiming
        // one place: ambiguous, reported under the written name.
        assert!(matches!(
            resolve_table_object_name_on_search_path_in_iter(
                tables.iter(),
                &obj(&[("things", false)]),
                default_path(),
                IdentifierCase::AsWritten,
            ),
            Err(LookupError::AmbiguousTableLookup { ref object_name, .. }) if object_name == "things"
        ));

        // The first schema on the path holding the name wins.
        let on_s_first = resolve_table_object_name_on_search_path_in_iter(
            tables.iter(),
            &obj(&[("users", false)]),
            [("s", false), ("public", false)].into_iter(),
            IdentifierCase::AsWritten,
        )
        .expect("resolves")
        .expect("matches");
        assert_eq!(on_s_first.table_schema(), Some("s"));

        let on_public_first = resolve_table_object_name_on_search_path_in_iter(
            tables.iter(),
            &obj(&[("users", false)]),
            [("public", false), ("s", false)].into_iter(),
            IdentifierCase::AsWritten,
        )
        .expect("resolves")
        .expect("matches");
        assert_eq!(on_public_first.table_schema(), None, "the schema-less spelling is public's");

        // A schema-less table is unreachable when `public` is off the path.
        assert!(
            resolve_table_object_name_on_search_path_in_iter(
                tables.iter(),
                &obj(&[("only_pub", false)]),
                core::iter::once(("s", false)),
                IdentifierCase::AsWritten,
            )
            .unwrap()
            .is_none()
        );

        // No match anywhere.
        assert!(
            resolve_table_object_name_on_search_path_in_iter(
                tables.iter(),
                &obj(&[("absent", false)]),
                default_path(),
                IdentifierCase::AsWritten,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn resolve_object_name_db_entry_point() {
        let db = ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT);").expect("parses");
        let resolved = resolve_object_name(&obj(&[("users", false)]), &db)
            .expect("resolves")
            .expect("matches");
        assert_eq!(resolved.table_name(), "users");
        assert!(resolve_object_name(&obj(&[("absent", false)]), &db).unwrap().is_none());
    }

    #[test]
    fn target_name_of_empty_object_name_returns_empty_str() {
        let name = ObjectName(vec![]);
        let result = target_name_of_object_name(&name);
        assert_eq!(result.name(), "");
        assert!(!result.name_is_quoted());
        assert!(result.schema().is_none());
    }

    #[test]
    fn views_render_and_resolve_on_the_search_path() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE SCHEMA s;
             CREATE TABLE t(id INT);
             CREATE VIEW s.v AS SELECT id FROM t;",
        )
        .expect("schema parses");
        let view = db
            .view_by_target(
                TargetName::new("v", false).with_schema("s", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("view exists");
        assert_eq!(render_view_candidate(view), "s.v");
        let resolved = resolve_view_on_search_path_in_iter(
            db.views(),
            &TargetName::new("v", false),
            [("s", false)].into_iter(),
            IdentifierCase::AsWritten,
            |key| relation_name_is_claimed(&db, key, IdentifierCase::AsWritten),
        )
        .expect("view resolves")
        .expect("view matches");
        assert_eq!(render_view_candidate(resolved), "s.v");
    }
}
