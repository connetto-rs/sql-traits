//! Per-query resolution of column references and definitions.

use core::fmt;

use sqlparser::ast::{Expr, ObjectName, Query, Select, SetOperator};

use crate::{
    errors::LookupError,
    impls::dql::{
        build_definition_graph,
        definition_graph::{DefinitionGraph, DefinitionId, QueryId, ScopeCursor, table_graph},
    },
    structs::IdentifierCase,
    traits::DatabaseLike,
};

/// The definition that determines one resolved column's declared type.
#[derive(Debug)]
pub enum ColumnDefinition<'scope, 'query, 'db, DB: DatabaseLike> {
    /// A stored table column and its table.
    Base {
        /// The table declaring the column.
        table: &'scope DB::Table,
        /// The declared column.
        column: &'scope DB::Column,
    },
    /// A projection expression and its defining scope.
    Expression {
        /// The expression defining the column.
        expression: &'scope Expr,
        /// The scope in which the expression was declared.
        scope: ColumnDefinitionScope<'scope, 'query, 'db, DB>,
    },
    /// The definitions combined by an ordinary set operation.
    SetOperation {
        /// The operation combining the definitions.
        operator: SetOperator,
        /// The left definition.
        left: ColumnDefinitionRef<'scope, 'query, 'db, DB>,
        /// The right definition.
        right: ColumnDefinitionRef<'scope, 'query, 'db, DB>,
    },
    /// The anchor and recursive definitions of a recursive union.
    RecursiveUnion {
        /// The nonrecursive anchor definition.
        anchor: ColumnDefinitionRef<'scope, 'query, 'db, DB>,
        /// The recursive definition.
        recursive: ColumnDefinitionRef<'scope, 'query, 'db, DB>,
    },
    /// A relation exposes the name without an inspectable definition.
    Opaque,
}

/// One entry of a query's ordered output list.
#[derive(Debug)]
pub enum QueryOutput<'scope, 'query, 'db, DB: DatabaseLike> {
    /// One output column and the definition that determines its type.
    Column(ColumnDefinition<'scope, 'query, 'db, DB>),
    /// A wildcard whose columns cannot be enumerated, such as one over an
    /// opaque relation or one reshaped by `EXCLUDE`, `REPLACE` or `RENAME`.
    /// It stands for an unknown number of columns, possibly none.
    Unexpanded,
}

