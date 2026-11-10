//! `ALTER DEFAULT PRIVILEGES`, and the role each object is created by.
//!
//! PostgreSQL keeps a default per role, per kind of object and optionally per
//! schema in `pg_default_acl`, and reads it when that role creates an object
//! of that kind. The role creating an object is the current role, which `SET
//! ROLE` and `SET SESSION AUTHORIZATION` move, and it becomes the object's
//! owner. An input that never sets a role runs as one role it never names,
//! which is `None` here.

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use sqlparser::{
    ast::{
        Action, AlterDefaultPrivileges, AlterDefaultPrivilegesOperation, ContextModifier,
        CreateFunction, DefaultPrivilegesObjectType, DefaultPrivilegesOption, Expr, Grant,
        GrantObjects, Grantee, GranteeName, GranteesType, Ident, ObjectName, ObjectNamePart,
        Privileges, Reset, Set, SetSessionAuthorizationParamKind, Statement,
    },
    tokenizer::Span,
};

use super::{
    AccessResolution, ParserDBBuilder, SchemaQualifier, grantee_names, grantee_role_ident,
    idents_match, rename_grantee, role_matches_lookup_ident, schema_is_declared,
    schema_qualifiers_match, stored_role_name, validate_access_targets_against_builder,
};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, grantee_is_public, grantee_is_session_principal, grantees_match},
    structs::{FunctionMetadata, IdentifierCase},
    utils::{
        identifier_resolution::{SessionPrincipal, normalize_identifier},
        last_str,
        object_name::{qualifier_of, search_path_entry},
    },
};

/// Whether the roles a session runs as are followed under `dialect`, which
/// they are where the engine follows PostgreSQL's, as the dialects naming no
/// single engine do.
pub(super) fn tracks_session_roles(dialect: SqlparserDialect) -> bool {
    matches!(
        dialect,
        SqlparserDialect::Ansi | SqlparserDialect::Generic | SqlparserDialect::PostgreSql
    )
}

/// A role a session statement names, with the name it is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NamedRole {
    ident: Ident,
    stored: String,
}

impl NamedRole {
    fn of(ident: Ident) -> Self {
        Self { stored: stored_role_name(&ident), ident }
    }

    /// The role stored under `name`, which compares as written.
    fn stored(name: String) -> Self {
        Self { ident: Ident::with_quote('"', name.as_str()), stored: name }
    }
}

/// The roles a session runs as.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Roles {
    /// The role `SET SESSION AUTHORIZATION` names, or the one the input
    /// runs as, `None` while that one is unnamed.
    session: Option<NamedRole>,
    /// The role `SET ROLE` names, `None` while the session role acts.
    role: Option<NamedRole>,
}

impl Roles {
    /// The role statements run as.
    fn current(&self) -> Option<&NamedRole> {
        self.role.as_ref().or(self.session.as_ref())
    }

    /// Applies `change`, a session reset returning to `login`.
    fn change(&mut self, change: &Change, login: Option<&NamedRole>) {
        match change {
            Change::Role(role) => self.role.clone_from(role),
            // A new session role drops the role `SET ROLE` named.
            Change::Session(session) => {
                self.session = session.as_ref().or(login).cloned();
                self.role = None;
            }
        }
    }
}

/// The roles a session runs as, as `SET ROLE` and `SET SESSION AUTHORIZATION`
/// leave them.
///
/// A `SET LOCAL` holds until its transaction ends, a plain `SET` lasts past
/// it, and a rollback restores the catalog copy carrying this state.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionRoles {
    /// The role the input runs as, which a session reset returns to.
    login: Option<NamedRole>,
    committed: Roles,
    local: Option<Roles>,
}

impl SessionRoles {
    /// A session that starts as the role stored under `login`, or as the
    /// unnamed role when there is none.
    pub(super) fn starting_as(login: Option<String>) -> Self {
        let login = login.map(NamedRole::stored);
        Self { committed: Roles { session: login.clone(), role: None }, login, local: None }
    }

    fn current(&self) -> &Roles {
        self.local.as_ref().unwrap_or(&self.committed)
    }

    /// The stored name of the role objects are created by now.
    pub(super) fn creating_role(&self) -> Option<&str> {
        self.current().current().map(|role| role.stored.as_str())
    }

    /// The stored name of the role `SESSION_USER` names now.
    pub(super) fn session_role(&self) -> Option<&str> {
        self.current().session.as_ref().map(|role| role.stored.as_str())
    }

