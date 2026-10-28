//! Submodule definining the `CheckConstraintLike` trait for SQL check
//! constraints.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{borrow::Borrow, fmt::Debug};

use sqlparser::ast::{
    BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Value,
};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{DatabaseLike, Metadata, TableLike, column::ColumnLike, function_like::FunctionLike},
};

fn unnested(mut expr: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expr {
        expr = inner;
    }
    expr
}

fn literal_equality(left: &Expr, right: &Expr) -> Option<bool> {
    let (Expr::Value(left), Expr::Value(right)) = (unnested(left), unnested(right)) else {
        return None;
    };
    match (&left.value, &right.value) {
        (Value::Boolean(left), Value::Boolean(right)) => Some(left == right),
        (Value::Number(left, _), Value::Number(right, _)) => {
            if left == right {
                Some(true)
            } else {
                Some(left.parse::<u128>().ok()? == right.parse::<u128>().ok()?)
            }
        }
        (Value::SingleQuotedString(left), Value::SingleQuotedString(right)) if left == right => {
            Some(true)
        }
        _ => None,
    }
}

pub(crate) fn evaluate_constant_expr(
    expr: &Expr,
    nullable: &mut impl FnMut(&Expr) -> Result<Option<bool>, LookupError>,
) -> Result<Option<bool>, LookupError> {
    let result = match unnested(expr) {
        Expr::Value(value) => {
            match value.value {
                Value::Boolean(value) => Some(value),
                _ => None,
            }
        }
        Expr::IsNull(column) | Expr::IsNotNull(column) => {
            match nullable(unnested(column))? {
                Some(false) => Some(matches!(unnested(expr), Expr::IsNotNull(_))),
                _ => None,
            }
        }
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => literal_equality(left, right),
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            let complement = match (unnested(left), unnested(right)) {
                (Expr::IsNull(left), Expr::IsNotNull(right))
                | (Expr::IsNotNull(left), Expr::IsNull(right)) => {
                    unnested(left) == unnested(right) && nullable(unnested(left))?.is_some()
                }
                _ => false,
            };
            if complement {
                Some(true)
            } else {
                match (
                    evaluate_constant_expr(left, nullable)?,
                    evaluate_constant_expr(right, nullable)?,
                ) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                }
            }
        }
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            match (
                evaluate_constant_expr(left, nullable)?,
                evaluate_constant_expr(right, nullable)?,
            ) {
                (Some(true), Some(true)) => Some(true),
                (Some(false), _) | (_, Some(false)) => Some(false),
                _ => None,
            }
        }
        Expr::UnaryOp { op: sqlparser::ast::UnaryOperator::Not, expr } => {
            evaluate_constant_expr(expr, nullable)?.map(|value| !value)
        }
        _ => None,
    };
    Ok(result)
}

/// Helper to extract column names from nullability checks in an AND chain
fn extract_null_columns(expr: &Expr, is_null: bool) -> Option<Vec<&Ident>> {
    use sqlparser::ast::BinaryOperator;
    match expr {
        Expr::IsNull(col_expr) if is_null => {
            if let Expr::Identifier(ident) = col_expr.as_ref() {
                Some(vec![ident])
            } else {
                None
            }
        }
        Expr::IsNotNull(col_expr) if !is_null => {
            if let Expr::Identifier(ident) = col_expr.as_ref() { Some(vec![ident]) } else { None }
        }
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            let mut left_cols = extract_null_columns(left, is_null)?;
            left_cols.extend(extract_null_columns(right, is_null)?);
            left_cols.sort_unstable();
            left_cols.dedup();
            Some(left_cols)
        }
        Expr::Nested(inner) => extract_null_columns(inner, is_null),
        _ => None,
    }
}

/// Helper to swap comparison operators
fn swap_cmp_op(op: &BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        _ => op.clone(),
    }
}

/// The direction of the length bound we are checking for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundDirection {
    /// Inclusive minimum length (e.g. `LEN > 5` -> 6)
    Lower,
    /// Strict maximum length (e.g. `LEN < 5` -> 5)
    Upper,
}

/// Helper to resolve the global bound for a column.
/// This prevents infinite recursion by tracking visited columns.
fn resolve_global_bound<C>(
    database: &C::DB,
    table: &<C::DB as DatabaseLike>::Table,
    target_col: &str,
    visited_cols: &mut Vec<String>,
    direction: BoundDirection,
) -> Result<Option<usize>, LookupError>
where
    C: CheckConstraintLike,
{
    visited_cols.push(target_col.to_string());

    let mut bound_agg = None;

    for constraint in table.check_constraints(database)? {
        let cc: &C = constraint.borrow();
        let expr = cc.expression(database);
        if let Some(bound) = check_text_length_bound_recursive(
            database,
            expr,
            cc,
            Some(target_col),
            visited_cols,
            direction,
        )? {
            bound_agg = match (bound_agg, direction) {
                (Some(current), BoundDirection::Upper) => Some(core::cmp::min(current, bound)),
                (Some(current), BoundDirection::Lower) => Some(core::cmp::max(current, bound)),
                (None, _) => Some(bound),
            };
        }
    }

    visited_cols.pop();
    Ok(bound_agg)
}

