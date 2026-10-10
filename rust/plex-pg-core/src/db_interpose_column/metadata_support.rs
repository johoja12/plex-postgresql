use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) fn mask_collection_metadata_type(
    pg_stmt: &PgStmt,
    col_name: *const c_char,
    raw_val: i64,
    out: &mut i64,
) -> bool {
    if col_name.is_null() {
        return false;
    }
    let sql_ptr = pg_stmt.pg_sql;
    if sql_ptr.is_null() {
        return false;
    }
    let rc = crate::db_interpose_helpers::rust_should_mask_collection_metadata_type(
        sql_ptr, col_name, raw_val,
    );
    if rc == 0 {
        return false;
    }
    let row = pg_stmt.current_row;
    log_debug_lazy!(
        "COMPAT_TYPE18: masking metadata_type 18 -> 0 for related-items query, row {}",
        row
    );
    *out = 0;
    true
}

pub(crate) unsafe fn set_metadata_result_state(
    pg_stmt: &mut PgStmt,
    result: *mut PgResultLibpq,
    exec_conn: *mut PgConnection,
    num_rows: c_int,
    current_row: c_int,
) {
    pg_stmt.result = result;
    pg_stmt.num_rows = num_rows;
    pg_stmt.current_row = current_row;
    pg_stmt.set_result_conn(exec_conn);
    pg_stmt.metadata_only_result = 1;
}

