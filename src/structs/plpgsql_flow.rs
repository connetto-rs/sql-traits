//! The values a PL/pgSQL body may give its variables, and which of them may
//! reach each place the body reads one.

use alloc::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    string::String,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{ArgMode, Expr, Statement, Value, Visit};

use crate::{
    errors::LookupError,
    impls::{SiteKey, dql::scoped_roots::ScopedRoot},
    structs::{
        ColumnDefinition, ExpressionContext, ExpressionScope, IdentifierCase, PlPgSqlScope,
        PlPgSqlVariable, VariableKind,
    },
    traits::DatabaseLike,
    utils::walk::walk_expressions,
};

/// Which part of what a query or a dynamic command returns a definition
/// assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Output {
    /// The output column at this position, from 0, as each of several
    /// targets and a lone scalar target take one.
    Ordinal(usize),
    /// The whole row, as a lone row or record target takes it.
    Row,
}

/// Which rows of a query or a dynamic command a definition assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rows {
    /// The first row, as `INTO` assigns it. Without `STRICT`, a command
    /// returning no row assigns a null instead, which this definition stands
    /// for too. With `STRICT`, any row count but one raises an error and
    /// assigns nothing.
    First {
        /// Whether `INTO STRICT` is written.
        strict: bool,
    },
    /// Every row in turn, as a `FOR` loop assigns them to its targets. A
    /// loop over no row assigns a null, which a [`DefinitionSource::Null`]
    /// definition of the same targets stands for.
    Each,
}

/// One step from a variable into the part of it a definition assigns.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a step is built once per write the walk meets and read in place, so boxing an \
              expression would add an allocation without saving one"
)]
pub enum TargetStep<'r> {
    /// A field of a row or record, by the name PostgreSQL stores.
    Field(String),
    /// An array element, at the index the expression evaluates.
    Index(Cow<'r, Expr>),
    /// An array slice, between the bounds the expressions evaluate, either
    /// of which may be omitted.
    Slice {
        /// The lower bound.
        lower: Option<Cow<'r, Expr>>,
        /// The upper bound.
        upper: Option<Cow<'r, Expr>>,
    },
}

/// What `GET DIAGNOSTICS` assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticsItem {
    /// `ROW_COUNT`.
    RowCount,
    /// `PG_ROUTINE_OID`.
    RoutineOid,
    /// `PG_CONTEXT`.
    Context,
    /// `RETURNED_SQLSTATE`.
    ReturnedSqlstate,
    /// `COLUMN_NAME`.
    ColumnName,
    /// `CONSTRAINT_NAME`.
    ConstraintName,
    /// `PG_DATATYPE_NAME`.
    DatatypeName,
    /// `MESSAGE_TEXT`.
    MessageText,
    /// `TABLE_NAME`.
    TableName,
    /// `SCHEMA_NAME`.
    SchemaName,
    /// `PG_EXCEPTION_DETAIL`.
    ExceptionDetail,
    /// `PG_EXCEPTION_HINT`.
    ExceptionHint,
    /// `PG_EXCEPTION_CONTEXT`.
    ExceptionContext,
}

