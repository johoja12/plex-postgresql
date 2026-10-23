use super::*;
use crate::byte_utils::{contains_icase_bytes, cstr_bytes};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or};
use crate::log_debug_lazy;

pub(super) fn advance_cached_result_impl(stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    if stmt.cached_result.is_null() {
        return STEP_RESULT_ERROR;
    }
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    stmt.current_row += 1;
    if stmt.current_row >= stmt.num_rows {
        crate::pg_query_cache::rust_query_cache_release(stmt.cached_result);
        stmt.cached_result = std::ptr::null_mut();
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }
    STEP_RESULT_ROW
}

pub(super) fn streaming_next_impl(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    let stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    if stmt.streaming_mode == 0 || stmt.streaming_conn().is_null() {
        return STEP_RESULT_ERROR;
    }

    if unsafe { (*stmt.streaming_conn()).conn.is_null() } {
        log_error(&format!(
            "STREAM: conn disappeared before next row, forcing cleanup stmt={:p} sql={:.100} streaming_conn={:p}",
            p_stmt,
            cstr_to_str(stmt.pg_sql),
            stmt.streaming_conn()
        ));
        unsafe {
            (*stmt.streaming_conn())
                .streaming_active
                .store(0, Ordering::SeqCst);
        }
        stmt.streaming_mode = 0;
        stmt.set_streaming_conn(std::ptr::null_mut());
        stmt.set_result_conn(std::ptr::null_mut());
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }

    if !stmt.result.is_null() {
        crate::libpq_helpers::rust_pq_clear(stmt.result);
        stmt.result = std::ptr::null_mut();
    }
    step_read_clear_row_caches(stmt as *mut PgStmt);

    let row_res =
        crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn()).conn });
    if row_res.is_null() {
        // libpq has no more results before the stream reported its end: the
        // connection dropped mid-stream. Answering DONE here handed Plex a
        // silently truncated list; it is an error, with the reason.
        let sc = unsafe { &mut *stmt.streaming_conn() };
        let message = crate::pg_client::error_class::result_message(sc.conn, std::ptr::null());
        log_error(&format!(
            "STREAM: NULL result mid-stream ({}) stmt={:p} sql={:.100} streaming_conn={:p}",
            message,
            p_stmt,
            cstr_to_str(stmt.pg_sql),
            stmt.streaming_conn()
        ));
        sc.set_last_error(SQLITE_ERROR, &message);
        sc.streaming_active.store(0, Ordering::SeqCst);
        stmt.streaming_mode = 0;
        stmt.set_streaming_conn(std::ptr::null_mut());
        stmt.read_done = 1;
        return STEP_RESULT_ERROR;
    }

    let row_status = crate::libpq_helpers::rust_pq_result_status(row_res);
    if row_status == PGRES_SINGLE_TUPLE {
        stmt.result = row_res;
        stmt.current_row = 0;
        stmt.num_rows = 1;
        stmt.num_cols = crate::libpq_helpers::rust_pq_nfields(row_res);
        stmt.ensure_column_capacity(stmt.num_cols as usize);
        unsafe { trace_play_queue_result(stmt as *mut PgStmt, row_res, "STREAM NEXT") };
        drop(stmt_guard);
        return STEP_RESULT_ROW;
    }
    if row_status == PGRES_TUPLES_OK {
        crate::libpq_helpers::rust_pq_clear(row_res);
        let final_null =
            crate::libpq_helpers::rust_pq_get_result(unsafe { (*stmt.streaming_conn()).conn });
        if !final_null.is_null() {
            crate::libpq_helpers::rust_pq_clear(final_null);
        }
        stmt.streaming_mode = 0;
        if !stmt.streaming_conn().is_null() {
            unsafe {
                (*stmt.streaming_conn())
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
        }
        stmt.set_streaming_conn(std::ptr::null_mut());
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }

    // An error after the first row (a statement_timeout, a lost connection,
    // a value the server could not produce). The query cannot be run again
    // from here without handing Plex the rows it already has a second time,
    // so this is SQLITE_ERROR whatever the class, with the reason. It used to
    // be DONE, and Plex read a cut-off list as the whole.
    let sc = unsafe { &mut *stmt.streaming_conn() };
    let (class, message) = crate::pg_client::classify_result(sc.conn, row_res);
    log_error(&format!(
        "STREAM ERROR ({:?}): {} (status={}) sql={:.100}",
        class,
        message,
        row_status,
        cstr_to_str(stmt.pg_sql)
    ));
    crate::libpq_helpers::rust_pq_clear(row_res);
    let mut drain = crate::libpq_helpers::rust_pq_get_result(sc.conn);
    while !drain.is_null() {
        crate::libpq_helpers::rust_pq_clear(drain);
        drain = crate::libpq_helpers::rust_pq_get_result(sc.conn);
    }
    sc.set_last_error(SQLITE_ERROR, &message);
    sc.streaming_active.store(0, Ordering::SeqCst);
    stmt.streaming_mode = 0;
    stmt.set_streaming_conn(std::ptr::null_mut());
    stmt.read_done = 1;
    STEP_RESULT_ERROR
}

pub(super) fn eager_next_impl(stmt: *mut PgStmt) -> c_int {
    if stmt.is_null() {
        return STEP_RESULT_ERROR;
    }
    let stmt_ptr = stmt;
    let stmt = unsafe { &mut *stmt };
    if stmt.result.is_null() {
        return STEP_RESULT_ERROR;
    }
    let _stmt_guard = unsafe { PgStmt::lock_mutex(stmt_ptr) };

    stmt.current_row += 1;
    if stmt.current_row >= stmt.num_rows {
        crate::libpq_helpers::rust_pq_clear(stmt.result);
        stmt.result = std::ptr::null_mut();
        stmt.set_result_conn(std::ptr::null_mut());
        stmt.read_done = 1;
        return STEP_RESULT_DONE;
    }
    STEP_RESULT_ROW
}

