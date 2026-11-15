//! The definitions a PL/pgSQL body may give its variables, followed through
//! its blocks, branches, loops, handlers and control transfers to every
//! statement and expression it evaluates.
//!
//! The walk holds, at each point, the definitions each variable may hold
//! there, `None` where no path reaches. A raise may happen at any statement
//! and after any write, so the state there joins every enclosing block whose
//! handlers are in force, whichever handler catches it. A loop walks its
//! statements until the state at its head stops growing.

use alloc::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    string::String,
    vec::Vec,
};
use core::fmt::Display;

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName, ObjectNamePart,
    Query, Statement, Value,
};
use sqlparser_plpgsql::{
    embedded::{
        Assignment, Block, BoundCursor, Branch, Declaration, DeclarationKind, Diagnostics,
        DiagnosticsKind, Dynamic, Exit, ExitKind, Indirection, Loop, LoopControl, Node, Open,
        RaiseLevel, Target, expression,
    },
    namespace::{Name, NameKind, Namespace, Site, Sql, TriggerVariable, Write, WriteSite},
};

use super::{
    data_statement::{BodyPath, unresolvable},
    dynamic_sites::{BodyNames, SiteKey, body_sql, namespace_error},
    plpgsql_body::transfer_expressions,
};
use crate::{
    errors::LookupError,
    structs::{
        DefinitionSource, DiagnosticsItem, Output, PlPgSqlScope, PlPgSqlVariable, Rows, TargetStep,
        plpgsql_flow::{Slot, State, ValueFlow, Written},
        plpgsql_scope::stored_parts,
    },
    traits::{DatabaseLike, FunctionLike},
    utils::identifier_resolution::stored_identifier,
};

/// Every definition of the PL/pgSQL body `block` of the function `names`
/// belongs to, and the definitions reaching each statement, expression and
/// dynamic SQL of it.
///
/// # Errors
///
/// Returns the errors [`body_sql`] returns, and
/// [`LookupError::FunctionBodyUnresolvable`] for a write whose target names
/// no variable.
pub(crate) fn value_flow<'b, 'db, DB: DatabaseLike>(
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
) -> Result<ValueFlow<'b, 'db, DB>, LookupError> {
    let function = names.function().name();
    let (sites, paths) = body_sql(names, block, hex_strings_may_be_integers)?;
    let writes = block
        .write_sites(&names.signature(), |name| names.holds_row(name))
        .map_err(|error| namespace_error(function, error))?;
    let mut flow = Flow {
        names,
        namespaces: sites
            .into_iter()
            .map(|Site { sql, namespace }| (SiteKey::of(sql), namespace))
            .collect(),
        paths,
        writes: writes.into_iter().map(|site| (write_address(site.write), site)).collect(),
        definitions: Vec::new(),
        keys: BTreeMap::new(),
        points: BTreeMap::new(),
        frames: Vec::new(),
    };
    flow.block(block, State::new())?;
    Ok(ValueFlow { definitions: flow.definitions, points: flow.points })
}

/// Where `write` sits in the body.
fn write_address(write: Write<'_>) -> usize {
    match write {
        Write::Declaration(declaration) => core::ptr::from_ref(declaration).addr(),
        Write::Node(node) => core::ptr::from_ref(node).addr(),
    }
}

/// The key of the `SELECT` evaluating an expression of the body.
fn query_key(query: &Query) -> SiteKey {
    SiteKey::of(Sql::Expression(query))
}

/// Adds the definitions `from` may hold to `into`.
fn join(into: &mut Option<State>, from: Option<&State>) {
    let Some(from) = from else { return };
    match into {
        Some(state) => merge(state, from),
        None => *into = Some(from.clone()),
    }
}

/// Adds the definitions `from` holds to `state`.
fn merge(state: &mut State, from: &State) {
    // A variable missing from either side holds its entry definition there.
    for (variable, slots) in state.iter_mut() {
        if !from.contains_key(variable) {
            slots.insert(Slot::Entry);
        }
    }
    for (variable, slots) in from {
        state.entry(*variable).or_insert_with(|| BTreeSet::from([Slot::Entry])).extend(slots);
    }
    state.retain(|_, slots| !is_entry(slots));
}

/// Whether `slots` holds the entry definition alone, as a variable missing
/// from a [`State`] does.
fn is_entry(slots: &BTreeSet<Slot>) -> bool {
    slots.len() == 1 && slots.contains(&Slot::Entry)
}

