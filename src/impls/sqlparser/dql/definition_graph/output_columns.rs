use alloc::{borrow::ToOwned, collections::BTreeMap, format, string::String, vec, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Ident, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator,
    Values, Visit, Visitor,
};

use super::{
    super::{
        ColumnOrigin, DerivationProfile, DerivedColumn, LookupOutcome, RelationKey, WildcardColumn,
        each_wildcard_column, qualified_wildcard_columns, resolve_definition_local,
        wildcard_reshapes_output,
    },
    AstRef, Bound, DefinitionDerivation, DefinitionGraph, DefinitionId, ScopeCursor, ScopeId,
    select_address,
};
use crate::{
    errors::LookupError,
    structs::{ColumnBinding, ColumnDefinition, IdentifierCase, QueryOutput},
    traits::DatabaseLike,
    utils::{
        figured_names::{Figure, figure},
        walk::walk,
    },
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
        figure.output_name().map_or(Self::Unknown, |(name, quoted)| Self::Known { name, quoted })
    }
}

#[derive(Clone)]
pub(super) struct OutputColumn<'query, 'db> {
    name: OutputName,
    definition: DefinitionId,
    expression: Option<AstRef<'query, 'db, Expr>>,
    /// The position of the select-list item, or `VALUES` column, of the
    /// leading `SELECT` that produced the column.
    item: usize,
    /// How a reference reaches a column a wildcard expanded into.
    wildcard: Option<WildcardReach<'query, 'db>>,
}

/// The relation a reference reaches a wildcard's column through, `None` for
/// a merged column or one of an anonymous subquery, and the column it reads.
#[derive(Clone, Copy)]
struct WildcardReach<'query, 'db> {
    relation: Option<RelationKey<'query, 'db>>,
    origin: ColumnOrigin,
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

/// One select-list position, or a wildcard whose columns cannot be
/// enumerated, with its select-list item, `None` when it stands for the whole
/// output of an operand that is not modeled.
#[derive(Clone)]
pub(super) enum OutputEntry<'query, 'db> {
    Column(OutputColumn<'query, 'db>),
    Unexpanded(Option<usize>),
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
    site: crate::errors::AmbiguousLookupSite,
    dialect: crate::impls::SqlparserDialect,
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
    Err(LookupError::AmbiguousTableLookup {
        object_name: dialect.stored_name(key),
        candidates,
        site,
    })
}

impl<'query, 'db, DB: DatabaseLike> DefinitionDerivation<'query, 'db, DB> {
    /// Records the outputs of every `SELECT` scope derived so far and of
    /// every query `root` holds, itself included when it is one.
    pub(crate) fn record_outputs<N: Visit + ?Sized>(
        &mut self,
        root: &N,
    ) -> Result<(), LookupError> {
        for scope in (0..self.graph.scopes.len()).map(ScopeId) {
            if let Some(select) = self.graph.scopes[scope.0].select {
                self.graph.scopes[scope.0].outputs = self.select_outputs(scope, select)?;
            }
        }
        self.graph.select_index.sort_by_key(|entry| entry.address);
        let mut collector = QueryCollector { derivation: self, queries: BTreeMap::new() };
        let _: ControlFlow<()> = walk(root, &mut collector);
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
        for (position, item) in select.map(|select| select.projection.as_slice()).iter().enumerate()
        {
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
                let definition = self.expression_definition(expression, cursor)?;
                outputs.push(OutputEntry::Column(OutputColumn {
                    name,
                    definition,
                    expression: Some(expression),
                    item: position,
                    wildcard: None,
                }));
                continue;
            }
            let data = &self.graph.scopes[scope.0].data;
            let expanded = match item.get() {
                SelectItem::Wildcard(options)
                    if !data.has_opaque() && !wildcard_reshapes_output(options) =>
                {
                    each_wildcard_column(data, |column| {
                        outputs.push(wildcard_output(&column, position));
                    });
                    true
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(object_name),
                    options,
                ) if !wildcard_reshapes_output(options) => {
                    match qualified_wildcard_columns(
                        &data.bases,
                        &data.derived,
                        object_name,
                        data.case,
                    ) {
                        Some(columns) => {
                            outputs
                                .extend(columns.map(|column| wildcard_output(&column, position)));
                            true
                        }
                        None => false,
                    }
                }
                _ => false,
            };
            if !expanded {
                outputs.push(OutputEntry::Unexpanded(Some(position)));
            }
        }
        Ok(outputs)
    }
}

