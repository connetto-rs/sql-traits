use alloc::{borrow::ToOwned, collections::BTreeMap, format, string::String, vec, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Ident, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator,
    Values, Visit, Visitor,
};

use super::{
    super::{
        ColumnOrigin, DerivationProfile, LookupOutcome, expand_qualified_wildcard,
        push_wildcard_columns, resolve_definition_local, wildcard_reshapes_output,
    },
    AstRef, Bound, DefinitionDerivation, DefinitionGraph, DefinitionId, ScopeCursor, ScopeId,
    select_address,
};
use crate::{
    errors::LookupError,
    structs::{ColumnBinding, ColumnDefinition, IdentifierCase, QueryOutput},
    traits::DatabaseLike,
    utils::figured_names::{Figure, figure},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueryId(usize);

#[derive(Clone)]
enum OutputName {
    Known { name: String, quoted: bool },
    Unknown,
}

impl OutputName {
    fn exact(name: &str) -> Self {
        Self::Known { name: name.to_owned(), quoted: true }
    }

    /// The name a select-list entry without an alias takes.
    fn figured(figure: Figure) -> Self {
        match figure {
            Figure::Strong(name, quoted) | Figure::Weak(name, quoted) => {
                Self::Known { name, quoted }
            }
            Figure::Unnamed => Self::exact("?column?"),
            Figure::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone)]
pub(super) struct OutputColumn<'query, 'db> {
    name: OutputName,
    definition: DefinitionId,
    expression: Option<AstRef<'query, 'db, Expr>>,
}

impl OutputColumn<'_, '_> {
    fn same_value(&self, other: &Self) -> bool {
        self.definition == other.definition
            || matches!(
                (self.expression, other.expression),
                (Some(left), Some(right)) if left.get() == right.get()
            )
    }
}

/// One select-list position, or a wildcard whose columns cannot be enumerated.
#[derive(Clone)]
pub(super) enum OutputEntry<'query, 'db> {
    Column(OutputColumn<'query, 'db>),
    Unexpanded,
}

enum QueryOutputs<'query, 'db> {
    Select(ScopeId),
    Merged(Vec<OutputEntry<'query, 'db>>),
}

enum Arm<'query, 'db> {
    Scope(ScopeId),
    Query(usize),
    Owned(Vec<OutputEntry<'query, 'db>>),
}

pub(super) struct QueryNode<'query, 'db> {
    address: usize,
    outputs: QueryOutputs<'query, 'db>,
}

enum OutputMatch<'entries> {
    Found { definition: DefinitionId, name: &'entries str, quoted: bool },
    Uncertain,
    Absent,
}

fn query_address(query: &Query) -> usize {
    core::ptr::from_ref(query).addr()
}

pub(super) fn unwrap_nested(mut expression: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    expression
}

fn innermost_query(mut query: &Query) -> &Query {
    while let SetExpr::Query(inner) = query.body.as_ref() {
        query = inner;
    }
    query
}

fn match_output<'entries>(
    entries: &'entries [OutputEntry<'_, '_>],
    key: &Ident,
    case: IdentifierCase,
) -> Result<OutputMatch<'entries>, LookupError> {
    let matches = || {
        entries.iter().filter_map(|entry| {
            match entry {
                OutputEntry::Column(
                    column @ OutputColumn { name: OutputName::Known { name, quoted }, .. },
                ) if case.identifiers_match(
                    name,
                    *quoted,
                    &key.value,
                    key.quote_style.is_some(),
                ) =>
                {
                    Some((column, name.as_str(), *quoted))
                }
                _ => None,
            }
        })
    };
    let Some((first, name, quoted)) = matches().next() else {
        let uncertain = entries.iter().any(|entry| {
            !matches!(
                entry,
                OutputEntry::Column(OutputColumn { name: OutputName::Known { .. }, .. })
            )
        });
        return Ok(if uncertain { OutputMatch::Uncertain } else { OutputMatch::Absent });
    };
    if matches().all(|(column, ..)| first.same_value(column)) {
        return Ok(OutputMatch::Found { definition: first.definition, name, quoted });
    }
    let mut candidates = matches()
        .map(|(column, ..)| {
            column
                .expression
                .map_or_else(|| key.value.clone(), |expression| format!("{}", expression.get()))
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();
    Err(LookupError::AmbiguousTableLookup { object_name: key.value.clone(), candidates })
}

fn values_entries<'query, 'db>(values: &Values) -> Vec<OutputEntry<'query, 'db>> {
    let width = values.rows.first().map_or(0, |row| row.len());
    (1..=width)
        .map(|position| {
            OutputEntry::Column(OutputColumn {
                name: OutputName::exact(&format!("column{position}")),
                definition: DefinitionId(0),
                expression: None,
            })
        })
        .collect()
}

impl<'query, 'db, DB: DatabaseLike> DefinitionDerivation<'query, 'db, DB> {
    pub(crate) fn record_outputs(&mut self, query: &Query) -> Result<(), LookupError> {
        for scope in (0..self.graph.scopes.len()).map(ScopeId) {
            if let Some(select) = self.graph.scopes[scope.0].select {
                self.graph.scopes[scope.0].outputs = self.select_outputs(scope, select)?;
            }
        }
        self.graph.select_index.sort_by_key(|entry| entry.address);
        let mut collector = QueryCollector { derivation: self, queries: BTreeMap::new() };
        let _: ControlFlow<()> = query.visit(&mut collector);
        let queries = collector.queries;
        self.graph.queries =
            queries.into_iter().map(|(address, outputs)| QueryNode { address, outputs }).collect();
        Ok(())
    }

    fn select_outputs(
        &mut self,
        scope: ScopeId,
        select: AstRef<'query, 'db, Select>,
    ) -> Result<Vec<OutputEntry<'query, 'db>>, LookupError> {
        let cursor = ScopeCursor {
            scope,
            visible_entries: self.graph.scopes[scope.0].data.from_entry_count,
        };
        let mut outputs = Vec::new();
        for item in select.map(|select| select.projection.as_slice()).iter() {
            let expression = item.try_map(|item| {
                match item {
                    SelectItem::UnnamedExpr(expression)
                    | SelectItem::ExprWithAlias { expr: expression, .. } => Some(expression),
                    _ => None,
                }
            });
            if let Some(expression) = expression {
                let name = match item.get() {
                    SelectItem::ExprWithAlias { alias, .. } => {
                        OutputName::Known {
                            name: alias.value.clone(),
                            quoted: alias.quote_style.is_some(),
                        }
                    }
                    _ => OutputName::figured(figure(expression.get())),
                };
                let definition = self.expression_definition(expression, Some(cursor))?;
                outputs.push(OutputEntry::Column(OutputColumn {
                    name,
                    definition,
                    expression: Some(expression),
                }));
                continue;
            }
            let data = &self.graph.scopes[scope.0].data;
            let expansion = match item.get() {
                SelectItem::Wildcard(options)
                    if !data.has_opaque() && !wildcard_reshapes_output(options) =>
                {
                    let mut columns = Vec::new();
                    push_wildcard_columns(data, &mut columns, DefinitionId(0));
                    Some(columns)
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(object_name),
                    options,
                ) if !wildcard_reshapes_output(options) => {
                    expand_qualified_wildcard(&data.bases, &data.derived, object_name, data.case)
                }
                _ => None,
            };
            match expansion {
                Some(columns) => {
                    outputs.extend(columns.into_iter().map(|column| {
                        OutputEntry::Column(OutputColumn {
                            name: OutputName::Known { name: column.name, quoted: column.quoted },
                            definition: column.definition,
                            expression: None,
                        })
                    }));
                }
                None => outputs.push(OutputEntry::Unexpanded),
            }
        }
        Ok(outputs)
    }
}

struct QueryCollector<'derivation, 'query, 'db, DB: DatabaseLike> {
    derivation: &'derivation mut DefinitionDerivation<'query, 'db, DB>,
    queries: BTreeMap<usize, QueryOutputs<'query, 'db>>,
}

impl<'query, 'db, DB: DatabaseLike> QueryCollector<'_, 'query, 'db, DB> {
    fn record(&mut self, query: &Query) -> Option<usize> {
        let query = innermost_query(query);
        let address = query_address(query);
        if !self.queries.contains_key(&address) {
            let outputs = self.body_outputs(&query.body)?;
            self.queries.insert(address, outputs);
        }
        Some(address)
    }

    fn body_outputs(&mut self, body: &SetExpr) -> Option<QueryOutputs<'query, 'db>> {
        match body {
            SetExpr::Select(select) => {
                self.derivation.graph.select_scope(select).map(QueryOutputs::Select)
            }
            SetExpr::SetOperation { op, left, right, .. } => {
                Some(QueryOutputs::Merged(self.merge(*op, left, right)))
            }
            SetExpr::Values(values) => Some(QueryOutputs::Merged(values_entries(values))),
            _ => None,
        }
    }

    fn arm(&mut self, body: &SetExpr) -> Option<Arm<'query, 'db>> {
        if let SetExpr::Query(query) = body {
            return self.record(query).map(Arm::Query);
        }
        Some(match self.body_outputs(body)? {
            QueryOutputs::Select(scope) => Arm::Scope(scope),
            QueryOutputs::Merged(entries) => Arm::Owned(entries),
        })
    }

    fn arm_entries<'arm>(
        &'arm self,
        arm: &'arm Arm<'query, 'db>,
    ) -> &'arm [OutputEntry<'query, 'db>] {
        let scope = match arm {
            Arm::Scope(scope) => *scope,
            Arm::Query(address) => {
                match &self.queries[address] {
                    QueryOutputs::Select(scope) => *scope,
                    QueryOutputs::Merged(entries) => return entries,
                }
            }
            Arm::Owned(entries) => return entries,
        };
        &self.derivation.graph.scopes[scope.0].outputs
    }

    fn merge(
        &mut self,
        operator: SetOperator,
        left: &SetExpr,
        right: &SetExpr,
    ) -> Vec<OutputEntry<'query, 'db>> {
        let Some(left) = self.arm(left) else {
            return vec![OutputEntry::Unexpanded];
        };
        let right_definitions = self.arm(right).and_then(|right| {
            self.arm_entries(&right)
                .iter()
                .map(|entry| {
                    match entry {
                        OutputEntry::Column(column) => Some(column.definition),
                        OutputEntry::Unexpanded => None,
                    }
                })
                .collect::<Option<Vec<_>>>()
        });
        let mut merged = match left {
            Arm::Owned(entries) => entries,
            stored => self.arm_entries(&stored).to_vec(),
        };
        let right_definitions = right_definitions.filter(|definitions| {
            definitions.len() == merged.len()
                && merged.iter().all(|entry| matches!(entry, OutputEntry::Column(_)))
        });
        let mut right_definitions = right_definitions.into_iter().flatten();
        for entry in &mut merged {
            if let OutputEntry::Column(column) = entry {
                column.definition = right_definitions.next().map_or(DefinitionId(0), |right| {
                    self.derivation.set_definition(operator, column.definition, right)
                });
                column.expression = None;
            }
        }
        merged
    }
}

