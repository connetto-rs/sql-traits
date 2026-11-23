//! Equal index-backed constraints declared in one statement.

use alloc::vec::Vec;

use sqlparser::ast::{
    ColumnDef, ColumnOption, ConstraintCharacteristics, ExcludeConstraint, Expr, Ident,
    IndexColumn, IndexType, NullsDistinctOption, OrderByOptions, Spanned, TableConstraint,
};

use super::{constraint_names::Position, inheritance::parsed_expr};
use crate::structs::{ConstraintDeferral, IdentifierCase};

/// The index properties compared by PostgreSQL before naming constraints.
struct Key<'node> {
    /// The declaration's slot in the table node.
    position: Position,
    /// The first written name among merged declarations.
    name: Option<&'node Ident>,
    /// Whether this key takes precedence over other declarations.
    primary: bool,
    /// The column owning a column-level key.
    column: Option<&'node Ident>,
    /// The ordered columns of a table-level key.
    columns: &'node [IndexColumn],
    /// Additional columns stored in the index.
    include: &'node [Ident],
    /// The explicitly selected access method of a unique key.
    method: Option<&'node IndexType>,
    /// The written constraint timing.
    characteristics: Option<&'node ConstraintCharacteristics>,
    /// Whether null values compare equal for uniqueness.
    nulls_not_distinct: bool,
    /// The exclusion operators, expressions and predicate.
    exclusion: Option<&'node ExcludeConstraint>,
}

impl<'node> Key<'node> {
    /// Reads the key stored at a column option or table constraint.
    fn at(
        columns: &'node [ColumnDef],
        constraints: &'node [TableConstraint],
        position: Position,
    ) -> Option<Self> {
        let (column, option_name, primary, unique, exclusion) = match position {
            Position::Column(column, option) => {
                let definition = &columns[column];
                let option = &definition.options[option];
                match &option.option {
                    ColumnOption::PrimaryKey(primary) => {
                        (Some(&definition.name), option.name.as_ref(), Some(primary), None, None)
                    }
                    ColumnOption::Unique(unique) => {
                        (Some(&definition.name), option.name.as_ref(), None, Some(unique), None)
                    }
                    _ => return None,
                }
            }
            Position::Table(position) => {
                match &constraints[position] {
                    TableConstraint::PrimaryKey(primary) => (None, None, Some(primary), None, None),
                    TableConstraint::Unique(unique) => (None, None, None, Some(unique), None),
                    TableConstraint::Exclude(exclusion) => {
                        (None, None, None, None, Some(exclusion))
                    }
                    _ => return None,
                }
            }
        };
        if let Some(primary) = primary {
            Some(Self {
                position,
                name: option_name.or(primary.name.as_ref()),
                primary: true,
                column,
                columns: &primary.columns,
                include: &primary.include,
                method: primary.index_type.as_ref(),
                characteristics: primary.characteristics.as_ref(),
                nulls_not_distinct: false,
                exclusion: None,
            })
        } else if let Some(unique) = unique {
            Some(Self {
                position,
                name: option_name.or(unique.name.as_ref()),
                primary: false,
                column,
                columns: &unique.columns,
                include: &unique.include,
                method: unique.index_type.as_ref(),
                characteristics: unique.characteristics.as_ref(),
                nulls_not_distinct: unique.nulls_distinct == NullsDistinctOption::NotDistinct,
                exclusion: None,
            })
        } else {
            exclusion.map(|exclusion| {
                Self {
                    position,
                    name: exclusion.name.as_ref(),
                    primary: false,
                    column: None,
                    columns: &[],
                    include: &exclusion.include,
                    method: None,
                    characteristics: exclusion.characteristics.as_ref(),
                    nulls_not_distinct: false,
                    exclusion: Some(exclusion),
                }
            })
        }
    }

    /// Whether two declarations require the same index.
    fn same_index(&self, other: &Self, case: IdentifierCase) -> bool {
        if (self.primary && other.primary)
            || self.nulls_not_distinct != other.nulls_not_distinct
            || ConstraintDeferral::of(self.characteristics.copied())
                != ConstraintDeferral::of(other.characteristics.copied())
            || !same_identifiers(self.include, other.include, case)
        {
            return false;
        }
        match (self.exclusion, other.exclusion) {
            (Some(left), Some(right)) => {
                let same_method = match (&left.index_method, &right.index_method) {
                    (Some(left), Some(right)) => same_identifier(left, right, case),
                    (Some(method), None) | (None, Some(method)) => {
                        case.identifiers_match(
                            &method.value,
                            method.quote_style.is_some(),
                            "btree",
                            false,
                        )
                    }
                    (None, None) => true,
                };
                same_method
                    && match (&left.where_clause, &right.where_clause) {
                        (Some(left), Some(right)) => {
                            left == right || parsed_expr(left) == parsed_expr(right)
                        }
                        (None, None) => true,
                        _ => false,
                    }
                    && left.elements.len() == right.elements.len()
                    && left.elements.iter().zip(&right.elements).all(|(left, right)| {
                        left.operator == right.operator
                            && same_column(&left.column, &right.column, case)
                    })
            }
            (None, None) => {
                self.method.unwrap_or(&IndexType::BTree)
                    == other.method.unwrap_or(&IndexType::BTree)
                    && match (self.column, other.column) {
                        (Some(left), Some(right)) => same_identifier(left, right, case),
                        (Some(column), None) => single_column(column, other.columns, case),
                        (None, Some(column)) => single_column(column, self.columns, case),
                        (None, None) => {
                            self.columns.len() == other.columns.len()
                                && self
                                    .columns
                                    .iter()
                                    .zip(other.columns)
                                    .all(|(left, right)| same_column(left, right, case))
                        }
                    }
            }
            _ => false,
        }
    }
}