    /// Binds session-principal grantees to known DDL-time roles.
    pub(super) fn bind_grantees(&self, grantees: &mut [Grantee]) {
        for grantee in grantees {
            let role = match grantee_is_session_principal(grantee) {
                Some(SessionPrincipal::CurrentUser) => self.creating_role(),
                Some(SessionPrincipal::SessionUser) => self.session_role(),
                None => continue,
            };
            if let Some(role) = role {
                grantee.name =
                    Some(GranteeName::ObjectName(ObjectName::from(vec![Ident::with_quote(
                        '"', role,
                    )])));
            }
        }
    }

    /// Whether the session runs as `role`, as its current or its session
    /// role, which PostgreSQL refuses to drop or rename.
    pub(super) fn runs_as(&self, role: &Ident) -> bool {
        let current = self.current();
        [current.session.as_ref(), current.role.as_ref()]
            .into_iter()
            .flatten()
            .any(|held| idents_match(&held.ident, role))
    }

    /// Applies one role change. A `SET LOCAL` outside a transaction changes
    /// nothing, as PostgreSQL only warns about it.
    pub(super) fn apply(&mut self, change: &SessionChange, in_transaction: bool) {
        let login = self.login.as_ref();
        if change.local {
            if in_transaction {
                let mut local = self.local.take().unwrap_or_else(|| self.committed.clone());
                local.change(&change.change, login);
                self.local = Some(local);
            }
            return;
        }
        self.committed.change(&change.change, login);
        if let Some(local) = &mut self.local {
            local.change(&change.change, login);
        }
    }

    /// Drops what a `SET LOCAL` held, at the end of its transaction.
    pub(super) fn end_transaction(&mut self) {
        self.local = None;
    }
}

/// The role a statement moves the session to.
#[derive(Debug)]
enum Change {
    /// `SET ROLE`, `None` for `NONE` and its resets.
    Role(Option<NamedRole>),
    /// `SET SESSION AUTHORIZATION`, `None` for `DEFAULT` and its resets,
    /// which return to the role the input runs as.
    Session(Option<NamedRole>),
}

/// A statement moving the role a session runs as.
#[derive(Debug)]
pub(super) struct SessionChange {
    change: Change,
    /// Whether the statement wrote `LOCAL`.
    local: bool,
}

