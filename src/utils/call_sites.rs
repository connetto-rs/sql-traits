//! Enumerates the function calls a node makes, subqueries included, and
//! decides which functions each may run.

use alloc::{boxed::Box, vec, vec::Vec};
use core::ops::ControlFlow;

use once_cell::race::OnceBox;
use sqlparser::ast::{
    ArgMode, CeilFloorKind, DateTimeField, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentList, FunctionArguments, Ident, InTarget, ObjectName, ObjectNamePart,
    TableFactor, Value, Visit, VisitMut, Visitor, VisitorMut,
};

use crate::{
    structs::{IdentifierCase, WrittenCall},
    traits::FunctionLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{Qualifier, function_search_path, object_name_last_part, qualifier_of},
        walk::{walk, walk_mut},
    },
};

/// One call a node makes.
#[derive(Clone, Copy)]
pub(crate) struct Call<'a> {
    /// The name the call is written with.
    pub(crate) name: &'a ObjectName,
    /// How many arguments the call passes.
    pub(crate) arguments: usize,
    /// The node the call is written as. A call in an `IN` target has none,
    /// and so does a [`KeywordCall`].
    pub(crate) node: Option<WrittenCall<'a>>,
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
    mut each: impl FnMut(Site<'_>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    walk(node, &mut Sites { each: &mut each })
}

/// Hands every call in `node` to `each`, in the order a pre-order walk meets
/// them, until `each` breaks.
///
/// The walk covers every clause of every subquery, so a call reads the same
/// wherever it is nested. A relation in a `FROM` clause is a call only when it
/// carries an argument list, and a [`KeywordCall`] is a call by its bare name.
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

/// Respells every call in `node` to the name `each` gives it from its written
/// name and argument count, in the order [`for_each_call`] meets the calls,
/// until `each` breaks. A call `each` gives no name keeps its spelling, and a
/// respelled [`KeywordCall`] is written as a function call.
pub(crate) fn respell_calls<N: VisitMut + ?Sized, B>(
    node: &mut N,
    mut each: impl FnMut(&ObjectName, usize) -> ControlFlow<B, Option<ObjectName>>,
) -> ControlFlow<B> {
    walk_mut(node, &mut Respellings { each: &mut each })
}

/// A call sqlparser parses into an expression of its own, which PostgreSQL's
/// grammar reads as a plain call and so resolves on the search path by its
/// bare name, as it does any other call.
///
/// `ceil` and `floor` take an optional scale, `substr` is a call in every
/// form, and `substring` is one when written with commas or with its source
/// alone. The `FROM`, `FOR` and `SIMILAR` forms of `SUBSTRING` are grammar
/// calling `pg_catalog`, and `CEIL(x TO field)` is no PostgreSQL syntax.
#[derive(Clone, Copy)]
enum KeywordCall {
    Ceil,
    Floor,
    Substr,
    Substring,
}

impl KeywordCall {
    /// The call `expression` writes and how many arguments it passes, or
    /// [`None`] for an expression that is no such call.
    fn of(expression: &Expr) -> Option<(Self, usize)> {
        let scaled = |field: &CeilFloorKind| {
            match field {
                CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) => Some(1),
                CeilFloorKind::Scale(_) => Some(2),
                CeilFloorKind::DateTimeField(_) => None,
            }
        };
        match expression {
            Expr::Ceil { field, .. } => scaled(field).map(|arguments| (Self::Ceil, arguments)),
            Expr::Floor { field, .. } => scaled(field).map(|arguments| (Self::Floor, arguments)),
            Expr::Substring { substring_from, substring_for, special, shorthand, .. }
                if *special
                    || *shorthand
                    || (substring_from.is_none() && substring_for.is_none()) =>
            {
                let call = if *shorthand { Self::Substr } else { Self::Substring };
                let arguments = 1
                    + usize::from(substring_from.is_some())
                    + usize::from(substring_for.is_some());
                Some((call, arguments))
            }
            _ => None,
        }
    }

    /// The bare name PostgreSQL resolves the call by.
    fn name(self) -> &'static ObjectName {
        static CEIL: OnceBox<ObjectName> = OnceBox::new();
        static FLOOR: OnceBox<ObjectName> = OnceBox::new();
        static SUBSTR: OnceBox<ObjectName> = OnceBox::new();
        static SUBSTRING: OnceBox<ObjectName> = OnceBox::new();
        let (name, spelled) = match self {
            Self::Ceil => (&CEIL, "ceil"),
            Self::Floor => (&FLOOR, "floor"),
            Self::Substr => (&SUBSTR, "substr"),
            Self::Substring => (&SUBSTRING, "substring"),
        };
        name.get_or_init(|| Box::new(ObjectName::from(vec![Ident::new(spelled)])))
    }

    /// Rewrites `expression`, a call [`KeywordCall::of`] reads, as a call to
    /// `name` passing the same arguments in the same order. Any other
    /// expression is left as it is.
    fn spell_as_function(expression: &mut Expr, name: ObjectName) {
        let written = core::mem::replace(expression, Expr::Value(Value::Null.with_empty_span()));
        let arguments: Vec<Expr> = match written {
            Expr::Ceil { expr, field } | Expr::Floor { expr, field } => {
                let scale = match field {
                    CeilFloorKind::Scale(scale) => Some(Expr::Value(scale)),
                    CeilFloorKind::DateTimeField(_) => None,
                };
                core::iter::once(*expr).chain(scale).collect()
            }
            Expr::Substring { expr, substring_from, substring_for, .. } => {
                core::iter::once(expr)
                    .chain(substring_from)
                    .chain(substring_for)
                    .map(|argument| *argument)
                    .collect()
            }
            written => {
                *expression = written;
                return;
            }
        };
        let args = arguments
            .into_iter()
            .map(|argument| FunctionArg::Unnamed(FunctionArgExpr::Expr(argument)))
            .collect();
        *expression = Expr::Function(Function {
            name,
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args,
                clauses: Vec::new(),
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: Vec::new(),
        });
    }
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

