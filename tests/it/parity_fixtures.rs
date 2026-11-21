//! rls-parity's recorded cases, shipped so each translator's CI replays them.
//!
//! Every embedded case must read and validate, the embedded list must be the
//! directory, and the judgement of one engine must report exactly the
//! discrepancies that `postgres.toml` and `known.toml` do not account for.

#![cfg(feature = "parity-fixtures")]
#![allow(clippy::expect_used, clippy::panic)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde_json::json;
use sql_traits::{
    parity::{
        Answer, Case, CaseKnown, ColumnCommand, Direction, DiscrepancyKind, Engine, EngineAnswer,
        EngineKnown, Fixture, KnownRow, Recorded, RecordedAnswer, Shape, Write, direction,
        fixtures, text_of,
    },
    prelude::{SetPacking, SettingContent, SettingValue},
};
use sqlparser::ast::DataType;

/// Every embedded case, read and validated.
fn all() -> Vec<Fixture> {
    fixtures().map(|fixture| fixture.expect("every embedded case reads")).collect()
}

/// The embedded case named `name`.
fn named(name: &str) -> Fixture {
    all().into_iter().find(|fixture| fixture.case.name == name).expect("the case is embedded")
}

/// An answer that differs from `postgres` in `direction`.
fn diverging(postgres: &Answer, direction: Direction) -> Answer {
    match (direction, postgres) {
        (Direction::Narrower, Answer::Visible) => Answer::Hidden,
        (Direction::Narrower, Answer::Changed)
        | (Direction::DifferentDenial, Answer::Rejected(_)) => Answer::Skipped,
        (Direction::Wider, Answer::Hidden) => Answer::Visible,
        (Direction::Wider, Answer::Skipped | Answer::Rejected(_)) => Answer::Changed,
        (Direction::DifferentDenial, Answer::Skipped) => Answer::Rejected("42501".into()),
        (Direction::Refused, _) => Answer::Refused("refused by the translator".into()),
        (Direction::Error, _) => Answer::Error("failed on the engine".into()),
        _ => panic!("no answer differs from {postgres:?} in {direction:?}"),
    }
}

/// The answers of an engine behaving exactly as the fixture records it.
fn as_recorded(fixture: &Fixture, engine: Engine) -> Vec<EngineAnswer> {
    let known = fixture.known.engine(engine);
    fixture
        .recorded
        .answer
        .iter()
        .map(|recorded| {
            let answer = known
                .row
                .iter()
                .find(|row| {
                    row.caller == recorded.caller
                        && row.row == recorded.row
                        && row.statement == recorded.statement
                })
                .map_or_else(
                    || recorded.postgres.clone(),
                    |row| diverging(&recorded.postgres, row.direction),
                );
            EngineAnswer {
                caller: recorded.caller.clone(),
                row: recorded.row.clone(),
                statement: recorded.statement.clone(),
                answer,
            }
        })
        .collect()
}

/// The embedded list is the directory, and a case carries known entries
/// exactly when its directory holds `known.toml`.
#[test]
fn fixture_list_matches_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("parity-fixtures");
    let mut directories: Vec<(String, bool)> = fs::read_dir(&root)
        .expect("the fixture directory exists")
        .map(|entry| {
            let path = entry.expect("the directory lists").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("case names are UTF-8")
                .to_owned();
            (name, path.join("known.toml").exists())
        })
        .collect();
    directories.sort();
    let mut embedded: Vec<(String, bool)> =
        all().into_iter().map(|fixture| (fixture.case.name, !fixture.known.is_empty())).collect();
    embedded.sort();
    assert_eq!(embedded, directories);
}

