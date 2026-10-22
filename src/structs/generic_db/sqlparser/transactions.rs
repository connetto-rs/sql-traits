//! Transaction control met during ingestion, performed the way the engine
//! behind each dialect performs it.
//!
//! A transaction keeps a copy of the catalog at every point a `ROLLBACK` can
//! return to. The copies share every stored node and duplicate only the
//! metadata recorded beside it.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{Ident, Statement, TransactionModifier};

use super::{
    AccessResolution, CreatedCollationMetadata, ParserDB, ParserDBBuilder, ParserIngestion,
    PostgresCatalog, parsed_source::StatementSource,
};
use crate::{errors::Error, impls::SqlparserDialect, structs::IdentifierCase};

/// The schema state statements change and a rollback restores.
#[derive(Clone)]
pub(super) struct Catalog {
    pub(super) builder: ParserDBBuilder,
    /// The PostgreSQL catalog collations and collatable types resolve
    /// against.
    pub(super) postgres: Arc<PostgresCatalog>,
    /// Collations the ingested statements created.
    pub(super) collations: Vec<CreatedCollationMetadata>,
}

impl Catalog {
    pub(super) fn from_database(database: ParserDB) -> (Self, AccessResolution) {
        let (builder, ingestion) = ParserDBBuilder::from_database(database);
        let ParserIngestion { access_resolution, active_postgres_catalog, collation_metadata } =
            Arc::try_unwrap(ingestion).unwrap_or_else(|shared| (*shared).clone());
        let catalog =
            Self { builder, postgres: active_postgres_catalog, collations: collation_metadata };
        (catalog, access_resolution)
    }

    pub(super) fn into_database(self, access_resolution: AccessResolution) -> ParserDB {
        self.builder.into_database(Arc::new(ParserIngestion {
            access_resolution,
            active_postgres_catalog: self.postgres,
            collation_metadata: self.collations,
        }))
    }

    pub(super) fn snapshot(&self, access_resolution: AccessResolution) -> ParserDB {
        self.builder.snapshot_with(Arc::new(ParserIngestion {
            access_resolution,
            active_postgres_catalog: Arc::clone(&self.postgres),
            collation_metadata: self.collations.clone(),
        }))
    }

    fn apply(
        self,
        statement: Statement,
        access_resolution: AccessResolution,
        source: Option<StatementSource<'_>>,
    ) -> Result<Self, Error> {
        let (builder, postgres, collations) = ParserDB::apply_statements(
            self.builder,
            self.postgres,
            self.collations,
            access_resolution,
            &mut core::iter::once(statement),
            source,
        )?;
        Ok(Self { builder, postgres, collations })
    }

    /// Returns `saved` as the catalog a rollback leaves, still drawing
    /// identities after every one this catalog handed out.
    fn rolled_back_to(self, saved: Self) -> Self {
        Self { builder: self.builder.rolled_back_to(saved.builder), ..saved }
    }
}

/// An open transaction.
pub(super) struct Transaction {
    /// The catalog when the transaction opened, absent where the engine
    /// commits every catalog change, so a rollback has nothing to restore.
    start: Option<Catalog>,
    savepoints: Vec<Savepoint>,
    /// `BEGIN` statements nested inside the first, each awaiting its own
    /// `COMMIT` before the transaction can end.
    nested: usize,
    /// The transaction was opened by `SAVEPOINT`, so releasing that first
    /// savepoint commits it.
    opened_by_savepoint: bool,
    /// A schema definition ran, so only the end of the transaction may
    /// follow.
    defined: bool,
}

struct Savepoint {
    name: Ident,
    saved: Option<Catalog>,
}

/// Applies one statement, transaction control included, to the catalog.
pub(super) fn apply(
    catalog: Catalog,
    transaction: &mut Option<Transaction>,
    access_resolution: AccessResolution,
    statement: Statement,
    source: Option<StatementSource<'_>>,
) -> Result<Catalog, Error> {
    let rules = Rules::of(*catalog.builder.dialect());
    let control = Control::of(&statement);
    if transaction.as_ref().is_some_and(|open| open.defined)
        && !matches!(control, Some(Control::Commit { .. } | Control::Rollback { .. }))
    {
        return Err(Error::StatementAfterDefinition { statement: statement.to_string() });
    }
    if let Some(control) = control {
        let Some(rules) = rules else { return Err(unsupported(&statement)) };
        return rules.control(control, &statement, catalog, transaction);
    }
    if let (Some(rules), Some(open)) = (rules, transaction.as_mut())
        && defines_schema(&statement)
    {
        match rules.definitions {
            Definitions::Transactional => {}
            Definitions::Commit { temporary_exempt } => {
                if !(temporary_exempt && is_temporary_table(&statement)) {
                    *transaction = None;
                }
            }
            Definitions::Refused { temporary_allowed } => {
                if !(temporary_allowed && is_temporary_table(&statement)) {
                    return Err(Error::DefinitionInTransaction {
                        statement: statement.to_string(),
                    });
                }
            }
            Definitions::Last => open.defined = true,
        }
    }
    catalog.apply(statement, access_resolution, source)
}