/// The relation and exposed column names matched by a resolved reference or
/// clause key.
#[derive(Debug, Clone, Copy)]
pub struct ColumnBinding<'scope> {
    relation: Option<(&'scope str, bool)>,
    exposed: Option<(&'scope str, bool)>,
}

impl<'scope> ColumnBinding<'scope> {
    pub(crate) fn new(
        relation: Option<(&'scope str, bool)>,
        exposed: Option<(&'scope str, bool)>,
    ) -> Self {
        Self { relation, exposed }
    }

    /// Returns the relation name, or `None` for a merged, anonymous or opaque
    /// binding and for a clause key matching a select-list output name.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&SQLiteDialect {}, "SELECT [D].[ID] FROM docs AS [d]")?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let reference =
    ///     Expr::CompoundIdentifier(vec![Ident::with_quote('[', "D"), Ident::with_quote('[', "ID")]);
    /// let (_, binding) = scope.resolve_column_binding(&reference)?.expect("column resolves");
    /// assert_eq!(binding.relation(), Some(("d", true)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn relation(&self) -> Option<(&'scope str, bool)> {
        self.relation
    }

    /// Returns the exposed column name, or the select-list output name a
    /// clause key matched, or `None` for an opaque binding.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let sql = "WITH c([VaLuE]) AS (SELECT id FROM docs) SELECT [VALUE] FROM c";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let reference = Expr::Identifier(Ident::with_quote('[', "VALUE"));
    /// let (_, binding) = scope.resolve_column_binding(&reference)?.expect("column resolves");
    /// assert_eq!(binding.exposed(), Some(("VaLuE", true)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn exposed(&self) -> Option<(&'scope str, bool)> {
        self.exposed
    }
}

/// The relation a qualifier with no column, such as a qualified wildcard's,
/// matched.
#[derive(Debug, Clone, Copy)]
pub struct RelationBinding<'scope> {
    relation: Option<(&'scope str, bool)>,
}

impl<'scope> RelationBinding<'scope> {
    pub(crate) fn new(relation: Option<(&'scope str, bool)>) -> Self {
        Self { relation }
    }

    /// Returns the alias, or the declared name of a relation referenced
    /// without one, or `None` for an opaque relation.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{ObjectName, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let sql = "SELECT [MIXED].*, [G].* FROM docs AS [MiXeD], generate_series(1, 2) AS [g]";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let qualifiers = select.projection.iter().map(|item| {
    ///     let SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(name), _) =
    ///         item
    ///     else {
    ///         unreachable!()
    ///     };
    ///     name
    /// });
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let relations = qualifiers
    ///     .map(|name| scope.resolve_relation_binding(name).map(|binding| binding.relation()))
    ///     .collect::<Vec<_>>();
    /// assert_eq!(relations, [Some(Some(("MiXeD", true))), Some(None)]);
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn relation(&self) -> Option<(&'scope str, bool)> {
        self.relation
    }
}

/// A borrowed handle to one immutable definition node.
///
/// ```compile_fail
/// use sql_traits::{prelude::ParserDB, structs::ColumnDefinitionRef};
/// let _ = ColumnDefinitionRef::<ParserDB> {};
/// ```
pub struct ColumnDefinitionRef<'scope, 'query, 'db, DB: DatabaseLike> {
    graph: &'scope DefinitionGraph<'query, 'db, DB>,
    id: DefinitionId,
}

impl<'scope, 'query, 'db, DB: DatabaseLike> ColumnDefinitionRef<'scope, 'query, 'db, DB> {
    pub(crate) fn new(graph: &'scope DefinitionGraph<'query, 'db, DB>, id: DefinitionId) -> Self {
        Self { graph, id }
    }

    /// Returns this node's definition view.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")?;
    /// let sql = "SELECT u.id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) AS u";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let reference = Expr::CompoundIdentifier(vec![Ident::new("u"), Ident::new("id")]);
    /// let Some(ColumnDefinition::SetOperation { left, right, .. }) =
    ///     scope.resolve_column_definition(&reference)?
    /// else {
    ///     unreachable!()
    /// };
    /// for (side, expected) in [(left, "a"), (right, "b")] {
    ///     let ColumnDefinition::Base { table, .. } = side.definition() else { unreachable!() };
    ///     assert_eq!(table.table_name(), expected);
    /// }
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn definition(self) -> ColumnDefinition<'scope, 'query, 'db, DB> {
        self.graph.definition(self.id)
    }
}

impl<DB: DatabaseLike> Copy for ColumnDefinitionRef<'_, '_, '_, DB> {}

#[expect(clippy::expl_impl_clone_on_copy, reason = "derive would require DB: Clone")]
impl<DB: DatabaseLike> Clone for ColumnDefinitionRef<'_, '_, '_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB: DatabaseLike> fmt::Debug for ColumnDefinitionRef<'_, '_, '_, DB> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ColumnDefinitionRef").field(&self.id).finish()
    }
}

/// A borrowed resolver for one definition-local scope.
pub struct ColumnDefinitionScope<'scope, 'query, 'db, DB: DatabaseLike> {
    graph: &'scope DefinitionGraph<'query, 'db, DB>,
    cursor: ScopeCursor,
}

impl<'scope, 'query, 'db, DB: DatabaseLike> ColumnDefinitionScope<'scope, 'query, 'db, DB> {
    pub(crate) fn new(
        graph: &'scope DefinitionGraph<'query, 'db, DB>,
        cursor: ScopeCursor,
    ) -> Self {
        Self { graph, cursor }
    }

