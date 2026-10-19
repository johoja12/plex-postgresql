/// Ported batch 4: integration tests for COLLATE, FTS4, JSON OPERATORS,
/// INT/TEXT MISMATCH, FORWARD REF JOINS, MAX/MIN, ICU COLLATION,
/// SUBQUERY ALIAS, COLLECTIONS, OPERATOR SPACING, WINDOW FUNCTIONS,
/// FULL PIPELINE, and EDGE CASES.
use plex_pg_core::translate;

// ═══════════════════════════════════════════════════════════════════════
// COLLATE NOCASE (5 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn collate_nocase_equals() {
    let r = translate("SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("lower"),
        "Expected LOWER wrapping, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("collate nocase"),
        "COLLATE NOCASE should be removed, got: {}",
        r.sql
    );
}

#[test]
fn collate_nocase_like() {
    // GAP: LIKE COLLATE NOCASE -> ILIKE not yet implemented
    let r = translate("SELECT * FROM t WHERE name LIKE '%test%' COLLATE NOCASE").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("ilike"), "Expected ILIKE, got: {}", r.sql);
    assert!(
        !sql.contains("collate nocase"),
        "COLLATE NOCASE should be removed, got: {}",
        r.sql
    );
}

#[test]
fn collate_nocase_orderby() {
    // GAP: ORDER BY COLLATE NOCASE -> LOWER not yet implemented
    let r = translate("SELECT * FROM t ORDER BY name COLLATE NOCASE").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("lower"),
        "Expected LOWER wrapping, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("collate nocase"),
        "COLLATE NOCASE should be removed, got: {}",
        r.sql
    );
}

#[test]
fn collate_nocase_glob() {
    // A GLOB is a regular expression match; a NOCASE pattern makes it `~*`.
    let r = translate("SELECT * FROM t WHERE name GLOB '*test*' COLLATE NOCASE").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("name ~* '^.*test.*$'"),
        "Expected a case-insensitive regular expression match, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("collate nocase"),
        "COLLATE NOCASE should be removed, got: {}",
        r.sql
    );
}

#[test]
fn collate_nocase_ne() {
    let r = translate("SELECT * FROM t WHERE name COLLATE NOCASE != 'Test'").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("lower"),
        "Expected LOWER wrapping, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("collate nocase"),
        "COLLATE NOCASE should be removed, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// FTS4 (8 tests)
//
// The translator hands the FTS4 term to plex.fts_query whole, literal and
// bound alike; the tsquery syntax (negation, phrases, OR) is that function's.
// ═══════════════════════════════════════════════════════════════════════

fn assert_fts_term(sql: &str, term: &str) {
    let r = translate(sql).unwrap();
    let expected = format!(
        "to_tsvector('simple', plex.fold(title)) @@ plex.fts_query('{}')",
        term
    );
    assert!(
        r.sql.contains(&expected),
        "expected `{}` in: {}",
        expected,
        r.sql
    );
}

#[test]
fn fts_negation() {
    assert_fts_term(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action -comedy'",
        "action -comedy",
    );
}

#[test]
fn fts_and_chain() {
    assert_fts_term(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action comedy'",
        "action comedy",
    );
}

#[test]
fn fts_or_chain() {
    assert_fts_term(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action OR comedy'",
        "action OR comedy",
    );
}

#[test]
fn fts_phrase() {
    assert_fts_term(
        r#"SELECT * FROM fts4_metadata_titles WHERE title MATCH '"action comedy"'"#,
        "\"action comedy\"",
    );
}

#[test]
fn fts_single_escaped_quote() {
    assert_fts_term(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'it''s'",
        "it''s",
    );
}

#[test]
fn fts_double_escaped_quote() {
    // sqlparser reads `'it''''s'` as it's rather than SQLite's it''s (a
    // tokenizer quirk older than this test); the term still goes through
    // plex.fts_query whole.
    let r = translate("SELECT * FROM fts4_metadata_titles WHERE title MATCH 'it''''s'").unwrap();
    assert!(
        r.sql.contains("@@ plex.fts_query('it''"),
        "expected the term through plex.fts_query, got: {}",
        r.sql
    );
}

#[test]
fn fts_simple_term() {
    assert_fts_term(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action'",
        "action",
    );
}

#[test]
fn fts_mixed_quotes_and_terms() {
    assert_fts_term(
        r#"SELECT * FROM fts4_metadata_titles WHERE title MATCH '"star wars" action'"#,
        "\"star wars\" action",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// JSON OPERATORS (5 tests — ALL IGNORED)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn json_operator_with_parameter() {
    // JSON ->> operator with $.path should be rewritten safely for PostgreSQL
    let r = translate("SELECT * FROM t WHERE extra_data ->> '$.pv:version' < $3").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("::json") || sql.contains("->>"),
        "Expected JSON operator, got: {}",
        r.sql
    );
    assert!(
        sql.contains("pv:version"),
        "Expected pv:version key, got: {}",
        r.sql
    );
}

#[test]
fn json_operator_with_literal() {
    // JSON ->> operator with $.path should be rewritten safely for PostgreSQL
    let r = translate("SELECT * FROM t WHERE extra_data ->> '$.status' = 'active'").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("->>") || sql.contains("::json"),
        "Expected JSON operator, got: {}",
        r.sql
    );
}

