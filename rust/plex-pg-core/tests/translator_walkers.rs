//! The translator's walkers (2026-10-16 audit, items 8, 11, 16, 17, 18):
//! every placeholder is numbered and every SQLite builtin rewritten in every
//! position, through sqlparser's visitor rather than a hand-written walk.
//!
//! Each test asserts the translated SQL and the parameter count.

use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

fn t(sql: &str) -> (String, usize) {
    let r = translate(sql).unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sql, e));
    Parser::parse_sql(&PostgreSqlDialect {}, &r.sql)
        .unwrap_or_else(|e| panic!("not PostgreSQL: `{}`: {}", r.sql, e));
    (r.sql, r.param_names.len())
}

// ── 8: placeholders in every position ───────────────────────────────────────

// Plex's `? IS NULL` was prepared with one parameter fewer than it bound
// (the live orig_q=2 / translated_params=1 mismatch).
#[test]
fn placeholder_under_is_null_and_is_not_null() {
    let (sql, n) = t("SELECT * FROM t WHERE ? IS NULL OR ? IS NOT NULL");
    assert_eq!(sql, "SELECT * FROM t WHERE $1 IS NULL OR $2 IS NOT NULL");
    assert_eq!(n, 2);
}

#[test]
fn placeholder_under_collate() {
    let (sql, n) = t("SELECT * FROM t WHERE title = ? COLLATE BINARY AND id = ?");
    // A later pass strips the collation; the slot under it was numbered first.
    assert_eq!(sql, "SELECT * FROM t WHERE title = $1 AND id = $2");
    assert_eq!(n, 2);
}

#[test]
fn placeholder_in_substring_and_ceil_and_floor() {
    let (sql, n) = t("SELECT substr(title, ?, ?), ceil(?), floor(?) FROM t");
    assert_eq!(
        sql,
        "SELECT SUBSTRING(title, $1, $2), CEIL($3), FLOOR($4) FROM t"
    );
    assert_eq!(n, 4);
}

#[test]
fn placeholder_in_window_partition_order_and_filter() {
    let (sql, n) = t(
        "SELECT count(*) FILTER (WHERE a = ?) OVER (PARTITION BY b + ? ORDER BY c - ?) FROM t WHERE d = ?",
    );
    assert_eq!(
        sql,
        "SELECT count(*) FILTER (WHERE a = $1) OVER (PARTITION BY b + $2 ORDER BY c - $3) FROM t WHERE d = $4"
    );
    assert_eq!(n, 4);
}

#[test]
fn placeholder_in_aggregate_order_by() {
    let (sql, n) = t("SELECT group_concat(a, ? ORDER BY b * ?) FROM t");
    assert_eq!(sql, "SELECT string_agg(a::TEXT, $1 ORDER BY b * $2) FROM t");
    assert_eq!(n, 2);
}

#[test]
fn placeholder_in_tuple() {
    let (sql, n) = t("SELECT * FROM t WHERE (a, b) IN ((?, ?), (?, ?))");
    assert_eq!(sql, "SELECT * FROM t WHERE (a, b) IN (($1, $2), ($3, $4))");
    assert_eq!(n, 4);
}

#[test]
fn placeholder_in_cte() {
    let (sql, n) = t("WITH q AS (SELECT id FROM t WHERE a = ?) SELECT * FROM q WHERE id > ?");
    assert_eq!(
        sql,
        "WITH q AS (SELECT id FROM t WHERE a = $1) SELECT * FROM q WHERE id > $2"
    );
    assert_eq!(n, 2);
}

#[test]
fn placeholder_in_explain() {
    let (sql, n) = t("EXPLAIN SELECT * FROM t WHERE a = ?");
    assert_eq!(sql, "EXPLAIN SELECT * FROM t WHERE a = $1");
    assert_eq!(n, 1);
}

#[test]
fn placeholder_in_on_conflict_do_update_set_and_where() {
    let (sql, n) =
        t("INSERT INTO t (a, b) VALUES (?, ?) ON CONFLICT (a) DO UPDATE SET b = ? WHERE t.c = ?");
    assert_eq!(
        sql,
        "INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT(a) DO UPDATE SET b = $3 WHERE t.c = $4"
    );
    assert_eq!(n, 4);
}

