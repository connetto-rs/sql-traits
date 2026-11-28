//! The values a data-modifying statement stores in columns and how
//! PostgreSQL converts each to its column's type.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{
    Assignment, AssignmentTarget, DoUpdate, Expr, Ident, Insert, MergeAction, MergeClause,
    MergeInsertExpr, MergeInsertKind, MergeUpdateExpr, MergeUpdateKind, ObjectName, OnConflict,
    OnConflictAction, OnInsert, Query, SetExpr, Statement,
};

use super::{ExpressionContext, plain_arguments, unparenthesized, unquoted_name};
use crate::{
    errors::LookupError,
    structs::{OperandEvidence, OperandType, QueryOutput},
    traits::DatabaseLike,
    utils::{
        call_typing::typed_input,
        coercion::{Context, assignment, type_name},
    },
};

/// Where a value a data-modifying statement stores comes from.
#[derive(Debug, Clone, Copy)]
pub enum AssignedValue<'node> {
    /// An expression the statement writes for the column: a cell of the
    /// `VALUES` row an `INSERT` or a `MERGE` inserts, the value of a `SET`
    /// item, or an element of the row a multi-column `SET` item writes.
    Expression(&'node Expr),
    /// An output column of a query: of the query an `INSERT` reads its rows
    /// from, or of the subquery a multi-column `SET` item reads.
    Output {
        /// The query.
        query: &'node Query,
        /// The position of the output column, from 0.
        position: usize,
    },
}

/// What PostgreSQL does with a value a statement stores in a column.
#[derive(Debug, Clone)]
pub enum AssignmentResolution<'db, DB: DatabaseLike> {
    /// PostgreSQL converts the value to the column's type by the steps of
    /// the evidence, the length coercion the column's modifier needs last.
    Converted(OperandEvidence<'db, DB>),
    /// PostgreSQL refuses the statement, since no conversion it applies by
    /// assignment reaches the column's type.
    Refused,
    /// The type of the value or of the column, or a cast or a type fact the
    /// conversion needs, is missing.
    Undetermined,
}

/// A value a data-modifying statement stores in a column of the relation it
/// writes, as [`ExpressionContext::assignments`] answers it.
#[derive(Debug, Clone)]
pub struct ColumnAssignment<'s, 'node, 'db, DB: DatabaseLike> {
    column: (&'s str, bool),
    value: AssignedValue<'node>,
    value_type: OperandType<'s>,
    column_type: OperandType<'s>,
    resolution: AssignmentResolution<'db, DB>,
}

impl<'s, 'node, 'db, DB: DatabaseLike> ColumnAssignment<'s, 'node, 'db, DB> {
    /// Returns the name of the column, as stored, and whether it is quoted.
    #[must_use]
    pub const fn column(&self) -> (&'s str, bool) {
        self.column
    }

    /// Returns where the value comes from.
    #[must_use]
    pub const fn value(&self) -> AssignedValue<'node> {
        self.value
    }

    /// Returns the type of the value, as [`ExpressionContext::type_of`]
    /// answers it, an untyped literal an `INSERT ... SELECT` outputs staying
    /// untyped as PostgreSQL leaves it for the column to read.
    #[must_use]
    pub const fn value_type(&self) -> &OperandType<'s> {
        &self.value_type
    }

    /// Returns the type of the column, with its modifier, unknown for a
    /// value stored in a field or an element of the column.
    #[must_use]
    pub const fn column_type(&self) -> &OperandType<'s> {
        &self.column_type
    }

    /// Returns what PostgreSQL does to store the value.
    #[must_use]
    pub const fn resolution(&self) -> &AssignmentResolution<'db, DB> {
        &self.resolution
    }
}

/// A column of the relation a statement writes, by its stored name, its
/// quoting and its type.
type Column<'s> = (&'s str, bool, OperandType<'s>);

/// Where a written name stores its value: the position of the column it
/// names and whether it names a field or an element of that column.
type Target = (usize, bool);