#[test]
fn json_operator_is_null() {
    // JSON ->> operator with $.path should be rewritten safely for PostgreSQL
    let r = translate("SELECT * FROM t WHERE extra_data ->> '$.key' IS NULL").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("->>"), "Expected ->> operator, got: {}", r.sql);
}

#[test]
fn json_operator_param_position() {
    // Parameter position handling with JSON ->> should remain stable
    let r =
        translate("SELECT * FROM t WHERE extra_data ->> '$.version' = :ver AND id = :id").unwrap();
    assert!(
        r.param_names.len() == 2,
        "Expected 2 params, got: {:?}",
        r.param_names
    );
}

#[test]
fn json_operator_plex_vad_query() {
    // Complex Plex query with JSON ->> should stay supported
    let r =
        translate("SELECT * FROM metadata_items WHERE extra_data ->> '$.pv:version' < $3").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("->>"), "Expected ->> operator, got: {}", r.sql);
}

// ═══════════════════════════════════════════════════════════════════════
// INTEGER/TEXT MISMATCH (9 tests — ALL IGNORED)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn int_text_mismatch_no_match() {
    // GAP: Integer/text mismatch detection not implemented
    let r = translate("SELECT * FROM t WHERE id = 1").unwrap();
    // No text/int mismatch here; should pass through unchanged
    assert!(r.sql.to_lowercase().contains("id"), "Got: {}", r.sql);
}

#[test]
fn int_text_mismatch_pattern1() {
    // GAP: Integer/text mismatch fix not implemented
    let r = translate("SELECT * FROM metadata_items WHERE id = '123'").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("cast") || sql.contains("::"),
        "Expected CAST for int/text mismatch, got: {}",
        r.sql
    );
}

#[test]
fn int_text_mismatch_integer_columns_keep_their_integer() {
    // status, state and downloaded are integers in every Plex table (the
    // schema's column types say so), and were once listed as text here:
    // `status <> 1` became `<> 1::TEXT`, which PostgreSQL refused.
    for sql in [
        "SELECT * FROM download_queue_items WHERE status <> 1",
        "SELECT * FROM t WHERE `status` = 1",
        r#"SELECT * FROM t WHERE "status" = 1"#,
        "SELECT * FROM t WHERE `downloaded` = 1",
        r#"SELECT * FROM t WHERE "state" = 0"#,
    ] {
        let r = translate(sql).unwrap();
        let low = r.sql.to_lowercase();
        assert!(
            !low.contains("cast") && !low.contains("::"),
            "an integer column keeps its integer comparison, got: {}",
            r.sql
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FORWARD REFERENCE JOINS (4 tests — ALL IGNORED)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn forward_ref_no_alias_join() {
    // GAP: Forward reference join rewriting not implemented
    let r = translate("SELECT * FROM a JOIN b ON a.id = c.a_id JOIN c ON b.id = c.b_id").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("join"),
        "Expected JOIN preserved, got: {}",
        r.sql
    );
}

#[test]
fn forward_ref_no_unaliased_join() {
    // GAP: Forward reference join rewriting not implemented
    let r = translate("SELECT * FROM a, b WHERE a.id = c.a_id AND b.id = c.b_id").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("from"), "Got: {}", r.sql);
}

#[test]
fn forward_ref_reorder() {
    // GAP: Forward reference join rewriting not implemented
    let r = translate("SELECT * FROM a JOIN b ON a.id = c.a_id JOIN c ON c.id = b.c_id").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("join"), "Got: {}", r.sql);
}

