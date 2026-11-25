//! Writes that invalidate trigger-row facts.

use alloc::{borrow::Cow, vec::Vec};
use core::{convert::Infallible, ops::ControlFlow, ptr, slice};

use sqlparser::ast::{
    Action, CopySource, FromTable, FunctionSecurity, ObjectName, ReferentialAction, Statement,
    TableFactor, TableObject, Visit, Visitor,
};

use crate::{
    errors::LookupError,
    impls::sqlparser::{
        callee_path::calls_path_changer,
        data_statement::{
            BodyPath, BodyWalk, RunTimeBody, reads_on_path, resolve_on_body_path, walk_body,
        },
        function_body::{SqlBody, hex_strings_may_be_integers, run_time_path, sql_body},
    },
    structs::{ActingRole, EmbeddedSql},
    traits::{
        ColumnGrantLike, DatabaseLike, ForeignKeyLike, FunctionLike, GrantLike, Relation, RoleLike,
        RuleLike, TableGrantLike, TableLike, TriggerLike, ViewLike, role::privilege_holders,
    },
    utils::{object_name::resolve_bound_relation_object_name, walk::walk},
};

/// Whether definer chains can write a table, including chains owned by another
/// role.
#[derive(Default)]
pub(super) struct DefinerWrites {
    pub(super) any: bool,
    pub(super) foreign: bool,
}

/// The strongest ownership boundary a write path crosses.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reach {
    None,
    Direct,
    Foreign,
}

/// Collects transitive partition and inheritance relatives while borrowing a
/// standalone table.
pub(super) fn table_family<'db, 'table, DB: DatabaseLike>(
    database: &'db DB,
    table: &'table &'db DB::Table,
) -> Result<Cow<'table, [&'db DB::Table]>, LookupError> {
    let mut family: Cow<'table, [&'db DB::Table]> = Cow::Borrowed(slice::from_ref(table));
    let mut position = 0;
    while position < family.len() {
        let member: &'db DB::Table = family[position];
        let relatives = member
            .partition_root(database)?
            .into_iter()
            .chain(member.inherits_from(database)?)
            .chain(member.partitions(database)?)
            .chain(member.inheritors(database)?);
        for relative in relatives {
            if !family.iter().any(|seen| ptr::eq(*seen, relative)) {
                family.to_mut().push(relative);
            }
        }
        position += 1;
    }
    Ok(family)
}

/// Whether a key can issue an owner-run write to its referencing table.
fn has_writing_action<DB: DatabaseLike>(key: &DB::ForeignKey, database: &DB) -> bool {
    [key.on_delete(database), key.on_update(database)].into_iter().any(|action| {
        matches!(
            action,
            ReferentialAction::Cascade | ReferentialAction::SetNull | ReferentialAction::SetDefault
        )
    })
}

/// Finds definer roots whose transitive writes can reach `table`.
pub(super) fn definer_writes<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
    role: ActingRole<'db, DB::Role>,
) -> Result<DefinerWrites, LookupError> {
    let mut writes = DefinerWrites::default();
    for function in database.functions() {
        if function.security_mode() != FunctionSecurity::Definer {
            continue;
        }
        let same_owner = match role {
            ActingRole::Declared(role) => {
                function.owner(database)?.is_some_and(|owner| owner == role.stored_name())
            }
            ActingRole::Public => false,
        };
        if writes.foreign {
            break;
        }
        let reached = chain_writes(database, table, function, false, &mut Vec::new());
        writes.any |= reached != Reach::None;
        writes.foreign |= reached == Reach::Foreign || (!same_owner && reached == Reach::Direct);
    }
    if !writes.foreign {
        let mut visited = Vec::new();
        for trigger in database.triggers() {
            let Relation::Table(fired) = trigger.relation(database)? else { continue };
            let mut owner_run = false;
            for member in table_family(database, &fired)?.iter() {
                owner_run |= member
                    .foreign_keys(database)?
                    .any(|key| has_writing_action::<DB>(key, database));
            }
            if !owner_run {
                continue;
            }
            let reached = trigger.function(database).map_or(Reach::Foreign, |function| {
                chain_writes(database, table, function, true, &mut visited)
            });
            if reached != Reach::None {
                writes.any = true;
                writes.foreign = true;
                break;
            }
        }
    }
    Ok(writes)
}

