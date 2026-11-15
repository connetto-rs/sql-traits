//! What a table rebuild carries of the operators each derived expression
//! selected, by the object owning the expression.
//!
//! PostgreSQL records the operator an application selects when the
//! expression is stored, so an expression a rebuild writes again keeps what
//! it selected, through renames and whatever operator was created since. An
//! expression the statement writes anew selects again, and `ALTER COLUMN
//! ... TYPE` has PostgreSQL parse the column's dependents again from their
//! deparsed text.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateFunction, DataType, Expr, Ident, ObjectName, ObjectNamePart,
    Visit, VisitMut, VisitorMut,
};

use super::function_dependents::call_count;
use crate::{
    structs::{
        ColumnIdentity, IdentifierCase,
        metadata::{
            CarriedNodes, ExpressionCalls, NodeState, OperatorNode, WrittenPins,
            operator_node_count,
        },
    },
    utils::walk::{walk_expressions, walk_mut},
};

/// The object owning a derived expression across a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    /// The column of this identity: its default, its generation expression
    /// and its inline checks.
    Column(ColumnIdentity),
    /// The table itself: its own checks, its exclusions and its unique keys.
    Table,
}

/// Which derived expression an entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A column's default, which a statement may write anew in place.
    Default,
    /// A column's generation expression, likewise.
    Generation,
    /// Any other column option, written anew only by adding it.
    ColumnOption,
    /// A check's record, inline or the table's own.
    Check,
    /// An exclusion constraint's record.
    Exclusion,
    /// A unique constraint's record, whose key may hold expressions.
    UniqueKey,
    /// The partition key's record.
    PartitionKey,
}

/// The operator nodes of one derived expression, with the columns retyped
/// under those rewritten and the functions its calls reached.
pub(super) type Selections = CarriedNodes<CreateFunction>;

/// A table's columns, by name, with their identities.
type Columns = Vec<(Ident, ColumnIdentity)>;

/// One derived expression's selections, waiting for the expression the
/// rebuild writes again.
struct Entry {
    owner: Owner,
    kind: Kind,
    /// The constraint's name, when it is a named constraint.
    name: Option<String>,
    /// The expression's spelling, each column it reads by identity and
    /// every other name blanked.
    shape: String,
    selections: Selections,
}

/// The selections a rebuild carries, and what the rebuilding statement
/// writes anew.
pub(super) struct CarriedOperators {
    entries: Vec<Entry>,
    /// The columns of the table as it was, which the recorded expressions
    /// read.
    recorded_columns: Columns,
    /// The columns of the table as the rebuild writes it, which the rebuilt
    /// expressions read.
    rebuilt_columns: Columns,
    /// The column whose inline checks are being built, `None` for the
    /// table's own constraints.
    building: Option<ColumnIdentity>,
    /// The name each check of the rebuilt table answers to, in the order
    /// they are built, and how many were built.
    check_names: (Vec<Option<String>>, usize),
    /// The column the statement retypes, with the type it had.
    retyped: Option<(Ident, DataType)>,
    /// The column whose default or generation expression the statement
    /// writes anew.
    written: Option<(Ident, Kind)>,
    case: IdentifierCase,
}

impl Default for CarriedOperators {
    /// Nothing carried, which a statement creating a table rebuilds from.
    fn default() -> Self {
        Self::recording(Vec::new(), IdentifierCase::AsWritten)
    }
}

impl CarriedOperators {
    /// Starts recording the derived expressions of a table whose columns
    /// are `columns`, under `case`.
    pub(super) const fn recording(columns: Columns, case: IdentifierCase) -> Self {
        Self {
            entries: Vec::new(),
            recorded_columns: columns,
            rebuilt_columns: Vec::new(),
            building: None,
            check_names: (Vec::new(), 0),
            retyped: None,
            written: None,
            case,
        }
    }

