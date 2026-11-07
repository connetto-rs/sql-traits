//! Enumerates the function calls a node makes, subqueries included, and
//! decides which functions each may run.

use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, InTarget, ObjectName,
    ObjectNamePart, TableFactor, Visit, VisitMut, Visitor, VisitorMut,
};

use crate::{
    structs::IdentifierCase,
    traits::FunctionLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{Qualifier, function_search_path, object_name_last_part, qualifier_of},
    },
};

/// One call a node makes.
#[derive(Clone, Copy)]
pub(crate) struct Call<'a> {
    /// The name the call is written with.
    pub(crate) name: &'a ObjectName,
    /// How many arguments the call passes.
    pub(crate) arguments: usize,
    /// The node of a call written as a function expression. A table function
    /// in a `FROM` clause or an `IN` target has none.
    pub(crate) node: Option<&'a Function>,
}

/// One node a walk meets that is written with the syntax of a call.
#[derive(Clone, Copy)]
pub(crate) enum Site<'a> {
    /// A call to a function.
    Call(Call<'a>),
    /// A function expression PostgreSQL reads as an expression rather than
    /// as a call, which runs no function, as [`expression_form`] decides.
    ExpressionForm(&'a Function),
}

/// Hands every node in `node` written with the syntax of a call to `each`, in
/// the order a pre-order walk meets them, until `each` breaks.
pub(crate) fn for_each_site<N: Visit + ?Sized, B>(
    node: &N,
    each: impl FnMut(Site<'_>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    node.visit(&mut Sites { each })
}

/// Hands every call in `node` to `each`, in the order a pre-order walk meets
/// them, until `each` breaks.
///
/// The walk covers every clause of every subquery, so a call reads the same
/// wherever it is nested. A relation in a `FROM` clause is a call only when it
/// carries an argument list.
pub(crate) fn for_each_call<N: Visit + ?Sized, B>(
    node: &N,
    mut each: impl FnMut(Call<'_>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    for_each_site(node, |site| {
        match site {
            Site::Call(call) => each(call),
            Site::ExpressionForm(_) => ControlFlow::Continue(()),
        }
    })
}

/// Hands the written name and the argument count of every call in `node` to
/// `each`, in the order [`for_each_call`] meets the calls, until `each`
/// breaks.
pub(crate) fn for_each_call_name_mut<N: VisitMut + ?Sized, B>(
    node: &mut N,
    each: impl FnMut(&mut ObjectName, usize) -> ControlFlow<B>,
) -> ControlFlow<B> {
    node.visit(&mut CallNames { each })
}

/// Whether a call to `called` passing `arguments` arguments may run
/// `function`, an unqualified name walking the function lookup path
/// [`function_search_path`] builds from `search_path`.
///
/// PostgreSQL picks among the reachable overloads by argument type, which is
/// not inferred here, so every overload accepting that many arguments counts.
/// A function stored without a schema is in `public`, and a name built at run
/// time reaches nothing.
pub(crate) fn call_reaches<'path, F: FunctionLike, I>(
    function: &F,
    called: &ObjectName,
    arguments: usize,
    search_path: impl Fn() -> I,
    case: IdentifierCase,
) -> bool
where
    I: Iterator<Item = (&'path str, bool)>,
{
    let names_match = object_name_last_part(called).is_some_and(|(called, called_quoted)| {
        identifiers_match(function.name(), function.name_is_quoted(), called, called_quoted)
    });
    if !names_match || !function.accepts_argument_count(arguments) {
        return false;
    }
    let declared_in = function.target_name();
    let (schema, quoted) = declared_in
        .schema()
        .map_or(("public", false), |schema| (schema, declared_in.schema_is_quoted()));
    let in_schema = |(entry, entry_quoted): (&str, bool)| {
        case.identifiers_match(schema, quoted, entry, entry_quoted)
    };
    match qualifier_of(called) {
        Qualifier::Named(entry, entry_quoted) => in_schema((entry, entry_quoted)),
        Qualifier::Absent => function_search_path(search_path).any(in_schema),
        Qualifier::RunTime => false,
    }
}

/// An expression PostgreSQL's grammar writes with the syntax of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpressionForm {
    /// `COALESCE`, `GREATEST` and `LEAST`, of the common type of their
    /// arguments.
    CommonType,
    /// `NULLIF`, of the type of its first argument.
    FirstArgument,
    /// A SQL value function such as `CURRENT_DATE`, of a fixed type, spelled
    /// as sqlparser writes it.
    Value(&'static str),
}

/// The expression a call written `called` is, when PostgreSQL reads that
/// name as one rather than as a call to a function.
///
/// Each of these names is a keyword the grammar takes before any function
/// lookup, so only a bare unquoted name is one. A quoted or qualified name
/// calls a function as any other does, and so does `current_schema()`,
/// which only bare is a value function.
pub(crate) fn expression_form(called: &ObjectName) -> Option<ExpressionForm> {
    let [ObjectNamePart::Identifier(Ident { value, quote_style: None, .. })] = called.0.as_slice()
    else {
        return None;
    };
    let form = match value.to_ascii_lowercase().as_str() {
        "coalesce" | "greatest" | "least" => ExpressionForm::CommonType,
        "nullif" => ExpressionForm::FirstArgument,
        "current_date" => ExpressionForm::Value("DATE"),
        "current_time" => ExpressionForm::Value("TIME WITH TIME ZONE"),
        "current_timestamp" => ExpressionForm::Value("TIMESTAMP WITH TIME ZONE"),
        "localtime" => ExpressionForm::Value("TIME"),
        "localtimestamp" => ExpressionForm::Value("TIMESTAMP"),
        "current_user" | "current_role" | "session_user" | "user" | "current_catalog" => {
            ExpressionForm::Value("name")
        }
        _ => return None,
    };
    Some(form)
}

/// Hands each node the walk meets with the syntax of a call to `each`.
struct Sites<F> {
    each: F,
}

impl<B, F: FnMut(Site<'_>) -> ControlFlow<B>> Visitor for Sites<F> {
    type Break = B;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<B> {
        match expression {
            Expr::Function(function) if expression_form(&function.name).is_some() => {
                (self.each)(Site::ExpressionForm(function))
            }
            Expr::Function(function) => {
                let arguments = argument_count(&function.args);
                (self.each)(Site::Call(Call {
                    name: &function.name,
                    arguments,
                    node: Some(function),
                }))
            }
            Expr::In { target: InTarget::Table { name, args: Some(args) }, .. } => {
                (self.each)(Site::Call(Call { name, arguments: args.len(), node: None }))
            }
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<B> {
        match factor {
            TableFactor::Table { name, args: Some(args), .. } => {
                (self.each)(Site::Call(Call { name, arguments: args.args.len(), node: None }))
            }
            TableFactor::Function { name, args, .. } => {
                (self.each)(Site::Call(Call { name, arguments: args.len(), node: None }))
            }
            _ => ControlFlow::Continue(()),
        }
    }
}

/// How many arguments a function expression passes.
fn argument_count(arguments: &FunctionArguments) -> usize {
    match arguments {
        FunctionArguments::None => 0,
        FunctionArguments::Subquery(_) => 1,
        // PostgreSQL reads `count(*)` as a call passing nothing.
        FunctionArguments::List(list)
            if matches!(
                list.args.as_slice(),
                [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]
            ) =>
        {
            0
        }
        FunctionArguments::List(list) => list.args.len(),
    }
}

/// Hands the name and argument count of each call the walk meets to `each`.
struct CallNames<F> {
    each: F,
}

impl<B, F: FnMut(&mut ObjectName, usize) -> ControlFlow<B>> VisitorMut for CallNames<F> {
    type Break = B;

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<B> {
        match expression {
            Expr::Function(function) if expression_form(&function.name).is_none() => {
                let arguments = argument_count(&function.args);
                (self.each)(&mut function.name, arguments)
            }
            Expr::In { target: InTarget::Table { name, args: Some(args) }, .. } => {
                let arguments = args.len();
                (self.each)(name, arguments)
            }
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<B> {
        match factor {
            TableFactor::Table { name, args: Some(args), .. } => {
                let arguments = args.args.len();
                (self.each)(name, arguments)
            }
            TableFactor::Function { name, args, .. } => {
                let arguments = args.len();
                (self.each)(name, arguments)
            }
            _ => ControlFlow::Continue(()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The bindings one walk records are applied by the other, so both have
    //! to meet the same calls in the same order.

    use alloc::{
        string::{String, ToString},
        vec::Vec,
    };
    use core::{convert::Infallible, ops::ControlFlow};

    use sqlparser::{dialect::GenericDialect, parser::Parser};

    use super::{for_each_call, for_each_call_name_mut};

    #[test]
    fn both_walks_meet_every_call_in_the_same_order() {
        let sql = "SELECT f(a, g()) FROM t(2) AS r, LATERAL lf(4, 5) \
                   WHERE a IN tf(3) AND count(*) > (SELECT h() FROM u)";
        let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("the query parses");

        let mut read: Vec<(String, usize)> = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(&statements, |call| {
            read.push((call.name.to_string(), call.arguments));
            ControlFlow::Continue(())
        });
        let mut written: Vec<(String, usize)> = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) =
            for_each_call_name_mut(&mut statements, |name, arguments| {
                written.push((name.to_string(), arguments));
                ControlFlow::Continue(())
            });

        assert_eq!(read, written);
        let names: Vec<&str> = read.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["f", "g", "t", "lf", "tf", "count", "h"]);
    }
}
