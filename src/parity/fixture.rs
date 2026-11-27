//! The embedded cases, PostgreSQL's recorded answers, the known discrepancies,
//! and the judgement of one engine's answers against them.

use alloc::{string::String, vec, vec::Vec};
use core::fmt;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{Answer, Case, CaseError, Direction, Engine, direction};

/// PostgreSQL's answer for every caller, row and statement of one case.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recorded {
    /// One answer per caller, row and statement.
    #[serde(default)]
    pub answer: Vec<RecordedAnswer>,
}

/// PostgreSQL's answer for one caller, row and statement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedAnswer {
    /// The caller's name.
    pub caller: String,
    /// The row, its table name and key joined by a colon.
    pub row: String,
    /// The statement the caller ran.
    pub statement: String,
    /// PostgreSQL's answer.
    pub postgres: Answer,
}

/// One case's known discrepancies, per engine.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseKnown {
    /// The SQLite replica's.
    #[serde(default, skip_serializing_if = "EngineKnown::is_empty")]
    pub sqlite: EngineKnown,
    /// The `OpenFGA` model's.
    #[serde(default, skip_serializing_if = "EngineKnown::is_empty")]
    pub openfga: EngineKnown,
}

impl CaseKnown {
    /// Whether neither engine has a known discrepancy.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sqlite.is_empty() && self.openfga.is_empty()
    }

    /// The known discrepancies of `engine`.
    #[must_use]
    pub fn engine(&self, engine: Engine) -> &EngineKnown {
        match engine {
            Engine::Sqlite => &self.sqlite,
            Engine::Openfga => &self.openfga,
        }
    }
}

/// One engine's known discrepancies in one case.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineKnown {
    /// The finding behind this engine's translator refusing the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// The rows this engine answers differently from PostgreSQL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row: Vec<KnownRow>,
}

impl EngineKnown {
    /// Whether the engine has no known discrepancy.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.refusal.is_none() && self.row.is_empty()
    }
}

/// A row on which one engine is known to answer differently from PostgreSQL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KnownRow {
    /// The caller's name.
    pub caller: String,
    /// The row, its table name and key joined by a colon.
    pub row: String,
    /// The statement the caller ran.
    pub statement: String,
    /// How the engine's answer differs.
    pub direction: Direction,
    /// The finding that explains the difference.
    pub finding: String,
}

/// An engine's answer for one caller, row and statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineAnswer {
    /// The caller's name.
    pub caller: String,
    /// The row, its table name and key joined by a colon.
    pub row: String,
    /// The statement the caller ran.
    pub statement: String,
    /// The engine's answer.
    pub answer: Answer,
}

/// One case with PostgreSQL's answers and its known discrepancies.
#[derive(Clone, Debug, PartialEq)]
pub struct Fixture {
    /// The case.
    pub case: Case,
    /// `postgres.toml`.
    pub recorded: Recorded,
    /// `known.toml`, empty when the case has none.
    pub known: CaseKnown,
}

/// Why an embedded case cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// `callers.toml` is invalid.
    #[error("callers.toml of {case} is invalid")]
    Case {
        /// The case.
        case: &'static str,
        /// The cause.
        #[source]
        source: CaseError,
    },
    /// `postgres.toml` or `known.toml` is invalid.
    #[error("{file} of {case} is invalid")]
    Toml {
        /// The case.
        case: &'static str,
        /// The file.
        file: &'static str,
        /// The cause.
        #[source]
        source: toml::de::Error,
    },
}

/// One departure of an engine from a fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discrepancy {
    /// The case.
    pub case: String,
    /// The engine judged.
    pub engine: Engine,
    /// What departed.
    pub kind: DiscrepancyKind,
}

