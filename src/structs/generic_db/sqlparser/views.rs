//! Recording, replacing and dropping the two kinds of view.
//!
//! Every rule here was measured against PostgreSQL 18.4 in Docker rather than
//! read from documentation.
//!
//! A view shares one pool of names with tables, materialized views and
//! indexes, so creating one under a taken name is refused whichever kind holds
//! it, and the two drop spellings refuse each other's kind with the server's
//! own wording. `CREATE OR REPLACE VIEW` may only add output columns on the
//! end: renaming one and dropping one are both refused, checked whenever the
//! recorded and the replacing shapes can both be read. A materialized view has
//! no replace form at all, and a plain view has no `IF NOT EXISTS` form, both
//! of which the parser accepts and the server rejects outright.
//!
//! A view whose definition reads a temporary relation is temporary whatever
//! the statement wrote, and lands in `pg_temp`. A materialized view may not
//! read one at all.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    AlterTableOperation, CreateView, Ident, ObjectName, ObjectNamePart, Owner, Query,
    RenameTableNameKind, TableFactor, Visit, VisitMut, Visitor, VisitorMut,
};

use super::{
    ParserDBBuilder, SchemaQualifier, bind_reference, object_name_last_identifier, place_relation,
    relation_name_holder, require_named_in_catalog,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::{IdentifierCase, MaterializedView, View, metadata::ViewMetadata},
    traits::ViewLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{
            RelationKey, qualifier_of, stored_view_key, target_key, target_name_from_object_name,
        },
    },
};

/// The kind a `CREATE VIEW` node declares.
fn declared_kind(node: &CreateView) -> ObjectKind {
    if node.materialized { ObjectKind::MaterializedView } else { ObjectKind::View }
}

/// The schema qualifier a view name carries, if it carries one.
fn view_schema_qualifier(name: &ObjectName) -> SchemaQualifier<'_> {
    qualifier_of(name).named()
}

/// Records a `CREATE VIEW` or `CREATE MATERIALIZED VIEW`.
///
/// # Errors
///
/// Refuses the spellings the parser accepts and PostgreSQL does not, a name
/// another relation in the schema already holds, a schema the input never
/// creates, a temporary view outside the temporary schema, a materialized view
/// reading a temporary relation, and a replacement that renames or drops an
/// output column.
pub(super) fn create_view(
    mut builder: ParserDBBuilder,
    mut node: CreateView,
) -> Result<ParserDBBuilder, Error> {
    let kind = declared_kind(&node);
    require_named_in_catalog(&mut node.name, kind, builder.catalog_name())?;

    if node.materialized && node.or_replace {
        return Err(Error::MaterializedViewCannotBeReplaced {
            view_name: rendered_name(&node.name),
        });
    }
    if node.materialized && node.temporary {
        return Err(Error::TemporaryMaterializedView { view_name: rendered_name(&node.name) });
    }
    if !node.materialized && node.if_not_exists {
        return Err(Error::ViewIfNotExistsUnsupported { view_name: rendered_name(&node.name) });
    }

    // A view reading a temporary relation is temporary whatever it wrote.
    let temporary_read = bind_definition(&builder, &mut node.query);
    let promoted = temporary_read.is_some();
    if node.materialized
        && let Some(relation_name) = temporary_read
    {
        return Err(Error::MaterializedViewReadsTemporaryRelation {
            view_name: rendered_name(&node.name),
            relation_name,
        });
    }
    node.temporary = place_relation(&builder, &mut node.name, node.temporary || promoted, kind)?;

    let schema = view_schema_qualifier(&node.name);
    let Some(name_ident) = object_name_last_identifier(&node.name) else {
        return Err(Error::UnnamedObject { object_kind: kind });
    };

    // Removed before the name-pool check, which would see the replaced view as
    // a collision.
    if node.or_replace
        && let Some(position) = stored_view_position(&builder, &node.name)
    {
        let (existing, metadata) = builder.views_mut().remove(position);
        check_replacement_columns(existing.as_ref(), &node)?;
        let Some(view) = View::from_node(&node) else {
            return Err(Error::UnnamedObject { object_kind: kind });
        };
        return Ok(builder.add_view(Arc::new(view), metadata));
    }

    let holder = relation_name_holder(&builder, name_ident, schema);

    if let Some(conflicting_kind) = holder {
        // `CREATE MATERIALIZED VIEW ... IF NOT EXISTS` skips silently when a
        // materialized view already holds the name, which is the only form
        // PostgreSQL offers this on.
        if node.if_not_exists && conflicting_kind == ObjectKind::MaterializedView {
            return Ok(builder);
        }
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: kind,
            conflicting_kind,
            object_name: name_ident.value.clone(),
        });
    }

    if node.materialized {
        let Some(view) = MaterializedView::from_node(&node) else {
            return Err(Error::UnnamedObject { object_kind: kind });
        };
        Ok(builder.add_materialized_view(Arc::new(view), ViewMetadata::default()))
    } else {
        let Some(view) = View::from_node(&node) else {
            return Err(Error::UnnamedObject { object_kind: kind });
        };
        Ok(builder.add_view(Arc::new(view), ViewMetadata::default()))
    }
}