impl SessionChange {
    /// Reads the role change a statement makes, or [`None`] when it makes
    /// none. `hex_strings_may_be_integers` reads a role value as
    /// [`search_path_entry`] reads a schema.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSessionRole`] for a value list PostgreSQL
    /// refuses, which is more than one value or a value naming no role.
    pub(super) fn of(
        statement: &Statement,
        hex_strings_may_be_integers: bool,
    ) -> Result<Option<Self>, Error> {
        let session = |change| Ok(Some(Self { change, local: false }));
        match statement {
            Statement::Set(Set::SetRole { context_modifier, role_name }) => {
                Ok(Some(Self {
                    change: Parameter::Role.change(role_name.clone().map(NamedRole::of)),
                    local: *context_modifier == Some(ContextModifier::Local),
                }))
            }
            Statement::Set(Set::SetSessionAuthorization(parameter)) => {
                Ok(Some(Self {
                    change: Change::Session(match &parameter.kind {
                        SetSessionAuthorizationParamKind::Default => None,
                        SetSessionAuthorizationParamKind::User(user) => {
                            Some(NamedRole::of(user.clone()))
                        }
                    }),
                    local: parameter.scope == ContextModifier::Local,
                }))
            }
            Statement::Set(Set::SingleAssignment { scope, variable, values, .. }) => {
                let Some(parameter) = Parameter::of(variable) else {
                    return Ok(None);
                };
                let invalid = || Error::InvalidSessionRole { statement: statement.to_string() };
                let [value] = values.as_slice() else {
                    return Err(invalid());
                };
                let role = match value {
                    Expr::Identifier(ident)
                        if ident.quote_style.is_none()
                            && ident.value.eq_ignore_ascii_case("DEFAULT") =>
                    {
                        None
                    }
                    value => {
                        let (name, quoted) = search_path_entry(value, hex_strings_may_be_integers)
                            .map_err(|_| invalid())?;
                        Some(NamedRole::of(Ident {
                            value: name.into_owned(),
                            quote_style: quoted.then_some('"'),
                            span: Span::empty(),
                        }))
                    }
                };
                Ok(Some(Self {
                    change: parameter.change(role),
                    local: *scope == Some(ContextModifier::Local),
                }))
            }
            Statement::Reset(reset) => {
                match &reset.reset {
                    Reset::SessionAuthorization => session(Change::Session(None)),
                    Reset::ConfigurationParameter(name) => {
                        Parameter::of(name)
                            .map_or(Ok(None), |parameter| session(parameter.change(None)))
                    }
                    Reset::ALL => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// The role the change names, which has to exist.
    pub(super) fn named_role(&self) -> Option<&Ident> {
        match &self.change {
            Change::Role(role) | Change::Session(role) => role.as_ref().map(|role| &role.ident),
        }
    }
}

/// A configuration parameter holding a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parameter {
    Role,
    Session,
}

impl Parameter {
    /// The parameter a name sets, compared without case however it is quoted,
    /// as PostgreSQL compares parameter names.
    fn of(name: &ObjectName) -> Option<Self> {
        let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
            return None;
        };
        if name.value.eq_ignore_ascii_case("role") {
            Some(Self::Role)
        } else if name.value.eq_ignore_ascii_case("session_authorization") {
            Some(Self::Session)
        } else {
            None
        }
    }

    /// The change setting the parameter to `role`, where the role parameter
    /// reads a role stored as `none` as `NONE`.
    fn change(self, role: Option<NamedRole>) -> Change {
        match self {
            Self::Role => Change::Role(role.filter(|role| role.stored != "none")),
            Self::Session => Change::Session(role),
        }
    }
}

/// The kinds of object a default applies to, `ROUTINES` being `FUNCTIONS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultKind {
    /// Tables, views and materialized views.
    Relations,
    Sequences,
    Functions,
    Types,
    Schemas,
    LargeObjects,
}

impl DefaultKind {
    fn of(object_type: DefaultPrivilegesObjectType) -> Self {
        match object_type {
            DefaultPrivilegesObjectType::Tables => Self::Relations,
            DefaultPrivilegesObjectType::Sequences => Self::Sequences,
            DefaultPrivilegesObjectType::Functions | DefaultPrivilegesObjectType::Routines => {
                Self::Functions
            }
            DefaultPrivilegesObjectType::Types => Self::Types,
            DefaultPrivilegesObjectType::Schemas => Self::Schemas,
            DefaultPrivilegesObjectType::LargeObjects => Self::LargeObjects,
        }
    }

    /// Every privilege the kind has, which its owner holds at creation.
    fn every(self) -> Rights {
        let all =
            |rights: &[Rights]| rights.iter().fold(Rights::NONE, |all, right| all.with(*right));
        match self {
            Self::Relations => {
                all(&[
                    Rights::SELECT,
                    Rights::INSERT,
                    Rights::UPDATE,
                    Rights::DELETE,
                    Rights::TRUNCATE,
                    Rights::REFERENCES,
                    Rights::TRIGGER,
                    Rights::MAINTAIN,
                ])
            }
            Self::Sequences => all(&[Rights::SELECT, Rights::UPDATE, Rights::USAGE]),
            Self::Functions => Rights::EXECUTE,
            Self::Types => Rights::USAGE,
            Self::Schemas => all(&[Rights::USAGE, Rights::CREATE]),
            Self::LargeObjects => all(&[Rights::SELECT, Rights::UPDATE]),
        }
    }

    /// What PostgreSQL gives `PUBLIC` on a new object of the kind.
    fn public(self) -> Rights {
        match self {
            Self::Functions => Rights::EXECUTE,
            Self::Types => Rights::USAGE,
            _ => Rights::NONE,
        }
    }

    /// The items a row starts from, which are the built-in ones for a global
    /// row and none for a schema's, since that one only adds to the global one.
    fn start(self, per_schema: bool) -> Vec<Item> {
        if per_schema {
            return Vec::new();
        }
        [(Holder::Public, self.public()), (Holder::Owner, self.every())]
            .into_iter()
            .filter(|(_, rights)| !rights.is_empty())
            .map(|(holder, privileges)| Item { holder, privileges, grantable: Rights::NONE })
            .collect()
    }
}

/// A set of privileges, one bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rights(u16);

impl Rights {
    const NONE: Self = Self(0);
    const SELECT: Self = Self(1);
    const INSERT: Self = Self(1 << 1);
    const UPDATE: Self = Self(1 << 2);
    const DELETE: Self = Self(1 << 3);
    const TRUNCATE: Self = Self(1 << 4);
    const REFERENCES: Self = Self(1 << 5);
    const TRIGGER: Self = Self(1 << 6);
    const EXECUTE: Self = Self(1 << 7);
    const USAGE: Self = Self(1 << 8);
    const CREATE: Self = Self(1 << 9);
    const MAINTAIN: Self = Self(1 << 10);