/// Each recorded answer and known row names a caller and a table its case
/// declares, and each known row names a probe PostgreSQL answered.
#[test]
fn answers_name_declared_callers_and_rows() {
    for fixture in all() {
        let callers: BTreeSet<&str> =
            fixture.case.config.callers.iter().map(|caller| caller.name.as_str()).collect();
        let declared = |caller: &str, row: &str| {
            let (table, key) = row.split_once(':').expect("a row is `table:key`");
            callers.contains(caller) && !key.is_empty() && fixture.case.table(table).is_some()
        };
        assert!(!fixture.recorded.answer.is_empty(), "{}", fixture.case.name);
        for recorded in &fixture.recorded.answer {
            assert!(
                declared(&recorded.caller, &recorded.row),
                "{}: {recorded:?}",
                fixture.case.name
            );
        }
        for engine in [Engine::Sqlite, Engine::Openfga] {
            for known in &fixture.known.engine(engine).row {
                assert!(
                    fixture.recorded.answer.iter().any(|recorded| {
                        recorded.caller == known.caller
                            && recorded.row == known.row
                            && recorded.statement == known.statement
                    }),
                    "{}: {known:?}",
                    fixture.case.name
                );
            }
        }
    }
}

/// An engine answering as `postgres.toml` and `known.toml` say, refusing a
/// schema exactly where a refusal is listed, has no discrepancy.
#[test]
fn recorded_behaviour_is_judged_clean() {
    for fixture in all() {
        for engine in [Engine::Sqlite, Engine::Openfga] {
            let refusal = fixture.known.engine(engine).refusal.as_deref();
            let discrepancies = fixture.judge(engine, refusal, &as_recorded(&fixture, engine));
            assert_eq!(discrepancies, Vec::new(), "{} {engine}", fixture.case.name);
        }
    }
}

/// Each way an engine can depart from the fixture is reported once, naming
/// the case, caller, row and statement.
#[test]
fn judgement_reports_each_discrepancy() {
    let fixture = named("column_grant_write");
    let mut answers = as_recorded(&fixture, Engine::Sqlite);
    let known = &fixture.known.sqlite.row[0];
    let stale = answers
        .iter()
        .position(|answer| {
            answer.caller == known.caller
                && answer.row == known.row
                && answer.statement == known.statement
        })
        .expect("the known row is answered");
    let stale_postgres = fixture.recorded.answer[stale].postgres.clone();
    answers[stale].answer = stale_postgres;
    let unlisted = answers
        .iter()
        .position(|answer| answer.answer == Answer::Visible)
        .expect("some row is visible");
    answers[unlisted].answer = Answer::Hidden;
    let missing = answers.remove(answers.len() - 1);
    answers.push(EngineAnswer {
        caller: "carol".into(),
        row: "docs:1".into(),
        statement: "SELECT \"id\" FROM \"docs\"".into(),
        answer: Answer::Visible,
    });

    let discrepancies = fixture.judge(Engine::Sqlite, Some("no partitions"), &answers);
    let kinds: Vec<&DiscrepancyKind> =
        discrepancies.iter().map(|discrepancy| &discrepancy.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &DiscrepancyKind::UnlistedRefusal { refusal: "no partitions".into() },
            &DiscrepancyKind::Unlisted {
                answer: answers[unlisted].clone(),
                postgres: Answer::Visible,
                direction: Direction::Narrower,
            },
            &DiscrepancyKind::Missing {
                recorded: fixture.recorded.answer[fixture.recorded.answer.len() - 1].clone(),
            },
            &DiscrepancyKind::Unexpected { answer: answers[answers.len() - 1].clone() },
            &DiscrepancyKind::Stale {
                known: known.clone(),
                answer: Some(fixture.recorded.answer[stale].postgres.clone()),
            },
        ]
    );
    for discrepancy in &discrepancies {
        assert_eq!(discrepancy.case, "column_grant_write");
        assert_eq!(discrepancy.engine, Engine::Sqlite);
        let text = discrepancy.to_string();
        assert!(text.contains("column_grant_write"), "{text}");
        assert!(text.contains("sqlite"), "{text}");
    }
    for (index, probe) in [(1, &answers[unlisted]), (3, &answers[answers.len() - 1])] {
        let text = discrepancies[index].to_string();
        for part in [&probe.caller, &probe.row, &probe.statement] {
            assert!(text.contains(part.as_str()), "{text} lacks {part}");
        }
    }
    for (index, part) in
        [(2, &missing.statement), (2, &missing.caller), (4, &known.statement), (4, &known.finding)]
    {
        let text = discrepancies[index].to_string();
        assert!(text.contains(part.as_str()), "{text} lacks {part}");
    }

    let partitioned = named("partitioned");
    let discrepancies =
        partitioned.judge(Engine::Sqlite, None, &as_recorded(&partitioned, Engine::Sqlite));
    assert_eq!(
        discrepancies.iter().map(|discrepancy| &discrepancy.kind).collect::<Vec<_>>(),
        vec![&DiscrepancyKind::StaleRefusal {
            finding: partitioned.known.sqlite.refusal.clone().expect("SQLite's refusal is listed"),
        }]
    );
    assert!(discrepancies[0].to_string().contains("partitioned"));
}

