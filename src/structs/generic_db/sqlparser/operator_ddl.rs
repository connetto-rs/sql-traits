//! `CREATE OPERATOR`, `ALTER OPERATOR` and `DROP OPERATOR`, as PostgreSQL 18
//! applies them to `pg_operator`.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use sqlparser::ast::{
    AlterOperator, AlterOperatorOperation, CreateFunction, CreateOperator, DataType, DropOperator,
    FunctionReturnType, ObjectName, OperateFunctionArg, OperatorOption,
};

use super::{
    ParserDBBuilder,
    default_privileges::{SessionRoles, creator_lacking_execute},
    domains,
    postgres_catalog::{CatalogName, PostgresCatalog},
    postgres_operator::{
        JOIN_ESTIMATOR_INPUTS, PostgresCatalogOperator, RESTRICTION_ESTIMATOR_INPUTS,
        function_name_matches, takes_estimator_inputs, takes_exactly, type_identity,
    },
    search_path_qualifier,
    type_arrays::TypeArrays,
    validate_owner_role, validate_relation_schema,
};
use crate::{
    errors::{Error, ObjectKind, OperatorAttribute},
    structs::FunctionMetadata,
    traits::OperatorKind,
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema},
        object_name::{PG_CATALOG, object_name_last_part, qualifier_of},
    },
};

/// The schema an object stored without one lives in.
const DEFAULT_SCHEMA: &str = "public";

/// Applies `CREATE OPERATOR`, filling a shell of the same identity and
/// creating a shell for each absent commutator and negator.
///
/// # Errors
///
/// Returns the refusal PostgreSQL 18 raises for the statement: a missing
/// right operand, an absent schema, function or estimator, a function the
/// creating role cannot execute, an attribute the operator's kind or result
/// cannot carry, a second definition, a negator naming the operator itself
/// and a commutator or negator already linked to another operator.
pub(super) fn create(
    builder: &ParserDBBuilder,
    catalog: &mut Arc<PostgresCatalog>,
    arrays: &TypeArrays,
    session: &SessionRoles,
    create: CreateOperator,
) -> Result<(), Error> {
    let CreateOperator { name, function, left_arg, right_arg, options, .. } = create;
    let binder = Binder::new(builder, session, arrays, symbol_of(&name));
    let target = place(builder, session, &name, binder.symbol)?;
    let Some(right) = right_arg else {
        return Err(Error::OperatorRightArgumentRequired { operator_name: binder.symbol.into() });
    };
    let left = left_arg.map(|left| operand(builder, arrays, left)).transpose()?;
    let right = operand(builder, arrays, right)?;

    let (implementation, called) = binder
        .function(&function, |args| takes_exactly(left.iter().chain([&right]), arrays, args))?;
    let result = called.return_type.as_ref().and_then(|returned| {
        let (FunctionReturnType::DataType(data_type) | FunctionReturnType::SetOf(data_type)) =
            returned;
        type_identity(arrays, data_type)
    });

    let attributes = Attributes::of(&options);
    // `DefineOperator` binds the estimators before `OperatorCreate` checks
    // the attributes against the operator's kind and result.
    let restriction_estimator =
        binder.estimators(&attributes.restrict, &RESTRICTION_ESTIMATOR_INPUTS)?;
    let join_estimator = binder.estimators(&attributes.join, &JOIN_ESTIMATOR_INPUTS)?;
    let kind = if left.is_some() { OperatorKind::Binary } else { OperatorKind::Prefix };
    attributes.check(kind, result.as_ref(), binder.symbol)?;

    let mut operator = PostgresCatalogOperator {
        target,
        kind,
        left,
        right,
        result,
        implementation: Some(implementation),
        commutator: None,
        negator: None,
        can_hash: attributes.hashes,
        can_merge: attributes.merges,
        restriction_estimator: restriction_estimator.into_named(),
        join_estimator: join_estimator.into_named(),
    };
    let held = catalog.operators().position(|held| held.repeats(&operator));
    if held.is_some_and(|position| catalog.operators_slice()[position].implementation.is_some()) {
        return Err(Error::OperatorAlreadyExists { operator_name: binder.symbol.into() });
    }

    let commutator = attributes
        .commutator
        .map(|name| {
            let left = operator.left.as_ref().unwrap_or(&operator.right);
            Link::resolve(&binder, catalog, name, Some(&operator.right), left)
        })
        .transpose()?;
    // An operator over equal operands may commute with itself, linking to
    // its own symbol.
    let commutator = match commutator {
        Some(link) if link.is_self(held, &operator) => {
            operator.commutator = Some(operator.target.clone());
            None
        }
        link => link,
    };
    let negator = attributes
        .negator
        .map(|name| Link::resolve(&binder, catalog, name, operator.left.as_ref(), &operator.right))
        .transpose()?;
    if negator.as_ref().is_some_and(|negator| negator.is_self(held, &operator)) {
        return Err(Error::OperatorOwnNegator { operator_name: binder.symbol.into() });
    }
    for (link, attribute) in [
        (commutator.as_ref(), OperatorAttribute::Commutator),
        (negator.as_ref(), OperatorAttribute::Negator),
    ] {
        if let Some(Link::Held(position)) = link {
            require_free(
                &catalog.operators_slice()[*position],
                &operator,
                attribute,
                binder.symbol,
            )?;
        }
    }

    let operators = Arc::make_mut(catalog).operators_mut();
    let mut shells = Vec::new();
    for (link, attribute) in
        [(commutator, OperatorAttribute::Commutator), (negator, OperatorAttribute::Negator)]
    {
        let linked = match link {
            None => continue,
            Some(Link::Held(position)) => {
                set_link(&mut operators[position], attribute, operator.target.clone());
                operators[position].target.clone()
            }
            Some(Link::Shell(mut shell)) => {
                set_link(&mut shell, attribute, operator.target.clone());
                let linked = shell.target.clone();
                shells.push(*shell);
                linked
            }
        };
        set_link(&mut operator, attribute, linked);
    }
    match held {
        Some(position) => operators[position] = operator,
        None => operators.push(operator),
    }
    operators.extend(shells);
    Ok(())
}

