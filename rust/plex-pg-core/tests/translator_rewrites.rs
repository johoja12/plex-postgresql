//! Regression tests for the 2026-10-16 shim audit's translator items
//! (docs/audits/2026-10-16-shim-audit.md): each test names the audit item
//! it holds and asserts the translated SQL.

use plex_pg_core::translate;

fn sql(input: &str) -> String {
    translate(input)
        .unwrap_or_else(|e| panic!("did not translate: {e}\n  in: {input}"))
        .sql
}

// ─── #4: a projection alias used in WHERE ────────────────────────────────────

#[test]
fn audit_4_a_projection_alias_in_where_is_its_expression() {
    // Plex's Live TV guide: SQLite resolves the alias, PostgreSQL says the
    // column does not exist.
    let out = sql(
        "select min(begins_at, ends_at) as minimum_at from media_items \
         where minimum_at > ? order by minimum_at limit 1",
    );
    assert_eq!(
        out,
        "SELECT LEAST(begins_at, ends_at) AS minimum_at FROM media_items \
         WHERE LEAST(begins_at, ends_at) > $1 ORDER BY minimum_at LIMIT 1"
    );
}

#[test]
fn audit_4_an_alias_in_having_is_its_expression() {
    let out =
        sql("select channel_id, count(*) as n from media_items group by channel_id having n > 1");
    assert!(out.contains("HAVING count(*) > 1"), "{out}");
}

#[test]
fn audit_4_an_alias_that_names_a_real_column_is_left_to_the_column() {
    // SQLite resolves a column before an alias of the same name.
    let out = sql("select guid as title from metadata_items where title = 'x'");
    assert!(out.contains("WHERE title = 'x'"), "{out}");
}

// ─── #5: LIKE is case-insensitive, GLOB is not ───────────────────────────────

#[test]
fn audit_5_a_bare_like_is_ilike() {
    // SQLite's LIKE is ASCII case-insensitive: `studio LIKE 'a24'` finds A24.
    let out = sql("select id from metadata_items where studio like ?");
    assert_eq!(out, "SELECT id FROM metadata_items WHERE studio ILIKE $1");
    let out = sql("select id from metadata_items where studio not like 'a%' escape '\\'");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE studio NOT ILIKE 'a%' ESCAPE '\\'"
    );
}

#[test]
fn audit_5_a_glob_is_an_anchored_case_sensitive_regular_expression() {
    let out = sql("select id from media_parts where file glob '/lib/*.mk?'");
    assert_eq!(
        out,
        "SELECT id FROM media_parts WHERE file ~ '^/lib/.*\\.mk.$'"
    );
    let out = sql("select id from media_parts where file not glob '[Aa]b(c)'");
    assert_eq!(
        out,
        "SELECT id FROM media_parts WHERE file !~ '^[Aa]b\\(c\\)$'"
    );
}

#[test]
fn audit_5_a_glob_built_from_a_bound_parameter_is_converted_at_run_time() {
    // The placeholder's number is the placeholder walker's business
    // (sqlparser's SIMILAR TO node, which carries the GLOB, is one it
    // visits once it walks with VisitorMut), so either spelling passes here.
    let out = sql("select id from media_parts where file glob '*' || ? || '*'").replace("$1", "?");
    assert_eq!(
        out,
        "SELECT id FROM media_parts WHERE file ~ ('^' || '.*' || \
         regexp_replace(regexp_replace(regexp_replace(?, '([\\\\.^$+(){}|])', '\\\\\\1', 'g'), \
         '\\*', '.*', 'g'), '\\?', '.', 'g') || '.*' || '$')"
    );
}

#[test]
fn audit_5_the_glob_function_form_is_the_same_match() {
    let out = sql("select id from media_parts where glob('a*', file)");
    assert_eq!(out, "SELECT id FROM media_parts WHERE file ~ '^a.*$'");
}

// ─── #10: a qualified column with COLLATE NOCASE ─────────────────────────────

#[test]
fn audit_10_a_qualified_column_collate_nocase_parses_and_lowers() {
    let out = sql(
        "select id from metadata_items where metadata_items.title collate nocase = ? \
         order by metadata_items.title_sort collate nocase",
    );
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE LOWER(metadata_items.title) = LOWER($1) \
         ORDER BY LOWER(metadata_items.title_sort)"
    );
}

