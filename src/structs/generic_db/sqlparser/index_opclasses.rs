//! Default operator classes for resolved PostgreSQL index key types.

use alloc::{
    format,
    string::{String, ToString},
};

use sqlparser::ast::{
    ArrayElemTypeDef, ColumnOption, CreateIndex, CreateTable, DataType, Expr, Ident, IndexColumn,
    IndexType, ObjectName, TableConstraint,
};

use super::{ParserDBBuilder, SqlparserDialect, domains};
use crate::{
    errors::Error,
    traits::{DomainLike, UserDefinedTypeKind, UserDefinedTypeLike},
    utils::{
        identifier_resolution::stored_identifier,
        object_name::{Qualifier, qualifier_of},
        postgres_types::{built_in_type, builtin_data_type_name},
        quote_identifier::quote_identifier,
    },
};

/// Checks the keys declared on a table.
pub(super) fn table(table: &CreateTable, builder: &ParserDBBuilder) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Ok(());
    }
    for column in &table.columns {
        if column.options.iter().any(|option| {
            matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
        }) {
            require_type(&column.data_type, Method::Btree, builder)?;
        }
    }
    for constraint in &table.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => {
                columns(&key.columns, Some(table), Method::Btree, builder)?;
            }
            TableConstraint::Unique(key) => {
                columns(&key.columns, Some(table), Method::Btree, builder)?;
            }
            TableConstraint::Exclude(key) => {
                let method = match &key.index_method {
                    None => Method::Btree,
                    Some(name) => {
                        let Some(method) = Method::named(name, builder) else { continue };
                        method
                    }
                };
                for element in &key.elements {
                    column(&element.column, Some(table), method, builder)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Checks the resolved key types of an index.
pub(super) fn index(
    index: &CreateIndex,
    table: Option<&CreateTable>,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql
        && let Some(method) = Method::of(index.using.as_ref(), builder)
    {
        columns(&index.columns, table, method, builder)?;
    }
    Ok(())
}

/// Checks every index key without inspecting included columns.
fn columns(
    columns: &[IndexColumn],
    table: Option<&CreateTable>,
    method: Method,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    columns.iter().try_for_each(|key| column(key, table, method, builder))
}

/// Checks a key using its default operator class.
fn column(
    column: &IndexColumn,
    table: Option<&CreateTable>,
    method: Method,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    if column.operator_class.is_none()
        && let Some(data_type) = expression_type(&column.column.expr, table, builder)
    {
        require_type(data_type, method, builder)?;
    }
    Ok(())
}

/// Resolves column references and explicitly typed expressions.
fn expression_type<'expression>(
    expression: &'expression Expr,
    table: Option<&'expression CreateTable>,
    builder: &ParserDBBuilder,
) -> Option<&'expression DataType> {
    match expression {
        Expr::Nested(expression) => expression_type(expression, table, builder),
        Expr::Cast { data_type, .. } => Some(data_type),
        Expr::Identifier(name) => {
            table?
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(&column.name, name, builder.identifier_case())
                })
                .map(|column| &column.data_type)
        }
        Expr::CompoundIdentifier(names) => {
            let name = names.last()?;
            table?
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(&column.name, name, builder.identifier_case())
                })
                .map(|column| &column.data_type)
        }
        _ => None,
    }
}

/// Refuses a resolved type with no default operator class.
fn require_type(
    data_type: &DataType,
    method: Method,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    if let Some(missing) = missing_class(data_type, method, builder) {
        let type_name = match missing {
            MissingClass::Builtin(name) => name.to_string(),
            MissingClass::Type(data_type) => type_display(data_type, builder),
            MissingClass::Domain(name, position) => {
                let spelling =
                    quote_identifier(&stored_identifier(&name.value, name.quote_style.is_some()));
                if matches!(domains::reach_type(builder, name), Some(domains::Reached::Domain(reached)) if reached == position)
                {
                    spelling
                } else {
                    let domain = &builder.domains()[position].0;
                    let schema = stored_identifier(
                        domain.schema().unwrap_or("public"),
                        domain.schema_is_quoted(),
                    );
                    format!("{}.{spelling}", quote_identifier(&schema))
                }
            }
        };
        return Err(Error::MissingDefaultOperatorClass { type_name, access_method: method.name() });
    }
    Ok(())
}

