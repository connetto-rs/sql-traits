# SQL Traits AI Coding Instructions

You are working on `sql-traits`, a Rust library that defines generic traits for SQL database schema introspection and provides an implementation using `sqlparser-rs`.

## Project Architecture

The library is designed around a set of traits that abstract over database schema definitions, allowing generic analysis of SQL structures regardless of the underlying representation (AST or live connection).

- **Core Traits** (`src/traits/`): Define the `DatabaseLike`, `TableLike`, `ColumnLike`, etc. interfaces. This is the primary abstraction layer.
- **Generic Database** (`src/structs/generic_db.rs`): `GenericDB` is the central struct that holds the schema state. It is generic over the types implementing the core traits.
- **Implementations** (`src/impls/`): Contains adapters for external types. The primary implementation is for `sqlparser-rs` AST nodes.
- **Metadata Wrappers** (`src/structs/metadata/`): Helper structs like `TableAttribute` that wrap AST nodes to attach parent context (like a reference to the table) which is necessary for trait implementations.

### Key Types
- **`ParserDB`**: A type alias for `GenericDB` specialized for `sqlparser` AST nodes (`CreateTable`, etc.). Defined in `src/structs/generic_db/sqlparser.rs`.
- **`TableAttribute<T, A>`**: A wrapper used to implement column/constraint traits on AST nodes while maintaining a reference to the parent table `T`.

## Coding Conventions

### Trait-Based Design
- **Implementing Features**: When adding functionality, prefer adding default methods to the relevant `*Like` trait in `src/traits/` rather than concrete implementations, unless it requires specific state.
- **Type Aliases**: Use `ParserDB` instead of the verbose `GenericDB<...>` when working with SQL parsing.

### SQL Parser Integration
- To parse SQL DDL into a model use `ParserDB::try_from(sql_string)`.
- Use `src/impls/sqlparser/` to extend how `sqlparser` AST nodes map to schema traits.

## Workflows & Testing

### Testing Strategy
- **Doc Tests**: This project relies heavily on documentation tests in `src/main.rs` (via README) and module-level docs to verify behavior.
- **Unit Tests**: Run `cargo test` to execute both unit and doc tests.
- **Integration Tests**: All of them build into the single `it` test binary (`autotests = false`). Add a file under `tests/it/` and declare it as a module in `tests/it/main.rs`, since an undeclared file is never compiled. Shared helpers live in `tests/support/`, reached as `crate::support`. Run one file's tests with `cargo test --test it column_scope::`.

### Build & Linting
- Comparison with strict lints: The project enables `pedantic` clippy lints and forbids missing docs. Ensure all new public items are documented.
- **Dependencies**: Note the git dependency on `geometric-traits`.

## Example: Traversing Schema

```rust
use sql_traits::prelude::*;

// 1. Create a DB model from SQL
let db = ParserDB::try_from("CREATE TABLE users (id INT PRIMARY KEY);")?;

// 2. Access schema via Traits, stating the comparison the engine uses
let table = db
    .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?
    .unwrap();
let pk = table.primary_key_columns().next().unwrap();

// 3. Trait methods provide intelligence
assert!(pk.is_primary_key());
```