/// A known divergence must still disagree in its declared direction, and an
/// answer carrying PostgreSQL's own error agrees with it.
#[test]
fn known_entries_must_be_observed() {
    let probe = |caller: &str, postgres: Answer| {
        RecordedAnswer {
            caller: caller.into(),
            row: "docs:1".into(),
            statement: "SELECT id FROM docs".into(),
            postgres,
        }
    };
    let answer = |caller: &str, answer: Answer| {
        EngineAnswer {
            caller: caller.into(),
            row: "docs:1".into(),
            statement: "SELECT id FROM docs".into(),
            answer,
        }
    };
    let mut fixture = small_fixture();
    fixture.recorded.answer = vec![
        probe("alice", Answer::Visible),
        probe("bob", Answer::Error("permission denied for table docs".into())),
    ];
    fixture.known.sqlite.row = vec![KnownRow {
        caller: "alice".into(),
        row: "docs:1".into(),
        statement: "SELECT id FROM docs".into(),
        direction: Direction::Narrower,
        finding: "test".into(),
    }];
    let judged = |alice: Answer, bob: Answer| {
        fixture
            .judge(Engine::Sqlite, None, &[answer("alice", alice), answer("bob", bob)])
            .into_iter()
            .map(|discrepancy| discrepancy.kind)
            .collect::<Vec<_>>()
    };
    let denied = || Answer::Refused("permission denied for table docs".into());
    assert_eq!(judged(Answer::Hidden, denied()), Vec::new());
    let errors = judged(Answer::Visible, Answer::Error("no such table: docs".into()));
    assert!(
        matches!(
            errors.as_slice(),
            [
                DiscrepancyKind::Unlisted { direction: Direction::Error, .. },
                DiscrepancyKind::Stale { .. }
            ]
        ),
        "{errors:?}"
    );
    let errors = judged(Answer::Refused("unsupported".into()), denied());
    assert!(
        matches!(
            errors.as_slice(),
            [
                DiscrepancyKind::Unlisted { direction: Direction::Refused, .. },
                DiscrepancyKind::Stale { .. }
            ]
        ),
        "{errors:?}"
    );
}

/// A one-table case with no recorded answer and no known entry.
fn small_fixture() -> Fixture {
    Fixture {
        case: Case::parse(
            "fixture",
            "CREATE TABLE docs (id INT PRIMARY KEY);",
            "INSERT INTO docs VALUES (1);",
            include_str!("../../parity-fixtures/grant_without_update/callers.toml"),
        )
        .expect("the case reads"),
        recorded: Recorded::default(),
        known: CaseKnown::default(),
    }
}