/// The type PostgreSQL names when a default class is missing.
enum MissingClass<'name> {
    Builtin(&'static str),
    Domain(&'name Ident, usize),
    Type(&'name DataType),
}

/// Resolves scalars and domain bases lacking a default btree operator class.
fn btree_missing_class<'name>(
    data_type: &'name DataType,
    builder: &ParserDBBuilder,
) -> Option<MissingClass<'name>> {
    let builtin = match data_type {
        DataType::Custom(name, _) => {
            let ident = name.0.last()?.as_ident()?;
            if let Some(position) = domains::named_domain(builder, data_type) {
                return btree_missing_class(&builder.domains()[position].0.data_type, builder)
                    .map(|_| MissingClass::Domain(ident, position));
            }
            if !matches!(reached_type(name, builder), Some(domains::Reached::Catalog)) {
                return None;
            }
            [
                "aclitem",
                "box",
                "cid",
                "circle",
                "gtsvector",
                "json",
                "jsonpath",
                "line",
                "lseg",
                "path",
                "pg_brin_bloom_summary",
                "pg_brin_minmax_multi_summary",
                "pg_snapshot",
                "point",
                "polygon",
                "refcursor",
                "txid_snapshot",
                "xid",
                "xml",
            ]
            .into_iter()
            .find(|name| {
                builder.identifier_case().identifiers_match(
                    &ident.value,
                    ident.quote_style.is_some(),
                    name,
                    false,
                )
            })?
        }
        _ => {
            builtin_data_type_name(data_type).filter(|name| {
                matches!(
                    *name,
                    "json" | "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle"
                )
            })?
        }
    };
    Some(MissingClass::Builtin(builtin))
}

/// An access method with catalog default classes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Btree,
    Gist,
    Gin,
    Spgist,
    Brin,
    Hash,
}

impl Method {
    /// Resolves an index's access method.
    fn of(method: Option<&IndexType>, builder: &ParserDBBuilder) -> Option<Self> {
        match method {
            None | Some(IndexType::BTree) => Some(Self::Btree),
            Some(IndexType::GiST) => Some(Self::Gist),
            Some(IndexType::GIN) => Some(Self::Gin),
            Some(IndexType::SPGiST) => Some(Self::Spgist),
            Some(IndexType::BRIN) => Some(Self::Brin),
            Some(IndexType::Hash) => Some(Self::Hash),
            Some(IndexType::Custom(name)) => Self::named(name, builder),
            Some(IndexType::Bloom) => None,
        }
    }

    /// Resolves a method identifier using the database's identifier case.
    fn named(name: &Ident, builder: &ParserDBBuilder) -> Option<Self> {
        [Self::Btree, Self::Gist, Self::Gin, Self::Spgist, Self::Brin, Self::Hash].into_iter().find(
            |method| {
                builder.identifier_case().identifiers_match(
                    &name.value,
                    name.quote_style.is_some(),
                    method.name(),
                    false,
                )
            },
        )
    }

    /// The catalog spelling of the method.
    const fn name(self) -> &'static str {
        match self {
            Self::Btree => "btree",
            Self::Gist => "gist",
            Self::Gin => "gin",
            Self::Spgist => "spgist",
            Self::Brin => "brin",
            Self::Hash => "hash",
        }
    }

    /// Whether a catalog type has a default class, including binary coercions.
    fn supports(self, name: &str) -> bool {
        match name {
            "anyrange" => matches!(self, Self::Gist | Self::Spgist | Self::Brin | Self::Hash),
            "anymultirange" => matches!(self, Self::Gist | Self::Hash),
            "anyarray" | "jsonb" => matches!(self, Self::Gin | Self::Hash),
            "anyenum" | "record" | "aclitem" | "bool" | "cid" | "xid" | "xid8" => {
                self == Self::Hash
            }
            "box" => matches!(self, Self::Gist | Self::Spgist | Self::Brin),
            "point" | "polygon" => matches!(self, Self::Gist | Self::Spgist),
            "circle" | "tsquery" => self == Self::Gist,
            "tsvector" => matches!(self, Self::Gist | Self::Gin),
            "bit" | "varbit" => self == Self::Brin,
            "inet" | "cidr" | "text" | "varchar" | "pg_node_tree" => {
                matches!(self, Self::Spgist | Self::Brin | Self::Hash)
            }
            "bpchar" | "bytea" | "char" | "date" | "float4" | "float8" | "int2" | "int4"
            | "int8" | "interval" | "macaddr" | "macaddr8" | "name" | "numeric" | "oid"
            | "pg_lsn" | "tid" | "time" | "timestamp" | "timestamptz" | "timetz" | "uuid"
            | "pg_dependencies" | "pg_mcv_list" | "pg_ndistinct" => {
                matches!(self, Self::Brin | Self::Hash)
            }
            _ => false,
        }
    }
}

