//! The names a PL/pgSQL body reaches where it evaluates an expression, and the
//! places it runs SQL it builds at run time.

use alloc::{borrow::Cow, format, string::String, vec::Vec};

use sqlparser::ast::{ArgMode, Expr, Ident};
use sqlparser_plpgsql::{embedded::Block, namespace::Namespace};

use crate::{
    errors::LookupError,
    impls::{BodyNames, BodyPath, dynamic_sites},
    structs::{ExpressionScope, OperandType},
    traits::DatabaseLike,
    utils::identifier_resolution::stored_identifier,
};

/// What declares a PL/pgSQL variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VariableKind {
    /// An argument of the function, `$position` counting every argument from
    /// 1, the output ones included, as PL/pgSQL numbers them. A `RETURNS
    /// TABLE` column follows the arguments as an [`ArgMode::Out`] parameter.
    Parameter {
        /// The position `$n` names the argument by.
        position: usize,
        /// The mode the argument is declared with.
        mode: ArgMode,
    },
    /// `$0`, the result of a function returning a polymorphic type through
    /// no output argument, typed as the function's declared result.
    ReturnValue,
    /// `FOUND`, which every function holds.
    Found,
    /// A variable a `DECLARE` section declares.
    Declared {
        /// Whether it is declared `CONSTANT`.
        constant: bool,
        /// Whether it is declared `NOT NULL`.
        not_null: bool,
    },
    /// A cursor a `DECLARE` section binds to a query, a `refcursor`.
    Cursor,
    /// A parameter of a cursor declared with its query, which only that
    /// query reads.
    CursorParameter,
    /// The integer index a `FOR index IN lower .. upper` loop declares for its
    /// body.
    LoopIndex,
    /// The record a `FOR row IN cursor` loop declares for its body.
    LoopRecord,
    /// `SQLSTATE` or `SQLERRM`, which an exception handler declares.
    Handler,
    /// `NEW`, `OLD` or a `TG_` variable, which a trigger function holds.
    Trigger,
}

/// A variable a PL/pgSQL body reaches by name, with the type PostgreSQL 18
/// declares it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlPgSqlVariable<'db> {
    id: usize,
    name: String,
    kind: VariableKind,
    data_type: Result<OperandType<'db>, LookupError>,
}

impl<'db> PlPgSqlVariable<'db> {
    pub(crate) const fn new(
        id: usize,
        name: String,
        kind: VariableKind,
        data_type: Result<OperandType<'db>, LookupError>,
    ) -> Self {
        Self { id, name, kind, data_type }
    }

    /// Returns what tells the variable apart from every other one of the same
    /// function, its position among them in the order the body declares them,
    /// the function's own first.
    ///
    /// Every name reaching the variable answers the same id, an alias
    /// included.
    #[must_use]
    pub const fn id(&self) -> usize {
        self.id
    }

    /// Returns the name the variable is declared under, as PostgreSQL stores
    /// it. An unquoted name folds to lower case, a quoted one stays as
    /// written, and an unnamed argument is `$n`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns what declares the variable.
    #[must_use]
    pub const fn kind(&self) -> &VariableKind {
        &self.kind
    }

    /// Returns the type the variable is declared with.
    ///
    /// A type written as a column's or a row's type, `%TYPE` or `%ROWTYPE`,
    /// is the type the schema gives what it names under the function's path.
    /// A `record` answers `pg_catalog.record`, whose fields only the rows it
    /// is assigned at run time decide.
    ///
    /// # Errors
    ///
    /// Returns the lookup error a `%TYPE` or `%ROWTYPE` naming nothing the
    /// schema or the body holds runs into.
    pub fn data_type(&self) -> Result<&OperandType<'db>, &LookupError> {
        self.data_type.as_ref()
    }
}

/// The names `idents` write, as PostgreSQL stores them.
pub(crate) fn stored_parts<'a>(idents: impl IntoIterator<Item = &'a Ident>) -> Vec<String> {
    idents
        .into_iter()
        .map(|ident| stored_identifier(&ident.value, ident.quote_style.is_some()).into_owned())
        .collect()
}
/// The names a PL/pgSQL expression reaches at the point of the body it sits
/// in, and the `search_path` the body runs it under.
///
/// [`ExpressionScope::PlPgSql`] hands it to
/// [`ExpressionContext`](crate::structs::ExpressionContext), which binds a
/// name no relation of the expression's own queries holds to the innermost
/// variable of that name, a name qualified by a block label or by the
/// function's name to that level's variable, and `$n` to argument `n`.
#[derive(Debug)]
pub struct PlPgSqlScope<'b, 'db, DB: DatabaseLike> {
    names: &'b BodyNames<'db, DB>,
    namespace: Namespace,
    path: BodyPath<'b>,
}

impl<DB: DatabaseLike> Clone for PlPgSqlScope<'_, '_, DB> {
    fn clone(&self) -> Self {
        Self { names: self.names, namespace: self.namespace.clone(), path: self.path.clone() }
    }
}

