//! Code a PL/pgSQL body may run without writing it, such as a function a
//! domain's checks call, an operator whose function lies outside
//! `pg_catalog`, a relation whose view, rules, policies or triggers run
//! functions, and a portal the caller opened.

use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{Expr, ObjectName, Query, SetExpr, Visit, Visitor};
use sqlparser_plpgsql::embedded::{
    Block, DeclarationKind, DeclaredType, IntoTargets, Node, NodeKind, Target,
};

use super::{
    callee_path::call_outside_catalog,
    dynamic_sites::{SettingCall, setting_call},
};
use crate::{
    structs::{binary_operator_name, prefix_operator_name},
    traits::{
        ColumnLike, DataTypeLike, DatabaseLike, DomainLike, FunctionLike, OperatorLike, TableLike,
        UserDefinedTypeLike, ViewLike,
    },
    utils::{
        call_sites::for_each_call,
        object_name::{PG_CATALOG, object_name_last_part},
        walk::{walk, walk_expressions},
    },
};

/// The `pg_catalog` functions that scan a relation, a schema, a database
/// or a portal the caller opened, which runs the views, policies and
/// queries behind them.
const SCANNING: [&str; 11] = [
    "table_to_xml",
    "table_to_xmlschema",
    "table_to_xml_and_xmlschema",
    "schema_to_xml",
    "schema_to_xmlschema",
    "schema_to_xml_and_xmlschema",
    "database_to_xml",
    "database_to_xmlschema",
    "database_to_xml_and_xmlschema",
    "cursor_to_xml",
    "cursor_to_xmlschema",
];

/// The operators PostgreSQL applies without the SQL writing them, for `IN`,
/// `BETWEEN`, `LIKE`, `SIMILAR TO`, `CASE`, `NULLIF`, `IS DISTINCT FROM`,
/// sorting, grouping and set operations.
const IMPLIED: [&str; 12] =
    ["=", "<>", "<", ">", "<=", ">=", "~~", "~~*", "!~~", "!~~*", "~", "!~"];

/// What in a database runs functions outside `pg_catalog`, or sets a
/// setting, where a body only names a type or an operator.
pub(super) struct OutsideCode {
    /// The names of the operators whose function lies outside `pg_catalog`
    /// or is unknown, each once.
    operators: Vec<String>,
    /// The names of every operator, each once, or `None` when the database
    /// lists none.
    known: Option<Vec<String>>,
    /// The names, unqualified and lowercase, of the domains whose checks
    /// may run such code or call `set_config`, of the tables and composite
    /// types holding one of them at any depth, and of every view once one of
    /// them exists.
    types: Vec<String>,
}

impl OutsideCode {
    /// What in `database` runs code outside `pg_catalog`.
    pub(super) fn of<DB: DatabaseLike>(database: &DB) -> Self {
        let mut operators = Vec::new();
        let known = database.operators().map(|listed| {
            let mut known = Vec::new();
            for operator in listed {
                let name = operator.target_name();
                let catalog = name.schema() == Some(PG_CATALOG)
                    && operator.implementation(database).is_some_and(|function| {
                        function.target_name().schema() == Some(PG_CATALOG)
                    });
                if !catalog {
                    once(&mut operators, name.name());
                }
                once(&mut known, name.name());
            }
            known
        });
        let mut code = Self { operators, known, types: Vec::new() };
        let mut domains: Vec<_> = database.domains().collect();
        let mut tables: Vec<_> = database.tables().collect();
        let mut composites: Vec<_> = database.user_defined_types().collect();
        let mut views: Vec<_> = database.views().collect();
        // Each pass adds the domains, tables, composite types and views
        // holding a type the passes before found, until one adds none.
        loop {
            let found = code.types.len();
            domains.retain(|domain| {
                let runs = domain.check_expressions().any(|check| {
                    call_outside_catalog(check, database).is_some()
                        || code.operator_in(check).is_some()
                        || sets_setting(check)
                }) || code.names_type(&domain.data_type(database).name(database));
                if runs {
                    once(&mut code.types, &base_name(domain.name()));
                }
                !runs
            });
            tables.retain(|table| {
                let runs = table.columns(database).map_or(true, |mut columns| {
                    columns
                        .any(|column| code.names_type(&column.data_type(database).name(database)))
                });
                if runs {
                    once(&mut code.types, &base_name(table.table_name()));
                }
                !runs
            });
            composites.retain(|composite| {
                let runs = composite.attributes(database).is_some_and(|mut fields| {
                    fields.any(|field| code.names_type(&field.data_type().name(database)))
                });
                if runs {
                    once(&mut code.types, &base_name(composite.name()));
                }
                !runs
            });
            // A view's row type holds the types its query outputs, which
            // are not read here, so it may hold any type found.
            if !code.types.is_empty() {
                for view in views.drain(..) {
                    once(&mut code.types, &base_name(view.view_name()));
                }
            }
            if code.types.len() == found {
                return code;
            }
        }
    }

