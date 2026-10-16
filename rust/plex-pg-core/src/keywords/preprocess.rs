use super::*;
use crate::byte_utils::push_utf8_byte;

pub(super) fn preprocess_sql(sql: &str) -> String {
    let sql = fix_placeholder_spacing(sql);
    let sql = rewrite_sqlite_utility_statements(&sql);
    let sql = rewrite_regexp_operator(&sql);
    let sql = rewrite_is_operator(&sql);
    let sql = rewrite_raise_function_calls(&sql);
    let sql = rewrite_or_conflict_prefix_statements(&sql);
    let sql = rewrite_update_delete_limit_statements(&sql);
    let sql = rewrite_transaction_control_statements(&sql);
    let sql = rewrite_pragma_statements(&sql);
    let sql = rewrite_sqlite_create_table_options(&sql);
    let sql = rewrite_fts_index_rebuild(&sql);
    let sql = rewrite_virtual_tables(&sql);
    let sql = rewrite_glob(&sql);
    let sql = rewrite_trim_calls(&sql);
    let sql = rewrite_indexed_by(&sql);
    let sql = rewrite_index_ddl_identifier_quotes(&sql);
    let sql = rewrite_sqlite_collations(&sql);
    let sql = rewrite_distinct_orderby_projection(&sql);
    rewrite_metadata_items_self_join(&sql)
}