/// Follows calls and triggers fired by writes, refusing unreadable chains.
fn chain_writes<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
    function: &'db DB::Function,
    cascaded: bool,
    visited: &mut Vec<(&'db DB::Function, bool)>,
) -> Reach {
    if visited.iter().any(|(seen, foreign)| ptr::eq(*seen, function) && *foreign == cascaded) {
        return Reach::None;
    }
    visited.push((function, cascaded));
    let Ok(body) = sql_body(function, database) else { return Reach::Foreign };
    let Ok(path) = run_time_path(function, database) else { return Reach::Foreign };
    let bound = matches!(
        &body,
        SqlBody::Atomic(_) | SqlBody::Return(_) | SqlBody::Expression(_) | SqlBody::Select(_)
    );
    let mut targets = Targets { database, path, bound, tables: Vec::new(), uncertain: false };
    let result = match &body {
        SqlBody::Statements(statements) | SqlBody::Atomic(statements) => {
            walk_body(
                &function.stored_name(),
                RunTimeBody::Sql(statements),
                &mut targets,
                hex_strings_may_be_integers(database),
            )
        }
        SqlBody::PlPgSql(block) => {
            walk_body(
                &function.stored_name(),
                RunTimeBody::PlPgSql(block),
                &mut targets,
                hex_strings_may_be_integers(database),
            )
        }
        SqlBody::Expression(expression) => {
            targets.scan(*expression);
            Ok(())
        }
        SqlBody::Return(expression) => {
            targets.scan(expression.as_ref());
            Ok(())
        }
        SqlBody::Select(select) => {
            targets.scan(*select);
            Ok(())
        }
    };
    if result.is_err() || targets.uncertain {
        return Reach::Foreign;
    }
    let Ok(referencing) = cascade_tables(database, &targets.tables) else { return Reach::Foreign };
    let mut reached = if referencing.iter().any(|written| ptr::eq(*written, table)) {
        Reach::Foreign
    } else if targets.tables.iter().any(|written| ptr::eq(*written, table)) {
        if cascaded { Reach::Foreign } else { Reach::Direct }
    } else {
        Reach::None
    };
    let Ok(callees) = function.referenced_functions(database) else { return Reach::Foreign };
    for callee in callees {
        if reached == Reach::Foreign {
            return reached;
        }
        if callee.target_name().schema() != Some("pg_catalog") {
            reached = reached.max(chain_writes(database, table, callee, cascaded, visited));
        }
    }
    for trigger in database.triggers() {
        if reached == Reach::Foreign {
            return reached;
        }
        let fired = match trigger.relation(database) {
            Ok(Relation::Table(fired)) => fired,
            Ok(_) => continue,
            Err(_) => return Reach::Foreign,
        };
        let cascade = referencing.iter().any(|written| ptr::eq(*written, fired));
        if cascade || targets.tables.iter().any(|written| ptr::eq(*written, fired)) {
            let Some(callee) = trigger.function(database) else { return Reach::Foreign };
            reached =
                reached.max(chain_writes(database, table, callee, cascaded || cascade, visited));
        }
    }
    reached
}

/// Collects referencing tables reachable through writing referential actions.
fn cascade_tables<'db, DB: DatabaseLike>(
    database: &'db DB,
    written: &[&'db DB::Table],
) -> Result<Vec<&'db DB::Table>, LookupError> {
    let mut cascades = Vec::new();
    let mut position = 0;
    while position < written.len() + cascades.len() {
        let parent = if position < written.len() {
            written[position]
        } else {
            cascades[position - written.len()]
        };
        position += 1;
        for child in database.tables() {
            if cascades.iter().any(|seen| ptr::eq(*seen, child)) {
                continue;
            }
            for key in child.foreign_keys(database)? {
                if has_writing_action::<DB>(key, database)
                    && ptr::eq(key.referenced_table(database)?, parent)
                {
                    cascades.extend(table_family(database, &child)?.iter().copied());
                    break;
                }
            }
        }
    }
    Ok(cascades)
}

/// Whether foreign-owner views bypass policies or permit rewriting the table.
#[derive(Default)]
pub(super) struct ViewWrites {
    pub(super) bypasses: bool,
    pub(super) rewrites: bool,
}

/// Finds owner-run views reaching `table` and the acting role's write grants on
/// them.
pub(super) fn view_writes<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
    role: ActingRole<'db, DB::Role>,
) -> Result<ViewWrites, LookupError> {
    let holders: Vec<_> = match role {
        ActingRole::Declared(role) => privilege_holders(role, database).collect(),
        ActingRole::Public => Vec::new(),
    };
    let mut writes = ViewWrites::default();
    for view in database.views() {
        if view.is_security_invoker() {
            continue;
        }
        let owner = view.owner(database)?;
        if matches!(role, ActingRole::Declared(role) if owner.is_some_and(|owner| owner == role.stored_name()))
        {
            continue;
        }
        let reads = reads_on_path(view.definition(), database, None)?;
        if !reads.tables.iter().any(|read| ptr::eq(*read, table)) {
            continue;
        }
        writes.bypasses = true;
        writes.rewrites |= database.table_grants().any(|grant| {
            grants_rewrite(grant, database, &holders)
                && grant.relations(database).any(|relation| matches!(relation, Relation::View(granted) if ptr::eq(granted, view)))
        }) || database.column_grants().any(|grant| {
            grants_rewrite(grant, database, &holders)
                && matches!(grant.relation(database), Some(Relation::View(granted)) if ptr::eq(granted, view))
        });
    }
    Ok(writes)
}

