//! Tests that an operator invocation resolves to the `pg_operator` overload
//! PostgreSQL 18 selects and names the type each operand takes. Every
//! expected selection was read back from a PostgreSQL 18 server.
#![expect(clippy::unwrap_used, reason = "Fixtures must parse.")]

use sql_traits::prelude::*;
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// An untyped literal operand.
const LITERAL: OperandType<'static> = OperandType::UntypedLiteral;

fn known(name: &str) -> OperandType<'_> {
    OperandType::Known { type_name: TargetName::new(name, false), typmod: None }
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

/// The resolution of `operator` over the operands, spelled as the selected
/// operator's signature and each operand's target, a polymorphic binding
/// marked with `*`.
fn resolved(
    db: &ParserDB,
    operator: TargetName<'_>,
    left: Option<OperandType<'_>>,
    right: OperandType<'_>,
) -> String {
    let signature = |operator: &PostgresCatalogOperator| {
        let left = operator.left_type(db).map(|left| left.to_string()).unwrap_or_default();
        let right = operator.right_type(db).unwrap().to_string();
        format!("{}({left},{right})", operator.target_name())
    };
    let target = |evidence: &OperandEvidence<'_, ParserDB>| {
        let marker = if evidence.is_polymorphic_binding() { "*" } else { "" };
        format!("{}{marker}", evidence.target())
    };
    match db.resolve_operator(operator, left, right, IdentifierCase::AsWritten) {
        OperatorResolution::Selected(selected) => {
            let left = selected.left().map(target).unwrap_or_default();
            format!("{} as {left},{}", signature(selected.operator()), target(selected.right()))
        }
        OperatorResolution::NotFound => "not found".to_owned(),
        OperatorResolution::Ambiguous(operators) => {
            let mut signatures: Vec<String> = operators.into_iter().map(signature).collect();
            signatures.sort();
            format!("ambiguous {}", signatures.join(" "))
        }
        OperatorResolution::UnboundPolymorphic(operator) => {
            format!("unbound {}", signature(operator))
        }
        OperatorResolution::Undetermined => "undetermined".to_owned(),
    }
}

fn op(name: &str) -> TargetName<'_> {
    TargetName::new(name, false)
}

/// The built-in selections a PostgreSQL 18 server stores for the same
/// expressions.
#[test]
fn built_in_operators_resolve_as_postgres_selects_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN dtext AS TEXT;").unwrap();
    let cases: [(&str, Option<OperandType<'_>>, OperandType<'_>, &str); 9] = [
        (
            ">",
            Some(known("int4")),
            known("int8"),
            "pg_catalog.>(pg_catalog.int4,pg_catalog.int8) as pg_catalog.int4,pg_catalog.int8",
        ),
        (
            ">",
            Some(known("integer")),
            known("numeric"),
            "pg_catalog.>(pg_catalog.numeric,pg_catalog.numeric) as \
             pg_catalog.numeric,pg_catalog.numeric",
        ),
        (
            "=",
            Some(known("text")),
            LITERAL,
            "pg_catalog.=(pg_catalog.text,pg_catalog.text) as pg_catalog.text,pg_catalog.text",
        ),
        (
            "=",
            Some(known("varchar")),
            known("text"),
            "pg_catalog.=(pg_catalog.text,pg_catalog.text) as pg_catalog.text,pg_catalog.text",
        ),
        // A domain operand meeting an untyped literal reads as its base type.
        (
            "=",
            Some(known("dtext")),
            LITERAL,
            "pg_catalog.=(pg_catalog.text,pg_catalog.text) as pg_catalog.text,pg_catalog.text",
        ),
        (
            "<@",
            Some(known("int4[]")),
            LITERAL,
            "pg_catalog.<@(pg_catalog.anyarray,pg_catalog.anyarray) as \
             pg_catalog._int4*,pg_catalog._int4*",
        ),
        (
            "||",
            Some(known("_int4")),
            known("int8"),
            "pg_catalog.||(pg_catalog.anycompatiblearray,pg_catalog.anycompatible) as \
             pg_catalog._int8*,pg_catalog.int8*",
        ),
        (
            "=",
            Some(LITERAL),
            LITERAL,
            "pg_catalog.=(pg_catalog.text,pg_catalog.text) as pg_catalog.text,pg_catalog.text",
        ),
        ("~~", Some(known("int4")), known("int4"), "not found"),
    ];
    for (name, left, right, expected) in cases {
        assert_eq!(resolved(&db, op(name), left, right), expected, "{name}");
    }
    let prefix = resolved(&db, op("~"), None, LITERAL);
    assert!(prefix.starts_with("ambiguous "), "{prefix}");
    assert!(prefix.contains("pg_catalog.~(,pg_catalog.int4)"), "{prefix}");
}