#[test]
fn placeholder_in_limit_and_offset() {
    let (sql, n) = t("SELECT * FROM t WHERE a = ? LIMIT ? OFFSET ?");
    assert_eq!(sql, "SELECT * FROM t WHERE a = $1 LIMIT $2 OFFSET $3");
    assert_eq!(n, 3);
}

#[test]
fn placeholder_explicit_index_is_its_slot_wherever_it_repeats() {
    let (sql, n) = t("SELECT * FROM t WHERE a = ?2 OR b = ?1 OR c = ?2 OR d = ?");
    assert_eq!(
        sql,
        "SELECT * FROM t WHERE a = $2 OR b = $1 OR c = $2 OR d = $3"
    );
    assert_eq!(n, 3);
}

#[test]
fn placeholder_at_and_dollar_names_are_named_slots_reused_when_repeated() {
    let r = translate("SELECT * FROM t WHERE a = @x AND b = $y AND c = @x AND d = :z").unwrap();
    assert_eq!(
        r.sql,
        "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $1 AND d = $3"
    );
    assert_eq!(
        r.param_names,
        vec![
            Some("x".to_string()),
            Some("y".to_string()),
            Some("z".to_string())
        ]
    );
}

#[test]
fn placeholder_in_set_operation_and_values() {
    let (sql, n) = t("SELECT ? AS v UNION ALL SELECT ? UNION ALL VALUES (?)");
    assert_eq!(
        sql,
        "SELECT $1 AS v UNION ALL SELECT $2 UNION ALL VALUES ($3)"
    );
    assert_eq!(n, 3);
}

#[test]
fn placeholder_in_case_arms_and_subqueries() {
    let (sql, n) = t(
        "SELECT CASE WHEN a = ? THEN ? ELSE (SELECT max(b) FROM u WHERE u.c = ?) END FROM t WHERE EXISTS (SELECT 1 FROM v WHERE v.d = ?)",
    );
    assert_eq!(
        sql,
        "SELECT CASE WHEN a = $1 THEN $2 ELSE (SELECT max(b) FROM u WHERE u.c = $3) END FROM t WHERE EXISTS (SELECT 1 FROM v WHERE v.d = $4)"
    );
    assert_eq!(n, 4);
}

// ── 16: a duplicate SET's dropped value keeps every bind slot ───────────────

#[test]
fn dedup_positional_duplicate_set_keeps_every_slot_and_the_last_value() {
    let r = translate("UPDATE t SET a = ?, a = ? WHERE id = ?").unwrap();
    assert_eq!(
        r.sql,
        "UPDATE t SET a = $2 WHERE (id = $3) AND (CAST(($1) AS TEXT) <> '' OR true)"
    );
    assert_eq!(r.param_names, vec![None, None, None]);
}

#[test]
fn dedup_named_before_positional_keeps_sqlites_slots() {
    let r = translate("UPDATE t SET a = :U1, b = ?, a = ? WHERE id = ?").unwrap();
    assert_eq!(
        r.sql,
        "UPDATE t SET b = $2, a = $3 WHERE (id = $4) AND (CAST(($1) AS TEXT) <> '' OR true)"
    );
    assert_eq!(
        r.param_names,
        vec![Some("U1".to_string()), None, None, None]
    );
}

// ── 11: SQLite builtins rewritten in every position ─────────────────────────

#[test]
fn builtin_under_is_null_and_is_not_null() {
    let (sql, n) = t(
        "SELECT * FROM t WHERE json_extract(extra_data, '$.a') IS NULL AND ifnull(b, '') IS NOT NULL",
    );
    assert_eq!(
        sql,
        "SELECT * FROM t WHERE jsonb_extract_path_text(extra_data::jsonb, 'a') IS NULL AND COALESCE(b, '') IS NOT NULL"
    );
    assert_eq!(n, 0);
}

