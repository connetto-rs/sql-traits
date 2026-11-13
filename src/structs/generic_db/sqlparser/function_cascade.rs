//! What a statement removing functions under `CASCADE` takes along: every
//! object holding one of them, and what that object takes in turn, as
//! PostgreSQL follows its dependencies.

use alloc::sync::Arc;

use sqlparser::ast::{ColumnOption, Ident, ObjectName};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable,
    constraint_names::Position,
    function_dependents::{Held, Holder, call_count},
    postgres_catalog::PostgresCatalog,
    type_arrays::TypeArrays,
    user_defined_types::{self, Doomed, TypeDrop},
    views,
};
use crate::errors::{Error, ObjectKind};

/// Every part of a table a cascade reaches, the tables below it included.
const WHOLE_TABLE: AlterTableScope = AlterTableScope { if_exists: false, only: false };

impl ParserDB {
    /// Takes along every object holding a function in `removed`, which the
    /// builder no longer holds, and then drops the operators and casts naming
    /// a removed type or running a removed function.
    ///
    /// # Errors
    ///
    /// Returns the errors taking an object along meets.
    pub(super) fn finish_drop(
        builder: ParserDBBuilder,
        mut removed: TypeDrop,
        arrays: &mut Arc<TypeArrays>,
        catalog: &mut Arc<PostgresCatalog>,
    ) -> Result<ParserDBBuilder, Error> {
        let builder = Self::take_function_dependents(builder, &mut removed, arrays)?;
        Self::drop_facts_for_type_drop(catalog, &builder, &removed, arrays);
        Ok(builder)
    }

    /// Takes along every object holding a function in `removed`, and what
    /// each takes in turn, until nothing holds a removed function. What the
    /// cascade removes joins `removed`.
    ///
    /// # Errors
    ///
    /// Returns the errors taking an object along meets.
    pub(super) fn take_function_dependents(
        mut builder: ParserDBBuilder,
        removed: &mut TypeDrop,
        arrays: &mut Arc<TypeArrays>,
    ) -> Result<ParserDBBuilder, Error> {
        while let Some(holder) =
            removed.functions.iter().find_map(|function| builder.function_holder(function, &[]))
        {
            let taken;
            (builder, taken) = Self::take_holder(builder, holder, arrays)?;
            removed.types.extend(taken.types);
            removed.functions.extend(taken.functions);
        }
        Ok(builder)
    }

    /// Takes the object `holder` names, as PostgreSQL takes it along with a
    /// function it holds.
    ///
    /// Returns the builder alongside the types and functions the object took
    /// with it.
    fn take_holder(
        mut builder: ParserDBBuilder,
        Holder { named: (_, name), held }: Holder,
        arrays: &mut Arc<TypeArrays>,
    ) -> Result<(ParserDBBuilder, TypeDrop), Error> {
        let mut taken = TypeDrop::default();
        match held {
            // The column stays, without its default.
            Held::ColumnDefault(table, column) => {
                builder = Self::replace_table_node(builder, &table, |_, node| {
                    for declared in
                        node.columns.iter_mut().filter(|declared| declared.name == column)
                    {
                        declared
                            .options
                            .retain(|option| !matches!(option.option, ColumnOption::Default(_)));
                    }
                    Ok(())
                })?;
            }
            Held::GeneratedColumn(table, column) => {
                let table_name = Self::cascade_table_name(&builder, &table)?;
                (builder, taken) = Self::alter_table_drop_columns(
                    builder,
                    &table_name,
                    WHOLE_TABLE,
                    &[column],
                    false,
                    true,
                    arrays,
                )?;
            }
            Held::Check(table, position) => {
                builder = Self::drop_cascaded_constraint(builder, &table, position, &name)?;
            }
            Held::Exclusion(table, position) => {
                builder = Self::drop_cascaded_constraint(
                    builder,
                    &table,
                    Position::Table(position),
                    &name,
                )?;
            }
            Held::Index(position) => builder.remove_index_at(position),
            Held::PartitionedTable(table) => {
                (builder, taken) = Self::drop_tables(
                    builder,
                    alloc::vec![table],
                    ObjectKind::Table,
                    None,
                    arrays,
                )?;
            }
            Held::Reader(reader) => {
                let doomed = views::readers_and_dependents(&builder, alloc::vec![reader]);
                taken = views::remove_readers(&mut builder, &doomed, arrays);
            }
            Held::Policy(policy) => {
                builder.policies_mut().retain(|(held, _)| !Arc::ptr_eq(held, &policy));
            }
            Held::Trigger(trigger) => {
                builder.triggers_mut().retain(|(held, _)| !Arc::ptr_eq(held, &trigger));
            }
            Held::DomainCheck(domain, check) => {
                let (node, metadata) = &mut builder.domains_mut()[domain];
                let node = Arc::make_mut(node);
                let first = call_count(&node.default)
                    + node.constraints[..check].iter().map(call_count).sum::<usize>();
                metadata
                    .calls_mut()
                    .remove_calls(first..first + call_count(&node.constraints[check]));
                node.constraints.remove(check);
            }
            Held::Domain(domain) => {
                (builder, taken) =
                    user_defined_types::drop_cascade(builder, Doomed::Domain(domain), arrays)?;
            }
            Held::Type(type_) => {
                (builder, taken) =
                    user_defined_types::drop_cascade(builder, Doomed::UserType(type_), arrays)?;
            }
        }
        Ok((builder, taken))
    }

    /// The name a cascade reaches `table` by, qualified so that the session's
    /// path cannot lead it to another table.
    fn cascade_table_name(
        builder: &ParserDBBuilder,
        table: &StoredTable,
    ) -> Result<ObjectName, Error> {
        Ok(user_defined_types::qualified_table_name(&Self::stored_node(builder, table)?.name))
    }

    /// Drops the constraint at `position` of `table`, its copies below it
    /// with it, a refusal naming it `name`.
    fn drop_cascaded_constraint(
        builder: ParserDBBuilder,
        table: &StoredTable,
        position: Position,
        name: &str,
    ) -> Result<ParserDBBuilder, Error> {
        let name = Ident::new(name);
        let node = Self::stored_node(&builder, table)?;
        match position {
            Position::Table(position) => {
                let dropped = node.constraints[position].clone();
                Self::drop_table_constraint(builder, table, WHOLE_TABLE, &name, position, dropped)
            }
            Position::Column(column, option) => {
                let declared = &node.columns[column];
                let (written_on, held) = (declared.name.clone(), declared.options[option].clone());
                Self::drop_column_constraint(
                    builder,
                    table,
                    WHOLE_TABLE,
                    &name,
                    (column, option),
                    (&written_on, &held),
                )
            }
        }
    }
}
