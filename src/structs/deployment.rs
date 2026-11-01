//! What a deployment runs its requests as, which the schema cannot say.

use alloc::{string::String, vec::Vec};

use sqlparser::ast::DataType;

use crate::{errors::DeploymentError, traits::DatabaseLike};

/// The roles a deployment's requests run as and the session settings its
/// policies read.
///
/// A request logs in as one role and acts as one of the acting roles, as
/// PostgREST and Supabase switch roles per request with `SET LOCAL ROLE`.
/// `current_user` and `current_role` are therefore the acting role and
/// `session_user` the login role. A pooled deployment acting as its login
/// role names that role as its only acting role.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
/// use sqlparser::{ast::DataType, dialect::PostgreSqlDialect};
///
/// # fn main() -> Result<(), Box<dyn core::error::Error>> {
/// let database = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE ROLE reader LOGIN NOINHERIT;
///      CREATE ROLE anonymous NOLOGIN;
///      CREATE ROLE member NOLOGIN;",
/// )?;
/// let deployment = Deployment::new(
///     "reader",
///     ["anonymous", "member"],
///     [
///         SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Uuid))?,
///         SessionSetting::value(
///             "app.subjects",
///             SettingValue::Set {
///                 element: DataType::Text,
///                 packing: SetPacking::Delimited(",".into()),
///             },
///         )?,
///     ],
/// )?;
///
/// let resolved = deployment.resolve(&database)?;
/// assert_eq!(resolved.login_role().name(), deployment.login_role());
/// assert_eq!(deployment.acting_roles().collect::<Vec<_>>(), ["anonymous", "member"]);
///
/// // A translator finds the caller among the settings, whatever case a policy spells it in.
/// let caller = deployment.setting("APP.USER_ID").expect("declared");
/// assert_eq!(caller.content(), &SettingContent::Value(SettingValue::Caller(DataType::Uuid)));
/// assert_eq!(caller.presence(), SettingPresence::Always);
/// assert_eq!(deployment.settings().len(), 2);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    login_role: String,
    acting_roles: Vec<String>,
    settings: Vec<SessionSetting>,
}

impl Deployment {
    /// Describes a deployment logging in as `login_role` and acting as each of
    /// `acting_roles`, its policies reading `settings`.
    ///
    /// Role names are the names PostgreSQL stores.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NoActingRole`] when `acting_roles` is empty,
    /// [`DeploymentError::DuplicateActingRole`] when it names a role twice,
    /// and [`DeploymentError::DuplicateSetting`] when two settings share a
    /// name compared without case, as PostgreSQL compares them.
    pub fn new<R: Into<String>>(
        login_role: impl Into<String>,
        acting_roles: impl IntoIterator<Item = R>,
        settings: impl IntoIterator<Item = SessionSetting>,
    ) -> Result<Self, DeploymentError> {
        let mut roles: Vec<String> = Vec::new();
        for role in acting_roles {
            let role = role.into();
            if roles.contains(&role) {
                return Err(DeploymentError::DuplicateActingRole { role });
            }
            roles.push(role);
        }
        if roles.is_empty() {
            return Err(DeploymentError::NoActingRole);
        }
        let mut declared: Vec<SessionSetting> = Vec::new();
        for setting in settings {
            if declared.iter().any(|seen| seen.is_named(&setting.name)) {
                return Err(DeploymentError::DuplicateSetting { name: setting.name });
            }
            declared.push(setting);
        }
        Ok(Self { login_role: login_role.into(), acting_roles: roles, settings: declared })
    }

    /// The role every request logs in as, which `session_user` names.
    #[must_use]
    pub fn login_role(&self) -> &str {
        &self.login_role
    }

    /// The roles a request may act as, in declaration order.
    pub fn acting_roles(&self) -> impl Iterator<Item = &str> {
        self.acting_roles.iter().map(String::as_str)
    }

    /// The settings the deployment's policies may read, in declaration order.
    #[must_use]
    pub fn settings(&self) -> &[SessionSetting] {
        &self.settings
    }

    /// The setting named `name`, compared without case.
    #[must_use]
    pub fn setting(&self, name: &str) -> Option<&SessionSetting> {
        self.settings.iter().find(|setting| setting.is_named(name))
    }

    /// The description's roles as the roles `database` declares.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::UndeclaredRole`] naming the first role,
    /// login role first, that `database` does not declare.
    pub fn resolve<'db, DB: DatabaseLike>(
        &self,
        database: &'db DB,
    ) -> Result<ResolvedDeployment<'db, DB>, DeploymentError> {
        let declared = |name: &str| {
            database.role(name).ok_or_else(|| DeploymentError::UndeclaredRole { role: name.into() })
        };
        let login_role = declared(&self.login_role)?;
        let acting_roles =
            self.acting_roles.iter().map(|role| declared(role)).collect::<Result<Vec<_>, _>>()?;
        Ok(ResolvedDeployment { login_role, acting_roles })
    }
}

/// A [`Deployment`]'s roles as declared in one database.
#[derive(Debug)]
pub struct ResolvedDeployment<'db, DB: DatabaseLike> {
    login_role: &'db DB::Role,
    acting_roles: Vec<&'db DB::Role>,
}

impl<'db, DB: DatabaseLike> ResolvedDeployment<'db, DB> {
    /// The role every request logs in as.
    #[must_use]
    pub fn login_role(&self) -> &'db DB::Role {
        self.login_role
    }

    /// The roles a request may act as, in declaration order.
    pub fn acting_roles(&self) -> impl Iterator<Item = &'db DB::Role> + '_ {
        self.acting_roles.iter().copied()
    }
}

/// One session setting a deployment's policies read with `current_setting`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSetting {
    name: String,
    content: SettingContent,
    presence: SettingPresence,
}