    /// Each privilege beside the action spelling it, in the order PostgreSQL
    /// lists a relation's.
    const ACTIONS: [(Self, Action); 11] = [
        (Self::SELECT, Action::Select { columns: None }),
        (Self::INSERT, Action::Insert { columns: None }),
        (Self::UPDATE, Action::Update { columns: None }),
        (Self::DELETE, Action::Delete),
        (Self::TRUNCATE, Action::Truncate),
        (Self::REFERENCES, Action::References { columns: None }),
        (Self::TRIGGER, Action::Trigger),
        (Self::MAINTAIN, Action::Maintain),
        (Self::EXECUTE, Action::Execute { obj_type: None }),
        (Self::USAGE, Action::Usage),
        (Self::CREATE, Action::Create { obj_type: None }),
    ];

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The privilege an action names, when it is one a default can carry.
    fn of_action(action: &Action) -> Option<Self> {
        Self::ACTIONS.iter().find(|(_, spelled)| spelled == action).map(|(right, _)| *right)
    }

    /// The actions spelling each privilege held, in [`Self::ACTIONS`] order.
    fn actions(self) -> Vec<Action> {
        Self::ACTIONS
            .into_iter()
            .filter(|(right, _)| self.contains(*right))
            .map(|(_, action)| action)
            .collect()
    }

    /// The privileges a statement names for `kind`, refusing one the kind
    /// does not have.
    fn of(
        privileges: &Privileges,
        kind: DefaultKind,
        object_type: DefaultPrivilegesObjectType,
    ) -> Result<Self, Error> {
        let Privileges::Actions(actions) = privileges else {
            return Ok(kind.every());
        };
        actions.iter().try_fold(Self::NONE, |rights, action| {
            match Self::of_action(action).filter(|right| kind.every().contains(*right)) {
                Some(right) => Ok(rights.with(right)),
                None => {
                    Err(Error::InvalidDefaultPrivilege {
                        privilege: action.to_string(),
                        object_type: object_type.to_string(),
                    })
                }
            }
        })
    }
}

/// Who an item of a default gives privileges to.
#[derive(Debug, Clone)]
enum Holder {
    Public,
    /// The role the default belongs to, which owns what it creates.
    Owner,
    Role(Grantee),
}

impl Holder {
    fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Public, Self::Public) | (Self::Owner, Self::Owner) => true,
            (Self::Role(left), Self::Role(right)) => grantees_match(left, right),
            _ => false,
        }
    }
}

/// One holder's privileges in a default.
#[derive(Debug, Clone)]
struct Item {
    holder: Holder,
    privileges: Rights,
    /// The privileges the holder may grant on.
    grantable: Rights,
}

/// Adds `item` to `items`, widening the item of the same holder.
fn merge(items: &mut Vec<Item>, item: &Item) {
    match items.iter_mut().find(|held| held.holder.is(&item.holder)) {
        Some(held) => {
            held.privileges = held.privileges.with(item.privileges);
            held.grantable = held.grantable.with(item.grantable);
        }
        None => items.push(item.clone()),
    }
}

/// Whether two item lists give the same privileges, in any order.
fn same_items(left: &[Item], right: &[Item]) -> bool {
    left.len() == right.len()
        && left.iter().all(|item| {
            right.iter().any(|other| {
                other.holder.is(&item.holder)
                    && other.privileges == item.privileges
                    && other.grantable == item.grantable
            })
        })
}

/// One `pg_default_acl` row.
#[derive(Debug, Clone)]
struct Row {
    /// The stored name of the role the default belongs to.
    role: Option<String>,
    kind: DefaultKind,
    /// The schema the default is limited to, with whether it was quoted.
    schema: Option<(String, bool)>,
    items: Vec<Item>,
}

/// What an `ALTER DEFAULT PRIVILEGES` does to the items it reaches.
#[derive(Debug, Clone, Copy)]
enum Edit {
    Grant { grant_option: bool },
    Revoke { grant_option_only: bool },
}

