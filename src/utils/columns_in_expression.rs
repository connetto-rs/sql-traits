//! Functions to extract columns from SQL expressions.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{
    AccessExpr, Expr, FunctionArg, FunctionArguments, Ident, InTarget, Query, Visitor,
};

use crate::{
    structs::{IdentifierCase, TargetName},
    traits::column::ColumnLike,
    utils::walk::walk,
};

/// Whether a written reference names a stored column under `case`.
fn column_named_by(name: &str, quoted: bool, written: &Ident, case: IdentifierCase) -> bool {
    case.identifiers_match(name, quoted, written.value.as_str(), written.quote_style.is_some())
}

/// Returns whether a column reference's qualifier chain names the table the
/// expression belongs to, comparing each part under `case`.
///
/// A reference may name the table, its schema and this catalog, in that
/// order outwards, which is what a server accepts inside a check constraint.
/// Anything else names another table, and a table recorded without a schema
/// resides in the default one, so `public` reaches it.
fn qualifier_names_table(
    qualifier: &[Ident],
    catalog_name: &str,
    table: &TargetName<'_>,
    case: IdentifierCase,
) -> bool {
    let matches = |ident: &Ident, value: &str, quoted: bool| {
        case.identifiers_match(ident.value.as_str(), ident.quote_style.is_some(), value, quoted)
    };
    let names_table = |ident: &Ident| matches(ident, table.name(), table.name_is_quoted());
    let names_schema = |ident: &Ident| {
        match table.schema() {
            Some(schema) => matches(ident, schema, table.schema_is_quoted()),
            None => matches(ident, "public", false),
        }
    };

    match qualifier {
        [] => true,
        [table_part] => names_table(table_part),
        [schema_part, table_part] => names_schema(schema_part) && names_table(table_part),
        [catalog_part, schema_part, table_part] => {
            matches(catalog_part, catalog_name, false)
                && names_schema(schema_part)
                && names_table(table_part)
        }
        _ => false,
    }
}

/// The labels among `arguments`, which name a parameter rather than read a
/// value.
fn argument_labels(arguments: &[FunctionArg]) -> impl Iterator<Item = &Expr> {
    arguments.iter().filter_map(|argument| {
        match argument {
            FunctionArg::ExprNamed { name, .. } => Some(name),
            FunctionArg::Named { .. } | FunctionArg::Unnamed(_) => None,
        }
    })
}

/// The expressions a walk has met standing in a name position, a named
/// argument's label or a field after a dot, which read no column.
#[derive(Default)]
pub(crate) struct Labels(Vec<*const Expr>);

impl Labels {
    /// Records the labels `expr` holds, before the walk reaches them.
    pub(crate) fn enter(&mut self, expr: &Expr) {
        match expr {
            Expr::Function(function) => {
                for arguments in [&function.parameters, &function.args] {
                    if let FunctionArguments::List(list) = arguments {
                        self.0.extend(argument_labels(&list.args).map(core::ptr::from_ref));
                    }
                }
            }
            Expr::In { target: InTarget::Table { args: Some(arguments), .. }, .. } => {
                self.0.extend(argument_labels(arguments).map(core::ptr::from_ref));
            }
            Expr::CompoundFieldAccess { access_chain, .. } => {
                self.0.extend(access_chain.iter().filter_map(|access| {
                    match access {
                        AccessExpr::Dot(field) => Some(core::ptr::from_ref(field)),
                        AccessExpr::Subscript(_) => None,
                    }
                }));
            }
            _ => {}
        }
    }

    /// Whether `expr` stands in a name position recorded earlier.
    pub(crate) fn holds(&self, expr: &Expr) -> bool {
        self.0.iter().any(|label| core::ptr::eq(*label, expr))
    }
}

/// Resolves the column references visited in an expression.
struct ColumnsRead<'a, 't, Resolve> {
    dialect: crate::impls::SqlparserDialect,
    catalog_name: &'a str,
    table: &'a TargetName<'t>,
    resolve: Resolve,
    /// The comparison every written name is read under.
    case: IdentifierCase,
    /// How many subqueries enclose the visited expression, which read the
    /// tables of their own `FROM` rather than this one.
    subqueries: usize,
    /// The expressions standing in a name position, which read no column.
    labels: Labels,
    /// The parameters of the lambdas enclosing the visited expression, which
    /// hide a column of the same name.
    parameters: Vec<Ident>,
}

