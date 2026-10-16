use super::*;

pub(super) unsafe fn eager_fetch_result(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut Option<StmtGuard>,
    conn_guard: &mut PthreadMutexGuard,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    let s = &mut *pg_stmt;
    let ec = &mut *exec_conn;
    // Clear any pre-existing result (e.g. from metadata-only fetch) to prevent leak
    if !s.result.is_null() {
        crate::libpq_helpers::rust_pq_clear(s.result);
    }
    s.result = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    let mut trail = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    while !trail.is_null() {
        crate::libpq_helpers::rust_pq_clear(trail);
        trail = crate::libpq_helpers::rust_pq_get_result(ec.conn);
    }
    conn_guard.unlock();

    if !s.result.is_null()
        && crate::libpq_helpers::rust_pq_result_status(s.result) == PGRES_TUPLES_OK
    {
        s.num_rows = crate::libpq_helpers::rust_pq_ntuples(s.result);
        s.num_cols = crate::libpq_helpers::rust_pq_nfields(s.result);
        s.ensure_column_capacity(s.num_cols as usize);
        s.current_row = 0;
        s.set_result_conn(exec_conn);
        s.metadata_only_result = 0;
        resolve_column_tables(pg_stmt, exec_conn);
        trace_play_queue_result(pg_stmt, s.result, "EAGER");
        if s.num_rows > 0 {
            *exec_conn_io = exec_conn;
            *stmt_guard = None; // unlock
            return STEP_RESULT_ROW;
        }
        s.read_done = 1;
        *exec_conn_io = exec_conn;
        *stmt_guard = None; // unlock
        return STEP_RESULT_DONE;
    }

    // Anything else is a failure. This used to log "EAGER FALLBACK" and
    // answer SQLITE_DONE with no rows, so a query PostgreSQL refused read to
    // Plex as an empty table: a filter that matched nothing, a setting that
    // did not exist. A failed read is SQLITE_ERROR, with the reason.
    let (class, message) = crate::pg_client::classify_result(ec.conn, s.result);
    log_error(&format!(
        "EAGER fetch error ({:?}): {} sql={:.200}",
        class,
        message,
        cstr_to_str(s.pg_sql)
    ));
    if !s.result.is_null() {
        crate::libpq_helpers::rust_pq_clear(s.result);
        s.result = std::ptr::null_mut();
    }
    *exec_conn_io = exec_conn;
    match class {
        PgErrorClass::StalePlan => {
            crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
            set_pg_conn_error(pg_conn_error_out);
        }
        PgErrorClass::Retry => {
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
            set_pg_conn_error(pg_conn_error_out);
        }
        PgErrorClass::Fail => {
            ec.set_last_error(SQLITE_ERROR, &message);
            s.read_done = 1;
        }
    }
    *stmt_guard = None; // unlock
    STEP_RESULT_ERROR
}