/// A variable a write assigns, and the part of it.
struct Resolved<'b, 'db> {
    variable: PlPgSqlVariable<'db>,
    part: Vec<TargetStep<'b>>,
    /// Whether the target is a whole variable holding a row, which a lone
    /// target of a query takes a whole row into.
    row: bool,
}

/// A definition a write makes each time the walk passes it.
struct Pending<'b, 'db, DB: DatabaseLike> {
    /// The write's address, the target's position and whether this is the
    /// null a loop over no row assigns.
    key: (usize, usize, bool),
    variable: PlPgSqlVariable<'db>,
    part: Vec<TargetStep<'b>>,
    source: DefinitionSource<'b, 'db>,
    scope: Option<PlPgSqlScope<'b, 'db, DB>>,
    /// Whether the definition replaces every earlier one of the variable,
    /// which only a write of the whole variable that surely assigns does.
    kills: bool,
}

/// A block or loop enclosing the point the walk stands at.
struct Frame<'b> {
    label: Option<&'b str>,
    repeats: bool,
    /// Whether the block's handlers catch what its statements raise, as
    /// they do while the walk is among those statements.
    catching: bool,
    /// What a raise the block's handlers may catch leaves.
    raised: Option<State>,
    /// What an `EXIT` leaving the block or loop leaves.
    exited: Option<State>,
    /// What a `CONTINUE` of the loop leaves.
    continued: Option<State>,
}

impl<'b> Frame<'b> {
    const fn new(label: Option<&'b str>, repeats: bool) -> Self {
        Self { label, repeats, catching: false, raised: None, exited: None, continued: None }
    }
}

/// The walk computing the value flow of one body.
struct Flow<'b, 'db, DB: DatabaseLike> {
    names: &'b BodyNames<'db, DB>,
    /// The names each statement, expression and dynamic SQL reaches.
    namespaces: BTreeMap<SiteKey, Namespace>,
    /// The path each runs under.
    paths: BTreeMap<SiteKey, BodyPath<'static>>,
    /// Every write, by the address of its declaration or node.
    writes: BTreeMap<usize, WriteSite<'b>>,
    definitions: Vec<Written<'b, 'db, DB>>,
    /// The index of each definition among `definitions`, by its
    /// [`Pending::key`].
    keys: BTreeMap<(usize, usize, bool), usize>,
    /// The definitions reaching each statement, expression and dynamic SQL
    /// where the body evaluates it.
    points: BTreeMap<SiteKey, State>,
    frames: Vec<Frame<'b>>,
}

impl<'b, 'db, DB: DatabaseLike> Flow<'b, 'db, DB> {
    /// The error the body answers for `sql`, which nothing in it resolves.
    fn unresolvable(&self, sql: &dyn Display) -> LookupError {
        unresolvable(self.names.function().name(), &format_args!("{sql}"))
    }

    /// The names and path the SQL `key` names is evaluated under.
    fn scope(
        &self,
        key: SiteKey,
        sql: &dyn Display,
    ) -> Result<PlPgSqlScope<'b, 'db, DB>, LookupError> {
        match (self.namespaces.get(&key), self.paths.get(&key)) {
            (Some(namespace), Some(path)) => {
                Ok(PlPgSqlScope::new(self.names, namespace.clone(), path.clone()))
            }
            _ => Err(self.unresolvable(sql)),
        }
    }

    /// The names the targets of the write at `address` reach.
    fn targets_namespace(
        &self,
        address: usize,
        sql: &dyn Display,
    ) -> Result<&'_ WriteSite<'b>, LookupError> {
        self.writes.get(&address).ok_or_else(|| self.unresolvable(sql))
    }

    /// The variable the name `parts` writes reaches in `namespace`, with the
    /// parts after it as fields.
    fn lookup(&self, namespace: &Namespace, parts: Vec<String>) -> Option<Resolved<'b, 'db>> {
        let (name, consumed) = namespace.lookup(&parts, |name| self.names.holds_row(name))?;
        let part: Vec<_> = parts.into_iter().skip(consumed).map(TargetStep::Field).collect();
        let row = part.is_empty() && holds_row(self.names, name);
        Some(Resolved { variable: self.names.variable(name), part, row })
    }