/// What a data-modifying statement writes values from.
enum Writes<'node> {
    /// An `INSERT`, with its `ON CONFLICT DO UPDATE`.
    Insert(&'node Insert),
    /// The `SET` list of an `UPDATE`.
    Set(&'node [Assignment]),
    /// The clauses of a `MERGE`.
    Merge(&'node [MergeClause]),
}

impl<'db, DB: DatabaseLike> ExpressionContext<'_, 'db, DB> {
    /// Returns the values the data-modifying `statement` stores in columns
    /// of the relation it writes, in the order it writes them, each with the
    /// column's type and what PostgreSQL does to store it, as
    /// `transformAssignedExpr` converts it under `COERCION_ASSIGNMENT`.
    ///
    /// An `INSERT` stores each cell of its `VALUES` rows, or each output of
    /// the query it reads its rows from, in the column its column list names
    /// at that position, or the relation's column there when it lists none.
    /// A `VALUES` list carrying `WITH`, `ORDER BY`, `LIMIT`, `OFFSET`,
    /// `FETCH` or a locking clause is such a query. Its `ON CONFLICT DO
    /// UPDATE`, an `UPDATE` and the `UPDATE` of a `MERGE` store each `SET`
    /// value in the column it names, a multi-column item each element of its
    /// row or each output of its subquery, and the `INSERT` of a `MERGE` each
    /// cell of its row. `DEFAULT` stores the column's default, which no value
    /// here stands for. A name no column of the relation takes, the outputs
    /// past a wildcard whose columns are not enumerated, a `ROW` passing a
    /// `*` and a multi-column item reading neither a `ROW` nor a subquery,
    /// which PostgreSQL refuses, store nothing here.
    ///
    /// Assignment converts by implicit and assignment casts, converts any
    /// value to a string type through text and ends with the length coercion
    /// the column's modifier needs, as [`CoercionStep`] lists the steps. The
    /// answer is `None` for a statement writing no column, or one whose
    /// target relation, a table, a foreign table or a plain view, is not
    /// known.
    ///
    /// [`CoercionStep`]: crate::structs::CoercionStep
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a statement the
    /// context was not built over, and the lookup errors typing a value or a
    /// column returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT4, b INT8);")?;
    /// let statement = Parser::parse_sql(&PostgreSqlDialect {}, "UPDATE t SET a = b")?.remove(0);
    /// let scope = ExpressionScope::Empty;
    /// let context = ExpressionContext::for_statement(&statement, &scope, &db, IdentifierCase::Folded)?;
    ///
    /// let assignments = context.assignments(&statement)?.expect("`t` is known");
    /// let [assignment] = assignments.as_slice() else { unreachable!() };
    /// assert_eq!(assignment.column(), ("a", false));
    /// // `b` converts to `int4` by the assignment cast `int4(int8)`.
    /// let AssignmentResolution::Converted(evidence) = assignment.resolution() else { unreachable!() };
    /// assert_eq!(evidence.target().name(), "int4");
    /// assert!(matches!(evidence.conversion(), [CoercionStep::Function(cast)]
    ///     if cast.source_type(&db).name() == "int8"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn assignments<'s, 'node>(
        &'s self,
        statement: &'node Statement,
    ) -> Result<Option<Vec<ColumnAssignment<'s, 'node, 'db, DB>>>, LookupError> {
        if !self.scope.modifies(statement) {
            return Err(LookupError::ExpressionNotInQuery { expression: statement.to_string() });
        }
        let writes = match statement {
            Statement::Insert(insert) => Writes::Insert(insert),
            Statement::Update(update) => Writes::Set(&update.assignments),
            Statement::Merge(merge) => Writes::Merge(&merge.clauses),
            // A `DELETE` writes no column.
            _ => return Ok(None),
        };
        let Some(written) = self.scope.written_columns(statement) else { return Ok(None) };
        let mut columns = Vec::new();
        for (name, quoted, definition) in written {
            columns.push((name, quoted, self.definition_type(definition)?));
        }
        let mut stored = Stored { context: self, columns: &columns, assignments: Vec::new() };
        match writes {
            Writes::Insert(insert) => stored.insert(insert)?,
            Writes::Set(assignments) => stored.set(assignments)?,
            Writes::Merge(clauses) => stored.merge(clauses)?,
        }
        Ok(Some(stored.assignments))
    }
}

