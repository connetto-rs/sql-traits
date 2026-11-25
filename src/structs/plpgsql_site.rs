//! The SQL a PL/pgSQL body runs, each piece with the names it reaches.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::Expr;
use sqlparser_plpgsql::{embedded::Case, namespace::Sql};

use crate::{
    errors::LookupError,
    impls::{SiteSource, dql::scoped_roots::ScopedRoot},
    structs::{
        EmbeddedSql, ExpressionContext, ExpressionScope, ExpressionTruth, IdentifierCase,
        PlPgSqlScope, SiteFlow,
        plpgsql_flow::{VariableRead, read_variables},
    },
    traits::DatabaseLike,
};

/// The statements and expressions of a PL/pgSQL body, owning the body they
/// borrow from.
///
/// [`FunctionLike::plpgsql_sites`](crate::traits::FunctionLike::plpgsql_sites)
/// parses the body into it, and [`Self::sites`] reads them.
#[derive(Debug)]
pub struct PlPgSqlSites<'db, DB: DatabaseLike> {
    source: SiteSource<'db, DB>,
}

impl<'db, DB: DatabaseLike> PlPgSqlSites<'db, DB> {
    pub(crate) const fn new(source: SiteSource<'db, DB>) -> Self {
        Self { source }
    }

    /// Returns every statement and expression the body runs, in body order,
    /// as [`FunctionLike::plpgsql_sites`](crate::traits::FunctionLike::plpgsql_sites)
    /// describes.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
    /// naming no variable and for an `OPEN` naming no cursor declared with a
    /// query, and the errors reading the path a statement of the body sets.
    pub fn sites(&self) -> Result<Vec<PlPgSqlSite<'_, 'db, DB>>, LookupError> {
        self.source.plpgsql_sites()
    }

    /// The parsed PL/pgSQL block the sites borrow from, `None` for a body
    /// in another language.
    pub(crate) const fn block(&self) -> Option<&sqlparser_plpgsql::embedded::Block> {
        self.source.block()
    }
}

/// What a PL/pgSQL body does with the statement or expression of a
/// [`PlPgSqlSite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlPgSqlSiteRole {
    /// A statement the body runs.
    Statement,
    /// The condition of an `IF`.
    IfCondition {
        /// Whether an `ELSIF` or an `ELSE` holding a statement follows it.
        followed: bool,
    },
    /// The condition of an `ELSIF`.
    ElsifCondition {
        /// Whether another `ELSIF` or an `ELSE` holding a statement follows
        /// it.
        followed: bool,
    },
    /// The subject of a simple `CASE`.
    CaseSubject,
    /// The values a `WHEN` of a simple `CASE` compares the subject with.
    CaseValue,
    /// The condition of a `WHEN` of a searched `CASE`.
    CaseWhen {
        /// Whether another `WHEN` or an `ELSE` follows it.
        followed: bool,
    },
    /// The condition of a `WHILE` loop.
    WhileCondition,
    /// The `WHEN` condition of an `EXIT`.
    ExitWhen,
    /// The `WHEN` condition of a `CONTINUE`.
    ContinueWhen,
    /// The condition of an `ASSERT`.
    AssertCondition,
    /// The value of an assignment.
    Assigned,
    /// The default of a declared variable.
    Default,
    /// Any other expression: a returned value, a loop bound, an argument of
    /// `RAISE`, a cursor argument, the command an `EXECUTE` runs or one of
    /// its `USING` arguments, among others.
    Expression,
}

impl PlPgSqlSiteRole {
    /// Whether the body reads the site as a condition, converted to
    /// `boolean` as PL/pgSQL converts it.
    #[must_use]
    pub const fn is_condition(self) -> bool {
        matches!(
            self,
            Self::IfCondition { .. }
                | Self::ElsifCondition { .. }
                | Self::CaseWhen { .. }
                | Self::WhileCondition
                | Self::ExitWhen
                | Self::ContinueWhen
                | Self::AssertCondition
        )
    }
}

/// A statement or expression a PL/pgSQL body runs, with the variables in
/// reach where PostgreSQL 18 compiles it and the path it runs under.
#[derive(Debug, Clone)]
pub struct PlPgSqlSite<'b, 'db, DB: DatabaseLike> {
    sql: EmbeddedSql<'b>,
    /// The statement, expression or dynamic SQL of the body the site stands
    /// at.
    at: Sql<'b>,
    role: PlPgSqlSiteRole,
    /// The `CASE` without `ELSE` whose end the site stands for.
    case: Option<&'b Case>,
    scope: PlPgSqlScope<'b, 'db, DB>,
    flow: Arc<SiteFlow<'b, 'db, DB>>,
    database: &'db DB,
}

