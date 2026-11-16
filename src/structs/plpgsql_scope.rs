//! The names a PL/pgSQL body reaches where it evaluates an expression.

use alloc::{format, string::String, vec::Vec};

use sqlparser::ast::{ArgMode, Ident};
use sqlparser_plpgsql::namespace::Namespace;

use crate::{
    errors::LookupError,
    impls::{BodyNames, BodyPath},
    structs::{OperandType, plpgsql_flow::FlowPoint},
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
/// [`ExpressionScope::PlPgSql`](crate::structs::ExpressionScope::PlPgSql) hands
/// it to [`ExpressionContext`](crate::structs::ExpressionContext), which binds
/// a name no relation of the expression's own queries holds to the innermost
/// variable of that name, a name qualified by a block label or by the
/// function's name to that level's variable, and `$n` to argument `n`.
///
/// The scope a site or a definition hands out also stands at its point in
/// the body's value flow, so a field of a `record` takes its type from the
/// rows the definitions reaching that point give the record.
#[derive(Debug)]
pub struct PlPgSqlScope<'b, 'db, DB: DatabaseLike> {
    names: &'b BodyNames<'db, DB>,
    namespace: Namespace,
    path: BodyPath<'b>,
    point: Option<FlowPoint<'b, 'db, DB>>,
}

impl<DB: DatabaseLike> Clone for PlPgSqlScope<'_, '_, DB> {
    fn clone(&self) -> Self {
        Self {
            names: self.names,
            namespace: self.namespace.clone(),
            path: self.path.clone(),
            point: self.point.clone(),
        }
    }
}

impl<'b, 'db, DB: DatabaseLike> PlPgSqlScope<'b, 'db, DB> {
    /// The scope of `namespace` and `path`, standing at no point of the
    /// value flow.
    pub(crate) const fn new(
        names: &'b BodyNames<'db, DB>,
        namespace: Namespace,
        path: BodyPath<'b>,
    ) -> Self {
        Self { names, namespace, path, point: None }
    }

    /// The scope standing at `point` of the value flow.
    pub(crate) fn at(self, point: FlowPoint<'b, 'db, DB>) -> Self {
        Self { point: Some(point), ..self }
    }

    /// The point of the value flow the scope stands at.
    pub(crate) const fn point(&self) -> Option<&FlowPoint<'b, 'db, DB>> {
        self.point.as_ref()
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

    /// Returns the variable PostgreSQL 18 binds the column reference `parts`
    /// writes to, whether or not a column of the same name binds it first,
    /// innermost level first. A bare name reaches a variable of that name,
    /// two parts a variable qualified by a block label or by the function's
    /// name or a field of a row variable, and three parts a field of a row
    /// variable qualified by a label. A scalar variable never reaches a field.
    ///
    /// Under the default `plpgsql.variable_conflict = error`, PostgreSQL 18
    /// refuses as ambiguous a reference that both a column and a variable
    /// reach. [`ExpressionContext::variable`](crate::structs::ExpressionContext::variable)
    /// answers the same for a column reference of a site's context.
    #[must_use]
    pub fn variable(&self, parts: &[Ident]) -> Option<PlPgSqlVariable<'db>> {
        self.lookup(&stored_parts(parts)).map(|(variable, _)| variable)
    }

    /// Returns the row variable PostgreSQL 18 expands the qualified wildcard
    /// `qualifier.*` to, as [`Self::variable`] reaches a name, such as the
    /// `r` of `SELECT r.* FROM notes r` with `r` a record variable.
    #[must_use]
    pub fn wildcard_variable(&self, qualifier: &[Ident]) -> Option<PlPgSqlVariable<'db>> {
        let mut parts = stored_parts(qualifier);
        parts.push(String::from("*"));
        self.lookup(&parts).map(|(variable, _)| variable)
    }

    /// Returns the variable the name `parts` writes reaches, with the number
    /// of parts naming it, one for a bare name or a field of a row variable
    /// and two for a name qualified by a label. PostgreSQL compares the parts
    /// as [`stored`](crate::utils::identifier_resolution) identifiers, and
    /// reads no variable in a name whose parts go past one field.
    pub(crate) fn lookup(&self, parts: &[String]) -> Option<(PlPgSqlVariable<'db>, usize)> {
        let (name, consumed) = self.namespace.lookup(parts, |name| self.names.holds_row(name))?;
        (consumed + 1 >= parts.len()).then(|| (self.names.variable(name), consumed))
    }

    /// The path the body runs the expression under.
    pub(crate) const fn path(&self) -> &BodyPath<'b> {
        &self.path
    }

    /// The database the function belongs to.
    pub(crate) const fn database(&self) -> &'db DB {
        self.names.database()
    }
}
