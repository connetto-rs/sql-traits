//! Submodule providing a trait for describing SQL rewrite rules.

use core::fmt::Debug;

use sqlparser::ast::{CreateRuleAction, CreateRuleEvent, Expr};

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
};

/// The relation a rule is defined on, saying which kind it is.
///
/// PostgreSQL attaches a rule to a table or to a plain view, and refuses one
/// on a materialized view, so these are the two cases a caller handles.
#[derive(Clone)]
pub enum RuleRelation<'a, DB: DatabaseLike> {
    /// A table.
    Table(&'a DB::Table),
    /// A plain view.
    View(&'a DB::View),
}

impl<DB: DatabaseLike> Copy for RuleRelation<'_, DB> {}

impl<DB: DatabaseLike> Debug for RuleRelation<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Table(table) => f.debug_tuple("Table").field(table).finish(),
            Self::View(view) => f.debug_tuple("View").field(view).finish(),
        }
    }
}

/// A trait for types that can be treated as PostgreSQL rewrite rules, the
/// objects `CREATE RULE` defines.
///
/// A rule rewrites a command on one table or view into other commands. It
/// carries the event it fires on, an optional condition, whether its action
/// replaces the original command or runs alongside it, and the action itself.
/// The condition and action refer to the rows the command touches through the
/// `NEW` and `OLD` pseudo-relations.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createrule.html).
pub trait RuleLike: Clone + Debug + Metadata + Send + Sync {
    /// The database type the rule belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the rule.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE RULE keep_orders AS ON DELETE TO orders DO INSTEAD NOTHING;
    /// ",
    /// )?;
    /// let rule = db.rules().next().unwrap();
    /// assert_eq!(rule.name(), "keep_orders");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns the relation name the rule is defined on, as ingestion bound
    /// it.
    ///
    /// The name carries the qualifier of the relation it reached when the rule
    /// was created, or none for a relation in the default schema, and keeps
    /// the quoting written on the relation name. Unlike [`Self::relation`]
    /// this applies no resolution and cannot fail.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.orders (id INT);
    /// CREATE RULE keep_orders AS ON DELETE TO app.orders DO INSTEAD NOTHING;
    /// ",
    /// )?;
    /// let target = db.rules().next().unwrap().target_relation_name();
    /// assert_eq!(target.name(), "orders");
    /// assert_eq!(target.schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    fn target_relation_name(&self) -> TargetName<'_>;

    /// Returns the table or view the rule is defined on, the one its target
    /// reached when the rule was created.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::RelationNotFound`] when neither a table nor a
    /// view matches the target, and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot
    /// denote a single relation.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE VIEW recent AS SELECT id FROM orders;
    /// CREATE RULE recent_insert AS ON INSERT TO recent
    ///     DO INSTEAD INSERT INTO orders VALUES (new.id);
    /// ",
    /// )?;
    /// let rule = db.rules().next().unwrap();
    /// assert!(matches!(
    ///     rule.relation(&db)?,
    ///     RuleRelation::View(view) if view.view_name() == "recent"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<RuleRelation<'db, Self::DB>, LookupError>
    where
        Self: 'db;

    /// Returns the command the rule fires on.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::CreateRuleEvent, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE RULE keep_orders AS ON DELETE TO orders DO INSTEAD NOTHING;
    /// ",
    /// )?;
    /// assert_eq!(db.rules().next().unwrap().event(), CreateRuleEvent::Delete);
    /// # Ok(())
    /// # }
    /// ```
    fn event(&self) -> CreateRuleEvent;

    /// Returns whether the action replaces the original command, as `DO
    /// INSTEAD` asks, rather than running alongside it, as `DO ALSO` and a
    /// bare `DO` ask.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE TABLE audit (id INT);
    /// CREATE RULE keep_orders AS ON DELETE TO orders DO INSTEAD NOTHING;
    /// CREATE RULE log_orders AS ON INSERT TO orders DO INSERT INTO audit VALUES (new.id);
    /// ",
    /// )?;
    /// let instead: Vec<(&str, bool)> =
    ///     db.rules().map(|rule| (rule.name(), rule.is_instead())).collect();
    /// assert!(instead.contains(&("keep_orders", true)));
    /// assert!(instead.contains(&("log_orders", false)));
    /// # Ok(())
    /// # }
    /// ```
    fn is_instead(&self) -> bool;

    /// Returns the `WHERE` condition the rule fires under, if it has one.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE RULE keep_orders AS ON DELETE TO orders WHERE old.id < 10 DO INSTEAD NOTHING;
    /// ",
    /// )?;
    /// let condition = db.rules().next().unwrap().condition().unwrap();
    /// assert_eq!(condition.to_string(), "old.id < 10");
    /// # Ok(())
    /// # }
    /// ```
    fn condition(&self) -> Option<&Expr>;

    /// Returns the action the rule performs, as written: `NOTHING`, one
    /// command, or a parenthesized list of commands.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::CreateRuleAction, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE orders (id INT);
    /// CREATE TABLE audit (id INT);
    /// CREATE RULE log_orders AS ON INSERT TO orders DO ALSO INSERT INTO audit VALUES (new.id);
    /// ",
    /// )?;
    /// let action = db.rules().next().unwrap().action();
    /// assert!(matches!(action, CreateRuleAction::Statement(_)));
    /// assert_eq!(action.to_string(), "INSERT INTO audit VALUES (new.id)");
    /// # Ok(())
    /// # }
    /// ```
    fn action(&self) -> &CreateRuleAction;
}

impl<T: RuleLike> RuleLike for &T {
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn target_relation_name(&self) -> TargetName<'_> {
        (*self).target_relation_name()
    }

    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<RuleRelation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        (*self).relation(database)
    }

    fn event(&self) -> CreateRuleEvent {
        (*self).event()
    }

    fn is_instead(&self) -> bool {
        (*self).is_instead()
    }

    fn condition(&self) -> Option<&Expr> {
        (*self).condition()
    }

    fn action(&self) -> &CreateRuleAction {
        (*self).action()
    }
}