/// Where the value a definition assigns comes from.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a source is built once per write the walk meets and read in place, so boxing an \
              expression would add an allocation without saving one"
)]
pub enum DefinitionSource<'r, 'db> {
    /// The value an input argument, a trigger function's variable,
    /// `SQLSTATE` or `SQLERRM` holds when the function starts or its handler
    /// runs.
    Incoming,
    /// The null a declaration without a default, an output argument, `$0`
    /// and the targets of a `FOR` loop over no row hold.
    Null,
    /// The value of an expression, as an assignment or a declaration's
    /// default gives it.
    Value(Cow<'r, Expr>),
    /// An output of a query PostgreSQL plans with the body, as `SELECT ...
    /// INTO`, `INSERT ... RETURNING ... INTO`, a `FOR` loop over a query and
    /// a `FOR` loop over a cursor declared with its query assign it.
    QueryOutput {
        /// The query, its `INTO` clause dropped.
        statement: &'r Statement,
        /// The part of each row assigned.
        output: Output,
        /// The rows assigned.
        rows: Rows,
    },
    /// An output of SQL the body builds at run time, as `EXECUTE ... INTO`
    /// and `FOR ... IN EXECUTE` assign it, which only running the command
    /// tells.
    DynamicOutput {
        /// The expression whose value is the command.
        command: Cow<'r, Expr>,
        /// The `USING` expressions.
        arguments: Vec<Cow<'r, Expr>>,
        /// The part of each row assigned.
        output: Output,
        /// The rows assigned.
        rows: Rows,
    },
    /// A row `FETCH` reads from a cursor, or a null when none is left, from
    /// whichever query the cursor was opened with.
    CursorOutput {
        /// The cursor variable.
        cursor: PlPgSqlVariable<'db>,
        /// The part of the row assigned.
        output: Output,
    },
    /// Every element or slice of an array in turn, as `FOREACH` assigns it.
    ArrayElement {
        /// The expression whose value is the array.
        array: Cow<'r, Expr>,
        /// The `SLICE` dimension, 0 for single elements.
        slice: u32,
        /// The part of each element assigned.
        output: Output,
    },
    /// Every integer of a `FOR` loop's range in turn.
    LoopIndex {
        /// The first bound written.
        lower: Cow<'r, Expr>,
        /// The second bound written.
        upper: Cow<'r, Expr>,
        /// The `BY` step.
        step: Option<Cow<'r, Expr>>,
    },
    /// The cursor's own name, which a cursor declared with its query holds
    /// on entering its block.
    CursorName,
    /// A portal name PostgreSQL generates, which `OPEN` and a `FOR` loop
    /// over a cursor give a cursor variable holding null.
    PortalName,
    /// What `GET DIAGNOSTICS` reads.
    Diagnostics(DiagnosticsItem),
    /// The value a procedure's `INOUT` or `OUT` parameter may return to the
    /// variable `CALL` passes there. Every variable passed has one, listed
    /// beside its earlier definitions, since the model holds no procedure
    /// whose signature tells which parameters return a value.
    CallOutput {
        /// The `CALL`.
        call: &'r Statement,
        /// The argument's position among those written, from 0.
        argument: usize,
    },
    /// A value nothing in the body tells, such as `FOUND` and a cursor
    /// parameter.
    Unknown,
}

/// The definitions that may reach one point of a body, by variable id, a
/// variable absent from the map holding only its entry definition.
pub(crate) type State = BTreeMap<usize, BTreeSet<Slot>>;

/// One definition a [`State`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Slot {
    /// The value the variable holds before the body writes it.
    Entry,
    /// The written definition at this index of [`ValueFlow::definitions`].
    Written(usize),
}

/// A definition a statement of the body writes.
#[derive(Debug)]
pub(crate) struct Written<'r, 'db, DB: DatabaseLike> {
    pub(crate) variable: PlPgSqlVariable<'db>,
    pub(crate) part: Vec<TargetStep<'r>>,
    pub(crate) source: DefinitionSource<'r, 'db>,
    /// The names and path the SQL of the source is evaluated under, `None`
    /// for a source without SQL.
    pub(crate) scope: Option<PlPgSqlScope<'r, 'db, DB>>,
    /// The definitions reaching the point where the source is evaluated,
    /// `None` while no path reaches it.
    pub(crate) inputs: Option<State>,
}

/// Every definition of a body and the definitions reaching each statement,
/// expression and dynamic SQL of it.
#[derive(Debug)]
pub(crate) struct ValueFlow<'r, 'db, DB: DatabaseLike> {
    pub(crate) definitions: Vec<Written<'r, 'db, DB>>,
    /// The definitions reaching each statement, expression and dynamic SQL
    /// some path reaches, where the body evaluates it.
    pub(crate) points: BTreeMap<SiteKey, State>,
}

