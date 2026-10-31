//! Parser changes this crate is waiting on.
//!
//! The parser moves slowly, so a gap here can outlive several releases of this
//! crate. Each entry names the pull request, what this crate cannot answer
//! until it lands, and what to remove here once it does. Grepping for
//! `sqlparser#` finds the sites that carry a marker.
//!
//! Filed and open:
//!
//! - [`sqlparser#2430`](https://github.com/apache/datafusion-sqlparser-rs/pull/2430),
//!   `ALTER TRIGGER ... RENAME TO` does not parse, so a rename never frees the
//!   old name and this crate refuses a schema PostgreSQL accepts.
//! - [`sqlparser#2316`](https://github.com/apache/datafusion-sqlparser-rs/pull/2316),
//!   opened by somebody else, adds `CREATE AGGREGATE`. Until it lands no
//!   aggregate can exist here, which is why
//!   [`Error::AggregateOwnerUnsupported`](crate::errors::Error::AggregateOwnerUnsupported)
//!   refuses `ALTER AGGREGATE ... OWNER TO` rather than attributing the owner
//!   to a function of the same name.
//! - [`sqlparser#2417`](https://github.com/apache/datafusion-sqlparser-rs/pull/2417),
//!   `ALTER DEFAULT PRIVILEGES` does not parse, so a schema carrying one fails
//!   to parse, and the `EXECUTE` a function receives on creation is always
//!   PostgreSQL's built-in default rather than the one such a statement set.
//! - [`sqlparser#2776`](https://github.com/apache/datafusion-sqlparser-rs/pull/2776),
//!   `ALTER MATERIALIZED VIEW` does not parse, so a schema carrying one fails
//!   to parse, and only `ALTER TABLE <materialized view> OWNER TO` and `RENAME
//!   TO` reach a materialized view.
//!
//! Found and written up, not yet filed:
//!
//! - `ALTER TABLE ... INHERIT` and `NO INHERIT` do not parse, so a schema
//!   history that attaches or detaches a parent leaves the inheritance edges as
//!   the `CREATE TABLE` wrote them.
//! - The `CREATE TABLE ... LIKE` option list (`INCLUDING INDEXES` and its
//!   siblings) does not parse beyond the spellings already honoured here.
//! - `ALTER PROCEDURE` and the PostgreSQL body spelling of `CREATE PROCEDURE`
//!   do not parse. This costs nothing yet, since no procedure can exist here
//!   for a statement to alter.
//! - `CREATE RECURSIVE VIEW`, `REFRESH MATERIALIZED VIEW` and `CREATE
//!   MATERIALIZED VIEW ... WITH NO DATA` do not parse, and the parser accepts
//!   `CREATE VIEW IF NOT EXISTS` and `CREATE OR REPLACE MATERIALIZED VIEW`,
//!   both of which PostgreSQL refuses, so this crate refuses them itself.