/// Checks the two rules a replacement has to keep: it may not rename an output
/// column and it may not drop one.
///
/// Only the names are checkable here. A view carries no declared types, so the
/// server's third rule, that a column's type may not change, needs a type for
/// an arbitrary expression, which this crate does not derive. When either
/// shape's names cannot be read the replacement is recorded without complaint,
/// which never refuses input the server accepts.
fn check_replacement_columns(existing: &View, node: &CreateView) -> Result<(), Error> {
    let (Some(before), Some(after)) = (existing.declared_output_names(), declared_names_of(node))
    else {
        return Ok(());
    };

    if after.len() < before.len() {
        return Err(Error::ViewColumnsDroppedByReplace {
            view_name: existing.view_name().to_string(),
        });
    }
    for ((existing_name, existing_quoted), (new_name, new_quoted)) in
        before.iter().zip(after.iter())
    {
        if !identifiers_match(existing_name, *existing_quoted, new_name, *new_quoted) {
            return Err(Error::ViewColumnRenamedByReplace {
                view_name: existing.view_name().to_string(),
                existing_column: existing_name.clone(),
                new_column: new_name.clone(),
            });
        }
    }
    Ok(())
}

/// The output names a `CREATE VIEW` node writes explicitly, or [`None`] when it
/// writes none and the definition would have to be read.
fn declared_names_of(node: &CreateView) -> Option<Vec<(String, bool)>> {
    (!node.columns.is_empty()).then(|| {
        node.columns
            .iter()
            .map(|column| (column.name.value.clone(), column.name.quote_style.is_some()))
            .collect()
    })
}

/// The position of the plain view a written `name` resolves to.
///
/// Resolved through the search path, so a bare name reaches the view a bare
/// reference would read, exactly as the table lookup beside it does. Comparing
/// the written qualifier against the stored one instead would miss every view
/// the path placed in a schema other than the default.
fn plain_view_position(builder: &ParserDBBuilder, name: &ObjectName) -> Option<usize> {
    let key =
        stored_view_key(builder.resolve_view_object_name(name).ok()??, IdentifierCase::AsWritten);
    builder
        .views()
        .iter()
        .position(|(view, _)| stored_view_key(view.as_ref(), IdentifierCase::AsWritten) == key)
}

/// The position of the plain view stored under exactly `name`, a bare name
/// meaning the default schema.
fn stored_view_position(builder: &ParserDBBuilder, name: &ObjectName) -> Option<usize> {
    let case = builder.identifier_case();
    let key = target_key(&target_name_from_object_name(name)?, case);
    builder.views().iter().position(|(view, _)| stored_view_key(view.as_ref(), case) == key)
}

