//! Tests that, under `SQLiteDialect`, the expression context answers the
//! affinity and storage classes SQLite gives each node, and the conversions
//! it applies to the operands of its operators and the arguments of its
//! built-in functions, as <https://www.sqlite.org/datatype3.html> defines
//! them.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, SelectItem, SetExpr},
    dialect::{Dialect, PostgreSqlDialect, SQLiteDialect},
};

use crate::support::query_parsing::query_with;

const SCHEMA: &str = "CREATE TABLE t (i INTEGER, b BIGINT, s TEXT, r REAL, d, n NUMERIC, \
                      v VARCHAR(3), dp DOUBLE PRECISION, st STRING, x BLOB);
                      CREATE TABLE strict_t (i INT, r REAL, s TEXT, x BLOB, a ANY) STRICT;
                      CREATE VIEW w AS SELECT i FROM t;";

/// The storage classes `storage` holds, `NULL` for none.
fn classes(storage: StorageClasses) -> String {
    let named = [
        (StorageClasses::INTEGER, "INTEGER"),
        (StorageClasses::REAL, "REAL"),
        (StorageClasses::TEXT, "TEXT"),
        (StorageClasses::BLOB, "BLOB"),
    ];
    let held: Vec<_> = named
        .into_iter()
        .filter(|(class, _)| storage.contains(*class))
        .map(|(_, name)| name)
        .collect();
    if held.is_empty() { "NULL".to_owned() } else { held.join("|") }
}

/// The type `operand` spells as `affinity:classes`, `-` standing for no
/// affinity and `?` for an unknown type.
fn typed(operand: &OperandType<'_>) -> String {
    match operand {
        OperandType::Sqlite { affinity, storage } => {
            let affinity = affinity.map_or("-".to_owned(), |affinity| format!("{affinity:?}"));
            format!("{affinity}:{}", classes(*storage))
        }
        OperandType::Unknown => "?".to_owned(),
        other => panic!("SQLite typing answered {other:?}"),
    }
}

/// The conversion `conversion` spells, `-` for none.
fn converted(conversion: Option<SqliteConversion>) -> String {
    conversion.map_or("-".to_owned(), |conversion| format!("{conversion:?}"))
}

/// Each projected expression of `sql` over [`SCHEMA`], with what `answer`
/// reads off it in the context of the query.
fn projected(
    sql: &str,
    answer: impl Fn(&ExpressionContext<'_, '_, ParserDB>, &Expr) -> String,
) -> Vec<String> {
    projected_in(&SQLiteDialect {}, sql, answer)
}

/// Each projected expression of `sql`, parsed in `dialect`, over [`SCHEMA`],
/// with what `answer` reads off it in the context of the query.
fn projected_in(
    dialect: &dyn Dialect,
    sql: &str,
    answer: impl Fn(&ExpressionContext<'_, '_, ParserDB>, &Expr) -> String,
) -> Vec<String> {
    let db = ParserDB::parse::<SQLiteDialect>(SCHEMA).expect("the schema parses");
    let query = query_with(dialect, sql);
    let context =
        ExpressionContext::new(&query, &db, IdentifierCase::Folded).expect("the query is typed");
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a plain SELECT") };
    select
        .projection
        .iter()
        .map(|item| {
            let (SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias { expr: expression, .. }) = item
            else {
                panic!("an expression item")
            };
            answer(&context, expression)
        })
        .collect()
}

/// The type each projected expression of `sql` takes.
fn types(sql: &str) -> Vec<String> {
    projected(sql, |context, expression| {
        typed(&context.type_of(expression).expect("a node of the query"))
    })
}

/// The operators each projected expression of `sql` applies, each spelled
/// as `name(left,right)` with the conversion of each operand, and `not
/// derived` for a node whose operators the context does not derive.
fn operators(sql: &str) -> Vec<String> {
    projected(sql, |context, expression| {
        let Some(applications) = context.operators(expression).expect("a node of the query") else {
            return "not derived".to_owned();
        };
        let spelled: Vec<_> = applications
            .iter()
            .map(|application| {
                match application.resolution() {
                    OperatorResolution::Sqlite(operator) => {
                        format!(
                            "{}({},{})",
                            application.name().name(),
                            converted(operator.left()),
                            converted(operator.right())
                        )
                    }
                    OperatorResolution::Undetermined => {
                        format!("{} undetermined", application.name().name())
                    }
                    other => panic!("SQLite typing resolved {other:?}"),
                }
            })
            .collect();
        spelled.join(" ")
    })
}