/// Whether a grant gives a privilege holder or `PUBLIC` a rewriting command.
fn grants_rewrite<DB: DatabaseLike>(
    grant: &impl GrantLike<DB = DB>,
    database: &DB,
    holders: &[&DB::Role],
) -> bool {
    (grant.applies_to_public() || holders.iter().any(|holder| grant.applies_to_role(holder)))
        && (grant.is_all_privileges()
            || grant
                .privileges(database)
                .any(|action| matches!(action, Action::Update { .. } | Action::Delete)))
}

/// Whether a rewrite rule may write `table`.
pub(super) fn rule_writes<DB: DatabaseLike>(database: &DB, table: &DB::Table) -> bool {
    for rule in database.rules() {
        let mut targets = Targets {
            database,
            path: BodyPath::Database,
            bound: true,
            tables: Vec::new(),
            uncertain: false,
        };
        targets.scan(rule.action());
        if targets.uncertain || targets.tables.iter().any(|written| ptr::eq(*written, table)) {
            return true;
        }
    }
    false
}

/// Resolves mutation targets under the path in force at each statement.
struct Targets<'db, 'path, DB: DatabaseLike> {
    database: &'db DB,
    path: BodyPath<'path>,
    bound: bool,
    tables: Vec<&'db DB::Table>,
    uncertain: bool,
}

impl<DB: DatabaseLike> Targets<'_, '_, DB> {
    /// Walks every mutation target without an early exit.
    fn scan<N: Visit + ?Sized>(&mut self, node: &N) {
        match walk(node, self) {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(never) => match never {},
        }
    }

    /// Records a target or refuses a target whose effects are unknown.
    fn target(&mut self, name: &ObjectName) {
        let resolved = if self.bound {
            resolve_bound_relation_object_name(name, self.database)
        } else {
            resolve_on_body_path(name, self.database, &self.path)
        };
        match resolved {
            Ok(Some(Relation::Table(table))) => {
                match table_family(self.database, &table) {
                    Ok(tree) => {
                        for member in tree.iter() {
                            if !self.tables.iter().any(|seen| ptr::eq(*seen, *member)) {
                                self.tables.push(*member);
                            }
                        }
                    }
                    Err(_) => self.uncertain = true,
                }
            }
            _ => self.uncertain = true,
        }
    }

    /// Records the named table of a mutation target.
    fn factor(&mut self, factor: &TableFactor) {
        if let TableFactor::Table { name, .. } = factor {
            self.target(name);
        } else {
            self.uncertain = true;
        }
    }
}

impl<DB: DatabaseLike> Visitor for Targets<'_, '_, DB> {
    type Break = Infallible;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        match statement {
            Statement::Insert(insert) => {
                match &insert.table {
                    TableObject::TableName(name) => self.target(name),
                    _ => self.uncertain = true,
                }
            }
            Statement::Update(update) => self.factor(&update.table.relation),
            Statement::Delete(delete) => {
                if delete.tables.is_empty() {
                    let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) =
                        &delete.from;
                    for table in tables {
                        self.factor(&table.relation);
                    }
                } else {
                    for table in &delete.tables {
                        self.target(table);
                    }
                }
            }
            Statement::Merge(merge) => self.factor(&merge.table),
            Statement::Copy { to: false, source: CopySource::Table { table_name, .. }, .. } => {
                self.target(table_name);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl<'path, DB: DatabaseLike> BodyWalk<'path> for Targets<'_, 'path, DB> {
    type SavedPath = BodyPath<'path>;

    fn save_path(&self) -> Self::SavedPath {
        self.path.clone()
    }

    fn restore_path(&mut self, saved: Self::SavedPath) {
        self.path = saved;
    }

    fn set_path(&mut self, path: BodyPath<'path>) {
        self.path = path;
    }

    fn walk(&mut self, sql: EmbeddedSql<'_>) -> Result<(), LookupError> {
        self.scan(&sql);
        Ok(())
    }

    fn calls_path_changer(&self, sql: EmbeddedSql<'_>) -> bool {
        calls_path_changer(&sql, self.database)
    }
}