/// The catalog an ingestion ending now leaves behind. A session that ends
/// inside a transaction rolls it back.
pub(super) fn finish(catalog: Catalog, transaction: Option<Transaction>) -> Catalog {
    match transaction.and_then(|open| open.start) {
        Some(start) => catalog.rolled_back_to(start),
        None => catalog,
    }
}

#[derive(Clone, Copy)]
enum Control<'a> {
    Begin,
    Commit { chain: bool, end: bool },
    Rollback { chain: bool },
    Savepoint(&'a Ident),
    Release(&'a Ident),
    RollbackTo(&'a Ident),
}

impl<'a> Control<'a> {
    fn of(statement: &'a Statement) -> Option<Self> {
        let block = |modifier: &Option<TransactionModifier>| {
            matches!(modifier, Some(TransactionModifier::Try | TransactionModifier::Catch))
        };
        match statement {
            Statement::StartTransaction {
                statements,
                exception: None,
                has_end_keyword: false,
                modifier,
                ..
            } if statements.is_empty() && !block(modifier) => Some(Self::Begin),
            Statement::Commit { chain, end, modifier } if !block(modifier) => {
                Some(Self::Commit { chain: *chain, end: *end })
            }
            Statement::Rollback { chain, savepoint: None } => {
                Some(Self::Rollback { chain: *chain })
            }
            Statement::Rollback { savepoint: Some(name), .. } => Some(Self::RollbackTo(name)),
            Statement::Savepoint { name } => Some(Self::Savepoint(name)),
            Statement::ReleaseSavepoint { name } => Some(Self::Release(name)),
            _ => None,
        }
    }
}

/// How an engine performs transaction control.
#[derive(Clone, Copy)]
struct Rules {
    begin: Begin,
    definitions: Definitions,
    commit_outside: Outside,
    rollback_outside: Outside,
    /// `AND CHAIN`, when the engine has it, and what it does with no
    /// transaction open.
    chain: Option<Outside>,
    /// Whether `COMMIT` ends a transaction, where `END` always does.
    commit_keyword: bool,
    savepoints: Option<Savepoints>,
}

/// What `BEGIN` does.
#[derive(Clone, Copy)]
enum Begin {
    /// The engine opens transactions implicitly and has no `BEGIN`.
    Unsupported,
    /// Inside a transaction it does nothing.
    Ignored,
    /// Inside a transaction it is an error.
    Refused,
    /// Inside a transaction it commits that one and opens another.
    CommitsOpen,
    /// Inside a transaction it nests, and only the outermost `COMMIT` ends
    /// the transaction.
    Counted,
}

/// What an open transaction makes of a statement defining a schema object.
#[derive(Clone, Copy)]
enum Definitions {
    /// A rollback undoes it like any other change.
    Transactional,
    /// It commits the open transaction first, so no rollback undoes it.
    /// Where temporary tables are exempt, creating or dropping one commits
    /// nothing and is still never undone.
    Commit { temporary_exempt: bool },
    /// It is an error, except for temporary tables where allowed.
    Refused { temporary_allowed: bool },
    /// It must be the last statement before the transaction ends.
    Last,
}

/// What a statement needing an open transaction does without one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outside {
    Ignored,
    Refused,
}

#[derive(Clone, Copy)]
struct Savepoints {
    outside: SavepointOutside,
    /// Whether a repeated name erases the earlier savepoint, rather than
    /// shadowing it until the later one is released.
    replaces: bool,
    /// Whether `ROLLBACK TO` keeps the savepoint it returns to.
    rollback_keeps: bool,
    /// Whether the engine has `RELEASE SAVEPOINT`.
    release: bool,
    /// How savepoint names compare.
    case: IdentifierCase,
}

/// What `SAVEPOINT` does with no transaction open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SavepointOutside {
    Refused,
    /// The statement succeeds, but the savepoint ends with the implicit
    /// transaction around the statement itself.
    Discarded,
    /// It opens a transaction.
    OpensTransaction,
}