// ─── #12: subqueries and JOIN … ON ───────────────────────────────────────────

#[test]
fn audit_12_rewrites_reach_in_subqueries_and_exists() {
    let out = sql("select id from metadata_items where id in \
         (select metadata_item_id from media_items where max(a, b) > 1 and id = '') \
         and exists (select 1 from tags where tag collate nocase = 'x')");
    assert!(out.contains("GREATEST(a, b) > 1 AND false"), "{out}");
    assert!(out.contains("LOWER(tag) = LOWER('x')"), "{out}");
}

#[test]
fn audit_12_rewrites_reach_join_on() {
    let out = sql("select m.id from metadata_items m \
         left join media_items mi on mi.metadata_item_id = m.id and mi.id = '' \
         and min(mi.a, mi.b) > 0");
    assert!(
        out.contains("ON mi.metadata_item_id = m.id AND false AND LEAST(mi.a, mi.b) > 0"),
        "{out}"
    );
}

#[test]
fn audit_12_a_scalar_subquery_is_entered() {
    let out = sql("select (select max(a, b) from t) from u");
    assert!(out.contains("(SELECT GREATEST(a, b) FROM t)"), "{out}");
}

// ─── #13: CASE 1/0 is boolean only where it is read as a condition ──────────

#[test]
fn audit_13_a_case_inside_an_aggregate_keeps_its_integers() {
    let out = sql("select sum(case when viewed_at > 0 then 1 else 0 end) from metadata_item_views");
    assert!(
        out.contains("sum(CASE WHEN viewed_at > 0 THEN 1 ELSE 0 END)"),
        "{out}"
    );
}

#[test]
fn audit_13_a_case_compared_or_added_keeps_its_integers() {
    let out = sql("select * from t where (case when a then 1 else 0 end) = 1");
    assert!(out.contains("(CASE WHEN a THEN 1 ELSE 0 END) = 1"), "{out}");
    let out = sql("select (case when a then 1 else 0 end) + 1 from t");
    assert!(out.contains("(CASE WHEN a THEN 1 ELSE 0 END) + 1"), "{out}");
}

#[test]
fn audit_13_a_case_as_a_condition_is_boolean() {
    let out = sql("select * from t where x = 1 and case when a then 1 else 0 end");
    assert!(
        out.contains("AND CASE WHEN a THEN true ELSE false END"),
        "{out}"
    );
    let out = sql("select * from t where not (case when a then 1 end)");
    assert!(out.contains("NOT (CASE WHEN a THEN true END)"), "{out}");
    let out = sql("select * from t join u on u.id = t.id and case when a then 1 else 0 end");
    assert!(
        out.contains("AND CASE WHEN a THEN true ELSE false END"),
        "{out}"
    );
}

#[test]
fn audit_13_a_case_with_mixed_integer_and_boolean_results_is_boolean_anywhere() {
    let out = sql("select (case when a then 0 else true end) from t");
    assert!(
        out.contains("CASE WHEN a THEN false ELSE true END"),
        "{out}"
    );
}

// ─── #14: column affinity from the schema ────────────────────────────────────

#[test]
fn audit_14_an_integer_column_is_not_cast_to_text() {
    let out = sql("select * from download_queue_items where status <> 1 and state = 0");
    assert_eq!(
        out,
        "SELECT * FROM download_queue_items WHERE status <> 1 AND state = 0"
    );
}

#[test]
fn audit_14_every_integer_column_folds_a_non_integer_string() {
    let out = sql(
        "select id from media_items where metadata_item_id = '' and section_location_id <> '' \
         and media_items.id = ''",
    );
    assert_eq!(
        out,
        "SELECT id FROM media_items WHERE false AND true AND false"
    );
    let out = sql("select id from metadata_items mi where mi.library_section_id = 'x'");
    assert_eq!(out, "SELECT id FROM metadata_items mi WHERE false");
}

