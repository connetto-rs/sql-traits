//! One standing `ALTER DEFAULT PRIVILEGES` entry, as a `pg_default_acl` row
//! holds it.

use alloc::{borrow::Cow, vec::Vec};

use sqlparser::ast::Action;

/// The kind of object a default privilege applies to, as
/// `pg_default_acl.defaclobjtype` names it. `ROUTINES` is spelled
/// [`Functions`](Self::Functions), as PostgreSQL stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DefaultPrivilegeKind {
    /// `r`, `ON TABLES`, which reaches tables, views, materialized views and
    /// foreign tables.
    Tables,
    /// `S`, `ON SEQUENCES`.
    Sequences,
    /// `f`, `ON FUNCTIONS` and `ON ROUTINES`.
    Functions,
    /// `T`, `ON TYPES`, which reaches domains as well.
    Types,
    /// `n`, `ON SCHEMAS`.
    Schemas,
    /// `L`, `ON LARGE OBJECTS`.
    LargeObjects,
}

/// Who a grant of a default privilege names, as an item of
/// `pg_default_acl.defaclacl` does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DefaultGrantee<'a> {
    /// Every role, through the `PUBLIC` pseudo-role.
    Public,
    /// The role the default belongs to, which owns each object it creates.
    /// PostgreSQL names it as that role, which an input running as a role it
    /// never names cannot spell.
    Owner,
    /// Any other role, by the name PostgreSQL stores.
    Role(Cow<'a, str>),
}

impl DefaultGrantee<'_> {
    /// Returns the grantee with its name copied, so it outlives the database
    /// it was read from.
    #[must_use]
    pub fn into_owned(self) -> DefaultGrantee<'static> {
        match self {
            Self::Public => DefaultGrantee::Public,
            Self::Owner => DefaultGrantee::Owner,
            Self::Role(name) => DefaultGrantee::Role(Cow::Owned(name.into_owned())),
        }
    }
}

/// The privileges one grantee holds on each new object a default reaches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DefaultGrant<'a> {
    grantee: DefaultGrantee<'a>,
    privileges: Vec<Action>,
    grantable: Vec<Action>,
}

impl<'a> DefaultGrant<'a> {
    /// Creates the grant of `privileges` to `grantee`, which may grant on the
    /// ones `grantable` lists, each also in `privileges`. Every action names no
    /// column, since a default covers whole objects.
    #[must_use]
    pub const fn new(
        grantee: DefaultGrantee<'a>,
        privileges: Vec<Action>,
        grantable: Vec<Action>,
    ) -> Self {
        Self { grantee, privileges, grantable }
    }

    /// Returns who the grant names.
    #[must_use]
    pub const fn grantee(&self) -> &DefaultGrantee<'a> {
        &self.grantee
    }

    /// Returns the privileges the grantee holds.
    #[must_use]
    pub fn privileges(&self) -> &[Action] {
        &self.privileges
    }

    /// Returns the privileges the grantee holds with grant option.
    #[must_use]
    pub fn grantable(&self) -> &[Action] {
        &self.grantable
    }

    /// Returns the grant with the grantee's name copied, so it outlives the
    /// database it was read from.
    #[must_use]
    pub fn into_owned(self) -> DefaultGrant<'static> {
        DefaultGrant {
            grantee: self.grantee.into_owned(),
            privileges: self.privileges,
            grantable: self.grantable,
        }
    }
}

/// One standing default, as a `pg_default_acl` row holds it.
///
/// A default without a schema replaces the privileges PostgreSQL gives each
/// new object of its kind, so it lists the owner's grant beside the others. A
/// default limited to a schema only adds to that, so it lists what it adds.
/// A default back at its starting point is no longer standing, as PostgreSQL
/// deletes its row.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::{ast::Action, dialect::PostgreSqlDialect};
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "
/// CREATE SCHEMA api;
/// ALTER DEFAULT PRIVILEGES IN SCHEMA api GRANT SELECT ON TABLES TO PUBLIC;
/// ",
/// )?;
/// let entries: Vec<DefaultPrivilege<'_>> = db.default_privileges().unwrap().collect();
/// let [entry] = entries.as_slice() else { unreachable!() };
/// assert_eq!(entry.kind(), DefaultPrivilegeKind::Tables);
/// assert_eq!(entry.schema(), Some("api"));
/// // A default limited to a schema lists only what it adds.
/// let [grant] = entry.grants() else { unreachable!() };
/// assert_eq!(grant.grantee(), &DefaultGrantee::Public);
/// assert_eq!(grant.privileges(), [Action::Select { columns: None }]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DefaultPrivilege<'a> {
    role: Option<Cow<'a, str>>,
    kind: DefaultPrivilegeKind,
    schema: Option<Cow<'a, str>>,
    grants: Vec<DefaultGrant<'a>>,
}

impl<'a> DefaultPrivilege<'a> {
    /// Creates the default `role` holds for objects of `kind` it creates in
    /// `schema`, or in any schema when `schema` is `None`, giving each grantee
    /// of `grants` once.
    #[must_use]
    pub const fn new(
        role: Option<Cow<'a, str>>,
        kind: DefaultPrivilegeKind,
        schema: Option<Cow<'a, str>>,
        grants: Vec<DefaultGrant<'a>>,
    ) -> Self {
        Self { role, kind, schema, grants }
    }

    /// Returns the role the default belongs to, named as
    /// [`FunctionLike::owner`](crate::traits::FunctionLike::owner) names it,
    /// or `None` for the role an input runs as without naming it.
    #[must_use]
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    /// Returns the kind of object the default applies to.
    #[must_use]
    pub const fn kind(&self) -> DefaultPrivilegeKind {
        self.kind
    }

    /// Returns the name PostgreSQL stores for the schema the default is
    /// limited to, or `None` for a default reaching every schema.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Returns the grant to each grantee.
    #[must_use]
    pub fn grants(&self) -> &[DefaultGrant<'a>] {
        &self.grants
    }

    /// Returns the grant to `grantee`, or `None` when the default gives it
    /// nothing.
    #[must_use]
    pub fn grant_to(&self, grantee: &DefaultGrantee<'_>) -> Option<&DefaultGrant<'a>> {
        self.grants.iter().find(|grant| grant.grantee == *grantee)
    }

    /// Returns the default with every name it borrows copied, so it outlives
    /// the database it was read from.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let owned: Vec<DefaultPrivilege<'static>> = {
    ///     let db = ParserDB::parse::<PostgreSqlDialect>(
    ///         "
    /// CREATE ROLE migrator; CREATE ROLE reader;
    /// ALTER DEFAULT PRIVILEGES FOR ROLE migrator GRANT SELECT ON TABLES TO reader, PUBLIC;
    /// ",
    ///     )?;
    ///     db.default_privileges().unwrap().map(DefaultPrivilege::into_owned).collect()
    /// };
    /// let [entry] = owned.as_slice() else { unreachable!() };
    /// assert_eq!(entry.role(), Some("migrator"));
    /// assert!(entry.grant_to(&DefaultGrantee::Role("reader".into())).is_some());
    /// assert!(entry.grant_to(&DefaultGrantee::Public).is_some());
    /// assert!(entry.grant_to(&DefaultGrantee::Owner).is_some());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn into_owned(self) -> DefaultPrivilege<'static> {
        DefaultPrivilege {
            role: self.role.map(|role| Cow::Owned(role.into_owned())),
            kind: self.kind,
            schema: self.schema.map(|schema| Cow::Owned(schema.into_owned())),
            grants: self.grants.into_iter().map(DefaultGrant::into_owned).collect(),
        }
    }
}