impl<'b, 'db, DB: DatabaseLike> PlPgSqlScope<'b, 'db, DB> {
    pub(crate) const fn new(
        names: &'b BodyNames<'db, DB>,
        namespace: Namespace,
        path: BodyPath<'b>,
    ) -> Self {
        Self { names, namespace, path }
    }

    /// Returns the function whose body the scope belongs to.
    #[must_use]
    pub const fn function(&self) -> &'db DB::Function {
        self.names.function()
    }

    /// Returns the argument `$position` names.
    #[must_use]
    pub fn parameter(&self, position: usize) -> Option<PlPgSqlVariable<'db>> {
        let (name, _) = self.namespace.lookup(&[format!("${position}")], |_| false)?;
        Some(self.names.variable(name))
    }

    /// Returns the variable the name `parts` writes reaches, innermost level
    /// first, with the number of parts it consumed, one for a bare name and
    /// two for a name qualified by a label. PostgreSQL compares the parts as
    /// [`stored`](crate::utils::identifier_resolution) identifiers.
    pub(crate) fn variable(&self, parts: &[String]) -> Option<(PlPgSqlVariable<'db>, usize)> {
        let (name, consumed) = self.namespace.lookup(parts, |name| self.names.holds_row(name))?;
        Some((self.names.variable(name), consumed))
    }

    /// The path the body runs the expression under.
    pub(crate) const fn path(&self) -> &BodyPath<'b> {
        &self.path
    }
}

/// The dynamic-execution sites of a function body, owning the body they
/// borrow from.
///
/// [`FunctionLike::dynamic_execution_sites`](crate::traits::FunctionLike::dynamic_execution_sites)
/// parses the body into it, and [`Self::sites`] reads the sites.
#[derive(Debug)]
pub struct DynamicExecutionSites<'db, DB: DatabaseLike> {
    names: BodyNames<'db, DB>,
    /// The parsed PL/pgSQL body, `None` for a body in another language.
    block: Option<Block>,
    hex_strings_may_be_integers: bool,
}

impl<'db, DB: DatabaseLike> DynamicExecutionSites<'db, DB> {
    pub(crate) const fn new(
        names: BodyNames<'db, DB>,
        block: Option<Block>,
        hex_strings_may_be_integers: bool,
    ) -> Self {
        Self { names, block, hex_strings_may_be_integers }
    }

    /// Returns every place the body runs SQL it builds at run time, in body
    /// order, as
    /// [`FunctionLike::dynamic_execution_sites`](crate::traits::FunctionLike::dynamic_execution_sites)
    /// describes.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
    /// naming no variable and for an `OPEN` naming no cursor declared with a
    /// query, and the errors reading the path a statement of the body sets.
    pub fn sites(&self) -> Result<Vec<DynamicExecution<'_, 'db, DB>>, LookupError> {
        let Some(block) = &self.block else { return Ok(Vec::new()) };
        dynamic_sites(&self.names, block, self.hex_strings_may_be_integers)
    }
}

/// A place a PL/pgSQL body runs SQL it builds at run time, an `EXECUTE`,
/// `RETURN QUERY EXECUTE`, `OPEN ... FOR EXECUTE` or `FOR ... IN EXECUTE`.
#[derive(Debug, Clone)]
pub struct DynamicExecution<'b, 'db, DB: DatabaseLike> {
    command: Cow<'b, Expr>,
    arguments: Vec<Cow<'b, Expr>>,
    scope: PlPgSqlScope<'b, 'db, DB>,
}

impl<'b, 'db, DB: DatabaseLike> DynamicExecution<'b, 'db, DB> {
    pub(crate) const fn new(
        command: Cow<'b, Expr>,
        arguments: Vec<Cow<'b, Expr>>,
        scope: PlPgSqlScope<'b, 'db, DB>,
    ) -> Self {
        Self { command, arguments, scope }
    }

    /// Returns the expression whose value is the SQL text the site runs.
    #[must_use]
    pub fn command(&self) -> &Expr {
        &self.command
    }

    /// Returns the `USING` expressions, in the order the site writes them,
    /// whose values the SQL text reads as `$1`, `$2` and so on.
    pub fn arguments(&self) -> impl ExactSizeIterator<Item = &Expr> {
        self.arguments.iter().map(|argument| &**argument)
    }

    /// Returns the names the command and arguments reach, as the scope to
    /// build their [`ExpressionContext`](crate::structs::ExpressionContext)
    /// under.
    #[must_use]
    pub const fn scope(&self) -> ExpressionScope<'_, 'db, DB> {
        ExpressionScope::PlPgSql(&self.scope)
    }

    /// Returns the names the command and arguments reach.
    #[must_use]
    pub const fn plpgsql_scope(&self) -> &PlPgSqlScope<'b, 'db, DB> {
        &self.scope
    }
}