pub(crate) fn ensure_pg_result_for_metadata(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }

    let pg_stmt_ref = unsafe { &mut *pg_stmt };

    if !pg_stmt_ref.result.is_null() || !pg_stmt_ref.cached_result.is_null() {
        return true;
    }
    if pg_stmt_ref.num_cols > 0 {
        return true;
    }
    if pg_stmt_ref.pg_sql.is_null()
        || pg_stmt_ref.conn().is_null()
        || unsafe { (*pg_stmt_ref.conn()).conn.is_null() }
    {
        return false;
    }

    let conn = pg_stmt_ref.conn();
    let conn_ref = unsafe { &*conn };
    let is_library =
        crate::db_interpose_helpers::rust_is_library_db_path(conn_ref.db_path.as_ptr());
    if is_library == 0 {
        return false;
    }

    let mut exec_conn = conn;
    let thread_conn = unsafe {
        if conn_ref.streaming_active.load(Ordering::SeqCst) != 0 {
            pg_get_thread_connection_excluding(conn_ref.db_path.as_ptr(), conn as *const c_void)
        } else {
            pg_get_thread_connection(conn_ref.db_path.as_ptr())
        }
    };
    if !thread_conn.is_null() {
        let tc = unsafe { &*thread_conn };
        if tc.is_pg_active != 0 && !tc.conn.is_null() {
            exec_conn = thread_conn;
        }
    }

    if unsafe { (&*exec_conn).streaming_active.load(Ordering::SeqCst) != 0 } {
        log_debug_lazy!(
            "METADATA: skipping — connection {:p} is streaming_active",
            exec_conn
        );
        return false;
    }

    let ec = unsafe { &mut *exec_conn };
    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut ec.mutex as *mut _) };

    if ec.streaming_active.load(Ordering::SeqCst) != 0 {
        log_debug_lazy!(
            "METADATA: skipping after lock — connection {:p} is streaming_active",
            exec_conn
        );
        return false;
    }

    crate::libpq_helpers::rust_pq_set_nonblocking(ec.conn, 0);
    while crate::libpq_helpers::rust_pq_is_busy(ec.conn) != 0 {
        crate::libpq_helpers::rust_pq_consume_input(ec.conn);
    }
    loop {
        let pending = crate::libpq_helpers::rust_pq_get_result(ec.conn);
        if pending.is_null() {
            break;
        }
        crate::libpq_helpers::rust_pq_clear(pending);
    }

    unsafe {
        // Column metadata must not execute a full library SELECT merely to
        // discard its rows at the first sqlite3_step. Describe works for bound
        // and parameter-free statements too, and preserves original column OIDs.
        if pg_stmt_ref.stmt_name[0] != 0 {
            log_info_lazy!(
                "METADATA_DESCRIBE: Using prepared-statement describe for: {}",
                cstr_prefix(pg_stmt_ref.pg_sql, 100, "?")
            );

            let mut cached_name: *const c_char = ptr::null();
            let cached = pg_stmt_cache_lookup(
                exec_conn,
                pg_stmt_ref.sql_hash,
                &mut cached_name as *mut *const c_char,
            );
            if cached == 0 {
                let prep = crate::libpq_helpers::rust_pq_prepare(
                    ec.conn,
                    pg_stmt_ref.stmt_name.as_ptr(),
                    pg_stmt_ref.pg_sql,
                    0,
                    ptr::null(),
                );
                if crate::libpq_helpers::rust_pq_result_status(prep) != PGRES_COMMAND_OK {
                    if pg_is_duplicate_prepared_stmt(prep) == 0 {
                        log_error(&format!(
                            "METADATA_DESCRIBE: PQprepare failed: {}\n  Original SQL: {}\n  Translated SQL: {}",
                            cstr_to_string_or(
                                crate::libpq_helpers::rust_pq_error_message(ec.conn),
                                "?"
                            ),
                            cstr_to_string_or(pg_stmt_ref.sql, "?"),
                            cstr_to_string_or(pg_stmt_ref.pg_sql, "?")
                        ));
                        crate::libpq_helpers::rust_pq_clear(prep);
                        return false;
                    }
                }
                pg_stmt_cache_add(
                    exec_conn,
                    pg_stmt_ref.sql_hash,
                    pg_stmt_ref.stmt_name.as_ptr(),
                    pg_stmt_ref.param_count,
                );
                crate::libpq_helpers::rust_pq_clear(prep);
            }

            let desc = crate::libpq_helpers::rust_pq_describe_prepared(
                ec.conn,
                pg_stmt_ref.stmt_name.as_ptr(),
            );

            if crate::libpq_helpers::rust_pq_result_status(desc) == PGRES_COMMAND_OK {
                pg_stmt_ref.num_cols = crate::libpq_helpers::rust_pq_nfields(desc);
                pg_stmt_ref.ensure_column_capacity(pg_stmt_ref.num_cols as usize);
                if pg_stmt_ref.num_cols > 0 {
                    let ncols = pg_stmt_ref.num_cols as usize;
                    let col_names =
                        libc::calloc(ncols, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
                    if !col_names.is_null() {
                        pg_stmt_ref.col_names = col_names;
                        pg_stmt_ref.num_col_names = pg_stmt_ref.num_cols;
                        for i in 0..ncols {
                            let name = crate::db_interpose_helpers::rust_pg_result_col_name(
                                helpers_result_ptr(desc),
                                i as c_int,
                            );
                            if !name.is_null() {
                                let dup = libc::strdup(name);
                                *col_names.add(i) = dup;
                            }
                        }
                    }
                }
                set_metadata_result_state(pg_stmt_ref, desc, exec_conn, 0, 0);
                log_info_lazy!(
                    "METADATA_DESCRIBE: Success - {} cols for: {}",
                    pg_stmt_ref.num_cols,
                    cstr_prefix(pg_stmt_ref.pg_sql, 100, "?")
                );
                // Table-origin metadata is present on described results too.
                // Resolve it before SOCI chooses its holders, just as the old
                // execute-for-metadata path did. Never nest the connection lock.
                drop(_conn_guard);
                if rust_resolve_column_tables(pg_stmt, exec_conn) < 0 {
                    log_error("Failed to resolve described column tables");
                }
                return true;
            }

            log_error(&format!(
                "METADATA_DESCRIBE: PQdescribePrepared failed: {}",
                cstr_to_string_or(crate::libpq_helpers::rust_pq_error_message(ec.conn), "?")
            ));
            crate::libpq_helpers::rust_pq_clear(desc);
            return false;
        }
    }

    log_info_lazy!(
        "METADATA_EXEC: Executing query for column metadata access: {}",
        cstr_prefix(pg_stmt_ref.pg_sql, 100, "?")
    );

    let param_values: Vec<*const c_char> = {
        let count = (pg_stmt_ref.param_count.max(0) as usize).min(pg_stmt_ref.param_values.len());
        let mut pv = vec![ptr::null(); count];
        for i in 0..count {
            pv[i] = pg_stmt_ref.param_values[i] as *const c_char;
        }
        pv
    };

    pg_stmt_ref.result = crate::libpq_helpers::rust_pq_exec_params(
        ec.conn,
        pg_stmt_ref.pg_sql,
        pg_stmt_ref.param_count,
        ptr::null(),
        param_values.as_ptr(),
        ptr::null(),
        ptr::null(),
        0,
    );

    let status = crate::libpq_helpers::rust_pq_result_status(pg_stmt_ref.result);
    if status == PGRES_TUPLES_OK {
        pg_stmt_ref.num_rows = crate::libpq_helpers::rust_pq_ntuples(pg_stmt_ref.result);
        pg_stmt_ref.num_cols = crate::libpq_helpers::rust_pq_nfields(pg_stmt_ref.result);
        pg_stmt_ref.ensure_column_capacity(pg_stmt_ref.num_cols as usize);

        unsafe {
            set_metadata_result_state(
                pg_stmt_ref,
                pg_stmt_ref.result,
                exec_conn,
                pg_stmt_ref.num_rows,
                -1,
            );
        }

        // Release conn mutex before resolve_column_tables to reduce hold time.
        // resolve_column_tables acquires its own conn lock scope.
        drop(_conn_guard);

        if rust_resolve_column_tables(pg_stmt, exec_conn) < 0 {
            log_error("Failed to resolve column tables");
        }

        true
    } else {
        let err = cstr_to_string_or(crate::libpq_helpers::rust_pq_error_message(ec.conn), "?");
        crate::libpq_helpers::rust_pq_clear(pg_stmt_ref.result);
        pg_stmt_ref.result = ptr::null_mut();
        drop(_conn_guard);
        log_error(&format!("METADATA_EXEC: Query failed: {}", err));
        false
    }
}