/// The output a wildcard at select-list position `item` lists for `column`.
fn wildcard_output<'query, 'db, DB: DatabaseLike>(
    column: &WildcardColumn<'_, 'query, 'db, DB, DefinitionId>,
    item: usize,
) -> OutputEntry<'query, 'db> {
    let DerivedColumn { name, quoted, definition, .. } = column.column(DefinitionId(0));
    OutputEntry::Column(OutputColumn {
        name: OutputName::Known { name, quoted },
        definition,
        expression: None,
        item,
        wildcard: Some(WildcardReach { relation: column.relation(), origin: column.origin() }),
    })
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

    #[cfg_attr(feature = "std", recursive::recursive)]
    fn body_outputs(&mut self, body: &SetExpr) -> Option<QueryOutputs<'query, 'db>> {
        match body {
            SetExpr::Select(select) => {
                self.derivation.graph.select_scope(select).map(QueryOutputs::Select)
            }
            SetExpr::SetOperation { op, left, right, .. } => {
                Some(QueryOutputs::Merged(self.merge(*op, left, right)))
            }
            SetExpr::Values(values) => Some(QueryOutputs::Merged(self.values_entries(values))),
            _ => None,
        }
    }

    /// The outputs of `values`, `column1`, `column2`, … with the definitions
    /// derived for them, or one unexpanded entry for a list whose columns
    /// were not enumerated, such as one whose rows differ in length.
    fn values_entries(&self, values: &Values) -> Vec<OutputEntry<'query, 'db>> {
        let Some(columns) = self.derivation.graph.values_columns(values) else {
            return vec![OutputEntry::Unexpanded(None)];
        };
        columns
            .iter()
            .enumerate()
            .map(|(item, definition)| {
                OutputEntry::Column(OutputColumn {
                    name: OutputName::exact(&format!("column{}", item + 1)),
                    definition: *definition,
                    expression: None,
                    item,
                    wildcard: None,
                })
            })
            .collect()
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
            return vec![OutputEntry::Unexpanded(None)];
        };
        let right_definitions = self.arm(right).and_then(|right| {
            self.arm_entries(&right)
                .iter()
                .map(|entry| {
                    match entry {
                        OutputEntry::Column(column) => Some(column.definition),
                        OutputEntry::Unexpanded(_) => None,
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
        site: crate::errors::AmbiguousLookupSite,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(match match_output(entries, key, case, site, self.dialect)? {
            OutputMatch::Found { definition, name, quoted } => {
                Some((
                    self.definition(definition),
                    ColumnBinding::new(
                        None,
                        Some((name, quoted)),
                        ColumnOrigin::Output,
                        None,
                        None,
                    ),
                ))
            }
            OutputMatch::Uncertain => {
                Some((
                    ColumnDefinition::Opaque,
                    ColumnBinding::new(None, None, ColumnOrigin::Unknown, None, None),
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
                    let name = match &column.name {
                        OutputName::Known { name, quoted } => Some((name.as_str(), *quoted)),
                        OutputName::Unknown => None,
                    };
                    QueryOutput::Column {
                        name,
                        definition: self.definition(column.definition),
                        item: column.item,
                        wildcard: column.wildcard.as_ref().map(|reach| {
                            let relation =
                                reach.relation.as_ref().map(|key| (key.value.get(), key.quoted));
                            self.binding(relation, name, reach.origin)
                        }),
                    }
                }
                OutputEntry::Unexpanded(item) => QueryOutput::Unexpanded { item: *item },
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
                    OutputEntry::Column(_) | OutputEntry::Unexpanded(_) => None,
                }
            })
            .collect()
    }

    /// The scope an `ORDER BY` key of `query` reads its input through, none
    /// for a set operation, whose keys name its outputs alone.
    pub(crate) fn order_by_cursor(&self, query: QueryId) -> Option<ScopeCursor> {
        match &self.queries[query.0].outputs {
            QueryOutputs::Select(scope) => Some(self.select_cursor(*scope)),
            QueryOutputs::Merged(_) => None,
        }
    }

    pub(crate) fn resolve_order_by<'scope>(
        &'scope self,
        query: QueryId,
        key: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        match &self.queries[query.0].outputs {
            QueryOutputs::Select(scope) => {
                self.resolve_output_first(
                    self.select_cursor(*scope),
                    key,
                    crate::errors::AmbiguousLookupSite::OrderBy,
                )
            }
            QueryOutputs::Merged(entries) => {
                match unwrap_nested(key) {
                    Expr::Identifier(name) => {
                        self.output_binding(
                            entries,
                            name,
                            self.case,
                            crate::errors::AmbiguousLookupSite::OrderBy,
                        )
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    pub(crate) fn resolve_output_first<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        key: &Expr,
        site: crate::errors::AmbiguousLookupSite,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        let key = unwrap_nested(key);
        let scope = &self.scopes[cursor.scope.0];
        if let Expr::Identifier(name) = key
            && let Some(bound) = self.output_binding(&scope.outputs, name, scope.data.case, site)?
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
                LookupOutcome::Found(resolved) => {
                    return Ok(Some(self.bound(&resolved, cursor.scope)));
                }
                LookupOutcome::Stop | LookupOutcome::SearchParent => {
                    if let Some(bound) = self.output_binding(
                        &scope.outputs,
                        name,
                        scope.data.case,
                        crate::errors::AmbiguousLookupSite::GroupBy,
                    )? {
                        return Ok(Some(bound));
                    }
                }
            }
        }
        self.resolve_binding(cursor, key)
    }
}
