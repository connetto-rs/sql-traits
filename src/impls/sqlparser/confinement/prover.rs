//! Proves that definer reads stay within their callers' row policies.

use alloc::{
    borrow::{Cow, ToOwned},
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{
    cell::{Cell, RefCell},
    convert::Infallible,
    iter, mem,
    ops::ControlFlow,
    ptr, slice,
};

use sqlparser::{
    ast::{
        ArgMode, BinaryOperator, CascadeOption, CopySource, DoUpdate, Expr, FromTable, Function,
        FunctionArg, FunctionArgExpr, FunctionArguments, FunctionBehavior, FunctionCalledOnNull,
        FunctionSecurity, Ident, InTarget, Insert, JoinConstraint, JoinOperator, MergeAction,
        ObjectName, ObjectNamePart, OnConflictAction, OnInsert, Query, ReferentialAction, Select,
        SelectItem, SetExpr, SetOperator, SetQuantifier, Statement, TableAlias,
        TableAliasColumnDef, TableFactor, TableInheritance, TableObject, TableWithJoins,
        TriggerEvent, Update, UpdateTableFromKind, Value, Visit, Visitor, With,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use super::{
    identity::{
        Identity, fixed_by, held_values, identity, pin, same_identity, stored_name, type_identity,
    },
    implication::{
        Columns, Condition, ParameterValues, Refused, Term, admits, clause_implies, membership,
        parameter, parameter_ordinal, renamed,
    },
    keys::{binds, equality_keys, key_pairs, no_inheritors},
    ladders::{Ladders, called, listed_arguments, plain, positional_arguments, relation_arguments},
    nulls::rejects_null,
    record::{Record, Row},
    relations::{same_name, stored_column},
    sites::Kinds,
    terms::{
        conjuncts, is_role_read, literal, sets_only_search_path, unnested, unreadable_body,
        unwrapped, user_type_named,
    },
};
use crate::{
    errors::{LookupError, RowSecurityError},
    impls::sqlparser::{
        body_calls::reaches_on_path,
        callee_path::calls_path_changer,
        data_statement::{BodyPath, BodyWalk, RunTimeBody, resolve_on_body_path, walk_body},
        dql::{join_constraint, scoped_roots::ScopedRoot},
        function_body::{
            SqlBody, hex_strings_may_be_integers, run_time_path, sql_body, sql_query_body,
        },
    },
    structs::{
        ActingRole, ColumnBinding, ColumnDefinition, DefinerReading, DefinitionSource, Deployment,
        EmbeddedSql, ExpressionContext, ExpressionScope, IdentifierCase, OperandType, PlPgSqlScope,
        PlPgSqlVariable, Reaching, RowStatement, Unconfinement, VariableKind, VariableRead,
        WrittenCall,
    },
    traits::{
        ColumnLike, DatabaseLike, ForeignKeyLike, FunctionLike, PolicyLike, Relation, TableLike,
        TriggerLike, UserDefinedTypeLike, foreign_key::checks_rows_holding,
    },
    utils::{
        call_sites::for_each_call,
        identifier_resolution::normalize_identifier,
        object_name::resolve_bound_relation_object_name,
        walk::{walk, walk_expressions, walk_expressions_mut},
        with_scopes::WithScopes,
    },
};

/// Decides whether a function shows its callers only rows of tables with
/// row-level security that their own `SELECT` policies show them.
pub(super) struct Confinement<'a, DB: DatabaseLike> {
    database: &'a DB,
    deployment: &'a Deployment,
    /// The roles the function runs for.
    callers: Vec<ActingRole<'a, DB::Role>>,
    /// What the proof asks of the reads of a trigger's function chain, or
    /// `None` for a function a caller executes, every read of which counts.
    reading: Option<DefinerReading>,
    /// The functions whose proof is under way, the first one proved first,
    /// so a cycle proves nothing.
    proving: Vec<&'a DB::Function>,
    /// The functions proved.
    proved: Vec<*const DB::Function>,
    /// The rows each function read so far draws the values it returns from,
    /// by the function's address.
    arms: RefCell<Vec<ReadArms<'a, DB>>>,
    /// The functions whose rows are being read, so a cycle reads none.
    drawing: RefCell<Vec<*const DB::Function>>,
    ladders: Ladders<'a, DB>,
    /// The function the callers execute.
    exposed: &'a DB::Function,
    /// Whether every value `exposed` returns lies in a set a `SELECT` policy
    /// of the table it is drawn from tests, once read.
    released: Cell<Option<bool>>,
    /// The last read found unconfined.
    refusal: RefCell<Option<Refusal<'a, DB>>>,
}

/// A read found unconfined.
pub(super) struct Refusal<'a, DB: DatabaseLike> {
    /// The functions the read is made through, the one proved first first.
    pub(super) through: Vec<&'a DB::Function>,
    /// The table read, when the read is of one table.
    pub(super) table: Option<&'a DB::Table>,
    /// The statement or expression making the read, as written.
    pub(super) statement: Option<String>,
    pub(super) reason: Unconfinement<'a, DB>,
}

impl<'a, DB: DatabaseLike> Confinement<'a, DB> {
    /// The proof for the callers of `exposed`, asking `reading` of the reads
    /// of a trigger's function chain or, for `None`, judging every read.
    pub(super) fn new(
        database: &'a DB,
        deployment: &'a Deployment,
        callers: Vec<ActingRole<'a, DB::Role>>,
        exposed: &'a DB::Function,
        reading: Option<DefinerReading>,
    ) -> Self {
        Self {
            database,
            deployment,
            callers,
            reading,
            proving: Vec::new(),
            proved: Vec::new(),
            arms: RefCell::default(),
            drawing: RefCell::default(),
            ladders: Ladders::new(database),
            exposed,
            released: Cell::new(None),
            refusal: RefCell::new(None),
        }
    }

    /// Records that a read of `table` in the function under proof is
    /// unconfined for `reason`, answering `false` so a check can return it.
    fn refuse(&self, table: Option<&'a DB::Table>, reason: Unconfinement<'a, DB>) -> bool {
        *self.refusal.borrow_mut() =
            Some(Refusal { through: self.proving.clone(), table, statement: None, reason });
        false
    }

    /// Records a read of the function under proof that the proof cannot
    /// follow, answering `false`.
    fn unfollowed(&self) -> bool {
        self.refuse(None, Unconfinement::Unfollowed)
    }

    /// Records `statement` as the one making the read last refused, unless
    /// a statement nested in it was recorded.
    fn refused_in(&self, statement: &dyn core::fmt::Display) {
        if let Some(refusal) = self.refusal.borrow_mut().as_mut()
            && refusal.statement.is_none()
        {
            refusal.statement = Some(statement.to_string());
        }
    }

    /// The last read found unconfined, clearing the record.
    #[expect(clippy::expect_used, reason = "every refused proof records its reason")]
    pub(super) fn take_refusal(&self) -> Refusal<'a, DB> {
        self.refusal.take().expect("an unconfined proof records its refusal")
    }

    /// Whether every row of a table with row-level security that `function`
    /// reads, itself or through the functions it calls, is one each caller's
    /// `SELECT` policies admit.
    pub(super) fn confines(&mut self, function: &'a DB::Function) -> Result<bool, LookupError> {
        let address = ptr::from_ref(function);
        if self.proved.contains(&address) {
            return Ok(true);
        }
        if self.proving.iter().any(|proving| ptr::eq(*proving, function)) {
            return Ok(self.unfollowed());
        }
        self.proving.push(function);
        let confined = self.confines_body(function, None);
        self.proving.pop();
        if matches!(confined, Ok(true)) {
            self.proved.push(address);
        }
        confined
    }

    /// Whether the chain of `function`, the function of a row trigger whose
    /// rows `record` describes, confines the reads it makes as the owner of
    /// a `SECURITY DEFINER` function. A function running as the acting role
    /// reads only what the role reads, so only the `SECURITY DEFINER`
    /// functions it reaches through others running as the role count.
    pub(super) fn confines_trigger(
        &mut self,
        function: &'a DB::Function,
        record: Option<&Record<'a, DB>>,
    ) -> Result<bool, LookupError> {
        if self.proving.iter().any(|proving| ptr::eq(*proving, function)) {
            return Ok(true);
        }
        self.proving.push(function);
        let confined = if function.security_mode() == FunctionSecurity::Definer {
            let firing = record.map(|record| Firing { record, passed: &[] });
            self.confines_body(function, firing.as_ref())
        } else {
            self.confines_invoker(function)
        };
        self.proving.pop();
        confined
    }

    /// Whether each `SECURITY DEFINER` function `function`, which runs as
    /// the acting role, reaches confines its reads.
    fn confines_invoker(&mut self, function: &'a DB::Function) -> Result<bool, LookupError> {
        let callees = match function.referenced_functions(self.database) {
            Ok(callees) => callees,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        for callee in callees {
            if callee.target_name().schema() == Some("pg_catalog")
                || (self.reading != Some(DefinerReading::Outcome)
                    && self.runs_for_every_caller(callee)?)
            {
                continue;
            }
            if !self.confines_trigger(callee, None)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the functions `function` calls confine their reads, and its
    /// own body reads no table with row-level security or confines each
    /// read, knowing of the trigger's rows what `firing` holds. A function
    /// it calls that confines its reads knowing nothing of them may confine
    /// them knowing what each call passes it, as [`Self::confines_passing`]
    /// reads the calls.
    fn confines_body(
        &mut self,
        function: &'a DB::Function,
        firing: Option<&Firing<'_, 'a, DB>>,
    ) -> Result<bool, LookupError> {
        if !self.confines_body_writes(function)? {
            return Ok(false);
        }
        let (tables, callees) = match function
            .referenced_tables(self.database)
            .and_then(|tables| Ok((tables, function.referenced_functions(self.database)?)))
        {
            Ok(found) => found,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        for callee in callees {
            if callee.target_name().schema() == Some("pg_catalog")
                || (self.reading != Some(DefinerReading::Outcome)
                    && self.runs_for_every_caller(callee)?)
                || self.confines(callee)?
            {
                continue;
            }
            if let Some(firing) = firing
                && self.confines_passing(function, callee, firing)?
            {
                self.refusal.take();
                continue;
            }
            let refused = self.refusal.take();
            if self.releases_through_policy_sets(callee)? {
                self.refusal.take();
                continue;
            }
            *self.refusal.borrow_mut() = refused;
            return Ok(false);
        }
        let mut protected = false;
        for table in tables {
            protected |= table.has_row_level_security(self.database)?;
        }
        if !protected {
            return Ok(true);
        }
        if !sets_only_search_path(function) {
            return Ok(self.unfollowed());
        }
        if function.stored_language().as_deref() == Some("plpgsql") {
            return self.confines_sites(function, firing);
        }
        let confined = self.read_body(
            function,
            |query| Lineage::of(function, query),
            |reads, query| {
                let confined = reads.confines_query(query, &[], &[])?;
                if !confined {
                    self.refused_in(query);
                }
                Ok(confined)
            },
        )?;
        match confined {
            Some(confined) => Ok(confined),
            None => self.confines_sql_body(function, true),
        }
    }

    /// Proves the trigger chains every write in the body can fire.
    fn confines_body_writes(&mut self, function: &'a DB::Function) -> Result<bool, LookupError> {
        if function.stored_language().as_deref() != Some("plpgsql") {
            return self.confines_sql_body(function, false);
        }
        let body = match function.plpgsql_sites(self.database) {
            Ok(body) => body,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        let sites = match body.sites() {
            Ok(sites) => sites,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        for site in sites {
            if !self.confines_writes(&site.sql(), site.plpgsql_scope().path(), false)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Proves writes or reads of an SQL body under each statement's path.
    fn confines_sql_body(
        &mut self,
        function: &'a DB::Function,
        reads: bool,
    ) -> Result<bool, LookupError> {
        let body = match sql_body(function, self.database) {
            Ok(body) => body,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        let path = run_time_path(function, self.database)?;
        let bound = matches!(
            &body,
            SqlBody::Atomic(_) | SqlBody::Return(_) | SqlBody::Expression(_) | SqlBody::Select(_)
        );
        match &body {
            SqlBody::Statements(statements) | SqlBody::Atomic(statements) => {
                let hex_strings = hex_strings_may_be_integers(self.database);
                let mut proof =
                    SqlProof { confinement: self, function, path, bound, reads, confined: true };
                let result = walk_body(
                    &function.stored_name(),
                    RunTimeBody::Sql(statements),
                    &mut proof,
                    hex_strings,
                );
                match result {
                    Ok(()) => Ok(proof.confined),
                    Err(error) if unreadable_body(&error) => Ok(proof.confinement.unfollowed()),
                    Err(error) => Err(error),
                }
            }
            SqlBody::Expression(expression) if !reads => {
                self.confines_writes(*expression, &path, bound)
            }
            SqlBody::Return(expression) if !reads => {
                self.confines_writes(expression.as_ref(), &path, bound)
            }
            SqlBody::Select(select) if !reads => self.confines_writes(*select, &path, bound),
            _ => Ok(self.unfollowed()),
        }
    }

    /// Proves one SQL statement's reads without trusting mismatched relation
    /// bindings.
    fn confines_sql_site(
        &self,
        function: &'a DB::Function,
        statement: &Statement,
        path: &BodyPath<'_>,
    ) -> Result<bool, LookupError> {
        let scope = ExpressionScope::FunctionBody(function);
        let root = ScopedRoot::Statement(statement);
        let Ok(context) =
            ExpressionContext::for_root(root, &scope, self.database, IdentifierCase::AsWritten)
        else {
            return Ok(self.unfollowed());
        };
        let parameters = self.parameter_values(function)?;
        let lineage = Lineage::default();
        let mut reads = Reads {
            body: Body { parameters: &parameters, confinement: self, function },
            context: &context,
            path,
            items: WithScopes::default(),
            lineage: &lineage,
            known: None,
        };
        let confined = match statement {
            Statement::Query(query) => reads.confines_query(query, &[], &[])?,
            statement => reads.confines_write(statement, false)?,
        };
        if !confined {
            self.refused_in(statement);
        }
        Ok(confined)
    }

    /// Proves the triggers fired by writes anywhere in an SQL syntax tree.
    fn confines_writes<N: Visit>(
        &mut self,
        node: &N,
        path: &BodyPath<'_>,
        bound: bool,
    ) -> Result<bool, LookupError> {
        /// Dispatches each statement to its write proof.
        struct Writes<Each> {
            each: Each,
        }
        /// Stops the walk at the first refused statement or lookup error.
        impl<Each: FnMut(&Statement) -> Result<bool, LookupError>> Visitor for Writes<Each> {
            type Break = Option<LookupError>;

            fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
                match (self.each)(statement) {
                    Ok(true) => ControlFlow::Continue(()),
                    Ok(false) => ControlFlow::Break(None),
                    Err(error) => ControlFlow::Break(Some(error)),
                }
            }
        }
        let result = walk(
            node,
            &mut Writes {
                each: |statement: &Statement| {
                    self.confines_written_statement(statement, path, bound)
                },
            },
        );
        match result {
            ControlFlow::Continue(()) => Ok(true),
            ControlFlow::Break(None) => Ok(false),
            ControlFlow::Break(Some(error)) => Err(error),
        }
    }

    /// Proves the trigger events a statement can issue on its target.
    fn confines_written_statement(
        &mut self,
        statement: &Statement,
        path: &BodyPath<'_>,
        bound: bool,
    ) -> Result<bool, LookupError> {
        let mut events = [false; 3];
        let name = match statement {
            Statement::Insert(insert) => {
                events[0] = true;
                events[1] = match &insert.on {
                    Some(OnInsert::OnConflict(conflict)) => {
                        matches!(conflict.action, OnConflictAction::DoUpdate(_))
                    }
                    Some(_) => true,
                    None => false,
                };
                let TableObject::TableName(name) = &insert.table else {
                    return Ok(self.unfollowed());
                };
                Some(name)
            }
            Statement::Copy { to: false, source: CopySource::Table { table_name, .. }, .. } => {
                events[0] = true;
                Some(table_name)
            }
            Statement::Update(update) => {
                events[1] = true;
                written_table_name(&update.table.relation)
            }
            Statement::Delete(delete) => {
                events[2] = true;
                let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) =
                    &delete.from;
                if from.len() != 1 || !delete.tables.is_empty() {
                    return Ok(self.unfollowed());
                }
                written_table_name(&from[0].relation)
            }
            Statement::Merge(merge) => {
                for clause in &merge.clauses {
                    match clause.action {
                        MergeAction::Insert(_) => events[0] = true,
                        MergeAction::Update(_) => events[1] = true,
                        MergeAction::Delete { .. } => events[2] = true,
                        MergeAction::DoNothing { .. } => {}
                    }
                }
                written_table_name(&merge.table)
            }
            Statement::Truncate(truncate) => {
                let event = WriteEvent::Truncate {
                    cascade: truncate.cascade == Some(CascadeOption::Cascade),
                };
                let mut visited = Vec::new();
                for target in &truncate.table_names {
                    let Ok(Some(table)) = written_table(self.database, &target.name, path, bound)
                    else {
                        return Ok(self.unfollowed());
                    };
                    if !self.confines_written_table(table, event, &mut visited)? {
                        self.refused_in(statement);
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            Statement::Copy { to: true, source: CopySource::Table { table_name, .. }, .. } => {
                let Ok(Some(table)) = written_table(self.database, table_name, path, bound) else {
                    return Ok(self.unfollowed());
                };
                return Ok(if table.has_row_level_security(self.database)? {
                    self.refuse(Some(table), Unconfinement::Unfollowed)
                } else {
                    true
                });
            }
            Statement::Query(_)
            | Statement::Copy { to: true, source: CopySource::Query(_), .. } => return Ok(true),
            _ => return Ok(self.unfollowed()),
        };
        let Some(name) = name else {
            return Ok(self.unfollowed());
        };
        let Ok(Some(table)) = written_table(self.database, name, path, bound) else {
            return Ok(self.unfollowed());
        };
        if table.rules(self.database)?.next().is_some() {
            return Ok(self.refuse(Some(table), Unconfinement::Unfollowed));
        }
        let mut visited = Vec::new();
        for (fires, event) in
            events.into_iter().zip([WriteEvent::Insert, WriteEvent::Update, WriteEvent::Delete])
        {
            if fires && !self.confines_written_table(table, event, &mut visited)? {
                self.refused_in(statement);
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Proves a table's trigger functions and the trigger functions reached by
    /// referential actions.
    fn confines_written_table(
        &mut self,
        table: &'a DB::Table,
        event: WriteEvent,
        visited: &mut Vec<(&'a DB::Table, WriteEvent)>,
    ) -> Result<bool, LookupError> {
        if visited.iter().any(|(seen, previous)| ptr::eq(*seen, table) && *previous == event) {
            return Ok(true);
        }
        visited.push((table, event));
        if table.rules(self.database)?.next().is_some() || !no_inheritors(self.database, table)? {
            return Ok(self.refuse(Some(table), Unconfinement::Unfollowed));
        }
        for trigger in table.triggers(self.database)? {
            if !trigger.events().iter().any(|trigger_event| {
                matches!(
                    (event, trigger_event),
                    (WriteEvent::Insert, TriggerEvent::Insert)
                        | (WriteEvent::Update, TriggerEvent::Update(_))
                        | (WriteEvent::Delete, TriggerEvent::Delete)
                        | (WriteEvent::Truncate { .. }, TriggerEvent::Truncate)
                )
            }) {
                continue;
            }
            let Some(function) = trigger.function(self.database) else {
                return Ok(self.refuse(Some(table), Unconfinement::Unfollowed));
            };
            if !self.confines(function)? {
                return Ok(false);
            }
        }
        let parent = table.partition_root(self.database)?;
        if let Some(parent) = parent
            && !self.confines_written_table(parent, event, visited)?
        {
            return Ok(false);
        }
        if table.is_partitioned() {
            for partition in table.partitions(self.database)? {
                if !self.confines_written_table(partition, event, visited)? {
                    return Ok(false);
                }
            }
        }
        if event == WriteEvent::Update && (parent.is_some() || table.is_partitioned()) {
            for movement in [WriteEvent::Delete, WriteEvent::Insert] {
                if !self.confines_written_table(table, movement, visited)? {
                    return Ok(false);
                }
            }
        }
        if matches!(event, WriteEvent::Insert | WriteEvent::Truncate { cascade: false }) {
            return Ok(true);
        }
        for host in self.database.tables() {
            for foreign_key in host.foreign_keys(self.database)? {
                if !ptr::eq(foreign_key.referenced_table(self.database)?, table) {
                    continue;
                }
                let next = if event == (WriteEvent::Truncate { cascade: true }) {
                    event
                } else {
                    let action = if event == WriteEvent::Update {
                        foreign_key.on_update(self.database)
                    } else {
                        foreign_key.on_delete(self.database)
                    };
                    match action {
                        ReferentialAction::Cascade => event,
                        ReferentialAction::SetNull | ReferentialAction::SetDefault => {
                            WriteEvent::Update
                        }
                        _ => continue,
                    }
                };
                if !self.confines_written_table(host, next, visited)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Whether each statement and expression of the PL/pgSQL body of
    /// `function` the proof asks of confines its reads, knowing of the
    /// trigger's rows what `firing` holds where the body reads them, or a
    /// parameter passed one of their fields, unchanged. Under
    /// [`DefinerReading::Outcome`] only the reads deciding an outcome count.
    fn confines_sites(
        &self,
        function: &'a DB::Function,
        firing: Option<&Firing<'_, 'a, DB>>,
    ) -> Result<bool, LookupError> {
        let case = IdentifierCase::AsWritten;
        let body = match function.plpgsql_sites(self.database) {
            Ok(body) => body,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        let Some(kinds) = body.block().map(Kinds::of) else {
            return Ok(self.unfollowed());
        };
        let sites = match body.sites() {
            Ok(sites) => sites,
            Err(error) if unreadable_body(&error) => return Ok(self.unfollowed()),
            Err(error) => return Err(error),
        };
        let mut found = false;
        let mut unchanged = Vec::with_capacity(sites.len());
        for site in &sites {
            let reads = site.reads(case)?;
            found |= reads.iter().any(|read| *read.variable().kind() == VariableKind::Found);
            unchanged.push(unchanged_variables(&reads));
        }
        let parameters = self.parameter_values(function)?;
        let lineage = Lineage::default();
        for (site, unchanged) in sites.iter().zip(unchanged) {
            let sql = site.sql();
            if self.reading == Some(DefinerReading::Outcome) && !kinds.decides(sql, found) {
                continue;
            }
            let context = site.context(case)?;
            let known = Known {
                record: firing.map(|firing| firing.record),
                passed: firing.map_or(&[], |firing| firing.passed),
                unchanged,
                function,
                scope: site.plpgsql_scope(),
            };
            let mut reads = Reads {
                body: Body { parameters: &parameters, confinement: self, function },
                context: &context,
                path: site.plpgsql_scope().path(),
                items: WithScopes::default(),
                lineage: &lineage,
                known: Some(&known),
            };
            let confined = match sql {
                EmbeddedSql::Expression(query) => reads.confines_query(query, &[], &[])?,
                EmbeddedSql::Statement(Statement::Query(query)) => {
                    reads.confines_query(query, &[], &[])?
                }
                EmbeddedSql::Statement(statement) => {
                    reads.confines_write(statement, found || kinds.diagnostics)?
                }
            };
            if !confined {
                match sql {
                    EmbeddedSql::Expression(query) => self.refused_in(query),
                    EmbeddedSql::Statement(statement) => self.refused_in(statement),
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// [`Self::confines`] for `callee` knowing of the trigger's rows what
    /// `firing` holds, proved anew for each firing.
    fn confines_firing(
        &mut self,
        callee: &'a DB::Function,
        firing: &Firing<'_, 'a, DB>,
    ) -> Result<bool, LookupError> {
        if self.proving.iter().any(|proving| ptr::eq(*proving, callee)) {
            return Ok(self.unfollowed());
        }
        self.proving.push(callee);
        let confined = self.confines_body(callee, Some(firing));
        self.proving.pop();
        confined
    }

    /// Whether `callee` confines its reads at each call of it the PL/pgSQL
    /// body of `function` makes, knowing of the trigger's rows what `firing`
    /// holds and of each parameter of `callee` the field of the rows the
    /// call passes it. A field the body reads unchanged, passed by position
    /// to a parameter of a type `=` compares alike to the field's column, is
    /// the value the parameter holds where `callee` reads it unchanged.
    fn confines_passing(
        &mut self,
        function: &'a DB::Function,
        callee: &'a DB::Function,
        firing: &Firing<'_, 'a, DB>,
    ) -> Result<bool, LookupError> {
        let database = self.database;
        let case = IdentifierCase::AsWritten;
        let body = function.plpgsql_sites(database)?;
        let sites = body.sites()?;
        let inputs: Vec<Option<Identity>> = callee
            .argument_modes()
            .zip(callee.argument_types(database))
            .filter(|(mode, _)| !matches!(mode, ArgMode::Out))
            .map(|(_, declared)| type_identity(declared, database))
            .collect();
        let mut passings: Vec<Vec<Option<(Row, String)>>> = Vec::new();
        for site in &sites {
            let sql = site.sql();
            let path = site.plpgsql_scope().path();
            let reached = for_each_call(&sql, |call| {
                if reaches_on_path(database, callee, call, path) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            if reached.is_continue() {
                continue;
            }
            let context = site.context(case)?;
            let reads = site.reads(case)?;
            let known = Known {
                record: Some(firing.record),
                passed: firing.passed,
                unchanged: unchanged_variables(&reads),
                function,
                scope: site.plpgsql_scope(),
            };
            let ControlFlow::Continue(()) = for_each_call::<_, Infallible>(&sql, |call| {
                if !reaches_on_path(database, callee, call, path) {
                    return ControlFlow::Continue(());
                }
                let arguments = call
                    .node
                    .and_then(|written| {
                        match written.factor() {
                            Some(factor) => relation_arguments(factor, callee),
                            None => {
                                written
                                    .function()
                                    .and_then(|expression| positional_arguments(expression, callee))
                            }
                        }
                    })
                    .unwrap_or_default();
                let passed: Vec<_> = arguments
                    .into_iter()
                    .zip(&inputs)
                    .map(|(argument, input)| {
                        self.passed_field(&context, &known, argument, input.as_ref())
                    })
                    .collect();
                if !passings.contains(&passed) {
                    passings.push(passed);
                }
                ControlFlow::Continue(())
            });
        }
        if passings.is_empty() {
            return Ok(false);
        }
        for passed in &passings {
            if !self.confines_firing(callee, &Firing { record: firing.record, passed })? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The field of the trigger's rows that `argument`, read in `context`
    /// knowing what `known` holds, passes a parameter of a type `=` compares
    /// by `input`, when the field's column compares alike, and none when a
    /// lookup fails.
    fn passed_field(
        &self,
        context: &ExpressionContext<'_, 'a, DB>,
        known: &Known<'_, 'a, DB>,
        argument: &Expr,
        input: Option<&Identity>,
    ) -> Option<(Row, String)> {
        let record = known.record?;
        let (definition, _) = context.reference(unnested(argument)).ok()??;
        let (row, name) = known.field(&definition, argument)?;
        let column = column_named(self.database, record.table, &name).ok()??;
        let held = identity(column, self.database).ok()??;
        (input == Some(&held)).then(|| (row, name.into_owned()))
    }

    /// Whether `callee`, a `SECURITY DEFINER` function that neither writes
    /// nor calls a function that may, reaches the callers only through the
    /// values the exposed function returns, each of which a `SELECT` policy
    /// of the table it is drawn from admits through the exposed function's
    /// own set. The callers then learn which of the rows they read it
    /// returns, and nothing of a row they cannot read.
    fn releases_through_policy_sets(&self, callee: &'a DB::Function) -> Result<bool, LookupError> {
        if callee.security_mode() != FunctionSecurity::Definer
            || !self.reads_only(callee, &mut Vec::new())?
        {
            return Ok(false);
        }
        if let Some(released) = self.released.get() {
            return Ok(released);
        }
        let released = self.releases_policy_sets()?;
        self.released.set(Some(released));
        Ok(released)
    }

    /// Whether neither `function` nor a function it reaches is `VOLATILE`,
    /// so none writes, with `seen` holding the functions already read.
    fn reads_only(
        &self,
        function: &'a DB::Function,
        seen: &mut Vec<*const DB::Function>,
    ) -> Result<bool, LookupError> {
        let address = ptr::from_ref(function);
        if seen.contains(&address) {
            return Ok(true);
        }
        seen.push(address);
        if function.volatility() == FunctionBehavior::Volatile {
            return Ok(false);
        }
        if function.target_name().schema() == Some("pg_catalog") {
            return Ok(true);
        }
        let callees = match function.referenced_functions(self.database) {
            Ok(callees) => callees,
            Err(error) if unreadable_body(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        for callee in callees {
            if !self.reads_only(callee, seen)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether every value the exposed function returns is the column of a
    /// row each caller's `SELECT` policies admit by testing that column
    /// against the function's own set.
    fn releases_policy_sets(&self) -> Result<bool, LookupError> {
        let function = self.exposed;
        let Some(member) = member(function) else {
            return Ok(false);
        };
        let arms = self.arms(function)?;
        if !arms.complete || arms.arms.is_empty() {
            return Ok(false);
        }
        let parameters = self.parameter_values(function)?;
        let body = Body { parameters: &parameters, confinement: self, function };
        for arm in &arms.arms {
            let tested = in_set(&arm.drawn.column.stored_column_name(), member.clone());
            if !body.admits_every_caller(arm.drawn.table, &tested)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// What `walk` finds reading the body of `function`, one `LANGUAGE sql`
    /// query, with the lineage `lineage` gives it, or `None` when the body is
    /// no such query or its names do not resolve.
    fn read_body<T>(
        &self,
        function: &'a DB::Function,
        lineage: impl FnOnce(&Query) -> Lineage,
        walk: impl FnOnce(&mut Reads<'_, '_, 'a, DB>, &Query) -> Result<T, LookupError>,
    ) -> Result<Option<T>, LookupError> {
        let Some(query) = sql_query_body(self.database, function)? else {
            return Ok(None);
        };
        let statement = Statement::Query(Box::new(query.into_owned()));
        let scope = ExpressionScope::FunctionBody(function);
        let (Statement::Query(query), Ok(context)) = (
            &statement,
            ExpressionContext::for_statement(
                &statement,
                &scope,
                self.database,
                IdentifierCase::AsWritten,
            ),
        ) else {
            return Ok(None);
        };
        let path = run_time_path(function, self.database)?;
        let lineage = lineage(query);
        let parameters = self.parameter_values(function)?;
        let mut reads = Reads {
            body: Body { parameters: &parameters, confinement: self, function },
            context: &context,
            path: &path,
            items: WithScopes::default(),
            lineage: &lineage,
            known: None,
        };
        walk(&mut reads, query).map(Some)
    }

    /// The values each input parameter of `function` may hold, in the order
    /// `$n` counts them. A parameter of an enum type holds its labels, and
    /// `NULL` unless the function is `STRICT`, and one of another type `None`.
    fn parameter_values(
        &self,
        function: &'a DB::Function,
    ) -> Result<Vec<Option<ParameterValues>>, LookupError> {
        let null = !matches!(
            function.null_input_behavior(),
            FunctionCalledOnNull::Strict | FunctionCalledOnNull::ReturnsNullOnNullInput
        );
        let scope = ExpressionScope::FunctionBody(function);
        let inputs = function.argument_modes().filter(|mode| !matches!(mode, ArgMode::Out)).count();
        let mut values = Vec::with_capacity(inputs);
        for ordinal in 0..inputs {
            let placeholder = parameter(ordinal);
            let context = ExpressionContext::for_expression(
                &placeholder,
                &scope,
                self.database,
                IdentifierCase::AsWritten,
            )?;
            let OperandType::Known { type_name, .. } = context.type_of(&placeholder)? else {
                values.push(None);
                continue;
            };
            values.push(
                user_type_named(self.database, &type_name)
                    .and_then(UserDefinedTypeLike::enumeration)
                    .map(|enumeration| {
                        ParameterValues {
                            labels: enumeration
                                .into_members()
                                .map(|member| {
                                    Expr::Value(
                                        Value::SingleQuotedString(member.label().to_owned()).into(),
                                    )
                                })
                                .collect(),
                            null,
                        }
                    }),
            );
        }
        Ok(values)
    }

    /// Whether `callee` runs as its owner and every caller may execute it
    /// directly, so calling it discloses nothing a direct call does not.
    fn runs_for_every_caller(&self, callee: &DB::Function) -> Result<bool, LookupError> {
        if callee.security_mode() != FunctionSecurity::Definer {
            return Ok(false);
        }
        for caller in &self.callers {
            let executes = match *caller {
                ActingRole::Declared(role) => callee.can_execute(role, self.database)?,
                ActingRole::Public => callee.is_executable_by_public(self.database)?,
            };
            if !executes {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `expression` evaluates alike for a caller and for the owner,
    /// reading no relation, no role, no privilege and no search path, and
    /// calling no function of the schema that runs as its caller.
    fn reads_alike(&self, expression: &Expr) -> Result<bool, LookupError> {
        match walk(expression, &mut CallerBound(self.database)) {
            ControlFlow::Continue(()) => Ok(true),
            ControlFlow::Break(None) => Ok(false),
            ControlFlow::Break(Some(error)) => Err(error),
        }
    }

    /// The rows `function` draws the values it returns from, read once.
    fn arms(&self, function: &'a DB::Function) -> Result<Rc<Arms<'a, DB>>, LookupError> {
        let address = ptr::from_ref(function);
        if let Some((_, arms)) = self.arms.borrow().iter().find(|(read, _)| *read == address) {
            return Ok(Rc::clone(arms));
        }
        if self.drawing.borrow().contains(&address) {
            return Ok(Rc::new(Arms::default()));
        }
        self.drawing.borrow_mut().push(address);
        let arms = self.read_arms(function);
        self.drawing.borrow_mut().pop();
        let arms = Rc::new(arms?);
        self.arms.borrow_mut().push((address, Rc::clone(&arms)));
        Ok(arms)
    }

    /// [`Self::arms`] read from the body of `function`. A `VOLATILE` function
    /// may read rows of a later snapshot, and a parameter it sets may change
    /// what its filters compute, so either draws from rows the rule cannot
    /// tell.
    fn read_arms(&self, function: &'a DB::Function) -> Result<Arms<'a, DB>, LookupError> {
        if function.volatility() == FunctionBehavior::Volatile
            || !function.returns_set()
            || !sets_only_search_path(function)
        {
            return Ok(Arms::default());
        }
        self.read_body(
            function,
            |_| Lineage::default(),
            |reads, query| {
                let mut carried = Carried::new();
                carried.query(query);
                let mut drawing = Drawing {
                    leaves: carried.leaves(),
                    arms: Arms { arms: Vec::new(), complete: carried.complete },
                    member: (function.security_mode() == FunctionSecurity::Definer)
                        .then(|| member(function))
                        .flatten(),
                };
                reads.draw_query(query, &mut drawing)?;
                drawing.arms.complete &= drawing.leaves.is_empty();
                Ok(drawing.arms)
            },
        )
        .map(Option::unwrap_or_default)
    }
}

/// The rows the values a set-returning function returns are drawn from.
struct Arms<'a, DB: DatabaseLike> {
    arms: Vec<Arm<'a, DB>>,
    /// Whether every value the function returns is the column of a row of
    /// `arms`.
    complete: bool,
}

impl<DB: DatabaseLike> Default for Arms<'_, DB> {
    fn default() -> Self {
        Self { arms: Vec::new(), complete: false }
    }
}

/// One way a function draws a value it returns from a row.
struct Arm<'a, DB: DatabaseLike> {
    drawn: Rc<Drawn<'a, DB>>,
    /// Whether the function returns the column of every row of the table
    /// meeting the conditions, those reading a parameter for the values the
    /// call passes.
    exhaustive: bool,
}

/// A row of a stored table that a function returns one column of.
struct Drawn<'a, DB: DatabaseLike> {
    table: &'a DB::Table,
    /// The column returned.
    column: &'a DB::Column,
    /// Conditions the row meets, over the stored names of the columns of
    /// `table`, each evaluating alike for every role.
    conditions: Vec<Expr>,
    /// Further such conditions reading the function's input parameters as
    /// `$n`, which the row meets for the values a call passes.
    bound: Vec<Expr>,
    /// `"column" IN (SELECT f($1, …))`, the column in the set of the
    /// function, a `SECURITY DEFINER` one, that returns it. The row meets it
    /// once drawn, though the function need not return the column of a row
    /// only for meeting it.
    member: Option<Expr>,
}

impl<DB: DatabaseLike> Drawn<'_, DB> {
    /// The conditions the row meets for a call passing `arguments` for the
    /// input parameters in order, or `None` when one reads a parameter and
    /// the call's values are not known.
    fn under(&self, arguments: Option<&[&Expr]>) -> Option<Cow<'_, [Expr]>> {
        if self.bound.is_empty() {
            return Some(Cow::Borrowed(&self.conditions));
        }
        let arguments = arguments?;
        let mut conditions = Vec::with_capacity(self.conditions.len() + self.bound.len());
        conditions.extend_from_slice(&self.conditions);
        for condition in &self.bound {
            conditions.push(bound_to(condition, arguments)?);
        }
        Some(Cow::Owned(conditions))
    }

    /// What the row is known to meet once drawn by a call passing
    /// `arguments`, [`Self::under`] those, or the conditions reading no
    /// parameter, and the membership.
    fn known(&self, arguments: Option<&[&Expr]>) -> Cow<'_, [Expr]> {
        let mut known = self.under(arguments).unwrap_or(Cow::Borrowed(&self.conditions));
        let member = self.member.as_ref().and_then(|member| {
            match arguments {
                Some(arguments) => bound_to(member, arguments),
                None => (!reads_parameter(member)).then(|| member.clone()),
            }
        });
        if let Some(member) = member {
            known.to_mut().push(member);
        }
        known
    }
}

/// The leaf `SELECT`s of a function body not read yet, and the rows read.
struct Drawing<'a, DB: DatabaseLike> {
    /// Each leaf not read yet. Every row of a leaf reaches the result.
    leaves: Vec<*const Select>,
    arms: Arms<'a, DB>,
    /// `SELECT f($1, …)` calling the function, a `SECURITY DEFINER` one, with
    /// its own arguments. A policy calling it with the same arguments gets
    /// the same set, which holds the column of each row drawn.
    member: Option<Box<Query>>,
}

impl<'a, DB: DatabaseLike> Drawing<'a, DB> {
    /// Adds the row of `table` whose `column` the function returns, meeting
    /// `conditions`, with its membership in the function's own set.
    fn push(
        &mut self,
        table: &'a DB::Table,
        column: &'a DB::Column,
        conditions: Vec<Expr>,
        exhaustive: bool,
    ) {
        let (bound, conditions) = conditions.into_iter().partition(reads_parameter);
        self.arms.arms.push(Arm {
            drawn: Rc::new(Drawn {
                table,
                column,
                conditions,
                bound,
                member: self
                    .member
                    .as_ref()
                    .map(|member| in_set(&column.stored_column_name(), member.clone())),
            }),
            exhaustive,
        });
    }
}

/// The proof of one function body, whose filters name its input parameters
/// as `$n`.
struct Body<'c, 'a, DB: DatabaseLike> {
    confinement: &'c Confinement<'a, DB>,
    /// The values each input parameter may hold, in the order `$n` counts
    /// them.
    parameters: &'c [Option<ParameterValues>],
    function: &'a DB::Function,
}

impl<'a, DB: DatabaseLike> Body<'_, 'a, DB> {
    /// Whether `filter`, stating conditions of the columns of `table` by
    /// their stored names, admits only rows every caller's `SELECT` policies
    /// admit, of which every caller may select every column. A refusal
    /// records what refused it.
    fn admits_every_caller(
        &self,
        table: &'a DB::Table,
        filter: &Expr,
    ) -> Result<bool, LookupError> {
        let database = self.confinement.database;
        for caller in &self.confinement.callers {
            let security = match table.row_security(RowStatement::Select, *caller, database) {
                Ok(security) => security,
                Err(RowSecurityError::Lookup(error)) => return Err(error),
                Err(_) => {
                    return Ok(self.confinement.refuse(Some(table), Unconfinement::Unfollowed));
                }
            };
            let refused = match security.gates() {
                Some(gates) => {
                    admits(database, table, gates, self, |condition| {
                        clause_implies(self, filter, condition, &[])
                    })?
                    .map(|refused| {
                        match refused {
                            Refused::Restrictive(policy) => Unconfinement::Restrictive(policy),
                            Refused::Permissive(policies) => Unconfinement::Permissive(policies),
                        }
                    })
                }
                None if security.exemption().is_some() => None,
                None => Some(Unconfinement::Unfollowed),
            };
            if let Some(refused) = refused {
                return Ok(self.confinement.refuse(Some(table), refused));
            }
            let held = table.privileges(*caller, database)?;
            for column in table.columns(database)? {
                if !held.select.on_column(column) {
                    return Ok(self
                        .confinement
                        .refuse(Some(table), Unconfinement::HiddenColumn(column)));
                }
            }
        }
        Ok(true)
    }
}

/// A policy condition states the columns of its table by their stored names,
/// as a confined filter does once renamed.
impl<DB: DatabaseLike> Columns for Body<'_, '_, DB> {
    fn is_set_within(&self, narrower: &Expr, wider: &Expr) -> Result<bool, LookupError> {
        let database = self.confinement.database;
        let ladders = &self.confinement.ladders;
        self.confinement.deployment.is_set_within(
            narrower,
            wider,
            |expression| {
                let Expr::Function(call) = unnested(expression) else {
                    return Ok(None);
                };
                let Some(helper) = called(database, call)? else {
                    return Ok(None);
                };
                if !positional_arguments(call, helper).is_some_and(|arguments| arguments.is_empty())
                    || !sets_only_search_path(helper)
                    || helper.volatility() == FunctionBehavior::Volatile
                {
                    return Ok(None);
                }
                let Some(query) = sql_query_body(database, helper)? else {
                    return Ok(None);
                };
                let Some(members) = super::implication::query_members(&query) else {
                    return Ok(None);
                };
                let Expr::Function(projected) = unnested(&members) else {
                    return Ok(None);
                };
                if projected.name.0.len() != 2 {
                    return Ok(None);
                }
                let Some(source) = called(database, projected)? else {
                    return Ok(None);
                };
                let return_type = helper.return_type_name(database);
                if source.security_mode() != FunctionSecurity::Definer
                    || source.volatility() == FunctionBehavior::Volatile
                    || !sets_only_search_path(source)
                    || !positional_arguments(projected, source).is_some_and(|arguments| {
                        arguments.iter().all(|argument| literal(argument).is_some())
                    })
                    || return_type.is_none()
                    || return_type != source.return_type_name(database)
                    || helper.returns_set() != source.returns_set()
                {
                    return Ok(None);
                }
                Ok(Some(members.into_owned()))
            },
            |narrower, wider| ladders.within(narrower, wider),
        )
    }

    fn tested(&self, column: &Expr) -> Option<String> {
        stored_name(column).map(Cow::into_owned)
    }

    fn tests(&self, column: &Expr, tested: &str) -> bool {
        stored_name(column).is_some_and(|name| name == tested)
    }

    fn parameter_values(&self, ordinal: usize) -> Option<&ParameterValues> {
        self.parameters.get(ordinal)?.as_ref()
    }

    fn returns_null_on_null_argument(&self, call: &Function, index: usize) -> bool {
        let database = self.confinement.database;
        let Ok(reached) = database.functions_reached_by(call, IdentifierCase::Folded) else {
            return false;
        };
        let mut any = false;
        for function in reached {
            if !function.returns_null_on_null_argument(database, index) {
                return false;
            }
            any = true;
        }
        any
    }
}

/// Finds a term that may evaluate otherwise for the caller than for the
/// owner, breaking with `None`, or with the error resolving a call.
struct CallerBound<'a, DB: DatabaseLike>(&'a DB);

impl<DB: DatabaseLike> CallerBound<'_, DB> {
    /// Whether `call` may reach a function of the schema running as its
    /// caller.
    fn runs_as_caller<'c>(&self, call: impl Into<WrittenCall<'c>>) -> Result<bool, LookupError> {
        Ok(self.0.functions_reached_by(call, IdentifierCase::Folded)?.any(|function| {
            function.target_name().schema() != Some("pg_catalog")
                && function.security_mode() != FunctionSecurity::Definer
        }))
    }

    /// The walk's step for whether a term runs as its caller.
    fn verdict(outcome: Result<bool, LookupError>) -> ControlFlow<Option<LookupError>> {
        match outcome {
            Ok(false) => ControlFlow::Continue(()),
            Ok(true) => ControlFlow::Break(None),
            Err(error) => ControlFlow::Break(Some(error)),
        }
    }
}

impl<DB: DatabaseLike> Visitor for CallerBound<'_, DB> {
    type Break = Option<LookupError>;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        let Expr::Function(call) = expression else {
            return ControlFlow::Continue(());
        };
        if is_role_read(call) || reads_session_state(call) {
            return ControlFlow::Break(None);
        }
        Self::verdict(self.runs_as_caller(call))
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { args: None, .. } = factor {
            return ControlFlow::Break(None);
        }
        WrittenCall::relation(factor)
            .map_or(ControlFlow::Continue(()), |call| Self::verdict(self.runs_as_caller(call)))
    }
}

/// Whether `call` asks what privileges or roles the current role holds, such
/// as `has_table_privilege` or `pg_has_role`, or which schemas the search
/// path names, which the function's own `SET search_path` changes.
fn reads_session_state(call: &Function) -> bool {
    call.name.0.last().and_then(|part| part.as_ident()).is_some_and(|name| {
        let name = name.value.to_lowercase();
        (name.starts_with("has_") && name.ends_with("_privilege"))
            || matches!(
                name.as_str(),
                "pg_has_role" | "row_security_active" | "current_schema" | "current_schemas"
            )
    })
}

/// What a relation of a `FROM` item holds under a column name.
enum Side<'c, DB: DatabaseLike> {
    /// Something other than a stored table or a source projecting a stored
    /// column under the name, whose columns are not read.
    Unknown,
    /// A stored table with no column of the name.
    Absent,
    /// The stored column the name reaches.
    Column(&'c DB::Column),
}

/// A relation reading a `WITH` item or a subquery whose body yields each
/// of its rows from a row passing the body's `WHERE`.
#[derive(Clone, Copy)]
struct Source<'s> {
    relation: &'s TableFactor,
    body: &'s Select,
    /// The names the alias of the relation gives its columns.
    aliased: &'s [TableAliasColumnDef],
    /// The names the `WITH` item gives its columns, which `aliased`
    /// overrides.
    renamed: &'s [TableAliasColumnDef],
}

impl Source<'_> {
    /// The position of the first output column named `name`, normalized,
    /// unless the body projects a `*`, which may shift or hold it.
    /// PostgreSQL refuses a `USING` name a side holds twice.
    fn ordinal(&self, name: &str) -> Option<usize> {
        let mut found = None;
        for (ordinal, item) in self.body.projection.iter().enumerate() {
            let projected = match item {
                SelectItem::ExprWithAlias { alias, .. } => Some(alias),
                SelectItem::UnnamedExpr(Expr::Identifier(name)) => Some(name),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => parts.last(),
                SelectItem::UnnamedExpr(_) => None,
                _ => return None,
            };
            let output = self
                .aliased
                .get(ordinal)
                .or_else(|| self.renamed.get(ordinal))
                .map(|column| &column.name)
                .or(projected);
            if found.is_none()
                && output.is_some_and(|output| {
                    normalize_identifier(&output.value, output.quote_style.is_some()) == name
                })
            {
                found = Some(ordinal);
            }
        }
        found
    }

    /// The definition and binding of the column the body projects as its
    /// output column named `name`, normalized.
    fn column<'c, 'q, 'a, DB: DatabaseLike>(
        &self,
        context: &'c ExpressionContext<'q, 'a, DB>,
        name: &str,
    ) -> Option<(ColumnDefinition<'c, 'q, 'a, DB>, ColumnBinding<'c>)> {
        let projected = projection_at(self.body, self.ordinal(name)?)?;
        context.reference(projected).ok().flatten()
    }
}

/// What a relation of a `FROM` item reads.
enum Read<'a, DB: DatabaseLike> {
    /// No table with row-level security.
    Open,
    /// The table with row-level security.
    Protected(&'a DB::Table),
    /// Something no filter confines, such as a view.
    Unconfined,
}

/// What a function of a row trigger's chain knows of the trigger's rows.
struct Firing<'f, 'a, DB: DatabaseLike> {
    /// What the trigger's rows hold.
    record: &'f Record<'a, DB>,
    /// The field of the trigger's rows each input parameter holds, by the
    /// ordinal `$n` counts it with, for a function a call passes them, and
    /// none for the trigger's own function.
    passed: &'f [Option<(Row, String)>],
}

/// What a statement of a PL/pgSQL body knows of the variables it reads.
struct Known<'k, 'a, DB: DatabaseLike> {
    /// What the trigger's rows hold, in a function of the trigger's chain.
    record: Option<&'k Record<'a, DB>>,
    /// The field of the trigger's rows each input parameter holds, as
    /// [`Firing::passed`] gives it.
    passed: &'k [Option<(Row, String)>],
    /// The variables the statement reads holding the value they enter the
    /// body with, by id.
    unchanged: Vec<usize>,
    /// The function whose body holds the statement.
    function: &'a DB::Function,
    /// The procedural variables available to a synthesized conflict-update
    /// context.
    scope: &'k PlPgSqlScope<'k, 'a, DB>,
}

impl<'k, DB: DatabaseLike> Known<'k, '_, DB> {
    /// The row of the trigger and the stored name of the field `reference`,
    /// which `definition` defines, holds the value of: a field of `NEW` or
    /// `OLD`, or a parameter passed one, holding the value it entered the
    /// function with.
    fn field(
        &self,
        definition: &ColumnDefinition<'_, '_, '_, DB>,
        reference: &Expr,
    ) -> Option<(Row, Cow<'k, str>)> {
        match definition {
            ColumnDefinition::VariableField(variable) => {
                self.record_field(variable, reference).map(|(row, name)| (row, Cow::Owned(name)))
            }
            ColumnDefinition::Variable(variable) => {
                let (row, name) = self.passed.get(self.parameter(variable)?)?.as_ref()?;
                Some((*row, Cow::Borrowed(name.as_str())))
            }
            _ => None,
        }
    }

    /// The ordinal among the input parameters, as `$n` counts them, of the
    /// parameter `variable` names, when it holds the argument the call
    /// passed.
    fn parameter(&self, variable: &PlPgSqlVariable<'_>) -> Option<usize> {
        let VariableKind::Parameter { position, mode } = variable.kind() else {
            return None;
        };
        if matches!(mode, ArgMode::Out) || !self.unchanged.contains(&variable.id()) {
            return None;
        }
        Some(
            self.function
                .argument_modes()
                .take(position.checked_sub(1)?)
                .filter(|mode| !matches!(mode, ArgMode::Out))
                .count(),
        )
    }

    /// The row of the trigger and the stored name of the field `reference`
    /// reads, when it reads a field of `NEW` or `OLD` holding the value the
    /// row entered the function with.
    fn record_field(
        &self,
        variable: &PlPgSqlVariable<'_>,
        reference: &Expr,
    ) -> Option<(Row, String)> {
        self.record?;
        if *variable.kind() != VariableKind::Trigger || !self.unchanged.contains(&variable.id()) {
            return None;
        }
        let row = match variable.name() {
            "new" => Row::New,
            "old" => Row::Old,
            _ => return None,
        };
        let Expr::CompoundIdentifier(parts) = unnested(reference) else {
            return None;
        };
        let [.., record, field] = parts.as_slice() else {
            return None;
        };
        (normalize_identifier(&record.value, record.quote_style.is_some()) == variable.name()).then(
            || (row, normalize_identifier(&field.value, field.quote_style.is_some()).into_owned()),
        )
    }
}

/// Walks a body for a read the filters around it leave unconfined.
struct Reads<'c, 'q, 'a, DB: DatabaseLike> {
    body: Body<'c, 'a, DB>,
    context: &'c ExpressionContext<'q, 'a, DB>,
    path: &'c BodyPath<'c>,
    items: WithScopes,
    lineage: &'c Lineage,
    /// What a statement of a PL/pgSQL body knows of the variables it reads.
    known: Option<&'c Known<'c, 'a, DB>>,
}

impl<'c, 'q, 'a, DB: DatabaseLike> Reads<'c, 'q, 'a, DB> {
    /// Proves joined reads and any target rows exposed through a write's
    /// result.
    fn confines_write(
        &mut self,
        statement: &Statement,
        observed: bool,
    ) -> Result<bool, LookupError> {
        if observed || returns_rows(statement) {
            match statement {
                Statement::Update(update) => {
                    let others = match &update.from {
                        Some(
                            UpdateTableFromKind::BeforeSet(from)
                            | UpdateTableFromKind::AfterSet(from),
                        ) => from.as_slice(),
                        None => &[],
                    };
                    return self.confines_returned_write(
                        statement,
                        FromItems { first: slice::from_ref(&update.table), second: others },
                        update.selection.as_ref(),
                    );
                }
                Statement::Delete(delete) => {
                    let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) =
                        &delete.from;
                    return self.confines_returned_write(
                        statement,
                        FromItems { first: from, second: delete.using.as_deref().unwrap_or(&[]) },
                        delete.selection.as_ref(),
                    );
                }
                Statement::Insert(insert) => {
                    if let Some(OnInsert::OnConflict(conflict)) = &insert.on
                        && let OnConflictAction::DoUpdate(update) = &conflict.action
                        && !self.confines_conflict(insert, update)?
                    {
                        return Ok(false);
                    }
                }
                Statement::Merge(merge) if !matches!(self.read(&merge.table)?, Read::Open) => {
                    let table = self.stored(&merge.table);
                    return Ok(self.body.confinement.refuse(table, Unconfinement::Unfollowed));
                }
                _ => {}
            }
        }
        let written = match statement {
            Statement::Update(update) => Some(&update.table.relation),
            Statement::Delete(delete) => {
                match &delete.from {
                    FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from) => {
                        from.first().map(|from| &from.relation)
                    }
                }
            }
            _ => None,
        };
        let joined = outer_relations(statement, |relation| {
            if written.is_some_and(|written| ptr::eq(written, relation)) {
                return ControlFlow::Continue(());
            }
            match self.read(relation) {
                Ok(Read::Open) => ControlFlow::Continue(()),
                Ok(Read::Protected(table)) => {
                    self.body.confinement.refuse(Some(table), Unconfinement::Unfollowed);
                    ControlFlow::Break(None)
                }
                Ok(Read::Unconfined) => {
                    self.body.confinement.unfollowed();
                    ControlFlow::Break(None)
                }
                Err(error) => ControlFlow::Break(Some(error)),
            }
        });
        match joined {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(None) => return Ok(false),
            ControlFlow::Break(Some(error)) => return Err(error),
        }
        self.confines_write_queries(statement)
    }

    /// Proves the target selection of a write whose affected rows are observed.
    fn confines_returned_write(
        &mut self,
        statement: &Statement,
        from: FromItems<'_>,
        selection: Option<&Expr>,
    ) -> Result<bool, LookupError> {
        let filters = self.filters_from(from, selection, &[], &[])?;
        if !self.confines_sources(&filters, None)? {
            return Ok(false);
        }
        self.confines_write_queries(statement)
    }

    /// Proves the existing rows returned by a conflict update in its own target
    /// scope.
    fn confines_conflict(&self, insert: &Insert, conflict: &DoUpdate) -> Result<bool, LookupError> {
        let TableObject::TableName(name) = &insert.table else {
            return Ok(self.body.confinement.unfollowed());
        };
        let Some(Relation::Table(table)) = self.resolved_relation(name) else {
            return Ok(self.body.confinement.unfollowed());
        };
        if !table.has_row_level_security(self.body.confinement.database)? {
            return Ok(true);
        }
        let target = TableWithJoins {
            relation: TableFactor::Table {
                name: name.clone(),
                alias: insert.table_alias.as_ref().map(|alias| {
                    TableAlias {
                        explicit: alias.explicit,
                        name: alias.alias.clone(),
                        columns: Vec::new(),
                        at: None,
                    }
                }),
                args: None,
                with_hints: Vec::new(),
                version: None,
                with_ordinality: false,
                partitions: Vec::new(),
                json_path: None,
                sample: None,
                index_directives: Vec::new(),
                inheritance: None,
            },
            joins: Vec::new(),
        };
        let statement = Statement::Update(Update {
            update_token: insert.insert_token.clone(),
            optimizer_hints: Vec::new(),
            table: target,
            on_cluster: None,
            assignments: Vec::new(),
            from: None,
            selection: conflict.selection.clone(),
            returning: None,
            output: None,
            or: None,
            order_by: Vec::new(),
            limit: None,
            settings: None,
        });
        let scope = self.known.map_or(ExpressionScope::FunctionBody(self.body.function), |known| {
            ExpressionScope::PlPgSql(known.scope)
        });
        let Ok(context) = ExpressionContext::for_statement(
            &statement,
            &scope,
            self.body.confinement.database,
            IdentifierCase::AsWritten,
        ) else {
            return Ok(self.body.confinement.unfollowed());
        };
        let reads = Reads {
            body: Body {
                parameters: self.body.parameters,
                confinement: self.body.confinement,
                function: self.body.function,
            },
            context: &context,
            path: self.path,
            items: WithScopes::default(),
            lineage: self.lineage,
            known: self.known,
        };
        let Statement::Update(update) = &statement else {
            return Ok(self.body.confinement.unfollowed());
        };
        let filters = reads.filters_from(
            FromItems { first: slice::from_ref(&update.table), second: &[] },
            update.selection.as_ref(),
            &[],
            &[],
        )?;
        reads.confines_sources(&filters, None)
    }

    /// Proves subqueries evaluated by a write statement.
    fn confines_write_queries(&mut self, statement: &Statement) -> Result<bool, LookupError> {
        let outcome = nested_queries(statement, 0, |nested| {
            match self.confines_query(nested, &[], &[]) {
                Ok(true) => ControlFlow::Continue(()),
                Ok(false) => ControlFlow::Break(None),
                Err(error) => ControlFlow::Break(Some(error)),
            }
        });
        match outcome {
            ControlFlow::Continue(()) => Ok(true),
            ControlFlow::Break(None) => Ok(false),
            ControlFlow::Break(Some(error)) => Err(error),
        }
    }

    /// Whether `query` and every query nested in it confine their reads,
    /// where `held` holds for every outer row the result of `query` reaches
    /// the function's result through and `owners` are the queries around it
    /// whose `WITH` holds items in reach.
    fn confines_query(
        &mut self,
        query: &Query,
        held: &[&Expr],
        owners: &[&Query],
    ) -> Result<bool, LookupError> {
        let mut owners = owners.to_vec();
        if query.with.is_some() {
            owners.push(query);
        }
        self.items.enter(query);
        let confined = match self.confines_nested(query, held, &owners) {
            Ok(true) => self.confines(&query.body, held, &owners),
            other => other,
        };
        self.items.leave(query);
        confined
    }

    /// Whether each query nested directly in `query` confines its reads. One
    /// in a condition of a `WHERE` also holds the other conditions of that
    /// `WHERE`, since the outer row passes them whenever its outcome counts.
    fn confines_nested(
        &mut self,
        query: &Query,
        held: &[&Expr],
        owners: &[&Query],
    ) -> Result<bool, LookupError> {
        let mut beside = Vec::new();
        conditions_beside(&query.body, &mut beside);
        let outcome = nested_queries(query, 1, |nested| {
            let mut within = held.to_vec();
            if let Some((_, others)) = beside.iter().find(|(at, _)| ptr::eq(*at, nested)) {
                within.extend_from_slice(others);
            }
            match self.confines_query(nested, &within, owners) {
                Ok(true) => ControlFlow::Continue(()),
                Ok(false) => ControlFlow::Break(None),
                Err(error) => ControlFlow::Break(Some(error)),
            }
        });
        match outcome {
            ControlFlow::Continue(()) => Ok(true),
            ControlFlow::Break(None) => Ok(false),
            ControlFlow::Break(Some(error)) => Err(error),
        }
    }

    /// Whether every `SELECT` of `body`, outside nested queries, confines its
    /// reads, a body that writes or reads `TABLE` never.
    fn confines(
        &self,
        body: &SetExpr,
        held: &[&Expr],
        owners: &[&Query],
    ) -> Result<bool, LookupError> {
        match body {
            SetExpr::Select(select) => self.confines_select(select, held, owners),
            SetExpr::SetOperation { left, right, .. } => {
                Ok(self.confines(left, held, owners)? && self.confines(right, held, owners)?)
            }
            SetExpr::Query(_) | SetExpr::Values(_) => Ok(true),
            _ => Ok(self.body.confinement.unfollowed()),
        }
    }

    /// Whether each relation with row-level security a `SELECT` reads is
    /// confined by the filters that drop its rows, the conditions `held` of
    /// the outer rows, and what the sets they test its columns against tell.
    fn confines_select(
        &self,
        select: &Select,
        held: &[&Expr],
        owners: &[&Query],
    ) -> Result<bool, LookupError> {
        let filters = self.filters(select, held, owners)?;
        self.confines_sources(&filters, Some(select))
    }

    /// Proves every source row using its predicates and optional query-result
    /// lineage.
    fn confines_sources(
        &self,
        filters: &Filters<'_>,
        select: Option<&Select>,
    ) -> Result<bool, LookupError> {
        for from in filters.from.iter() {
            for (position, relation) in relations(from).enumerate() {
                let table = match self.read(relation)? {
                    Read::Open => continue,
                    Read::Unconfined => return Ok(self.body.confinement.unfollowed()),
                    Read::Protected(table) => table,
                };
                let Stated { resolver, equalities, mut conditions, caller_bound, .. } =
                    self.stated(filters, from, position)?;
                let mut drawn = Vec::new();
                for condition in conditions.iter().chain(&caller_bound) {
                    drawn.extend(self.drawn_condition(table, condition, &conditions)?);
                }
                conditions.extend(drawn);
                let witnessed =
                    self.witnessed_sets(filters, table, relation, &equalities, &conditions)?;
                conditions.extend(witnessed);
                if let Some(select) = select
                    && let Some(membership) = self.lineage.membership(select, |column| {
                        match term(&resolver, column, relation, &equalities)? {
                            Term::Column(column) => Some(column),
                            Term::Parameter(_) => None,
                        }
                    })
                {
                    conditions.push(membership);
                }
                if !self.body.admits_every_caller(table, &conjunction(conditions))? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// The filters of `select`, with the conditions `held` of the outer rows
    /// and the queries `owners` around it whose `WITH` holds items in reach.
    fn filters<'e>(
        &self,
        select: &'e Select,
        held: &'e [&'e Expr],
        owners: &[&'e Query],
    ) -> Result<Filters<'e>, LookupError> {
        self.filters_from(
            FromItems { first: &select.from, second: &[] },
            select.selection.as_ref(),
            held,
            owners,
        )
    }

    /// Collects predicates over borrowed read or write sources.
    fn filters_from<'e>(
        &self,
        from: FromItems<'e>,
        selection: Option<&'e Expr>,
        held: &'e [&'e Expr],
        owners: &[&'e Query],
    ) -> Result<Filters<'e>, LookupError> {
        let filter: Vec<&Expr> = selection.into_iter().flat_map(conjuncts).collect();
        let sources = self.sources(from, owners);
        let witnessed = filter
            .iter()
            .copied()
            .filter_map(semi_join)
            .chain(sources.iter().map(|source| source.body))
            .flat_map(|found| found.selection.iter().flat_map(conjuncts))
            .collect();
        let mut rejected = Vec::with_capacity(from.len());
        for item in from.iter() {
            rejected.push(self.rejected(item, &filter)?);
        }
        let merges = self.merges(from, &sources)?;
        Ok(Filters { held, filter, witnessed, sources, from, rejected, merges })
    }

    /// What the filters of the relation at `position` of `from` state of it.
    #[expect(
        clippy::expect_used,
        reason = "the relation belongs to a FROM item of the filtered SELECT"
    )]
    fn stated<'r>(
        &'r self,
        filters: &'r Filters<'_>,
        from: &'r TableWithJoins,
        position: usize,
    ) -> Result<Stated<'r, 'q, 'a, DB>, LookupError> {
        let relation = relation_at(from, position);
        let item = filters
            .from
            .iter()
            .position(|item| ptr::eq(item, from))
            .expect("the relation is of a `FROM` item of the filtered `SELECT`");
        let resolver = Resolver {
            context: self.context,
            merges: &filters.merges,
            from,
            rejected: &filters.rejected[item],
            sources: &filters.sources,
            known: self.known,
        };
        let dropping = filters.dropping(&resolver, position);
        let mut equalities = Equalities::of(&resolver, &dropping, position)?;
        let record = self.known.and_then(|known| known.record);
        if let Some(record) = record
            && let Some(table) = self.stored(relation)
        {
            self.link_keys(record, &mut equalities, relation, table)?;
        }
        let mut conditions = Vec::new();
        if let Some(record) = record {
            conditions.extend(
                record
                    .facts
                    .iter()
                    .filter_map(|(row, fact)| stated_fact(fact, *row, relation, &equalities)),
            );
        }
        let mut caller_bound = Vec::new();
        let mut whole = true;
        for &(clause, conjunct) in &dropping {
            let stated =
                renamed(conjunct, |reference| term(&resolver, reference, relation, &equalities));
            match stated {
                Some(stated) if self.body.confinement.reads_alike(conjunct)? => {
                    conditions.push(stated);
                }
                Some(stated) => {
                    caller_bound.push(stated);
                    whole &= !matches!(clause, Clause::Where);
                }
                None => whole &= !matches!(clause, Clause::Where),
            }
        }
        Ok(Stated { resolver, equalities, conditions, caller_bound, whole })
    }

    /// Holds the columns of the read `relation` of `table` equal to the
    /// fields of the trigger's row that a foreign key of `record` holds
    /// equal to them, once `equalities` hold the key's columns of a unique
    /// key of `table` equal to their fields. The row the read reaches is then
    /// the one the key references, when the key checks every row holding a
    /// value in the columns held equal.
    fn link_keys<'r>(
        &self,
        record: &'r Record<'a, DB>,
        equalities: &mut Equalities<'r>,
        relation: &'r TableFactor,
        table: &'a DB::Table,
    ) -> Result<(), LookupError> {
        let database = self.body.confinement.database;
        if !no_inheritors(database, table)? {
            return Ok(());
        }
        let unique = equality_keys(database, table)?;
        for &(row, key) in &record.keys {
            if !ptr::eq(key.referenced_table(database)?, table) {
                continue;
            }
            let pairs = key_pairs(database, key)?;
            let node = |column: &'a DB::Column| {
                Node::Column { factor: relation, name: column.stored_column_name() }
            };
            let field =
                |column: &'a DB::Column| Node::Record { row, name: column.stored_column_name() };
            let (bound, held): (Vec<&DB::Column>, Vec<&DB::Column>) = pairs
                .iter()
                .filter(|(host, referenced)| equalities.same(&field(host), &node(referenced)))
                .map(|(host, referenced)| (*referenced, *host))
                .unzip();
            if !unique.iter().any(|columns| {
                columns.iter().all(|column| bound.iter().any(|bound| ptr::eq(*bound, *column)))
            }) {
                continue;
            }
            if !checks_rows_holding(key, &held, database)? {
                continue;
            }
            for (host, referenced) in &pairs {
                if same_identity(database, host, referenced)? {
                    equalities.link(field(host), node(referenced));
                }
            }
        }
        Ok(())
    }

    /// The stored table `relation` reads every row of under its own column
    /// names, which a `WITH` item, a call, `ONLY` and a sample never are.
    fn stored(&self, relation: &TableFactor) -> Option<&'a DB::Table> {
        let TableFactor::Table {
            name,
            alias,
            args: None,
            sample: None,
            inheritance: None | Some(TableInheritance::IncludeDescendants),
            ..
        } = relation
        else {
            return None;
        };
        if self.items.binding(name).is_some()
            || alias.as_ref().is_some_and(|alias| !alias.columns.is_empty())
        {
            return None;
        }
        match self.resolved_relation(name) {
            Some(Relation::Table(table)) => Some(table),
            _ => None,
        }
    }

    /// What `condition`, stated of the columns of `table`, tells of its row
    /// when it tests a column against the set of a function, besides the
    /// `conditions` of the row: one of the rows the function may draw the
    /// value from that the keys do not keep apart from the row meets its
    /// conditions that carry over to the row, those reading a parameter
    /// stated of the argument the call passes when each is a literal or a
    /// parameter.
    fn drawn_condition(
        &self,
        table: &'a DB::Table,
        condition: &Expr,
        conditions: &[Expr],
    ) -> Result<Option<Expr>, LookupError> {
        let confinement = self.body.confinement;
        let database = confinement.database;
        let Some(([tested], members)) = membership(condition) else {
            return Ok(None);
        };
        let (Some(name), Expr::Function(call)) = (stored_name(tested), members.as_ref()) else {
            return Ok(None);
        };
        let (Some(column), Some(function)) =
            (column_named(database, table, &name)?, called(database, call)?)
        else {
            return Ok(None);
        };
        let arms = confinement.arms(function)?;
        if !arms.complete {
            return Ok(None);
        }
        let arguments = positional_arguments(call, function).filter(|arguments| {
            arguments.iter().all(|argument| {
                literal(argument).is_some() || parameter_ordinal(unwrapped(argument)).is_some()
            })
        });
        let tested_kinds = discriminators(database, table, column, conditions)?;
        let mut alternatives = Vec::new();
        for arm in &arms.arms {
            let drawn = arm.drawn.as_ref();
            let source = drawn.column;
            let kinds = discriminators(database, drawn.table, source, &drawn.conditions)?;
            if apart(&tested_kinds, &kinds) {
                continue;
            }
            let met = drawn.known(arguments.as_deref());
            let carried = carried_conditions(database, drawn, &met, table, column)?;
            if carried.is_empty() {
                return Ok(None);
            }
            alternatives.push(conjunction(carried));
        }
        Ok(alternatives.into_iter().reduce(|left, right| {
            Expr::BinaryOp { left: Box::new(left), op: BinaryOperator::Or, right: Box::new(right) }
        }))
    }

    /// `column IN (SELECT helper())` for `relation`, a read of `table` its
    /// filters state `conditions` of, for each helper a policy of the table
    /// tests a column against, when a row of a table `select` reads meets
    /// every condition of an exhaustive arm of the helper over that table,
    /// those reading a parameter stated of the literal the policy passes,
    /// and the filters hold the column the arm returns equal to `column`.
    fn witnessed_sets(
        &self,
        filters: &Filters<'_>,
        table: &'a DB::Table,
        relation: &TableFactor,
        equalities: &Equalities<'_>,
        conditions: &[Expr],
    ) -> Result<Vec<Expr>, LookupError> {
        let sets = self.policy_sets(table)?;
        let mut witnessed = Vec::new();
        if sets.is_empty() {
            return Ok(witnessed);
        }
        let arguments: Vec<Option<Vec<&Expr>>> = sets
            .iter()
            .map(|set| set.literals.then(|| listed_arguments(&set.call)).flatten())
            .collect();
        for from in filters.from.iter() {
            for (position, joined) in relations(from).enumerate() {
                let same = ptr::eq(joined, relation);
                let Some(joined_table) = self.stored(joined) else {
                    continue;
                };
                if !same && !present(from, position) {
                    continue;
                }
                let arms: Vec<_> = sets
                    .iter()
                    .zip(&arguments)
                    .flat_map(|(set, arguments)| {
                        set.arms
                            .arms
                            .iter()
                            .filter(|arm| arm.exhaustive && arm.drawn.table == joined_table)
                            .filter_map(move |arm| {
                                let met = arm.drawn.under(arguments.as_deref())?;
                                Some((&set.call, arm.drawn.as_ref(), met))
                            })
                    })
                    .collect();
                if arms.is_empty() {
                    continue;
                }
                let stated;
                let row = if same {
                    conditions
                } else {
                    stated = self.stated(filters, from, position)?;
                    stated.conditions.as_slice()
                };
                let clause = conjunction(row.to_vec());
                for (members, drawn, met) in arms {
                    if !self.implies_every(&clause, &met)? {
                        continue;
                    }
                    let column = if same {
                        Some(drawn.column.stored_column_name())
                    } else {
                        equalities
                            .of_relation(
                                &Node::Column {
                                    factor: joined,
                                    name: drawn.column.stored_column_name(),
                                },
                                relation,
                            )
                            .map(Cow::Borrowed)
                    };
                    if let Some(column) = column
                        && let Some(membership) = set_membership(&column, members)
                        && !witnessed.contains(&membership)
                    {
                        witnessed.push(membership);
                    }
                }
            }
        }
        Ok(witnessed)
    }

    /// Whether `clause` implies each of `conditions`.
    fn implies_every(&self, clause: &Expr, conditions: &[Expr]) -> Result<bool, LookupError> {
        for condition in conditions {
            if !clause_implies(&self.body, clause, &Condition::new(&self.body, condition), &[])? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The calls of `SECURITY DEFINER` functions the policies of `table` test
    /// a column against, as written, each with the rows it draws from, when
    /// the function runs as the owner this body runs as, so that it sees the
    /// rows the body reads, some arm of it is exhaustive for the literals
    /// the call passes, and a `STRICT` function gets no argument that may be
    /// `NULL`.
    fn policy_sets(&self, table: &'a DB::Table) -> Result<Vec<SetCall<'a, DB>>, LookupError> {
        let confinement = self.body.confinement;
        let database = confinement.database;
        let function = self.body.function;
        let mut sets = Vec::new();
        if function.security_mode() != FunctionSecurity::Definer {
            return Ok(sets);
        }
        let owner = function.owner(database)?;
        let mut calls: Vec<Function> = Vec::new();
        for policy in table.policies(database)? {
            let Some(using) = policy.using_clause(database)? else {
                continue;
            };
            let _ = walk_expressions(using.expression(), |expression| {
                if let Some((_, members)) = membership(expression)
                    && matches!(members.as_ref(), Expr::Function(written) if !calls.contains(written))
                    && let Expr::Function(written) = members.into_owned()
                {
                    calls.push(written);
                }
                ControlFlow::<()>::Continue(())
            });
        }
        for written in calls {
            let Some(helper) = called(database, &written)? else {
                continue;
            };
            if helper.security_mode() != FunctionSecurity::Definer
                || helper.owner(database)? != owner
                || (matches!(
                    helper.null_input_behavior(),
                    FunctionCalledOnNull::Strict | FunctionCalledOnNull::ReturnsNullOnNullInput
                ) && !arguments_set(&written, helper))
            {
                continue;
            }
            let literals = positional_arguments(&written, helper)
                .is_some_and(|passed| passed.iter().all(|argument| literal(argument).is_some()));
            let arms = confinement.arms(helper)?;
            if arms
                .arms
                .iter()
                .any(|arm| arm.exhaustive && (arm.drawn.bound.is_empty() || literals))
            {
                sets.push(SetCall { call: written, literals, arms });
            }
        }
        Ok(sets)
    }

    /// Reads, standing in the `WITH` scopes of each, the rows each leaf of
    /// `drawing` in `query` draws its value from.
    fn draw_query(
        &mut self,
        query: &Query,
        drawing: &mut Drawing<'a, DB>,
    ) -> Result<(), LookupError> {
        self.items.enter(query);
        let drawn = query
            .with
            .iter()
            .flat_map(|with| &with.cte_tables)
            .try_for_each(|item| self.draw_query(&item.query, drawing))
            .and_then(|()| self.draw_body(&query.body, drawing));
        self.items.leave(query);
        drawn
    }

    /// [`Self::draw_query`] over the arms of `body`.
    fn draw_body(
        &mut self,
        body: &SetExpr,
        drawing: &mut Drawing<'a, DB>,
    ) -> Result<(), LookupError> {
        match body {
            SetExpr::Select(select) => {
                let address = ptr::from_ref(select.as_ref());
                if let Some(position) = drawing.leaves.iter().position(|leaf| *leaf == address) {
                    drawing.leaves.swap_remove(position);
                    self.draw_select(select, drawing)?;
                }
                Ok(())
            }
            SetExpr::SetOperation { left, right, .. } => {
                self.draw_body(left, drawing)?;
                self.draw_body(right, drawing)
            }
            SetExpr::Query(query) => self.draw_query(query, drawing),
            _ => Ok(()),
        }
    }

    /// Adds to `drawing` the row of a stored table whose column the leaf
    /// `select` returns, with what its filters state of the row, or the rows
    /// of the function whose call it returns the value of, with what those
    /// state of that function's parameters for the arguments the call passes,
    /// and marks the arms incomplete for any other leaf.
    fn draw_select(
        &self,
        select: &Select,
        drawing: &mut Drawing<'a, DB>,
    ) -> Result<(), LookupError> {
        let projected = carried_column(select).map(|(projected, _)| projected);
        let factor = projected
            .and_then(|projected| self.context.reference(projected).ok().flatten())
            .and_then(|(_, binding)| binding.entry())
            .map(|entry| ptr::from_ref(entry.factor()));
        let found = factor.and_then(|factor| {
            select.from.iter().find_map(|from| {
                relations(from)
                    .position(|relation| ptr::eq(relation, factor))
                    .map(|position| (from, position))
            })
        });
        if let (Some(projected), Some((from, position))) = (projected, found) {
            let relation = relation_at(from, position);
            if let Some(table) = self.stored(relation) {
                let filters = self.filters(select, &[], &[])?;
                let Stated { resolver, equalities, conditions, whole: stated, .. } =
                    self.stated(&filters, from, position)?;
                if let Some(Term::Column(name)) = term(&resolver, projected, relation, &equalities)
                    && let Some(column) =
                        column_named(self.body.confinement.database, table, &name)?
                {
                    drawing.push(
                        table,
                        column,
                        conditions,
                        stated && select.from.len() == 1 && from.joins.is_empty(),
                    );
                    return Ok(());
                }
            } else if let Some(call) = whole_call(relation)
                && let Some(function) = called(self.body.confinement.database, call)?
            {
                let called = self.body.confinement.arms(function)?;
                let passed = self.passed(relation, function);
                let passed: Option<Vec<&Expr>> =
                    passed.as_ref().map(|passed| passed.iter().collect());
                drawing.arms.complete &= called.complete;
                for arm in &called.arms {
                    let drawn = &arm.drawn;
                    if drawing.member.is_none() && drawn.bound.is_empty() && drawn.member.is_none()
                    {
                        drawing.arms.arms.push(Arm { drawn: Rc::clone(drawn), exhaustive: false });
                        continue;
                    }
                    let conditions = drawn.known(passed.as_deref()).into_owned();
                    drawing.push(drawn.table, drawn.column, conditions, false);
                }
                return Ok(());
            }
        }
        drawing.arms.complete = false;
        Ok(())
    }

    /// The arguments `relation`, a call to `function`, passes it, each a
    /// literal or an input parameter of the body read as `$n`, or `None`
    /// when one is neither.
    fn passed(&self, relation: &TableFactor, function: &DB::Function) -> Option<Vec<Expr>> {
        relation_arguments(relation, function)?
            .into_iter()
            .map(|argument| {
                if literal(argument).is_some() || parameter_ordinal(argument).is_some() {
                    return Some(argument.clone());
                }
                match self.context.reference(argument).ok().flatten()? {
                    (ColumnDefinition::Parameter { ordinal, .. }, _) => Some(parameter(ordinal)),
                    _ => None,
                }
            })
            .collect()
    }

    /// The columns merged by the sources' `USING` joins.
    fn merges(
        &self,
        from: FromItems<'_>,
        sources: &[Source<'_>],
    ) -> Result<Vec<Merge>, LookupError> {
        let mut merges: Vec<Merge> = Vec::new();
        for from in from.iter() {
            let natural = from.joins.iter().any(|join| {
                matches!(join_constraint(&join.join_operator), Some(JoinConstraint::Natural))
            });
            for (index, join) in from.joins.iter().enumerate() {
                let Some(JoinConstraint::Using(names) | JoinConstraint::UsingAlias(names, _)) =
                    join_constraint(&join.join_operator)
                else {
                    continue;
                };
                for name in
                    names.iter().filter_map(|name| name.0.last().and_then(ObjectNamePart::as_ident))
                {
                    let right = self.identity_at(from, index + 1, name, sources)?;
                    let normalized = normalize_identifier(&name.value, name.quote_style.is_some());
                    if let Some(merge) = merges
                        .iter_mut()
                        .find(|merge| ptr::eq(merge.from, from) && merge.name == normalized)
                    {
                        merge.last = index;
                        merge.sides.push(index + 1);
                        if merge.identity != right {
                            merge.identity = None;
                        }
                    } else {
                        let left = self.left_side(from, index, name, sources)?;
                        merges.push(Merge {
                            from,
                            name: normalized.into_owned(),
                            last: index,
                            sides: left.iter().map(|(side, _)| *side).chain([index + 1]).collect(),
                            identity: left.and_then(|(_, identity)| {
                                (!natural && right.as_ref() == Some(&identity)).then_some(identity)
                            }),
                        });
                    }
                }
            }
        }
        Ok(merges)
    }

    /// For each join of `from`, whether a condition of `filter` cannot be
    /// true on a row it extends with nulls, so that it drops the rows of its
    /// outer join that find no match.
    fn rejected(&self, from: &TableWithJoins, filter: &[&Expr]) -> Result<Vec<bool>, LookupError> {
        let database = self.body.confinement.database;
        let mut rejected = Vec::with_capacity(from.joins.len());
        for (index, join) in from.joins.iter().enumerate() {
            let extended = match join.join_operator {
                JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => index + 1..index + 2,
                JoinOperator::Right(_) | JoinOperator::RightOuter(_) => 0..index + 1,
                _ => {
                    rejected.push(false);
                    continue;
                }
            };
            let bound = |expression: &Expr| {
                Ok(self
                    .context
                    .reference(expression)?
                    .and_then(|(_, binding)| binding.entry())
                    .is_some_and(|entry| {
                        extended
                            .clone()
                            .any(|position| ptr::eq(entry.factor(), relation_at(from, position)))
                    }))
            };
            let mut found = false;
            for condition in filter {
                if rejects_null(condition, &bound, database, self.context)? {
                    found = true;
                    break;
                }
            }
            rejected.push(found);
        }
        Ok(rejected)
    }

    /// The relations of the `FROM` items of `select` that read a `WITH` item
    /// or a subquery whose body yields each of its rows from a row passing
    /// its `WHERE`. A body two of them read is left out, since their rows
    /// come from different rows of it.
    fn sources<'s>(&self, from: FromItems<'s>, owners: &[&'s Query]) -> Vec<Source<'s>> {
        let mut sources: Vec<Source<'_>> = Vec::new();
        for from in from.iter() {
            for (position, relation) in relations(from).enumerate() {
                if !present(from, position) {
                    continue;
                }
                let read = match relation {
                    TableFactor::Derived { subquery, alias, .. } => {
                        Some((subquery.as_ref(), &[][..], alias))
                    }
                    TableFactor::Table { name, alias, args: None, .. } => {
                        self.items.binding(name).filter(|binding| !binding.in_own_body).and_then(
                            |binding| {
                                let owner =
                                    owners.iter().find(|owner| ptr::eq(**owner, binding.owner))?;
                                let item = &owner.with.as_ref()?.cte_tables[binding.item];
                                Some((item.query.as_ref(), item.alias.columns.as_slice(), alias))
                            },
                        )
                    }
                    _ => None,
                };
                if let Some((query, renamed, alias)) = read
                    && let Some(body) = witnessed(query)
                {
                    sources.push(Source {
                        relation,
                        body,
                        aliased: alias.as_ref().map_or(&[], |alias| alias.columns.as_slice()),
                        renamed,
                    });
                }
            }
        }
        sources
            .iter()
            .filter(|source| {
                sources.iter().filter(|other| ptr::eq(other.body, source.body)).count() == 1
            })
            .copied()
            .collect()
    }

    /// The position of the first relation of `from`, up to the one at
    /// `index`, with a column named `name`, with what `=` compares its values
    /// by, `None` when any of those relations is no stored table. PostgreSQL
    /// refuses a `USING` name more than one relation before the join holds.
    fn left_side(
        &self,
        from: &TableWithJoins,
        index: usize,
        name: &Ident,
        sources: &[Source<'_>],
    ) -> Result<Option<(usize, Identity)>, LookupError> {
        let mut found = None;
        for position in 0..=index {
            match self.column_at(from, position, name, sources)? {
                Side::Unknown => return Ok(None),
                Side::Column(column) if found.is_none() => found = Some((position, column)),
                Side::Column(_) | Side::Absent => {}
            }
        }
        found.map_or(Ok(None), |(position, column)| {
            Ok(identity(column, self.body.confinement.database)?
                .map(|identity| (position, identity)))
        })
    }

    /// What `=` compares the values of the column named `name` of the
    /// relation at `position` of `from` by, when it reaches a stored column.
    fn identity_at(
        &self,
        from: &TableWithJoins,
        position: usize,
        name: &Ident,
        sources: &[Source<'_>],
    ) -> Result<Option<Identity>, LookupError> {
        match self.column_at(from, position, name, sources)? {
            Side::Column(column) => identity(column, self.body.confinement.database),
            Side::Unknown | Side::Absent => Ok(None),
        }
    }

    /// The column named `name` of the relation at `position` of `from`, for
    /// a source the stored column its body projects under the name.
    fn column_at(
        &self,
        from: &TableWithJoins,
        position: usize,
        name: &Ident,
        sources: &[Source<'_>],
    ) -> Result<Side<'c, DB>, LookupError> {
        let database = self.body.confinement.database;
        let factor = relation_at(from, position);
        if let Some(source) = sources.iter().find(|source| ptr::eq(source.relation, factor)) {
            let normalized = normalize_identifier(&name.value, name.quote_style.is_some());
            return Ok(match source.column(self.context, &normalized) {
                Some((ColumnDefinition::Base { column, .. }, _)) => Side::Column(column),
                _ => Side::Unknown,
            });
        }
        let TableFactor::Table { name: relation, alias, args: None, .. } = factor else {
            return Ok(Side::Unknown);
        };
        let stored = if self.items.binding(relation).is_none() {
            match self.resolved_relation(relation) {
                Some(Relation::Table(table)) => Some(table),
                _ => None,
            }
        } else {
            None
        };
        let Some(table) = stored else {
            return Ok(Side::Unknown);
        };
        Ok(stored_column(database, table, alias.as_ref(), name)?.map_or(Side::Absent, Side::Column))
    }

    /// Resolves a relation only when the statement and expression context bind
    /// it alike.
    fn resolved_relation(&self, name: &ObjectName) -> Option<Relation<'a, DB>> {
        let database = self.body.confinement.database;
        let actual = resolve_on_body_path(name, database, self.path).ok()??;
        if matches!(self.path, BodyPath::Database) {
            return Some(actual);
        }
        let contextual = resolve_on_body_path(name, database, &BodyPath::Database).ok()??;
        actual.is_same(&contextual).then_some(actual)
    }

    /// What `relation` reads, a `WITH` item and a call nothing.
    fn read(&self, relation: &TableFactor) -> Result<Read<'a, DB>, LookupError> {
        let database = self.body.confinement.database;
        match relation {
            TableFactor::Table { name, alias, args: None, .. } => {
                if self.items.binding(name).is_some() {
                    return Ok(Read::Open);
                }
                Ok(match self.resolved_relation(name) {
                    Some(Relation::Table(table)) if table.has_row_level_security(database)? => {
                        if alias.as_ref().is_some_and(|alias| !alias.columns.is_empty()) {
                            Read::Unconfined
                        } else {
                            Read::Protected(table)
                        }
                    }
                    Some(Relation::View(_)) | None => Read::Unconfined,
                    Some(_) => Read::Open,
                })
            }
            TableFactor::NestedJoin { table_with_joins, .. } => self.read_nested(table_with_joins),
            _ => Ok(Read::Open),
        }
    }

    /// What a parenthesized join reads, which is nothing when none of its
    /// relations reads a table with row-level security and else something
    /// left unconfined.
    fn read_nested(&self, joined: &TableWithJoins) -> Result<Read<'a, DB>, LookupError> {
        for relation in relations(joined) {
            if !matches!(self.read(relation)?, Read::Open) {
                return Ok(Read::Unconfined);
            }
        }
        Ok(Read::Open)
    }
}

/// The ids of the variables `reads` read holding the value they enter the
/// body with.
fn unchanged_variables<DB: DatabaseLike>(reads: &[VariableRead<'_, '_, DB>]) -> Vec<usize> {
    reads
        .iter()
        .filter(|read| {
            match read.reaching() {
                Reaching::Definitions(definitions) => {
                    definitions.iter().all(|definition| {
                        definition.part().is_empty()
                            && matches!(definition.source(), DefinitionSource::Incoming)
                    })
                }
                Reaching::Unreachable => false,
            }
        })
        .map(|read| read.variable().id())
        .collect()
}

/// Records, for each query nested in a condition of a `WHERE` of a `SELECT`
/// of `body`, outside nested queries, the other conditions of that `WHERE`.
fn conditions_beside<'q>(body: &'q SetExpr, beside: &mut Vec<(*const Query, Vec<&'q Expr>)>) {
    match body {
        SetExpr::Select(select) => {
            let filter: Vec<&Expr> = select.selection.iter().flat_map(conjuncts).collect();
            for (index, condition) in filter.iter().enumerate() {
                let _ = nested_queries(*condition, 0, |nested| {
                    let others = filter
                        .iter()
                        .enumerate()
                        .filter(|(other, _)| *other != index)
                        .map(|(_, other)| *other);
                    beside.push((ptr::from_ref(nested), others.collect()));
                    ControlFlow::<()>::Continue(())
                });
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            conditions_beside(left, beside);
            conditions_beside(right, beside);
        }
        _ => {}
    }
}

/// Hands `each` every relation `node` names outside its queries, until
/// `each` breaks.
fn outer_relations<N: Visit + ?Sized, B>(
    node: &N,
    each: impl FnMut(&TableFactor) -> ControlFlow<B>,
) -> ControlFlow<B> {
    /// The walk's state, `entered` counting the queries it is inside.
    struct Outer<F> {
        entered: usize,
        each: F,
    }

    impl<B, F: FnMut(&TableFactor) -> ControlFlow<B>> Visitor for Outer<F> {
        type Break = B;

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<B> {
            self.entered += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<B> {
            self.entered -= 1;
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, relation: &TableFactor) -> ControlFlow<B> {
            if self.entered == 0 { (self.each)(relation) } else { ControlFlow::Continue(()) }
        }
    }

    walk(node, &mut Outer { entered: 0, each })
}

/// Hands `each` every query of `node` that `depth` of its queries enclose,
/// until `each` breaks.
fn nested_queries<N: Visit + ?Sized, B>(
    node: &N,
    depth: usize,
    each: impl FnMut(&Query) -> ControlFlow<B>,
) -> ControlFlow<B> {
    /// The walk's state, `entered` counting the queries it is inside.
    struct Nested<F> {
        depth: usize,
        entered: usize,
        each: F,
    }

    impl<B, F: FnMut(&Query) -> ControlFlow<B>> Visitor for Nested<F> {
        type Break = B;

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<B> {
            let outcome = if self.entered == self.depth {
                (self.each)(query)
            } else {
                ControlFlow::Continue(())
            };
            self.entered += 1;
            outcome
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<B> {
            self.entered -= 1;
            ControlFlow::Continue(())
        }
    }

    walk(node, &mut Nested { depth, entered: 0, each })
}

/// The columns the column equalities among some filters hold equal, where
/// `=` holds only between identical values.
#[derive(Default)]
struct Equalities<'r> {
    /// Each column an equality names.
    columns: Vec<Node<'r>>,
    /// For each column, the index of one of its class nearer the column
    /// heading the class, or its own for the head.
    parents: Vec<usize>,
}

/// A column an equality names.
#[derive(PartialEq, Eq)]
enum Node<'r> {
    /// The column of this name of a `FROM` item.
    Column { factor: *const TableFactor, name: Cow<'r, str> },
    /// The column of this name the `USING` joins of a `FROM` item merge.
    Merged { from: *const TableWithJoins, name: &'r str },
    /// The field of this stored name of the trigger's row.
    Record { row: Row, name: Cow<'r, str> },
}

impl<'r> Node<'r> {
    /// The column named `name` of the relation at `position` of `from`.
    fn at(from: &TableWithJoins, position: usize, name: &'r str) -> Self {
        Self::Column { factor: relation_at(from, position), name: Cow::Borrowed(name) }
    }
}

impl Equalities<'_> {
    /// Whether `left` and `right` are held equal.
    fn same(&self, left: &Node<'_>, right: &Node<'_>) -> bool {
        match (self.position(left), self.position(right)) {
            (Some(left), Some(right)) => self.head(left) == self.head(right),
            _ => false,
        }
    }
}

impl<'r> Equalities<'r> {
    /// The classes the conditions `a.x = b.y` and `a.x IN (SELECT b.y …)`
    /// among `filters` form, with those the `USING` joins `resolver` reads
    /// form for the relation at `position` of its `FROM` item. A `USING`
    /// join holds the merged column equal to the column of each relation it
    /// merges whose row is present, and the column of its first join equal
    /// on both sides wherever that join drops rows of the relation.
    fn of<DB: DatabaseLike>(
        resolver: &Resolver<'r, '_, '_, DB>,
        filters: &[(Clause, &Expr)],
        position: usize,
    ) -> Result<Self, LookupError> {
        let mut equalities = Self::default();
        for &(_, filter) in filters {
            let (left, right) = match unnested(filter) {
                Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
                    (left.as_ref(), right.as_ref())
                }
                Expr::In { expr, .. } => {
                    match semi_join(filter).and_then(carried_column) {
                        Some((projected, _)) => (expr.as_ref(), projected),
                        None => continue,
                    }
                }
                _ => continue,
            };
            if let (Some((left, kind)), Some((right, other))) =
                (resolver.equated(left)?, resolver.equated(right)?)
                && kind == other
            {
                equalities.link(left, right);
            }
        }
        let from = resolver.from;
        for merge in resolver
            .merges
            .iter()
            .filter(|merge| ptr::eq(merge.from, from) && merge.identity.is_some())
        {
            if merge.sides.contains(&position) {
                equalities.link(merge.node(), Node::at(from, position, &merge.name));
            }
            for (index, &side) in merge.sides.iter().enumerate().skip(1) {
                if resolver.dropping(side - 1, position).is_some()
                    && merge.sides[1..index].iter().all(|earlier| resolver.keeps_left(earlier - 1))
                {
                    equalities.link(
                        resolver.column_node(merge.sides[0], &merge.name),
                        resolver.column_node(side, &merge.name),
                    );
                }
            }
        }
        Ok(equalities)
    }

    /// Joins the classes of `left` and `right`.
    fn link(&mut self, left: Node<'r>, right: Node<'r>) {
        let left = self.joined(left);
        let right = self.joined(right);
        self.parents[left] = right;
    }

    /// The head of the class of `column`, which forms a class of its own
    /// when no equality named it yet.
    fn joined(&mut self, column: Node<'r>) -> usize {
        let index = self.position(&column).unwrap_or_else(|| {
            self.parents.push(self.columns.len());
            self.columns.push(column);
            self.columns.len() - 1
        });
        self.head(index)
    }

    /// The index of `column`.
    fn position(&self, column: &Node<'_>) -> Option<usize> {
        self.columns.iter().position(|named| named == column)
    }

    /// The head of the class of the column at `index`.
    fn head(&self, mut index: usize) -> usize {
        while self.parents[index] != index {
            index = self.parents[index];
        }
        index
    }

    /// The name of a column of `relation` held equal to `column`.
    fn of_relation(&self, column: &Node<'_>, relation: &TableFactor) -> Option<&str> {
        let class = self.head(self.position(column)?);
        self.columns.iter().enumerate().find_map(|(index, named)| {
            match named {
                Node::Column { factor, name }
                    if ptr::eq(*factor, relation) && self.head(index) == class =>
                {
                    Some(name.as_ref())
                }
                _ => None,
            }
        })
    }
}

/// A column the `USING` joins of one `FROM` item merge.
struct Merge {
    from: *const TableWithJoins,
    /// The name the joins merge.
    name: String,
    /// The index of the last join merging the name.
    last: usize,
    /// The positions of the relations the joins merge the column of, the
    /// one before the first join leading.
    sides: Vec<usize>,
    /// What `=` compares the merged values by, when every relation the
    /// joins merge is a stored table whose column compares by it and no
    /// `NATURAL` join merges more.
    identity: Option<Identity>,
}

impl Merge {
    /// The merged column.
    fn node(&self) -> Node<'_> {
        Node::Merged { from: self.from, name: &self.name }
    }
}

/// Where a filter of a read stands.
#[derive(Clone, Copy)]
enum Clause {
    /// A condition of the outer rows.
    Held,
    /// The `WHERE` of the `SELECT`.
    Where,
    /// The `WHERE` of a query an `EXISTS` or `IN` of that `WHERE` finds a
    /// row of, or of the body of a `WITH` item or subquery of its `FROM`.
    Witness,
    /// An `ON` condition of the `FROM` item.
    On,
}

/// The filters of a `SELECT`, with the conditions held of its outer rows.
struct Filters<'e> {
    held: &'e [&'e Expr],
    /// The conjuncts of the `WHERE`.
    filter: Vec<&'e Expr>,
    /// The conjuncts of the `WHERE` of each query an `EXISTS` or `IN` of
    /// the `WHERE` finds a row of, and of the body each relation of
    /// `sources` reads.
    witnessed: Vec<&'e Expr>,
    /// The relations of the `SELECT` reading a `WITH` item or subquery
    /// every row of which comes from a row of its body.
    sources: Vec<Source<'e>>,
    /// The `FROM` items of the `SELECT`.
    from: FromItems<'e>,
    /// For each join of each item of `from`, whether the `WHERE` drops the
    /// rows it extends with nulls.
    rejected: Vec<Vec<bool>>,
    /// The columns the `USING` joins of the `SELECT` merge.
    merges: Vec<Merge>,
}

impl Filters<'_> {
    /// The filters that drop rows of the relation at `position` of the
    /// `FROM` item `resolver` resolves, each with where it stands.
    fn dropping<'f, DB: DatabaseLike>(
        &'f self,
        resolver: &Resolver<'f, '_, '_, DB>,
        position: usize,
    ) -> Vec<(Clause, &'f Expr)> {
        let present = resolver.present(position);
        self.held
            .iter()
            .map(|condition| (Clause::Held, *condition))
            .chain(
                self.filter.iter().filter(|_| present).map(|condition| (Clause::Where, *condition)),
            )
            .chain(
                self.witnessed
                    .iter()
                    .filter(|_| present)
                    .map(|condition| (Clause::Witness, *condition)),
            )
            .chain(resolver.joined_filter(position).map(|condition| (Clause::On, condition)))
            .collect()
    }
}

/// What the filters of one relation of a `FROM` item state of its row.
struct Stated<'r, 'q, 'a, DB: DatabaseLike> {
    resolver: Resolver<'r, 'q, 'a, DB>,
    equalities: Equalities<'r>,
    /// The filters, over the stored names of the relation's columns, that
    /// evaluate alike for every role.
    conditions: Vec<Expr>,
    /// The other filters over those names, which may evaluate otherwise for
    /// a caller than for the owner. A row such a filter tests against the
    /// set of a function still meets the conditions the function draws the
    /// set under.
    caller_bound: Vec<Expr>,
    /// Whether `conditions` states every conjunct of the `WHERE`.
    whole: bool,
}

/// The call of a function a policy tests a column against, as written,
/// with the rows the function draws from.
struct SetCall<'a, DB: DatabaseLike> {
    call: Function,
    /// Whether the call passes a literal for each input parameter.
    literals: bool,
    arms: Rc<Arms<'a, DB>>,
}

/// A function, by its address, with the rows it draws from.
type ReadArms<'a, DB> = (*const <DB as DatabaseLike>::Function, Rc<Arms<'a, DB>>);

/// Resolves the columns the filters of the relations of one `FROM` item
/// name.
struct Resolver<'r, 'q, 'a, DB: DatabaseLike> {
    context: &'r ExpressionContext<'q, 'a, DB>,
    /// The columns the `USING` joins of the `SELECT` merge.
    merges: &'r [Merge],
    from: &'r TableWithJoins,
    /// For each join of `from`, whether the `WHERE` drops the rows it
    /// extends with nulls.
    rejected: &'r [bool],
    /// The relations of the `SELECT` reading a `WITH` item or subquery
    /// every row of which comes from a row of its body.
    sources: &'r [Source<'r>],
    /// What a statement of a PL/pgSQL body knows of the variables it reads.
    known: Option<&'r Known<'r, 'a, DB>>,
}

impl<'r, 'q, 'a, DB: DatabaseLike> Resolver<'r, 'q, 'a, DB> {
    /// Whether every join that can null-extend this relation rejects its padded
    /// rows.
    fn present(&self, position: usize) -> bool {
        self.from.joins.iter().enumerate().all(|(index, join)| {
            if self.rejected[index] {
                return true;
            }
            match &join.join_operator {
                JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::CrossJoin(_) => true,
                JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => position != index + 1,
                JoinOperator::Right(_) | JoinOperator::RightOuter(_) => position > index,
                _ => false,
            }
        })
    }

    /// The field of the trigger's row `reference` reads, itself or through a
    /// parameter passed it, with the column of the trigger's table it holds
    /// the value of, when it reads one the row entered the function with.
    fn record_field(&self, reference: &Expr) -> Option<(Node<'r>, &'a DB::Column)> {
        let known = self.known?;
        let record = known.record?;
        let (definition, _) = self.context.reference(unnested(reference)).ok()??;
        let (row, name) = known.field(&definition, reference)?;
        let column = column_named(self.context.database(), record.table, &name).ok()??;
        Some((Node::Record { row, name }, column))
    }

    /// The definition and binding of the column `reference` names. A column
    /// a source projects from a column reference is that column of the row
    /// its `SELECT` yields it from.
    fn reference(
        &self,
        reference: &Expr,
    ) -> Option<(ColumnDefinition<'r, 'q, 'a, DB>, ColumnBinding<'r>)> {
        let (definition, binding) = self.context.reference(reference).ok()??;
        if let Some(entry) = binding.entry()
            && let Some(source) =
                self.sources.iter().find(|source| ptr::eq(source.relation, entry.factor()))
            && let Some(projected) = projection_at(source.body, entry.ordinal())
            && let Some(resolved) = self.context.reference(projected).ok().flatten()
        {
            return Some(resolved);
        }
        Some((definition, binding))
    }

    /// The conjuncts of the `ON` conditions of the `FROM` item that drop
    /// rows of its relation at `position`, counted from zero.
    fn joined_filter(&self, position: usize) -> impl Iterator<Item = &'r Expr> + '_ {
        (0..self.from.joins.len())
            .filter_map(move |index| {
                match self.dropping(index, position) {
                    Some(JoinConstraint::On(condition)) => Some(condition),
                    _ => None,
                }
            })
            .flat_map(conjuncts)
    }

    /// The constraint of the join at `index` of the `FROM` item when it
    /// drops rows of its relation at `position`. An inner join drops rows of
    /// the relations up to the one it joins, a left join of the one it joins
    /// and a right join of those before it. An outer join whose rows
    /// extended with nulls the `WHERE` drops acts as an inner join.
    fn dropping(&self, index: usize, position: usize) -> Option<&'r JoinConstraint> {
        let joined = index + 1;
        match &self.from.joins[index].join_operator {
            JoinOperator::Join(constraint) | JoinOperator::Inner(constraint)
                if position <= joined =>
            {
                Some(constraint)
            }
            JoinOperator::Left(constraint)
            | JoinOperator::LeftOuter(constraint)
            | JoinOperator::Right(constraint)
            | JoinOperator::RightOuter(constraint)
                if self.rejected[index] && position <= joined =>
            {
                Some(constraint)
            }
            JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint)
                if position == joined =>
            {
                Some(constraint)
            }
            JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint)
                if position < joined =>
            {
                Some(constraint)
            }
            _ => None,
        }
    }

    /// Whether the join at `index` of the `FROM` item yields, on each row,
    /// the value its left side holds in a column a `USING` merges.
    fn keeps_left(&self, index: usize) -> bool {
        match self.from.joins[index].join_operator {
            JoinOperator::Join(_)
            | JoinOperator::Inner(_)
            | JoinOperator::Left(_)
            | JoinOperator::LeftOuter(_) => true,
            JoinOperator::Right(_) | JoinOperator::RightOuter(_) => self.rejected[index],
            _ => false,
        }
    }

    /// The column named `name` of the relation at `position` of the `FROM`
    /// item, for a source the column of the row of its body it comes from.
    fn column_node(&self, position: usize, name: &'r str) -> Node<'r> {
        let relation = relation_at(self.from, position);
        self.sources
            .iter()
            .find(|source| ptr::eq(source.relation, relation))
            .and_then(|source| source.column(self.context, name))
            .and_then(|(_, binding)| self.node(&binding))
            .unwrap_or_else(|| Node::at(self.from, position, name))
    }

    /// The column a `USING` join of the `SELECT` merged that `binding`
    /// reached.
    fn merge(&self, binding: &ColumnBinding<'_>) -> Option<&'r Merge> {
        let found = binding.merge()?;
        let (name, quoted) = binding.exposed()?;
        let name = normalize_identifier(name, quoted);
        self.merges.iter().find(|merge| {
            ptr::eq(merge.from, found.from()) && merge.last == found.join() && merge.name == name
        })
    }

    /// The column `binding` reached.
    fn node(&self, binding: &ColumnBinding<'r>) -> Option<Node<'r>> {
        if let Some(merge) = self.merge(binding) {
            return Some(merge.node());
        }
        let (entry, (name, quoted)) = (binding.entry()?, binding.exposed()?);
        Some(Node::Column { factor: entry.factor(), name: normalize_identifier(name, quoted) })
    }

    /// The column `side` names, with what `=` compares its values by, or
    /// `None` when two equal values of it may differ.
    fn equated(&self, side: &Expr) -> Result<Option<(Node<'r>, Cow<'r, Identity>)>, LookupError> {
        if let Some((node, column)) = self.record_field(side) {
            return Ok(identity(column, self.context.database())?
                .map(|identity| (node, Cow::Owned(identity))));
        }
        let Some((definition, binding)) = self.reference(unnested(side)) else {
            return Ok(None);
        };
        let identity = match (self.merge(&binding), definition) {
            (Some(merge), _) => merge.identity.as_ref().map(Cow::Borrowed),
            (None, ColumnDefinition::Base { column, database, .. }) => {
                identity(column, database)?.map(Cow::Owned)
            }
            _ => None,
        };
        Ok(identity.zip(self.node(&binding)).map(|(identity, node)| (node, identity)))
    }
}

/// The `SELECT`s of a function body each of whose rows reaches the
/// function's result holding the one column it projects, unchanged.
#[derive(Default)]
struct Lineage {
    /// `SELECT f($1, …)` calling the function with its own arguments, `None`
    /// when calls need not agree on the set it returns.
    member: Option<Box<Query>>,
    selects: Vec<*const Select>,
}

impl Lineage {
    /// The lineage of `query`, the body of `function`. A `SECURITY DEFINER`
    /// function that is not `VOLATILE` returns the same set to a caller as to
    /// a policy calling it with the same arguments.
    fn of<F: FunctionLike>(function: &F, query: &Query) -> Self {
        let member = (function.security_mode() == FunctionSecurity::Definer
            && function.volatility() != FunctionBehavior::Volatile
            && function.returns_set())
        .then(|| member(function))
        .flatten();
        let mut carried = Carried::default();
        if member.is_some() {
            carried.query(query);
        }
        Self { member, selects: carried.selects }
    }

    /// `"column" IN (SELECT f($1, …))` when `select` belongs to the lineage
    /// and `own` names the stored column of the projection.
    fn membership(
        &self,
        select: &Select,
        own: impl FnOnce(&Expr) -> Option<String>,
    ) -> Option<Expr> {
        let template = self.member.as_ref()?;
        if !self.selects.iter().any(|carried| ptr::eq(*carried, select)) {
            return None;
        }
        let (projected, _) = carried_column(select)?;
        Some(in_set(&own(projected)?, template.clone()))
    }
}

/// `"column" IN query`.
fn in_set(column: &str, query: Box<Query>) -> Expr {
    Expr::In {
        expr: Box::new(Expr::Identifier(Ident::with_quote('"', column))),
        target: InTarget::Subquery(query),
        negated: false,
        global: false,
    }
}

/// `"column" IN (SELECT members)`, testing `column` against the set
/// `members` yields.
fn set_membership(column: &str, members: &Function) -> Option<Expr> {
    Parser::new(&PostgreSqlDialect {})
        .try_with_sql(&format!("SELECT {members}"))
        .and_then(|mut parser| parser.parse_query())
        .ok()
        .map(|query| in_set(column, query))
}

/// The column of `table` whose stored name is `name`.
fn column_named<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
    name: &str,
) -> Result<Option<&'db DB::Column>, LookupError> {
    Ok(table.columns(database)?.find(|column| column.stored_column_name() == name))
}

/// Whether `condition` reads an input parameter, written `$n`.
fn reads_parameter(condition: &Expr) -> bool {
    walk_expressions(condition, |expression| {
        if let Expr::Value(value) = expression
            && matches!(value.value, Value::Placeholder(_))
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

/// Whether `column` alone is a unique key of `table`.
fn unique_alone<DB: DatabaseLike>(
    database: &DB,
    table: &DB::Table,
    column: &DB::Column,
) -> Result<bool, LookupError> {
    Ok(no_inheritors(database, table)?
        && equality_keys(database, table)?
            .iter()
            .any(|key| matches!(key.as_slice(), [only] if *only == column)))
}

/// The `conditions` the row `drawn` meets that hold of the row of `table`
/// whose `column` equals the `source` column of the row drawn, over the
/// columns of `table`. When `source` is `column` and unique in `table`, the
/// row drawn is that row and each condition holds of it. Otherwise the
/// value drawn carries the conditions on `source` when `=` compares both
/// columns by the same identity. When `column` is unique in `table`, a
/// foreign key of the drawn row that binds it maps `source` onto `column`
/// carries those on each column it maps onto one compared alike. When
/// `source` is unique in the drawn table, a foreign key of `table` binding
/// `column` onto `source` makes the row drawn the one it references, and
/// carries the conditions on each column it maps from one compared alike.
fn carried_conditions<DB: DatabaseLike>(
    database: &DB,
    drawn: &Drawn<'_, DB>,
    conditions: &[Expr],
    table: &DB::Table,
    column: &DB::Column,
) -> Result<Vec<Expr>, LookupError> {
    let source = drawn.column;
    let unique = unique_alone(database, table, column)?;
    if unique && source == column {
        return Ok(conditions.to_vec());
    }
    let mut renames = Vec::new();
    if same_identity(database, source, column)? {
        renames.push((source.stored_column_name(), column.stored_column_name()));
    }
    if unique {
        for foreign_key in drawn.table.foreign_keys(database)? {
            if !ptr::eq(foreign_key.referenced_table(database)?, table)
                || !binds(database, foreign_key, source)?
            {
                continue;
            }
            let pairs = key_pairs(database, foreign_key)?;
            if !pairs.iter().any(|(host, referenced)| *host == source && *referenced == column) {
                continue;
            }
            for (host, referenced) in pairs {
                if same_identity(database, host, referenced)? {
                    renames.push((host.stored_column_name(), referenced.stored_column_name()));
                }
            }
        }
    }
    if unique_alone(database, drawn.table, source)? {
        for foreign_key in table.foreign_keys(database)? {
            if !ptr::eq(foreign_key.referenced_table(database)?, drawn.table)
                || !binds(database, foreign_key, column)?
            {
                continue;
            }
            let pairs = key_pairs(database, foreign_key)?;
            if !pairs.iter().any(|(host, referenced)| *host == column && *referenced == source) {
                continue;
            }
            for (host, referenced) in pairs {
                if same_identity(database, host, referenced)? {
                    renames.push((referenced.stored_column_name(), host.stored_column_name()));
                }
            }
        }
    }
    Ok(conditions
        .iter()
        .filter_map(|condition| {
            renamed(condition, |reference| {
                let name = stored_name(reference)?;
                renames
                    .iter()
                    .find(|(from, _)| from.as_ref() == name)
                    .map(|(_, to)| Term::Column(to.clone().into_owned()))
            })
        })
        .collect())
}

/// The values a column of a row is pinned to, on a table whose column
/// holding the value followed is unique, so that every row of the table
/// holding that value pins them.
struct Pinned<'a, DB: DatabaseLike> {
    table: &'a DB::Table,
    /// The stored name of the column holding the value followed.
    keyed: String,
    /// The stored name of the pinned column.
    column: String,
    values: Vec<String>,
}

/// What pins the row holding the value of `column` of a row of `table`
/// meeting `conditions`. Following each foreign key that binds the column
/// holding the value, it reaches the one row each key references. Each row
/// reached pins the columns its table holds to literals and the columns a
/// key fills from a column pinned before, the first also those `conditions`
/// fix, counting only enum and text columns compared by identity.
fn discriminators<'a, DB: DatabaseLike>(
    database: &'a DB,
    table: &'a DB::Table,
    column: &'a DB::Column,
    conditions: &[Expr],
) -> Result<Vec<Pinned<'a, DB>>, LookupError> {
    let mut fixed = Vec::new();
    for (name, values) in conditions.iter().filter_map(fixed_by) {
        if let Some(column) = column_named(database, table, &name)?
            && let Some(pinned) = pin(database, column, &values)?
        {
            fixed.push(pinned);
        }
    }
    let mut pending = vec![(table, column, fixed)];
    let mut visited: Vec<&DB::Column> = Vec::new();
    let mut pinned = Vec::new();
    while let Some((table, column, mut fixed)) = pending.pop() {
        if visited.contains(&column) {
            continue;
        }
        visited.push(column);
        for held in table.columns(database)? {
            if let Some(values) = held_values(database, held)? {
                fixed.extend(pin(database, held, &values)?);
            }
        }
        if unique_alone(database, table, column)? {
            pinned.extend(fixed.iter().map(|(fixed_column, values)| {
                Pinned {
                    table,
                    keyed: column.stored_column_name().into_owned(),
                    column: fixed_column.stored_column_name().into_owned(),
                    values: values.clone(),
                }
            }));
        }
        for foreign_key in column.foreign_keys(database)? {
            if !binds(database, foreign_key, column)? {
                continue;
            }
            let pairs = key_pairs(database, foreign_key)?;
            let mut next = None;
            let mut carried = Vec::new();
            for &(host, referenced) in &pairs {
                if host == column {
                    next = Some(referenced);
                }
                if let Some((_, values)) = fixed.iter().find(|(fixed, _)| *fixed == host)
                    && same_identity(database, host, referenced)?
                {
                    carried.push((referenced, values.clone()));
                }
            }
            let referenced_table = foreign_key.referenced_table(database)?;
            pending.extend(next.map(|next| (referenced_table, next, carried)));
        }
    }
    Ok(pinned)
}

/// `condition` with each input parameter it reads as `$n` replaced by the
/// argument `arguments` holds for it, or `None` when one holds none.
fn bound_to(condition: &Expr, arguments: &[&Expr]) -> Option<Expr> {
    let mut bound = condition.clone();
    let unbound = walk_expressions_mut(&mut bound, |expression| {
        let Some(ordinal) = parameter_ordinal(expression) else {
            return ControlFlow::Continue(());
        };
        match arguments.get(ordinal) {
            Some(argument) => {
                *expression = (*argument).clone();
                ControlFlow::Continue(())
            }
            None => ControlFlow::Break(()),
        }
    });
    unbound.is_continue().then_some(bound)
}

/// Whether `call` passes `function` a literal other than `NULL` for each of
/// its input parameters, leaving none to a default.
fn arguments_set<F: FunctionLike>(call: &Function, function: &F) -> bool {
    let inputs = function.argument_modes().filter(|mode| !matches!(mode, ArgMode::Out)).count();
    matches!(&call.args, FunctionArguments::List(list) if list.args.len() == inputs
    && list.args.iter().all(|argument| {
        let (FunctionArg::Unnamed(argument)
        | FunctionArg::Named { arg: argument, .. }
        | FunctionArg::ExprNamed { arg: argument, .. }) = argument;
        matches!(argument, FunctionArgExpr::Expr(value)
            if literal(value).is_some_and(|value| !matches!(value, Value::Null)))
    }))
}

/// The call `relation` reads every row of, numbering none.
fn whole_call(relation: &TableFactor) -> Option<WrittenCall<'_>> {
    match relation {
        TableFactor::Table { with_ordinality: false, sample: None, .. }
        | TableFactor::Function { with_ordinality: false, .. } => WrittenCall::relation(relation),
        _ => None,
    }
}

/// Whether some table on which `left` and `right` each pin a column of the
/// row holding their value, keyed by the same column, pins it to values
/// none of which both allow, so the two values differ.
fn apart<DB: DatabaseLike>(left: &[Pinned<'_, DB>], right: &[Pinned<'_, DB>]) -> bool {
    left.iter().any(|left| {
        right.iter().any(|right| {
            ptr::eq(left.table, right.table)
                && left.keyed == right.keyed
                && left.column == right.column
                && !left.values.iter().any(|value| right.values.contains(value))
        })
    })
}

/// The relations of `from`, in order.
fn relations(from: &TableWithJoins) -> impl Iterator<Item = &TableFactor> {
    iter::once(&from.relation).chain(from.joins.iter().map(|join| &join.relation))
}

/// Whether each row `from` yields holds a row of the relation at
/// `position`, which no outer join fills with `NULL`s.
fn present(from: &TableWithJoins, position: usize) -> bool {
    from.joins.iter().enumerate().all(|(index, join)| {
        match &join.join_operator {
            JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::CrossJoin(_) => true,
            JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => position != index + 1,
            JoinOperator::Right(_) | JoinOperator::RightOuter(_) => position > index,
            _ => false,
        }
    })
}

/// `SELECT schema.name($1, …)` calling `function`, its schema `public` when
/// its name has none, or `None` when an argument is not an input.
fn member<F: FunctionLike>(function: &F) -> Option<Box<Query>> {
    let mut arguments = Vec::new();
    for (position, mode) in function.argument_modes().enumerate() {
        if mode != ArgMode::In {
            return None;
        }
        arguments.push(parameter(position).to_string());
    }
    let written = |name: &str, quoted: bool| {
        let stored = normalize_identifier(name, quoted).into_owned();
        if quoted { Ident::with_quote('"', stored) } else { Ident::new(stored) }
    };
    let target = function.target_name();
    let schema = target
        .schema()
        .map_or_else(|| Ident::new("public"), |schema| written(schema, target.schema_is_quoted()));
    let name = written(target.name(), target.name_is_quoted());
    Parser::new(&PostgreSqlDialect {})
        .try_with_sql(&format!("SELECT {schema}.{name}({})", arguments.join(", ")))
        .and_then(|mut parser| parser.parse_query())
        .ok()
}

/// Walks down from a result to the `SELECT`s every row of which reaches it.
#[derive(Default)]
struct Carried<'q> {
    selects: Vec<*const Select>,
    /// The `SELECT`s of `selects` that project a `WITH` item followed.
    through: Vec<*const Select>,
    /// Whether every row of the result comes from a `SELECT` of `selects`.
    complete: bool,
    /// The `WITH` clauses in reach, each with how many of its items, from
    /// the first, it shows.
    frames: Vec<(&'q With, usize)>,
    /// The `WITH` item bodies already followed, so a recursive item is
    /// followed once.
    followed: Vec<*const Query>,
}

impl<'q> Carried<'q> {
    /// A walk that has met no query yet.
    fn new() -> Self {
        Self { complete: true, ..Self::default() }
    }

    /// The `SELECT`s of `selects` drawing their rows from no `WITH` item
    /// followed.
    fn leaves(&self) -> Vec<*const Select> {
        self.selects.iter().copied().filter(|select| !self.through.contains(select)).collect()
    }

    /// Follows `query` unless a limit, an offset or a lock may leave some of
    /// its rows behind.
    fn query(&mut self, query: &'q Query) {
        if query.limit_clause.is_some()
            || query.fetch.is_some()
            || !query.pipe_operators.is_empty()
            || !query.locks.is_empty()
        {
            self.complete = false;
            return;
        }
        let depth = self.frames.len();
        if let Some(with) = &query.with {
            self.frames.push((with, with.cte_tables.len()));
        }
        self.body(&query.body);
        self.frames.truncate(depth);
    }

    /// Follows every arm of a `UNION`, which keeps each row of each arm.
    fn body(&mut self, body: &'q SetExpr) {
        match body {
            SetExpr::Select(select) => self.select(select),
            SetExpr::Query(query) => self.query(query),
            SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier: SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None,
                left,
                right,
            } => {
                self.body(left);
                self.body(right);
            }
            _ => self.complete = false,
        }
    }

    /// Records `select` when its rows each hold its one projected column,
    /// and follows the `WITH` item it projects whole.
    fn select(&mut self, select: &'q Select) {
        let Some((_, projected)) = carried_column(select) else {
            self.complete = false;
            return;
        };
        self.selects.push(select);
        let ([from], None) = (select.from.as_slice(), &select.selection) else {
            return;
        };
        let TableFactor::Table { name, alias, args: None, .. } = &from.relation else {
            return;
        };
        let ([ObjectNamePart::Identifier(written)], []) =
            (name.0.as_slice(), from.joins.as_slice())
        else {
            return;
        };
        let Some((frame, item)) = self.item(written) else {
            return;
        };
        let (with, _) = self.frames[frame];
        let cte = &with.cte_tables[item];
        let renamed = alias.as_ref().map_or(&[][..], |alias| alias.columns.as_slice());
        let column = match (renamed, cte.alias.columns.as_slice()) {
            ([column], _) | ([], [column]) => &column.name,
            ([], []) => {
                match output_name(&cte.query) {
                    Some(name) => name,
                    None => return,
                }
            }
            _ => return,
        };
        let relation = alias.as_ref().map_or(written, |alias| &alias.name);
        let body = ptr::from_ref(cte.query.as_ref());
        if !names(projected, relation, column) || self.followed.contains(&body) {
            return;
        }
        self.followed.push(body);
        self.through.push(select);
        let outer = mem::take(&mut self.frames);
        self.frames.extend_from_slice(&outer[..frame]);
        self.frames.push((with, if with.recursive { with.cte_tables.len() } else { item }));
        self.query(&cte.query);
        self.frames = outer;
    }

    /// The frame and position of the visible `WITH` item `written` names.
    fn item(&self, written: &Ident) -> Option<(usize, usize)> {
        self.frames.iter().enumerate().rev().find_map(|(frame, (with, visible))| {
            with.cte_tables[..*visible]
                .iter()
                .position(|item| same_name(&item.alias.name, written))
                .map(|item| (frame, item))
        })
    }
}

/// The column reference a `SELECT` projects alone, with the parts of its
/// name, when each row its `FROM` and filters leave yields one row holding
/// it, with no grouping, aggregate or `DISTINCT ON` choosing among them.
fn carried_column(select: &Select) -> Option<(&Expr, &[Ident])> {
    let [SelectItem::UnnamedExpr(projected) | SelectItem::ExprWithAlias { expr: projected, .. }] =
        select.projection.as_slice()
    else {
        return None;
    };
    let parts = match projected {
        Expr::Identifier(name) => slice::from_ref(name),
        Expr::CompoundIdentifier(parts) => parts.as_slice(),
        _ => return None,
    };
    plain(select).then_some((projected, parts))
}

/// The `SELECT` that `condition`, an `EXISTS` or `IN` over a query, finds
/// a row of that passes its `WHERE` whenever `condition` holds.
fn semi_join(condition: &Expr) -> Option<&Select> {
    let (Expr::Exists { subquery, negated: false }
    | Expr::In { target: InTarget::Subquery(subquery), negated: false, .. }) = unnested(condition)
    else {
        return None;
    };
    witnessed(subquery)
}

/// The expression `select` projects as its output column at `ordinal`,
/// counted from zero, when no `*` comes before it.
fn projection_at(select: &Select, ordinal: usize) -> Option<&Expr> {
    let mut projected = None;
    for item in select.projection.get(..=ordinal)? {
        match item {
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias { expr: expression, .. } => projected = Some(expression),
            _ => return None,
        }
    }
    projected
}

/// The `SELECT` of `query` each row of which comes from a row of its `FROM`
/// passing its `WHERE`, with no row when none passes, projecting columns,
/// constants or `*`.
fn witnessed(query: &Query) -> Option<&Select> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let projected = select.projection.iter().all(|item| {
        match item {
            SelectItem::UnnamedExpr(projected)
            | SelectItem::ExprWithAlias { expr: projected, .. } => {
                matches!(
                    projected,
                    Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_)
                )
            }
            _ => matches!(item, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)),
        }
    });
    (query.order_by.is_none() && plain(select) && projected).then_some(select)
}

