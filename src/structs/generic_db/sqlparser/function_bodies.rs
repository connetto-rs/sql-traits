//! Binding the relations a function body reads, as PostgreSQL does at
//! `CREATE FUNCTION` time.
//!
//! Two body forms bind at creation: a `RETURN <expression>` statement and
//! the statements of a `BEGIN ATOMIC` block. Every relation name in them
//! resolves under the session's `search_path` in effect at creation, exactly
//! as a view's definition does, and a name that does not resolve refuses the
//! statement. A block also binds the target of each `INSERT`, which sits
//! outside any query. A block holding any statement other than a query,
//! `INSERT`, `UPDATE`, `DELETE`, `MERGE` or `RETURN` is refused, as
//! PostgreSQL refuses `CALL`, `SET` and DDL in an unquoted SQL function body.
//! A body written as a string is never bound, and
//! neither is a name a visible `WITH` item answers or a table function.
//!
//! A block's statements are also analyzed the way PostgreSQL analyzes them
//! at creation: a clause reference carrying a schema or a database part is
//! dropped to the parts it keeps when it names a relation the statement's
//! own clauses scope under a matching schema, and refused otherwise, a
//! factor name spelling this catalog's qualifier is dropped, and an
//! `INSERT` offering more expressions than its target has columns is
//! refused.

use alloc::{boxed::Box, string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    ArgMode, CreateFunction, CreateFunctionBody, DataType, Expr, FunctionReturnType, GroupByExpr,
    Insert, ObjectName, OperateFunctionArg, Query, Select, SelectFlavor, SelectItem, SetExpr,
    Statement, TableFactor, TableObject, VisitMut, VisitorMut,
    helpers::attached_token::AttachedToken,
};

use super::{
    ParserDBBuilder, bind_reference,
    views::{relations_read_in, statements_read},
};
use crate::{
    errors::Error,
    utils::{last_str, object_name::RelationKey, with_scopes::WithScopes},
};

