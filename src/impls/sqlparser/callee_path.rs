//! Whether the functions a call reaches may leave `search_path` changed for
//! the statements of the caller after the call.
//!
//! PostgreSQL restores the path on exit from a function with its own
//! `SET search_path` clause, which undoes a `SET LOCAL` or
//! `set_config(..., true)` the function ran, but keeps a plain `SET`, a
//! `RESET` and `set_config(..., false)` after the function ends. A function
//! with no such clause leaves every change in place until the transaction
//! ends. A function changes the path for its caller when its body, or a
//! function its calls reach, makes such a change, runs code outside the
//! body, or builds SQL at run time.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{ContextModifier, Set, Statement, Visit};
use sqlparser_plpgsql::embedded::Dynamic;

use super::{
    data_statement::{runs_code_outside, sets_search_path_past_exit},
    function_body::{SqlBody, hex_strings_may_be_integers, sql_body},
    plpgsql_body::{constant_command, each_block_sql},
};
use crate::{
    structs::EmbeddedSql,
    traits::{DatabaseLike, FunctionLike},
    utils::{
        call_sites::{Call, for_each_call},
        identifier_resolution::identifiers_match,
        object_name::{
            PG_CATALOG, Qualifier, object_name_last_part, qualifier_of, search_path_change,
        },
    },
};

/// Whether a call in `node` may leave `search_path` changed for the
/// statements after it.
///
/// A `pg_catalog` function keeps the path, `set_config` aside, which
/// [`sets_search_path_past_exit`] reads. Any other function keeps it only
/// when its body is in `database` and neither it nor a function its calls
/// reach changes the path past its own end. A call no function the database
/// holds answers, or one whose name or schema only run time knows, may run
/// any code. PostgreSQL picks among overloads by argument type, which is not
/// inferred here, so every overload accepting that many arguments counts.
pub(crate) fn calls_path_changer<N: Visit + ?Sized, DB: DatabaseLike>(
    node: &N,
    database: &DB,
) -> bool {
    CalleeWalk { database, entered: Vec::new() }.calls_changer(node)
}

/// A walk through the bodies of the functions calls reach.
struct CalleeWalk<'db, DB: DatabaseLike> {
    database: &'db DB,
    /// The functions whose bodies the walk has entered. A function met again
    /// counts as keeping the path, since the walk reads its body once and
    /// any change that body makes answers there.
    entered: Vec<&'db DB::Function>,
}

impl<'db, DB: DatabaseLike> CalleeWalk<'db, DB> {
    /// Whether a call in `node` may leave the path changed for its caller.
    fn calls_changer<N: Visit + ?Sized>(&mut self, node: &N) -> bool {
        for_each_call(node, |call| {
            if self.call_changes(call) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        })
        .is_break()
    }

    /// Whether `call` may reach a function that leaves the path changed.
    fn call_changes(&mut self, call: Call<'_>) -> bool {
        match reach(call, self.database) {
            Reach::Any => true,
            Reach::Catalog => false,
            Reach::Functions(functions) => {
                functions.is_empty()
                    || functions.into_iter().any(|function| {
                        !declared_in_catalog(function) && !self.keeps_path(function)
                    })
            }
        }
    }

    /// Whether `function` leaves its caller's `search_path` as it found it.
    fn keeps_path(&mut self, function: &'db DB::Function) -> bool {
        if self.entered.iter().any(|entered| core::ptr::eq(*entered, function)) {
            return true;
        }
        self.entered.push(function);
        let Ok(own_path) = function.search_path(self.database) else {
            return false;
        };
        let Ok(body) = sql_body(function, self.database) else {
            return false;
        };
        let restores = own_path.is_some();
        let hex_strings_may_be_integers = hex_strings_may_be_integers(self.database);
        match &body {
            SqlBody::Statements(statements) | SqlBody::Atomic(statements) => {
                statements.iter().all(|statement| {
                    self.statement_keeps(statement, restores, hex_strings_may_be_integers)
                })
            }
            SqlBody::PlPgSql(block) => {
                each_block_sql(block, &mut |sql, command: Option<&Dynamic>| {
                    let keeps = match (command, sql) {
                        (Some(dynamic), _) => {
                            constant_command(dynamic).is_some_and(|statements| {
                                statements.iter().all(|statement| {
                                    self.statement_keeps(
                                        statement,
                                        restores,
                                        hex_strings_may_be_integers,
                                    )
                                })
                            })
                        }
                        (None, EmbeddedSql::Statement(statement)) => {
                            self.statement_keeps(statement, restores, hex_strings_may_be_integers)
                        }
                        (None, EmbeddedSql::Expression(query)) => self.sql_keeps(query, restores),
                    };
                    if keeps { ControlFlow::Continue(()) } else { ControlFlow::Break(()) }
                })
                .is_continue()
            }
            SqlBody::Expression(expression) => self.sql_keeps(*expression, restores),
            SqlBody::Select(select) => self.sql_keeps(*select, restores),
            SqlBody::Return(expression) => self.sql_keeps(expression.as_ref(), restores),
        }
    }