impl<Resolve: FnMut(&Ident) -> bool> ColumnsRead<'_, '_, Resolve> {
    fn is_parameter(&self, ident: &Ident) -> bool {
        self.parameters.iter().any(|parameter| {
            self.case.identifiers_match(
                parameter.value.as_str(),
                parameter.quote_style.is_some(),
                ident.value.as_str(),
                ident.quote_style.is_some(),
            )
        })
    }

    /// Records the column `ident` names, `reference` being the whole
    /// reference as written.
    fn column(&mut self, ident: &Ident, reference: &Expr) -> Result<(), crate::errors::Error> {
        if !(self.resolve)(ident) {
            let parts = match reference {
                Expr::Identifier(ident) => core::slice::from_ref(ident),
                Expr::CompoundIdentifier(parts) => parts.as_slice(),
                _ => &[],
            };
            return Err(crate::errors::Error::UnknownColumn {
                reference: crate::errors::ColumnReference::unresolved(
                    self.dialect,
                    parts,
                    self.catalog_name,
                    true,
                    false,
                ),
            });
        }
        Ok(())
    }

    fn reference(&mut self, expr: &Expr) -> Result<(), crate::errors::Error> {
        match expr {
            Expr::Identifier(ident) if !self.is_parameter(ident) => self.column(ident, expr),
            Expr::CompoundIdentifier(idents) => {
                let Some((column, qualifier)) = idents.split_last() else {
                    return Ok(());
                };
                if qualifier.first().is_some_and(|first| self.is_parameter(first)) {
                    return Ok(());
                }
                if !qualifier_names_table(qualifier, self.catalog_name, self.table, self.case) {
                    return Err(crate::errors::Error::ForeignColumnReference {
                        reference: crate::errors::ColumnReference::unresolved(
                            self.dialect,
                            idents,
                            self.catalog_name,
                            false,
                            false,
                        ),
                        table_name: self
                            .dialect
                            .stored_spelling(self.table.name(), self.table.name_is_quoted()),
                    });
                }
                self.column(column, expr)
            }
            _ => Ok(()),
        }
    }
}

impl<Resolve: FnMut(&Ident) -> bool> Visitor for ColumnsRead<'_, '_, Resolve> {
    type Break = crate::errors::Error;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.subqueries += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.subqueries -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.subqueries > 0 || self.labels.holds(expr) {
            return ControlFlow::Continue(());
        }
        self.labels.enter(expr);
        if let Expr::Lambda(lambda) = expr {
            self.parameters.extend(lambda.params.iter().map(|parameter| parameter.name.clone()));
        }
        match self.reference(expr) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }

    fn post_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.subqueries == 0
            && let Expr::Lambda(lambda) = expr
        {
            self.parameters.truncate(self.parameters.len() - lambda.params.len());
        }
        ControlFlow::Continue(())
    }
}

