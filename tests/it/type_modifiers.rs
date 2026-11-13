//! The type modifier each declared column type applies.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared types.")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

/// Every column answers the `pg_attribute.atttypmod` a PostgreSQL 18 server
/// stores for the same declaration, and a modifier the model cannot encode
/// stays unknown.
#[test]
fn columns_answer_the_modifier_postgres_stores() -> Result<(), sql_traits::errors::Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE DOMAIN d3 AS VARCHAR(3);
         CREATE TABLE t (
             c_char CHAR, c_char5 CHAR(5), c_vc VARCHAR, c_vc5 VARCHAR(5),
             c_num NUMERIC, c_num10 NUMERIC(10), c_num102 NUMERIC(10, 2),
             c_bit BIT, c_bit3 BIT(3), c_vb VARBIT, c_vb4 VARBIT(4),
             c_ts TIMESTAMP, c_ts3 TIMESTAMP(3), c_tstz3 TIMESTAMPTZ(3), c_t2 TIME(2), c_ttz2 TIMETZ(2),
             c_iv INTERVAL, c_iv3 INTERVAL(3), c_ivy INTERVAL YEAR,
             c_ivds2 INTERVAL DAY TO SECOND(2), c_ivhm INTERVAL HOUR TO MINUTE,
             c_arr INT[], c_vcarr VARCHAR(5)[], c_dom d3, c_int INT, c_text TEXT,
             c_geo geometry(Point, 4326)
         );",
    )?;
    let table =
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    let modifiers: Vec<_> = table
        .columns(&db)?
        .map(|column| (column.column_name(), column.data_type(&db).typmod(&db)))
        .collect();
    let modified = |value| Some(TypeModifier::Modified(value));
    let unmodified = Some(TypeModifier::Unmodified);
    assert_eq!(
        modifiers,
        [
            ("c_char", modified(5)),
            ("c_char5", modified(9)),
            ("c_vc", unmodified),
            ("c_vc5", modified(9)),
            ("c_num", unmodified),
            ("c_num10", modified(655_364)),
            ("c_num102", modified(655_366)),
            ("c_bit", modified(1)),
            ("c_bit3", modified(3)),
            ("c_vb", unmodified),
            ("c_vb4", modified(4)),
            ("c_ts", unmodified),
            ("c_ts3", modified(3)),
            ("c_tstz3", modified(3)),
            ("c_t2", modified(2)),
            ("c_ttz2", modified(2)),
            ("c_iv", unmodified),
            ("c_iv3", modified(2_147_418_115)),
            ("c_ivy", modified(327_679)),
            ("c_ivds2", modified(470_286_338)),
            ("c_ivhm", modified(201_392_127)),
            ("c_arr", unmodified),
            ("c_vcarr", modified(9)),
            ("c_dom", unmodified),
            ("c_int", unmodified),
            ("c_text", unmodified),
            ("c_geo", None),
        ]
    );
    assert_eq!(TypeModifier::Modified(9).value(), Some(9));
    assert_eq!(TypeModifier::Unmodified.value(), None);
    Ok(())
}

/// A backend type that answers every parameter as the parser profile does but
/// stores no modifier, so it answers [`DataTypeLike::typmod`] by default.
#[derive(Debug)]
struct Forwarded<'a> {
    declared: &'a sqlparser::ast::DataType,
    element: Option<Box<Forwarded<'a>>>,
}

impl<'a> Forwarded<'a> {
    fn new(declared: &'a sqlparser::ast::DataType, database: &'a ParserDB) -> Self {
        let element =
            declared.array_element(database).map(|element| Box::new(Self::new(element, database)));
        Self { declared, element }
    }
}