    /// Whether the type written `name` may be, hold, or be an array of a
    /// type whose checks run code outside `pg_catalog`.
    fn names_type(&self, name: &str) -> bool {
        self.types.contains(&base_name(name))
    }

    /// The first operator `node` applies, by writing it or through a form
    /// that compares, that may run a function outside `pg_catalog`.
    pub(super) fn operator_in<N: Visit + ?Sized>(&self, node: &N) -> Option<String> {
        let implied = self.operators.iter().find(|name| IMPLIED.contains(&name.as_str()));
        let written = walk_expressions(node, |expression| {
            let name = match expression {
                Expr::BinaryOp { op, .. }
                | Expr::AnyOp { compare_op: op, .. }
                | Expr::AllOp { compare_op: op, .. } => {
                    binary_operator_name(op).map(|name| name.name().to_owned())
                }
                Expr::UnaryOp { op, .. } => prefix_operator_name(*op).map(ToOwned::to_owned),
                _ => None,
            };
            match name {
                Some(name) if self.runs(&name) => ControlFlow::Break(name),
                _ => ControlFlow::Continue(()),
            }
        });
        match written {
            ControlFlow::Break(name) => Some(name),
            // Any expression may compare, so an implied operator outside
            // `pg_catalog` reaches every node holding one.
            ControlFlow::Continue(()) => {
                implied
                    .filter(|_| walk_expressions(node, |_| ControlFlow::Break(())).is_break())
                    .cloned()
            }
        }
    }

    /// Whether applying the operator `name` may run a function outside
    /// `pg_catalog`, or no operator of that name exists.
    fn runs(&self, name: &str) -> bool {
        self.operators.iter().any(|held| held == name)
            || self.known.as_ref().is_none_or(|known| !known.iter().any(|held| held == name))
    }

    /// The first type `node` casts to whose checks may run code outside
    /// `pg_catalog`, as written.
    pub(super) fn cast_in<N: Visit + ?Sized>(&self, node: &N) -> Option<String> {
        let cast = walk_expressions(node, |expression| {
            let data_type = match expression {
                Expr::Cast { data_type, .. } => data_type,
                Expr::TypedString(typed) => &typed.data_type,
                _ => return ControlFlow::Continue(()),
            };
            let written = data_type.to_string();
            if self.names_type(&written) {
                ControlFlow::Break(written)
            } else {
                ControlFlow::Continue(())
            }
        });
        match cast {
            ControlFlow::Break(written) => Some(written),
            ControlFlow::Continue(()) => None,
        }
    }

