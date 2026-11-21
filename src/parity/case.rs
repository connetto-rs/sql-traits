//! A case's tables, callers and writes, and the statements that probe them.

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlparser::ast::DataType;

use crate::{
    errors::DeploymentError,
    structs::{Deployment, SessionSetting, SetPacking, SettingValue},
};

/// A caller a case measures, the role it acts as and the session settings it
/// sets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Caller {
    /// The name answers and known entries name the caller by.
    pub name: String,
    /// The role the caller acts as.
    pub role: String,
    /// The value of every session setting the deployment declares.
    pub settings: BTreeMap<String, String>,
}

/// A session setting a case's deployment declares.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// The setting's name, as policies read it with `current_setting`.
    pub name: String,
    /// What the setting holds.
    pub value: SettingValue,
}

/// A statement shape a case measures, each asked of `OpenFGA` as its own
/// action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// `SELECT` of the declared columns.
    Select,
    /// `SELECT ... FOR UPDATE` of the declared columns.
    SelectForUpdate,
    /// `UPDATE` of one row, chosen by its key.
    Update,
    /// `UPDATE` of every row, with no `WHERE`.
    UpdateWithoutWhere,
    /// `DELETE` of one row, chosen by its key.
    Delete,
    /// `INSERT` of one row.
    Insert,
}

impl Shape {
    /// Whether the shape reads rows without writing any.
    #[must_use]
    pub fn reads(self) -> bool {
        matches!(self, Self::Select | Self::SelectForUpdate)
    }
}

/// A command `PostgreSQL` grants on single columns that a probe needs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ColumnCommand {
    /// `SELECT`, needed on every column a statement reads.
    Select,
    /// `INSERT`, needed on every column an `INSERT` assigns.
    Insert,
    /// `UPDATE`, needed on every column an `UPDATE` sets.
    Update,
}

/// A write, or a locking read, a case measures beside `SELECT`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Write {
    /// The declared table the write targets.
    pub table: String,
    /// The statement shape.
    pub shape: Shape,
    /// Columns an update assigns, with the values it assigns.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    /// The row an insert writes.
    #[serde(default)]
    pub row: BTreeMap<String, Value>,
}

/// A measured table, the columns its row key spans and the columns a read
/// projects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    /// The table name.
    pub name: String,
    /// The columns whose values name a row.
    pub key: Vec<String>,
    /// The columns a read projects, the key among them.
    pub columns: Vec<String>,
}

/// A case's `callers.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The session settings the deployment declares, a text caller
    /// `app.user_id` and comma-packed text subjects `app.subjects` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Vec<Setting>>,
    /// The measured tables.
    pub tables: Vec<Table>,
    /// The callers, the first one's role being the login role.
    pub callers: Vec<Caller>,
    /// The writes and locking reads measured beside `SELECT`.
    #[serde(default)]
    pub writes: Vec<Write>,
}

/// One case, its schema, its seed and the callers, tables and writes it
/// measures.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// The case directory's name.
    pub name: String,
    /// `schema.sql`, which the table owner runs.
    pub schema: String,
    /// `seed.sql`, which the table owner runs after the schema.
    pub seed: String,
    /// `callers.toml`.
    pub config: Config,
}

/// Why a case's `callers.toml` is refused.
#[derive(Debug, thiserror::Error)]
pub enum CaseError {
    /// The file is not TOML of the expected shape.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// The case declares no caller.
    #[error("No callers")]
    NoCallers,
    /// The case declares no table.
    #[error("No tables")]
    NoTables,
    /// A table's projection leaves out a key column, or the key is empty.
    #[error("Projection of {0} must include the row key")]
    ProjectionWithoutKey(String),
    /// Two tables share a name.
    #[error("Table {0} is declared twice")]
    DuplicateTable(String),
    /// A caller sets other settings than the deployment declares.
    #[error("Caller {caller} must set every session setting the deployment names: {declared}")]
    Settings {
        /// The caller.
        caller: String,
        /// The declared settings, comma separated.
        declared: String,
    },
    /// A write targets a table the case does not declare.
    #[error("A write targets undeclared table {0}")]
    UndeclaredTable(String),
    /// A write repeats the `SELECT` every case measures.
    #[error("Every case already measures SELECT")]
    SelectWrite,
    /// An update without `set`, or another shape with one.
    #[error("{shape:?} {} `set`", if *required { "requires" } else { "takes no" })]
    Set {
        /// The write's shape.
        shape: Shape,
        /// Whether the shape needs `set`.
        required: bool,
    },
    /// An insert without `row`, or another shape with one.
    #[error("{shape:?} {} `row`", if *required { "requires" } else { "takes no" })]
    Row {
        /// The write's shape.
        shape: Shape,
        /// Whether the shape needs `row`.
        required: bool,
    },
    /// An inserted row leaves out a key column.
    #[error("An inserted row must carry the row key")]
    InsertWithoutKey,
    /// The deployment the callers describe is refused.
    #[error(transparent)]
    Deployment(#[from] DeploymentError),
}

/// A row lacking one of its table's key columns.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("A row of {table} has no key column {column}")]
pub struct MissingKeyColumn {
    /// The table.
    pub table: String,
    /// The missing key column.
    pub column: String,
}

impl Case {
    /// Read and validate one case from its files' contents.
    ///
    /// # Errors
    ///
    /// Returns a [`CaseError`] when `callers` is not a valid `callers.toml`.
    pub fn parse(
        name: impl Into<String>,
        schema: impl Into<String>,
        seed: impl Into<String>,
        callers: impl AsRef<str>,
    ) -> Result<Self, CaseError> {
        let config: Config = toml::from_str(callers.as_ref())?;
        config.validate()?;
        Ok(Self { name: name.into(), schema: schema.into(), seed: seed.into(), config })
    }