/// The defaults `ALTER DEFAULT PRIVILEGES` recorded, as `pg_default_acl`
/// holds them.
#[derive(Debug, Clone, Default)]
pub(super) struct DefaultPrivileges {
    rows: Vec<Row>,
}

impl DefaultPrivileges {
    /// Records one `ALTER DEFAULT PRIVILEGES`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefaultPrivilege`] for a privilege the object
    /// type does not have, [`Error::DefaultPrivilegesInSchema`] for `IN
    /// SCHEMA` on `SCHEMAS` or `LARGE OBJECTS`,
    /// [`Error::SchemaNotFoundForDefaultPrivileges`] for a schema the input
    /// never creates and, under the closed world,
    /// [`Error::RoleNotFoundForGrant`] for a role it never creates.
    pub(super) fn alter(
        &mut self,
        statement: AlterDefaultPrivileges,
        session: &SessionRoles,
        builder: &ParserDBBuilder,
        access_resolution: AccessResolution,
    ) -> Result<(), Error> {
        let AlterDefaultPrivileges { options, operation } = statement;
        let (privileges, object_type, grantees, edit) = match operation {
            AlterDefaultPrivilegesOperation::Grant {
                privileges,
                object_type,
                grantees,
                with_grant_option,
            } => {
                (privileges, object_type, grantees, Edit::Grant { grant_option: with_grant_option })
            }
            AlterDefaultPrivilegesOperation::Revoke {
                grant_option_for,
                privileges,
                object_type,
                grantees,
                ..
            } => {
                (
                    privileges,
                    object_type,
                    grantees,
                    Edit::Revoke { grant_option_only: grant_option_for },
                )
            }
        };
        let kind = DefaultKind::of(object_type);
        let rights = Rights::of(&privileges, kind, object_type)?;

        let mut roles = Vec::new();
        let mut schemas = Vec::new();
        for option in options {
            match option {
                DefaultPrivilegesOption::ForRole(named) => roles.extend(named),
                DefaultPrivilegesOption::InSchema(named) => schemas.extend(named),
            }
        }
        if !schemas.is_empty() && matches!(kind, DefaultKind::Schemas | DefaultKind::LargeObjects) {
            return Err(Error::DefaultPrivilegesInSchema { object_type: object_type.to_string() });
        }

        if access_resolution == AccessResolution::ClosedWorld {
            if let Some(absent) = roles.iter().find(|role| {
                !builder
                    .roles()
                    .iter()
                    .any(|(declared, ())| role_matches_lookup_ident(declared, role))
            }) {
                return Err(Error::RoleNotFoundForGrant { role_name: absent.value.clone() });
            }
            validate_access_targets_against_builder(builder, &grantees, None)?;
        }
        let mut row_schemas = Vec::with_capacity(schemas.len().max(1));
        for schema in &schemas {
            // PostgreSQL's grammar takes a bare schema name here.
            let [ObjectNamePart::Identifier(ident)] = schema.0.as_slice() else {
                return Err(Error::SchemaNotFoundForDefaultPrivileges {
                    schema_name: schema.to_string(),
                });
            };
            let (name, quoted) = (ident.value.as_str(), ident.quote_style.is_some());
            if !schema_is_declared(builder, name, quoted) {
                return Err(Error::SchemaNotFoundForDefaultPrivileges {
                    schema_name: name.to_string(),
                });
            }
            row_schemas.push(Some((name.to_string(), quoted)));
        }
        if row_schemas.is_empty() {
            row_schemas.push(None);
        }

        let row_roles: Vec<Option<String>> = if roles.is_empty() {
            vec![session.creating_role().map(ToString::to_string)]
        } else {
            roles.iter().map(|role| Some(stored_role_name(role))).collect()
        };
        let case = builder.identifier_case();
        for role in &row_roles {
            let holders: Vec<Holder> = grantees
                .iter()
                .filter_map(|grantee| holder(grantee, role.as_deref(), session))
                .collect();
            for schema in &row_schemas {
                let key = Key {
                    role: role.as_deref(),
                    kind,
                    schema: schema.as_ref().map(|(name, quoted)| (name.as_str(), *quoted)),
                };
                self.edit(key, case, &holders, rights, edit);
            }
        }
        Ok(())
    }