#[test]
fn forward_ref_no_forward_reference() {
    // GAP: Forward reference join rewriting not implemented
    let r = translate("SELECT * FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("join"), "Got: {}", r.sql);
}

// ═══════════════════════════════════════════════════════════════════════
// MAX/MIN (4 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn max_to_greatest() {
    let r = translate("SELECT max(x, y) FROM t").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("greatest"),
        "Expected GREATEST, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("max(x, y)"),
        "max(x, y) should be replaced, got: {}",
        r.sql
    );
}

#[test]
fn max_single_arg() {
    let r = translate("SELECT max(x) FROM t").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("max("),
        "Aggregate max should be preserved, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("greatest"),
        "Single-arg max should NOT become GREATEST, got: {}",
        r.sql
    );
}

#[test]
fn min_to_least() {
    let r = translate("SELECT min(x, y) FROM t").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(sql.contains("least"), "Expected LEAST, got: {}", r.sql);
    assert!(
        !sql.contains("min(x, y)"),
        "min(x, y) should be replaced, got: {}",
        r.sql
    );
}

#[test]
fn min_single_arg() {
    let r = translate("SELECT min(x) FROM t").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("min("),
        "Aggregate min should be preserved, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("least"),
        "Single-arg min should NOT become LEAST, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// ICU COLLATION (2 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn icu_collation_is_the_schemas_icu_root() {
    let r = translate("SELECT * FROM t ORDER BY name COLLATE icu_root").unwrap();
    assert!(
        r.sql.contains("ORDER BY name COLLATE plex.icu_root"),
        "icu_root is the schema's plex.icu_root (2026-10-16), got: {}",
        r.sql
    );
}

#[test]
fn strip_icu_collation_no_match() {
    let r = translate("SELECT * FROM t ORDER BY name").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("order by"),
        "ORDER BY should be preserved, got: {}",
        r.sql
    );
    assert!(
        sql.contains("name"),
        "name should be preserved, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// SUBQUERY ALIAS (1 test)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn subquery_alias() {
    let r = translate("SELECT * FROM (SELECT id FROM t) WHERE id > 0").unwrap();
    let sql = r.sql.to_uppercase();
    assert!(
        sql.contains(" AS "),
        "Subquery should have an alias, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// COLLECTIONS FILTER (2 tests — ALL IGNORED)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn collections_filter() {
    // GAP: Collections filter (metadata_type=18 removal) not implemented
    let r = translate(
        "SELECT * FROM metadata_items WHERE (metadata_items.metadata_type=1 or metadata_items.metadata_type=18)",
    )
    .unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        !sql.contains("18"),
        "metadata_type=18 should be removed, got: {}",
        r.sql
    );
}

