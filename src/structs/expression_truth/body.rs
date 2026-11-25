//! What the expressions of a PL/pgSQL body evaluate to, read through the
//! value guarantees of `sqlparser-plpgsql-dataflow` over the database.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::{
    ast::{
        ArgMode, DataType, Expr, Function, FunctionCalledOnNull, Ident, Statement, TriggerEvent,
        TriggerObject, TriggerObjectKind, TriggerPeriod,
    },
    dialect::PostgreSqlDialect,
};
use sqlparser_expr_eval::{
    self as eval, Callee, Evaluation, PostgreSqlOptions, Resolved, ScalarType, Unlisted, Values,
};
use sqlparser_plpgsql::namespace::{Name, NameKind, TriggerVariable};
use sqlparser_plpgsql_dataflow::values;

use super::{Site, Source, TruthContext, validated_checks};
use crate::{
    errors::LookupError,
    impls::BodyNames,
    structs::{
        EmbeddedSql, ExpressionContext, IdentifierCase, OperandType, PlPgSqlSite, PlPgSqlVariable,
        VariableKind, plpgsql_flow::is_record,
    },
    traits::{CheckConstraintLike, DatabaseLike, FunctionLike, Relation, TableLike, TriggerLike},
};

/// The database as the value guarantees domain asks for it over one
/// PL/pgSQL body: the calls and the names no variable answers bound where
/// each site of the body writes them, and what its variables hold by their
/// types and as the body starts.
pub(crate) struct BodyTruth<'b, 'db, DB: DatabaseLike> {
    truth: TruthContext<'db, DB>,
    names: &'b BodyNames<'db, DB>,
}

impl<'b, 'db, DB: DatabaseLike> BodyTruth<'b, 'db, DB> {
    /// The context of the body `names` describes, binding what the `SELECT`
    /// of each of `sites` writes outside the queries nested in it, its
    /// identifiers compared under `case`.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors building the context of a
    /// site runs into, and the lookup errors resolving a reference or a call
    /// one of them writes.
    pub(crate) fn new(
        names: &'b BodyNames<'db, DB>,
        sites: &[PlPgSqlSite<'_, 'db, DB>],
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        let mut truth = TruthContext::new(names.database(), case, None);
        let mut site = Site::empty();
        for each in sites {
            let query = match each.sql() {
                EmbeddedSql::Expression(query) => query,
                EmbeddedSql::Statement(Statement::Query(query)) => &**query,
                EmbeddedSql::Statement(_) => continue,
            };
            truth.bind_nodes(&each.context(case)?, query, 1, &mut site)?;
        }
        truth.site = Some(site);
        truth.source = Source::Site;
        Ok(Self { truth, names })
    }

    /// Binds what `expression`, a node of `context`'s root, writes outside
    /// the queries nested in it.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// root, and the lookup errors resolving a reference or a call it
    /// writes.
    pub(crate) fn bind(
        &mut self,
        context: &ExpressionContext<'_, 'db, DB>,
        expression: &Expr,
    ) -> Result<(), LookupError> {
        let mut site = self.truth.site.take().unwrap_or_else(Site::empty);
        let bound = self.truth.bind_site(context, expression, &mut site);
        self.truth.site = Some(site);
        bound
    }

    /// Any value of the type `operand`, or `NULL`.
    fn any_of(&self, operand: &OperandType<'_>) -> Evaluation {
        self.truth.engine.operand(operand, self.truth.database, &self.truth.enums)
    }

    /// What the column `field`, as PostgreSQL stores its name, of a row of
    /// `table` holds by its type alone, `NULL` included. Reading a column the
    /// table does not have raises.
    fn column_of(&mut self, table: &'db DB::Table, field: &str) -> Result<Evaluation, LookupError> {
        let Some(column) = stored_column(table, field, self.truth.database)? else {
            return Ok(Evaluation::raising(ScalarType::Unknown));
        };
        let truth = &mut self.truth;
        let evaluation =
            truth.engine.column(column, truth.database, &mut truth.collations, &truth.enums)?;
        Ok(evaluation.with_null(true))
    }

    /// What the field `field` of `variable`, a row of a table or of the
    /// relations the triggers executing the function fire on, holds by its
    /// type, and `None` for a variable of any other type.
    fn row_field(
        &mut self,
        name: &Name,
        variable: &PlPgSqlVariable<'_>,
        field: &str,
    ) -> Result<Option<Evaluation>, LookupError> {
        let database = self.truth.database;
        if matches!(name.kind(), NameKind::Trigger(TriggerVariable::New | TriggerVariable::Old)) {
            let mut joined: Option<Evaluation> = None;
            for trigger in self.names.function().triggers(database) {
                let evaluation = match trigger.relation(database)? {
                    Relation::Table(table) | Relation::ForeignTable(table) => {
                        self.column_of(table, field)?
                    }
                    Relation::View(_) | Relation::MaterializedView(_) | Relation::Sequence(_) => {
                        any(ScalarType::Unknown)
                    }
                };
                joined = Some(match joined {
                    Some(held) => held.join(&evaluation),
                    None => evaluation,
                });
            }
            return Ok(joined);
        }
        if let Ok(OperandType::Known { type_name, .. }) = variable.data_type()
            && let Some(table) =
                database.table_by_target(type_name.clone(), database.identifier_case())?
        {
            return self.column_of(table, field).map(Some);
        }
        Ok(None)
    }

