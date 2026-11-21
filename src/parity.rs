//! rls-parity's recorded row-level security cases, for replaying them against
//! one translated engine.
//!
//! Each case under `parity-fixtures/` holds a schema, a seed, the callers,
//! tables and writes it measures, PostgreSQL 18's answer for every caller, row
//! and statement, and the discrepancies known per engine. A translator's CI
//! runs every case on its engine and hands the answers to
//! [`Fixture::judge`], which reports what the fixture does not account for.
//!
//! ```
//! use sql_traits::parity::{Engine, EngineAnswer, fixtures};
//!
//! for fixture in fixtures() {
//!     let fixture = fixture?;
//!     // An engine that answers every probe as PostgreSQL does.
//!     let answers: Vec<EngineAnswer> = fixture
//!         .recorded
//!         .answer
//!         .iter()
//!         .map(|recorded| {
//!             EngineAnswer {
//!                 caller: recorded.caller.clone(),
//!                 row: recorded.row.clone(),
//!                 statement: recorded.statement.clone(),
//!                 answer: recorded.postgres.clone(),
//!             }
//!         })
//!         .collect();
//!     let discrepancies = fixture.judge(Engine::Sqlite, None, &answers);
//!     assert_eq!(
//!         discrepancies.is_empty(),
//!         fixture.known.sqlite.is_empty(),
//!         "{}",
//!         fixture.case.name
//!     );
//! }
//! # Ok::<(), sql_traits::parity::FixtureError>(())
//! ```

mod answer;
mod case;
mod fixture;

pub use answer::{Answer, Direction, Engine, direction};
pub use case::{
    Caller, Case, CaseError, ColumnCommand, Config, MissingKeyColumn, Setting, Shape, Table, Write,
    identifier, text_of,
};
pub use fixture::{
    CaseKnown, Discrepancy, DiscrepancyKind, EngineAnswer, EngineKnown, Fixture, FixtureError,
    KnownRow, Recorded, RecordedAnswer, fixtures,
};
