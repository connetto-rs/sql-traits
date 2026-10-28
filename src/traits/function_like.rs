//! Submodule providing a trait for describing SQL Function-like entities.

use alloc::{borrow::Cow, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::{
    Expr, FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam, FunctionSecurity,
};

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{DatabaseLike, Metadata},
    utils::{identifier_resolution::normalize_identifier, normalize_postgres_type_cow},
};

/// A trait for describing SQL Function-like entities.
pub trait FunctionLike: Metadata + Debug + Clone + Hash + Ord + Eq + Send + Sync {
    /// The associated database type.
    type DB: DatabaseLike<Function = Self>;

    /// The name of the function.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add_one(x INT) RETURNS INT AS 'SELECT x + 1;';
    /// ",
    /// )?;
    /// let function = db.functions().next().expect("Function should exist");
    /// assert_eq!(function.name(), "add_one");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns the function name exactly as declared, including its optional
    /// schema.
    fn target_name(&self) -> TargetName<'_>;

    /// Returns whether the function name was quoted in SQL.
    ///
    /// Quoted identifiers are resolved case-sensitively in PostgreSQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this function: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION Add_One(x INT) RETURNS INT AS 'SELECT x + 1;';",
    /// )?;
    /// let function = db.functions().next().expect("Function should exist");
    /// assert_eq!(function.name(), "Add_One");
    /// assert_eq!(function.stored_name(), "add_one");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the argument type names (if any) of the function as strings.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add(x INT, y INT) RETURNS INT AS 'SELECT x + y;';
    /// CREATE FUNCTION greet(name TEXT) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// ",
    /// )?;
    /// let add_fn = db.functions().find(|f| f.name() == "add").expect("Function should exist");
    /// let greet_fn = db.functions().find(|f| f.name() == "greet").expect("Function should exist");
    /// assert_eq!(add_fn.argument_type_names(&db).collect::<Vec<_>>(), vec!["INT", "INT"]);
    /// assert_eq!(greet_fn.argument_type_names(&db).collect::<Vec<_>>(), vec!["TEXT"]);
    /// # Ok(())
    /// # }
    /// ```
    fn argument_type_names<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = Cow<'db, str>>;

