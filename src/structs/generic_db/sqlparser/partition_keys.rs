//! The partition key a table declares, and what PostgreSQL requires of it and
//! of the unique constraints and indexes on a partitioned table.

use alloc::{string::ToString, vec, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateIndex, CreateTable, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, Ident, IndexColumn, TableConstraint, Visitor,
};

use crate::{
    errors::Error,
    structs::IdentifierCase,
    traits::{PartitionStrategy, TableLike},
    utils::walk::walk,
};

/// One entry of a `PARTITION BY` key.
enum KeyItem<'a> {
    /// A plain column.
    Column(&'a Ident),
    /// An expression over the table's columns.
    Expression(&'a Expr),
}

/// The strategy and the entries of the key `table` partitions by, or `None`
/// for a table that is not partitioned.
fn partition_key(table: &CreateTable) -> Option<(PartitionStrategy, Vec<KeyItem<'_>>)> {
    let (
        Some(strategy),
        Some(Expr::Function(Function { args: FunctionArguments::List(list), .. })),
    ) = (table.partition_strategy(), table.partition_by.as_deref())
    else {
        return None;
    };
    let items = list
        .args
        .iter()
        .filter_map(|argument| {
            match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
                _ => None,
            }
        })
        .map(|expr| {
            match expr {
                Expr::Identifier(ident) => KeyItem::Column(ident),
                Expr::Nested(inner) => KeyItem::Expression(inner),
                other => KeyItem::Expression(other),
            }
        })
        .collect();
    Some((strategy, items))
}

/// The column of `table` that `ident` names.
fn column_named<'a>(
    table: &'a CreateTable,
    ident: &Ident,
    case: IdentifierCase,
) -> Option<&'a ColumnDef> {
    table.columns.iter().find(|column| same_name(&column.name, ident, case))
}

/// The strategy of a partitioned table and, for each entry of its key, the
/// column it names, or `None` for an expression entry or a column the table
/// does not hold.
pub(super) fn key_columns(
    table: &CreateTable,
    case: IdentifierCase,
) -> Option<(PartitionStrategy, Vec<Option<&ColumnDef>>)> {
    let (strategy, items) = partition_key(table)?;
    let columns = items
        .iter()
        .map(|item| {
            match item {
                KeyItem::Column(ident) => column_named(table, ident, case),
                KeyItem::Expression(_) => None,
            }
        })
        .collect();
    Some((strategy, columns))
}

/// Whether two identifiers name the same column under `case`.
fn same_name(left: &Ident, right: &Ident, case: IdentifierCase) -> bool {
    case.identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// Whether the column is a stored generated column.
fn is_generated(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(option.option, ColumnOption::Generated { generation_expr: Some(_), .. })
    })
}

/// Every identifier an expression of the key reads.
fn identifiers(expr: &Expr) -> Vec<Ident> {
    struct Collect(Vec<Ident>);
    impl Visitor for Collect {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if let Expr::Identifier(ident) = expr {
                self.0.push(ident.clone());
            }
            ControlFlow::Continue(())
        }
    }
    let mut collect = Collect(Vec::new());
    let _: ControlFlow<()> = walk(expr, &mut collect);
    collect.0
}

/// Refuses what PostgreSQL refuses of a partitioned table's key and of the
/// unique constraints it holds.
///
/// The key may name only columns the table holds, none of them stored
/// generated, and `LIST` takes one entry. Every primary key, unique and
/// exclusion constraint has to cover each key column, an exclusion with `=`,
/// and none may stand on a key with an expression.
pub(super) fn validate_partitioned_table(
    table: &CreateTable,
    case: IdentifierCase,
) -> Result<(), Error> {
    let Some((strategy, items)) = partition_key(table) else {
        return Ok(());
    };
    let table_name = || table.table_name().to_string();
    if strategy == PartitionStrategy::List && items.len() > 1 {
        return Err(Error::ListPartitionKeyHasManyColumns { table_name: table_name() });
    }
    for item in &items {
        let read = match item {
            KeyItem::Column(ident) => vec![(*ident).clone()],
            KeyItem::Expression(expr) => identifiers(expr),
        };
        for ident in read {
            let Some(column) = column_named(table, &ident, case) else {
                return Err(Error::PartitionKeyColumnNotFound {
                    table_name: table_name(),
                    column_name: ident.value,
                });
            };
            if is_generated(column) {
                return Err(Error::GeneratedColumnInPartitionKey {
                    table_name: table_name(),
                    column_name: column.name.value.clone(),
                });
            }
        }
    }
    for covered in unique_constraints(table) {
        covers_key(table, &items, &covered, case)?;
    }
    Ok(())
}

/// Refuses a unique index on a partitioned table that leaves out a column of
/// its partition key, or that stands on a key with an expression.
pub(super) fn validate_unique_index(
    index: &CreateIndex,
    table: &CreateTable,
    case: IdentifierCase,
) -> Result<(), Error> {
    if !index.unique {
        return Ok(());
    }
    let Some((_, items)) = partition_key(table) else {
        return Ok(());
    };
    covers_key(table, &items, &plain_columns(&index.columns), case)
}

/// The plain columns an index or key lists, leaving out its expressions.
fn plain_columns(columns: &[IndexColumn]) -> Vec<Ident> {
    columns
        .iter()
        .filter_map(|column| {
            match &column.column.expr {
                Expr::Identifier(ident) => Some(ident.clone()),
                _ => None,
            }
        })
        .collect()
}

/// The columns each primary key, unique and exclusion constraint of `table`
/// enforces equality on, written on a column or as a table constraint.
fn unique_constraints(table: &CreateTable) -> Vec<Vec<Ident>> {
    let on_columns = table
        .columns
        .iter()
        .filter(|column| {
            column.options.iter().any(|option| {
                matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
            })
        })
        .map(|column| vec![column.name.clone()]);
    let on_table = table.constraints.iter().filter_map(|constraint| {
        match constraint {
            TableConstraint::PrimaryKey(key) => Some(plain_columns(&key.columns)),
            TableConstraint::Unique(key) => Some(plain_columns(&key.columns)),
            TableConstraint::Exclude(exclude) => {
                Some(
                    exclude
                        .elements
                        .iter()
                        .filter(|element| element.operator.to_string() == "=")
                        .filter_map(|element| {
                            match &element.column.column.expr {
                                Expr::Identifier(ident) => Some(ident.clone()),
                                _ => None,
                            }
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    });
    on_columns.chain(on_table).collect()
}

/// Refuses `covered` unless it holds every column of the key `items`.
fn covers_key(
    table: &CreateTable,
    items: &[KeyItem<'_>],
    covered: &[Ident],
    case: IdentifierCase,
) -> Result<(), Error> {
    for item in items {
        match item {
            KeyItem::Expression(_) => {
                return Err(Error::UniqueConstraintOverExpressionPartitionKey {
                    table_name: table.table_name().to_string(),
                });
            }
            KeyItem::Column(key) => {
                if !covered.iter().any(|column| same_name(column, key, case)) {
                    return Err(Error::UniqueConstraintMissesPartitionKey {
                        table_name: table.table_name().to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}
