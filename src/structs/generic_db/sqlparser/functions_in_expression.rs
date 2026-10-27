//! Collects local function dependencies, including table-valued `IN` targets.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::{
    ArgMode, CreateFunction, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, InTarget,
    ObjectName,
};

use super::schema_qualifiers_match;
use crate::{
    structs::IdentifierCase,
    traits::function_like::FunctionLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{Qualifier, object_name_last_part, qualifier_of},
    },
};

/// The functions a call may reach, and the path an unqualified call walks.
pub(super) struct CallScope<'a> {
    /// Every function the schema holds.
    pub(super) functions: &'a [Arc<CreateFunction>],
    /// The search path in force where the call is written.
    pub(super) search_path: &'a [(&'a str, bool)],
    /// The comparison schema names are taken under.
    pub(super) case: IdentifierCase,
}

impl CallScope<'_> {
    /// Whether a call to `called` passing `arguments` arguments may run
    /// `function`.
    ///
    /// PostgreSQL picks among the reachable overloads by argument type, which
    /// is not inferred here, so every overload accepting that many arguments
    /// counts.
    pub(super) fn reaches(
        &self,
        function: &CreateFunction,
        called: &ObjectName,
        arguments: usize,
    ) -> bool {
        let names_match = object_name_last_part(called).is_some_and(|(called, called_quoted)| {
            identifiers_match(function.name(), function.name_is_quoted(), called, called_quoted)
        });
        let declared_in = qualifier_of(&function.name).named();
        let schema_reached = match qualifier_of(called) {
            Qualifier::Named(schema, quoted) => {
                schema_qualifiers_match(declared_in, Some((schema, quoted)), self.case)
            }
            // PostgreSQL searches `pg_catalog` first unless the path names it.
            Qualifier::Absent => {
                declared_in.is_some_and(|(schema, quoted)| {
                    self.case.identifiers_match(schema, quoted, "pg_catalog", false)
                }) || self
                    .search_path
                    .iter()
                    .any(|&entry| schema_qualifiers_match(declared_in, Some(entry), self.case))
            }
            Qualifier::RunTime => false,
        };
        names_match && schema_reached && accepts(function, arguments)
    }

    /// The functions a call to `called` passing `arguments` arguments may run.
    fn reached_by<'s>(
        &'s self,
        called: &'s ObjectName,
        arguments: usize,
    ) -> impl Iterator<Item = &'s Arc<CreateFunction>> {
        self.functions.iter().filter(move |function| self.reaches(function, called, arguments))
    }
}

/// Whether `function` takes `arguments` arguments, counting the trailing
/// ones a default fills and any number a `VARIADIC` parameter collects.
fn accepts(function: &CreateFunction, arguments: usize) -> bool {
    let inputs: Vec<_> = function
        .args
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|argument| argument.mode != Some(ArgMode::Out))
        .collect();
    let variadic = inputs.last().is_some_and(|argument| argument.mode == Some(ArgMode::Variadic));
    let required = inputs
        .iter()
        .filter(|argument| {
            argument.default_expr.is_none() && argument.mode != Some(ArgMode::Variadic)
        })
        .count();
    arguments >= required && (variadic || arguments <= inputs.len())
}

fn functions_in_arguments(
    arguments: &[FunctionArg],
    scope: &CallScope<'_>,
    result: &mut Vec<Arc<CreateFunction>>,
) {
    for argument in arguments {
        let argument = match argument {
            FunctionArg::Named { arg, .. }
            | FunctionArg::ExprNamed { arg, .. }
            | FunctionArg::Unnamed(arg) => arg,
        };
        if let FunctionArgExpr::Expr(expression) = argument {
            result.extend(functions_in_expression(expression, scope));
        }
    }
}

