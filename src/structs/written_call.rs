//! Submodule defining a call written in a statement, the node a caller asks
//! which functions it reaches.

use alloc::vec::Vec;
use core::fmt;

use sqlparser::ast::{Function, FunctionArg, ObjectName, TableFactor};

use crate::utils::call_typing::{WrittenName, written_argument_names, written_names};

/// A call written in a statement, borrowed from the statement that writes
/// it, either as a function expression or as a `FROM` item.
///
/// A function expression converts with [`From`]. A `FROM` item is a call
/// only when it carries an argument list, as `my_teams() t` or
/// `LATERAL team_size(t)` do, so it converts through [`Self::relation`].
#[derive(Debug, Clone, Copy)]
pub struct WrittenCall<'node>(CallNode<'node>);

/// The node a call is written as.
#[derive(Debug, Clone, Copy)]
enum CallNode<'node> {
    /// A function expression.
    Function(&'node Function),
    /// A `FROM` item written as a call, with the name it calls and the
    /// arguments it passes.
    Relation {
        factor: &'node TableFactor,
        name: &'node ObjectName,
        arguments: &'node [FunctionArg],
    },
}

impl<'node> WrittenCall<'node> {
    /// Returns the call `factor` writes, or [`None`] for a `FROM` item that
    /// carries no argument list, such as a table or a subquery.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let parsed = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT * FROM members, my_teams() t")?;
    /// let Statement::Query(query) = &parsed[0] else { unreachable!() };
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// assert!(WrittenCall::relation(&select.from[0].relation).is_none());
    /// let call = WrittenCall::relation(&select.from[1].relation).expect("a call in `FROM`");
    /// assert_eq!(call.to_string(), "my_teams()");
    /// # Ok::<(), sqlparser::parser::ParserError>(())
    /// ```
    #[must_use]
    pub fn relation(factor: &'node TableFactor) -> Option<Self> {
        let (name, arguments) = match factor {
            TableFactor::Table { name, args: Some(args), .. } => (name, args.args.as_slice()),
            TableFactor::Function { name, args, .. } => (name, args.as_slice()),
            _ => return None,
        };
        Some(Self(CallNode::Relation { factor, name, arguments }))
    }

    /// Returns the name the call is written with.
    pub(crate) fn name(self) -> &'node ObjectName {
        match self.0 {
            CallNode::Function(function) => &function.name,
            CallNode::Relation { name, .. } => name,
        }
    }

    /// Returns the arguments the call writes, by the name each is passed
    /// under, as [`written_argument_names`] reads them.
    pub(crate) fn written_names(self) -> Option<Vec<WrittenName<'node>>> {
        match self.0 {
            CallNode::Function(function) => written_argument_names(function),
            CallNode::Relation { arguments, .. } => written_names(arguments),
        }
    }

    /// Whether `self` and `other` are the same node, matched by address.
    pub(crate) fn is(self, other: Self) -> bool {
        match (self.0, other.0) {
            (CallNode::Function(left), CallNode::Function(right)) => core::ptr::eq(left, right),
            (CallNode::Relation { factor: left, .. }, CallNode::Relation { factor: right, .. }) => {
                core::ptr::eq(left, right)
            }
            _ => false,
        }
    }

    /// Returns the function expression the call is written as, or [`None`]
    /// for a `FROM` item.
    pub(crate) fn function(self) -> Option<&'node Function> {
        match self.0 {
            CallNode::Function(function) => Some(function),
            CallNode::Relation { .. } => None,
        }
    }

    /// Returns the `FROM` item the call is written as, or [`None`] for a
    /// function expression.
    pub(crate) fn factor(self) -> Option<&'node TableFactor> {
        match self.0 {
            CallNode::Function(_) => None,
            CallNode::Relation { factor, .. } => Some(factor),
        }
    }
}

impl<'node> From<&'node Function> for WrittenCall<'node> {
    fn from(function: &'node Function) -> Self {
        Self(CallNode::Function(function))
    }
}

impl fmt::Display for WrittenCall<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            CallNode::Function(function) => function.fmt(formatter),
            CallNode::Relation { name, arguments, .. } => {
                write!(formatter, "{name}(")?;
                for (index, argument) in arguments.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    argument.fmt(formatter)?;
                }
                formatter.write_str(")")
            }
        }
    }
}