    /// Has the rebuilt expressions read the columns `columns`, the checks
    /// built next answering to `check_names` in turn.
    pub(super) fn rebuild(&mut self, columns: Columns, check_names: Vec<Option<String>>) {
        self.rebuilt_columns = columns;
        self.check_names = (check_names, 0);
    }

    /// Records the selections of `calls`, the record of the column `column`
    /// of identity `identity`, option by option.
    pub(super) fn record_column(
        &mut self,
        identity: ColumnIdentity,
        column: &ColumnDef,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        // A column's record is never parsed again, so it holds no retyped
        // column and needs no earlier calls.
        let mut nodes = calls.take_operator_nodes().nodes;
        for option in &column.options {
            let count = operator_node_count(&option.option).min(nodes.len());
            let rest = nodes.split_off(count);
            let shape = self.shape(&option.option, &self.recorded_columns);
            self.entries.push(Entry {
                owner: Owner::Column(identity),
                kind: option_kind(&option.option),
                name: option.name.as_ref().map(|name| name.value.clone()),
                shape,
                selections: CarriedNodes {
                    nodes: core::mem::replace(&mut nodes, rest),
                    retyped: Vec::new(),
                    calls: Vec::new(),
                    written: WrittenPins::default(),
                },
            });
        }
    }

    /// Records the selections of `calls`, the record of `node`, a `kind`
    /// expression of `owner` answering to `name`.
    pub(super) fn record<N: VisitMut + Clone + ToString>(
        &mut self,
        owner: Owner,
        kind: Kind,
        name: Option<String>,
        node: &N,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        let shape = self.shape(node, &self.recorded_columns);
        let selections = calls.take_operator_nodes();
        self.entries.push(Entry { owner, kind, name, shape, selections });
    }

    /// Records the selections of `calls`, the partition key's record.
    pub(super) fn record_partition_key(&mut self, calls: &mut ExpressionCalls<CreateFunction>) {
        self.entries.push(Entry {
            owner: Owner::Table,
            kind: Kind::PartitionKey,
            name: None,
            shape: String::new(),
            selections: calls.take_operator_nodes(),
        });
    }

    /// The column whose checks are recorded or built next, by name in the
    /// table as it was.
    pub(super) fn recorded_identity(&self, column: &Ident) -> Option<ColumnIdentity> {
        identity_of(&self.recorded_columns, column, self.case)
    }

    /// Has the inline checks built next belong to the column of `identity`,
    /// or to the table itself for `None`.
    pub(super) const fn build_column(&mut self, identity: Option<ColumnIdentity>) {
        self.building = identity;
    }

    /// Has the statement retype the column `column`, which was of type
    /// `previous`.
    pub(super) fn retype(&mut self, column: Ident, previous: DataType) {
        self.retyped = Some((column, previous));
    }

    /// Has the statement write anew the column `column`'s `option`, a
    /// default or a generation expression.
    pub(super) fn write(&mut self, column: Ident, option: &ColumnOption) {
        let kind = option_kind(option);
        if matches!(kind, Kind::Default | Kind::Generation) {
            self.written = Some((column, kind));
        }
    }

    /// Gives `calls`, the record of the column `column` of identity
    /// `identity` the rebuild writes again, what each of its options
    /// selected, an option the statement writes anew or added left waiting.
    pub(super) fn take_column(
        &mut self,
        identity: Option<ColumnIdentity>,
        column: &ColumnDef,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        let Some(identity) = identity else {
            calls.set_call_states(0..usize::MAX, NodeState::Pending);
            return;
        };
        let owner = Owner::Column(identity);
        let mut nodes = Vec::new();
        let mut first_call = 0;
        for option in &column.options {
            let kind = option_kind(&option.option);
            let written = self.written.as_ref().is_some_and(|(written, slot)| {
                *slot == kind && same_identifier(written, &column.name, self.case)
            });
            let taken = match kind {
                _ if written => None,
                Kind::Default | Kind::Generation => {
                    self.take_entry(owner, kind, None, |entry| entry.kind == kind)
                }
                _ => {
                    let shape = self.shape(&option.option, &self.rebuilt_columns);
                    let name = option.name.as_ref().map(|name| name.value.as_str());
                    self.take_entry(owner, kind, name, |entry| entry.shape == shape)
                }
            };
            let count = operator_node_count(&option.option);
            let carried = match taken {
                Some(taken) if taken.nodes.len() == count => {
                    nodes.extend(taken.nodes);
                    NodeState::Settled
                }
                _ => {
                    nodes.extend(core::iter::repeat_n(OperatorNode::PENDING, count));
                    NodeState::Pending
                }
            };
            let last_call = first_call + call_count(&option.option);
            calls.set_call_states(first_call..last_call, carried);
            first_call = last_call;
        }
        calls.carry_operator_nodes(CarriedNodes {
            nodes,
            retyped: Vec::new(),
            calls: Vec::new(),
            written: WrittenPins::default(),
        });
    }

