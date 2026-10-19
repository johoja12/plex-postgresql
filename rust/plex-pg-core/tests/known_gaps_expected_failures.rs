//! Intentionally failing tests for known SQLite->PostgreSQL translation gaps.
//!
//! These are TDD guardrails: they describe desired future behavior.
//! They are expected to fail until the corresponding translator rules exist.

use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn translate_sql(sqlite_sql: &str) -> String {
    translate(sqlite_sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sqlite_sql, e))
        .sql
}

fn assert_output_is_valid_postgres(sql: &str) {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|e| panic!("postgres parse failed for output `{}`: {}", sql, e));
}

#[test]
fn gap_virtual_table_fts5_should_be_rewritten_to_pg_fts_schema() {
    let out = translate_sql("CREATE VIRTUAL TABLE docs USING fts5(title, body)");
    assert!(
        !out.to_lowercase().contains("virtual table"),
        "expected no VIRTUAL TABLE in PostgreSQL output, got: {}",
        out
    );
    assert_output_is_valid_postgres(&out);
}

#[test]
fn gap_virtual_table_rtree_should_be_rewritten() {
    let out =
        translate_sql("CREATE VIRTUAL TABLE spatial_idx USING rtree(id, minX, maxX, minY, maxY)");
    assert!(
        !out.to_lowercase().contains("using rtree"),
        "expected RTREE rewrite, got: {}",
        out
    );
    assert_output_is_valid_postgres(&out);
}

#[test]
fn gap_json_tree_should_map_to_pg_json_set_returning_function() {
    let out = translate_sql("SELECT * FROM json_tree('{\"a\":1,\"b\":2}')");
    let low = out.to_lowercase();
    assert!(
        low.contains("jsonb_path_query")
            || low.contains("jsonb_each")
            || low.contains("jsonb_each_text"),
        "expected json_tree rewrite to PostgreSQL jsonb function, got: {}",
        out
    );
    assert_output_is_valid_postgres(&out);
}

#[test]
fn gap_unicode_function_should_be_rewritten() {
    let out = translate_sql("SELECT unicode('A')");
    let low = out.to_lowercase();
    assert!(
        low.contains("ascii(") || low.contains("unicode_codepoint("),
        "expected unicode() rewrite, got: {}",
        out
    );
    assert_output_is_valid_postgres(&out);
}

#[test]
fn gap_pragma_should_be_removed_or_mapped() {
    let out = translate_sql("PRAGMA case_sensitive_like = ON");
    assert!(
        out.trim().is_empty() || !out.to_lowercase().contains("pragma"),
        "expected PRAGMA to be removed or mapped, got: {}",
        out
    );
    if !out.trim().is_empty() {
        assert_output_is_valid_postgres(&out);
    }
}

#[test]
fn gap_fts5_match_phrase_syntax_should_be_pg_tsquery() {
    let out = translate_sql("SELECT * FROM docs WHERE docs MATCH '\"star wars\" NEAR action'");
    let low = out.to_lowercase();
    assert!(
        low.contains("@@ plex.fts_query("),
        "expected MATCH rewrite to a tsquery through plex.fts_query, got: {}",
        out
    );
    assert_output_is_valid_postgres(&out);
}