/// Applies `ALTER OPERATOR`.
///
/// # Errors
///
/// Returns [`Error::OperatorNotFound`] for an absent operator, the refusal
/// of an `OWNER TO` role absent under a closed world, of a move onto a held
/// identity, and the refusals `SET` meets: an absent estimator or link, an
/// attribute the operator's kind or result cannot carry, a link changed once
/// set, a negator naming the operator itself and a link already held
/// elsewhere.
pub(super) fn alter(
    builder: &ParserDBBuilder,
    catalog: &mut Arc<PostgresCatalog>,
    arrays: &TypeArrays,
    session: &SessionRoles,
    closed_world: bool,
    alter: AlterOperator,
) -> Result<(), Error> {
    let AlterOperator { name, left_type, right_type, operation } = alter;
    let binder = Binder::new(builder, session, arrays, symbol_of(&name));
    let left = left_type.map(|left| operand(builder, arrays, left)).transpose()?;
    let right = operand(builder, arrays, right_type)?;
    let position = binder
        .lookup(catalog, &name, left.as_ref(), &right)
        .ok_or_else(|| Error::OperatorNotFound { operator_name: binder.symbol.into() })?;
    match operation {
        AlterOperatorOperation::OwnerTo(owner) => {
            if closed_world {
                validate_owner_role(builder, &owner, binder.symbol)?;
            }
            Ok(())
        }
        AlterOperatorOperation::SetSchema { schema_name } => {
            let schema = object_name_last_part(&schema_name).unwrap_or(("", false));
            validate_relation_schema(builder, Some(schema), ObjectKind::Operator, binder.symbol)?;
            let operator = &catalog.operators_slice()[position];
            let moved = CatalogName::new(
                schema,
                (operator.target.name(), operator.target.name_is_quoted()),
            );
            if catalog
                .operators()
                .any(|held| held.is_named_by(&moved, operator.left.as_ref(), &operator.right))
            {
                return Err(Error::OperatorAlreadyExists { operator_name: binder.symbol.into() });
            }
            Arc::make_mut(catalog).move_operator(position, schema);
            Ok(())
        }
        AlterOperatorOperation::Set { options } => set(&binder, catalog, position, &options),
    }
}

