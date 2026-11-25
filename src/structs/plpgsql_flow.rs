//! The values a PL/pgSQL body may give its variables, and which of them may
//! reach each place the body reads one.

use alloc::{borrow::Cow, boxed::Box, collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{fmt, ops::ControlFlow, sync::atomic::AtomicBool};

use once_cell::race::OnceBox;
use sqlparser::ast::{ArgMode, Expr, Statement, Value, Visit};
use sqlparser_plpgsql::namespace::Sql;
use sqlparser_plpgsql_dataflow::{
    Flow,
    reaching::{Slot, State},
};
pub use sqlparser_plpgsql_dataflow::{Found, Output, Rows};

use crate::{
    errors::LookupError,
    impls::dql::scoped_roots::ScopedRoot,
    structs::{
        ColumnDefinition, ExpressionContext, ExpressionScope, IdentifierCase, OperandType,
        PlPgSqlScope, PlPgSqlVariable, VariableKind, row_of,
    },
    traits::DatabaseLike,
    utils::{object_name::PG_CATALOG, walk::walk_expressions},
};

/// Whether `variable` is declared `record` or is a loop's record, whose
/// fields the row it holds decides, where a composite type converts the row
/// to its own fields.
pub(crate) fn is_record(variable: &PlPgSqlVariable<'_>) -> bool {
    matches!(
        variable.data_type(),
        Ok(OperandType::Known { type_name, .. })
            if type_name.name() == "record" && type_name.schema() == Some(PG_CATALOG)
    )
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
    /// The null a declaration without a default, an output argument and `$0`
    /// hold.
    Null,
    /// The `false` PostgreSQL gives `FOUND` as each call starts.
    False,
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
    /// Every integer of a `FOR` loop's range in turn, from the first bound
    /// written to the second.
    LoopIndex {
        /// Whether `REVERSE` counts down from `lower` to `upper`.
        reverse: bool,
        /// The first bound written, where the count starts.
        lower: Cow<'r, Expr>,
        /// The second bound written, past which the count stops.
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
    /// The value a procedure's `INOUT` or `OUT` parameter returns to the
    /// variable `CALL` passes there. A call reaching one procedure replaces
    /// what the variables at those parameters held, and any other call gives
    /// every variable it passes one beside its earlier definitions.
    CallOutput {
        /// The `CALL`.
        call: &'r Statement,
        /// The argument's position among those written, from 0.
        argument: usize,
    },
    /// Whether the step found a row, which a statement, `FETCH`, `MOVE`,
    /// `PERFORM`, `RETURN QUERY` and a `FOR` or `FOREACH` loop give `FOUND`.
    Found(Found),
    /// A value nothing in the body tells, such as a cursor parameter.
    Unknown,
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
    /// The definitions reaching the point where the source is evaluated.
    pub(crate) inputs: State,
}

/// Every definition of a body and the definitions reaching each statement,
/// expression and dynamic SQL of it.
#[derive(Debug)]
pub(crate) struct ValueFlow<'r, 'db, DB: DatabaseLike> {
    /// The written definitions, by their index among the walk's, which
    /// [`Slot::Written`] names.
    pub(crate) definitions: Vec<Written<'r, 'db, DB>>,
    /// The definitions reaching each statement, expression and dynamic SQL
    /// some path reaches, where the body evaluates it.
    pub(crate) flow: Flow<'r, State>,
    /// The query each cursor declared with one opens, by the cursor
    /// variable's id.
    pub(crate) cursors: BTreeMap<usize, CursorQuery<'r, 'db, DB>>,
    /// The rows each row source gives a record, by identifier case, solved
    /// on the first record field typed under that case.
    pub(crate) rows: [OnceBox<RowTypes>; 3],
}

/// Where a record's row comes from, as [`RowTypes`] keys it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RowSource {
    /// The query of the written definition at this index.
    Written(usize),
    /// The query of the cursor variable of this id.
    Cursor(usize),
}

/// The outputs of a row source's query up to the first one without a
/// modeled name, each by stored name with its type, `None` while no round
/// has typed it.
pub(crate) type Row = Vec<(String, Option<OperandType<'static>>)>;

/// The row of every row source of a body.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RowTypes {
    rows: BTreeMap<RowSource, Result<Row, LookupError>>,
}

impl RowTypes {
    /// The type the row of `source` gives the field stored as `field`,
    /// `None` while untyped and unknown for a field the row does not hold.
    ///
    /// # Errors
    ///
    /// Returns the lookup error typing the source's query ran into.
    pub(crate) fn field(
        &self,
        source: RowSource,
        field: &str,
    ) -> Result<Option<OperandType<'static>>, LookupError> {
        match self.rows.get(&source) {
            None => Ok(None),
            Some(Err(error)) => Err(error.clone()),
            Some(Ok(row)) => {
                Ok(row
                    .iter()
                    .find(|(name, _)| name == field)
                    .map_or(Some(OperandType::Unknown), |(_, data_type)| data_type.clone()))
            }
        }
    }
}