    /// Applies one edit to the row `key` names, creating the row from its
    /// start and dropping it once it is back there.
    fn edit(
        &mut self,
        key: Key<'_>,
        case: IdentifierCase,
        holders: &[Holder],
        rights: Rights,
        edit: Edit,
    ) {
        let position = self.rows.iter().position(|row| row.key().is(key, case));
        let kind = key.kind;
        let mut row = match position {
            Some(position) => self.rows.remove(position),
            None => {
                Row {
                    role: key.role.map(ToString::to_string),
                    kind,
                    schema: key.schema.map(|(name, quoted)| (name.to_string(), quoted)),
                    items: kind.start(key.schema.is_some()),
                }
            }
        };
        for holder in holders {
            let item = row.items.iter_mut().find(|item| item.holder.is(holder));
            match (edit, item) {
                (Edit::Grant { grant_option }, item) => {
                    // PostgreSQL gives `PUBLIC` the privilege alone.
                    let grantable = if grant_option && !matches!(holder, Holder::Public) {
                        rights
                    } else {
                        Rights::NONE
                    };
                    match item {
                        Some(item) => {
                            item.privileges = item.privileges.with(rights);
                            item.grantable = item.grantable.with(grantable);
                        }
                        None => {
                            row.items.push(Item {
                                holder: holder.clone(),
                                privileges: rights,
                                grantable,
                            });
                        }
                    }
                }
                (Edit::Revoke { grant_option_only }, Some(item)) => {
                    item.grantable = item.grantable.without(rights);
                    if !grant_option_only {
                        item.privileges = item.privileges.without(rights);
                    }
                }
                (Edit::Revoke { .. }, None) => {}
            }
        }
        row.items.retain(|item| !item.privileges.is_empty());
        if !same_items(&row.items, &kind.start(row.schema.is_some())) {
            self.rows.push(row);
        }
    }

    /// The items a new object of `kind` in `schema`, created by `owner`,
    /// starts with, which are the global row or the built-in default widened
    /// by the row of its schema. PostgreSQL stores an empty merge as no list
    /// at all, which reads as the built-in default.
    fn items(
        &self,
        owner: Option<&str>,
        kind: DefaultKind,
        schema: SchemaQualifier<'_>,
        case: IdentifierCase,
    ) -> Vec<Item> {
        let global = Key { role: owner, kind, schema: None };
        let mut items = self
            .rows
            .iter()
            .find(|row| row.key().is(global, case))
            .map_or_else(|| kind.start(false), |row| row.items.clone());
        if let Some(row) = self.rows.iter().find(|row| {
            row.role.as_deref() == owner
                && row.kind == kind
                && row.schema.as_ref().is_some_and(|(name, quoted)| {
                    schema_qualifiers_match(Some((name.as_str(), *quoted)), schema, case)
                })
        }) {
            for item in &row.items {
                merge(&mut items, item);
            }
        }
        if items.is_empty() {
            return kind.start(false);
        }
        items
    }

    /// Whether a default belongs to `role` or gives it privileges, which
    /// keeps PostgreSQL from dropping the role.
    pub(super) fn references_role(&self, role: &Ident) -> bool {
        let name = stored_role_name(role);
        self.rows.iter().any(|row| {
            row.role.as_deref() == Some(name.as_str())
                || row.items.iter().any(
                    |item| matches!(&item.holder, Holder::Role(grantee) if grantee_names(grantee, role)),
                )
        })
    }

    /// Follows a role rename, as the defaults name roles by identity.
    pub(super) fn rename_role(&mut self, previous: &Ident, replacement: &Ident) {
        let previous_name = stored_role_name(previous);
        for row in &mut self.rows {
            if row.role.as_deref() == Some(previous_name.as_str()) {
                row.role = Some(stored_role_name(replacement));
            }
            for item in &mut row.items {
                if let Holder::Role(grantee) = &mut item.holder
                    && grantee_names(grantee, previous)
                {
                    rename_grantee(grantee, replacement);
                }
            }
        }
    }

    /// Follows a schema rename.
    pub(super) fn rename_schema(
        &mut self,
        from: (&str, bool),
        to: (&str, bool),
        case: IdentifierCase,
    ) {
        for row in &mut self.rows {
            if let Some((name, quoted)) = &mut row.schema
                && case.identifiers_match(name, *quoted, from.0, from.1)
            {
                *name = to.0.to_string();
                *quoted = to.1;
            }
        }
    }

    /// Drops the defaults limited to a dropped schema, which PostgreSQL drops
    /// with it.
    pub(super) fn drop_schema(&mut self, schema: (&str, bool), case: IdentifierCase) {
        self.rows.retain(|row| {
            !row.schema.as_ref().is_some_and(|(name, quoted)| {
                case.identifiers_match(name, *quoted, schema.0, schema.1)
            })
        });
    }
}

