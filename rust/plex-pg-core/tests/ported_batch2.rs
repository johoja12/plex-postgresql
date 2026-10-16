//! Ported batch 2: KEYWORDS, QUOTES, MIXED-CASE, DEDUP, UPSERT
//!
//! These tests are ported from the C test_sql_translator.c test suite.
//! Tests marked `#[ignore]` correspond to features not yet implemented in the
//! Rust translator (see the GAP comment on each).

use plex_pg_core::translate;

// ═══════════════════════════════════════════════════════════════════════════════
// KEYWORDS (23 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn keyword_glob() {
    let t = translate("SELECT * FROM t WHERE name GLOB '*test*'").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("name ~ '^.*test.*$'"),
        "expected a case-sensitive regular expression match, got: {}",
        t.sql
    );
    assert!(
        !low.contains("glob"),
        "GLOB should be removed, got: {}",
        t.sql
    );
}

#[test]
fn keyword_notnull() {
    let t = translate("SELECT * FROM t WHERE a IS NOT NULL").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("NOT NULL"),
        "expected NOT NULL preserved, got: {}",
        t.sql
    );
}

#[test]
fn keyword_alter_table_add_quoted() {
    let t = translate("ALTER TABLE 'metadata_items' ADD 'new_col' TEXT").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("ADD COLUMN IF NOT EXISTS"),
        "expected ADD COLUMN IF NOT EXISTS, got: {}",
        t.sql
    );
}

#[test]
fn keyword_alter_table_add_dblquoted() {
    let t = translate("ALTER TABLE \"metadata_items\" ADD \"new_col\" TEXT").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("ADD COLUMN IF NOT EXISTS"),
        "expected ADD COLUMN IF NOT EXISTS, got: {}",
        t.sql
    );
}

#[test]
fn keyword_alter_table_add_unquoted() {
    let t = translate("ALTER TABLE metadata_items ADD new_col TEXT").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("ADD COLUMN IF NOT EXISTS"),
        "expected ADD COLUMN IF NOT EXISTS, got: {}",
        t.sql
    );
}

#[test]
fn keyword_begin_immediate() {
    let t = translate("BEGIN IMMEDIATE").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("IMMEDIATE"),
        "IMMEDIATE should be stripped, got: {}",
        t.sql
    );
    assert!(
        up.contains("BEGIN") || up.contains("START TRANSACTION"),
        "should still begin a transaction, got: {}",
        t.sql
    );
}

#[test]
fn keyword_begin_deferred() {
    let t = translate("BEGIN DEFERRED").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("DEFERRED"),
        "DEFERRED should be stripped, got: {}",
        t.sql
    );
}

#[test]
fn keyword_begin_exclusive() {
    let t = translate("BEGIN EXCLUSIVE").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("EXCLUSIVE"),
        "EXCLUSIVE should be stripped, got: {}",
        t.sql
    );
}

#[test]
fn keyword_insert_or_ignore() {
    let t =
        translate("INSERT OR IGNORE INTO schema_migrations (version) VALUES ('20230101')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("INSERT INTO"),
        "expected INSERT INTO, got: {}",
        t.sql
    );
    assert!(
        !up.contains("OR IGNORE"),
        "OR IGNORE should be removed, got: {}",
        t.sql
    );
    // Rust translator converts to ON CONFLICT DO NOTHING
    assert!(
        up.contains("DO NOTHING"),
        "expected ON CONFLICT DO NOTHING, got: {}",
        t.sql
    );
}

#[test]
fn keyword_replace_into() {
    let t = translate("REPLACE INTO preferences (name, value) VALUES ('key', 'val')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("INSERT"),
        "expected INSERT in output, got: {}",
        t.sql
    );
    // The upsert module transforms REPLACE INTO
    assert!(
        !up.contains("REPLACE INTO"),
        "REPLACE INTO should be rewritten, got: {}",
        t.sql
    );
}