/// The call each projected expression of `sql` makes, spelled as the
/// conversion of each argument and the classes it returns, and `no call` for
/// a node that is none.
fn calls(sql: &str) -> Vec<String> {
    projected(sql, |context, expression| {
        let Some(application) = context.call(expression).expect("a node of the query") else {
            return "no call".to_owned();
        };
        match application.resolution() {
            CallResolution::Sqlite(call) => {
                let arguments: Vec<_> = call.arguments().iter().copied().map(converted).collect();
                format!("({}) {}", arguments.join(","), classes(call.storage()))
            }
            CallResolution::Undetermined => "undetermined".to_owned(),
            other => panic!("SQLite typing resolved {other:?}"),
        }
    })
}

#[test]
fn a_column_takes_the_affinity_its_declared_type_gives_it() {
    assert_eq!(
        types("SELECT i, b, s, r, d, n, v, dp, st, x FROM t"),
        [
            "Integer:INTEGER",
            "Integer:INTEGER",
            "Text:TEXT",
            "Real:REAL",
            "Blob:INTEGER|REAL|TEXT|BLOB",
            "Numeric:INTEGER|REAL",
            "Text:TEXT",
            "Real:REAL",
            // `STRING` matches no rule but the last.
            "Numeric:INTEGER|REAL",
            "Blob:INTEGER|REAL|TEXT|BLOB",
        ]
    );
}

#[test]
fn a_strict_column_holds_its_declared_class_alone() {
    assert_eq!(
        types("SELECT i, r, s, x, a FROM strict_t"),
        ["Integer:INTEGER", "Real:REAL", "Text:TEXT", "Blob:BLOB", "Blob:INTEGER|REAL|TEXT|BLOB",]
    );
}

#[test]
fn a_literal_has_no_affinity_and_the_class_it_writes() {
    assert_eq!(
        types("SELECT 5, -5, 1.5, 1e5, 9223372036854775808, 'a', x'41', NULL, ?"),
        [
            "-:INTEGER",
            "-:INTEGER",
            "-:REAL",
            "-:REAL",
            "-:REAL",
            "-:TEXT",
            // sqlparser reads `0x41` and `x'41'` alike under SQLite.
            "-:INTEGER|BLOB",
            "-:NULL",
            "-:INTEGER|REAL|TEXT|BLOB",
        ]
    );
}

#[test]
fn a_cast_takes_the_affinity_of_its_target_and_a_unary_plus_drops_it() {
    assert_eq!(
        types(
            "SELECT CAST(s AS INTEGER), CAST(i AS TEXT), CAST(s AS NUMERIC), CAST(i AS BLOB), +s, \
             (s), s COLLATE NOCASE FROM t"
        ),
        [
            "Integer:INTEGER",
            "Text:TEXT",
            "Numeric:INTEGER|REAL",
            "Blob:BLOB",
            "-:TEXT",
            "Text:TEXT",
            "Text:TEXT"
        ]
    );
}

#[test]
fn an_operator_returns_values_without_affinity() {
    assert_eq!(
        types(
            "SELECT i + r, i + b, s + 1, i % 2, i = s, s || i, -s, i & 3, NULL + 1, i IN (1, 2), \
             CASE WHEN i THEN s ELSE r END FROM t"
        ),
        [
            "-:REAL",
            "-:INTEGER",
            "-:INTEGER|REAL",
            "-:INTEGER",
            "-:INTEGER",
            "-:TEXT",
            "-:INTEGER|REAL",
            "-:INTEGER",
            "-:NULL",
            "-:INTEGER",
            "-:REAL|TEXT",
        ]
    );
}

