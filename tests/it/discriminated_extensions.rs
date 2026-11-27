//! Tests that a subtype keyed to its supertype through the primary key plus
//! columns held to constants, by generation or by a check, extends the
//! supertype, and that the key hands those constants to whoever writes the
//! supertype's row.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the schema parses")
}

/// The foreign key of `table_name` whose host columns are `host_columns`.
fn foreign_key<'db>(
    database: &'db ParserDB,
    table_name: &str,
    host_columns: &[&str],
) -> &'db <ParserDB as DatabaseLike>::ForeignKey {
    table(database, None, table_name)
        .foreign_keys(database)
        .expect("the table is in the database")
        .find(|foreign_key| {
            foreign_key
                .host_columns(database)
                .expect("the key resolves")
                .map(ColumnLike::column_name)
                .eq(host_columns.iter().copied())
        })
        .expect("the key exists")
}

fn is_extension(database: &ParserDB, table_name: &str, host_columns: &[&str]) -> bool {
    foreign_key(database, table_name, host_columns)
        .is_extension_foreign_key(database)
        .expect("the key resolves")
}

/// The ancestors of `table_name`, root first.
fn ancestors<'db>(database: &'db ParserDB, table_name: &str) -> Vec<&'db str> {
    table(database, None, table_name)
        .ancestral_extended_tables_topological(database)
        .expect("every key resolves")
        .into_iter()
        .map(TableLike::table_name)
        .collect()
}

/// The referenced columns the key fills with a constant, each with the
/// constant as written.
fn constants(
    database: &ParserDB,
    table_name: &str,
    host_columns: &[&str],
) -> Vec<(String, String)> {
    foreign_key(database, table_name, host_columns)
        .referenced_constants(database)
        .expect("the key resolves")
        .map(|(column, constant)| (column.column_name().to_owned(), constant.to_string()))
        .collect()
}

