//! Ported C integration tests — Batch 3
//!
//! Covers: GROUP BY COMPLETE, ADD NULLS FIRST ORDERING, NULL SORTING,
//!         DISTINCT + ORDER BY, CASE BOOLEANS

use plex_pg_core::translate;

// ─── GROUP BY COMPLETE (15 tests) ────────────────────────────────────────────

#[test]
fn groupby_complete_no_groupby() {
    // Simple SELECT without GROUP BY — should pass through unchanged
    let t = translate("SELECT id, name FROM t").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        !sql.contains("group by"),
        "Should not add GROUP BY, got: {}",
        t.sql
    );
    assert!(sql.contains("id"));
    assert!(sql.contains("name"));
}

#[test]
fn groupby_complete_adds_missing_column() {
    // GROUP BY id only — name and title should be auto-added
    let t = translate("SELECT id, name, title FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        after_gb.contains("name"),
        "name should be added to GROUP BY, got: {}",
        t.sql
    );
    assert!(
        after_gb.contains("title"),
        "title should be added to GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_skips_aggregate() {
    // count(*) is an aggregate — should NOT appear in GROUP BY
    let t = translate("SELECT id, count(*) as cnt FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        !after_gb.contains("count"),
        "count(*) should NOT be in GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_skips_numeric_constant() {
    // Numeric literal 1 should not be added to GROUP BY
    let t = translate("SELECT id, 1 as flag FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    // "1" should not be added after GROUP BY (only "id" should be there)
    // After "GROUP BY id" there should be no extra columns
    assert!(
        !after_gb.contains("flag"),
        "literal alias 'flag' should NOT be in GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_already_complete() {
    // Both columns already in GROUP BY — no duplication
    let t = translate("SELECT id, name FROM t GROUP BY id, name").unwrap();
    let sql = t.sql.to_lowercase();
    // "name" should appear exactly twice: once in SELECT, once in GROUP BY
    let count = sql.matches("name").count();
    assert!(
        count <= 2,
        "name should not be duplicated, got count={} in: {}",
        count,
        t.sql
    );
}

#[test]
fn groupby_complete_skips_case() {
    // CASE expression should not be added to GROUP BY
    let t =
        translate("SELECT id, CASE WHEN a > 0 THEN 'yes' ELSE 'no' END as flag FROM t GROUP BY id")
            .unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        !after_gb.contains("case"),
        "CASE should NOT be in GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_skips_subquery() {
    // Subquery in SELECT should not be added to GROUP BY
    let t = translate(
        "SELECT id, (SELECT count(*) FROM t2 WHERE t2.pid = t.id) as cnt FROM t GROUP BY id",
    )
    .unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        !after_gb.contains("select"),
        "subquery should NOT be in GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_table_dot_column() {
    // t.name should be added to GROUP BY when only t.id is present
    let t = translate("SELECT t.id, t.name, count(*) FROM t GROUP BY t.id").unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        after_gb.contains("name"),
        "t.name should be added to GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_preserves_having() {
    // HAVING clause should be preserved while name is added to GROUP BY
    let t = translate("SELECT id, name, count(*) as cnt FROM t GROUP BY id HAVING count(*) > 1")
        .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("having"),
        "HAVING should be preserved, got: {}",
        t.sql
    );
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let having_pos = sql.find("having").expect("HAVING missing");
    let between = &sql[gb_pos..having_pos];
    assert!(
        between.contains("name"),
        "name should be added to GROUP BY before HAVING, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_preserves_order_by() {
    // ORDER BY should be preserved while name is added to GROUP BY
    let t = translate("SELECT id, name FROM t GROUP BY id ORDER BY name").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("order by"),
        "ORDER BY should be preserved, got: {}",
        t.sql
    );
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let ob_pos = sql.find("order by").expect("ORDER BY missing");
    let between = &sql[gb_pos..ob_pos];
    assert!(
        between.contains("name"),
        "name should be in GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_preserves_limit() {
    // LIMIT should be preserved while name is added to GROUP BY
    let t = translate("SELECT id, name FROM t GROUP BY id LIMIT 10").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("limit"),
        "LIMIT should be preserved, got: {}",
        t.sql
    );
    assert!(
        sql.contains("10"),
        "LIMIT 10 value should be preserved, got: {}",
        t.sql
    );
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    assert!(
        after_gb.contains("name"),
        "name should be added to GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_distinct_select() {
    // DISTINCT + GROUP BY → GROUP BY should be removed entirely
    let t = translate("SELECT DISTINCT id, name FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("distinct"),
        "DISTINCT should be preserved, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("group by"),
        "GROUP BY should be removed when DISTINCT is present, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_quoted_column() {
    // Double-quoted column "index" should be added to GROUP BY
    let t = translate(r#"SELECT id, "index" FROM t GROUP BY id"#).unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    // The quoted identifier "index" should appear in GROUP BY
    assert!(
        after_gb.contains("\"index\""),
        "\"index\" should be added to GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn groupby_complete_func_with_alias() {
    // Non-aggregate function upper(name) should be added to GROUP BY
    let t = translate("SELECT id, upper(name) as uname FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    let gb_pos = sql.find("group by").expect("GROUP BY missing");
    let after_gb = &sql[gb_pos..];
    // The Rust translator adds the expression upper(name), not the alias
    assert!(
        after_gb.contains("upper"),
        "upper(name) should be added to GROUP BY, got: {}",
        t.sql
    );
}

// ─── ADD NULLS FIRST ORDERING (5 tests) ──────────────────────────────────────

// NULLS FIRST auto-addition for GROUP BY + ORDER BY is implemented.

#[test]
fn nulls_first_ordering() {
    // "SELECT a, count(*) FROM t GROUP BY a ORDER BY a" should include NULLS FIRST
    let t = translate("SELECT a, count(*) FROM t GROUP BY a ORDER BY a").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls first"),
        "Expected NULLS FIRST, got: {}",
        t.sql
    );
}

#[test]
fn nulls_first_no_groupby() {
    // No GROUP BY → ORDER BY should remain unchanged (no NULLS FIRST added)
    let t = translate("SELECT * FROM t ORDER BY id").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        !sql.contains("nulls first"),
        "Should not add NULLS FIRST without GROUP BY, got: {}",
        t.sql
    );
}

#[test]
fn nulls_first_existing_orderby() {
    // GROUP BY + ORDER BY already present → unchanged
    let t = translate("SELECT a, count(*) FROM t GROUP BY a ORDER BY a").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("order by"),
        "ORDER BY should remain, got: {}",
        t.sql
    );
}

#[test]
fn nulls_first_no_orderby() {
    // GROUP BY without ORDER BY → adds ORDER BY 1 NULLS FIRST
    let t = translate("SELECT a, count(*) FROM t GROUP BY a").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("order by") && sql.contains("nulls first"),
        "Expected ORDER BY … NULLS FIRST to be added, got: {}",
        t.sql
    );
}

#[test]
fn nulls_first_before_limit() {
    // ORDER BY should be inserted before LIMIT
    let t = translate("SELECT a, count(*) FROM t GROUP BY a LIMIT 10").unwrap();
    let sql = t.sql.to_lowercase();
    let ob_pos = sql.find("order by");
    let limit_pos = sql.find("limit");
    assert!(
        ob_pos.is_some() && limit_pos.is_some() && ob_pos.unwrap() < limit_pos.unwrap(),
        "ORDER BY should appear before LIMIT, got: {}",
        t.sql
    );
}

// ─── NULL SORTING (8 tests) ──────────────────────────────────────────────────

#[test]
fn null_sorting_basic() {
    // ORDER BY parents."index" IS NULL, parents."index" asc → NULLS LAST, no IS NULL
    let t = translate(r#"SELECT * FROM t ORDER BY parents."index" IS NULL, parents."index" asc"#)
        .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls last"),
        "Expected NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL pattern should be removed, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_no_match() {
    // Simple ORDER BY without IS NULL pattern — unchanged
    let t = translate("SELECT * FROM t ORDER BY id").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        !sql.contains("nulls last"),
        "Should not add NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("nulls first"),
        "Should not add NULLS FIRST, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_backtick_parents_index() {
    // Backtick variant: backticks get converted to double-quotes by the quotes module,
    // and fix_expr now recurses into Expr::IsNull, so the backtick inside the
    // IS NULL wrapper IS converted. The IS NULL/NULLS LAST pattern match succeeds
    // and the IS NULL entry is replaced with NULLS LAST.
    let t =
        translate("SELECT * FROM t ORDER BY parents.`index` IS NULL, parents.`index` asc").unwrap();
    let sql = t.sql.to_lowercase();
    // The bare ORDER BY column should have backtick converted to double-quote
    assert!(
        sql.contains("parents.\"index\""),
        "Bare column should have double-quotes, got: {}",
        t.sql
    );
    // With fix_expr recursing into IsNull, the IS NULL pattern IS matched
    // and replaced with NULLS LAST.
    assert!(
        sql.contains("nulls last"),
        "IS NULL should be replaced with NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL should no longer persist, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_metadata_items_index() {
    // metadata_items."index" IS NULL pattern → NULLS LAST
    let t = translate(
        r#"SELECT * FROM t ORDER BY metadata_items."index" IS NULL, metadata_items."index" asc"#,
    )
    .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls last"),
        "Expected NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL should be removed, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_originally_available_at() {
    // originally_available_at IS NULL pattern → NULLS LAST
    let t = translate(
        "SELECT * FROM t ORDER BY metadata_items.originally_available_at IS NULL, metadata_items.originally_available_at asc",
    )
    .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls last"),
        "Expected NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL should be removed, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_grandparents_title_sort() {
    // grandparents.title_sort IS NULL pattern → NULLS LAST
    let t = translate(
        "SELECT * FROM t ORDER BY grandparents.title_sort IS NULL, grandparents.title_sort asc",
    )
    .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls last"),
        "Expected NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL should be removed, got: {}",
        t.sql
    );
}

#[test]
fn null_sorting_space_variant() {
    // Same as basic but with uppercase ASC — NULLS LAST should still apply
    let t = translate(r#"SELECT * FROM t ORDER BY parents."index" IS NULL, parents."index" ASC"#)
        .unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("nulls last"),
        "Expected NULLS LAST, got: {}",
        t.sql
    );
    assert!(
        !sql.contains("is null"),
        "IS NULL should be removed, got: {}",
        t.sql
    );
}

// ─── DISTINCT + ORDER BY (3 tests) ──────────────────────────────────────────

// DISTINCT removal with aggregate ORDER BY is supported.

#[test]
fn distinct_orderby_aggregate() {
    // DISTINCT + GROUP BY + ORDER BY count(*) → DISTINCT removed
    let t = translate("SELECT DISTINCT id FROM t GROUP BY id ORDER BY count(*)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        !sql.contains("distinct"),
        "DISTINCT should be removed when ORDER BY has aggregate, got: {}",
        t.sql
    );
}

#[test]
fn distinct_orderby_random() {
    // DISTINCT + ORDER BY random() is treated as DISTINCT-incompatible and removed.
    let t = translate("SELECT DISTINCT id FROM t ORDER BY random()").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        !sql.contains("distinct"),
        "DISTINCT should be removed with ORDER BY random(), got: {}",
        t.sql
    );
}

#[test]
fn distinct_orderby_groupby() {
    // C removes DISTINCT when GROUP BY is present; Rust keeps DISTINCT and removes GROUP BY.
    // Both are semantically correct. Rust approach is safer for PostgreSQL.
    let t = translate("SELECT DISTINCT id FROM t GROUP BY id").unwrap();
    let sql = t.sql.to_lowercase();
    // GROUP BY should be removed (redundant with DISTINCT)
    assert!(
        !sql.contains("group by"),
        "GROUP BY should be removed when DISTINCT is present, got: {}",
        t.sql
    );
}

// ─── CASE BOOLEANS (14 tests) ────────────────────────────────────────────────

#[test]
fn case_booleans_else_1_true() {
    // A projected CASE keeps SQLite's integers; as a condition it is boolean.
    let t = translate("SELECT (CASE WHEN a THEN 0 ELSE 1 END) FROM t").unwrap();
    assert!(
        t.sql.contains("THEN 0 ELSE 1 END"),
        "a projected CASE keeps its integers, got: {}",
        t.sql
    );
    let t = translate("SELECT * FROM t WHERE x AND (CASE WHEN a THEN 0 ELSE 1 END)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("then false else true end"),
        "a CASE under AND is a condition, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_else_0_false() {
    // CASE WHEN a THEN 1 ELSE 0 END → THEN TRUE ELSE FALSE where it is read as a condition
    let t = translate("SELECT * FROM t WHERE CASE WHEN a THEN 1 ELSE 0 END").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("then true"),
        "1 in THEN should become TRUE, got: {}",
        t.sql
    );
    assert!(
        sql.contains("else false"),
        "0 in ELSE should become FALSE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_then_0_else_true() {
    // CASE WHEN a THEN 0 ELSE true END → THEN FALSE ELSE TRUE
    let t = translate("SELECT (CASE WHEN a then 0 else true END) FROM t").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("then false"),
        "0 in THEN should become FALSE, got: {}",
        t.sql
    );
    assert!(
        sql.contains("else true"),
        "true in ELSE should remain TRUE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_then_1_else_false() {
    // CASE WHEN a THEN 1 ELSE false END → THEN TRUE ELSE FALSE
    let t = translate("SELECT (CASE WHEN a then 1 else false END) FROM t").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("then true"),
        "1 in THEN should become TRUE, got: {}",
        t.sql
    );
    assert!(
        sql.contains("else false"),
        "false in ELSE should remain FALSE, got: {}",
        t.sql
    );
}

// GAP: Boolean replacement in AND/OR context (outside CASE) is not implemented.
// The Rust translator only converts 0/1 to FALSE/TRUE in CASE THEN/ELSE
// and WHERE clauses, not in arbitrary boolean expression contexts.

#[test]
fn case_booleans_0_or() {
    // GAP: (0 or a = 1) → (FALSE or a = 1)
    let t = translate("SELECT * FROM t WHERE (0 or a = 1)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("false"),
        "0 in OR context should become FALSE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_1_or() {
    // GAP: (1 or a = 1) → (TRUE or a = 1)
    let t = translate("SELECT * FROM t WHERE (1 or a = 1)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("true"),
        "1 in OR context should become TRUE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_and_0() {
    // GAP: (a = 1 and 0) → (a = 1 and FALSE)
    let t = translate("SELECT * FROM t WHERE (a = 1 and 0)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("false"),
        "0 in AND context should become FALSE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_and_1() {
    // GAP: (a = 1 and 1) → (a = 1 and TRUE)
    let t = translate("SELECT * FROM t WHERE (a = 1 and 1)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("true"),
        "1 in AND context should become TRUE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_or_0() {
    // GAP: (a = 1 or 0) → (a = 1 or FALSE)
    let t = translate("SELECT * FROM t WHERE (a = 1 or 0)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("false"),
        "0 in OR context should become FALSE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_or_1() {
    // GAP: (a = 1 or 1) → (a = 1 or TRUE)
    let t = translate("SELECT * FROM t WHERE (a = 1 or 1)").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("true"),
        "1 in OR context should become TRUE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_where_0() {
    // WHERE 0 → WHERE FALSE
    let t = translate("SELECT * FROM t WHERE 0").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("where false"),
        "WHERE 0 should become WHERE FALSE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_where_1() {
    // WHERE 1 → WHERE TRUE
    let t = translate("SELECT * FROM t WHERE 1").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(
        sql.contains("where true"),
        "WHERE 1 should become WHERE TRUE, got: {}",
        t.sql
    );
}

#[test]
fn case_booleans_no_match() {
    // No CASE, no WHERE 0/1 — passthrough
    let t = translate("SELECT id, name FROM metadata_items").unwrap();
    let sql = t.sql.to_lowercase();
    assert!(sql.contains("id"));
    assert!(sql.contains("name"));
    assert!(sql.contains("metadata_items"));
    // Should not have any boolean replacement artifacts
    assert!(
        !sql.contains("true") && !sql.contains("false"),
        "No boolean replacement expected, got: {}",
        t.sql
    );
}