/// How an engine departs from a fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscrepancyKind {
    /// The translator refuses the schema and no refusal is listed for the
    /// engine.
    UnlistedRefusal {
        /// The translator's reason.
        refusal: String,
    },
    /// A refusal is listed for the engine and the translator accepts the
    /// schema.
    StaleRefusal {
        /// The listed finding.
        finding: String,
    },
    /// The engine answers differently from PostgreSQL, in a direction no known
    /// row lists.
    Unlisted {
        /// The engine's answer.
        answer: EngineAnswer,
        /// PostgreSQL's answer.
        postgres: Answer,
        /// How the answers differ.
        direction: Direction,
    },
    /// A known row the engine no longer answers in its listed direction.
    Stale {
        /// The known row.
        known: KnownRow,
        /// The engine's answer for the row, if it gave one.
        answer: Option<Answer>,
    },
    /// A recorded answer the engine gave no answer for.
    Missing {
        /// PostgreSQL's recorded answer.
        recorded: RecordedAnswer,
    },
    /// An engine answer naming no recorded caller, row and statement, or a
    /// second answer for one.
    Unexpected {
        /// The engine's answer.
        answer: EngineAnswer,
    },
}

impl fmt::Display for Discrepancy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { case, engine, kind } = self;
        match kind {
            DiscrepancyKind::UnlistedRefusal { refusal } => {
                write!(formatter, "{case}: {engine} refuses the schema, unlisted: {refusal}")
            }
            DiscrepancyKind::StaleRefusal { finding } => {
                write!(
                    formatter,
                    "{case}: {engine} accepts the schema its listed refusal names: {finding}"
                )
            }
            DiscrepancyKind::Unlisted { answer, postgres, direction } => {
                write!(
                    formatter,
                    "{case}: {engine} answers {:?} where PostgreSQL answers {postgres:?}, \
                 {direction:?} and unlisted, for {} on {} by `{}`",
                    answer.answer, answer.caller, answer.row, answer.statement
                )
            }
            DiscrepancyKind::Stale { known, answer } => {
                write!(
                    formatter,
                    "{case}: {engine} no longer answers {:?} ({}) for {} on {} by `{}`, \
                 answering {answer:?}",
                    known.direction, known.finding, known.caller, known.row, known.statement
                )
            }
            DiscrepancyKind::Missing { recorded } => {
                write!(
                    formatter,
                    "{case}: {engine} gives no answer for {} on {} by `{}`, where PostgreSQL \
                 answers {:?}",
                    recorded.caller, recorded.row, recorded.statement, recorded.postgres
                )
            }
            DiscrepancyKind::Unexpected { answer } => {
                write!(
                    formatter,
                    "{case}: {engine} answers {:?} for {} on {} by `{}`, which PostgreSQL has \
                 no single recorded answer for",
                    answer.answer, answer.caller, answer.row, answer.statement
                )
            }
        }
    }
}

impl RecordedAnswer {
    fn probe(&self) -> (&str, &str, &str) {
        (&self.caller, &self.row, &self.statement)
    }
}

impl KnownRow {
    fn probe(&self) -> (&str, &str, &str) {
        (&self.caller, &self.row, &self.statement)
    }
}

impl EngineAnswer {
    fn probe(&self) -> (&str, &str, &str) {
        (&self.caller, &self.row, &self.statement)
    }
}

impl Fixture {
    /// Judge one engine's run of this case, `refusal` being its translator's
    /// reason for refusing the schema and `answers` one answer per recorded
    /// caller, row and statement.
    ///
    /// Reports a refusal unless one is listed for this engine, whatever the
    /// other engine does, a listed refusal the translator no longer makes, each
    /// answer differing from PostgreSQL's in a direction no known row lists,
    /// each known row no longer answered in its direction, each recorded answer
    /// left unanswered and each answer matching no recorded one.
    #[must_use]
    pub fn judge(
        &self,
        engine: Engine,
        refusal: Option<&str>,
        answers: &[EngineAnswer],
    ) -> Vec<Discrepancy> {
        let known = self.known.engine(engine);
        let mut kinds = Vec::new();
        match (refusal, &known.refusal) {
            (Some(refusal), None) => {
                kinds.push(DiscrepancyKind::UnlistedRefusal { refusal: refusal.into() });
            }
            (None, Some(finding)) => {
                kinds.push(DiscrepancyKind::StaleRefusal { finding: finding.clone() });
            }
            _ => {}
        }
        let mut matched = vec![false; answers.len()];
        let mut seen = vec![false; known.row.len()];
        for recorded in &self.recorded.answer {
            let Some(index) = (0..answers.len())
                .find(|&index| !matched[index] && answers[index].probe() == recorded.probe())
            else {
                kinds.push(DiscrepancyKind::Missing { recorded: recorded.clone() });
                continue;
            };
            matched[index] = true;
            let answer = &answers[index];
            let Some(direction) = direction(&recorded.postgres, &answer.answer) else {
                continue;
            };
            match known
                .row
                .iter()
                .position(|row| row.direction == direction && row.probe() == recorded.probe())
            {
                Some(position) => seen[position] = true,
                None => {
                    kinds.push(DiscrepancyKind::Unlisted {
                        answer: answer.clone(),
                        postgres: recorded.postgres.clone(),
                        direction,
                    });
                }
            }
        }
        kinds.extend(
            answers
                .iter()
                .zip(&matched)
                .filter(|(_, matched)| !**matched)
                .map(|(answer, _)| DiscrepancyKind::Unexpected { answer: answer.clone() }),
        );
        kinds.extend(known.row.iter().zip(seen).filter(|(_, seen)| !seen).map(|(row, _)| {
            DiscrepancyKind::Stale {
                known: row.clone(),
                answer: answers
                    .iter()
                    .find(|answer| answer.probe() == row.probe())
                    .map(|answer| answer.answer.clone()),
            }
        }));
        kinds
            .into_iter()
            .map(|kind| Discrepancy { case: self.case.name.clone(), engine, kind })
            .collect()
    }
}