impl Row {
    fn key(&self) -> Key<'_> {
        Key {
            role: self.role.as_deref(),
            kind: self.kind,
            schema: self.schema.as_ref().map(|(name, quoted)| (name.as_str(), *quoted)),
        }
    }
}

/// What tells one row from another.
#[derive(Debug, Clone, Copy)]
struct Key<'a> {
    role: Option<&'a str>,
    kind: DefaultKind,
    schema: Option<(&'a str, bool)>,
}

impl Key<'_> {
    fn is(self, other: Key<'_>, case: IdentifierCase) -> bool {
        self.role == other.role
            && self.kind == other.kind
            && match (self.schema, other.schema) {
                (None, None) => true,
                (Some((left, left_quoted)), Some((right, right_quoted))) => {
                    case.identifiers_match(left, left_quoted, right, right_quoted)
                }
                _ => false,
            }
    }
}

/// Reads who a grantee of a default for `role` names, folding the role itself
/// into its owner item. A session principal is the role the session runs as
/// when the statement runs, and one naming the role the input never names
/// reaches no role anything can ask about unless it is the default's own.
fn holder(grantee: &Grantee, role: Option<&str>, session: &SessionRoles) -> Option<Holder> {
    if grantee_is_public(grantee) {
        return Some(Holder::Public);
    }
    if let Some(principal) = grantee_is_session_principal(grantee) {
        let current = session.current();
        let resolved = match principal {
            SessionPrincipal::CurrentUser => current.current(),
            SessionPrincipal::SessionUser => current.session.as_ref(),
        };
        if resolved.map(|resolved| resolved.stored.as_str()) == role {
            return Some(Holder::Owner);
        }
        return resolved.map(|resolved| {
            Holder::Role(Grantee {
                grantee_type: GranteesType::None,
                name: Some(GranteeName::ObjectName(ObjectName::from(vec![resolved.ident.clone()]))),
            })
        });
    }
    match grantee_role_ident(grantee) {
        Some(ident) if role == Some(stored_role_name(ident).as_str()) => Some(Holder::Owner),
        _ => Some(Holder::Role(grantee.clone())),
    }
}

/// The role creating an object, and the defaults it creates the object
/// under.
#[derive(Clone, Copy)]
pub(super) struct Creator<'a> {
    /// The stored name of the role, which becomes the owner.
    owner: Option<&'a str>,
    defaults: &'a DefaultPrivileges,
}

impl<'a> Creator<'a> {
    pub(super) fn new(owner: Option<&'a str>, defaults: &'a DefaultPrivileges) -> Self {
        Self { owner, defaults }
    }

    /// The role the session creates objects as, under its defaults.
    pub(super) fn session(session: &'a SessionRoles, defaults: &'a DefaultPrivileges) -> Self {
        Self::new(session.creating_role(), defaults)
    }

    /// The owner the new object records.
    pub(super) fn owner(&self) -> Option<String> {
        self.owner.map(ToString::to_string)
    }

