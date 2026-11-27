//! Submodule providing a trait for describing SQL sequences.

use alloc::{borrow::Cow, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::Action;

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ActingRole, SequenceParameters, SequencePrivileges, TargetName},
    traits::{DatabaseLike, Metadata, Relation, RoleLike, TableGrantLike, role::privilege_holders},
    utils::identifier_resolution::{is_temporary_schema, stored_identifier},
};

/// The column a sequence is owned by, saying which kind of relation holds it.
///
/// PostgreSQL lets a column of a table or of a plain view own a sequence, and
/// drops the sequence when that column or its relation goes.
#[derive(Clone, Debug)]
pub enum SequenceOwner<'a, DB: DatabaseLike> {
    /// A column of a table.
    Column(&'a DB::Column),
    /// An output column of a plain view, by its position in the view's
    /// output, counted from zero.
    ///
    /// A view keeps a column at its position through every rename, and a
    /// replacement may only add columns after the last, so the position is
    /// what stays put.
    ViewColumn {
        /// The view.
        view: &'a DB::View,
        /// Position of the column in the view's output.
        ordinal: usize,
    },
}

impl<DB: DatabaseLike> Copy for SequenceOwner<'_, DB> {}

/// Why a sequence exists.
///
/// PostgreSQL creates a sequence of its own for every `serial` column and
/// every identity column, besides the ones `CREATE SEQUENCE` declares, and
/// the column it serves owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SequenceOrigin {
    /// Declared by `CREATE SEQUENCE`.
    Declared,
    /// Created for a `serial`, `bigserial` or `smallserial` column, whose
    /// default draws from it.
    Serial,
    /// Created for an identity column, which generates its values from it
    /// and cannot be left without it.
    Identity,
}

/// A trait for types that can be treated as SQL sequences, the number
/// generators `CREATE SEQUENCE` defines and PostgreSQL creates for `serial`
/// and identity columns.
///
/// A sequence is a relation. It takes its name from the pool tables, views
/// and indexes of its schema share, and a client reads or advances it through
/// the privileges granted on it.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createsequence.html).
pub trait SequenceLike: Debug + Clone + Hash + Ord + Eq + Metadata + Send + Sync {
    /// The database type the sequence belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the sequence, exactly as declared.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SEQUENCE ticket_seq;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_name(), "ticket_seq");
    /// # Ok(())
    /// # }
    /// ```
    fn sequence_name(&self) -> &str;