#[test]
fn a_comparison_applies_the_affinity_section_four_two_picks() {
    assert_eq!(
        operators(
            "SELECT i = s, i = b, i = '5', s = 5, n = s, s = r, i = d, d = '5', s = x, '5' = 5 \
             FROM t"
        ),
        [
            "=(-,Affinity(Numeric))",
            "=(-,-)",
            "=(-,Affinity(Numeric))",
            "=(-,Affinity(Text))",
            "=(-,Affinity(Numeric))",
            "=(Affinity(Numeric),-)",
            // A `BLOB` affinity column may hold numbers, so nothing says
            // `NUMERIC` affinity converts its values.
            "=(-,-)",
            "=(-,-)",
            "=(-,-)",
            "=(-,-)",
        ]
    );
}

#[test]
fn between_and_in_compare_each_bound_and_element() {
    assert_eq!(
        operators(
            "SELECT i BETWEEN '1' AND 2, i NOT IN ('1', 2), s IN (1, s), i IS '5', \
             i IS NOT DISTINCT FROM '5' FROM t"
        ),
        [
            ">=(-,Affinity(Numeric)) <=(-,-)",
            "<>(-,Affinity(Numeric)) <>(-,-)",
            // An element of the list has no affinity, even a column.
            "=(-,Affinity(Text)) =(-,-)",
            "IS(-,Affinity(Numeric))",
            "IS(-,Affinity(Numeric))",
        ]
    );
}

#[test]
fn arithmetic_reads_numbers_and_concatenation_reads_text() {
    assert_eq!(
        operators(
            "SELECT i + r, s + 1, i * 2, -s, i % 2.5, s || i, s || s, s LIKE i, r || s, i << s, \
             -5 FROM t"
        ),
        [
            "+(Real,-)",
            "+(Numeric,-)",
            "*(-,-)",
            "-(-,Numeric)",
            "%(-,Integer)",
            "||(-,Text)",
            "||(-,-)",
            "LIKE(-,Text)",
            "||(Text,-)",
            "<<(-,Integer)",
            // The grammar folds `-5` into a constant.
            "",
        ]
    );
}

#[test]
fn an_operand_of_unknown_type_leaves_the_operator_undetermined() {
    assert_eq!(
        operators("SELECT my_function(i) = '5', my_function(i) + 1 FROM t"),
        ["= undetermined", "+ undetermined"]
    );
}

#[test]
fn a_query_column_keeps_the_affinity_of_its_definition() {
    assert_eq!(
        operators(
            "SELECT k = '5', (SELECT s FROM t) = 5 FROM (SELECT i AS k FROM t UNION SELECT b FROM t)"
        ),
        ["=(-,Affinity(Numeric))", "=(-,Affinity(Text))"]
    );
    assert_eq!(types("SELECT k FROM (SELECT i AS k FROM t UNION SELECT s FROM t)"), ["?"]);
    // A column of a view keeps the affinity of the expression defining it.
    assert_eq!(operators("SELECT i = '5' FROM w"), ["=(-,Affinity(Numeric))"]);
}

#[test]
fn a_builtin_function_reads_its_arguments_as_it_documents() {
    assert_eq!(
        calls(
            "SELECT round(i, 2), round(s), substr(i, 1), substr(x, '1'), abs(s), length(x), \
             length(i), upper(s), coalesce(i, s), char(s), substring(s, 2, 1), ceil(s), floor(i), \
             trim(i) FROM t"
        ),
        [
            "(Real,-) REAL",
            "(Real) REAL",
            "(Text,-) TEXT",
            // A `BLOB` affinity column may hold text or blobs, read as they
            // are.
            "(-,Integer) TEXT|BLOB",
            "(Numeric) INTEGER|REAL",
            "(-) INTEGER",
            "(Text) INTEGER",
            "(-) TEXT",
            "(-,-) INTEGER|TEXT",
            "(Integer) TEXT",
            "(-,-,-) TEXT",
            "(Numeric) INTEGER|REAL",
            "(-) INTEGER",
            "(Text) TEXT",
        ]
    );
    // `instr` reads blobs as blobs only when every argument is one.
    assert_eq!(
        calls("SELECT instr(x, x), instr(x, s), instr(i, s), substr(x, 1) FROM strict_t"),
        ["(-,-) INTEGER", "(Text,-) INTEGER", "(Text,-) INTEGER", "(-,-) BLOB"]
    );
}

