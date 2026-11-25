//! The places a function body runs SQL text it builds at run time.

use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};
use core::{cell::RefCell, fmt};

use once_cell::race::OnceBox;
use sqlparser::ast::Expr;
use sqlparser_plpgsql::embedded::Block;
use sqlparser_plpgsql_dataflow::values::Analysis;

use crate::{
    errors::LookupError,
    impls::{
        BodyNames, SiteSource, dql::scoped_roots::ScopedRoot, plpgsql_sql_sites, value_analysis,
        value_flow,
    },
    structs::{
        BodyTruth, ExpressionContext, ExpressionScope, IdentifierCase, PlPgSqlScope,
        PlPgSqlVariable,
        plpgsql_flow::{FlowSource, ValueFlow, VariableRead, case_slot, read_variables},
    },
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
        self.source.dynamic_sites()
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
    /// The variables in reach where a PL/pgSQL body evaluates it, standing
    /// at the statement, expression or dynamic SQL of the body holding the
    /// site.
    PlPgSql {
        /// The variables in reach.
        scope: PlPgSqlScope<'b, 'db, DB>,
        /// The value flow of the body.
        flow: Arc<SiteFlow<'b, 'db, DB>>,
    },
    /// The input parameters of the function whose SQL body holds it.
    FunctionBody(&'db DB::Function),
}

impl<DB: DatabaseLike> Clone for SiteScope<'_, '_, DB> {
    fn clone(&self) -> Self {
        match self {
            Self::PlPgSql { scope, flow } => {
                Self::PlPgSql { scope: scope.clone(), flow: Arc::clone(flow) }
            }
            Self::FunctionBody(function) => Self::FunctionBody(function),
        }
    }
}

/// The value flow of one PL/pgSQL body, which the sites of one
/// [`DynamicExecutionSites::sites`] or
/// [`PlPgSqlSites::sites`](crate::structs::PlPgSqlSites::sites) call share
/// and the first read or record field typed computes, and the walk with the
/// value guarantees domain under each identifier case, which the first truth
/// read under that case computes.
pub(crate) struct SiteFlow<'b, 'db, DB: DatabaseLike> {
    names: &'b BodyNames<'db, DB>,
    block: &'b Block,
    hex_strings_may_be_integers: bool,
    flow: OnceBox<Result<ValueFlow<'b, 'db, DB>, LookupError>>,
    analyses: [OnceBox<Result<BodyAnalysis<'b, 'db, DB>, LookupError>>; 3],
}

/// The walk of a body with the value guarantees domain.
pub(crate) type BodyAnalysis<'b, 'db, DB> = RefCell<Analysis<'b, BodyTruth<'b, 'db, DB>>>;

impl<DB: DatabaseLike> fmt::Debug for SiteFlow<'_, '_, DB> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteFlow")
            .field("names", self.names)
            .field("block", self.block)
            .field("flow", &self.flow)
            .finish_non_exhaustive()
    }
}

impl<'b, 'db, DB: DatabaseLike> SiteFlow<'b, 'db, DB> {
    pub(crate) const fn new(
        names: &'b BodyNames<'db, DB>,
        block: &'b Block,
        hex_strings_may_be_integers: bool,
    ) -> Self {
        Self {
            names,
            block,
            hex_strings_may_be_integers,
            flow: OnceBox::new(),
            analyses: [OnceBox::new(), OnceBox::new(), OnceBox::new()],
        }
    }

