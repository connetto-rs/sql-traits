//! What a deployment runs its requests as, which the schema cannot say.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use sql_values::postgres::PgTimeZone;
use sqlparser::{
    ast::{DataType, Expr},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::{
    errors::DeploymentError, structs::RoleSetting, traits::DatabaseLike,
    utils::search_path_text::split_search_path,
};

mod callable_functions;

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
    database: Option<String>,
    default_time_zone: Option<(String, PgTimeZone)>,
    /// The `search_path` each request sets itself, as written and as read.
    search_path: Option<(String, Vec<(String, bool)>)>,
    acting_role_settings: bool,
    /// The declared narrower and wider sets, as written.
    sets_within: Vec<(Expr, Expr)>,
    /// Each group of settings every request sets together, by their
    /// positions in `settings`.
    set_together: Vec<Vec<usize>>,
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
        Ok(Self {
            login_role: login_role.into(),
            acting_roles: roles,
            settings: declared,
            database: None,
            default_time_zone: None,
            search_path: None,
            acting_role_settings: false,
            sets_within: Vec::new(),
            set_together: Vec::new(),
        })
    }

    /// The same deployment, connecting to the database PostgreSQL stores as
    /// `database`, whose `ALTER ROLE ... IN DATABASE` settings then reach its
    /// requests.
    #[must_use]
    pub fn connected_to(self, database: impl Into<String>) -> Self {
        Self { database: Some(database.into()), ..self }
    }

    /// The same deployment on a server whose `postgresql.conf` sets
    /// `TimeZone` to `zone`, which a request runs in when no role setting
    /// reaching it sets one.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::InvalidTimeZone`] for a zone PostgreSQL
    /// refuses, as [`PgTimeZone::parse`] reads it.
    pub fn with_default_time_zone(self, zone: impl Into<String>) -> Result<Self, DeploymentError> {
        let zone = zone.into();
        match PgTimeZone::parse(&zone) {
            Ok(parsed) => Ok(Self { default_time_zone: Some((zone, parsed)), ..self }),
            Err(reason) => Err(DeploymentError::InvalidTimeZone { zone, reason }),
        }
    }

    /// The same deployment, each request setting its `search_path` to `path`
    /// itself, as a client running `SET search_path` on every connection
    /// does. `path` is written as `SHOW search_path` prints one, such as
    /// `"$user", public`, and replaces the path any role setting gives.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::InvalidSearchPath`] for a path PostgreSQL
    /// refuses as list syntax.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::DeploymentError> {
    /// let deployment = Deployment::new("login", ["reader"], [])?.with_search_path("api, public")?;
    /// assert_eq!(deployment.search_path(), Some("api, public"));
    /// assert!(Deployment::new("login", ["reader"], [])?.with_search_path("api,").is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_search_path(self, path: impl Into<String>) -> Result<Self, DeploymentError> {
        let path = path.into();
        match split_search_path(&path) {
            Some(entries) => Ok(Self { search_path: Some((path, entries)), ..self }),
            None => Err(DeploymentError::InvalidSearchPath { path }),
        }
    }

    /// The same deployment applying the acting role's own settings to each
    /// request, as PostgREST does after switching roles, where PostgreSQL's
    /// `SET ROLE` applies only the login role's.
    #[must_use]
    pub fn with_acting_role_settings(self) -> Self {
        Self { acting_role_settings: true, ..self }
    }

    /// Declares that every value `narrower` yields is also a value `wider`
    /// yields.
    ///
    /// Declarations compose, and expressions match as written after unfolding
    /// eligible `LANGUAGE sql` helpers. An eligible helper takes no inputs, is
    /// non-volatile, and has a single query body that projects one set without
    /// filtering or limiting it. The projected set must be a schema-qualified
    /// call with literal positional arguments to a non-volatile `SECURITY
    /// DEFINER` function, both functions must set no configuration except
    /// `search_path`, and their return types and set-returning flags must
    /// match.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::InvalidSetExpression`] when either expression
    /// cannot be parsed as PostgreSQL.
    ///
    /// # Example
    ///
    /// ```
    /// use sql_traits::prelude::*;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::DeploymentError> {
    /// let deployment = Deployment::new("login", ["reader"], [])?
    ///     .with_set_within("authz.capped('writer')", "authz.capped('reader')")?;
    /// assert!(deployment.with_set_within("authz.capped(", "authz.capped('reader')").is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_set_within(mut self, narrower: &str, wider: &str) -> Result<Self, DeploymentError> {
        self.sets_within.push((parse_expression(narrower)?, parse_expression(wider)?));
        Ok(self)
    }

    /// Returns whether every value `narrower` yields is one `wider` yields,
    /// where `unfolded` gives another spelling of the set an expression
    /// yields, such as the set a helper's body selects for a call to it, or
    /// `None`, and `inferred` tells whether one set lies within another as
    /// the rule reads the schema. Expressions and declarations compare once
    /// fully unfolded.
    pub(crate) fn is_set_within<E>(
        &self,
        narrower: &Expr,
        wider: &Expr,
        mut unfolded: impl FnMut(&Expr) -> Result<Option<Expr>, E>,
        mut inferred: impl FnMut(&Expr, &Expr) -> Result<bool, E>,
    ) -> Result<bool, E> {
        let wider = fully_unfolded(wider, &mut unfolded)?;
        let mut declared = Vec::with_capacity(self.sets_within.len());
        for (inner, outer) in &self.sets_within {
            declared.push((
                fully_unfolded(inner, &mut unfolded)?,
                fully_unfolded(outer, &mut unfolded)?,
            ));
        }
        let narrower = fully_unfolded(narrower, &mut unfolded)?;
        let mut reached: Vec<&Expr> = vec![&narrower];
        let mut next = 0;
        while let Some(&set) = reached.get(next) {
            if *set == *wider || inferred(set, &wider)? {
                return Ok(true);
            }
            for (inner, outer) in &declared {
                if (**inner == *set || inferred(set, inner)?) && !reached.contains(&&**outer) {
                    reached.push(outer);
                }
            }
            next += 1;
        }
        Ok(false)
    }

    /// Declares that every request sets each of `names` together. On every
    /// request either each of them reads text other than `''` or each reads
    /// `NULL` or `''` through `current_setting(name, true)`.
    ///
    /// Names compare without case, as PostgreSQL compares setting names, and
    /// groups sharing a setting compose.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::UndeclaredSetting`] for the first name the
    /// deployment does not declare, [`DeploymentError::AlwaysSetInGroup`]
    /// for the first setting declared [`SettingPresence::Always`], and
    /// [`DeploymentError::TooFewSettingsSetTogether`] when `names` name
    /// fewer than two distinct settings.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::DataType;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::DeploymentError> {
    /// let optional = |name: &str| {
    ///     SessionSetting::value(name, SettingValue::Scalar(DataType::Uuid))
    ///         .map(SessionSetting::may_be_unset)
    /// };
    /// let deployment = Deployment::new(
    ///     "app",
    ///     ["app"],
    ///     [optional("app.installation_namespace_id")?, optional("app.installation_id")?],
    /// )?
    /// .with_settings_set_together(["app.installation_namespace_id", "app.installation_id"])?;
    /// assert!(deployment.sets_together("APP.INSTALLATION_ID", "app.installation_namespace_id"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_settings_set_together<N: AsRef<str>>(
        mut self,
        names: impl IntoIterator<Item = N>,
    ) -> Result<Self, DeploymentError> {
        let mut written = Vec::new();
        let mut group = Vec::new();
        for name in names {
            let name = name.as_ref();
            let Some(position) = self.position(name) else {
                return Err(DeploymentError::UndeclaredSetting { name: name.into() });
            };
            let setting = &self.settings[position];
            if setting.presence == SettingPresence::Always {
                return Err(DeploymentError::AlwaysSetInGroup { name: setting.name.clone() });
            }
            if !group.contains(&position) {
                group.push(position);
            }
            written.push(name.to_string());
        }
        if group.len() < 2 {
            return Err(DeploymentError::TooFewSettingsSetTogether { names: written });
        }
        self.set_together.push(group);
        Ok(self)
    }

    /// Returns whether every request sets `first` and `second` together, as
    /// the groups [`Self::with_settings_set_together`] declares compose. A
    /// declared setting is set together with itself, and a name the
    /// deployment does not declare with none.
    #[must_use]
    pub fn sets_together(&self, first: &str, second: &str) -> bool {
        let (Some(first), Some(second)) = (self.position(first), self.position(second)) else {
            return false;
        };
        self.set_together_with(first).contains(&second)
    }

    /// The database the deployment connects to, when declared.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// The server's default `TimeZone`, as written, when declared.
    #[must_use]
    pub fn default_time_zone(&self) -> Option<&str> {
        self.default_time_zone.as_ref().map(|(written, _)| written.as_str())
    }

    /// The `search_path` each request sets itself, as written, when declared.
    #[must_use]
    pub fn search_path(&self) -> Option<&str> {
        self.search_path.as_ref().map(|(written, _)| written.as_str())
    }

    /// Whether each request applies the acting role's own settings.
    #[must_use]
    pub fn applies_acting_role_settings(&self) -> bool {
        self.acting_role_settings
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

    /// The position in `settings` of the setting named `name`, compared
    /// without case.
    fn position(&self, name: &str) -> Option<usize> {
        self.settings.iter().position(|setting| setting.is_named(name))
    }

    /// The positions of the settings every request sets together with the
    /// setting at `position`, that setting first, as the declared groups
    /// compose.
    fn set_together_with(&self, position: usize) -> Vec<usize> {
        let mut reached = vec![position];
        let mut next = 0;
        while let Some(&setting) = reached.get(next) {
            for group in self.set_together.iter().filter(|group| group.contains(&setting)) {
                for &member in group {
                    if !reached.contains(&member) {
                        reached.push(member);
                    }
                }
            }
            next += 1;
        }
        reached
    }

    /// The kinds of request the deployment admits, told apart by which
    /// settings each sets. There is one for each way of setting or leaving
    /// unset the groups set together that hold a setting `read` accepts. A
    /// setting declared [`SettingPresence::Always`] is set on each, and any
    /// other setting outside those groups may be set or not. A setting
    /// `changed` accepts, one the body read may set itself, holds what no
    /// request tells on each.
    pub(crate) fn request_presences(
        &self,
        read: impl Fn(&SessionSetting) -> bool,
        changed: impl Fn(&SessionSetting) -> bool,
    ) -> Vec<RequestPresence<'_>> {
        let mut grouped = vec![false; self.settings.len()];
        let mut split = Vec::new();
        for (position, setting) in self.settings.iter().enumerate() {
            if grouped[position] || setting.presence == SettingPresence::Always {
                continue;
            }
            let group = self.set_together_with(position);
            for &member in &group {
                grouped[member] = true;
            }
            let told = |member: &usize| {
                let setting = &self.settings[*member];
                read(setting) && !changed(setting)
            };
            if group.len() > 1 && group.iter().any(told) {
                split.push(group);
            }
        }
        let unsplit = self
            .settings
            .iter()
            .map(|setting| (!changed(setting)).then_some(setting.held()))
            .collect();
        let mut presences: Vec<Vec<Option<Held>>> = vec![unsplit];
        for group in split {
            presences = presences
                .into_iter()
                .flat_map(|held| {
                    [Held::Set, Held::Unset].map(|chosen| {
                        let mut held = held.clone();
                        for &member in &group {
                            if held[member].is_some() {
                                held[member] = Some(chosen);
                            }
                        }
                        held
                    })
                })
                .collect();
        }
        presences
            .into_iter()
            .map(|held| RequestPresence { settings: &self.settings, held })
            .collect()
    }

    /// The description's roles as the roles `database` declares.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::UndeclaredRole`] naming the first role,
    /// login role first, that `database` does not declare.
    pub fn resolve<'db, DB: DatabaseLike>(
        &'db self,
        database: &'db DB,
    ) -> Result<ResolvedDeployment<'db, DB>, DeploymentError> {
        let declared = |name: &str| {
            database.role(name).ok_or_else(|| DeploymentError::UndeclaredRole { role: name.into() })
        };
        let login_role = declared(&self.login_role)?;
        let acting_roles =
            self.acting_roles.iter().map(|role| declared(role)).collect::<Result<Vec<_>, _>>()?;
        Ok(ResolvedDeployment { deployment: self, database, login_role, acting_roles })
    }
}