    /// Whether `statement`, run by a function that restores its own path on
    /// exit when `restores`, leaves the caller's path alone.
    fn statement_keeps(
        &mut self,
        statement: &Statement,
        restores: bool,
        hex_strings_may_be_integers: bool,
    ) -> bool {
        if runs_code_outside(statement) {
            return false;
        }
        match search_path_change(statement, hex_strings_may_be_integers) {
            Ok(None) => self.sql_keeps(statement, restores),
            Ok(Some(_)) | Err(_) => {
                restores
                    && matches!(
                        statement,
                        Statement::Set(Set::SingleAssignment {
                            scope: Some(ContextModifier::Local),
                            ..
                        })
                    )
            }
        }
    }

    /// Whether the calls in `node`, run by a function that restores its own
    /// path on exit when `restores`, leave the caller's path alone.
    fn sql_keeps<N: Visit + ?Sized>(&mut self, node: &N, restores: bool) -> bool {
        !sets_search_path_past_exit(node, restores) && !self.calls_changer(node)
    }
}

/// The functions a call may run.
enum Reach<'db, F> {
    /// Any function, its name or schema built at run time.
    Any,
    /// Only `pg_catalog`'s, the schema the call names.
    Catalog,
    /// One of these, every overload of the name accepting the call's
    /// argument count in any schema the call may name, none when the
    /// database holds no such function.
    Functions(Vec<&'db F>),
}

/// The functions `call` may run among those `database` holds. PostgreSQL
/// picks among overloads by argument type, which is not inferred here, so
/// every overload accepting that many arguments counts.
fn reach<'db, DB: DatabaseLike>(call: Call<'_>, database: &'db DB) -> Reach<'db, DB::Function> {
    let Some((called, called_quoted)) = object_name_last_part(call.name) else {
        return Reach::Any;
    };
    let schema = match qualifier_of(call.name) {
        // A name built at run time may name any function.
        Qualifier::RunTime => return Reach::Any,
        Qualifier::Named(schema, quoted)
            if identifiers_match(schema, quoted, PG_CATALOG, false) =>
        {
            return Reach::Catalog;
        }
        Qualifier::Named(schema, quoted) => Some((schema, quoted)),
        Qualifier::Absent => None,
    };
    let functions = database
        .functions_named(called)
        .filter(|function| {
            let declared = function.target_name();
            // A function stored without a schema is in `public`.
            let (declared_schema, declared_quoted) = declared
                .schema()
                .map_or(("public", false), |name| (name, declared.schema_is_quoted()));
            identifiers_match(function.name(), function.name_is_quoted(), called, called_quoted)
                && function.accepts_argument_count(call.arguments)
                && schema.is_none_or(|(schema, quoted)| {
                    identifiers_match(declared_schema, declared_quoted, schema, quoted)
                })
        })
        .collect();
    Reach::Functions(functions)
}

/// Whether `function` is declared in `pg_catalog`.
fn declared_in_catalog<F: FunctionLike>(function: &F) -> bool {
    let declared = function.target_name();
    declared.schema().is_some_and(|schema| {
        identifiers_match(schema, declared.schema_is_quoted(), PG_CATALOG, false)
    })
}

/// The name of the first call in `node` that may run a function outside
/// `pg_catalog`, or that no function `database` holds answers.
pub(crate) fn call_outside_catalog<N: Visit + ?Sized, DB: DatabaseLike>(
    node: &N,
    database: &DB,
) -> Option<String> {
    let outside = for_each_call(node, |call| {
        let outside = match reach(call, database) {
            Reach::Any => true,
            Reach::Catalog => false,
            Reach::Functions(functions) => {
                functions.is_empty() || !functions.into_iter().all(declared_in_catalog)
            }
        };
        if outside { ControlFlow::Break(call.name.to_string()) } else { ControlFlow::Continue(()) }
    });
    match outside {
        ControlFlow::Break(name) => Some(name),
        ControlFlow::Continue(()) => None,
    }
}
