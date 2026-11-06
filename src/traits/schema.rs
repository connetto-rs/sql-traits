//! Submodule providing a trait for describing SQL Schema-like entities.

use alloc::{borrow::Cow, vec::Vec};
use core::fmt::Debug;

use sqlparser::ast::Action;

use crate::{
    structs::ActingRole,
    traits::{
        DatabaseLike, GrantLike, Metadata, RoleLike, grant::GrantTarget, role::privilege_holders,
    },
    utils::identifier_resolution::{identifiers_match, normalize_identifier},
};

/// A trait for types that can be treated as SQL schemas.
///
/// Schemas in SQL are namespaces that contain database objects like tables,
/// views, functions, etc. They help organize database objects and manage
/// access control.
pub trait SchemaLike: Debug + Clone + Ord + Eq + Metadata + Send + Sync {
    /// The database type the schema belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SCHEMA my_schema;")?;
    /// let schema = db.schema("my_schema").unwrap();
    /// assert_eq!(schema.name(), "my_schema");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns whether the schema identifier was quoted in SQL.
    ///
    /// Quoted identifiers are resolved case-sensitively in PostgreSQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this schema: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE SCHEMA My_Schema;")?;
    /// let schema = db.schema("my_schema").unwrap();
    /// assert_eq!(schema.name(), "My_Schema");
    /// assert_eq!(schema.stored_name(), "my_schema");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the authorization owner of the schema, if specified.
    ///
    /// In PostgreSQL, schemas can have an owner specified via the
    /// `AUTHORIZATION` clause. Without one the schema belongs to the role it
    /// was created under, as `SET ROLE` and `SET SESSION AUTHORIZATION` leave
    /// it, and to none when the input had set no role.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE admin; CREATE SCHEMA my_schema AUTHORIZATION admin;",
    /// )?;
    /// let schema = db.schema("my_schema").unwrap();
    /// assert_eq!(schema.authorization(), Some("admin"));
    /// # Ok(())
    /// # }
    /// ```
    fn authorization(&self) -> Option<&str>;

    /// Returns whether `PUBLIC` holds the `USAGE` the schema was created
    /// with, apart from any `GRANT`.
    ///
    /// A new PostgreSQL database's `public` schema gives it, until `REVOKE
    /// USAGE ON SCHEMA public FROM PUBLIC` takes it. Every schema created
    /// later starts without it.
    fn public_holds_usage(&self) -> bool;

    /// Returns whether the owner's own entry still holds `USAGE`, which the
    /// owner holds from creation until a `REVOKE` naming the owner takes it.
    fn owner_holds_usage(&self) -> bool;

    /// Returns whether `role` holds `USAGE` on this schema, as PostgreSQL's
    /// `has_schema_privilege(role, schema, 'USAGE')` answers it.
    ///
    /// `USAGE` reaches `role` through a grant naming the role itself,
    /// `PUBLIC`, or one of the
    /// [`inherited_roles`](crate::traits::RoleLike::inherited_roles) of the
    /// role, through `PUBLIC`'s initial entry as
    /// [`public_holds_usage`](Self::public_holds_usage) reports it, and
    /// through the owner's own entry when the role or one it inherits from is
    /// the [`authorization`](Self::authorization) owner. `ALL PRIVILEGES`
    /// carries it and `CREATE` does not. A superuser holds it, and
    /// [`ActingRole::Public`] holds what `PUBLIC` holds.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE reader; CREATE ROLE stranger;
    ///      CREATE SCHEMA app;
    ///      GRANT USAGE ON SCHEMA app TO reader;
    ///      REVOKE USAGE ON SCHEMA public FROM PUBLIC;",
    /// )?;
    /// let (reader, stranger) = (db.role("reader").unwrap(), db.role("stranger").unwrap());
    /// let app = db.schema("app").unwrap();
    /// assert!(app.has_usage(reader, &db));
    /// assert!(!app.has_usage(stranger, &db));
    /// assert!(!db.schema("public").unwrap().has_usage(stranger, &db));
    /// # Ok(())
    /// # }
    /// ```
    fn has_usage<'db>(
        &'db self,
        role: impl Into<ActingRole<'db, <Self::DB as DatabaseLike>::Role>>,
        database: &'db Self::DB,
    ) -> bool
    where
        Self: 'db,
    {
        let holders: Vec<&<Self::DB as DatabaseLike>::Role> = match role.into() {
            ActingRole::Declared(role) if role.is_superuser() => return true,
            ActingRole::Declared(role) => privilege_holders(role, database).collect(),
            ActingRole::Public => Vec::new(),
        };
        if self.public_holds_usage() {
            return true;
        }
        if self.owner_holds_usage()
            && let Some(owner) = self.authorization()
            && holders.iter().any(|holder| holder.stored_name() == owner)
        {
            return true;
        }
        database.table_grants().any(|grant| {
            let on_schema = match grant.target() {
                GrantTarget::Schemas(mut names) => {
                    names.any(|name| {
                        identifiers_match(
                            name.name(),
                            name.name_is_quoted(),
                            self.name(),
                            self.name_is_quoted(),
                        )
                    })
                }
                _ => false,
            };
            on_schema
                && (grant.applies_to_public()
                    || holders.iter().any(|holder| grant.applies_to_role(holder)))
                && (grant.is_all_privileges()
                    || grant
                        .privileges(database)
                        .any(|privilege| matches!(privilege, Action::Usage)))
        })
    }
}