impl<'r, 'db, DB: DatabaseLike> ValueFlow<'r, 'db, DB> {
    /// Each of `variables` with the definitions `state` lets reach it, or
    /// none when no path reaches the point `state` stands for.
    pub(crate) fn reaching(
        &'r self,
        variables: Vec<PlPgSqlVariable<'db>>,
        state: Option<&State>,
    ) -> Vec<VariableRead<'r, 'db, DB>> {
        variables
            .into_iter()
            .map(|variable| {
                let reaching = match state {
                    None => Reaching::Unreachable,
                    Some(state) => Reaching::Definitions(self.definitions_of(state, &variable)),
                };
                VariableRead { variable, reaching }
            })
            .collect()
    }

    /// The definitions of `variable` that `state` holds, never none.
    fn definitions_of(
        &'r self,
        state: &State,
        variable: &PlPgSqlVariable<'db>,
    ) -> Vec<Definition<'r, 'db, DB>> {
        let Some(slots) = state.get(&variable.id()) else {
            return Vec::from([self.definition(Slot::Entry, variable)]);
        };
        slots.iter().map(|slot| self.definition(*slot, variable)).collect()
    }

    /// The definition `slot` of `variable` names.
    fn definition(
        &'r self,
        slot: Slot,
        variable: &PlPgSqlVariable<'db>,
    ) -> Definition<'r, 'db, DB> {
        let kind = match slot {
            Slot::Written(index) => {
                DefinitionKind::Written { index, written: &self.definitions[index] }
            }
            // The handle owns the variable, which the flow holds no entry for.
            Slot::Entry => {
                DefinitionKind::Entry {
                    variable: variable.clone(),
                    source: entry_source(variable.kind()),
                }
            }
        };
        Definition { flow: self, kind }
    }
}

/// Where the value a variable holds before the body writes it comes from.
fn entry_source(kind: &VariableKind) -> &'static DefinitionSource<'static, 'static> {
    match kind {
        VariableKind::Parameter { mode: ArgMode::Out, .. } | VariableKind::ReturnValue => {
            &DefinitionSource::Null
        }
        VariableKind::Parameter { .. } | VariableKind::Trigger | VariableKind::Handler => {
            &DefinitionSource::Incoming
        }
        // A declared variable, cursor, loop index and loop record are
        // written where they are declared, before anything reads them.
        VariableKind::Found
        | VariableKind::Declared { .. }
        | VariableKind::Cursor
        | VariableKind::CursorParameter
        | VariableKind::LoopIndex
        | VariableKind::LoopRecord => &DefinitionSource::Unknown,
    }
}