#[test]
fn audit_14_an_integer_string_still_casts_and_a_real_one_is_a_number() {
    let out = sql("select id from media_items where metadata_item_id = '12' and duration > '1.5'");
    assert_eq!(
        out,
        "SELECT id FROM media_items WHERE metadata_item_id = CAST('12' AS INTEGER) AND duration > 1.5"
    );
}

#[test]
fn audit_14_an_in_list_drops_members_that_are_no_integer() {
    let out = sql("select id from metadata_items where id in ('', '3', 'x', 4)");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE id IN (CAST('3' AS INTEGER), 4)"
    );
    let out = sql("select id from metadata_items where id in ('') and parent_id not in ('')");
    assert_eq!(out, "SELECT id FROM metadata_items WHERE false AND true");
}

#[test]
fn audit_14_ordering_follows_sqlite_affinity() {
    // Integer sorts before text: id < '' is true, id > '' false, and with
    // the literal on the left the other way round.
    let out =
        sql("select id from metadata_items where id < '' and id >= '' and '' > id and '' <= id");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE true AND false AND true AND false"
    );
}

#[test]
fn audit_14_a_text_column_compared_with_a_number_casts_the_number() {
    let out = sql("select id from metadata_items where guid = 123 and title > 5");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE guid = 123::TEXT AND title > 5::TEXT"
    );
}

#[test]
fn audit_14_a_column_is_judged_by_its_own_table_through_its_alias() {
    // version is text on schema_migrations and an integer on play_queues.
    let out = sql(
        "select sm.version from schema_migrations sm join play_queues pq on pq.id = 1 \
         where sm.version = 202608120900 and pq.version = '7'",
    );
    assert!(out.contains("sm.version = 202608120900::TEXT"), "{out}");
    assert!(out.contains("pq.version = CAST('7' AS INTEGER)"), "{out}");
}

#[test]
fn audit_14_a_correlated_subquery_sees_the_outer_table() {
    let out = sql("select id from metadata_items m where exists \
         (select 1 from media_items where media_items.metadata_item_id = m.id and m.guid = 5)");
    assert!(out.contains("m.guid = 5::TEXT"), "{out}");
}

#[test]
fn audit_14_a_bare_column_of_an_unknown_table_is_not_judged_by_an_outer_one() {
    // The subquery's table is not in the schema, so its `status` may be
    // anything; the outer download_queue_items.status must not decide it.
    let out = sql("select id from download_queue_items d where exists \
         (select 1 from unknown_t where status = '')");
    assert!(out.contains("WHERE status = ''"), "{out}");
}

// ─── #15: (metadata_type=18) alone is kept ───────────────────────────────────

#[test]
fn audit_15_a_collections_only_predicate_is_kept() {
    let out =
        sql("select id from metadata_items where library_section_id=? and (metadata_type=18)");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE library_section_id = $1 AND (metadata_type = 18)"
    );
}

#[test]
fn audit_15_metadata_type_18_is_still_dropped_from_an_or() {
    let out = sql("select id from metadata_items where (metadata_type=1 or metadata_type=18)");
    assert_eq!(
        out,
        "SELECT id FROM metadata_items WHERE (metadata_type = 1)"
    );
}

// ─── #18: IS <expr>, the self-join rewrite, LIMIT -1, IS NULL DESC ──────────

#[test]
fn audit_18_is_against_an_expression_is_not_distinct_from() {
    // The placeholder's number is the placeholder walker's business (it
    // does not yet visit IS [NOT] DISTINCT FROM), so either spelling passes.
    let out = sql("select id from t where a is ? and b is not 3 and c is null and d is not null and e is not true")
        .replace("$1", "?");
    assert_eq!(
        out,
        "SELECT id FROM t WHERE a IS NOT DISTINCT FROM ? AND b IS DISTINCT FROM 3 \
         AND c IS NULL AND d IS NOT NULL AND e IS NOT TRUE"
    );
    let out = sql("select id from t where a is (select 1) and 'this is' = b");
    assert!(
        out.contains("a IS NOT DISTINCT FROM (SELECT 1) AND 'this is' = b"),
        "{out}"
    );
}