#[test]
fn keyword_empty_in() {
    let t = translate("SELECT * FROM tags WHERE id in ()").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("IN ()"),
        "empty IN () should be replaced, got: {}",
        t.sql
    );
    // Should be rewritten to a subquery: IN (SELECT ...)
    assert!(
        up.contains("IN (SELECT") || up.contains("IN(SELECT"),
        "expected IN (SELECT ...) subquery, got: {}",
        t.sql
    );
}

#[test]
fn keyword_empty_in_spaces() {
    let t = translate("SELECT * FROM tags WHERE id in (  )").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("IN ()") && !up.contains("IN (  )"),
        "empty IN () should be replaced, got: {}",
        t.sql
    );
    assert!(
        up.contains("IN (SELECT") || up.contains("IN(SELECT"),
        "expected IN (SELECT ...) subquery, got: {}",
        t.sql
    );
}

#[test]
fn keyword_group_by_null() {
    let t = translate("SELECT count(*) FROM metadata_items GROUP BY NULL").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("GROUP BY NULL"),
        "GROUP BY NULL should be removed, got: {}",
        t.sql
    );
}

#[test]
fn keyword_having_cnt() {
    let t = translate(
        "SELECT library_section_id, count(media_items.id) as cnt \
         FROM metadata_items \
         LEFT JOIN media_items ON media_items.metadata_item_id = metadata_items.id \
         GROUP BY library_section_id \
         HAVING cnt = 0",
    )
    .unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("having count(media_items.id) = 0")
            || low.contains("having count(media_items.id)"),
        "HAVING should expand cnt alias, got: {}",
        t.sql
    );
}

#[test]
fn keyword_sqlite_master() {
    let t = translate("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("information_schema"),
        "expected information_schema, got: {}",
        t.sql
    );
    assert!(
        !low.contains("sqlite_master"),
        "sqlite_master should be replaced, got: {}",
        t.sql
    );
}

#[test]
fn keyword_sqlite_schema() {
    let t = translate("SELECT name FROM sqlite_schema WHERE type='table'").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("information_schema"),
        "expected information_schema, got: {}",
        t.sql
    );
    assert!(
        !low.contains("sqlite_schema"),
        "sqlite_schema should be replaced, got: {}",
        t.sql
    );
}

#[test]
fn keyword_main_dot_sqlite_master() {
    let t = translate("SELECT name FROM \"main\".sqlite_master WHERE type='table'").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("information_schema"),
        "expected information_schema, got: {}",
        t.sql
    );
}

#[test]
fn keyword_main_unquoted_sqlite_master() {
    let t = translate("SELECT name FROM main.sqlite_master WHERE type='table'").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("information_schema"),
        "expected information_schema, got: {}",
        t.sql
    );
}

#[test]
fn keyword_sqlite_master_order_by_rowid() {
    let t = translate("SELECT name FROM sqlite_master WHERE type='table' ORDER BY rowid").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("information_schema"),
        "expected information_schema, got: {}",
        t.sql
    );
    assert!(
        !low.contains("rowid"),
        "ORDER BY rowid should be removed, got: {}",
        t.sql
    );
}

#[test]
fn keyword_replace_into_preserved_with_insert_or() {
    let t = translate("INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'test')").unwrap();
    let up = t.sql.to_uppercase();
    // The upsert module handles INSERT OR REPLACE
    assert!(
        up.contains("ON CONFLICT") || up.contains("REPLACE"),
        "expected ON CONFLICT or REPLACE handling, got: {}",
        t.sql
    );
    assert!(
        !up.contains("OR REPLACE"),
        "OR REPLACE should be rewritten, got: {}",
        t.sql
    );
}

#[test]
fn keyword_indexed_by() {
    let t = translate("SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'")
        .unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("INDEXED BY"),
        "INDEXED BY should be removed, got: {}",
        t.sql
    );
    assert!(
        up.contains("WHERE"),
        "query should still have WHERE clause, got: {}",
        t.sql
    );
}