impl SessionSetting {
    /// Declares the setting `name` holding one value, always set.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::InvalidSettingName`] unless `name` is a
    /// custom setting name, two or more non-empty parts joined by dots, and
    /// [`DeploymentError::EmptySeparator`] for a set split on the empty
    /// string, which `string_to_array` reads as one field.
    pub fn value(name: impl Into<String>, value: SettingValue) -> Result<Self, DeploymentError> {
        let name = custom_name(name.into())?;
        if value.has_empty_separator() {
            return Err(DeploymentError::EmptySeparator { setting: name });
        }
        Ok(Self { name, content: SettingContent::Value(value), presence: SettingPresence::Always })
    }

    /// Declares the setting `name` holding a JSON document whose `fields` the
    /// policies read, always set.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::InvalidSettingName`] as
    /// [`SessionSetting::value`] does, [`DeploymentError::NoJsonFields`] when
    /// `fields` is empty, [`DeploymentError::DuplicateJsonField`] when two
    /// fields share a path, and [`DeploymentError::EmptySeparator`] for a
    /// field split on the empty string.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::DataType;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::DeploymentError> {
    /// // PostgREST's token, whose `sub` Supabase's `auth.uid()` reads as the caller.
    /// let claims = SessionSetting::json(
    ///     "request.jwt.claims",
    ///     [JsonField::new(["sub"], SettingValue::Caller(DataType::Uuid))?],
    /// )?;
    /// let claims = claims.may_be_unset();
    ///
    /// assert_eq!(claims.presence(), SettingPresence::MayBeUnset);
    /// let SettingContent::Json(fields) = claims.content() else { unreachable!() };
    /// assert_eq!(fields[0].path(), ["sub"]);
    /// assert_eq!(fields[0].value(), &SettingValue::Caller(DataType::Uuid));
    /// # Ok(())
    /// # }
    /// ```
    pub fn json(
        name: impl Into<String>,
        fields: impl IntoIterator<Item = JsonField>,
    ) -> Result<Self, DeploymentError> {
        let name = custom_name(name.into())?;
        let mut declared: Vec<JsonField> = Vec::new();
        for field in fields {
            if declared.iter().any(|seen| seen.path == field.path) {
                return Err(DeploymentError::DuplicateJsonField {
                    setting: name,
                    path: field.path,
                });
            }
            if field.value.has_empty_separator() {
                return Err(DeploymentError::EmptySeparator { setting: name });
            }
            declared.push(field);
        }
        if declared.is_empty() {
            return Err(DeploymentError::NoJsonFields { setting: name });
        }
        Ok(Self {
            name,
            content: SettingContent::Json(declared),
            presence: SettingPresence::Always,
        })
    }

    /// The same setting, which a request may leave unset.
    #[must_use]
    pub fn may_be_unset(self) -> Self {
        Self { presence: SettingPresence::MayBeUnset, ..self }
    }

    /// The setting's name, as declared.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the setting holds.
    #[must_use]
    pub fn content(&self) -> &SettingContent {
        &self.content
    }

    /// Whether a request always sets the setting.
    #[must_use]
    pub fn presence(&self) -> SettingPresence {
        self.presence
    }

    fn is_named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// Refuses a name that is not a custom setting name.
fn custom_name(name: String) -> Result<String, DeploymentError> {
    let custom = name.contains('.') && name.split('.').all(|part| !part.is_empty());
    if custom { Ok(name) } else { Err(DeploymentError::InvalidSettingName { name }) }
}

/// What a [`SessionSetting`] holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingContent {
    /// The setting's text is the value.
    Value(SettingValue),
    /// The setting's text is a JSON document, read field by field.
    Json(Vec<JsonField>),
}

/// One field of a JSON [`SessionSetting`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonField {
    path: Vec<String>,
    value: SettingValue,
}

impl JsonField {
    /// Declares the field at `path`, one key per nesting level, holding
    /// `value`.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::EmptyFieldPath`] when `path` is empty.
    pub fn new<K: Into<String>>(
        path: impl IntoIterator<Item = K>,
        value: SettingValue,
    ) -> Result<Self, DeploymentError> {
        let path: Vec<String> = path.into_iter().map(Into::into).collect();
        if path.is_empty() {
            return Err(DeploymentError::EmptyFieldPath);
        }
        Ok(Self { path, value })
    }

    /// The keys leading to the field.
    #[must_use]
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// What the field holds.
    #[must_use]
    pub fn value(&self) -> &SettingValue {
        &self.value
    }
}

/// What a setting or a JSON field holds.
///
/// A type is a promise that the text is that type's canonical rendering, so
/// casting it to the type renames nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// The caller's identity.
    Caller(DataType),
    /// One value the request carries.
    Scalar(DataType),
    /// A set of values the request carries.
    Set {
        /// The type of every element.
        element: DataType,
        /// How the elements are packed into the text.
        packing: SetPacking,
    },
}

impl SettingValue {
    fn has_empty_separator(&self) -> bool {
        matches!(self, Self::Set { packing: SetPacking::Delimited(separator), .. } if separator.is_empty())
    }
}

/// How a set's elements are packed into one setting's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetPacking {
    /// Joined by a non-empty delimiter, read with `string_to_array`.
    Delimited(String),
    /// A JSON array, read with `jsonb_array_elements_text`.
    JsonArray,
}

/// Whether a request always sets a [`SessionSetting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingPresence {
    /// Every request sets it.
    Always,
    /// A request may leave it unset. `current_setting(name, true)` then reads
    /// `NULL` on a connection that never set it and `''` once a transaction
    /// that set it has ended.
    MayBeUnset,
}
