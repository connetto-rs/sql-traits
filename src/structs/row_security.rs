//! What row-level security applies to one statement on one table, for one
//! role, as PostgreSQL's table "Policies Applied by Command Type" in
//! `CREATE POLICY` lays it out.

use alloc::{string::String, vec::Vec};

use sqlparser::ast::Expr;

/// Who a statement runs as.
#[derive(Debug)]
pub enum ActingRole<'db, R> {
    /// A role the schema declares.
    Declared(&'db R),
    /// A caller holding no role, known only as `PUBLIC`. It owns nothing and
    /// bypasses nothing, and a policy scoped to a role cannot be settled for
    /// it.
    Public,
}

impl<'db, R> From<&'db R> for ActingRole<'db, R> {
    fn from(role: &'db R) -> Self {
        Self::Declared(role)
    }
}

/// Who a statement runs as, and the role that ran the schema.
///
/// The role running `CREATE TABLE` or `CREATE VIEW` owns what it creates, so
/// the schema's text names no owner unless it says `OWNER TO`. The schema
/// owner stands for that role: it owns every table and view whose owner the
/// schema never names. Without one, such a table belongs to a role other than
/// the acting role, and such a view cannot be read.
#[derive(Debug)]
pub struct Requester<'db, R> {
    role: ActingRole<'db, R>,
    schema_owner: Option<&'db R>,
}

impl<'db, R> Requester<'db, R> {
    /// A statement run as `role` against a schema run as `schema_owner`.
    #[must_use]
    pub const fn new(role: ActingRole<'db, R>, schema_owner: Option<&'db R>) -> Self {
        Self { role, schema_owner }
    }

    /// Who the statement runs as.
    #[must_use]
    pub const fn role(&self) -> &ActingRole<'db, R> {
        &self.role
    }

    /// The role owning every table and view whose owner the schema never
    /// names.
    #[must_use]
    pub const fn schema_owner(&self) -> Option<&'db R> {
        self.schema_owner
    }
}

impl<'db, R> From<ActingRole<'db, R>> for Requester<'db, R> {
    fn from(role: ActingRole<'db, R>) -> Self {
        Self::new(role, None)
    }
}

impl<'db, R> From<&'db R> for Requester<'db, R> {
    fn from(role: &'db R) -> Self {
        Self::new(ActingRole::Declared(role), None)
    }
}

/// The shape of a statement, as far as row-level security tells shapes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowStatement {
    /// `SELECT`, and `COPY ... TO`.
    Select,
    /// `SELECT` with `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE` or
    /// `FOR KEY SHARE`.
    SelectLocking,
    /// `INSERT`.
    Insert {
        /// Whether `RETURNING` reads the inserted row back.
        returning: bool,
    },
    /// `UPDATE`.
    Update {
        /// Whether the statement reads a column of the table, in `WHERE`, a
        /// `SET` expression, `FROM` or `RETURNING`.
        reads: bool,
    },
    /// `DELETE`.
    Delete {
        /// Whether the statement reads a column of the table, in `WHERE`,
        /// `USING` or `RETURNING`.
        reads: bool,
    },
    /// `INSERT ... ON CONFLICT DO NOTHING`.
    InsertOnConflictDoNothing {
        /// Whether the clause names an arbiter index or constraint.
        arbiter: bool,
    },
    /// `INSERT ... ON CONFLICT DO UPDATE`.
    InsertOnConflictDoUpdate,
    /// `MERGE ... WHEN NOT MATCHED THEN INSERT`.
    MergeInsert {
        /// Whether `RETURNING` reads the inserted row back.
        returning: bool,
    },
    /// `MERGE ... WHEN MATCHED THEN UPDATE`.
    MergeUpdate,
    /// `MERGE ... WHEN MATCHED THEN DELETE`.
    MergeDelete,
    /// `MERGE ... THEN DO NOTHING`.
    MergeDoNothing,
}

/// Whose policies a gate applies, the command they are written for or `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GatedCommand {
    /// `FOR SELECT` and `FOR ALL` policies.
    Select,
    /// `FOR INSERT` and `FOR ALL` policies.
    Insert,
    /// `FOR UPDATE` and `FOR ALL` policies.
    Update,
    /// `FOR DELETE` and `FOR ALL` policies.
    Delete,
}

/// Which version of a row a gate judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowVersion {
    /// The row as stored before the statement.
    Existing,
    /// The row an `INSERT` proposes, judged whether or not a conflict keeps it
    /// out.
    Inserted,
    /// The row an `UPDATE` produces, or the auxiliary update of `ON CONFLICT
    /// DO UPDATE`.
    Updated,
}

