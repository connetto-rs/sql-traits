//! The index PostgreSQL builds for each `EXCLUDE` constraint, as measured on
//! PostgreSQL 18.7 through `pg_constraint.conindid` and `pg_index`.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{ast::IndexType, dialect::PostgreSqlDialect};

/// Parses PostgreSQL schema statements.
fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

/// The table `name` of `database`.
fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

/// The names the exclusion indices of table `name` answer to, in order.
fn exclusion_index_names(database: &ParserDB, name: &str) -> Vec<String> {
    table(database, name)
        .exclusion_indices(database)
        .expect("table belongs to database")
        .map(|index| {
            index
                .backing_index_name(database)
                .expect("index belongs to database")
                .expect("PostgreSQL names every exclusion index")
                .into_owned()
        })
        .collect()
}

#[test]
fn an_exclusion_constraint_builds_a_non_unique_index_of_its_own() {
    let database =
        parse("CREATE TABLE reservations (period INT4RANGE, EXCLUDE USING gist (period WITH &&));");
    let reservations = table(&database, "reservations");
    let mut indices = reservations.exclusion_indices(&database).expect("table belongs to database");
    let index = indices.next().expect("one exclusion index");
    assert!(indices.next().is_none(), "a second exclusion index");

    assert_eq!(index.name(), None);
    assert_eq!(
        index.constraint_name(&database).expect("index belongs to database").as_deref(),
        Some("reservations_period_excl")
    );
    assert_eq!(exclusion_index_names(&database, "reservations"), ["reservations_period_excl"]);
    assert!(!index.is_unique());
    assert_eq!(index.access_method(), Some(&IndexType::GiST));
    let keys: Vec<_> = index
        .keys(&database)
        .expect("index belongs to database")
        .map(|key| key.expression().to_string())
        .collect();
    assert_eq!(keys, ["period"]);
    assert_eq!(index.deferral(&database), ConstraintDeferral::NotDeferrable);
    assert_eq!(index.predicate(), None);
    assert!(index.include_column_names().next().is_none());
    assert_eq!(TableIndexLike::table(index, &database).table_name(), "reservations");
    assert!(reservations.has_exclusion_constraints(&database).expect("table belongs to database"));
    assert_eq!(reservations.indices(&database).expect("table belongs to database").count(), 0);
    assert_eq!(
        reservations.unique_indices(&database).expect("table belongs to database").count(),
        0
    );
}

#[test]
fn an_added_exclusion_constraint_answers_every_clause_it_writes() {
    let database = parse(
        "CREATE TABLE bookings (room INT, period INT4RANGE);
         ALTER TABLE bookings ADD CONSTRAINT bookings_no_overlap
             EXCLUDE USING gist (period range_ops WITH &&) INCLUDE (room) WHERE (room > 0)
             DEFERRABLE INITIALLY DEFERRED;",
    );
    let bookings = table(&database, "bookings");
    let index = bookings
        .exclusion_indices(&database)
        .expect("table belongs to database")
        .next()
        .expect("one exclusion index");

    assert_eq!(index.name(), Some("bookings_no_overlap"));
    assert_eq!(exclusion_index_names(&database, "bookings"), ["bookings_no_overlap"]);
    let key = index.keys(&database).expect("index belongs to database").next().expect("one key");
    assert_eq!(key.expression().to_string(), "period");
    assert_eq!(key.operator_class().map(ToString::to_string).as_deref(), Some("range_ops"));
    let included: Vec<_> = index.include_column_names().collect();
    assert_eq!(included, [("room", false)]);
    let columns: Vec<_> = index
        .include_columns(&database)
        .expect("index belongs to database")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(columns, ["room"]);
    assert_eq!(index.predicate().map(ToString::to_string).as_deref(), Some("room > 0"));
    assert_eq!(index.deferral(&database), ConstraintDeferral::InitiallyDeferred);
}