impl<'b, 'db, DB: DatabaseLike> PlPgSqlSite<'b, 'db, DB> {
    pub(crate) const fn new(
        sql: EmbeddedSql<'b>,
        at: Sql<'b>,
        (role, case): (PlPgSqlSiteRole, Option<&'b Case>),
        scope: PlPgSqlScope<'b, 'db, DB>,
        flow: Arc<SiteFlow<'b, 'db, DB>>,
        database: &'db DB,
    ) -> Self {
        Self { sql, at, role, case, scope, flow, database }
    }

    /// Returns what the body does with the [`sql`](Self::sql).
    #[must_use]
    pub const fn role(&self) -> PlPgSqlSiteRole {
        self.role
    }

    /// Returns the SQL, a statement of the body or the `SELECT` evaluating
    /// one of its expressions.
    #[must_use]
    pub const fn sql(&self) -> EmbeddedSql<'b> {
        self.sql
    }

    /// Returns the context typing the [`sql`](Self::sql), its column
    /// references bound as [`ExpressionScope::PlPgSql`] describes.
    /// Identifiers compare under `case`.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors resolving the SQL runs into.
    pub fn context(
        &self,
        case: IdentifierCase,
    ) -> Result<ExpressionContext<'_, 'db, DB>, LookupError> {
        let root = match self.sql {
            EmbeddedSql::Statement(statement) => ScopedRoot::Statement(statement),
            EmbeddedSql::Expression(query) => ScopedRoot::Query(query),
        };
        ExpressionContext::for_root(root, &self.scope(), self.database, case)
    }

    /// Returns the variables in reach of the SQL, as an expression scope.
    #[must_use]
    pub const fn scope(&self) -> ExpressionScope<'_, 'db, DB> {
        ExpressionScope::PlPgSql(&self.scope)
    }

    /// Returns the variables in reach of the SQL.
    #[must_use]
    pub const fn plpgsql_scope(&self) -> &PlPgSqlScope<'b, 'db, DB> {
        &self.scope
    }