/// Blanket implementation for references to `SchemaLike` types.
impl<S: SchemaLike> SchemaLike for &S {
    type DB = S::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn name_is_quoted(&self) -> bool {
        (*self).name_is_quoted()
    }

    fn authorization(&self) -> Option<&str> {
        (*self).authorization()
    }

    fn public_holds_usage(&self) -> bool {
        (*self).public_holds_usage()
    }

    fn owner_holds_usage(&self) -> bool {
        (*self).owner_holds_usage()
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

    use super::*;
    use crate::structs::{IdentifierCase, ParserDB, TargetName};

    /// Helper to parse SQL using PostgreSQL dialect
    fn parse_postgres(sql: &str) -> Result<ParserDB, crate::errors::Error> {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql)?;
        ParserDB::from_statements(statements, "test".to_string())
    }

    #[test]
    fn test_create_simple_schema() {
        let db = parse_postgres("CREATE SCHEMA my_schema;").unwrap();
        let schema = db.schema("my_schema").expect("Schema should exist");
        assert_eq!(schema.name(), "my_schema");
        assert_eq!(schema.authorization(), None);
    }

    #[test]
    fn test_create_schema_with_authorization() {
        let db = parse_postgres("CREATE ROLE admin; CREATE SCHEMA my_schema AUTHORIZATION admin;")
            .unwrap();
        let schema = db.schema("my_schema").expect("Schema should exist");
        assert_eq!(schema.name(), "my_schema");
        assert_eq!(schema.authorization(), Some("admin"));
    }

    #[test]
    fn test_create_schema_authorization_only() {
        // CREATE SCHEMA AUTHORIZATION admin creates schema named "admin"
        let db = parse_postgres("CREATE ROLE admin; CREATE SCHEMA AUTHORIZATION admin;").unwrap();
        let schema = db.schema("admin").expect("Schema should exist");
        assert_eq!(schema.name(), "admin");
        assert_eq!(schema.authorization(), Some("admin"));
    }

    #[test]
    fn test_create_multiple_schemas() {
        let db = parse_postgres(
            "
            CREATE ROLE owner_b;
            CREATE ROLE owner_c;
            CREATE SCHEMA schema_a;
            CREATE SCHEMA schema_b AUTHORIZATION owner_b;
            CREATE SCHEMA AUTHORIZATION owner_c;
            ",
        )
        .unwrap();

        let schemas: Vec<_> = db.schemas().collect();
        assert_eq!(schemas.len(), 4, "public and the three created");

        let schema_a = db.schema("schema_a").expect("schema_a should exist");
        assert_eq!(schema_a.authorization(), None);

        let schema_b = db.schema("schema_b").expect("schema_b should exist");
        assert_eq!(schema_b.authorization(), Some("owner_b"));

        let schema_c = db.schema("owner_c").expect("owner_c should exist");
        assert_eq!(schema_c.authorization(), Some("owner_c"));
    }

    #[test]
    fn test_create_schema_if_not_exists() {
        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE SCHEMA IF NOT EXISTS my_schema;
            ",
        )
        .unwrap();