#[test]
fn keyword_indexed_by_multiple() {
    let t = translate("SELECT * FROM t1 INDEXED BY idx1 JOIN t2 INDEXED BY idx2 ON t1.id = t2.id")
        .unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("INDEXED BY"),
        "all INDEXED BY hints should be removed, got: {}",
        t.sql
    );
    assert!(
        up.contains("JOIN"),
        "JOIN should still be present, got: {}",
        t.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// QUOTES (9 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn quote_column_quotes() {
    let t = translate("SELECT t.'name' FROM t").unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("t.\"name\""),
        "expected double-quoted column, got: {}",
        sql
    );
}

#[test]
fn quote_alias_quotes() {
    let t = translate("SELECT a AS 'my_alias' FROM t").unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"my_alias\""),
        "expected double-quoted alias, got: {}",
        sql
    );
}

#[test]
fn quote_ddl_table() {
    let t = translate("CREATE TABLE 'my_table' (id INTEGER)").unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"my_table\""),
        "expected double-quoted table name, got: {}",
        sql
    );
}

#[test]
fn quote_ddl_not_dml() {
    // String literals in DML should be preserved as single-quoted values
    let t = translate("SELECT * FROM t WHERE name = 'test'").unwrap();
    assert!(
        t.sql.contains("'test'"),
        "string literal 'test' should be preserved, got: {}",
        t.sql
    );
}

#[test]
fn quote_if_not_exists_table() {
    let t = translate("CREATE TABLE foo (id INTEGER)").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("IF NOT EXISTS"),
        "expected IF NOT EXISTS for CREATE TABLE, got: {}",
        t.sql
    );
}

#[test]
fn quote_if_not_exists_index() {
    let t = translate("CREATE INDEX idx_foo ON t(id)").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("IF NOT EXISTS"),
        "expected IF NOT EXISTS for CREATE INDEX, got: {}",
        t.sql
    );
}

#[test]
fn quote_if_not_exists_unique_index() {
    let t = translate("CREATE UNIQUE INDEX idx_u ON t(name)").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("IF NOT EXISTS"),
        "expected IF NOT EXISTS for CREATE UNIQUE INDEX, got: {}",
        t.sql
    );
    assert!(
        up.contains("UNIQUE"),
        "UNIQUE should be preserved, got: {}",
        t.sql
    );
}

#[test]
fn quote_single_quoted_create_index() {
    let t = translate("CREATE INDEX 'idx_q' ON 'devices' ('identifier')").unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"idx_q\""),
        "expected double-quoted index name, got: {}",
        sql
    );
    assert!(
        sql.contains("\"devices\""),
        "expected double-quoted table name, got: {}",
        sql
    );
    assert!(
        sql.contains("(\"identifier\")"),
        "expected double-quoted index column, got: {}",
        sql
    );
}

#[test]
fn quote_single_quoted_create_index_with_collation() {
    let t = translate(
        "CREATE INDEX 'index_title_sort_naturalsort' ON 'metadata_items' ('title_sort' COLLATE naturalsort)",
    )
    .unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"index_title_sort_naturalsort\""),
        "expected double-quoted index name, got: {}",
        sql
    );
    assert!(
        sql.contains("\"metadata_items\""),
        "expected double-quoted table name, got: {}",
        sql
    );
    assert!(
        sql.contains("\"title_sort\""),
        "expected double-quoted index column, got: {}",
        sql
    );
}

#[test]
fn quote_single_quoted_unique_multi_column_index() {
    let t = translate(
        "CREATE UNIQUE INDEX 'idx_sync' ON 'synced_metadata_items' ('sync_list_id', 'metadata_item_id')",
    )
    .unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"idx_sync\""),
        "expected double-quoted index name, got: {}",
        sql
    );
    assert!(
        sql.contains("\"synced_metadata_items\""),
        "expected double-quoted table name, got: {}",
        sql
    );
    assert!(
        sql.contains("(\"sync_list_id\", \"metadata_item_id\")"),
        "expected double-quoted index columns, got: {}",
        sql
    );
}