/// Applies `ALTER OPERATOR ... SET` to the operator at `position`.
fn set(
    binder: &Binder<'_>,
    catalog: &mut Arc<PostgresCatalog>,
    position: usize,
    options: &[OperatorOption],
) -> Result<(), Error> {
    let symbol = binder.symbol;
    let attributes = Attributes::of(options);
    let operator = &catalog.operators_slice()[position];
    let restriction_estimator =
        binder.estimators(&attributes.restrict, &RESTRICTION_ESTIMATOR_INPUTS)?;
    let join_estimator = binder.estimators(&attributes.join, &JOIN_ESTIMATOR_INPUTS)?;
    attributes.check(operator.kind, operator.result.as_ref(), symbol)?;

    let not_found =
        |name: &ObjectName| Error::OperatorNotFound { operator_name: symbol_of(name).into() };
    let commutator = attributes
        .commutator
        .map(|name| {
            let left = operator.left.as_ref().unwrap_or(&operator.right);
            binder.lookup(catalog, name, Some(&operator.right), left).ok_or_else(|| not_found(name))
        })
        .transpose()?;
    let negator = attributes
        .negator
        .map(|name| {
            binder
                .lookup(catalog, name, operator.left.as_ref(), &operator.right)
                .ok_or_else(|| not_found(name))
        })
        .transpose()?;
    if negator == Some(position) {
        return Err(Error::OperatorOwnNegator { operator_name: symbol.into() });
    }
    for (linked, attribute) in
        [(commutator, OperatorAttribute::Commutator), (negator, OperatorAttribute::Negator)]
    {
        let Some(linked) = linked.map(|linked| &catalog.operators_slice()[linked]) else {
            continue;
        };
        let (held, kept) = match attribute {
            OperatorAttribute::Commutator => {
                (operator.commutator.is_some(), operator.commutes_with(linked))
            }
            _ => (operator.negator.is_some(), operator.negates(linked)),
        };
        if held && !kept {
            return Err(Error::OperatorAttributeAlreadySet {
                operator_name: symbol.into(),
                attribute,
            });
        }
        require_free(linked, operator, attribute, symbol)?;
    }

    let operators = Arc::make_mut(catalog).operators_mut();
    let operator = &mut operators[position];
    restriction_estimator.apply(&mut operator.restriction_estimator);
    join_estimator.apply(&mut operator.join_estimator);
    operator.can_hash |= attributes.hashes;
    operator.can_merge |= attributes.merges;
    for (linked, attribute) in
        [(commutator, OperatorAttribute::Commutator), (negator, OperatorAttribute::Negator)]
    {
        let Some(linked) = linked else { continue };
        let target = operators[linked].target.clone();
        set_link(&mut operators[position], attribute, target);
        let back = operators[position].target.clone();
        set_link(&mut operators[linked], attribute, back);
    }
    Ok(())
}

/// Applies `DROP OPERATOR`, clearing the links other operators keep to the
/// dropped ones.
///
/// # Errors
///
/// Returns [`Error::OperatorNotFound`] for an absent operator without `IF
/// EXISTS`.
pub(super) fn drop(
    builder: &ParserDBBuilder,
    catalog: &mut Arc<PostgresCatalog>,
    arrays: &TypeArrays,
    session: &SessionRoles,
    drop: DropOperator,
) -> Result<(), Error> {
    let mut doomed = Vec::with_capacity(drop.operators.len());
    for signature in drop.operators {
        let binder = Binder::new(builder, session, arrays, symbol_of(&signature.name));
        let left = signature.left_type.map(|left| operand(builder, arrays, left)).transpose()?;
        let right = operand(builder, arrays, signature.right_type)?;
        match binder.lookup(catalog, &signature.name, left.as_ref(), &right) {
            Some(position) => doomed.push(position),
            None if drop.if_exists => {}
            None => return Err(Error::OperatorNotFound { operator_name: binder.symbol.into() }),
        }
    }
    if !doomed.is_empty() {
        Arc::make_mut(catalog).remove_operators(|position, _| doomed.contains(&position));
    }
    Ok(())
}

/// What a statement writes for an estimator attribute.
#[derive(Default)]
enum Estimator<T> {
    /// The attribute is not written.
    #[default]
    Unwritten,
    /// `RESTRICT = NONE` or `JOIN = NONE`.
    Cleared,
    /// The estimator named.
    Named(T),
}

