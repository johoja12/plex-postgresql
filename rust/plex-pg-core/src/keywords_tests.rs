#![allow(non_snake_case)]

mod self_join_tests {
    use super::super::preprocess;

    // ── Shape A tests ──────────────────────────────────────────────────────────

    /// Original failing pattern: FROM metadata_item_settings + aliased parents join
    /// + UNALIASED metadata_items join.  The unaliased join must get AS mi and all
    /// metadata_items.<col> refs must become mi.<col>.
    #[test]
    fn compat_aliases__preprocess_rewrites_metadata_items_self_join_alias_refs() {
        // Shape A: unaliased second join that needs AS mi
        let input = concat!(
            "SELECT metadata_items.id, metadata_items.title ",
            "FROM metadata_item_settings ",
            "JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id ",
            "JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id ",
            "WHERE metadata_items.library_section_id = 1"
        );
        let out = preprocess(input);
        // The unaliased JOIN should have AS mi now
        assert!(
            out.to_lowercase().contains("join metadata_items as mi"),
            "unaliased join should get AS mi, out={}",
            out
        );
        // All metadata_items.<col> should be mi.<col>
        assert!(
            !out.to_lowercase().contains("metadata_items."),
            "no bare metadata_items. refs should remain, out={}",
            out
        );
        // The aliased parents join must stay
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must be preserved, out={}",
            out
        );
        // mi. refs are present
        assert!(
            out.to_lowercase().contains("mi.id"),
            "mi.id should appear, out={}",
            out
        );
    }

    /// Legacy test: all joins already aliased — nothing should change (no unaliased join).
    #[test]
    fn compat_aliases__preprocess_no_rewrite_when_all_joins_aliased() {
        let input = concat!(
            "select metadata_items.id from metadata_item_settings ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id"
        );
        let out = preprocess(input);
        // When all joins are aliased there is no unaliased join to fix, so the
        // function should return the input unchanged (aside from other preprocess steps).
        // Both aliases must still be present.
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must stay, out={}",
            out
        );
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as grandparents"),
            "grandparents alias must stay, out={}",
            out
        );
        // No spurious AS mi should have been injected
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi should appear when all joins aliased, out={}",
            out
        );
    }

    /// Shape B (from-metadata_items root): no rewrite should happen — base table
    /// IS metadata_items so bare refs are valid in PostgreSQL.
    #[test]
    fn compat_aliases__preprocess_no_rewrite_for_shape_b_metadata_items_root() {
        let input = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2)"
        );
        let out = preprocess(input);
        // FROM is metadata_items, NOT metadata_item_settings, so no rewrite fires.
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi for shape-B query, out={}",
            out
        );
    }
}
mod tests {
    use super::super::{preprocess, Ordering, TEST_STRICT_PRAGMA_OVERRIDE};
    use crate::test_utils::env_lock;
    use crate::translate;