    /// Gives `calls`, the record of the check over `node` the rebuild builds
    /// next, what the check it continues selected, of the column whose
    /// checks are being built or else of the table.
    pub(super) fn take_check(&mut self, node: &Expr, calls: &mut ExpressionCalls<CreateFunction>) {
        let owner = self.building.map_or(Owner::Table, Owner::Column);
        let (names, built) = &mut self.check_names;
        let name = names.get_mut(*built).and_then(Option::take);
        *built += 1;
        self.take(owner, Kind::Check, name.as_deref(), node, calls);
    }

    /// Gives `calls`, the record of `node`, a `kind` expression of `owner`
    /// answering to `name` the rebuild writes again, what the expression it
    /// continues selected, rewritten when it reads the column the statement
    /// retypes.
    pub(super) fn take<N: Visit + VisitMut + Clone + ToString>(
        &mut self,
        owner: Owner,
        kind: Kind,
        name: Option<&str>,
        node: &N,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        let shape = self.shape(node, &self.rebuilt_columns);
        let carried = self.take_entry(owner, kind, name, |entry| entry.shape == shape);
        let state = if carried.is_some() { NodeState::Settled } else { NodeState::Pending };
        calls.set_call_states(0..usize::MAX, state);
        if let Some(carried) = carried {
            calls.carry_operator_nodes(carried);
        }
        self.rewrite_reading(node, calls);
    }

    /// Gives `calls`, the partition key's record, what the key selected.
    /// PostgreSQL refuses to retype a column the key reads, so the key is
    /// never parsed again.
    pub(super) fn take_partition_key(&mut self, calls: &mut ExpressionCalls<CreateFunction>) {
        let carried = self.take_entry(Owner::Table, Kind::PartitionKey, None, |_| true);
        let state = if carried.is_some() { NodeState::Settled } else { NodeState::Pending };
        calls.set_call_states(0..usize::MAX, state);
        if let Some(carried) = carried {
            calls.carry_operator_nodes(carried);
        }
        calls.keep_previous_calls_for_rewritten();
    }

    /// Gives `calls`, the record of an index's `node` the rebuild writes
    /// again, the `selections` the index held.
    pub(super) fn take_index<N: Visit + ?Sized>(
        &self,
        node: &N,
        carried: Selections,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        calls.carry_operator_nodes(carried);
        calls.set_call_states(0..usize::MAX, NodeState::Settled);
        self.rewrite_reading(node, calls);
    }

    /// Has `calls`, the record of `node`, parsed again when `node` reads
    /// the column the statement retypes.
    fn rewrite_reading<N: Visit + ?Sized>(
        &self,
        node: &N,
        calls: &mut ExpressionCalls<CreateFunction>,
    ) {
        if let Some((column, previous)) = &self.retyped
            && reads_column(node, column, self.case)
        {
            calls.rewrite_operators(column, previous);
        }
        calls.keep_previous_calls_for_rewritten();
    }

