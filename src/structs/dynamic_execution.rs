//! The places a function body runs SQL text it builds at run time.

use alloc::{borrow::Cow, vec::Vec};

use sqlparser::ast::Expr;

use crate::{
    errors::LookupError,
    impls::{SiteSource, dql::scoped_roots::ScopedRoot},
    structs::{ExpressionContext, ExpressionScope, IdentifierCase, PlPgSqlScope},
    traits::DatabaseLike,
};

/// The dynamic-execution sites of a function body, owning the body they
/// borrow from.
///
/// [`FunctionLike::dynamic_execution_sites`](crate::traits::FunctionLike::dynamic_execution_sites)
/// parses the body into it, and [`Self::sites`] reads the sites.
#[derive(Debug)]
pub struct DynamicExecutionSites<'db, DB: DatabaseLike> {
    source: SiteSource<'db, DB>,
}

impl<'db, DB: DatabaseLike> DynamicExecutionSites<'db, DB> {
    pub(crate) const fn new(source: SiteSource<'db, DB>) -> Self {
        Self { source }
    }

    /// Returns every place the body runs SQL text, in body order, as
    /// [`FunctionLike::dynamic_execution_sites`](crate::traits::FunctionLike::dynamic_execution_sites)
    /// describes.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
    /// naming no variable and for an `OPEN` naming no cursor declared with a
    /// query, the errors reading the path a statement of the body sets, and
    /// the lookup errors resolving a call of one of the functions running
    /// SQL text.
    pub fn sites(&self) -> Result<Vec<DynamicExecution<'_, 'db, DB>>, LookupError> {
        self.source.sites()
    }
}

/// The SQL a site's [`ExpressionContext`] is built over.
#[derive(Debug, Clone, Copy)]
enum SiteRoot<'b> {
    /// The command itself, the expression an `EXECUTE`-family statement
    /// evaluates.
    Command,
    /// The statement or expression of the body holding a call.
    Holding(ScopedRoot<'b>),
}

/// The names a site's SQL reaches besides its own relations.
#[derive(Debug)]
pub(crate) enum SiteScope<'b, 'db, DB: DatabaseLike> {
    /// The variables in reach where a PL/pgSQL body evaluates it.
    PlPgSql(PlPgSqlScope<'b, 'db, DB>),
    /// The input parameters of the function whose SQL body holds it.
    FunctionBody(&'db DB::Function),
}

impl<DB: DatabaseLike> Clone for SiteScope<'_, '_, DB> {
    fn clone(&self) -> Self {
        match self {
            Self::PlPgSql(scope) => Self::PlPgSql(scope.clone()),
            Self::FunctionBody(function) => Self::FunctionBody(function),
        }
    }
}

/// A place a function body runs SQL text, an `EXECUTE`,
/// `RETURN QUERY EXECUTE`, `OPEN ... FOR EXECUTE` or `FOR ... IN EXECUTE`
/// of a PL/pgSQL body or a call of a `pg_catalog` function running the text
/// one of its arguments gives.
#[derive(Debug, Clone)]
pub struct DynamicExecution<'b, 'db, DB: DatabaseLike> {
    root: SiteRoot<'b>,
    command: Cow<'b, Expr>,
    arguments: Vec<Cow<'b, Expr>>,
    scope: SiteScope<'b, 'db, DB>,
    function: Option<&'db DB::Function>,
    database: &'db DB,
}

impl<'b, 'db, DB: DatabaseLike> DynamicExecution<'b, 'db, DB> {
    /// The `EXECUTE`-family site evaluating `command` with the `USING`
    /// `arguments` under `scope`.
    pub(crate) const fn execute(
        command: Cow<'b, Expr>,
        arguments: Vec<Cow<'b, Expr>>,
        scope: PlPgSqlScope<'b, 'db, DB>,
        database: &'db DB,
    ) -> Self {
        Self {
            root: SiteRoot::Command,
            command,
            arguments,
            scope: SiteScope::PlPgSql(scope),
            function: None,
            database,
        }
    }

    /// The call of `function`, written in `root`, running the text its
    /// argument `command` gives.
    pub(crate) const fn call(
        root: ScopedRoot<'b>,
        command: &'b Expr,
        scope: SiteScope<'b, 'db, DB>,
        function: &'db DB::Function,
        database: &'db DB,
    ) -> Self {
        Self {
            root: SiteRoot::Holding(root),
            command: Cow::Borrowed(command),
            arguments: Vec::new(),
            scope,
            function: Some(function),
            database,
        }
    }

    /// Returns the expression whose value is the SQL text the site runs, a
    /// node of the [`context`](Self::context).
    #[must_use]
    pub fn command(&self) -> &Expr {
        &self.command
    }

    /// Returns the `USING` expressions of an `EXECUTE`-family site, in the
    /// order the site writes them, whose values the SQL text reads as `$1`,
    /// `$2` and so on, and none for a call. Each is evaluated by itself, so
    /// [`ExpressionContext::for_expression`] types it under
    /// [`scope`](Self::scope).
    pub fn arguments(&self) -> impl ExactSizeIterator<Item = &Expr> {
        self.arguments.iter().map(|argument| &**argument)
    }

    /// Returns the `pg_catalog` function a call site runs the text with, and
    /// `None` for an `EXECUTE`-family site.
    #[must_use]
    pub const fn function(&self) -> Option<&'db DB::Function> {
        self.function
    }

    /// Returns the context typing the [`command`](Self::command), built over
    /// the statement or expression of the body holding it, so a column of
    /// an enclosing query binds as PostgreSQL binds it, ahead of a variable
    /// or parameter of the same name. Identifiers compare under `case`.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors resolving the statement or
    /// expression holding the command runs into.
    pub fn context(
        &self,
        case: IdentifierCase,
    ) -> Result<ExpressionContext<'_, 'db, DB>, LookupError> {
        let root = match self.root {
            SiteRoot::Command => ScopedRoot::Expression(&self.command),
            SiteRoot::Holding(root) => root,
        };
        ExpressionContext::for_root(root, &self.scope(), self.database, case)
    }

    /// Returns the names the SQL holding the site reaches besides its own
    /// relations, the PL/pgSQL variables in reach where the body evaluates
    /// it or the input parameters of a function with a SQL body.
    #[must_use]
    pub const fn scope(&self) -> ExpressionScope<'_, 'db, DB> {
        match &self.scope {
            SiteScope::PlPgSql(scope) => ExpressionScope::PlPgSql(scope),
            SiteScope::FunctionBody(function) => ExpressionScope::FunctionBody(*function),
        }
    }

    /// Returns the PL/pgSQL variables in reach of a site of a PL/pgSQL body,
    /// and `None` for a site of a SQL body.
    #[must_use]
    pub const fn plpgsql_scope(&self) -> Option<&PlPgSqlScope<'b, 'db, DB>> {
        match &self.scope {
            SiteScope::PlPgSql(scope) => Some(scope),
            SiteScope::FunctionBody(_) => None,
        }
    }
}