#[test]
fn builtin_as_like_operand() {
    let (sql, n) = t(
        "SELECT * FROM t WHERE ifnull(title, '') LIKE ? AND json_extract(d, '$.x') NOT LIKE '%a%'",
    );
    assert_eq!(
        sql,
        "SELECT * FROM t WHERE COALESCE(title, '') ILIKE $1 AND jsonb_extract_path_text(d::jsonb, 'x') NOT ILIKE '%a%'"
    );
    assert_eq!(n, 1);
}

#[test]
fn builtin_in_left_and_right_outer_join_on() {
    let (sql, n) = t(
        "SELECT * FROM t LEFT JOIN u ON u.id = json_extract(t.d, '$.id') AND ifnull(u.k, 0) = ? RIGHT OUTER JOIN v ON v.at = strftime('%s', t.added_at)",
    );
    assert_eq!(
        sql,
        "SELECT * FROM t LEFT JOIN u ON u.id = jsonb_extract_path_text(t.d::jsonb, 'id') AND COALESCE(u.k, 0) = $1 RIGHT OUTER JOIN v ON v.at = EXTRACT(EPOCH FROM TO_TIMESTAMP(t.added_at))::BIGINT"
    );
    assert_eq!(n, 1);
}

#[test]
fn builtin_in_on_conflict_do_update_set_and_where() {
    let (sql, n) = t(
        "INSERT INTO t (a) VALUES (?) ON CONFLICT (a) DO UPDATE SET b = ifnull(excluded.b, ?) WHERE json_extract(t.d, '$.k') = ?",
    );
    assert_eq!(
        sql,
        "INSERT INTO t (a) VALUES ($1) ON CONFLICT(a) DO UPDATE SET b = COALESCE(excluded.b, $2) WHERE jsonb_extract_path_text(t.d::jsonb, 'k') = $3"
    );
    assert_eq!(n, 3);
}

#[test]
fn builtin_in_limit_and_offset() {
    let (sql, n) = t("SELECT * FROM t LIMIT ifnull(?, 10) OFFSET ifnull(?, 0)");
    assert_eq!(
        sql,
        "SELECT * FROM t LIMIT COALESCE($1, 10) OFFSET COALESCE($2, 0)"
    );
    assert_eq!(n, 2);
}

#[test]
fn builtin_in_subquery_case_arm_and_window() {
    let (sql, n) = t(
        "SELECT CASE WHEN ifnull(a, 0) > 0 THEN json_extract(d, '$.a') ELSE typeof(b) END, row_number() OVER (PARTITION BY ifnull(c, 0) ORDER BY strftime('%s', e)) FROM t WHERE id IN (SELECT id FROM u WHERE ifnull(x, 0) > ?)",
    );
    assert_eq!(
        sql,
        "SELECT CASE WHEN COALESCE(a, 0) > 0 THEN jsonb_extract_path_text(d::jsonb, 'a') ELSE plex.sqlite_typeof(b) END, row_number() OVER (PARTITION BY COALESCE(c, 0) ORDER BY EXTRACT(EPOCH FROM TO_TIMESTAMP(e))::BIGINT) FROM t WHERE id IN (SELECT id FROM u WHERE COALESCE(x, 0) > $1)"
    );
    assert_eq!(n, 1);
}

// `json_extract(extra_data, ?)` reached PostgreSQL as `json_extract()`: the
// rewrite drained the arguments before it bailed on the bound path.
#[test]
fn json_extract_with_a_bound_path_is_the_jsonpath_form() {
    let (sql, n) = t("SELECT json_extract(extra_data, ?) FROM t WHERE id = ?");
    assert_eq!(
        sql,
        "SELECT jsonb_path_query_first(extra_data::jsonb, $1::jsonpath)::jsonb #>> '{}' FROM t WHERE id = $2"
    );
    assert_eq!(n, 2);
}