/// Adds every variable `node` reads under `context`, whole, through a field
/// or by `$n` in `scope`, to `variables` unless already there.
///
/// # Errors
///
/// Returns the lookup errors resolving a name `node` writes.
pub(crate) fn read_variables<'db, DB: DatabaseLike, N: Visit + ?Sized>(
    context: &ExpressionContext<'_, 'db, DB>,
    scope: &PlPgSqlScope<'_, 'db, DB>,
    node: &N,
    variables: &mut Vec<PlPgSqlVariable<'db>>,
) -> Result<(), LookupError> {
    let found = walk_expressions(node, |node| {
        let read = match node {
            Expr::Value(value) => {
                match &value.value {
                    Value::Placeholder(placeholder) => {
                        scope
                            .variable(core::slice::from_ref(placeholder))
                            .map(|(variable, _)| variable)
                    }
                    _ => None,
                }
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                match context.reference(node) {
                    Ok(Some((
                        ColumnDefinition::Variable(variable)
                        | ColumnDefinition::VariableField(variable),
                        _,
                    ))) => Some(variable),
                    Ok(_) => None,
                    Err(error) => return ControlFlow::Break(error),
                }
            }
            _ => None,
        };
        if let Some(variable) = read
            && variables.iter().all(|known| known.id() != variable.id())
        {
            variables.push(variable);
        }
        ControlFlow::Continue(())
    });
    match found {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

/// A variable some SQL of a body reads, with the definitions that may reach
/// that read.
#[derive(Debug)]
pub struct VariableRead<'r, 'db, DB: DatabaseLike> {
    variable: PlPgSqlVariable<'db>,
    reaching: Reaching<'r, 'db, DB>,
}

impl<'r, 'db, DB: DatabaseLike> VariableRead<'r, 'db, DB> {
    /// Returns the variable read, the one every name, alias and label
    /// reaching it shares, whether the SQL reads it whole or a field of it.
    #[must_use]
    pub const fn variable(&self) -> &PlPgSqlVariable<'db> {
        &self.variable
    }

    /// Returns the definitions that may reach the read.
    #[must_use]
    pub const fn reaching(&self) -> &Reaching<'r, 'db, DB> {
        &self.reaching
    }
}

/// The definitions that may reach a read.
#[derive(Debug)]
pub enum Reaching<'r, 'db, DB: DatabaseLike> {
    /// Each definition some path to the read may leave in the variable,
    /// never none, in body order of the writes after the entry definition.
    /// A field or element write is listed beside the definitions of the
    /// whole variable it changes part of.
    Definitions(Vec<Definition<'r, 'db, DB>>),
    /// No path reaches the read, which sits after a `RETURN`, an `EXIT`, a
    /// `CONTINUE` or an error that always leaves.
    Unreachable,
}

/// Tells apart the definitions of one body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionId(DefinitionIdKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum DefinitionIdKind {
    /// The entry definition of the variable of this id.
    Entry(usize),
    /// The written definition of this index.
    Written(usize),
}

/// A value the body may give a variable, where it gives it.
///
/// A variable's entry definition stands for the value it holds before the
/// body writes it, as [`DefinitionSource::Incoming`],
/// [`DefinitionSource::Null`] or [`DefinitionSource::Unknown`].
#[derive(Debug, Clone)]
pub struct Definition<'r, 'db, DB: DatabaseLike> {
    flow: &'r ValueFlow<'r, 'db, DB>,
    kind: DefinitionKind<'r, 'db, DB>,
}

#[derive(Debug, Clone)]
enum DefinitionKind<'r, 'db, DB: DatabaseLike> {
    Entry { variable: PlPgSqlVariable<'db>, source: &'static DefinitionSource<'static, 'static> },
    Written { index: usize, written: &'r Written<'r, 'db, DB> },
}

impl<'r, 'db, DB: DatabaseLike> Definition<'r, 'db, DB> {
    /// Returns what tells the definition apart from every other one of the
    /// body, so a walk over definitions that read each other can stop at one
    /// it has seen.
    #[must_use]
    pub fn id(&self) -> DefinitionId {
        DefinitionId(match &self.kind {
            DefinitionKind::Written { index, .. } => DefinitionIdKind::Written(*index),
            DefinitionKind::Entry { variable, .. } => DefinitionIdKind::Entry(variable.id()),
        })
    }

    /// Returns the variable the definition assigns.
    #[must_use]
    pub const fn variable(&self) -> &PlPgSqlVariable<'db> {
        match &self.kind {
            DefinitionKind::Entry { variable, .. } => variable,
            DefinitionKind::Written { written, .. } => &written.variable,
        }
    }

    /// Returns the steps from the variable to the field, element or slice
    /// the definition assigns, none when it assigns the whole variable.
    ///
    /// A definition of a part leaves the rest of the variable as the
    /// definitions before it left it, so those still reach the reads after
    /// it.
    #[must_use]
    pub fn part(&self) -> &[TargetStep<'r>] {
        match &self.kind {
            DefinitionKind::Entry { .. } => &[],
            DefinitionKind::Written { written, .. } => &written.part,
        }
    }

