//! The names PostgreSQL's `FigureColname` gives an expression, which it uses
//! for a query output written without an alias and, as `FigureIndexColname`,
//! for an index column that is an expression.

use alloc::{borrow::ToOwned, string::String};

use sqlparser::ast::{
    AccessExpr, ArrayElemTypeDef, BinaryOperator, DataType, ExactNumberInfo, Expr,
    GeometricTypeKind, Ident, InTarget, SelectItem, SetExpr, TimezoneInfo, TrimWhereField,
};

use crate::utils::object_name::object_name_last_part;

/// The name PostgreSQL figures for an expression, with whether it is read
/// literally (`true`) or folds as an unquoted identifier does.
pub(crate) enum Figure {
    /// A name nothing overrides, such as a column or a function name.
    Strong(String, bool),
    /// A name an enclosing cast or `CASE` replaces, such as a type name.
    Weak(String, bool),
    /// An expression PostgreSQL names nothing, such as an operator.
    Unnamed,
    /// An expression whose name is not modeled.
    Unknown,
}

impl Figure {
    fn strong(name: &str) -> Self {
        Self::Strong(name.to_owned(), true)
    }

    fn ident(ident: &Ident) -> Self {
        Self::Strong(ident.value.clone(), ident.quote_style.is_some())
    }
}

/// Mirrors PostgreSQL's `FigureColname`, answering [`Figure::Unknown`] where
/// the name is not modeled.
pub(crate) fn figure(expression: &Expr) -> Figure {
    match expression {
        Expr::Identifier(ident) => Figure::ident(ident),
        Expr::CompoundIdentifier(parts) => parts.last().map_or(Figure::Unknown, Figure::ident),
        Expr::CompoundFieldAccess { root, access_chain } => {
            let mut field = None;
            for access in access_chain {
                if let AccessExpr::Dot(part) = access {
                    let Expr::Identifier(ident) = part else {
                        return Figure::Unknown;
                    };
                    field = Some(ident);
                }
            }
            field.map_or_else(|| figure(root), Figure::ident)
        }
        Expr::Nested(inner) | Expr::Collate { expr: inner, .. } => figure(inner),
        Expr::Function(function) => {
            object_name_last_part(&function.name)
                .map_or(Figure::Unknown, |(name, quoted)| Figure::Strong(name.to_owned(), quoted))
        }
        Expr::Cast { expr, data_type, .. } => {
            match figure(expr) {
                Figure::Weak(..) | Figure::Unnamed => type_figure(data_type),
                named => named,
            }
        }
        Expr::TypedString(typed) => type_figure(&typed.data_type),
        Expr::Interval(_) => Figure::Weak("interval".to_owned(), true),
        Expr::Case { else_result, .. } => {
            match else_result.as_deref().map_or(Figure::Unnamed, figure) {
                Figure::Weak(..) | Figure::Unnamed => Figure::Weak("case".to_owned(), true),
                named => named,
            }
        }
        Expr::Array(_) => Figure::strong("array"),
        Expr::Tuple(_) => Figure::strong("row"),
        Expr::Exists { negated: false, .. } => Figure::strong("exists"),
        Expr::Subquery(query) => first_output_figure(&query.body),
        Expr::Extract { .. } => Figure::strong("extract"),
        Expr::Ceil { .. } => Figure::strong("ceil"),
        Expr::Floor { .. } => Figure::strong("floor"),
        Expr::Position { .. } => Figure::strong("position"),
        Expr::Overlay { .. } => Figure::strong("overlay"),
        Expr::AtTimeZone { .. } => Figure::strong("timezone"),
        Expr::Substring { shorthand: true, .. } => Figure::strong("substr"),
        Expr::Substring { shorthand: false, .. } => Figure::strong("substring"),
        Expr::Trim { trim_where: Some(TrimWhereField::Leading), .. } => Figure::strong("ltrim"),
        Expr::Trim { trim_where: Some(TrimWhereField::Trailing), .. } => Figure::strong("rtrim"),
        Expr::Trim { .. } => Figure::strong("btrim"),
        Expr::BinaryOp { op: BinaryOperator::Overlaps, .. } => Figure::strong("overlaps"),
        Expr::In { target, .. } => {
            match target {
                InTarget::List(_)
                | InTarget::Subquery(_)
                | InTarget::Unnest(_)
                | InTarget::Table { .. } => Figure::Unnamed,
            }
        }
        Expr::Value(_)
        | Expr::BinaryOp { .. }
        | Expr::UnaryOp { .. }
        | Expr::IsFalse(_)
        | Expr::IsNotFalse(_)
        | Expr::IsTrue(_)
        | Expr::IsNotTrue(_)
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_)
        | Expr::IsDistinctFrom(..)
        | Expr::IsNotDistinctFrom(..)
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::SimilarTo { .. }
        | Expr::AnyOp { .. }
        | Expr::AllOp { .. }
        | Expr::Exists { negated: true, .. } => Figure::Unnamed,
        _ => Figure::Unknown,
    }
}