/// The name of the one column `query` yields, given by its leftmost
/// `SELECT`.
fn output_name(query: &Query) -> Option<&Ident> {
    let mut body = query.body.as_ref();
    loop {
        match body {
            SetExpr::SetOperation { left, .. } => body = left,
            SetExpr::Query(query) => body = query.body.as_ref(),
            SetExpr::Select(select) => {
                return match select.projection.as_slice() {
                    [SelectItem::ExprWithAlias { alias, .. }] => Some(alias),
                    [SelectItem::UnnamedExpr(Expr::Identifier(name))] => Some(name),
                    [SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))] => parts.last(),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
}

/// Whether the column reference named by `parts` names `column` of the
/// relation a query names `relation`.
fn names(parts: &[Ident], relation: &Ident, column: &Ident) -> bool {
    matches!(parts, [name] if same_name(name, column))
        || matches!(parts, [qualifier, name] if same_name(qualifier, relation) && same_name(name, column))
}

/// The relation at `position` of `from`, counted from zero.
fn relation_at(from: &TableWithJoins, position: usize) -> &TableFactor {
    match position.checked_sub(1) {
        Some(join) => &from.joins[join].relation,
        None => &from.relation,
    }
}

/// The input parameter of the function whose body holds `reference` that
/// it names where no column of the same name is in reach, a PL/pgSQL
/// parameter holding the argument included, or else the stored name of the
/// column of `relation` it names or `equalities` hold equal to the column or
/// trigger row field it names.
fn term<'r, DB: DatabaseLike>(
    resolver: &Resolver<'r, '_, '_, DB>,
    reference: &Expr,
    relation: &TableFactor,
    equalities: &Equalities<'r>,
) -> Option<Term> {
    if let Some((node, _)) = resolver.record_field(reference)
        && let Some(column) = equalities.of_relation(&node, relation)
    {
        return Some(Term::Column(column.to_owned()));
    }
    match resolver.reference(reference)? {
        (ColumnDefinition::Parameter { ordinal, .. }, _) => Some(Term::Parameter(ordinal)),
        (ColumnDefinition::Variable(variable), _) => {
            resolver.known?.parameter(&variable).map(Term::Parameter)
        }
        (_, binding) => {
            match resolver.node(&binding)? {
                Node::Column { factor, name } if ptr::eq(factor, relation) => {
                    Some(Term::Column(name.into_owned()))
                }
                node => {
                    equalities
                        .of_relation(&node, relation)
                        .map(|column| Term::Column(column.to_owned()))
                }
            }
        }
    }
}

/// What `fact`, a condition of the trigger's `row` written over the columns
/// of its table, states of the columns of `relation` `equalities` hold
/// equal to those it reads: each conjunct stated whole, and each disjunct
/// weakened to its conjuncts stated whole. `None` when it states nothing.
fn stated_fact(
    fact: &Expr,
    row: Row,
    relation: &TableFactor,
    equalities: &Equalities<'_>,
) -> Option<Expr> {
    let joined = |left: Option<Expr>, op: BinaryOperator, right: Option<Expr>| {
        Some(Expr::BinaryOp { left: Box::new(left?), op, right: Box::new(right?) })
    };
    match unnested(fact) {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            match (
                stated_fact(left, row, relation, equalities),
                stated_fact(right, row, relation, equalities),
            ) {
                (Some(left), Some(right)) => joined(Some(left), BinaryOperator::And, Some(right)),
                (stated, None) | (None, stated) => stated,
            }
        }
        Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
            joined(
                stated_fact(left, row, relation, equalities),
                BinaryOperator::Or,
                stated_fact(right, row, relation, equalities),
            )
        }
        fact => {
            renamed(fact, |reference| {
                let (row, name) = match reference {
                    Expr::CompoundIdentifier(parts) => {
                        match parts.as_slice() {
                            [record, field] if record.quote_style.is_none() => {
                                let row = if record.value.eq_ignore_ascii_case("new") {
                                    Row::New
                                } else if record.value.eq_ignore_ascii_case("old") {
                                    Row::Old
                                } else {
                                    row
                                };
                                (
                                    row,
                                    normalize_identifier(&field.value, field.quote_style.is_some()),
                                )
                            }
                            _ => (row, stored_name(reference)?),
                        }
                    }
                    _ => (row, stored_name(reference)?),
                };
                equalities
                    .of_relation(&Node::Record { row, name }, relation)
                    .map(|column| Term::Column(column.to_owned()))
            })
        }
    }
}