    /// What `NEW` or `OLD`, `row`, or its column `fields` name, holds as the
    /// body starts, joined over every trigger executing the function. `None`
    /// when no trigger does, for a trigger on a relation other than a table,
    /// a column the table does not have and a field of a field.
    fn trigger_row(
        &mut self,
        name: &Name,
        row: TriggerVariable,
        fields: &[String],
    ) -> Result<Option<Evaluation>, LookupError> {
        let database = self.truth.database;
        let whole = self.variable_type(name, &[])?;
        let mut joined: Option<Evaluation> = None;
        for trigger in self.names.function().triggers(database) {
            let table = match trigger.relation(database)? {
                Relation::Table(table) | Relation::ForeignTable(table) => table,
                Relation::View(_) | Relation::MaterializedView(_) | Relation::Sequence(_) => {
                    return Ok(None);
                }
            };
            let timing = trigger.timing();
            let per_row = matches!(
                trigger.orientation(),
                Some(
                    TriggerObjectKind::For(TriggerObject::Row)
                        | TriggerObjectKind::ForEach(TriggerObject::Row)
                )
            );
            let (mut present, mut absent) = (false, !per_row);
            for event in trigger.events() {
                let holds = matches!(
                    (row, event),
                    (TriggerVariable::New, TriggerEvent::Insert | TriggerEvent::Update(_))
                        | (TriggerVariable::Old, TriggerEvent::Update(_) | TriggerEvent::Delete)
                );
                present |= per_row && holds;
                absent |= !holds;
            }
            let evaluation = match fields {
                [] if present => whole.clone().with_null(absent),
                [] => Evaluation::null(whole.ty().clone()),
                [field] => {
                    let Some(column) = stored_column(table, field, database)? else {
                        return Ok(None);
                    };
                    if present {
                        let case = self.truth.case;
                        let held = if row == TriggerVariable::New
                            && timing == Some(TriggerPeriod::Before)
                        {
                            proposed_value(table, column, database, case)?
                        } else {
                            stored_value(table, column, database, case)?
                        };
                        if absent { held.join(&Evaluation::null(held.ty().clone())) } else { held }
                    } else {
                        Evaluation::null(self.column_of(table, field)?.ty().clone())
                    }
                }
                _ => return Ok(None),
            };
            joined = Some(match joined {
                Some(held) => held.join(&evaluation),
                None => evaluation,
            });
        }
        Ok(joined)
    }

    /// What `name`, or its field at `fields`, holds by its declared type
    /// alone.
    fn variable_type(&mut self, name: &Name, fields: &[String]) -> Result<Evaluation, LookupError> {
        let variable = self.names.variable(name);
        if let [field] = fields
            && let Some(evaluation) = self.row_field(name, &variable, field)?
        {
            return Ok(evaluation);
        }
        let never_null = fields.is_empty()
            && matches!(
                variable.kind(),
                VariableKind::Declared { not_null: true, .. }
                    | VariableKind::LoopIndex
                    | VariableKind::Found
            );
        let operand = if fields.is_empty() {
            variable.data_type().map_err(Clone::clone)?.clone()
        } else {
            OperandType::Unknown
        };
        // A record not yet assigned refuses a read of a field.
        let refused = !fields.is_empty() && is_record(&variable);
        Ok(self.any_of(&operand).with_null(!never_null).with_error(refused))
    }
}

/// Any value of `ty`, or `NULL`, read without raising.
fn any(ty: ScalarType) -> Evaluation {
    Evaluation::new(ty, true, false, Values::Unlisted(Unlisted::any()))
}

/// The column of `table` PostgreSQL stores under the name `field`.
fn stored_column<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    field: &str,
    database: &'db DB,
) -> Result<Option<&'db DB::Column>, LookupError> {
    let quoted = Ident::with_quote('"', field).to_string();
    table.column(&quoted, database, IdentifierCase::AsWritten)
}

/// What `column` of `table` holds on a row the table stores, under its
/// type, `NOT NULL`, the checks of its domain, the stored generated columns
/// and the validated checks of the table reading one column.
///
/// A check relating several columns holds of the row and of no column alone,
/// so it narrows none.
fn stored_value<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    column: &'db DB::Column,
    database: &'db DB,
    case: IdentifierCase,
) -> Result<Evaluation, LookupError> {
    let mut truth = TruthContext::new(database, case, None);
    let read = truth.read_table(None, table, false)?;
    let mut checks = Vec::new();
    for check in validated_checks(table, database, None)? {
        if check.number_of_columns(database)? == 1 {
            checks.push(check);
        }
    }
    truth.tables[read].checks = checks;
    truth.caller = Some(read);
    let guarantees = truth.guarantees(&PostgreSqlDialect {}, &PostgreSqlOptions::default(), &[])?;
    Ok(held_value(&truth, read, column, &guarantees))
}