    /// The first type `function`'s parameters or the declarations of
    /// `blocks` name whose checks may run code outside `pg_catalog`, as
    /// written. Assigning a value of such a type, a `NULL` included, runs
    /// the checks.
    pub(super) fn declared_in<DB: DatabaseLike>(
        &self,
        function: &DB::Function,
        blocks: &[&Block],
        database: &DB,
    ) -> Option<String> {
        if let Some(name) =
            function.argument_type_names(database).find(|name| self.names_type(name))
        {
            return Some(name.into_owned());
        }
        blocks.iter().flat_map(|block| &block.declarations).find_map(|declaration| {
            let declared = match &declaration.kind {
                DeclarationKind::Variable(variable) => Vec::from([&variable.data_type]),
                DeclarationKind::Cursor(cursor) => {
                    cursor.parameters.iter().map(|parameter| &parameter.data_type).collect()
                }
                DeclarationKind::Alias(_) => Vec::new(),
            };
            declared.into_iter().find_map(|declared| self.declared_type(declared, database))
        })
    }

    /// `declared`, as written, when its checks may run code outside
    /// `pg_catalog`. A `%TYPE` or `%ROWTYPE` of a relation the database
    /// does not hold may name any type.
    fn declared_type<DB: DatabaseLike>(
        &self,
        declared: &DeclaredType,
        database: &DB,
    ) -> Option<String> {
        let (runs, written) = match declared {
            DeclaredType::Data(data_type) => {
                let written = data_type.to_string();
                (self.names_type(&written), written)
            }
            DeclaredType::RowType { of, .. } => {
                (self.relation_runs(of, database), format!("{of}%ROWTYPE"))
            }
            // A single name is a variable or parameter, whose own type is
            // read where it is declared.
            DeclaredType::ColumnType { of, .. } => {
                let relation = of.0.split_last().map(|(_, relation)| ObjectName(relation.to_vec()));
                let runs = relation.is_some_and(|relation| {
                    !relation.0.is_empty() && self.relation_runs(&relation, database)
                });
                (runs, format!("{of}%TYPE"))
            }
        };
        runs.then_some(written)
    }

    /// Whether the relation `name` names holds a type whose checks may run
    /// code outside `pg_catalog`, or the database holds no table of that
    /// name while such a type exists.
    fn relation_runs<DB: DatabaseLike>(&self, name: &ObjectName, database: &DB) -> bool {
        if self.types.is_empty() {
            return false;
        }
        let Some((relation, _)) = object_name_last_part(name) else { return true };
        let relation = base_name(relation);
        self.types.contains(&relation)
            || !database.tables().any(|table| base_name(table.table_name()) == relation)
    }
}

/// The first relation `node` reads, as written, through a name or a `TABLE`
/// query.
pub(super) fn relation_read<N: Visit + ?Sized>(node: &N) -> Option<String> {
    match walk(node, &mut Relations) {
        ControlFlow::Break(relation) => Some(relation),
        ControlFlow::Continue(()) => None,
    }
}

/// Breaks with the first relation a walk meets.
struct Relations;

