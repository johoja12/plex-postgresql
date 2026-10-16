use super::*;
use crate::log_debug_lazy;

pub(crate) fn should_clear_cross_thread_result(
    stmt: *const PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null() || exec_conn.is_null() {
        return false;
    }
    let stmt = unsafe { &*stmt };
    if stmt.result_conn() == exec_conn {
        return false;
    }
    stmt.streaming_mode != 0
}

pub(crate) fn should_use_streaming(stmt: *const PgStmt, disable_streaming_env: bool) -> bool {
    if disable_streaming_env || stmt.is_null() {
        return false;
    }
    let stmt = unsafe { &*stmt };
    if stmt.needs_requery != 0 {
        return false;
    }

    let pg_sql = unsafe { cstr_bytes(stmt.pg_sql) };
    if has_limit_one(pg_sql) {
        return false;
    }

    let sql = unsafe { cstr_bytes(stmt.sql) };
    if has_limit_one(sql) {
        return false;
    }

    true
}

/// Whether the statement ends its result at one row: `LIMIT 1` as a token.
///
/// This was a substring test for "limit 1", which `LIMIT 10`, `LIMIT 100`
/// and `LIMIT 1, 20` all contain, so every paginated listing was fetched
/// eagerly as if it were a single-row probe. A `1` followed by a digit is
/// another number; followed by a comma it is the offset of SQLite's
/// `LIMIT offset, count`.
pub(crate) fn has_limit_one(sql: &[u8]) -> bool {
    use std::sync::LazyLock;
    static LIMIT_ONE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        // After `1\b` the next non-space byte must not be a comma; `\b`
        // already refuses a digit. `\s*([^,\s]|$)` cannot skip over a comma,
        // whichever way `\s*` backtracks.
        regex::bytes::Regex::new(r"(?i)\blimit\s+1\b\s*([^,\s]|$)").unwrap()
    });
    LIMIT_ONE.is_match(sql)
}

pub(crate) unsafe fn adopt_materialized_result_owner(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null() || exec_conn.is_null() {
        return false;
    }
    let stmt = &mut *stmt;
    if stmt.result.is_null() || stmt.streaming_mode != 0 || stmt.result_conn() == exec_conn {
        return false;
    }

    stmt.set_result_conn(exec_conn);
    stmt.executing_thread = libc::pthread_self();
    true
}

#[no_mangle]
pub extern "C" fn rust_step_read_prepare_reexecution_state(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    if stmt.is_null() {
        return;
    }
    let stmt_ref = unsafe { &mut *stmt };
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt) };
    if should_clear_cross_thread_result(stmt, exec_conn) {
        stmt_ref.needs_requery = 1;
        log_debug_lazy!(
            "STEP: Streaming stmt crossed threads; forcing eager requery (result_conn={:p} exec_conn={:p})",
            stmt_ref.result_conn(),
            exec_conn
        );
        crate::pg_statement::rust_stmt_clear_result(stmt);
    } else if unsafe { adopt_materialized_result_owner(stmt, exec_conn) } {
        log_debug_lazy!(
            "STEP: Reusing materialized eager result across threads (result_conn={:p} exec_conn={:p})",
            stmt_ref.result_conn(),
            exec_conn
        );
    }

    if !stmt_ref.result.is_null() && stmt_ref.metadata_only_result != 0 {
        log_debug("STEP: Clearing metadata-only result for re-execution");
        crate::libpq_helpers::rust_pq_clear(stmt_ref.result);
        stmt_ref.result = std::ptr::null_mut();
        stmt_ref.metadata_only_result = 0;
        stmt_ref.current_row = -1;
    }
}

/// The connection a statement that is mid-stream must keep stepping on, or
/// null when the pool should be asked as usual.
///
/// The step resolved its connection through the pool on every call, and the
/// pool refuses to hand a thread the slot it is streaming from -- the flag the
/// statement itself set on its first row. So the statement's own second step
/// was given a different connection, `should_clear_cross_thread_result` read
/// that as the statement having crossed threads, cancelled the stream and ran
/// the query again eagerly, and Plex was handed the first row twice. A play
/// queue built from one movie held it twice, and the movie played again when
/// it finished. Every SELECT Plex steps more than once did this; the pods'
/// statement logs show each one executed on two connections, a few
/// milliseconds apart.
///
/// A statement streaming on the thread that started it stays on that
/// connection. Another thread stepping it keeps the requery, which is what
/// the check was for.
pub(crate) unsafe fn streaming_conn_for_this_thread(stmt: *const PgStmt) -> *mut PgConnection {
    if stmt.is_null() {
        return std::ptr::null_mut();
    }
    let s = &*stmt;
    if s.streaming_mode == 0 {
        return std::ptr::null_mut();
    }
    let sc = s.streaming_conn();
    if sc.is_null() || (*sc).conn.is_null() {
        return std::ptr::null_mut();
    }
    if libc::pthread_equal(s.executing_thread, libc::pthread_self()) == 0 {
        return std::ptr::null_mut();
    }
    sc
}