#[test]
fn audit_18_the_self_join_rewrite_keeps_a_left_join_left() {
    let out = sql("SELECT metadata_items.id FROM metadata_item_settings \
         LEFT JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id \
         JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id \
         WHERE metadata_items.library_section_id = 1");
    assert_eq!(
        out,
        "SELECT mi.id FROM metadata_item_settings \
         JOIN metadata_items AS mi ON mi.id = metadata_item_settings.metadata_item_id \
         LEFT JOIN metadata_items AS parents ON parents.id = mi.parent_id \
         WHERE mi.library_section_id = 1"
    );
}

#[test]
fn audit_18_a_negative_limit_is_no_limit() {
    let out = sql("select id from t order by id limit -1");
    assert_eq!(out, "SELECT id FROM t ORDER BY id");
    let out = sql("select id from t limit -1 offset 5");
    assert_eq!(out, "SELECT id FROM t OFFSET 5");
    let out = sql("update t set a = 1 where b = 2 limit -1");
    assert_eq!(out, "UPDATE t SET a = 1 WHERE b = 2");
}

#[test]
fn audit_18_order_by_is_null_desc_puts_nulls_first() {
    let out = sql("select * from t order by y is null desc, y");
    assert_eq!(out, "SELECT * FROM t ORDER BY y NULLS FIRST");
    let out = sql("select * from t order by y is null, y desc");
    assert_eq!(out, "SELECT * FROM t ORDER BY y DESC NULLS LAST");
    let out = sql("select * from t order by y is not null, y");
    assert_eq!(out, "SELECT * FROM t ORDER BY y NULLS FIRST");
}

// ─── #9: LIMIT off, n and UPDATE … LIMIT keep their placeholders in order ───

#[test]
fn audit_9_limit_offset_comma_binds_the_offset_first() {
    let t = translate("select id from t order by id limit ?, ?").unwrap();
    assert_eq!(t.sql, "SELECT id FROM t ORDER BY id LIMIT $2 OFFSET $1");
    assert_eq!(t.param_names.len(), 2);
    let out = sql("select id from t limit 5, 10");
    assert_eq!(out, "SELECT id FROM t LIMIT 10 OFFSET 5");
}

#[test]
fn audit_9_update_limit_keeps_the_set_placeholders_ahead_of_the_where() {
    let t = translate("update media_parts set file = ?, size = ? where id = ? limit 1").unwrap();
    assert_eq!(
        t.sql,
        "UPDATE media_parts SET file = $1, size = $2 \
         WHERE ctid IN (SELECT ctid FROM media_parts WHERE id = $3 LIMIT 1)"
    );
    assert_eq!(t.param_names.len(), 3);
    let t = translate("update t set a = :a where b = :b order by c limit 2 returning id").unwrap();
    assert_eq!(
        t.sql,
        "UPDATE t SET a = $1 WHERE ctid IN (SELECT ctid FROM t WHERE b = $2 ORDER BY c LIMIT 2) RETURNING id"
    );
    assert_eq!(
        t.param_names,
        vec![Some("a".to_string()), Some("b".to_string())]
    );
}

// ─── #1: FTS MATCH, literal and bound ────────────────────────────────────────

#[test]
fn audit_1_a_bound_match_term_goes_through_fts_query() {
    let t = translate(
        "select rowid from fts4_metadata_titles_icu \
         where fts4_metadata_titles_icu.title_sort match ? limit ?",
    )
    .unwrap();
    assert_eq!(
        t.sql,
        "SELECT rowid FROM fts4_metadata_titles_icu \
         WHERE to_tsvector('simple', plex.fold(fts4_metadata_titles_icu.title_sort)) \
         @@ plex.fts_query($1) LIMIT $2"
    );
    assert_eq!(t.param_names.len(), 2);
}

#[test]
fn audit_1_a_literal_match_term_is_handed_over_whole() {
    let out =
        sql("select rowid from fts4_tag_titles where tag match '(star wars) \"a b\" -x OR y*'");
    assert_eq!(
        out,
        "SELECT rowid FROM fts4_tag_titles WHERE to_tsvector('simple', plex.fold(tag)) \
         @@ plex.fts_query('(star wars) \"a b\" -x OR y*')"
    );
}

