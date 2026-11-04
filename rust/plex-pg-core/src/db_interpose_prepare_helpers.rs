use crate::byte_utils::push_utf8_byte;
use std::cell::RefCell;

#[inline]
fn contains_ascii_icase(haystack: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() || haystack.len() < pat.len() {
        return false;
    }
    haystack
        .windows(pat.len())
        .any(|w| w.eq_ignore_ascii_case(pat))
}

#[inline]
fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, pat: &[u8]) -> bool {
    if haystack.len() < at + pat.len() {
        return false;
    }
    haystack[at..at + pat.len()].eq_ignore_ascii_case(pat)
}

fn find_ascii_icase_from(haystack: &[u8], start: usize, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || haystack.len() < pat.len() || start >= haystack.len() {
        return None;
    }
    let mut i = start;
    while i + pat.len() <= haystack.len() {
        if haystack[i..i + pat.len()].eq_ignore_ascii_case(pat) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_icase_in_string(s: &str, pat: &str, from: usize) -> Option<usize> {
    if from >= s.len() || pat.is_empty() {
        return None;
    }
    let hay = s[from..].to_ascii_lowercase();
    let pat = pat.to_ascii_lowercase();
    hay.find(&pat).map(|i| from + i)
}

fn replace_ascii_icase_all(input: &str, pattern: &str, replacement: &str) -> String {
    let bytes = input.as_bytes();
    let pat = pattern.as_bytes();
    if pat.is_empty() || bytes.len() < pat.len() {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len() + replacement.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if i + pat.len() <= bytes.len() && bytes[i..i + pat.len()].eq_ignore_ascii_case(pat) {
            out.push_str(replacement);
            i += pat.len();
        } else {
            push_utf8_byte(&mut out, input, i);
            i += 1;
        }
    }
    out
}

pub(crate) fn alias_collection_sync_aggregates(sqlite_sql: &str, pg_sql: &str) -> Option<String> {
    let sqlite_bytes = sqlite_sql.as_bytes();
    if !(contains_ascii_icase(sqlite_bytes, b"min(year)")
        && contains_ascii_icase(sqlite_bytes, b"max(year)")
        && contains_ascii_icase(sqlite_bytes, b"count(*)"))
    {
        return None;
    }
    if !(contains_ascii_icase(sqlite_bytes, b"from tags")
        && contains_ascii_icase(sqlite_bytes, b"join taggings")
        && contains_ascii_icase(sqlite_bytes, b"group by tags.id"))
    {
        return None;
    }

    let pg_bytes = pg_sql.as_bytes();
    let sel = find_ascii_icase_from(pg_bytes, 0, b"select")?;
    let from = find_ascii_icase_from(pg_bytes, sel, b" from ")?;

    let prefix = &pg_sql[..sel];
    let select_part = &pg_sql[sel..from];
    let suffix = &pg_sql[from..];

    let step1 = replace_ascii_icase_all(select_part, "count(*)", "count(*) AS \"count(*)\"");
    let step2 = replace_ascii_icase_all(&step1, "min(year)", "min(year) AS \"min(year)\"");
    let step3 = replace_ascii_icase_all(&step2, "max(year)", "max(year) AS \"max(year)\"");

    Some(format!("{prefix}{step3}{suffix}"))
}

pub(crate) fn strip_collate_icu_root(sql: &str) -> Option<String> {
    if !contains_ascii_icase(sql.as_bytes(), b"collate icu_root") {
        return None;
    }
    let step1 = replace_ascii_icase_all(sql, " collate icu_root", "");
    let step2 = replace_ascii_icase_all(&step1, "collate icu_root", "");
    Some(step2)
}

pub(crate) fn add_if_not_exists_for_sqlite_ddl(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let has_create = contains_ascii_icase(bytes, b"CREATE TABLE")
        || contains_ascii_icase(bytes, b"CREATE INDEX")
        || contains_ascii_icase(bytes, b"CREATE UNIQUE INDEX");
    if !has_create || contains_ascii_icase(bytes, b"IF NOT EXISTS") {
        return None;
    }

    let create_pos = find_ascii_icase_from(bytes, 0, b"CREATE ")?;
    let mut keyword_after_create = create_pos + "CREATE ".len();
    if starts_with_ascii_icase_at(bytes, keyword_after_create, b"UNIQUE ") {
        keyword_after_create += "UNIQUE ".len();
    }
    if starts_with_ascii_icase_at(bytes, keyword_after_create, b"TABLE ") {
        keyword_after_create += "TABLE ".len();
    } else if starts_with_ascii_icase_at(bytes, keyword_after_create, b"INDEX ") {
        keyword_after_create += "INDEX ".len();
    } else {
        return None;
    }

    let mut out = String::with_capacity(sql.len() + "IF NOT EXISTS ".len());
    out.push_str(&sql[..keyword_after_create]);
    out.push_str("IF NOT EXISTS ");
    out.push_str(&sql[keyword_after_create..]);
    Some(out)
}

pub(crate) fn simplify_fts_for_sqlite(sql: &str) -> Option<String> {
    if !contains_ascii_icase(sql.as_bytes(), b"fts4_") {
        return None;
    }
    let mut result = sql.to_string();

    let fts_patterns = [
        "join fts4_metadata_titles_icu",
        "join fts4_metadata_titles",
        "join fts4_tag_titles_icu",
        "join fts4_tag_titles",
    ];
    let end_keywords = [" where ", " join ", " left ", " group ", " order "];

    for pat in fts_patterns {
        while let Some(join_start) = find_icase_in_string(&result, pat, 0) {
            let mut join_end = result.len();
            for kw in end_keywords {
                if let Some(pos) = find_icase_in_string(&result, kw, join_start + 1) {
                    join_end = join_end.min(pos);
                }
            }
            result.replace_range(join_start..join_end, "");
        }
    }

    let match_patterns = [
        "fts4_metadata_titles_icu.title match ",
        "fts4_metadata_titles_icu.title_sort match ",
        "fts4_metadata_titles.title match ",
        "fts4_metadata_titles.title_sort match ",
        "fts4_tag_titles_icu.title match ",
        "fts4_tag_titles_icu.tag match ",
        "fts4_tag_titles.title match ",
        "fts4_tag_titles.tag match ",
    ];

    for pat in match_patterns {
        while let Some(match_pos) = find_icase_in_string(&result, pat, 0) {
            let Some(quote_start_rel) = result[match_pos..].find('\'') else {
                break;
            };
            let quote_start = match_pos + quote_start_rel;

            let bytes = result.as_bytes();
            let mut i = quote_start + 1;
            let mut quote_end: Option<usize> = None;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    quote_end = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(quote_end) = quote_end else {
                break;
            };

            result.replace_range(match_pos..=quote_end, "1=0");
        }
    }

    Some(result)
}

/// What the real-SQLite prepare does with a statement that names an FTS
/// table or a `MATCH`, decided by `fts_for_sqlite`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SqliteFts {
    /// The statement reaches SQLite as PMS wrote it.
    AsIs,
    /// The statement reaches SQLite with its FTS joins and `MATCH` clauses
    /// taken out (`simplify_fts_for_sqlite`).
    Simplified(String),
    /// The statement is replaced by `SELECT 1 WHERE 0`.
    Blocked,
}

/// How a statement that reaches the real SQLite prepare treats FTS, by
/// whether the handle is one the shim serves from PostgreSQL.
///
/// On a PG-backed handle the SQLite file is the shadow: no rows, and no
/// `collating` tokenizer, since PMS registers it only on the connections it
/// uses as SQLite. An FTS statement there is first simplified, then, if it
/// still names an FTS table or a `MATCH`, blocked, as it has been since
/// upstream `ff63333` (2026-01-09), when the library was the only database
/// the shim served and every SQLite file it saw was a shadow.
///
/// On a genuine SQLite handle neither applies. PMS registers its tokenizer
/// on the connection (`fts3_tokenizer` is passed through), the FTS tables
/// hold real rows, and its statements bind by position: a Live TV cloud
/// guide load (PMS 1.43.4, 2026-11-04) prepares `select ... join
/// fts4_metadata_titles_icu on metadata_items.id = fts4_metadata_titles_icu
/// .rowid ...` on its `-loading.db` and binds position 1, which the
/// stand-in refused with SQLITE_RANGE ("Error 25 when binding to pos 1 on
/// bulk operations"), so every cloud load failed.
///
/// `skip_complex` is the low-stack mode, in which nothing is rewritten.
pub(crate) fn fts_for_sqlite(sql: &str, pg_backed: bool, skip_complex: bool) -> SqliteFts {
    if !pg_backed {
        return SqliteFts::AsIs;
    }
    let simplified = if skip_complex {
        None
    } else {
        simplify_fts_for_sqlite(sql)
    };
    let effective = simplified.as_deref().unwrap_or(sql);
    let bytes = effective.as_bytes();
    if contains_ascii_icase(bytes, b"fts4_") || contains_ascii_icase(bytes, b" match ") {
        return SqliteFts::Blocked;
    }
    match simplified {
        Some(out) => SqliteFts::Simplified(out),
        None => SqliteFts::AsIs,
    }
}

/// The SQL of a PG-routed statement's shadow: one placeholder per parameter
/// of the translation, named as the translation names it, so a bind PMS
/// makes on the SQLite statement lands at the index it names. No parameter
/// gives `SELECT 1 WHERE 0`. The list is as long as the statement needs:
/// the C shim built it in a 4 KiB buffer and stopped at about 225
/// placeholders, so a wider statement's later binds answered SQLITE_RANGE.
pub(crate) fn dummy_shadow_sql(param_names: &[Option<&str>]) -> String {
    if param_names.is_empty() {
        return "SELECT 1 WHERE 0".to_string();
    }
    let mut out = String::with_capacity(16 + param_names.len() * 20);
    out.push_str("SELECT 1 WHERE ");
    for (i, name) in param_names.iter().enumerate() {
        if i > 0 {
            out.push_str(" AND ");
        }
        match name {
            Some(name) => {
                out.push(':');
                out.push_str(name);
                out.push_str(" IS NOT NULL");
            }
            None => out.push_str("? IS NOT NULL"),
        }
    }
    out
}

#[derive(Clone, Copy, Default)]
struct QueryLoopEntry {
    hash: u32,
    first_seen_ms: u64,
    count: i32,
}

const LOOP_DETECT_WINDOW_MS: u64 = 1000;
const LOOP_DETECT_THRESHOLD: i32 = 100;
const LOOP_DETECT_SLOTS: usize = 16;

thread_local! {
    static PREPARE_LOOP_DETECT: RefCell<[QueryLoopEntry; LOOP_DETECT_SLOTS]> =
        RefCell::new([QueryLoopEntry::default(); LOOP_DETECT_SLOTS]);
}

pub(crate) fn prepare_simple_hash(s: &str, max_len: i32) -> u32 {
    let mut hash: u32 = 5381;
    let limit = if max_len <= 0 { 0 } else { max_len as usize };
    for b in s.as_bytes().iter().take(limit) {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

pub(crate) fn prepare_time_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

pub(crate) fn prepare_query_loop_tick(sql: &str) -> Option<(i32, u64)> {
    let hash = prepare_simple_hash(sql, 200);
    let now = prepare_time_ms();
    let slot = (hash as usize) % LOOP_DETECT_SLOTS;

    let mut detected: Option<(i32, u64)> = None;

    let _ = PREPARE_LOOP_DETECT.try_with(|state| {
        let mut state = state.borrow_mut();
        let entry = &mut state[slot];

        if entry.hash == hash {
            if now.saturating_sub(entry.first_seen_ms) < LOOP_DETECT_WINDOW_MS {
                entry.count += 1;
                if entry.count >= LOOP_DETECT_THRESHOLD {
                    detected = Some((entry.count, now.saturating_sub(entry.first_seen_ms)));
                    entry.count = 0;
                    entry.first_seen_ms = now;
                }
            } else {
                entry.first_seen_ms = now;
                entry.count = 1;
            }
        } else {
            entry.hash = hash;
            entry.first_seen_ms = now;
            entry.count = 1;
        }
    });

    detected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_loop_detect() {
        PREPARE_LOOP_DETECT.with(|state| {
            *state.borrow_mut() = [QueryLoopEntry::default(); LOOP_DETECT_SLOTS];
        });
    }

    /// PMS's loader query on a cloud guide's `-loading.db` (2026-11-04),
    /// which binds position 1.
    const CLOUD_LOADER_SQL: &str = "select metadata_items.id, media_items.id from media_items \
        join metadata_items on media_items.metadata_item_id = metadata_items.id \
        join fts4_metadata_titles_icu on metadata_items.id = fts4_metadata_titles_icu.rowid \
        where fts4_metadata_titles_icu.title match ? and metadata_items.metadata_type = 4";

    #[test]
    fn fts_reaches_a_genuine_sqlite_handle_as_written() {
        assert_eq!(
            fts_for_sqlite(CLOUD_LOADER_SQL, false, false),
            SqliteFts::AsIs
        );
        assert_eq!(
            fts_for_sqlite(
                "SELECT rowid FROM fts4_metadata_titles WHERE title MATCH 'War*'",
                false,
                true
            ),
            SqliteFts::AsIs
        );
    }

    #[test]
    fn fts_on_a_pg_backed_handle_is_simplified_then_blocked_as_before() {
        // A join and a MATCH the simplifier knows come out rewritten.
        let sql = "SELECT * FROM metadata_items \
                   JOIN fts4_metadata_titles ON metadata_items.id = fts4_metadata_titles.rowid \
                   WHERE fts4_metadata_titles.title match 'test'";
        match fts_for_sqlite(sql, true, false) {
            SqliteFts::Simplified(out) => {
                assert!(!out.to_ascii_lowercase().contains("fts4_"));
                assert!(out.contains("1=0"));
            }
            other => panic!("expected Simplified, got {:?}", other),
        }
        // A bound MATCH term is one the simplifier does not rewrite, so the
        // statement still names the table and is blocked.
        assert_eq!(
            fts_for_sqlite(CLOUD_LOADER_SQL, true, false),
            SqliteFts::Blocked
        );
        // Low stack: no rewrite, still blocked.
        assert_eq!(fts_for_sqlite(sql, true, true), SqliteFts::Blocked);
        // A statement without FTS is untouched on either handle.
        assert_eq!(
            fts_for_sqlite("SELECT 1 FROM metadata_items", true, false),
            SqliteFts::AsIs
        );
    }

    #[test]
    fn dummy_shadow_sql_carries_every_parameter() {
        assert_eq!(dummy_shadow_sql(&[]), "SELECT 1 WHERE 0");
        assert_eq!(
            dummy_shadow_sql(&[Some("id"), None, Some("title")]),
            "SELECT 1 WHERE :id IS NOT NULL AND ? IS NOT NULL AND :title IS NOT NULL"
        );
        // A 1,000-parameter statement (PMS binds that many in one bulk
        // insert) keeps every placeholder; the C shim's 4 KiB buffer kept
        // about 225.
        let names: Vec<Option<&str>> = vec![None; 1000];
        let out = dummy_shadow_sql(&names);
        assert_eq!(out.matches('?').count(), 1000);
        assert!(out.len() > 4096);
    }

    #[test]
    fn prepare_hash_consistency() {
        let sql1 = "SELECT * FROM metadata_items WHERE id = ?";
        let sql2 = "SELECT * FROM metadata_items WHERE id = ?";
        let sql3 = "SELECT * FROM media_items WHERE id = ?";

        let h1 = prepare_simple_hash(sql1, 200);
        let h2 = prepare_simple_hash(sql2, 200);
        let h3 = prepare_simple_hash(sql3, 200);

        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn prepare_hash_distribution() {
        let mut slots = [0usize; LOOP_DETECT_SLOTS];
        for i in 0..1000 {
            let sql = format!("SELECT * FROM table_{} WHERE col = {}", i % 50, i);
            let h = prepare_simple_hash(&sql, 200);
            slots[(h as usize) % LOOP_DETECT_SLOTS] += 1;
        }
        let avg = 1000 / LOOP_DETECT_SLOTS;
        let mut bad = 0;
        for count in slots.iter() {
            if *count > avg * 3 {
                bad += 1;
            }
        }
        assert_eq!(bad, 0, "hash distribution too uneven: {:?}", slots);
    }

    #[test]
    fn loop_detection_triggers_after_threshold() {
        reset_loop_detect();
        let sql = "SELECT * FROM metadata_items WHERE id = ?";
        let mut detected = false;
        for _ in 0..(LOOP_DETECT_THRESHOLD + 5) {
            if prepare_query_loop_tick(sql).is_some() {
                detected = true;
                break;
            }
        }
        assert!(detected, "expected loop detection to trigger");
    }

    #[test]
    fn loop_detection_resets_after_window() {
        reset_loop_detect();
        let sql = "SELECT * FROM table_reset";
        for _ in 0..(LOOP_DETECT_THRESHOLD - 1) {
            assert!(prepare_query_loop_tick(sql).is_none());
        }

        let hash = prepare_simple_hash(sql, 200);
        let slot = (hash as usize) % LOOP_DETECT_SLOTS;
        PREPARE_LOOP_DETECT.with(|state| {
            let mut state = state.borrow_mut();
            state[slot].first_seen_ms = state[slot]
                .first_seen_ms
                .saturating_sub(LOOP_DETECT_WINDOW_MS + 1);
            state[slot].count = 1;
        });

        let mut detected = false;
        for _ in 0..(LOOP_DETECT_THRESHOLD - 1) {
            if prepare_query_loop_tick(sql).is_some() {
                detected = true;
                break;
            }
        }
        assert!(!detected, "time window did not reset as expected");
    }

    #[test]
    fn loop_detection_collision_handling_no_panic() {
        reset_loop_detect();
        let mut pair: Option<(String, String)> = None;
        for i in 0..1000 {
            let sql1 = format!("SELECT * FROM table_a WHERE x = {}", i);
            let h1 = prepare_simple_hash(&sql1, 200);
            for j in (i + 1)..1000 {
                let sql2 = format!("SELECT * FROM table_b WHERE y = {}", j);
                let h2 = prepare_simple_hash(&sql2, 200);
                if (h1 as usize % LOOP_DETECT_SLOTS) == (h2 as usize % LOOP_DETECT_SLOTS)
                    && h1 != h2
                {
                    pair = Some((sql1, sql2));
                    break;
                }
            }
            if pair.is_some() {
                break;
            }
        }
        if let Some((a, b)) = pair {
            for i in 0..(LOOP_DETECT_THRESHOLD / 2) {
                let sql = if i % 2 == 0 { &a } else { &b };
                let _ = prepare_query_loop_tick(sql);
            }
        }
    }

    #[test]
    fn replace_ascii_icase_all_keeps_non_ascii_text() {
        let out = replace_ascii_icase_all("SELECT COUNT(*), 'Caché' FROM t", "count(*)", "n");
        assert_eq!(out, "SELECT n, 'Caché' FROM t");
    }
}