/// Which settings one kind of request a [`Deployment`] admits sets, from
/// [`Deployment::request_presences`].
#[derive(Debug, Clone)]
pub(crate) struct RequestPresence<'d> {
    settings: &'d [SessionSetting],
    /// Whether the request sets each of `settings`, by position, or `None`
    /// for one the body read may set itself.
    held: Vec<Option<Held>>,
}

impl<'d> RequestPresence<'d> {
    /// The declared setting named `name`, compared without case, with
    /// whether the request sets it, or `None` for a setting the deployment
    /// does not declare or the body read may set itself.
    pub(crate) fn setting(&self, name: &str) -> Option<(&'d SessionSetting, Held)> {
        let position = self.settings.iter().position(|setting| setting.is_named(name))?;
        Some((&self.settings[position], self.held[position]?))
    }
}

/// Whether a kind of request sets a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// It is declared [`SettingPresence::Always`] and reads any text, `''`
    /// included.
    Always,
    /// It reads text other than `''`, as every setting of a group set
    /// together does on a request setting the group.
    Set,
    /// It reads `NULL` or `''` through `current_setting(name, true)`.
    Unset,
    /// Either.
    Either,
}

/// Parses one PostgreSQL expression.
fn parse_expression(expression: &str) -> Result<Expr, DeploymentError> {
    Parser::new(&PostgreSqlDialect {})
        .try_with_sql(expression)
        .and_then(|mut parser| parser.parse_expr())
        .map_err(|error| {
            DeploymentError::InvalidSetExpression {
                expression: expression.into(),
                message: error.to_string(),
            }
        })
}

