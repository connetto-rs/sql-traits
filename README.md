# SQL Traits

[![CI](https://github.com/connetto-rs/sql-traits/workflows/Rust%20CI/badge.svg)](https://github.com/connetto-rs/sql-traits/actions)
[![MSRV](https://img.shields.io/badge/MSRV-1.88.0-blue.svg)](https://github.com/connetto-rs/sql-traits/blob/main/Cargo.toml)
[![Security Audit](https://github.com/connetto-rs/sql-traits/workflows/Security%20Audit/badge.svg)](https://github.com/connetto-rs/sql-traits/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Codecov](https://codecov.io/gh/connetto-rs/sql-traits/branch/main/graph/badge.svg)](https://codecov.io/gh/connetto-rs/sql-traits)

Rust traits describing SQL-like objects.

This library provides a set of traits to abstract over SQL database schemas, allowing generic code to operate on database definitions regardless of the underlying representation (e.g., parsed SQL ASTs or active database connection metadata).

## Features

- **Generic Schema Abstraction**: Traits such as `DatabaseLike`, `TableLike`, `ColumnLike`, `ForeignKeyLike`, and `UniqueIndexLike` define a common interface for schema introspection.
- **SQL Parser Integration**: The `ParserDB` struct implements these traits using `sqlparser-rs`, enabling the construction of an introspectable database model directly from SQL DDL strings.
- **Metadata Support**: Comprehensive support for table attributes, indices, and constraints.

## Usage

This crate is currently not published to `crates.io`. You can add it to your project via git:

```toml
[dependencies]
sql-traits = { git = "https://github.com/connetto-rs/sql-traits" }
```

## Example: Parsing SQL DDL

The following example demonstrates how to parse SQL `CREATE TABLE` statements and inspect the resulting schema model using `ParserDB`.

```rust
use sql_traits::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse SQL DDL into a database model
    let db = ParserDB::parse::<GenericDialect>("
        CREATE TABLE users (
            id INT PRIMARY KEY,
            username TEXT NOT NULL
        );
        ")?;

    // Retrieve a table by the name a statement writes for it, compared the way
    // the engine compares it
    let users_table = db
        .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?
        .expect("Table not found");

    assert_eq!(users_table.table_name(), "users");

    // Inspect columns
    let columns: Vec<_> = users_table.columns(&db)?.collect();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].column_name(), "id");
    assert_eq!(columns[1].column_name(), "username");

    Ok(())
}
```

## Implementations

- **[`sqlparser`](https://github.com/apache/datafusion-sqlparser-rs) AST**: Included in this crate (`ParserDB`), useful for static analysis and schema validation of SQL files.
- **[`pg_diesel`](https://github.com/connetto-rs/pg_diesel)**: An external, downstream crate that implements these traits for inspecting an existing PostgreSQL database.

## Status

This crate is not yet published to `crates.io` because we still need to finalize the upstream crate [`geometric-traits`](https://github.com/earth-metabolome-initiative/geometric-traits).