#[test]
fn the_index_follows_renames_of_its_constraint_its_table_and_its_columns() {
    let schema =
        "CREATE TABLE reservations (period INT4RANGE, EXCLUDE USING gist (period WITH &&));
         CREATE TABLE bookings (room INT, period INT4RANGE,
             CONSTRAINT bookings_no_overlap EXCLUDE USING gist (period WITH &&));
         ALTER TABLE bookings RENAME CONSTRAINT bookings_no_overlap TO no_overlap;
         ALTER TABLE reservations RENAME TO stays;
         ALTER TABLE stays RENAME COLUMN period TO span;";
    let database = parse(schema);
    assert_eq!(exclusion_index_names(&database, "bookings"), ["no_overlap"]);
    assert_eq!(exclusion_index_names(&database, "stays"), ["reservations_period_excl"]);
    let index = table(&database, "stays")
        .exclusion_indices(&database)
        .expect("table belongs to database")
        .next()
        .expect("one exclusion index");
    let columns: Vec<_> = index
        .columns(&database)
        .expect("index belongs to database")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(columns, ["span"]);

    let database = parse(&format!("{schema} ALTER INDEX no_overlap RENAME TO overlap_index;"));
    assert_eq!(exclusion_index_names(&database, "bookings"), ["overlap_index"]);
}

#[test]
fn dropping_the_constraint_drops_its_index() {
    let database = parse(
        "CREATE TABLE reservations (period INT4RANGE, EXCLUDE USING gist (period WITH &&));
         ALTER TABLE reservations DROP CONSTRAINT reservations_period_excl;",
    );
    assert_eq!(exclusion_index_names(&database, "reservations"), Vec::<String>::new());
    assert!(
        !table(&database, "reservations")
            .has_exclusion_constraints(&database)
            .expect("table belongs to database")
    );
}

#[test]
fn each_partition_holds_an_exclusion_index_under_a_name_of_its_own() {
    let database = parse(
        "CREATE TABLE rooms (room INT) PARTITION BY LIST (room);
         CREATE TABLE rooms_1 PARTITION OF rooms FOR VALUES IN (1);
         ALTER TABLE rooms ADD CONSTRAINT rooms_x EXCLUDE (room WITH =);
         CREATE TABLE rooms_2 PARTITION OF rooms FOR VALUES IN (2);
         CREATE TABLE rooms_3 (room INT);
         ALTER TABLE rooms ATTACH PARTITION rooms_3 FOR VALUES IN (3);
         CREATE TABLE rooms_4 (room INT, CONSTRAINT own EXCLUDE (room WITH =));
         ALTER TABLE rooms ATTACH PARTITION rooms_4 FOR VALUES IN (4);",
    );
    for (name, expected) in [
        ("rooms", "rooms_x"),
        ("rooms_1", "rooms_1_room_excl"),
        ("rooms_2", "rooms_2_room_excl"),
        ("rooms_3", "rooms_3_room_excl"),
        ("rooms_4", "own"),
    ] {
        assert_eq!(exclusion_index_names(&database, name), [expected], "{name}");
    }
}

#[test]
fn an_exclusion_added_only_to_a_partitioned_table_holding_a_partition_is_invalid() {
    let database = parse(
        "CREATE TABLE halls (room INT) PARTITION BY LIST (room);
         CREATE TABLE halls_1 PARTITION OF halls FOR VALUES IN (1);
         ALTER TABLE ONLY halls ADD CONSTRAINT halls_x EXCLUDE (room WITH =);
         CREATE TABLE rooms (room INT) PARTITION BY LIST (room);
         ALTER TABLE ONLY rooms ADD CONSTRAINT rooms_x EXCLUDE (room WITH =);",
    );
    for (name, valid) in [("halls", false), ("rooms", true)] {
        let index = table(&database, name)
            .exclusion_indices(&database)
            .expect("table belongs to database")
            .next()
            .expect("one exclusion index");
        assert_eq!(index.is_valid(&database).expect("index belongs to database"), valid, "{name}");
    }
    assert_eq!(exclusion_index_names(&database, "halls_1"), Vec::<String>::new());
}

#[test]
fn a_call_in_an_exclusion_key_reaches_its_function() {
    let database = parse(
        "CREATE FUNCTION bucket(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x / 10';
         CREATE TABLE readings (x INT, EXCLUDE ((bucket(x)) WITH =));",
    );
    let index = table(&database, "readings")
        .exclusion_indices(&database)
        .expect("table belongs to database")
        .next()
        .expect("one exclusion index");
    assert_eq!(exclusion_index_names(&database, "readings"), ["readings_bucket_excl"]);
    let key = index.keys(&database).expect("index belongs to database").next().expect("one key");
    let call = match key.expression() {
        sqlparser::ast::Expr::Nested(nested) => {
            match nested.as_ref() {
                sqlparser::ast::Expr::Function(call) => call,
                other => panic!("the key is a call, found {other}"),
            }
        }
        other => panic!("the key is a parenthesized call, found {other}"),
    };
    let reached: Vec<_> = index
        .functions_reached_by(call, &database)
        .expect("the call is in the key")
        .map(FunctionLike::name)
        .collect();
    assert_eq!(reached, ["bucket"]);
}