    /// Returns whether the sequence identifier was quoted in SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn sequence_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this sequence, an unquoted
    /// identifier folded to lowercase and a quoted one kept in its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SEQUENCE Ticket_Seq;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_name(), "Ticket_Seq");
    /// assert_eq!(sequence.stored_sequence_name(), "ticket_seq");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_sequence_name(&self) -> Cow<'_, str> {
        stored_identifier(self.sequence_name(), self.sequence_name_is_quoted())
    }

    /// Returns the schema the sequence is stored in, or `None` for one stored
    /// in the default schema without a qualifier.
    ///
    /// Ingestion records where the sequence landed, which is the schema the
    /// search path selected for a bare name and `pg_temp` for a temporary
    /// sequence.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      SET search_path = app, public;
    ///      CREATE SEQUENCE ticket_seq;",
    /// )?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert_eq!(sequence.sequence_schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    fn sequence_schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier of this sequence was quoted in
    /// SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn sequence_schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this sequence's schema.
    #[inline]
    fn stored_sequence_schema(&self) -> Option<Cow<'_, str>> {
        self.sequence_schema()
            .map(|schema| stored_identifier(schema, self.sequence_schema_is_quoted()))
    }

    /// Returns whether this sequence lives in the session's temporary schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TEMP SEQUENCE scratch;")?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert!(sequence.is_temporary());
    /// assert_eq!(sequence.sequence_schema(), Some("pg_temp"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_temporary(&self) -> bool {
        self.sequence_schema()
            .is_some_and(|schema| is_temporary_schema(schema, self.sequence_schema_is_quoted()))
    }

    /// Returns the sequence name exactly as declared, including its optional
    /// schema.
    #[inline]
    fn target_name(&self) -> TargetName<'_> {
        let name = TargetName::new(self.sequence_name(), self.sequence_name_is_quoted());
        match self.sequence_schema() {
            Some(schema) => name.with_schema(schema, self.sequence_schema_is_quoted()),
            None => name,
        }
    }

    /// Returns the values the sequence counts with, as `pg_sequence` records
    /// them.
    ///
    /// Every option a declaration leaves out takes PostgreSQL's default for
    /// the type and direction, so two declarations that differ only in
    /// spelling a default out answer alike. A sequence created for a column
    /// counts in the column's type, which an identity sequence follows through
    /// every type change of its column.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SEQUENCE implicit;
    ///      CREATE SEQUENCE spelled START WITH 1 INCREMENT BY 1;
    ///      CREATE SEQUENCE countdown AS integer INCREMENT -1;",
    /// )?;
    /// let parameters = |name| {
    ///     db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(|sequence| sequence.map(SequenceLike::parameters))
    /// };
    /// assert_eq!(parameters("implicit")?, parameters("spelled")?);
    /// let countdown = parameters("countdown")?.unwrap();
    /// assert_eq!(countdown.data_type(), SequenceType::Integer);
    /// assert_eq!((countdown.start(), countdown.max_value()), (-1, -1));
    /// assert_eq!(countdown.min_value(), i64::from(i32::MIN));
    /// # Ok(())
    /// # }
    /// ```
    fn parameters(&self) -> SequenceParameters;

    /// Returns the column owning this sequence, if one does.
    ///
    /// A declared sequence is owned by the column its `OWNED BY` names, and
    /// one created for a column by that column. The column keeps owning it
    /// through every rename of the column and of its relation, and dropping
    /// either drops the sequence.
    ///
    /// `Ok(None)` means the sequence is unowned, including `OWNED BY NONE`.
    ///
    /// # Errors
    ///
    /// Returns a [`LookupError`] when the recorded owning column or relation is
    /// absent from `database`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE tickets (id BIGINT);
    ///      CREATE SEQUENCE ticket_seq OWNED BY tickets.id;
    ///      ALTER TABLE tickets RENAME COLUMN id TO ticket_id;",
    /// )?;
    /// let sequence = db.sequences().next().expect("the sequence is recorded");
    /// assert!(matches!(
    ///     sequence.owner(&db)?,
    ///     Some(SequenceOwner::Column(column)) if column.column_name() == "ticket_id"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<SequenceOwner<'db, Self::DB>>, LookupError>
    where
        Self: 'db;

    /// Returns why the sequence exists.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE tickets (id BIGINT GENERATED ALWAYS AS IDENTITY, n serial);",
    /// )?;
    /// let origin = |name| {
    ///     db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(|sequence| sequence.map(SequenceLike::origin))
    /// };
    /// assert_eq!(origin("tickets_id_seq")?, Some(SequenceOrigin::Identity));
    /// assert_eq!(origin("tickets_n_seq")?, Some(SequenceOrigin::Serial));
    /// # Ok(())
    /// # }
    /// ```
    fn origin(&self) -> SequenceOrigin;

    /// Returns the role owning the sequence, whose own entry holds the
    /// [`authorization_privileges`](Self::authorization_privileges), or
    /// `None` when the input names none.
    ///
    /// It is the role that created the sequence, or the one that created the
    /// table for a sequence created for a column. Handing that table to
    /// another role hands its sequences over too. The owning role is apart
    /// from the column [`owner`](Self::owner) names, which decides only when
    /// the sequence is dropped.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app_owner;
    ///      CREATE TABLE notes (id SERIAL PRIMARY KEY);
    ///      ALTER TABLE notes OWNER TO app_owner;",
    /// )?;
    /// let sequence = db
    ///     .sequence_by_target(TargetName::new("notes_id_seq", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(sequence.authorization(), Some("app_owner"));
    /// # Ok(())
    /// # }
    /// ```
    fn authorization(&self) -> Option<&str>;

    /// Iterates over the privileges the own entry of the
    /// [`authorization`](Self::authorization) role holds on this sequence.
    ///
    /// PostgreSQL gives the owning role `USAGE`, `SELECT` and `UPDATE` when it
    /// creates the sequence, less what the default privileges of the creating
    /// role take, through an entry the owner can revoke and grant back like
    /// any other. A `REVOKE` naming the owning role takes from it, and an
    /// owner change hands it to the new owner as it stands.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app_owner;
    ///      GRANT CREATE ON SCHEMA public TO app_owner;
    ///      SET ROLE app_owner;
    ///      CREATE SEQUENCE ticket_seq;
    ///      REVOKE UPDATE ON SEQUENCE ticket_seq FROM app_owner;",
    /// )?;
    /// let sequence = db.sequences().next().unwrap();
    /// let held: Vec<_> = sequence.authorization_privileges().map(ToString::to_string).collect();
    /// assert_eq!(held, ["SELECT", "USAGE"]);
    /// # Ok(())
    /// # }
    /// ```
    fn authorization_privileges(&self) -> impl Iterator<Item = &Action>;

    /// Returns the privileges `role` holds on this sequence, as PostgreSQL's
    /// `has_sequence_privilege` answers them.
    ///
    /// A privilege reaches `role` through a grant naming the role itself,
    /// `PUBLIC`, or one of the
    /// [`inherited_roles`](crate::traits::RoleLike::inherited_roles) of the
    /// role, and through the owner's own entry, as
    /// [`authorization_privileges`](Self::authorization_privileges) reports
    /// it, when the role or one it inherits from is the sequence's
    /// [`authorization`](Self::authorization) role. A grant reaches the
    /// sequence through `ON SEQUENCE`, through a plain `ON` relation list and
    /// through `ALL SEQUENCES IN SCHEMA`, and `ALL PRIVILEGES` on it means
    /// `USAGE`, `SELECT` and `UPDATE`. What a `REVOKE` took is gone already. A
    /// superuser holds every privilege, and a role that is a member of a
    /// superuser does not. [`ActingRole::Public`], a caller holding no role,
    /// holds what `PUBLIC` holds.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this sequence.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE app;
    ///      CREATE ROLE app_group;
    ///      GRANT app_group TO app;
    ///      CREATE TABLE notes (id SERIAL PRIMARY KEY, body TEXT);
    ///      CREATE SEQUENCE ticket_seq;
    ///      GRANT INSERT ON notes TO app;
    ///      GRANT USAGE ON SEQUENCE ticket_seq TO app_group;",
    /// )?;
    /// let sequence = |name| {
    ///     db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(Option::unwrap)
    /// };
    /// let app = db.role("app").unwrap();
    ///
    /// let notes_id = sequence("notes_id_seq")?.privileges(app, &db)?;
    /// assert!(!notes_id.usage && !notes_id.update);
    ///
    /// let ticket = sequence("ticket_seq")?.privileges(app, &db)?;
    /// assert!(ticket.usage && !ticket.select && !ticket.update);
    /// # Ok(())
    /// # }
    /// ```
    fn privileges<'db>(
        &'db self,
        role: impl Into<ActingRole<'db, <Self::DB as DatabaseLike>::Role>>,
        database: &'db Self::DB,
    ) -> Result<SequencePrivileges, LookupError>
    where
        Self: 'db,
    {
        let target = self.target_name();
        if !database.sequences().any(|sequence| sequence.target_name() == target) {
            return Err(ObjectKind::Sequence.not_in_database(&target.to_string()));
        }
        let holders: Vec<&<Self::DB as DatabaseLike>::Role> = match role.into() {
            ActingRole::Declared(role) if role.is_superuser() => {
                return Ok(SequencePrivileges::ALL);
            }
            ActingRole::Declared(role) => privilege_holders(role, database).collect(),
            ActingRole::Public => Vec::new(),
        };
        let mut held = SequencePrivileges::NONE;
        if let Some(owner) = self.authorization()
            && holders.iter().any(|holder| holder.stored_name() == owner)
        {
            for privilege in self.authorization_privileges() {
                held.take(privilege);
            }
        }
        let is_this = |relation: Relation<'_, Self::DB>| {
            let Relation::Sequence(sequence) = relation else {
                return false;
            };
            sequence.target_name() == target
        };
        for grant in database.table_grants().filter(|grant| grant.relations(database).any(is_this))
        {
            held.take_grant(grant, &holders, database);
        }
        Ok(held)
    }
}