/// The join of two field types, `None` below every type and unknown above
/// them.
pub(crate) fn join<'t>(
    left: Option<OperandType<'t>>,
    right: Option<OperandType<'t>>,
) -> Option<OperandType<'t>> {
    match (left, right) {
        (None, data_type) | (data_type, None) => data_type,
        (Some(left), Some(right)) => Some(if left == right { left } else { OperandType::Unknown }),
    }
}

/// One round of solving [`RowTypes`]: the rows the previous round left, and
/// whether typing the current output read a field they leave untyped.
#[derive(Debug)]
pub(crate) struct Round<'r> {
    /// The rows of the previous round.
    pub(crate) rows: &'r RowTypes,
    /// Whether a field read since the last reset joined to `None`.
    pub(crate) untyped: AtomicBool,
}

/// The slot of a value kept per identifier case, such as
/// [`ValueFlow::rows`], for `case`.
pub(crate) const fn case_slot(case: IdentifierCase) -> usize {
    match case {
        IdentifierCase::AsWritten => 0,
        IdentifierCase::Folded => 1,
        IdentifierCase::Exact => 2,
    }
}

/// The query a cursor declared with one opens.
#[derive(Debug)]
pub(crate) struct CursorQuery<'r, 'db, DB: DatabaseLike> {
    /// The query.
    pub(crate) statement: &'r Statement,
    /// The names and path the query is evaluated under.
    pub(crate) scope: PlPgSqlScope<'r, 'db, DB>,
}

/// Gives the value flow of a body to the scopes of its sites. The flow comes
/// back under the borrow of the source alone, which keeps a scope holding
/// the source as covariant as the database it borrows.
pub(crate) trait FlowSource<DB: DatabaseLike>: fmt::Debug {
    /// The value flow, computed on the first call.
    fn flow(&self) -> Result<&ValueFlow<'_, '_, DB>, LookupError>;
}

/// The point of a body's value flow a [`PlPgSqlScope`] stands at, which
/// types the fields of a record from the definitions reaching it.
#[derive(Debug, Clone)]
pub(crate) enum FlowPoint<'r, 'db, DB: DatabaseLike> {
    /// Where a site's statement, expression or dynamic SQL is evaluated.
    Site {
        /// The flow, computed on first use.
        flow: Arc<dyn FlowSource<DB> + 'r>,
        /// The statement, expression or dynamic SQL.
        sql: Sql<'r>,
    },
    /// Where the definition at `index` evaluates its source.
    Definition {
        /// The flow.
        flow: &'r ValueFlow<'r, 'db, DB>,
        /// The definition's index.
        index: usize,
        /// The round solving the rows, `None` once they are solved.
        round: Option<&'r Round<'r>>,
    },
    /// Where the query of a cursor is evaluated, which every opening of the
    /// cursor reaches.
    Cursor {
        /// The flow.
        flow: &'r ValueFlow<'r, 'db, DB>,
        /// The query.
        statement: &'r Statement,
        /// The round solving the rows.
        round: &'r Round<'r>,
    },
}