        let schemas: Vec<_> = db.schemas().collect();
        assert_eq!(schemas.len(), 2, "Should not create duplicate schema");
    }

    #[test]
    fn test_create_duplicate_schema_fails() {
        let result = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE SCHEMA my_schema;
            ",
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::SchemaAlreadyExists { schema_name } if schema_name == "my_schema")
        );
    }

    #[test]
    fn test_schema_not_found() {
        let db = parse_postgres("CREATE SCHEMA my_schema;").unwrap();
        assert!(db.schema("nonexistent").is_none());
    }

    #[test]
    fn test_schema_lookup_unquoted_identifier_is_case_insensitive() {
        let db = parse_postgres("CREATE SCHEMA Foo;").unwrap();

        assert!(db.schema("foo").is_some());
        assert!(db.schema("FOO").is_some());
        assert!(db.schema("\"foo\"").is_some());
        assert!(db.schema("\"Foo\"").is_none());
    }

    #[test]
    fn test_schema_lookup_quoted_identifier_is_case_sensitive() {
        let db = parse_postgres("CREATE SCHEMA \"Foo\";").unwrap();

        assert!(db.schema("\"Foo\"").is_some());
        assert!(db.schema("\"foo\"").is_none());
        assert!(db.schema("foo").is_none());
        assert!(db.schema("FOO").is_none());
    }

    #[test]
    fn test_create_schema_duplicate_unquoted_and_quoted_equivalent_fails() {
        let result = parse_postgres(
            "
            CREATE SCHEMA Foo;
            CREATE SCHEMA \"foo\";
            ",
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::SchemaAlreadyExists { schema_name } if schema_name == "foo")
        );
    }

    #[test]
    fn test_has_schemas() {
        let db_with_schemas = parse_postgres("CREATE SCHEMA my_schema;").unwrap();
        assert!(db_with_schemas.has_schemas());

        let db_without_schemas = parse_postgres("DROP SCHEMA public;").unwrap();
        assert!(!db_without_schemas.has_schemas());
    }

    // ========================
    // DROP SCHEMA tests
    // ========================

    #[test]
    fn test_drop_empty_schema() {
        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            DROP SCHEMA my_schema;
            ",
        )
        .unwrap();

        assert!(db.schema("my_schema").is_none());
        assert_eq!(db.schemas().map(|(schema, ())| schema.name()).collect::<Vec<_>>(), ["public"]);
    }

    #[test]
    fn test_drop_schema_if_exists() {
        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            DROP SCHEMA IF EXISTS my_schema;
            DROP SCHEMA IF EXISTS nonexistent;
            ",
        )
        .unwrap();

        assert!(db.schema("my_schema").is_none());
    }

    #[test]
    fn test_drop_schema_unquoted_name_is_case_insensitive() {
        let db = parse_postgres(
            "
            CREATE SCHEMA Foo;
            DROP SCHEMA foo;
            ",
        )
        .unwrap();
        assert!(db.schema("foo").is_none());
    }

    #[test]
    fn test_drop_schema_quoted_name_is_case_sensitive() {
        let result = parse_postgres(
            "
            CREATE SCHEMA Foo;
            DROP SCHEMA \"Foo\";
            ",
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::DropSchemaNotFound { schema_name } if schema_name == "Foo")
        );
    }

    #[test]
    fn test_drop_nonexistent_schema_fails() {
        let result = parse_postgres("DROP SCHEMA nonexistent;");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::DropSchemaNotFound { schema_name } if schema_name == "nonexistent")
        );
    }

    #[test]
    fn test_drop_non_empty_schema_fails_restrict() {
        let result = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE TABLE my_schema.my_table (id INT);
            DROP SCHEMA my_schema;
            ",
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::SchemaNotEmpty { schema_name } if schema_name == "my_schema")
        );
    }

    #[test]
    fn test_drop_schema_cascade() {
        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE TABLE my_schema.my_table (id INT);
            DROP SCHEMA my_schema CASCADE;
            ",
        )
        .unwrap();

        assert!(db.schema("my_schema").is_none());
        assert!(
            db.table_by_target(
                TargetName::new("my_table", false).with_schema("my_schema", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous lookup")
            .is_none()
        );
    }

    #[test]
    fn test_drop_schema_cascade_with_multiple_tables() {
        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE TABLE my_schema.table1 (id INT);
            CREATE TABLE my_schema.table2 (name TEXT);
            CREATE TABLE other_table (id INT);
            DROP SCHEMA my_schema CASCADE;
            ",
        )
        .unwrap();

        assert!(db.schema("my_schema").is_none());
        assert!(
            db.table_by_target(
                TargetName::new("table1", false).with_schema("my_schema", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous lookup")
            .is_none()
        );
        assert!(
            db.table_by_target(
                TargetName::new("table2", false).with_schema("my_schema", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous lookup")
            .is_none()
        );
        // Table outside schema should remain
        assert!(
            db.table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_some()
        );
    }

    /// `DROP SCHEMA ... CASCADE` takes the keys from other schemas into the
    /// tables it removes, and leaves the referencing table its columns.
    #[test]
    fn test_drop_schema_cascade_drops_keys_into_removed_tables() {
        use crate::traits::TableLike;

        let db = parse_postgres(
            "
            CREATE SCHEMA my_schema;
            CREATE TABLE my_schema.my_table (id INT PRIMARY KEY);
            CREATE TABLE other_table (id INT PRIMARY KEY, ref_id INT REFERENCES my_schema.my_table(id));
            DROP SCHEMA my_schema CASCADE;
            ",
        )
        .unwrap();
        let other_table = db
            .table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("other_table survives the drop");

        assert_eq!(other_table.columns(&db).expect("column lookup").count(), 2);
        assert_eq!(other_table.foreign_keys(&db).expect("fk lookup").count(), 0);
    }

    #[test]
    fn test_drop_multiple_schemas() {
        let db = parse_postgres(
            "
            CREATE SCHEMA schema_a;
            CREATE SCHEMA schema_b;
            DROP SCHEMA schema_a, schema_b;
            ",
        )
        .unwrap();

        assert!(db.schema("schema_a").is_none());
        assert!(db.schema("schema_b").is_none());
    }

    // ========================
    // ALTER SCHEMA tests
    // ========================

    #[test]
    fn test_alter_schema_rename() {
        let db = parse_postgres(
            "
            CREATE SCHEMA old_name;
            ALTER SCHEMA old_name RENAME TO new_name;
            ",
        )
        .unwrap();

        assert!(db.schema("old_name").is_none());
        let schema = db.schema("new_name").expect("new_name should exist");
        assert_eq!(schema.name(), "new_name");
    }

    #[test]
    fn test_alter_schema_owner_to() {
        let db = parse_postgres(
            "
            CREATE ROLE new_owner;
            CREATE SCHEMA my_schema;
            ALTER SCHEMA my_schema OWNER TO new_owner;
            ",
        )
        .unwrap();

        let schema = db.schema("my_schema").expect("Schema should exist");
        assert_eq!(schema.authorization(), Some("new_owner"));
    }

    #[test]
    fn test_alter_schema_preserves_authorization_on_rename() {
        let db = parse_postgres(
            "
            CREATE ROLE admin;
            CREATE SCHEMA old_name AUTHORIZATION admin;
            ALTER SCHEMA old_name RENAME TO new_name;
            ",
        )
        .unwrap();

        let schema = db.schema("new_name").expect("new_name should exist");
        assert_eq!(schema.authorization(), Some("admin"));
    }

    #[test]
    fn test_alter_nonexistent_schema_fails() {
        let result = parse_postgres("ALTER SCHEMA nonexistent RENAME TO other;");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::errors::Error::AlterSchemaNotFound { schema_name } if schema_name == "nonexistent")
        );
    }

    #[test]
    fn test_alter_schema_if_exists() {
        // Should not error when IF EXISTS is used on non-existent schema
        let db = parse_postgres(
            "
            CREATE SCHEMA existing;
            ALTER SCHEMA IF EXISTS nonexistent RENAME TO other;
            ",
        )
        .unwrap();

        assert!(db.schema("existing").is_some());
    }

    /// Exercises the `impl SchemaLike for &S` blanket forwarding — same
    /// pattern as `traits::column::tests::reference_impl::test_all_methods`.
    mod reference_impl {
        use super::*;

        #[test]
        fn test_all_methods() {
            let db =
                parse_postgres("CREATE ROLE admin; CREATE SCHEMA my_schema AUTHORIZATION admin;")
                    .unwrap();
            let schema = db.schema("my_schema").expect("schema");

            // Explicitly invoke through the &T blanket impl so coverage
            // credits the forwarding bodies in `impl SchemaLike for &S`.
            let s_ref: &<ParserDB as DatabaseLike>::Schema = schema;
            assert_eq!(<&_ as SchemaLike>::name(&s_ref), "my_schema");
            assert!(!<&_ as SchemaLike>::name_is_quoted(&s_ref));
            assert_eq!(<&_ as SchemaLike>::authorization(&s_ref), Some("admin"));
            assert!(!<&_ as SchemaLike>::public_holds_usage(&s_ref));
            assert!(<&_ as SchemaLike>::owner_holds_usage(&s_ref));
        }
    }
}
