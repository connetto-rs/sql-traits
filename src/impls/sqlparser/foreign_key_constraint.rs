//! Implement the [`ForeignKeyConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use alloc::string::ToString;

use sqlparser::ast::{
    ConstraintReferenceMatchKind, CreateTable, ForeignKeyConstraint, ReferentialAction,
};

use crate::{
    errors::LookupError,
    structs::{ParserDB, TableAttribute, TargetName},
    traits::{ForeignKeyLike, Metadata, database::DatabaseLike, table::TableLike},
    utils::object_name::{resolve_required_bound_table, target_name_of_object_name},
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
    fn referenced_table<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError> {
        resolve_required_bound_table(&self.attribute().foreign_table, database)
    }

    #[inline]
    fn referenced_table_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.attribute().foreign_table)
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

    fn referenced_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        let referenced_table = self.referenced_table(database)?;
        resolve_columns(referenced_table, database, &self.attribute().referred_columns)
    }
}

/// Resolves `column_names` against the columns `table` declares.
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
    let mut columns = alloc::vec::Vec::with_capacity(column_names.len());
    for column_name in column_names {
        let column = declared
            .iter()
            .copied()
            .find(|column| &column.attribute().name == column_name)
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