/// What `column` of `table` holds on a row a `BEFORE` trigger reads before
/// PostgreSQL checks it: its type, and the checks of its domain on a value
/// that is not `NULL`, since coercing a value to the column's type checks
/// them.
fn proposed_value<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    column: &'db DB::Column,
    database: &'db DB,
    case: IdentifierCase,
) -> Result<Evaluation, LookupError> {
    let mut truth = TruthContext::new(database, case, None);
    let read = truth.read_table(None, table, false)?;
    let guarantees =
        truth.domain_guarantees(&PostgreSqlDialect {}, &PostgreSqlOptions::default())?;
    Ok(held_value(&truth, read, column, &guarantees).with_null(true))
}

/// What `column` of the read `read` holds under `guarantees`.
fn held_value<DB: DatabaseLike>(
    truth: &TruthContext<'_, DB>,
    read: usize,
    column: &DB::Column,
    guarantees: &eval::Guarantees<usize>,
) -> Evaluation {
    let Some(named) = truth.column_named(read, column) else { return any(ScalarType::Unknown) };
    let base = &truth.named[named].evaluation;
    guarantees.reference(&named).map_or_else(|| base.clone(), |held| base.meet(held))
}

impl<DB: DatabaseLike> eval::Context for BodyTruth<'_, '_, DB> {
    type Reference = usize;
    type Error = LookupError;

    fn reference(&mut self, expression: &Expr) -> Result<Option<Resolved<usize>>, LookupError> {
        self.truth.reference(expression)
    }

    fn callees(&mut self, call: &Function) -> Result<Vec<Callee>, LookupError> {
        self.truth.callees(call)
    }

    fn data_type(&self, data_type: &DataType) -> ScalarType {
        self.truth.data_type(data_type)
    }

    fn may_plan_as_constant(&self, reference: &usize) -> bool {
        self.truth.may_plan_as_constant(reference)
    }
}

impl<DB: DatabaseLike> values::Context for BodyTruth<'_, '_, DB> {
    /// What `variable`, or its field at `fields`, holds by its declared type:
    /// a column of the table a row is declared as, and of the relations the
    /// triggers executing the function fire on for `NEW` and `OLD`, with
    /// `NULL` possible but for a whole variable declared `NOT NULL`.
    fn variable(&mut self, variable: &Name, fields: &[String]) -> Result<Evaluation, LookupError> {
        self.variable_type(variable, fields)
    }

    /// What a parameter or trigger variable, or its field at `fields`, holds
    /// as the body starts.
    ///
    /// An `OUT` parameter and `$0` start `NULL`, and the input parameters of
    /// a strict function start with a value, since PostgreSQL does not run
    /// its body on a `NULL` argument. Every trigger variable holds a value
    /// but `NEW` and `OLD`. Those are joined over every trigger executing the
    /// function. Each is `NULL` on a statement-level trigger and on an event
    /// that has no such row, `NEW` on `DELETE` and `OLD` on `INSERT`. Where
    /// the row holds, a column of `OLD` and of the `NEW` an `AFTER` trigger
    /// reads holds under its type, `NOT NULL`, its domain, the stored
    /// generated columns and the validated checks reading that column alone,
    /// as a row the table stores does. A check relating several columns
    /// narrows no field, since a guarantee here holds of one field alone. The
    /// `NEW` a `BEFORE` trigger reads holds a row PostgreSQL has not checked
    /// yet, so a column of it may be `NULL` and holds only its type and,
    /// when not `NULL`, its domain. A trigger on a view, a column the table
    /// does not have and a field of a field leave only the type.
    fn entry(
        &mut self,
        variable: &Name,
        fields: &[String],
    ) -> Result<Option<Evaluation>, LookupError> {
        match variable.kind() {
            NameKind::Parameter { .. } | NameKind::ReturnValue => {
                let declared = self.names.variable(variable);
                let whole = self.variable_type(variable, &[])?;
                Ok(match declared.kind() {
                    VariableKind::Parameter { mode: ArgMode::Out, .. }
                    | VariableKind::ReturnValue => Some(Evaluation::null(whole.ty().clone())),
                    VariableKind::Parameter { .. }
                        if fields.is_empty()
                            && matches!(
                                self.names.function().null_input_behavior(),
                                FunctionCalledOnNull::Strict
                                    | FunctionCalledOnNull::ReturnsNullOnNullInput
                            ) =>
                    {
                        Some(whole.with_null(false))
                    }
                    _ => None,
                })
            }
            NameKind::Trigger(row @ (TriggerVariable::New | TriggerVariable::Old)) => {
                self.trigger_row(variable, *row, fields)
            }
            NameKind::Trigger(_) if fields.is_empty() => {
                Ok(Some(self.variable_type(variable, &[])?.with_null(false)))
            }
            _ => Ok(None),
        }
    }
}