/// Helper to extract length limit from an expression
#[allow(clippy::too_many_arguments)]
fn get_length_bound<C>(
    database: &C::DB,
    func_expr: &Expr,
    op: &BinaryOperator,
    val_expr: &Expr,
    check_constraint: &C,
    target_col: Option<&str>,
    visited_cols: &mut Vec<String>,
    direction: BoundDirection,
) -> Result<Option<usize>, LookupError>
where
    C: CheckConstraintLike,
{
    // Check Operator matches direction
    let is_inclusive = match (direction, op) {
        (BoundDirection::Upper, BinaryOperator::Lt)
        | (BoundDirection::Lower, BinaryOperator::Gt) => false,
        (BoundDirection::Upper, BinaryOperator::LtEq)
        | (BoundDirection::Lower, BinaryOperator::GtEq) => true,
        _ => return Ok(None),
    };

    // Parse Left Side: Function(col_ident)
    let Expr::Function(Function { name, args, .. }) = func_expr else {
        return Ok(None);
    };

    let name_str = name.to_string();
    let valid_funcs = ["length", "len", "char_length", "character_length", "octet_length"];
    if !valid_funcs.iter().any(|&f| name_str.eq_ignore_ascii_case(f)) {
        return Ok(None);
    }

    let args_list = match args {
        FunctionArguments::List(list) => &list.args,
        _ => return Ok(None),
    };
    if args_list.len() != 1 {
        return Ok(None);
    }
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(col_ident))) = &args_list[0]
    else {
        return Ok(None);
    };

    // Check if col_ident matches target_col if specified
    if target_col.is_some_and(|target| col_ident.value != target) {
        return Ok(None);
    }

    // Verify it's a textual column
    if !check_constraint
        .column(database, &col_ident.value, IdentifierCase::AsWritten)?
        .is_some_and(|c| c.is_textual(database))
    {
        return Ok(None);
    }

    // Check Right Side
    // Case 1: Constant Number
    if let Expr::Value(val) = val_expr
        && let Value::Number(num_str, _) = &val.value
        && let Ok(limit) = num_str.parse::<usize>()
    {
        return Ok(match direction {
            BoundDirection::Upper => Some(if is_inclusive { limit + 1 } else { limit }),
            BoundDirection::Lower => Some(if is_inclusive { limit } else { limit + 1 }),
        });
    }

    // Case 2: Another Function Call (Transitive Check)
    if let Expr::Function(Function { name: inner_name, args: inner_args, .. }) = val_expr {
        let inner_name_str = inner_name.to_string();
        if valid_funcs.iter().any(|&f| inner_name_str.eq_ignore_ascii_case(f)) {
            let inner_args_list = match inner_args {
                FunctionArguments::List(list) => &list.args,
                _ => return Ok(None),
            };
            if inner_args_list.len() == 1
                && let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(
                    inner_col_ident,
                ))) = &inner_args_list[0]
            {
                // We need to resolve the bound for `inner_col` with the SAME
                // direction. len(A) < len(B) AND len(B) < 10 =>
                // len(A) < 10 (Upper transitive)
                // len(A) > len(B) AND len(B) > 10 => len(A) > 10 (Lower
                // transitive)

                // Avoid cycles
                if visited_cols.contains(&inner_col_ident.value) {
                    return Ok(None);
                }

                let table = check_constraint.table(database)?;
                if let Some(limit) = resolve_global_bound::<C>(
                    database,
                    table,
                    &inner_col_ident.value,
                    visited_cols,
                    direction,
                )? {
                    return Ok(match direction {
                        BoundDirection::Upper => Some(if is_inclusive { limit + 1 } else { limit }),
                        BoundDirection::Lower => Some(if is_inclusive { limit } else { limit + 1 }),
                    });
                }
            }
        }
    }

    Ok(None)
}