    /// Resolves a column through this scope and its enclosing scopes.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);")?;
    /// let sql = "SELECT d.doubled FROM (SELECT id * 2 AS doubled FROM a) AS d";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let output = Expr::CompoundIdentifier(vec![Ident::new("d"), Ident::new("doubled")]);
    /// let Some(ColumnDefinition::Expression { scope: defining, .. }) =
    ///     scope.resolve_column_definition(&output)?
    /// else {
    ///     unreachable!()
    /// };
    /// let Some(ColumnDefinition::Base { table, column }) =
    ///     defining.resolve_column_definition(&Expr::Identifier(Ident::new("id")))?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!((table.table_name(), column.column_name()), ("a", "id"));
    /// let missing = Expr::Identifier(Ident::new("missing"));
    /// assert!(defining.resolve_column_definition(&missing)?.is_none());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_column_definition(
        &self,
        reference: &Expr,
    ) -> Result<Option<ColumnDefinition<'scope, 'query, 'db, DB>>, LookupError> {
        self.graph.resolve_definition(self.cursor, reference)
    }

    /// Resolves a column's definition and binding through this scope and its
    /// parents.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous
    /// reference and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);")?;
    /// let sql = "SELECT d.doubled FROM (SELECT id * 2 AS doubled FROM a) AS d";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let output = Expr::CompoundIdentifier(vec![Ident::new("d"), Ident::new("doubled")]);
    /// let Some((ColumnDefinition::Expression { scope: defining, .. }, outer_binding)) =
    ///     scope.resolve_column_binding(&output)?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(outer_binding.relation(), Some(("d", false)));
    /// assert_eq!(outer_binding.exposed(), Some(("doubled", false)));
    /// let Some((ColumnDefinition::Base { table, column }, inner_binding)) =
    ///     defining.resolve_column_binding(&Expr::Identifier(Ident::new("id")))?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!((table.table_name(), column.column_name()), ("a", "id"));
    /// assert_eq!(inner_binding.relation(), Some(("a", false)));
    /// assert_eq!(inner_binding.exposed(), Some(("id", false)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_column_binding(
        &self,
        reference: &Expr,
    ) -> Result<
        Option<(ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>)>,
        LookupError,
    > {
        self.graph.resolve_binding(self.cursor, reference)
    }

    /// Resolves a whole `GROUP BY` key of this scope's `SELECT` under
    /// PostgreSQL's rules, preferring a `FROM` column over a select-list output
    /// name.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, GroupByExpr, Ident, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT, x INT);")?;
    /// let sql = "SELECT id AS x, id AS y FROM a GROUP BY x, y";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let GroupByExpr::Expressions(keys, _) = &select.group_by else { unreachable!() };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let select_scope = scope.scope_for_select(select).expect("the root scope is recorded");
    /// for (key, expected) in keys.iter().zip(["x", "id"]) {
    ///     let Some(ColumnDefinition::Base { column, .. }) =
    ///         select_scope.resolve_group_by_definition(key)?
    ///     else {
    ///         unreachable!()
    ///     };
    ///     assert_eq!(column.column_name(), expected);
    /// }
    /// let parameter = Expr::Identifier(Ident::new("p"));
    /// assert!(select_scope.resolve_group_by_definition(&parameter)?.is_none());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_group_by_definition(
        &self,
        key: &Expr,
    ) -> Result<Option<ColumnDefinition<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(self.graph.resolve_input_first(self.cursor, key)?.map(|(definition, _)| definition))
    }

    /// Resolves a whole `GROUP BY` key like
    /// [`Self::resolve_group_by_definition`], with the names it matched.
    ///
    /// A key naming a select-list output reports that output's name as its
    /// exposed name and no relation.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{GroupByExpr, SetExpr, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT, x INT);")?;
    /// let sql = "SELECT id AS [VaLuE], id AS [X] FROM docs AS [D] GROUP BY [VALUE], x";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let GroupByExpr::Expressions(keys, _) = &select.group_by else { unreachable!() };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let select_scope = scope.scope_for_select(select).expect("the root scope is recorded");
    /// let (_, output) = select_scope.resolve_group_by_binding(&keys[0])?.expect("key resolves");
    /// assert_eq!((output.relation(), output.exposed()), (None, Some(("VaLuE", true))));
    /// let (_, input) = select_scope.resolve_group_by_binding(&keys[1])?.expect("key resolves");
    /// assert_eq!((input.relation(), input.exposed()), (Some(("D", true)), Some(("x", false))));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_group_by_binding(
        &self,
        key: &Expr,
    ) -> Result<
        Option<(ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>)>,
        LookupError,
    > {
        self.graph.resolve_input_first(self.cursor, key)
    }

    /// Resolves a whole `DISTINCT ON` key of this scope's `SELECT` under
    /// PostgreSQL's rules, preferring a select-list output name over an input
    /// column.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Distinct, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT, x INT);")?;
    /// let sql = "SELECT DISTINCT ON (x, x + 1) id AS x FROM a";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let Some(Distinct::On(keys)) = &select.distinct else { unreachable!() };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let select_scope = scope.scope_for_select(select).expect("the root scope is recorded");
    /// let Some(ColumnDefinition::Base { column, .. }) =
    ///     select_scope.resolve_distinct_on_definition(&keys[0])?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(column.column_name(), "id");
    /// assert!(select_scope.resolve_distinct_on_definition(&keys[1])?.is_none());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_distinct_on_definition(
        &self,
        key: &Expr,
    ) -> Result<Option<ColumnDefinition<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(self.graph.resolve_output_first(self.cursor, key)?.map(|(definition, _)| definition))
    }

    /// Resolves a whole `DISTINCT ON` key like
    /// [`Self::resolve_distinct_on_definition`], with the names it matched.
    ///
    /// A key naming a select-list output reports that output's name as its
    /// exposed name and no relation.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Distinct, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE docs(id INT, x INT);")?;
    /// let sql = "SELECT DISTINCT ON (VALUE, docs.x) id AS \"VaLuE\" FROM docs";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let Some(Distinct::On(keys)) = &select.distinct else { unreachable!() };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let select_scope = scope.scope_for_select(select).expect("the root scope is recorded");
    /// let (_, output) = select_scope.resolve_distinct_on_binding(&keys[0])?.expect("key resolves");
    /// assert_eq!((output.relation(), output.exposed()), (None, Some(("VaLuE", true))));
    /// let (_, input) = select_scope.resolve_distinct_on_binding(&keys[1])?.expect("key resolves");
    /// assert_eq!((input.relation(), input.exposed()), (Some(("docs", false)), Some(("x", false))));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_distinct_on_binding(
        &self,
        key: &Expr,
    ) -> Result<
        Option<(ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>)>,
        LookupError,
    > {
        self.graph.resolve_output_first(self.cursor, key)
    }

    /// Resolves a relation qualifier with no column, such as a qualified
    /// wildcard's, through this scope and its enclosing scopes.
    ///
    /// Answers `None` when no visible relation matches.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{
    ///         Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, SetExpr, Statement,
    ///     },
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let sql = "SELECT (SELECT count([O].*) FROM docs AS i) FROM docs AS [o]";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(Expr::Subquery(nested)) = &select.projection[0] else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(nested) = nested.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(Expr::Function(count)) = &nested.projection[0] else {
    ///     unreachable!()
    /// };
    /// let FunctionArguments::List(arguments) = &count.args else { unreachable!() };
    /// let FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(qualifier)) = &arguments.args[0]
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let nested_scope = scope.scope_for_select(nested).expect("the subquery scope is recorded");
    /// let binding = nested_scope.resolve_relation_binding(qualifier).expect("relation resolves");
    /// assert_eq!(binding.relation(), Some(("o", true)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn resolve_relation_binding(
        &self,
        qualifier: &ObjectName,
    ) -> Option<RelationBinding<'scope>> {
        self.graph.resolve_relation(self.cursor, qualifier)
    }

    /// Returns the recorded scope for this exact nested `Select`.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, SetExpr, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")?;
    /// let sql = "SELECT d.x FROM (SELECT (SELECT id FROM b WHERE b.id = a.id) AS x FROM a) AS d";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let output = Expr::CompoundIdentifier(vec![Ident::new("d"), Ident::new("x")]);
    /// let Some(ColumnDefinition::Expression { expression: Expr::Subquery(nested), scope: defining }) =
    ///     scope.resolve_column_definition(&output)?
    /// else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = nested.body.as_ref() else { unreachable!() };
    /// let nested_scope = defining.scope_for_select(select).expect("the subquery scope is recorded");
    /// for (reference, expected) in [
    ///     (Expr::Identifier(Ident::new("id")), "b"),
    ///     (Expr::CompoundIdentifier(vec![Ident::new("a"), Ident::new("id")]), "a"),
    /// ] {
    ///     let Some(ColumnDefinition::Base { table, .. }) =
    ///         nested_scope.resolve_column_definition(&reference)?
    ///     else {
    ///         unreachable!()
    ///     };
    ///     assert_eq!(table.table_name(), expected);
    /// }
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn scope_for_select(
        &self,
        select: &Select,
    ) -> Option<ColumnDefinitionScope<'scope, 'query, 'db, DB>> {
        self.graph.scope_for_select(Some(self.cursor), select)
    }
}

