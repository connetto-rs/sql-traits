//! Implement the [`ForeignKeyConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::ast::{
    ConstraintCharacteristics, ConstraintReferenceMatchKind, CreateTable, DeferrableInitial,
    ForeignKeyConstraint, Ident, ObjectName, ReferentialAction,
};

use crate::{
    errors::LookupError,
    structs::{
        ConstraintDeferral, ConstraintObject, ParserDB, TableAttribute, TargetName,
        generic_db::is_candidate_key,
    },
    traits::{ForeignKeyLike, Metadata, database::DatabaseLike, table::TableLike},
    utils::object_name::{
        object_name_identifiers, target_name_of_idents, target_name_of_object_name,
    },
};

impl Metadata for TableAttribute<CreateTable, ForeignKeyConstraint> {
    type Meta = ();
}

impl ForeignKeyLike for TableAttribute<CreateTable, ForeignKeyConstraint> {
    type DB = ParserDB;

    #[inline]
    fn foreign_key_name(&self) -> Option<&str> {
        self.attribute().name.as_ref().map(|s| s.value.as_str())
    }

    fn constraint_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        let table = self.table();
        let held: Vec<_> = table.foreign_keys(database)?.collect();
        Ok(super::create_table::position_among(&held, self).and_then(|position| {
            database.constraint_object_name(table, ConstraintObject::ForeignKey, position)
        }))
    }

    #[inline]
    fn host_table<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.table()
    }

    /// Reads the target as ingestion bound it, the table the name reached
    /// when the key was declared, so neither a later search path nor a later
    /// temporary table of the same name moves it.
    ///
    /// A dialect binding keys on use bound only the schema, so the name
    /// reaches whichever table holds it in that schema now, and none while
    /// the target is absent or a view holds the name.
    fn referenced_table<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError> {
        let foreign_table = &self.attribute().foreign_table;
        let (schema, table) = object_name_identifiers(foreign_table)?;
        database
            .table_by_target(target_name_of_idents(schema, table), database.identifier_case())?
            .ok_or_else(|| LookupError::TableNotFound { object_name: foreign_table.to_string() })
    }

    #[inline]
    fn referenced_table_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.attribute().foreign_table)
    }

    #[inline]
    fn referenced_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        self.attribute()
            .referred_columns
            .iter()
            .map(|column| (column.value.as_str(), column.quote_style.is_some()))
    }

    #[inline]
    fn on_delete(&self, _database: &Self::DB) -> ReferentialAction {
        self.attribute().on_delete.unwrap_or(ReferentialAction::NoAction)
    }

    #[inline]
    fn on_update(&self, _database: &Self::DB) -> ReferentialAction {
        self.attribute().on_update.unwrap_or(ReferentialAction::NoAction)
    }

    #[inline]
    fn match_kind(&self, _database: &Self::DB) -> ConstraintReferenceMatchKind {
        self.attribute().match_kind.unwrap_or(ConstraintReferenceMatchKind::Simple)
    }

    fn is_validated(&self, database: &Self::DB) -> Result<bool, LookupError> {
        super::create_table::holds_validated(self.table(), self.attribute(), database)
    }

    #[inline]
    fn deferral(&self, _database: &Self::DB) -> ConstraintDeferral {
        deferral_of(self.attribute().characteristics)
    }

    fn host_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        let host_table = self.host_table(database);
        resolve_columns(host_table, database, &self.attribute().columns)
    }

    /// A key naming no referenced columns references the primary key of its
    /// target, which only a key bound on use still leaves unnamed, since
    /// binding at declaration spells the key out the way `confkey` does.
    ///
    /// A dialect binding keys on use checks nothing about the target when
    /// the key is declared, so the mismatches SQLite reports on use are
    /// answered here.
    fn referenced_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        let foreign_key = self.attribute();
        let referenced_table = self.referenced_table(database)?;
        let host_table = || self.table().name.to_string();
        if foreign_key.referred_columns.is_empty() {
            let primary_key: Vec<_> = referenced_table.primary_key_columns(database)?.collect();
            if primary_key.is_empty() {
                return Err(LookupError::ReferencedPrimaryKeyNotFound {
                    host_table: host_table(),
                    referenced_table: foreign_key.foreign_table.to_string(),
                });
            }
            if primary_key.len() != foreign_key.columns.len() {
                return Err(LookupError::ForeignKeyColumnCountMismatch {
                    host_table: host_table(),
                    host_columns: foreign_key.columns.len(),
                    referenced_table: foreign_key.foreign_table.to_string(),
                    referenced_columns: primary_key.len(),
                });
            }
            return Ok(primary_key.into_iter());
        }

        let columns = resolve_columns(referenced_table, database, &foreign_key.referred_columns)?;
        if database.dialect().binds_foreign_keys_on_use() {
            // SQLite takes a unique index only when it covers every row and
            // names plain columns.
            let unique_indexes = referenced_table
                .indices(database)?
                .map(TableAttribute::attribute)
                .filter(|index| index.unique && index.predicate.is_none());
            let referred: Vec<&Ident> = foreign_key.referred_columns.iter().collect();
            if !is_candidate_key(
                referenced_table,
                unique_indexes,
                &referred,
                database.identifier_case(),
            ) {
                return Err(LookupError::ReferencedColumnsNotUnique {
                    host_table: host_table(),
                    referenced_table: foreign_key.foreign_table.to_string(),
                    referenced_columns: foreign_key
                        .referred_columns
                        .iter()
                        .map(|column| column.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }
        Ok(columns)
    }
}

/// `NOT DEFERRABLE` outweighs `INITIALLY DEFERRED`, which is how SQLite checks
/// a key declaring both and a combination every other dialect refuses at
/// ingestion. Otherwise `INITIALLY DEFERRED` implies `DEFERRABLE`, as in
/// PostgreSQL.
fn deferral_of(characteristics: Option<ConstraintCharacteristics>) -> ConstraintDeferral {
    let characteristics = characteristics.unwrap_or_default();
    match (characteristics.deferrable, characteristics.initially) {
        (Some(false), _) => ConstraintDeferral::NotDeferrable,
        (_, Some(DeferrableInitial::Deferred)) => ConstraintDeferral::InitiallyDeferred,
        (Some(true), _) => ConstraintDeferral::InitiallyImmediate,
        (None, _) => ConstraintDeferral::NotDeferrable,
    }
}

/// A foreign key as `pg_constraint` stores it, unnamed and with every omitted
/// clause read as its default.
#[derive(PartialEq)]
struct StoredForeignKey<'key> {
    columns: &'key [Ident],
    foreign_table: &'key ObjectName,
    referred_columns: &'key [Ident],
    on_delete: ReferentialAction,
    on_update: ReferentialAction,
    match_kind: ConstraintReferenceMatchKind,
    deferral: ConstraintDeferral,
    enforced: bool,
}

impl<'key> From<&'key ForeignKeyConstraint> for StoredForeignKey<'key> {
    fn from(key: &'key ForeignKeyConstraint) -> Self {
        let ForeignKeyConstraint {
            name: _,
            index_name: _,
            columns,
            foreign_table,
            referred_columns,
            on_delete,
            on_update,
            match_kind,
            characteristics,
        } = key;
        Self {
            columns,
            foreign_table,
            referred_columns,
            on_delete: on_delete.unwrap_or(ReferentialAction::NoAction),
            on_update: on_update.unwrap_or(ReferentialAction::NoAction),
            match_kind: match_kind.unwrap_or(ConstraintReferenceMatchKind::Simple),
            deferral: deferral_of(*characteristics),
            enforced: characteristics.and_then(|written| written.enforced).unwrap_or(true),
        }
    }
}