impl<DB: DatabaseLike> Visitor for QueryCollector<'_, '_, '_, DB> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.record(query);
        ControlFlow::Continue(())
    }
}

impl<'query, 'db, DB: DatabaseLike> DefinitionGraph<'query, 'db, DB> {
    pub(super) fn select_scope(&self, select: &Select) -> Option<ScopeId> {
        let address = select_address(select);
        let end = self.select_index.partition_point(|entry| entry.address <= address);
        self.select_index[..end]
            .last()
            .filter(|entry| entry.address == address)
            .map(|entry| entry.scope)
    }

    pub(super) fn select_cursor(&self, scope: ScopeId) -> ScopeCursor {
        ScopeCursor { scope, visible_entries: self.scopes[scope.0].data.from_entry_count }
    }

    fn output_binding<'scope>(
        &'scope self,
        entries: &'scope [OutputEntry<'query, 'db>],
        key: &Ident,
        case: IdentifierCase,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(match match_output(entries, key, case)? {
            OutputMatch::Found { definition, name, quoted } => {
                Some((
                    self.definition(definition),
                    ColumnBinding::new(None, Some((name, quoted)), ColumnOrigin::Output),
                ))
            }
            OutputMatch::Uncertain => {
                Some((
                    ColumnDefinition::Opaque,
                    ColumnBinding::new(None, None, ColumnOrigin::Unknown),
                ))
            }
            OutputMatch::Absent => None,
        })
    }

    pub(crate) fn query_id(&self, query: &Query) -> Option<QueryId> {
        let address = query_address(innermost_query(query));
        self.queries.binary_search_by_key(&address, |node| node.address).ok().map(QueryId)
    }

    fn query_entries(&self, query: QueryId) -> &[OutputEntry<'query, 'db>] {
        match &self.queries[query.0].outputs {
            QueryOutputs::Select(scope) => &self.scopes[scope.0].outputs,
            QueryOutputs::Merged(entries) => entries,
        }
    }

    pub(crate) fn query_outputs<'scope>(
        &'scope self,
        query: QueryId,
    ) -> impl ExactSizeIterator<Item = QueryOutput<'scope, 'query, 'db, DB>> {
        self.query_entries(query).iter().map(|entry| {
            match entry {
                OutputEntry::Column(column) => {
                    QueryOutput::Column {
                        name: match &column.name {
                            OutputName::Known { name, quoted } => Some((name.as_str(), *quoted)),
                            OutputName::Unknown => None,
                        },
                        definition: self.definition(column.definition),
                    }
                }
                OutputEntry::Unexpanded => QueryOutput::Unexpanded,
            }
        })
    }

    /// The names `query` outputs, in order, each with its quote state, named
    /// the way PostgreSQL names an unaliased select-list item, or `None` when
    /// some output cannot be named.
    pub(crate) fn query_output_names(&self, query: QueryId) -> Option<Vec<(String, bool)>> {
        self.query_entries(query)
            .iter()
            .map(|entry| {
                match entry {
                    OutputEntry::Column(OutputColumn {
                        name: OutputName::Known { name, quoted },
                        ..
                    }) => Some((name.clone(), *quoted)),
                    OutputEntry::Column(_) | OutputEntry::Unexpanded => None,
                }
            })
            .collect()
    }

    pub(crate) fn resolve_order_by<'scope>(
        &'scope self,
        query: QueryId,
        key: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        match &self.queries[query.0].outputs {
            QueryOutputs::Select(scope) => {
                self.resolve_output_first(self.select_cursor(*scope), key)
            }
            QueryOutputs::Merged(entries) => {
                match unwrap_nested(key) {
                    Expr::Identifier(name) => self.output_binding(entries, name, self.case),
                    _ => Ok(None),
                }
            }
        }
    }

    pub(crate) fn resolve_output_first<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        key: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        let key = unwrap_nested(key);
        let scope = &self.scopes[cursor.scope.0];
        if let Expr::Identifier(name) = key
            && let Some(bound) = self.output_binding(&scope.outputs, name, scope.data.case)?
        {
            return Ok(Some(bound));
        }
        self.resolve_binding(cursor, key)
    }

    pub(crate) fn resolve_input_first<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        key: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        let key = unwrap_nested(key);
        let scope = &self.scopes[cursor.scope.0];
        if let Expr::Identifier(name) = key {
            match resolve_definition_local(
                &scope.data,
                cursor.visible_entries,
                DefinitionId(0),
                key,
                false,
                scope.data.case,
            )? {
                LookupOutcome::Found(resolved) => return Ok(Some(self.bound(&resolved))),
                LookupOutcome::Stop | LookupOutcome::SearchParent => {
                    if let Some(bound) =
                        self.output_binding(&scope.outputs, name, scope.data.case)?
                    {
                        return Ok(Some(bound));
                    }
                }
            }
        }
        self.resolve_binding(cursor, key)
    }
}