impl<DB: DatabaseLike> Copy for ColumnDefinitionScope<'_, '_, '_, DB> {}

#[expect(clippy::expl_impl_clone_on_copy, reason = "derive would require DB: Clone")]
impl<DB: DatabaseLike> Clone for ColumnDefinitionScope<'_, '_, '_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB: DatabaseLike> fmt::Debug for ColumnDefinitionScope<'_, '_, '_, DB> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ColumnDefinitionScope").field(&self.cursor).finish()
    }
}

/// A borrowed view of one query's outputs and of its `ORDER BY` keys under
/// PostgreSQL's rules.
pub struct ColumnQueryScope<'scope, 'query, 'db, DB: DatabaseLike> {
    graph: &'scope DefinitionGraph<'query, 'db, DB>,
    query: QueryId,
}

impl<'scope, 'query, 'db, DB: DatabaseLike> ColumnQueryScope<'scope, 'query, 'db, DB> {
    /// Returns the query's outputs in select-list order, with every enumerable
    /// wildcard expanded in place.
    ///
    /// The iterator's length counts entries, and an
    /// [`QueryOutput::Unexpanded`] entry stands for an unknown number of
    /// columns. The SQL column count, and the ordinal of every entry after an
    /// unexpanded one, are therefore known only when no entry is unexpanded.
    /// A set operation pairs its operands' columns only when both widths are
    /// known, and otherwise answers [`ColumnDefinition::Opaque`] for every
    /// column of its left operand.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT, x INT);")?;
    /// let sql = "SELECT (SELECT * FROM a LIMIT 1) FROM a";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(Expr::Subquery(nested)) = &select.projection[0] else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let outputs = scope.scope_for_query(nested).expect("the nested query is recorded");
    /// let names = outputs
    ///     .output_definitions()
    ///     .map(|output| {
    ///         let QueryOutput::Column(ColumnDefinition::Base { column, .. }) = output else {
    ///             unreachable!()
    ///         };
    ///         column.column_name()
    ///     })
    ///     .collect::<Vec<_>>();
    /// assert_eq!(names, ["id", "x"]);
    ///
    /// let sql = "SELECT a.id, * FROM a, generate_series(1, 2) AS g";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let outputs = scope.scope_for_query(&query).expect("the root query is recorded");
    /// let mut entries = outputs.output_definitions();
    /// assert_eq!(entries.len(), 2);
    /// assert!(matches!(entries.next(), Some(QueryOutput::Column(ColumnDefinition::Base { .. }))));
    /// assert!(matches!(entries.next(), Some(QueryOutput::Unexpanded)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn output_definitions(
        &self,
    ) -> impl ExactSizeIterator<Item = QueryOutput<'scope, 'query, 'db, DB>> + use<'scope, 'query, 'db, DB>
    {
        self.graph.query_outputs(self.query)
    }

    /// Resolves a whole `ORDER BY` key, preferring a select-list output name
    /// over an input column and seeing only output names after a set operation.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, OrderByKind, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT, row_alias INT);")?;
    /// let sql = "SELECT id AS row_alias FROM a ORDER BY row_alias";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let Some(OrderByKind::Expressions(keys)) = query.order_by.as_ref().map(|order| &order.kind)
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let order = scope.scope_for_query(&query).expect("the root query is recorded");
    /// let Some(ColumnDefinition::Base { column, .. }) =
    ///     order.resolve_order_by_definition(&keys[0].expr)?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(column.column_name(), "id");
    /// let parameter = Expr::Identifier(Ident::new("p"));
    /// assert!(order.resolve_order_by_definition(&parameter)?.is_none());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_order_by_definition(
        &self,
        key: &Expr,
    ) -> Result<Option<ColumnDefinition<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(self.graph.resolve_order_by(self.query, key)?.map(|(definition, _)| definition))
    }

    /// Resolves a whole `ORDER BY` key like
    /// [`Self::resolve_order_by_definition`], with the names it matched.
    ///
    /// A key naming a select-list output reports that output's name as its
    /// exposed name and no relation. After a set operation that name is the
    /// leading operand's.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] for an ambiguous reference
    /// or output name and relation-name lookup errors from modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let sql = "SELECT id AS [VaLuE] FROM docs ORDER BY [VALUE], docs.[ID]";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let Some(sqlparser::ast::OrderByKind::Expressions(keys)) =
    ///     query.order_by.as_ref().map(|order| &order.kind)
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let order = scope.scope_for_query(&query).expect("the root query is recorded");
    /// let (_, output) = order.resolve_order_by_binding(&keys[0].expr)?.expect("key resolves");
    /// assert_eq!((output.relation(), output.exposed()), (None, Some(("VaLuE", true))));
    /// let (_, input) = order.resolve_order_by_binding(&keys[1].expr)?.expect("key resolves");
    /// assert_eq!((input.relation(), input.exposed()), (Some(("docs", false)), Some(("id", false))));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_order_by_binding(
        &self,
        key: &Expr,
    ) -> Result<
        Option<(ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>)>,
        LookupError,
    > {
        self.graph.resolve_order_by(self.query, key)
    }
}