/// The function among `candidates` that PostgreSQL's `LookupFuncName` finds
/// for `called` with no arguments, as a trigger's `EXECUTE FUNCTION` looks it
/// up.
///
/// Only a function taking no input arguments counts, since the lookup fills
/// no defaults. A qualified name reaches its own schema, and an unqualified
/// one the first schema of the function lookup path that
/// [`function_search_path`] builds from `search_path`. Whether the function
/// found is a procedure is left to the caller.
pub(crate) fn zero_argument_function<'function, 'path, F: FunctionLike + 'function, I>(
    candidates: impl IntoIterator<Item = &'function F>,
    called: &ObjectName,
    search_path: impl Fn() -> I,
    case: IdentifierCase,
) -> Option<&'function F>
where
    I: Iterator<Item = (&'path str, bool)>,
{
    let path_position = |function: &F| {
        let declared_in = function.target_name();
        let (schema, quoted) = declared_in
            .schema()
            .map_or(("public", false), |schema| (schema, declared_in.schema_is_quoted()));
        function_search_path(&search_path)
            .position(|(entry, entry_quoted)| {
                case.identifiers_match(schema, quoted, entry, entry_quoted)
            })
            .unwrap_or(0)
    };
    candidates
        .into_iter()
        .filter(|function| {
            function.argument_modes().all(|mode| mode == ArgMode::Out)
                && call_reaches(*function, called, 0, &search_path, case)
        })
        .min_by_key(|function| path_position(function))
}

/// An expression PostgreSQL's grammar writes with the syntax of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpressionForm {
    /// `COALESCE`, `GREATEST` and `LEAST`, of the common type of their
    /// arguments.
    CommonType,
    /// `NULLIF`, of the type of its first argument as the `=` it applies
    /// converts it.
    NullIf,
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
        "nullif" => ExpressionForm::NullIf,
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
struct Sites<'f, B> {
    each: &'f mut dyn FnMut(Site<'_>) -> ControlFlow<B>,
}

impl<B> Visitor for Sites<'_, B> {
    type Break = B;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<B> {
        match expression {
            Expr::Function(function) if expression_form(&function.name).is_some() => {
                (self.each)(Site::ExpressionForm(function))
            }
            Expr::Function(function) => {
                let arguments = argument_count(function);
                (self.each)(Site::Call(Call {
                    name: &function.name,
                    arguments,
                    node: Some(WrittenCall::from(function)),
                }))
            }
            Expr::In { target: InTarget::Table { name, args: Some(args) }, .. } => {
                (self.each)(Site::Call(Call { name, arguments: args.len(), node: None }))
            }
            _ => {
                KeywordCall::of(expression).map_or(
                    ControlFlow::Continue(()),
                    |(call, arguments)| {
                        (self.each)(Site::Call(Call { name: call.name(), arguments, node: None }))
                    },
                )
            }
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<B> {
        match factor {
            TableFactor::Table { name, args: Some(args), .. } => {
                (self.each)(Site::Call(Call {
                    name,
                    arguments: args.args.len(),
                    node: WrittenCall::relation(factor),
                }))
            }
            TableFactor::Function { name, args, .. } => {
                (self.each)(Site::Call(Call {
                    name,
                    arguments: args.len(),
                    node: WrittenCall::relation(factor),
                }))
            }
            _ => ControlFlow::Continue(()),
        }
    }
}

/// How many arguments a function expression passes, the ordering arguments
/// of `WITHIN GROUP` included.
pub(crate) fn argument_count(function: &Function) -> usize {
    let written = match &function.args {
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
    written + function.within_group.len()
}

/// Answers, for a call's written name and argument count, the name it is
/// respelled to, or breaks with `B`.
type Respell<'f, B> = dyn FnMut(&ObjectName, usize) -> ControlFlow<B, Option<ObjectName>> + 'f;

/// Respells each call the walk meets to the name `each` gives it.
struct Respellings<'f, B> {
    each: &'f mut Respell<'f, B>,
}

impl<B> Respellings<'_, B> {
    /// Respells `name`, a call passing `arguments` arguments, to the name
    /// `each` gives it.
    fn respell(&mut self, name: &mut ObjectName, arguments: usize) -> ControlFlow<B> {
        if let Some(respelled) = (self.each)(name, arguments)? {
            *name = respelled;
        }
        ControlFlow::Continue(())
    }
}