/// A schema a translator refuses fails unless listed for that engine, whatever
/// the other translator answers, and a listed refusal fails once that
/// translator accepts.
#[test]
fn refusal_parity() {
    let mut fixture = small_fixture();
    fixture.known.sqlite = EngineKnown { refusal: Some("test".into()), row: Vec::new() };
    let judged = |engine: Engine, refusal: Option<&str>| {
        fixture
            .judge(engine, refusal, &[])
            .into_iter()
            .map(|discrepancy| discrepancy.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(judged(Engine::Sqlite, Some("partitioned")), Vec::new());
    assert_eq!(
        judged(Engine::Openfga, Some("diverges")),
        vec![DiscrepancyKind::UnlistedRefusal { refusal: "diverges".into() }]
    );
    assert_eq!(
        judged(Engine::Sqlite, None),
        vec![DiscrepancyKind::StaleRefusal { finding: "test".into() }]
    );
    assert_eq!(judged(Engine::Openfga, None), Vec::new());
    fixture.known.sqlite = EngineKnown::default();
    let unlisted = fixture.judge(Engine::Sqlite, Some("partitioned"), &[]);
    assert_eq!(unlisted.len(), 1);
    let text = unlisted[0].to_string();
    assert!(text.contains("partitioned") && text.contains("sqlite"), "{text}");
    let text = fixture.judge(Engine::Openfga, Some("diverges"), &[])[0].to_string();
    assert!(text.contains("diverges") && text.contains("openfga"), "{text}");
}

/// Visibility, write outcomes, translation refusals and execution failures
/// have distinct directions, and an answer carrying PostgreSQL's own error
/// agrees with it.
#[test]
fn answer_directions() {
    for (oracle, actual, expected) in [
        (Answer::Hidden, Answer::Visible, Some(Direction::Wider)),
        (Answer::Visible, Answer::Hidden, Some(Direction::Narrower)),
        (Answer::Visible, Answer::Refused("unsupported".into()), Some(Direction::Refused)),
        (Answer::Visible, Answer::Error("failed".into()), Some(Direction::Error)),
        (
            Answer::Error("permission denied for table docs".into()),
            Answer::Hidden,
            Some(Direction::Error),
        ),
        (Answer::Hidden, Answer::Hidden, None),
        (Answer::Skipped, Answer::Denied, None),
        (Answer::Rejected("42501".into()), Answer::Denied, None),
        (Answer::Rejected("42501".into()), Answer::Rejected("42501".into()), None),
        (Answer::Changed, Answer::Denied, Some(Direction::Narrower)),
        (Answer::Changed, Answer::Skipped, Some(Direction::Narrower)),
        (Answer::Skipped, Answer::Changed, Some(Direction::Wider)),
        (Answer::Rejected("42501".into()), Answer::Changed, Some(Direction::Wider)),
        (Answer::Rejected("42501".into()), Answer::Skipped, Some(Direction::DifferentDenial)),
        (
            Answer::Rejected("42501".into()),
            Answer::Rejected("23505".into()),
            Some(Direction::DifferentDenial),
        ),
        (Answer::Visible, Answer::Changed, Some(Direction::Error)),
        (
            Answer::Error("permission denied for table docs".into()),
            Answer::Refused("permission denied for table docs".into()),
            None,
        ),
        (
            Answer::Error("permission denied for table docs".into()),
            Answer::Error("permission denied for table docs".into()),
            None,
        ),
        (
            Answer::Error("permission denied for table docs".into()),
            Answer::Refused("unsupported".into()),
            Some(Direction::Refused),
        ),
        (
            Answer::Error("permission denied for table docs".into()),
            Answer::Error("no such table: docs".into()),
            Some(Direction::Error),
        ),
    ] {
        assert_eq!(direction(&oracle, &actual), expected, "{oracle:?} {actual:?}");
    }
}

/// Invalid case declarations fail before contacting an engine.
#[test]
fn invalid_case_configuration() {
    let valid = include_str!("../../parity-fixtures/grant_without_update/callers.toml");
    for (config, expected) in [
        ("tables=[]\ncallers=[]".to_owned(), "No callers"),
        (format!("tables = []\n{}", &valid[valid.find("[[callers]]").unwrap_or(0)..]), "No tables"),
        (valid.replacen("[[tables]]", "[[unused]]", 1), "unknown field"),
        (valid.replace("columns = [\"id\"]", "columns = []"), "Projection of docs must include"),
        (
            format!("{valid}\n[[tables]]\nname = \"docs\"\nkey = [\"id\"]\ncolumns = [\"id\"]\n"),
            "declared twice",
        ),
        (
            format!("{valid}\n[[writes]]\ntable = \"other\"\nshape = \"delete\"\n"),
            "undeclared table other",
        ),
        (valid.replace(", \"app.subjects\" = \"\"", ""), "every session setting"),
        (valid.replace("\"app.subjects\"", "\"app.other\""), "every session setting"),
        (format!("unknown = true\n{valid}"), "unknown field"),
        (format!("source = \"connetto\"\n{valid}"), "unknown field"),
        (
            format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"select\"\n"),
            "already measures SELECT",
        ),
        (format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"update\"\n"), "requires `set`"),
        (
            format!(
                "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"delete\"\nset = {{ body = \"x\" }}\n"
            ),
            "takes no `set`",
        ),
        (format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"insert\"\n"), "requires `row`"),
        (
            format!(
                "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"delete\"\nrow = {{ id = 9 }}\n"
            ),
            "takes no `row`",
        ),
        (
            format!(
                "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"insert\"\nrow = {{ owner = \"alice\" }}\n"
            ),
            "must carry the row key",
        ),
        (
            format!("[[settings]]\nname = \"app.user_id\"\nvalue = {{ caller = \"1\" }}\n{valid}"),
            "\"1\" is not a PostgreSQL type",
        ),
        (
            format!(
                "[[settings]]\nname = \"app.user_id\"\nvalue = {{ owner = \"uuid\" }}\n{valid}"
            ),
            "unknown variant",
        ),
        (
            format!(
                "[[settings]]\nname = \"app.user_id\"\nvalue = {{ caller = \"uuid\" }}\nstale = \
                 true\n{valid}"
            ),
            "unknown field",
        ),
        (
            format!(
                "[[settings]]\nname = \"app.user_id\"\nvalue = {{ caller = \"uuid\" }}\n{valid}"
            ),
            "every session setting",
        ),
        (
            format!(
                "[[settings]]\nname = \"app.user_id\"\nvalue = {{ caller = \"uuid\" }}\n\
                 [[settings]]\nname = \"app.subjects\"\nvalue = {{ set = {{ element = \"text\", \
                 packing = {{ delimited = \"\" }} }} }}\n{valid}"
            ),
            "splits a set on an empty separator",
        ),
        (
            format!(
                "[[settings]]\nname = \"app.user_id\"\nvalue = {{ caller = \"uuid\" }}\n\
                 [[settings]]\nname = \"app.subjects\"\nvalue = {{ principal = {{ kind = \"\", \
                 identity = \"uuid\" }} }}\n{valid}"
            ),
            "principal of no type",
        ),
    ] {
        let error =
            Case::parse("fixture", "", "", config).expect_err("the configuration is invalid");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

const DECLARED_SETTINGS: &str = r#"
[[settings]]
name = "app.user_id"
value = { caller = "uuid" }

[[settings]]
name = "app.installation"
value = { principal = { kind = "installation", identity = "bigint" } }

[[settings]]
name = "app.tenant"
value = { scalar = "integer" }

[[settings]]
name = "app.subjects"
value = { set = { element = "uuid", packing = "json_array" } }

[[settings]]
name = "app.teams"
value = { set = { element = "text", packing = { delimited = ";" } } }

[[tables]]
name = "docs"
key = ["id"]
columns = ["id"]

[[callers]]
name = "a"
role = "app_reader"
settings = { "app.user_id" = "00000000-0000-0000-0000-00000000000a", "app.installation" = "7", "app.tenant" = "1", "app.subjects" = "[]", "app.teams" = "red;blue" }
"#;

/// A case's deployment declares each session setting as `callers.toml` types
/// and packs it, the declarations survive the round trip rls-parity records
/// cases through, and a case declaring none keeps a text caller and
/// comma-packed text subjects.
#[test]
fn declared_settings() {
    let case = Case::parse("fixture", "", "", DECLARED_SETTINGS).expect("the case reads");
    let deployment = case.config.deployment().expect("the deployment is valid");
    let value = SettingContent::Value;
    assert_eq!(
        deployment
            .settings()
            .iter()
            .map(|setting| (setting.name(), setting.content()))
            .collect::<Vec<_>>(),
        [
            ("app.user_id", &value(SettingValue::Caller(DataType::Uuid))),
            (
                "app.installation",
                &value(SettingValue::Principal {
                    kind: "installation".into(),
                    identity: DataType::BigInt(None),
                }),
            ),
            ("app.tenant", &value(SettingValue::Scalar(DataType::Integer(None)))),
            (
                "app.subjects",
                &value(SettingValue::Set {
                    element: DataType::Uuid,
                    packing: SetPacking::JsonArray
                }),
            ),
            (
                "app.teams",
                &value(SettingValue::Set {
                    element: DataType::Text,
                    packing: SetPacking::Delimited(";".into()),
                }),
            ),
        ]
    );

    let recorded = toml::to_string(&case.config).expect("the case serializes");
    let reread = Case::parse("fixture", "", "", &recorded).expect("the recorded case reads");
    assert_eq!(reread.config, case.config);

    let legacy = small_fixture().case.config;
    assert!(!toml::to_string(&legacy).expect("the case serializes").contains("settings ="));
    let legacy = legacy.deployment().expect("the deployment is valid");
    assert_eq!(
        legacy
            .settings()
            .iter()
            .map(|setting| (setting.name(), setting.content()))
            .collect::<Vec<_>>(),
        [
            ("app.user_id", &value(SettingValue::Caller(DataType::Text))),
            (
                "app.subjects",
                &value(SettingValue::Set {
                    element: DataType::Text,
                    packing: SetPacking::Delimited(",".into()),
                }),
            ),
        ]
    );
}

/// SQL identifiers escape embedded quotes in the projection and table.
#[test]
fn quoted_projection() {
    let mut case = named("grant_without_update").case;
    let table = &mut case.config.tables[0];
    table.name = "odd\"table".into();
    table.columns = vec!["id".into(), "odd\"column".into()];
    assert_eq!(
        table.statement(Shape::Select),
        "SELECT \"id\", \"odd\"\"column\" FROM \"odd\"\"table\""
    );
    assert_eq!(
        table.statement(Shape::SelectForUpdate),
        "SELECT \"id\", \"odd\"\"column\" FROM \"odd\"\"table\" FOR UPDATE"
    );
}

/// Write values reach PostgreSQL as literals of their JSON type, quotes
/// escaped.
#[test]
fn write_literals() {
    let case = named("grant_without_update").case;
    let write: Write = serde_json::from_value(json!({
        "table": "docs",
        "shape": "update",
        "set": { "a": null, "b": true, "c": "it's", "d": ["x'y"] },
    }))
    .expect("the write reads");
    assert_eq!(
        case.table("docs")
            .expect("docs is declared")
            .write_statement(&write, Some(&json!({ "id": "o'k", "body": 1 }))),
        "UPDATE \"docs\" SET \"a\" = NULL, \"b\" = true, \"c\" = 'it''s', \"d\" = '[\"x''y\"]' \
         WHERE \"id\" = 'o''k'"
    );
}

/// A composite key filters a write on every key column, names a row by all of
/// them, and refuses a row missing one.
#[test]
fn composite_key() {
    let case = named("composite_key").case;
    let table = case.table("lines").expect("lines is declared");
    let delete: Write = serde_json::from_value(json!({
        "table": "lines",
        "shape": "delete",
    }))
    .expect("the write reads");
    let row = json!({ "order_id": 1, "line_no": "2", "owner": "bob" });
    assert_eq!(
        table.write_statement(&delete, Some(&row)),
        "DELETE FROM \"lines\" WHERE \"order_id\" = 1 AND \"line_no\" = '2'"
    );
    assert_eq!(table.row_key(|column| row.get(column)).expect("the row is keyed"), "1,2");
    assert_eq!(
        table.key_sql(|key| text_of("q", key)),
        "CAST(q.\"order_id\" AS TEXT) || ',' || CAST(q.\"line_no\" AS TEXT)"
    );
    let error = table
        .row_key(|column| row.get(column).filter(|_| column == "order_id"))
        .expect_err("a row without line_no has no key");
    assert!(error.to_string().contains("no key column line_no"), "{error}");
}

/// A locking read is measured as a read of its table, after `SELECT`, and
/// never as a write probe.
#[test]
fn locking_reads_are_reads() {
    let case = named("select_for_update").case;
    let table = case.table("docs").expect("docs is declared");
    assert_eq!(case.reads(table).collect::<Vec<_>>(), vec![Shape::Select, Shape::SelectForUpdate]);
    assert_eq!(case.writes().count(), 0);
    let composite = named("composite_key").case;
    let lines = composite.table("lines").expect("lines is declared");
    assert_eq!(composite.reads(lines).collect::<Vec<_>>(), vec![Shape::Select]);
    assert_eq!(
        composite.writes().map(|write| write.shape).collect::<Vec<_>>(),
        vec![Shape::Update, Shape::Delete, Shape::Insert]
    );
}

/// Each write is spelled as the caller runs it and as the table owner runs it
/// to learn the row it produces, and names the columns each command touches.
#[test]
fn write_probes() {
    let case = named("composite_key").case;
    let table = case.table("lines").expect("lines is declared");
    let [update, delete, insert] = case.config.writes.as_slice() else {
        panic!("composite_key declares an update, a delete and an insert");
    };
    let row = json!({ "order_id": 1, "line_no": 2, "owner": "bob" });
    let key = || vec!["order_id".to_owned(), "line_no".to_owned()];
    assert_eq!(
        table.write_statement(insert, None),
        "INSERT INTO \"lines\" (\"line_no\", \"order_id\", \"owner\", \"quantity\") \
         VALUES (3, 1, 'alice', 1)"
    );
    assert_eq!(
        table.resulting_statement(insert, None),
        "INSERT INTO \"lines\" AS t (\"line_no\", \"order_id\", \"owner\", \"quantity\") \
         VALUES (3, 1, 'alice', 1) RETURNING to_jsonb(t) AS row"
    );
    assert_eq!(
        table.resulting_statement(update, Some(&row)),
        "UPDATE \"lines\" AS t SET \"quantity\" = 2 WHERE \"order_id\" = 1 AND \"line_no\" = 2 \
         RETURNING to_jsonb(t) AS row"
    );
    assert_eq!(
        table.read_columns(),
        BTreeMap::from([(
            ColumnCommand::Select,
            vec!["order_id".to_owned(), "line_no".to_owned(), "owner".to_owned()]
        )])
    );
    assert_eq!(
        update.columns(&table.key),
        BTreeMap::from([
            (ColumnCommand::Select, key()),
            (ColumnCommand::Update, vec!["quantity".to_owned()]),
        ])
    );
    assert_eq!(
        delete.columns(&table.key),
        BTreeMap::from([(ColumnCommand::Select, key()), (ColumnCommand::Update, Vec::new())])
    );
    assert_eq!(
        insert.columns(&table.key),
        BTreeMap::from([
            (ColumnCommand::Select, Vec::new()),
            (
                ColumnCommand::Insert,
                vec![
                    "line_no".to_owned(),
                    "order_id".to_owned(),
                    "owner".to_owned(),
                    "quantity".to_owned()
                ]
            ),
        ])
    );
}