/// The `AND` of `conjuncts`, `TRUE` for none.
fn conjunction(conjuncts: Vec<Expr>) -> Expr {
    conjuncts
        .into_iter()
        .reduce(|left, right| {
            Expr::BinaryOp { left: Box::new(left), op: BinaryOperator::And, right: Box::new(right) }
        })
        .unwrap_or(Expr::Value(Value::Boolean(true).into()))
}

/// The stored table name targeted by an ordinary write relation.
fn written_table_name(relation: &TableFactor) -> Option<&ObjectName> {
    match relation {
        TableFactor::Table { name, args: None, .. } => Some(name),
        _ => None,
    }
}

/// The write event whose table triggers and referential actions may fire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteEvent {
    Insert,
    Update,
    Delete,
    Truncate { cascade: bool },
}

/// Walks SQL statements under the search path each statement establishes.
struct SqlProof<'proof, 'path, 'db, DB: DatabaseLike> {
    confinement: &'proof mut Confinement<'db, DB>,
    function: &'db DB::Function,
    path: BodyPath<'path>,
    bound: bool,
    reads: bool,
    confined: bool,
}

impl<'path, DB: DatabaseLike> BodyWalk<'path> for SqlProof<'_, 'path, '_, DB> {
    type SavedPath = BodyPath<'path>;

    fn save_path(&self) -> Self::SavedPath {
        self.path.clone()
    }

    fn restore_path(&mut self, saved: Self::SavedPath) {
        self.path = saved;
    }

    fn set_path(&mut self, path: BodyPath<'path>) {
        self.path = path;
    }

    fn walk(&mut self, sql: EmbeddedSql<'_>) -> Result<(), LookupError> {
        if self.confined
            && let EmbeddedSql::Statement(statement) = sql
        {
            self.confined = if self.reads {
                self.confinement.confines_sql_site(self.function, statement, &self.path)?
            } else {
                self.confinement.confines_writes(&sql, &self.path, self.bound)?
            };
        }
        Ok(())
    }

    fn calls_path_changer(&self, sql: EmbeddedSql<'_>) -> bool {
        calls_path_changer(&sql, self.confinement.database)
    }
}