/// Extracts columns from a SQL expression.
///
/// Every expression nested in `expr` is read, except inside the subqueries it
/// holds, whose names belong to the tables they read. A named argument's
/// label, a field named after a dot and a lambda's parameter read no column.
///
/// # Arguments
///
/// * `expr` - The SQL expression to extract columns from.
/// * `catalog_name` - The name of the catalog the table belongs to, which a
///   reference may name outermost.
/// * `table` - The table the expression belongs to, with the schema and quoting
///   a qualified reference is matched against.
/// * `columns` - The list of columns available in the table.
/// * `case` - The comparison a written name is read under, the catalog's
///   [`DatabaseLike::identifier_case`](crate::traits::DatabaseLike::identifier_case)
///   for an expression a catalog stores.
/// * `dialect` - The engine's identifier storage rules.
///
/// # Returns
///
/// * The columns found in the expression, each once, in the order it first
///   reads them.
///
/// # Errors
///
/// * [`Error::UnknownColumn`](crate::errors::Error::UnknownColumn) when a
///   column in the expression is not one of `columns`.
/// * [`Error::ForeignColumnReference`](crate::errors::Error::ForeignColumnReference)
///   when a reference is qualified by another table, schema or catalog, since
///   such a column is not this table's under a shorter name.
pub fn columns_in_expression<C: ColumnLike + Clone>(
    expr: &Expr,
    catalog_name: &str,
    table: &TargetName<'_>,
    columns: &[C],
    case: IdentifierCase,
    dialect: &dyn sqlparser::dialect::Dialect,
) -> Result<Vec<C>, crate::errors::Error> {
    let dialect = if dialect.is::<sqlparser::dialect::PostgreSqlDialect>() {
        crate::impls::SqlparserDialect::PostgreSql
    } else {
        crate::impls::SqlparserDialect::Generic
    };
    let mut read = Vec::new();
    visit_columns(expr, catalog_name, table, case, dialect, |ident| {
        let Some(column) = columns.iter().find(|column| {
            column_named_by(column.column_name(), column.column_name_is_quoted(), ident, case)
        }) else {
            return false;
        };
        if !read.contains(column) {
            read.push(column.clone());
        }
        true
    })?;
    Ok(read)
}

