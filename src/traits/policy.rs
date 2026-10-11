//! Submodule providing a trait for describing SQL Policy-like entities.

use core::{borrow::Borrow, fmt::Debug, hash::Hash};

use sqlparser::ast::{CreatePolicyCommand, CreatePolicyType, Expr, Owner};

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{DatabaseLike, DocumentationMetadata, Metadata},
};

/// A trait for types that can be treated as SQL policies.
pub trait PolicyLike:
    Debug
    + Clone
    + Send
    + Sync
    + Hash
    + Ord
    + Eq
    + Metadata
    + DocumentationMetadata
    + Borrow<<<Self as PolicyLike>::DB as DatabaseLike>::Policy>
{
    /// The database type the policy belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the policy.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table USING (id > 0);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// assert_eq!(policy.name(), "my_policy");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns the table the policy is defined on, the one its target reached
    /// when the policy was created.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::TableNotFound`] when no table matches the target,
    /// and [`LookupError::InvalidObjectName`] or
    /// [`LookupError::AmbiguousTableLookup`] when the target name cannot denote
    /// a single table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.\"MyTable\" (id INT);
    /// CREATE POLICY my_policy ON app.\"MyTable\" USING (id > 0);
    /// ",
    /// )?;
    /// let table = db
    ///     .table_by_target(
    ///         TargetName::new("MyTable", true).with_schema("app", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// assert_eq!(policy.table(&db)?, table);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// A policy naming a table nothing creates is refused as it is read, so a
    /// recorded policy always has a target. The failure this reports is a
    /// policy queried against a database that does not hold it:
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{errors::LookupError, prelude::*};
    ///
    /// let owned = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE docs (id INT); CREATE POLICY p ON docs USING (true);",
    /// )?;
    /// let elsewhere = ParserDB::parse::<GenericDialect>("CREATE TABLE other (id INT);")?;
    /// let policy = owned.policies().next().unwrap();
    /// assert_eq!(
    ///     policy.table(&elsewhere),
    ///     Err(LookupError::TableNotFound { object_name: "docs".to_string() })
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db;

    /// Returns the table name the policy targets, as ingestion bound it.
    ///
    /// The name carries the qualifier of the table it reached when the policy
    /// was created, or none for a table in the default schema, and keeps the
    /// quoting written on the table name. Unlike [`Self::table`] this applies
    /// no resolution and cannot fail, so a caller with its own resolution
    /// rules can read the target and resolve it itself.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// SET search_path TO app;
    /// CREATE TABLE app.docs (id INT);
    /// CREATE POLICY docs_policy ON docs USING (true);
    /// ",
    /// )?;
    /// let policy = db.policies().next().unwrap();
    /// // The search path carried the bare name into `app` when the policy was made.
    /// let target = policy.target_table_name();
    /// assert_eq!(target.name(), "docs");
    /// assert_eq!(target.schema(), Some("app"));
    /// assert_eq!(policy.table(&db)?.table_schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Quoting is preserved on both parts, so a caller can tell a
    /// case-sensitive target from a folded one:
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.\"MyTable\" (id INT);
    /// CREATE POLICY my_policy ON app.\"MyTable\" USING (id > 0);
    /// ",
    /// )?;
    /// let policy = db.policies().next().unwrap();
    /// let target = policy.target_table_name();
    /// assert_eq!(target.schema(), Some("app"));
    /// assert!(!target.schema_is_quoted());
    /// assert_eq!(target.name(), "MyTable");
    /// assert!(target.name_is_quoted());
    /// assert_eq!(target.to_string(), "app.\"MyTable\"");
    /// # Ok(())
    /// # }
    /// ```
    fn target_table_name(&self) -> TargetName<'_>;

    /// Returns the command the policy applies to.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::CreatePolicyCommand;
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY select_policy ON my_table FOR SELECT USING (true);
    /// CREATE POLICY all_policy ON my_table USING (true);
    /// ";
    /// let db = ParserDB::parse::<GenericDialect>(sql)?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let select_policy = table.policies(&db)?.find(|p| p.name() == "select_policy").unwrap();
    /// assert_eq!(select_policy.command(), CreatePolicyCommand::Select);
    ///
    /// let all_policy = table.policies(&db)?.find(|p| p.name() == "all_policy").unwrap();
    /// assert_eq!(all_policy.command(), CreatePolicyCommand::All);
    /// # Ok(())
    /// # }
    /// ```
    fn command(&self) -> CreatePolicyCommand;

    /// Returns whether the policy grants access (permissive) or further
    /// restricts it (restrictive).
    ///
    /// PostgreSQL combines policies as
    /// `(PERMISSIVE_1 OR PERMISSIVE_2 OR ...) AND RESTRICTIVE_1 AND ...`, so
    /// the two kinds are not interchangeable. A table with row-level
    /// security enabled and only restrictive policies denies every row, as
    /// no permissive policy exists to grant access in the first place.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::CreatePolicyType;
    ///
    /// let sql = "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY restrictive_policy ON my_table AS RESTRICTIVE USING (id > 0);
    /// CREATE POLICY permissive_policy ON my_table AS PERMISSIVE USING (id > 0);
    /// CREATE POLICY implicit_policy ON my_table USING (id > 0);
    /// ";
    /// let db = ParserDB::parse::<GenericDialect>(sql)?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let policy = table.policies(&db)?.find(|p| p.name() == "restrictive_policy").unwrap();
    /// assert_eq!(policy.policy_type(), CreatePolicyType::Restrictive);
    ///
    /// let policy = table.policies(&db)?.find(|p| p.name() == "permissive_policy").unwrap();
    /// assert_eq!(policy.policy_type(), CreatePolicyType::Permissive);
    ///
    /// // The modifier is optional, and PostgreSQL defaults to permissive.
    /// let policy = table.policies(&db)?.find(|p| p.name() == "implicit_policy").unwrap();
    /// assert_eq!(policy.policy_type(), CreatePolicyType::Permissive);
    /// # Ok(())
    /// # }
    /// ```
    fn policy_type(&self) -> CreatePolicyType;

    /// Returns the roles the policy applies to.
    /// If empty, it applies to all roles (PUBLIC).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let sql = "
    /// CREATE ROLE user1;
    /// CREATE ROLE user2;
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table TO user1, user2 USING (true);
    /// CREATE POLICY public_policy ON my_table TO PUBLIC USING (true);
    /// ";
    /// let db = ParserDB::parse::<GenericDialect>(sql)?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let policy = table.policies(&db)?.find(|p| p.name() == "my_policy").unwrap();
    /// // Logic to verify roles (roles() returns iterator)
    /// assert_eq!(policy.roles(&db).count(), 2);
    ///
    /// let public_policy = table.policies(&db)?.find(|p| p.name() == "public_policy").unwrap();
    /// assert_eq!(public_policy.roles(&db).count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn roles<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Owner>
    where
        Self: 'db;

    /// Returns whether the policy applies to every role.
    ///
    /// A policy says so in two ways, and this reader folds both: writing
    /// `TO PUBLIC`, and writing no `TO` clause at all, which PostgreSQL
    /// defaults to `PUBLIC`. Neither is visible in [`Self::roles`], where
    /// `PUBLIC` arrives as an ordinary unquoted name and an absent clause
    /// arrives as an empty iterator, so a caller reading roles alone cannot
    /// tell "everyone" from a role somebody created called `public`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE ROLE \"PUBLIC\";
    /// CREATE ROLE reader;
    /// CREATE TABLE docs (id INT);
    /// CREATE POLICY spelled ON docs TO PUBLIC USING (true);
    /// CREATE POLICY implied ON docs USING (true);
    /// CREATE POLICY named ON docs TO reader USING (true);
    /// CREATE POLICY quoted ON docs TO \"PUBLIC\" USING (true);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = |name: &str| table.policies(&db).unwrap().find(|p| p.name() == name).unwrap();
    ///
    /// assert!(policy("spelled").applies_to_public());
    /// assert!(policy("implied").applies_to_public());
    /// assert!(!policy("named").applies_to_public());
    /// // A quoted name is a role of that exact name, not the pseudo-role.
    /// assert!(!policy("quoted").applies_to_public());
    /// # Ok(())
    /// # }
    /// ```
    fn applies_to_public(&self) -> bool;

    /// Returns the `USING` expression of the policy, if any.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table USING (id > 0);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// assert!(policy.using_expression(&db).is_some());
    /// # Ok(())
    /// # }
    /// ```
    fn using_expression<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db;

    /// Returns the functions used in the `USING` expression.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this policy.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION my_func() RETURNS BOOLEAN AS 'SELECT true';
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table USING (my_func());
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// let functions: Vec<_> = policy.using_functions(&db)?.collect();
    /// assert_eq!(functions.len(), 1);
    /// assert_eq!(functions[0].name(), "my_func");
    /// # Ok(())
    /// # }
    /// ```
    fn using_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError>;

    /// Returns the `WITH CHECK` expression of the policy, if any.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table WITH CHECK (id < 10);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// assert!(policy.check_expression(&db).is_some());
    /// # Ok(())
    /// # }
    /// ```
    fn check_expression<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db;

    /// Returns the functions used in the `WITH CHECK` expression.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this policy.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION check_func() RETURNS BOOLEAN AS 'SELECT true';
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY my_policy ON my_table WITH CHECK (check_func());
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let policy = table.policies(&db)?.next().unwrap();
    /// let functions: Vec<_> = policy.check_functions(&db)?.collect();
    /// assert_eq!(functions.len(), 1);
    /// assert_eq!(functions[0].name(), "check_func");
    /// # Ok(())
    /// # }
    /// ```
    fn check_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError>;
}