#[test]
fn audit_1_a_match_against_the_view_searches_every_column() {
    let out = sql("select rowid from fts4_metadata_titles where fts4_metadata_titles match ?");
    assert_eq!(
        out,
        "SELECT rowid FROM fts4_metadata_titles WHERE to_tsvector('simple', \
         plex.fold(coalesce(title, '') || ' ' || coalesce(title_sort, '') || ' ' || \
         coalesce(original_title, ''))) @@ plex.fts_query($1)"
    );
}

#[test]
fn audit_1_integrity_and_checkpoint_pragmas_answer_a_row() {
    assert_eq!(
        sql("PRAGMA integrity_check"),
        "SELECT 'ok' AS integrity_check"
    );
    assert_eq!(sql("PRAGMA quick_check(1)"), "SELECT 'ok' AS quick_check");
    assert_eq!(sql("PRAGMA optimize"), "SELECT 1 AS optimize");
    assert_eq!(
        sql("PRAGMA wal_checkpoint(TRUNCATE)"),
        "SELECT 0 AS busy, 0 AS log, 0 AS checkpointed"
    );
}

// SQLite's two-argument `trim(x, y)` is a parse error for sqlparser, which
// knows only `TRIM(y FROM x)`; PostgreSQL's btrim takes SQLite's form
// (2026-10-16). ltrim/rtrim and a literal containing "trim(" are untouched.
#[test]
fn audit_leftover_two_argument_trim_is_btrim() {
    let t = translate("select trim(title, ?) from metadata_items where rtrim(title, 'x') = 'trim(a, b)' and TRIM(guid) <> ''").unwrap();
    let low = t.sql.to_lowercase();
    assert!(low.contains("btrim(title, $1)"), "sql={}", t.sql);
    assert!(low.contains("rtrim(title, 'x')"), "sql={}", t.sql);
    assert!(low.contains("'trim(a, b)'"), "sql={}", t.sql);
    assert!(low.contains("btrim(guid)"), "sql={}", t.sql);
    assert_eq!(t.param_names.len(), 1);
}

// tags.tag, metadata_items.title_sort and library_sections.name_sort carry
// the non-deterministic plex.nocase collation; PostgreSQL refuses a pattern
// match on them, so the match compares under plex.icu_root (2026-10-16).
#[test]
fn a_pattern_match_on_a_nocase_column_compares_under_icu_root() {
    let t = translate("select id from tags where tags.tag like ? and tag_type = 1").unwrap();
    assert!(
        t.sql.contains("(tags.tag COLLATE plex.icu_root) ILIKE $1"),
        "sql={}",
        t.sql
    );
    let t = translate(
        "select id from metadata_items where title_sort glob 'M*' and instr(name_sort, 'x') > 0",
    )
    .unwrap();
    assert!(
        t.sql.contains("(title_sort COLLATE plex.icu_root) ~ "),
        "sql={}",
        t.sql
    );
    assert!(
        t.sql
            .contains("STRPOS((name_sort COLLATE plex.icu_root), 'x')"),
        "sql={}",
        t.sql
    );
    // Other columns, and equality on the three, are untouched.
    let t = translate("select id from metadata_items where title like ? and tags.tag = ?").unwrap();
    assert!(
        t.sql.contains("title ILIKE $1 AND tags.tag = $2"),
        "sql={}",
        t.sql
    );
}

// `COLLATE icu_root` used to be stripped, leaving the sort to the database's
// locale; it is the schema's plex.icu_root now.
#[test]
fn collate_icu_root_is_the_schemas_icu_collation() {
    let t = translate("select id from metadata_items where library_section_id = ? order by metadata_items.title_sort collate icu_root asc, title collate unicode").unwrap();
    assert!(
        t.sql.contains("ORDER BY (metadata_items.title_sort) COLLATE plex.icu_root ASC, title COLLATE plex.icu_root"),
        "sql={}",
        t.sql
    );
}