/// Helper function to recursively determine the bound of a text length
/// constraint.
fn check_text_length_bound_recursive<C>(
    database: &C::DB,
    expr: &Expr,
    check_constraint: &C,
    target_col: Option<&str>,
    visited_cols: &mut Vec<String>,
    direction: BoundDirection,
) -> Result<Option<usize>, LookupError>
where
    C: CheckConstraintLike,
{
    match expr {
        Expr::BinaryOp { left, op, right } => {
            // Check direct comparison: func(col) <op> right
            if let Some(bound) = get_length_bound(
                database,
                left,
                op,
                right,
                check_constraint,
                target_col,
                visited_cols,
                direction,
            )? {
                return Ok(Some(bound));
            }
            // Check reversed comparison: right <op> func(col)
            // If checking Upper Bound (func(col) < N), we might see N >
            // func(col). N > func(col) is equivalent to func(col) <
            // N. If `op` is Gt, `swap` is Lt. `get_length_bound`
            // accepts `Lt` for `Upper`.
            if let Some(bound) = get_length_bound(
                database,
                right,
                &swap_cmp_op(op),
                left,
                check_constraint,
                target_col,
                visited_cols,
                direction,
            )? {
                return Ok(Some(bound));
            }

            if matches!(op, BinaryOperator::And) {
                let l = check_text_length_bound_recursive(
                    database,
                    left,
                    check_constraint,
                    target_col,
                    visited_cols,
                    direction,
                )?;
                let r = check_text_length_bound_recursive(
                    database,
                    right,
                    check_constraint,
                    target_col,
                    visited_cols,
                    direction,
                )?;
                return Ok(match (l, r, direction) {
                    // AND + Upper: Minimize (most restrictive limit)
                    (Some(a), Some(b), BoundDirection::Upper) => Some(core::cmp::min(a, b)),
                    // AND + Lower: Maximize (most restrictive minimum)
                    (Some(a), Some(b), BoundDirection::Lower) => Some(core::cmp::max(a, b)),
                    (Some(a), None, _) => Some(a),
                    (None, Some(b), _) => Some(b),
                    _ => None,
                });
            }

            if matches!(op, BinaryOperator::Or) {
                let l = check_text_length_bound_recursive(
                    database,
                    left,
                    check_constraint,
                    target_col,
                    visited_cols,
                    direction,
                )?;
                let r = check_text_length_bound_recursive(
                    database,
                    right,
                    check_constraint,
                    target_col,
                    visited_cols,
                    direction,
                )?;
                return Ok(match (l, r, direction) {
                    // OR + Lower: Minimize (least restrictive minimum)
                    (Some(a), Some(b), BoundDirection::Lower) => Some(core::cmp::min(a, b)),
                    _ => None,
                });
            }
            Ok(None)
        }
        Expr::Nested(inner) => {
            check_text_length_bound_recursive(
                database,
                inner,
                check_constraint,
                target_col,
                visited_cols,
                direction,
            )
        }
        _ => Ok(None),
    }
}

/// Helper function to recursively determine if an expression checks for a
/// not-empty text constraint.
fn check_not_empty_text_recursive<C>(
    database: &C::DB,
    expr: &Expr,
    check_constraint: &C,
) -> Result<bool, LookupError>
where
    C: CheckConstraintLike,
{
    match expr {
        Expr::BinaryOp { left, op, right } => {
            if matches!(op, BinaryOperator::NotEq) {
                for (col_expr, val_expr) in
                    [(left.as_ref(), right.as_ref()), (right.as_ref(), left.as_ref())]
                {
                    if let (Expr::Identifier(ident), Expr::Value(val_wrapper)) =
                        (col_expr, val_expr)
                        && let Value::SingleQuotedString(s) = &val_wrapper.value
                        && s.is_empty()
                        && check_constraint
                            .column(database, &ident.value, IdentifierCase::AsWritten)?
                            .is_some_and(|c| c.is_textual(database))
                    {
                        return Ok(true);
                    }
                }
            }

            if matches!(op, BinaryOperator::And) {
                return Ok(check_not_empty_text_recursive(database, left, check_constraint)?
                    || check_not_empty_text_recursive(database, right, check_constraint)?);
            }

            Ok(false)
        }
        Expr::Nested(inner) => check_not_empty_text_recursive(database, inner, check_constraint),
        _ => Ok(false),
    }
}

/// The constraints among `checks` that are not tautologies, in order.
pub(crate) fn non_tautological<'db, C: CheckConstraintLike>(
    checks: impl Iterator<Item = &'db C>,
    database: &C::DB,
) -> Result<Vec<&'db C>, LookupError> {
    let mut kept = Vec::new();
    for check in checks {
        if !check.is_tautology(database)? {
            kept.push(check);
        }
    }
    Ok(kept)
}