const RESOURCES: &str = "
CREATE TYPE resource_kind AS ENUM ('organization', 'folder', 'project', 'ownable');
CREATE TABLE resources (
  id UUID PRIMARY KEY,
  kind resource_kind NOT NULL,
  organization_id UUID NOT NULL,
  UNIQUE (id, kind),
  UNIQUE (organization_id, id)
);
CREATE TABLE ownables (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL,
  kind resource_kind GENERATED ALWAYS AS ('ownable'::resource_kind) STORED NOT NULL,
  project_id UUID NOT NULL,
  FOREIGN KEY (id, kind) REFERENCES resources (id, kind) DEFERRABLE INITIALLY DEFERRED,
  FOREIGN KEY (organization_id, id) REFERENCES resources (organization_id, id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE asset_models (
  id UUID PRIMARY KEY REFERENCES ownables (id) ON DELETE CASCADE,
  name TEXT NOT NULL
);
";

/// The primary key plus a discriminator generated as a constant, into the
/// supertype's primary key plus its discriminator, is an extension key.
#[test]
fn a_key_carrying_a_generated_discriminator_extends_the_supertype() {
    let database = parse(RESOURCES);
    assert!(is_extension(&database, "ownables", &["id", "kind"]));
    assert_eq!(ancestors(&database, "ownables"), ["resources"]);
    assert_eq!(ancestors(&database, "asset_models"), ["resources", "ownables"]);
}

/// A host column beside the primary key that is not generated as a constant
/// makes the key a reference, so a table keeps one extension key per parent.
#[test]
fn a_key_carrying_a_written_column_is_no_extension() {
    let database = parse(RESOURCES);
    assert!(!is_extension(&database, "ownables", &["organization_id", "id"]));
    let extension_keys: Vec<_> = table(&database, None, "ownables")
        .extension_foreign_keys(&database)
        .expect("every key resolves")
        .collect();
    assert_eq!(extension_keys, [foreign_key(&database, "ownables", &["id", "kind"])]);
}

/// The key hands over the constant its generated column holds, so a writer of
/// the subtype knows what the supertype's discriminator must be.
#[test]
fn the_key_hands_over_the_constant_of_each_discriminator() {
    let database = parse(RESOURCES);
    assert_eq!(
        constants(&database, "ownables", &["id", "kind"]),
        [("kind".to_owned(), "'ownable'::resource_kind".to_owned())],
    );
    assert_eq!(constants(&database, "ownables", &["organization_id", "id"]), []);
    assert_eq!(constants(&database, "asset_models", &["id"]), []);
}

/// Only a column generated as one literal other than `NULL` is a
/// discriminator, since a key holding `NULL` is never checked.
#[test]
fn only_a_non_null_literal_is_a_discriminator() {
    let database = parse(
        "
CREATE TABLE parent (id INT PRIMARY KEY, kind TEXT NOT NULL, code INT NOT NULL,
  since DATE NOT NULL, UNIQUE (id, kind), UNIQUE (id, code), UNIQUE (id, since));
CREATE TABLE written (id INT PRIMARY KEY, kind TEXT NOT NULL,
  FOREIGN KEY (id, kind) REFERENCES parent (id, kind));
CREATE TABLE computed (id INT PRIMARY KEY, kind TEXT GENERATED ALWAYS AS (id::text) STORED,
  FOREIGN KEY (id, kind) REFERENCES parent (id, kind));
CREATE TABLE nothing (id INT PRIMARY KEY, kind TEXT GENERATED ALWAYS AS (NULL) STORED,
  FOREIGN KEY (id, kind) REFERENCES parent (id, kind));
CREATE TABLE negative (id INT PRIMARY KEY, code INT GENERATED ALWAYS AS ((-1)) STORED,
  FOREIGN KEY (id, code) REFERENCES parent (id, code));
CREATE TABLE plain (id INT PRIMARY KEY, kind TEXT GENERATED ALWAYS AS ('plain') STORED,
  FOREIGN KEY (kind, id) REFERENCES parent (kind, id));
CREATE TABLE dated (id INT PRIMARY KEY, since DATE GENERATED ALWAYS AS (DATE '2020-01-01') STORED,
  FOREIGN KEY (id, since) REFERENCES parent (id, since));
",
    );
    assert!(!is_extension(&database, "written", &["id", "kind"]));
    assert!(!is_extension(&database, "computed", &["id", "kind"]));
    assert!(!is_extension(&database, "nothing", &["id", "kind"]));
    assert!(is_extension(&database, "negative", &["id", "code"]));
    assert_eq!(
        constants(&database, "negative", &["id", "code"]),
        [("code".to_owned(), "(-1)".to_owned())]
    );
    assert!(is_extension(&database, "plain", &["kind", "id"]));
    assert!(is_extension(&database, "dated", &["id", "since"]));
    assert_eq!(constants(&database, "computed", &["id", "kind"]), []);
}

/// The host primary key must land on the referenced primary key, column for
/// column, so a host without a primary key or with a wider one extends
/// nothing, and a self-reference stays no extension.
#[test]
fn the_primary_keys_must_meet_column_for_column() {
    let database = parse(
        "
CREATE TABLE parent (id INT PRIMARY KEY, code INT NOT NULL, kind TEXT NOT NULL,
  UNIQUE (code, kind), UNIQUE (id, kind));
CREATE TABLE elsewhere (id INT PRIMARY KEY, kind TEXT GENERATED ALWAYS AS ('elsewhere') STORED,
  FOREIGN KEY (id, kind) REFERENCES parent (code, kind));
CREATE TABLE looped (id INT PRIMARY KEY, kind TEXT GENERATED ALWAYS AS ('looped') STORED,
  UNIQUE (id, kind),
  FOREIGN KEY (id, kind) REFERENCES looped (id, kind));
CREATE TABLE pair (a INT, b INT, PRIMARY KEY (a, b));
CREATE TABLE swapped (a INT, b INT, PRIMARY KEY (a, b),
  FOREIGN KEY (b, a) REFERENCES pair (b, a));
CREATE TABLE crossed (a INT, b INT, PRIMARY KEY (a, b),
  FOREIGN KEY (a, b) REFERENCES pair (b, a));
CREATE TABLE keyless (id INT, kind TEXT GENERATED ALWAYS AS ('keyless') STORED,
  FOREIGN KEY (id, kind) REFERENCES parent (id, kind));
CREATE TABLE wider (id INT, kind TEXT, PRIMARY KEY (id, kind),
  FOREIGN KEY (id, kind) REFERENCES parent (id, kind));
",
    );
    assert!(!is_extension(&database, "elsewhere", &["id", "kind"]));
    assert!(!is_extension(&database, "looped", &["id", "kind"]));
    assert!(is_extension(&database, "swapped", &["b", "a"]));
    assert!(!is_extension(&database, "crossed", &["a", "b"]));
    assert!(!is_extension(&database, "keyless", &["id", "kind"]));
    assert!(!is_extension(&database, "wider", &["id", "kind"]));
}

/// A `NOT NULL` column that a validated check of its table holds `=` one
/// literal is a discriminator, as a column generated as that literal is.
#[test]
fn a_check_pinning_a_not_null_column_makes_a_discriminator() {
    let database = parse(
        "
CREATE TYPE grantee_kind AS ENUM ('user', 'organization', 'team', 'bot', 'group', 'guest');
CREATE TABLE grantees (id UUID PRIMARY KEY, kind grantee_kind NOT NULL, UNIQUE (id, kind));
CREATE TABLE users (
  id UUID PRIMARY KEY,
  kind grantee_kind NOT NULL DEFAULT 'user' CHECK (kind = 'user'),
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
CREATE TABLE organizations (
  id UUID PRIMARY KEY,
  kind grantee_kind NOT NULL,
  CHECK ('organization'::grantee_kind = kind AND id IS NOT NULL),
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
CREATE TABLE teams (
  id UUID PRIMARY KEY,
  kind grantee_kind DEFAULT 'team' CHECK (kind = 'team'),
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
CREATE TABLE bots (
  id UUID PRIMARY KEY,
  kind grantee_kind NOT NULL,
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
ALTER TABLE bots ADD CONSTRAINT bots_kind CHECK (kind = 'bot') NOT VALID;
CREATE TABLE groups (
  id UUID PRIMARY KEY,
  kind grantee_kind NOT NULL CHECK (kind IN ('group', 'team')),
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
CREATE TABLE guests (
  id UUID PRIMARY KEY,
  kind grantee_kind NOT NULL CHECK (kind = 'guest' OR kind = 'bot'),
  FOREIGN KEY (id, kind) REFERENCES grantees (id, kind)
);
",
    );
    assert!(is_extension(&database, "users", &["id", "kind"]));
    assert_eq!(ancestors(&database, "users"), ["grantees"]);
    assert_eq!(
        constants(&database, "users", &["id", "kind"]),
        [("kind".to_owned(), "'user'".to_owned())]
    );
    assert!(is_extension(&database, "organizations", &["id", "kind"]));
    assert_eq!(
        constants(&database, "organizations", &["id", "kind"]),
        [("kind".to_owned(), "'organization'::grantee_kind".to_owned())]
    );
    assert!(!is_extension(&database, "teams", &["id", "kind"]), "a NULL kind skips the key");
    assert!(!is_extension(&database, "bots", &["id", "kind"]), "old rows may hold any kind");
    assert!(!is_extension(&database, "groups", &["id", "kind"]), "two kinds are allowed");
    assert!(!is_extension(&database, "guests", &["id", "kind"]), "either kind is allowed");
}

const ARCHIVED_RESOURCES: &str = "
CREATE TYPE resource_kind AS ENUM ('project', 'ownable');
CREATE TABLE resources (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL,
  kind resource_kind NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  UNIQUE (organization_id, id, kind, archived)
);
CREATE TABLE asset_models (
  id UUID PRIMARY KEY REFERENCES resources (id) ON DELETE CASCADE,
  organization_id UUID NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  kind resource_kind NOT NULL DEFAULT 'ownable' CHECK (kind = 'ownable'),
  FOREIGN KEY (organization_id, id, kind, archived)
    REFERENCES resources (organization_id, id, kind, archived) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE TABLE loose_models (
  id UUID PRIMARY KEY REFERENCES resources (id),
  organization_id UUID,
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  kind resource_kind NOT NULL DEFAULT 'ownable' CHECK (kind = 'ownable'),
  FOREIGN KEY (organization_id, id, kind, archived)
    REFERENCES resources (organization_id, id, kind, archived)
);
CREATE TABLE late_models (
  id UUID PRIMARY KEY REFERENCES resources (id),
  organization_id UUID NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  kind resource_kind NOT NULL DEFAULT 'ownable' CHECK (kind = 'ownable')
);
ALTER TABLE late_models ADD CONSTRAINT late_models_archive
  FOREIGN KEY (organization_id, id, kind, archived)
  REFERENCES resources (organization_id, id, kind, archived) NOT VALID;
CREATE TABLE linked_models (
  id UUID PRIMARY KEY REFERENCES resources (id),
  organization_id UUID NOT NULL,
  archived BOOLEAN NOT NULL DEFAULT FALSE,
  linked_id UUID NOT NULL,
  kind resource_kind NOT NULL DEFAULT 'ownable' CHECK (kind = 'ownable'),
  FOREIGN KEY (organization_id, linked_id, kind, archived)
    REFERENCES resources (organization_id, id, kind, archived)
);
";

/// Another key that pairs the host primary key with the same referenced
/// columns reaches the same row of the supertype, so the constants it pins
/// there are the supertype's discriminators too.
#[test]
fn a_sibling_key_hands_over_the_constants_it_pins_into_the_same_row() {
    let database = parse(ARCHIVED_RESOURCES);
    assert!(is_extension(&database, "asset_models", &["id"]));
    assert!(!is_extension(
        &database,
        "asset_models",
        &["organization_id", "id", "kind", "archived"]
    ));
    assert_eq!(ancestors(&database, "asset_models"), ["resources"]);
    assert_eq!(
        constants(&database, "asset_models", &["id"]),
        [("kind".to_owned(), "'ownable'".to_owned())]
    );
}

/// A sibling key hands over nothing when the database may skip it on a row,
/// through a `NULL` host column or rows older than the key, or when it
/// reaches another row.
#[test]
fn a_sibling_key_the_database_may_skip_hands_over_nothing() {
    let database = parse(ARCHIVED_RESOURCES);
    assert_eq!(constants(&database, "loose_models", &["id"]), []);
    assert_eq!(constants(&database, "late_models", &["id"]), []);
    assert_eq!(constants(&database, "linked_models", &["id"]), []);
}
