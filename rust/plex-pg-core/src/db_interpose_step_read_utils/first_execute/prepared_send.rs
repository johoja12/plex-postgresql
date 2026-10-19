use super::*;
use crate::log_debug_lazy;

pub(super) fn disable_streaming_env() -> bool {
    unsafe {
        let key = CString::new("PLEX_PG_DISABLE_STREAMING").unwrap();
        let val = libc::getenv(key.as_ptr());
        crate::db_interpose_helpers::rust_env_truthy(val) != 0
    }
}

unsafe fn send_prepared_or_params(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) -> c_int {
    let s = &*pg_stmt;
    let ec = &*exec_conn;
    if s.use_prepared != 0 && s.stmt_name[0] != 0 && !s.pg_sql.is_null() {
        let mut cached_name: *const c_char = std::ptr::null();
        let is_cached = crate::pg_client::rust_stmt_cache_lookup(
            exec_conn as *mut c_void,
            s.sql_hash,
            &mut cached_name,
        ) != 0;

        let mut cached = is_cached;
        if !cached {
            let prep_res = crate::libpq_helpers::rust_pq_prepare(
                ec.conn,
                s.stmt_name.as_ptr(),
                s.pg_sql,
                s.param_count,
                std::ptr::null(),
            );
            let ok = crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK
                || is_duplicate_prepared_stmt(prep_res);
            if ok {
                crate::pg_client::rust_stmt_cache_add(
                    exec_conn as *mut c_void,
                    s.sql_hash,
                    s.stmt_name.as_ptr(),
                    s.param_count,
                );
                cached_name = s.stmt_name.as_ptr();
                cached = true;
            } else {
                log_error(&format!(
                    "PQprepare failed for {}: {}",
                    cstr_to_str(s.stmt_name.as_ptr()),
                    cstr_to_str(crate::libpq_helpers::rust_pq_error_message(ec.conn))
                ));
            }
            crate::libpq_helpers::rust_pq_clear(prep_res);
        }

        if cached && !cached_name.is_null() {
            return crate::libpq_helpers::rust_pq_send_query_prepared(
                ec.conn,
                cached_name,
                s.param_count,
                param_values,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
        }
    }

    crate::libpq_helpers::rust_pq_send_query_params(
        ec.conn,
        s.pg_sql,
        s.param_count,
        std::ptr::null(),
        param_values,
        std::ptr::null(),
        std::ptr::null(),
        0,
    )
}

pub(super) unsafe fn send_query_for_read(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
    stmt_guard: &mut Option<StmtGuard>,
    conn_guard: &mut PthreadMutexGuard,
    pg_conn_error_out: *mut c_int,
) -> Result<(), c_int> {
    let s = &*pg_stmt;
    let scope = CString::new("STEP READ").unwrap();
    crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());

    let timeout = CString::new(STATEMENT_TIMEOUT_SQL).unwrap();
    let ec_ref = &*exec_conn;
    let to_res = crate::libpq_helpers::rust_pq_exec(ec_ref.conn, timeout.as_ptr());
    if !to_res.is_null() {
        crate::libpq_helpers::rust_pq_clear(to_res);
    }

    if !s.pg_sql.is_null() {
        pg_exception_note_query(s.pg_sql);
    }

    trace_play_queue_params(pg_stmt, param_values, "EXEC");

    log_debug_lazy!(
        "PREPARED CHECK: use_prepared={} stmt_name[0]={} pg_sql={:p}",
        s.use_prepared,
        s.stmt_name[0] as i32,
        s.pg_sql
    );

    let send_ok = send_prepared_or_params(pg_stmt, exec_conn, param_values);
    if send_ok != 0 {
        return Ok(());
    }

    // PQsend* refuses without a result: a dead connection, or libpq itself
    // (a command already in flight, out of memory). The connection's status
    // decides whether a retry can help.
    let (class, message) = crate::pg_client::classify_result(ec_ref.conn, std::ptr::null());
    log_error(&format!(
        "PQsend* failed ({:?}): {} sql={:.200}",
        class,
        message,
        cstr_to_str(s.pg_sql)
    ));
    if message.contains("does not exist") {
        crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
    }
    conn_guard.unlock();
    if class == PgErrorClass::Fail {
        (&mut *exec_conn).set_last_error(SQLITE_ERROR, &message);
        (&mut *pg_stmt).read_done = 1;
        *stmt_guard = None; // unlock
        return Err(STEP_RESULT_ERROR);
    }
    crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
    *stmt_guard = None; // unlock
    set_pg_conn_error(pg_conn_error_out);
    Err(STEP_RESULT_ERROR)
}
