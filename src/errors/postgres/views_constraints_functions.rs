use crate::errors::ObjectKind;

/// PostgreSQL's object description tag.
pub(super) fn object_kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::ForeignTable => "foreign table",
        ObjectKind::VirtualTable => "virtual table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Column => "column",
        ObjectKind::ColumnDefault => "default value",
        ObjectKind::Index | ObjectKind::UniqueIndex => "index",
        ObjectKind::CheckConstraint => "constraint",
        ObjectKind::Policy => "policy",
        ObjectKind::Function => "function",
        ObjectKind::Trigger => "trigger",
        ObjectKind::Rule => "rule",
        ObjectKind::Extension => "extension",
        ObjectKind::Role => "role",
        ObjectKind::Schema => "schema",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Domain | ObjectKind::Type => "type",
        ObjectKind::Operator => "operator",
        ObjectKind::Cast => "cast",
    }
}