/// Drops the views a `DROP VIEW` or `DROP MATERIALIZED VIEW` names.
///
/// PostgreSQL checks the kind rather than treating the two spellings as
/// interchangeable: `DROP VIEW` refuses a materialized view and a table, and
/// `DROP MATERIALIZED VIEW` refuses a plain view, each pointing at the right
/// spelling. A name nothing holds is refused unless the statement wrote `IF
/// EXISTS`.
///
/// # Errors
///
/// Returns [`Error::RelationKindMismatch`] for a name held by another relation
/// kind, [`Error::RelationNotFound`] for a name nothing holds, and
/// [`Error::RelationHasDependents`] when a view reads the one being dropped
/// and the statement wrote no `CASCADE`.
pub(super) fn drop_views(
    mut builder: ParserDBBuilder,
    names: &[ObjectName],
    materialized: bool,
    if_exists: bool,
    cascade: bool,
) -> Result<ParserDBBuilder, Error> {
    let expected_kind = if materialized { ObjectKind::MaterializedView } else { ObjectKind::View };
    for name in names {
        if object_name_last_identifier(name).is_none() {
            return Err(Error::UnnamedObject { object_kind: expected_kind });
        }

        let position = if materialized {
            materialized_view_position(&builder, name)
        } else {
            plain_view_position(&builder, name)
        };
        if let Some(position) = position {
            let key = if materialized {
                stored_view_key(
                    builder.materialized_views()[position].0.as_ref(),
                    IdentifierCase::AsWritten,
                )
            } else {
                stored_view_key(builder.views()[position].0.as_ref(), IdentifierCase::AsWritten)
            };
            if cascade {
                remove_dependent_views(&mut builder, &key);
            } else {
                refuse_dependent_views(&builder, &key, expected_kind, &rendered_name(name))?;
            }
            // Re-read the position: a cascade may have removed views ahead of
            // this one in the same collection.
            let position = if materialized {
                materialized_view_position(&builder, name)
            } else {
                plain_view_position(&builder, name)
            };
            if let Some(position) = position {
                if materialized {
                    builder.materialized_views_mut().remove(position);
                } else {
                    builder.views_mut().remove(position);
                }
            }
            continue;
        }

        // Nothing of the asked-for kind holds the name. Another relation kind
        // holding it is the wrong-spelling case PostgreSQL names, and a name
        // nothing holds is absent.
        match builder.relation_reached(name).map(|(kind, _)| kind) {
            Some(actual_kind) => {
                return Err(Error::RelationKindMismatch {
                    object_name: rendered_name(name),
                    expected_kind,
                    actual_kind,
                });
            }
            None if !if_exists => {
                return Err(Error::RelationNotFound {
                    object_kind: expected_kind,
                    object_name: rendered_name(name),
                });
            }
            None => {}
        }
    }
    Ok(builder)
}

/// The position of the materialized view a written `name` resolves to,
/// through the search path.
fn materialized_view_position(builder: &ParserDBBuilder, name: &ObjectName) -> Option<usize> {
    let key = stored_view_key(
        builder.resolve_materialized_view_object_name(name).ok()??,
        IdentifierCase::AsWritten,
    );
    builder
        .materialized_views()
        .iter()
        .position(|(view, _)| stored_view_key(view.as_ref(), IdentifierCase::AsWritten) == key)
}

/// Refuses a `DROP TABLE` naming a view, as PostgreSQL does.
///
/// # Errors
///
/// Returns [`Error::RelationKindMismatch`] when the name is held by either
/// view kind.
pub(super) fn refuse_dropping_view_as_table(
    builder: &ParserDBBuilder,
    name: &ObjectName,
) -> Result<(), Error> {
    match builder.relation_reached(name) {
        Some((actual_kind @ (ObjectKind::View | ObjectKind::MaterializedView), _)) => {
            Err(Error::RelationKindMismatch {
                object_name: rendered_name(name),
                expected_kind: ObjectKind::Table,
                actual_kind,
            })
        }
        _ => Ok(()),
    }
}

/// A view name as the statement wrote it, for an error message.
fn rendered_name(name: &ObjectName) -> String {
    name.to_string()
}

/// Applies an `ALTER TABLE` whose target is a view.
///
/// PostgreSQL accepts `ALTER TABLE` against a view for the actions a view
/// supports and refuses the rest naming the action. Of those the parser can
/// read, this handles renaming and changing the owner.
///
/// Only called when [`holds_view`] answered, so the name is known to be a
/// view's.
///
/// # Errors
///
/// Returns [`Error::AlterActionUnsupportedOnRelation`] for an action a view
/// does not support and [`Error::RelationNameAlreadyTaken`] when a rename asks
/// for a name another relation in the schema holds.
pub(super) fn alter_view(
    builder: ParserDBBuilder,
    name: &ObjectName,
    kind: ObjectKind,
    operation: &AlterTableOperation,
) -> Result<ParserDBBuilder, Error> {
    match operation {
        AlterTableOperation::RenameTable { table_name } => {
            let (RenameTableNameKind::As(new_name) | RenameTableNameKind::To(new_name)) =
                table_name;
            rename_view(builder, name, kind, new_name)
        }
        AlterTableOperation::OwnerTo { new_owner } => {
            Ok(set_view_owner(builder, name, kind, new_owner))
        }
        other => {
            Err(Error::AlterActionUnsupportedOnRelation {
                object_kind: kind,
                relation_name: rendered_name(name),
                operation: other.to_string(),
            })
        }
    }
}

