//! Implementation of the `RoleLike` trait for sqlparser's `CreateRole` type.
#![allow(
    clippy::expect_used,
    reason = "sqlparser only produces a CreateRole after parsing at least one role name"
)]

use sqlparser::ast::{CreateRole, Expr, UnaryOperator, Value, ValueWithSpan};

use crate::{
    structs::ParserDB,
    traits::{DatabaseLike, Metadata, PolicyLike, RoleLike},
    utils::{
        identifier_resolution::{identifiers_match, is_public_pseudo_role, normalize_identifier},
        last_str,
        object_name::object_name_last_part,
    },
};

impl Metadata for CreateRole {
    type Meta = ();
}

impl RoleLike for CreateRole {
    type DB = ParserDB;

    fn name(&self) -> &str {
        last_str(self.names.first().expect("CREATE ROLE must have a name"))
    }

    fn name_is_quoted(&self) -> bool {
        self.names.first().and_then(object_name_last_part).is_some_and(|(_, quoted)| quoted)
    }

    fn is_superuser(&self) -> bool {
        self.superuser == Some(true)
    }

    fn can_create_db(&self) -> bool {
        self.create_db == Some(true)
    }

    fn can_create_role(&self) -> bool {
        self.create_role == Some(true)
    }

    fn inherits(&self) -> bool {
        // Default is INHERIT in PostgreSQL, so we check if explicitly set to
        // false
        self.inherit != Some(false)
    }

    fn can_login(&self) -> bool {
        self.login == Some(true)
    }

    fn can_bypass_rls(&self) -> bool {
        self.bypassrls == Some(true)
    }

    fn is_replication(&self) -> bool {
        self.replication == Some(true)
    }

    fn connection_limit(&self) -> Option<i32> {
        // PostgreSQL stores `-1` for a role without a limit.
        self.connection_limit.as_ref().and_then(signed_integer).filter(|&limit| limit != -1)
    }

    fn member_of<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Role> {
        self.in_role.iter().filter_map(move |role_ident| {
            let stored_name =
                normalize_identifier(&role_ident.value, role_ident.quote_style.is_some());
            database.role(stored_name.as_ref())
        })
    }

    fn policies<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Policy> {
        database.policies().filter(move |policy| {
            policy.roles(database).any(|owner| owner_matches_role(owner, self))
        })
    }
}

/// Returns whether an owner identifier names the role.
fn owner_matches_role(owner: &sqlparser::ast::Owner, role: &CreateRole) -> bool {
    match owner {
        sqlparser::ast::Owner::Ident(owner_ident)
            if !is_public_pseudo_role(&owner_ident.value, owner_ident.quote_style.is_some()) =>
        {
            role.names.iter().any(|role_name| {
                object_name_last_part(role_name).is_some_and(|(role_name, role_quoted)| {
                    identifiers_match(
                        role_name,
                        role_quoted,
                        &owner_ident.value,
                        owner_ident.quote_style.is_some(),
                    )
                })
            })
        }
        sqlparser::ast::Owner::Ident(_)
        | sqlparser::ast::Owner::CurrentUser
        | sqlparser::ast::Owner::CurrentRole
        | sqlparser::ast::Owner::SessionUser => false,
    }
}