/// Whether `left` and `right` are one foreign key as PostgreSQL stores it,
/// whatever each is named and whichever defaults each spells out.
pub(crate) fn foreign_keys_stored_alike(
    left: &ForeignKeyConstraint,
    right: &ForeignKeyConstraint,
) -> bool {
    StoredForeignKey::from(left) == StoredForeignKey::from(right)
}

/// Resolves `column_names` against the columns `table` declares, compared
/// under the identifier case the catalog took its names under.
///
/// A foreign key naming a column its table does not declare is a dangling
/// reference rather than a translatable constraint, so it is reported instead
/// of being skipped.
fn resolve_columns<'db>(
    table: &'db <ParserDB as DatabaseLike>::Table,
    database: &'db ParserDB,
    column_names: &[sqlparser::ast::Ident],
) -> Result<alloc::vec::IntoIter<&'db <ParserDB as DatabaseLike>::Column>, LookupError> {
    // Resolve the declared columns once rather than per named column: the
    // metadata lookup is the expensive half and the answer does not change.
    let declared: alloc::vec::Vec<_> = table.columns(database)?.collect();
    let case = database.identifier_case();
    let mut columns = alloc::vec::Vec::with_capacity(column_names.len());
    for column_name in column_names {
        let column = declared
            .iter()
            .copied()
            .find(|column| {
                let declared_name = &column.attribute().name;
                case.identifiers_match(
                    &declared_name.value,
                    declared_name.quote_style.is_some(),
                    &column_name.value,
                    column_name.quote_style.is_some(),
                )
            })
            .ok_or_else(|| {
                LookupError::ColumnNotFound {
                    table_name: table.table_name().to_string(),
                    column_name: column_name.value.clone(),
                }
            })?;
        columns.push(column);
    }

    Ok(columns.into_iter())
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use crate::{
        prelude::ParserDB,
        structs::{IdentifierCase, TargetName},
        traits::{ColumnLike, DatabaseLike, ForeignKeyLike, TableLike},
    };

    /// An unnamed inline `REFERENCES` clause produces a foreign key whose
    /// `foreign_key_name()` returns `None`.
    #[test]
    fn test_unnamed_inline_foreign_key_has_no_name() {
        let sql = "
            CREATE TABLE parent (id INT PRIMARY KEY);
            CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = child.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");
        assert!(fk.foreign_key_name().is_none(), "inline REFERENCES has no name");
    }

    /// `CONSTRAINT name REFERENCES` names the foreign key the way the
    /// table-constraint spelling does.
    #[test]
    fn test_named_inline_foreign_key_keeps_its_name() {
        let sql = "
            CREATE TABLE parent (id INT PRIMARY KEY);
            CREATE TABLE child (id INT PRIMARY KEY, parent_id INT CONSTRAINT fk_parent REFERENCES parent(id));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = child.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");
        assert_eq!(fk.foreign_key_name(), Some("fk_parent"));
    }

    /// Every written action reaches `on_delete` and `on_update` unchanged,
    /// from both the table-constraint and the inline column spelling, and an
    /// action left unwritten reads as `NO ACTION`, the PostgreSQL default.
    #[test]
    fn test_referential_actions_follow_the_declaration() {
        use sqlparser::ast::ReferentialAction::{Cascade, NoAction, Restrict, SetDefault, SetNull};

        let sql = "
            CREATE TABLE parent (id INT PRIMARY KEY);
            CREATE TABLE child (
                unwritten INT REFERENCES parent(id),
                inline_key INT REFERENCES parent(id) ON DELETE SET NULL ON UPDATE CASCADE,
                FOREIGN KEY (unwritten) REFERENCES parent(id) ON DELETE SET DEFAULT ON UPDATE RESTRICT,
                FOREIGN KEY (unwritten) REFERENCES parent(id) ON UPDATE SET NULL ON DELETE NO ACTION,
                FOREIGN KEY (unwritten) REFERENCES parent(id) ON DELETE CASCADE ON UPDATE SET DEFAULT,
                FOREIGN KEY (unwritten) REFERENCES parent(id) ON DELETE RESTRICT
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let mut actions: Vec<_> = child
            .foreign_keys(&db)
            .expect("fk lookup")
            .map(|fk| {
                let host = fk.host_columns(&db).expect("host columns").next().expect("one column");
                (host.column_name(), fk.on_delete(&db), fk.on_update(&db))
            })
            .collect();
        actions.sort_unstable_by_key(|&(_, on_delete, on_update)| (on_delete, on_update));
        let mut expected = vec![
            ("unwritten", NoAction, NoAction),
            ("inline_key", SetNull, Cascade),
            ("unwritten", SetDefault, Restrict),
            ("unwritten", NoAction, SetNull),
            ("unwritten", Cascade, SetDefault),
            ("unwritten", Restrict, NoAction),
        ];
        expected.sort_unstable_by_key(|&(_, on_delete, on_update)| (on_delete, on_update));
        assert_eq!(actions, expected);
    }

    /// A multi-column FK reports both host and referenced columns in
    /// declaration order.
    #[test]
    fn test_multi_column_foreign_key_columns() {
        let sql = "
            CREATE TABLE parent (a INT, b INT, PRIMARY KEY (a, b));
            CREATE TABLE child (
                x INT,
                y INT,
                CONSTRAINT fk FOREIGN KEY (x, y) REFERENCES parent(a, b)
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = child.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");

        let host_names: Vec<&str> = fk
            .host_columns(&db)
            .expect("host_columns lookup")
            .map(ColumnLike::column_name)
            .collect();
        let ref_names: Vec<&str> = fk
            .referenced_columns(&db)
            .expect("referenced_columns lookup")
            .map(ColumnLike::column_name)
            .collect();
        assert_eq!(host_names, vec!["x", "y"]);
        assert_eq!(ref_names, vec!["a", "b"]);
    }

    /// A self-referential FK targets the same table as its host.
    #[test]
    fn test_self_referential_foreign_key() {
        let sql = "
            CREATE TABLE t (
                id INT PRIMARY KEY,
                parent_id INT REFERENCES t(id)
            );
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let t = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = t.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");

        assert_eq!(fk.host_table(&db).table_name(), "t");
        assert_eq!(fk.referenced_table(&db).expect("ref table lookup").table_name(), "t");
    }

    /// The referenced table resolves under PostgreSQL identifier folding even
    /// when the `REFERENCES` clause spells the target with different casing
    /// than the `CREATE TABLE` that defines it. Regression: the previous
    /// resolver compared raw strings and never matched a case-differing
    /// unquoted reference, panicking instead.
    #[test]
    fn test_referenced_table_resolves_case_insensitively() {
        let sql = "
            CREATE TABLE parent (id INT PRIMARY KEY);
            CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES Parent(id));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = child.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");
        assert_eq!(fk.referenced_table(&db).expect("ref table lookup").table_name(), "parent");
    }

    /// `match_kind()` defaults to `Simple` when no `MATCH` clause is given.
    #[test]
    fn test_match_kind_defaults_to_simple() {
        use sqlparser::ast::ConstraintReferenceMatchKind;

        let sql = "
            CREATE TABLE parent (id INT PRIMARY KEY);
            CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
        ";
        let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
        let child = db
            .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap();
        let fk = child.foreign_keys(&db).expect("fk lookup").next().expect("FK should exist");
        assert!(matches!(fk.match_kind(&db), ConstraintReferenceMatchKind::Simple));
    }
}
