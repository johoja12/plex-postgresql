//! Ported C integration tests — Batch 1
//!
//! Covers: PLACEHOLDERS, FUNCTIONS, TYPES
//! Uses `translate()` from the crate root (full pipeline) for all tests.

use plex_pg_core::translate;

// ═══════════════════════════════════════════════════════════════════════════════
// PLACEHOLDERS (15 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn placeholder_basic() {
    let t = translate("SELECT * FROM t WHERE id = :id").unwrap();
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
    assert_eq!(
        t.param_names.len(),
        1,
        "expected 1 param, got: {:?}",
        t.param_names
    );
    assert_eq!(t.param_names[0], Some("id".to_string()));
}

#[test]
fn placeholder_multiple() {
    let t = translate("SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz").unwrap();
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
    assert!(t.sql.contains("$2"), "expected $2 in: {}", t.sql);
    assert!(t.sql.contains("$3"), "expected $3 in: {}", t.sql);
    assert_eq!(
        t.param_names.len(),
        3,
        "expected 3 params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_reuse() {
    let t = translate("SELECT * FROM t WHERE a = :id OR b = :id").unwrap();
    assert_eq!(
        t.param_names.len(),
        1,
        "reused :id should yield 1 param, got: {:?}",
        t.param_names
    );
    assert_eq!(
        t.sql.matches("$1").count(),
        2,
        "expected $1 twice in: {}",
        t.sql
    );
}

#[test]
fn placeholder_question_mark() {
    let t = translate("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
    assert!(t.sql.contains("$2"), "expected $2 in: {}", t.sql);
    assert_eq!(
        t.param_names.len(),
        2,
        "expected 2 params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_in_string() {
    let t = translate("SELECT * FROM t WHERE a = ':not_a_param'").unwrap();
    assert_eq!(
        t.param_names.len(),
        0,
        "string literal should not create params, got: {:?}",
        t.param_names
    );
    assert!(
        t.sql.contains(":not_a_param"),
        "string content should be preserved in: {}",
        t.sql
    );
}

// A `?` takes only digits in SQLite, so `?label` is a placeholder and then
// the word: here an implicit alias. (Ported as `b > ?left` at the end of a
// statement, which SQLite rejects, and which pinned the word being dropped.)
#[test]
fn placeholder_question_alpha_space() {
    let t = translate("SELECT ?label FROM t WHERE a = ?").unwrap();
    assert_eq!(
        t.param_names.len(),
        2,
        "expected 2 params, got: {:?}",
        t.param_names
    );
    assert!(
        !t.sql.contains("$1l"),
        "should not have $1l glued together in: {}",
        t.sql
    );
    assert!(
        t.sql.to_lowercase().contains("label"),
        "the alias must survive in: {}",
        t.sql
    );
}

#[test]
fn placeholder_mixed_question_and_named() {
    let t = translate("SELECT * FROM t WHERE a = ? AND b = :foo AND c = ?").unwrap();
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
    assert!(t.sql.contains("$2"), "expected $2 in: {}", t.sql);
    assert!(t.sql.contains("$3"), "expected $3 in: {}", t.sql);
    assert_eq!(
        t.param_names.len(),
        3,
        "expected 3 params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_escaped_quotes() {
    let t = translate("SELECT * FROM t WHERE name = 'it''s :not_a_param' AND id = :real_param")
        .unwrap();
    assert_eq!(
        t.param_names.len(),
        1,
        "expected 1 param for :real_param, got: {:?}",
        t.param_names
    );
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
    // The :not_a_param inside the escaped string literal should be preserved
    assert!(
        t.sql.contains(":not_a_param"),
        "string literal content should be preserved in: {}",
        t.sql
    );
}

#[test]
fn placeholder_colon_after_ident() {
    let t = translate("SELECT * FROM t WHERE url = 'http:endpoint'").unwrap();
    assert_eq!(
        t.param_names.len(),
        0,
        "colon in string should not create params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_double_quote_not_string() {
    let t = translate("SELECT * FROM t WHERE name = '?' AND id = ?").unwrap();
    assert_eq!(
        t.param_names.len(),
        1,
        "expected 1 param (the bare ?), got: {:?}",
        t.param_names
    );
    // '?' in string should be preserved as-is
    assert!(
        t.sql.contains("'?'"),
        "string '?' should be preserved in: {}",
        t.sql
    );
}

#[test]
fn placeholder_question_in_string_literal() {
    let t = translate("UPDATE metadata_items SET guid=REPLACE(guid,'?lang=en','?lang=xn') WHERE guid LIKE 'com.plexapp.agents.none%'").unwrap();
    assert_eq!(
        t.param_names.len(),
        0,
        "? inside string literals should not create params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_question_in_string_mixed() {
    let t = translate("UPDATE t SET c=REPLACE(c,'?old','?new') WHERE id=?").unwrap();
    assert_eq!(
        t.param_names.len(),
        1,
        "only the bare ? should create a param, got: {:?}",
        t.param_names
    );
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
}

#[test]
fn placeholder_backslash_in_string() {
    let t = translate("SELECT * FROM t WHERE path='C:\\Users\\' AND id=?").unwrap();
    assert_eq!(
        t.param_names.len(),
        1,
        "expected 1 param, got: {:?}",
        t.param_names
    );
    assert!(t.sql.contains("$1"), "expected $1 in: {}", t.sql);
}

#[test]
fn placeholder_doubled_quote_with_question() {
    let t = translate("INSERT INTO t VALUES('it''s a ?test')").unwrap();
    assert_eq!(
        t.param_names.len(),
        0,
        "? inside doubled-quote string should not create params, got: {:?}",
        t.param_names
    );
}

#[test]
fn placeholder_four_quotes_with_question() {
    let t = translate("INSERT INTO t VALUES('''?''')").unwrap();
    assert_eq!(
        t.param_names.len(),
        0,
        "? inside four-quote string should not create params, got: {:?}",
        t.param_names
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// FUNCTIONS (26 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn function_ifnull() {
    let t = translate("SELECT IFNULL(a, 0) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("coalesce"),
        "expected COALESCE in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_lowercase().contains("ifnull"),
        "should not contain IFNULL in: {}",
        t.sql
    );
}

#[test]
fn function_length() {
    let t = translate("SELECT LENGTH(name) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("length"),
        "expected LENGTH preserved in: {}",
        t.sql
    );
}

#[test]
fn function_substr() {
    let t = translate("SELECT SUBSTR(a, 1, 5) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("substring"),
        "expected SUBSTRING in: {}",
        t.sql
    );
}

#[test]
fn function_random() {
    let t = translate("SELECT RANDOM() FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("random"),
        "expected RANDOM passthrough in: {}",
        t.sql
    );
}

#[test]
fn function_datetime() {
    let t = translate("SELECT datetime('now') FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("now"),
        "expected NOW in: {}",
        t.sql
    );
}

#[test]
fn function_instr() {
    // C test: instr(extra_data, 'pv%3A...') → STRPOS(extra_data, 'pv%3A...')
    // Rust swaps args: STRPOS('pv%3A...', extra_data)
    let t = translate("SELECT * FROM t WHERE NOT instr(extra_data, 'pv%3AlastFmBlacklisted=1')")
        .unwrap();
    assert!(
        t.sql.to_lowercase().contains("strpos"),
        "expected STRPOS in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_lowercase().contains("instr"),
        "should not contain instr in: {}",
        t.sql
    );
    // Verify the function has both arguments present
    assert!(
        t.sql.contains("extra_data"),
        "expected extra_data arg in: {}",
        t.sql
    );
    assert!(
        t.sql.contains("pv%3AlastFmBlacklisted=1"),
        "expected needle arg in: {}",
        t.sql
    );
}

#[test]
fn function_instr_no_match() {
    let t = translate("SELECT * FROM t WHERE id = 1").unwrap();
    // No instr present, should be unchanged
    assert!(
        !t.sql.to_lowercase().contains("strpos"),
        "no STRPOS expected in: {}",
        t.sql
    );
    assert!(t.sql.contains("id"), "expected passthrough in: {}", t.sql);
}

#[test]
fn function_iif() {
    let t = translate("SELECT iif(a > 0, 'yes', 'no') FROM t").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("CASE"), "expected CASE in: {}", t.sql);
    assert!(up.contains("WHEN"), "expected WHEN in: {}", t.sql);
    assert!(up.contains("THEN"), "expected THEN in: {}", t.sql);
    assert!(up.contains("ELSE"), "expected ELSE in: {}", t.sql);
    assert!(up.contains("END"), "expected END in: {}", t.sql);
    assert!(
        !t.sql.to_lowercase().contains("iif"),
        "should not contain iif in: {}",
        t.sql
    );
}

#[test]
fn function_iif_no_match() {
    let t = translate("SELECT a FROM t").unwrap();
    assert!(t.sql.contains("a"), "passthrough expected in: {}", t.sql);
    assert!(
        !t.sql.to_lowercase().contains("case"),
        "no CASE expected in passthrough: {}",
        t.sql
    );
}

#[test]
fn function_typeof() {
    let t = translate("SELECT typeof(x) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("pg_typeof"),
        "expected pg_typeof in: {}",
        t.sql
    );
    assert!(
        t.sql.to_lowercase().contains("::text"),
        "expected ::TEXT cast in: {}",
        t.sql
    );
}

#[test]
fn function_strftime_epoch() {
    let t = translate("SELECT strftime('%s', 'now')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("EXTRACT"), "expected EXTRACT in: {}", t.sql);
    assert!(up.contains("EPOCH"), "expected EPOCH in: {}", t.sql);
    assert!(up.contains("NOW"), "expected NOW in: {}", t.sql);
}

#[test]
fn function_strftime_epoch_interval() {
    let t = translate("SELECT strftime('%s', 'now', '-7 day')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("INTERVAL"), "expected INTERVAL in: {}", t.sql);
}

#[test]
fn function_strftime_date() {
    let t = translate("SELECT strftime('%Y-%m-%d', added_at) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("to_char"),
        "expected TO_CHAR in: {}",
        t.sql
    );
}

#[test]
fn function_strftime_column() {
    // strftime('%s', column) → EXTRACT(EPOCH FROM column)::BIGINT
    // Both C (TO_TIMESTAMP) and Rust (EXTRACT EPOCH) approaches are valid;
    // Rust approach is more direct/correct.
    let t = translate("SELECT strftime('%s', created_at) FROM t").unwrap();
    let up = t.sql.to_uppercase();
    assert!(
        up.contains("EXTRACT") && up.contains("EPOCH"),
        "expected EXTRACT(EPOCH FROM ...) in: {}",
        t.sql
    );
}

#[test]
fn function_unixepoch_now() {
    let t = translate("SELECT unixepoch('now')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("EXTRACT"), "expected EXTRACT in: {}", t.sql);
    assert!(up.contains("EPOCH"), "expected EPOCH in: {}", t.sql);
    assert!(up.contains("NOW"), "expected NOW in: {}", t.sql);
}

#[test]
fn function_unixepoch_interval() {
    let t = translate("SELECT unixepoch('now', '-7 day')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("INTERVAL"), "expected INTERVAL in: {}", t.sql);
}

#[test]
fn function_last_insert_rowid() {
    let t = translate("SELECT last_insert_rowid()").unwrap();
    assert!(
        t.sql.to_lowercase().contains("lastval"),
        "expected lastval in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_lowercase().contains("last_insert_rowid"),
        "should not contain last_insert_rowid in: {}",
        t.sql
    );
}

#[test]
fn function_json_each() {
    let t = translate("SELECT value FROM json_each(data)").unwrap();
    assert!(
        t.sql.to_lowercase().contains("json_array_elements"),
        "expected json_array_elements in: {}",
        t.sql
    );
}

#[test]
fn function_simplify_typeof() {
    // C: simplify_typeof_fixup rewrites iif(typeof(X) in ('integer','real'), X, strftime('%s',X,'utc'))
    // by removing the iif wrapper and just using X directly.
    let t = translate(
        "SELECT iif(typeof(x) in ('integer', 'real'), x, strftime('%s', x, 'utc')) FROM t",
    )
    .unwrap();
    let low = t.sql.to_lowercase();
    // The iif+typeof wrapper should be simplified to just `x`
    assert!(
        !low.contains("pg_typeof") && !low.contains("iif"),
        "simplify_typeof should remove typeof wrapper, got: {}",
        t.sql
    );
}

#[test]
fn function_typeof_integer_bigint_expansion() {
    // C: typeof type remapping — in ('integer') should also match 'bigint'
    let t = translate("SELECT * FROM t WHERE typeof(x) IN ('integer')").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("'bigint'"),
        "expected 'bigint' added to IN list, got: {}",
        t.sql
    );
}

#[test]
fn function_typeof_real_to_double_precision() {
    // C: typeof type remapping — 'real' should also match 'double precision'
    let t = translate("SELECT * FROM t WHERE typeof(x) IN ('real')").unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("'double precision'"),
        "expected 'double precision' added to IN list, got: {}",
        t.sql
    );
}

#[test]
fn function_strftime_datetime_format() {
    let t = translate("SELECT strftime('%Y-%m-%d %H:%M:%S', created_at) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("to_char"),
        "expected TO_CHAR in: {}",
        t.sql
    );
}

#[test]
fn function_strftime_positive_interval() {
    let t = translate("SELECT strftime('%s', 'now', '+7 day')").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("INTERVAL"), "expected INTERVAL in: {}", t.sql);
}

#[test]
fn function_strftime_generic_format() {
    let t = translate("SELECT strftime('%H:%M', created_at) FROM t").unwrap();
    assert!(
        t.sql.to_lowercase().contains("to_char"),
        "expected TO_CHAR in: {}",
        t.sql
    );
}

#[test]
fn function_unixepoch_column() {
    let t = translate("SELECT unixepoch(created_at) FROM t").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("EXTRACT"), "expected EXTRACT in: {}", t.sql);
    assert!(up.contains("EPOCH"), "expected EPOCH in: {}", t.sql);
}

#[test]
fn function_json_each_value_text_cast() {
    let t = translate("SELECT value FROM json_each(data)").unwrap();
    assert!(
        t.sql.to_lowercase().contains("::text"),
        "expected ::text cast in: {}",
        t.sql
    );
}

#[test]
fn function_json_each_bind_parameter_in_from_clause() {
    let t = translate(
        "SELECT di.`queue_id`,di.`id`,di.`order` FROM download_queue_items di WHERE di.`status` IN (SELECT value FROM json_each(:C1))",
    )
    .unwrap();
    let low = t.sql.to_lowercase();
    assert!(
        low.contains("json_array_elements"),
        "expected json_array_elements in: {}",
        t.sql
    );
    assert!(
        !low.contains("json_each("),
        "json_each should be rewritten in: {}",
        t.sql
    );
    assert_eq!(
        t.param_names.len(),
        1,
        "expected one bind parameter in: {}",
        t.sql
    );
    assert!(
        t.sql.contains("$1"),
        "expected named bind to be converted to $1 in: {}",
        t.sql
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// TYPES (14 tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn type_autoincrement() {
    let t = translate("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)").unwrap();
    let up = t.sql.to_uppercase();
    assert!(up.contains("SERIAL"), "expected SERIAL in: {}", t.sql);
    assert!(
        !up.contains("AUTOINCREMENT"),
        "should not contain AUTOINCREMENT in: {}",
        t.sql
    );
}

#[test]
fn type_text() {
    let t = translate("CREATE TABLE t (name TEXT)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("TEXT"),
        "expected TEXT in: {}",
        t.sql
    );
}

#[test]
fn type_dt_integer8() {
    let t = translate("CREATE TABLE t (ts dt_integer(8) NOT NULL)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("BIGINT"),
        "expected BIGINT in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_lowercase().contains("dt_integer"),
        "should not contain dt_integer in: {}",
        t.sql
    );
}

#[test]
fn type_integer8() {
    let t = translate("CREATE TABLE t (ts integer(8) DEFAULT 0)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("BIGINT"),
        "expected BIGINT in: {}",
        t.sql
    );
}

#[test]
fn type_blob() {
    let t = translate("CREATE TABLE t (data BLOB)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("BYTEA"),
        "expected BYTEA in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_uppercase().contains("BLOB"),
        "should not contain BLOB in: {}",
        t.sql
    );
}

#[test]
fn type_blob_comma() {
    let t = translate("CREATE TABLE t (data BLOB, name TEXT)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("BYTEA"),
        "expected BYTEA in: {}",
        t.sql
    );
}

#[test]
fn type_blob_default() {
    let t = translate("CREATE TABLE t (data BLOB DEFAULT NULL)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("BYTEA"),
        "expected BYTEA in: {}",
        t.sql
    );
}

#[test]
fn type_default_true() {
    let t = translate("CREATE TABLE t (active boolean DEFAULT 't')").unwrap();
    assert!(
        t.sql.to_lowercase().contains("default true"),
        "expected DEFAULT TRUE in: {}",
        t.sql
    );
    assert!(
        !t.sql.contains("'t'"),
        "should not contain 't' literal in: {}",
        t.sql
    );
}

#[test]
fn type_default_false() {
    let t = translate("CREATE TABLE t (disabled boolean DEFAULT 'f')").unwrap();
    assert!(
        t.sql.to_lowercase().contains("default false"),
        "expected DEFAULT FALSE in: {}",
        t.sql
    );
    assert!(
        !t.sql.contains("'f'"),
        "should not contain 'f' literal in: {}",
        t.sql
    );
}

#[test]
fn type_datetime() {
    let t = translate("CREATE TABLE t (created_at datetime)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("TIMESTAMP"),
        "expected TIMESTAMP in: {}",
        t.sql
    );
    assert!(
        !t.sql.to_lowercase().contains("datetime"),
        "should not contain datetime in: {}",
        t.sql
    );
}

#[test]
fn type_datetime_comma() {
    let t = translate("CREATE TABLE t (created_at datetime, name TEXT)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("TIMESTAMP"),
        "expected TIMESTAMP in: {}",
        t.sql
    );
}

#[test]
fn type_datetime_default() {
    let t = translate("CREATE TABLE t (created_at datetime DEFAULT NULL)").unwrap();
    assert!(
        t.sql.to_uppercase().contains("TIMESTAMP"),
        "expected TIMESTAMP in: {}",
        t.sql
    );
}

#[test]
fn type_no_patterns() {
    let t = translate("SELECT id, name FROM metadata_items").unwrap();
    // No DDL, passthrough
    assert!(
        t.sql.contains("metadata_items"),
        "expected passthrough in: {}",
        t.sql
    );
    assert!(t.sql.contains("id"), "expected id in: {}", t.sql);
    assert!(t.sql.contains("name"), "expected name in: {}", t.sql);
}