/// Binds every stored relation the body of `function` reads, refusing the
/// statement when one of them is not held.
///
/// A `BEGIN ATOMIC` body is also analyzed the way PostgreSQL analyzes it at
/// `CREATE FUNCTION`: a clause reference with a schema or a database part is
/// kept only when it names a relation the statement's own clauses scope
/// under a matching schema, dropped to the parts it keeps, and a reference
/// or a factor name naming another database is refused.
///
/// # Errors
///
/// Returns [`Error::RelationNotFoundForFunctionBody`] naming the first
/// relation the body names that the builder does not hold, the way PostgreSQL
/// refuses the `CREATE FUNCTION`. A `BEGIN ATOMIC` body is also refused with
/// [`Error::FunctionBodyStatementUnsupported`] for a statement kind or a
/// clause shape PostgreSQL does not support in an unquoted SQL function body,
/// with [`Error::FunctionBodyResultMissing`] when it declares a result its
/// final statement produces none, with
/// [`Error::FunctionBodyReferenceAmbiguous`] for a bare column reference two
/// relations in its scope expose, with
/// [`Error::FunctionBodyInsertTooWide`] for an `INSERT` offering more
/// expressions than its target has columns, and with
/// [`Error::IdentifierLookupError`] for a reference or a factor name it will
/// not analyze.
pub(super) fn bind(builder: &ParserDBBuilder, function: &mut CreateFunction) -> Result<(), Error> {
    let Some(body) = function.function_body.as_mut() else {
        return Ok(());
    };
    match body {
        CreateFunctionBody::Return(expression) => {
            let mut binder =
                BodyBinder { builder, bare_temporary: false, scopes: WithScopes::default() };
            match expression.visit(&mut binder) {
                ControlFlow::Continue(()) => Ok(()),
                ControlFlow::Break(missing) => missing_relation(&missing, function),
            }
        }
        CreateFunctionBody::BeginAtomic(block) => {
            super::view_rewrites::normalize_block_references(builder, &mut block.statements)?;
            let function_name = last_str(&function.name).to_string();
            let mut binder =
                BodyBinder { builder, bare_temporary: true, scopes: WithScopes::default() };
            for statement in &mut block.statements {
                if !matches!(
                    statement,
                    Statement::Query(_)
                        | Statement::Insert(_)
                        | Statement::Update(_)
                        | Statement::Delete(_)
                        | Statement::Merge(_)
                        | Statement::Return(_)
                ) {
                    return Err(Error::FunctionBodyStatementUnsupported {
                        statement: statement.to_string(),
                        function_name,
                    });
                }
                super::view_rewrites::shape_of_block_statement(statement, &function_name)?;
                super::view_rewrites::refuse_ambiguous_block_reference(
                    builder,
                    statement,
                    &function_name,
                )?;
                super::view_rewrites::spell_block_statement(builder, statement, &function_name)?;
                match statement.visit(&mut binder) {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(missing) => return missing_relation(&missing, function),
                }
            }
            let declares_result =
                function_declares_result(function.args.as_deref(), function.return_type.as_ref());
            if declares_result
                && (block.statements.is_empty()
                    || block.statements.last().is_some_and(final_statement_produces_no_result))
            {
                return Err(Error::FunctionBodyResultMissing { function_name });
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Whether the function declares a result, which is any `RETURNS` other
/// than `void` and any `OUT` or `INOUT` argument.
fn function_declares_result(
    args: Option<&[OperateFunctionArg]>,
    return_type: Option<&FunctionReturnType>,
) -> bool {
    if args.is_some_and(|args| {
        args.iter().any(|arg| matches!(arg.mode, Some(ArgMode::Out | ArgMode::InOut)))
    }) {
        return true;
    }
    match return_type {
        Some(FunctionReturnType::DataType(data_type)) => !is_void(data_type),
        Some(FunctionReturnType::SetOf(_)) => true,
        None => false,
    }
}

/// Whether a declared return type is `void`, which a body writes no result
/// for.
fn is_void(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Custom(name, segments)
            if segments.is_empty() && name.to_string().eq_ignore_ascii_case("void")
    )
}

/// Whether a block's final statement produces no result, which is a
/// data-modifying statement without `RETURNING`, written directly or under
/// its `WITH` clause.
fn final_statement_produces_no_result(statement: &Statement) -> bool {
    match statement {
        Statement::Query(query) => dml_without_returning(query.body.as_ref()),
        _ => {
            super::view_rewrites::statement_dml(statement)
                .is_some_and(|dml| returning_is_empty(dml.returning()))
        }
    }
}

/// Whether a query body is a data-modifying statement without `RETURNING`.
fn dml_without_returning(body: &SetExpr) -> bool {
    super::view_rewrites::dml_ref(body).is_some_and(|dml| returning_is_empty(dml.returning()))
}

/// Whether a data-modifying statement's `RETURNING` list is absent or empty.
fn returning_is_empty(returning: Option<&[SelectItem]>) -> bool {
    returning.is_none_or(<[SelectItem]>::is_empty)
}

/// The refusal a body's unresolvable relation names.
fn missing_relation(missing: &ObjectName, function: &CreateFunction) -> Result<(), Error> {
    Err(Error::RelationNotFoundForFunctionBody {
        relation_name: missing.to_string(),
        function_name: last_str(&function.name).to_string(),
    })
}

/// The query a `RETURN` body's expression is wrapped in, the shape the
/// rename machinery rewrites and checks.
pub(super) fn definition(expr: &Expr) -> Query {
    Query {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(body_select(expr)))),
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: Vec::new(),
    }
}

/// The select a wrapper query holds around a body expression: one projection
/// item, no `FROM`, nothing else to resolve.
fn body_select(expr: &Expr) -> Select {
    Select {
        select_token: AttachedToken::empty(),
        optimizer_hints: Vec::new(),
        distinct: None,
        select_modifiers: None,
        top: None,
        top_before_distinct: false,
        projection: alloc::vec![SelectItem::UnnamedExpr(expr.clone())],
        exclude: None,
        into: None,
        from: Vec::new(),
        lateral_views: Vec::new(),
        prewhere: None,
        selection: None,
        connect_by: Vec::new(),
        group_by: GroupByExpr::Expressions(Vec::new(), Vec::new()),
        cluster_by: Vec::new(),
        distribute_by: Vec::new(),
        sort_by: Vec::new(),
        having: None,
        named_window: Vec::new(),
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        flavor: SelectFlavor::Standard,
    }
}

/// Writes a rewritten body back into `body`, in the variant it was read from.
pub(super) fn write_definition(body: &mut Option<CreateFunctionBody>, query: Query) {
    // `definition` built `query`, so the shape below always matches.
    if let Some(CreateFunctionBody::Return(expr)) = body
        && let SetExpr::Select(select) = *query.body
        && let Some(SelectItem::UnnamedExpr(rewritten)) = select.projection.into_iter().next()
    {
        *expr = rewritten;
    }
}

/// Writes the respelled statements of a `BEGIN ATOMIC` body back into `body`.
pub(super) fn write_statements(body: &mut Option<CreateFunctionBody>, statements: Vec<Statement>) {
    if let Some(CreateFunctionBody::BeginAtomic(block)) = body {
        block.statements = statements;
    }
}

/// The normalized identity of every relation a function body reads or
/// writes, or [`None`] for a body PostgreSQL does not bind.
pub(super) fn relations_read(
    builder: &ParserDBBuilder,
    body: &CreateFunctionBody,
) -> Option<Vec<RelationKey>> {
    match body {
        CreateFunctionBody::Return(expr) => Some(relations_read_in(&definition(expr))),
        CreateFunctionBody::BeginAtomic(block) => Some(statements_read(builder, &block.statements)),
        _ => None,
    }
}

/// Binds every stored relation a function body reads, stopping at the first
/// one the builder does not hold.
struct BodyBinder<'builder> {
    builder: &'builder ParserDBBuilder,
    /// A `BEGIN ATOMIC` body drops the temporary schema a name binds under,
    /// the way PostgreSQL stores it, while a `RETURN` body keeps it.
    bare_temporary: bool,
    scopes: WithScopes,
}