const POSTGRES: Rules = Rules {
    begin: Begin::Ignored,
    definitions: Definitions::Transactional,
    commit_outside: Outside::Ignored,
    rollback_outside: Outside::Ignored,
    chain: Some(Outside::Refused),
    commit_keyword: true,
    savepoints: Some(Savepoints {
        outside: SavepointOutside::Refused,
        replaces: false,
        rollback_keeps: true,
        release: true,
        case: IdentifierCase::AsWritten,
    }),
};

const REDSHIFT: Rules = Rules { chain: None, savepoints: None, ..POSTGRES };

const DUCKDB: Rules = Rules {
    begin: Begin::Refused,
    commit_outside: Outside::Refused,
    rollback_outside: Outside::Refused,
    chain: None,
    savepoints: None,
    ..POSTGRES
};

const SQLITE: Rules = Rules {
    savepoints: Some(Savepoints {
        outside: SavepointOutside::OpensTransaction,
        replaces: false,
        rollback_keeps: true,
        release: true,
        case: IdentifierCase::Folded,
    }),
    ..DUCKDB
};

const MSSQL: Rules = Rules {
    begin: Begin::Counted,
    savepoints: Some(Savepoints {
        outside: SavepointOutside::Refused,
        replaces: false,
        rollback_keeps: false,
        release: false,
        case: IdentifierCase::Folded,
    }),
    ..DUCKDB
};

const MYSQL: Rules = Rules {
    begin: Begin::CommitsOpen,
    definitions: Definitions::Commit { temporary_exempt: true },
    chain: Some(Outside::Ignored),
    savepoints: Some(Savepoints {
        outside: SavepointOutside::Discarded,
        replaces: true,
        rollback_keeps: true,
        release: true,
        case: IdentifierCase::Folded,
    }),
    ..POSTGRES
};

const ORACLE: Rules = Rules {
    begin: Begin::Unsupported,
    definitions: Definitions::Commit { temporary_exempt: false },
    chain: None,
    savepoints: Some(Savepoints {
        outside: SavepointOutside::OpensTransaction,
        replaces: true,
        rollback_keeps: true,
        release: false,
        case: IdentifierCase::AsWritten,
    }),
    ..POSTGRES
};

const SNOWFLAKE: Rules = Rules {
    definitions: Definitions::Commit { temporary_exempt: false },
    chain: None,
    savepoints: None,
    ..POSTGRES
};

const BIGQUERY: Rules =
    Rules { definitions: Definitions::Refused { temporary_allowed: true }, ..DUCKDB };

const DATABRICKS: Rules = Rules {
    definitions: Definitions::Refused { temporary_allowed: false },
    rollback_outside: Outside::Ignored,
    ..DUCKDB
};

const TERADATA: Rules = Rules {
    begin: Begin::Counted,
    definitions: Definitions::Last,
    commit_outside: Outside::Refused,
    chain: None,
    commit_keyword: false,
    savepoints: None,
    ..POSTGRES
};

impl Rules {
    /// The rules of the engine behind a dialect, absent for engines without
    /// transaction control. Dialects naming no single engine follow
    /// PostgreSQL.
    const fn of(dialect: SqlparserDialect) -> Option<Self> {
        match dialect {
            SqlparserDialect::Ansi | SqlparserDialect::Generic | SqlparserDialect::PostgreSql => {
                Some(POSTGRES)
            }
            SqlparserDialect::Redshift => Some(REDSHIFT),
            SqlparserDialect::DuckDb => Some(DUCKDB),
            SqlparserDialect::SQLite => Some(SQLITE),
            SqlparserDialect::MsSql => Some(MSSQL),
            SqlparserDialect::MySql => Some(MYSQL),
            SqlparserDialect::Oracle => Some(ORACLE),
            SqlparserDialect::Snowflake => Some(SNOWFLAKE),
            SqlparserDialect::BigQuery => Some(BIGQUERY),
            SqlparserDialect::Databricks => Some(DATABRICKS),
            SqlparserDialect::Teradata => Some(TERADATA),
            SqlparserDialect::ClickHouse | SqlparserDialect::Hive | SqlparserDialect::Spark => None,
        }
    }

