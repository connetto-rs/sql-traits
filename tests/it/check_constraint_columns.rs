//! The columns a check constraint reads, whatever expression form reads them.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn checks_read_the_columns_inside_any_and_case_forms() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE grants (
            role TEXT CHECK (role = ANY (ARRAY['read', 'write'])),
            verb VARCHAR(9) CHECK (((verb)::text = ANY ((ARRAY['get'::character varying])::text[]))),
            level INT CHECK (CASE WHEN role = 'read' THEN level < 3 ELSE level < 9 END)
        );",
    )?;
    let table = db.tables().next().ok_or_else(|| std::io::Error::other("no table"))?;
    let read: Vec<Vec<&str>> = table
        .check_constraints(&db)?
        .map(|check| Ok(check.columns(&db)?.map(ColumnLike::column_name).collect()))
        .collect::<Result<_, Error>>()?;
    assert_eq!(read, [vec!["role"], vec!["verb"], vec!["role", "level"]]);
    Ok(())
}

#[test]
fn an_unknown_column_inside_an_any_form_is_refused() {
    let parsed = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE grants (role TEXT CHECK (role = ANY (ARRAY[missing])));",
    );
    assert!(matches!(
        parsed,
        Err(Error::UnknownColumnInCheckConstraint { column_name, .. }) if column_name == "missing"
    ));
}
