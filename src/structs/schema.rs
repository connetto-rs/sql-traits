//! Schema struct for storing parsed schema information.

use alloc::string::String;

/// A database schema parsed from a CREATE SCHEMA statement.
///
/// This struct stores the schema name, optional authorization owner, and the
/// `USAGE` entries PostgreSQL gives a schema before any `GRANT`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Schema {
    /// The name of the schema.
    name: String,
    /// Whether the schema identifier was quoted in SQL.
    quoted: bool,
    /// The authorization owner of the schema, if specified.
    authorization: Option<String>,
    /// Whether `PUBLIC` holds the `USAGE` the schema was created with.
    public_usage: bool,
    /// Whether the owner's own entry holds `USAGE`.
    owner_usage: bool,
}

impl Schema {
    /// Creates a new `Schema` with the given name.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::structs::Schema;
    ///
    /// let s = Schema::new("app".to_string());
    /// assert_eq!(s.name(), "app");
    /// assert!(!s.public_holds_usage(), "a created schema gives PUBLIC nothing");
    /// assert!(s.owner_holds_usage());
    /// ```
    #[must_use]
    pub fn new(name: String) -> Self {
        Self::with_quoted(name, false)
    }

    /// Creates a new `Schema` with the given name and authorization owner.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::structs::Schema;
    ///
    /// let s = Schema::with_authorization("public".to_string(), "admin".to_string());
    /// assert_eq!(s.name(), "public");
    /// assert_eq!(s.authorization(), Some("admin"));
    /// assert!(!s.is_quoted());
    /// ```
    #[must_use]
    pub fn with_authorization(name: String, authorization: String) -> Self {
        Self::with_authorization_and_quoted(name, authorization, false)
    }

    /// Creates a new `Schema` with quoted-name metadata.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::structs::Schema;
    ///
    /// let s = Schema::with_quoted("MySchema".to_string(), true);
    /// assert_eq!(s.name(), "MySchema");
    /// assert!(s.is_quoted());
    /// assert_eq!(s.authorization(), None);
    /// ```
    #[must_use]
    pub fn with_quoted(name: String, quoted: bool) -> Self {
        Self { name, quoted, authorization: None, public_usage: false, owner_usage: true }
    }

    /// Creates a new `Schema` with authorization and quoted-name metadata.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::structs::Schema;
    ///
    /// let s =
    ///     Schema::with_authorization_and_quoted("MySchema".to_string(), "admin".to_string(), true);
    /// assert_eq!(s.name(), "MySchema");
    /// assert_eq!(s.authorization(), Some("admin"));
    /// assert!(s.is_quoted());
    /// ```
    #[must_use]
    pub fn with_authorization_and_quoted(
        name: String,
        authorization: String,
        quoted: bool,
    ) -> Self {
        Self { authorization: Some(authorization), ..Self::with_quoted(name, quoted) }
    }

    /// The `public` schema a new database holds, owned by `owner`, on which
    /// `PUBLIC` holds `USAGE`.
    pub(crate) fn initial_public(owner: Option<String>) -> Self {
        Self {
            authorization: owner,
            public_usage: true,
            ..Self::with_quoted("public".into(), false)
        }
    }

    /// This schema under another name, keeping its owner and entries.
    pub(crate) fn renamed(&self, name: String, quoted: bool) -> Self {
        Self { name, quoted, ..self.clone() }
    }

    /// This schema handed to `owner`, who takes over the owner's entry.
    pub(crate) fn owned_by(&self, owner: String) -> Self {
        Self { authorization: Some(owner), ..self.clone() }
    }

    /// Takes `PUBLIC`'s `USAGE` from the schema's initial entry.
    pub(crate) fn revoke_public_usage(&mut self) {
        self.public_usage = false;
    }

    /// Takes `USAGE` from the owner's own entry.
    pub(crate) fn revoke_owner_usage(&mut self) {
        self.owner_usage = false;
    }

    /// Returns the name of the schema.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the authorization owner of the schema, if specified.
    #[must_use]
    pub fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }

    /// Returns whether this schema name was quoted in SQL.
    #[must_use]
    pub fn is_quoted(&self) -> bool {
        self.quoted
    }

    /// Returns whether `PUBLIC` holds the `USAGE` the schema was created
    /// with, as a new database's `public` schema gives it until a `REVOKE`.
    #[must_use]
    pub fn public_holds_usage(&self) -> bool {
        self.public_usage
    }

    /// Returns whether the owner's own entry still holds `USAGE`, which a
    /// `REVOKE` naming the owner takes.
    #[must_use]
    pub fn owner_holds_usage(&self) -> bool {
        self.owner_usage
    }
}