impl<'s> Estimator<&'s ObjectName> {
    /// The attribute an option writes, `None` for `NONE`.
    fn written(name: Option<&'s ObjectName>) -> Self {
        name.map_or(Self::Cleared, Self::Named)
    }
}

impl<T> Estimator<T> {
    const fn is_named(&self) -> bool {
        matches!(self, Self::Named(_))
    }

    /// The estimator a new operator takes.
    fn into_named(self) -> Option<T> {
        match self {
            Self::Named(estimator) => Some(estimator),
            Self::Unwritten | Self::Cleared => None,
        }
    }

    /// Writes the attribute into `held`, leaving it alone when unwritten.
    fn apply(self, held: &mut Option<T>) {
        match self {
            Self::Unwritten => {}
            Self::Cleared => *held = None,
            Self::Named(estimator) => *held = Some(estimator),
        }
    }
}

/// The attributes an operator statement lists, the last of a repeated one
/// winning.
#[derive(Default)]
struct Attributes<'s> {
    commutator: Option<&'s ObjectName>,
    negator: Option<&'s ObjectName>,
    hashes: bool,
    merges: bool,
    restrict: Estimator<&'s ObjectName>,
    join: Estimator<&'s ObjectName>,
}

impl<'s> Attributes<'s> {
    fn of(options: &'s [OperatorOption]) -> Self {
        let mut attributes = Self::default();
        for option in options {
            match option {
                OperatorOption::Commutator(name) => attributes.commutator = Some(name),
                OperatorOption::Negator(name) => attributes.negator = Some(name),
                OperatorOption::Hashes => attributes.hashes = true,
                OperatorOption::Merges => attributes.merges = true,
                OperatorOption::Restrict(name) => {
                    attributes.restrict = Estimator::written(name.as_ref());
                }
                OperatorOption::Join(name) => attributes.join = Estimator::written(name.as_ref()),
            }
        }
        attributes
    }

    /// Refuses an attribute the operator's kind or result cannot carry, in
    /// the order `OperatorValidateParams` checks them.
    fn check(
        &self,
        kind: OperatorKind,
        result: Option<&CatalogName>,
        symbol: &str,
    ) -> Result<(), Error> {
        let join = self.join.is_named();
        let first = |checks: &[(bool, OperatorAttribute)]| {
            checks.iter().find_map(|&(set, attribute)| set.then_some(attribute))
        };
        if kind == OperatorKind::Prefix
            && let Some(attribute) = first(&[
                (self.commutator.is_some(), OperatorAttribute::Commutator),
                (join, OperatorAttribute::Join),
                (self.merges, OperatorAttribute::Merges),
                (self.hashes, OperatorAttribute::Hashes),
            ])
        {
            return Err(Error::OperatorAttributeNeedsBinary {
                operator_name: symbol.into(),
                attribute,
            });
        }
        if !result.is_some_and(is_bool)
            && let Some(attribute) = first(&[
                (self.negator.is_some(), OperatorAttribute::Negator),
                (self.restrict.is_named(), OperatorAttribute::Restrict),
                (join, OperatorAttribute::Join),
                (self.merges, OperatorAttribute::Merges),
                (self.hashes, OperatorAttribute::Hashes),
            ])
        {
            return Err(Error::OperatorAttributeNeedsBoolean {
                operator_name: symbol.into(),
                attribute,
            });
        }
        Ok(())
    }
}

/// What an operator statement binds names against.
struct Binder<'b> {
    builder: &'b ParserDBBuilder,
    session: &'b SessionRoles,
    arrays: &'b TypeArrays,
    /// The schemas an unqualified name reaches, in order.
    schemas: Vec<(String, bool)>,
    /// The symbol of the operator the statement names.
    symbol: &'b str,
}

impl<'b> Binder<'b> {
    fn new(
        builder: &'b ParserDBBuilder,
        session: &'b SessionRoles,
        arrays: &'b TypeArrays,
        symbol: &'b str,
    ) -> Self {
        Self { builder, session, arrays, schemas: reached_schemas(builder, session), symbol }
    }