    /// Returns where the value the definition assigns comes from.
    #[must_use]
    pub const fn source(&self) -> &DefinitionSource<'r, 'db> {
        match &self.kind {
            DefinitionKind::Entry { source, .. } => source,
            DefinitionKind::Written { written, .. } => &written.source,
        }
    }

    /// Returns the names and path the SQL of [`Self::source`] and of the
    /// subscripts of [`Self::part`] is evaluated under, as the scope to
    /// build their [`ExpressionContext`] under, or `None` for a source
    /// without SQL.
    ///
    /// The scope is the one where the body writes the definition, even where
    /// the block declaring it has ended before a read the definition reaches.
    #[must_use]
    pub fn scope(&self) -> Option<ExpressionScope<'_, 'db, DB>> {
        match &self.kind {
            DefinitionKind::Entry { .. } => None,
            DefinitionKind::Written { written, .. } => {
                written.scope.as_ref().map(ExpressionScope::PlPgSql)
            }
        }
    }

    /// Returns the variables the SQL of [`Self::source`] reads, each once in
    /// the order first read, with the definitions that may reach the point
    /// the body evaluates it. Identifiers compare under `case`.
    ///
    /// A source without SQL reads nothing. A `FETCH` reads the query its
    /// cursor was opened with, which the definition does not hold, and a
    /// `CALL` reads the arguments it passes.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors building the contexts of the
    /// source's SQL runs into.
    pub fn reads(
        &self,
        case: IdentifierCase,
    ) -> Result<Vec<VariableRead<'r, 'db, DB>>, LookupError> {
        let DefinitionKind::Written { written, .. } = &self.kind else { return Ok(Vec::new()) };
        let Some(scope) = &written.scope else { return Ok(Vec::new()) };
        let mut roots = Vec::new();
        match &written.source {
            DefinitionSource::Value(value)
            | DefinitionSource::ArrayElement { array: value, .. } => {
                roots.push(ScopedRoot::Expression(value));
            }
            DefinitionSource::QueryOutput { statement, .. }
            | DefinitionSource::CallOutput { call: statement, .. } => {
                roots.push(ScopedRoot::Statement(statement));
            }
            DefinitionSource::DynamicOutput { command, arguments, .. } => {
                roots.push(ScopedRoot::Expression(command));
                roots.extend(arguments.iter().map(|argument| ScopedRoot::Expression(argument)));
            }
            DefinitionSource::LoopIndex { lower, upper, step } => {
                let bounds = [lower, upper].into_iter().chain(step);
                roots.extend(bounds.map(|bound| ScopedRoot::Expression(bound)));
            }
            DefinitionSource::Incoming
            | DefinitionSource::Null
            | DefinitionSource::CursorOutput { .. }
            | DefinitionSource::CursorName
            | DefinitionSource::PortalName
            | DefinitionSource::Diagnostics(_)
            | DefinitionSource::Unknown => {}
        }
        let database = scope.database();
        let plpgsql = ExpressionScope::PlPgSql(scope);
        let mut variables = Vec::new();
        for root in roots {
            let context = ExpressionContext::for_root(root, &plpgsql, database, case)?;
            match root {
                ScopedRoot::Expression(expression) => {
                    read_variables(&context, scope, expression, &mut variables)?;
                }
                ScopedRoot::Query(query) => {
                    read_variables(&context, scope, query, &mut variables)?;
                }
                ScopedRoot::Statement(statement) => {
                    read_variables(&context, scope, statement, &mut variables)?;
                }
            }
        }
        Ok(self.flow.reaching(variables, written.inputs.as_ref()))
    }
}

impl<DB: DatabaseLike> PartialEq for Definition<'_, '_, DB> {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl<DB: DatabaseLike> Eq for Definition<'_, '_, DB> {}
