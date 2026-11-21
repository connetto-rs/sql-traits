//! SQLite's databases, which `ParserDB` models as schemas under the SQLite
//! dialect.
//!
//! `main` always exists and `temp` is always searched first, while every
//! `ATTACH DATABASE` adds a database searched after those already present.
//! A permanent table, index, view or trigger reads the database it lives in
//! alone, and an index, a permanent view or a permanent trigger may name no
//! other.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{CreateIndex, Ident, ObjectName, ObjectNamePart, Statement};

use super::{ParserDBBuilder, declared_schema, object_name_last_identifier, relation_name_holder};
use crate::{
    errors::{Error, ObjectKind},
    structs::Schema,
    traits::TableLike,
    utils::{
        last_str,
        object_name::{Namespaces, qualifier_of},
        walk::walk_relations,
    },
};

/// Attaches the database `name`, which joins the schemas and the end of the
/// search path.
///
/// # Errors
///
/// Returns [`Error::DatabaseAlreadyInUse`] when `name` names `main`, `temp`
/// or a database already attached.
pub(super) fn attach(builder: ParserDBBuilder, name: &Ident) -> Result<ParserDBBuilder, Error> {
    let quoted = name.quote_style.is_some();
    if builder.namespaces().is_temporary((&name.value, quoted), builder.identifier_case())
        || declared_schema(&builder, &name.value, quoted).is_some()
    {
        return Err(Error::DatabaseAlreadyInUse { database_name: name.value.clone() });
    }
    let search_path: Vec<(String, bool)> = builder
        .search_path()
        .map(|(schema, quoted)| (schema.into(), quoted))
        .chain([(name.value.clone(), quoted)])
        .collect();
    Ok(builder
        .add_schema(Arc::new(Schema::with_quoted(name.value.clone(), quoted)), ())
        .with_search_path(search_path))
}

/// Names the table, index, view, trigger or virtual table `statement`
/// creates under `database` when it names that object without a database, as
/// SQLite stores the definition of an object in its database.
pub(super) fn place_created_object(statement: &mut Statement, database: &Ident) {
    let name = match statement {
        Statement::CreateTable(create_table) => &mut create_table.name,
        Statement::CreateIndex(CreateIndex { name: Some(name), .. })
        | Statement::CreateVirtualTable { name, .. } => name,
        Statement::CreateView(create_view) => &mut create_view.name,
        Statement::CreateTrigger(create_trigger) => &mut create_trigger.name,
        _ => return,
    };
    if name.0.len() == 1 {
        name.0.insert(0, ObjectNamePart::Identifier(database.clone()));
    }
}