    /// The function `name` reaches whose arguments `takes` accepts, with the
    /// identity it is stored under.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OperatorFunctionNotFound`] when no function matches
    /// and [`Error::OperatorFunctionNotExecutable`] when the creating role
    /// cannot execute the one that does.
    fn function(
        &self,
        name: &ObjectName,
        takes: impl Fn(Option<&[OperateFunctionArg]>) -> bool,
    ) -> Result<(CatalogName, &'b CreateFunction), Error> {
        let not_found = || {
            Error::OperatorFunctionNotFound {
                operator_name: self.symbol.into(),
                function_name: name.to_string().into(),
            }
        };
        let (written, quoted) = object_name_last_part(name).ok_or_else(not_found)?;
        let case = self.builder.identifier_case();
        let find = |identity: CatalogName| {
            self.builder
                .functions()
                .iter()
                .find(|(function, _)| {
                    function_name_matches(&identity, &function.name, case)
                        && takes(function.args.as_deref())
                })
                .map(|(function, metadata)| (identity, function.as_ref(), metadata))
        };
        let (identity, function, metadata) = match qualifier_of(name).named() {
            Some(schema) => find(CatalogName::new(schema, (written, quoted))),
            None => {
                self.schemas.iter().find_map(|(schema, schema_quoted)| {
                    find(CatalogName::new((schema, *schema_quoted), (written, quoted)))
                })
            }
        }
        .ok_or_else(not_found)?;
        self.require_executable(function, metadata)?;
        Ok((identity, function))
    }

    /// Binds an estimator attribute taking `inputs`.
    fn estimators(
        &self,
        written: &Estimator<&ObjectName>,
        inputs: &[&'static str],
    ) -> Result<Estimator<CatalogName>, Error> {
        Ok(match written {
            Estimator::Unwritten => Estimator::Unwritten,
            Estimator::Cleared => Estimator::Cleared,
            Estimator::Named(name) => {
                Estimator::Named(
                    self.function(name, |args| takes_estimator_inputs(inputs, self.arrays, args))?
                        .0,
                )
            }
        })
    }

    /// Refuses a function the creating role cannot execute.
    fn require_executable(
        &self,
        function: &CreateFunction,
        metadata: &FunctionMetadata,
    ) -> Result<(), Error> {
        match creator_lacking_execute(self.builder, self.session, metadata) {
            Some(creator) => {
                Err(Error::OperatorFunctionNotExecutable {
                    operator_name: self.symbol.into(),
                    function_name: object_name_last_part(&function.name)
                        .map_or("", |(name, _)| name)
                        .into(),
                    role_name: creator.into(),
                })
            }
            None => Ok(()),
        }
    }

    /// The position of the held operator `name` writes over `left` and
    /// `right`, an unqualified name walking the path.
    fn lookup(
        &self,
        catalog: &PostgresCatalog,
        name: &ObjectName,
        left: Option<&CatalogName>,
        right: &CatalogName,
    ) -> Option<usize> {
        let symbol = symbol_of(name);
        let reached = |identity: CatalogName| {
            catalog.operators().position(|held| held.is_named_by(&identity, left, right))
        };
        match qualifier_of(name).named() {
            Some(schema) => reached(CatalogName::new(schema, (symbol, false))),
            None => {
                self.schemas.iter().find_map(|(schema, quoted)| {
                    reached(CatalogName::new((schema, *quoted), (symbol, false)))
                })
            }
        }
    }
}

/// What a commutator or negator `CREATE OPERATOR` names reaches.
enum Link {
    /// The held operator at this position.
    Held(usize),
    /// A shell to create, the name reaching no operator.
    Shell(Box<PostgresCatalogOperator>),
}

impl Link {
    /// Finds the operator `name` writes over `left` and `right`, or the shell
    /// PostgreSQL creates for it in the schema the name places it in.
    fn resolve(
        binder: &Binder<'_>,
        catalog: &PostgresCatalog,
        name: &ObjectName,
        left: Option<&CatalogName>,
        right: &CatalogName,
    ) -> Result<Self, Error> {
        if let Some(position) = binder.lookup(catalog, name, left, right) {
            return Ok(Self::Held(position));
        }
        let target = place(binder.builder, binder.session, name, symbol_of(name))?;
        Ok(Self::Shell(Box::new(PostgresCatalogOperator {
            target,
            kind: if left.is_some() { OperatorKind::Binary } else { OperatorKind::Prefix },
            left: left.cloned(),
            right: right.clone(),
            result: None,
            implementation: None,
            commutator: None,
            negator: None,
            can_hash: false,
            can_merge: false,
            restriction_estimator: None,
            join_estimator: None,
        })))
    }