/// Resolves types lacking a default class for the selected access method.
fn missing_class<'name>(
    data_type: &'name DataType,
    method: Method,
    builder: &ParserDBBuilder,
) -> Option<MissingClass<'name>> {
    if method == Method::Btree {
        return btree_missing_class(data_type, builder);
    }
    let class_type = if array_element(data_type).is_some() {
        "anyarray"
    } else if let DataType::Custom(name, _) = data_type {
        match reached_type(name, builder)? {
            domains::Reached::Domain(position) => {
                return missing_class(&builder.domains()[position].0.data_type, method, builder)
                    .map(|_| MissingClass::Type(data_type));
            }
            domains::Reached::UserType(position, multirange) => {
                match builder.user_defined_types()[position].0.kind() {
                    UserDefinedTypeKind::Enum => "anyenum",
                    UserDefinedTypeKind::Composite => "record",
                    UserDefinedTypeKind::Range if multirange => "anymultirange",
                    UserDefinedTypeKind::Range => "anyrange",
                    UserDefinedTypeKind::Base | UserDefinedTypeKind::Shell => return None,
                }
            }
            domains::Reached::RowType(_) => "record",
            domains::Reached::Catalog => class_input(builtin_name(data_type)?),
        }
    } else {
        class_input(builtin_name(data_type)?)
    };
    (!method.supports(class_type)).then_some(MissingClass::Type(data_type))
}

/// The polymorphic or binary-coercible class input for a catalog type.
fn class_input(name: &str) -> &str {
    match name {
        "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
            "anyrange"
        }
        "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
        | "tsmultirange" | "tstzmultirange" => "anymultirange",
        "int2vector" | "oidvector" => "anyarray",
        "regclass" | "regcollation" | "regconfig" | "regdictionary" | "regnamespace"
        | "regoper" | "regoperator" | "regproc" | "regprocedure" | "regrole" | "regtype" => "oid",
        _ => name,
    }
}

/// Resolves a custom type through its qualifier or the search path.
fn reached_type(name: &ObjectName, builder: &ParserDBBuilder) -> Option<domains::Reached> {
    let identifier = name.0.last()?.as_ident()?;
    match qualifier_of(name) {
        Qualifier::Absent => domains::reach_type(builder, identifier),
        Qualifier::Named(schema, quoted) => {
            domains::reach_in_schema(builder, identifier, (schema, quoted))
        }
        Qualifier::RunTime => None,
    }
}

/// The element type of any array spelling.
fn array_element(data_type: &DataType) -> Option<&DataType> {
    match data_type {
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => Some(element),
        _ => None,
    }
}

/// The catalog identity of a scalar builtin declaration.
fn builtin_name(data_type: &DataType) -> Option<&'static str> {
    if let DataType::Custom(name, _) = data_type {
        let identifier = name.0.last()?.as_ident()?;
        built_in_type(&stored_identifier(&identifier.value, identifier.quote_style.is_some()))
            .map(|(name, _, _)| name)
    } else {
        builtin_data_type_name(data_type)
    }
}

/// The PostgreSQL diagnostic spelling of a catalog type.
fn builtin_display(name: &str) -> &str {
    match name {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bool" => "boolean",
        "varchar" => "character varying",
        "bpchar" => "character",
        "char" => "\"char\"",
        "varbit" => "bit varying",
        "time" => "time without time zone",
        "timetz" => "time with time zone",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        _ => name,
    }
}

/// Formats a refused type without its typmod or array dimensions.
fn type_display(data_type: &DataType, builder: &ParserDBBuilder) -> String {
    if let Some(mut element) = array_element(data_type) {
        while let Some(inner) = array_element(element) {
            element = inner;
        }
        return format!("{}[]", type_display(element, builder));
    }
    if let DataType::Custom(name, _) = data_type
        && let Some(identifier) = name.0.last().and_then(|part| part.as_ident())
    {
        if matches!(reached_type(name, builder), Some(domains::Reached::Catalog))
            && let Some(builtin) = builtin_name(data_type)
        {
            return builtin_display(builtin).to_string();
        }
        return named_display(name, identifier, builder);
    }
    builtin_name(data_type)
        .map_or_else(|| data_type.to_string(), |name| builtin_display(name).to_string())
}

/// Qualifies a named type when the search path cannot reach it.
fn named_display(name: &ObjectName, identifier: &Ident, builder: &ParserDBBuilder) -> String {
    let spelling =
        quote_identifier(&stored_identifier(&identifier.value, identifier.quote_style.is_some()));
    let visible = match (reached_type(name, builder), domains::reach_type(builder, identifier)) {
        (Some(domains::Reached::Domain(left)), Some(domains::Reached::Domain(right))) => {
            left == right
        }
        (
            Some(domains::Reached::UserType(left, left_multi)),
            Some(domains::Reached::UserType(right, right_multi)),
        ) => left == right && left_multi == right_multi,
        _ => matches!(qualifier_of(name), Qualifier::Absent),
    };
    if !visible && let Qualifier::Named(schema, quoted) = qualifier_of(name) {
        format!("{}.{}", quote_identifier(&stored_identifier(schema, quoted)), spelling)
    } else {
        spelling
    }
}