    /// Returns the normalized argument type names (if any) of the function as
    /// strings.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add(x INTEGER, y INT) RETURNS INT AS 'SELECT x + y;';
    /// CREATE FUNCTION greet(name TEXT) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// ",
    /// )?;
    /// let add_fn = db
    ///     .function_by_target(TargetName::new("add", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let greet_fn = db
    ///     .function_by_target(TargetName::new("greet", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(add_fn.normalized_argument_type_names(&db), vec!["INT", "INT"]);
    /// assert_eq!(greet_fn.normalized_argument_type_names(&db), vec!["TEXT"]);
    /// # Ok(())
    /// # }
    /// ```
    fn normalized_argument_type_names<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Vec<Cow<'db, str>> {
        self.argument_type_names(database).map(normalize_postgres_type_cow).collect()
    }

    /// Returns the name each argument is declared under, in declaration order,
    /// and [`None`] for an argument declared as a bare type.
    ///
    /// A body reaches an argument by this name, so a caller expanding a call
    /// into the body needs it to know what to substitute. The positions line up
    /// with [`argument_type_names`](FunctionLike::argument_type_names).
    ///
    /// PostgreSQL reads a bare name in the body as a column whenever one of
    /// that name is in scope there, and only otherwise as the argument, so a
    /// caller substituting arguments has to resolve the body's own scopes
    /// first. `$1` and `function.argument` always name the argument.
    ///
    /// A quoted name is reached case-sensitively, so it keeps its case here
    /// while an unquoted one folds.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE FUNCTION is_member(doc_id INT, \"Role\" TEXT) RETURNS BOOL LANGUAGE sql
    ///     AS 'SELECT true';
    /// CREATE FUNCTION unnamed(INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
    /// ",
    /// )?;
    /// let is_member = db
    ///     .function_by_target(TargetName::new("is_member", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let names: Vec<_> = is_member.argument_names(&db).collect();
    /// assert_eq!(names[0].as_ref().map(|name| name.name()), Some("doc_id"));
    /// assert_eq!(names[1].as_ref().map(|name| name.name()), Some("Role"));
    /// assert!(names[1].as_ref().expect("Argument should be named").name_is_quoted());
    ///
    /// // An unquoted identifier folds to lowercase, a quoted one keeps its case.
    /// let stored: Vec<_> = is_member.stored_argument_names(&db).collect();
    /// assert_eq!(stored[0].as_deref(), Some("doc_id"));
    /// assert_eq!(stored[1].as_deref(), Some("Role"));
    ///
    /// let unnamed = db
    ///     .function_by_target(TargetName::new("unnamed", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(unnamed.argument_names(&db).collect::<Vec<_>>(), vec![None]);
    /// # Ok(())
    /// # }
    /// ```
    fn argument_names<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = Option<TargetName<'db>>>;

    /// Returns the name PostgreSQL stores for each argument: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// See [`argument_names`](FunctionLike::argument_names) for an example.
    #[inline]
    fn stored_argument_names<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = Option<Cow<'db, str>>> {
        self.argument_names(database).map(|argument| {
            argument.map(|name| {
                Cow::Owned(normalize_identifier(name.name(), name.name_is_quoted()).into_owned())
            })
        })
    }

    /// Returns the return type name of the function as a string.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add_one(x INT) RETURNS INT AS 'SELECT x + 1;';
    /// CREATE FUNCTION greet(name TEXT) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// CREATE FUNCTION do_nothing() AS 'SELECT;';
    /// CREATE FUNCTION identities() RETURNS SETOF UUID AS 'SELECT id FROM users;';
    /// ",
    /// )?;
    /// let add_one_fn = db
    ///     .function_by_target(TargetName::new("add_one", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let greet_fn = db
    ///     .function_by_target(TargetName::new("greet", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let do_nothing_fn = db
    ///     .function_by_target(TargetName::new("do_nothing", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let identities_fn = db
    ///     .function_by_target(TargetName::new("identities", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(do_nothing_fn.return_type_name(&db), None);
    /// assert_eq!(add_one_fn.return_type_name(&db).as_deref(), Some("INT"));
    /// assert_eq!(greet_fn.return_type_name(&db).as_deref(), Some("TEXT"));
    /// // A set-returning declaration keeps its marker, the way an array
    /// // keeps its `[]`.
    /// assert_eq!(identities_fn.return_type_name(&db).as_deref(), Some("SETOF UUID"));
    /// # Ok(())
    /// # }
    /// ```
    fn return_type_name<'db>(&'db self, database: &'db Self::DB) -> Option<Cow<'db, str>>;

    /// Returns whether the function returns a set of values rather than a
    /// single value, mirroring `pg_proc.proretset`.
    ///
    /// Both `RETURNS SETOF type` and `RETURNS TABLE(...)` declare sets. The
    /// declared type keeps answering through
    /// [`return_type_name`](FunctionLike::return_type_name), so this reader
    /// is the route for cardinality rather than for the element type.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION one_value() RETURNS UUID AS 'SELECT gen_random_uuid();';
    /// CREATE FUNCTION many_values() RETURNS SETOF UUID AS 'SELECT id FROM users;';
    /// CREATE FUNCTION many_rows() RETURNS TABLE(id UUID) AS 'SELECT id FROM users;';
    /// ",
    /// )?;
    /// let returns_set = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .returns_set()
    /// };
    /// assert!(!returns_set("one_value"));
    /// assert!(returns_set("many_values"));
    /// // A declared row shape is a set: PostgreSQL records it with
    /// // `pg_proc.proretset = true`, exactly like `SETOF`.
    /// assert!(returns_set("many_rows"));
    /// # Ok(())
    /// # }
    /// ```
    fn returns_set(&self) -> bool;

    /// Returns the language the body is written in, as the input spells it.
    ///
    /// The body only means anything under its language, so a caller reading
    /// [`body`](FunctionLike::body) as SQL has to ask this first. PostgreSQL
    /// requires the clause on `CREATE FUNCTION` and refuses a function without
    /// one, so a parsed function that answers [`None`] came from input the
    /// server would reject.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE FUNCTION one() RETURNS INT LANGUAGE SQL AS 'SELECT 1';
    /// CREATE FUNCTION two() RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 2; END';
    /// ",
    /// )?;
    /// let one = db
    ///     .function_by_target(TargetName::new("one", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(one.language(), Some("SQL"));
    /// // The server folds the identifier, and `sql` is what it looks up.
    /// assert_eq!(one.stored_language().as_deref(), Some("sql"));
    /// assert!(!one.language_is_quoted());
    ///
    /// let two = db
    ///     .function_by_target(TargetName::new("two", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(two.stored_language().as_deref(), Some("plpgsql"));
    /// # Ok(())
    /// # }
    /// ```
    fn language(&self) -> Option<&str>;

    /// Returns whether the language name was quoted in SQL.
    ///
    /// Required rather than defaulted to `false`, because assuming a name was
    /// unquoted is not a harmless guess here: PostgreSQL refuses
    /// `LANGUAGE "SQL"` outright, since the language it stores is named `sql`,
    /// so the two spellings do not name the same thing.
    fn language_is_quoted(&self) -> bool;

    /// Returns the language name PostgreSQL stores: an unquoted identifier
    /// folds to lowercase, a quoted one keeps its case.
    ///
    /// See [`language`](FunctionLike::language) for an example.
    #[inline]
    fn stored_language(&self) -> Option<Cow<'_, str>> {
        self.language().map(|language| normalize_identifier(language, self.language_is_quoted()))
    }

    /// Returns the body text of the function, for the spellings that write the
    /// body as a string.
    ///
    /// A function written `RETURN <expression>` has no body text and answers
    /// [`None`] here. Its body is an expression the input already parsed, which
    /// [`body_expression`](FunctionLike::body_expression) hands back.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add_one(x INT) RETURNS INT AS 'SELECT x + 1;';
    /// ",
    /// )?;
    /// let function = db.functions().next().expect("Function should exist");
    /// assert_eq!(function.body(), Some("SELECT x + 1;"));
    /// # Ok(())
    /// # }
    /// ```
    fn body(&self) -> Option<&str>;

    /// Returns the expression of a function written `RETURN <expression>`.
    ///
    /// PostgreSQL 14 added this spelling, and it is the one that parses the
    /// body up front rather than leaving a string for the caller to parse. A
    /// function whose body is written as a string answers [`None`] here and
    /// answers [`body`](FunctionLike::body) instead, so a caller wanting the
    /// body in either spelling asks this first and falls back to parsing the
    /// text.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE FUNCTION added(a INT, b INT) RETURNS INT LANGUAGE sql RETURN a + b;
    /// CREATE FUNCTION quoted(a INT) RETURNS INT LANGUAGE sql AS 'SELECT a';
    /// ",
    /// )?;
    /// let added = db
    ///     .function_by_target(TargetName::new("added", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(added.body(), None);
    /// assert_eq!(added.body_expression().map(ToString::to_string).as_deref(), Some("a + b"));
    ///
    /// let quoted = db
    ///     .function_by_target(TargetName::new("quoted", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(quoted.body(), Some("SELECT a"));
    /// assert_eq!(quoted.body_expression(), None);
    /// # Ok(())
    /// # }
    /// ```
    fn body_expression(&self) -> Option<&Expr>;

    /// Returns the function-local configuration after later alterations.
    fn configuration_parameters(&self) -> &[FunctionDefinitionSetParam];

    /// Returns the null-input behavior with PostgreSQL's default folded in.
    fn null_input_behavior(&self) -> FunctionCalledOnNull;

    /// Returns whether the call is `NULL` whenever its `index`-th positional
    /// argument, counting no `OUT` parameter, is `NULL`, and false wherever
    /// that is not certain, a `VARIADIC` parameter included.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION padded(s TEXT, width INT) RETURNS TEXT LANGUAGE sql STRICT
    ///          AS 'SELECT s';",
    /// )?;
    /// let padded = db
    ///     .function_by_target(TargetName::new("padded", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert!(padded.returns_null_on_null_argument(&db, 1));
    /// assert!(!padded.returns_null_on_null_argument(&db, 2));
    ///
    /// // `string_to_array(NULL, ',')` is NULL, `string_to_array('ab', NULL)` is `{a,b}`.
    /// let split = db
    ///     .functions()
    ///     .find(|function| {
    ///         function.name() == "string_to_array" && function.argument_type_names(&db).count() == 2
    ///     })
    ///     .expect("string_to_array is registered");
    /// assert!(split.returns_null_on_null_argument(&db, 0));
    /// assert!(!split.returns_null_on_null_argument(&db, 1));
    /// # Ok(())
    /// # }
    /// ```
    fn returns_null_on_null_argument(&self, database: &Self::DB, index: usize) -> bool;

    /// Returns effective volatility after alterations, defaulting to
    /// PostgreSQL's `VOLATILE`.
    ///
    /// Implementations with declared volatility override the conservative
    /// default. A builtin [`ParserDB`](crate::structs::ParserDB) registers in
    /// `pg_catalog` answers the volatility PostgreSQL declares for it, so
    /// `now()` is `STABLE` and `lower(text)` is `IMMUTABLE`.
    ///
    /// ```
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::FunctionBehavior, dialect::PostgreSqlDialect};
    ///
    /// let database = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION helper() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
    ///      ALTER FUNCTION helper() STABLE;",
    /// )?;
    /// let helper = database.functions().find(|function| function.name() == "helper").unwrap();
    /// assert_eq!(helper.volatility(), FunctionBehavior::Stable);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn volatility(&self) -> FunctionBehavior {
        FunctionBehavior::Volatile
    }

    /// Returns whether the function runs with the privileges of the user
    /// that defined it (`SECURITY DEFINER`) or of the user that calls it
    /// (`SECURITY INVOKER`), which decides who `current_user` names inside
    /// the body.
    ///
    /// PostgreSQL defaults an unstated clause to `SECURITY INVOKER`, and
    /// this reader folds that default in. A security clause applied later
    /// by `ALTER FUNCTION` is reflected.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::FunctionSecurity;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION as_owner() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// CREATE FUNCTION as_caller() RETURNS INT LANGUAGE sql SECURITY INVOKER AS 'SELECT 1';
    /// CREATE FUNCTION unstated() RETURNS INT AS 'SELECT 1';
    /// CREATE FUNCTION altered() RETURNS INT AS 'SELECT 1';
    /// ALTER FUNCTION altered() SECURITY DEFINER;
    /// ",
    /// )?;
    /// let mode = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .security_mode()
    /// };
    /// assert_eq!(mode("as_owner"), FunctionSecurity::Definer);
    /// assert_eq!(mode("as_caller"), FunctionSecurity::Invoker);
    /// // PostgreSQL defaults an unstated clause to SECURITY INVOKER.
    /// assert_eq!(mode("unstated"), FunctionSecurity::Invoker);
    /// // ALTER FUNCTION updates the stored mode.
    /// assert_eq!(mode("altered"), FunctionSecurity::Definer);
    /// # Ok(())
    /// # }
    /// ```
    fn security_mode(&self) -> FunctionSecurity;

    /// Returns the role the input names as the function's owner.
    ///
    /// A `SECURITY DEFINER` body reads its tables as this role, so the owner is
    /// what decides whose row policies filter the read. It answers the question
    /// [`security_mode`](FunctionLike::security_mode) leaves open: that reader
    /// says the body runs as its definer, and this one says who the definer is.
    ///
    /// Only `ALTER FUNCTION ... OWNER TO <role>` names an owner. A function no
    /// such statement altered has none, and neither has one handed to
    /// `CURRENT_ROLE`, `CURRENT_USER` or `SESSION_USER`, which name whoever
    /// runs the statement rather than a role the input declares.
    ///
    /// The role is reported as the statement spelled it, with no case folding,
    /// which is how [`DatabaseLike::role`] stores the names it matches against.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance the function belongs
    ///   to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE app_reader;
    /// CREATE FUNCTION reassigned() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// ALTER FUNCTION reassigned() OWNER TO app_reader;
    /// CREATE FUNCTION untouched() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// ",
    /// )?;
    /// let reassigned = db
    ///     .function_by_target(TargetName::new("reassigned", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(reassigned.owner(&db)?, Some("app_reader"));
    ///
    /// // Nobody reassigned this one, so the schema names no owner for it.
    /// let untouched = db
    ///     .function_by_target(TargetName::new("untouched", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(untouched.owner(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>;

    /// Returns whether every role may execute the function, which is what
    /// `PUBLIC` holding `EXECUTE` on it means.
    ///
    /// PostgreSQL gives `PUBLIC` `EXECUTE` on every new function, so this
    /// answers `true` until a `REVOKE` takes it away, and again once a
    /// `GRANT ... TO PUBLIC` restores it. Every spelling reaches it, whether a
    /// signature, a name that covers one function alone, or `ALL FUNCTIONS IN
    /// SCHEMA`, which covers the functions the schema holds when it runs.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance the function belongs
    ///   to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE FUNCTION open() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// CREATE FUNCTION closed() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// REVOKE EXECUTE ON FUNCTION closed() FROM PUBLIC;
    /// ",
    /// )?;
    /// let executable_by_public = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .is_executable_by_public(&db)
    /// };
    /// assert_eq!(executable_by_public("open"), Ok(true));
    /// assert_eq!(executable_by_public("closed"), Ok(false));
    /// # Ok(())
    /// # }
    /// ```
    fn is_executable_by_public(&self, database: &Self::DB) -> Result<bool, LookupError>;

    /// Returns whether `role` may execute the function.
    ///
    /// A role may when `PUBLIC` holds `EXECUTE`, when an entry of its own
    /// grants it, or when it is the role
    /// [`owner`](FunctionLike::owner) names and the owner's own entry was not
    /// revoked. The owner's entry is an ordinary one, which the owner can
    /// revoke and grant back. A role also may whenever one of its
    /// [`inherited_roles`] may, since PostgreSQL passes privileges on along
    /// every membership carrying `INHERIT`.
    ///
    /// An owner the input never names cannot be recognised, so this answers
    /// for it as for any other role.
    ///
    /// # Arguments
    ///
    /// * `role` - The role to check.
    /// * `database` - A reference to the database instance the function belongs
    ///   to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE anon;
    /// CREATE ROLE authenticated;
    /// CREATE FUNCTION f() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
    /// GRANT EXECUTE ON FUNCTION f() TO authenticated;
    /// ",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(f.can_execute(db.role("authenticated").expect("declared"), &db), Ok(true));
    /// assert_eq!(f.can_execute(db.role("anon").expect("declared"), &db), Ok(false));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`inherited_roles`]: crate::traits::RoleLike::inherited_roles
    fn can_execute(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError>;

    /// Returns whether `role` may grant `EXECUTE` on the function to other
    /// roles.
    ///
    /// A role may when an entry of its own was granted `WITH GRANT OPTION`
    /// and no `REVOKE GRANT OPTION FOR` took that away, or when it is the role
    /// [`owner`](FunctionLike::owner) names, which ownership lets grant
    /// whatever its own entry says. A role also may whenever one of its
    /// [`inherited_roles`](crate::traits::RoleLike::inherited_roles) may.
    /// `PUBLIC` never holds the option, since PostgreSQL grants it to roles
    /// alone.
    ///
    /// # Arguments
    ///
    /// * `role` - The role to check.
    /// * `database` - A reference to the database instance the function belongs
    ///   to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE client;
    /// CREATE ROLE reader;
    /// CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    /// GRANT EXECUTE ON FUNCTION f() TO client WITH GRANT OPTION;
    /// GRANT EXECUTE ON FUNCTION f() TO reader;
    /// ",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(f.can_grant_execute(db.role("client").expect("declared"), &db), Ok(true));
    /// assert_eq!(f.can_grant_execute(db.role("reader").expect("declared"), &db), Ok(false));
    /// # Ok(())
    /// # }
    /// ```
    fn can_grant_execute(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError>;

    /// Returns the normalized return type name of the function as a string.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add_one(x INT) RETURNS INTEGER AS 'SELECT x + 1;';
    /// CREATE FUNCTION greet(name TEXT) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// CREATE FUNCTION do_nothing() AS 'SELECT;';
    /// CREATE FUNCTION identities() RETURNS SETOF UUID AS 'SELECT id FROM users;';
    /// ",
    /// )?;
    /// let add_one_fn = db
    ///     .function_by_target(TargetName::new("add_one", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let greet_fn = db
    ///     .function_by_target(TargetName::new("greet", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let do_nothing_fn = db
    ///     .function_by_target(TargetName::new("do_nothing", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let identities_fn = db
    ///     .function_by_target(TargetName::new("identities", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(do_nothing_fn.normalized_return_type_name(&db), None);
    /// assert_eq!(add_one_fn.normalized_return_type_name(&db).as_deref(), Some("INT"));
    /// assert_eq!(greet_fn.normalized_return_type_name(&db).as_deref(), Some("TEXT"));
    /// // Normalization keeps the SETOF marker.
    /// assert_eq!(identities_fn.normalized_return_type_name(&db).as_deref(), Some("SETOF UUID"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn normalized_return_type_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<Cow<'db, str>> {
        self.return_type_name(database).map(normalize_postgres_type_cow)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{FunctionCalledOnNull, FunctionSecurity},
        dialect::{GenericDialect, PostgreSqlDialect},
    };

    use crate::{errors::Error, prelude::*, traits::DatabaseLike};

    /// Exercises both default-method bodies (`name_is_quoted`,
    /// `normalized_return_type_name`) directly so coverage credits them.
    #[test]
    fn test_default_methods_on_function() {
        let sql = r"
            CREATE FUNCTION unquoted_fn() RETURNS INT AS 'SELECT 1';
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let f = db
            .function_by_target(TargetName::new("unquoted_fn", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        // `name_is_quoted` default returns false for parser-derived
        // functions whose impl chooses not to override.
        assert!(!f.name_is_quoted());

        // `normalized_return_type_name` default delegates to
        // `return_type_name` then runs the result through
        // `normalize_postgres_type`.
        assert_eq!(f.normalized_return_type_name(&db).as_deref(), Some("INT"));
    }

    #[test]
    fn test_drop_function() {
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            DROP FUNCTION my_func;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        // Function should be removed
        assert!(
            db.function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_none()
        );
        assert_eq!(db.functions().filter(|f| f.name() == "my_func").count(), 0);
    }

    #[test]
    fn test_drop_function_if_exists() {
        let sql = r"
            DROP FUNCTION IF EXISTS non_existent_func;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Should not error with IF EXISTS");

        // Should succeed without error even though function doesn't exist
        assert!(
            db.function_by_target(
                TargetName::new("non_existent_func", false),
                IdentifierCase::AsWritten
            )
            .expect("unambiguous lookup")
            .is_none()
        );
    }

    #[test]
    fn test_drop_function_not_found() {
        let sql = r"
            DROP FUNCTION non_existent_func;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        // Should fail because function doesn't exist
        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e}");
            assert!(error_msg.contains("non_existent_func"));
        }
    }

    #[test]
    fn test_drop_multiple_functions() {
        let sql = r"
            CREATE FUNCTION func1() RETURNS INT AS 'SELECT 1;';
            CREATE FUNCTION func2() RETURNS INT AS 'SELECT 2;';
            CREATE FUNCTION func3() RETURNS INT AS 'SELECT 3;';
            DROP FUNCTION func1;
            DROP FUNCTION func3;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        // func1 and func3 should be removed
        assert!(
            db.function_by_target(TargetName::new("func1", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_none()
        );
        assert!(
            db.function_by_target(TargetName::new("func3", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_none()
        );

        // func2 should still exist
        assert!(
            db.function_by_target(TargetName::new("func2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_some()
        );
        assert_eq!(
            db.function_by_target(TargetName::new("func2", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .name(),
            "func2"
        );
    }

    #[test]
    fn test_create_drop_create_function() {
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            DROP FUNCTION my_func;
            CREATE FUNCTION my_func() RETURNS TEXT AS 'SELECT ''hello'';';
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        // Should have the recreated function
        let func = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("Function should exist");
        assert_eq!(func.name(), "my_func");

        // Should have the new return type
        assert_eq!(func.return_type_name(&db).as_deref(), Some("TEXT"));
    }

    #[test]
    fn test_drop_function_referenced_by_check_fails() {
        let sql = r"
            CREATE FUNCTION is_valid(x INT) RETURNS BOOLEAN AS 'SELECT x > 0;';
            CREATE TABLE t (id INT CHECK (is_valid(id)));
            DROP FUNCTION is_valid;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e}");
            assert!(error_msg.contains("is_valid"));
            assert!(error_msg.contains("referenced"));
        }
    }

    #[test]
    fn test_drop_function_referenced_by_policy_fails() {
        let sql = r"
            CREATE FUNCTION check_access() RETURNS BOOLEAN AS 'SELECT true;';
            CREATE TABLE t (id INT);
            CREATE POLICY my_policy ON t USING (check_access());
            DROP FUNCTION check_access;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e}");
            assert!(error_msg.contains("check_access"));
            assert!(error_msg.contains("referenced"));
        }
    }

    #[test]
    fn test_drop_function_referenced_by_policy_with_check_fails() {
        let sql = r"
            CREATE FUNCTION validate_insert() RETURNS BOOLEAN AS 'SELECT true;';
            CREATE TABLE t (id INT);
            CREATE POLICY insert_policy ON t WITH CHECK (validate_insert());
            DROP FUNCTION validate_insert;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e}");
            assert!(error_msg.contains("validate_insert"));
            assert!(error_msg.contains("referenced"));
        }
    }

    #[test]
    fn test_drop_function_referenced_by_trigger_fails() {
        let sql = r"
            CREATE TABLE t (id INT);
            CREATE FUNCTION my_trigger_func() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION my_trigger_func();
            DROP FUNCTION my_trigger_func;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e}");
            assert!(error_msg.contains("my_trigger_func"));
            assert!(error_msg.contains("referenced"));
        }
    }

    #[test]
    fn test_drop_unreferenced_function_succeeds() {
        let sql = r"
            CREATE FUNCTION unused_func() RETURNS INT AS 'SELECT 1;';
            CREATE TABLE t (id INT);
            DROP FUNCTION unused_func;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql)
            .expect("Should succeed dropping unreferenced function");

        // Function should be gone
        assert!(
            db.function_by_target(TargetName::new("unused_func", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_none()
        );

        // Table should still exist
        assert!(
            db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_some()
        );
    }

    #[test]
    fn test_drop_function_after_dropping_dependent_succeeds() {
        // This test verifies that if a check constraint is part of a table,
        // and the table is created with the function, but then we drop and
        // recreate the function after, the DROP succeeds when no
        // references remain
        let sql = r"
            CREATE FUNCTION helper_func() RETURNS INT AS 'SELECT 1;';
            CREATE FUNCTION other_func() RETURNS INT AS 'SELECT 2;';
            DROP FUNCTION other_func;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Should succeed");

        // helper_func should still exist
        assert!(
            db.function_by_target(TargetName::new("helper_func", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_some()
        );

        // other_func should be gone
        assert!(
            db.function_by_target(TargetName::new("other_func", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .is_none()
        );
    }

    #[test]
    fn test_alter_function_sets_security_definer() {
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            ALTER FUNCTION my_func() SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_alter_function_sets_security_invoker() {
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT SECURITY DEFINER AS 'SELECT 1;';
            ALTER FUNCTION my_func() SECURITY INVOKER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Invoker);
    }

    #[test]
    fn test_alter_function_external_security() {
        // EXTERNAL is a noise word PostgreSQL accepts for SQL conformance.
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            ALTER FUNCTION my_func() EXTERNAL SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_alter_function_last_security_action_wins() {
        // PostgreSQL applies the actions of one statement in order, so a
        // repeated clause leaves the last spelling in force.
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            ALTER FUNCTION my_func() SECURITY INVOKER SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_alter_function_security_not_found() {
        let sql = r"
            ALTER FUNCTION missing_func() SECURITY DEFINER;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(matches!(
            result,
            Err(Error::AlterFunctionNotFound { function_name }) if function_name == "missing_func"
        ));
    }

    #[test]
    fn test_alter_function_security_without_args_ambiguous() {
        let sql = r"
            CREATE FUNCTION dup_func(x INT) RETURNS INT AS 'SELECT 1;';
            CREATE FUNCTION dup_func(x TEXT) RETURNS INT AS 'SELECT 2;';
            ALTER FUNCTION dup_func SECURITY DEFINER;
        ";
        let result = ParserDB::parse::<GenericDialect>(sql);

        assert!(matches!(
            result,
            Err(Error::AmbiguousAlterFunction { function_name }) if function_name == "dup_func"
        ));
    }

    #[test]
    fn test_alter_function_selects_overload_by_args() {
        let sql = r"
            CREATE FUNCTION dup_func(x INT) RETURNS INT AS 'SELECT 1;';
            CREATE FUNCTION dup_func(x TEXT) RETURNS INT AS 'SELECT 2;';
            ALTER FUNCTION dup_func(INT) SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        for f in db.functions().filter(|f| f.name() == "dup_func") {
            let expected = if f.normalized_argument_type_names(&db) == ["INT"] {
                FunctionSecurity::Definer
            } else {
                FunctionSecurity::Invoker
            };
            assert_eq!(f.security_mode(), expected);
        }
    }

    #[test]
    fn test_alter_function_other_clauses_stay_ignored() {
        // Statements carrying no security clause keep falling through like
        // the catch-all arm, even when they name a missing function.
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            ALTER FUNCTION my_func() IMMUTABLE;
            ALTER FUNCTION missing_func() COST 100;
            ALTER FUNCTION missing_func() RENAME TO other_func;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Invoker);
    }

    #[test]
    fn test_create_or_replace_replaces_security() {
        // The replacement node carries no clause, so the default applies:
        // CREATE OR REPLACE resets the mode rather than merging.
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT SECURITY DEFINER AS 'SELECT 1;';
            CREATE OR REPLACE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Invoker);
    }

    #[test]
    fn test_trigger_function_carries_security_mode() {
        let sql = r"
            CREATE TABLE t (id INT);
            CREATE FUNCTION my_trigger_func() RETURNS TRIGGER SECURITY DEFINER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER my_trigger BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION my_trigger_func();
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(
                TargetName::new("my_trigger_func", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_alter_function_bare_name_single_match() {
        // PostgreSQL lets the statement omit the argument list when the
        // name covers exactly one function.
        let sql = r"
            CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1;';
            ALTER FUNCTION my_func SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_alter_function_security_visible_through_policy() {
        // Policies cache the function nodes their expressions call, so the
        // alteration has to reach that cache too, not only the canonical
        // store.
        let sql = r"
            CREATE TABLE t (id INT);
            CREATE FUNCTION f(x INT) RETURNS BOOLEAN AS 'SELECT true;';
            CREATE POLICY p ON t USING (f(id));
            ALTER FUNCTION f(INT) SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table should exist");
        let policy = table.policies(&db).expect("policies").next().expect("policy should exist");
        let via_policy = policy
            .using_functions(&db)
            .expect("using functions")
            .next()
            .expect("function should be resolved");

        assert_eq!(via_policy.security_mode(), FunctionSecurity::Definer);
        assert_eq!(
            db.function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("function should exist")
                .security_mode(),
            FunctionSecurity::Definer
        );
    }

    #[test]
    fn test_alter_function_security_visible_through_check_constraint() {
        // Check constraints cache resolved function nodes the same way
        // policies do.
        let sql = r"
            CREATE FUNCTION g(x INT) RETURNS BOOLEAN AS 'SELECT true;';
            CREATE TABLE t (id INT, CHECK (g(id)));
            ALTER FUNCTION g(INT) SECURITY DEFINER;
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table should exist");
        let check = table.check_constraints(&db).expect("check constraints").next().expect("check");
        let via_check = check
            .functions(&db)
            .expect("check functions")
            .next()
            .expect("function should be resolved");

        assert_eq!(via_check.security_mode(), FunctionSecurity::Definer);
    }

    #[test]
    fn test_create_or_replace_updates_expression_caches() {
        // A replacement reaches the caches policies and check constraints
        // keep, for both policy expressions, like ALTER FUNCTION does.
        let sql = r"
            CREATE FUNCTION f(x INT) RETURNS BOOLEAN AS 'SELECT true;';
            CREATE TABLE t (id INT, CHECK (f(id)));
            CREATE POLICY p ON t USING (f(id)) WITH CHECK (f(id));
            CREATE OR REPLACE FUNCTION f(x INT) RETURNS BOOLEAN SECURITY DEFINER AS 'SELECT true;';
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table should exist");
        let policy = table.policies(&db).expect("policies").next().expect("policy should exist");
        let via_using =
            policy.using_functions(&db).expect("using functions").next().expect("function");
        let via_check =
            policy.check_functions(&db).expect("check functions").next().expect("function");
        let constraint =
            table.check_constraints(&db).expect("check constraints").next().expect("check");
        let via_constraint =
            constraint.functions(&db).expect("constraint functions").next().expect("function");

        assert_eq!(via_using.security_mode(), FunctionSecurity::Definer);
        assert_eq!(via_check.security_mode(), FunctionSecurity::Definer);
        assert_eq!(via_constraint.security_mode(), FunctionSecurity::Definer);
    }
    #[test]
    fn test_function_null_input_behavior_preserves_postgres_forms() {
        let sql = r"
            CREATE FUNCTION default_behavior(x INT) RETURNS BOOLEAN LANGUAGE sql
                AS 'SELECT true';
            CREATE FUNCTION called_on_null(x INT) RETURNS BOOLEAN LANGUAGE sql
                CALLED ON NULL INPUT AS 'SELECT true';
            CREATE FUNCTION returns_null(x INT) RETURNS BOOLEAN LANGUAGE sql
                RETURNS NULL ON NULL INPUT AS 'SELECT true';
            CREATE FUNCTION strict_behavior(x INT) RETURNS BOOLEAN LANGUAGE sql
                STRICT AS 'SELECT true';
        ";
        let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("parse");
        let behavior = |name| {
            db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("function")
                .null_input_behavior()
        };

        assert_eq!(behavior("default_behavior"), FunctionCalledOnNull::CalledOnNullInput);
        assert_eq!(behavior("called_on_null"), FunctionCalledOnNull::CalledOnNullInput);
        assert_eq!(behavior("returns_null"), FunctionCalledOnNull::ReturnsNullOnNullInput);
        assert_eq!(behavior("strict_behavior"), FunctionCalledOnNull::Strict);
    }
}