    /// The variable `target` assigns in `namespace`, and the part of it.
    fn target(
        &self,
        namespace: &Namespace,
        target: &'b Target,
    ) -> Result<Resolved<'b, 'db>, LookupError> {
        let parts = stored_parts(target.name.0.iter().filter_map(ObjectNamePart::as_ident));
        let mut resolved =
            self.lookup(namespace, parts).ok_or_else(|| self.unresolvable(&target.name))?;
        if !target.indirection.is_empty() {
            resolved.row = false;
        }
        resolved.part.extend(target.indirection.iter().map(|step| {
            match step {
                Indirection::Field(field) => {
                    TargetStep::Field(
                        stored_identifier(&field.value, field.quote_style.is_some()).into_owned(),
                    )
                }
                Indirection::Index(index) => TargetStep::Index(expression(index)),
                Indirection::Slice { lower, upper } => {
                    TargetStep::Slice {
                        lower: lower.as_deref().map(expression),
                        upper: upper.as_deref().map(expression),
                    }
                }
            }
        }));
        Ok(resolved)
    }

    /// The definitions the write at `address` gives `targets`, each the
    /// output `source` builds, evaluated under `scope`, or the null a loop
    /// over no row assigns with `null`.
    ///
    /// A lone target holding a row takes the whole row, any other the
    /// column at its position. With `whole`, a lone target takes the whole
    /// value, as `FOREACH` assigns an element.
    #[expect(clippy::too_many_arguments, reason = "each describes one side of the write")]
    fn targets(
        &self,
        address: usize,
        targets: &'b [Target],
        whole: bool,
        null: bool,
        scope: Option<&PlPgSqlScope<'b, 'db, DB>>,
        source: impl Fn(Output) -> DefinitionSource<'b, 'db>,
        sql: &dyn Display,
    ) -> Result<Vec<Pending<'b, 'db, DB>>, LookupError> {
        let namespace = &self.targets_namespace(address, sql)?.namespace;
        let resolved: Vec<_> = targets
            .iter()
            .map(|target| self.target(namespace, target))
            .collect::<Result<_, _>>()?;
        let lone = match resolved.as_slice() {
            [target] => whole || target.row,
            _ => false,
        };
        Ok(resolved
            .into_iter()
            .enumerate()
            .map(|(position, Resolved { variable, part, .. })| {
                let (source, scope) = if null {
                    (DefinitionSource::Null, None)
                } else {
                    let output = if lone { Output::Row } else { Output::Ordinal(position) };
                    (source(output), scope.cloned())
                };
                Pending {
                    key: (address, position, null),
                    variable,
                    part,
                    source,
                    scope,
                    kills: true,
                }
            })
            .collect())
    }

    /// Makes the definition `pending`, whose source is evaluated where
    /// `inputs` reach, and leaves it in `state`.
    fn write(&mut self, pending: &Pending<'b, 'db, DB>, inputs: &State, state: &mut State) {
        let index = self.define(pending, inputs);
        let slots =
            state.entry(pending.variable.id()).or_insert_with(|| BTreeSet::from([Slot::Entry]));
        if pending.kills && pending.part.is_empty() {
            slots.clear();
        }
        slots.insert(Slot::Written(index));
        self.raised(state);
    }

    /// Every write in `writes`, each source evaluated where `state` reaches.
    fn write_all(&mut self, writes: &[Pending<'b, 'db, DB>], state: &mut State) {
        let inputs = state.clone();
        for pending in writes {
            self.write(pending, &inputs, state);
        }
    }

    /// The index of the definition `pending`, made on the walk's first pass
    /// over its write, so definitions follow the body's order.
    fn define(&mut self, pending: &Pending<'b, 'db, DB>, inputs: &State) -> usize {
        let index = *self.keys.entry(pending.key).or_insert_with(|| {
            self.definitions.push(Written {
                variable: pending.variable.clone(),
                part: pending.part.clone(),
                source: pending.source.clone(),
                scope: pending.scope.clone(),
                inputs: None,
            });
            self.definitions.len() - 1
        });
        join(&mut self.definitions[index].inputs, Some(inputs));
        index
    }

    /// Hands what a raise at a point holding `state` leaves to every
    /// enclosing block whose handlers may catch it.
    fn raised(&mut self, state: &State) {
        for frame in self.frames.iter_mut().filter(|frame| frame.catching) {
            join(&mut frame.raised, Some(state));
        }
    }

    /// Records `state` as reaching the SQL `key` names where the body
    /// evaluates it.
    fn evaluated(&mut self, key: SiteKey, state: &State) {
        match self.points.entry(key) {
            Entry::Occupied(mut reaching) => merge(reaching.get_mut(), state),
            Entry::Vacant(reaching) => {
                reaching.insert(state.clone());
            }
        }
    }

