//! Submodule providing a trait for describing SQL Function-like entities.

use alloc::{borrow::Cow, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::{
    ArgMode, Expr, Function, FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam,
    FunctionSecurity, Statement, Value,
};

use crate::{
    errors::{HelperSetError, LookupError},
    impls::{atomic_statements, body_calls, body_expression, body_reads, helper_set},
    structs::{Deployment, FunctionBody, FunctionKind, HelperSet, PrivilegeHolder, TargetName},
    traits::{DatabaseLike, Metadata, Relation},
    utils::{
        call_typing::{
            accepts_positional, actuals, bind, check_argument_count, declared_at_call, input_modes,
            written_argument_names,
        },
        identifier_resolution::{SearchPath, normalize_identifier},
        normalize_postgres_type_cow,
    },
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

    /// Returns the mode of each argument, in declaration order, mirroring
    /// `pg_proc.proargmodes`. An argument declared without a mode is
    /// [`ArgMode::In`].
    ///
    /// The positions line up with
    /// [`argument_type_names`](FunctionLike::argument_type_names). A call
    /// passes the `IN`, `INOUT` and `VARIADIC` arguments, and an `OUT`
    /// argument describes the result.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::ArgMode, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION f(a INT, OUT b INT, VARIADIC c INT[]) LANGUAGE sql AS 'SELECT 1';",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(
    ///     f.argument_modes().collect::<Vec<_>>(),
    ///     [ArgMode::In, ArgMode::Out, ArgMode::Variadic]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn argument_modes(&self) -> impl Iterator<Item = ArgMode>;

    /// Returns how many of the trailing input arguments carry a default,
    /// mirroring `pg_proc.pronargdefaults`. A call may leave those out.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION f(a INT, b INT DEFAULT 1) RETURNS INT LANGUAGE sql AS 'SELECT a + b';",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(f.argument_default_count(), 1);
    /// assert!(f.accepts_argument_count(1));
    /// assert!(f.accepts_argument_count(2));
    /// assert!(!f.accepts_argument_count(0));
    /// # Ok(())
    /// # }
    /// ```
    fn argument_default_count(&self) -> usize;

    /// Returns whether a call passing `arguments` positional arguments
    /// reaches the function. The trailing arguments a default fills may be
    /// left out, and a `VARIADIC` argument collects any number, at least one
    /// unless it has a default.
    ///
    /// See [`argument_default_count`](FunctionLike::argument_default_count)
    /// for an example.
    #[inline]
    fn accepts_argument_count(&self, arguments: usize) -> bool {
        accepts_positional(&input_modes(self), self.argument_default_count(), arguments)
    }

    /// Returns the type one call of the function yields, mirroring
    /// `pg_proc.prorettype`.
    ///
    /// A set-returning function yields its element type once per row. A
    /// function whose result is described by one `OUT` argument or one
    /// `RETURNS TABLE` column yields that column's type, and one described
    /// by several yields `record`. A polymorphic result keeps its
    /// pseudo-type here, and
    /// [`call_result_type_name`](FunctionLike::call_result_type_name) answers
    /// what it becomes for a given call.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION ids() RETURNS SETOF UUID LANGUAGE sql AS 'SELECT gen_random_uuid()';
    ///      CREATE FUNCTION one() RETURNS TABLE (id UUID) LANGUAGE sql AS 'SELECT gen_random_uuid()';
    ///      CREATE FUNCTION two(OUT a INT, OUT b TEXT) LANGUAGE sql AS 'SELECT 1, ''x''';",
    /// )?;
    /// let result = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .result_type_name(&db)
    ///         .map(|name| name.into_owned())
    /// };
    /// assert_eq!(result("ids").as_deref(), Some("UUID"));
    /// assert_eq!(result("one").as_deref(), Some("UUID"));
    /// assert_eq!(result("two").as_deref(), Some("record"));
    /// # Ok(())
    /// # }
    /// ```
    fn result_type_name<'db>(&'db self, database: &'db Self::DB) -> Option<Cow<'db, str>>;

    /// Returns the type a call of this function returns, given the type of
    /// each argument the call writes, in written order.
    ///
    /// Each entry of `argument_types` names a type, `unknown` for an untyped
    /// literal such as `NULL` or `'abc'`, or is [`None`] for an argument whose
    /// type the caller does not know. A polymorphic result takes the type its
    /// arguments bind as PostgreSQL's `enforce_generic_type_consistency`
    /// decides, so `anyelement` takes the type its arguments share,
    /// `anycompatible` their common type by
    /// [`common_type_name`](crate::utils::common_type_name), and the array
    /// forms the array over it. The answer reuses the caller's
    /// spelling where it can and otherwise spells the type as sqlparser
    /// writes it, so it parses back as a type with the PostgreSQL dialect.
    ///
    /// Answers [`None`] when the function cannot take the call, an argument
    /// of a known type converting to its parameter by no implicit cast, when
    /// an argument the result depends on has no known type, and when the
    /// function declares no result.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ArgumentTypeCount`] when `argument_types` does
    /// not hold one entry per argument `call` writes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION pick(a anycompatible, b anycompatible) RETURNS anycompatible
    ///          LANGUAGE sql AS 'SELECT coalesce(a, b)';",
    /// )?;
    /// let call = |sql: &str| {
    ///     let Statement::Query(query) =
    ///         Parser::parse_sql(&PostgreSqlDialect {}, &format!("SELECT {sql}")).unwrap().remove(0)
    ///     else {
    ///         unreachable!()
    ///     };
    ///     let SetExpr::Select(select) = *query.body else { unreachable!() };
    ///     let SelectItem::UnnamedExpr(Expr::Function(call)) = select.projection[0].clone() else {
    ///         unreachable!()
    ///     };
    ///     call
    /// };
    /// let pick = db
    ///     .function_by_target(TargetName::new("pick", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let returns = |sql: &str, types: &[Option<&str>]| {
    ///     pick.call_result_type_name(&call(sql), types, &db)
    ///         .map(|name| name.map(|name| name.into_owned()))
    /// };
    /// assert_eq!(
    ///     returns("pick(b, NULL)", &[Some("BYTEA"), Some("unknown")])?.as_deref(),
    ///     Some("BYTEA")
    /// );
    /// assert_eq!(returns("pick(i, j)", &[Some("INT"), Some("BIGINT")])?.as_deref(), Some("BIGINT"));
    /// // PostgreSQL finds no common type for a binary string and text.
    /// assert_eq!(returns("pick(b, t)", &[Some("BYTEA"), Some("TEXT")])?, None);
    /// assert_eq!(returns("pick(b, x)", &[Some("BYTEA"), None])?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn call_result_type_name<'a>(
        &'a self,
        call: &Function,
        argument_types: &[Option<&'a str>],
        database: &'a Self::DB,
    ) -> Result<Option<Cow<'a, str>>, LookupError> {
        let Some(written) = written_argument_names(call) else { return Ok(None) };
        check_argument_count(call, written.len(), argument_types.len())?;
        let Some(declared) = declared_at_call(self, database, &written) else { return Ok(None) };
        let Some(binding) = bind(declared, &actuals(argument_types)) else { return Ok(None) };
        Ok(self.result_type_name(database).and_then(|result| binding.result(result)))
    }

    /// Returns what a call of the function computes, mirroring
    /// `pg_proc.prokind`.
    ///
    /// A function the input declares is [`FunctionKind::Normal`]. The
    /// built-in aggregates such as `count` and the built-in window functions
    /// such as `row_number` answer their own kinds, and `count(*)` reaches the
    /// parameterless `count` PostgreSQL resolves it to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION is_member(who TEXT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';
    ///      CREATE TABLE members (member TEXT);
    ///      CREATE TABLE items (owner TEXT);
    ///      CREATE POLICY p ON items
    ///          USING (EXISTS (SELECT count(*) FROM members WHERE is_member(owner)));",
    /// )?;
    /// let policy = db.policies().next().expect("Policy should exist");
    /// let kinds = policy
    ///     .using_functions(&db)?
    ///     .map(|function| Ok((function.name(), function.kind(&db)?)))
    ///     .collect::<Result<Vec<_>, sql_traits::errors::LookupError>>()?;
    /// // The aggregate yields one row from no input, so this `EXISTS` holds
    /// // even when no member matches.
    /// assert_eq!(kinds, [("count", FunctionKind::Aggregate), ("is_member", FunctionKind::Normal)]);
    /// # Ok(())
    /// # }
    /// ```
    fn kind(&self, database: &Self::DB) -> Result<FunctionKind, LookupError>;

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

    /// Returns the body as it is written, or [`None`] for a function whose
    /// code lives outside the schema or that has no body.
    ///
    /// [`body`](FunctionLike::body),
    /// [`body_expression`](FunctionLike::body_expression),
    /// [`atomic_statements`](FunctionLike::atomic_statements),
    /// [`body_is_bound`](FunctionLike::body_is_bound),
    /// [`referenced_tables`](FunctionLike::referenced_tables),
    /// [`referenced_relations`](FunctionLike::referenced_relations),
    /// [`referenced_functions`](FunctionLike::referenced_functions) and
    /// [`helper_set`](FunctionLike::helper_set) all read the body from here,
    /// so an implementation handing over the body a catalog stores answers
    /// them as a parsed script does. A body bound at creation answers the
    /// bound form, each relation it names written as the store holds the
    /// relation it reached. A catalog that stores a bound body only as a node
    /// tree hands over the definition it prints instead,
    /// [`FunctionBody::BoundDefinition`], which those readers parse when they
    /// need the body.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION quoted() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    ///      CREATE FUNCTION returned() RETURNS INT LANGUAGE sql RETURN 1;
    ///      CREATE FUNCTION linked() RETURNS INT LANGUAGE c AS 'library', 'symbol';",
    /// )?;
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// assert_eq!(function("quoted").written_body(), Some(FunctionBody::Text("SELECT 1")));
    /// assert!(matches!(function("returned").written_body(), Some(FunctionBody::Return(_))));
    /// // Text in another language is still text, which its language reads.
    /// assert_eq!(function("linked").written_body(), Some(FunctionBody::Text("library")));
    /// # Ok(())
    /// # }
    /// ```
    fn written_body(&self) -> Option<FunctionBody<'_>>;

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
    #[inline]
    fn body(&self) -> Option<&str> {
        match self.written_body()? {
            FunctionBody::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the expression of a function written `RETURN <expression>`.
    ///
    /// PostgreSQL 14 added this spelling, and it is the one that parses the
    /// body up front rather than leaving a string for the caller to parse. A
    /// function whose body is written as a string answers [`None`] here and
    /// answers [`body`](FunctionLike::body) instead, so a caller wanting the
    /// body in either spelling asks this first and falls back to parsing the
    /// text. A bound body handed over as the definition a catalog prints,
    /// [`FunctionBody::BoundDefinition`], is parsed here under the dialect of
    /// `database`.
    ///
    /// The relation names a body writes are bound at `CREATE FUNCTION` under
    /// the session `search_path` in effect at creation, the way PostgreSQL
    /// stores them, so the expression answers the name the store holds for
    /// the relation each name reached. A body written as a string is never
    /// bound, and its names stay as written. A body written as a
    /// `BEGIN ATOMIC` block binds the same way, but it holds a list of
    /// statements rather than an expression, so it answers [`None`] here and
    /// in [`body`](FunctionLike::body), and
    /// [`atomic_statements`](FunctionLike::atomic_statements) hands its
    /// statements back.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnparsable`] when the printed
    /// definition of a bound body does not parse, or holds no body bound at
    /// creation.
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
    /// assert_eq!(
    ///     added.body_expression(&db)?.map(|expression| expression.to_string()).as_deref(),
    ///     Some("a + b"),
    /// );
    ///
    /// let quoted = db
    ///     .function_by_target(TargetName::new("quoted", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert_eq!(quoted.body(), Some("SELECT a"));
    /// assert_eq!(quoted.body_expression(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.t (a INT);
    /// SET search_path = app;
    /// CREATE FUNCTION reads_table() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
    /// ",
    /// )?;
    /// // The function lands in `app`, the first schema on the path.
    /// let in_app = TargetName::new("reads_table", false).with_schema("app", false);
    /// let reads_table =
    ///     db.function_by_target(in_app, IdentifierCase::AsWritten)?.expect("Function should exist");
    /// assert_eq!(
    ///     reads_table.body_expression(&db)?.map(|expression| expression.to_string()).as_deref(),
    ///     Some("(SELECT a FROM app.t)"),
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn body_expression<'f>(
        &'f self,
        database: &Self::DB,
    ) -> Result<Option<Cow<'f, Expr>>, LookupError> {
        body_expression(self, database)
    }

    /// Returns the statements of a function written as a `BEGIN ATOMIC`
    /// block.
    ///
    /// The block is the form of a body PostgreSQL binds at `CREATE FUNCTION`
    /// that holds a list of statements, beside the `RETURN <expression>` form
    /// [`body_expression`](FunctionLike::body_expression) hands back. Its
    /// statements answer the name the store holds for each relation they
    /// reached, as that expression does. A bound body handed over as the
    /// definition a catalog prints, [`FunctionBody::BoundDefinition`], is
    /// parsed here under the dialect of `database`.
    ///
    /// Every other body answers [`None`]. That includes SQL Server's
    /// `AS BEGIN ... END`, read each time the function runs, whose statements
    /// [`written_body`](FunctionLike::written_body) hands over as
    /// [`FunctionBody::Statements`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnparsable`] when the printed
    /// definition of a bound body does not parse, or holds no body bound at
    /// creation.
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
    /// CREATE SCHEMA app;
    /// CREATE TABLE app.t (a INT);
    /// SET search_path = app;
    /// CREATE FUNCTION cleared() RETURNS INT LANGUAGE sql BEGIN ATOMIC
    ///     DELETE FROM t WHERE a = 0;
    ///     SELECT count(*)::int FROM t;
    /// END;
    /// CREATE FUNCTION counted() RETURNS INT LANGUAGE sql RETURN (SELECT count(*)::int FROM t);
    /// ",
    /// )?;
    /// let function = |name| {
    ///     let in_app = TargetName::new(name, false).with_schema("app", false);
    ///     db.function_by_target(in_app, IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// let cleared = function("cleared").atomic_statements(&db)?.expect("a BEGIN ATOMIC body");
    /// assert_eq!(
    ///     cleared.iter().map(ToString::to_string).collect::<Vec<_>>(),
    ///     ["DELETE FROM app.t WHERE a = 0", "SELECT count(*)::INT FROM app.t"],
    /// );
    /// assert_eq!(function("counted").atomic_statements(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn atomic_statements<'f>(
        &'f self,
        database: &Self::DB,
    ) -> Result<Option<Cow<'f, [Statement]>>, LookupError> {
        atomic_statements(self, database)
    }

    /// Returns the function-local configuration after later alterations, one
    /// entry per setting in the order `pg_proc.proconfig` stores them.
    /// `SET name = DEFAULT` removes the setting as `RESET name` does, so no
    /// entry holds `DEFAULT`.
    fn configuration_parameters(&self) -> &[FunctionDefinitionSetParam];

    /// Returns the `search_path` the function sets for its own body, each
    /// schema as `pg_proc.proconfig` stores it with whether it compares
    /// exactly, the way [`DatabaseLike::search_path`] yields the session's
    /// path, or [`None`] when the body runs under the caller's path.
    ///
    /// The setting is read after every later `ALTER FUNCTION`, so
    /// `RESET search_path`, `RESET ALL` and `SET search_path TO DEFAULT` leave
    /// [`None`]. `SET search_path FROM CURRENT` answers the path in effect
    /// where the clause ran. `SET search_path = ''` answers the one
    /// zero-length quoted entry PostgreSQL stores, `("", true)`, which names
    /// no schema, so the temporary schema stays implicit and is searched
    /// first for relations.
    ///
    /// A body written as a string resolves its unqualified names under this
    /// path when it runs. A body [bound at
    /// creation](FunctionLike::body_is_bound) keeps the relations and functions
    /// it reached then, whatever path is in effect.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::InvalidObjectName`] for an entry PostgreSQL
    /// refuses in `search_path`, which only a node `database` never ingested
    /// can hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, utils::identifier_resolution::is_temporary_schema};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      SET search_path = app, public;
    ///      CREATE FUNCTION pinned() RETURNS INT LANGUAGE sql SECURITY DEFINER
    ///          SET search_path = app, pg_temp AS 'SELECT 1';
    ///      CREATE FUNCTION captured() RETURNS INT LANGUAGE sql
    ///          SET search_path FROM CURRENT AS 'SELECT 1';
    ///      CREATE FUNCTION unpinned() RETURNS INT LANGUAGE sql
    ///          SET search_path = app AS 'SELECT 1';
    ///      ALTER FUNCTION unpinned() RESET search_path;",
    /// )?;
    /// let function = |name| {
    ///     let in_app = TargetName::new(name, false).with_schema("app", false);
    ///     db.function_by_target(in_app, IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// let pinned = function("pinned").search_path(&db)?.expect("the function sets a path");
    /// assert_eq!(pinned, [("app".into(), false), ("pg_temp".into(), false)]);
    /// // The temporary schema comes last, so no relation in it shadows one in `app`.
    /// assert!(pinned.last().is_some_and(|(schema, quoted)| is_temporary_schema(schema, *quoted)));
    ///
    /// assert_eq!(
    ///     function("captured").search_path(&db)?,
    ///     Some(vec![("app".into(), false), ("public".into(), false)]),
    /// );
    /// assert_eq!(function("unpinned").search_path(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn search_path<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<SearchPath<'db>>, LookupError>;

    /// Returns whether PostgreSQL bound the body when the function was
    /// created, so that no `search_path`, the caller's or the function's own,
    /// redirects the relations and functions it names when it runs.
    ///
    /// `CREATE FUNCTION` parses and binds a body written `RETURN <expression>`
    /// or as a `BEGIN ATOMIC` block. A body written as a string, in any
    /// language, is read again each time the function runs, and code outside
    /// the schema, as `AS 'library', 'symbol'` names it, holds no SQL to bind,
    /// so both answer false.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id INT);
    ///      CREATE FUNCTION returned() RETURNS BIGINT LANGUAGE sql
    ///          RETURN (SELECT count(*) FROM docs);
    ///      CREATE FUNCTION atomic() RETURNS BIGINT LANGUAGE sql
    ///          BEGIN ATOMIC SELECT count(*) FROM docs; END;
    ///      CREATE FUNCTION quoted() RETURNS BIGINT LANGUAGE sql
    ///          AS 'SELECT count(*) FROM docs';",
    /// )?;
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// assert!(function("returned").body_is_bound());
    /// assert!(function("atomic").body_is_bound());
    /// // A caller's temporary `docs` would shadow `public.docs` here.
    /// assert!(!function("quoted").body_is_bound());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn body_is_bound(&self) -> bool {
        self.written_body().is_some_and(|body| body.is_bound())
    }

    /// Returns every function the calls of a body
    /// [bound at creation](FunctionLike::body_is_bound) reached when it was
    /// bound, each once, and none for a body read again each time the
    /// function runs.
    ///
    /// PostgreSQL records these when it binds the body, so the body text
    /// alone does not decide them.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] for a function `database`
    /// does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE FUNCTION app.one() RETURNS INT LANGUAGE sql RETURN 1;
    ///      SET search_path = app;
    ///      CREATE FUNCTION public.two() RETURNS INT LANGUAGE sql RETURN one() + one();
    ///      RESET search_path;",
    /// )?;
    /// let two = db
    ///     .function_by_target(TargetName::new("two", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// // `app` was on the path when `two` was created, and is no longer.
    /// let reached: Vec<_> = two.bound_body_functions(&db)?.map(FunctionLike::target_name).collect();
    /// assert_eq!(reached, [TargetName::new("one", false).with_schema("app", false)]);
    /// # Ok(())
    /// # }
    /// ```
    fn bound_body_functions<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + use<'db, Self>,
        LookupError,
    >
    where
        Self: 'db;

    /// Returns every base table the body reads or writes, plain views
    /// expanded to the base tables they read, resolved the way PostgreSQL
    /// resolves them when the function runs, and deduplicated in first-seen
    /// order.
    ///
    /// An unqualified name in a body written as a string resolves under the
    /// function's own [`search_path`](FunctionLike::search_path) when it sets
    /// one, and under [`DatabaseLike::search_path`] otherwise. A body written
    /// `RETURN <expression>` or as a `BEGIN ATOMIC` block was
    /// [bound at creation](FunctionLike::body_is_bound), so it reads the
    /// relations it reached then, whatever path the function sets. The body
    /// is read with the rules of
    /// [`DataStatementLike::referenced_tables`](crate::traits::DataStatementLike::referenced_tables).
    /// A body of several statements is read one statement after another, as
    /// PostgreSQL 18 runs it, so a `SET` or `RESET` of `search_path` in it
    /// reaches the statements after it. A `LANGUAGE plpgsql` body is read
    /// through every SQL statement and expression it embeds, in body order,
    /// and never reads a name it declares or a parameter as a relation. A
    /// `SET` of `search_path` there reaches what follows it on the straight
    /// line only. An unqualified name such a `SET` may reach from a branch, a
    /// loop or a block with an exception handler cannot be resolved, and a
    /// handler starts under the path its block started with, since
    /// PostgreSQL rolls the block back first. A function without a `LANGUAGE`
    /// clause is read as SQL, which is what PostgreSQL assumes for a `RETURN`
    /// body and what dialects without the clause mean.
    ///
    /// # Errors
    ///
    /// Which tables the body reads cannot be known when it is written in a
    /// language other than SQL or PL/pgSQL
    /// ([`LookupError::FunctionBodyNotSql`]), when the function's code lives
    /// outside the schema ([`LookupError::FunctionBodyUnavailable`]), or when
    /// the body runs code outside it, through `CALL`, `EXECUTE` or a
    /// `set_config` of `search_path` before an unqualified name
    /// ([`LookupError::FunctionBodyUnresolvable`]). Returns
    /// [`LookupError::FunctionBodyUnparsable`] when the body text does not
    /// parse, under the database's dialect for SQL, or when the printed
    /// definition of a bound body does not parse or holds no body bound at
    /// creation, and any error resolving a name the body reads.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE owner_admins (owner uuid);
    ///      CREATE FUNCTION has_admin(o uuid) RETURNS boolean LANGUAGE sql SECURITY DEFINER
    ///          SET search_path = ''
    ///          AS $$ SELECT EXISTS (SELECT 1 FROM public.owner_admins WHERE owner = o) $$;
    ///      CREATE FUNCTION unreachable(o uuid) RETURNS boolean LANGUAGE sql
    ///          SET search_path = ''
    ///          AS $$ SELECT EXISTS (SELECT 1 FROM owner_admins WHERE owner = o) $$;",
    /// )?;
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// let read = function("has_admin").referenced_tables(&db)?;
    /// let names: Vec<_> = read.iter().map(|table| table.table_name()).collect();
    /// assert_eq!(names, ["owner_admins"]);
    /// // The emptied path carries the bare name nowhere when the function runs.
    /// assert!(function("unreachable").referenced_tables(&db)?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn referenced_tables<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, LookupError> {
        Ok(body_reads(self, database)?.tables)
    }

    /// Returns every relation the body names, whether table, plain view or
    /// materialized view, resolved and deduplicated as
    /// [`referenced_tables`](FunctionLike::referenced_tables) resolves them.
    ///
    /// This is what the body reads directly, before any view is expanded, as
    /// [`DataStatementLike::referenced_relations`](crate::traits::DataStatementLike::referenced_relations)
    /// answers for a statement.
    ///
    /// # Errors
    ///
    /// The errors of [`referenced_tables`](FunctionLike::referenced_tables).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id INT);
    ///      CREATE VIEW open_docs AS SELECT id FROM docs;
    ///      CREATE FUNCTION counted() RETURNS BIGINT LANGUAGE sql
    ///          AS 'SELECT count(*) FROM open_docs';",
    /// )?;
    /// let counted = db
    ///     .function_by_target(TargetName::new("counted", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let named = counted.referenced_relations(&db)?;
    /// assert!(matches!(named.as_slice(), [Relation::View(view)] if view.view_name() == "open_docs"));
    /// // The view's base table is what the body reads in the end.
    /// let tables = counted.referenced_tables(&db)?;
    /// assert_eq!(tables.iter().map(|table| table.table_name()).collect::<Vec<_>>(), ["docs"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn referenced_relations<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<Relation<'db, Self::DB>>, LookupError> {
        Ok(body_reads(self, database)?.relations)
    }

    /// Returns every function the calls the body writes may reach, resolved
    /// the way PostgreSQL resolves them, and deduplicated in first-reached
    /// order.
    ///
    /// A call in a body written as a string resolves when the function runs,
    /// under the path [`referenced_tables`](FunctionLike::referenced_tables)
    /// reads relations under, `pg_catalog` searched first unless that path
    /// places it, and reaches the functions the database holds. A body
    /// written `RETURN <expression>` or as a `BEGIN ATOMIC` block was
    /// [bound at creation](FunctionLike::body_is_bound), so each of its calls
    /// reaches the functions it reached then, under the path of the statement
    /// that created it. PostgreSQL picks among overloads by argument type,
    /// which is not inferred, so every overload accepting the call's argument
    /// count counts, as
    /// [`PolicyLike::functions_reached_by`](crate::traits::PolicyLike::functions_reached_by)
    /// counts them. A view the body reads adds none of the calls its
    /// definition makes.
    ///
    /// # Errors
    ///
    /// The body-level errors of
    /// [`referenced_tables`](FunctionLike::referenced_tables), for the bodies
    /// read each time the function runs, an unqualified call after a
    /// `set_config` of `search_path` among them
    /// ([`LookupError::FunctionBodyUnresolvable`]). A bound body answers the
    /// functions [`bound_body_functions`](FunctionLike::bound_body_functions)
    /// records, so a printed definition is never parsed here, and returns
    /// [`LookupError::ObjectNotInDatabase`] for a function `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE members (doc_id TEXT, user_id TEXT);
    ///      CREATE FUNCTION is_member(d TEXT) RETURNS BOOLEAN LANGUAGE sql
    ///          AS $$ SELECT EXISTS (SELECT 1 FROM members WHERE doc_id = d) $$;
    ///      CREATE FUNCTION can_read(d TEXT) RETURNS BOOLEAN LANGUAGE sql
    ///          AS $$ SELECT is_member(d) $$;",
    /// )?;
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    /// };
    /// // `can_read` reads nothing itself, and reaches `members` through its call.
    /// assert!(function("can_read").referenced_tables(&db)?.is_empty());
    /// let called = function("can_read").referenced_functions(&db)?;
    /// assert_eq!(called.iter().map(|called| called.name()).collect::<Vec<_>>(), ["is_member"]);
    /// let read = called[0].referenced_tables(&db)?;
    /// assert_eq!(read.iter().map(|table| table.table_name()).collect::<Vec<_>>(), ["members"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn referenced_functions<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Function>, LookupError> {
        body_calls(self, database)
    }

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
    /// The owner is the role the function was created under, as `SET ROLE` and
    /// `SET SESSION AUTHORIZATION` leave it, until `ALTER FUNCTION ... OWNER
    /// TO <role>` hands it to another. A function created while the input had
    /// set no role has none, and neither has one handed to `CURRENT_ROLE`,
    /// `CURRENT_USER` or `SESSION_USER`, which name whoever runs the statement
    /// rather than a role the input declares.
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
    /// CREATE ROLE app_writer;
    /// CREATE FUNCTION reassigned() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// ALTER FUNCTION reassigned() OWNER TO app_reader;
    /// CREATE FUNCTION untouched() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// SET ROLE app_writer;
    /// CREATE FUNCTION written() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// ",
    /// )?;
    /// let owner = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .owner(&db)
    /// };
    /// assert_eq!(owner("reassigned")?, Some("app_reader"));
    /// assert_eq!(owner("written")?, Some("app_writer"));
    /// // Created before any role was set and never reassigned.
    /// assert_eq!(owner("untouched")?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>;

    /// Returns whether every role may execute the function, which is what
    /// `PUBLIC` holding `EXECUTE` on it means.
    ///
    /// PostgreSQL gives `PUBLIC` `EXECUTE` on every new function unless an
    /// `ALTER DEFAULT PRIVILEGES` of the role creating it takes it away first.
    /// A `REVOKE` takes it away later and a `GRANT ... TO PUBLIC` restores it.
    /// Every spelling reaches it, whether a signature, a name that covers one
    /// function alone, or `ALL FUNCTIONS IN SCHEMA`, which covers the
    /// functions the schema holds when it runs.
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

    /// Returns who holds `EXECUTE` on the function directly, the entries
    /// `aclexplode` lists for its access list.
    ///
    /// `PUBLIC` comes first while it holds the privilege, then the role
    /// [`owner`](FunctionLike::owner) names while its own entry holds, then
    /// every other grantee in the order first granted, each role once and by
    /// the name PostgreSQL stores, declared or not. A `CURRENT_USER`,
    /// `CURRENT_ROLE` or `SESSION_USER` grantee is listed as the principal
    /// PostgreSQL resolved to a role when the statement ran. A role holding
    /// the privilege only through a membership is not listed, which
    /// [`can_execute`](FunctionLike::can_execute) answers for one role.
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
    /// CREATE ROLE app_owner;
    /// CREATE ROLE authenticated;
    /// CREATE FUNCTION f() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    /// ALTER FUNCTION f() OWNER TO app_owner;
    /// REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
    /// GRANT EXECUTE ON FUNCTION f() TO authenticated;
    /// ",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let holders: Vec<_> = f.execute_holders(&db)?.collect();
    /// assert_eq!(
    ///     holders,
    ///     [PrivilegeHolder::Role("app_owner".into()), PrivilegeHolder::Role("authenticated".into())]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn execute_holders<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = PrivilegeHolder<'db>>, LookupError>;

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

    /// What this helper returns for a caller of `deployment`, called with
    /// `arguments`, as a union of grant arms and parent arms.
    ///
    /// A helper answers when it is a `LANGUAGE sql STABLE SECURITY DEFINER`
    /// function returning `SETOF` one scalar type with a pinned
    /// `search_path`, owned by a declared role holding `SELECT` on every
    /// table it reads, or on every column of it the body names, `USAGE` on
    /// the schema of every table and helper the body names, and exempt from
    /// the row-level security of the tables, and when its body is a `UNION`
    /// of arms of two shapes.
    ///
    /// - A grant arm, `SELECT <column> FROM <table> WHERE <caller column> =
    ///   current_setting('<setting>'[, true]) [AND <condition>]`, where the
    ///   deployment declares the setting as the caller and the condition, the
    ///   arguments substituted, reduces to `TRUE`, to `FALSE` or `NULL`, or to
    ///   the literal values one column has to hold.
    /// - A parent arm, `SELECT <primary key> FROM <table> WHERE <column> IN
    ///   (SELECT <helper>(<arguments>))`, each argument a parameter or a
    ///   literal, the helper called answered in turn for the arguments it
    ///   receives. The column names its parent through its one single-column
    ///   foreign key or, where it has none, through the table the called
    ///   helper's set covers.
    ///
    /// A grant arm whose condition reduces to `FALSE` or `NULL` keeps no row
    /// and is left out. A column named like a parameter is the column, as
    /// PostgreSQL resolves names in a SQL function. An unqualified name is
    /// looked up past the schemas on the path the owner cannot use, as
    /// PostgreSQL leaves them out of the owner's path.
    ///
    /// # Arguments
    ///
    /// * `arguments` - One literal per input parameter.
    /// * `deployment` - The deployment declaring the caller setting.
    /// * `database` - The database the helper belongs to.
    ///
    /// # Errors
    ///
    /// Returns the [`HelperSetError`] naming why the helper, or a helper a
    /// parent arm calls, refuses to answer.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{errors::HelperSetError, prelude::*};
    /// use sqlparser::{
    ///     ast::{DataType, Value},
    ///     dialect::PostgreSqlDialect,
    /// };
    ///
    /// # fn main() -> Result<(), HelperSetError> {
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
    ///      CREATE TABLE grants (doc_id TEXT, principal_id TEXT, role TEXT);
    ///      CREATE FUNCTION docs_with(min_role TEXT) RETURNS SETOF TEXT
    ///      LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    ///        SELECT doc_id FROM grants
    ///        WHERE principal_id = current_setting('app.user_id', true)
    ///          AND (role = 'admin' OR role = min_role)
    ///      $$;
    ///      ALTER FUNCTION docs_with(TEXT) OWNER TO authz_owner;
    ///      GRANT SELECT ON grants TO authz_owner;",
    /// )
    /// .expect("the schema parses");
    /// let caller = SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))
    ///     .expect("a custom setting name");
    /// let deployment = Deployment::new("app", ["app"], [caller]).expect("a valid deployment");
    /// let docs_with = db
    ///     .function_by_target(TargetName::new("docs_with", false), IdentifierCase::AsWritten)?
    ///     .expect("the helper exists");
    ///
    /// let set =
    ///     docs_with.helper_set(&[Value::SingleQuotedString("write".into())], &deployment, &db)?;
    /// let HelperArm::Grant(grant) = &set.arms()[0] else { unreachable!() };
    /// assert_eq!(grant.column().column_name(), "doc_id");
    /// assert_eq!(grant.caller_column().column_name(), "principal_id");
    /// let roles = grant.restriction().expect("the condition keeps two roles");
    /// let admin = Value::SingleQuotedString("admin".into());
    /// let write = Value::SingleQuotedString("write".into());
    /// assert_eq!(roles.values(), [admin, write]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn helper_set<'db>(
        &'db self,
        arguments: &[Value],
        deployment: &'db Deployment,
        database: &'db Self::DB,
    ) -> Result<HelperSet<'db, Self::DB>, HelperSetError> {
        helper_set(self, arguments, deployment, database)
    }

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
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
        let f = db
            .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function should exist");

        assert_eq!(f.security_mode(), FunctionSecurity::Invoker);
    }

    #[test]
    fn test_alter_function_depends_on_extension_names_a_held_function() {
        // PostgreSQL 16 refuses `function missing() does not exist`, and a bare
        // name covering two overloads as not unique.
        let held = "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
                    CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT x';";
        let parse = |statement: &str| {
            ParserDB::parse::<PostgreSqlDialect>(&alloc::format!("{held} {statement}"))
        };
        parse("ALTER FUNCTION f() DEPENDS ON EXTENSION plpgsql;").expect("f() is held");
        let missing = parse("ALTER FUNCTION missing() DEPENDS ON EXTENSION plpgsql;");
        assert!(matches!(missing, Err(Error::AlterFunctionNotFound { .. })), "got {missing:?}");
        let bare = parse("ALTER FUNCTION f DEPENDS ON EXTENSION plpgsql;");
        assert!(matches!(bare, Err(Error::AmbiguousAlterFunction { .. })), "got {bare:?}");
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