impl<DB: DatabaseLike> Copy for ColumnQueryScope<'_, '_, '_, DB> {}

#[expect(clippy::expl_impl_clone_on_copy, reason = "derive would require DB: Clone")]
impl<DB: DatabaseLike> Clone for ColumnQueryScope<'_, '_, '_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB: DatabaseLike> fmt::Debug for ColumnQueryScope<'_, '_, '_, DB> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ColumnQueryScope").field(&self.query).finish()
    }
}

/// The relations available to one query projection or table definition.
pub struct ColumnScope<'query, 'db, DB: DatabaseLike> {
    graph: DefinitionGraph<'query, 'db, DB>,
    root: ScopeCursor,
}

impl<'query, 'db, DB: DatabaseLike> ColumnScope<'query, 'db, DB> {
    /// Builds query scopes with `case` and reads stored definitions as written.
    ///
    /// # Errors
    ///
    /// Returns relation-name lookup errors and
    /// [`LookupError::AmbiguousTableLookup`] when a modeled relation lookup is
    /// ambiguous.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);")?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&GenericDialect {}, "SELECT id FROM a")?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let Some(ColumnDefinition::Base { table, column }) =
    ///     scope.resolve_column_definition(&Expr::Identifier(Ident::new("id")))?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!((table.table_name(), column.column_name()), ("a", "id"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn from_query(
        query: &'query Query,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        let (graph, root) = build_definition_graph(query, database, case)?;
        Ok(Self { graph, root })
    }

    /// Resolves one column to its source table without following definition
    /// parents.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when more than one local
    /// relation exposes the reference.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&GenericDialect {}, "SELECT a.id FROM a JOIN b ON true")?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let qualified = Expr::CompoundIdentifier(vec![Ident::new("a"), Ident::new("id")]);
    /// assert_eq!(scope.resolve_column(&qualified)?.map(TableLike::table_name), Some("a"));
    /// assert!(scope.resolve_column(&Expr::Identifier(Ident::new("id"))).is_err());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_column(&self, reference: &Expr) -> Result<Option<&'db DB::Table>, LookupError> {
        self.graph.resolve_source(self.root, reference)
    }

    /// Resolves one column to the definition that determines its type.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when more than one
    /// relation exposes the reference and relation-name lookup errors from
    /// modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);")?;
    /// let sql = "WITH c AS (SELECT id FROM a) SELECT c.id FROM c";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let reference = Expr::CompoundIdentifier(vec![Ident::new("c"), Ident::new("id")]);
    /// let Some(ColumnDefinition::Base { table, column }) =
    ///     scope.resolve_column_definition(&reference)?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!((table.table_name(), column.column_name()), ("a", "id"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_column_definition(
        &self,
        reference: &Expr,
    ) -> Result<Option<ColumnDefinition<'_, 'query, 'db, DB>>, LookupError> {
        self.graph.resolve_definition(self.root, reference)
    }

    /// Resolves a column's definition and matched relation and exposed names.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when more than one
    /// relation exposes the reference and relation-name lookup errors from
    /// modeled definitions.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE docs (id INT);")?;
    /// let sql = "SELECT [MIXED].[ID] AS result FROM docs AS [MiXeD]";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let reference = Expr::CompoundIdentifier(vec![
    ///     Ident::with_quote('[', "MIXED"),
    ///     Ident::with_quote('[', "ID"),
    /// ]);
    /// let Some((ColumnDefinition::Base { column, .. }, binding)) =
    ///     scope.resolve_column_binding(&reference)?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(column.column_name(), "id");
    /// assert_eq!(binding.relation(), Some(("MiXeD", true)));
    /// assert_eq!(binding.exposed(), Some(("id", false)));
    ///
    /// let cte_sql = "WITH [MiXeD]([VaLuE]) AS (SELECT id FROM docs) \
    ///                SELECT [MIXED].[VALUE] AS result FROM [mixed]";
    /// let Statement::Query(cte_query) = Parser::parse_sql(&SQLiteDialect {}, cte_sql)?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let cte_scope = ColumnScope::from_query(&cte_query, &db, IdentifierCase::Folded)?;
    /// let cte_reference = Expr::CompoundIdentifier(vec![
    ///     Ident::with_quote('[', "MIXED"),
    ///     Ident::with_quote('[', "VALUE"),
    /// ]);
    /// let Some((ColumnDefinition::Base { column, .. }, cte_binding)) =
    ///     cte_scope.resolve_column_binding(&cte_reference)?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(column.column_name(), "id");
    /// assert_eq!(cte_binding.relation(), Some(("MiXeD", true)));
    /// assert_eq!(cte_binding.exposed(), Some(("VaLuE", true)));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn resolve_column_binding(
        &self,
        reference: &Expr,
    ) -> Result<Option<(ColumnDefinition<'_, 'query, 'db, DB>, ColumnBinding<'_>)>, LookupError>
    {
        self.graph.resolve_binding(self.root, reference)
    }

    /// Resolves a relation qualifier with no column, such as a qualified
    /// wildcard's, against the query's own `FROM` relations.
    ///
    /// Answers `None` when no relation matches.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement},
    ///     dialect::SQLiteDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);")?;
    /// let sql = "SELECT [MIXED].*, [DOCS].* FROM docs AS [MiXeD], docs";
    /// let Statement::Query(query) = Parser::parse_sql(&SQLiteDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Folded)?;
    /// let relations =
    ///     select
    ///         .projection
    ///         .iter()
    ///         .map(|item| {
    ///             let SelectItem::QualifiedWildcard(
    ///                 SelectItemQualifiedWildcardKind::ObjectName(name),
    ///                 _,
    ///             ) = item
    ///             else {
    ///                 unreachable!()
    ///             };
    ///             scope.resolve_relation_binding(name).and_then(|binding| binding.relation())
    ///         })
    ///         .collect::<Vec<_>>();
    /// assert_eq!(relations, [Some(("MiXeD", true)), Some(("docs", false))]);
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn resolve_relation_binding(&self, qualifier: &ObjectName) -> Option<RelationBinding<'_>> {
        self.graph.resolve_relation(self.root, qualifier)
    }

    /// Returns the recorded scope for this exact `Select` of the query.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, SetExpr, Statement},
    ///     dialect::GenericDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")?;
    /// let sql = "SELECT id FROM a UNION ALL SELECT id FROM b";
    /// let Statement::Query(query) = Parser::parse_sql(&GenericDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let SetExpr::SetOperation { left, right, .. } = query.body.as_ref() else { unreachable!() };
    /// for (operand, expected) in [(left, "a"), (right, "b")] {
    ///     let SetExpr::Select(select) = operand.as_ref() else { unreachable!() };
    ///     let operand_scope = scope.scope_for_select(select).expect("the operand scope is recorded");
    ///     let Some(ColumnDefinition::Base { table, .. }) =
    ///         operand_scope.resolve_column_definition(&Expr::Identifier(Ident::new("id")))?
    ///     else {
    ///         unreachable!()
    ///     };
    ///     assert_eq!(table.table_name(), expected);
    ///     let parameter = Expr::Identifier(Ident::new("argument_only"));
    ///     assert!(operand_scope.resolve_column_definition(&parameter)?.is_none());
    /// }
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn scope_for_select(
        &self,
        select: &Select,
    ) -> Option<ColumnDefinitionScope<'_, 'query, 'db, DB>> {
        self.graph.scope_for_select(None, select)
    }

    /// Returns the recorded output scope for this exact query or one nested in
    /// it.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db =
    ///     ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")?;
    /// let sql = "SELECT id FROM a WHERE id = (SELECT id AS x FROM b ORDER BY x LIMIT 1)";
    /// let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
    ///     unreachable!()
    /// };
    /// let scope = ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten)?;
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let Some(Expr::BinaryOp { right, .. }) = &select.selection else { unreachable!() };
    /// let Expr::Subquery(nested) = right.as_ref() else { unreachable!() };
    /// let order = scope.scope_for_query(nested).expect("the nested query is recorded");
    /// let Some(ColumnDefinition::Base { table, .. }) =
    ///     order.resolve_order_by_definition(&Expr::Identifier(Ident::new("x")))?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(table.table_name(), "b");
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn scope_for_query(&self, query: &Query) -> Option<ColumnQueryScope<'_, 'query, 'db, DB>> {
        self.graph.query_id(query).map(|query| ColumnQueryScope { graph: &self.graph, query })
    }
}

impl<'db, DB: DatabaseLike> ColumnScope<'db, 'db, DB> {
    /// Builds the definition scope for one stored table.
    ///
    /// # Examples
    ///
    /// ```
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, Ident},
    ///     dialect::GenericDialect,
    /// };
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE a(id INT);")?;
    /// let table = db
    ///     .table_by_target(TargetName::new("a", false), IdentifierCase::AsWritten)?
    ///     .expect("table a exists");
    /// let scope = ColumnScope::for_table(table, &db, IdentifierCase::AsWritten);
    /// let Some(ColumnDefinition::Base { column, .. }) =
    ///     scope.resolve_column_definition(&Expr::Identifier(Ident::new("id")))?
    /// else {
    ///     unreachable!()
    /// };
    /// assert_eq!(column.column_name(), "id");
    /// assert!(scope.resolve_column_definition(&Expr::Identifier(Ident::new("missing")))?.is_none());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    #[must_use]
    pub fn for_table(table: &'db DB::Table, database: &'db DB, case: IdentifierCase) -> Self {
        let (graph, root) = table_graph(table, database, case);
        Self { graph, root }
    }
}