/// The name of the first output of a query body, which names a scalar
/// subquery.
fn first_output_figure(body: &SetExpr) -> Figure {
    match body {
        SetExpr::Select(select) => {
            match select.projection.first() {
                Some(SelectItem::ExprWithAlias { alias, .. }) => Figure::ident(alias),
                Some(SelectItem::UnnamedExpr(expression)) => {
                    match figure(expression) {
                        Figure::Strong(name, quoted) | Figure::Weak(name, quoted) => {
                            Figure::Strong(name, quoted)
                        }
                        Figure::Unnamed => Figure::strong("?column?"),
                        Figure::Unknown => Figure::Unknown,
                    }
                }
                _ => Figure::Unknown,
            }
        }
        SetExpr::Query(query) => first_output_figure(&query.body),
        SetExpr::SetOperation { left, .. } => first_output_figure(left),
        SetExpr::Values(_) => Figure::strong("column1"),
        _ => Figure::Unknown,
    }
}

/// The last part of the name PostgreSQL resolves a written type to, which a
/// cast contributes when its operand names nothing stronger.
fn type_figure(data_type: &DataType) -> Figure {
    let name = match data_type {
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => "int4",
        DataType::BigInt(_) | DataType::Int8(_) => "int8",
        DataType::SmallInt(_) | DataType::Int2(_) => "int2",
        DataType::Text => "text",
        DataType::Varchar(_) | DataType::CharacterVarying(_) | DataType::CharVarying(_) => {
            "varchar"
        }
        DataType::Numeric(_) | DataType::Decimal(_) | DataType::Dec(_) => "numeric",
        DataType::Boolean | DataType::Bool => "bool",
        DataType::Float(ExactNumberInfo::Precision(precision)) if *precision <= 24 => "float4",
        DataType::Float(ExactNumberInfo::None | ExactNumberInfo::Precision(_))
        | DataType::Float8
        | DataType::DoublePrecision => "float8",
        DataType::Real | DataType::Float4 => "float4",
        DataType::Date => "date",
        DataType::JSON => "json",
        DataType::JSONB => "jsonb",
        DataType::Char(_) | DataType::Character(_) => "bpchar",
        DataType::Bytea => "bytea",
        DataType::Uuid => "uuid",
        DataType::Interval { .. } => "interval",
        DataType::Bit(_) => "bit",
        DataType::VarBit(_) | DataType::BitVarying(_) => "varbit",
        DataType::Time(_, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => "time",
        DataType::Time(_, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => "timetz",
        DataType::Timestamp(_, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => "timestamp",
        DataType::Timestamp(_, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => "timestamptz",
        DataType::Regclass => "regclass",
        DataType::TsVector => "tsvector",
        DataType::TsQuery => "tsquery",
        DataType::GeometricType(kind) => {
            match kind {
                GeometricTypeKind::Point => "point",
                GeometricTypeKind::Line => "line",
                GeometricTypeKind::LineSegment => "lseg",
                GeometricTypeKind::GeometricBox => "box",
                GeometricTypeKind::GeometricPath => "path",
                GeometricTypeKind::Polygon => "polygon",
                GeometricTypeKind::Circle => "circle",
            }
        }
        DataType::Array(ArrayElemTypeDef::SquareBracket(element, _)) => {
            return type_figure(element);
        }
        DataType::Custom(name, _) => {
            return object_name_last_part(name)
                .map_or(Figure::Unknown, |(name, quoted)| Figure::Weak(name.to_owned(), quoted));
        }
        _ => return Figure::Unknown,
    };
    Figure::Weak(name.to_owned(), true)
}
