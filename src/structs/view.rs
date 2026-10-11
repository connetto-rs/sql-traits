//! The two kinds of view a schema can hold.
//!
//! A plain view and a materialized view are near twins in what they expose, a
//! name and a definition query, and differ in four ways that matter: only the
//! plain one can be replaced, the two drop spellings refuse each other, nothing
//! can be written to a materialized one, and its rows are a stored snapshot
//! rather than the live rows of the relations underneath. Keeping them as two
//! types puts those differences in the type system rather than in a flag every
//! reader has to remember to check.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{CreateView, Query};

use crate::utils::object_name::{Qualifier, TEMPORARY_SCHEMA, object_name_last_part, qualifier_of};

/// The parts of a `CREATE VIEW` a schema records, shared by both view kinds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewDeclaration {
    /// The view's own identifier, exactly as written.
    name: String,
    /// Whether that identifier was quoted.
    name_is_quoted: bool,
    /// The schema qualifier the declaration wrote, if it wrote one.
    schema: Option<String>,
    /// Whether that qualifier was quoted.
    schema_is_quoted: bool,
    /// The query the view is defined by.
    query: Query,
    /// The column names the declaration wrote, each with its quote state.
    columns: Vec<(String, bool)>,
}

impl ViewDeclaration {
    /// Reads the parts a schema records out of a parsed `CREATE VIEW`.
    ///
    /// A `TEMP` declaration written without a schema is recorded in
    /// `pg_temp`, where PostgreSQL puts it, so an ordinary declaration and a
    /// temporary one never read the same. Answers [`None`] for a declaration
    /// carrying no usable name, which the caller refuses before it reaches
    /// here.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{prelude::*, structs::View};
    /// use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let declaration = |sql: &str| {
    ///     let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
    ///     let Some(Statement::CreateView(node)) = statements.pop() else {
    ///         panic!("expected a view declaration")
    ///     };
    ///     View::from_node(&node)
    /// };
    /// let ordinary = declaration("CREATE VIEW v AS SELECT 1 AS result").unwrap();
    /// let temporary = declaration("CREATE TEMP VIEW v AS SELECT 1 AS result").unwrap();
    /// assert_ne!(ordinary, temporary);
    /// assert_eq!(temporary.declaration().schema(), Some("pg_temp"));
    /// assert!(temporary.is_temporary());
    /// ```
    #[must_use]
    pub fn from_node(node: &CreateView) -> Option<Self> {
        let (name, name_is_quoted) = object_name_last_part(&node.name)?;
        // A qualifier built at run time names no schema, so the view cannot be
        // recorded at all rather than being recorded unqualified.
        let schema = match qualifier_of(&node.name) {
            Qualifier::Named(schema, quoted) => Some((schema, quoted)),
            Qualifier::Absent if node.temporary => Some((TEMPORARY_SCHEMA, false)),
            Qualifier::Absent => None,
            Qualifier::RunTime => return None,
        };
        Some(Self {
            name: name.to_string(),
            name_is_quoted,
            schema: schema.map(|(value, _)| value.to_string()),
            schema_is_quoted: schema.is_some_and(|(_, quoted)| quoted),
            query: (*node.query).clone(),
            columns: node
                .columns
                .iter()
                .map(|column| (column.name.value.clone(), column.name.quote_style.is_some()))
                .collect(),
        })
    }

    /// Returns the view's own identifier, exactly as written.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether that identifier was quoted.
    #[must_use]
    pub fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    /// Returns the schema qualifier the declaration wrote, if any.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Returns whether that qualifier was quoted.
    #[must_use]
    pub fn schema_is_quoted(&self) -> bool {
        self.schema_is_quoted
    }

    /// Returns the query the view is defined by.
    #[must_use]
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// Returns the column names the declaration wrote, each with its quote
    /// state.
    #[must_use]
    pub fn columns(&self) -> &[(String, bool)] {
        &self.columns
    }

    /// Replaces the view's own identifier, which a rename supplies.
    ///
    /// Crate-private: the container keys views by their stored identity and
    /// sorts them by it, so a name may only change where the index is rebuilt
    /// afterwards.
    pub(crate) fn set_name(&mut self, name: String, quoted: bool) {
        self.name = name;
        self.name_is_quoted = quoted;
    }
}

/// A view whose definition runs on every read.
///
/// Reading one reads the current rows of whatever it is defined over, so a
/// column reference through it resolves and, when its body preserves row
/// identity, so does the row-identity question.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct View {
    /// What the declaration said.
    declaration: ViewDeclaration,
}

impl View {
    /// Records a parsed `CREATE VIEW` that carries no `MATERIALIZED` modifier.
    ///
    /// Answers [`None`] for a declaration carrying no usable name.
    #[must_use]
    pub fn from_node(node: &CreateView) -> Option<Self> {
        ViewDeclaration::from_node(node).map(|declaration| Self { declaration })
    }

    /// Returns what the declaration said.
    #[must_use]
    pub fn declaration(&self) -> &ViewDeclaration {
        &self.declaration
    }

    /// Returns what the declaration said, for a statement that changes it.
    ///
    /// Crate-private, for the same reason [`ViewDeclaration::set_name`] is.
    pub(crate) fn declaration_mut(&mut self) -> &mut ViewDeclaration {
        &mut self.declaration
    }
}

/// A view holding a stored snapshot of its definition's output.
///
/// Its rows were produced when it was last populated, so they are not the
/// current rows of anything. A column reference through it still resolves,
/// because a column's declared type is inherited and cannot go stale, but the
/// row-identity question never answers through one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaterializedView {
    /// What the declaration said.
    declaration: ViewDeclaration,
}

impl MaterializedView {
    /// Records a parsed `CREATE VIEW` that carries the `MATERIALIZED`
    /// modifier.
    ///
    /// Answers [`None`] for a declaration carrying no usable name.
    #[must_use]
    pub fn from_node(node: &CreateView) -> Option<Self> {
        ViewDeclaration::from_node(node).map(|declaration| Self { declaration })
    }

    /// Returns what the declaration said.
    #[must_use]
    pub fn declaration(&self) -> &ViewDeclaration {
        &self.declaration
    }

    /// Returns what the declaration said, for a statement that changes it.
    ///
    /// Crate-private, for the same reason [`ViewDeclaration::set_name`] is.
    pub(crate) fn declaration_mut(&mut self) -> &mut ViewDeclaration {
        &mut self.declaration
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{Ident, ObjectNamePart, ObjectNamePartFunction, Statement},
        dialect::PostgreSqlDialect,
        parser::Parser,
    };

    use super::ViewDeclaration;

    /// A qualifier built while the statement runs names no schema, so the
    /// declaration reads as unusable rather than as an unqualified view.
    #[test]
    fn a_run_time_qualifier_leaves_no_declaration() {
        let mut statements =
            Parser::parse_sql(&PostgreSqlDialect {}, "CREATE VIEW app.v AS SELECT 1 AS one")
                .expect("the view parses");
        let Some(Statement::CreateView(mut node)) = statements.pop() else {
            panic!("expected CREATE VIEW");
        };

        assert_eq!(
            ViewDeclaration::from_node(&node).map(|declaration| declaration.schema().is_some()),
            Some(true)
        );

        node.name.0[0] = ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::new("IDENTIFIER"),
            args: Vec::new(),
        });
        assert!(ViewDeclaration::from_node(&node).is_none());
    }
}