    /// Returns the variables the [`sql`](Self::sql) reads, each once in the
    /// order first read, with the definitions that may reach the site.
    /// Identifiers compare under `case`.
    ///
    /// A variable is read whole or through a field, by any name reaching
    /// it, an alias or a label-qualified name included, and not where a
    /// column of the same name binds the reference. Its definitions are what
    /// each path from the start of the body to the site may have left in
    /// it, through branches, loops, `EXIT`, `CONTINUE`, `RETURN` and the
    /// errors a handler catches. The value flow of the body is computed once
    /// for every site of one [`PlPgSqlSites::sites`] call.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors building the
    /// [`context`](Self::context) runs into,
    /// [`LookupError::FunctionBodyUnresolvable`] for a write whose target
    /// names no variable, and the errors [`PlPgSqlSites::sites`] returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE team_members (team_id INT, user_id INT);
    ///      CREATE FUNCTION can_edit(doc_team INT) RETURNS BOOLEAN LANGUAGE plpgsql STABLE AS $$
    ///      DECLARE me INT := current_setting('app.user_id')::int; teams INT[];
    ///      BEGIN
    ///          IF doc_team < 0 THEN
    ///              me := doc_team;
    ///          END IF;
    ///          teams := ARRAY(SELECT team_id FROM team_members WHERE user_id = me);
    ///          RETURN doc_team = ANY (teams);
    ///      END $$;",
    /// )?;
    /// let can_edit = db
    ///     .function_by_target(TargetName::new("can_edit", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = can_edit.plpgsql_sites(&db)?;
    /// let sites = body.sites()?;
    /// let reads = sites[3].reads(IdentifierCase::Folded)?;
    /// let [me] = reads.as_slice() else { panic!("me is read") };
    /// assert_eq!(me.variable().name(), "me");
    /// let Reaching::Definitions(definitions) = me.reaching() else { panic!("me is reached") };
    /// let values: Vec<String> = definitions
    ///     .iter()
    ///     .map(|definition| {
    ///         let DefinitionSource::Value(value) = definition.source() else { panic!("assigned") };
    ///         value.to_string()
    ///     })
    ///     .collect();
    /// assert_eq!(values, ["current_setting('app.user_id')::INT", "doc_team"]);
    /// let inputs = definitions[1].reads(IdentifierCase::Folded)?;
    /// let [argument] = inputs.as_slice() else { panic!("doc_team is read") };
    /// assert!(matches!(argument.variable().kind(), VariableKind::Parameter { position: 1, .. }));
    ///
    /// let returned = sites[4].reads(IdentifierCase::Folded)?;
    /// let names: Vec<&str> = returned.iter().map(|read| read.variable().name()).collect();
    /// assert_eq!(names, ["doc_team", "teams"]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn reads(
        &self,
        case: IdentifierCase,
    ) -> Result<Vec<VariableRead<'_, 'db, DB>>, LookupError> {
        let mut variables = Vec::new();
        read_variables(&self.context(case)?, &self.scope, &self.sql, &mut variables)?;
        self.flow.reads(&self.scope, variables)
    }

    /// Returns whether a path from the start of the body reaches the site.
    /// Identifiers compare under `case`.
    ///
    /// The walk follows the body as PostgreSQL 18 runs it, under what each
    /// variable may hold, so a branch whose condition is never true there is
    /// not taken. It runs once per body and `case`, for every site of one
    /// [`PlPgSqlSites::sites`] call.
    ///
    /// # Errors
    ///
    /// Returns the errors [`PlPgSqlSites::sites`] returns, the
    /// relation-name lookup errors building the [`context`](Self::context)
    /// of a site of the body runs into, and the lookup errors resolving a
    /// reference or a call the body evaluates.
    pub fn reached(&self, case: IdentifierCase) -> Result<bool, LookupError> {
        Ok(self.flow.analysis(case)?.borrow().reached(self.at))
    }

    /// Returns what `expression`, a node of the [`context`](Self::context),
    /// evaluates to where the body evaluates the site, under what each
    /// variable may hold there, and `None` when no path reaches the site.
    /// Identifiers compare under `case`.
    ///
    /// A variable holds what the paths reaching the site leave in it: its
    /// declared type, what the body assigned, the conditions taken to get
    /// there and, as the body starts, what
    /// [`PlPgSqlScope`]'s variables hold on entry: a parameter of a strict
    /// function a value, an `OUT` parameter `NULL`, and `NEW` and `OLD` the
    /// rows the triggers executing the function fire with. Any other name
    /// reads as [`ExpressionContext::truth_under`] reads it.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, and the errors [`Self::reached`] returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{SelectItem, SetExpr},
    ///     dialect::PostgreSqlDialect,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION sign_of(n integer) RETURNS text LANGUAGE plpgsql AS $$
    ///      BEGIN
    ///          IF n IS NULL THEN
    ///              RETURN n > 3;
    ///          END IF;
    ///          RETURN 'set';
    ///      END $$;",
    /// )?;
    /// let sign_of = db
    ///     .function_by_target(TargetName::new("sign_of", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = sign_of.plpgsql_sites(&db)?;
    /// let sites = body.sites()?;
    /// let returned = &sites[1];
    /// let EmbeddedSql::Expression(query) = returned.sql() else { panic!("RETURN n > 3") };
    /// let SetExpr::Select(select) = &*query.body else { panic!("a SELECT") };
    /// let SelectItem::UnnamedExpr(compared) = &select.projection[0] else { panic!("n > 3") };
    /// let truth = returned.truth_under(compared, IdentifierCase::Folded)?;
    /// assert_eq!(truth, Some(ExpressionTruth { outcomes: Outcomes::NULL, raising: Raising::Never }));
    /// # Ok(())
    /// # }
    /// ```
    pub fn truth_under(
        &self,
        expression: &Expr,
        case: IdentifierCase,
    ) -> Result<Option<ExpressionTruth>, LookupError> {
        let context = self.context(case)?;
        let mut analysis = self.flow.analysis(case)?.borrow_mut();
        analysis.context_mut().bind(&context, expression)?;
        Ok(analysis.truth_under(self.at, expression)?.as_ref().map(ExpressionTruth::from))
    }

    /// Returns what the condition the site evaluates is where the body
    /// evaluates it, converted to `boolean` as PL/pgSQL converts it, which
    /// tells whether it is always true, never true or always raising there.
    /// `None` when no path reaches the site and for a site whose
    /// [`role`](Self::role) is no condition. Identifiers compare under
    /// `case`.
    ///
    /// # Errors
    ///
    /// Returns the errors [`Self::reached`] returns.
    pub fn condition(&self, case: IdentifierCase) -> Result<Option<ExpressionTruth>, LookupError> {
        let (true, EmbeddedSql::Expression(query)) = (self.role.is_condition(), self.sql) else {
            return Ok(None);
        };
        let mut analysis = self.flow.analysis(case)?.borrow_mut();
        Ok(analysis.condition(query)?.as_ref().map(ExpressionTruth::from))
    }

    /// Returns, for the subject of a simple `CASE` without `ELSE` and the
    /// first `WHEN` of a searched one, whether a path reaches the end of the
    /// `CASE` with no `WHEN` matched, where PostgreSQL raises
    /// `case_not_found`, and `None` for any other site. Identifiers compare
    /// under `case`.
    ///
    /// # Errors
    ///
    /// Returns the errors [`Self::reached`] returns.
    pub fn unmatched(&self, case: IdentifierCase) -> Result<Option<bool>, LookupError> {
        let Some(statement) = self.case else { return Ok(None) };
        Ok(Some(self.flow.analysis(case)?.borrow().unmatched(statement)))
    }
}