/// The view kind a written relation name resolves to, if a view holds it.
///
/// Resolved through the search path, so a bare name naming a view the path
/// placed in another schema still answers.
pub(super) fn holds_view(builder: &ParserDBBuilder, name: &ObjectName) -> Option<ObjectKind> {
    if builder.resolve_view_object_name(name).ok().flatten().is_some() {
        return Some(ObjectKind::View);
    }
    builder
        .resolve_materialized_view_object_name(name)
        .ok()
        .flatten()
        .map(|_| ObjectKind::MaterializedView)
}

/// Renames a stored view, refusing a name another relation already holds.
///
/// The new name is checked against the pool in the schema the view actually
/// sits in, which for a view the search path placed is not the schema the
/// statement wrote.
fn rename_view(
    mut builder: ParserDBBuilder,
    name: &ObjectName,
    kind: ObjectKind,
    new_name: &ObjectName,
) -> Result<ParserDBBuilder, Error> {
    let Some(new_ident) = object_name_last_identifier(new_name) else {
        return Err(Error::UnnamedObject { object_kind: kind });
    };
    let position = if kind == ObjectKind::MaterializedView {
        materialized_view_position(&builder, name)
    } else {
        plain_view_position(&builder, name)
    };
    let Some(position) = position else {
        return Ok(builder);
    };

    let (current_name, current_quoted, schema) = if kind == ObjectKind::MaterializedView {
        let view = builder.materialized_views()[position].0.as_ref();
        (view.view_name().to_string(), view.view_name_is_quoted(), stored_schema_of(view))
    } else {
        let view = builder.views()[position].0.as_ref();
        (view.view_name().to_string(), view.view_name_is_quoted(), stored_schema_of(view))
    };

    // A rename to the name it already answers to changes nothing, and asking
    // the name pool first would report the view colliding with itself.
    if !crate::utils::identifier_resolution::identifiers_match(
        current_name.as_str(),
        current_quoted,
        new_ident.value.as_str(),
        new_ident.quote_style.is_some(),
    ) && let Some(conflicting_kind) = relation_name_holder(
        &builder,
        new_ident,
        schema.as_ref().map(|(value, quoted)| (value.as_str(), *quoted)),
    ) {
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: kind,
            conflicting_kind,
            object_name: new_ident.value.clone(),
        });
    }

    let renamed = (new_ident.value.clone(), new_ident.quote_style.is_some());
    if kind == ObjectKind::MaterializedView {
        let (view, _) = &mut builder.materialized_views_mut()[position];
        Arc::make_mut(view).declaration_mut().set_name(renamed.0, renamed.1);
    } else {
        let (view, _) = &mut builder.views_mut()[position];
        Arc::make_mut(view).declaration_mut().set_name(renamed.0, renamed.1);
    }
    Ok(builder)
}

/// The schema a stored view sits in, with its quote state.
fn stored_schema_of<V: ViewLike>(view: &V) -> Option<(String, bool)> {
    view.view_schema().map(|schema| (schema.to_string(), view.view_schema_is_quoted()))
}

/// Records the role a view is handed to.
///
/// `CURRENT_ROLE`, `CURRENT_USER` and `SESSION_USER` name whoever runs the
/// statement, so the owner became one the input never spells and the model
/// stops naming it too, exactly as the table path does.
fn set_view_owner(
    mut builder: ParserDBBuilder,
    name: &ObjectName,
    kind: ObjectKind,
    new_owner: &Owner,
) -> ParserDBBuilder {
    let owner = match new_owner {
        Owner::Ident(ident) => Some(super::stored_role_name(ident)),
        Owner::CurrentRole | Owner::CurrentUser | Owner::SessionUser => None,
    };
    if kind == ObjectKind::MaterializedView {
        if let Some(position) = materialized_view_position(&builder, name) {
            builder.materialized_views_mut()[position].1.set_owner(owner);
        }
    } else if let Some(position) = plain_view_position(&builder, name) {
        builder.views_mut()[position].1.set_owner(owner);
    }
    builder
}