impl Visitor for Relations {
    type Break = String;

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<String> {
        ControlFlow::Break(relation.to_string())
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<String> {
        table_query(&query.body).map_or(ControlFlow::Continue(()), ControlFlow::Break)
    }
}

/// The relation a `TABLE name` query among the operands of `body` reads.
fn table_query(body: &SetExpr) -> Option<String> {
    match body {
        SetExpr::Table(table) => Some(table.to_string()),
        SetExpr::SetOperation { left, right, .. } => {
            table_query(left).or_else(|| table_query(right))
        }
        _ => None,
    }
}

/// The first call in `node` to a `pg_catalog` function scanning a relation,
/// a schema, a database or a portal, by name.
pub(super) fn scan_in<N: Visit + ?Sized>(node: &N) -> Option<String> {
    let scan = for_each_call(node, |call| {
        match object_name_last_part(call.name) {
            Some((name, false)) if SCANNING.iter().any(|scan| scan.eq_ignore_ascii_case(name)) => {
                ControlFlow::Break(call.name.to_string())
            }
            Some((name, true)) if SCANNING.contains(&name) => {
                ControlFlow::Break(call.name.to_string())
            }
            _ => ControlFlow::Continue(()),
        }
    });
    match scan {
        ControlFlow::Break(name) => Some(name),
        ControlFlow::Continue(()) => None,
    }
}

/// The first `FETCH` or `MOVE` among `nodes` taking a cursor other than one
/// `blocks` declare bound to a query, as written. Any other cursor may name
/// a portal the caller opened, whose query runs as it is fetched.
pub(super) fn foreign_cursor<DB: DatabaseLike>(
    function: &DB::Function,
    blocks: &[&Block],
    nodes: &[&Node],
    database: &DB,
) -> Option<String> {
    nodes.iter().find_map(|node| {
        let (NodeKind::Fetch(sqlparser_plpgsql::embedded::Fetch { cursor, .. })
        | NodeKind::Move { cursor, .. }) = &node.kind
        else {
            return None;
        };
        (!bound_cursor(cursor, function, blocks, nodes, database)).then(|| cursor.to_string())
    })
}

/// Whether `cursor` names a cursor `blocks` declare bound to a query under
/// that name alone, which no parameter shares and no step assigns.
fn bound_cursor<DB: DatabaseLike>(
    cursor: &ObjectName,
    function: &DB::Function,
    blocks: &[&Block],
    nodes: &[&Node],
    database: &DB,
) -> bool {
    let [part] = cursor.0.as_slice() else { return false };
    let Some(ident) = part.as_ident() else { return false };
    let name =
        if ident.quote_style.is_some() { ident.value.clone() } else { ident.value.to_lowercase() };
    let mut declared = blocks
        .iter()
        .flat_map(|block| &block.declarations)
        .filter(|declaration| declaration.name == name);
    let bound = declared.clone().next().is_some()
        && declared.all(|declaration| matches!(declaration.kind, DeclarationKind::Cursor(_)));
    let parameter =
        function.stored_argument_names(database).flatten().any(|parameter| parameter == name);
    bound && !parameter && !nodes.iter().any(|node| assigns(node, &name))
}

/// Whether the step `node` assigns a target whose first part is `name`.
fn assigns(node: &Node, name: &str) -> bool {
    let named = |target: &Target| {
        target
            .name
            .0
            .first()
            .and_then(|part| part.as_ident())
            .is_some_and(|ident| ident.value.eq_ignore_ascii_case(name))
    };
    let into = |into: &Option<IntoTargets>| {
        into.as_ref().is_some_and(|into| into.targets.iter().any(named))
    };
    match &node.kind {
        NodeKind::Assignment(assignment) => named(&assignment.target),
        NodeKind::Fetch(fetch) => fetch.targets.iter().any(named),
        NodeKind::Statement { into: targets, .. } | NodeKind::Dynamic { into: targets, .. } => {
            into(targets)
        }
        _ => false,
    }
}

/// The type `name` writes, unqualified, without array brackets, modifiers
/// or quotes, and lowercase. Comparing these may tell two types alike that
/// PostgreSQL tells apart, never the reverse.
fn base_name(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_prefix("SETOF ").unwrap_or(name);
    let name = name.split('(').next().unwrap_or(name);
    let name = name.trim_end_matches(|character: char| {
        character == '[' || character == ']' || character.is_whitespace()
    });
    let name = name.rsplit('.').next().unwrap_or(name);
    name.trim_matches('"').to_lowercase()
}

/// Pushes `name` to `names` unless it is there.
fn once(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|held| held == name) {
        names.push(name.to_owned());
    }
}

/// Whether `check` calls `set_config`, which changes a setting wherever a
/// value is coerced to the domain the check belongs to.
fn sets_setting(check: &Expr) -> bool {
    walk_expressions(check, |expression| {
        if matches!(setting_call(expression), Some((SettingCall::Set, _))) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}
