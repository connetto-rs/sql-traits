//! Submodule providing a trait for describing SQL Role-like entities.

use alloc::{borrow::Cow, vec::Vec};
use core::{fmt::Debug, iter::once};

use crate::{
    traits::{DatabaseLike, Metadata, RoleMembershipLike},
    utils::identifier_resolution::normalize_identifier,
};

/// A trait for types that can be treated as SQL roles.
///
/// Roles in SQL are used to manage permissions and access control.
/// They can be granted to users or other roles, and can own database objects.
pub trait RoleLike: Debug + Clone + Ord + Eq + Metadata + Send + Sync {
    /// The database type the role belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the role.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE ROLE admin;")?;
    /// let role = db.role("admin").unwrap();
    /// assert_eq!(role.name(), "admin");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns whether the role name was quoted.
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the canonical stored role name.
    fn stored_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns whether this role has the `SUPERUSER` attribute.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE super_admin SUPERUSER;
    /// CREATE ROLE normal_user;
    /// ",
    /// )?;
    ///
    /// let super_role = db.role("super_admin").unwrap();
    /// assert!(super_role.is_superuser());
    ///
    /// let normal_role = db.role("normal_user").unwrap();
    /// assert!(!normal_role.is_superuser());
    /// # Ok(())
    /// # }
    /// ```
    fn is_superuser(&self) -> bool;

    /// Returns whether this role can create databases.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE db_creator CREATEDB;
    /// CREATE ROLE normal_user;
    /// ",
    /// )?;
    ///
    /// let creator = db.role("db_creator").unwrap();
    /// assert!(creator.can_create_db());
    ///
    /// let normal = db.role("normal_user").unwrap();
    /// assert!(!normal.can_create_db());
    /// # Ok(())
    /// # }
    /// ```
    fn can_create_db(&self) -> bool;

    /// Returns whether this role can create other roles.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE role_manager CREATEROLE;
    /// CREATE ROLE normal_user;
    /// ",
    /// )?;
    ///
    /// let manager = db.role("role_manager").unwrap();
    /// assert!(manager.can_create_role());
    ///
    /// let normal = db.role("normal_user").unwrap();
    /// assert!(!normal.can_create_role());
    /// # Ok(())
    /// # }
    /// ```
    fn can_create_role(&self) -> bool;

    /// Returns whether this role inherits privileges from roles it is a member
    /// of.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE inheriting_role INHERIT;
    /// CREATE ROLE non_inheriting NOINHERIT;
    /// ",
    /// )?;
    ///
    /// let inheriting = db.role("inheriting_role").unwrap();
    /// assert!(inheriting.inherits());
    ///
    /// let non_inheriting = db.role("non_inheriting").unwrap();
    /// assert!(!non_inheriting.inherits());
    /// # Ok(())
    /// # }
    /// ```
    fn inherits(&self) -> bool;

    /// Returns whether this role can log in.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE login_user LOGIN;
    /// CREATE ROLE nologin_role NOLOGIN;
    /// ",
    /// )?;
    ///
    /// let login = db.role("login_user").unwrap();
    /// assert!(login.can_login());
    ///
    /// let nologin = db.role("nologin_role").unwrap();
    /// assert!(!nologin.can_login());
    /// # Ok(())
    /// # }
    /// ```
    fn can_login(&self) -> bool;

    /// Returns whether this role can bypass row-level security policies.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE bypass_role BYPASSRLS;
    /// CREATE ROLE normal_role;
    /// ",
    /// )?;
    ///
    /// let bypass = db.role("bypass_role").unwrap();
    /// assert!(bypass.can_bypass_rls());
    ///
    /// let normal = db.role("normal_role").unwrap();
    /// assert!(!normal.can_bypass_rls());
    /// # Ok(())
    /// # }
    /// ```
    fn can_bypass_rls(&self) -> bool;

    /// Returns whether this role is a replication role.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE repl_role REPLICATION;
    /// CREATE ROLE normal_role;
    /// ",
    /// )?;
    ///
    /// let repl = db.role("repl_role").unwrap();
    /// assert!(repl.is_replication());
    ///
    /// let normal = db.role("normal_role").unwrap();
    /// assert!(!normal.is_replication());
    /// # Ok(())
    /// # }
    /// ```
    fn is_replication(&self) -> bool;