    /// The declared table named `name`.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.config.tables.iter().find(|table| table.name == name)
    }

    /// Every read shape this case measures on `table`, `SELECT` first.
    pub fn reads<'case>(&'case self, table: &'case Table) -> impl Iterator<Item = Shape> + 'case {
        core::iter::once(Shape::Select).chain(
            self.config
                .writes
                .iter()
                .filter(move |write| write.shape.reads() && write.table == table.name)
                .map(|write| write.shape),
        )
    }

    /// Every write probe this case measures.
    pub fn writes(&self) -> impl Iterator<Item = &Write> {
        self.config.writes.iter().filter(|write| !write.shape.reads())
    }
}

impl Config {
    /// Check the declarations agree with each other and with the deployment.
    fn validate(&self) -> Result<(), CaseError> {
        if self.callers.is_empty() {
            return Err(CaseError::NoCallers);
        }
        if self.tables.is_empty() {
            return Err(CaseError::NoTables);
        }
        for (index, table) in self.tables.iter().enumerate() {
            if table.key.is_empty() || !table.key.iter().all(|key| table.columns.contains(key)) {
                return Err(CaseError::ProjectionWithoutKey(table.name.clone()));
            }
            if self.tables[..index].iter().any(|earlier| earlier.name == table.name) {
                return Err(CaseError::DuplicateTable(table.name.clone()));
            }
        }
        let deployment = self.deployment()?;
        let declared: BTreeSet<&str> =
            deployment.settings().iter().map(SessionSetting::name).collect();
        for caller in &self.callers {
            if !caller.settings.keys().map(String::as_str).eq(declared.iter().copied()) {
                return Err(CaseError::Settings {
                    caller: caller.name.clone(),
                    declared: declared.iter().copied().collect::<Vec<_>>().join(", "),
                });
            }
        }
        for write in &self.writes {
            let table = self
                .tables
                .iter()
                .find(|table| table.name == write.table)
                .ok_or_else(|| CaseError::UndeclaredTable(write.table.clone()))?;
            if write.shape == Shape::Select {
                return Err(CaseError::SelectWrite);
            }
            let needs_set = matches!(write.shape, Shape::Update | Shape::UpdateWithoutWhere);
            let needs_row = write.shape == Shape::Insert;
            if write.set.is_empty() == needs_set {
                return Err(CaseError::Set { shape: write.shape, required: needs_set });
            }
            if write.row.is_empty() == needs_row {
                return Err(CaseError::Row { shape: write.shape, required: needs_row });
            }
            if needs_row && !table.key.iter().all(|key| write.row.contains_key(key)) {
                return Err(CaseError::InsertWithoutKey);
            }
        }
        Ok(())
    }

    /// Every acting role the callers name, in the order they first appear.
    #[must_use]
    pub fn acting_roles(&self) -> Vec<&str> {
        let mut roles: Vec<&str> = Vec::new();
        for caller in &self.callers {
            if !roles.contains(&caller.role.as_str()) {
                roles.push(&caller.role);
            }
        }
        roles
    }

    /// The deployment the translators read, declaring [`Self::settings`],
    /// logging in as the first caller's role and acting as every role a
    /// caller names.
    ///
    /// # Errors
    ///
    /// Returns a [`DeploymentError`] when no caller is declared or
    /// [`Deployment::new`] or [`SessionSetting::value`] refuses a declared
    /// setting.
    pub fn deployment(&self) -> Result<Deployment, DeploymentError> {
        let login = self.callers.first().ok_or(DeploymentError::NoActingRole)?;
        let settings = match &self.settings {
            Some(settings) => {
                settings
                    .iter()
                    .map(|setting| {
                        SessionSetting::value(setting.name.as_str(), setting.value.clone())
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => {
                vec![
                    SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))?,
                    SessionSetting::value(
                        "app.subjects",
                        SettingValue::Set {
                            element: DataType::Text,
                            packing: SetPacking::Delimited(",".into()),
                        },
                    )?,
                ]
            }
        };
        Deployment::new(login.role.as_str(), self.acting_roles(), settings)
    }
}

impl Table {
    /// Build the read measured under a read shape.
    #[must_use]
    pub fn statement(&self, shape: Shape) -> String {
        let read = format!(
            "SELECT {} FROM {}",
            self.columns.iter().map(|column| identifier(column)).collect::<Vec<_>>().join(", "),
            identifier(&self.name)
        );
        if shape == Shape::SelectForUpdate { format!("{read} FOR UPDATE") } else { read }
    }