#[test]
fn json_set_and_json_remove_with_a_bound_path_keep_their_arguments() {
    let (sql, n) = t("UPDATE t SET d = json_set(d, ?, ?), e = json_remove(e, ?) WHERE id = ?");
    assert_eq!(
        sql,
        "UPDATE t SET d = jsonb_set(d::jsonb, string_to_array(ltrim($1, '$.'), '.'), to_jsonb($2::TEXT), true), e = e::jsonb #- string_to_array(ltrim($3, '$.'), '.') WHERE id = $4"
    );
    assert_eq!(n, 4);
}

#[test]
fn strftime_with_a_bound_format_is_left_whole() {
    let (sql, n) = t("SELECT strftime(?, added_at) FROM t");
    assert_eq!(sql, "SELECT strftime($1, added_at) FROM t");
    assert_eq!(n, 1);
}

// ── 17: time functions and their modifiers ──────────────────────────────────

#[test]
fn time_modifiers_utc_and_localtime_are_no_ops() {
    let (sql, n) = t(
        "SELECT strftime('%s', added_at, 'utc'), strftime('%Y-%m-%d', added_at, 'localtime') FROM t",
    );
    assert_eq!(
        sql,
        "SELECT EXTRACT(EPOCH FROM TO_TIMESTAMP(added_at))::BIGINT, TO_CHAR(TO_TIMESTAMP(added_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD') FROM t"
    );
    assert_eq!(n, 0);
}

#[test]
fn time_modifiers_start_of_day_month_year_are_date_trunc() {
    let (sql, n) = t(
        "SELECT strftime('%Y-%m-%d', 'now', 'start of day'), datetime('now', 'start of month'), datetime('now', 'start of year')",
    );
    assert_eq!(
        sql,
        "SELECT TO_CHAR(date_trunc('day', NOW()) AT TIME ZONE 'UTC', 'YYYY-MM-DD'), date_trunc('month', NOW()), date_trunc('year', NOW())"
    );
    assert_eq!(n, 0);
}

#[test]
fn time_modifier_unixepoch_is_to_timestamp() {
    let (sql, n) = t("SELECT datetime(x, 'unixepoch') FROM t WHERE y > datetime(?, 'unixepoch')");
    assert_eq!(
        sql,
        "SELECT TO_CHAR(TO_TIMESTAMP(x) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') FROM t WHERE y > TO_CHAR(TO_TIMESTAMP($1) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')"
    );
    assert_eq!(n, 1);
}

// PMS asks a Live TV guide for a day's airings with the day bound as text:
// begins_at < strftime('%s', ?, '+1 days') with '2026-10-21', which SQLite
// reads as that date. TO_TIMESTAMP($1) takes only a number, so PostgreSQL
// refused every such query once the guide moved there (talos, 2026-10-21:
// 4,341 in ten minutes, "invalid input syntax for type double precision").
// A bound value reads as a date when its text is one, and as seconds since
// the epoch when it is a number, as before.
#[test]
fn a_bound_time_value_is_read_as_a_date_or_as_seconds() {
    let (sql, n) = t(
        "SELECT id FROM media_items WHERE begins_at < strftime('%s', ?, '+1 days') AND ends_at > strftime('%s', ?)",
    );
    let bound = |p: &str| {
        format!(
            "CASE WHEN {p}::TEXT ~ '^\\s*[-+]?[0-9]+(\\.[0-9]*)?\\s*$' THEN TO_TIMESTAMP({p}::TEXT::DOUBLE PRECISION) ELSE {p}::TEXT::TIMESTAMP AT TIME ZONE 'UTC' END"
        )
    };
    assert_eq!(
        sql,
        format!(
            "SELECT id FROM media_items WHERE begins_at < EXTRACT(EPOCH FROM {} + INTERVAL '1 days')::BIGINT AND ends_at > EXTRACT(EPOCH FROM {})::BIGINT",
            bound("$1"),
            bound("$2")
        )
    );
    assert_eq!(n, 2);
}

#[test]
fn time_modifier_interval_and_weekday() {
    let (sql, n) = t("SELECT datetime('now', '-7 days'), datetime('now', 'weekday 1')");
    assert_eq!(
        sql,
        "SELECT NOW() - INTERVAL '7 days', NOW() + ((1 - EXTRACT(DOW FROM NOW())::INT + 7) % 7) * INTERVAL '1 day'"
    );
    assert_eq!(n, 0);
}