    /// Whether a rollback can undo a catalog change, which decides whether
    /// a transaction keeps copies of the catalog.
    const fn restores(self) -> bool {
        !matches!(self.definitions, Definitions::Commit { .. })
    }

    fn open(self, catalog: &Catalog, opened_by_savepoint: bool) -> Transaction {
        Transaction {
            start: self.restores().then(|| catalog.clone()),
            savepoints: Vec::new(),
            nested: 0,
            opened_by_savepoint,
            defined: false,
        }
    }

    fn control(
        self,
        control: Control<'_>,
        statement: &Statement,
        mut catalog: Catalog,
        transaction: &mut Option<Transaction>,
    ) -> Result<Catalog, Error> {
        match control {
            Control::Begin => {
                match (self.begin, transaction.as_mut()) {
                    (Begin::Unsupported, _) => return Err(unsupported(statement)),
                    (_, None) => *transaction = Some(self.open(&catalog, false)),
                    (Begin::Ignored, Some(_)) => {}
                    (Begin::Refused, Some(_)) => {
                        return Err(Error::TransactionAlreadyOpen {
                            statement: statement.to_string(),
                        });
                    }
                    (Begin::CommitsOpen, Some(_)) => {
                        *transaction = Some(self.open(&catalog, false));
                    }
                    (Begin::Counted, Some(open)) => open.nested += 1,
                }
            }
            Control::Commit { chain, end } => {
                if !(end || self.commit_keyword) {
                    return Err(unsupported(statement));
                }
                let outside = self.completion(chain, self.commit_outside, statement)?;
                match transaction.as_mut() {
                    None => return refuse_outside(outside, statement).map(|()| catalog),
                    Some(open) if open.nested > 0 => open.nested -= 1,
                    Some(_) => {
                        *transaction = chain.then(|| self.open(&catalog, false));
                    }
                }
            }
            Control::Rollback { chain } => {
                let outside = self.completion(chain, self.rollback_outside, statement)?;
                let Some(open) = transaction.take() else {
                    return refuse_outside(outside, statement).map(|()| catalog);
                };
                if let Some(start) = open.start {
                    catalog = catalog.rolled_back_to(start);
                }
                if chain {
                    *transaction = Some(self.open(&catalog, false));
                }
            }
            Control::Savepoint(name) => {
                let Some(savepoints) = self.savepoints else { return Err(unsupported(statement)) };
                let open = match (transaction.as_mut(), savepoints.outside) {
                    (Some(open), _) => open,
                    (None, SavepointOutside::Refused) => {
                        return Err(Error::NoOpenTransaction { statement: statement.to_string() });
                    }
                    (None, SavepointOutside::Discarded) => return Ok(catalog),
                    (None, SavepointOutside::OpensTransaction) => {
                        transaction.insert(self.open(&catalog, true))
                    }
                };
                if savepoints.replaces
                    && let Some(position) = savepoints.position(&open.savepoints, name)
                {
                    open.savepoints.remove(position);
                }
                open.savepoints.push(Savepoint {
                    name: name.clone(),
                    saved: self.restores().then(|| catalog.clone()),
                });
            }
            Control::Release(name) => {
                let Some(savepoints) = self.savepoints.filter(|savepoints| savepoints.release)
                else {
                    return Err(unsupported(statement));
                };
                let (open, position) = savepoints.find(transaction.as_mut(), name, statement)?;
                open.savepoints.truncate(position);
                if position == 0 && open.opened_by_savepoint {
                    *transaction = None;
                }
            }
            Control::RollbackTo(name) => {
                let Some(savepoints) = self.savepoints else { return Err(unsupported(statement)) };
                let (open, position) = savepoints.find(transaction.as_mut(), name, statement)?;
                open.savepoints.truncate(position + 1);
                let returned = if savepoints.rollback_keeps {
                    open.savepoints[position].saved.clone()
                } else {
                    open.savepoints.pop().and_then(|savepoint| savepoint.saved)
                };
                if let Some(saved) = returned {
                    catalog = catalog.rolled_back_to(saved);
                }
            }
        }
        Ok(catalog)
    }

    /// Checks `AND CHAIN` is available, and returns what the completion does
    /// with no transaction open.
    fn completion(
        self,
        chain: bool,
        outside: Outside,
        statement: &Statement,
    ) -> Result<Outside, Error> {
        if chain {
            return self.chain.ok_or_else(|| unsupported(statement));
        }
        Ok(outside)
    }
}