/// The one database the unqualified names of `statement` resolve in, or
/// [`None`] when they search every database.
///
/// A table, an index, a permanent view and a permanent trigger read the
/// database they are created in, and `ALTER TABLE` reads the database of the
/// table it alters. An index or trigger named without a database lives in
/// `temp` when its table does and in `main` otherwise, so a trigger on a
/// temporary table is itself temporary. Temporary views and triggers, and a
/// table filled from a query, search every database, as does any other
/// statement.
///
/// # Errors
///
/// Returns [`Error::CrossDatabaseReference`] when an index, a permanent
/// view or a permanent trigger names a relation of another database.
pub(super) fn home_database<'a>(
    builder: &'a ParserDBBuilder,
    statement: &'a Statement,
) -> Result<Option<(String, bool)>, Error> {
    let namespaces = builder.namespaces();
    let case = builder.identifier_case();
    let home = match statement {
        Statement::CreateTable(create_table) if create_table.query.is_none() => {
            if create_table.temporary {
                (namespaces.temporary_schema(), false)
            } else {
                database_of(namespaces, &create_table.name)
            }
        }
        Statement::AlterTable(alter_table) => {
            match qualifier_of(&alter_table.name).named() {
                Some(database) => database,
                None => {
                    match builder.resolve_table_object_name(&alter_table.name) {
                        Ok(Some(table)) => {
                            table
                                .table_schema()
                                .map_or((namespaces.default_schema(), false), |schema| {
                                    (schema, table.table_schema_is_quoted())
                                })
                        }
                        _ => return Ok(None),
                    }
                }
            }
        }
        Statement::CreateIndex(create_index) => {
            let home = match create_index.name.as_ref().and_then(|name| qualifier_of(name).named())
            {
                Some(database) => database,
                None => table_database(builder, &create_index.table_name),
            };
            let index_name = create_index.name.as_ref().unwrap_or(&create_index.table_name);
            refuse_other_databases(
                builder,
                home,
                (ObjectKind::Index, index_name),
                Some(&create_index.table_name),
                statement,
            )?;
            home
        }
        Statement::CreateView(create_view) if !create_view.temporary => {
            let home = database_of(namespaces, &create_view.name);
            if namespaces.is_temporary(home, case) {
                return Ok(None);
            }
            refuse_other_databases(
                builder,
                home,
                (ObjectKind::View, &create_view.name),
                None,
                statement,
            )?;
            home
        }
        Statement::CreateTrigger(create_trigger) if !create_trigger.temporary => {
            let home = match qualifier_of(&create_trigger.name).named() {
                Some(database) => database,
                None => table_database(builder, &create_trigger.table_name),
            };
            if namespaces.is_temporary(home, case) {
                return Ok(None);
            }
            refuse_other_databases(
                builder,
                home,
                (ObjectKind::Trigger, &create_trigger.name),
                Some(&create_trigger.table_name),
                statement,
            )?;
            home
        }
        _ => return Ok(None),
    };
    Ok(Some((home.0.to_string(), home.1)))
}

/// The database `name` names, `main` when it names none.
fn database_of(namespaces: Namespaces, name: &ObjectName) -> (&str, bool) {
    qualifier_of(name).named().unwrap_or((namespaces.default_schema(), false))
}

/// The database of an index or trigger named without one on `table`, which
/// is `temp` when `table` names `temp` or, unqualified, a temporary relation,
/// and `main` otherwise.
fn table_database(builder: &ParserDBBuilder, table: &ObjectName) -> (&'static str, bool) {
    let namespaces = builder.namespaces();
    let temporary = (namespaces.temporary_schema(), false);
    let in_temp = match qualifier_of(table).named() {
        Some(database) => namespaces.is_temporary(database, builder.identifier_case()),
        None => {
            object_name_last_identifier(table).is_some_and(|name| {
                relation_name_holder(builder, name, Some(temporary), None).is_some()
            })
        }
    };
    if in_temp { temporary } else { (namespaces.default_schema(), false) }
}

/// Refuses a relation `statement` names, `table` first when given, that lives
/// in a database other than `home`, the database of the permanent object
/// `object_name`.
fn refuse_other_databases(
    builder: &ParserDBBuilder,
    home: (&str, bool),
    (object_kind, object_name): (ObjectKind, &ObjectName),
    table: Option<&ObjectName>,
    statement: &Statement,
) -> Result<(), Error> {
    let namespaces = builder.namespaces();
    let case = builder.identifier_case();
    let outside = |relation: &ObjectName| {
        qualifier_of(relation)
            .named()
            .filter(|&database| !namespaces.schemas_match(database, home, case))
            .map(|(database, _)| database.to_string())
    };
    let other = table.and_then(outside).or_else(|| {
        match walk_relations(statement, |relation| {
            outside(relation).map_or(ControlFlow::Continue(()), ControlFlow::Break)
        }) {
            ControlFlow::Break(database) => Some(database),
            ControlFlow::Continue(()) => None,
        }
    });
    other.map_or(Ok(()), |database_name| {
        Err(Error::CrossDatabaseReference {
            object_kind,
            object_name: last_str(object_name).to_string(),
            database_name,
        })
    })
}