impl<B> VisitorMut for Respellings<'_, B> {
    type Break = B;

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<B> {
        match expression {
            Expr::Function(function) if expression_form(&function.name).is_none() => {
                let arguments = argument_count(function);
                self.respell(&mut function.name, arguments)
            }
            Expr::In { target: InTarget::Table { name, args: Some(args) }, .. } => {
                let arguments = args.len();
                self.respell(name, arguments)
            }
            _ => {
                if let Some((call, arguments)) = KeywordCall::of(expression)
                    && let Some(respelled) = (self.each)(call.name(), arguments)?
                {
                    KeywordCall::spell_as_function(expression, respelled);
                }
                ControlFlow::Continue(())
            }
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<B> {
        match factor {
            TableFactor::Table { name, args: Some(args), .. } => {
                let arguments = args.args.len();
                self.respell(name, arguments)
            }
            TableFactor::Function { name, args, .. } => {
                let arguments = args.len();
                self.respell(name, arguments)
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
        vec,
        vec::Vec,
    };
    use core::{convert::Infallible, ops::ControlFlow};

    use sqlparser::{
        ast::{Ident, ObjectName, ObjectNamePart},
        dialect::GenericDialect,
        parser::Parser,
    };

    use super::{for_each_call, respell_calls};

    #[test]
    fn both_walks_meet_every_call_in_the_same_order() {
        let sql = "SELECT f(a, g()), CEIL(a), FLOOR(a, 2), substr(b, 1), SUBSTRING(b, 1, 2), \
                   SUBSTRING(b), SUBSTRING(b FROM 1), CEIL(c TO DAY) \
                   FROM t(2) AS r, LATERAL lf(4, 5) \
                   WHERE a IN tf(3) AND count(*) > (SELECT h() FROM u)";
        let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("the query parses");

        let mut read: Vec<(String, usize)> = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(&statements, |call| {
            read.push((call.name.to_string(), call.arguments));
            ControlFlow::Continue(())
        });
        let mut written: Vec<(String, usize)> = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) =
            respell_calls(&mut statements, |name, arguments| {
                written.push((name.to_string(), arguments));
                ControlFlow::Continue(None)
            });

        assert_eq!(read, written);
        let calls: Vec<(&str, usize)> =
            read.iter().map(|(name, arguments)| (name.as_str(), *arguments)).collect();
        assert_eq!(
            calls,
            [
                ("f", 2),
                ("g", 0),
                ("ceil", 1),
                ("floor", 2),
                ("substr", 2),
                ("substring", 3),
                ("substring", 1),
                ("t", 1),
                ("lf", 2),
                ("tf", 1),
                ("count", 0),
                ("h", 0),
            ]
        );
    }

    #[test]
    fn a_respelled_keyword_call_is_written_as_a_function_call() {
        let sql = "SELECT CEIL(a), FLOOR(a, 2), substr(b, 1), SUBSTRING(b, 1, 2), \
                   SUBSTRING(b FROM 1)";
        let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("the query parses");

        let ControlFlow::<Infallible>::Continue(()) = respell_calls(&mut statements, |name, _| {
            let ObjectName(parts) = name;
            let mut respelled = vec![ObjectNamePart::Identifier(Ident::new("s"))];
            respelled.extend(parts.iter().cloned());
            ControlFlow::Continue(Some(ObjectName(respelled)))
        });

        assert_eq!(
            statements[0].to_string(),
            "SELECT s.ceil(a), s.floor(a, 2), s.substr(b, 1), s.substring(b, 1, 2), \
             SUBSTRING(b FROM 1)"
        );
    }
}