/// Reads an integer literal with an optional sign, the shape PostgreSQL takes
/// after `CONNECTION LIMIT`.
fn signed_integer(expr: &Expr) -> Option<i32> {
    match expr {
        Expr::Value(ValueWithSpan { value: Value::Number(digits, _), .. }) => digits.parse().ok(),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => signed_integer(expr),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => signed_integer(expr)?.checked_neg(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

    use crate::{
        structs::ParserDB,
        traits::{PolicyLike, RoleLike},
    };

    /// Helper to parse SQL using PostgreSQL dialect
    fn parse_postgres(sql: &str) -> ParserDB {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        ParserDB::from_statements(statements, "test".to_string()).unwrap()
    }

    #[test]
    fn test_basic_role() {
        let db = ParserDB::parse::<PostgreSqlDialect>("CREATE ROLE test_role;").unwrap();
        let role = db.role("test_role").unwrap();

        assert_eq!(role.name(), "test_role");
        assert!(!role.is_superuser());
        assert!(!role.can_create_db());
        assert!(!role.can_create_role());
        assert!(role.inherits()); // Default is INHERIT
        assert!(!role.can_login());
        assert!(!role.can_bypass_rls());
        assert!(!role.is_replication());
        assert!(role.connection_limit().is_none());
    }

    #[test]
    fn role_identity_uses_canonical_stored_names() {
        let db = parse_postgres(
            "CREATE ROLE App_Reader;
             CREATE ROLE \"ACTOR\";",
        );

        let unquoted = db.role("app_reader").expect("unquoted role resolves");
        assert_eq!(unquoted.name(), "App_Reader");
        assert!(!unquoted.name_is_quoted());
        assert_eq!(unquoted.stored_name(), "app_reader");
        assert!(db.role("App_Reader").is_none());

        let quoted = db.role("ACTOR").expect("quoted role resolves");
        assert_eq!(quoted.name(), "ACTOR");
        assert!(quoted.name_is_quoted());
        assert_eq!(quoted.stored_name(), "ACTOR");
    }

    #[test]
    fn role_metadata_uses_canonical_sort_order() {
        let db = parse_postgres(
            "CREATE ROLE Zed;
             CREATE ROLE \"alpha\";
             CREATE ROLE \"middle\";",
        );

        for name in ["zed", "alpha", "middle"] {
            let role = db.role(name).expect("role resolves");
            assert!(db.role_metadata(role).is_some(), "{name} metadata is unreachable");
        }
    }

    #[test]
    fn test_role_with_all_options() {
        let db = parse_postgres(
            "CREATE ROLE admin SUPERUSER CREATEDB CREATEROLE LOGIN BYPASSRLS REPLICATION CONNECTION LIMIT 10;",
        );
        let role = db.role("admin").unwrap();

        assert!(role.is_superuser());
        assert!(role.can_create_db());
        assert!(role.can_create_role());
        assert!(role.can_login());
        assert!(role.can_bypass_rls());
        assert!(role.is_replication());
        assert_eq!(role.connection_limit(), Some(10));
    }

    /// PostgreSQL stores `-1` for a role without a limit, so both spellings of
    /// unlimited answer `None`, while `0` admits no connection at all.
    #[test]
    fn connection_limit_reads_the_sign() {
        let db = parse_postgres(
            "CREATE ROLE unlimited CONNECTION LIMIT -1;
             CREATE ROLE plus CONNECTION LIMIT +5;
             CREATE ROLE closed CONNECTION LIMIT 0;",
        );

        assert_eq!(db.role("unlimited").unwrap().connection_limit(), None);
        assert_eq!(db.role("plus").unwrap().connection_limit(), Some(5));
        assert_eq!(db.role("closed").unwrap().connection_limit(), Some(0));
    }

    /// A prepared statement binds the limit only when it runs, so the schema
    /// holds no limit to report.
    #[test]
    fn connection_limit_of_a_placeholder_is_unknown() {
        let db = parse_postgres("CREATE ROLE templated CONNECTION LIMIT $1;");

        assert_eq!(db.role("templated").unwrap().connection_limit(), None);
    }

    #[test]
    fn test_role_noinherit() {
        let db = parse_postgres("CREATE ROLE noinherit_role NOINHERIT;");
        let role = db.role("noinherit_role").unwrap();

        assert!(!role.inherits());
    }

    #[test]
    fn test_role_membership() {
        let db = parse_postgres(
            r#"
            CREATE ROLE Parent_One;
            CREATE ROLE "Parent_Two";
            CREATE ROLE Child_Role IN ROLE Parent_One, "Parent_Two";
        "#,
        );

        let child = db.role("child_role").expect("child resolves");
        let memberships: Vec<_> = child.member_of(&db).collect();
        let stored_names: Vec<_> = memberships.iter().map(|role| role.stored_name()).collect();

        assert_eq!(stored_names, ["parent_one", "Parent_Two"]);
    }

    #[test]
    fn test_role_policies() {
        let db = parse_postgres(
            r#"
            CREATE ROLE My_Role;
            CREATE ROLE "MY_ROLE";
            CREATE ROLE "public";
            CREATE TABLE t1 (id INT);
            CREATE TABLE t2 (id INT);
            CREATE POLICY p1 ON t1 TO My_Role USING (true);
            CREATE POLICY p2 ON t2 TO "MY_ROLE" USING (true);
            CREATE POLICY p3 ON t1 TO PUBLIC USING (true);
            CREATE POLICY p4 ON t2 TO "public" USING (true);
        "#,
        );

        let unquoted = db.role("my_role").expect("unquoted role resolves");
        let unquoted_policies: Vec<_> = unquoted.policies(&db).map(PolicyLike::name).collect();
        assert_eq!(unquoted_policies, ["p1"]);

        let quoted = db.role("MY_ROLE").expect("quoted role resolves");
        let quoted_policies: Vec<_> = quoted.policies(&db).map(PolicyLike::name).collect();
        assert_eq!(quoted_policies, ["p2"]);

        let quoted_public = db.role("public").expect("quoted public role resolves");
        let quoted_public_policies: Vec<_> =
            quoted_public.policies(&db).map(PolicyLike::name).collect();
        assert_eq!(quoted_public_policies, ["p4"]);
    }
}
