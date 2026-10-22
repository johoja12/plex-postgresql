use super::support::{is_duplicate_prepared_stmt, malloc_cstring, parse_positive_returning_rowid};
use super::*;
use crate::log_info_lazy;
use crate::pg_client::PgErrorClass;

fn set_pg_last_error(pg: &mut crate::ffi_types::PgConnection, msg: &str) {
    pg.set_last_error(SQLITE_ERROR, msg);
}

/// Hand the caller `*errmsg` as `sqlite3_exec` does: a copy from
/// `sqlite3_malloc`, which it frees with `sqlite3_free`. Nothing is written
/// when the caller passed no slot.
pub(crate) fn set_errmsg_out(errmsg: *mut *mut c_char, msg: &str) {
    if errmsg.is_null() {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len() + 1;
    let out = crate::db_interpose_metadata::rust_my_sqlite3_malloc(n as c_int) as *mut c_char;
    if out.is_null() {
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
        *out.add(bytes.len()) = 0;
        *errmsg = out;
    }
}

/// The row callback `sqlite3_exec` promises: once per row, with the column
/// names and the row's values as C strings, NULL for SQL NULL. A non-zero
/// return stops the loop and the exec answers SQLITE_ABORT.
///
/// `sqlite3_exec` with a callback on a PostgreSQL-routed handle never called
/// it before 2026-10-16: the rows were fetched and dropped, so anything Plex
/// read this way (a PRAGMA-style probe, a count) saw no rows at all.
pub(crate) unsafe fn deliver_rows_to_callback(
    res: *const PGresult,
    callback: ExecCallback,
    arg: *mut c_void,
) -> c_int {
    let Some(cb) = callback else {
        return SQLITE_OK;
    };
    let nrows = crate::libpq_helpers::rust_pq_ntuples(res);
    let ncols = crate::libpq_helpers::rust_pq_nfields(res);
    let mut names: Vec<*mut c_char> = (0..ncols)
        .map(|c| crate::db_interpose_helpers::rust_pg_result_col_name(res, c) as *mut c_char)
        .collect();
    let mut values: Vec<*mut c_char> = vec![std::ptr::null_mut(); ncols.max(0) as usize];
    for row in 0..nrows {
        for col in 0..ncols {
            let mut value: *const c_char = std::ptr::null();
            let mut len: c_int = 0;
            let mut is_null: c_int = 0;
            crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
                res,
                row,
                col,
                &mut value,
                &mut len,
                &mut is_null,
            );
            values[col as usize] = if is_null != 0 {
                std::ptr::null_mut()
            } else {
                value as *mut c_char
            };
        }
        if cb(arg, ncols, values.as_mut_ptr(), names.as_mut_ptr()) != 0 {
            return SQLITE_ABORT;
        }
    }
    SQLITE_OK
}