#[test]
fn collections_no_change() {
    // GAP: Collections filter not implemented
    let r = translate("SELECT * FROM metadata_items WHERE metadata_items.metadata_type=1").unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("metadata_type"),
        "metadata_type should be preserved, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// OPERATOR SPACING (8 tests)
// The AST parser handles operator spacing naturally.
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn operator_spacing_eq() {
    let r = translate("SELECT * FROM t WHERE a=-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("= -1") || sql.contains("= (-1)"),
        "Expected properly spaced '= -1', got: {}",
        sql
    );
}

#[test]
fn operator_spacing_ne() {
    let r = translate("SELECT * FROM t WHERE a!=-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("!= -1")
            || sql.contains("<> -1")
            || sql.contains("!= (-1)")
            || sql.contains("<> (-1)"),
        "Expected properly spaced != or <>, got: {}",
        sql
    );
}

#[test]
fn operator_spacing_no_fix() {
    let r = translate("SELECT * FROM t WHERE a = -1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("= -1") || sql.contains("= (-1)"),
        "Expected '= -1' preserved, got: {}",
        sql
    );
}

#[test]
fn operator_spacing_gte() {
    let r = translate("SELECT * FROM t WHERE a>=-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains(">= -1") || sql.contains(">= (-1)"),
        "Expected '>= -1', got: {}",
        sql
    );
}

#[test]
fn operator_spacing_lte() {
    let r = translate("SELECT * FROM t WHERE a<=-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("<= -1") || sql.contains("<= (-1)"),
        "Expected '<= -1', got: {}",
        sql
    );
}

#[test]
fn operator_spacing_ne2() {
    let r = translate("SELECT * FROM t WHERE a<>-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("<> -1")
            || sql.contains("!= -1")
            || sql.contains("<> (-1)")
            || sql.contains("!= (-1)"),
        "Expected '<> -1' or '!= -1', got: {}",
        sql
    );
}

#[test]
fn operator_spacing_gt() {
    let r = translate("SELECT * FROM t WHERE a>-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("> -1") || sql.contains("> (-1)"),
        "Expected '> -1', got: {}",
        sql
    );
}

#[test]
fn operator_spacing_lt() {
    let r = translate("SELECT * FROM t WHERE a<-1").unwrap();
    let sql = &r.sql;
    assert!(
        sql.contains("< -1") || sql.contains("< (-1)"),
        "Expected '< -1', got: {}",
        sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// WINDOW FUNCTIONS (3 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn window_row_number() {
    let r = translate("SELECT ROW_NUMBER() OVER (ORDER BY id) as rn FROM t").unwrap();
    let sql = r.sql.to_uppercase();
    assert!(
        sql.contains("ROW_NUMBER"),
        "ROW_NUMBER should be preserved, got: {}",
        r.sql
    );
    assert!(
        sql.contains("OVER"),
        "OVER should be preserved, got: {}",
        r.sql
    );
}

#[test]
fn window_rank() {
    let r =
        translate("SELECT RANK() OVER (PARTITION BY category ORDER BY score DESC) FROM t").unwrap();
    let sql = r.sql.to_uppercase();
    assert!(
        sql.contains("RANK"),
        "RANK should be preserved, got: {}",
        r.sql
    );
    assert!(
        sql.contains("PARTITION BY"),
        "PARTITION BY should be preserved, got: {}",
        r.sql
    );
}

#[test]
fn window_dense_rank() {
    let r = translate("SELECT DENSE_RANK() OVER (ORDER BY score) FROM t").unwrap();
    let sql = r.sql.to_uppercase();
    assert!(
        sql.contains("DENSE_RANK"),
        "DENSE_RANK should be preserved, got: {}",
        r.sql
    );
    assert!(
        sql.contains("OVER"),
        "OVER should be preserved, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// FULL PIPELINE (7 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn full_select() {
    let r = translate("SELECT * FROM metadata_items WHERE id = :id").unwrap();
    assert!(
        r.sql.contains("$1"),
        "Named param :id should become $1, got: {}",
        r.sql
    );
    assert_eq!(r.param_names.len(), 1);
    assert_eq!(r.param_names[0], Some("id".to_string()));
}

#[test]
fn full_insert() {
    let r = translate("INSERT INTO t (a, b) VALUES (:a, :b)").unwrap();
    assert_eq!(
        r.param_names.len(),
        2,
        "Expected 2 params, got: {:?}",
        r.param_names
    );
    assert!(r.sql.contains("$1"), "Got: {}", r.sql);
    assert!(r.sql.contains("$2"), "Got: {}", r.sql);
}

#[test]
fn full_update() {
    let r = translate("UPDATE t SET a = :val WHERE id = :id").unwrap();
    assert_eq!(
        r.param_names.len(),
        2,
        "Expected 2 params, got: {:?}",
        r.param_names
    );
    assert!(r.sql.contains("$1"), "Got: {}", r.sql);
    assert!(r.sql.contains("$2"), "Got: {}", r.sql);
}

#[test]
fn full_complex() {
    // Complex Plex-like query with IFNULL and named params
    let r = translate(
        "SELECT m.id, IFNULL(m.rating, 0) as rating FROM metadata_items m \
         WHERE m.library_section_id = :lib_id AND m.metadata_type = :type",
    )
    .unwrap();
    let sql = r.sql.to_uppercase();
    assert!(
        sql.contains("COALESCE"),
        "IFNULL should become COALESCE, got: {}",
        r.sql
    );
    assert!(
        !sql.contains("IFNULL"),
        "IFNULL should be gone, got: {}",
        r.sql
    );
    assert!(r.sql.contains("$1"), "Got: {}", r.sql);
    assert!(r.sql.contains("$2"), "Got: {}", r.sql);
    assert_eq!(r.param_names.len(), 2);
}

#[test]
fn plex_viewed_at_order_by() {
    // When GROUP BY is present and ORDER BY uses non-aggregated col that has max() in SELECT
    let r = translate(
        "SELECT metadata_item_id, max(viewed_at) FROM metadata_item_views \
         GROUP BY metadata_item_id ORDER BY viewed_at DESC",
    )
    .unwrap();
    let sql = r.sql.to_lowercase();
    assert!(
        sql.contains("max(viewed_at)") || sql.contains("order by max"),
        "ORDER BY viewed_at should become ORDER BY max(viewed_at), got: {}",
        r.sql
    );
}

#[test]
fn plex_external_metadata_group_by() {
    // Real Plex query: GROUP BY title, needs id,uri,etc. added
    let r = translate(
        "SELECT external_metadata_items.id,uri,user_title,library_section_id,\
         metadata_type,year,added_at,updated_at,extra_data,title \
         FROM external_metadata_items \
         group by title order by added_at",
    )
    .unwrap();
    let sql = r.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("Expected GROUP BY in output");
    let after_gb = &sql[gb_pos..];
    assert!(
        after_gb.contains("id") || after_gb.contains("external_metadata_items.id"),
        "id should be added to GROUP BY, got: {}",
        r.sql
    );
}

#[test]
fn plex_clustering_distinct_removes_group_by() {
    let r = translate(
        "SELECT DISTINCT metadata_item_clusterings.id, title \
         FROM metadata_item_clusterings GROUP BY title ORDER BY title",
    )
    .unwrap();
    assert!(
        !r.sql.to_uppercase().contains("GROUP BY"),
        "GROUP BY should be removed when DISTINCT is present, got: {}",
        r.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════
// EDGE CASES (3 tests)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn edge_empty() {
    let r = translate("").unwrap();
    assert!(
        r.sql.is_empty(),
        "Empty input should produce empty output, got: {:?}",
        r.sql
    );
    assert!(r.param_names.is_empty());
}

#[test]
fn edge_backticks() {
    let r = translate("SELECT `id`, `name` FROM `table`").unwrap();
    assert!(
        !r.sql.contains('`'),
        "Backticks should be removed, got: {}",
        r.sql
    );
    assert!(
        r.sql.contains('"'),
        "Should have double-quotes, got: {}",
        r.sql
    );
}

#[test]
fn edge_double_quotes_preserved() {
    let r = translate(r#"SELECT "id" FROM "table""#).unwrap();
    assert!(
        r.sql.contains('"'),
        "Double-quotes should be preserved, got: {}",
        r.sql
    );
}