/// `set` unfolded by `unfolded` until it gives no other spelling or one it
/// gave before, as a helper calling itself does.
fn fully_unfolded<'expression, E>(
    set: &'expression Expr,
    unfolded: &mut impl FnMut(&Expr) -> Result<Option<Expr>, E>,
) -> Result<Cow<'expression, Expr>, E> {
    let mut spellings: Vec<Expr> = Vec::new();
    while let Some(next) = unfolded(spellings.last().unwrap_or(set))? {
        if next == *set || spellings.contains(&next) {
            break;
        }
        spellings.push(next);
    }
    Ok(spellings.pop().map_or(Cow::Borrowed(set), Cow::Owned))
}

/// A [`Deployment`]'s roles as declared in one database.
#[derive(Debug)]
pub struct ResolvedDeployment<'db, DB: DatabaseLike> {
    deployment: &'db Deployment,
    database: &'db DB,
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

    /// The `TimeZone` a request acting as `acting_role` runs in, and the
    /// step that supplies it.
    ///
    /// PostgreSQL takes the first entry setting `TimeZone` of the login role
    /// in the database, the login role, every role in the database, every
    /// role, and then `postgresql.conf`. `SET ROLE` applies none of the
    /// acting role's own, unless [`Deployment::with_acting_role_settings`]
    /// puts the acting role in the database and the acting role first.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NotAnActingRole`] for a role the
    /// deployment does not act as, [`DeploymentError::NoDatabase`] when an
    /// entry set in one database is reached and the deployment names no
    /// database, and [`DeploymentError::NoDefaultTimeZone`] when no entry
    /// sets the zone and the deployment declares no default.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let database = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authenticator LOGIN NOINHERIT;
    ///      CREATE ROLE web_user NOLOGIN;
    ///      ALTER ROLE ALL IN DATABASE app SET timezone = 'Europe/Rome';
    ///      ALTER ROLE web_user SET timezone = 'America/New_York';",
    /// )
    /// .expect("the schema parses");
    /// let plain = Deployment::new("authenticator", ["web_user"], [])
    ///     .and_then(|deployment| deployment.connected_to("app").with_default_time_zone("Etc/UTC"))
    ///     .expect("a valid description");
    /// let postgrest = plain.clone().with_acting_role_settings();
    ///
    /// let zone = plain.resolve(&database).and_then(|resolved| {
    ///     resolved.time_zone("web_user").map(|zone| (zone.name(), zone.source()))
    /// });
    /// assert_eq!(zone, Ok(("Europe/Rome", TimeZoneSource::Database)));
    ///
    /// // The zone PostgreSQL reads from the setting, which has transitions.
    /// let resolved = plain.resolve(&database).expect("every role is declared");
    /// let zone = resolved.time_zone("web_user").expect("a zone is set").zone();
    /// assert_eq!((zone.name(), zone.fixed_offset()), ("Europe/Rome", None));
    ///
    /// let zone = postgrest.resolve(&database).and_then(|resolved| {
    ///     resolved.time_zone("web_user").map(|zone| (zone.name(), zone.source()))
    /// });
    /// assert_eq!(zone, Ok(("America/New_York", TimeZoneSource::ActingRole)));
    /// ```
    pub fn time_zone(&self, acting_role: &str) -> Result<RequestTimeZone<'db>, DeploymentError> {
        self.acting_role(acting_role)?;
        self.time_zone_from(self.deployment.acting_role_settings.then_some(acting_role))
    }

    /// The `search_path` a request acting as `acting_role` resolves names
    /// on, each schema as stored and whether it was quoted, `"$user"`
    /// standing for the acting role as PostgreSQL reads it.
    ///
    /// A path the deployment declares with [`Deployment::with_search_path`]
    /// wins, as the request sets it itself. Otherwise PostgreSQL takes the
    /// first `search_path` entry [`Self::time_zone`] would take for
    /// `TimeZone`, and then the database's own path,
    /// [`DatabaseLike::search_path`].
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NotAnActingRole`] for a role the
    /// deployment does not act as, and [`DeploymentError::NoDatabase`] when
    /// an entry set in one database is reached and the deployment names no
    /// database.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{errors::DeploymentError, prelude::*};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// # fn main() -> Result<(), DeploymentError> {
    /// let database = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authenticator LOGIN NOINHERIT;
    ///      CREATE ROLE web_user NOLOGIN;
    ///      CREATE SCHEMA api;
    ///      ALTER ROLE authenticator SET search_path = api;",
    /// )
    /// .expect("the schema parses");
    /// let deployment = Deployment::new("authenticator", ["web_user"], [])?;
    /// let path = deployment.resolve(&database)?.search_path("web_user")?;
    /// assert_eq!(path, [("api", false)]);
    ///
    /// let declared = deployment.with_search_path("\"$user\", public")?;
    /// let path = declared.resolve(&database)?.search_path("web_user")?;
    /// assert_eq!(path, [("$user", true), ("public", false)]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn search_path(&self, acting_role: &str) -> Result<Vec<(&'db str, bool)>, DeploymentError> {
        self.acting_role(acting_role)?;
        let deployment = self.deployment;
        let entries = |path: &'db [(String, bool)]| {
            path.iter().map(|(schema, quoted)| (schema.as_str(), *quoted)).collect()
        };
        if let Some((_, declared)) = &deployment.search_path {
            return Ok(entries(declared));
        }
        let acting = deployment.acting_role_settings.then_some(acting_role);
        let reached = self.reached_setting(acting, "search_path")?;
        if let Some(path) = reached.and_then(|(setting, _)| setting.search_path()) {
            return Ok(entries(path));
        }
        Ok(self.database.search_path().collect())
    }

    /// The declared role a request acting as `name` runs as.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NotAnActingRole`] for a role the
    /// deployment does not act as.
    fn acting_role(&self, name: &str) -> Result<&'db DB::Role, DeploymentError> {
        self.deployment
            .acting_roles
            .iter()
            .zip(&self.acting_roles)
            .find_map(|(acting, role)| (acting == name).then_some(*role))
            .ok_or_else(|| DeploymentError::NotAnActingRole { role: name.into() })
    }

    /// The `TimeZone` a request runs in before any `SET ROLE`, and the step
    /// that supplies it.
    ///
    /// PostgreSQL takes the steps [`Self::time_zone`] takes after the acting
    /// role's. A request that has not switched roles has no acting role, so
    /// no acting role's own settings are read, whatever
    /// [`Deployment::with_acting_role_settings`] says.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NoDatabase`] when an entry set in one
    /// database is reached and the deployment names no database, and
    /// [`DeploymentError::NoDefaultTimeZone`] when no entry sets the zone
    /// and the deployment declares no default.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let database = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authenticator LOGIN NOINHERIT;
    ///      CREATE ROLE web_user NOLOGIN;
    ///      ALTER ROLE ALL IN DATABASE app SET timezone = 'Europe/Rome';
    ///      ALTER ROLE web_user SET timezone = 'America/New_York';",
    /// )
    /// .expect("the schema parses");
    /// let postgrest = Deployment::new("authenticator", ["web_user"], [])
    ///     .and_then(|deployment| deployment.connected_to("app").with_default_time_zone("Etc/UTC"))
    ///     .expect("a valid description")
    ///     .with_acting_role_settings();
    /// let resolved = postgrest.resolve(&database).expect("every role is declared");
    ///
    /// let zone = resolved.login_time_zone().map(|zone| (zone.name(), zone.source()));
    /// assert_eq!(zone, Ok(("Europe/Rome", TimeZoneSource::Database)));
    ///
    /// let zone = resolved.time_zone("web_user").map(|zone| (zone.name(), zone.source()));
    /// assert_eq!(zone, Ok(("America/New_York", TimeZoneSource::ActingRole)));
    /// ```
    pub fn login_time_zone(&self) -> Result<RequestTimeZone<'db>, DeploymentError> {
        self.time_zone_from(None)
    }

    /// The `TimeZone` of a request reading `acting_role`'s own settings, when
    /// given, ahead of the login role's chain.
    fn time_zone_from(
        &self,
        acting_role: Option<&str>,
    ) -> Result<RequestTimeZone<'db>, DeploymentError> {
        let reached =
            self.reached_setting(acting_role, "TimeZone")?.and_then(|(setting, source)| {
                Some(RequestTimeZone { name: setting.value(), zone: setting.time_zone()?, source })
            });
        if let Some(zone) = reached {
            return Ok(zone);
        }
        self.deployment
            .default_time_zone
            .as_ref()
            .map(|(name, zone)| {
                RequestTimeZone { name, zone, source: TimeZoneSource::ServerDefault }
            })
            .ok_or(DeploymentError::NoDefaultTimeZone)
    }

    /// The first entry setting `name` that reaches a request reading
    /// `acting_role`'s own settings, when given, ahead of the login role's
    /// chain, and the step it is, or [`None`] when no role setting sets it.
    ///
    /// # Errors
    ///
    /// Returns [`DeploymentError::NoDatabase`] when an entry set in one
    /// database is reached and the deployment names no database.
    fn reached_setting(
        &self,
        acting_role: Option<&str>,
        name: &str,
    ) -> Result<Option<(&'db RoleSetting, TimeZoneSource)>, DeploymentError> {
        let deployment = self.deployment;
        let database = self.database;
        let acting = acting_role.map(|role| {
            (TimeZoneSource::ActingRoleInDatabase, TimeZoneSource::ActingRole, Some(role))
        });
        let login = [
            (
                TimeZoneSource::LoginRoleInDatabase,
                TimeZoneSource::LoginRole,
                Some(deployment.login_role.as_str()),
            ),
            (TimeZoneSource::Database, TimeZoneSource::AllRoles, None),
        ];
        for &(in_database, everywhere, role) in acting.iter().chain(&login) {
            let set = |set_in: Option<&str>, source| {
                database
                    .role_settings()
                    .find(|setting| setting.is_named(name) && setting.is_keyed(role, set_in))
                    .map(|setting| (setting, source))
            };
            if let Some(connected) = deployment.database.as_deref() {
                if let Some(reached) = set(Some(connected), in_database) {
                    return Ok(Some(reached));
                }
            } else if database.role_settings().any(|setting| {
                setting.role() == role && setting.database().is_some() && setting.is_named(name)
            }) {
                return Err(DeploymentError::NoDatabase {
                    role: role.map(Into::into),
                    setting: name.into(),
                });
            }
            if let Some(reached) = set(None, everywhere) {
                return Ok(Some(reached));
            }
        }
        Ok(None)
    }
}