pub(crate) fn exec_via_postgres(
    pg_conn: *mut crate::ffi_types::PgConnection,
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    let _metadata = crate::pg_client::registry_api::MetadataGuard::new(db, pg_conn);
    let pg = unsafe { &mut *pg_conn };
    unsafe {
        if pg.conn.is_null() || crate::libpq_helpers::rust_pq_status(pg.conn) != CONNECTION_OK {
            log_error(&format!(
                "EXEC: CONNECTION_BAD pre-flight, attempting reconnect (thread {:p})",
                libc::pthread_self() as *mut c_void
            ));
            let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);
            if !pg.conn.is_null() {
                crate::libpq_helpers::rust_pq_reset(pg.conn);
                crate::pg_client::txn::forget_connection(pg_conn);
                if crate::libpq_helpers::rust_pq_status(pg.conn) != CONNECTION_OK {
                    log_error("EXEC: PQreset failed, trying fresh PQconnectdb...");
                    crate::pg_client::rust_stmt_cache_clear(pg_conn as *mut c_void);
                    crate::libpq_helpers::rust_pq_finish(pg.conn);
                    pg.conn = std::ptr::null_mut();

                    let rcfg = pg_config_get();
                    if rcfg.is_null() {
                        pg.is_pg_active = 0;
                        conn_guard.unlock();
                        EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        return SQLITE_ERROR;
                    }
                    let cfg = &*rcfg;
                    let new_conn = connect_new(cfg);
                    if crate::libpq_helpers::rust_pq_status(new_conn) == CONNECTION_OK {
                        pg.conn = new_conn;
                        pg.is_pg_active = 1;
                        log_info("EXEC: fresh connection succeeded (reconnected)");
                        apply_pg_session_settings(pg.conn, cfg, pg.db_path.as_ptr());
                    } else {
                        log_error(&format!(
                            "EXEC: fresh connection also failed: {}",
                            cstr_to_string_or(
                                crate::libpq_helpers::rust_pq_error_message(new_conn),
                                "(null)"
                            )
                        ));
                        crate::libpq_helpers::rust_pq_finish(new_conn);
                        pg.is_pg_active = 0;
                        conn_guard.unlock();
                        EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        return SQLITE_ERROR;
                    }
                } else {
                    log_error("EXEC: PQreset succeeded, connection recovered");
                }
                let cfg = pg_config_get();
                if !cfg.is_null() {
                    apply_pg_session_settings(pg.conn, &*cfg, pg.db_path.as_ptr());
                }
            } else {
                let rcfg = pg_config_get();
                if rcfg.is_null() {
                    pg.is_pg_active = 0;
                    conn_guard.unlock();
                    EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                    return SQLITE_ERROR;
                }
                let cfg = &*rcfg;
                let new_conn = connect_new(cfg);
                if crate::libpq_helpers::rust_pq_status(new_conn) == CONNECTION_OK {
                    pg.conn = new_conn;
                    pg.is_pg_active = 1;
                    log_error("EXEC: fresh connection from NULL succeeded");
                    let cfg2 = pg_config_get();
                    if !cfg2.is_null() {
                        apply_pg_session_settings(pg.conn, &*cfg2, pg.db_path.as_ptr());
                    }
                } else {
                    log_error(&format!(
                        "EXEC: fresh connection from NULL failed: {}",
                        cstr_to_string_or(
                            crate::libpq_helpers::rust_pq_error_message(new_conn),
                            "(null)"
                        )
                    ));
                    crate::libpq_helpers::rust_pq_finish(new_conn);
                    pg.is_pg_active = 0;
                    conn_guard.unlock();
                    EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                    return SQLITE_ERROR;
                }
            }
            conn_guard.unlock();
        }

        let mut exec_sql = sql;
        let blobs_rewrite = rewrite_blobs_schema_migrations(sql, pg.db_path.as_ptr());
        if !blobs_rewrite.is_null() {
            exec_sql = blobs_rewrite;
        }

        // BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE: forwarded on this
        // connection (pg_client::txn), a no-op with PLEX_PG_TRANSACTIONS=0.
        if crate::pg_client::txn::enabled() {
            let sql_str = crate::db_interpose_helpers::cstr_to_str_or_empty(exec_sql);
            if let Some(cmd) = crate::pg_client::txn::classify_sql(sql_str) {
                let rc = match crate::pg_client::txn::run(pg_conn, db, &cmd) {
                    Ok(()) => SQLITE_OK,
                    Err(msg) => {
                        set_errmsg_out(errmsg, &msg);
                        SQLITE_ERROR
                    }
                };
                if !blobs_rewrite.is_null() {
                    libc::free(blobs_rewrite as *mut c_void);
                }
                return rc;
            }
        }

        if crate::pg_config::pg_config_should_skip_sql(exec_sql) == 0 {
            if crate::db_interpose_helpers::rust_is_junk_metadata_insert(exec_sql) != 0 {
                log_error(
                    "GUARD: Blocked exec junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL)",
                );
                if !blobs_rewrite.is_null() {
                    libc::free(blobs_rewrite as *mut c_void);
                }
                return SQLITE_OK;
            }

            let mut trans = sql_translate(exec_sql);
            if trans.success != 0 && !trans.sql.is_null() {
                let mut owned_insert: *mut c_char = std::ptr::null_mut();
                let mut exec_pg_sql = trans.sql;
                let sql_bytes = CStr::from_ptr(exec_sql).to_bytes();

                if starts_with_icase_bytes(sql_bytes, b"INSERT")
                    && !contains_bytes(CStr::from_ptr(trans.sql).to_bytes(), b"RETURNING")
                {
                    let base = cstr_to_string_or(trans.sql, "");
                    let sql = format!("{base} RETURNING id");
                    owned_insert = malloc_cstring(&sql);
                    if !owned_insert.is_null() {
                        exec_pg_sql = owned_insert;
                        if contains_bytes(sql_bytes, b"play_queue_generators") {
                            log_info_lazy!(
                                "EXEC play_queue_generators INSERT with RETURNING: {}",
                                cstr_prefix(exec_pg_sql, 300, "NULL")
                            );
                        }
                    }
                }

                let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);

                let normalized =
                    crate::db_interpose_helpers::rust_normalize_sql_literals(exec_pg_sql);
                let res: *mut PGresult = if !normalized.is_null() {
                    let norm = &*normalized;
                    let norm_hash = crate::pg_client::rust_hash_sql(norm.normalized_sql);
                    let mut cached_stmt_name: *const c_char = std::ptr::null();

                    if crate::pg_client::rust_stmt_cache_lookup(
                        pg_conn as *mut c_void,
                        norm_hash,
                        &mut cached_stmt_name,
                    ) != 0
                    {
                        crate::libpq_helpers::rust_pq_exec_prepared(
                            pg.conn,
                            cached_stmt_name,
                            norm.param_count,
                            norm.param_values as *const *const c_char,
                            std::ptr::null(),
                            std::ptr::null(),
                            0,
                        )
                    } else {
                        let stmt_name = format!("nx_{:x}", norm_hash);
                        let stmt_name_c =
                            CString::new(stmt_name).unwrap_or_else(|_| CString::new("").unwrap());
                        let prep_res = crate::libpq_helpers::rust_pq_prepare(
                            pg.conn,
                            stmt_name_c.as_ptr(),
                            norm.normalized_sql,
                            0,
                            std::ptr::null(),
                        );
                        let ok = crate::libpq_helpers::rust_pq_result_status(prep_res)
                            == PGRES_COMMAND_OK
                            || is_duplicate_prepared_stmt(prep_res);
                        if ok {
                            crate::pg_client::rust_stmt_cache_add(
                                pg_conn as *mut c_void,
                                norm_hash,
                                stmt_name_c.as_ptr(),
                                norm.param_count,
                            );
                            crate::libpq_helpers::rust_pq_clear(prep_res);
                            crate::libpq_helpers::rust_pq_exec_prepared(
                                pg.conn,
                                stmt_name_c.as_ptr(),
                                norm.param_count,
                                norm.param_values as *const *const c_char,
                                std::ptr::null(),
                                std::ptr::null(),
                                0,
                            )
                        } else {
                            crate::libpq_helpers::rust_pq_clear(prep_res);
                            crate::libpq_helpers::rust_pq_exec(pg.conn, exec_pg_sql)
                        }
                    }
                } else {
                    let sql_hash = crate::pg_client::rust_hash_sql(exec_pg_sql);
                    let mut cached_stmt_name: *const c_char = std::ptr::null();
                    if crate::pg_client::rust_stmt_cache_lookup(
                        pg_conn as *mut c_void,
                        sql_hash,
                        &mut cached_stmt_name,
                    ) != 0
                    {
                        crate::libpq_helpers::rust_pq_exec_prepared(
                            pg.conn,
                            cached_stmt_name,
                            0,
                            std::ptr::null(),
                            std::ptr::null(),
                            std::ptr::null(),
                            0,
                        )
                    } else {
                        crate::libpq_helpers::rust_pq_exec(pg.conn, exec_pg_sql)
                    }
                };

                if !normalized.is_null() {
                    crate::db_interpose_helpers::rust_free_normalized_sql(normalized);
                }

                let status = crate::libpq_helpers::rust_pq_result_status(res);
                if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
                    let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
                    let tuples_ptr = if cmd_tuples.is_null() {
                        c"1".as_ptr()
                    } else {
                        cmd_tuples
                    };
                    pg.clear_last_error();
                    // SQLite changes() is the last DML count, not the number
                    // of rows returned by a SELECT on this pooled session.
                    if starts_with_icase_bytes(sql_bytes, b"INSERT")
                        || starts_with_icase_bytes(sql_bytes, b"UPDATE")
                        || starts_with_icase_bytes(sql_bytes, b"DELETE")
                        || starts_with_icase_bytes(sql_bytes, b"REPLACE")
                    {
                        pg.last_changes =
                            crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);
                    }

                    if status == PGRES_TUPLES_OK
                        && callback.is_some()
                        && !starts_with_icase_bytes(sql_bytes, b"INSERT")
                    {
                        // The INSERT's rows are the RETURNING id the shim
                        // added, which Plex did not ask for.
                        let rc = deliver_rows_to_callback(res, callback, arg);
                        if rc != SQLITE_OK {
                            if !owned_insert.is_null() {
                                libc::free(owned_insert as *mut c_void);
                            }
                            crate::libpq_helpers::rust_pq_clear(res);
                            conn_guard.unlock();
                            sql_translation_free(&mut trans as *mut SqlTranslation);
                            if !blobs_rewrite.is_null() {
                                libc::free(blobs_rewrite as *mut c_void);
                            }
                            set_pg_last_error(pg, "query aborted");
                            pg.last_error_code = rc;
                            set_errmsg_out(errmsg, "query aborted");
                            return rc;
                        }
                    }

                    if starts_with_icase_bytes(sql_bytes, b"INSERT")
                        && status == PGRES_TUPLES_OK
                        && crate::libpq_helpers::rust_pq_ntuples(res) > 0
                    {
                        let mut id_buf = [0 as c_char; 64];
                        let mut id_str: *const c_char = std::ptr::null();
                        if crate::db_interpose_helpers::rust_pg_result_text_copy(
                            res as *const crate::db_interpose_helpers::PGresult,
                            0,
                            0,
                            id_buf.as_mut_ptr(),
                            id_buf.len(),
                        ) >= 0
                        {
                            id_str = id_buf.as_ptr();
                        }
                        if !id_str.is_null() && !CStr::from_ptr(id_str).to_bytes().is_empty() {
                            if let Some(rowid) = parse_positive_returning_rowid(id_str) {
                                pg.last_insert_rowid = rowid;
                                pg.last_insert_failed
                                    .store(0, std::sync::atomic::Ordering::Release);
                                crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                            }
                            if contains_bytes(sql_bytes, b"play_queue_generators") {
                                log_info_lazy!(
                                    "EXEC play_queue_generators: RETURNING id = {}",
                                    cstr_to_string_or(id_str, "?")
                                );
                            }
                            let meta_id = crate::pg_statement::rust_extract_metadata_id(exec_sql);
                            if meta_id > 0 {
                                crate::pg_client::rust_set_global_metadata_id(meta_id);
                            }
                        }
                    }
                } else {
                    // Refused. One classifier with the step: a lost connection
                    // or a stale plan is retried by the caller; anything else
                    // is SQLITE_ERROR now, with PostgreSQL's message as
                    // sqlite3_errmsg and in *errmsg. This used to answer
                    // SQLITE_OK for a constraint violation.
                    let (class, message) = crate::pg_client::classify_result(pg.conn, res);
                    log_error(&format!("PostgreSQL exec error ({:?}): {}", class, message));
                    if !owned_insert.is_null() {
                        libc::free(owned_insert as *mut c_void);
                    }
                    crate::libpq_helpers::rust_pq_clear(res);
                    conn_guard.unlock();
                    sql_translation_free(&mut trans as *mut SqlTranslation);
                    if !blobs_rewrite.is_null() {
                        libc::free(blobs_rewrite as *mut c_void);
                    }
                    match class {
                        PgErrorClass::StalePlan => {
                            crate::pg_client::rust_stmt_cache_clear_local(pg_conn as *mut c_void);
                            EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        }
                        PgErrorClass::Retry => {
                            crate::pg_client::rust_pool_check_health(pg_conn as *mut c_void);
                            EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        }
                        PgErrorClass::Fail => {
                            crate::db_interpose_step_write_utils::note_failed_write(
                                pg, exec_sql, &message,
                            );
                            set_errmsg_out(errmsg, &message);
                        }
                    }
                    return SQLITE_ERROR;
                }

                if !owned_insert.is_null() {
                    libc::free(owned_insert as *mut c_void);
                }
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
            } else {
                let err = cstr_to_string_or(trans.error.as_ptr(), "translation failed");
                let msg = format!(
                    "PG exec translation failed: {} :: {}",
                    cstr_prefix(exec_sql, 220, "NULL"),
                    err
                );
                log_error(&msg);
                set_pg_last_error(pg, &msg);
                set_errmsg_out(errmsg, &msg);
                sql_translation_free(&mut trans as *mut SqlTranslation);
                if !blobs_rewrite.is_null() {
                    libc::free(blobs_rewrite as *mut c_void);
                }
                return SQLITE_ERROR;
            }
            sql_translation_free(&mut trans as *mut SqlTranslation);
        }

        if !blobs_rewrite.is_null() {
            libc::free(blobs_rewrite as *mut c_void);
        }
        SQLITE_OK
    }
}
