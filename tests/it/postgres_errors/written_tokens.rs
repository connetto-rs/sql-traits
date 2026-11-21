//! A syntax error names its token as the failing statement writes it.

use super::refuses;

#[test]
fn a_syntax_error_names_its_token_as_written() {
    for (sql, token) in [
        ("create temp materialized view v as select 1;", "materialized"),
        ("CREATE TEMP UnLoGgEd TABLE t (id int);", "UnLoGgEd"),
        (
            "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c (id int);
             ALTER TABLE p ADD COLUMN x int, attach PARTITION c DEFAULT;",
            "attach",
        ),
        (
            "CREATE TABLE p (id int) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p DEFAULT;
             ALTER TABLE p ADD COLUMN x int, DeTaCh PARTITION c;",
            "DeTaCh",
        ),
        ("set role = array['x'];", "array"),
        ("set session_authorization = null;", "null"),
        ("CREATE ROLE wt_probe; alter role wt_probe set timezone = null;", "null"),
        (
            "create function f() returns int language sql set search_path = null as 'select 1';",
            "null",
        ),
        ("create or replace materialized view case_probe as select 1;", "materialized"),
        ("create view if not exists case_probe as select 1;", "not"),
        (
            "CREATE TABLE t (id int); create function case_probe() returns void language sql
             begin atomic update t LeFt join t b on true set id=1; end;",
            "LeFt",
        ),
        ("create procedure pr() as SeLeCt 1;", "SeLeCt"),
    ] {
        refuses(sql, "42601", &format!("syntax error at or near \"{token}\""));
    }
}