    /// Records `state` as reaching each `SELECT` of `queries`.
    fn evaluated_queries<'q>(
        &mut self,
        queries: impl IntoIterator<Item = &'q Query>,
        state: &State,
    ) {
        for query in queries {
            self.evaluated(query_key(query), state);
        }
    }

    /// Records `state` as reaching the arguments and the query of `cursor`
    /// as it opens.
    fn opened(&mut self, cursor: &BoundCursor, state: &State) {
        self.evaluated_queries(&cursor.arguments, state);
        self.evaluated(SiteKey::of(Sql::Statement(&cursor.query)), state);
    }

    /// Walks `nodes` from `state`, `None` where no path reaches them.
    fn nodes(
        &mut self,
        nodes: &'b [Node],
        mut state: Option<State>,
    ) -> Result<Option<State>, LookupError> {
        for node in nodes {
            let Some(reached) = state else { return Ok(None) };
            state = self.node(node, reached)?;
        }
        Ok(state)
    }

    /// Walks `node` from `state`, giving what it leaves, `None` where it
    /// leaves on no path.
    fn node(&mut self, node: &'b Node, mut state: State) -> Result<Option<State>, LookupError> {
        self.raised(&state);
        let address = core::ptr::from_ref(node).addr();
        match node {
            Node::Statement { statement, into } => {
                let key = SiteKey::of(Sql::Statement(statement));
                self.evaluated(key, &state);
                if let Some(into) = into {
                    let scope = self.scope(key, statement)?;
                    let rows = Rows::First { strict: into.strict };
                    let source = |output| DefinitionSource::QueryOutput { statement, output, rows };
                    let writes = self.targets(
                        address,
                        &into.targets,
                        false,
                        false,
                        Some(&scope),
                        source,
                        statement,
                    )?;
                    self.write_all(&writes, &mut state);
                } else if let Statement::Call(_) = &**statement {
                    let writes = self.call(address, statement)?;
                    self.write_all(&writes, &mut state);
                }
            }
            Node::Assignment(assignment) => self.assignment(address, assignment, &mut state)?,
            Node::Dynamic { dynamic, into } => {
                let key = SiteKey::of(Sql::Dynamic(dynamic));
                self.evaluated(key, &state);
                if let Some(into) = into {
                    let command = expression(&dynamic.command);
                    let scope = self.scope(key, &command)?;
                    let rows = Rows::First { strict: into.strict };
                    let source = |output| dynamic_output(dynamic, output, rows);
                    let writes = self.targets(
                        address,
                        &into.targets,
                        false,
                        false,
                        Some(&scope),
                        source,
                        &command,
                    )?;
                    self.write_all(&writes, &mut state);
                }
            }
            Node::Fetch(fetch) => {
                let cursor = self.cursor(address, &fetch.cursor)?;
                let source =
                    |output| DefinitionSource::CursorOutput { cursor: cursor.clone(), output };
                let writes = self.targets(
                    address,
                    &fetch.targets,
                    false,
                    false,
                    None,
                    source,
                    &fetch.cursor,
                )?;
                self.write_all(&writes, &mut state);
            }
            Node::Diagnostics(diagnostics) => {
                self.diagnostics(address, diagnostics, &mut state)?;
            }
            Node::Open(open) => self.open(address, open, &mut state)?,
            Node::Block(block) => return self.block(block, state),
            Node::If(choice) => return self.branches(&choice.branches, &choice.otherwise, state),
            // The parser does not tell a missing `ELSE` from an empty one, so
            // a value no branch matches also falls through, beside the
            // `case_not_found` the raise at the statement stands for.
            Node::Case(choice) => {
                self.evaluated_queries(choice.subject.as_deref(), &state);
                return self.branches(&choice.branches, &choice.otherwise, state);
            }
            Node::Loop(repeated) => return self.repeated(address, repeated, &state),
            Node::Exit(exit) => {
                self.evaluated_queries(transfer_expressions(node), &state);
                self.exit(exit, &state);
                if exit.condition.is_none() {
                    return Ok(None);
                }
            }
            Node::Return { .. } => {
                self.evaluated_queries(transfer_expressions(node), &state);
                return Ok(None);
            }
            Node::Raise(raise) => {
                self.evaluated_queries(transfer_expressions(node), &state);
                if raise.level == RaiseLevel::Exception {
                    return Ok(None);
                }
            }
            Node::Assert(_) => self.evaluated_queries(transfer_expressions(node), &state),
        }
        Ok(Some(state))
    }

    /// Writes the value `assignment` at `address` gives its target.
    fn assignment(
        &mut self,
        address: usize,
        assignment: &'b Assignment,
        state: &mut State,
    ) -> Result<(), LookupError> {
        let subscripts = assignment.target.indirection.iter().flat_map(|step| {
            match step {
                Indirection::Field(_) => [None, None],
                Indirection::Index(index) => [Some(&**index), None],
                Indirection::Slice { lower, upper } => [lower.as_deref(), upper.as_deref()],
            }
            .into_iter()
            .flatten()
        });
        self.evaluated_queries(subscripts, state);
        let value = &*assignment.value;
        self.evaluated(query_key(value), state);
        let target = &assignment.target;
        let namespace = &self.targets_namespace(address, &target.name)?.namespace;
        let Resolved { variable, part, .. } = self.target(namespace, target)?;
        let pending = Pending {
            key: (address, 0, false),
            variable,
            part,
            source: DefinitionSource::Value(expression(value)),
            scope: Some(self.scope(query_key(value), value)?),
            kills: true,
        };
        self.write_all(core::slice::from_ref(&pending), state);
        Ok(())
    }

    /// Writes what `diagnostics` at `address` reads into its targets.
    fn diagnostics(
        &mut self,
        address: usize,
        diagnostics: &'b Diagnostics,
        state: &mut State,
    ) -> Result<(), LookupError> {
        let mut writes = Vec::new();
        for (position, item) in diagnostics.items.iter().enumerate() {
            let namespace = &self.targets_namespace(address, &item.target.name)?.namespace;
            let Resolved { variable, part, .. } = self.target(namespace, &item.target)?;
            writes.push(Pending {
                key: (address, position, false),
                variable,
                part,
                source: DefinitionSource::Diagnostics(diagnostics_item(item.kind)),
                scope: None,
                kills: true,
            });
        }
        self.write_all(&writes, state);
        Ok(())
    }

    /// Writes the portal name `open` at `address` may give its cursor.
    fn open(
        &mut self,
        address: usize,
        open: &'b Open,
        state: &mut State,
    ) -> Result<(), LookupError> {
        match open {
            Open::Bound(cursor) => self.opened(cursor, state),
            Open::Query { query, .. } => {
                self.evaluated(SiteKey::of(Sql::Statement(query)), state);
            }
            Open::Dynamic { dynamic, .. } => {
                self.evaluated(SiteKey::of(Sql::Dynamic(dynamic)), state);
            }
        }
        // A cursor variable already naming a portal keeps its name.
        let pending = Pending {
            key: (address, 0, false),
            variable: self.cursor(address, open.cursor())?,
            part: Vec::new(),
            source: DefinitionSource::PortalName,
            scope: None,
            kills: false,
        };
        self.write_all(core::slice::from_ref(&pending), state);
        Ok(())
    }

    /// The cursor variable `cursor` names at the write at `address`.
    fn cursor(
        &self,
        address: usize,
        cursor: &ObjectName,
    ) -> Result<PlPgSqlVariable<'db>, LookupError> {
        let namespace = &self.targets_namespace(address, cursor)?.namespace;
        let parts = stored_parts(cursor.0.iter().filter_map(ObjectNamePart::as_ident));
        match self.lookup(namespace, parts) {
            Some(Resolved { variable, part, .. }) if part.is_empty() => Ok(variable),
            _ => Err(self.unresolvable(cursor)),
        }
    }

    /// The value the procedure `call` may return to each variable it
    /// passes, listed beside the variable's earlier definitions. Only the
    /// procedure's signature tells which parameters return one, and the
    /// model holds no procedure.
    fn call(
        &self,
        address: usize,
        call: &'b Statement,
    ) -> Result<Vec<Pending<'b, 'db, DB>>, LookupError> {
        let scope = self.scope(SiteKey::of(Sql::Statement(call)), call)?;
        let namespace = &self.targets_namespace(address, call)?.namespace;
        let mut writes = Vec::new();
        for (argument, mut passed) in call_arguments(call) {
            // Parentheses and a cast to the variable's own type leave the
            // argument writable, and a cast to another type fails the call.
            while let Expr::Nested(inner) | Expr::Cast { expr: inner, .. } = passed {
                passed = inner;
            }
            let parts = match passed {
                Expr::Identifier(ident) => stored_parts(core::slice::from_ref(ident)),
                Expr::CompoundIdentifier(idents) => stored_parts(idents),
                Expr::Value(value) => {
                    match &value.value {
                        Value::Placeholder(placeholder) => Vec::from([placeholder.clone()]),
                        _ => continue,
                    }
                }
                _ => continue,
            };
            let Some(Resolved { variable, part, .. }) = self.lookup(namespace, parts) else {
                continue;
            };
            writes.push(Pending {
                key: (address, argument, false),
                variable,
                part,
                source: DefinitionSource::CallOutput { call, argument },
                scope: Some(scope.clone()),
                kills: false,
            });
        }
        Ok(writes)
    }

    /// Walks a block, its declarations evaluated before its handlers take
    /// effect, and each handler from what any raise among its statements
    /// leaves.
    fn block(&mut self, block: &'b Block, mut state: State) -> Result<Option<State>, LookupError> {
        self.frames.push(Frame::new(block.label.as_deref(), false));
        for declaration in &block.declarations {
            self.raised(&state);
            self.declaration(declaration, &mut state)?;
        }
        let index = self.frames.len() - 1;
        self.frames[index].catching = !block.handlers.is_empty();
        let mut end = self.nodes(&block.statements, Some(state))?;
        self.frames[index].catching = false;
        let raised = self.frames[index].raised.take();
        for handler in &block.handlers {
            let handled = self.nodes(handler, raised.clone())?;
            join(&mut end, handled.as_ref());
        }
        let exited = self.frames.pop().and_then(|frame| frame.exited);
        join(&mut end, exited.as_ref());
        Ok(end)
    }

    /// Writes the value `declaration` gives its variable on entering its
    /// block.
    fn declaration(
        &mut self,
        declaration: &'b Declaration,
        state: &mut State,
    ) -> Result<(), LookupError> {
        let (source, scope) = match &declaration.kind {
            DeclarationKind::Variable(declared) => {
                match declared.default.as_deref() {
                    Some(default) => {
                        self.evaluated(query_key(default), state);
                        let scope = self.scope(query_key(default), default)?;
                        (DefinitionSource::Value(expression(default)), Some(scope))
                    }
                    None => (DefinitionSource::Null, None),
                }
            }
            DeclarationKind::Cursor(_) => (DefinitionSource::CursorName, None),
            // An alias declares no variable of its own, nor a write.
            DeclarationKind::Alias(_) => return Ok(()),
        };
        let address = core::ptr::from_ref(declaration).addr();
        let Some(name) = &self.targets_namespace(address, &declaration.name)?.declared else {
            return Err(self.unresolvable(&declaration.name));
        };
        let variable = self.names.variable(name);
        let pending = Pending {
            key: (address, 0, false),
            variable,
            part: Vec::new(),
            source,
            scope,
            kills: true,
        };
        self.write_all(core::slice::from_ref(&pending), state);
        Ok(())
    }

    /// Walks the branches of an `IF` or `CASE`, each condition tried after
    /// the ones before it failed, and the statements after none matched.
    fn branches(
        &mut self,
        branches: &'b [Branch],
        otherwise: &'b [Node],
        state: State,
    ) -> Result<Option<State>, LookupError> {
        let mut end = None;
        for branch in branches {
            self.evaluated(query_key(&branch.condition), &state);
            let taken = self.nodes(&branch.statements, Some(state.clone()))?;
            join(&mut end, taken.as_ref());
        }
        let fallen = self.nodes(otherwise, Some(state))?;
        join(&mut end, fallen.as_ref());
        Ok(end)
    }

    /// Hands `state` to the block or loop `exit` leaves or continues, the
    /// innermost carrying its label, or the innermost loop without one. A
    /// label nothing carries is the function's, which leaves the body.
    fn exit(&mut self, exit: &Exit, state: &State) {
        let frame = match &exit.label {
            Some(label) => {
                self.frames.iter_mut().rev().find(|frame| frame.label == Some(label.as_str()))
            }
            None => self.frames.iter_mut().rev().find(|frame| frame.repeats),
        };
        let Some(frame) = frame else { return };
        match exit.kind {
            ExitKind::Exit => join(&mut frame.exited, Some(state)),
            ExitKind::Continue => join(&mut frame.continued, Some(state)),
        }
    }

    /// Walks the loop at `address` until the state at its head stops
    /// growing.
    fn repeated(
        &mut self,
        address: usize,
        repeated: &'b Loop,
        entry: &State,
    ) -> Result<Option<State>, LookupError> {
        match &repeated.control {
            LoopControl::Unconditional | LoopControl::While(_) => {}
            LoopControl::Range { lower, upper, step, .. } => {
                let bounds = [&**lower, &**upper].into_iter().chain(step.as_deref());
                self.evaluated_queries(bounds, entry);
            }
            LoopControl::Query { query, .. } => {
                self.evaluated(SiteKey::of(Sql::Statement(query)), entry);
            }
            LoopControl::Cursor { cursor, .. } => self.opened(cursor, entry),
            LoopControl::Dynamic { dynamic, .. } => {
                self.evaluated(SiteKey::of(Sql::Dynamic(dynamic)), entry);
            }
            LoopControl::Array { array, .. } => self.evaluated(query_key(array), entry),
        }
        let (each, none) = self.iterations(address, &repeated.control)?;
        // Made before the statements' definitions, in the loop's place in
        // the body.
        for pending in &none {
            self.define(pending, entry);
        }
        self.frames.push(Frame::new(repeated.label.as_deref(), true));
        let mut head = entry.clone();
        let back = loop {
            if let LoopControl::While(condition) = &repeated.control {
                self.evaluated(query_key(condition), &head);
            }
            self.raised(&head);
            let mut state = head.clone();
            for pending in &each {
                self.write(pending, entry, &mut state);
            }
            let mut back = self.nodes(&repeated.statements, Some(state))?;
            join(&mut back, self.frames.last().and_then(|frame| frame.continued.as_ref()));
            let mut next = entry.clone();
            if let Some(back) = &back {
                merge(&mut next, back);
            }
            if next == head {
                break back;
            }
            head = next;
        };
        let exited = self.frames.pop().and_then(|frame| frame.exited);
        let mut exit = match &repeated.control {
            LoopControl::Unconditional => None,
            // A loop over no row assigns its targets a null.
            LoopControl::Query { .. } | LoopControl::Dynamic { .. } => {
                let mut empty = entry.clone();
                for pending in &none {
                    self.write(pending, entry, &mut empty);
                }
                if let Some(back) = &back {
                    merge(&mut empty, back);
                }
                Some(empty)
            }
            LoopControl::While(_)
            | LoopControl::Range { .. }
            | LoopControl::Cursor { .. }
            | LoopControl::Array { .. } => Some(head),
        };
        join(&mut exit, exited.as_ref());
        if let (LoopControl::Cursor { .. }, [_, portal], Some(state)) =
            (&repeated.control, each.as_slice(), &mut exit)
        {
            // Leaving the loop, `EXIT` included, gives a cursor variable the
            // loop named back its null.
            let named = self.keys.get(&portal.key).map(|&index| Slot::Written(index));
            state.entry(portal.variable.id()).and_modify(|slots| {
                slots.retain(|slot| Some(slot) != named.as_ref());
            });
        }
        Ok(exit)
    }

    /// The definitions each iteration of the loop at `address` makes, and
    /// the ones a loop over no row makes instead.
    #[expect(clippy::type_complexity, reason = "two lists of one type")]
    fn iterations(
        &self,
        address: usize,
        control: &'b LoopControl,
    ) -> Result<(Vec<Pending<'b, 'db, DB>>, Vec<Pending<'b, 'db, DB>>), LookupError> {
        let declared = |variable: &str| -> Result<PlPgSqlVariable<'db>, LookupError> {
            let name = self.targets_namespace(address, &variable)?.declared.as_ref();
            name.map(|name| self.names.variable(name)).ok_or_else(|| self.unresolvable(&variable))
        };
        Ok(match control {
            LoopControl::Unconditional | LoopControl::While(_) => (Vec::new(), Vec::new()),
            LoopControl::Range { variable, lower, upper, step } => {
                let pending = Pending {
                    key: (address, 0, false),
                    variable: declared(variable)?,
                    part: Vec::new(),
                    source: DefinitionSource::LoopIndex {
                        lower: expression(lower),
                        upper: expression(upper),
                        step: step.as_deref().map(expression),
                    },
                    scope: Some(self.scope(query_key(lower), lower)?),
                    kills: true,
                };
                (Vec::from([pending]), Vec::new())
            }
            LoopControl::Query { targets, query } => {
                let scope = self.scope(SiteKey::of(Sql::Statement(query)), query)?;
                let source = |output| {
                    DefinitionSource::QueryOutput { statement: query, output, rows: Rows::Each }
                };
                (
                    self.targets(address, targets, false, false, Some(&scope), source, query)?,
                    self.targets(address, targets, false, true, None, source, query)?,
                )
            }
            LoopControl::Cursor { variable, cursor } => {
                let query = &*cursor.query;
                let row = Pending {
                    key: (address, 0, false),
                    variable: declared(variable)?,
                    part: Vec::new(),
                    source: DefinitionSource::QueryOutput {
                        statement: query,
                        output: Output::Row,
                        rows: Rows::Each,
                    },
                    scope: Some(self.scope(SiteKey::of(Sql::Statement(query)), query)?),
                    kills: true,
                };
                // A cursor variable holding null names the portal the loop
                // opens until the loop ends, and keeps any other name.
                let portal = Pending {
                    key: (address, 1, false),
                    variable: self.cursor(address, &cursor.name)?,
                    part: Vec::new(),
                    source: DefinitionSource::PortalName,
                    scope: None,
                    kills: false,
                };
                (Vec::from([row, portal]), Vec::new())
            }
            LoopControl::Dynamic { targets, dynamic } => {
                let command = expression(&dynamic.command);
                let scope = self.scope(SiteKey::of(Sql::Dynamic(dynamic)), &command)?;
                let source = |output| dynamic_output(dynamic, output, Rows::Each);
                (
                    self.targets(address, targets, false, false, Some(&scope), source, &command)?,
                    self.targets(address, targets, false, true, None, source, &command)?,
                )
            }
            LoopControl::Array { targets, slice, array } => {
                let scope = self.scope(query_key(array), array)?;
                let source = |output| {
                    DefinitionSource::ArrayElement {
                        array: expression(array),
                        slice: *slice,
                        output,
                    }
                };
                (
                    self.targets(address, targets, true, false, Some(&scope), source, array)?,
                    Vec::new(),
                )
            }
        })
    }
}