/// A range operand reaches the range pseudo-types, but which range binds
/// needs `pg_range` facts, so the invocation stays open where PostgreSQL 18
/// selects `@>(anyrange,anyelement)`.
#[test]
fn range_operands_stay_open() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    assert_eq!(resolved(&db, op("@>"), Some(known("int4range")), known("int4")), "undetermined");
}

/// User operators over the polymorphic pseudo-types bind as PostgreSQL 18
/// binds them, and refuse when only untyped literals reach a family.
#[test]
fn polymorphic_operators_bind_their_operands() {
    let operator = |name: &str, left: &str, right: &str| {
        PostgresCatalogOperator::new(name, Some(pg(left)), pg(right)).with_schema("public", false)
    };
    let catalog = PostgresCatalog::default()
        .with_operator(operator("===", "anyelement", "anyelement"))
        .with_operator(operator("&&&", "anyelement", "anyarray"))
        .with_operator(operator("@@@", "anyrange", "anyelement"))
        .with_operator(operator("~~~", "int4", "any"));
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("CREATE TYPE mood AS ENUM ('calm');")
        .unwrap();
    assert_eq!(
        resolved(&db, op("==="), Some(LITERAL), LITERAL),
        "unbound public.===(pg_catalog.anyelement,pg_catalog.anyelement)"
    );
    assert_eq!(
        resolved(&db, op("==="), Some(known("int4")), known("int4")),
        "public.===(pg_catalog.anyelement,pg_catalog.anyelement) as \
         pg_catalog.int4*,pg_catalog.int4*"
    );
    assert_eq!(
        resolved(&db, op("==="), Some(known("mood")), known("public.mood")),
        "public.===(pg_catalog.anyelement,pg_catalog.anyelement) as \"public\".\"mood\"*,\
         \"public\".\"mood\"*"
    );
    // A type PostgreSQL does not know binds to no type it can name.
    assert_eq!(resolved(&db, op("==="), Some(known("TINYINT")), known("TINYINT")), "undetermined");
    // An operand typed as the pseudo-type itself matches exactly and binds
    // nothing.
    assert_eq!(
        resolved(&db, op("==="), Some(known("anyelement")), known("anyelement")),
        "undetermined"
    );
    // `anyarray` takes the array over the type `anyelement` binds.
    assert_eq!(
        resolved(&db, op("&&&"), Some(known("int4")), LITERAL),
        "public.&&&(pg_catalog.anyelement,pg_catalog.anyarray) as \
         pg_catalog.int4*,pg_catalog._int4*"
    );
    // PostgreSQL cannot tell which range an untyped literal is.
    assert_eq!(
        resolved(&db, op("@@@"), Some(LITERAL), known("int4")),
        "unbound public.@@@(pg_catalog.anyrange,pg_catalog.anyelement)"
    );
    // `"any"` takes an operand as it is, an untyped literal staying untyped.
    assert_eq!(
        resolved(&db, op("~~~"), Some(known("int4")), known("bytea")),
        "public.~~~(pg_catalog.int4,pg_catalog.any) as pg_catalog.int4,pg_catalog.bytea"
    );
    assert_eq!(
        resolved(&db, op("~~~"), Some(known("int4")), LITERAL),
        "public.~~~(pg_catalog.int4,pg_catalog.any) as pg_catalog.int4,pg_catalog.unknown"
    );
}

/// `anyenum` binds only an enum type, refusing a domain over one and a base
/// type of the enum category, as PostgreSQL 18 refuses `#? 1`,
/// `#? 'calm'::dmood` and `#? '1'::fake`.
#[test]
fn enum_operators_take_only_enum_operands() {
    let catalog = PostgresCatalog::default().with_operator(
        PostgresCatalogOperator::new("#?", None, pg("anyenum")).with_schema("public", false),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE TYPE mood AS ENUM ('calm'); CREATE DOMAIN dmood AS mood;
             CREATE TYPE fake (INPUT = fake_in, OUTPUT = fake_out, CATEGORY = 'E');",
        )
        .unwrap();
    assert_eq!(resolved(&db, op("#?"), None, known("int4")), "not found");
    assert_eq!(resolved(&db, op("#?"), None, known("dmood")), "not found");
    assert_eq!(resolved(&db, op("#?"), None, known("mood[]")), "not found");
    assert_eq!(resolved(&db, op("#?"), None, known("fake")), "not found");
    assert_eq!(
        resolved(&db, op("#?"), None, known("mood")),
        "public.#?(,pg_catalog.anyenum) as ,\"public\".\"mood\"*"
    );
    assert_eq!(resolved(&db, op("#?"), None, known("missing")), "undetermined");
    // An operand typed as the pseudo-type itself binds nothing.
    assert_eq!(resolved(&db, op("#?"), None, known("anyenum")), "undetermined");
}