/// The roles the recorded views name as their owners.
pub(super) fn view_owner_names(builder: &ParserDBBuilder) -> Vec<String> {
    builder
        .views()
        .iter()
        .map(|(_, metadata)| metadata)
        .chain(builder.materialized_views().iter().map(|(_, metadata)| metadata))
        .filter_map(|metadata| metadata.owner().map(ToString::to_string))
        .collect()
}

/// The `WITH` names visible where a walk over a query stands, scoped as
/// PostgreSQL scopes them.
///
/// An item's body sees the items before it, or every item under `WITH
/// RECURSIVE`, and the rest of the query sees them all, so a body naming its
/// own item reads the outer relation unless the `WITH` is recursive.
#[derive(Default)]
struct WithScopes {
    frames: Vec<WithFrame>,
}

/// The items one `WITH` clause binds.
struct WithFrame {
    /// The query carrying the clause, compared by address.
    owner: *const Query,
    /// Each item's body, compared by address.
    bodies: Vec<*const Query>,
    /// Each item's name.
    names: Vec<Ident>,
    /// Whether the clause is `WITH RECURSIVE`.
    recursive: bool,
    /// How many items, from the first, the walk currently sees.
    visible: usize,
}

impl WithScopes {
    fn enter(&mut self, query: &Query) {
        if let Some(frame) = self.frames.last_mut()
            && let Some(position) = frame.bodies.iter().position(|body| core::ptr::eq(*body, query))
        {
            frame.visible = if frame.recursive { frame.names.len() } else { position };
        }
        if let Some(with) = &query.with {
            self.frames.push(WithFrame {
                owner: query,
                bodies: with.cte_tables.iter().map(|cte| &raw const *cte.query).collect(),
                names: with.cte_tables.iter().map(|cte| cte.alias.name.clone()).collect(),
                recursive: with.recursive,
                visible: 0,
            });
        }
    }

    fn leave(&mut self, query: &Query) {
        if self.frames.last().is_some_and(|frame| core::ptr::eq(frame.owner, query)) {
            self.frames.pop();
        }
        if let Some(frame) = self.frames.last_mut()
            && !frame.recursive
            && let Some(position) = frame.bodies.iter().position(|body| core::ptr::eq(*body, query))
        {
            frame.visible = position + 1;
        }
    }

    /// Whether `name` reads a visible `WITH` item rather than a stored
    /// relation.
    fn reads_item(&self, name: &ObjectName) -> bool {
        let [ObjectNamePart::Identifier(written)] = name.0.as_slice() else {
            return false;
        };
        self.frames.iter().any(|frame| {
            frame.names[..frame.visible].iter().any(|item| {
                identifiers_match(
                    item.value.as_str(),
                    item.quote_style.is_some(),
                    written.value.as_str(),
                    written.quote_style.is_some(),
                )
            })
        })
    }
}

/// Binds every stored relation a definition reads, remembering the first
/// temporary one.
struct DefinitionBinder<'builder> {
    builder: &'builder ParserDBBuilder,
    scopes: WithScopes,
    temporary_read: Option<String>,
}

impl VisitorMut for DefinitionBinder<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.leave(query);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args: None, .. } = factor
            && !self.scopes.reads_item(name)
            && let Some(bound) = self.builder.bound_relation(name)
        {
            if bound.temporary && self.temporary_read.is_none() {
                self.temporary_read = Some(name.to_string());
            }
            bind_reference(name, bound.qualifier);
        }
        ControlFlow::Continue(())
    }
}

/// Binds every stored relation a view definition reads to the relation it
/// reaches now, answering the name of the first temporary one.
fn bind_definition(builder: &ParserDBBuilder, query: &mut Query) -> Option<String> {
    let mut binder =
        DefinitionBinder { builder, scopes: WithScopes::default(), temporary_read: None };
    let walk = query.visit(&mut binder);
    debug_assert!(walk.is_continue(), "the binder never breaks");
    binder.temporary_read
}