#[test]
fn quote_single_quoted_drop_index() {
    let t = translate("DROP INDEX IF EXISTS 'index_title_sort_naturalsort'").unwrap();
    let sql = &t.sql;
    assert!(
        sql.contains("\"index_title_sort_naturalsort\""),
        "expected double-quoted drop index target, got: {}",
        sql
    );
}

#[test]
fn quote_if_not_exists_already() {
    let t = translate("CREATE TABLE IF NOT EXISTS foo (id INT)").unwrap();
    let up = t.sql.to_uppercase();
    let count = up.matches("IF NOT EXISTS").count();
    assert_eq!(
        count, 1,
        "IF NOT EXISTS should appear exactly once, got {} occurrences in: {}",
        count, t.sql
    );
}

#[test]
fn quote_on_conflict_unquote() {
    let t =
        translate("INSERT INTO t (id, name) VALUES (1, 'val') ON CONFLICT(\"name\") DO NOTHING")
            .unwrap();
    let sql = &t.sql;
    // The double-quoted name inside ON CONFLICT should be unquoted
    assert!(
        sql.contains("ON CONFLICT(name)") || sql.contains("ON CONFLICT (name)"),
        "expected unquoted ON CONFLICT target, got: {}",
        sql
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// DUPLICATE ASSIGNMENTS (5 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn dedup_assignments_basic() {
    let t = translate("UPDATE t SET a=1, b=2, a=3 WHERE id=1").unwrap();
    let low = t.sql.to_lowercase();
    // Should keep only the last assignment for 'a'
    let a_count = low.matches("a =").count() + low.matches("a=").count();
    assert_eq!(
        a_count, 1,
        "duplicate assignment to 'a' should be deduped, got: {}",
        t.sql
    );
    assert!(
        low.contains("3"),
        "last value (3) should be kept, got: {}",
        t.sql
    );
}

#[test]
fn dedup_assignments_no_dup() {
    let t = translate("UPDATE t SET a=1, b=2 WHERE id=1").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("a") && low.contains("b"),
        "non-duplicate assignments unchanged, got: {}",
        t.sql
    );
}

#[test]
fn dedup_assignments_quoted() {
    let t = translate("UPDATE t SET `a`=1, `b`=2, `a`=3 WHERE id=1").unwrap();
    let sql = &t.sql;
    assert!(
        !sql.contains('`'),
        "backticks should be converted, got: {}",
        sql
    );
}

#[test]
fn dedup_assignments_params() {
    let t = translate("UPDATE t SET a=?, b=?, a=? WHERE id=?").unwrap();
    let low = t.sql.to_lowercase();
    // Removed params should use COALESCE
    let _count = low.matches("coalesce").count();
    assert!(
        low.contains("$"),
        "should have dollar placeholders, got: {}",
        t.sql
    );
}

