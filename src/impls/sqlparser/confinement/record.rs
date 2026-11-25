//! What a row trigger's function knows of `NEW` and `OLD` when it runs: the
//! write policies the row passed, the foreign keys it meets and the trigger's
//! `WHEN`.

use alloc::{borrow::Cow, boxed::Box, string::ToString, vec::Vec};
use core::ptr;

use sqlparser::ast::{
    BinaryOperator, Expr, ReferentialAction, TriggerEvent, TriggerObject, TriggerObjectKind,
    TriggerPeriod,
};

use super::{
    sites::Kinds,
    terms::conjuncts,
    writes::{definer_writes, rule_writes, table_family, view_writes},
};
use crate::{
    errors::{LookupError, RowSecurityError},
    structs::{ActingRole, ConstraintDeferral, DefinerReading, GateTerm, RowStatement, RowVersion},
    traits::{DatabaseLike, ForeignKeyLike, IndexLike, Relation, TableLike, TriggerLike},
};

const KEY_CHECK_PREFIX: &str = "RI_ConstraintTrigger_c_";

/// The row of a firing a fact or key holds of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Row {
    New,
    Old,
}

/// What every firing of a row trigger knows of its rows where its function
/// reads them unchanged.
pub(super) struct Record<'a, DB: DatabaseLike> {
    /// The table the trigger fires on.
    pub(super) table: &'a DB::Table,
    /// Conditions each row meets. A bare or table-qualified column names the
    /// column of the row the entry gives, and `NEW.x` or `OLD.x` that field.
    pub(super) facts: Vec<(Row, Cow<'a, Expr>)>,
    /// The foreign keys of `table` each row meets.
    pub(super) keys: Vec<(Row, &'a DB::ForeignKey)>,
}

impl<'a, DB: DatabaseLike> Record<'a, DB> {
    /// What the function of `trigger` knows of its rows under `reading` when
    /// `role` writes them, its body of `kinds`, or `None` for a trigger
    /// firing on no table row.
    ///
    /// Foreign keys hold row versions only when the table cannot be rewritten
    /// within the statement. A `BEFORE INSERT` row can be skipped by `ON
    /// CONFLICT`, so its keys hold only without uniqueness or exclusion
    /// constraints. Write-policy facts require a firing that cannot originate
    /// from owner-run views, rules, foreign definer writers or referential
    /// actions.
    /// Every partition or inheritance relative must admit the policy facts and
    /// preserve the row versions.
    pub(super) fn of<T: TriggerLike<DB = DB>>(
        database: &'a DB,
        trigger: &'a T,
        role: ActingRole<'a, DB::Role>,
        reading: DefinerReading,
        kinds: &Kinds,
    ) -> Result<Option<Self>, LookupError> {
        if !fires_per_row(trigger.orientation()) {
            return Ok(None);
        }
        let Relation::Table(table) = trigger.relation(database)? else {
            return Ok(None);
        };
        let tables = table_family(database, &table)?;
        let timing = trigger.timing();
        let events = trigger.events();
        let before = timing == Some(TriggerPeriod::Before);
        let after = timing == Some(TriggerPeriod::After);
        let written = !before
            || (!kinds.writes_new
                && kinds.returns_new
                && !Self::later_before_trigger(database, trigger, &tables)?);
        let new_checked = match reading {
            DefinerReading::Outcome => written,
            DefinerReading::Disclosure => after,
        };
        let mut record = Self { table, facts: Vec::new(), keys: Vec::new() };
        if let Some(condition) = trigger.condition() {
            record.facts.extend(
                conjuncts(condition).into_iter().map(|fact| (Row::New, Cow::Borrowed(fact))),
            );
        }
        if matches!(timing, Some(TriggerPeriod::InsteadOf)) {
            return Ok(Some(record));
        }
        let interference = Interference::of(
            database,
            table,
            &tables,
            role,
            before && events.contains(&TriggerEvent::Insert),
        )?;
        record.hold_policies(database, &tables, events, role, new_checked, |event| {
            interference.bypasses(event)
        })?;
        let name = trigger.stored_name();
        let after_key_checks = after
            && name.as_bytes() > KEY_CHECK_PREFIX.as_bytes()
            && !name.starts_with(KEY_CHECK_PREFIX);
        let new_key_checked = !interference.rewritable
            && match reading {
                DefinerReading::Outcome => {
                    written
                        && !(before
                            && events.contains(&TriggerEvent::Insert)
                            && interference.conflict)
                }
                DefinerReading::Disclosure => after_key_checks,
            };
        record.hold_keys(database, events, new_key_checked, interference.rewritable)?;
        Ok(Some(record))
    }

    /// Records write-policy facts shared by every firing event.
    fn hold_policies(
        &mut self,
        database: &'a DB,
        tables: &[&'a DB::Table],
        events: &[TriggerEvent],
        role: ActingRole<'a, DB::Role>,
        new_checked: bool,
        bypasses: impl Fn(&TriggerEvent) -> bool,
    ) -> Result<(), LookupError> {
        let mut new = Vec::new();
        let mut old = Vec::new();
        let update_bypassed = bypasses(&TriggerEvent::Update(Vec::new()));
        for event in events {
            let statement = match event {
                TriggerEvent::Insert => RowStatement::Insert { returning: false },
                TriggerEvent::Update(_) => RowStatement::Update { reads: false },
                TriggerEvent::Delete => RowStatement::Delete { reads: false },
                TriggerEvent::Truncate => continue,
            };
            for table in tables {
                let (new_facts, old_facts) = if bypasses(event) {
                    (Vec::new(), Vec::new())
                } else {
                    gate_facts(database, *table, statement, role)?
                };
                if matches!(event, TriggerEvent::Insert | TriggerEvent::Update(_)) {
                    new.push(new_facts);
                }
                if matches!(event, TriggerEvent::Update(_) | TriggerEvent::Delete) {
                    old.push(old_facts);
                }
                if tables.len() > 1
                    && matches!(event, TriggerEvent::Insert | TriggerEvent::Delete)
                    && (update_bypassed || table.privileges(role, database)?.update.on_any_column())
                {
                    let (updated, replaced) = if update_bypassed {
                        (Vec::new(), Vec::new())
                    } else {
                        gate_facts(database, *table, RowStatement::Update { reads: false }, role)?
                    };
                    match event {
                        TriggerEvent::Insert => new.push(updated),
                        TriggerEvent::Delete => old.push(replaced),
                        _ => {}
                    }
                }
            }
        }
        if new_checked {
            self.facts.extend(shared(new).into_iter().map(|fact| (Row::New, fact)));
        }
        self.facts.extend(shared(old).into_iter().map(|fact| (Row::Old, fact)));
        Ok(())
    }

    /// Records enforced keys whose checks hold the firing's row versions.
    fn hold_keys(
        &mut self,
        database: &'a DB,
        events: &[TriggerEvent],
        new_checked: bool,
        rewritable: bool,
    ) -> Result<(), LookupError> {
        let writes_new = events
            .iter()
            .any(|event| matches!(event, TriggerEvent::Insert | TriggerEvent::Update(_)));
        let updates = events.iter().any(|event| matches!(event, TriggerEvent::Update(_)));
        for key in self.table.foreign_keys(database)? {
            if !key.is_enforced(database) {
                continue;
            }
            let immediate = key.deferral(database) == ConstraintDeferral::NotDeferrable;
            let validated = key.is_validated(database)?;
            let new_holds = immediate && new_checked && (!updates || validated);
            if writes_new && new_holds {
                self.keys.push((Row::New, key));
            }
            if immediate && validated && !rewritable {
                self.keys.push((Row::Old, key));
            }
        }
        Ok(())
    }

    /// Whether a later `BEFORE` row trigger on the table family may change the
    /// written row.
    fn later_before_trigger<T: TriggerLike<DB = DB>>(
        database: &'a DB,
        trigger: &T,
        tables: &[&DB::Table],
    ) -> Result<bool, LookupError> {
        for other in database.triggers() {
            if other.timing() != Some(TriggerPeriod::Before)
                || !fires_per_row(other.orientation())
                || other.stored_name().as_bytes() <= trigger.stored_name().as_bytes()
                || !other.events().iter().any(|event| {
                    matches!(event, TriggerEvent::Insert | TriggerEvent::Update(_))
                        && trigger.events().iter().any(|own| {
                            core::mem::discriminant(own) == core::mem::discriminant(event)
                        })
                })
            {
                continue;
            }
            if let Relation::Table(fired) = other.relation(database)?
                && tables.iter().any(|table| ptr::eq(fired, *table))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Whether an insert can skip its proposed row through conflict handling.
fn conflict_can_skip<DB: DatabaseLike>(
    table: &DB::Table,
    database: &DB,
) -> Result<bool, LookupError> {
    Ok(table.has_primary_key(database)?
        || table.unique_indices(database)?.next().is_some()
        || table.indices(database)?.any(IndexLike::is_unique)
        || table.has_exclusion_constraints(database)?)
}

/// Ways a table tree can bypass policies, rewrite rows or skip an insert.
struct Interference {
    rewritable: bool,
    conflict: bool,
    /// Bypassed policy checks for insertion, update and deletion.
    bypassed: [bool; 3],
}

impl Interference {
    /// Combines policy bypasses and row rewrites over every table-family
    /// member.
    fn of<'db, DB: DatabaseLike>(
        database: &'db DB,
        table: &'db DB::Table,
        tables: &[&'db DB::Table],
        role: ActingRole<'db, DB::Role>,
        check_conflicts: bool,
    ) -> Result<Self, LookupError> {
        let definer = definer_writes(database, table, role)?;
        let mut interference =
            Self { rewritable: definer.any, conflict: false, bypassed: [definer.foreign; 3] };
        for member in tables {
            let views = view_writes(database, *member, role)?;
            let ruled = rule_writes(database, *member);
            let privileges = member.privileges(role, database)?;
            interference.rewritable |=
                privileges.update.on_any_column() || privileges.delete || views.rewrites || ruled;
            if views.bypasses || ruled {
                interference.bypassed = [true; 3];
            }
            for key in member.foreign_keys(database)? {
                let deletion = key.on_delete(database) == ReferentialAction::Cascade;
                let update = matches!(
                    key.on_delete(database),
                    ReferentialAction::SetNull | ReferentialAction::SetDefault
                ) || matches!(
                    key.on_update(database),
                    ReferentialAction::Cascade
                        | ReferentialAction::SetNull
                        | ReferentialAction::SetDefault
                );
                interference.rewritable |= deletion || update;
                interference.bypassed[1] |= update;
                interference.bypassed[2] |= deletion;
            }
            if check_conflicts && !interference.conflict {
                interference.conflict = conflict_can_skip(*member, database)?;
            }
        }
        Ok(interference)
    }

    /// Whether an event can reach the trigger without the acting role's policy
    /// checks.
    fn bypasses(&self, event: &TriggerEvent) -> bool {
        match event {
            TriggerEvent::Insert => self.bypassed[0],
            TriggerEvent::Update(_) => self.bypassed[1],
            TriggerEvent::Delete => self.bypassed[2],
            TriggerEvent::Truncate => false,
        }
    }
}

/// Whether a trigger of `orientation` fires once per row.
fn fires_per_row(orientation: Option<TriggerObjectKind>) -> bool {
    matches!(
        orientation,
        Some(
            TriggerObjectKind::For(TriggerObject::Row)
                | TriggerObjectKind::ForEach(TriggerObject::Row)
        )
    )
}

/// Conditions a row of a firing meets.
type Facts<'a> = Vec<Cow<'a, Expr>>;

/// The conditions the write policies of `table` for `statement` hold of the
/// row it writes and of the row it replaces when `role` writes it: the
/// restrictive terms of each gate and the `OR` of its permissive ones.
fn gate_facts<'a, DB: DatabaseLike>(
    database: &'a DB,
    table: &'a DB::Table,
    statement: RowStatement,
    role: ActingRole<'a, DB::Role>,
) -> Result<(Facts<'a>, Facts<'a>), LookupError> {
    let security = match table.row_security(statement, role, database) {
        Ok(security) => security,
        Err(RowSecurityError::Lookup(error)) => return Err(error),
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };
    let mut new = Vec::new();
    let mut old = Vec::new();
    for gate in security.gates().unwrap_or_default() {
        let facts = if gate.row() == RowVersion::Existing { &mut old } else { &mut new };
        for term in gate.restrictive() {
            facts.extend(conjuncts(term.expression().expression()).into_iter().map(Cow::Borrowed));
        }
        if let Some(alternatives) = alternatives(gate.permissive()) {
            facts.push(alternatives);
        }
    }
    Ok((new, old))
}

/// The `OR` of the conditions of `terms`, `None` for no term.
fn alternatives<'a, P>(terms: &[GateTerm<'a, P>]) -> Option<Cow<'a, Expr>> {
    let mut conditions = terms.iter().map(|term| term.expression().expression());
    let first = conditions.next()?;
    Some(conditions.fold(Cow::Borrowed(first), |left, right| {
        Cow::Owned(Expr::BinaryOp {
            left: Box::new(left.into_owned()),
            op: BinaryOperator::Or,
            right: Box::new(right.clone()),
        })
    }))
}

/// The facts every list of `lists` holds, written alike.
fn shared(mut lists: Vec<Facts<'_>>) -> Facts<'_> {
    let Some(first) = lists.pop() else {
        return Vec::new();
    };
    first
        .into_iter()
        .filter(|fact| {
            let written = fact.to_string();
            lists.iter().all(|list| list.iter().any(|other| other.to_string() == written))
        })
        .collect()
}
