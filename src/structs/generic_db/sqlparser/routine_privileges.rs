//! `GRANT` and `REVOKE` on functions and routines, which change who may
//! execute each function.
//!
//! A relation grant is stored as written and read back through
//! [`TableGrantLike`](crate::traits::TableGrantLike). A function grant instead
//! edits the privileges of each function it reaches, since a function's
//! privileges begin at PostgreSQL's default rather than at nothing, and a
//! revoke has to be able to take that default away.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{
    Action, FunctionDesc, Grant, GrantObjects, Grantee, ObjectName, Privileges, Revoke,
};

use super::{
    AccessResolution, ParserDB, ParserDBBuilder, grantee_role_ident, procedures::RoutineKeyword,
    schema_qualifiers_match, stored_role_name, type_arrays::TypeArrays,
    validate_access_targets_against_builder,
};
use crate::{
    errors::Error,
    utils::{
        last_str,
        object_name::{object_name_last_part, qualifier_of},
    },
};

/// The routines a grant or a revoke names, taken out of its object list,
/// with the keyword telling which kinds it reaches.
pub(super) enum Routines {
    /// `ON FUNCTION`, `ON PROCEDURE` or `ON ROUTINE`, one signature per
    /// routine.
    Signatures(RoutineKeyword, Vec<FunctionDesc>),
    /// `ON ALL FUNCTIONS`, `ALL PROCEDURES` or `ALL ROUTINES IN SCHEMA`.
    Schemas(RoutineKeyword, Vec<ObjectName>),
}

impl Routines {
    /// Takes the objects out when they name routines, and leaves relation
    /// objects in place.
    pub(super) fn take(objects: &mut Option<GrantObjects>) -> Option<Self> {
        match objects.take() {
            Some(GrantObjects::Functions(signatures)) => {
                Some(Self::Signatures(RoutineKeyword::Function, signatures))
            }
            Some(GrantObjects::Procedures(signatures)) => {
                Some(Self::Signatures(RoutineKeyword::Procedure, signatures))
            }
            Some(GrantObjects::Routines(signatures)) => {
                Some(Self::Signatures(RoutineKeyword::Routine, signatures))
            }
            Some(GrantObjects::AllFunctionsInSchema { schemas }) => {
                Some(Self::Schemas(RoutineKeyword::Function, schemas))
            }
            Some(GrantObjects::AllProceduresInSchema { schemas }) => {
                Some(Self::Schemas(RoutineKeyword::Procedure, schemas))
            }
            Some(GrantObjects::AllRoutinesInSchema { schemas }) => {
                Some(Self::Schemas(RoutineKeyword::Routine, schemas))
            }
            other => {
                *objects = other;
                None
            }
        }
    }
}

/// Gives `EXECUTE` on each function the grant reaches.
///
/// Returns the signatures that reach no function, which only the open world
/// lets through and keeps so it can report them.
pub(super) fn grant(
    builder: &mut ParserDBBuilder,
    routines: Routines,
    grant: &Grant,
    access_resolution: AccessResolution,
    arrays: &TypeArrays,
) -> Result<Vec<FunctionDesc>, Error> {
    let reached = Reached::of(builder, &routines, access_resolution, arrays)?;
    validate(builder, &grant.grantees, &grant.privileges, access_resolution)?;

    for &position in &reached.functions {
        builder.functions_mut()[position]
            .1
            .execute_privileges_mut()
            .grant(&grant.grantees, grant.with_grant_option);
    }

    let Routines::Signatures(_, signatures) = routines else {
        return Ok(Vec::new());
    };
    Ok(signatures
        .into_iter()
        .enumerate()
        .filter(|(index, _)| reached.unresolved.contains(index))
        .map(|(_, signature)| signature)
        .collect())
}