/// Walks references once, delegating column lookup and collection to the
/// caller.
pub(crate) fn visit_columns(
    expr: &Expr,
    catalog_name: &str,
    table: &TargetName<'_>,
    case: IdentifierCase,
    dialect: crate::impls::SqlparserDialect,
    resolve: impl FnMut(&Ident) -> bool,
) -> Result<(), crate::errors::Error> {
    let mut visitor = ColumnsRead {
        dialect,
        catalog_name,
        table,
        resolve,
        case,
        subqueries: 0,
        labels: Labels::default(),
        parameters: Vec::new(),
    };
    match walk(expr, &mut visitor) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        string::{String, ToString},
        sync::Arc,
    };

    use sqlparser::{
        ast::{
            BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
            ObjectName, ObjectNamePart, SelectItem, SetExpr, Statement,
        },
        dialect::{GenericDialect, SQLiteDialect},
        parser::Parser,
    };

    use super::*;
    use crate::{
        structs::{ParserDB, TableAttribute},
        traits::DatabaseLike,
    };

    fn create_column(name: &str) -> <ParserDB as DatabaseLike>::Column {
        let sql = format!("CREATE TABLE t ({name} INT)");
        let dialect = GenericDialect {};
        let mut ast = Parser::parse_sql(&dialect, &sql).expect("Failed to parse SQL");
        let Some(Statement::CreateTable(table)) = ast.pop() else {
            panic!("Expected CreateTable statement");
        };
        TableAttribute::declared_in(Arc::new(table), |table| table.columns.first())
            .expect("the table declares a column")
    }

    /// A qualifier chain reaching past this catalog names another one, and a
    /// reference with no parts names nothing at all. The parser produces
    /// neither, so both are built here.
    #[test]
    fn a_qualifier_beyond_this_catalog_names_no_local_column() {
        let columns = vec![create_column("id")];
        let table = TargetName::new("t", false);

        let deep = Expr::CompoundIdentifier(vec![
            Ident::new("cluster"),
            Ident::new("catalog"),
            Ident::new("public"),
            Ident::new("t"),
            Ident::new("id"),
        ]);
        assert!(matches!(
            columns_in_expression(
                &deep,
                "catalog",
                &table,
                &columns,
                IdentifierCase::AsWritten,
                &GenericDialect,
            ),
            Err(crate::errors::Error::ForeignColumnReference { .. })
        ));

        let empty = Expr::CompoundIdentifier(Vec::new());
        let found = columns_in_expression(
            &empty,
            "catalog",
            &table,
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .expect("a reference with no parts reads no column");
        assert_eq!(
            found.iter().map(ColumnLike::column_name).collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn test_columns_in_expression_identifier() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        let expr = Expr::Identifier(Ident::new("a"));

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column_name(), "a");
    }

    #[test]
    fn test_columns_in_expression_compound_identifier() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        let expr = Expr::CompoundIdentifier(vec![Ident::new("t"), Ident::new("a")]);

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column_name(), "a");
    }

    #[test]
    fn test_columns_in_expression_binary_op() {
        let col_a = create_column("a");
        let col_b = create_column("b");
        let columns = vec![col_a.clone(), col_b.clone()];
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("a"))),
            op: BinaryOperator::Plus,
            right: Box::new(Expr::Identifier(Ident::new("b"))),
        };

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].column_name(), "a");
        assert_eq!(result[1].column_name(), "b");
    }

    #[test]
    fn test_columns_in_expression_nested_and_deduplication() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        // (a) AND (a)
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(Expr::Identifier(Ident::new("a"))))),
            op: BinaryOperator::And,
            right: Box::new(Expr::Identifier(Ident::new("a"))),
        };

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column_name(), "a");
    }

    #[test]
    fn test_columns_in_expression_function() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        // my_func(a)
        let expr = Expr::Function(Function {
            name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("my_func"))]),
            args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(
                    Ident::new("a"),
                )))],
                clauses: vec![],
            }),
            over: None,
            filter: None,
            null_treatment: None,
            within_group: vec![],
            parameters: FunctionArguments::None,
            uses_odbc_syntax: false,
        });

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column_name(), "a");
    }

    #[test]
    fn test_columns_in_expression_between() {
        let col_a = create_column("a");
        let col_b = create_column("b");
        let col_c = create_column("c");
        let columns = vec![col_a.clone(), col_b.clone(), col_c.clone()];
        // a BETWEEN b AND c
        let expr = Expr::Between {
            expr: Box::new(Expr::Identifier(Ident::new("a"))),
            negated: false,
            low: Box::new(Expr::Identifier(Ident::new("b"))),
            high: Box::new(Expr::Identifier(Ident::new("c"))),
        };

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].column_name(), "a");
        assert_eq!(result[1].column_name(), "b");
        assert_eq!(result[2].column_name(), "c");
    }

    #[test]
    fn test_columns_in_expression_in_list() {
        let col_a = create_column("a");
        let col_b = create_column("b");
        let columns = vec![col_a.clone(), col_b.clone()];
        // a IN (b)
        let expr = Expr::In {
            expr: Box::new(Expr::Identifier(Ident::new("a"))),
            target: InTarget::List(vec![Expr::Identifier(Ident::new("b"))]),
            negated: false,
            global: false,
        };

        let result: Vec<<ParserDB as DatabaseLike>::Column> = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].column_name(), "a");
        assert_eq!(result[1].column_name(), "b");
    }

    #[test]
    fn test_columns_in_expression_unknown_column() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        let expr = Expr::Identifier(Ident::new("b"));

        let result = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        );
        assert!(matches!(
            result,
            Err(crate::errors::Error::UnknownColumn { reference }) if reference.to_string() == "b"
        ));
    }

    /// `CompoundIdentifier` error path — the last component of a
    /// `schema.bad_column` reference must resolve, otherwise the
    /// walker returns `UnknownColumn` naming the whole reference.
    #[test]
    fn test_columns_in_expression_compound_identifier_unknown() {
        let col_a = create_column("a");
        let columns = vec![col_a.clone()];
        // `t.b` — last ident is `b`, not in our column list.
        let expr = Expr::CompoundIdentifier(vec![Ident::new("t"), Ident::new("b")]);

        let result = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        );
        assert!(matches!(
            result,
            Err(crate::errors::Error::UnknownColumn { reference }) if reference.to_string() == "t.b"
        ));
    }

    /// `Expr::Tuple` branch — every element is walked, columns inside the
    /// tuple are surfaced, and the result is deduped.
    #[test]
    fn test_columns_in_expression_tuple() {
        let col_a = create_column("a");
        let col_b = create_column("b");
        let col_c = create_column("c");
        let columns = vec![col_a.clone(), col_b.clone(), col_c.clone()];
        // `(a, b, c)` as an expression tuple.
        let expr = Expr::Tuple(vec![
            Expr::Identifier(Ident::new("a")),
            Expr::Identifier(Ident::new("b")),
            Expr::Identifier(Ident::new("c")),
        ]);

        let result = columns_in_expression(
            &expr,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .expect("tuple of known columns parses");
        let names: Vec<&str> = result.iter().map(ColumnLike::column_name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn in_subqueries_only_read_the_outer_expression() {
        let column = create_column("a");
        let statements = Parser::parse_sql(&GenericDialect {}, "SELECT a IN (SELECT missing)")
            .expect("query parses");
        let Statement::Query(query) = &statements[0] else { panic!("expected a query") };
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select") };
        let SelectItem::UnnamedExpr(expression) = &select.projection[0] else {
            panic!("expected an expression")
        };

        let result = columns_in_expression(
            expression,
            "catalog",
            &TargetName::new("t", false),
            &[column],
            IdentifierCase::AsWritten,
            &GenericDialect,
        )
        .expect("column resolves");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column_name(), "a");
    }

    #[test]
    fn in_table_targets_read_local_operands_and_arguments() {
        let columns = vec![create_column("a"), create_column("b")];
        for (sql, expected) in [
            ("SELECT * FROM t WHERE a IN u", vec!["a"]),
            ("SELECT * FROM t WHERE a IN b", vec!["a"]),
            ("SELECT * FROM t WHERE f(a) IN json_each(b)", vec!["a", "b"]),
            ("SELECT * FROM t WHERE f(a) NOT IN json_each(b, a)", vec!["a", "b"]),
            ("SELECT * FROM t WHERE a IN json_each()", vec!["a"]),
            ("SELECT * FROM t WHERE a IN json_each((SELECT missing))", vec!["a"]),
        ] {
            let statements = Parser::parse_sql(&SQLiteDialect {}, sql).expect("query parses");
            let Statement::Query(query) = &statements[0] else { panic!("expected a query") };
            let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select") };
            let found = columns_in_expression(
                select.selection.as_ref().expect("a predicate"),
                "catalog",
                &TargetName::new("t", false),
                &columns,
                IdentifierCase::AsWritten,
                &GenericDialect,
            )
            .expect("local columns resolve");
            let names: Vec<_> = found.iter().map(ColumnLike::column_name).collect();
            assert_eq!(names, expected, "{sql}");
        }
    }

    #[test]
    fn in_table_target_arguments_reject_unknown_local_columns() {
        let statements =
            Parser::parse_sql(&SQLiteDialect {}, "SELECT * FROM t WHERE a IN json_each(missing)")
                .expect("query parses");
        let Statement::Query(query) = &statements[0] else { panic!("expected a query") };
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select") };
        let result = columns_in_expression(
            select.selection.as_ref().expect("a predicate"),
            "catalog",
            &TargetName::new("t", false),
            &[create_column("a")],
            IdentifierCase::AsWritten,
            &SQLiteDialect {},
        );
        assert!(matches!(
            result,
            Err(crate::errors::Error::UnknownColumn { reference }) if reference.to_string() == "missing"
        ));
    }

    #[test]
    fn in_unnest_reads_both_operands() {
        let columns = vec![create_column("a"), create_column("b")];
        for (sql, expected) in
            [("f(a) IN UNNEST(b)", vec!["a", "b"]), ("COUNT(*) IN UNNEST(b)", vec!["b"])]
        {
            let expression = Parser::new(&GenericDialect {})
                .try_with_sql(sql)
                .expect("expression tokenizes")
                .parse_expr()
                .expect("expression parses");
            let found = columns_in_expression(
                &expression,
                "catalog",
                &TargetName::new("t", false),
                &columns,
                IdentifierCase::AsWritten,
                &GenericDialect,
            )
            .expect("local columns resolve");
            let names: Vec<_> = found.iter().map(ColumnLike::column_name).collect();
            assert_eq!(names, expected, "{sql}");
        }
    }

    #[test]
    fn in_unnest_named_arguments_read_values_without_reading_labels() {
        let expression = Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
            .try_with_sql("f(input => a) IN UNNEST(g(other => b))")
            .expect("expression tokenizes")
            .parse_expr()
            .expect("expression parses");
        let found = columns_in_expression(
            &expression,
            "catalog",
            &TargetName::new("t", false),
            &[create_column("a"), create_column("b")],
            IdentifierCase::AsWritten,
            &sqlparser::dialect::PostgreSqlDialect {},
        )
        .expect("argument values resolve");
        let names: Vec<_> = found.iter().map(ColumnLike::column_name).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// The names `columns_in_expression` finds in `sql`, parsed under
    /// `dialect`, against a table holding the columns `a`, `b` and `c`.
    fn names_in(
        dialect: &dyn sqlparser::dialect::Dialect,
        sql: &str,
    ) -> Result<Vec<String>, crate::errors::Error> {
        let expression = Parser::new(dialect)
            .try_with_sql(sql)
            .expect("expression tokenizes")
            .parse_expr()
            .expect("expression parses");
        let columns = [create_column("a"), create_column("b"), create_column("c")];
        Ok(columns_in_expression(
            &expression,
            "catalog",
            &TargetName::new("t", false),
            &columns,
            IdentifierCase::AsWritten,
            dialect,
        )?
        .iter()
        .map(|column| ColumnLike::column_name(column).to_owned())
        .collect())
    }

    #[test]
    fn every_expression_form_reads_the_columns_inside_it() {
        let postgres = sqlparser::dialect::PostgreSqlDialect {};
        for (sql, expected) in [
            ("b = ANY (ARRAY['x', 'y'])", &["b"][..]),
            ("b = ALL (ARRAY[a, c])", &["b", "a", "c"]),
            ("(b)::text = ANY ((ARRAY['x'::varchar])::text[])", &["b"]),
            ("b COLLATE \"C\" = 'x'", &["b"]),
            ("b LIKE 'x%' OR c ILIKE a", &["b", "c", "a"]),
            ("CASE WHEN a > 0 THEN b ELSE c END = 'x'", &["a", "b", "c"]),
            ("a IS DISTINCT FROM c", &["a", "c"]),
            ("b IS TRUE", &["b"]),
            ("position('x' IN b) > a", &["b", "a"]),
            ("substring(b FROM a FOR c) = 'x'", &["b", "a", "c"]),
            ("ARRAY[a, b] && ARRAY[c]", &["a", "b", "c"]),
            ("b[a] = 'x'", &["b", "a"]),
            ("(b).field[c] = 'x'", &["b", "c"]),
        ] {
            assert_eq!(names_in(&postgres, sql).expect("columns resolve"), expected, "{sql}");
        }
    }

    #[test]
    fn names_that_are_not_columns_are_not_read() {
        let postgres = sqlparser::dialect::PostgreSqlDialect {};
        for (sql, expected) in [
            ("f(label => b) > a", &["b", "a"][..]),
            ("(b).field = 'x'", &["b"]),
            ("b = ANY (SELECT missing)", &["b"]),
            ("EXISTS (SELECT missing) OR a > 0", &["a"]),
        ] {
            assert_eq!(names_in(&postgres, sql).expect("columns resolve"), expected, "{sql}");
        }
        let duckdb = sqlparser::dialect::DuckDbDialect {};
        for (sql, expected) in [
            ("list_transform(b, x -> x || a)", &["b", "a"][..]),
            ("list_transform(b, x -> x.field || a)", &["b", "a"]),
        ] {
            assert_eq!(names_in(&duckdb, sql).expect("columns resolve"), expected, "{sql}");
        }
    }

    #[test]
    fn an_unknown_column_inside_any_expression_form_is_refused() {
        let postgres = sqlparser::dialect::PostgreSqlDialect {};
        for sql in [
            "b = ANY (ARRAY[missing])",
            "CASE WHEN missing THEN a END",
            "b COLLATE \"C\" = missing",
            "list_transform(b, x -> missing)",
        ] {
            let dialect: &dyn sqlparser::dialect::Dialect =
                if sql.contains("->") { &sqlparser::dialect::DuckDbDialect {} } else { &postgres };
            assert!(
                matches!(
                    names_in(dialect, sql),
                    Err(crate::errors::Error::UnknownColumn { reference }) if reference.to_string() == "missing"
                ),
                "{sql}"
            );
        }
    }
}
