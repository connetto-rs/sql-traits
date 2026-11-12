//! PostgreSQL casts and exact implementation links.

use alloc::{borrow::Cow, format, string::String, vec::Vec};

use sqlparser::ast::{CreateFunction, ObjectName, OperateFunctionArg};

use super::{
    postgres_catalog::CatalogName,
    postgres_operator::{
        arguments_match, function_name_matches, identity_repeats, operand_spelling, operand_type,
    },
    type_arrays::TypeArrays,
};
use crate::{
    structs::{IdentifierCase, TargetName, generic_db::sqlparser::ParserDB},
    traits::{CastContext, CastLike, CastMethod, DatabaseLike, Metadata},
};

mod postgres_18_casts;
pub(super) use postgres_18_casts::POSTGRES_18_CASTS;

/// A PostgreSQL cast between two types.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostgresCatalogCast {
    /// The type the cast converts from.
    source: CatalogName,
    /// The type the cast converts to.
    target: CatalogName,
    /// The contexts that apply the cast without being asked.
    context: CastContext,
    /// How the cast converts a value.
    implementation: CastImplementation,
}

/// How a cast converts a value, with the function a function cast runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum CastImplementation {
    /// The function converts the value.
    Function(CastFunction),
    /// The types' output and input functions convert the value.
    InOut,
    /// The value is relabelled.
    Binary,
}

/// The exact `pg_proc` signature a function cast runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CastFunction {
    /// The function's schema-qualified name.
    name: CatalogName,
    /// The function's input types, in order.
    arguments: Cow<'static, [CatalogName]>,
}

impl Metadata for PostgresCatalogCast {
    type Meta = ();
}

impl PostgresCatalogCast {
    /// Creates a cast running `function` over `arguments`, an identity without
    /// a schema reading as `pg_catalog`.
    #[must_use]
    pub fn through_function<'a>(
        source: TargetName<'_>,
        target: TargetName<'_>,
        context: CastContext,
        function: TargetName<'_>,
        arguments: impl IntoIterator<Item = TargetName<'a>>,
    ) -> Self {
        let function = CastFunction {
            name: operand_type(function),
            arguments: Cow::Owned(arguments.into_iter().map(operand_type).collect()),
        };
        Self::new(source, target, context, CastImplementation::Function(function))
    }

    /// Creates a cast relabelling values of binary-coercible types.
    #[must_use]
    pub fn binary_coercible(
        source: TargetName<'_>,
        target: TargetName<'_>,
        context: CastContext,
    ) -> Self {
        Self::new(source, target, context, CastImplementation::Binary)
    }

    /// Creates a cast through the types' output and input functions.
    #[must_use]
    pub fn through_io(
        source: TargetName<'_>,
        target: TargetName<'_>,
        context: CastContext,
    ) -> Self {
        Self::new(source, target, context, CastImplementation::InOut)
    }

    fn new(
        source: TargetName<'_>,
        target: TargetName<'_>,
        context: CastContext,
        implementation: CastImplementation,
    ) -> Self {
        Self { source: operand_type(source), target: operand_type(target), context, implementation }
    }

    /// Whether both casts convert between the same types, which `pg_cast`
    /// holds one row for.
    pub(super) fn repeats(&self, other: &Self) -> bool {
        identity_repeats(&self.source, &other.source)
            && identity_repeats(&self.target, &other.target)
    }

    /// The `(source AS target)` spelling naming the cast.
    pub(super) fn described(&self) -> String {
        format!("({} AS {})", self.source.target_name(), self.target.target_name())
    }

    /// Whether the source or the target is the type `name` writes.
    pub(super) fn uses_type(&self, name: &ObjectName) -> bool {
        self.source.names_type(name) || self.target.names_type(name)
    }

    /// Whether any type identity the cast holds, its function's inputs
    /// included, is the type `name` writes.
    pub(super) fn holds_type(&self, name: &ObjectName) -> bool {
        self.type_identities().any(|identity| identity.names_type(name))
    }

    /// Repoints every type identity naming `from`, the function's inputs
    /// included, at `to`.
    pub(super) fn rename_type_use(&mut self, from: &ObjectName, to: &ObjectName) {
        for type_ in
            [&mut self.source, &mut self.target].into_iter().filter(|type_| type_.names_type(from))
        {
            type_.set_object_name(to);
        }
        if let CastImplementation::Function(function) = &mut self.implementation
            && function.arguments.iter().any(|argument| argument.names_type(from))
        {
            for argument in
                function.arguments.to_mut().iter_mut().filter(|argument| argument.names_type(from))
            {
                argument.set_object_name(to);
            }
        }
    }

    /// Whether any identity the cast holds lives in `schema`.
    pub(super) fn touches_schema(&self, schema: &str, quoted: bool) -> bool {
        self.identities().any(|identity| identity.schema_matches(schema, quoted))
    }

    /// Moves every identity the cast holds in `from` to `to`.
    pub(super) fn follow_schema_rename(
        &mut self,
        from: &str,
        from_quoted: bool,
        to: &str,
        to_quoted: bool,
    ) {
        self.source.rename_schema(from, from_quoted, to, to_quoted);
        self.target.rename_schema(from, from_quoted, to, to_quoted);
        if let CastImplementation::Function(function) = &mut self.implementation {
            function.name.rename_schema(from, from_quoted, to, to_quoted);
            for argument in function.arguments.to_mut() {
                argument.rename_schema(from, from_quoted, to, to_quoted);
            }
        }
    }

    /// Whether the cast runs the function `name` declares with `args`.
    pub(super) fn declares_function(
        &self,
        type_arrays: &TypeArrays,
        name: &ObjectName,
        args: Option<&[OperateFunctionArg]>,
        case: IdentifierCase,
    ) -> bool {
        let CastImplementation::Function(function) = &self.implementation else {
            return false;
        };
        function_name_matches(&function.name, name, case)
            && arguments_match(function.spelled_arguments(type_arrays), args)
    }

    /// Repoints the function `from` declares with `from_args` at `to` when
    /// the cast runs it.
    pub(super) fn follow_function_rename(
        &mut self,
        type_arrays: &TypeArrays,
        from: &ObjectName,
        from_args: Option<&[OperateFunctionArg]>,
        to: &ObjectName,
        case: IdentifierCase,
    ) {
        if let CastImplementation::Function(function) = &mut self.implementation
            && function_name_matches(&function.name, from, case)
            && arguments_match(function.spelled_arguments(type_arrays), from_args)
        {
            function.name.set_object_name(to);
        }
    }

    /// Every identity the cast holds.
    fn identities(&self) -> impl Iterator<Item = &CatalogName> {
        let function = match &self.implementation {
            CastImplementation::Function(function) => Some(&function.name),
            CastImplementation::InOut | CastImplementation::Binary => None,
        };
        self.type_identities().chain(function)
    }

    /// Every type identity the cast holds, its function's inputs included.
    fn type_identities(&self) -> impl Iterator<Item = &CatalogName> {
        let arguments = match &self.implementation {
            CastImplementation::Function(function) => Some(function.arguments.iter()),
            CastImplementation::InOut | CastImplementation::Binary => None,
        };
        [&self.source, &self.target].into_iter().chain(arguments.into_iter().flatten())
    }
}