/// One case's files, as embedded.
struct Embedded {
    name: &'static str,
    schema: &'static str,
    seed: &'static str,
    callers: &'static str,
    postgres: &'static str,
    known: Option<&'static str>,
}

impl Embedded {
    fn read(&self) -> Result<Fixture, FixtureError> {
        let case = self.name;
        Ok(Fixture {
            case: Case::parse(case, self.schema, self.seed, self.callers)
                .map_err(|source| FixtureError::Case { case, source })?,
            recorded: parse(case, "postgres.toml", self.postgres)?,
            known: self.known.map_or_else(
                || Ok(CaseKnown::default()),
                |known| parse(case, "known.toml", known),
            )?,
        })
    }
}

/// Read `file` of `case`, whose contents are `text`.
fn parse<T: DeserializeOwned>(
    case: &'static str,
    file: &'static str,
    text: &str,
) -> Result<T, FixtureError> {
    toml::from_str(text).map_err(|source| FixtureError::Toml { case, file, source })
}

/// Embed each named case directory, `known` marking one that holds
/// `known.toml`.
macro_rules! embedded {
    ($($name:literal $($known:ident)?),* $(,)?) => {
        [$(Embedded {
            name: $name,
            schema: embedded!(@file $name "schema.sql"),
            seed: embedded!(@file $name "seed.sql"),
            callers: embedded!(@file $name "callers.toml"),
            postgres: embedded!(@file $name "postgres.toml"),
            known: embedded!(@known $name $($known)?),
        }),*]
    };
    (@known $name:literal known) => {
        Some(embedded!(@file $name "known.toml"))
    };
    (@known $name:literal) => {
        None
    };
    (@file $name:literal $file:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/parity-fixtures/", $name, "/", $file))
    };
}

static EMBEDDED: &[Embedded] = &embedded![
    "bare_boolean_column",
    "blind_update" known,
    "blind_update_check_abort",
    "column_grant_body",
    "column_grant_id",
    "column_grant_write" known,
    "composite_key",
    "current_user_policy",
    "delete_own_rows",
    "grant_without_update" known,
    "insert_only_grant" known,
    "jsonb_numeric_owner",
    "membership_under_rls",
    "owner_rights_view",
    "partitioned" known,
    "policy_for_role_without_privilege",
    "restrictive_to_role",
    "select_for_update",
    "self_referential" known,
    "separator_mismatch" known,
    "to_role_member",
    "to_role_member_p2s_role",
    "to_role_noinherit_p2s_role",
    "two_acting_roles" known,
    "update_into_invisibility_apart",
    "update_into_invisibility_mirrored",
    "uuid_owner_blob",
];

/// Every case of `parity-fixtures/`, in name order, read and validated.
pub fn fixtures() -> impl Iterator<Item = Result<Fixture, FixtureError>> {
    EMBEDDED.iter().map(Embedded::read)
}