#[test]
fn a_call_sqlite_does_not_define_is_undetermined() {
    assert_eq!(
        calls(
            "SELECT my_function(i), round(i, 2, 3), main.round(i), round(my_function(i)), i FROM t"
        ),
        ["undetermined", "undetermined", "undetermined", "undetermined", "no call"]
    );
}

#[test]
fn the_conditional_builtins_return_what_they_pick() {
    assert_eq!(
        calls("SELECT nullif(i, s), iif(i, s, r), iif(i, s), hex(i), hex(x) FROM strict_t"),
        ["(-,-) INTEGER", "(-,-,-) REAL|TEXT", "(-,-) TEXT", "(Blob) TEXT", "(-) TEXT"]
    );
    assert_eq!(
        types("SELECT round(i), nullif(i, s), iif(i, s, r) FROM t"),
        ["-:REAL", "-:INTEGER", "-:REAL|TEXT"]
    );
}

#[test]
fn every_comparison_and_arithmetic_operator_names_what_it_applies() {
    assert_eq!(
        operators(
            "SELECT i <> s, i < s, i <= s, i > s, i >= s, i IS NOT s, s IS DISTINCT FROM 5, \
             5 = s FROM t"
        ),
        [
            "<>(-,Affinity(Numeric))",
            "<(-,Affinity(Numeric))",
            "<=(-,Affinity(Numeric))",
            ">(-,Affinity(Numeric))",
            ">=(-,Affinity(Numeric))",
            "IS NOT(-,Affinity(Numeric))",
            "IS NOT(-,Affinity(Text))",
            "=(Affinity(Text),-)",
        ]
    );
    assert_eq!(
        operators(
            "SELECT i - s, i / s, i & s, i | s, i >> s, s GLOB i, ~s, +s, i, 'a', \
             CAST(s AS INT), s COLLATE NOCASE FROM t"
        ),
        [
            "-(-,Numeric)",
            "/(-,Numeric)",
            "&(-,Integer)",
            "|(-,Integer)",
            ">>(-,Integer)",
            "GLOB(-,Text)",
            "~(-,Integer)",
            "",
            "",
            "",
            "",
            "",
        ]
    );
}

#[test]
fn connectives_and_calls_apply_no_operator_the_context_derives() {
    assert_eq!(
        operators("SELECT i AND s, NOT i, i LIKE s ESCAPE 'x', round(i), i IS NULL FROM t"),
        ["not derived", "not derived", "not derived", "not derived", "not derived"]
    );
}

#[test]
fn a_test_or_connective_returns_an_integer_and_an_unknown_operand_leaves_the_result_unknown() {
    assert_eq!(
        types(
            "SELECT NOT i, ~i, TRUE, i IS NULL, i AND s, -my_function(i), +my_function(i), \
             my_function(i) + 1, CASE WHEN i THEN my_function(i) END, nosuch, i REGEXP s, value, \
             (i, s) FROM t, json_each('[1]')"
        ),
        [
            "-:INTEGER",
            "-:INTEGER",
            "-:INTEGER",
            "-:INTEGER",
            "-:INTEGER",
            "?",
            "?",
            "?",
            "?",
            "?",
            "?",
            "?",
            "?"
        ]
    );
}

#[test]
fn an_operator_sqlite_does_not_have_leaves_the_node_untyped() {
    let unknown = |context: &ExpressionContext<'_, '_, ParserDB>, expression: &Expr| {
        let typed = typed(&context.type_of(expression).expect("a node of the query"));
        let applied = context.operators(expression).expect("a node of the query");
        format!("{typed} {}", if applied.is_some() { "derived" } else { "not derived" })
    };
    assert_eq!(
        projected_in(&PostgreSqlDialect {}, "SELECT @ i, i ->> 'a', B'01' FROM t", unknown),
        ["? not derived", "? not derived", "? derived"]
    );
}

#[test]
fn a_derived_column_takes_the_type_of_its_definition() {
    assert_eq!(
        types(
            "SELECT k, column1, m, n FROM (SELECT CAST(s AS INTEGER) AS k FROM t), \
             (VALUES (1), (2.5)), (SELECT my_function(i) AS m FROM t UNION SELECT i FROM t), \
             (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) \
              SELECT n FROM c)"
        ),
        ["Integer:INTEGER", "-:INTEGER|REAL", "?", "-:INTEGER"]
    );
}