    /// Returns the connection limit for this role, if any.
    ///
    /// `None` means unlimited connections, whether no limit was written or
    /// it was written as `-1`, the value PostgreSQL stores for no limit.
    /// `Some(0)` admits no connection at all.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE limited_role CONNECTION LIMIT 5;
    /// CREATE ROLE unlimited_role;
    /// CREATE ROLE reset_role CONNECTION LIMIT -1;
    /// ",
    /// )?;
    ///
    /// let limited = db.role("limited_role").unwrap();
    /// assert_eq!(limited.connection_limit(), Some(5));
    ///
    /// let unlimited = db.role("unlimited_role").unwrap();
    /// assert!(unlimited.connection_limit().is_none());
    ///
    /// let reset = db.role("reset_role").unwrap();
    /// assert!(reset.connection_limit().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn connection_limit(&self) -> Option<i32>;

    /// Returns the memberships this role holds, one for each role it was
    /// granted, whether by `GRANT role TO role` or by a membership clause of
    /// `CREATE ROLE`.
    ///
    /// A membership may name a granted role the database does not hold, as a
    /// schema checked under an open world may.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE bypasser BYPASSRLS;
    /// CREATE ROLE app_user NOINHERIT;
    /// GRANT bypasser TO app_user;
    /// ",
    /// )?;
    ///
    /// let app_user = db.role("app_user").unwrap();
    /// let membership = app_user.memberships(&db).next().unwrap();
    /// assert_eq!(membership.role_name(), "bypasser");
    /// assert!(!membership.admin_option());
    /// assert!(!membership.inherit_option());
    /// assert!(membership.set_option());
    /// # Ok(())
    /// # }
    /// ```
    fn memberships<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::RoleMembership> {
        let name = self.stored_name();
        database
            .role_memberships()
            .filter(move |membership| membership.stored_member_name() == name)
    }

    /// Returns the roles that this role is a member of, those of its
    /// [`memberships`](Self::memberships) the database holds.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE parent_role;
    /// CREATE ROLE granted_role;
    /// CREATE ROLE child_role IN ROLE parent_role;
    /// GRANT granted_role TO child_role;
    /// ",
    /// )?;
    ///
    /// let child = db.role("child_role").unwrap();
    /// let parents: Vec<_> = child.member_of(&db).map(RoleLike::name).collect();
    /// assert_eq!(parents, ["parent_role", "granted_role"]);
    /// # Ok(())
    /// # }
    /// ```
    fn member_of<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Role> {
        self.memberships(database).filter_map(move |membership| membership.role(database))
    }

    /// Returns the roles whose privileges this role uses without `SET ROLE`,
    /// each once and never the role itself.
    ///
    /// PostgreSQL passes a granted role's privileges on along a membership
    /// carrying [`inherit_option`](RoleMembershipLike::inherit_option), and
    /// on again along each inheriting membership of that role in turn. A
    /// membership without the option ends the chain there, and a granted
    /// role the database does not hold passes nothing on.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE reader;
    /// CREATE ROLE auditor;
    /// CREATE ROLE staff IN ROLE reader;
    /// CREATE ROLE app_user;
    /// GRANT staff TO app_user;
    /// GRANT auditor TO app_user WITH INHERIT FALSE;
    /// ",
    /// )?;
    ///
    /// let app_user = db.role("app_user").unwrap();
    /// let inherited: Vec<_> = app_user.inherited_roles(&db).map(RoleLike::name).collect();
    /// assert_eq!(inherited, ["staff", "reader"]);
    /// # Ok(())
    /// # }
    /// ```
    fn inherited_roles<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Role> {
        /// Appends to `reached` each role an inheriting membership among
        /// `memberships` grants, unless it is the walk's own role or was
        /// reached already.
        fn reach<'db, DB: DatabaseLike>(
            memberships: impl Iterator<Item = &'db DB::RoleMembership>,
            database: &'db DB,
            own_name: &str,
            reached: &mut Vec<&'db DB::Role>,
        ) {
            for granted in memberships
                .filter(|membership| membership.inherit_option())
                .filter_map(|membership| membership.role(database))
            {
                if granted.stored_name() != own_name && !reached.contains(&granted) {
                    reached.push(granted);
                }
            }
        }