    #[test]
    fn subset_txn__keyword_begin_immediate() {
        let r = translate("BEGIN IMMEDIATE").unwrap();
        assert!(!r.sql.to_uppercase().contains("IMMEDIATE"));
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn subset_txn__keyword_begin_deferred() {
        let r = translate("BEGIN DEFERRED").unwrap();
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
    }

    #[test]
    fn subset_txn__keyword_begin_exclusive() {
        let r = translate("BEGIN EXCLUSIVE").unwrap();
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn subset_txn__keyword_end_to_commit() {
        let r = translate("END").unwrap();
        assert!(r.sql.to_uppercase().contains("COMMIT"));
    }

    #[test]
    fn subset_txn__keyword_release_without_savepoint_keyword() {
        let r = translate("RELEASE sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("RELEASE SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn subset_txn__keyword_rollback_transaction_to() {
        let r = translate("ROLLBACK TRANSACTION TO sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("ROLLBACK TO SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_glob_wildcard() {
        // GLOB is case-sensitive, so it is a regular expression match, not
        // the ILIKE it used to become.
        let r = translate("SELECT * FROM t WHERE name GLOB '*test*'").unwrap();
        assert!(r.sql.contains("name ~ '^.*test.*$'"), "{}", r.sql);
        assert!(!r.sql.to_uppercase().contains(" GLOB "));
    }

    #[test]
    fn subset_core__keyword_indexed_by_removed() {
        let r = translate("SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'")
            .unwrap();
        assert!(!r.sql.to_uppercase().contains("INDEXED BY"));
        assert!(r.sql.to_uppercase().contains("WHERE"));
    }

    #[test]
    fn subset_core__keyword_not_indexed_removed() {
        let r = translate("SELECT * FROM metadata_items NOT INDEXED WHERE id = 1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("NOT INDEXED"), "{}", r.sql);
        assert!(up.contains("WHERE"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_sqlite_master_replaced() {
        let r = translate("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
        assert!(
            r.sql.to_lowercase().contains("information_schema")
                || r.sql.to_lowercase().contains("pg_")
        );
        assert!(!r.sql.to_lowercase().contains("sqlite_master"));
    }

    #[test]
    fn subset_core__keyword_empty_in_list() {
        let r = translate("SELECT * FROM tags WHERE id IN ()").unwrap();
        assert!(!r.sql.contains("IN ()"));
        assert!(r.sql.to_uppercase().contains("IN") && r.sql.to_uppercase().contains("SELECT"));
    }

    #[test]
    fn subset_core__keyword_group_by_null_removed() {
        let r = translate("SELECT count(*) FROM metadata_items GROUP BY NULL").unwrap();
        assert!(!r.sql.to_uppercase().contains("GROUP BY NULL"));
    }

    #[test]
    fn subset_pragma__keyword_pragma_read_is_mapped_to_select() {
        let r = translate("PRAGMA foreign_keys").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SELECT 1 AS FOREIGN_KEYS"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_set_is_mapped_to_select_one() {
        let r = translate("PRAGMA journal_mode=WAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.JOURNAL_MODE"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_schema_prefix_is_supported() {
        let r = translate("PRAGMA main.busy_timeout = 5000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_unknown_is_removed() {
        let _guard = env_lock().lock().unwrap();
        TEST_STRICT_PRAGMA_OVERRIDE.store(-1, Ordering::Relaxed);
        let r = translate("PRAGMA this_is_unknown").unwrap();
        TEST_STRICT_PRAGMA_OVERRIDE.store(-1, Ordering::Relaxed);
        assert!(r.sql.trim().is_empty(), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_busy_timeout_read_uses_current_setting() {
        let r = translate("PRAGMA busy_timeout").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_synchronous_set_uses_set_config() {
        let r = translate("PRAGMA synchronous = FULL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("SYNCHRONOUS_COMMIT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_temp_store_set_uses_session_setting() {
        let r = translate("PRAGMA temp_store=MEMORY").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_temp_store_read_uses_current_setting() {
        let r = translate("PRAGMA temp_store").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_cache_size_set_uses_session_setting() {
        let r = translate("PRAGMA cache_size=-4000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_cache_size_read_uses_current_setting() {
        let r = translate("PRAGMA cache_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_locking_mode_set_uses_session_setting() {
        let r = translate("PRAGMA locking_mode=EXCLUSIVE").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_locking_mode_read_uses_current_setting() {
        let r = translate("PRAGMA locking_mode").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_wal_autocheckpoint_set_uses_session_setting() {
        let r = translate("PRAGMA wal_autocheckpoint=200").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_wal_autocheckpoint_read_uses_current_setting() {
        let r = translate("PRAGMA wal_autocheckpoint").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_mmap_size_set_uses_session_setting() {
        let r = translate("PRAGMA mmap_size=1048576").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_mmap_size_read_uses_current_setting() {
        let r = translate("PRAGMA mmap_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_page_size_set_uses_session_setting() {
        let r = translate("PRAGMA page_size=8192").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_page_size_read_uses_current_setting() {
        let r = translate("PRAGMA page_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_auto_vacuum_set_uses_session_setting() {
        let r = translate("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_auto_vacuum_read_uses_current_setting() {
        let r = translate("PRAGMA auto_vacuum").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_strict_mode_causes_translation_failure_for_unknown() {
        let _guard = env_lock().lock().unwrap();
        TEST_STRICT_PRAGMA_OVERRIDE.store(1, Ordering::Relaxed);
        let result = translate("PRAGMA totally_unknown_pragma");
        TEST_STRICT_PRAGMA_OVERRIDE.store(-1, Ordering::Relaxed);
        assert!(
            result.is_err(),
            "strict pragma mode should fail unknown PRAGMA"
        );
    }

    #[test]
    fn subset_core__keyword_explain_query_plan_rewritten_to_explain() {
        let r = translate("EXPLAIN QUERY PLAN SELECT * FROM t").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.starts_with("EXPLAIN "), "{}", r.sql);
        assert!(!up.contains("QUERY PLAN"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_vacuum_rewritten_to_select_one() {
        let r = translate("VACUUM").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_reindex_rewritten_to_select_one() {
        let r = translate("REINDEX").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_attach_database_rewritten_to_select_one() {
        let r = translate("ATTACH DATABASE 'x.db' AS aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_detach_database_rewritten_to_select_one() {
        let r = translate("DETACH DATABASE aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_analyze_sqlite_internal_rewritten_to_select_one() {
        let r = translate("ANALYZE sqlite_master").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    // ── Plex rebuilding its full-text index ────────────────────────────────
    //
    // Plex decides its search index needs rebuilding when it opens a library
    // an older server wrote, and tears the index down before building it back:
    // it drops eight triggers and the table per index, then issues CREATE
    // VIRTUAL TABLE ... USING fts4. None of that is expressible here, and none
    // of it should be. Under this shim `fts4_metadata_titles` and friends are
    // *views* the schema provides over the real tables, so letting the drops
    // through would delete the compatibility layer rather than an index Plex
    // owns -- PostgreSQL says as much, "fts4_metadata_titles" is not a table.
    //
    // So the whole rebuild is a no-op, the same answer VACUUM and REINDEX get.
    // Search is already served by other means; see simplify_fts_for_sqlite.
    //
    // Without this, Plex 1.43.4 against a library dumped from 1.43.0 fails
    // every one of these statements and exits before it serves:
    //   Unable to set up server: sqlite3_statement_backend::loadOne
    // The statements below are copied from that server's own log.

    #[test]
    fn subset_fts__rebuilding_the_search_index_does_not_drop_the_compatibility_views() {
        let r = translate("drop table if exists fts4_metadata_titles").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");

        let r = translate("drop table if exists fts4_tag_titles_icu").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_fts__dropping_one_of_the_index_triggers_is_a_no_op() {
        // PostgreSQL needs DROP TRIGGER <name> ON <table>; SQLite's form has no
        // table, so this reaches the server as "syntax error at end of input".
        let r = translate("drop trigger if exists fts4_metadata_titles_after_insert").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");

        let r = translate("drop trigger if exists fts4_tag_titles_before_delete_icu").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_fts__recreating_an_fts4_index_is_a_no_op() {
        // rewrite_virtual_tables knows fts5 and rtree. Plex writes fts4, which
        // fell through untouched and reached PostgreSQL as CREATE VIRTUAL
        // TABLE -- "syntax error at or near VIRTUAL".
        let r = translate(
            "CREATE VIRTUAL TABLE fts4_metadata_titles USING fts4(title, title_sort, original_title)",
        )
        .unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_fts__the_icu_index_carries_a_tokenizer_and_is_still_a_no_op() {
        // The ICU pair names a tokenizer that only Plex's SQLite has, so this
        // one cannot be translated even in principle.
        let r = translate(concat!(
            "CREATE VIRTUAL TABLE fts4_tag_titles_icu USING fts4(tag, ",
            "tokenize=collating 'root@colStrength=primary;colAlternate=shifted')"
        ))
        .unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_fts__recreating_one_of_the_index_triggers_is_a_no_op() {
        // The other half of the rebuild: having dropped the triggers that keep
        // the index in step with metadata_items, Plex writes them back. They
        // maintain an fts4 table that is a view here, so they have nothing to
        // maintain and PostgreSQL cannot parse their SQLite bodies anyway --
        // "syntax error at or near BEGIN", then at or near "new".
        let r = translate(concat!(
            "CREATE TRIGGER fts4_metadata_titles_before_delete BEFORE DELETE ON metadata_items ",
            "BEGIN DELETE FROM fts4_metadata_titles WHERE docid=old.rowid; END"
        ))
        .unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_fts__an_index_trigger_whose_body_holds_a_semicolon_is_still_one_statement() {
        // A trigger body carries its own statement terminator, so anything
        // that splits on semicolons first turns this into "SELECT 1; END".
        let r = translate(concat!(
            "CREATE TRIGGER fts4_tag_titles_after_insert AFTER INSERT ON tags ",
            "WHEN new.tag_type in (0,1,2,4,6,207,400) ",
            "BEGIN INSERT INTO fts4_tag_titles(docid, tag) VALUES(new.rowid, new.tag); END"
        ))
        .unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
        assert!(!r.sql.to_uppercase().contains("END"), "{}", r.sql);
    }

    #[test]
    fn subset_fts__a_drop_that_is_not_part_of_the_index_still_runs() {
        // The no-op is scoped to the shim's own fts4 objects. Swallowing DROP
        // generally would turn a real schema change into silence.
        let r = translate("drop table if exists metadata_items").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("DROP TABLE"), "{}", r.sql);

        let r = translate("drop trigger if exists metadata_items_after_insert").unwrap();
        assert!(r.sql.to_uppercase().contains("DROP TRIGGER"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_create_table_without_rowid_strict_stripped() {
        let r = translate("CREATE TABLE t(id INTEGER PRIMARY KEY) WITHOUT ROWID, STRICT").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("WITHOUT ROWID"), "{}", r.sql);
        assert!(!up.contains("STRICT"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains('~'), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_not_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name NOT REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains("!~"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_raise_function_rewritten_to_null() {
        let r = translate(
            "CREATE TRIGGER tr_bi BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT, 'boom'); END",
        )
        .unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("RAISE("), "{}", r.sql);
        assert!(up.contains("NULL"), "{}", r.sql);
    }

    // ── COLLATE stripping tests ──────────────────────────────────────────────

    #[test]
    fn compat_aliases__preprocess_strips_icu_root_in_order_by() {
        // This is the exact pattern that was causing parse failures in production
        let sql = "select metadata_items.id from metadata_items where metadata_items.library_section_id in (1) order by metadata_items.added_at desc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped, got: {}",
            r.sql
        );
        assert!(
            out.contains("order by"),
            "ORDER BY should still be present, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_icu_root_multiple_order_by_cols() {
        // Multiple COLLATE icu_root in same ORDER BY (from production query)
        let sql = "select id from metadata_items order by added_at desc, grandparents.title_sort collate icu_root asc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "All icu_root collations should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_nocase_handled_at_ast_level() {
        // COLLATE NOCASE is left by pre-parse stripping; the AST-level handler in
        // query.rs converts standalone NOCASE in ORDER BY to LOWER(expr).
        let sql = "SELECT * FROM t ORDER BY name COLLATE NOCASE ASC";
        let r = translate(sql).unwrap();
        let out = r.sql.to_uppercase();
        // The final output should not contain raw COLLATE NOCASE
        assert!(
            !out.contains("COLLATE NOCASE"),
            "COLLATE NOCASE should be handled (converted to LOWER or stripped), got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_rtrim_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE RTRIM";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE RTRIM"),
            "RTRIM collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_binary_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE BINARY";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE BINARY"),
            "BINARY collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_unicode_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE UNICODE";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE UNICODE"),
            "UNICODE collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_collate_not_stripped_inside_string() {
        // A string literal containing 'COLLATE icu_root' must not be touched
        let sql = "SELECT 'COLLATE icu_root' FROM t";
        let r = translate(sql).unwrap();
        assert!(
            r.sql.contains("COLLATE icu_root"),
            "Collate inside string literal should not be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_long_query_collate_icu_parse_succeeds() {
        // Regression test: long query with COLLATE icu_root used to fail at parse time
        let sql = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2) ",
            "and (metadata_items.metadata_type=4 and metadata_items.added_at>1000000) ",
            "order by metadata_items.added_at desc, ",
            "grandparents.title_sort collate icu_root asc, ",
            "parents.`index` IS NULL, parents.`index` asc, ",
            "metadata_items.`index` IS NULL, metadata_items.`index` asc, ",
            "metadata_items.title_sort collate icu_root asc, ",
            "metadata_items.id asc"
        );
        let r = translate(sql);
        assert!(
            r.is_ok(),
            "Long query with COLLATE icu_root should parse successfully, got: {:?}",
            r.err()
        );
        let out = r.unwrap().sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped from output, got: {}",
            out
        );
    }

    #[test]
    fn compat_aliases__preprocess_does_not_treat_backtick_limit_identifier_as_limit_clause() {
        let sql = "SELECT pqg.`id`,pqg.`limit`,pqg.`continuous` FROM play_queue_generators pqg WHERE pqg.`type`!=:C1";
        let out = preprocess(sql);
        let low = out.to_ascii_lowercase();
        assert!(
            !low.contains(" offset "),
            "identifier `limit` should not trigger LIMIT/OFFSET rewrite: {}",
            out
        );
        assert!(
            out.contains("`limit`"),
            "backtick identifier should remain unchanged in preprocess output: {}",
            out
        );
    }
}