/// The `TimeZone` a request runs in, from [`ResolvedDeployment::time_zone`]
/// or [`ResolvedDeployment::login_time_zone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTimeZone<'db> {
    name: &'db str,
    zone: &'db PgTimeZone,
    source: TimeZoneSource,
}

impl<'db> RequestTimeZone<'db> {
    /// The zone as its setting writes it, which [`Self::zone`] reads.
    #[must_use]
    pub fn name(&self) -> &'db str {
        self.name
    }

    /// The zone PostgreSQL runs the request in, as [`PgTimeZone::parse`]
    /// reads [`Self::name`].
    #[must_use]
    pub fn zone(&self) -> &'db PgTimeZone {
        self.zone
    }

    /// The step that supplies the zone.
    #[must_use]
    pub fn source(&self) -> TimeZoneSource {
        self.source
    }
}

/// Which step of PostgreSQL's precedence supplies a request's `TimeZone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeZoneSource {
    /// `ALTER ROLE acting IN DATABASE d SET`, applied per request.
    ActingRoleInDatabase,
    /// `ALTER ROLE acting SET`, applied per request.
    ActingRole,
    /// `ALTER ROLE login IN DATABASE d SET`.
    LoginRoleInDatabase,
    /// `ALTER ROLE login SET`.
    LoginRole,
    /// `ALTER ROLE ALL IN DATABASE d SET`, which `ALTER DATABASE d SET`
    /// writes too.
    Database,
    /// `ALTER ROLE ALL SET`.
    AllRoles,
    /// The deployment's declared server default.
    ServerDefault,
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
    /// custom setting name, two or more non-empty parts joined by dots,
    /// [`DeploymentError::EmptySeparator`] for a set split on the empty
    /// string, which `string_to_array` reads as one field, and
    /// [`DeploymentError::EmptyPrincipalKind`] for a principal of no type.
    pub fn value(name: impl Into<String>, value: SettingValue) -> Result<Self, DeploymentError> {
        let name = value.checked(custom_name(name.into())?)?;
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
    /// fields share a path, and [`DeploymentError::EmptySeparator`] and
    /// [`DeploymentError::EmptyPrincipalKind`] for a field whose value
    /// [`SessionSetting::value`] refuses.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::DataType;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::DeploymentError> {
    /// // PostgREST's token, whose `sub` Supabase's `auth.uid()` reads as the caller.
    /// let sub = JsonField::new(["sub"], SettingValue::Caller(DataType::Uuid))?;
    /// let claims = SessionSetting::json("request.jwt.claims", [sub])?.may_be_unset();
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
        let mut name = custom_name(name.into())?;
        let mut declared: Vec<JsonField> = Vec::new();
        for field in fields {
            if declared.iter().any(|seen| seen.path == field.path) {
                return Err(DeploymentError::DuplicateJsonField {
                    setting: name,
                    path: field.path,
                });
            }
            name = field.value.checked(name)?;
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

    /// Whether the setting is named `name`, compared without case as
    /// PostgreSQL compares setting names.
    pub(crate) fn is_named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// Whether a request sets the setting, read over every request alike,
    /// before a group of settings set together is split into the requests
    /// setting it and those leaving it unset.
    pub(crate) fn held(&self) -> Held {
        match self.presence {
            SettingPresence::Always => Held::Always,
            SettingPresence::MayBeUnset => Held::Either,
        }
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
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case", deny_unknown_fields)
)]
pub enum SettingValue {
    /// The caller's identity.
    Caller(#[cfg_attr(feature = "serde", serde(with = "spelled_type"))] DataType),
    /// The identity of a principal of type `kind` the request acts as or
    /// for beside the caller, such as the app installation acting for the
    /// caller.
    Principal {
        /// The type of the principal, such as `installation`.
        kind: String,
        /// The type of the identity.
        #[cfg_attr(feature = "serde", serde(with = "spelled_type"))]
        identity: DataType,
    },
    /// One value the request carries.
    Scalar(#[cfg_attr(feature = "serde", serde(with = "spelled_type"))] DataType),
    /// A set of values the request carries.
    Set {
        /// The type of every element.
        #[cfg_attr(feature = "serde", serde(with = "spelled_type"))]
        element: DataType,
        /// How the elements are packed into the text.
        packing: SetPacking,
    },
}

impl SettingValue {
    /// The type of the one value the setting holds, `None` for a set.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::DataType;
    ///
    /// let installation =
    ///     SettingValue::Principal { kind: "installation".into(), identity: DataType::Uuid };
    /// assert_eq!(installation.single_type(), Some(&DataType::Uuid));
    /// let links = SettingValue::Set { element: DataType::Uuid, packing: SetPacking::JsonArray };
    /// assert_eq!(links.single_type(), None);
    /// ```
    #[must_use]
    pub fn single_type(&self) -> Option<&DataType> {
        match self {
            Self::Caller(declared)
            | Self::Principal { identity: declared, .. }
            | Self::Scalar(declared) => Some(declared),
            Self::Set { .. } => None,
        }
    }

    /// `setting`, the name of the setting declaring this value, unless no
    /// request can carry the value.
    fn checked(&self, setting: String) -> Result<String, DeploymentError> {
        match self {
            Self::Set { packing: SetPacking::Delimited(separator), .. } if separator.is_empty() => {
                Err(DeploymentError::EmptySeparator { setting })
            }
            Self::Principal { kind, .. } if kind.is_empty() => {
                Err(DeploymentError::EmptyPrincipalKind { setting })
            }
            _ => Ok(setting),
        }
    }
}

/// How a set's elements are packed into one setting's text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum SetPacking {
    /// Joined by a non-empty delimiter, read with `string_to_array`.
    Delimited(String),
    /// A JSON array, read with `jsonb_array_elements_text`.
    JsonArray,
}

/// A [`DataType`] as the SQL spelling PostgreSQL reads.
#[cfg(feature = "serde")]
mod spelled_type {
    use alloc::string::String;

    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    use sqlparser::ast::DataType;

    use crate::utils::postgres_types::parse_data_type;

    /// Writes `data_type` as its SQL spelling.
    pub(super) fn serialize<S: Serializer>(
        data_type: &DataType,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(data_type)
    }

    /// Reads a type from its SQL spelling.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DataType, D::Error> {
        let spelling = String::deserialize(deserializer)?;
        parse_data_type(&spelling)
            .ok_or_else(|| D::Error::custom(format_args!("{spelling:?} is not a PostgreSQL type")))
    }
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

#[cfg(test)]
mod tests {
    use sqlparser::{ast::Expr, dialect::PostgreSqlDialect, parser::Parser};

    use super::Deployment;
    use crate::errors::DeploymentError;

    /// Parses a set expression for a containment assertion.
    fn expression(written: &str) -> Expr {
        #[expect(clippy::expect_used, reason = "The test supplies a valid expression.")]
        Parser::new(&PostgreSqlDialect {})
            .try_with_sql(written)
            .and_then(|mut parser| parser.parse_expr())
            .expect("valid set expression")
    }

    /// Declared containment composes in its declared direction.
    #[test]
    fn declarations_compose() -> Result<(), DeploymentError> {
        let deployment = Deployment::new("login", ["reader"], [])?
            .with_set_within("authz.capped('writer')", "authz.capped('reader')")?
            .with_set_within("authz.capped('reader')", "authz.capped('guest')")?;
        let writer = expression("authz.capped('writer')");
        let guest = expression("authz.capped('guest')");
        let within = |narrower: &Expr, wider: &Expr| {
            deployment.is_set_within(
                narrower,
                wider,
                |_| Ok::<_, DeploymentError>(None),
                |_, _| Ok(false),
            )
        };
        assert_eq!(within(&writer, &guest), Ok(true));
        assert_eq!(within(&guest, &writer), Ok(false));
        Ok(())
    }

    /// Either malformed expression retains its spelling and parser message.
    #[test]
    fn malformed_declarations_are_refused() -> Result<(), DeploymentError> {
        for (narrower, wider) in [("authz.capped(", "1"), ("1", "authz.capped(")] {
            let result = Deployment::new("login", ["reader"], [])?.with_set_within(narrower, wider);
            assert!(matches!(
                result,
                Err(DeploymentError::InvalidSetExpression { expression, message })
                    if expression == "authz.capped(" && !message.is_empty()
            ));
        }
        Ok(())
    }
}
