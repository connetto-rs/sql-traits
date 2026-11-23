//! Ingesting and analysing long operator and set-operation chains, which
//! sqlparser parses into trees as deep as the chain is long.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "Fixtures must parse into the statements they spell and their lookups must succeed."
)]

use core::{fmt::Write, time::Duration};

use sql_traits::prelude::*;
use sqlparser::{
    ast::{DataType, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

const STACK_SIZE: usize = 2 << 20;

/// How long ingesting one chain may take, far above what typing each node of
/// a chain once costs and far below what typing the subtree of every node
/// again costs.
const DEADLINE: Duration = Duration::from_secs(30);

/// Joins `first` and `length` more items with `separator`.
fn chain(length: usize, first: &str, separator: &str, item: &str) -> String {
    let mut text = String::from(first);
    for _ in 0..length {
        text.push_str(separator);
        text.push_str(item);
    }
    text
}

fn sum(length: usize) -> String {
    chain(length, "a", " + ", "1")
}

/// Runs `work` on a thread whose stack is [`STACK_SIZE`].
fn on_small_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|threads| {
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(threads, work)
            .unwrap()
            .join()
            .unwrap()
    })
}

/// Ingests `sql` on a thread whose stack is [`STACK_SIZE`], within
/// [`DEADLINE`].
fn ingests(sql: &str) {
    let sql = sql.to_owned();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || sender.send(ParserDB::parse::<PostgreSqlDialect>(&sql).map(drop)))
        .unwrap();
    match receiver.recv_timeout(DEADLINE) {
        Ok(ingested) => ingested.unwrap(),
        Err(error) => panic!("ingesting did not finish: {error}"),
    }
}

#[test]
fn generated_column_summing_1000_terms_ingests() {
    ingests(&format!("CREATE TABLE t (a int, b int GENERATED ALWAYS AS ({}) STORED);", sum(1000)));
}

#[test]
fn column_check_summing_1000_terms_ingests() {
    ingests(&format!("CREATE TABLE t (a int CHECK ({} > 0));", sum(1000)));
}

#[test]
fn column_check_of_1000_alternatives_ingests() {
    ingests(&format!("CREATE TABLE t (a int CHECK ({}));", chain(1000, "a = 0", " OR ", "a = 1")));
}

#[test]
fn table_check_summing_1000_terms_ingests() {
    ingests(&format!("CREATE TABLE t (a int, CHECK ({} > 0), PRIMARY KEY (a));", sum(1000)));
}

#[test]
fn default_concatenating_1000_terms_ingests() {
    ingests(&format!("CREATE TABLE t (a text DEFAULT {});", chain(1000, "'x'", " || ", "'x'")));
}

#[test]
fn domain_check_summing_1000_terms_ingests() {
    ingests(&format!("CREATE DOMAIN d AS int CHECK ({} > 0);", chain(1000, "VALUE", " + ", "1")));
}

#[test]
fn view_projecting_a_sum_of_1000_terms_ingests() {
    ingests(&format!("CREATE TABLE t (a int); CREATE VIEW v AS SELECT {} AS b FROM t;", sum(1000)));
}

#[test]
fn view_filtering_on_1000_alternatives_ingests() {
    ingests(&format!(
        "CREATE TABLE t (a int); CREATE VIEW v AS SELECT a FROM t WHERE {};",
        chain(1000, "a = 0", " OR ", "a = 1")
    ));
}

#[test]
fn view_of_2000_union_arms_ingests() {
    ingests(&format!(
        "CREATE TABLE t (a int); CREATE VIEW v AS {};",
        chain(2000, "SELECT a FROM t", " UNION ALL ", "TABLE t")
    ));
}

#[test]
fn query_of_4000_union_arms_scopes() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a int);").unwrap();
    let sql = chain(4000, "SELECT a FROM t", " UNION ALL ", "SELECT a FROM t");
    let Statement::Query(query) =
        Parser::new(&PostgreSqlDialect {}).try_with_sql(&sql).unwrap().parse_statement().unwrap()
    else {
        panic!("expected a query")
    };
    on_small_stack(|| {
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).unwrap();
    });
}

#[test]
fn helper_chain_160_deep_reads_its_helper_set() {
    let mut sql = String::from(
        "CREATE ROLE authz_owner NOLOGIN BYPASSRLS;
         CREATE TABLE docs (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL);
         GRANT SELECT ON docs TO authz_owner;
         CREATE FUNCTION ids_0() RETURNS SETOF TEXT
         LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
           SELECT id FROM docs WHERE owner_id = current_setting('app.user_id', true)
         $$;
         ALTER FUNCTION ids_0() OWNER TO authz_owner;",
    );
    for level in 1..=160 {
        let below = level - 1;
        write!(
            sql,
            "CREATE FUNCTION ids_{level}() RETURNS SETOF TEXT
             LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
               SELECT x.id FROM public.ids_{below}() AS x(id)
             $$;
             ALTER FUNCTION ids_{level}() OWNER TO authz_owner;"
        )
        .unwrap();
    }
    let db = ParserDB::parse::<PostgreSqlDialect>(&sql).unwrap();
    let caller =
        SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text)).unwrap();
    let deployment = Deployment::new("app", ["app"], [caller]).unwrap();
    let top = db
        .function_by_target(TargetName::new("ids_160", false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap();
    on_small_stack(|| {
        top.helper_set(&[], &deployment, &db, &[]).unwrap();
    });
}
