//! Implementation of the `DatabaseLike` trait for `GenericDB`.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    errors::LookupError,
    structs::{GenericDB, IdentifierCase, SchemaProfile, TargetName, generic_db::RelationSlot},
    traits::{DatabaseLike, TableLike},
    utils::object_name::{
        RelationKey, function_has_stored_identity, relation_search_path, render_table_candidate,
        render_view_candidate, resolve_one_function, resolve_one_relation,
        resolve_target_from_candidates, stored_function_key, stored_identity_key, stored_table_key,
        stored_view_key,
    },
};

impl<P: SchemaProfile> DatabaseLike for GenericDB<P> {
    type Table = P::Table;
    type View = P::View;
    type MaterializedView = P::MaterializedView;
    type Column = P::Column;
    type Index = P::Index;
    type ForeignKey = P::ForeignKey;
    type Function = P::Function;
    type UniqueIndex = P::UniqueIndex;
    type CheckConstraint = P::CheckConstraint;
    type Trigger = P::Trigger;
    type Policy = P::Policy;
    type Role = P::Role;
    type TableGrant = P::TableGrant;
    type ColumnGrant = P::ColumnGrant;
    type Schema = P::Schema;
    type Dialect = P::Dialect;

    #[inline]
    fn dialect(&self) -> &Self::Dialect {
        &self.dialect
    }

    #[inline]
    fn catalog_name(&self) -> &str {
        &self.catalog_name
    }

    #[inline]
    fn number_of_tables(&self) -> usize {
        self.tables.len()
    }

    #[inline]
    fn timezone(&self) -> Option<&str> {
        self.timezone.as_deref()
    }

    #[inline]
    fn search_path(&self) -> impl Iterator<Item = (&str, bool)> {
        self.search_path.iter().map(|(name, quoted)| (name.as_str(), *quoted))
    }

    fn table_by_stored_identity(&self, schema: Option<&str>, name: &str) -> Option<&Self::Table> {
        // One bucket holds both spellings of the default schema, so the probe
        // narrows to a name and the comparison decides the identity.
        self.indexed_tables(&stored_identity_key(schema, name)).find(|table| {
            table.stored_table_schema().as_deref() == schema && table.stored_table_name() == name
        })
    }

    fn views(&self) -> impl Iterator<Item = &Self::View> {
        self.views.iter().map(|(view, _)| view.as_ref())
    }

    fn materialized_views(&self) -> impl Iterator<Item = &Self::MaterializedView> {
        self.materialized_views.iter().map(|(view, _)| view.as_ref())
    }

    fn table_id(&self, table: &Self::Table) -> Option<usize> {
        self.tables
            .binary_search_by_key(&(table.table_schema(), table.table_name()), |(t, _)| {
                (t.table_schema(), t.table_name())
            })
            .ok()
    }

    fn resolve_target_table(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        self.resolve_target_table_on_path(&target, case)
    }