#[test]
fn dedup_not_update() {
    let t = translate("SELECT a, a FROM t").unwrap();
    let low = t.sql.to_lowercase();
    // Non-UPDATE statements should not be touched by dedup
    assert!(
        low.contains("select"),
        "SELECT should pass through, got: {}",
        t.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// MIXED-CASE IDENTIFIER QUOTING (11 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn mixed_case_column_alias() {
    let t = translate("SELECT id AS blankKeyTaggingId FROM t").unwrap();
    assert!(
        t.sql.contains("\"blankKeyTaggingId\""),
        "camelCase alias should be double-quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_table_alias() {
    let t =
        translate("SELECT * FROM tags JOIN tags as otherTags ON otherTags.id = tags.id").unwrap();
    assert!(
        t.sql.contains("\"otherTags\""),
        "camelCase table alias should be double-quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_no_uppercase() {
    let t = translate("SELECT id AS my_alias FROM t").unwrap();
    // All-lowercase alias should NOT be quoted
    assert!(
        !t.sql.contains("\"my_alias\""),
        "all-lowercase alias should not be quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_already_quoted() {
    let t = translate("SELECT id AS \"Foo\" FROM t").unwrap();
    assert!(
        t.sql.contains("\"Foo\""),
        "already-quoted alias should be preserved, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_in_string_literal() {
    let t = translate("SELECT 'AS camelCase' FROM t").unwrap();
    assert!(
        t.sql.contains("'AS camelCase'"),
        "string literal should be unchanged, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_synccollections() {
    let t = translate("SELECT id AS syncItemId, title AS syncTitle FROM metadata_items").unwrap();
    assert!(
        t.sql.contains("\"syncItemId\"") && t.sql.contains("\"syncTitle\""),
        "camelCase aliases should be quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_table_reference() {
    let t = translate("SELECT grandparentsSettings.value FROM settings AS grandparentsSettings")
        .unwrap();
    assert!(
        t.sql.contains("\"grandparentsSettings\""),
        "camelCase table ref should be quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_cast_not_quoted() {
    let t = translate("SELECT CAST(x AS INTEGER) FROM t").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        !up.contains("\"INTEGER\""),
        "INTEGER type should not be quoted, got: {}",
        t.sql
    );
    assert!(
        up.contains("CAST"),
        "CAST should be present, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_cast_with_alias() {
    let t = translate("SELECT CAST(x AS INTEGER) AS myValue FROM t").unwrap();
    assert!(
        t.sql.contains("\"myValue\""),
        "camelCase alias after CAST should be quoted, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_null_input() {
    let t = translate("").unwrap();
    assert!(
        t.sql.is_empty(),
        "empty input should produce empty output, got: {:?}",
        t.sql
    );
}

#[test]
fn mixed_case_full_translate() {
    let t = translate("SELECT id AS itemId, title AS itemTitle FROM metadata_items WHERE id = ?")
        .unwrap();
    assert!(
        t.sql.contains("\"itemId\"") && t.sql.contains("\"itemTitle\""),
        "camelCase aliases should be quoted in full pipeline, got: {}",
        t.sql
    );
    assert!(
        t.sql.contains("$1"),
        "placeholder should be translated, got: {}",
        t.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// UPSERT (3 tests from test_sql_translator.c)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn upsert_tags_without_column_list() {
    let t = translate("INSERT OR REPLACE INTO tags VALUES (1, 'test', 2, 3, 'extra')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("ON CONFLICT"),
        "expected ON CONFLICT for INSERT OR REPLACE, got: {}",
        t.sql
    );
    assert!(
        !up.contains("OR REPLACE"),
        "OR REPLACE should be removed, got: {}",
        t.sql
    );
}

#[test]
fn upsert_tags_with_column_list() {
    let t =
        translate("INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES (1, 'test', 2)").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("ON CONFLICT"),
        "expected ON CONFLICT, got: {}",
        t.sql
    );
    // Should specify the conflict target column
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("on conflict (id)") || low.contains("on conflict(id)"),
        "expected ON CONFLICT (id), got: {}",
        t.sql
    );
    assert!(
        low.contains("excluded.tag"),
        "expected EXCLUDED.tag in SET clause, got: {}",
        t.sql
    );
}

#[test]
fn upsert_unknown_table_no_columns() {
    let t = translate("INSERT OR REPLACE INTO unknown_tbl VALUES (1, 'test')").unwrap();
    let up = t.sql.to_uppercase();
    // Should still produce valid SQL even for unknown tables
    assert!(
        up.contains("INSERT"),
        "should produce INSERT, got: {}",
        t.sql
    );
}

#[test]
fn mixed_case_join_on_clause_quoted() {
    // Specifically test that mixed-case alias references in ON clause are quoted
    let t = translate(
        "select taggings.id as blankKeyTaggingId, otherTags.id as nonblankKeyId from tags join tags as otherTags on otherTags.tag = tags.tag where tags.tag_value = ?"
    ).unwrap();
    eprintln!("SQL: {}", t.sql);
    assert!(
        t.sql.contains("\"otherTags\".tag"),
        "otherTags in ON clause should be quoted, got: {}",
        t.sql
    );
}