    /// The role the statement runs as, which `"$user"` on the search path
    /// names.
    pub(super) fn role(&self) -> Option<&'a str> {
        self.owner
    }

    fn items(
        &self,
        kind: DefaultKind,
        schema: SchemaQualifier<'_>,
        case: IdentifierCase,
    ) -> Vec<Item> {
        self.defaults.items(self.owner, kind, schema, case)
    }

    /// The metadata a new function in `schema` starts with, which holds its
    /// owner and who may execute it.
    pub(super) fn function_metadata(
        &self,
        schema: SchemaQualifier<'_>,
        case: IdentifierCase,
    ) -> FunctionMetadata {
        let items = self.items(DefaultKind::Functions, schema, case);
        let holds = |holder: &Holder| {
            items
                .iter()
                .any(|item| item.holder.is(holder) && item.privileges.contains(Rights::EXECUTE))
        };
        let grants: Vec<(Grantee, bool)> = items
            .iter()
            .filter(|item| item.privileges.contains(Rights::EXECUTE))
            .filter_map(|item| {
                match &item.holder {
                    Holder::Role(grantee) => {
                        Some((grantee.clone(), item.grantable.contains(Rights::EXECUTE)))
                    }
                    Holder::Public | Holder::Owner => None,
                }
            })
            .collect();
        let mut metadata = FunctionMetadata::default();
        metadata.set_owner(self.owner());
        metadata.set_execute_defaults(holds(&Holder::Public), holds(&Holder::Owner), &grants);
        metadata
    }

    /// The privileges the owner of a new table in `schema` holds.
    pub(super) fn owner_table_privileges(
        &self,
        schema: SchemaQualifier<'_>,
        case: IdentifierCase,
    ) -> Vec<Action> {
        self.items(DefaultKind::Relations, schema, case)
            .iter()
            .find(|item| matches!(item.holder, Holder::Owner))
            .map_or_else(Vec::new, |item| item.privileges.actions())
    }

    /// Records the grants the defaults give on the new object `objects`
    /// names, one per holder other than the owner and whether it may grant
    /// on, as a `GRANT` naming the object would.
    pub(super) fn grant(
        &self,
        mut builder: ParserDBBuilder,
        kind: DefaultKind,
        objects: &GrantObjects,
    ) -> ParserDBBuilder {
        let schema = match objects {
            GrantObjects::Tables(names) | GrantObjects::Sequences(names) => {
                names.first().and_then(|name| qualifier_of(name).named())
            }
            _ => None,
        };
        let items = self.items(kind, schema, builder.identifier_case());
        for item in items {
            let grantee = match item.holder {
                Holder::Owner => continue,
                Holder::Public => Grantee { grantee_type: GranteesType::Public, name: None },
                Holder::Role(grantee) => grantee,
            };
            for (rights, with_grant_option) in
                [(item.privileges.without(item.grantable), false), (item.grantable, true)]
            {
                if rights.is_empty() {
                    continue;
                }
                // Every privilege covers the whole object, so no column grant
                // arises, and the owner the defaults belong to grants it.
                let grant = Grant {
                    privileges: Privileges::Actions(rights.actions()),
                    objects: Some(objects.clone()),
                    grantees: vec![grantee.clone()],
                    with_grant_option,
                    as_grantor: None,
                    granted_by: self.owner.map(super::grantors::grantor_ident),
                    current_grants: None,
                };
                builder = builder.add_table_grant(alloc::sync::Arc::new(grant), ());
            }
        }
        builder
    }
}

/// Refuses a function the role creating it could not execute, as the `sql`
/// and `plpgsql` validators do through `CheckFunctionValidatorAccess`.
///
/// Only a role the input names and declares as no superuser is checked, and
/// only when every role it inherits from is declared, since any other role
/// may hold a privilege the input does not show.
///
/// # Errors
///
/// Returns [`Error::FunctionNotExecutableByCreator`] when neither the role
/// nor any role it inherits from may execute the function.
pub(super) fn require_creator_executes(
    builder: &ParserDBBuilder,
    session: &SessionRoles,
    function: &CreateFunction,
    metadata: &FunctionMetadata,
) -> Result<(), Error> {
    use crate::traits::RoleLike as _;

    // Without `LANGUAGE`, only a `RETURN` or `BEGIN ATOMIC` body parses, which
    // is `sql`.
    let validated = function.language.as_ref().is_none_or(|language| {
        matches!(
            normalize_identifier(&language.value, language.quote_style.is_some()).as_ref(),
            "sql" | "plpgsql"
        )
    });
    let Some(creator) = session.creating_role().filter(|_| validated) else {
        return Ok(());
    };
    let declared = |name: &str| {
        builder
            .roles()
            .iter()
            .map(|(role, ())| role.as_ref())
            .find(|role| role.stored_name() == name)
    };
    let Some(role) = declared(creator).filter(|role| !role.is_superuser()) else {
        return Ok(());
    };

    let mut holders = vec![role];
    let mut next = 0;
    while let Some(holder) = holders.get(next).copied() {
        next += 1;
        let holder_name = holder.stored_name();
        for membership in builder.role_memberships().iter().map(|(membership, ())| membership) {
            if !membership.grants.iter().any(|grant| grant.inherit)
                || stored_role_name(&membership.member) != holder_name
            {
                continue;
            }
            let Some(granted) = declared(&stored_role_name(&membership.role)) else {
                return Ok(());
            };
            if !holders.iter().any(|held| core::ptr::eq(*held, granted)) {
                holders.push(granted);
            }
        }
    }

    if holders.iter().any(|holder| metadata.can_execute(holder)) {
        return Ok(());
    }
    Err(Error::FunctionNotExecutableByCreator {
        function_name: last_str(&function.name).to_string(),
        role_name: creator.to_string(),
    })
}