/// A check constraint is a rule that specifies a condition that must be met
/// for data to be inserted or updated in a table. This trait represents such
/// a check constraint in a database-agnostic way.
pub trait CheckConstraintLike:
    Clone
    + Send
    + Sync
    + Eq
    + Ord
    + Debug
    + Metadata
    + Borrow<<<Self as CheckConstraintLike>::DB as DatabaseLike>::CheckConstraint>
{
    /// The type of the database that this column belongs to.
    type DB: DatabaseLike<CheckConstraint: Borrow<Self>>;

    /// Returns the expression of the check constraint as an SQL AST node.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the check
    ///   constraint from.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT CHECK (id > 0), name TEXT CHECK (length(name) > 0));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> =
    ///     table.check_constraints(&db)?.map(|cc| cc.expression(&db).to_string()).collect();
    /// assert_eq!(check_constraints, vec!["id > 0", "length(name) > 0"]);
    /// # Ok(())
    /// # }
    /// ```
    fn expression<'db>(&'db self, database: &'db Self::DB) -> &'db Expr;

    /// Returns whether the check was written `NO INHERIT`, staying on the
    /// table declaring it rather than passing to the tables inheriting from
    /// it.
    ///
    /// Defaults to `false`, which is what every check not spelling the
    /// attribute means.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE par (id INT, CONSTRAINT own CHECK (id > 0) NO INHERIT, CHECK (id < 9));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("par", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [own, passed] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// assert!(own.no_inherit());
    /// assert!(!passed.no_inherit());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn no_inherit(&self) -> bool {
        false
    }

    /// Returns whether the rows the table held when the check arrived have
    /// been checked against it.
    ///
    /// False for a check added `NOT VALID` until `ALTER TABLE ... VALIDATE
    /// CONSTRAINT` runs, matching `pg_constraint.convalidated`. New rows are
    /// checked either way. Defaults to `true`, which is what every check not
    /// added `NOT VALID` means.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table of this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let added = "CREATE TABLE orders (amount INT);
    ///     ALTER TABLE orders ADD CONSTRAINT positive CHECK (amount > 0) NOT VALID;";
    /// for (sql, validated) in [
    ///     (added.to_owned(), false),
    ///     (format!("{added} ALTER TABLE orders VALIDATE CONSTRAINT positive;"), true),
    /// ] {
    ///     let db = ParserDB::parse::<PostgreSqlDialect>(&sql)?;
    ///     let table = db
    ///         .table_by_target(TargetName::new("orders", false), IdentifierCase::AsWritten)?
    ///         .unwrap();
    ///     let check = table.check_constraints(&db)?.next().unwrap();
    ///     assert_eq!(check.is_validated(&db)?, validated);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_validated(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(true)
    }

    /// Returns a reference to the table that the check constraint is defined
    /// on.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, CHECK (id > 0));")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let cc = check_constraints[0];
    /// let table_ref = CheckConstraintLike::table(cc, &db)?;
    /// assert_eq!(table_ref, table);
    /// # Ok(())
    /// # }
    /// ```
    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>;

    /// Iterates over the columns involved in the check constraint.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the columns
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT, CHECK ((id, name) = (1, 'test')), CHECK (length(name) > 0), CHECK (id BETWEEN 1 AND 10), CHECK (id IS NOT NULL));")?;
    /// let table = db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let columns = table.columns(&db)?.collect::<Vec<_>>();
    /// let [id, name] = &columns.as_slice() else {
    ///     panic!("Expected two columns");
    /// };
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4] = &check_constraints.as_slice() else {
    ///     panic!("Expected four check constraints");
    /// };
    /// assert_eq!(cc1.columns(&db)?.collect::<Vec<_>>(), vec![*id, *name]);
    /// assert_eq!(cc2.columns(&db)?.collect::<Vec<_>>(), vec![*name]);
    /// assert_eq!(cc3.columns(&db)?.collect::<Vec<_>>(), vec![*id]);
    /// assert_eq!(cc4.columns(&db)?.collect::<Vec<_>>(), vec![*id]);
    /// # Ok(())
    /// # }
    /// ```
    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>;

    /// Returns the number of columns involved in the check constraint.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the columns
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT CHECK (id > 0), name TEXT CHECK (length(name) > 0));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// assert_eq!(cc1.number_of_columns(&db)?, 1);
    /// assert_eq!(cc2.number_of_columns(&db)?, 1);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn number_of_columns(&self, database: &Self::DB) -> Result<usize, LookupError> {
        Ok(self.columns(database)?.count())
    }

    /// Returns a reference to the requested column by name, if any.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the columns
    ///   from.
    /// * `name` - The name of the column to retrieve.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT CHECK (id > 0), name TEXT CHECK (length(name) > 0));",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let columns = table.columns(&db)?.collect::<Vec<_>>();
    /// let [id, name] = &columns.as_slice() else {
    ///     panic!("Expected two columns");
    /// };
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// let col = cc1.column(&db, "id", IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(col, *id);
    /// assert!(cc2.column(&db, "id", IdentifierCase::AsWritten)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column<'db>(
        &'db self,
        database: &'db Self::DB,
        name: &str,
        case: IdentifierCase,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Column>, LookupError> {
        Ok(self.columns(database)?.find(|column| {
            case.names_stored(column.column_name(), column.column_name_is_quoted(), name)
        }))
    }

    /// Iterates over the functions used in the check constraint.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   functions from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE FUNCTION is_positive(INT) RETURNS BOOLEAN;
    ///        CREATE TABLE my_table (id INT CHECK (is_positive(id)));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc] = &check_constraints.as_slice() else {
    ///     panic!("Expected one check constraint");
    /// };
    /// let functions: Vec<_> = cc.functions(&db)?.collect();
    /// assert_eq!(functions.len(), 1);
    /// assert_eq!(functions[0].name(), "is_positive");
    /// # Ok(())
    /// # }
    /// ```
    fn functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>;

    /// Returns the functions one call in the constraint's expression may
    /// reach, resolved under the search path of the statement that wrote it.
    ///
    /// `call` is a node of [`Self::expression`] at any depth, subqueries
    /// included. Resolution follows the rules of
    /// [`PolicyLike::functions_reached_by`](crate::traits::PolicyLike::functions_reached_by).
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint, and [`LookupError::CallNotInExpression`]
    /// when `call` is not a node of its expression, which is matched by
    /// address.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::Expr, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (name TEXT CHECK (lower(name) = name));",
    /// )?;
    /// let table = db.tables().next().unwrap();
    /// let check = table.check_constraints(&db)?.next().unwrap();
    /// let Expr::BinaryOp { left, .. } = check.expression(&db) else { panic!() };
    /// let Expr::Function(call) = left.as_ref() else { panic!() };
    /// let reached: Vec<_> = check.functions_reached_by(call, &db)?.collect();
    /// assert_eq!(reached.len(), 1);
    /// assert_eq!(reached[0].target_name().to_string(), "pg_catalog.lower");
    /// # Ok(())
    /// # }
    /// ```
    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>;

    /// Returns a reference to the requested function by name, if any.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   functions from.
    /// * `name` - The name of the function to retrieve.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE FUNCTION is_positive(INT) RETURNS BOOLEAN;
    ///        CREATE TABLE my_table (id INT CHECK (is_positive(id)), age INT CHECK (age > 0));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// let func = cc1.function(&db, "is_positive")?.unwrap();
    /// assert_eq!(func.name(), "is_positive");
    /// assert!(cc2.function(&db, "is_positive")?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
        name: &str,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        Ok(self.functions(database)?.find(|f| {
            crate::utils::identifier_resolution::stored_identifier_matches_lookup(
                f.name(),
                f.name_is_quoted(),
                name,
            )
        }))
    }

    /// Returns whether the check constraint involves any functions.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   functions from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE FUNCTION is_positive(INT) RETURNS BOOLEAN;
    ///        CREATE TABLE my_table (id INT CHECK (is_positive(id)), age INT CHECK (age > 0));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// assert!(cc1.has_functions(&db)?);
    /// assert!(!cc2.has_functions(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_functions(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.functions(database)?.next().is_some())
    }

    /// Returns whether the check constraint involves a specific column.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    /// * `column` - A reference to the column to check for involvement.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT CHECK (id > 0), name TEXT CHECK (length(name) > 0));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let columns = table.columns(&db)?.collect::<Vec<_>>();
    /// let [id, name] = &columns.as_slice() else {
    ///     panic!("Expected two columns");
    /// };
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2] = &check_constraints.as_slice() else {
    ///     panic!("Expected two check constraints");
    /// };
    /// assert!(cc1.involves_column(&db, id)?);
    /// assert!(!cc1.involves_column(&db, name)?);
    /// assert!(!cc2.involves_column(&db, id)?);
    /// assert!(cc2.involves_column(&db, name)?);
    /// # Ok(())
    /// # }
    /// ```
    fn involves_column(
        &self,
        database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Result<bool, LookupError> {
        Ok(self.columns(database)?.any(|col| col == column))
    }

    /// Returns whether the check constraint is a tautology (always true).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Implementation Note
    ///
    /// This method recognizes several tautological patterns:
    /// - `CHECK (TRUE)` - literal true
    /// - `CHECK (1 = 1)`, `CHECK (0 = 0)` - equal constant comparisons
    /// - `CHECK (NOT FALSE)` - negated false
    /// - `CHECK (column IS NOT NULL)` for `NOT NULL` columns
    /// - `CHECK (column IS NULL OR column IS NOT NULL)` - always true for any
    ///   column
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         col1 INT NOT NULL,
    ///         col2 INT,
    ///         CHECK (col1 IS NOT NULL),
    ///         CHECK (TRUE),
    ///         CHECK (1 = 1),
    ///         CHECK (NOT FALSE),
    ///         CHECK (col2 IS NOT NULL),
    ///         CHECK (col2 IS NULL OR col2 IS NOT NULL),
    ///         CHECK (col1 IS NULL OR col2 IS NOT NULL)
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4, cc5, cc6, cc7] = &check_constraints.as_slice() else {
    ///     panic!("Expected seven check constraints");
    /// };
    /// assert!(cc1.is_tautology(&db)?); // col1 IS NOT NULL on NOT NULL column
    /// assert!(cc2.is_tautology(&db)?); // TRUE
    /// assert!(cc3.is_tautology(&db)?); // 1 = 1
    /// assert!(cc4.is_tautology(&db)?); // NOT FALSE
    /// assert!(!cc5.is_tautology(&db)?); // col2 IS NOT NULL on nullable column
    /// assert!(cc6.is_tautology(&db)?); // IS NULL OR IS NOT NULL is always true
    /// assert!(!cc7.is_tautology(&db)?); // mixed columns
    /// //
    /// # Ok(())
    /// # }
    /// ```
    fn is_tautology(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.table(database)?.expression_truth(
            database,
            self.expression(database),
            IdentifierCase::AsWritten,
        )? == Some(true))
    }

    /// Returns whether the check constraint is a negation (always false).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Implementation Note
    ///
    /// This method recognizes several negation patterns:
    /// - `CHECK (FALSE)` - literal false
    /// - `CHECK (1 = 0)` - unequal constant comparisons
    /// - `CHECK (NOT TRUE)` - negated true
    /// - `CHECK (column IS NULL)` for `NOT NULL` columns
    /// - `CHECK (len(col) < X AND len(col) > Y)` where X <= Y (contradictory
    ///   length constraints)
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         col1 INT NOT NULL,
    ///         col2 INT,
    ///         s1 TEXT CHECK (length(s1) < 5 AND length(s1) > 10),
    ///         s2 TEXT CHECK (length(s2) < 5 AND length(s2) > 2),
    ///         CHECK (col1 IS NULL),
    ///         CHECK (FALSE),
    ///         CHECK (1 = 0),
    ///         CHECK (NOT TRUE),
    ///         CHECK (col2 IS NULL)
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc_s1, cc_s2, cc1, cc2, cc3, cc4, cc5] = &check_constraints.as_slice() else {
    ///     panic!("Expected seven check constraints");
    /// };
    /// assert!(cc_s1.is_negation(&db)?); // len < 5 AND len > 10 is impossible
    /// assert!(!cc_s2.is_negation(&db)?); // len < 5 AND len > 2 is possible (3, 4)
    /// assert!(cc1.is_negation(&db)?); // col1 IS NULL on NOT NULL column
    /// assert!(cc2.is_negation(&db)?); // FALSE
    /// assert!(cc3.is_negation(&db)?); // 1 = 0
    /// assert!(cc4.is_negation(&db)?); // NOT TRUE
    /// assert!(!cc5.is_negation(&db)?); // col2 IS NULL on nullable column
    /// //
    /// # Ok(())
    /// # }
    /// ```
    fn is_negation(&self, database: &Self::DB) -> Result<bool, LookupError> {
        if self.table(database)?.expression_truth(
            database,
            self.expression(database),
            IdentifierCase::AsWritten,
        )? == Some(false)
        {
            return Ok(true);
        }

        // Check for contradicting length constraints
        if let (Some(upper), Some(lower)) = (
            self.is_upper_bounded_text_constraint(database)?,
            self.is_lower_bounded_text_constraint(database)?,
        ) {
            // upper is exclusive upper bound, lower is inclusive lower bound
            // if lower >= upper, then impossible
            if lower >= upper {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns whether the check constraint is a mutual nullability constraint.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         col1 INT,
    ///         col2 INT,
    ///         col3 INT,
    ///         CHECK ((col1 IS NULL AND col2 IS NULL) OR (col2 IS NOT NULL AND col1 IS NOT NULL)),
    ///         CHECK ((col1 IS NULL AND col2 IS NULL AND col3 IS NULL) OR (col1 IS NOT NULL AND col3 IS NOT NULL AND col2 IS NOT NULL)),
    ///         CHECK ((col3 IS NULL OR col3 IS NULL) OR (col3 IS NOT NULL AND col1 IS NOT NULL)),
    ///         CHECK (col1 IS NOT NULL),
    ///         CHECK (col1 > 0),
    ///         CHECK (col2 < 100)
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4, cc5, cc6] = &check_constraints.as_slice() else {
    ///     panic!("Expected six check constraints");
    /// };
    /// assert!(cc1.is_mutual_nullability_constraint(&db));
    /// assert!(cc2.is_mutual_nullability_constraint(&db));
    /// assert!(!cc3.is_mutual_nullability_constraint(&db));
    /// assert!(!cc4.is_mutual_nullability_constraint(&db));
    /// assert!(!cc5.is_mutual_nullability_constraint(&db));
    /// assert!(!cc6.is_mutual_nullability_constraint(&db));
    /// # Ok(())
    /// # }
    /// ```
    fn is_mutual_nullability_constraint(&self, database: &Self::DB) -> bool {
        use sqlparser::ast::{BinaryOperator, Expr};

        let expr = self.expression(database);

        // Must be an OR expression
        let Expr::BinaryOp { left, op: BinaryOperator::Or, right } = expr else {
            return false;
        };

        // Left side should be all NULL checks, right side all NOT NULL checks
        // (or vice versa)
        if let (Some(null_cols), Some(not_null_cols)) =
            (extract_null_columns(left, true), extract_null_columns(right, false))
        {
            null_cols == not_null_cols && null_cols.len() >= 2
        } else if let (Some(not_null_cols), Some(null_cols)) =
            (extract_null_columns(left, false), extract_null_columns(right, true))
        {
            null_cols == not_null_cols && null_cols.len() >= 2
        } else {
            false
        }
    }

    /// Returns whether the check constraint checks that a textual column is not
    /// empty (i.e., `col <> ''` or `col != ''`).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         text_col TEXT CHECK (text_col <> ''),
    ///         int_col INT CHECK (int_col <> 0),
    ///         desc TEXT CHECK (desc != ''),
    ///         chained TEXT CHECK (chained <> '' AND chained IS NOT NULL),
    ///         chained_or TEXT CHECK (chained_or <> '' OR chained_or = 'default')
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4, cc5] = &check_constraints.as_slice() else {
    ///     panic!("Expected five check constraints");
    /// };
    /// assert!(cc1.is_not_empty_text_constraint(&db)?);
    /// assert!(!cc2.is_not_empty_text_constraint(&db)?);
    /// assert!(cc3.is_not_empty_text_constraint(&db)?);
    /// assert!(cc4.is_not_empty_text_constraint(&db)?);
    /// assert!(!cc5.is_not_empty_text_constraint(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn is_not_empty_text_constraint(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let expr = self.expression(database);
        check_not_empty_text_recursive(database, expr, self)
    }

    /// Returns the upper bound of a text length constraint if the constraint
    /// enforces one.
    ///
    /// The returned value is the first non-accepted length (i.e., the strict
    /// upper bound).
    /// - `length(col) < N` returns `Some(N)`
    /// - `length(col) <= N` returns `Some(N + 1)`
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         s1 TEXT CHECK (length(s1) < 10),
    ///         s2 TEXT CHECK (length(s2) <= 10),
    ///         s3 TEXT CHECK (10 > length(s3)),
    ///         s4 TEXT CHECK (10 >= length(s4)),
    ///         s5 TEXT CHECK (len(s5) < 10 AND len(s5) < 5),
    ///         s6 INT CHECK (s6 < 10),
    ///         s7 TEXT CHECK (length(s7) < length(s1)),
    ///         s8 TEXT CHECK (length(s8) < 10 OR length(s8) < 5),
    ///         s9 TEXT CHECK (length(s9) < length(s10)),
    ///         s10 TEXT CHECK (length(s10) < length(s9)),
    ///         s11 TEXT CHECK (length(s11) < length(s12)),
    ///         s12 TEXT CHECK (length(s12) < length(s13)),
    ///         s13 TEXT CHECK (length(s13) < 10)
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4, cc5, cc6, cc7, cc8, cc9, cc10, cc11, cc12, cc13] =
    ///     &check_constraints.as_slice()
    /// else {
    ///     panic!("Expected thirteen check constraints");
    /// };
    /// assert_eq!(cc1.is_upper_bounded_text_constraint(&db)?, Some(10));
    /// assert_eq!(cc2.is_upper_bounded_text_constraint(&db)?, Some(11));
    /// assert_eq!(cc3.is_upper_bounded_text_constraint(&db)?, Some(10));
    /// assert_eq!(cc4.is_upper_bounded_text_constraint(&db)?, Some(11));
    /// assert_eq!(cc5.is_upper_bounded_text_constraint(&db)?, Some(5));
    /// assert_eq!(cc6.is_upper_bounded_text_constraint(&db)?, None);
    /// assert_eq!(cc7.is_upper_bounded_text_constraint(&db)?, Some(10));
    /// assert_eq!(cc8.is_upper_bounded_text_constraint(&db)?, None);
    /// assert_eq!(cc9.is_upper_bounded_text_constraint(&db)?, None);
    /// assert_eq!(cc10.is_upper_bounded_text_constraint(&db)?, None);
    /// assert_eq!(cc11.is_upper_bounded_text_constraint(&db)?, Some(10));
    /// # Ok(())
    /// # }
    /// ```
    fn is_upper_bounded_text_constraint(
        &self,
        database: &Self::DB,
    ) -> Result<Option<usize>, LookupError> {
        let expr = self.expression(database);
        let mut visited_cols = Vec::new();
        check_text_length_bound_recursive(
            database,
            expr,
            self,
            None,
            &mut visited_cols,
            BoundDirection::Upper,
        )
    }

    /// Returns the lower bound of a text length constraint if the constraint
    /// enforces one.
    ///
    /// The returned value is the inclusive lower bound (i.e., the minimum
    /// accepted length).
    /// - `length(col) > N` returns `Some(N + 1)`
    /// - `length(col) >= N` returns `Some(N)`
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this check constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///         s1 TEXT CHECK (length(s1) > 10),
    ///         s2 TEXT CHECK (length(s2) >= 10),
    ///         s3 TEXT CHECK (10 < length(s3)),
    ///         s4 TEXT CHECK (10 <= length(s4)),
    ///         s5 TEXT CHECK (len(s5) > 10 AND len(s5) > 5),
    ///         s6 INT CHECK (s6 > 10),
    ///         s7 TEXT CHECK (length(s7) > length(s1)),
    ///         s8 TEXT CHECK (length(s8) > 10 OR length(s8) > 5)
    ///     );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> = table.check_constraints(&db)?.collect();
    /// let [cc1, cc2, cc3, cc4, cc5, cc6, cc7, cc8] = &check_constraints.as_slice() else {
    ///     panic!("Expected eight check constraints");
    /// };
    /// assert_eq!(cc1.is_lower_bounded_text_constraint(&db)?, Some(11));
    /// assert_eq!(cc2.is_lower_bounded_text_constraint(&db)?, Some(10));
    /// assert_eq!(cc3.is_lower_bounded_text_constraint(&db)?, Some(11));
    /// assert_eq!(cc4.is_lower_bounded_text_constraint(&db)?, Some(10));
    /// assert_eq!(cc5.is_lower_bounded_text_constraint(&db)?, Some(11));
    /// assert_eq!(cc6.is_lower_bounded_text_constraint(&db)?, None);
    /// assert_eq!(cc7.is_lower_bounded_text_constraint(&db)?, Some(12));
    /// assert_eq!(cc8.is_lower_bounded_text_constraint(&db)?, Some(6));
    /// # Ok(())
    /// # }
    /// ```
    fn is_lower_bounded_text_constraint(
        &self,
        database: &Self::DB,
    ) -> Result<Option<usize>, LookupError> {
        let expr = self.expression(database);
        let mut visited_cols = Vec::new();
        check_text_length_bound_recursive(
            database,
            expr,
            self,
            None,
            &mut visited_cols,
            BoundDirection::Lower,
        )
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use crate::prelude::*;

    #[test]
    fn test_built_in_functions_in_check_constraint() {
        let sql = r"
            CREATE TABLE t (
                col TEXT CHECK (length(col) > 0),
                col2 TEXT CHECK (len(col2) > 0)
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table 't' not found");
        let constraints: Vec<_> =
            table.check_constraints(&db).expect("check_constraints").collect();

        assert_eq!(constraints.len(), 2);

        let cc_length = &constraints[0];
        let functions_length: Vec<_> =
            cc_length.functions(&db).expect("functions").map(|f| f.name().to_string()).collect();
        assert!(
            functions_length.contains(&"length".to_string()),
            "Function 'length' not found. Found: {functions_length:?}"
        );

        let cc_len = &constraints[1];
        let functions_len: Vec<_> =
            cc_len.functions(&db).expect("functions").map(|f| f.name().to_string()).collect();
        assert!(
            functions_len.contains(&"len".to_string()),
            "Function 'len' not found. Found: {functions_len:?}"
        );
    }

    #[test]
    fn test_extended_built_in_functions() {
        let sql = r"
            CREATE TABLE t (
                c1 TIMESTAMP CHECK (c1 <= now()),
                c2 TEXT CHECK (coalesce(c2, 'default') = 'default'),
                c3 INT CHECK (c3 > 0)
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table 't' not found");
        let constraints: Vec<_> =
            table.check_constraints(&db).expect("check_constraints").collect();

        // Find constraint with 'now'
        let has_now = constraints
            .iter()
            .any(|cc| cc.functions(&db).expect("functions").any(|f| f.name() == "now"));
        assert!(has_now, "Function 'now' not found in constraints");

        // Find constraint with 'coalesce'
        let has_coalesce = constraints
            .iter()
            .any(|cc| cc.functions(&db).expect("functions").any(|f| f.name() == "coalesce"));
        assert!(has_coalesce, "Function 'coalesce' not found in constraints");
    }

    #[test]
    fn test_uuid_functions() {
        let sql = r"
            CREATE TABLE t (
                c1 UUID CHECK (c1 = gen_random_uuid()),
                c2 UUID CHECK (c2 = uuidv4()),
                c3 UUID CHECK (c3 = uuidv7()),
                c4 UUID CHECK (c4 = uuidv7('10 minutes'::INTERVAL))
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table 't' not found");
        let constraints: Vec<_> =
            table.check_constraints(&db).expect("check_constraints").collect();
        assert_eq!(constraints.len(), 4);

        let functions: Vec<_> = constraints
            .iter()
            .flat_map(|cc| cc.functions(&db).expect("functions").map(|f| f.name().to_string()))
            .collect();

        assert!(functions.contains(&"gen_random_uuid".to_string()));
        assert!(functions.contains(&"uuidv4".to_string()));
        assert!(functions.contains(&"uuidv7".to_string()));
    }

    #[test]
    fn test_check_constraint_function_lookup_respects_quoted_identifiers() {
        let sql = r#"
            CREATE FUNCTION "FooBar"(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
            CREATE TABLE t (id INT CHECK ("FooBar"(id)));
        "#;
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table 't' not found");
        let constraint = table
            .check_constraints(&db)
            .expect("check_constraints")
            .next()
            .expect("Expected one check constraint on table 't'");

        assert!(constraint.function(&db, "\"FooBar\"").expect("function lookup").is_some());
        assert!(constraint.function(&db, "foobar").expect("function lookup").is_none());
        assert!(constraint.function(&db, "\"foobar\"").expect("function lookup").is_none());
    }

    #[test]
    fn test_check_constraint_function_lookup_respects_unquoted_folding() {
        let sql = r"
            CREATE FUNCTION foobar(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
            CREATE TABLE t (id INT CHECK (foobar(id)));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table 't' not found");
        let constraint = table
            .check_constraints(&db)
            .expect("check_constraints")
            .next()
            .expect("Expected one check constraint on table 't'");

        assert!(constraint.function(&db, "foobar").expect("function lookup").is_some());
        assert!(constraint.function(&db, "FOOBAR").expect("function lookup").is_some());
        assert!(constraint.function(&db, "\"FOOBAR\"").expect("function lookup").is_none());
    }
}