    /// Whether the link reaches `operator`, held at `held` when a shell of it
    /// is held.
    fn is_self(&self, held: Option<usize>, operator: &PostgresCatalogOperator) -> bool {
        match self {
            Self::Held(position) => Some(*position) == held,
            Self::Shell(shell) => shell.repeats(operator),
        }
    }
}

/// Refuses linking `operator` to `linked` while `linked` links to another
/// operator through the same attribute.
fn require_free(
    linked: &PostgresCatalogOperator,
    operator: &PostgresCatalogOperator,
    attribute: OperatorAttribute,
    symbol: &str,
) -> Result<(), Error> {
    let (back, links_back) = match attribute {
        OperatorAttribute::Commutator => {
            (linked.commutator.as_ref(), linked.commutes_with(operator))
        }
        _ => (linked.negator.as_ref(), linked.negates(operator)),
    };
    match back {
        Some(back) if !links_back => {
            Err(Error::OperatorLinkTaken {
                operator_name: symbol.into(),
                attribute,
                linked_name: linked.target.name().into(),
                linked_to: back.name().into(),
            })
        }
        _ => Ok(()),
    }
}

/// Points the commutator or negator link of `operator` at `target`.
fn set_link(
    operator: &mut PostgresCatalogOperator,
    attribute: OperatorAttribute,
    target: CatalogName,
) {
    match attribute {
        OperatorAttribute::Commutator => operator.commutator = Some(target),
        _ => operator.negator = Some(target),
    }
}

/// Whether `identity` is `pg_catalog.bool`.
fn is_bool(identity: &CatalogName) -> bool {
    identity.schema_matches(PG_CATALOG, false)
        && identifiers_match(identity.name(), identity.name_is_quoted(), "bool", false)
}

/// The symbol an operator name ends in.
fn symbol_of(name: &ObjectName) -> &str {
    object_name_last_part(name).map_or("", |(symbol, _)| symbol)
}

/// The schema-qualified symbol an operator `name` creates, in the schema it
/// names or else the first schema of the path that exists.
fn place(
    builder: &ParserDBBuilder,
    session: &SessionRoles,
    name: &ObjectName,
    symbol: &str,
) -> Result<CatalogName, Error> {
    if let Some(schema) = qualifier_of(name).named() {
        validate_relation_schema(builder, Some(schema), ObjectKind::Operator, symbol)?;
        return Ok(CatalogName::new(schema, (symbol, false)));
    }
    let qualifier =
        search_path_qualifier(builder, ObjectKind::Operator, symbol, session.creating_role())?;
    Ok(match qualifier {
        Some(schema) => {
            CatalogName::new((&schema.value, schema.quote_style.is_some()), (symbol, false))
        }
        None => CatalogName::new((DEFAULT_SCHEMA, false), (symbol, false)),
    })
}

/// The schemas an unqualified function or operator name reaches, in order,
/// `pg_catalog` first unless the path places it, the temporary schema left
/// out.
fn reached_schemas(builder: &ParserDBBuilder, session: &SessionRoles) -> Vec<(String, bool)> {
    let mut schemas: Vec<(String, bool)> = builder
        .search_path()
        .filter(|&(schema, quoted)| !schema.is_empty() && !is_temporary_schema(schema, quoted))
        .filter_map(|(schema, quoted)| {
            match (schema, session.creating_role()) {
                ("$user", Some(role)) => Some((String::from(role), true)),
                ("$user", None) => None,
                _ => Some((String::from(schema), quoted)),
            }
        })
        .collect();
    if !schemas.iter().any(|(schema, quoted)| identifiers_match(schema, *quoted, PG_CATALOG, false))
    {
        schemas.insert(0, (String::from(PG_CATALOG), false));
    }
    schemas
}

/// The identity of the type an operand declaration reaches.
fn operand(
    builder: &ParserDBBuilder,
    arrays: &TypeArrays,
    mut data_type: DataType,
) -> Result<CatalogName, Error> {
    domains::bind_type(builder, &mut data_type, arrays)?;
    type_identity(arrays, &data_type)
        .ok_or_else(|| Error::TypeNotFound { type_name: data_type.to_string() })
}