pub(super) fn functions_in_expression(
    expr: &Expr,
    scope: &CallScope<'_>,
) -> Vec<Arc<CreateFunction>> {
    let mut result = Vec::new();

    match expr {
        Expr::Function(func) => {
            let arguments = match &func.args {
                FunctionArguments::None => 0,
                FunctionArguments::Subquery(_) => 1,
                FunctionArguments::List(list) => list.args.len(),
            };
            result.extend(scope.reached_by(&func.name, arguments).cloned());

            if let FunctionArguments::List(args) = &func.args {
                functions_in_arguments(&args.args, scope, &mut result);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            result.extend(functions_in_expression(left, scope));
            result.extend(functions_in_expression(right, scope));
        }
        Expr::Nested(nested_expr) => {
            result.extend(functions_in_expression(nested_expr, scope));
        }
        Expr::Between { expr, negated: _, low, high } => {
            result.extend(functions_in_expression(expr, scope));
            result.extend(functions_in_expression(low, scope));
            result.extend(functions_in_expression(high, scope));
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            result.extend(functions_in_expression(expr, scope));
        }
        Expr::In { expr, target, .. } => {
            result.extend(functions_in_expression(expr, scope));
            match target {
                InTarget::List(list) => {
                    for expression in list {
                        result.extend(functions_in_expression(expression, scope));
                    }
                }
                InTarget::Subquery(_) => {}
                InTarget::Unnest(expression) => {
                    result.extend(functions_in_expression(expression, scope));
                }
                InTarget::Table { name, args } => {
                    if let Some(arguments) = args {
                        result.extend(scope.reached_by(name, arguments.len()).cloned());
                        functions_in_arguments(arguments, scope, &mut result);
                    }
                }
            }
        }
        _ => {}
    }

    // Remove duplicates while preserving order. BTreeSet works on raw
    // pointers (which implement `Ord`) and keeps the helper `alloc`-only
    // for a no_std-compatible build.
    let mut seen: alloc::collections::BTreeSet<*const ()> = alloc::collections::BTreeSet::new();
    result
        .into_iter()
        .filter(|func| {
            let ptr = Arc::as_ptr(func).cast::<()>();
            seen.insert(ptr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! These tests exercise `functions_in_expression` indirectly by parsing
    //! schemas whose CHECK constraints reference user-defined functions in
    //! the shapes that drive the un-tested branches: no-arg calls, nested
    //! calls, and call-twice-dedup.

    use alloc::sync::Arc;

    use sqlparser::{
        ast::{Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction, SetExpr, Statement},
        dialect::{GenericDialect, SQLiteDialect},
        parser::Parser,
    };

    use crate::{
        prelude::ParserDB,
        structs::{IdentifierCase, TargetName},
        traits::{DatabaseLike, FunctionLike, TableLike},
    };

    /// `FunctionArguments::None` branch — a no-argument function call in a
    /// CHECK expression is correctly attributed to the defining function.
    #[test]
    fn test_no_arg_function_call_is_attributed() {
        let sql = "
            CREATE FUNCTION ping() RETURNS BOOLEAN AS 'SELECT TRUE';
            CREATE TABLE t (id INT, CHECK (ping()));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let check = t.check_constraints(&db).expect("check constraints").next().expect("check");
        let meta = db.check_constraint_metadata(check).expect("check meta");
        let names: Vec<&str> = meta.functions().map(FunctionLike::name).collect();
        assert!(names.contains(&"ping"));
    }

    /// Same function used twice in one expression is deduplicated (BTreeSet
    /// over `Arc::as_ptr` keeps a single entry).
    #[test]
    fn test_same_function_used_twice_is_deduped() {
        let sql = "
            CREATE FUNCTION ping() RETURNS BOOLEAN AS 'SELECT TRUE';
            CREATE TABLE t (id INT, CHECK (ping() AND ping()));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let check = t.check_constraints(&db).expect("check constraints").next().expect("check");
        let meta = db.check_constraint_metadata(check).expect("check meta");
        let names: Vec<&str> = meta.functions().map(FunctionLike::name).collect();
        assert_eq!(names.len(), 1, "ping() appears twice in source but dedups to one");
        assert_eq!(names[0], "ping");
    }

    /// Nested function calls (`f(g(...))`) — the recursion walks into the
    /// inner arg list and attributes both functions.
    #[test]
    fn test_nested_function_calls_are_both_attributed() {
        let sql = "
            CREATE FUNCTION inner_fn(x INT) RETURNS INT AS 'SELECT $1';
            CREATE FUNCTION outer_fn(x INT) RETURNS BOOLEAN AS 'SELECT $1 > 0';
            CREATE TABLE t (id INT, CHECK (outer_fn(inner_fn(id))));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let check = t.check_constraints(&db).expect("check constraints").next().expect("check");
        let meta = db.check_constraint_metadata(check).expect("check meta");
        let names: Vec<&str> = meta.functions().map(FunctionLike::name).collect();
        assert!(names.contains(&"outer_fn"), "outer function attributed");
        assert!(names.contains(&"inner_fn"), "inner function attributed via recursion");
    }

    /// Functions in an `IN` list belong to the check constraint.
    #[test]
    fn test_in_list_function_call_is_attributed() {
        let sql = "
            CREATE FUNCTION classify(x INT) RETURNS INT AS 'SELECT $1';
            CREATE TABLE t (
                id INT,
                CHECK (id IN (classify(1), classify(2), 3))
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let check = t.check_constraints(&db).expect("check constraints").next().expect("check");
        let meta = db.check_constraint_metadata(check).expect("check meta");
        let names: Vec<&str> = meta.functions().map(FunctionLike::name).collect();
        assert!(names.contains(&"classify"));
    }

    /// `Expr::Between` branch: function calls in any of the three
    /// sub-expressions (`expr`, `low`, `high`) are all attributed.
    #[test]
    fn test_between_function_calls_in_all_three_positions() {
        let sql = "
            CREATE FUNCTION lo() RETURNS INT AS 'SELECT 1';
            CREATE FUNCTION hi() RETURNS INT AS 'SELECT 10';
            CREATE FUNCTION mid(x INT) RETURNS INT AS 'SELECT $1';
            CREATE TABLE t (
                id INT,
                CHECK (mid(id) BETWEEN lo() AND hi())
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let check = t.check_constraints(&db).expect("check constraints").next().expect("check");
        let meta = db.check_constraint_metadata(check).expect("check meta");
        let names: Vec<&str> = meta.functions().map(FunctionLike::name).collect();
        assert!(names.contains(&"lo"));
        assert!(names.contains(&"hi"));
        assert!(names.contains(&"mid"));
    }

    #[test]
    fn in_subqueries_only_read_the_outer_expression() {
        let sql = "
            CREATE FUNCTION outside_fn(x INT) RETURNS BOOLEAN AS 'SELECT TRUE';
            CREATE FUNCTION inside_fn(x INT) RETURNS INT AS 'SELECT $1';
            CREATE TABLE t (
                id INT,
                CHECK (outside_fn(id) IN (SELECT inside_fn(id)))
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("schema parses");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists");
        let check =
            table.check_constraints(&db).expect("constraints resolve").next().expect("check");
        let metadata = db.check_constraint_metadata(check).expect("metadata resolves");
        let names: Vec<&str> = metadata.functions().map(FunctionLike::name).collect();
        assert_eq!(names, vec!["outside_fn"]);
    }

    #[test]
    fn in_table_targets_collect_calls_without_confusing_tables_with_functions() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT $1';
             CREATE FUNCTION g(x INT) RETURNS INT AS 'SELECT $1';
             CREATE FUNCTION json_each() RETURNS INT AS 'SELECT 1';
             CREATE FUNCTION json_each(x INT) RETURNS INT AS 'SELECT $1';
             CREATE FUNCTION json_each(x INT, y INT) RETURNS INT AS 'SELECT $1';
             CREATE FUNCTION u() RETURNS INT AS 'SELECT 1';",
        )
        .expect("function declarations parse");
        let functions: Vec<_> = db.functions().cloned().map(Arc::new).collect();
        let scope = scope(&functions);
        for (sql, expected) in [
            ("SELECT * FROM t WHERE a IN u", vec![]),
            ("SELECT * FROM t WHERE f(a) IN u", vec!["f"]),
            ("SELECT * FROM t WHERE f(a) IN json_each(b)", vec!["f", "json_each"]),
            ("SELECT * FROM t WHERE f(a) NOT IN json_each(g(b), a)", vec!["f", "json_each", "g"]),
            ("SELECT * FROM t WHERE a IN json_each()", vec!["json_each"]),
            ("SELECT * FROM t WHERE f(a) IN f(f(b))", vec!["f"]),
            ("SELECT * FROM t WHERE f(a) IN json_each((SELECT g(b)))", vec!["f", "json_each"]),
        ] {
            let statements = Parser::parse_sql(&SQLiteDialect {}, sql).expect("query parses");
            let Statement::Query(query) = &statements[0] else { panic!("expected a query") };
            let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select") };
            let calls = super::functions_in_expression(
                select.selection.as_ref().expect("a predicate"),
                &scope,
            );
            let names: Vec<_> = calls.iter().map(|function| function.name()).collect();
            assert_eq!(names, expected, "{sql}");
        }
    }

    #[test]
    fn in_unnest_collects_calls_from_both_operands() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT $1';
             CREATE FUNCTION g(x INT) RETURNS INT AS 'SELECT $1';",
        )
        .expect("function declarations parse");
        let functions: Vec<_> = db.functions().cloned().map(Arc::new).collect();
        let scope = scope(&functions);
        let cases: [(&str, &[&str]); 4] = [
            ("f(a) IN UNNEST(g(b))", &["f", "g"]),
            ("COUNT(*) IN UNNEST(g(b))", &["count", "g"]),
            ("(f(a)) IN UNNEST(g(b))", &["f", "g"]),
            // `ARRAY(SELECT ...)` passes its query as the one argument.
            ("ARRAY(SELECT 1) IN UNNEST(g(b))", &["g"]),
        ];
        for (sql, expected) in cases {
            let expression = Parser::new(&GenericDialect {})
                .try_with_sql(sql)
                .expect("expression tokenizes")
                .parse_expr()
                .expect("expression parses");
            let calls = super::functions_in_expression(&expression, &scope);
            let names: Vec<_> = calls.iter().map(|function| function.name()).collect();
            assert_eq!(names, expected, "{sql}");
        }
    }

    /// A call whose name is built at run time matches no declaration, since
    /// which one it will reach is unknown until it runs.
    #[test]
    fn a_run_time_call_name_matches_no_declaration() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE FUNCTION ping() RETURNS BOOLEAN AS 'SELECT TRUE';",
        )
        .expect("schema parses");
        let function = db
            .function_by_target(TargetName::new("ping", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function exists");
        let dynamic_name = ObjectName(vec![ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::new("ping"),
            args: Vec::new(),
        })]);
        let scope = scope(&[]);
        assert!(!scope.reaches(function, &dynamic_name, 0));
        assert!(!scope.reaches(function, &ObjectName(Vec::new()), 0));
        let run_time_schema = ObjectName(vec![
            ObjectNamePart::Function(ObjectNamePartFunction {
                name: Ident::new("IDENTIFIER"),
                args: Vec::new(),
            }),
            ObjectNamePart::Identifier(Ident::new("ping")),
        ]);
        assert!(!scope.reaches(function, &run_time_schema, 0));
    }

    /// The scope a call written on the default search path sees.
    fn scope(functions: &[Arc<sqlparser::ast::CreateFunction>]) -> super::CallScope<'_> {
        super::CallScope {
            functions,
            search_path: &[("public", false)],
            case: IdentifierCase::AsWritten,
        }
    }
}