impl<T: PolicyLike> PolicyLike for &T
where
    Self: Borrow<<<T as PolicyLike>::DB as DatabaseLike>::Policy>,
{
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db,
    {
        (*self).table(database)
    }

    fn target_table_name(&self) -> TargetName<'_> {
        (*self).target_table_name()
    }

    fn command(&self) -> CreatePolicyCommand {
        (*self).command()
    }

    fn policy_type(&self) -> CreatePolicyType {
        (*self).policy_type()
    }

    fn roles<'db>(&'db self, database: &'db Self::DB) -> impl Iterator<Item = &'db Owner>
    where
        Self: 'db,
    {
        (*self).roles(database)
    }

    fn applies_to_public(&self) -> bool {
        (*self).applies_to_public()
    }

    fn using_expression<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        (*self).using_expression(database)
    }

    fn using_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        (*self).using_functions(database)
    }

    fn check_expression<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Expr>
    where
        Self: 'db,
    {
        (*self).check_expression(database)
    }

    fn check_functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        (*self).check_functions(database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{CreatePolicyCommand, CreatePolicyType},
        dialect::GenericDialect,
    };

    use super::*;
    use crate::{
        structs::{IdentifierCase, ParserDB, TargetName},
        traits::{DatabaseLike, FunctionLike, TableLike},
    };

    #[test]
    fn test_policy_ref_implementation() {
        let sql = r"
            CREATE TABLE my_table (id INT);
            CREATE FUNCTION my_func() RETURNS BOOLEAN AS 'SELECT true';
            CREATE FUNCTION check_func() RETURNS BOOLEAN AS 'SELECT true';
            CREATE POLICY my_policy ON my_table
                FOR SELECT
                TO PUBLIC
                USING (id > 0 AND my_func())
                WITH CHECK (id < 10 AND check_func());
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table not found");
        let policy = table.policies(&db).expect("policies").next().expect("Policy not found");

        // Use reference to policy
        let policy_ref = &policy;

        assert_eq!(policy_ref.name(), "my_policy");

        let policy_table = policy_ref.table(&db).expect("Table not found");
        assert_eq!(policy_table.table_name(), "my_table");

        assert_eq!(policy_ref.command(), CreatePolicyCommand::Select);
        assert_eq!(policy_ref.policy_type(), CreatePolicyType::Permissive);

        let roles: Vec<_> = policy_ref.roles(&db).collect();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].to_string(), "PUBLIC");

        let using_expr = policy_ref.using_expression(&db);
        assert!(using_expr.is_some());
        let using_str = using_expr.unwrap().to_string();
        assert!(using_str.contains("id > 0"));
        assert!(using_str.contains("my_func()"));

        let using_funcs: Vec<_> =
            policy_ref.using_functions(&db).expect("using_functions").collect();
        assert_eq!(using_funcs.len(), 1);
        assert_eq!(using_funcs[0].name(), "my_func");

        let check_expr = policy_ref.check_expression(&db);
        assert!(check_expr.is_some());
        let check_str = check_expr.unwrap().to_string();
        assert!(check_str.contains("id < 10"));
        assert!(check_str.contains("check_func()"));

        let check_funcs: Vec<_> =
            policy_ref.check_functions(&db).expect("check_functions").collect();
        assert_eq!(check_funcs.len(), 1);
        assert_eq!(check_funcs[0].name(), "check_func");
    }

    #[test]
    fn test_policy_type_modifier() {
        let sql = r"
            CREATE TABLE my_table (id INT);
            CREATE POLICY restrictive_policy ON my_table AS RESTRICTIVE USING (id > 0);
            CREATE POLICY permissive_policy ON my_table AS PERMISSIVE USING (id > 0);
            CREATE POLICY implicit_policy ON my_table USING (id > 0);
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table not found");

        let policy_type = |name: &str| {
            table
                .policies(&db)
                .expect("policies")
                .find(|policy| policy.name() == name)
                .expect("Policy not found")
                .policy_type()
        };

        assert_eq!(policy_type("restrictive_policy"), CreatePolicyType::Restrictive);
        assert_eq!(policy_type("permissive_policy"), CreatePolicyType::Permissive);
        assert_eq!(policy_type("implicit_policy"), CreatePolicyType::Permissive);
    }

    #[test]
    fn test_alter_policy_rename() {
        let sql = r"
            CREATE TABLE my_table (id INT);
            CREATE POLICY old_policy ON my_table USING (true);
            ALTER POLICY old_policy ON my_table RENAME TO new_policy;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let table = db
            .table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Table not found");
        let policies: Vec<_> = table.policies(&db).expect("policies").collect();

        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name(), "new_policy");
    }

    #[test]
    fn test_alter_nonexistent_policy_fails() {
        let sql = r"
            CREATE TABLE my_table (id INT);
            ALTER POLICY nonexistent ON my_table RENAME TO other;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::AlterPolicyNotFound { policy_name } if policy_name == "nonexistent")
        );
    }

    #[test]
    fn test_policy_on_schema_qualified_table_resolves() {
        let sql = r"
            CREATE SCHEMA app;
            CREATE TABLE app.docs (id INT, owner TEXT);
            CREATE POLICY p ON app.docs USING (owner = 'x');
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let policy = db.policies().next().expect("Policy not found");
        let table = policy.table(&db).expect("Policy target should resolve");

        assert_eq!(table.table_name(), "docs");
        assert_eq!(table.table_schema(), Some("app"));
        assert_eq!(table.policies(&db).expect("policies").count(), 1);
    }

    #[test]
    fn test_policy_on_quoted_table_resolves() {
        let sql = r#"
            CREATE TABLE "MyTable" (id INT, owner TEXT);
            CREATE POLICY p ON "MyTable" USING (owner = 'x');
        "#;
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let policy = db.policies().next().expect("Policy not found");
        let table = policy.table(&db).expect("Policy target should resolve");

        assert_eq!(table.table_name(), "MyTable");
        assert_eq!(table.table_schema(), None);
        assert_eq!(table.policies(&db).expect("policies").count(), 1);
    }

    /// A policy exists only on its table, so one naming a table nothing
    /// creates is refused as it is read, which is what the database does.
    #[test]
    fn test_policy_on_an_absent_table_is_refused() {
        let sql = r"CREATE POLICY orphan ON absent_table USING (true);";
        assert!(matches!(
            ParserDB::parse::<GenericDialect>(sql),
            Err(crate::errors::Error::TableNotFoundForPolicy { ref table_name, ref policy_name })
                if table_name == "absent_table" && policy_name == "orphan"
        ));
    }

    #[test]
    fn test_policy_on_an_unquoted_target_does_not_match_a_quoted_table() {
        // PostgreSQL folds the unquoted target to lowercase, so it must not
        // match a table registered under a quoted mixed-case name, and the
        // policy is then refused for naming a table that does not exist.
        let sql = r#"
            CREATE TABLE "MyTable" (id INT);
            CREATE POLICY p ON MyTable USING (true);
        "#;
        assert!(matches!(
            ParserDB::parse::<GenericDialect>(sql),
            Err(crate::errors::Error::TableNotFoundForPolicy { ref table_name, .. })
                if table_name == "MyTable"
        ));
    }
}