impl DataTypeLike for Forwarded<'_> {
    type DB = ParserDB;

    fn name<'db>(&'db self, database: &'db ParserDB) -> std::borrow::Cow<'db, str> {
        self.declared.name(database)
    }

    fn max_binary_length(&self, database: &ParserDB) -> Option<u64> {
        self.declared.max_binary_length(database)
    }

    fn max_bit_length(&self, database: &ParserDB) -> Option<u64> {
        self.declared.max_bit_length(database)
    }

    fn max_character_length(&self, database: &ParserDB) -> Option<u64> {
        self.declared.max_character_length(database)
    }

    fn character_length_unit(&self, database: &ParserDB) -> Option<CharacterLengthUnit> {
        self.declared.character_length_unit(database)
    }

    fn fractional_seconds_precision(&self, database: &ParserDB) -> Option<u64> {
        self.declared.fractional_seconds_precision(database)
    }

    fn time_zone<'db>(&'db self, database: &'db ParserDB) -> Option<&'db str> {
        self.declared.time_zone(database)
    }

    fn numeric_precision(&self, database: &ParserDB) -> Option<u64> {
        self.declared.numeric_precision(database)
    }

    fn numeric_scale(&self, database: &ParserDB) -> Option<i64> {
        self.declared.numeric_scale(database)
    }

    fn is_unsigned(&self, database: &ParserDB) -> Option<bool> {
        self.declared.is_unsigned(database)
    }

    fn admits_null(&self, database: &ParserDB) -> bool {
        self.declared.admits_null(database)
    }

    fn send_function(&self, database: &ParserDB) -> Option<bool> {
        self.declared.send_function(database)
    }

    fn array_element<'db>(&'db self, _database: &'db ParserDB) -> Option<&'db Self> {
        self.element.as_deref()
    }

    fn range_subtype<'db>(&'db self, _database: &'db ParserDB) -> Option<&'db Self> {
        None
    }

    fn structure_kind(&self, database: &ParserDB) -> Option<StructureKind> {
        self.declared.structure_kind(database)
    }

    fn structured_fields<'db>(
        &'db self,
        _database: &'db ParserDB,
    ) -> Option<impl Iterator<Item = StructuredField<'db, Self>>> {
        None::<core::iter::Empty<StructuredField<'db, Self>>>
    }

    fn map_entry<'db>(&'db self, _database: &'db ParserDB) -> Option<MapEntry<'db, Self>> {
        None
    }

    fn enumeration<'db>(
        &'db self,
        _database: &'db ParserDB,
    ) -> Option<Enumeration<impl Iterator<Item = EnumerationMember<'db>>>> {
        None::<Enumeration<core::iter::Empty<EnumerationMember<'db>>>>
    }
}

/// A backend that stores no modifier answers one wherever the parameters it
/// reports tell it apart, and leaves the rest unknown.
#[test]
fn the_default_reads_the_modifier_from_unambiguous_parameters()
-> Result<(), sql_traits::errors::Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE DOMAIN d3 AS VARCHAR(3);
         CREATE TABLE t (
             c_char CHAR, c_vc5 VARCHAR(5), c_vc VARCHAR, c_num NUMERIC, c_num102 NUMERIC(10, 2),
             c_bit BIT, c_vb4 VARBIT(4), c_int INT, c_ts TIMESTAMP, c_iv INTERVAL DAY TO SECOND(2),
             c_dom d3, c_vcarr VARCHAR(5)[], c_arr INT[]
         );",
    )?;
    let table =
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    let modifiers: Vec<_> = table
        .columns(&db)?
        .map(|column| Forwarded::new(column.data_type(&db), &db).typmod(&db))
        .collect();
    let modified = |value| Some(TypeModifier::Modified(value));
    let unmodified = Some(TypeModifier::Unmodified);
    assert_eq!(
        modifiers,
        [
            modified(5),
            modified(9),
            unmodified,
            unmodified,
            modified(655_366),
            modified(1),
            modified(4),
            unmodified,
            None,
            None,
            None,
            modified(9),
            unmodified,
        ]
    );
    Ok(())
}