impl Savepoints {
    fn position(self, savepoints: &[Savepoint], name: &Ident) -> Option<usize> {
        savepoints.iter().rposition(|savepoint| {
            self.case.identifiers_match(
                &savepoint.name.value,
                savepoint.name.quote_style.is_some(),
                &name.value,
                name.quote_style.is_some(),
            )
        })
    }

    /// Finds the most recent savepoint carrying `name` in the open
    /// transaction.
    fn find<'t>(
        self,
        transaction: Option<&'t mut Transaction>,
        name: &Ident,
        statement: &Statement,
    ) -> Result<(&'t mut Transaction, usize), Error> {
        let Some(open) = transaction else {
            return Err(if self.outside == SavepointOutside::Refused {
                Error::NoOpenTransaction { statement: statement.to_string() }
            } else {
                Error::SavepointNotFound { savepoint_name: name.value.clone() }
            });
        };
        match self.position(&open.savepoints, name) {
            Some(position) => Ok((open, position)),
            None => Err(Error::SavepointNotFound { savepoint_name: name.value.clone() }),
        }
    }
}

fn refuse_outside(outside: Outside, statement: &Statement) -> Result<(), Error> {
    match outside {
        Outside::Ignored => Ok(()),
        Outside::Refused => Err(Error::NoOpenTransaction { statement: statement.to_string() }),
    }
}

fn unsupported(statement: &Statement) -> Error {
    Error::UnsupportedTransactionControl { statement: statement.to_string() }
}

/// Whether a statement defines a schema object or a privilege, the class of
/// statement engines without transactional definitions commit or refuse.
fn defines_schema(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::CreateView(_)
            | Statement::CreateTable(_)
            | Statement::CreateVirtualTable { .. }
            | Statement::CreateIndex(_)
            | Statement::CreateRole(_)
            | Statement::CreateSecret { .. }
            | Statement::CreateServer(_)
            | Statement::CreateForeignTable(_)
            | Statement::CreatePolicy(_)
            | Statement::CreateConnector(_)
            | Statement::CreateOperator(_)
            | Statement::CreateOperatorFamily(_)
            | Statement::CreateOperatorClass(_)
            | Statement::CreateTextSearch(_)
            | Statement::AlterTable(_)
            | Statement::AlterSchema(_)
            | Statement::AlterIndex { .. }
            | Statement::AlterView { .. }
            | Statement::AlterFunction(_)
            | Statement::AlterType(_)
            | Statement::AlterCollation(_)
            | Statement::AlterOperator(_)
            | Statement::AlterOperatorFamily(_)
            | Statement::AlterOperatorClass(_)
            | Statement::AlterTextSearch(_)
            | Statement::AlterRole { .. }
            | Statement::AlterPolicy(_)
            | Statement::AlterConnector { .. }
            | Statement::Drop { .. }
            | Statement::DropFunction(_)
            | Statement::DropDomain(_)
            | Statement::DropProcedure { .. }
            | Statement::DropSecret { .. }
            | Statement::DropPolicy(_)
            | Statement::DropConnector { .. }
            | Statement::CreateExtension(_)
            | Statement::CreateCollation(_)
            | Statement::DropExtension(_)
            | Statement::DropOperator(_)
            | Statement::DropOperatorFamily(_)
            | Statement::DropOperatorClass(_)
            | Statement::Comment { .. }
            | Statement::CreateSchema { .. }
            | Statement::CreateDatabase { .. }
            | Statement::CreateFunction(_)
            | Statement::CreateTrigger(_)
            | Statement::DropTrigger(_)
            | Statement::CreateProcedure { .. }
            | Statement::CreateMacro { .. }
            | Statement::CreateStage { .. }
            | Statement::CreateFileFormat { .. }
            | Statement::CreateWarehouse { .. }
            | Statement::Grant(_)
            | Statement::Deny(_)
            | Statement::Revoke(_)
            | Statement::CreateSequence { .. }
            | Statement::CreateDomain(_)
            | Statement::CreateType { .. }
            | Statement::RenameTable(_)
            | Statement::CreateUser(_)
            | Statement::AlterUser(_)
    )
}

fn is_temporary_table(statement: &Statement) -> bool {
    match statement {
        Statement::CreateTable(create_table) => create_table.temporary,
        Statement::Drop { temporary, .. } => *temporary,
        _ => false,
    }
}
