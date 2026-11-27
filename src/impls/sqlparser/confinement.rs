//! Whether the reads a `SECURITY DEFINER` function makes as its owner reach
//! only rows the roles it runs for read themselves.

mod identity;
mod implication;
mod keys;
mod ladders;
mod nulls;
mod prover;
mod record;
mod relations;
mod sites;
mod terms;
mod writes;

use alloc::vec::Vec;

pub(crate) use self::identity::checked_value_lists;
use self::{prover::Confinement, record::Record, sites::Kinds};
use crate::{
    errors::LookupError,
    structs::{ActingRole, DefinerReading, Deployment, UnconfinedRead},
    traits::{DatabaseLike, FunctionLike, TriggerLike},
};

/// The first read `function`, executed by each of `callers`, makes as the
/// owner of a `SECURITY DEFINER` function that may reach a row the caller
/// cannot read, as
/// [`FunctionLike::unconfined_definer_read`] describes.
pub(crate) fn unconfined_function_read<'a, DB: DatabaseLike>(
    database: &'a DB,
    deployment: &'a Deployment,
    function: &'a DB::Function,
    callers: &[ActingRole<'a, DB::Role>],
) -> Result<Option<UnconfinedRead<'a, DB>>, LookupError> {
    if callers.is_empty() {
        return Ok(None);
    }
    let mut confinement = Confinement::new(database, deployment, callers.to_vec(), function, None);
    Ok((!confinement.confines_trigger(function, None)?).then(|| refused(&confinement)))
}

/// The first read the function of `trigger`, firing on a write of `role`,
/// makes as the owner of a `SECURITY DEFINER` function that may reach a row
/// the role cannot read, as [`TriggerLike::unconfined_definer_read`]
/// describes.
pub(crate) fn unconfined_trigger_read<'a, T: TriggerLike>(
    database: &'a T::DB,
    deployment: &'a Deployment,
    trigger: &'a T,
    role: ActingRole<'a, <T::DB as DatabaseLike>::Role>,
    reading: DefinerReading,
) -> Result<Option<UnconfinedRead<'a, T::DB>>, LookupError> {
    let Some(function) = trigger.function(database) else {
        return Ok(None);
    };
    let kinds = match function.plpgsql_sites(database) {
        Ok(body) => body.block().map(Kinds::of),
        Err(_) => None,
    };
    let record = match &kinds {
        Some(kinds) => Record::of(database, trigger, role, reading, kinds)?,
        None => None,
    };
    let mut confinement =
        Confinement::new(database, deployment, Vec::from([role]), function, Some(reading));
    Ok((!confinement.confines_trigger(function, record.as_ref())?).then(|| refused(&confinement)))
}

/// The read `confinement` found unconfined.
fn refused<'a, DB: DatabaseLike>(confinement: &Confinement<'a, DB>) -> UnconfinedRead<'a, DB> {
    let refusal = confinement.take_refusal();
    UnconfinedRead {
        through: refusal.through,
        table: refusal.table,
        statement: refusal.statement,
        reason: refusal.reason,
    }
}
