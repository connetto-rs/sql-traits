//! Submodule providing a trait for describing SQL Function-like entities.

use alloc::{borrow::Cow, vec::Vec};
use core::{fmt::Debug, hash::Hash};

use sqlparser::ast::{
    ArgMode, Expr, Function, FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam,
    FunctionSecurity, Statement, Value,
};

use crate::{
    errors::{HelperSetError, LookupError, ReturnedValuesError},
    impls::{
        atomic_statements, body_calls, body_expression, body_reads, body_site_source,
        body_unqualified_names, helper_set, unconfined_function_read,
    },
    structs::{
        ActingRole, AggregateKind, ArmCondition, Deployment, DynamicExecutionSites, FunctionBody,
        FunctionKind, HelperSet, PlPgSqlSites, PrivilegeHolder, ReturnedValue, TargetName,
        UnconfinedRead, UnqualifiedName,
    },
    traits::{DataTypeLike, DatabaseLike, Metadata, Relation, TriggerLike},
    utils::{
        call_typing::{
            accepts_positional, actuals, bind, check_argument_count, declared_at_call, input_modes,
            written_argument_names,
        },
        identifier_resolution::{SearchPath, stored_identifier},
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
        stored_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the type each argument is declared with, in declaration order,
    /// type modifiers included.
    ///
    /// PostgreSQL discards a modifier on an argument type, so `VARCHAR(3)`
    /// accepts and returns a longer string. A catalog-backed implementation
    /// answers the types `pg_proc` stores, which carry no modifier.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION f(OUT id VARCHAR(3), note TEXT) LANGUAGE sql AS 'SELECT 1';",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let modifiers: Vec<_> = f.argument_types(&db).map(|declared| declared.typmod(&db)).collect();
    /// assert_eq!(modifiers, [Some(TypeModifier::Modified(7)), Some(TypeModifier::Unmodified)]);
    /// # Ok(())
    /// # }
    /// ```
    fn argument_types<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::DataType>;

    /// Returns the name of each argument's type, in declaration order, as
    /// [`DataTypeLike::name`] answers it for
    /// [`argument_types`](FunctionLike::argument_types).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add(x INT, y INT) RETURNS INT AS 'SELECT x + y;';
    /// CREATE FUNCTION greet(name VARCHAR(20)) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// ",
    /// )?;
    /// let add_fn = db.functions().find(|f| f.name() == "add").expect("Function should exist");
    /// let greet_fn = db.functions().find(|f| f.name() == "greet").expect("Function should exist");
    /// assert_eq!(add_fn.argument_type_names(&db).collect::<Vec<_>>(), vec!["INT", "INT"]);
    /// assert_eq!(greet_fn.argument_type_names(&db).collect::<Vec<_>>(), vec!["VARCHAR"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn argument_type_names<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = Cow<'db, str>> {
        self.argument_types(database).map(|declared| declared.name(database))
    }

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
                Cow::Owned(stored_identifier(name.name(), name.name_is_quoted()).into_owned())
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

    /// Returns the type the function is declared to return, type modifiers
    /// included, or [`None`] for a function declared without `RETURNS`.
    ///
    /// A set-returning `RETURNS SETOF type` answers its element type, and
    /// [`returns_set`](FunctionLike::returns_set) answers the cardinality.
    /// `RETURNS TABLE (...)` answers the table type it declares, whose
    /// columns [`returned_columns`](FunctionLike::returned_columns) answers.
    /// PostgreSQL discards a modifier on a return type, and a catalog-backed
    /// implementation answers the type `pg_proc.prorettype` stores.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION codes() RETURNS SETOF VARCHAR(2) LANGUAGE sql AS 'SELECT ''ab''';",
    /// )?;
    /// let codes = db
    ///     .function_by_target(TargetName::new("codes", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let declared = codes.return_type(&db).expect("A return type is declared");
    /// assert_eq!(declared.name(&db), "VARCHAR");
    /// assert_eq!(declared.typmod(&db), Some(TypeModifier::Modified(6)));
    /// # Ok(())
    /// # }
    /// ```
    fn return_type<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::DataType>;

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

    /// Returns the number of rows the planner expects the function to
    /// return, mirroring `pg_proc.prorows`, or [`None`] for a function that
    /// returns no set.
    ///
    /// A set-returning function declared without `ROWS` answers PostgreSQL's
    /// default of 1000, which `pg_dump` leaves unwritten, so a declared
    /// `ROWS 1000` answers the same. `ALTER FUNCTION ... ROWS` applies in
    /// statement order, and `CREATE OR REPLACE FUNCTION` takes the
    /// replacement's declaration. A builtin
    /// [`ParserDB`](crate::structs::ParserDB) registers in `pg_catalog`
    /// answers its estimate in PostgreSQL 18, so `unnest(anyarray)` answers
    /// 100.
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
    /// CREATE FUNCTION one() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    /// CREATE FUNCTION unsized() RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
    /// CREATE FUNCTION sized() RETURNS SETOF INT LANGUAGE sql ROWS 10 AS 'SELECT 1';
    /// CREATE FUNCTION altered() RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
    /// ALTER FUNCTION altered() ROWS 5;
    /// ",
    /// )?;
    /// let estimated_rows = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .estimated_rows()
    /// };
    /// assert_eq!(estimated_rows("one"), None);
    /// assert_eq!(estimated_rows("unsized"), Some(1000.0));
    /// assert_eq!(estimated_rows("sized"), Some(10.0));
    /// assert_eq!(estimated_rows("altered"), Some(5.0));
    /// # Ok(())
    /// # }
    /// ```
    fn estimated_rows(&self) -> Option<f32>;

    /// Returns the mode of each argument, in declaration order, mirroring
    /// `pg_proc.proargmodes`. An argument declared without a mode is
    /// [`ArgMode::In`].
    ///
    /// The positions line up with
    /// [`argument_type_names`](FunctionLike::argument_type_names). A call
    /// passes the `IN`, `INOUT` and `VARIADIC` arguments, an `OUT` argument
    /// describes a function's result, and `CALL` passes a procedure's `OUT`
    /// arguments too.
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

    /// Returns the name and the declared type of each column a `RETURNS
    /// TABLE` clause declares, in declaration order, and nothing for any
    /// other result.
    ///
    /// PostgreSQL stores these columns after the arguments in
    /// `pg_proc.proargnames` with mode `t`, so a PL/pgSQL body reaches each
    /// as an output parameter. It discards a modifier on a column type, as it
    /// does on an argument type.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION f() RETURNS TABLE (\"Total\" NUMERIC(5,2)) LANGUAGE sql AS 'SELECT 1';",
    /// )?;
    /// let f = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let columns: Vec<_> = f
    ///     .returned_columns(&db)
    ///     .map(|(name, declared)| (name, declared.name(&db), declared.numeric_scale(&db)))
    ///     .collect();
    /// assert_eq!(columns, [(TargetName::new("Total", true), "NUMERIC".into(), Some(2))]);
    /// # Ok(())
    /// # }
    /// ```
    fn returned_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = (TargetName<'db>, &'db <Self::DB as DatabaseLike>::DataType)>;

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
    /// [`DatabaseLike::common_type_name`], and the array forms the array over
    /// it. The answer reuses the caller's spelling where it can and otherwise
    /// spells the type as sqlparser writes it, so it parses back as a type
    /// with the PostgreSQL dialect.
    ///
    /// Answers [`None`] when the database proves the function cannot take
    /// the call, an argument of a known type converting to its parameter by
    /// no implicit cast, when an argument the result depends on has no known
    /// type or the database's casts and type facts leave the result open, and
    /// when the function declares no result.
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
        let Some(matched) = declared_at_call(self, database, &written) else { return Ok(None) };
        let actuals = actuals(database, argument_types);
        let Some(binding) = bind(database, matched.declared, &actuals) else { return Ok(None) };
        Ok(self.result_type_name(database).and_then(|result| binding.result(result)))
    }

    /// Returns what a call of the function computes, mirroring
    /// `pg_proc.prokind`.
    ///
    /// A `CREATE FUNCTION` declares a [`FunctionKind::Normal`] function and
    /// a `CREATE PROCEDURE` a [`FunctionKind::Procedure`]. The
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

    /// Returns how the function takes its arguments when it is an aggregate,
    /// mirroring `pg_aggregate.aggkind`, or `None` for any other function.
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
    /// let db = ParserDB::parse::<PostgreSqlDialect>("")?;
    /// let mut kinds = Vec::new();
    /// for function in db.functions().filter(|function| function.name() == "rank") {
    ///     kinds.push(function.aggregate_kind(&db)?);
    /// }
    /// // The window function `rank()` is no aggregate, `rank(1) WITHIN GROUP
    /// // (ORDER BY a)` ranks one hypothetical row.
    /// kinds.sort();
    /// assert_eq!(kinds, [None, Some(AggregateKind::HypotheticalSet { direct_arguments: 1 })]);
    /// # Ok(())
    /// # }
    /// ```
    fn aggregate_kind(&self, database: &Self::DB) -> Result<Option<AggregateKind>, LookupError>;

    /// Returns whether the function, as an aggregate, returns `NULL` when it
    /// aggregates no input row.
    ///
    /// The answer is provable from the aggregate's initial state
    /// (`pg_aggregate.agginitval`) and the strictness of its final function. A
    /// `NULL` initial state that no non-strict final function turns into a
    /// value returns `NULL` over no input row, as `max` and `bool_or` do. An
    /// aggregate that starts from a value, like `count`, returns that value,
    /// so it answers `false`. A non-strict final function may turn a `NULL`
    /// state into a value, so it answers `false` even when the state starts
    /// `NULL`, as `array_agg` does.
    ///
    /// A function that is no aggregate answers `false`, and a builtin answers
    /// from the `pg_aggregate` facts of its `pg_proc` row.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let database = ParserDB::parse::<PostgreSqlDialect>("")?;
    /// let max =
    ///     database.functions().find(|function| function.name() == "max").expect("max is a builtin");
    /// // `max` starts from `NULL` and has no final function, so over no
    /// // input row it returns `NULL`.
    /// assert!(max.returns_null_over_no_input_row(&database));
    ///
    /// let count = database
    ///     .functions()
    ///     .find(|function| function.name() == "count")
    ///     .expect("count is a builtin");
    /// // `count` starts from `0`, so over no input row it returns `0`.
    /// assert!(!count.returns_null_over_no_input_row(&database));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn returns_null_over_no_input_row(&self, _database: &Self::DB) -> bool {
        false
    }

    /// Returns whether the function, as an aggregate, returns `NULL` when
    /// every input it aggregates is `NULL`.
    ///
    /// The answer is provable from the aggregate's initial state
    /// (`pg_aggregate.agginitval`) and the strictness of its transition and
    /// final functions. A `NULL` initial state a strict transition function
    /// never moves and no non-strict final function turns into a value returns
    /// `NULL` over `NULL` inputs, as `max` and `bool_or` do. A non-strict
    /// transition function may turn `NULL` inputs into a value, so it answers
    /// `false`, as `array_agg` does. See
    /// [`returns_null_over_no_input_row`](FunctionLike::returns_null_over_no_input_row)
    /// for the no-input-row case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let database = ParserDB::parse::<PostgreSqlDialect>("")?;
    /// let bool_or = database
    ///     .functions()
    ///     .find(|function| function.name() == "bool_or")
    ///     .expect("bool_or is a builtin");
    /// // `bool_or` starts from `NULL`, its transition function is strict, so
    /// // over `NULL` inputs it returns `NULL`.
    /// assert!(bool_or.returns_null_over_null_inputs(&database));
    ///
    /// let array_agg = database
    ///     .functions()
    ///     .find(|function| function.name() == "array_agg")
    ///     .expect("array_agg is a builtin");
    /// // `array_agg`'s transition function is not strict, so the answer is
    /// // not provable from the catalog.
    /// assert!(!array_agg.returns_null_over_null_inputs(&database));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn returns_null_over_null_inputs(&self, _database: &Self::DB) -> bool {
        false
    }

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
        self.language().map(|language| stored_identifier(language, self.language_is_quoted()))
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
    /// [`referenced_functions`](FunctionLike::referenced_functions),
    /// [`unqualified_names`](FunctionLike::unqualified_names) and
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
    /// the body runs code outside it, through `CALL`, `EXECUTE` or `DO`, or
    /// changes `search_path` at run time before an unqualified name, through
    /// `set_config` or a call to a function whose body, or a function it
    /// calls, may leave the path changed for its caller
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

    /// Returns every relation and function name the body writes without a
    /// schema, so that the path in force when the function runs decides what
    /// it reaches, each with what it reached under the path
    /// [`referenced_relations`](FunctionLike::referenced_relations) and
    /// [`referenced_functions`](FunctionLike::referenced_functions) resolve
    /// it under.
    ///
    /// The body is read by the rules of those readers, so a `WITH` item, an
    /// alias, a parameter, a name a PL/pgSQL body declares and an expression
    /// written like a call, such as `COALESCE`, is no name here, and only a
    /// name the body itself writes counts, never one in the definition of a
    /// view it reads. A name after a straight-line `SET search_path` is still
    /// left to a path, the one the body set, and resolves under it. A call
    /// counts whatever schema it reached, `pg_catalog` included, since a path
    /// placing another schema ahead of `pg_catalog` redirects it too. A name
    /// reaching nothing the database holds counts as well, with no relation or
    /// no function, since a caller may supply one.
    ///
    /// A body [bound at creation](FunctionLike::body_is_bound) answers none,
    /// since it keeps what it reached then, whatever path is in effect.
    /// Names are answered in the order of the statements writing them, a
    /// statement's relations ahead of its calls, each spelling reaching the
    /// same objects once.
    ///
    /// # Errors
    ///
    /// The errors of [`referenced_tables`](FunctionLike::referenced_tables)
    /// for a body read each time the function runs, an unqualified name a
    /// path change in a branch, a loop or a block with a handler may reach
    /// among them ([`LookupError::FunctionBodyUnresolvable`]).
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE items (v INT);
    ///      CREATE TABLE fixed_items (v INT);
    ///      CREATE FUNCTION helper(x INT) RETURNS INT LANGUAGE sql RETURN x;
    ///      CREATE FUNCTION totals(x INT) RETURNS INT LANGUAGE sql AS $$
    ///          WITH local_rows AS (SELECT v FROM items)
    ///          SELECT helper(r.v) + f.v FROM local_rows r CROSS JOIN public.fixed_items f
    ///      $$;",
    /// )?;
    /// let totals = db
    ///     .function_by_target(TargetName::new("totals", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let names = totals.unqualified_names(&db)?;
    /// // A caller's temporary `items` would shadow `public.items` here.
    /// assert!(matches!(
    ///     names.as_slice(),
    ///     [
    ///         UnqualifiedName::Relation { name: items, relation: Some(Relation::Table(_)) },
    ///         UnqualifiedName::Function { name: helper, functions },
    ///     ] if items.name() == "items" && helper.name() == "helper" && functions.len() == 1
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn unqualified_names<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<UnqualifiedName<'db, Self::DB>>, LookupError> {
        body_unqualified_names(self, database)
    }

    /// Returns the dynamic-execution sites of the body, owning the parsed
    /// body, whose [`sites`](DynamicExecutionSites::sites) are every place
    /// the body runs SQL text, in body order through nested blocks,
    /// branches, loops and exception handlers. Body order says where each
    /// sits, never that a branch or a loop runs.
    ///
    /// A site is an `EXECUTE`, `RETURN QUERY EXECUTE`, `OPEN ... FOR EXECUTE`
    /// or `FOR ... IN EXECUTE` of a PL/pgSQL body, or a call PostgreSQL 18
    /// resolves to one of the `pg_catalog` functions running the text an
    /// argument gives, the first argument of `query_to_xml`,
    /// `query_to_xmlschema`, `query_to_xml_and_xmlschema` and `ts_stat` and
    /// the second of `ts_rewrite(tsquery, text)`. A call is read in any
    /// statement or expression of a PL/pgSQL body, a default, a condition,
    /// an `EXECUTE`'s command and `USING` expressions and a cursor's
    /// arguments and query where it opens included, and in a SQL body,
    /// quoted, `RETURN` or `BEGIN ATOMIC`, a `FROM` item included. An
    /// `EXECUTE` follows the calls its command and `USING` expressions
    /// write, which run first. A call resolving to a function of the same
    /// name in another schema is no site.
    ///
    /// Each site holds the expression building its command, an
    /// `EXECUTE`'s `USING` expressions, the function a call runs, and the
    /// [`ExpressionContext`](crate::structs::ExpressionContext) of the SQL
    /// holding the command. There a column of an enclosing query binds
    /// first, then in a PL/pgSQL body the variable the name reaches as
    /// [`ColumnDefinition::Variable`](crate::structs::ColumnDefinition), a
    /// local hiding an argument of its name in an enclosing block, which the
    /// block's label or the function's name still reaches, and `$n` types as
    /// argument `n`, and in a SQL body the input parameter of the name.
    /// Calls and operators of a PL/pgSQL body resolve under the path the body
    /// runs the site with. The SQL a command builds is never read, except
    /// that a constant `EXECUTE` command setting `search_path` sets it for
    /// later sites. A command built at run time, a `CALL` and any call to
    /// code that may set its own path leave later sites under no known path.
    ///
    /// A body in another language answers no site.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyNotSql`] for a body in a language
    /// sql-traits does not read, [`LookupError::FunctionBodyUnavailable`] when
    /// the source is unavailable, [`LookupError::FunctionBodyUnparsable`] for
    /// a body that does not parse, a PL/pgSQL block declaring a name twice
    /// included, and the errors reading the function's own `search_path`
    /// setting.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::Expr, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (name varchar(10));
    ///      CREATE FUNCTION count_of(p text) RETURNS bigint LANGUAGE plpgsql AS $$
    ///      DECLARE result bigint;
    ///      BEGIN
    ///          EXECUTE 'SELECT count(*) FROM t WHERE name = ''' || p || '''' INTO result;
    ///          RETURN result;
    ///      END $$;
    ///      CREATE FUNCTION as_xml(p text) RETURNS SETOF xml LANGUAGE sql
    ///      RETURN (SELECT query_to_xml('SELECT ' || name, false, false, '') FROM t);",
    /// )?;
    /// let text = TargetName::new("text", false).with_schema("pg_catalog", false);
    /// let count_of = db
    ///     .function_by_target(TargetName::new("count_of", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = count_of.dynamic_execution_sites(&db)?;
    /// let sites = body.sites()?;
    /// let [site] = sites.as_slice() else { panic!("one site") };
    /// let context = site.context(IdentifierCase::Folded)?;
    /// let Expr::BinaryOp { left, .. } = site.command() else { panic!("a concatenation") };
    /// let Expr::BinaryOp { right: p, .. } = left.as_ref() else { panic!("a concatenation") };
    /// let Some((ColumnDefinition::Variable(parameter), _)) = context.reference(p)? else {
    ///     panic!("p binds the argument")
    /// };
    /// assert!(matches!(parameter.kind(), VariableKind::Parameter { position: 1, .. }));
    /// assert_eq!(context.type_of(p)?, OperandType::Known { type_name: text, typmod: None });
    ///
    /// let as_xml = db
    ///     .function_by_target(TargetName::new("as_xml", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = as_xml.dynamic_execution_sites(&db)?;
    /// let sites = body.sites()?;
    /// let [site] = sites.as_slice() else { panic!("one site") };
    /// assert_eq!(site.function().map(|called| called.name()), Some("query_to_xml"));
    /// let context = site.context(IdentifierCase::Folded)?;
    /// let Expr::BinaryOp { right: name, .. } = site.command() else { panic!("a concatenation") };
    /// // The column of `t` the enclosing query reads.
    /// let varchar = TargetName::new("varchar", false).with_schema("pg_catalog", false);
    /// assert_eq!(context.type_of(name)?, OperandType::Known { type_name: varchar, typmod: Some(14) });
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn dynamic_execution_sites<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<DynamicExecutionSites<'db, Self::DB>, LookupError> {
        body_site_source(self, database).map(DynamicExecutionSites::new)
    }

    /// Returns the statements and expressions of a PL/pgSQL body, owning the
    /// parsed body, whose [`sites`](PlPgSqlSites::sites) are every piece of
    /// SQL PostgreSQL 18 compiles while the body runs, in body order through
    /// nested blocks, branches, loops and exception handlers.
    ///
    /// A site is a statement, the query of a `FOR` loop or of a cursor where
    /// it opens, or the `SELECT` evaluating an expression, such as a
    /// condition, an assigned value, a loop bound or array, a default, a
    /// cursor argument, a `RETURN` value, and an `EXECUTE`'s command and each
    /// `USING` argument by itself. A `SET` or `RESET` of `search_path`, which
    /// reads no name, is no site.
    ///
    /// Each site's [`context`](crate::structs::PlPgSqlSite::context) binds
    /// its column references as
    /// [`ExpressionScope::PlPgSql`](crate::structs::ExpressionScope::PlPgSql)
    /// describes, under the variables in reach where the site sits, and
    /// [`ExpressionContext::variable`](crate::structs::ExpressionContext::variable)
    /// answers the variable a column reference's name also reaches.
    ///
    /// A body in SQL answers no site.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyNotSql`] for a body in a language
    /// sql-traits does not read, [`LookupError::FunctionBodyUnavailable`] when
    /// the source is unavailable, [`LookupError::FunctionBodyUnparsable`] for
    /// a body that does not parse, a PL/pgSQL block declaring a name twice
    /// included, and the errors reading the function's own `search_path`
    /// setting.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE notes (id integer PRIMARY KEY);
    ///      CREATE FUNCTION find_note(id integer) RETURNS integer LANGUAGE plpgsql AS $$
    ///      DECLARE result integer;
    ///      BEGIN
    ///          SELECT id INTO result FROM notes;
    ///          RETURN result;
    ///      END $$;",
    /// )?;
    /// let find_note = db
    ///     .function_by_target(TargetName::new("find_note", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = find_note.plpgsql_sites(&db)?;
    /// let sites = body.sites()?;
    /// let [select, _returned] = sites.as_slice() else { panic!("two sites") };
    /// let EmbeddedSql::Statement(Statement::Query(query)) = select.sql() else { panic!("a query") };
    /// let SetExpr::Select(projection) = query.body.as_ref() else { panic!("a select") };
    /// let SelectItem::UnnamedExpr(id) = &projection.projection[0] else { panic!("one column") };
    /// let context = select.context(IdentifierCase::Folded)?;
    /// // The column binds, and PostgreSQL 18 refuses the name as ambiguous.
    /// assert!(matches!(context.reference(id)?, Some((ColumnDefinition::Base { .. }, _))));
    /// let parameter = context.variable(id)?.expect("the argument shares the name");
    /// assert!(matches!(parameter.kind(), VariableKind::Parameter { position: 1, .. }));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn plpgsql_sites<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<PlPgSqlSites<'db, Self::DB>, LookupError> {
        body_site_source(self, database).map(PlPgSqlSites::new)
    }

    /// Returns the first read the function makes, when each of `callers`
    /// executes it, as the owner of a `SECURITY DEFINER` function, itself or
    /// one it reaches, that may reach a row of a table with row-level
    /// security the caller cannot read, or `None` when every such read
    /// reaches only rows each caller's own `SELECT` policies admit, of which
    /// it may select every column.
    ///
    /// A read is confined by the facts PostgreSQL holds of the rows it
    /// reaches: the conditions of its `WHERE` and of the `ON` of the joins
    /// that drop rows, those of the queries around it, a `LANGUAGE sql`
    /// function's parameters, the sets a column is tested against, declared
    /// through [`Deployment::with_set_within`] or inferred to lie within a
    /// policy's set, and what the rows a helper draws a
    /// tested value from meet, carried across the keys that hold them equal.
    /// A `SECURITY DEFINER` function a caller may execute directly discloses
    /// nothing a direct call does not, so its reads count only as the values
    /// it hands on. A PL/pgSQL body is read statement by statement, a
    /// parameter counting as one where the statement reads the argument
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors reading the schema runs into. A body that
    /// cannot be read is an [`crate::structs::Unconfinement::Unfollowed`] read
    /// instead.
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
    ///      CREATE TABLE notes (id INT PRIMARY KEY, team_id INT NOT NULL, body TEXT);
    ///      ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
    ///      CREATE POLICY notes_select ON notes FOR SELECT TO app
    ///          USING (team_id = current_setting('app.team_id')::INT);
    ///      GRANT SELECT ON notes TO app;
    ///      CREATE FUNCTION team_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
    ///      SECURITY DEFINER SET search_path = pg_catalog AS $$
    ///          SELECT body FROM public.notes WHERE team_id = current_setting('app.team_id')::INT
    ///      $$;
    ///      CREATE FUNCTION every_note() RETURNS SETOF TEXT LANGUAGE sql STABLE
    ///      SECURITY DEFINER SET search_path = pg_catalog AS $$
    ///          SELECT body FROM public.notes
    ///      $$;",
    /// )?;
    /// let deployment = Deployment::new("app", ["app"], []).expect("a valid deployment");
    /// let app = ActingRole::Declared(db.role("app").expect("the role exists"));
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    /// };
    /// let team_notes = function("team_notes")?.expect("the function exists");
    /// assert!(team_notes.unconfined_definer_read(&db, &deployment, &[app])?.is_none());
    ///
    /// let every_note = function("every_note")?.expect("the function exists");
    /// let read = every_note
    ///     .unconfined_definer_read(&db, &deployment, &[app])?
    ///     .expect("every row is read");
    /// assert_eq!(read.table().map(TableLike::table_name), Some("notes"));
    /// assert!(matches!(read.reason(), Unconfinement::Permissive(policies) if policies.len() == 1));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn unconfined_definer_read<'db>(
        &'db self,
        database: &'db Self::DB,
        deployment: &'db Deployment,
        callers: &[ActingRole<'db, <Self::DB as DatabaseLike>::Role>],
    ) -> Result<Option<UnconfinedRead<'db, Self::DB>>, LookupError> {
        unconfined_function_read(database, deployment, self, callers)
    }

    /// Returns the triggers that execute the function, row-level and
    /// statement-level alike, in the order `database` holds them. The reverse
    /// of [`TriggerLike::function`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE a (id INT);
    ///      CREATE TABLE b (id INT);
    ///      CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
    ///      CREATE FUNCTION idle() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
    ///      CREATE TRIGGER on_a BEFORE INSERT ON a FOR EACH ROW EXECUTE FUNCTION touch();
    ///      CREATE TRIGGER on_b AFTER UPDATE ON b FOR EACH STATEMENT EXECUTE FUNCTION touch();",
    /// )?;
    /// let function = |name| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .map(|found| found.expect("Function should exist"))
    /// };
    /// let names: Vec<&str> = function("touch")?.triggers(&db).map(TriggerLike::name).collect();
    /// assert_eq!(names, ["on_a", "on_b"]);
    /// assert_eq!(function("idle")?.triggers(&db).count(), 0);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn triggers<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Trigger>
    where
        Self: 'db,
    {
        database.triggers().filter(move |trigger| {
            trigger.function(database).is_some_and(|function| core::ptr::eq(function, self))
        })
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

    /// Returns whether the call is `NULL` only when an argument
    /// [`returns_null_on_null_argument`](Self::returns_null_on_null_argument)
    /// names is `NULL`. The default answers false, which is safe for a
    /// function whose `NULL` results are unknown.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION stamp() RETURNS TIMESTAMPTZ LANGUAGE sql AS 'SELECT now()';",
    /// )?;
    /// let builtin = |name: &str, arguments: usize| {
    ///     db.functions()
    ///         .find(|function| {
    ///             function.name() == name && function.argument_type_names(&db).count() == arguments
    ///         })
    ///         .expect("the builtin is registered")
    /// };
    /// assert!(builtin("now", 0).returns_null_only_on_null_input(&db));
    /// assert!(builtin("current_setting", 1).returns_null_only_on_null_input(&db));
    /// // `current_setting(name, true)` is `NULL` for an unknown setting.
    /// assert!(!builtin("current_setting", 2).returns_null_only_on_null_input(&db));
    /// let stamp = db
    ///     .function_by_target(TargetName::new("stamp", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// assert!(!stamp.returns_null_only_on_null_input(&db));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn returns_null_only_on_null_input(&self, database: &Self::DB) -> bool {
        let _ = database;
        false
    }

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

    /// Returns whether the function is marked `LEAKPROOF`, which lets
    /// PostgreSQL evaluate it ahead of row-level security policies and
    /// [security-barrier](crate::traits::ViewLike::is_security_barrier) view
    /// conditions, so its errors and notices may reveal rows those conditions
    /// would hide.
    ///
    /// A declared function answers `false` when its declaration writes
    /// `NOT LEAKPROOF` or omits the attribute, which is how `pg_dump` writes a
    /// function that is not leakproof. `ALTER FUNCTION ... LEAKPROOF` and
    /// `ALTER FUNCTION ... NOT LEAKPROOF` apply in statement order, and
    /// `CREATE OR REPLACE FUNCTION` takes the replacement's declaration. A
    /// builtin [`ParserDB`](crate::structs::ParserDB) registers in
    /// `pg_catalog` answers `pg_proc.proleakproof` from PostgreSQL 18, so
    /// `int4eq(integer, integer)` is leakproof and `lower(text)` is not. A
    /// function an extension or a range type creates is not leakproof, as
    /// PostgreSQL creates them.
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
    /// CREATE FUNCTION marked(value INT) RETURNS BOOLEAN LANGUAGE sql LEAKPROOF AS 'SELECT true';
    /// CREATE FUNCTION unmarked(value INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';
    /// CREATE FUNCTION altered(value INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';
    /// ALTER FUNCTION altered(INT) LEAKPROOF;
    /// ",
    /// )?;
    /// let leakproof = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .is_leakproof()
    /// };
    /// assert!(leakproof("marked"));
    /// assert!(!leakproof("unmarked"));
    /// assert!(leakproof("altered"));
    /// # Ok(())
    /// # }
    /// ```
    fn is_leakproof(&self) -> bool;

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

    /// Returns the extension the function is a member of, the one whose
    /// install script created it.
    ///
    /// PostgreSQL refuses `DROP FUNCTION` on a member and drops it along with
    /// its extension, and the next `ALTER EXTENSION ... UPDATE` may replace
    /// whatever an `ALTER FUNCTION` changed on it, so the schema owns neither
    /// its existence nor its definition. A function the schema created answers
    /// `None`, also once `ALTER FUNCTION ... DEPENDS ON EXTENSION` ties it to
    /// an extension, since the schema still owns its definition.
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
    ///     r#"
    /// CREATE EXTENSION "uuid-ossp";
    /// CREATE FUNCTION mine() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    /// CREATE FUNCTION tied() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    /// ALTER FUNCTION tied() DEPENDS ON EXTENSION "uuid-ossp";
    /// "#,
    /// )?;
    /// let extension = |name: &str| {
    ///     db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .expect("Function should exist")
    ///         .extension(&db)
    ///         .map(|extension| extension.map(ExtensionLike::name))
    /// };
    /// assert_eq!(extension("uuid_generate_v4")?, Some("uuid-ossp"));
    /// assert_eq!(extension("mine")?, None);
    /// assert_eq!(extension("tied")?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn extension<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Extension>, LookupError>;

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

    /// What this helper returns for a request of `deployment`, called with
    /// `arguments`, as a union of grant arms, parent arms and called arms,
    /// walked along a tree where the body is recursive.
    ///
    /// A helper answers when it is a `LANGUAGE sql STABLE SECURITY DEFINER`
    /// function returning `SETOF` one scalar type or `TABLE (...)` with a
    /// pinned `search_path`, owned by a declared role holding `SELECT` on
    /// every table it reads, or on every column of it the body names,
    /// `USAGE` on the schema of every table and helper the body names, and
    /// exempt from the row-level security of the tables, and when its body
    /// is a `UNION` of arms of three shapes, alone or walked along a tree. A
    /// helper returning `TABLE (...)` of several columns answers no parent
    /// arm.
    ///
    /// An arm may cross its rows with lists of labels of an enum type, each
    /// a `CROSS JOIN` of `unnest(enum_range(NULL::<type>))` or of `(VALUES
    /// (<label>), ...)`, which the arm returns as one column the helper
    /// declares in that type (see [`Returned`](crate::structs::Returned)).
    /// No condition names a label, and a list the arm does not return, or a
    /// bare name both a list and a row of the arm hold, refuses the answer.
    ///
    /// - A grant arm, `SELECT <columns> FROM <table> [WHERE <conditions>]`, one
    ///   column per column the helper returns, the conditions joined by `AND`.
    ///   The ones naming the grantee are comparisons of columns with values of
    ///   the request, any number of them all holding on the arm's row, or, in
    ///   an arm writing none, one `<column> IN (SELECT <helper>(<arguments>))`,
    ///   the helper answered in turn as a parent arm's is. A comparison is
    ///   `<column> = current_setting('<setting>'[, <missing_ok>])` with a
    ///   setting the deployment declares as the caller, as another principal or
    ///   as a scalar, a scalar read optionally through `lower(...)`, or
    ///   `<column>` compared with each element of a setting it declares as a
    ///   set, unpacked as declared (see
    ///   [`SettingComparison`](crate::structs::SettingComparison)). `<column>
    ///   IN (<values>)` listing principals or scalars beside literals and
    ///   parameters answers one grant arm per read and one granted to anyone
    ///   for the literals together, each keeping the other conditions, the
    ///   comparisons among them included. An arm naming no grantee returns the
    ///   same rows for every request its gate lets through. A cap,
    ///   `(<operands>) IN (SELECT * FROM <helper>(<arguments>))`, optionally
    ///   behind `<setting> IS NULL OR`, keeps the rows whose operands are a row
    ///   the helper returns, the helper answered in turn (see
    ///   [`RowCap`](crate::structs::RowCap)). In an arm naming its grantee by a
    ///   value of the request, `<column> IN (SELECT <helper>(<arguments>))` on
    ///   a column of its table is a cap of one operand. Conditions
    ///   `current_setting('<setting>') <> <text>` and
    ///   `current_setting('<setting>') IS NOT NULL` on a caller setting gate
    ///   the arm on what the setting reads (see
    ///   [`CallerGate`](crate::structs::CallerGate)). A condition
    ///   `current_setting('<setting>') IS NULL` on any declared setting keeps
    ///   the arm's rows only for a request leaving it unset (see
    ///   [`UnsetSetting`](crate::structs::UnsetSetting)), read where
    ///   `conditions` names [`ArmCondition::UnsetSetting`] and refusing the
    ///   answer otherwise. The body may cast a read of one value to its
    ///   declared type, and a set to an array of its declared element type or
    ///   each element to that type, and any other cast refuses the answer. It
    ///   may read any setting through `NULLIF(current_setting(...), '')`, which
    ///   reads `''` as `NULL` and so maps both states of an unset setting to
    ///   `NULL`. Wherever it reads a setting it may call an accessor instead, a
    ///   zero-argument `sql` function written as a string whose body selects
    ///   one such read, cast or not, and returns its type, and wherever it
    ///   builds the array of a set it may call an accessor whose body selects
    ///   that array and returns its array type. The accessor's body resolves on
    ///   its own path or, where it pins none, on the caller's, as the role it
    ///   runs as, and an accessor that is `IMMUTABLE` or sets the setting it
    ///   reads refuses the answer. A time bound is a conjunct `<column>
    ///   <comparison> now()`, a `timestamptz` column of the table compared with
    ///   `<`, `<=`, `>` or `>=` either way round, optionally `OR <column> IS
    ///   NULL`, and the arm answers it as a
    ///   [`TimeBound`](crate::structs::TimeBound). `CURRENT_TIMESTAMP` and
    ///   `transaction_timestamp()` read the same statement time as `now()`. An
    ///   anti-join is a conjunct `NOT EXISTS (SELECT 1 FROM <table> WHERE
    ///   <conditions>)` whose conditions equal columns of `<table>` with
    ///   columns of the arm's table and otherwise read `<table>` alone, and the
    ///   arm answers it as an [`AntiJoin`](crate::structs::AntiJoin). An
    ///   exclusion, a conjunct `<column> NOT IN (SELECT <helper>(<arguments>))`
    ///   or another spelling of it, is read where `conditions` names
    ///   [`ArmCondition::Exclusion`] and refuses the answer otherwise (see
    ///   [`ExcludedSet`](crate::structs::ExcludedSet)). The rest, the arguments
    ///   substituted, reduce to `TRUE`, to `FALSE` or `NULL`, or to what
    ///   columns have to hold, literal values, a `boolean` column alone holding
    ///   `TRUE`, `<column> IS NULL` or `<column> IS NOT NULL` (see
    ///   [`Restriction`](crate::structs::Restriction)). A column of an enum
    ///   type compared with a label by `<`, `<=`, `>` or `>=`, either way
    ///   round, holds the labels on the kept side of it in the order the type
    ///   declares them, `ALTER TYPE ... ADD VALUE` applied. The table may be
    ///   followed by `[INNER] JOIN <table> ON <condition>` for each row it
    ///   names through a foreign key, the conditions in `ON` read as those in
    ///   `WHERE`. Each column of the key equated with the column it references
    ///   joins the row, and the conditions on that row alone reduce to a
    ///   [`JoinedRow`](crate::structs::JoinedRow), the grantee and the returned
    ///   columns staying on the arm's own table. A cap may take operands from
    ///   the joined rows as well. The arm's table's primary key, returned by
    ///   the arm and referencing no table through a single-column foreign key,
    ///   compared by `<key> IN (SELECT <helper>(<arguments>))` or `<key> IN
    ///   (SELECT * FROM <helper>(<arguments>))`, is a cap of one operand,
    ///   keeping the rows whose key the helper returns. Any other arm naming no
    ///   grantee by a value of the request and comparing a helper's set while
    ///   returning a key it would return as a parent arm is one, so it joins no
    ///   row and compares no other set but in an exclusion.
    /// - A parent arm, `SELECT <key> FROM <table> WHERE <column> IN (SELECT
    ///   <helper>(<arguments>)) [AND <conditions>]`, each argument a parameter
    ///   or a literal, the helper called answered in turn for the arguments it
    ///   receives. `<key>` is the table's primary key, and the column names its
    ///   parent through its one single-column foreign key or, where it has
    ///   none, through the table the called helper's set covers. `<key>` is
    ///   otherwise a column a unique key covers alone, referencing another
    ///   table's primary key through its one single-column foreign key, as in
    ///   an edge table holding the parent of each child, and the column, one
    ///   other than `<key>`, has one single-column foreign key. The conditions
    ///   reduce to restrictions and bounds against the statement time the way a
    ///   grant arm's do, or exclude a column from another helper's set the way
    ///   a grant arm's exclusion does. They are read where `conditions` names
    ///   [`ArmCondition::ParentCondition`], and for an exclusion
    ///   [`ArmCondition::Exclusion`] as well, refusing the answer otherwise. An
    ///   arm whose conditions keep no row is left out.
    /// - A called arm, `SELECT <columns> FROM <helper>(<arguments>) [<alias>
    ///   [(<names>)]]`, returning columns of the rows another helper returns,
    ///   answered in turn, crossed with lists of labels and with no other
    ///   condition (see [`CalledArm`](crate::structs::CalledArm)).
    ///
    /// A body walking a tree, or any graph of edges, is `WITH RECURSIVE
    /// <walk> [(<columns>)] AS (<arms> UNION <term>) SELECT <columns> FROM
    /// <walk>`, the walk's columns being every column the helper returns, in
    /// its order. The term walks down as `SELECT <children> FROM <table> JOIN
    /// <walk> ON <parents> = <walk>.<columns>` and up as `SELECT <parents>
    /// FROM <table> JOIN <walk> ON <children> = <walk>.<columns>`, the join
    /// either way round, each column of the walk compared with the column of
    /// the table at its position, one equality each or the columns together
    /// as rows. A term walking several edges at once is `SELECT
    /// <step>.<columns> FROM <walk> CROSS JOIN LATERAL (<edges>) <step>`, or
    /// `FROM <walk>, LATERAL`, the edges a `UNION` or `UNION ALL` of `SELECT
    /// <children> FROM <table> WHERE <parents> = <walk>.<columns>` walking
    /// down and `SELECT <parents> FROM <table> WHERE <children> =
    /// <walk>.<columns>` walking up. `<parents>` name a row of the tree
    /// through a foreign key holding each of them at the position of a
    /// different column of the tree's primary key, which they cover, alone
    /// or among other columns, and checking every row naming one, its match
    /// `FULL` or its other columns holding a value on each such row, as
    /// `NOT NULL` or the table's validated checks require. The table is the
    /// tree itself, with `<children>` its primary key, or an edge table, with
    /// `<children>` naming a row of the tree the same way, each column holding
    /// the same column of the key as the parent column at its position. The
    /// tree is the one table the two lists name a row of, so a key into a table
    /// only one of them names constrains the edge without changing the tree,
    /// and two tables both name refuse the walk. `<children>` are the table's
    /// primary key or columns a unique key covers alone where one of the two
    /// lists is, and otherwise the columns the term adds, the walk then going
    /// down. The term may follow only the edges meeting conditions beside its
    /// comparison with the walk, in `ON` and in `WHERE` of a join and in
    /// `WHERE` of a `LATERAL` edge. They name columns of the edge table
    /// alone, never the walk's row, and reduce to restrictions and
    /// bounds against the statement time the way a grant arm's do, or exclude
    /// a column from another helper's set the way a grant arm's exclusion
    /// does, a set reaching the walking helper again being a call cycle. They
    /// are read where `conditions` names [`ArmCondition::EdgeCondition`], and
    /// for an exclusion [`ArmCondition::Exclusion`] as well, refusing the
    /// answer otherwise. The walk answers a
    /// [`WalkArm`](crate::structs::WalkArm) per edge after the arms, and none
    /// for an edge whose conditions keep no row. `UNION ALL` between the arms
    /// and the term is refused, since it never ends on edges closing a cycle.
    ///
    /// The final query may instead reselect what a walk of one column
    /// reached, `SELECT <key> FROM <table> WHERE <key> IN (SELECT <column>
    /// FROM <walk>) [AND <conditions>]`, where `<table>` is the one the set
    /// covers (see [`HelperSet::covered_table`]), so the walk reaches keys of
    /// it alone. The conditions are read as a grant arm's and answered as the
    /// [`selection`](HelperSet::selection), left out where they keep every
    /// row.
    ///
    /// A grant arm whose conditions reduce to `FALSE` or `NULL` keeps no row
    /// and is left out, and the walks with it where no other arm remains. A
    /// column named like a parameter is the column, as PostgreSQL resolves
    /// names in a SQL function. An unqualified name is looked up past the
    /// schemas on the path the owner cannot use, as PostgreSQL leaves them out
    /// of the owner's path.
    ///
    /// An arm writing a condition `conditions` does not name refuses the
    /// answer, so a consumer ignoring such conditions never keeps a row they
    /// take away. Every helper the body calls is read with the same
    /// `conditions`.
    ///
    /// # Arguments
    ///
    /// * `arguments` - One literal per input parameter.
    /// * `deployment` - The deployment declaring the settings the arms compare
    ///   with.
    /// * `database` - The database the helper belongs to.
    /// * `conditions` - The conditions the consumer reads beside those every
    ///   answer carries, each of which it applies or refuses.
    ///
    /// # Errors
    ///
    /// Returns the [`HelperSetError`] naming why the helper, or a helper its
    /// body calls, refuses to answer.
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
    /// let set = docs_with.helper_set(
    ///     &[Value::SingleQuotedString("write".into())],
    ///     &deployment,
    ///     &db,
    ///     &[],
    /// )?;
    /// let HelperArm::Grant(grant) = &set.arms()[0] else { unreachable!() };
    /// let [Returned::Read(doc)] = grant.returned() else { panic!("the helper returns one column") };
    /// assert_eq!(doc.column_name(), "doc_id");
    /// let GrantedTo::Request(compared) = grant.granted_to() else { unreachable!() };
    /// let [request] = compared.as_slice() else { panic!("the arm compares one column") };
    /// assert_eq!(request.column().column_name(), "principal_id");
    /// let [Restriction::In(roles)] = grant.restrictions() else {
    ///     panic!("the condition restricts one column to literals")
    /// };
    /// assert_eq!(roles.column().column_name(), "role");
    /// let admin = Value::SingleQuotedString("admin".into());
    /// let write = Value::SingleQuotedString("write".into());
    /// assert_eq!(roles.values(), [admin, write]);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// An exclusion is read only for a consumer naming it:
    ///
    /// ```rust
    /// use sql_traits::{errors::HelperSetError, prelude::*};
    /// use sqlparser::{ast::DataType, dialect::PostgreSqlDialect};
    ///
    /// # fn main() -> Result<(), HelperSetError> {
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
    ///      CREATE TABLE projects (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL);
    ///      CREATE TABLE project_blocks (project_id TEXT NOT NULL REFERENCES projects (id),
    ///        principal_id TEXT NOT NULL, PRIMARY KEY (project_id, principal_id));
    ///      CREATE FUNCTION blocked_projects() RETURNS SETOF TEXT
    ///      LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    ///        SELECT project_id FROM project_blocks
    ///        WHERE principal_id = current_setting('app.user_id', true)
    ///      $$;
    ///      CREATE FUNCTION visible_projects() RETURNS SETOF TEXT
    ///      LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    ///        SELECT id FROM projects
    ///        WHERE owner_id = current_setting('app.user_id', true)
    ///          AND NOT (id = ANY (ARRAY(SELECT blocked_projects())))
    ///      $$;
    ///      ALTER FUNCTION blocked_projects() OWNER TO authz_owner;
    ///      ALTER FUNCTION visible_projects() OWNER TO authz_owner;
    ///      GRANT SELECT ON projects, project_blocks TO authz_owner;",
    /// )
    /// .expect("the schema parses");
    /// let caller = SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))
    ///     .expect("a custom setting name");
    /// let deployment = Deployment::new("app", ["app"], [caller]).expect("a valid deployment");
    /// let visible = db
    ///     .function_by_target(TargetName::new("visible_projects", false), IdentifierCase::AsWritten)?
    ///     .expect("the helper exists");
    ///
    /// assert!(matches!(
    ///     visible.helper_set(&[], &deployment, &db, &[]),
    ///     Err(HelperSetError::UnreadCondition { kind: ArmCondition::Exclusion, .. })
    /// ));
    /// let set = visible.helper_set(&[], &deployment, &db, &[ArmCondition::Exclusion])?;
    /// let HelperArm::Grant(grant) = &set.arms()[0] else { unreachable!() };
    /// let [blocked] = grant.exclusions() else { panic!("the arm excludes one set") };
    /// assert_eq!(blocked.column().column_name(), "id");
    /// assert_eq!(blocked.function().name(), "blocked_projects");
    /// let HelperArm::Grant(block) = &blocked.set().arms()[0] else { unreachable!() };
    /// let GrantedTo::Request(compared) = block.granted_to() else { unreachable!() };
    /// let [request] = compared.as_slice() else { panic!("the arm compares one column") };
    /// assert_eq!(request.column().column_name(), "principal_id");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn helper_set<'db>(
        &'db self,
        arguments: &[Value],
        deployment: &'db Deployment,
        database: &'db Self::DB,
        conditions: &[ArmCondition],
    ) -> Result<HelperSet<'db, Self::DB>, HelperSetError> {
        helper_set(self, arguments, deployment, database, conditions)
    }

    /// Returns what each output parameter of the `plpgsql` function is
    /// assigned when it returns, in declaration order, on every request
    /// `deployment` admits.
    ///
    /// A parameter holds the one value every definition reaching a return
    /// gives it. That is an expression reading no variable of the body, the
    /// `NULL` it starts with, or neither, as [`ReturnedOutput`] tells.
    ///
    /// The answer holds only where the function returns. Each request is
    /// read with what the deployment declares its settings hold, so
    /// `current_setting(name, true)`, `NULLIF` of it and `''`, and either
    /// cast to the setting's declared type are read as the setting set and
    /// unset, and settings set together as
    /// [`Deployment::with_settings_set_together`] declares are set or unset
    /// as a whole. A setting the function's `SET` clause names, or the body
    /// may change through `SET`, `RESET` or `set_config`, is read as any
    /// text, and so is every expression once the body may change a built-in
    /// parameter such as `role`. A body running code it does not hold, a
    /// function outside `pg_catalog`, dynamic SQL, a relation a query reads,
    /// a statement other than a query, `SET` and `RESET`, an operator or a
    /// domain check calling such a function, a domain check calling
    /// `set_config`, or a cursor the caller opened, refuses the function,
    /// since that code may raise or set a setting unseen. A raise a path
    /// reaches and no handler of the body catches, `RAISE EXCEPTION`, a
    /// failing `ASSERT` and a cast that may fail among them, refuses the
    /// function, and so does a body no request returns from.
    ///
    /// [`ReturnedOutput`]: crate::structs::ReturnedOutput
    ///
    /// # Errors
    ///
    /// Returns [`ReturnedValuesError::Language`] for a function in another
    /// language, [`ReturnedValuesError::ReturnsSet`] for a set-returning
    /// one, [`ReturnedValuesError::NoOutputParameters`] for one without
    /// output parameters, [`ReturnedValuesError::RunsCode`] naming the first
    /// code it runs that it does not hold, [`ReturnedValuesError::Raises`]
    /// naming the first raise a path reaches on some request,
    /// [`ReturnedValuesError::NeverReturns`] when no path returns, and the
    /// errors [`plpgsql_sites`](FunctionLike::plpgsql_sites) returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{errors::ReturnedValuesError, prelude::*};
    /// use sqlparser::{ast::DataType, dialect::PostgreSqlDialect};
    ///
    /// # fn main() -> Result<(), ReturnedValuesError> {
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION acting_installation(OUT namespace_id UUID, OUT id UUID)
    ///      LANGUAGE plpgsql STABLE SET search_path = pg_catalog, pg_temp AS $$
    ///      BEGIN
    ///        namespace_id := NULLIF(current_setting('app.installation_namespace_id', true), '')::UUID;
    ///        id := NULLIF(current_setting('app.installation_id', true), '')::UUID;
    ///        IF (namespace_id IS NULL) <> (id IS NULL) THEN
    ///          RAISE EXCEPTION 'incomplete installation identity' USING ERRCODE = '22023';
    ///        END IF;
    ///      END $$;",
    /// )
    /// .expect("the schema parses");
    /// let setting = |name| {
    ///     SessionSetting::value(name, SettingValue::Scalar(DataType::Uuid))
    ///         .expect("a custom setting name")
    ///         .may_be_unset()
    /// };
    /// let settings = [setting("app.installation_namespace_id"), setting("app.installation_id")];
    /// let deployment = Deployment::new("app", ["app"], settings).expect("a valid deployment");
    /// let validator = db
    ///     .function_by_target(
    ///         TargetName::new("acting_installation", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .expect("the validator exists");
    ///
    /// // Either setting alone may be set, so the raise is reachable.
    /// assert!(matches!(
    ///     validator.returned_values(&db, &deployment),
    ///     Err(ReturnedValuesError::Raises { surely: true, .. })
    /// ));
    ///
    /// let deployment = deployment
    ///     .with_settings_set_together(["app.installation_namespace_id", "app.installation_id"])
    ///     .expect("both settings are declared");
    /// let returned = validator.returned_values(&db, &deployment)?;
    /// let [namespace_id, id] = returned.as_slice() else { panic!("two output parameters") };
    /// assert_eq!(namespace_id.name(), "namespace_id");
    /// assert_eq!(
    ///     id.expression().expect("one expression").to_string(),
    ///     "NULLIF(current_setting('app.installation_id', true), '')::UUID"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn returned_values(
        &self,
        database: &Self::DB,
        deployment: &Deployment,
    ) -> Result<Vec<ReturnedValue>, ReturnedValuesError> {
        let language = self.stored_language();
        if language.as_deref() != Some("plpgsql") {
            return Err(ReturnedValuesError::Language {
                function: self.stored_name().into_owned(),
                language: language.map(Cow::into_owned),
            });
        }
        if self.returns_set() {
            return Err(ReturnedValuesError::ReturnsSet {
                function: self.stored_name().into_owned(),
            });
        }
        body_site_source(self, database)?.returned_values(deployment, database.identifier_case())
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
            assert!(error_msg.contains("t.CHECK (is_valid(id))"), "{error_msg}");
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
            assert!(error_msg.contains("my_policy"));
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
            assert!(error_msg.contains("insert_policy"));
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
            assert!(error_msg.contains("my_trigger"));
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
            Err(Error::AlterFunctionNotFound { function_name, .. }) if function_name == "missing_func"
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
    fn test_postgis_members_answer_their_extension() {
        // An unquoted name folds, so `POSTGIS` installs and owns the same
        // functions `postgis` does.
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE EXTENSION POSTGIS VERSION '3.7.0';
             CREATE FUNCTION st_mine() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
        )
        .expect("Failed to parse SQL");
        let members: Vec<_> =
            db.functions().filter(|function| function.name() == "st_geomfromgeojson").collect();
        assert!(!members.is_empty(), "PostGIS registers st_geomfromgeojson");
        for member in members {
            let extension = member.extension(&db).expect("held").map(ExtensionLike::name);
            assert_eq!(extension, Some("POSTGIS"));
        }
        let mine = db.functions().find(|function| function.name() == "st_mine").expect("held");
        assert_eq!(mine.extension(&db), Ok(None));
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
