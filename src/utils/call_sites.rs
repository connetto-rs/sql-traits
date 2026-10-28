//! Enumerates the function calls an expression makes, subqueries included.

use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Function, FunctionArguments, InTarget, ObjectName, TableFactor, Visit, Visitor,
};

/// One call an expression makes.
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

/// Hands every call in `expression` to `each`, in the order a pre-order walk
/// meets them, until `each` breaks.
///
/// The walk covers every clause of every subquery, so a call reads the same
/// wherever it is nested. A relation in a `FROM` clause is a call only when it
/// carries an argument list.
pub(crate) fn for_each_call<B>(
    expression: &Expr,
    each: impl FnMut(Call<'_>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    expression.visit(&mut Calls { each })
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