/// What happens to a row that fails a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GateFailure {
    /// The row is silently left out of the statement.
    Filter,
    /// The statement fails with `new row violates row-level security policy`
    /// (`42501`).
    Raise,
}

/// Which clause of a policy an expression comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyClause {
    /// `USING`.
    Using,
    /// `WITH CHECK`.
    WithCheck,
}

/// One policy's expression in a gate.
#[derive(Debug, PartialEq, Eq)]
pub struct GateTerm<'db, P> {
    pub(crate) policy: &'db P,
    pub(crate) clause: PolicyClause,
    pub(crate) expression: &'db Expr,
}

impl<'db, P> GateTerm<'db, P> {
    /// The policy the expression belongs to.
    #[must_use]
    pub fn policy(&self) -> &'db P {
        self.policy
    }

    /// The clause the expression is taken from. A check falls back to `USING`
    /// where the policy has no `WITH CHECK`.
    #[must_use]
    pub fn clause(&self) -> PolicyClause {
        self.clause
    }

    /// The expression.
    #[must_use]
    pub fn expression(&self) -> &'db Expr {
        self.expression
    }
}

/// One test a row has to pass.
///
/// The row passes when some permissive term holds and every restrictive term
/// holds, so a gate with no permissive term admits nothing.
#[derive(Debug, PartialEq, Eq)]
pub struct RowGate<'db, P> {
    pub(crate) command: GatedCommand,
    pub(crate) row: RowVersion,
    pub(crate) failure: GateFailure,
    pub(crate) permissive: Vec<GateTerm<'db, P>>,
    pub(crate) restrictive: Vec<GateTerm<'db, P>>,
}

impl<'db, P> RowGate<'db, P> {
    /// Whose policies the gate applies.
    #[must_use]
    pub fn command(&self) -> GatedCommand {
        self.command
    }

    /// The row version the gate judges.
    #[must_use]
    pub fn row(&self) -> RowVersion {
        self.row
    }

    /// What happens to a row that fails the gate.
    #[must_use]
    pub fn failure(&self) -> GateFailure {
        self.failure
    }

    /// The permissive terms, any of which admits the row.
    #[must_use]
    pub fn permissive(&self) -> &[GateTerm<'db, P>] {
        &self.permissive
    }

    /// The restrictive terms, every one of which has to hold.
    #[must_use]
    pub fn restrictive(&self) -> &[GateTerm<'db, P>] {
        &self.restrictive
    }
}

/// Why row-level security does not apply to a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowSecurityExemption {
    /// The table does not enable row-level security.
    Disabled,
    /// The role is a superuser.
    Superuser,
    /// The role has `BYPASSRLS`.
    BypassRls,
    /// The role owns the table, or inherits from its owner, and the table does
    /// not `FORCE` row-level security.
    Owner,
}

/// What row-level security applies to one statement for one role.
#[derive(Debug, PartialEq, Eq)]
pub enum RowSecurity<'db, P> {
    /// No policy applies.
    Exempt(RowSecurityExemption),
    /// Every row the statement touches has to pass these gates.
    Gated(Vec<RowGate<'db, P>>),
    /// Expanding the statement's policies reads back into a table whose
    /// policies are still being expanded, so PostgreSQL refuses the statement
    /// with `infinite recursion detected in policy for relation` (`42P17`).
    InfiniteRecursion {
        /// The relation PostgreSQL names.
        relation: String,
    },
}

impl<'db, P> RowSecurity<'db, P> {
    /// The gates, or `None` where the statement is exempt or refused.
    #[must_use]
    pub fn gates(&self) -> Option<&[RowGate<'db, P>]> {
        match self {
            Self::Gated(gates) => Some(gates),
            Self::Exempt(_) | Self::InfiniteRecursion { .. } => None,
        }
    }

    /// Why no policy applies, or `None` where policies do.
    #[must_use]
    pub fn exemption(&self) -> Option<RowSecurityExemption> {
        match self {
            Self::Exempt(exemption) => Some(*exemption),
            Self::Gated(_) | Self::InfiniteRecursion { .. } => None,
        }
    }

    /// The relation PostgreSQL's recursion error names, or `None` where the
    /// statement runs.
    #[must_use]
    pub fn infinite_recursion(&self) -> Option<&str> {
        match self {
            Self::InfiniteRecursion { relation } => Some(relation),
            Self::Exempt(_) | Self::Gated(_) => None,
        }
    }
}