/// `pg_catalog` comes before a path that does not name it, a path naming it
/// later lets an earlier schema win, and a qualified name searches its
/// schema alone.
#[test]
fn the_search_path_and_qualification_choose_among_identical_signatures() {
    let catalog = || {
        PostgresCatalog::default()
            .with_operator(
                PostgresCatalogOperator::new("=", Some(pg("int4")), pg("int4"))
                    .with_schema("app", false),
            )
            .with_operator(
                PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
                    .with_schema("app", false),
            )
    };
    let parse = |sql: &str| {
        ParseOptions::default()
            .with_postgres_catalog(catalog())
            .parse::<PostgreSqlDialect>(sql)
            .unwrap()
    };
    let int4 = || Some(known("int4"));
    let app_first = parse("CREATE SCHEMA app; SET search_path = app, pg_catalog;");
    assert!(resolved(&app_first, op("="), int4(), known("int4")).starts_with("app.="));
    let catalog_first = parse("CREATE SCHEMA app; SET search_path = app;");
    assert!(resolved(&catalog_first, op("="), int4(), known("int4")).starts_with("pg_catalog.="));
    assert!(resolved(&catalog_first, op("==="), int4(), known("int4")).starts_with("app.==="));
    let off_path = parse("CREATE SCHEMA app;");
    assert_eq!(resolved(&off_path, op("==="), int4(), known("int4")), "not found");
    assert!(resolved(&off_path, app("="), int4(), known("int4")).starts_with("app.="));
}

/// A missing operator catalog, an operand of unknown type or a type the
/// database holds no facts about leaves the invocation open.
#[test]
fn missing_facts_leave_the_invocation_undetermined() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    assert_eq!(resolved(&db, op("="), Some(OperandType::Unknown), known("int4")), "undetermined");
    assert_eq!(resolved(&db, op("="), Some(known("missing")), known("int4")), "undetermined");
    assert_eq!(
        resolved(&db, op("="), Some(known("missing")), OperandType::UntypedLiteral),
        "undetermined"
    );
    assert_eq!(resolved(&db, op("="), Some(known("not a type (")), known("int4")), "undetermined");
    let sqlite = ParserDB::parse::<SQLiteDialect>("").unwrap();
    assert_eq!(resolved(&sqlite, op("="), Some(known("int4")), known("int4")), "undetermined");
}

