//! `GRANT` and `REVOKE` on functions, which change who may execute each one.
//!
//! A relation grant is stored as written and read back through
//! [`TableGrantLike`](crate::traits::TableGrantLike). A function grant instead
//! edits the privileges of each function it reaches, since a function's
//! privileges begin at PostgreSQL's default rather than at nothing, and a
//! revoke has to be able to take that default away.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{Action, FunctionDesc, Grant, GrantObjects, Grantee, Privileges, Revoke};

use super::{
    AccessResolution, ParserDBBuilder, function_names_match, function_signatures_match,
    grantee_role_ident, schema_qualifiers_match, stored_role_name,
    validate_access_targets_against_builder,
};
use crate::{
    errors::Error,
    utils::{
        last_str,
        object_name::{Qualifier, object_name_last_part, qualifier_of},
    },
};

/// Whether a grant or a revoke reaches functions rather than relations.
pub(super) fn names_routines(objects: Option<&GrantObjects>) -> bool {
    matches!(
        objects,
        Some(
            GrantObjects::Functions(_)
                | GrantObjects::Procedures(_)
                | GrantObjects::AllFunctionsInSchema { .. }
        )
    )
}

/// Gives `EXECUTE` on each function the grant reaches.
///
/// Returns the signatures that reach no function, which only the open world
/// lets through and keeps so it can report them.
pub(super) fn grant(
    builder: &mut ParserDBBuilder,
    grant: Grant,
    access_resolution: AccessResolution,
) -> Result<Vec<FunctionDesc>, Error> {
    let Some(objects) = &grant.objects else {
        return Ok(Vec::new());
    };
    let reached = Reached::of(builder, objects)?;
    validate(builder, &reached, &grant.grantees, &grant.privileges, access_resolution)?;
    let Reached { functions, unresolved, .. } = reached;

    for position in functions {
        builder.functions_mut()[position]
            .1
            .execute_privileges_mut()
            .grant(&grant.grantees, grant.with_grant_option);
    }

    let Some(GrantObjects::Functions(signatures) | GrantObjects::Procedures(signatures)) =
        grant.objects
    else {
        return Ok(Vec::new());
    };
    Ok(signatures
        .into_iter()
        .enumerate()
        .filter(|(index, _)| unresolved.contains(index))
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
    revoke: &Revoke,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    let Some(objects) = &revoke.objects else {
        return Ok(());
    };
    let reached = Reached::of(builder, objects)?;
    validate(builder, &reached, &revoke.grantees, &revoke.privileges, access_resolution)?;

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

/// Refuses what PostgreSQL refuses, in the order it reports it, which is a
/// signature reaching no function, then a grantee naming no role, then a
/// privilege functions do not have. The first two are the closed world's to
/// refuse.
fn validate(
    builder: &ParserDBBuilder,
    reached: &Reached<'_>,
    grantees: &[Grantee],
    privileges: &Privileges,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    if access_resolution == AccessResolution::ClosedWorld {
        if let Some(&index) = reached.unresolved.first() {
            return Err(Error::FunctionNotFoundForGrant {
                function_name: reached.signatures[index].to_string(),
            });
        }
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
struct Reached<'statement> {
    /// The signatures the statement lists, empty for a schema-wide statement.
    signatures: &'statement [FunctionDesc],
    /// Positions in the function store of every function reached.
    functions: Vec<usize>,
    /// Indices into `signatures` of those reaching no function.
    unresolved: Vec<usize>,
}

impl<'statement> Reached<'statement> {
    /// Resolves the functions `objects` names, as of this statement.
    ///
    /// A signature with an argument list reaches the one function of that
    /// identity, and one without reaches the only function of that name,
    /// which PostgreSQL refuses to guess between when there are several. A
    /// schema-wide statement reaches the functions the schema holds now, and
    /// none created after it.
    fn of(builder: &ParserDBBuilder, objects: &'statement GrantObjects) -> Result<Self, Error> {
        let case = builder.identifier_case();
        let functions = builder.functions();
        let mut reached = Self { signatures: &[], functions: Vec::new(), unresolved: Vec::new() };

        match objects {
            GrantObjects::Functions(signatures) => {
                reached.signatures = signatures;
                for (index, signature) in signatures.iter().enumerate() {
                    let matching: Vec<usize> = functions
                        .iter()
                        .enumerate()
                        .filter(|(_, (function, _))| {
                            match signature.args.as_deref() {
                                Some(args) => {
                                    function_signatures_match(
                                        &function.name,
                                        function.args.as_deref(),
                                        &signature.name,
                                        Some(args),
                                        case,
                                    )
                                }
                                None => function_names_match(&function.name, &signature.name, case),
                            }
                        })
                        .map(|(position, _)| position)
                        .collect();
                    match matching.as_slice() {
                        [] => reached.unresolved.push(index),
                        [_, _, ..] if signature.args.is_none() => {
                            return Err(Error::AmbiguousFunctionForGrant {
                                function_name: last_str(&signature.name).to_string(),
                            });
                        }
                        _ => reached.functions.extend(matching),
                    }
                }
            }
            // This model holds no procedure, so none is ever reached.
            GrantObjects::Procedures(signatures) => {
                reached.signatures = signatures;
                reached.unresolved.extend(0..signatures.len());
            }
            GrantObjects::AllFunctionsInSchema { schemas } => {
                reached.functions.extend(functions.iter().enumerate().filter_map(
                    |(position, (function, _))| {
                        let qualifier = match qualifier_of(&function.name) {
                            Qualifier::Absent => None,
                            Qualifier::Named(name, quoted) => Some((name, quoted)),
                            Qualifier::RunTime => return None,
                        };
                        schemas
                            .iter()
                            .filter_map(object_name_last_part)
                            .any(|schema| schema_qualifiers_match(qualifier, Some(schema), case))
                            .then_some(position)
                    },
                ));
            }
            _ => {}
        }
        Ok(reached)
    }
}