        let own_name = self.stored_name();
        let mut reached = Vec::new();
        reach(self.memberships(database), database, &own_name, &mut reached);
        let mut next = 0;
        while let Some(&role) = reached.get(next) {
            next += 1;
            reach(role.memberships(database), database, &own_name, &mut reached);
        }
        reached.into_iter()
    }

    /// Returns the policies that reference this role.
    ///
    /// These are the policies whose `TO` clause names this role.
    /// [`PolicyLike::applies_to_role`] answers which policies apply to it,
    /// `PUBLIC` and inherited roles included.
    ///
    /// [`PolicyLike::applies_to_role`]: crate::traits::PolicyLike::applies_to_role
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
    /// CREATE ROLE my_role;
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table TO my_role USING (true);
    /// ",
    /// )?;
    ///
    /// let role = db.role("my_role").unwrap();
    /// let policies: Vec<_> = role.policies(&db).collect();
    /// assert_eq!(policies.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn policies<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Policy>;
}

/// Returns `role` and its [`inherited_roles`](RoleLike::inherited_roles),
/// which together hold every privilege `role` uses without `SET ROLE`.
pub(crate) fn privilege_holders<'db, DB: DatabaseLike>(
    role: &'db DB::Role,
    database: &'db DB,
) -> impl Iterator<Item = &'db DB::Role> {
    once(role).chain(role.inherited_roles(database))
}

impl<T: RoleLike> RoleLike for &T {
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn name_is_quoted(&self) -> bool {
        (*self).name_is_quoted()
    }

    fn stored_name(&self) -> Cow<'_, str> {
        (*self).stored_name()
    }

    fn is_superuser(&self) -> bool {
        (*self).is_superuser()
    }

    fn can_create_db(&self) -> bool {
        (*self).can_create_db()
    }

    fn can_create_role(&self) -> bool {
        (*self).can_create_role()
    }

    fn inherits(&self) -> bool {
        (*self).inherits()
    }

    fn can_login(&self) -> bool {
        (*self).can_login()
    }

    fn can_bypass_rls(&self) -> bool {
        (*self).can_bypass_rls()
    }

    fn is_replication(&self) -> bool {
        (*self).is_replication()
    }

    fn connection_limit(&self) -> Option<i32> {
        (*self).connection_limit()
    }

    fn policies<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Policy> {
        (*self).policies(database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

    use super::*;
    use crate::structs::ParserDB;

    /// Helper to parse SQL using PostgreSQL dialect
    fn parse_postgres(sql: &str) -> ParserDB {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        ParserDB::from_statements(statements, "test".to_string()).unwrap()
    }

    #[test]
    fn test_role_ref_implementation() {
        let sql = r"
            CREATE TABLE t (id INT);
            CREATE ROLE parent_role;
            CREATE ROLE test_role SUPERUSER CREATEDB CREATEROLE LOGIN IN ROLE parent_role
                CONNECTION LIMIT 5;
            CREATE POLICY p ON t TO test_role USING (id > 0);
        ";
        let db = parse_postgres(sql);
        let role = db.role("test_role").expect("Role not found");

        // Use reference to role and explicitly route every call through
        // `impl RoleLike for &T` so coverage credits the blanket forwarding
        // bodies (lines 292–344).
        let role_ref: &<ParserDB as DatabaseLike>::Role = role;

        assert_eq!(<&_ as RoleLike>::name(&role_ref), "test_role");
        assert!(!<&_ as RoleLike>::name_is_quoted(&role_ref));
        assert_eq!(<&_ as RoleLike>::stored_name(&role_ref), "test_role");
        assert!(<&_ as RoleLike>::is_superuser(&role_ref));
        assert!(<&_ as RoleLike>::can_create_db(&role_ref));
        assert!(<&_ as RoleLike>::can_create_role(&role_ref));
        assert!(<&_ as RoleLike>::inherits(&role_ref));
        assert!(<&_ as RoleLike>::can_login(&role_ref));
        assert!(!<&_ as RoleLike>::can_bypass_rls(&role_ref));
        assert!(!<&_ as RoleLike>::is_replication(&role_ref));
        assert_eq!(<&_ as RoleLike>::connection_limit(&role_ref), Some(5));

        let memberships: Vec<_> = <&_ as RoleLike>::member_of(&role_ref, &db).collect();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].name(), "parent_role");

        let policies: Vec<_> = <&_ as RoleLike>::policies(&role_ref, &db).collect();
        assert_eq!(policies.len(), 1);
    }
}