/// The conversion of each operand of the selected operator, the steps of
/// one operand joined by `,` and the operands by ` | `.
fn converted(
    db: &ParserDB,
    operator: TargetName<'_>,
    left: Option<OperandType<'_>>,
    right: OperandType<'_>,
) -> String {
    let OperatorResolution::Selected(selected) =
        db.resolve_operator(operator, left, right, IdentifierCase::AsWritten)
    else {
        return "not selected".to_owned();
    };
    let plan = |evidence: &OperandEvidence<'_, ParserDB>| steps(db, evidence.conversion());
    selected
        .left()
        .map(plan)
        .into_iter()
        .chain([plan(selected.right())])
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The steps of a conversion plan, each named by what it applies.
fn steps(db: &ParserDB, plan: &[CoercionStep<'_, ParserDB>]) -> String {
    let cast = |cast: &PostgresCatalogCast| {
        format!("{}->{}", cast.source_type(db).name(), cast.target_type(db).name())
    };
    plan.iter()
        .map(|step| {
            match step {
                CoercionStep::LiteralInput { type_name } => format!("input {}", type_name.name()),
                CoercionStep::DomainToBase { base } => format!("base {}", base.name()),
                CoercionStep::Relabel(relabel) => format!("relabel {}", cast(relabel)),
                CoercionStep::Function(function) => format!("function {}", cast(function)),
                CoercionStep::InOut(io) => format!("io {}", cast(io)),
                CoercionStep::Array { element } => format!("array[{}]", steps(db, element)),
                CoercionStep::RecordToComposite => "record->composite".to_owned(),
                CoercionStep::CompositeToRecord => "composite->record".to_owned(),
                CoercionStep::Inherited => "inherited".to_owned(),
                CoercionStep::LengthCoercion { cast: length, typmod } => {
                    format!("length {} {typmod}", cast(length))
                }
                CoercionStep::DomainCheck { domain } => format!("check {}", domain.name()),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The conversions a PostgreSQL 18 server stores for the same built-in
/// operator invocations.
#[test]
fn built_in_operators_convert_operands_as_postgres_does() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN dtext AS TEXT;").unwrap();
    let cases: [(&str, OperandType<'_>, OperandType<'_>, &str); 6] = [
        (">", known("int4"), known("numeric"), "function int4->numeric | "),
        ("=", known("text"), LITERAL, " | input text"),
        ("=", known("varchar"), known("text"), "relabel varchar->text | "),
        ("=", known("dtext"), LITERAL, "base text | input text"),
        ("=", known("dtext"), known("text"), "base text | "),
        ("||", known("int4[]"), known("int8"), "array[function int4->int8] | "),
    ];
    for (name, left, right, expected) in cases {
        assert_eq!(converted(&db, op(name), Some(left), right), expected, "{name}");
    }
}

/// A conversion into a domain ends with the length coercion the domain's
/// modifier needs and the domain's check, as PostgreSQL 18 stores them.
#[test]
fn domain_operands_end_with_the_domain_steps() {
    let catalog = PostgresCatalog::default()
        .with_operator(
            PostgresCatalogOperator::new(
                "<%>",
                Some(TargetName::new("dint", false).with_schema("public", false)),
                TargetName::new("dint", false).with_schema("public", false),
            )
            .with_schema("public", false),
        )
        .with_operator(
            PostgresCatalogOperator::new(
                "<~>",
                Some(TargetName::new("d3", false).with_schema("public", false)),
                TargetName::new("d3", false).with_schema("public", false),
            )
            .with_schema("public", false),
        );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("CREATE DOMAIN dint AS INT4; CREATE DOMAIN d3 AS VARCHAR(3);")
        .unwrap();
    let modified = |name, typmod| {
        OperandType::Known { type_name: TargetName::new(name, false), typmod: Some(typmod) }
    };
    assert_eq!(
        converted(&db, op("<%>"), Some(known("int2")), known("int4")),
        "function int2->int4,check dint | check dint"
    );
    // `varchar(5)` carries modifier 9 and `varchar(3)` modifier 7.
    assert_eq!(
        converted(&db, op("<~>"), Some(known("text")), modified("varchar", 9)),
        "relabel text->varchar,length varchar->varchar 7,check d3 | \
         length varchar->varchar 7,check d3"
    );
    assert_eq!(
        converted(&db, op("<~>"), Some(modified("varchar", 7)), LITERAL),
        "check d3 | input varchar,length varchar->varchar 7,check d3"
    );
}

/// Row types meet `record` and the tables they descend from, and a
/// configured conversion through text runs the types' input and output.
#[test]
fn row_and_text_conversions_name_their_steps() {
    let catalog = PostgresCatalog::default()
        .with_operator(
            PostgresCatalogOperator::new(
                "<#>",
                Some(TargetName::new("pair", false).with_schema("public", false)),
                pg("record"),
            )
            .with_schema("public", false),
        )
        .with_operator(
            PostgresCatalogOperator::new(
                "<^>",
                Some(TargetName::new("parent", false).with_schema("public", false)),
                pg("_record"),
            )
            .with_schema("public", false),
        )
        .with_operator(
            PostgresCatalogOperator::new("<$>", None, app("fahrenheit"))
                .with_schema("public", false),
        )
        .with_cast(PostgresCatalogCast::through_io(
            app("celsius"),
            app("fahrenheit"),
            CastContext::Implicit,
        ));
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app.celsius AS ENUM ('c');
             CREATE TYPE app.fahrenheit AS ENUM ('f');
             CREATE TYPE pair AS (a INT);
             CREATE TABLE parent (a INT);
             CREATE TABLE child () INHERITS (parent);",
        )
        .unwrap();
    assert_eq!(
        converted(&db, op("<#>"), Some(known("record")), known("pair")),
        "record->composite | composite->record"
    );
    assert_eq!(
        converted(&db, op("<^>"), Some(known("child")), known("parent[]")),
        "inherited | composite->record"
    );
    assert_eq!(converted(&db, op("<$>"), None, known("app.celsius")), "io celsius->fahrenheit");
}

/// A polymorphic result takes the type the operands bind its family to, and
/// stays unanswered when the operands bind no type of that family.
#[test]
fn polymorphic_results_take_the_type_their_family_binds() {
    let operator = |name: &str, result: &str| {
        PostgresCatalogOperator::new(name, Some(pg("anyelement")), pg("anyelement"))
            .with_schema("public", false)
            .with_implementation(
                TargetName::new("pick", false).with_schema("public", false),
                pg(result),
            )
    };
    let catalog = PostgresCatalog::default()
        .with_operator(operator("===", "anyelement"))
        .with_operator(operator("@@@", "anyrange"));
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    // The result of the selected operator, `None` when nothing is selected.
    let result = |name| {
        match db.resolve_operator(
            op(name),
            Some(known("int4")),
            known("int4"),
            IdentifierCase::AsWritten,
        ) {
            OperatorResolution::Selected(selected) => {
                Some(selected.result_type().map(ToString::to_string))
            }
            _ => None,
        }
    };
    assert_eq!(result("==="), Some(Some("pg_catalog.int4".to_owned())));
    assert_eq!(result("@@@"), Some(None));
}
