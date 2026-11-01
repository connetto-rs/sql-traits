//! Enumerates the function calls a node makes, subqueries included, and
//! decides which functions each may run.

use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, InTarget, ObjectName,
    TableFactor, Visit, Visitor,
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

/// Hands every call in `node` to `each`, in the order a pre-order walk meets
/// them, until `each` breaks.
///
/// The walk covers every clause of every subquery, so a call reads the same
/// wherever it is nested. A relation in a `FROM` clause is a call only when it
/// carries an argument list.
pub(crate) fn for_each_call<N: Visit + ?Sized, B>(
    node: &N,
    each: impl FnMut(Call<'_>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    node.visit(&mut Calls { each })
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

/// Hands each call the walk meets to `each`.
struct Calls<F> {
    each: F,
}

impl<B, F: FnMut(Call<'_>) -> ControlFlow<B>> Visitor for Calls<F> {
    type Break = B;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<B> {
        match expression {
            Expr::Function(function) => {
                let arguments = match &function.args {
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
                };
                (self.each)(Call { name: &function.name, arguments, node: Some(function) })
            }
            Expr::In { target: InTarget::Table { name, args: Some(args) }, .. } => {
                (self.each)(Call { name, arguments: args.len(), node: None })
            }
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<B> {
        match factor {
            TableFactor::Table { name, args: Some(args), .. } => {
                (self.each)(Call { name, arguments: args.args.len(), node: None })
            }
            TableFactor::Function { name, args, .. } => {
                (self.each)(Call { name, arguments: args.len(), node: None })
            }
            _ => ControlFlow::Continue(()),
        }
    }
}