/// Collects the identity of every stored relation a bound definition reads.
struct RelationsRead {
    scopes: WithScopes,
    read: Vec<RelationKey>,
}

impl Visitor for RelationsRead {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.leave(query);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args: None, .. } = factor
            && !self.scopes.reads_item(name)
            && let Some(target) = target_name_from_object_name(name)
        {
            self.read.push(target_key(&target, IdentifierCase::AsWritten));
        }
        ControlFlow::Continue(())
    }
}

/// The normalized identity of every relation a view's definition reads.
///
/// Ingestion bound each name to the relation it reached, so a bare one means
/// the default schema.
fn relations_read_by<V: ViewLike>(view: &V) -> Vec<RelationKey> {
    let mut visitor = RelationsRead { scopes: WithScopes::default(), read: Vec::new() };
    let walk = view.definition().visit(&mut visitor);
    debug_assert!(walk.is_continue(), "the visitor never breaks");
    visitor.read
}

/// Every view reading the relation `key` names, and every view reading one of
/// those, transitively.
///
/// PostgreSQL refuses to drop a relation while anything reads it, and takes
/// the whole chain with it under `CASCADE`, so the walk closes over the chain
/// rather than stopping at the first level.
pub(super) fn dependent_views(
    builder: &ParserDBBuilder,
    key: &RelationKey,
) -> Vec<(ObjectKind, RelationKey)> {
    // Each view's read set is derived once, not once per step of the walk.
    // Deriving it means visiting a whole definition and normalizing every name
    // it reads, so re-deriving per step made one drop cost the square of the
    // number of views.
    let reads: Vec<(ObjectKind, RelationKey, Vec<RelationKey>)> = builder
        .views()
        .iter()
        .map(|(view, _)| {
            (
                ObjectKind::View,
                stored_view_key(view.as_ref(), IdentifierCase::AsWritten),
                relations_read_by(view.as_ref()),
            )
        })
        .chain(builder.materialized_views().iter().map(|(view, _)| {
            (
                ObjectKind::MaterializedView,
                stored_view_key(view.as_ref(), IdentifierCase::AsWritten),
                relations_read_by(view.as_ref()),
            )
        }))
        .collect();

    let mut frontier = alloc::vec![key.clone()];
    let mut found: Vec<(ObjectKind, RelationKey)> = Vec::new();

    while let Some(current) = frontier.pop() {
        for (kind, own_key, read) in &reads {
            if !read.contains(&current) || *own_key == *key {
                continue;
            }
            let reader = (*kind, own_key.clone());
            if !found.contains(&reader) {
                frontier.push(own_key.clone());
                found.push(reader);
            }
        }
    }

    found
}

/// Refuses dropping the relation `key` names while a view reads it.
///
/// # Errors
///
/// Returns [`Error::RelationHasDependents`] naming the first reader, which is
/// what PostgreSQL reports before listing the rest.
pub(super) fn refuse_dependent_views(
    builder: &ParserDBBuilder,
    key: &RelationKey,
    object_kind: ObjectKind,
    object_name: &str,
) -> Result<(), Error> {
    if let Some((dependent_kind, dependent)) = dependent_views(builder, key).first() {
        return Err(Error::RelationHasDependents {
            object_kind,
            object_name: object_name.to_string(),
            dependent_kind: *dependent_kind,
            dependent_name: dependent.name.clone(),
        });
    }
    Ok(())
}

/// Removes the views a `CASCADE` takes along with the relation `key` names.
pub(super) fn remove_dependent_views(builder: &mut ParserDBBuilder, key: &RelationKey) {
    let doomed = dependent_views(builder, key);
    builder.views_mut().retain(|(view, _)| {
        !doomed.iter().any(|(kind, dead)| {
            *kind == ObjectKind::View
                && *dead == stored_view_key(view.as_ref(), IdentifierCase::AsWritten)
        })
    });
    builder.materialized_views_mut().retain(|(view, _)| {
        !doomed.iter().any(|(kind, dead)| {
            *kind == ObjectKind::MaterializedView
                && *dead == stored_view_key(view.as_ref(), IdentifierCase::AsWritten)
        })
    });
}