    /// Takes the selections of the entry of `owner` and `kind` that
    /// `matches` keeps, one answering to `name` first, and else the first
    /// one recorded. The others keep their order.
    fn take_entry(
        &mut self,
        owner: Owner,
        kind: Kind,
        name: Option<&str>,
        matches: impl Fn(&Entry) -> bool,
    ) -> Option<Selections> {
        let candidate =
            |entry: &Entry| entry.owner == owner && entry.kind == kind && matches(entry);
        let named = name.and_then(|name| {
            self.entries
                .iter()
                .position(|entry| candidate(entry) && entry.name.as_deref() == Some(name))
        });
        let position = named.or_else(|| self.entries.iter().position(candidate))?;
        Some(self.entries.remove(position).selections)
    }

    /// The spelling of `node` with each column of `columns` it reads written
    /// as the column's identity and every other name blanked, which a rename
    /// leaves as it was.
    fn shape<N: VisitMut + Clone + ToString>(&self, node: &N, columns: &Columns) -> String {
        let mut blanked = node.clone();
        let ControlFlow::<Infallible>::Continue(()) =
            walk_mut(&mut blanked, &mut Blank { columns, case: self.case });
        blanked.to_string()
    }
}

/// The identity of the column `column` names among `columns`.
fn identity_of(columns: &Columns, column: &Ident, case: IdentifierCase) -> Option<ColumnIdentity> {
    columns
        .iter()
        .find(|(name, _)| same_identifier(name, column, case))
        .map(|(_, identity)| *identity)
}

/// The kind of entry a column option's expression makes.
const fn option_kind(option: &ColumnOption) -> Kind {
    match option {
        ColumnOption::Default(_) => Kind::Default,
        ColumnOption::Generated { generation_expr: Some(_), .. } => Kind::Generation,
        _ => Kind::ColumnOption,
    }
}

/// Whether two column names name the same column under `case`.
fn same_identifier(left: &Ident, right: &Ident, case: IdentifierCase) -> bool {
    case.identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// Whether `node` references the column `column` by name.
fn reads_column<N: Visit + ?Sized>(node: &N, column: &Ident, case: IdentifierCase) -> bool {
    let found = walk_expressions(node, |expression| {
        let named = match expression {
            Expr::Identifier(ident) => Some(ident),
            Expr::CompoundIdentifier(parts) => parts.last(),
            _ => None,
        };
        if named.is_some_and(|named| same_identifier(named, column, case)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    found.is_break()
}

/// Writes each column reference as the column's identity, however qualified,
/// and every other name as one blank part, which a rename or a move leaves
/// as it was.
struct Blank<'c> {
    columns: &'c Columns,
    case: IdentifierCase,
}

impl Blank<'_> {
    /// Writes `ident`, the column a reference ends in, as its identity, and
    /// blanks it when it names no column of the table.
    fn column(&self, ident: &mut Ident) {
        let identity = identity_of(self.columns, ident, self.case);
        ident.quote_style = None;
        ident.value = identity.map_or_else(String::new, |identity| format!("{identity:?}"));
    }
}

/// Writes `name` as one blank part, however qualified.
fn blank_name(name: &mut ObjectName) {
    name.0 = alloc::vec![ObjectNamePart::Identifier(Ident::new(String::new()))];
}

/// Blanks the name of a user-defined type `data_type` names.
fn blank_data_type(data_type: &mut DataType) {
    if let DataType::Custom(name, _) = data_type {
        blank_name(name);
    }
}

impl VisitorMut for Blank<'_> {
    type Break = Infallible;

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        match expression {
            Expr::Identifier(ident) => self.column(ident),
            Expr::CompoundIdentifier(parts) => {
                // One part spells the column as a bare reference does.
                parts.drain(..parts.len().saturating_sub(1));
                for part in parts {
                    self.column(part);
                }
            }
            Expr::Function(function) => blank_name(&mut function.name),
            Expr::Cast { data_type, .. } => blank_data_type(data_type),
            Expr::TypedString(typed) => blank_data_type(&mut typed.data_type),
            _ => {}
        }
        ControlFlow::Continue(())
    }
}