#[test]
fn a_call_outside_the_key_is_refused() {
    let database = parse(
        "CREATE FUNCTION bucket(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x / 10';
         CREATE TABLE readings (x INT, EXCLUDE ((bucket(x)) WITH =));
         CREATE TABLE samples (x INT, EXCLUDE ((bucket(x)) WITH =));",
    );
    let index_of = |name| {
        table(&database, name)
            .exclusion_indices(&database)
            .expect("table belongs to database")
            .next()
            .expect("one exclusion index")
    };
    let sqlparser::ast::Expr::Nested(nested) = index_of("samples")
        .keys(&database)
        .expect("index belongs to database")
        .next()
        .expect("one key")
        .expression()
    else {
        panic!("the key is parenthesized");
    };
    let sqlparser::ast::Expr::Function(call) = nested.as_ref() else {
        panic!("the key is a call");
    };
    assert!(matches!(
        index_of("readings").functions_reached_by(call, &database).map(Iterator::count),
        Err(LookupError::CallNotInExpression { .. })
    ));
}

#[test]
fn renaming_the_function_of_a_key_respells_only_the_keys_calling_it() {
    let database = parse(
        "CREATE FUNCTION bucket(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x / 10';
         CREATE TABLE readings (x INT, EXCLUDE ((bucket(x)) WITH =));
         CREATE TABLE magnitudes (x INT, EXCLUDE ((abs(x)) WITH =));
         ALTER FUNCTION bucket(INT) RENAME TO bin;",
    );
    for (name, expected) in [("readings", "(bin(x))"), ("magnitudes", "(abs(x))")] {
        let index = table(&database, name)
            .exclusion_indices(&database)
            .expect("table belongs to database")
            .next()
            .expect("one exclusion index");
        let key =
            index.keys(&database).expect("index belongs to database").next().expect("one key");
        assert_eq!(key.expression().to_string(), expected, "{name}");
    }
}

#[test]
fn an_exclusion_index_answers_only_the_database_holding_it() {
    let database = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.rooms (room INT, CONSTRAINT \"Rooms_X\" EXCLUDE (room WITH =));
         CREATE TABLE app.halls (room INT, EXCLUDE (room WITH =));",
    );
    let other = parse("CREATE TABLE unrelated (room INT);");
    let index_of = |name: &str| {
        database
            .tables()
            .find(|table| table.table_name() == name)
            .expect("table exists")
            .exclusion_indices(&database)
            .expect("table belongs to database")
            .next()
            .expect("one exclusion index")
    };
    let (rooms, halls) = (index_of("rooms"), index_of("halls"));

    assert!(rooms.name_is_quoted());
    assert!(!halls.name_is_quoted());
    assert_eq!((rooms.schema(), rooms.schema_is_quoted()), (Some("app"), false));
    assert!(matches!(
        rooms.relation(&database),
        Ok(Relation::Table(table)) if table.table_name() == "rooms"
    ));
    let missing = |result: Result<_, LookupError>| {
        match result {
            Err(LookupError::ObjectNotInDatabase { object, .. }) => object,
            Ok(()) => panic!("the other database answered"),
            Err(error) => panic!("unexpected error {error}"),
        }
    };
    assert_eq!(missing(rooms.relation(&other).map(drop)), "`Rooms_X`");
    assert_eq!(missing(halls.expression(&other).map(drop)), "declared on table `halls`");
}

#[test]
fn outside_postgresql_an_exclusion_index_answers_its_written_name() {
    let database = ParserDB::parse::<sqlparser::dialect::GenericDialect>(
        "CREATE TABLE slots (x INT, CONSTRAINT slots_x EXCLUDE (x WITH =), EXCLUDE (x WITH <>));",
    )
    .expect("schema parses");
    let names: Vec<_> = table(&database, "slots")
        .exclusion_indices(&database)
        .expect("table belongs to database")
        .map(|index| {
            index
                .backing_index_name(&database)
                .expect("index belongs to database")
                .map(std::borrow::Cow::into_owned)
        })
        .collect();
    assert_eq!(names, [Some("slots_x".to_owned()), None]);
}