/// The values one statement stores, gathered in the order it writes them.
struct Stored<'s, 'c, 'node, 'query, 'db, DB: DatabaseLike> {
    context: &'s ExpressionContext<'query, 'db, DB>,
    columns: &'c [Column<'s>],
    assignments: Vec<ColumnAssignment<'s, 'node, 'db, DB>>,
}

impl<'s, 'node, 'db, DB: DatabaseLike> Stored<'s, '_, 'node, '_, 'db, DB> {
    /// The values of an `INSERT` and of its `ON CONFLICT DO UPDATE`.
    fn insert(&mut self, insert: &'node Insert) -> Result<(), LookupError> {
        let targets = self.targets(&insert.columns);
        // `DEFAULT VALUES` stores every default, which no value stands for.
        if let Some(source) = insert.source.as_deref() {
            match direct_values(source) {
                Some(rows) => {
                    for row in rows {
                        self.row(&targets, row.iter())?;
                    }
                }
                None => self.outputs(&targets, source)?,
            }
        }
        if let Some(OnInsert::OnConflict(OnConflict {
            action: OnConflictAction::DoUpdate(DoUpdate { assignments, .. }),
            ..
        })) = &insert.on
        {
            self.set(assignments)?;
        }
        Ok(())
    }

    /// The values the `UPDATE` and `INSERT` actions of `MERGE` clauses store.
    fn merge(&mut self, clauses: &'node [MergeClause]) -> Result<(), LookupError> {
        for clause in clauses {
            match &clause.action {
                MergeAction::Update(MergeUpdateExpr {
                    kind: MergeUpdateKind::Set(assignments),
                    ..
                }) => self.set(assignments)?,
                MergeAction::Insert(MergeInsertExpr {
                    columns,
                    kind: MergeInsertKind::Values(values),
                    ..
                }) => {
                    let targets = self.targets(columns);
                    for row in &values.rows {
                        self.row(&targets, row.iter())?;
                    }
                }
                // `DELETE`, `DO NOTHING` and `INSERT DEFAULT VALUES` store no
                // value.
                _ => {}
            }
        }
        Ok(())
    }