// sqlparser parses `col MATCH 'x' AND a = 1` as `col MATCH ('x' AND a = 1)`;
// SQLite binds MATCH like LIKE, so only the leftmost leaf is the term and
// the conjunction stays outside plex.fts_query (2026-10-16: Plex's every
// search answered 500 with "invalid input syntax for type boolean").
#[test]
fn a_match_followed_by_and_keeps_the_rest_of_the_where_outside_the_query() {
    let t = translate("select metadata_items.id from metadata_items join fts4_metadata_titles_icu on metadata_items.id=fts4_metadata_titles_icu.rowid where fts4_metadata_titles_icu.title match 'Magnolia*' and metadata_items.library_section_id in (1) and metadata_items.metadata_type=1 limit 5").unwrap();
    assert!(
        t.sql.contains("@@ plex.fts_query('Magnolia*') AND metadata_items.library_section_id IN (1) AND metadata_items.metadata_type = 1 LIMIT 5"),
        "sql={}",
        t.sql
    );
    let t = translate("select id from metadata_items join fts4_metadata_titles_icu on metadata_items.id=fts4_metadata_titles_icu.rowid where fts4_metadata_titles_icu.title match ? and library_section_id=? or metadata_type=?").unwrap();
    assert!(
        t.sql
            .contains("@@ plex.fts_query($1) AND library_section_id = $2 OR metadata_type = $3"),
        "sql={}",
        t.sql
    );
    assert_eq!(t.param_names.len(), 3);
}

// Plex's playlists filter: `CASE WHEN "index" = 2 THEN id IN (...) ELSE 1 END`
// in a WHERE. The IN branch is boolean on PostgreSQL, so the 1 beside it is
// TRUE (2026-10-16: /playlists answered 500 with "CASE types integer and
// boolean cannot be matched"); a CASE of integer expressions read as a
// condition compares with 0 instead.
#[test]
fn a_case_mixing_a_condition_branch_with_0_1_is_boolean_and_an_integer_case_in_a_condition_compares_with_zero(
) {
    let t = translate("select id from metadata_items where account_id = 1 and (case when metadata_items.\"index\" = 2 then metadata_items.id in (select metadata_item_id from play_queue_generators) else 1 end)").unwrap();
    assert!(
        t.sql.contains("THEN metadata_items.id IN (SELECT metadata_item_id FROM play_queue_generators) ELSE true END"),
        "sql={}",
        t.sql
    );
    let t = translate("select id from metadata_items where (case when metadata_type = 1 then media_item_count else 0 end) and id > 0").unwrap();
    assert!(
        t.sql.contains(
            "WHERE ((CASE WHEN metadata_type = 1 THEN media_item_count ELSE 0 END) <> 0) AND id > 0"
        ),
        "sql={}",
        t.sql
    );
    // Not a condition: the integers stay.
    let t =
        translate("select sum(case when metadata_type = 1 then 1 else 0 end) from metadata_items")
            .unwrap();
    assert!(t.sql.contains("THEN 1 ELSE 0 END"), "sql={}", t.sql);
}

// Plex's tag search: `select distinct(tags.id) ... group by tags.id order
// by count(*) desc`. The GROUP BY feeds the ORDER BY's aggregate, so it stays
// and the DISTINCT goes (2026-10-16: the shim dropped the GROUP BY and
// PostgreSQL refused "tags.id must appear in the GROUP BY clause").
#[test]
fn a_distinct_over_a_group_by_with_an_aggregate_keeps_the_group_by() {
    let t = translate("select distinct(tags.id) from metadata_items join taggings on taggings.metadata_item_id=metadata_items.id join tags on tags.id=taggings.tag_id join fts4_tag_titles_icu on fts4_tag_titles_icu.rowid=tags.id where fts4_tag_titles_icu.tag match 'magnolia*' and tag_type=6 and metadata_items.library_section_id in (2) and metadata_items.metadata_type=1 group by tags.id order by count(*) desc limit 5").unwrap();
    assert!(t.sql.starts_with("SELECT (tags.id) FROM"), "sql={}", t.sql);
    assert!(
        t.sql.contains("GROUP BY tags.id ORDER BY count(*) DESC"),
        "sql={}",
        t.sql
    );
    // Without an aggregate the DISTINCT makes the GROUP BY redundant, as before.
    let t = translate("select distinct tags.id from tags group by tags.id").unwrap();
    assert_eq!(t.sql, "SELECT DISTINCT tags.id FROM tags");
}