/// Resolves a write target using runtime bindings or its stored creation-time
/// binding.
fn written_table<'db, DB: DatabaseLike>(
    database: &'db DB,
    name: &ObjectName,
    path: &BodyPath<'_>,
    bound: bool,
) -> Result<Option<&'db DB::Table>, LookupError> {
    let relation = if bound {
        resolve_bound_relation_object_name(name, database)?
    } else {
        resolve_on_body_path(name, database, path)?
    };
    Ok(match relation {
        Some(Relation::Table(table)) => Some(table),
        _ => None,
    })
}

/// Borrows the target and auxiliary source lists of a read or write.
#[derive(Clone, Copy)]
struct FromItems<'source> {
    first: &'source [TableWithJoins],
    second: &'source [TableWithJoins],
}

impl<'source> FromItems<'source> {
    /// Iterates the source lists without copying their syntax trees.
    fn iter(self) -> impl Iterator<Item = &'source TableWithJoins> {
        self.first.iter().chain(self.second.iter())
    }

    /// Counts the sources across both lists.
    fn len(self) -> usize {
        self.first.len() + self.second.len()
    }
}

/// Whether a mutation exposes rows through a result clause.
fn returns_rows(statement: &Statement) -> bool {
    match statement {
        Statement::Update(update) => update.returning.is_some() || update.output.is_some(),
        Statement::Delete(delete) => delete.returning.is_some() || delete.output.is_some(),
        Statement::Insert(insert) => insert.returning.is_some() || insert.output.is_some(),
        Statement::Merge(merge) => merge.output.is_some(),
        _ => false,
    }
}