/// What `dynamic` returns, the part `output` of the rows `rows`.
fn dynamic_output(dynamic: &Dynamic, output: Output, rows: Rows) -> DefinitionSource<'_, 'static> {
    DefinitionSource::DynamicOutput {
        command: expression(&dynamic.command),
        arguments: dynamic.arguments.iter().map(expression).collect(),
        output,
        rows,
    }
}

/// The expressions `CALL` passes its procedure, each with its position
/// among the arguments written, from 0.
fn call_arguments(call: &Statement) -> impl Iterator<Item = (usize, &Expr)> {
    let arguments = match call {
        Statement::Call(Function { args: FunctionArguments::List(list), .. }) => {
            list.args.as_slice()
        }
        _ => &[],
    };
    arguments.iter().enumerate().filter_map(|(position, argument)| {
        match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expression))
            | FunctionArg::Named { arg: FunctionArgExpr::Expr(expression), .. }
            | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(expression), .. } => {
                Some((position, expression))
            }
            FunctionArg::Unnamed(_) | FunctionArg::Named { .. } | FunctionArg::ExprNamed { .. } => {
                None
            }
        }
    })
}

/// Whether the variable `name` reaches holds a row, asking `names` for the
/// variables whose declared type decides.
fn holds_row<DB: DatabaseLike>(names: &BodyNames<'_, DB>, name: &Name) -> bool {
    match name.kind() {
        NameKind::LoopRecord { .. }
        | NameKind::Trigger(TriggerVariable::New | TriggerVariable::Old) => true,
        NameKind::Parameter { .. } | NameKind::CursorParameter { .. } => names.holds_row(name),
        NameKind::Declared { declaration, .. } => {
            matches!(declaration.kind, DeclarationKind::Variable(_)) && names.holds_row(name)
        }
        NameKind::ReturnValue
        | NameKind::Trigger(_)
        | NameKind::Found
        | NameKind::LoopIndex { .. }
        | NameKind::SqlState
        | NameKind::SqlErrm => false,
    }
}

