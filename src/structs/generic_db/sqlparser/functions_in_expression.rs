//! Functions to extract functions from SQL expressions.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::{Expr, InTarget, ObjectName};

use crate::{
    traits::{DatabaseLike, function_like::FunctionLike},
    utils::{identifier_resolution::identifiers_match, object_name::object_name_last_part},
};

fn function_matches_object_name<DB: DatabaseLike>(
    function: &DB::Function,
    object_name: &ObjectName,
) -> bool {
    object_name_last_part(object_name).is_some_and(|(called, called_quoted)| {
        identifiers_match(function.name(), function.name_is_quoted(), called, called_quoted)
    })
}

pub(super) fn functions_in_expression<DB: DatabaseLike>(
    expr: &Expr,
    functions: &[Arc<DB::Function>],
) -> Vec<Arc<DB::Function>> {
    let mut result = Vec::new();

    match expr {
        Expr::Function(func) => {
            // Match by function identifier, ignoring optional schema
            // qualifiers.
            result.extend(
                functions
                    .iter()
                    .filter(|f| function_matches_object_name::<DB>(f.as_ref(), &func.name))
                    .cloned(),
            );

            // Recursively check function arguments for nested function calls
            if let sqlparser::ast::FunctionArguments::List(args) = &func.args {
                for arg in &args.args {
                    match arg {
                        sqlparser::ast::FunctionArg::Named {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(expr),
                            ..
                        }
                        | sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(expr),
                        ) => {
                            result.extend(functions_in_expression::<DB>(expr, functions));
                        }
                        sqlparser::ast::FunctionArg::ExprNamed { .. }
                        | sqlparser::ast::FunctionArg::Named { .. }
                        | sqlparser::ast::FunctionArg::Unnamed(_) => {}
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            result.extend(functions_in_expression::<DB>(left, functions));
            result.extend(functions_in_expression::<DB>(right, functions));
        }
        Expr::Nested(nested_expr) => {
            result.extend(functions_in_expression::<DB>(nested_expr, functions));
        }
        Expr::Between { expr, negated: _, low, high } => {
            result.extend(functions_in_expression::<DB>(expr, functions));
            result.extend(functions_in_expression::<DB>(low, functions));
            result.extend(functions_in_expression::<DB>(high, functions));
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            result.extend(functions_in_expression::<DB>(expr, functions));
        }
        Expr::In { expr, target: InTarget::List(list), .. } => {
            result.extend(functions_in_expression::<DB>(expr, functions));
            for list_expr in list {
                result.extend(functions_in_expression::<DB>(list_expr, functions));
            }
        }
        Expr::In { expr, target: InTarget::Subquery(_), .. } => {
            result.extend(functions_in_expression::<DB>(expr, functions));
            // Note: We don't traverse into subqueries as they have their own
            // scope
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

    use sqlparser::{
        ast::{Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction},
        dialect::GenericDialect,
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
        assert!(!super::function_matches_object_name::<ParserDB>(function, &dynamic_name));
        assert!(!super::function_matches_object_name::<ParserDB>(
            function,
            &ObjectName(Vec::new()),
        ));
    }
}