    /// Build `write` as a caller runs it, on the key of `row` or, without one,
    /// as declared.
    #[must_use]
    pub fn write_statement(&self, write: &Write, row: Option<&Value>) -> String {
        self.write_sql(write, row, false)
    }

    /// Build `write` returning the row it produces, which the table owner runs
    /// to learn it.
    #[must_use]
    pub fn resulting_statement(&self, write: &Write, row: Option<&Value>) -> String {
        self.write_sql(write, row, true)
    }

    fn write_sql(&self, write: &Write, row: Option<&Value>, returning: bool) -> String {
        let table = identifier(&self.name);
        let alias = if returning { " AS t" } else { "" };
        let returning = if returning { " RETURNING to_jsonb(t) AS row" } else { "" };
        let filter = row.map_or_else(String::new, |row| {
            let matches: Vec<String> = self
                .key
                .iter()
                .map(|key| format!("{} = {}", identifier(key), literal(&row[key.as_str()])))
                .collect();
            format!(" WHERE {}", matches.join(" AND "))
        });
        match write.shape {
            Shape::Insert => {
                format!(
                    "INSERT INTO {table}{alias} ({}) VALUES ({}){returning}",
                    write
                        .row
                        .keys()
                        .map(|column| identifier(column))
                        .collect::<Vec<_>>()
                        .join(", "),
                    write.row.values().map(literal).collect::<Vec<_>>().join(", ")
                )
            }
            Shape::Delete => format!("DELETE FROM {table}{alias}{filter}{returning}"),
            _ => {
                format!(
                    "UPDATE {table}{alias} SET {}{filter}{returning}",
                    write
                        .set
                        .iter()
                        .map(|(column, value)| {
                            format!("{} = {}", identifier(column), literal(value))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }

    /// The columns a read touches under each column command.
    #[must_use]
    pub fn read_columns(&self) -> BTreeMap<ColumnCommand, Vec<String>> {
        BTreeMap::from([(ColumnCommand::Select, self.columns.clone())])
    }

    /// The text a row's key compares as, each key column as `CAST(column AS
    /// TEXT)` prints it, joined by commas, `column` looking up the row's value
    /// of a column.
    ///
    /// # Errors
    ///
    /// Returns [`MissingKeyColumn`] when `column` finds no value for a key
    /// column.
    pub fn row_key<'row>(
        &self,
        column: impl Fn(&str) -> Option<&'row Value>,
    ) -> Result<String, MissingKeyColumn> {
        let parts = self
            .key
            .iter()
            .map(|key| {
                column(key).map(key_text).ok_or_else(|| {
                    MissingKeyColumn { table: self.name.clone(), column: key.clone() }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(parts.join(","))
    }

    /// The SQL expression spelling a row's key as [`Self::row_key`] does,
    /// `column` spelling one key column's text.
    #[must_use]
    pub fn key_sql(&self, column: impl Fn(&str) -> String) -> String {
        self.key.iter().map(|key| column(key)).collect::<Vec<_>>().join(" || ',' || ")
    }
}

impl Write {
    /// The columns this write touches under each column command, `key` being
    /// the row key columns a keyed `WHERE` reads.
    #[must_use]
    pub fn columns(&self, key: &[String]) -> BTreeMap<ColumnCommand, Vec<String>> {
        let read = match self.shape {
            Shape::Update | Shape::Delete => key.to_vec(),
            _ => Vec::new(),
        };
        let (command, assigned) = match self.shape {
            Shape::Insert => (ColumnCommand::Insert, &self.row),
            _ => (ColumnCommand::Update, &self.set),
        };
        BTreeMap::from([
            (ColumnCommand::Select, read),
            (command, assigned.keys().cloned().collect()),
        ])
    }
}

/// The text a row key column compares as, the way `CAST(column AS TEXT)` prints
/// it.
fn key_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A column read through `qualifier` as text, the way PostgreSQL prints it.
#[must_use]
pub fn text_of(qualifier: &str, column: &str) -> String {
    format!("CAST({qualifier}.{} AS TEXT)", identifier(column))
}

/// Spell a declared or seeded value as a PostgreSQL literal.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("'{}'", text.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

/// Quote an SQL identifier supplied by a case.
#[must_use]
pub fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