    /// The values of the items of a `SET` list.
    fn set(&mut self, assignments: &'node [Assignment]) -> Result<(), LookupError> {
        for Assignment { target, value } in assignments {
            match target {
                AssignmentTarget::ColumnName(name) => {
                    let target = self.target(name);
                    self.expression(target, value)?;
                }
                AssignmentTarget::Tuple(names) => {
                    let targets: Vec<_> = names.iter().map(|name| self.target(name)).collect();
                    match unparenthesized(value) {
                        Expr::Subquery(query) => self.outputs(&targets, query)?,
                        Expr::Tuple(elements) => self.row(&targets, elements.iter())?,
                        Expr::Function(function)
                            if unquoted_name(function)
                                .is_some_and(|name| name.eq_ignore_ascii_case("row")) =>
                        {
                            if let Some(elements) = plain_arguments(function) {
                                self.row(&targets, elements.into_iter())?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Where the names of a column list store their values, the relation's
    /// columns in order when the list is empty.
    fn targets(&self, names: &[ObjectName]) -> Vec<Option<Target>> {
        if names.is_empty() {
            (0..self.columns.len()).map(|position| Some((position, false))).collect()
        } else {
            names.iter().map(|name| self.target(name)).collect()
        }
    }

    /// Where the written column name `name` stores its value, `None` when no
    /// column of the relation takes it.
    fn target(&self, name: &ObjectName) -> Option<Target> {
        let mut parts = name.0.iter();
        let Ident { value, quote_style, .. } = parts.next()?.as_ident()?;
        let case = self.context.case;
        let position = self.columns.iter().position(|(stored, quoted, _)| {
            case.identifiers_match(stored, *quoted, value, quote_style.is_some())
        })?;
        Some((position, parts.next().is_some()))
    }

    /// The values a row of expressions stores, one per target in order.
    fn row(
        &mut self,
        targets: &[Option<Target>],
        row: impl Iterator<Item = &'node Expr>,
    ) -> Result<(), LookupError> {
        for (target, value) in targets.iter().zip(row) {
            self.expression(*target, value)?;
        }
        Ok(())
    }

    /// The value the expression `value` stores in `target`.
    fn expression(
        &mut self,
        target: Option<Target>,
        value: &'node Expr,
    ) -> Result<(), LookupError> {
        if is_default(value) {
            return Ok(());
        }
        let value_type = self.context.type_of(value)?;
        self.push(target, AssignedValue::Expression(value), value_type);
        Ok(())
    }

    /// The values the outputs of `query` store, one per target in order, up
    /// to a wildcard whose columns are not enumerated.
    fn outputs(
        &mut self,
        targets: &[Option<Target>],
        query: &'node Query,
    ) -> Result<(), LookupError> {
        let context = self.context;
        for (position, (target, output)) in
            targets.iter().zip(context.query_outputs(query)).enumerate()
        {
            let QueryOutput::Column { definition, .. } = output else { break };
            let value_type = context.definition_type(definition)?;
            self.push(*target, AssignedValue::Output { query, position }, value_type);
        }
        Ok(())
    }

    /// Records the value `value` of type `value_type` stored in `target`.
    fn push(
        &mut self,
        target: Option<Target>,
        value: AssignedValue<'node>,
        value_type: OperandType<'s>,
    ) {
        let Some((position, indirect)) = target else { return };
        let (name, quoted, declared) = &self.columns[position];
        let column_type = if indirect { OperandType::Unknown } else { declared.clone() };
        let resolution = self.resolution(&value_type, &column_type);
        self.assignments.push(ColumnAssignment {
            column: (name, *quoted),
            value,
            value_type,
            column_type,
            resolution,
        });
    }

    /// What PostgreSQL does to store a value of type `value` in a column of
    /// type `column`.
    fn resolution(
        &self,
        value: &OperandType<'_>,
        column: &OperandType<'_>,
    ) -> AssignmentResolution<'db, DB> {
        let database = self.context.database;
        let Some(((source, target), name)) =
            typed_input(database, value).zip(typed_input(database, column)).and_then(|types| {
                let name = type_name(database, &types.1.0)?;
                Some((types, name))
            })
        else {
            return AssignmentResolution::Undetermined;
        };
        match assignment(
            database,
            (&source.0, source.1),
            (&target.0, target.1),
            Context::Assignment,
        ) {
            Ok(Some(steps)) => {
                AssignmentResolution::Converted(OperandEvidence::new(name, false, steps))
            }
            Ok(None) => AssignmentResolution::Refused,
            Err(_) => AssignmentResolution::Undetermined,
        }
    }
}

/// The rows of `source` when it is a `VALUES` list whose cells an `INSERT`
/// stores one by one, which PostgreSQL does unless the list carries a clause
/// of a query.
pub(super) fn direct_values(source: &Query) -> Option<impl Iterator<Item = &[Expr]>> {
    let plain = source.with.is_none()
        && source.order_by.is_none()
        && source.limit_clause.is_none()
        && source.fetch.is_none()
        && source.locks.is_empty();
    match source.body.as_ref() {
        SetExpr::Values(values) if plain => {
            Some(values.rows.iter().map(|row| row.content.as_slice()))
        }
        _ => None,
    }
}

/// Whether `expression` is the `DEFAULT` keyword a value list or a `SET`
/// item writes.
fn is_default(expression: &Expr) -> bool {
    matches!(expression, Expr::Identifier(Ident { value, quote_style: None, .. })
        if value.eq_ignore_ascii_case("default"))
}