fn rewrite_or_conflict_prefix_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        // UPDATE OR <algo> ...
        if lower.starts_with("update or ") {
            let tail = t["update or ".len()..].trim_start();
            let algos = ["ignore", "replace", "rollback", "abort", "fail"];
            let mut matched = false;
            for algo in algos {
                if tail.to_ascii_lowercase().starts_with(algo) {
                    let rest = tail[algo.len()..].trim_start();
                    out.push(format!("UPDATE {}", rest));
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }

        // INSERT OR ABORT/FAIL/ROLLBACK INTO ...
        if lower.starts_with("insert or abort into ")
            || lower.starts_with("insert or fail into ")
            || lower.starts_with("insert or rollback into ")
        {
            if let Some((_, rest)) = t.split_once("INTO ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
            if let Some((_, rest)) = t.split_once("into ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
        }

        out.push(t.to_string());
    }
    out.join("; ")
}

/// Rewrite SQLite `DELETE ... [ORDER BY ...] LIMIT ...` and
/// `UPDATE ... ORDER BY ... LIMIT ...` forms to ctid targeting.
///
/// A plain `UPDATE ... [WHERE ...] LIMIT n` parses, and the query rewrite
/// handles it at AST level after placeholder numbering; only the ORDER BY
/// form, which sqlparser does not accept on an UPDATE, is rewritten here.
fn rewrite_update_delete_limit_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(r) = rewrite_single_update_limit_stmt(t) {
            out.push(r);
            continue;
        }
        if let Some(r) = rewrite_single_delete_limit_stmt(t) {
            out.push(r);
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn rewrite_single_update_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("update ") {
        return None;
    }

    let after_update = "update ".len();
    let set_pos = find_top_level_keyword_from(stmt, after_update, "set")?;
    let table_part = stmt[after_update..set_pos].trim();
    if table_part.is_empty() {
        return None;
    }

    let after_set = set_pos + "set".len();
    let where_pos = find_top_level_keyword_from(stmt, after_set, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_set, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_set, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_set, "returning");

    // Without ORDER BY, and without RETURNING after the LIMIT (sqlparser
    // reads RETURNING first), the statement parses, and the AST rewrite
    // keeps the SET placeholders ahead of the WHERE's.
    let has_order_by = order_pos.is_some_and(|op| op < limit_pos);
    let returning_after_limit = returning_pos.is_some_and(|rp| rp > limit_pos);
    if !has_order_by && !returning_after_limit {
        return None;
    }

    let set_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_set)
        .min()
        .unwrap_or(stmt.len());
    let set_part = stmt[after_set..set_end].trim();
    if set_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    // The SET stays first so its placeholders keep their numbers ahead of
    // the WHERE's: a CTE hoisting the WHERE in front of the SET renumbered
    // every bind onto the wrong column.
    let mut out = format!(
        "UPDATE {} SET {} WHERE ctid IN ({})",
        table_part, set_part, target_sel
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

fn rewrite_single_delete_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("delete from ") {
        return None;
    }

    let after_delete_from = "delete from ".len();
    let where_pos = find_top_level_keyword_from(stmt, after_delete_from, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_delete_from, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_delete_from, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_delete_from, "returning");

    let table_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_delete_from)
        .min()
        .unwrap_or(stmt.len());
    let table_part = stmt[after_delete_from..table_end].trim();
    if table_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    let mut out = format!(
        "WITH _plex_target AS ({}) DELETE FROM {} WHERE ctid IN (SELECT ctid FROM _plex_target)",
        target_sel, table_part
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

fn find_top_level_keyword_from(stmt: &str, start: usize, keyword: &str) -> Option<usize> {
    if start >= stmt.len() {
        return None;
    }
    let bytes = stmt.as_bytes();
    let kw = keyword.as_bytes();
    let mut i = start;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'`' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'`' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 => {
                if i + kw.len() <= bytes.len()
                    && bytes[i..i + kw.len()].eq_ignore_ascii_case(kw)
                    && (i == 0 || !is_ident_char(bytes[i - 1]))
                    && (i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]))
                {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Normalize SQLite utility statements that are unsupported or differ in PostgreSQL.
///
/// - `EXPLAIN QUERY PLAN <stmt>` -> `EXPLAIN <stmt>`
/// - `VACUUM` / `REINDEX` -> `SELECT 1` (safe no-op compatibility)
/// - `ANALYZE sqlite_*` -> `SELECT 1` (SQLite-internal analyze target)
/// - `ATTACH/DETACH DATABASE` -> `SELECT 1` (safe no-op compatibility)
fn rewrite_sqlite_utility_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("explain query plan ") {
            let prefix_len = "explain query plan ".len();
            out.push(format!("EXPLAIN {}", t[prefix_len..].trim()));
            continue;
        }
        if lower == "vacuum" || lower.starts_with("vacuum ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower == "reindex" || lower.starts_with("reindex ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("attach database ") || lower.starts_with("detach database ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("analyze sqlite_")
            || lower.starts_with("analyze main.sqlite_")
            || lower.starts_with("analyze temp.sqlite_")
        {
            out.push("SELECT 1".to_string());
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    haystack[at..at + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(a, n)| a.eq_ignore_ascii_case(n))
}

/// Rewrite SQLite `REGEXP` / `NOT REGEXP` operators to PostgreSQL `~` / `!~`.
/// This is a lexical rewrite outside of quoted strings.
fn rewrite_regexp_operator(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let not_regexp = b"not regexp";
    let regexp = b"regexp";

    while i < bytes.len() {
        // Keep quoted strings untouched
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_utf8_byte(&mut out, sql, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, not_regexp) {
            let end = i + not_regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push_str("!~");
                i = end;
                continue;
            }
        }
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, regexp) {
            let end = i + regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push('~');
                i = end;
                continue;
            }
        }

        push_utf8_byte(&mut out, sql, i);
        i += 1;
    }
    out
}

/// Rewrite SQLite trigger-only `RAISE(...)` calls to `NULL` so parsing remains
/// PostgreSQL-compatible when trigger bodies are translated.
fn rewrite_raise_function_calls(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_utf8_byte(&mut out, sql, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"raise(") {
            let mut j = i + "raise(".len();
            let mut depth = 1usize;
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    j += 1;
                    while j < bytes.len() {
                        if bytes[j] == b'\'' {
                            j += 1;
                            if j < bytes.len() && bytes[j] == b'\'' {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        j += 1;
                    }
                    continue;
                }
                if bytes[j] == b'(' {
                    depth += 1;
                } else if bytes[j] == b')' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                j += 1;
            }
            out.push_str("NULL");
            i = j;
            continue;
        }

        push_utf8_byte(&mut out, sql, i);
        i += 1;
    }
    out
}

/// Strip SQLite-only table options from CREATE TABLE statements:
/// - WITHOUT ROWID
/// - STRICT
///   including combined forms like `... ) WITHOUT ROWID, STRICT`.
fn rewrite_sqlite_create_table_options(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !lower.starts_with("create table ") {
            out.push(t.to_string());
            continue;
        }
        let no_conflict = strip_create_table_on_conflict_clauses(t);
        out.push(strip_create_table_tail_options(&no_conflict));
    }
    out.join("; ")
}

fn strip_create_table_on_conflict_clauses(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut out = String::with_capacity(stmt.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                push_utf8_byte(&mut out, stmt, i);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"on conflict") {
            let prev_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            if prev_ok {
                let mut j = i + "on conflict".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let rest = &stmt[j..].to_ascii_lowercase();
                let actions = ["rollback", "abort", "fail", "ignore", "replace"];
                let mut matched = false;
                for action in actions {
                    if rest.starts_with(action) {
                        j += action.len();
                        matched = true;
                        break;
                    }
                }
                if matched {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        push_utf8_byte(&mut out, stmt, i);
        i += 1;
    }
    out
}

fn strip_create_table_tail_options(stmt: &str) -> String {
    let mut s = stmt.trim().to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let trimmed = lower.trim_end();
        if trimmed.ends_with(", strict") {
            s.truncate(s.trim_end().len() - ", strict".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with(", without rowid") {
            s.truncate(s.trim_end().len() - ", without rowid".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("strict") {
            s.truncate(s.trim_end().len() - "strict".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("without rowid") {
            s.truncate(s.trim_end().len() - "without rowid".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        break;
    }
    s
}

/// Normalize SQLite transaction/control statement variants to PostgreSQL-friendly forms.
///
/// Examples:
/// - `BEGIN IMMEDIATE` / `BEGIN EXCLUSIVE` / `BEGIN TRANSACTION` -> `BEGIN`
/// - `END` / `END TRANSACTION` -> `COMMIT`
/// - `ROLLBACK TRANSACTION TO x` / `ROLLBACK TO x` -> `ROLLBACK TO SAVEPOINT x`
/// - `RELEASE x` -> `RELEASE SAVEPOINT x`
fn rewrite_transaction_control_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        let rewritten = if lower.starts_with("begin immediate")
            || lower.starts_with("begin deferred")
            || lower.starts_with("begin exclusive")
            || lower.starts_with("begin transaction")
            || lower == "begin"
        {
            "BEGIN".to_string()
        } else if lower == "end"
            || lower.starts_with("end transaction")
            || lower.starts_with("commit transaction")
        {
            "COMMIT".to_string()
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to savepoint ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback to savepoint ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("rollback to ") {
            format!("ROLLBACK TO SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("release savepoint ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("release ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("savepoint ") {
            format!("SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else {
            t.to_string()
        };

        out.push(rewritten);
    }
    out.join("; ")
}

// Temporary compatibility shim: keep preprocess pipeline stable while
// DISTINCT/ORDER BY projection rewrite is disabled on this branch.
fn rewrite_distinct_orderby_projection(sql: &str) -> String {
    sql.to_string()
}

/// Rewrite common PRAGMA statements to safe PostgreSQL-compatible no-op/select forms.
///
/// This keeps more behavior stable than dropping all PRAGMAs:
/// - assignment-like pragmas become `SELECT 1`
/// - read-like pragmas become a single-row `SELECT ... AS <pragma_name>`
///   Unknown pragmas are still stripped.
fn rewrite_pragma_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.len() >= 6 && t[..6].eq_ignore_ascii_case("pragma") {
            if let Some(mapped) = rewrite_single_pragma_stmt(t) {
                if !mapped.trim().is_empty() {
                    out.push(mapped);
                }
            }
            continue;
        }
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_pragma_stmt(stmt: &str) -> Option<String> {
    let original_stmt = stmt.trim().to_string();
    let mut body = stmt.trim();
    if body.len() < 6 || !body[..6].eq_ignore_ascii_case("pragma") {
        return None;
    }
    body = body[6..].trim();
    if body.is_empty() {
        return None;
    }

    // Drop optional schema qualifier (e.g. `main.journal_mode`)
    let body = if let Some((_, rest)) = body.rsplit_once('.') {
        rest.trim()
    } else {
        body
    };

    let (name_raw, is_set, value_opt) = if let Some((lhs, rhs)) = body.split_once('=') {
        (lhs.trim(), true, Some(rhs.trim()))
    } else if let Some(open) = body.find('(') {
        if body.ends_with(')') && open < body.len() - 1 {
            (
                body[..open].trim(),
                true,
                Some(body[open + 1..body.len() - 1].trim()),
            )
        } else {
            (body.trim(), false, None)
        }
    } else {
        (body.trim(), false, None)
    };

    if name_raw.is_empty() {
        return None;
    }
    let name = name_raw
        .trim_matches('`')
        .trim_matches('"')
        .to_ascii_lowercase();
    let normalized_value = value_opt.map(|v| {
        v.trim_matches('\'')
            .trim_matches('"')
            .trim()
            .to_ascii_lowercase()
    });

    let (mapped, outcome) = match (name.as_str(), is_set) {
        // Assignment-like pragma calls: safe no-op.
        ("foreign_keys", true) => {
            let _ = value_opt;
            ("SELECT 1".to_string(), "noop_set")
        }

        // SQLite's LIKE is ASCII case-insensitive until this is turned on;
        // the query rewrite reads the flag to pick ILIKE or LIKE.
        ("case_sensitive_like", true) => {
            let on = matches!(
                normalized_value.as_deref(),
                Some("1" | "true" | "yes" | "on")
            );
            crate::keywords::set_case_sensitive_like(on);
            ("SELECT 1".to_string(), "noop_set")
        }

        // Plex's "Optimize database" runs these and reads the answer: an
        // integrity check answers one `ok` row, and a checkpoint its three
        // counters. Stripped to nothing, the command reported no `ok` row.
        ("integrity_check" | "quick_check", _) => (
            format!("SELECT 'ok' AS {}", name),
            "mapped_read",
        ),
        ("optimize", _) => ("SELECT 1 AS optimize".to_string(), "mapped_read"),
        ("wal_checkpoint", _) => (
            "SELECT 0 AS busy, 0 AS log, 0 AS checkpointed".to_string(),
            "mapped_read",
        ),

        // busy_timeout maps best to PostgreSQL lock_timeout (session-level).
        ("busy_timeout", true) => {
            if let Some(raw) = value_opt {
                if let Ok(ms) = raw
                    .trim_matches('\'')
                    .trim_matches('"')
                    .trim()
                    .parse::<i64>()
                {
                    let clamped = ms.max(0);
                    (
                        format!("SELECT set_config('lock_timeout', '{}ms', true)", clamped),
                        "mapped_set",
                    )
                } else {
                    ("SELECT 1".to_string(), "noop_set")
                }
            } else {
                ("SELECT 1".to_string(), "noop_set")
            }
        }
        ("busy_timeout", false) => {
            (
                "SELECT current_setting('lock_timeout') AS busy_timeout".to_string(),
                "mapped_read",
            )
        }

        // foreign_keys ON/OFF has no safe direct PostgreSQL equivalent in this shim.
        ("foreign_keys", false) => ("SELECT 1 AS foreign_keys".to_string(), "mapped_read"),

        // journal_mode has no direct equivalent; keep it as session metadata.
        ("journal_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "delete" | "truncate" | "persist" | "memory" | "wal" | "off" => v,
                    _ => String::new(),
                })
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.journal_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("journal_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.journal_mode', true), 'wal') AS journal_mode"
                .to_string(),
            "mapped_read",
        ),

        // synchronous maps to PostgreSQL synchronous_commit where safe.
        ("synchronous", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "off" => "off",
                    "1" | "normal" | "2" | "full" | "3" | "extra" | "on" => "on",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('synchronous_commit', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("synchronous", false) => {
            (
                "SELECT current_setting('synchronous_commit') AS synchronous".to_string(),
                "mapped_read",
            )
        }

        // temp_store has no direct PG equivalent; preserve intent via custom session setting.
        ("temp_store", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "default" => "default",
                    "1" | "file" => "file",
                    "2" | "memory" => "memory",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.temp_store', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("temp_store", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.temp_store', true), 'memory') AS temp_store"
                .to_string(),
            "mapped_read",
        ),

        // cache_size has no exact PG equivalent; keep a custom session value for compatibility.
        ("cache_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.cache_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("cache_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.cache_size', true), '-2000') AS cache_size"
                .to_string(),
            "mapped_read",
        ),

        // page_size has no session-level equivalent; keep compatibility metadata.
        ("page_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('plex.sqlite.page_size', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("page_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.page_size', true), '4096') AS page_size"
                .to_string(),
            "mapped_read",
        ),

        // auto_vacuum has no session-level equivalent; keep compatibility metadata.
        ("auto_vacuum", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "none" => "none",
                    "1" | "full" => "full",
                    "2" | "incremental" => "incremental",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.auto_vacuum', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("auto_vacuum", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.auto_vacuum', true), 'none') AS auto_vacuum"
                .to_string(),
            "mapped_read",
        ),

        // locking_mode has no direct PG equivalent; persist as session metadata.
        ("locking_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "normal" => "normal",
                    "exclusive" => "exclusive",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.locking_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("locking_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.locking_mode', true), 'normal') AS locking_mode"
                .to_string(),
            "mapped_read",
        ),

        // wal_autocheckpoint has no PG analog at session-level; persist as compatibility metadata.
        ("wal_autocheckpoint", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.wal_autocheckpoint', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("wal_autocheckpoint", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.wal_autocheckpoint', true), '1000') AS wal_autocheckpoint"
                .to_string(),
            "mapped_read",
        ),

        // mmap_size has no PG analog at session-level; persist as compatibility metadata.
        ("mmap_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.mmap_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("mmap_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.mmap_size', true), '0') AS mmap_size"
                .to_string(),
            "mapped_read",
        ),
        ("case_sensitive_like", false) => ("SELECT 0 AS case_sensitive_like".to_string(), "mapped_read"),

        // Unknown pragma: strip, unless strict mode is enabled.
        _ if pragma_strict_mode_enabled() => (STRICT_PRAGMA_ERROR_SQL.to_string(), "strict_fail"),
        _ => (String::new(), "stripped"),
    };
    pragma_trace_mapping(&original_stmt, &name, &mapped, outcome);
    Some(mapped)
}

fn pragma_trace_enabled() -> bool {
    env_utils::env_string("PLEX_PG_TRACE_PRAGMA")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_metrics_enabled() -> bool {
    env_utils::env_string("PLEX_PG_TRACE_PRAGMA_METRICS")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_strict_mode_enabled() -> bool {
    #[cfg(test)]
    {
        match TEST_STRICT_PRAGMA_OVERRIDE.load(Ordering::Relaxed) {
            0 => return false,
            1 => return true,
            _ => {}
        }
    }
    env_utils::env_string("PLEX_PG_STRICT_PRAGMA")
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn pragma_trace_mapping(
    original_stmt: &str,
    pragma_name: &str,
    rewritten_sql: &str,
    outcome: &str,
) {
    match outcome {
        "mapped_set" => {
            PRAGMA_MAPPED_SET.fetch_add(1, Ordering::Relaxed);
        }
        "mapped_read" => {
            PRAGMA_MAPPED_READ.fetch_add(1, Ordering::Relaxed);
        }
        "noop_set" => {
            PRAGMA_NOOP_SET.fetch_add(1, Ordering::Relaxed);
        }
        "strict_fail" => {
            PRAGMA_STRICT_FAIL.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            PRAGMA_STRIPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let total = PRAGMA_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;

    if pragma_trace_enabled() {
        if rewritten_sql.trim().is_empty() {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> <stripped>",
                original_stmt, pragma_name, outcome
            );
        } else {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> {}",
                original_stmt, pragma_name, outcome, rewritten_sql
            );
        }
    }

    if pragma_metrics_enabled() && total.is_multiple_of(50) {
        eprintln!(
            "[PRAGMA_MAP] stats total={} mapped_set={} mapped_read={} noop_set={} stripped={} strict_fail={}",
            total,
            PRAGMA_MAPPED_SET.load(Ordering::Relaxed),
            PRAGMA_MAPPED_READ.load(Ordering::Relaxed),
            PRAGMA_NOOP_SET.load(Ordering::Relaxed),
            PRAGMA_STRIPPED.load(Ordering::Relaxed),
            PRAGMA_STRICT_FAIL.load(Ordering::Relaxed),
        );
    }
}

/// Answer Plex's full-text index rebuild with `SELECT 1`.
///
/// Plex decides its search index needs rebuilding when it opens a library an
/// older server wrote, and tears the index down before building it back: eight
/// `DROP TRIGGER`s and a `DROP TABLE` per index, then `CREATE VIRTUAL TABLE
/// ... USING fts4`. None of the three survives the trip to PostgreSQL. SQLite's
/// `DROP TRIGGER` names no table, so it arrives as "syntax error at end of
/// input"; fts4 is not a virtual table module PostgreSQL has, so the create is
/// a "syntax error at or near VIRTUAL"; and the drops report
/// `"fts4_metadata_titles" is not a table`.
///
/// That last message is the one that settles what to do about it. Under this
/// shim `fts4_metadata_titles` and its three siblings are *views* the schema
/// provides over the real tables — the compatibility layer, not an index Plex
/// owns. Letting the drops through wherever the syntax happened to be accepted
/// would delete it.
///
/// So the rebuild is answered the way `VACUUM` and `REINDEX` are. Search is
/// already served by other means; see `simplify_fts_for_sqlite`.
///
/// Scoped to the shim's own `fts4_*` objects: swallowing `DROP` in general
/// would turn a real schema change into silence.
fn rewrite_fts_index_rebuild(sql: &str) -> String {
    // Tested whole, before splitting: a CREATE TRIGGER body ends its own
    // statements, so splitting one on semicolons would leave a stray END
    // behind after the head became SELECT 1.
    if is_fts_index_rebuild(sql.trim()) {
        return "SELECT 1".to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if is_fts_index_rebuild(t) {
            out.push("SELECT 1".to_string());
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn is_fts_index_rebuild(stmt: &str) -> bool {
    let lower = stmt.to_ascii_lowercase();

    // CREATE VIRTUAL TABLE <any> USING fts4(...). The module is what makes it
    // untranslatable, so this does not depend on the name; the ICU pair also
    // names a tokenizer only Plex's own SQLite has.
    if lower.starts_with("create virtual table ") && lower.contains(" using fts4") {
        return true;
    }

    for prefix in ["drop table ", "drop trigger "] {
        let Some(rest) = lower.strip_prefix(prefix) else {
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
        if names_fts_object(rest.trim_start()) {
            return true;
        }
    }

    // The other half of the rebuild: the triggers that keep the index in step
    // with metadata_items and tags. They maintain a view here, so they have
    // nothing to maintain, and their SQLite bodies do not parse as PostgreSQL.
    if let Some(rest) = lower.strip_prefix("create trigger ") {
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("if not exists ").unwrap_or(rest);
        if names_fts_object(rest.trim_start()) {
            return true;
        }
    }
    false
}

/// Whether what follows names one of the shim's `fts4_*` objects, quoted or not.
///
/// Only the leading identifier is considered. The rest of a CREATE TRIGGER is
/// its body, and a body mentions `old.rowid` -- so anything that reaches for
/// the first `.` in the whole remainder ends up asking whether `rowid` is an
/// fts4 object, and concludes the statement is nothing to do with the index.
fn names_fts_object(rest: &str) -> bool {
    let token = rest
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';' || c == ',')
        .next()
        .unwrap_or("");
    let name = token.trim_start_matches(['"', '\'', '`', '[']);
    // A schema-qualified name is the same object: main.fts4_tag_titles.
    let name = match name.split_once('.') {
        Some((_, tail)) => tail.trim_start_matches(['"', '\'', '`', '[']),
        None => name,
    };
    name.starts_with("fts4_")
}

/// Rewrite SQLite virtual table DDL to PostgreSQL-compatible CREATE TABLE.
///
/// - `CREATE VIRTUAL TABLE x USING fts5(a,b)` ->
///   `CREATE TABLE x (id BIGSERIAL PRIMARY KEY, a TEXT, b TEXT, _fts TSVECTOR)`
///
/// - `CREATE VIRTUAL TABLE x USING rtree(id,minX,maxX,...)` ->
///   `CREATE TABLE x (id DOUBLE PRECISION, minX DOUBLE PRECISION, ...)`
fn rewrite_virtual_tables(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rewritten) = rewrite_single_virtual_table_stmt(t) {
            out.push(rewritten);
        } else {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_virtual_table_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let prefix = "create virtual table ";
    if !lower.starts_with(prefix) {
        return None;
    }

    let using_fts = " using fts5(";
    let using_rtree = " using rtree(";
    let (using_idx, mode, using_len) = if let Some(idx) = lower.find(using_fts) {
        (idx, "fts5", using_fts.len())
    } else if let Some(idx) = lower.find(using_rtree) {
        (idx, "rtree", using_rtree.len())
    } else {
        return None;
    };

    let table_name = stmt[prefix.len()..using_idx].trim();
    if table_name.is_empty() {
        return None;
    }

    let cols_start = using_idx + using_len;
    let cols_end = stmt.rfind(')')?;
    if cols_end < cols_start {
        return None;
    }
    let cols_raw = &stmt[cols_start..cols_end];
    let cols = split_csv_top_level(cols_raw);

    let mut defs: Vec<String> = Vec::new();
    if mode == "fts5" {
        defs.push("id BIGSERIAL PRIMARY KEY".to_string());
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() || col.to_ascii_lowercase().starts_with("tokenize") {
                continue;
            }
            defs.push(format!("{} TEXT", col));
        }
        defs.push("_fts TSVECTOR".to_string());
    } else {
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() {
                continue;
            }
            defs.push(format!("{} DOUBLE PRECISION", col));
        }
    }

    if defs.is_empty() {
        return None;
    }
    Some(format!("CREATE TABLE {} ({})", table_name, defs.join(", ")))
}

fn split_sql_statements(sql: &str) -> Vec<String> {
    // SQLite trigger bodies contain internal semicolons inside BEGIN...END blocks.
    // Treat CREATE TRIGGER statements as a single unit to avoid splitting mid-body.
    if sql.to_ascii_lowercase().contains("create trigger") {
        let t = sql.trim();
        if t.is_empty() {
            return Vec::new();
        }
        return vec![t.to_string()];
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;

    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            ';' if !in_single && !in_double => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn rewrite_index_ddl_identifier_quotes(sql: &str) -> String {
    let mut out = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        out.push(rewrite_single_index_ddl_identifier_quotes(t));
    }
    out.join("; ")
}

fn rewrite_single_index_ddl_identifier_quotes(stmt: &str) -> String {
    let start = skip_leading_ws_and_sql_comments_offset(stmt);
    let core = &stmt[start..];
    if core.is_empty() {
        return stmt.to_string();
    }

    if is_create_index_stmt(core) {
        let rewritten = rewrite_create_index_identifier_quotes_core(core);
        if rewritten != core {
            return format!("{}{}", &stmt[..start], rewritten);
        }
        return stmt.to_string();
    }

    if is_drop_index_stmt(core) {
        let rewritten = rewrite_drop_index_identifier_quotes_core(core);
        if rewritten != core {
            return format!("{}{}", &stmt[..start], rewritten);
        }
    }

    stmt.to_string()
}

fn is_create_index_stmt(stmt: &str) -> bool {
    let lower = stmt.to_ascii_lowercase();
    lower.starts_with("create index") || lower.starts_with("create unique index")
}

fn is_drop_index_stmt(stmt: &str) -> bool {
    stmt.to_ascii_lowercase().starts_with("drop index")
}

fn skip_leading_ws_and_sql_comments_offset(stmt: &str) -> usize {
    let bytes = stmt.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        break;
    }
    i
}

fn skip_ascii_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn consume_keyword_ci(stmt: &str, i: usize, keyword: &str) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let start = skip_ascii_ws(bytes, i);
    let end = start + keyword.len();
    if end > bytes.len() || !stmt[start..end].eq_ignore_ascii_case(keyword) {
        return None;
    }
    if start > 0 && is_ident_char(bytes[start - 1]) {
        return None;
    }
    if end < bytes.len() && is_ident_char(bytes[end]) {
        return None;
    }
    Some(end)
}

fn find_token_end(stmt: &str, start: usize) -> usize {
    let bytes = stmt.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b if b.is_ascii_whitespace() => break,
            _ => i += 1,
        }
    }
    i
}

fn find_next_unquoted_char(stmt: &str, start: usize, target: u8) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b if b == target => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn find_matching_paren(stmt: &str, open_pos: usize) -> Option<usize> {
    let bytes = stmt.as_bytes();
    if open_pos >= bytes.len() || bytes[open_pos] != b'(' {
        return None;
    }

    let mut depth = 0usize;
    let mut i = open_pos;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    None
}

fn parse_quoted_token(stmt: &str, start: usize) -> Option<(char, String, usize)> {
    let bytes = stmt.as_bytes();
    if start >= bytes.len() {
        return None;
    }

    let quote = bytes[start] as char;
    if quote != '\'' && quote != '"' && quote != '`' {
        return None;
    }

    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == quote {
            i += 1;
            if i < bytes.len() && bytes[i] as char == quote {
                out.push(quote);
                i += 1;
                continue;
            }
            return Some((quote, out, i));
        }
        push_utf8_byte(&mut out, stmt, i);
        i += 1;
    }

    None
}

fn is_simple_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn rewrite_identifier_quotes_in_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(segment.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'`' {
            if let Some((_, content, next)) = parse_quoted_token(segment, i) {
                if is_simple_identifier(&content) {
                    out.push('"');
                    out.push_str(&content);
                    out.push('"');
                    i = next;
                    continue;
                }
            }
        }

        push_utf8_byte(&mut out, segment, i);
        i += 1;
    }

    out
}

fn rewrite_last_quoted_identifier_token(segment: &str) -> String {
    let trimmed_end = segment
        .trim_end_matches(|c: char| c.is_ascii_whitespace())
        .len();
    if trimmed_end == 0 {
        return segment.to_string();
    }

    let token_start = segment[..trimmed_end]
        .rfind(|c: char| c.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let token = &segment[token_start..trimmed_end];
    let rewritten = rewrite_identifier_quotes_in_segment(token);
    if rewritten == token {
        return segment.to_string();
    }

    format!(
        "{}{}{}",
        &segment[..token_start],
        rewritten,
        &segment[trimmed_end..]
    )
}

fn rewrite_index_column_item(item: &str) -> String {
    let trimmed = item.trim_start();
    let leading = &item[..item.len() - trimmed.len()];
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() || (bytes[0] != b'\'' && bytes[0] != b'`') {
        return item.to_string();
    }

    let Some((_, content, next)) = parse_quoted_token(trimmed, 0) else {
        return item.to_string();
    };
    if !is_simple_identifier(&content) {
        return item.to_string();
    }

    let rest = &trimmed[next..];
    let rest_trimmed = rest.trim_start();
    let allowed_suffix = rest_trimmed.is_empty()
        || rest_trimmed
            .split_whitespace()
            .next()
            .map(|kw| {
                kw.eq_ignore_ascii_case("collate")
                    || kw.eq_ignore_ascii_case("asc")
                    || kw.eq_ignore_ascii_case("desc")
                    || kw.eq_ignore_ascii_case("nulls")
            })
            .unwrap_or(false);
    if !allowed_suffix {
        return item.to_string();
    }

    format!("{leading}\"{content}\"{rest}")
}

fn rewrite_index_column_list(columns: &str) -> String {
    split_csv_top_level(columns)
        .into_iter()
        .map(|item| rewrite_index_column_item(&item))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rewrite_create_index_identifier_quotes_core(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut pos = 0usize;
    let Some(next) = consume_keyword_ci(stmt, pos, "create") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next) = consume_keyword_ci(stmt, pos, "unique") {
        pos = next;
    }
    let Some(next) = consume_keyword_ci(stmt, pos, "index") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next_if) = consume_keyword_ci(stmt, pos, "if") {
        if let Some(next_not) = consume_keyword_ci(stmt, next_if, "not") {
            if let Some(next_exists) = consume_keyword_ci(stmt, next_not, "exists") {
                pos = next_exists;
            }
        }
    }

    let name_start = skip_ascii_ws(bytes, pos);
    if name_start >= bytes.len() {
        return stmt.to_string();
    }
    let name_end = find_token_end(stmt, name_start);
    let Some(on_pos) = find_top_level_keyword_from(stmt, name_end, "on") else {
        return stmt.to_string();
    };

    let table_start = skip_ascii_ws(bytes, on_pos + 2);
    let Some(open_paren) = find_next_unquoted_char(stmt, table_start, b'(') else {
        return stmt.to_string();
    };
    let Some(close_paren) = find_matching_paren(stmt, open_paren) else {
        return stmt.to_string();
    };

    let mut out = String::with_capacity(stmt.len());
    out.push_str(&stmt[..name_start]);
    out.push_str(&rewrite_last_quoted_identifier_token(
        &stmt[name_start..name_end],
    ));
    out.push_str(&stmt[name_end..table_start]);
    out.push_str(&rewrite_identifier_quotes_in_segment(
        &stmt[table_start..open_paren],
    ));
    out.push('(');
    out.push_str(&rewrite_index_column_list(
        &stmt[open_paren + 1..close_paren],
    ));
    out.push_str(&stmt[close_paren..]);
    out
}

fn rewrite_drop_index_identifier_quotes_core(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut pos = 0usize;
    let Some(next) = consume_keyword_ci(stmt, pos, "drop") else {
        return stmt.to_string();
    };
    pos = next;
    let Some(next) = consume_keyword_ci(stmt, pos, "index") else {
        return stmt.to_string();
    };
    pos = next;
    if let Some(next_if) = consume_keyword_ci(stmt, pos, "if") {
        if let Some(next_exists) = consume_keyword_ci(stmt, next_if, "exists") {
            pos = next_exists;
        }
    }

    let name_start = skip_ascii_ws(bytes, pos);
    if name_start >= bytes.len() {
        return stmt.to_string();
    }
    let name_end = find_token_end(stmt, name_start);

    let mut out = String::with_capacity(stmt.len());
    out.push_str(&stmt[..name_start]);
    out.push_str(&rewrite_last_quoted_identifier_token(
        &stmt[name_start..name_end],
    ));
    out.push_str(&stmt[name_end..]);
    out
}

fn split_csv_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                cur.push(ch);
            }
            ')' if !in_single && !in_double => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if !in_single && !in_double && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn extract_ident_token(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    let tok = t.split_whitespace().next().unwrap_or("");
    tok.trim_matches(',').to_string()
}

// Plex self-join compatibility:
//
// Plex emits two related query shapes that PostgreSQL rejects:
//
// Shape A – metadata_item_settings as root, metadata_items joined twice (one aliased, one NOT):
//   SELECT ... FROM metadata_item_settings
//   JOIN metadata_items AS parents  ON parents.id = metadata_items.parent_id
//   JOIN metadata_items             ON metadata_items.id = metadata_item_settings.metadata_item_id
//   WHERE metadata_items.col = ...
//
//   Problem: `metadata_items` appears as alias-target (`AS parents`) AND as an
//   unaliased join + bare column qualifier. PostgreSQL raises:
//     "invalid reference to FROM-clause entry for table metadata_items"
//
//   Fix: add `AS mi` to the unaliased join and replace every
//        `metadata_items.<col>` reference with `mi.<col>`.
//
// Shape B – metadata_items as root, self-joined via parents / grandparents:
//   SELECT metadata_items.id FROM metadata_items
//   JOIN metadata_items AS parents      ON parents.id = metadata_items.parent_id
//   JOIN metadata_items AS grandparents ON grandparents.id = parents.parent_id
//   WHERE metadata_items.library_section_id IN (...)
//   ORDER BY grandparents.title_sort ...
//
//   In this shape the base table IS `metadata_items` (unaliased), so bare
//   `metadata_items.col` refs are valid in PostgreSQL.  Nothing to rewrite.
//   (The COLLATE icu_root / parse-error is handled by rewrite_sqlite_collations
//   which runs before this function.)
fn rewrite_metadata_items_self_join(sql: &str) -> String {
    let lower = sql.to_lowercase();

    // ── Shape A detection ──────────────────────────────────────────────────────
    // Plex emits queries like:
    //   SELECT metadata_items.id FROM metadata_item_settings
    //   JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id
    //   JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id
    //   WHERE metadata_items.col = ...
    //
    // PostgreSQL rejects this because `metadata_items` is both an alias target
    // and used as a bare qualifier.
    //
    // Fix strategy: reorder the joins so the unaliased metadata_items join comes
    // FIRST (renamed to AS mi), then the aliased joins (parents, etc.) can refer
    // to mi.col in their ON clauses. Then replace metadata_items.col → mi.col
    // throughout SELECT/WHERE/HAVING/ORDER BY.
    if lower.contains("from metadata_item_settings") && lower.contains("join metadata_items") {
        let total_joins = count_word_occurrences(&lower, "join metadata_items");
        let aliased_joins = count_word_occurrences(&lower, "join metadata_items as ");
        let unaliased_joins = total_joins.saturating_sub(aliased_joins);

        if unaliased_joins > 0 && aliased_joins > 0 {
            return reorder_and_alias_self_join(sql);
        }
    }

    sql.to_string()
}

/// Rewrite Shape A self-join query by:
/// 1. Extracting the unaliased "JOIN metadata_items ON ..." fragment
/// 2. Placing it first in the join list as "JOIN metadata_items AS mi ON ..."
///    (rewriting its ON clause: metadata_items.col → mi.col)
/// 3. Rewriting the ON clauses of subsequent aliased joins: metadata_items.col → mi.col
/// 4. Replacing all remaining metadata_items.col references in SELECT/WHERE/etc.
fn reorder_and_alias_self_join(sql: &str) -> String {
    // Split into: prefix (everything up to the first join keyword), the list
    // of join fragments, each with its own keyword sequence (`JOIN`, `LEFT
    // JOIN`, `LEFT OUTER JOIN`, ...), and suffix (WHERE + rest after all
    // JOINs). Splitting on the bare word "join" left a LEFT behind in the
    // previous fragment, so a reordered LEFT JOIN came back as an inner one.
    let Some(first) = find_join_keyword(sql, 0) else {
        return sql.to_string();
    };

    // prefix: "SELECT ... FROM metadata_item_settings"
    let prefix = sql[..first.0].trim_end();

    // Find WHERE, HAVING, ORDER BY, LIMIT, GROUP BY at top level (not inside parens)
    let rest = &sql[first.0..];
    let suffix_start = find_top_level_clause(&rest.to_lowercase());
    let joins_str = &rest[..suffix_start];
    let suffix = &rest[suffix_start..];

    // (keyword sequence, body) per join
    let mut join_fragments: Vec<(String, String)> = Vec::new();
    let mut pos = 0usize;
    while let Some((kw_start, kw_end)) = find_join_keyword(joins_str, pos) {
        let keyword = joins_str[kw_start..kw_end].to_string();
        let body_end = find_join_keyword(joins_str, kw_end)
            .map(|(s, _)| s)
            .unwrap_or(joins_str.len());
        join_fragments.push((keyword, joins_str[kw_end..body_end].trim().to_string()));
        pos = body_end;
    }

    // Identify which fragment(s) are unaliased metadata_items joins.
    // An unaliased fragment starts with "metadata_items " or "metadata_items\n"
    // but NOT "metadata_items as".
    let mut unaliased_idxs: Vec<usize> = Vec::new();
    let mut aliased_idxs: Vec<usize> = Vec::new();
    for (idx, (_, body)) in join_fragments.iter().enumerate() {
        let fl = body.to_lowercase();
        let names_metadata_items = fl
            .strip_prefix("metadata_items")
            .is_some_and(|after| after.is_empty() || after.starts_with(char::is_whitespace));
        if let Some(after) = fl
            .strip_prefix("metadata_items")
            .filter(|_| names_metadata_items)
        {
            let after = after.trim_start();
            if after.starts_with("as ") || after.starts_with("as\t") {
                aliased_idxs.push(idx);
            } else {
                unaliased_idxs.push(idx);
            }
        } else {
            aliased_idxs.push(idx);
        }
    }

    if unaliased_idxs.is_empty() {
        return sql.to_string();
    }

    // Build the new join list: unaliased first (as mi), then aliased, each
    // under its own join keyword
    let mut new_joins: Vec<String> = Vec::new();

    for &idx in &unaliased_idxs {
        let (keyword, body) = &join_fragments[idx];
        // Strip leading "metadata_items" and get the rest (ON clause etc.)
        let rest_of_frag = &body["metadata_items".len()..];
        // Rewrite metadata_items.col → mi.col in the ON clause of this fragment
        let on_fixed = rest_of_frag.replace("metadata_items.", "mi.");
        new_joins.push(format!("{} metadata_items AS mi{}", keyword, on_fixed));
    }

    for &idx in &aliased_idxs {
        let (keyword, body) = &join_fragments[idx];
        // In aliased join ON clauses, replace metadata_items.col → mi.col
        new_joins.push(format!(
            "{} {}",
            keyword,
            body.replace("metadata_items.", "mi.")
        ));
    }

    // Reconstruct: prefix + join fragments + suffix
    let reconstructed = format!("{} {} {}", prefix, new_joins.join(" "), suffix);

    // Finally replace remaining metadata_items.col in SELECT/WHERE/etc.
    replace_metadata_items_refs(&reconstructed, "mi")
}

/// The next join keyword sequence at or after `from`, outside quotes and
/// parentheses: the byte range of `[NATURAL] [LEFT|RIGHT|FULL|INNER|CROSS]
/// [OUTER] JOIN`.
fn find_join_keyword(sql: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    let mut i = from;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0
                && !(i > 0 && is_ident_char(bytes[i - 1]))
                && starts_with_ascii_icase_at(bytes, i, b"join")
                && !(i + 4 < bytes.len() && is_ident_char(bytes[i + 4])) =>
            {
                let end = i + 4;
                // Walk back over the qualifiers in front of JOIN.
                let mut start = i;
                for group in [
                    &["outer"][..],
                    &["left", "right", "full", "inner", "cross"][..],
                    &["natural"][..],
                ] {
                    let mut k = start;
                    while k > 0 && bytes[k - 1].is_ascii_whitespace() {
                        k -= 1;
                    }
                    let word_end = k;
                    while k > 0 && is_ident_char(bytes[k - 1]) {
                        k -= 1;
                    }
                    let word = sql[k..word_end].to_ascii_lowercase();
                    if group.contains(&word.as_str()) {
                        start = k;
                    }
                }
                return Some((start, end));
            }
            _ => i += 1,
        }
    }
    None
}

/// Find the byte offset in `s` (lowercase) where a top-level SQL clause starts
/// (WHERE, HAVING, ORDER BY, GROUP BY, LIMIT, UNION). Returns s.len() if none found.
fn find_top_level_clause(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'\'' => {
                // skip string literal
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ if depth == 0 => {
                for kw in &[
                    "where ",
                    "having ",
                    "order by ",
                    "group by ",
                    "limit ",
                    "union ",
                ] {
                    if s[i..].starts_with(kw) {
                        return i;
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    s.len()
}

/// Count non-overlapping occurrences of `needle` in `haystack` (both lowercase).
fn count_word_occurrences(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

/// Replace every occurrence of `metadata_items.<identifier>` with
/// `<new_alias>.<identifier>` throughout the SQL, being careful not to
/// touch occurrences inside quoted string literals.
fn replace_metadata_items_refs(sql: &str, new_alias: &str) -> String {
    let needle = "metadata_items.";
    let needle_lower = needle.to_lowercase();
    let sql_lower = sql.to_lowercase();

    let mut result = String::with_capacity(sql.len());
    let mut i = 0usize;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                push_utf8_byte(&mut result, sql, i);
                i += 1;
                if ch == b'\'' {
                    // escaped '' or end of string
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        // Check for "metadata_items." at current position
        if sql_lower[i..].starts_with(&needle_lower) {
            // Emit the alias instead
            result.push_str(new_alias);
            result.push('.');
            i += needle.len();
            continue;
        }

        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }

    result
}

/// Separate a `?` placeholder from the word written against it.
///
/// In SQLite a `?` takes only digits (`?NNN`); named parameters are `:name`,
/// `@name` and `$name`. So `?left` is a placeholder and then `left`, whatever
/// the word is, and sqlparser has to be shown the space SQLite's tokenizer
/// implies. Plex writes `account_id=?left join media_items` in its tag
/// directories; reading `?left` as one named placeholder dropped the LEFT,
/// turned the join into an inner one, and emptied every TV library's genres
/// (2026-10-08).
fn fix_placeholder_spacing(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let mut in_string = false;
    let mut string_char: u8 = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            push_utf8_byte(&mut result, sql, i);
            if b == string_char {
                if i + 1 < bytes.len() && bytes[i + 1] == string_char {
                    result.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b'\'' || b == b'"' {
            in_string = true;
            string_char = b;
            result.push(b as char);
            i += 1;
            continue;
        }

        if b == b'?'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
        {
            result.push_str("? ");
            i += 1;
            continue;
        }

        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }
    result
}

/// `trim(x, y)` is SQLite's two-argument trim; sqlparser knows only the
/// standard `TRIM([BOTH] y FROM x)` and refuses the comma ("Expected: ),
/// found: ?", 2026-10-16). PostgreSQL's `btrim` takes both forms, so the
/// name is swapped outside string literals; `ltrim`/`rtrim` already parse.
fn rewrite_trim_calls(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    let mut in_string = false;
    let mut string_char: u8 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            push_utf8_byte(&mut result, sql, i);
            if b == string_char {
                if i + 1 < bytes.len() && bytes[i + 1] == string_char {
                    result.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            in_string = true;
            string_char = b;
            result.push(b as char);
            i += 1;
            continue;
        }
        let word_start = i == 0
            || !(bytes[i - 1].is_ascii_alphanumeric()
                || bytes[i - 1] == b'_'
                || bytes[i - 1] == b'.');
        if word_start && i + 4 <= bytes.len() && bytes[i..i + 4].eq_ignore_ascii_case(b"trim") {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                result.push_str("btrim");
                i += 4;
                continue;
            }
        }
        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }
    result
}
/// Spell the GLOB operator `SIMILAR TO`, which sqlparser accepts and SQLite
/// never writes, so the query rewrite can turn the whole right operand --
/// a literal, a bound parameter, or `'*' || ? || '*'` -- into an anchored
/// regular expression match. `NOT GLOB` becomes `NOT SIMILAR TO` the same
/// way. GLOB is case-sensitive; the ILIKE this used to emit was not, and
/// its `%`/`_` could not carry a glob's character classes.
fn rewrite_glob(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    // Fast path – nothing to do
    if !upper.contains("GLOB") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len() + 16);
    let mut i = 0;

    while i < bytes.len() {
        // Inside a single-quoted string → copy verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_utf8_byte(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // The GLOB keyword on its own (not `glob(`, the function form, which
        // the query rewrite handles as a call)
        let rest_upper = upper.get(i..).unwrap_or("");
        if rest_upper.starts_with("GLOB") {
            let after = i + 4;
            let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let is_call = j < bytes.len() && bytes[j] == b'(';
            if !prev_is_ident && boundary && !is_call {
                result.push_str("SIMILAR TO");
                i = after;
                continue;
            }
        }

        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }

    result
}

/// Remove SQLite index hints:
/// - `INDEXED BY <identifier>`
/// - `NOT INDEXED`
fn rewrite_indexed_by(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("INDEXED") {
        return sql.to_string();
    }

    // Find all occurrences of INDEXED BY <ident> and remove them.
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted strings verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_utf8_byte(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        let rest_upper = upper.get(i..).unwrap_or("");
        if rest_upper.starts_with("NOT INDEXED") {
            let after = i + "NOT INDEXED".len();
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                i = after;
                continue;
            }
        }
        if rest_upper.starts_with("INDEXED") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip "INDEXED"
                let mut j = i + 7;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Check for BY
                if sql[j..].to_uppercase().starts_with("BY") {
                    j += 2;
                    // Skip whitespace
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    // Skip identifier
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }

    result
}

/// Remove `COLLATE <name>` clauses for SQLite-specific collations that either
/// sqlparser (SQLiteDialect) cannot parse, or that have no PostgreSQL equivalent.
///
/// ICU extension names like `icu_root` and `icu_und`, as well as `UNICODE` /
/// `UNICODE_CI`, are not in sqlparser's keyword list and cause a hard parse error:
///   "Expected: end of statement, found: collate"
///
/// `RTRIM` and `BINARY` parse fine but PostgreSQL has no matching collation, so
/// they are dropped here as well.
///
/// `NOCASE` is intentionally left intact; the AST-level transform in query.rs
/// converts it to LOWER(…) which is the correct PostgreSQL semantic.
fn rewrite_sqlite_collations(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    // Fast path — nothing to do
    if !upper.contains("COLLATE") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote: ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                push_utf8_byte(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // Skip double-quoted identifiers verbatim
        if bytes[i] == b'"' {
            result.push('"');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    result.push('"');
                    i += 1;
                    // Escaped double-quote: ""
                    if i < bytes.len() && bytes[i] == b'"' {
                        result.push('"');
                        i += 1;
                    }
                    break;
                }
                push_utf8_byte(&mut result, sql, i);
                i += 1;
            }
            continue;
        }

        // Skip backtick-quoted identifiers verbatim
        if bytes[i] == b'`' {
            result.push('`');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                push_utf8_byte(&mut result, sql, i);
                i += 1;
                if ch == '`' {
                    break;
                }
            }
            continue;
        }

        // Check for COLLATE keyword (case-insensitive, word boundary)
        let rest_upper = upper.get(i..).unwrap_or("");
        if rest_upper.starts_with("COLLATE") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip optional whitespace after COLLATE
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Read the collation name (letters, digits, underscores, hyphens)
                let name_start = j;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                let collation_name = sql[name_start..j].to_uppercase();

                // Strip SQLite-only collations that have no direct PostgreSQL equivalent.
                //
                // ICU names (icu_root, icu_und, …) and unicode/unicode_ci are not
                // recognised by sqlparser/SQLiteDialect and cause a hard parse error:
                //   "Expected: end of statement, found: collate"
                //
                // RTRIM and BINARY parse fine, but PostgreSQL has no matching collation
                // so they are stripped here too.  NOCASE is intentionally left for the
                // AST-level handler in query.rs which converts it to LOWER(…).
                // ICU root and the unicode names are the schema's
                // `plex.icu_root` (a deterministic ICU root collation), so
                // "Sort by title" orders as stock Plex does instead of by
                // the database's locale (2026-10-16).
                let is_icu = collation_name.starts_with("ICU")
                    || collation_name == "UNICODE"
                    || collation_name == "UNICODE_CI";
                if is_icu {
                    parenthesize_trailing_qualified_identifier(&mut result);
                    result.push_str("COLLATE plex.icu_root");
                    i = j;
                    continue;
                }

                let should_strip = collation_name == "RTRIM" || collation_name == "BINARY";

                if should_strip {
                    // Skip the entire `COLLATE <name>` token.
                    // Also consume one leading space before COLLATE (if any) to avoid
                    // leaving a double-space artefact.
                    if result.ends_with(' ') {
                        result.pop();
                    }
                    i = j;
                    continue;
                }

                // sqlparser 0.61 accepts COLLATE only after a bare
                // identifier: `metadata_items.title COLLATE NOCASE` is
                // "Expected: end of statement, found: COLLATE". A qualified
                // column is parenthesised, which it does accept, and the
                // query rewrite unwraps again.
                parenthesize_trailing_qualified_identifier(&mut result);
            }
        }

        push_utf8_byte(&mut result, sql, i);
        i += 1;
    }

    result
}

/// If `out` ends (before optional whitespace) in a dotted identifier such as
/// `t.col` or `"t"."col"`, wrap that identifier in parentheses.
fn parenthesize_trailing_qualified_identifier(out: &mut String) {
    let bytes = out.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return;
    }
    let mut start = end;
    let mut has_dot = false;
    while start > 0 {
        let b = bytes[start - 1];
        if is_ident_char(b) || b == b'.' || b == b'"' || b == b'`' {
            if b == b'.' {
                has_dot = true;
            }
            start -= 1;
        } else {
            break;
        }
    }
    if !has_dot || start == end {
        return;
    }
    let ident = &out[start..end];
    if ident.starts_with('.')
        || ident.ends_with('.')
        || ident.starts_with(|c: char| c.is_ascii_digit())
    {
        return;
    }
    let wrapped = format!("({})", ident);
    out.replace_range(start..end, &wrapped);
}

/// Spell SQLite's `IS` / `IS NOT` against an expression as
/// `IS NOT DISTINCT FROM` / `IS DISTINCT FROM`.
///
/// In SQLite `x IS y` compares like `=` but treats two NULLs as equal, and
/// Plex writes it against a parameter (`guid IS ?`). sqlparser accepts only
/// NULL, TRUE, FALSE, UNKNOWN, DISTINCT FROM and NORMALIZED after IS, so any
/// other operand was a parse error. PostgreSQL's `IS [NOT] DISTINCT FROM`
/// has the same null-safe meaning.
fn rewrite_is_operator(sql: &str) -> String {
    const KEYWORD_OPERANDS: &[&str] = &[
        "null",
        "true",
        "false",
        "unknown",
        "distinct",
        "normalized",
        "nfc",
        "nfd",
        "nfkc",
        "nfkd",
    ];

    fn word_at(bytes: &[u8], at: usize) -> (usize, usize) {
        let mut i = at;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && is_ident_char(bytes[i]) {
            i += 1;
        }
        (start, i)
    }

    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // Quoted strings and identifiers, and comments, are copied whole.
        if b == b'\'' || b == b'"' || b == b'`' {
            push_utf8_byte(&mut out, sql, i);
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                push_utf8_byte(&mut out, sql, i);
                i += 1;
                if c == b {
                    if b != b'`' && i < bytes.len() && bytes[i] == b {
                        push_utf8_byte(&mut out, sql, i);
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            continue;
        }
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                push_utf8_byte(&mut out, sql, i);
                i += 1;
            }
            continue;
        }

        let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
        if !prev_is_ident
            && starts_with_ascii_icase_at(bytes, i, b"is")
            && !(i + 2 < bytes.len() && is_ident_char(bytes[i + 2]))
        {
            let (w1s, w1e) = word_at(bytes, i + 2);
            let w1 = sql[w1s..w1e].to_ascii_lowercase();
            if w1 == "not" {
                let (w2s, w2e) = word_at(bytes, w1e);
                let w2 = sql[w2s..w2e].to_ascii_lowercase();
                if w2s == w2e || !KEYWORD_OPERANDS.contains(&w2.as_str()) {
                    out.push_str("IS DISTINCT FROM");
                    i = w1e;
                    continue;
                }
            } else if w1s == w1e || !KEYWORD_OPERANDS.contains(&w1.as_str()) {
                out.push_str("IS NOT DISTINCT FROM");
                i += 2;
                continue;
            }
        }

        push_utf8_byte(&mut out, sql, i);
        i += 1;
    }
    out
}