/// The item `GET DIAGNOSTICS` reads as `kind`.
const fn diagnostics_item(kind: DiagnosticsKind) -> DiagnosticsItem {
    match kind {
        DiagnosticsKind::RowCount => DiagnosticsItem::RowCount,
        DiagnosticsKind::RoutineOid => DiagnosticsItem::RoutineOid,
        DiagnosticsKind::Context => DiagnosticsItem::Context,
        DiagnosticsKind::ReturnedSqlstate => DiagnosticsItem::ReturnedSqlstate,
        DiagnosticsKind::ColumnName => DiagnosticsItem::ColumnName,
        DiagnosticsKind::ConstraintName => DiagnosticsItem::ConstraintName,
        DiagnosticsKind::DatatypeName => DiagnosticsItem::DatatypeName,
        DiagnosticsKind::MessageText => DiagnosticsItem::MessageText,
        DiagnosticsKind::TableName => DiagnosticsItem::TableName,
        DiagnosticsKind::SchemaName => DiagnosticsItem::SchemaName,
        DiagnosticsKind::ExceptionDetail => DiagnosticsItem::ExceptionDetail,
        DiagnosticsKind::ExceptionHint => DiagnosticsItem::ExceptionHint,
        DiagnosticsKind::ExceptionContext => DiagnosticsItem::ExceptionContext,
    }
}