    /// The value flow of the body, computed on the first call.
    fn get(&self) -> Result<&ValueFlow<'b, 'db, DB>, LookupError> {
        self.flow
            .get_or_init(|| {
                Box::new(value_flow(self.names, self.block, self.hex_strings_may_be_integers))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The walk of the body with the value guarantees domain, its sites
    /// compared under `case`, computed on the first call under `case`.
    ///
    /// # Errors
    ///
    /// Returns the errors
    /// [`PlPgSqlSites::sites`](crate::structs::PlPgSqlSites::sites)
    /// returns, the relation-name lookup errors building the context of a
    /// site runs into, and the lookup errors resolving a reference or a call
    /// the walk reads.
    pub(crate) fn analysis(
        self: &Arc<Self>,
        case: IdentifierCase,
    ) -> Result<&BodyAnalysis<'b, 'db, DB>, LookupError> {
        self.analyses[case_slot(case)]
            .get_or_init(|| {
                let (names, block, hex) =
                    (self.names, self.block, self.hex_strings_may_be_integers);
                let analyzed = plpgsql_sql_sites(names, block, hex, self)
                    .and_then(|sites| BodyTruth::new(names, &sites, case))
                    .and_then(|context| value_analysis(names, block, hex, context));
                Box::new(analyzed.map(RefCell::new))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Each of `variables`, which the SQL of a site standing under `scope`
    /// reads, with the definitions that may reach the site.
    ///
    /// # Errors
    ///
    /// Returns the errors computing the value flow of the body.
    pub(crate) fn reads(
        &self,
        scope: &PlPgSqlScope<'b, 'db, DB>,
        variables: Vec<PlPgSqlVariable<'db>>,
    ) -> Result<Vec<VariableRead<'_, 'db, DB>>, LookupError> {
        let Some(point) = scope.point() else { return Ok(Vec::new()) };
        let state = point.reaching()?.state;
        Ok(self.get()?.reaching(variables, state))
    }
}

impl<DB: DatabaseLike> FlowSource<DB> for SiteFlow<'_, '_, DB> {
    fn flow(&self) -> Result<&ValueFlow<'_, '_, DB>, LookupError> {
        self.get()
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
        scope: SiteScope<'b, 'db, DB>,
        database: &'db DB,
    ) -> Self {
        Self { root: SiteRoot::Command, command, arguments, scope, function: None, database }
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
            SiteScope::PlPgSql { scope, .. } => ExpressionScope::PlPgSql(scope),
            SiteScope::FunctionBody(function) => ExpressionScope::FunctionBody(*function),
        }
    }

    /// Returns the PL/pgSQL variables in reach of a site of a PL/pgSQL body,
    /// and `None` for a site of a SQL body.
    #[must_use]
    pub const fn plpgsql_scope(&self) -> Option<&PlPgSqlScope<'b, 'db, DB>> {
        match &self.scope {
            SiteScope::PlPgSql { scope, .. } => Some(scope),
            SiteScope::FunctionBody(_) => None,
        }
    }

    /// Returns the PL/pgSQL variables the [`command`](Self::command) and
    /// [`arguments`](Self::arguments) read, each once in the order first
    /// read, with the definitions that may reach the site. Identifiers
    /// compare under `case`. A site of a SQL body reads no variable.
    ///
    /// A variable is read whole or through a field, by any name reaching
    /// it, an alias or a label-qualified name included. Its definitions are
    /// what each path from the start of the body to the site may have left
    /// in it, through branches, loops, `EXIT`, `CONTINUE`, `RETURN` and the
    /// errors a handler catches. The value flow of the body is computed once
    /// for every site of one [`DynamicExecutionSites::sites`] call.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors building the contexts of the
    /// command and arguments runs into,
    /// [`LookupError::FunctionBodyUnresolvable`] for a write whose target
    /// names no variable, and the errors [`DynamicExecutionSites::sites`]
    /// returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION run(p text) RETURNS void LANGUAGE plpgsql AS $$
    ///      DECLARE s text := 'SELECT 1';
    ///      BEGIN
    ///          IF p IS NOT NULL THEN
    ///              s := 'SELECT ' || p;
    ///          END IF;
    ///          EXECUTE s;
    ///      END $$;",
    /// )?;
    /// let run = db
    ///     .function_by_target(TargetName::new("run", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = run.dynamic_execution_sites(&db)?;
    /// let sites = body.sites()?;
    /// let reads = sites[0].reads(IdentifierCase::Folded)?;
    /// let [command] = reads.as_slice() else { panic!("s is read") };
    /// assert_eq!(command.variable().name(), "s");
    /// let Reaching::Definitions(definitions) = command.reaching() else { panic!("s is reached") };
    /// let values: Vec<String> = definitions
    ///     .iter()
    ///     .map(|definition| {
    ///         let DefinitionSource::Value(value) = definition.source() else { panic!("assigned") };
    ///         value.to_string()
    ///     })
    ///     .collect();
    /// assert_eq!(values, ["'SELECT 1'", "'SELECT ' || p"]);
    /// let inputs = definitions[1].reads(IdentifierCase::Folded)?;
    /// let [input] = inputs.as_slice() else { panic!("p is read") };
    /// assert!(matches!(input.variable().kind(), VariableKind::Parameter { position: 1, .. }));
    /// let Reaching::Definitions(incoming) = input.reaching() else { panic!("p is reached") };
    /// assert_eq!(incoming[0].source(), &DefinitionSource::Incoming);
    /// # Ok(())
    /// # }
    /// ```
    pub fn reads(
        &self,
        case: IdentifierCase,
    ) -> Result<Vec<VariableRead<'_, 'db, DB>>, LookupError> {
        let SiteScope::PlPgSql { scope, flow } = &self.scope else { return Ok(Vec::new()) };
        let mut variables = Vec::new();
        read_variables(&self.context(case)?, scope, self.command(), &mut variables)?;
        let plpgsql = ExpressionScope::PlPgSql(scope);
        for argument in self.arguments() {
            let context =
                ExpressionContext::for_expression(argument, &plpgsql, self.database, case)?;
            read_variables(&context, scope, argument, &mut variables)?;
        }
        flow.reads(scope, variables)
    }
}