pub(super) fn log_debug_context_impl(stmt: *mut PgStmt, exec_conn: *mut PgConnection) {
    if stmt.is_null() {
        return;
    }
    let stmt_ref = unsafe { &*stmt };
    if stmt_ref.result.is_null() {
        log_debug_lazy!(
            "STEP READ: thread={:p} stmt={:p} exec_conn={:p}",
            unsafe { libc::pthread_self() as usize as *const c_void },
            stmt,
            exec_conn
        );
        let has_match = unsafe {
            !stmt_ref.sql.is_null()
                && (contains_icase_bytes(cstr_bytes(stmt_ref.sql), b"devices")
                    || contains_icase_bytes(cstr_bytes(stmt_ref.sql), b"library_sections"))
                || !stmt_ref.pg_sql.is_null()
                    && (contains_icase_bytes(cstr_bytes(stmt_ref.pg_sql), b"devices")
                        || contains_icase_bytes(cstr_bytes(stmt_ref.pg_sql), b"library_sections"))
        };
        if has_match {
            log_debug_lazy!(
                "STEP READ devices/library_sections: param_count={} is_pg={}",
                stmt_ref.param_count,
                stmt_ref.is_pg
            );
            for i in 0..(stmt_ref.param_count as usize) {
                if i < stmt_ref.param_values.len() {
                    let p = stmt_ref.param_values[i];
                    log_debug_lazy!("  PARAM[{}]: {}", i, cstr_to_string_or(p, "NULL"));
                } else {
                    log_debug_lazy!("  PARAM[{}]: OUT_OF_BOUNDS", i);
                }
            }
            log_debug_lazy!("  SQL: {}", cstr_prefix(stmt_ref.sql, 1000, "NULL"));
            if !stmt_ref.pg_sql.is_null() {
                log_debug_lazy!("  PG_SQL: {}", cstr_prefix(stmt_ref.pg_sql, 1000, "NULL"));
            }
        }
    }
}

/// A materialized result is independent of its libpq connection. Advancing it
/// does not need parameter rebuilding or a thread-local pool checkout. Keep
/// the original result connection reference alive until the result is cleared.
pub(crate) unsafe fn try_advance_materialized(stmt: *mut PgStmt) -> Option<c_int> {
    if stmt.is_null() { return None; }
    let _guard = PgStmt::lock_mutex(stmt);
    let s = &mut *stmt;
    if (s.is_pg != 2 && !(s.is_pg == 1 && s.write_executed != 0))
        || s.streaming_mode != 0 || s.metadata_only_result != 0 {
        return None;
    }
    if s.read_done != 0 { return Some(STEP_RESULT_DONE); }
    if s.current_row < 0 || (s.result.is_null() && s.cached_result.is_null()) {
        return None;
    }
    s.current_row += 1;
    if s.current_row < s.num_rows { return Some(STEP_RESULT_ROW); }
    if !s.cached_result.is_null() {
        crate::pg_query_cache::rust_query_cache_release(s.cached_result);
        s.cached_result = std::ptr::null_mut();
    } else {
        crate::libpq_helpers::rust_pq_clear(s.result);
        s.result = std::ptr::null_mut();
        s.set_result_conn(std::ptr::null_mut());
    }
    s.read_done = 1;
    Some(STEP_RESULT_DONE)
}

#[cfg(test)]
mod fast_advance_tests {
    use super::*;
    extern "C" {
        fn PQmakeEmptyPGresult(conn: *mut crate::libpq_helpers::PGconn, status: c_int) -> *mut PGresult;
    }
    #[test]
    fn materialized_rows_advance_without_connection_and_clear_at_end() {
        let mut s = PgStmt::new();
        s.is_pg = 2; s.current_row = 0; s.num_rows = 2;
        s.result = unsafe { PQmakeEmptyPGresult(std::ptr::null_mut(), PGRES_TUPLES_OK) };
        assert!(!s.result.is_null());
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_ROW));
        assert_eq!(s.current_row, 1);
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_DONE));
        assert!(s.result.is_null());
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_DONE));
    }
    #[test]
    fn returning_write_advances_all_rows_once_without_reexecuting() {
        let mut s = PgStmt::new();
        s.is_pg = 1;
        s.write_executed = 1;
        s.current_row = 0;
        s.num_rows = 3;
        s.result = unsafe { PQmakeEmptyPGresult(std::ptr::null_mut(), PGRES_TUPLES_OK) };
        assert!(!s.result.is_null());
        for row in [1, 2] {
            assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_ROW));
            assert_eq!(s.current_row, row);
        }
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_DONE));
        assert!(s.result.is_null());
        assert_eq!(s.write_executed, 1);
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, Some(STEP_RESULT_DONE));
    }
    #[test]
    fn first_execution_metadata_and_streaming_keep_normal_path() {
        let mut s = PgStmt::new(); s.is_pg = 2;
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, None);
        s.read_done = 1; s.metadata_only_result = 1;
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, None);
        s.metadata_only_result = 0; s.streaming_mode = 1;
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, None);
        s.streaming_mode = 0; s.is_pg = 1;
        assert_eq!(unsafe { try_advance_materialized(&mut s) }, None);
    }
}