#[test]
fn time_modifier_subsec_keeps_the_fraction() {
    let (sql, n) = t("SELECT strftime('%s', 'now', 'subsec'), time(x, 'subsec') FROM t");
    assert_eq!(
        sql,
        "SELECT EXTRACT(EPOCH FROM NOW()), TO_CHAR(TO_TIMESTAMP(x) AT TIME ZONE 'UTC', 'HH24:MI:SS.MS') FROM t"
    );
    assert_eq!(n, 0);
}

// `INTERVAL 'utc'` was what an unknown modifier became.
#[test]
fn time_unknown_modifier_leaves_the_call_as_written() {
    let (sql, n) =
        t("SELECT datetime('now', 'julianday'), datetime(x, 'bogus') FROM t WHERE id = ?");
    assert_eq!(
        sql,
        "SELECT datetime('now', 'julianday'), datetime(x, 'bogus') FROM t WHERE id = $1"
    );
    assert_eq!(n, 1);
}

#[test]
fn date_and_time_functions() {
    let (sql, n) = t("SELECT date(x), time(x), date('now', 'start of day') FROM t");
    assert_eq!(
        sql,
        "SELECT (TO_TIMESTAMP(x) AT TIME ZONE 'UTC')::DATE::TEXT, TO_CHAR(TO_TIMESTAMP(x) AT TIME ZONE 'UTC', 'HH24:MI:SS'), (date_trunc('day', NOW()) AT TIME ZONE 'UTC')::DATE::TEXT FROM t"
    );
    assert_eq!(n, 0);
}

// A non-%s format on an epoch column skipped TO_TIMESTAMP, so TO_CHAR got a
// bigint; a literal letter in a format is quoted for TO_CHAR.
#[test]
fn strftime_other_formats_wrap_the_epoch_column_and_map_every_conversion() {
    let (sql, n) = t(
        "SELECT strftime('%H:%M', created_at), strftime('%Y-%m-%dT%H:%M:%S', x), strftime('%j %W %f', x), strftime('%w', x), strftime('%u', x) FROM t",
    );
    assert_eq!(
        sql,
        "SELECT TO_CHAR(TO_TIMESTAMP(created_at) AT TIME ZONE 'UTC', 'HH24:MI'), TO_CHAR(TO_TIMESTAMP(x) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS'), TO_CHAR(TO_TIMESTAMP(x) AT TIME ZONE 'UTC', 'DDD WW SS.MS'), EXTRACT(DOW FROM TO_TIMESTAMP(x) AT TIME ZONE 'UTC')::INT::TEXT, EXTRACT(ISODOW FROM TO_TIMESTAMP(x) AT TIME ZONE 'UTC')::INT::TEXT FROM t"
    );
    assert_eq!(n, 0);
}

// ── 18: group_concat, substr, typeof, printf, hex, round, json_each ─────────

#[test]
fn group_concat_keeps_distinct_and_order_by() {
    let (sql, n) = t(
        "SELECT group_concat(DISTINCT x), group_concat(x, ',' ORDER BY y), group_concat(x) FROM t",
    );
    assert_eq!(
        sql,
        "SELECT string_agg(DISTINCT x::TEXT, ','), string_agg(x::TEXT, ',' ORDER BY y), string_agg(x::TEXT, ',') FROM t"
    );
    assert_eq!(n, 0);
}

#[test]
fn substr_negative_start_counts_from_the_end() {
    let (sql, n) =
        t("SELECT substr(x, -3), substr(x, -3, 2), substr(x, 2, 3), substr(x, ?) FROM t");
    assert_eq!(
        sql,
        "SELECT RIGHT(x, 3), LEFT(RIGHT(x, 3), 2), SUBSTRING(x, 2, 3), SUBSTRING(x, $1) FROM t"
    );
    assert_eq!(n, 1);
}