/// What reaches a [`FlowPoint`].
pub(crate) struct Reached<'f, DB: DatabaseLike> {
    /// The flow of the body.
    pub(crate) flow: &'f ValueFlow<'f, 'f, DB>,
    /// The definitions reaching the point, `None` where no path reaches it.
    pub(crate) state: Option<&'f State>,
    /// The round solving the rows, `None` once they are solved.
    pub(crate) round: Option<&'f Round<'f>>,
}

impl<DB: DatabaseLike> FlowPoint<'_, '_, DB> {
    /// What reaches the point.
    pub(crate) fn reaching(&self) -> Result<Reached<'_, DB>, LookupError> {
        Ok(match self {
            Self::Site { flow, sql } => {
                let flow = flow.flow()?;
                Reached { flow, state: flow.flow.at(*sql), round: None }
            }
            Self::Definition { flow, index, round } => {
                Reached { flow, state: Some(&flow.definitions[*index].inputs), round: *round }
            }
            Self::Cursor { flow, statement, round } => {
                Reached { flow, state: flow.flow.at(Sql::Statement(statement)), round: Some(round) }
            }
        })
    }
}

impl<DB: DatabaseLike> ValueFlow<'_, '_, DB> {
    /// The rows every row source gives a record under `case`, solved on the
    /// first call.
    pub(crate) fn row_types(&self, case: IdentifierCase) -> &RowTypes {
        self.rows[case_slot(case)].get_or_init(|| Box::new(self.solve_rows(case)))
    }

    /// The row source the written definition at `index` gives a record its
    /// row from: a query it assigns the whole of a record from, or the
    /// declared query of a cursor it fetches from while the cursor variable
    /// holds its own name on every path to it. `None` for any other
    /// definition.
    pub(crate) fn row_source(&self, index: usize) -> Option<RowSource> {
        let written = &self.definitions[index];
        if !written.part.is_empty() || !is_record(&written.variable) {
            return None;
        }
        match &written.source {
            DefinitionSource::QueryOutput {
                statement: Statement::Query(_),
                output: Output::Row,
                ..
            } if written.scope.is_some() => Some(RowSource::Written(index)),
            DefinitionSource::CursorOutput { cursor, output: Output::Row } => {
                let inputs = &written.inputs;
                let own = inputs.get(&cursor.id()).is_some_and(|slots| {
                    slots.iter().all(|slot| {
                        matches!(slot, Slot::Written(name) if matches!(
                            self.definitions[*name].source,
                            DefinitionSource::CursorName | DefinitionSource::PortalName
                        ))
                    })
                });
                (own && self.cursors.contains_key(&cursor.id()))
                    .then_some(RowSource::Cursor(cursor.id()))
            }
            _ => None,
        }
    }