/// Takes `EXECUTE`, or only the right to grant it, on each function the
/// revoke reaches.
///
/// A signature reaching no function takes nothing, as PostgreSQL would have
/// nothing to take, so the open world drops it.
pub(super) fn revoke(
    builder: &mut ParserDBBuilder,
    routines: &Routines,
    revoke: &Revoke,
    access_resolution: AccessResolution,
    arrays: &TypeArrays,
) -> Result<(), Error> {
    let reached = Reached::of(builder, routines, access_resolution, arrays)?;
    validate(builder, &revoke.grantees, &revoke.privileges, access_resolution)?;

    let names_owner = |grantee: &Grantee, owner: &str| {
        grantee_role_ident(grantee).is_some_and(|ident| stored_role_name(ident) == owner)
    };
    for &position in &reached.functions {
        builder.functions_mut()[position].1.revoke_execute(
            &revoke.grantees,
            revoke.grant_option_for,
            names_owner,
        );
    }
    Ok(())
}

/// Refuses a grantee naming no role, which only the closed world refuses, and
/// then a privilege functions do not have, in the order PostgreSQL reports
/// them once the functions resolved.
fn validate(
    builder: &ParserDBBuilder,
    grantees: &[Grantee],
    privileges: &Privileges,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    if access_resolution == AccessResolution::ClosedWorld {
        validate_access_targets_against_builder(builder, grantees, None)?;
    }

    let Privileges::Actions(actions) = privileges else {
        return Ok(());
    };
    match actions.iter().find(|action| !matches!(action, Action::Execute { obj_type: None })) {
        Some(action) => Err(Error::InvalidFunctionPrivilege { privilege: action.to_string() }),
        None => Ok(()),
    }
}

/// The stored functions a statement reaches.
struct Reached {
    /// Positions in the function store of every function reached.
    functions: Vec<usize>,
    /// Indices into the statement's signatures of those reaching no function.
    unresolved: Vec<usize>,
}

impl Reached {
    /// Resolves the routines `routines` names, as of this statement.
    ///
    /// A signature with an argument list reaches the one routine of that
    /// identity, and one without reaches the only routine of that name and
    /// of a kind the keyword admits, which PostgreSQL refuses to guess
    /// between when there are several. A schema-wide statement reaches the
    /// routines of the admitted kinds the schema holds now, and none created
    /// after it. A signature reaching nothing is refused under the closed
    /// world, in list order alongside an ambiguous one.
    fn of(
        builder: &ParserDBBuilder,
        routines: &Routines,
        access_resolution: AccessResolution,
        arrays: &TypeArrays,
    ) -> Result<Self, Error> {
        let case = builder.identifier_case();
        let functions = builder.functions();
        let mut reached = Self { functions: Vec::new(), unresolved: Vec::new() };

        match routines {
            Routines::Signatures(keyword, signatures) => {
                for (index, signature) in signatures.iter().enumerate() {
                    let matching = ParserDB::functions_named(builder, signature, arrays, *keyword)?;
                    match matching.as_slice() {
                        [] => reached.miss(index, signature, access_resolution)?,
                        [_, _, ..] if signature.args.is_none() => {
                            return Err(Error::AmbiguousFunctionForGrant {
                                function_name: last_str(&signature.name).to_string(),
                            });
                        }
                        _ => reached.functions.extend(matching),
                    }
                }
            }
            Routines::Schemas(keyword, schemas) => {
                reached.functions.extend(functions.iter().enumerate().filter_map(
                    |(position, (function, metadata))| {
                        // `CREATE FUNCTION` refuses a qualifier built at run
                        // time.
                        let qualifier = qualifier_of(&function.name).named();
                        (keyword.admits(metadata.kind())
                            && schemas.iter().filter_map(object_name_last_part).any(|schema| {
                                schema_qualifiers_match(
                                    builder.namespaces(),
                                    qualifier,
                                    Some(schema),
                                    case,
                                )
                            }))
                        .then_some(position)
                    },
                ));
            }
        }
        Ok(reached)
    }

    /// Records a signature reaching no function, which the closed world
    /// refuses and the open world keeps.
    fn miss(
        &mut self,
        index: usize,
        signature: &FunctionDesc,
        access_resolution: AccessResolution,
    ) -> Result<(), Error> {
        if access_resolution == AccessResolution::ClosedWorld {
            return Err(Error::FunctionNotFoundForGrant { function_name: signature.to_string() });
        }
        self.unresolved.push(index);
        Ok(())
    }
}