impl CastFunction {
    /// The normalized spellings of the input types, in order.
    fn spelled_arguments<'a>(
        &'a self,
        type_arrays: &'a TypeArrays,
    ) -> impl Iterator<Item = Cow<'static, str>> + 'a {
        self.arguments.iter().map(|argument| operand_spelling(argument, type_arrays))
    }
}

impl CastLike for PostgresCatalogCast {
    type DB = ParserDB;

    fn source_type<'db>(&'db self, _database: &'db ParserDB) -> TargetName<'db> {
        self.source.target_name()
    }

    fn target_type<'db>(&'db self, _database: &'db ParserDB) -> TargetName<'db> {
        self.target.target_name()
    }

    fn context(&self) -> CastContext {
        self.context
    }

    fn method(&self) -> CastMethod {
        match self.implementation {
            CastImplementation::Function(_) => CastMethod::Function,
            CastImplementation::InOut => CastMethod::InOut,
            CastImplementation::Binary => CastMethod::Binary,
        }
    }

    /// Finds the declared function without coercion or overload selection.
    fn function<'db>(&'db self, database: &'db ParserDB) -> Option<&'db CreateFunction> {
        let CastImplementation::Function(function) = &self.implementation else {
            return None;
        };
        let case = database.identifier_case();
        let spelled: Vec<_> = function.spelled_arguments(&database.ingestion.type_arrays).collect();
        database.functions().find(|candidate| {
            function_name_matches(&function.name, &candidate.name, case)
                && arguments_match(&spelled, candidate.args.as_deref())
        })
    }
}

/// Whether PostgreSQL 18 converts `source` to `target` implicitly, by stored
/// type names, a type's length coercion of itself left out.
pub(crate) fn built_in_implicit_cast(source: &str, target: &str) -> bool {
    source != target
        && POSTGRES_18_CASTS
            .binary_search_by(|cast| {
                (cast.source.name(), cast.target.name()).cmp(&(source, target))
            })
            .is_ok_and(|position| POSTGRES_18_CASTS[position].context == CastContext::Implicit)
}