/// Whether identifiers resolve to the same name.
fn same_identifier(left: &Ident, right: &Ident, case: IdentifierCase) -> bool {
    case.identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// Whether two ordered identifier lists match.
fn same_identifiers(left: &[Ident], right: &[Ident], case: IdentifierCase) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| same_identifier(left, right, case))
}

/// Whether index columns have matching names and options.
fn same_column(left: &IndexColumn, right: &IndexColumn, case: IdentifierCase) -> bool {
    left.operator_class == right.operator_class
        && left.column.options == right.column.options
        && left.column.with_fill == right.column.with_fill
        && match (&left.column.expr, &right.column.expr) {
            (Expr::Identifier(left), Expr::Identifier(right)) => same_identifier(left, right, case),
            (Expr::Identifier(_), _) | (_, Expr::Identifier(_)) => false,
            (left, right) => left == right || parsed_expr(left) == parsed_expr(right),
        }
}

/// Accepts one matching column name without index expressions or options.
fn single_column(column: &Ident, columns: &[IndexColumn], case: IdentifierCase) -> bool {
    let [held] = columns else {
        return false;
    };
    held.operator_class.is_none()
        && held.column.options == OrderByOptions::default()
        && held.column.with_fill.is_none()
        && matches!(&held.column.expr, Expr::Identifier(name) if same_identifier(column, name, case))
}

/// Merges the equal keys declared by one `CREATE TABLE` or `ADD COLUMN`.
pub(super) fn merge(
    columns: &mut [ColumnDef],
    constraints: &mut Vec<TableConstraint>,
    case: IdentifierCase,
) {
    let positions = columns
        .iter()
        .enumerate()
        .flat_map(|(column, definition)| {
            (0..definition.options.len()).map(move |option| Position::Column(column, option))
        })
        .chain((0..constraints.len()).map(Position::Table));
    let mut ordered: Vec<_> =
        positions.filter_map(|position| Key::at(columns, constraints, position)).collect();
    ordered.sort_by_key(|key| {
        let start = match key.position {
            Position::Column(column, _) => columns[column].name.span.start,
            Position::Table(position) => constraints[position].span().start,
        };
        (!key.primary, start)
    });
    let mut surviving: Vec<Key<'_>> = Vec::new();
    let mut removed = Vec::new();
    let mut renamed = Vec::new();
    for current in ordered {
        if let Some(previous) =
            surviving.iter_mut().find(|previous| previous.same_index(&current, case))
        {
            if previous.name.is_none() && current.name.is_some() {
                previous.name = current.name;
                renamed.push((previous.position, current.position));
            }
            removed.push(current.position);
        } else {
            surviving.push(current);
        }
    }
    let mut names = Vec::with_capacity(renamed.len());
    for (position, slot) in name_slots(columns, constraints) {
        if let Some((destination, _)) = renamed.iter().find(|(_, source)| *source == position) {
            names.push((*destination, slot.take()));
        }
    }
    for (position, slot) in name_slots(columns, constraints) {
        if let Some(index) = names.iter().position(|(destination, _)| *destination == position) {
            *slot = names.swap_remove(index).1;
        }
    }
    for (column, definition) in columns.iter_mut().enumerate() {
        let mut option = 0;
        definition.options.retain(|_| {
            let keep = !removed.contains(&Position::Column(column, option));
            option += 1;
            keep
        });
    }
    let mut position = 0;
    constraints.retain(|_| {
        let keep = !removed.contains(&Position::Table(position));
        position += 1;
        keep
    });
}

/// The written name fields of every index-backed constraint, with their slots.
fn name_slots<'node>(
    columns: &'node mut [ColumnDef],
    constraints: &'node mut [TableConstraint],
) -> impl Iterator<Item = (Position, &'node mut Option<Ident>)> {
    let column_slots = columns.iter_mut().enumerate().flat_map(|(column, definition)| {
        definition.options.iter_mut().enumerate().filter_map(move |(option, declared)| {
            let key_name = match &mut declared.option {
                ColumnOption::PrimaryKey(key) => &mut key.name,
                ColumnOption::Unique(key) => &mut key.name,
                _ => return None,
            };
            let slot = if declared.name.is_some() { &mut declared.name } else { key_name };
            Some((Position::Column(column, option), slot))
        })
    });
    let table_slots = constraints.iter_mut().enumerate().filter_map(|(position, constraint)| {
        let slot = match constraint {
            TableConstraint::PrimaryKey(key) => &mut key.name,
            TableConstraint::Unique(key) => &mut key.name,
            TableConstraint::Exclude(key) => &mut key.name,
            _ => return None,
        };
        Some((Position::Table(position), slot))
    });
    column_slots.chain(table_slots)
}