    fn resolve_target_view(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::View>, LookupError> {
        self.resolve_target_view_on_path(&target, case)
    }

    fn resolve_target_materialized_view(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::MaterializedView>, LookupError> {
        self.resolve_target_materialized_view_on_path(&target, case)
    }

    fn table_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        self.resolve_target_table_strict(&target, case)
    }

    fn view_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::View>, LookupError> {
        let probe = IndexProbe::new(&target, case, None);
        resolve_one_relation(&target, &self.probed_views(&probe), render_view_candidate)
    }

    fn materialized_view_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::MaterializedView>, LookupError> {
        let probe = IndexProbe::new(&target, case, None);
        resolve_one_relation(
            &target,
            &self.probed_materialized_views(&probe),
            render_view_candidate,
        )
    }

    fn function_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Function>, LookupError> {
        let probe = IndexProbe::new(&target, case, None);
        resolve_one_function(&target.to_string(), &self.probed_functions(&probe))
    }

    fn table_by_id(&self, table_id: usize) -> Option<&Self::Table> {
        self.tables.get(table_id).map(|(table, _)| table.as_ref())
    }

    #[inline]
    fn tables(&self) -> impl Iterator<Item = &Self::Table> {
        self.tables.iter().map(|(table, _)| table.as_ref())
    }

    #[inline]
    fn triggers(&self) -> impl Iterator<Item = &Self::Trigger> {
        self.triggers.iter().map(|(trigger, _)| trigger.as_ref())
    }

    #[inline]
    fn indexes(&self) -> impl Iterator<Item = &Self::Index> {
        self.indices.iter().map(|(index, _)| index.as_ref())
    }

    #[inline]
    fn functions(&self) -> impl Iterator<Item = &Self::Function> {
        self.functions.iter().map(|(func, _)| func.as_ref())
    }

    fn function_by_stored_identity(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<&Self::Function>, LookupError> {
        // One bucket holds both spellings of the default schema, so the probe
        // narrows by name and the comparison decides the identity.
        let candidates: Vec<&P::Function> = self
            .indexed_functions(&stored_identity_key(schema, name))
            .filter(|function| function_has_stored_identity(*function, schema, name))
            .collect();
        resolve_one_function(name, &candidates)
    }

    fn resolve_target_function(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Function>, LookupError> {
        self.resolve_target_function_on_path(&target, case)
    }

    fn policies(&self) -> impl Iterator<Item = &Self::Policy> {
        self.policies.iter().map(|(p, _)| p.as_ref())
    }

    fn roles(&self) -> impl Iterator<Item = &Self::Role> {
        self.roles.iter().map(|(r, _)| r.as_ref())
    }

    fn table_grants(&self) -> impl Iterator<Item = &Self::TableGrant> {
        self.table_grants.iter().map(|(g, _)| g.as_ref())
    }

    fn column_grants(&self) -> impl Iterator<Item = &Self::ColumnGrant> {
        self.column_grants.iter().map(|(g, _)| g.as_ref())
    }

    fn schemas(&self) -> impl Iterator<Item = &Self::Schema> {
        self.schemas.iter().map(|(s, _)| s.as_ref())
    }
}

/// How a lookup reaches the index, and which of the relations it reaches the
/// lookup then accepts.
///
/// The index is keyed by PostgreSQL's rule, so a folding lookup probes the
/// folded key and follows the aliases a quoted stored spelling registered
/// under. An exact lookup gathers the same candidates, since byte equality
/// implies folded equality, and then keeps only those whose stored key
/// matches byte for byte.
struct IndexProbe {
    /// Key whose bucket, with `aliases` when they are consulted, holds every
    /// candidate.
    key: RelationKey,
    /// Whether the folded aliases of `key` are consulted.
    aliases: bool,
    /// Stored key a candidate must equal, when the bucket alone does not
    /// decide it.
    exact: Option<RelationKey>,
}

/// Case the index is probed under for a lookup comparing under `case`.
fn probing_case(case: IdentifierCase) -> IdentifierCase {
    match case {
        IdentifierCase::AsWritten => IdentifierCase::AsWritten,
        IdentifierCase::Folded | IdentifierCase::Exact => IdentifierCase::Folded,
    }
}

impl IndexProbe {
    /// Probe for `target` under `case`, aimed at `entry` when the search path
    /// supplies the schema rather than the target's own qualifier.
    fn new(target: &TargetName<'_>, case: IdentifierCase, entry: Option<(&str, bool)>) -> Self {
        let name = |under: IdentifierCase| {
            under.compared_form(target.name(), target.name_is_quoted()).into_owned()
        };
        let schema = |under: IdentifierCase| {
            match entry {
                Some((entry, quoted)) => under.compared_form(entry, quoted).into_owned(),
                None => {
                    target.schema().map_or_else(
                        || String::from("public"),
                        |schema| {
                            under.compared_form(schema, target.schema_is_quoted()).into_owned()
                        },
                    )
                }
            }
        };
        let probing = probing_case(case);
        Self {
            key: RelationKey { schema: schema(probing), name: name(probing) },
            aliases: case != IdentifierCase::AsWritten,
            exact: (case == IdentifierCase::Exact).then(|| {
                RelationKey {
                    schema: schema(IdentifierCase::Exact),
                    name: name(IdentifierCase::Exact),
                }
            }),
        }
    }

    /// Whether a relation whose stored key is `stored` is one this probe
    /// accepts.
    fn accepts(&self, stored: impl FnOnce() -> RelationKey) -> bool {
        self.exact.as_ref().is_none_or(|exact| stored() == *exact)
    }
}

impl<P: SchemaProfile> GenericDB<P> {
    /// Slots of every relation whose stored identity equals `key`.
    fn indexed_relation_slots(&self, key: &RelationKey) -> &[RelationSlot] {
        self.relation_index.get(key).map_or(&[][..], Vec::as_slice)
    }

    /// Slots a probe reaches, its own bucket first and its folded aliases
    /// after, in storage order within each.
    fn probed_relation_slots<'db>(
        &'db self,
        probe: &IndexProbe,
    ) -> impl Iterator<Item = &'db RelationSlot> {
        let aliases: &[RelationKey] = if probe.aliases {
            self.folded_relation_index.get(&probe.key).map_or(&[][..], Vec::as_slice)
        } else {
            &[]
        };
        self.indexed_relation_slots(&probe.key)
            .iter()
            .chain(aliases.iter().flat_map(|key| self.indexed_relation_slots(key)))
    }

    /// Tables a probe accepts, in storage order.
    fn probed_tables<'db>(&'db self, probe: &IndexProbe) -> Vec<&'db P::Table> {
        self.probed_relation_slots(probe)
            .filter_map(|slot| {
                match slot {
                    RelationSlot::Table(position) => self.tables.get(*position),
                    RelationSlot::View(_) | RelationSlot::MaterializedView(_) => None,
                }
            })
            .map(|(table, _)| table.as_ref())
            .filter(|table| probe.accepts(|| stored_table_key(*table, IdentifierCase::Exact)))
            .collect()
    }

    /// Plain views a probe accepts, in storage order.
    fn probed_views<'db>(&'db self, probe: &IndexProbe) -> Vec<&'db P::View> {
        self.probed_relation_slots(probe)
            .filter_map(|slot| {
                match slot {
                    RelationSlot::View(position) => self.views.get(*position),
                    RelationSlot::Table(_) | RelationSlot::MaterializedView(_) => None,
                }
            })
            .map(|(view, _)| view.as_ref())
            .filter(|view| probe.accepts(|| stored_view_key(*view, IdentifierCase::Exact)))
            .collect()
    }

    /// Materialized views a probe accepts, in storage order.
    fn probed_materialized_views<'db>(
        &'db self,
        probe: &IndexProbe,
    ) -> Vec<&'db P::MaterializedView> {
        self.probed_relation_slots(probe)
            .filter_map(|slot| {
                match slot {
                    RelationSlot::MaterializedView(position) => {
                        self.materialized_views.get(*position)
                    }
                    RelationSlot::Table(_) | RelationSlot::View(_) => None,
                }
            })
            .map(|(view, _)| view.as_ref())
            .filter(|view| probe.accepts(|| stored_view_key(*view, IdentifierCase::Exact)))
            .collect()
    }

    /// Whether any relation of any kind answers a probe.
    ///
    /// Tables, views and materialized views share one pool of names, so a
    /// schema holding the name under any kind ends a search-path walk, as
    /// PostgreSQL's own name resolution does.
    fn probed_name_is_taken(&self, probe: &IndexProbe) -> bool {
        self.probed_relation_slots(probe).any(|slot| {
            match slot {
                RelationSlot::Table(position) => {
                    self.tables.get(*position).is_some_and(|(table, _)| {
                        probe.accepts(|| stored_table_key(table.as_ref(), IdentifierCase::Exact))
                    })
                }
                RelationSlot::View(position) => {
                    self.views.get(*position).is_some_and(|(view, _)| {
                        probe.accepts(|| stored_view_key(view.as_ref(), IdentifierCase::Exact))
                    })
                }
                RelationSlot::MaterializedView(position) => {
                    self.materialized_views.get(*position).is_some_and(|(view, _)| {
                        probe.accepts(|| stored_view_key(view.as_ref(), IdentifierCase::Exact))
                    })
                }
            }
        })
    }

    /// Functions whose stored identity equals `key`, in storage order.
    fn indexed_functions(&self, key: &RelationKey) -> impl Iterator<Item = &P::Function> {
        self.function_index
            .get(key)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|position| self.functions.get(*position))
            .map(|(function, _)| function.as_ref())
    }

    /// Functions a probe accepts, in storage order.
    fn probed_functions<'db>(&'db self, probe: &IndexProbe) -> Vec<&'db P::Function> {
        let aliases: &[RelationKey] = if probe.aliases {
            self.folded_function_index.get(&probe.key).map_or(&[][..], Vec::as_slice)
        } else {
            &[]
        };
        self.indexed_functions(&probe.key)
            .chain(aliases.iter().flat_map(|key| self.indexed_functions(key)))
            .filter(|function| {
                probe.accepts(|| stored_function_key(*function, IdentifierCase::Exact))
            })
            .collect()
    }

    /// Resolves a written function reference through the index, walking the
    /// search path for an unqualified name.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries
    /// several declarations in the schema that wins.
    fn resolve_target_function_on_path(
        &self,
        target: &TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&P::Function>, LookupError> {
        let written = target.to_string();
        if target.schema().is_some() {
            let probe = IndexProbe::new(target, case, None);
            return resolve_one_function(&written, &self.probed_functions(&probe));
        }

        for (entry_schema, entry_quoted) in &self.search_path {
            let probe = IndexProbe::new(target, case, Some((entry_schema, *entry_quoted)));
            let candidates = self.probed_functions(&probe);
            if !candidates.is_empty() {
                return resolve_one_function(&written, &candidates);
            }
        }

        Ok(None)
    }

    /// Tables whose stored identity equals `key`, in storage order.
    pub(super) fn indexed_tables(&self, key: &RelationKey) -> impl Iterator<Item = &P::Table> {
        self.indexed_relation_slots(key)
            .iter()
            .filter_map(|slot| {
                match slot {
                    RelationSlot::Table(position) => self.tables.get(*position),
                    RelationSlot::View(_) | RelationSlot::MaterializedView(_) => None,
                }
            })
            .map(|(table, _)| table.as_ref())
    }

    /// Resolves a written target against the index, ignoring any search path.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one table.
    pub(super) fn resolve_target_table_strict(
        &self,
        target: &TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&P::Table>, LookupError> {
        let probe = IndexProbe::new(target, case, None);
        resolve_target_from_candidates(target, &self.probed_tables(&probe))
    }

    /// Resolves a written target through the index and the search path.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one table in the schema that wins.
    pub(super) fn resolve_target_table_on_path(
        &self,
        target: &TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&P::Table>, LookupError> {
        self.resolve_relation_on_path(target, case, Self::probed_tables, render_table_candidate)
    }

    /// Resolves a written target to a plain view, applying the search path to
    /// an unqualified name exactly as a table lookup does.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one view in the schema that wins.
    pub(super) fn resolve_target_view_on_path(
        &self,
        target: &TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&P::View>, LookupError> {
        self.resolve_relation_on_path(target, case, Self::probed_views, render_view_candidate)
    }

    /// Resolves a written target to a materialized view, applying the search
    /// path to an unqualified name exactly as a table lookup does.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one materialized view in the schema that wins.
    pub(super) fn resolve_target_materialized_view_on_path(
        &self,
        target: &TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&P::MaterializedView>, LookupError> {
        self.resolve_relation_on_path(
            target,
            case,
            Self::probed_materialized_views,
            render_view_candidate,
        )
    }

    /// Resolves a written target to one relation of a kind, walking the search
    /// path for an unqualified name, with the temporary schema first unless
    /// the path names it.
    fn resolve_relation_on_path<'db, R>(
        &'db self,
        target: &TargetName<'_>,
        case: IdentifierCase,
        candidates_of: impl for<'probe> Fn(&'db Self, &'probe IndexProbe) -> Vec<&'db R>,
        render: impl Fn(&R) -> String,
    ) -> Result<Option<&'db R>, LookupError> {
        if target.schema().is_some() {
            let probe = IndexProbe::new(target, case, None);
            return resolve_one_relation(target, &candidates_of(self, &probe), render);
        }

        let path = || self.search_path.iter().map(|(schema, quoted)| (schema.as_str(), *quoted));
        for (entry_schema, entry_quoted) in relation_search_path(path) {
            let probe = IndexProbe::new(target, case, Some((entry_schema, entry_quoted)));
            if self.probed_name_is_taken(&probe) {
                // Reported under the written name: the entry qualifier is
                // resolution machinery, not something the statement spelled.
                return resolve_one_relation(target, &candidates_of(self, &probe), render);
            }
        }

        Ok(None)
    }
}