    /// The least fixed point of the rows, each round typing every source's
    /// query from the rows the previous one left and joining the result with
    /// them, until no row changes.
    fn solve_rows(&self, case: IdentifierCase) -> RowTypes {
        let mut sources = Vec::new();
        for (index, written) in self.definitions.iter().enumerate() {
            if let Some(RowSource::Written(_)) = self.row_source(index)
                && let (Some(scope), DefinitionSource::QueryOutput { statement, .. }) =
                    (&written.scope, &written.source)
            {
                sources.push((RowSource::Written(index), *statement, scope));
            }
        }
        for (id, cursor) in &self.cursors {
            sources.push((RowSource::Cursor(*id), cursor.statement, &cursor.scope));
        }
        let mut rows = RowTypes::default();
        loop {
            let round = Round { rows: &rows, untyped: AtomicBool::new(false) };
            let mut next = BTreeMap::new();
            for (source, statement, scope) in &sources {
                let point = match source {
                    RowSource::Written(index) => {
                        FlowPoint::Definition { flow: self, index: *index, round: Some(&round) }
                    }
                    RowSource::Cursor(_) => {
                        FlowPoint::Cursor { flow: self, statement, round: &round }
                    }
                };
                let typed = row_of(statement, &(*scope).clone().at(point), case, &round);
                let joined = match (rows.rows.get(source), typed) {
                    (Some(Ok(earlier)), Ok(typed)) => {
                        Ok(earlier
                            .iter()
                            .zip(typed)
                            .map(|((name, earlier), (_, typed))| {
                                (name.clone(), join(earlier.clone(), typed))
                            })
                            .collect())
                    }
                    (_, typed) => typed,
                };
                next.insert(*source, joined);
            }
            let next = RowTypes { rows: next };
            if next == rows {
                return rows;
            }
            rows = next;
        }
    }
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
    pub(crate) fn definitions_of(
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
        VariableKind::Found => &DefinitionSource::False,
        // A declared variable, cursor, loop index and loop record are
        // written where they are declared, before anything reads them.
        VariableKind::Declared { .. }
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
                            .lookup(core::slice::from_ref(placeholder))
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
/// [`DefinitionSource::Null`], [`DefinitionSource::False`] or
/// [`DefinitionSource::Unknown`].
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
    /// build their [`ExpressionContext`] under through
    /// [`ExpressionScope::PlPgSql`], or `None` for a source without SQL.
    ///
    /// The scope is the one where the body writes the definition, even where
    /// the block declaring it has ended before a read the definition reaches.
    /// It stands where the body evaluates the source, so a field of a record
    /// read there takes its type from the definitions reaching that point.
    #[must_use]
    pub fn scope(&self) -> Option<PlPgSqlScope<'r, 'db, DB>> {
        let DefinitionKind::Written { index, written } = &self.kind else { return None };
        let point = FlowPoint::Definition { flow: self.flow, index: *index, round: None };
        Some(written.scope.as_ref()?.clone().at(point))
    }

    /// The row source this definition gives a record its row from, `None`
    /// for any definition [`ValueFlow::row_source`] gives none.
    pub(crate) fn row_source(&self) -> Option<RowSource> {
        let DefinitionKind::Written { index, .. } = &self.kind else { return None };
        self.flow.row_source(*index)
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
        let mut statement = None;
        let mut expressions: Vec<&Expr> = Vec::new();
        match &written.source {
            DefinitionSource::Value(value)
            | DefinitionSource::ArrayElement { array: value, .. } => expressions.push(value),
            DefinitionSource::QueryOutput { statement: query, .. }
            | DefinitionSource::CallOutput { call: query, .. } => statement = Some(*query),
            DefinitionSource::DynamicOutput { command, arguments, .. } => {
                expressions.push(command);
                expressions.extend(arguments.iter().map(|argument| &**argument));
            }
            DefinitionSource::LoopIndex { lower, upper, step, .. } => {
                expressions.extend([lower, upper].into_iter().chain(step).map(|bound| &**bound));
            }
            DefinitionSource::Incoming
            | DefinitionSource::Null
            | DefinitionSource::False
            | DefinitionSource::CursorOutput { .. }
            | DefinitionSource::CursorName
            | DefinitionSource::PortalName
            | DefinitionSource::Diagnostics(_)
            | DefinitionSource::Found(_)
            | DefinitionSource::Unknown => return Ok(Vec::new()),
        }
        let Some(scope) = &written.scope else { return Ok(Vec::new()) };
        let database = scope.database();
        let plpgsql = ExpressionScope::PlPgSql(scope);
        let mut variables = Vec::new();
        if let Some(statement) = statement {
            let root = ScopedRoot::Statement(statement);
            let context = ExpressionContext::for_root(root, &plpgsql, database, case)?;
            read_variables(&context, scope, statement, &mut variables)?;
        }
        for expression in expressions {
            let root = ScopedRoot::Expression(expression);
            let context = ExpressionContext::for_root(root, &plpgsql, database, case)?;
            read_variables(&context, scope, expression, &mut variables)?;
        }
        Ok(self.flow.reaching(variables, Some(&written.inputs)))
    }
}

impl<DB: DatabaseLike> PartialEq for Definition<'_, '_, DB> {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl<DB: DatabaseLike> Eq for Definition<'_, '_, DB> {}