impl BodyBinder<'_> {
    /// Binds one factor's name, refusing it when no stored relation answers.
    fn bind_factor_name(&mut self, name: &mut ObjectName) -> ControlFlow<ObjectName> {
        if !self.scopes.reads_item(name) {
            let Some(bound) = self.builder.bound_relation(name) else {
                return ControlFlow::Break(name.clone());
            };
            if bound.temporary && self.bare_temporary {
                name.0.drain(..name.0.len() - 1);
            } else {
                bind_reference(name, bound.qualifier);
            }
        }
        ControlFlow::Continue(())
    }
}

impl VisitorMut for BodyBinder<'_> {
    type Break = ObjectName;

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.leave(query);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args: None, .. } = factor {
            return self.bind_factor_name(name);
        }
        ControlFlow::Continue(())
    }

    /// The target of an `INSERT` sits outside any query, where the factor
    /// hook does not reach, and it must name a stored relation, which a
    /// `WITH` item's name does not.
    fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
        if let Statement::Insert(Insert { table: TableObject::TableName(name), .. }) = statement {
            if self.scopes.reads_item(name) {
                return ControlFlow::Break(name.clone());
            }
            return self.bind_factor_name(name);
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{ast::Ident, dialect::PostgreSqlDialect};

    use super::*;
    use crate::{
        prelude::ParserDB,
        structs::{IdentifierCase, TargetName},
        traits::DatabaseLike,
    };

    /// The rename machinery stores a rewritten wrapper into a function slot,
    /// which must never turn a string body into a `RETURN` one.
    #[test]
    fn a_rewrite_leaves_a_string_body_as_written() {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
        )
        .expect("the schema parses");
        let function = db
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the function exists");
        let mut string = function.function_body.clone();
        write_definition(&mut string, definition(&Expr::Identifier(Ident::new("x"))));
        assert_eq!(string, function.function_body);
    }

    /// Respelling a statement body must never turn an expression body into a
    /// `BEGIN ATOMIC` one.
    #[test]
    fn a_rewrite_leaves_an_expression_body_as_written() {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql RETURN 1;",
        )
        .expect("the schema parses");
        let function = db
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the function exists");
        let mut body = function.function_body.clone();
        write_statements(&mut body, Vec::new());
        assert_eq!(body, function.function_body);
    }
}