#[test]
fn printf_conversions_become_format_with_casts() {
    let (sql, n) = t("SELECT printf('%s-%d', name, ?), printf('%5.2f|%x|%%', v, n) FROM t");
    assert_eq!(
        sql,
        "SELECT format('%s-%s', name, $1::BIGINT), format('%5s|%s|%%', TO_CHAR(v::NUMERIC, 'FM999999999999999990.00'), TO_HEX(n::BIGINT)) FROM t"
    );
    assert_eq!(n, 1);
}

#[test]
fn hex_and_round() {
    let (sql, n) = t("SELECT hex(blob_col), hex(255), round(v, 2), round(v) FROM t");
    assert_eq!(
        sql,
        "SELECT UPPER(ENCODE(blob_col::BYTEA, 'hex')), UPPER(TO_HEX(255)), ROUND(v::NUMERIC, 2), round(v) FROM t"
    );
    assert_eq!(n, 0);
}

#[test]
fn json_each_with_a_path_takes_the_array_at_the_path() {
    let (sql, n) = t("SELECT value FROM json_each(x, '$.a') WHERE value = ?");
    assert_eq!(
        sql,
        "SELECT value::TEXT FROM jsonb_array_elements_text(x::jsonb #> string_to_array('a', ',')) AS json_each WHERE value = $1"
    );
    assert_eq!(n, 1);
}

#[test]
fn json_each_value_and_key_references_resolve_to_the_functions_columns() {
    let (sql, n) =
        t("SELECT key, value FROM json_each(x) WHERE json_each.value = ? AND json_each.key > 0");
    assert_eq!(
        sql,
        "SELECT (key - 1) AS key, value::TEXT FROM json_array_elements_text(x::JSON) WITH ORDINALITY AS json_each (value, key) WHERE json_each.value = $1 AND (json_each.key - 1) > 0"
    );
    assert_eq!(n, 1);
}

// ── 8, the shapes the rewrites branch produces before numbering ─────────────
//
// GLOB is spelled SIMILAR TO before parsing, `IS <expr>` becomes IS [NOT]
// DISTINCT FROM, and `UPDATE ... LIMIT ?` keeps the Update's own `limit`
// until an AST rewrite after numbering. The preprocessor on this branch
// still rewrites those forms as text, so the walker is called directly on
// the parsed statement.

fn numbered(dialect: &dyn sqlparser::dialect::Dialect, sql: &str) -> (String, Vec<Option<String>>) {
    let mut stmt = Parser::parse_sql(dialect, sql)
        .unwrap_or_else(|e| panic!("parse failed for `{}`: {}", sql, e))
        .remove(0);
    let mut names = Vec::new();
    plex_pg_core::placeholders::transform(&mut stmt, &mut names);
    (stmt.to_string(), names)
}

#[test]
fn placeholder_under_similar_to() {
    let (sql, names) = numbered(
        &SQLiteDialect {},
        "SELECT * FROM t WHERE title SIMILAR TO ? AND id = ?",
    );
    assert_eq!(sql, "SELECT * FROM t WHERE title SIMILAR TO $1 AND id = $2");
    assert_eq!(names, vec![None, None]);
}

#[test]
fn placeholder_under_is_distinct_from_and_is_not_distinct_from() {
    let (sql, names) = numbered(
        &SQLiteDialect {},
        "SELECT * FROM t WHERE a IS DISTINCT FROM ? AND ? IS NOT DISTINCT FROM b AND c = ?",
    );
    assert_eq!(
        sql,
        "SELECT * FROM t WHERE a IS DISTINCT FROM $1 AND $2 IS NOT DISTINCT FROM b AND c = $3"
    );
    assert_eq!(names, vec![None, None, None]);
}

#[test]
fn placeholder_in_an_update_statements_limit() {
    let (sql, names) = numbered(
        &sqlparser::dialect::MySqlDialect {},
        "UPDATE t SET a = ? WHERE b = ? LIMIT ?",
    );
    assert_eq!(sql, "UPDATE t SET a = $1 WHERE b = $2 LIMIT $3");
    assert_eq!(names, vec![None, None, None]);
}
